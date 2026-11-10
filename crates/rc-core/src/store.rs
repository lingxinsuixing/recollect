//! `core.db`：注册表、每 agent 三张加密表（doc/rev/note）、访问日志。
//!
//! 物理布局逐字照设计 §10.1；形状固定，payload 演进在密文内完成。动态表名只有两个来源：
//! `table_names(agent_id)` 与注册表行，两者都必须先过 `is_valid_core_table`，SQL 里用双引号
//! 包裹。原始 `agent_id` 只作为绑定参数出现，从不拼进 SQL 文本。

use crate::crypto::{
    aad, agent_table_suffix, is_valid_core_table, load_key, load_or_create_key, note_table_name,
    open, seal, sha256_hex_of, table_names, NONCE_LEN,
};
use crate::payload::CorePayload;
use crate::render::{render_brief_with_notebook, Brief};
use crate::CoreError;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};

/// 全局表。`core_doc_<h>`/`core_rev_<h>`/`core_note_<h>` 是每 agent 注册时才建的动态表。
/// （`core_sync_state` 已删：多设备同步未立项、全仓零读者零写者，2026-11-07 治理
/// 分诊裁定死表删除。存量库里的孤表是合法状态——`CREATE TABLE IF NOT EXISTS` 对
/// 其余全局表幂等，孤表不挡路、不写迁移。）
const CORE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS core_agents(
  agent_id TEXT PRIMARY KEY,
  table_name TEXT NOT NULL UNIQUE,
  rev_table_name TEXT NOT NULL UNIQUE,
  key_id TEXT NOT NULL,
  doc_version INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS core_access_log(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at_ms INTEGER NOT NULL, caller_agent_id TEXT NOT NULL,
  tool TEXT NOT NULL, target_section TEXT,
  result TEXT NOT NULL,                  -- ok|denied|not_found|bad_version
  detail TEXT
);

CREATE TABLE IF NOT EXISTS core_injection_log(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id TEXT NOT NULL, user_id TEXT NOT NULL, session_key TEXT,
  assembled_at_ms INTEGER NOT NULL, token_estimate INTEGER NOT NULL,
  section_tokens_json TEXT NOT NULL, cache_hit INTEGER, truncated_json TEXT
);
"#;

/// 当前文档单行表。`{doc}` 由 `table_names` 派生并过 `is_valid_core_table`。
const DOC_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS "{doc}"(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  doc_version INTEGER NOT NULL,
  nonce BLOB NOT NULL,
  ciphertext BLOB NOT NULL,
  aad_hash TEXT NOT NULL,
  payload_sha256 TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  device_id TEXT NOT NULL
);
"#;

/// 版本历史表。
const REV_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS "{rev}"(
  version INTEGER PRIMARY KEY,
  section TEXT NOT NULL,
  op TEXT NOT NULL CHECK(op IN ('ADD','UPDATE','NOOP','RETIRE','MERGE')),
  nonce BLOB NOT NULL, ciphertext BLOB NOT NULL,
  reason_hash TEXT, device_id TEXT NOT NULL, created_at_ms INTEGER NOT NULL
);
"#;

/// 小本本表（W36 细案 §1）：per-agent 第三张动态表，**append-only 的物理表达是两个触发器**，
/// 不是约定。`created_at_ms` 是明文列（调用方传入的发话钟）：list 可以先 ORDER BY LIMIT
/// 再解密；除此之外没有明文列——正文/标题/标签都在 AEAD 内的 JSON 里。
const NOTE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS "{note}"(
  id TEXT PRIMARY KEY,
  created_at_ms INTEGER NOT NULL,
  device_id TEXT NOT NULL,
  nonce BLOB NOT NULL,
  ciphertext BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS "{note}_by_time" ON "{note}"(created_at_ms DESC);
CREATE TRIGGER IF NOT EXISTS "{note}_no_upd" BEFORE UPDATE ON "{note}"
  BEGIN SELECT RAISE(ABORT,'notebook is append-only'); END;
CREATE TRIGGER IF NOT EXISTS "{note}_no_del" BEFORE DELETE ON "{note}"
  BEGIN SELECT RAISE(ABORT,'notebook is append-only'); END;
"#;

pub(crate) const SECTION_DOC: &str = "doc";

/// `core_rev_<h>` 一行的 `(nonce, ciphertext)`。
pub(crate) type RevisionCiphertext = (Vec<u8>, Vec<u8>);

/// 注册表一行的读侧投影。
#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub agent_id: String,
    pub key_id: String,
    pub doc_version: i64,
    pub updated_at_ms: i64,
}

struct Registration {
    doc_table: String,
    rev_table: String,
    doc_version: i64,
    /// 注册表行的"上次动过"时刻：注册时 = created_at_ms；此后每次 `write_doc_version`
    /// 与版本同事务前进。`doc_version > 0` 时它就是"核心文档上次更新"的时间
    /// （brief 自我提醒的取数家，治理小刀②）。
    updated_at_ms: i64,
}

/// `core.db` 句柄。`data_dir` 与库同根：key 文件在 `<data_dir>/keys/`。
pub struct CoreStore {
    conn: Connection,
    data_dir: PathBuf,
}

impl CoreStore {
    /// 打开（必要时创建）`<data_dir>/core.db`；WAL + 外键；建全局表。
    ///
    /// 建完全局表后扫一遍 `core_agents`，给每个存量 agent **幂等补建**小本本表
    /// （W36 细案 §1：本表是纯新增表，`IF NOT EXISTS` 幂等即可——core.db 不立
    /// `user_version`，存量 agent 不用重注册）。
    pub fn open(data_dir: &Path) -> Result<Self, CoreError> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join("core.db"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(CORE_SCHEMA)?;
        let existing: Vec<String> = {
            let mut st = conn.prepare("SELECT agent_id FROM core_agents")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for agent_id in existing {
            let note_table = checked_table(&note_table_name(&agent_id))?.to_owned();
            conn.execute_batch(&render(NOTE_DDL, &note_table))?;
        }
        Ok(CoreStore { conn, data_dir: data_dir.to_path_buf() })
    }

    /// 注册 agent：建 `core_doc_<h>`/`core_rev_<h>`/`core_note_<h>`、生成或加载 key 文件、
    /// 写注册行。重复注册幂等（`CREATE ... IF NOT EXISTS` + `INSERT OR IGNORE`，key 文件不改写）。
    /// 时钟由调用方给：库代码不取系统时间（判据 12a）。
    pub fn register_agent(&self, agent_id: &str, now_ms: i64) -> Result<(), CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let (doc_table, rev_table) = table_names(agent_id);
        let doc_table = checked_table(&doc_table)?.to_owned();
        let rev_table = checked_table(&rev_table)?.to_owned();
        let note_table = checked_table(&note_table_name(agent_id))?.to_owned();
        let key = load_or_create_key(&self.data_dir, agent_id)?;
        drop(key);
        let key_id = format!("k-{}", agent_table_suffix(agent_id));
        let ddl = render(DOC_DDL, &doc_table) + &render(REV_DDL, &rev_table)
            + &render(NOTE_DDL, &note_table);
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(&ddl)?;
        tx.execute(
            "INSERT OR IGNORE INTO core_agents(agent_id, table_name, rev_table_name, key_id, \
             doc_version, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
            params![agent_id, doc_table, rev_table, key_id, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 读当前文档。未注册 ⇒ `Unregistered`；已注册但还没写过 ⇒ `None`。
    pub fn load_doc(&self, agent_id: &str) -> Result<Option<CorePayload>, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let reg = self
            .registration(agent_id)?
            .ok_or(CoreError::Unregistered)?;
        let doc_table = checked_table(&reg.doc_table)?;
        self.open_current_doc(&self.conn, agent_id, doc_table)
    }

    /// 在打开的连接/事务上读并解开当前文档单行。`load_doc`（锁外读）与
    /// [`Self::update_doc`]（锁内读）共用同一口径：AAD 用该行的 `doc_version` 与
    /// `"doc"` 段，解开后形状自检。没有行 ⇒ `Ok(None)`。
    fn open_current_doc(
        &self,
        conn: &Connection,
        agent_id: &str,
        doc_table: &str,
    ) -> Result<Option<CorePayload>, CoreError> {
        let mut st = conn.prepare(&format!(
            "SELECT doc_version, nonce, ciphertext FROM \"{doc_table}\" WHERE singleton=1"
        ))?;
        let row = st
            .query_row([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .optional()?;
        let Some((version, nonce, ciphertext)) = row else {
            return Ok(None);
        };
        let version =
            u32::try_from(version).map_err(|_| CoreError::BadPayload("doc_version".to_string()))?;
        let nonce: [u8; NONCE_LEN] = nonce
            .as_slice()
            .try_into()
            .map_err(|_| CoreError::Crypto)?;
        let key = load_key(&self.data_dir, agent_id)?;
        let plain = open(&key, &aad(agent_id, version, SECTION_DOC), &nonce, &ciphertext)?;
        let payload: CorePayload = serde_json::from_slice(&plain)
            .map_err(|e| CoreError::BadPayload(e.to_string()))?;
        if payload.agent_id != agent_id {
            return Err(CoreError::BadPayload(
                "payload agent_id does not match the caller".to_string(),
            ));
        }
        payload.validate()?;
        Ok(Some(payload))
    }

    /// 当前文档版本；未注册 ⇒ `Unregistered`。
    ///
    /// 给幂等分支用（例如「已关闭的交接条再关一次」）：那条路径**不 save**，
    /// 但回执里的 `version` 必须与库里的真实版本对齐，而不是少报一版。
    pub fn doc_version(&self, agent_id: &str) -> Result<u32, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let reg = self
            .registration(agent_id)?
            .ok_or(CoreError::Unregistered)?;
        u32::try_from(reg.doc_version)
            .map_err(|_| CoreError::BadPayload("doc_version".to_string()))
    }

    /// 写一份新版本：版本 = 当前 +1；同一事务里 upsert 单行、追加 revision、更新注册表。
    ///
    /// 事务是 **`BEGIN IMMEDIATE`**，而且**版本号在事务内读**：写锁先拿到手，再读注册表。
    /// Deferred 会先把读快照放进来，两个写者都看到 `vN`、各自写 `vN+1` ——
    /// 一个把另一个的整份载荷盖掉（丢条），或者 rev 主键相撞被报成 `core_unavailable`。
    /// 拿不到写锁的写者得到的是 SQLite 的 busy 错误（`CoreError::Sql`），不是静默成功。
    pub fn save_doc(
        &self,
        agent_id: &str,
        payload: &CorePayload,
        device_id: &str,
        now_ms: i64,
    ) -> Result<u32, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        // 与版本无关的自检放在拿锁之前：形状错的整份不存，也不该占着写锁去做。
        let mut payload = payload.clone();
        payload.agent_id = agent_id.to_owned();
        payload.validate()?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let reg = registration_on(&tx, agent_id)?.ok_or(CoreError::Unregistered)?;
        let doc_table = checked_table(&reg.doc_table)?;
        let rev_table = checked_table(&reg.rev_table)?;
        let current = u32::try_from(reg.doc_version)
            .map_err(|_| CoreError::BadPayload("doc_version".to_string()))?;
        let version = current
            .checked_add(1)
            .ok_or_else(|| CoreError::BadPayload("doc_version overflow".to_string()))?;
        self.write_doc_version(
            &tx,
            agent_id,
            &payload,
            doc_table,
            rev_table,
            version,
            device_id,
            now_ms,
        )?;
        tx.commit()?;
        Ok(version)
    }

    /// 原子读改写原语（core.db CAS 修复波）：`BEGIN IMMEDIATE` 拿写锁 → 事务内读注册表
    /// 版本 + 解密当前载荷 → 调用方闭包改载荷 → 新版本加密落库，**同一事务提交**。
    ///
    /// 事务语义照 [`Self::save_doc`] 注释逐条保持：写锁先到手（Deferred 会先把读快照放进
    /// 事务，两个写者都看到 `vN`），busy 错误原样上抛，rev 追加与注册表更新同事务，
    /// aad/payload_sha256 口径不变（落库走 [`Self::write_doc_version`]，与 `save_doc` 共字）。
    /// 与 `save_doc` 唯一的差别是**载荷不是调用方给的，是锁内现读的**：闭包看到的一定是
    /// 最新已提交版本（或注册后还没写过文档的空载荷），"两个写者各持 vN 载荷、先后各写
    /// vN+1/vN+2、后者把前者整份盖掉"的丢更新形状在这里造不出来。
    ///
    /// 闭包返回 `Ok(false)` 表示"这次没有要落的变化"：不写、不提交（事务随 drop 回滚）、
    /// 版本不前进，返回闭包看到的当前版本（没写过文档 ⇒ 0）。`mutate` 报错同样整份不存。
    pub fn update_doc<F>(
        &self,
        agent_id: &str,
        device_id: &str,
        now_ms: i64,
        mutate: F,
    ) -> Result<u32, CoreError>
    where
        F: FnOnce(&mut CorePayload) -> Result<bool, CoreError>,
    {
        let agent_id = checked_agent_id(agent_id)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let reg = registration_on(&tx, agent_id)?.ok_or(CoreError::Unregistered)?;
        let doc_table = checked_table(&reg.doc_table)?;
        let rev_table = checked_table(&reg.rev_table)?;
        let current = u32::try_from(reg.doc_version)
            .map_err(|_| CoreError::BadPayload("doc_version".to_string()))?;
        let mut payload = self
            .open_current_doc(&tx, agent_id, doc_table)?
            .unwrap_or_else(|| CorePayload::empty(agent_id));
        let changed = mutate(&mut payload)?;
        if !changed {
            // 没有要落的变化：tx 落盘即回滚，库里一个字节不动。
            return Ok(current);
        }
        payload.agent_id = agent_id.to_owned();
        payload.validate()?;
        let version = current
            .checked_add(1)
            .ok_or_else(|| CoreError::BadPayload("doc_version overflow".to_string()))?;
        self.write_doc_version(
            &tx,
            agent_id,
            &payload,
            doc_table,
            rev_table,
            version,
            device_id,
            now_ms,
        )?;
        tx.commit()?;
        Ok(version)
    }

    /// 加密并把 `version` 落进 doc 单行 + rev 历史 + 注册表，全部发生在调用方**打开的
    /// 写事务**里。`save_doc`/`update_doc` 共用这一段，保证两条写路的 aad/payload_sha256/
    /// rev op 口径逐字一致。
    #[allow(clippy::too_many_arguments)]
    fn write_doc_version(
        &self,
        tx: &Transaction,
        agent_id: &str,
        payload: &CorePayload,
        doc_table: &str,
        rev_table: &str,
        version: u32,
        device_id: &str,
        now_ms: i64,
    ) -> Result<(), CoreError> {
        let plain = serde_json::to_vec(payload)
            .map_err(|e| CoreError::BadPayload(e.to_string()))?;
        let aad_bytes = aad(agent_id, version, SECTION_DOC);
        let key = load_key(&self.data_dir, agent_id)?;
        let (nonce, ciphertext) = seal(&key, &aad_bytes, &plain)?;
        let aad_hash = sha256_hex_of(&aad_bytes);
        let payload_sha256 = sha256_hex_of(&plain);
        let op = if version == 1 { "ADD" } else { "UPDATE" };

        tx.execute(
            &format!(
                "INSERT INTO \"{doc_table}\"(singleton, doc_version, nonce, ciphertext, aad_hash, \
                 payload_sha256, updated_at_ms, device_id) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(singleton) DO UPDATE SET doc_version=excluded.doc_version, \
                 nonce=excluded.nonce, ciphertext=excluded.ciphertext, \
                 aad_hash=excluded.aad_hash, payload_sha256=excluded.payload_sha256, \
                 updated_at_ms=excluded.updated_at_ms, device_id=excluded.device_id"
            ),
            params![
                version,
                nonce.as_slice(),
                ciphertext,
                aad_hash,
                payload_sha256,
                now_ms,
                device_id
            ],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO \"{rev_table}\"(version, section, op, nonce, ciphertext, reason_hash, \
                 device_id, created_at_ms) VALUES (?1, 'doc', ?2, ?3, ?4, NULL, ?5, ?6)"
            ),
            params![version, op, nonce.as_slice(), ciphertext, device_id, now_ms],
        )?;
        let touched = tx.execute(
            "UPDATE core_agents SET doc_version=?2, updated_at_ms=?3 WHERE agent_id=?1",
            params![agent_id, version, now_ms],
        )?;
        if touched != 1 {
            return Err(CoreError::Unregistered);
        }
        Ok(())
    }

    /// 注册表投影，按 `agent_id` 排序。
    pub fn list_agents(&self) -> Result<Vec<AgentRecord>, CoreError> {
        let mut st = self.conn.prepare(
            "SELECT agent_id, key_id, doc_version, updated_at_ms FROM core_agents ORDER BY agent_id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(AgentRecord {
                agent_id: r.get(0)?,
                key_id: r.get(1)?,
                doc_version: r.get(2)?,
                updated_at_ms: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 撤销：删注册行 + `DROP` 三张表（含 W36 的小本本表）。key 文件不在这里删（由调用方/CLI 决定）。
    pub fn revoke_agent(&self, agent_id: &str) -> Result<(), CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let reg = self
            .registration(agent_id)?
            .ok_or(CoreError::NotFound)?;
        let doc_table = checked_table(&reg.doc_table)?;
        let rev_table = checked_table(&reg.rev_table)?;
        let note_name = note_table_name(agent_id);
        let note_table = checked_table(&note_name)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM core_agents WHERE agent_id=?1", [agent_id])?;
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS \"{doc_table}\"; DROP TABLE IF EXISTS \"{rev_table}\"; \
             DROP TABLE IF EXISTS \"{note_table}\";"
        ))?;
        tx.commit()?;
        Ok(())
    }

    /// 写 `core_access_log`。只记调用事实，不记正文。
    pub fn log_access(
        &self,
        caller: &str,
        tool: &str,
        section: Option<&str>,
        result: &str,
        detail: Option<&str>,
        now_ms: i64,
    ) -> Result<(), CoreError> {
        self.conn.execute(
            "INSERT INTO core_access_log(at_ms, caller_agent_id, tool, target_section, result, \
             detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now_ms, caller, tool, section, result, detail],
        )?;
        Ok(())
    }

    /// 写 `core_injection_log`（设计 §10.1）：只记分区 token 读数与截断的区名，不记简报正文。
    /// `section_tokens_json`/`truncated_json` 由调用方给 JSON 文本，原样落库。
    #[allow(clippy::too_many_arguments)]
    pub fn log_injection(
        &self,
        agent_id: &str,
        user_id: &str,
        session_key: Option<&str>,
        token_estimate: usize,
        section_tokens_json: &str,
        cache_hit: Option<bool>,
        truncated_json: &str,
        now_ms: i64,
    ) -> Result<(), CoreError> {
        let token_estimate = i64::try_from(token_estimate)
            .map_err(|_| CoreError::BadPayload("token_estimate overflow".to_string()))?;
        let cache_hit = cache_hit.map(|hit| if hit { 1i64 } else { 0i64 });
        self.conn.execute(
            "INSERT INTO core_injection_log(agent_id, user_id, session_key, assembled_at_ms, \
             token_estimate, section_tokens_json, cache_hit, truncated_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                agent_id,
                user_id,
                session_key,
                now_ms,
                token_estimate,
                section_tokens_json,
                cache_hit,
                truncated_json
            ],
        )?;
        Ok(())
    }

    /// `core_brief`（设计 §9.3/§10.3）：读当前文档 → 渲染常驻简报 → 写一条 `core_injection_log`。
    ///
    /// 已注册但还没写过文档 ⇒ 返回只有头部的空简报（不是错误）。`agent_id` 必须先过
    /// `checked_agent_id`；未注册 ⇒ `Unregistered`，不写注入日志。
    ///
    /// 交接段用 [`crate::DEFAULT_INCLUDE`]（接着做 + 到期）；要别的种类走 [`Self::brief_with`]。
    /// **纯读**：这里不碰 `seen_count`，唯一的写是既有的注入日志。
    #[allow(clippy::too_many_arguments)]
    pub fn brief(
        &self,
        agent_id: &str,
        user_id: &str,
        scene: &str,
        budget: usize,
        session_key: Option<&str>,
        now_ms: i64,
    ) -> Result<Brief, CoreError> {
        self.brief_with(
            agent_id,
            user_id,
            scene,
            budget,
            session_key,
            &crate::DEFAULT_INCLUDE,
            now_ms,
        )
    }

    /// 同 [`Self::brief`]，但 `include` 决定交接段列哪几种 kind（闭集见 `HANDOFF_KINDS`）。
    /// 其余逐字相同：渲染、注入日志、错误形状都不因这个参数而变。
    ///
    /// 自我提醒（治理小刀②）：`doc_version > 0` ⇒ 核心文档存在，把注册表行的
    /// `updated_at_ms`（与 doc 单行同事务落的同一个钟）交给渲染层，头部多一行
    /// "核心记忆上次更新：X 天前"——让 agent 自己看见自己多久没写，不是替它写。
    /// `doc_version == 0`（注册了但从未写过）⇒ `None`，一行不加，不造假数据。
    #[allow(clippy::too_many_arguments)]
    pub fn brief_with(
        &self,
        agent_id: &str,
        user_id: &str,
        scene: &str,
        budget: usize,
        session_key: Option<&str>,
        include: &[&str],
        now_ms: i64,
    ) -> Result<Brief, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let last_write_at_ms = self
            .registration(agent_id)?
            .filter(|reg| reg.doc_version > 0)
            .map(|reg| reg.updated_at_ms);
        let payload = match self.load_doc(agent_id)? {
            Some(payload) => payload,
            None => CorePayload::empty(agent_id),
        };
        // 小本本索引栏（W36 细案 §4）：行读出来交给渲染；有行 ⇒ 载荷不算空（见 render）。
        let notebook = self.notebook_index(agent_id)?;
        let brief = render_brief_with_notebook(
            &payload,
            user_id,
            scene,
            budget,
            include,
            &notebook,
            last_write_at_ms,
            now_ms,
        );
        let section_tokens_json = serde_json::to_string(&brief.section_tokens)
            .map_err(|e| CoreError::BadPayload(e.to_string()))?;
        let truncated_json = serde_json::to_string(&brief.truncated)
            .map_err(|e| CoreError::BadPayload(e.to_string()))?;
        self.log_injection(
            agent_id,
            user_id,
            session_key,
            brief.token_estimate,
            &section_tokens_json,
            None,
            &truncated_json,
            now_ms,
        )?;
        Ok(brief)
    }

    /// key 文件所在根目录（`<data_dir>/keys/` 的父目录）。
    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 同 crate 操作层（ops.rs 的小本本四操作，W36）用：直接走本连接的 SQL。
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 已注册 agent 的小本本表名（派生后过 [`is_valid_core_table`] 闸）。
    /// 未注册 ⇒ `Unregistered`——「没身份的人不该知道表在不在」的同一道闸。
    pub(crate) fn note_table_for(&self, agent_id: &str) -> Result<String, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        self.registration(agent_id)?.ok_or(CoreError::Unregistered)?;
        Ok(checked_table(&note_table_name(agent_id))?.to_owned())
    }

    /// `core_rev_<h>` 里某一版的 `(nonce, ciphertext)`；`section='doc'` 才是整份载荷。
    /// 未注册 ⇒ `Unregistered`；该版本不存在 ⇒ `None`。
    pub(crate) fn revision_row(
        &self,
        agent_id: &str,
        version: u32,
    ) -> Result<Option<RevisionCiphertext>, CoreError> {
        let agent_id = checked_agent_id(agent_id)?;
        let reg = self.registration(agent_id)?.ok_or(CoreError::Unregistered)?;
        let rev_table = checked_table(&reg.rev_table)?;
        let row = self
            .conn
            .prepare(&format!(
                "SELECT nonce, ciphertext FROM \"{rev_table}\" \
                 WHERE version=?1 AND section='doc'"
            ))?
            .query_row([i64::from(version)], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .optional()?;
        Ok(row)
    }

    fn registration(&self, agent_id: &str) -> Result<Option<Registration>, CoreError> {
        registration_on(&self.conn, agent_id)
    }
}

/// 注册表投影。读连接与事务都用它：`save_doc` 必须在**同一个写事务里**读版本，
/// 所以这里只收一个 `&Connection`（`Transaction` 解引用即连接）。
fn registration_on(conn: &Connection, agent_id: &str) -> Result<Option<Registration>, CoreError> {
    let reg = conn
        .prepare(
            "SELECT table_name, rev_table_name, doc_version, updated_at_ms FROM core_agents \
             WHERE agent_id=?1",
        )?
        .query_row([agent_id], |r| {
            Ok(Registration {
                doc_table: r.get(0)?,
                rev_table: r.get(1)?,
                doc_version: r.get(2)?,
                updated_at_ms: r.get(3)?,
            })
        })
        .optional()?;
    if let Some(reg) = &reg {
        checked_table(&reg.doc_table)?;
        checked_table(&reg.rev_table)?;
    }
    Ok(reg)
}

/// agent 身份的闸：`safe_token`（非空、≤64、`[A-Za-z0-9_\-:.]`）且原样返回。
/// 直接拒绝会让 `safe_token` 的占位串混进表名哈希，所以比较原值。
pub(crate) fn checked_agent_id(agent_id: &str) -> Result<&str, CoreError> {
    if rc_contract::safe_token(agent_id) != agent_id {
        return Err(CoreError::BadAgentId);
    }
    Ok(agent_id)
}

fn checked_table(name: &str) -> Result<&str, CoreError> {
    if is_valid_core_table(name) {
        Ok(name)
    } else {
        Err(CoreError::BadTableName)
    }
}

fn render(ddl: &str, table: &str) -> String {
    ddl.replace("{doc}", table)
        .replace("{rev}", table)
        .replace("{note}", table)
}
