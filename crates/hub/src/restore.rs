//! 按清单恢复：校验各备份段 → 替换转录层 → 整替记忆库（含判定族）→ 整替账本
//! （W28：清单带 ledger 段时）→ 重建索引 → 把蒸馏水位设回清单里的停点 →
//! 私房本文件替换（W28：清单带 core 段时）→ 记数/审计。
//!
//! **不归零**：全量重蒸在 LLM 非确定性下会产出不同 id，旧判定族的 `attestation` /
//! `retirement` 会指向不存在的行。记忆库带自己的 id 回来，水位必须停在备份点；
//! 下一轮 cycle 因此只处理备份点**之后**的 turn。
//!
//! W28 的两段：清单**没有** ledger/core 段（本工单之前产的旧备份）时两段都跳过——
//! 旧备份照旧恢复，活库的账本/私房本一个字节不被碰。有段时校验（版本门/行数/摘要）
//! 与既有两件同构；core.db 用**文件替换**而不是 ATTACH 整替——`core_note_<h>` 的
//! append-only 触发器会拦 DELETE，而 restore 进程里没有任何打开的 core 连接
//! （`Root` 不含 core 柄），整文件快照回放语义最直白。
//!
//! 本模块不打印任何东西：输出面只在 CLI，错误串也不得带路径。

use crate::backup::{
    digest_of_tables, extra_stored_name, memory_rows_of, present_memory_tables, present_tables_of,
    sum_rows_of, BackupEncryption, BackupManifest,
};
use rc_ledger::water::{self, Cursor};
use rc_memory::schema::MEMORY_TABLES;
use rc_secret::backup::{decrypt_file, BackupKey};
use rc_store::{MemoryHandle, Root, StoreError, TranscriptHandle};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 校验读数：清单说的转录行数、记忆行数与随组的配置/密钥箱份数。
///
/// `extras` 是**校验通过的份数**；`extras_restored` / `extras_skipped` 才是"写没写回"，
/// 两者之和等于 `extras`。清单是未签名的普通 JSON，所以"校验通过"不等于"该写"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreCheck {
    pub turns: i64,
    pub memory_rows: i64,
    /// 账本段的行数读数（清单没带 ledger 段 ⇒ 0；恢复侧从副本重算核对后如实回报）。
    pub ledger_rows: i64,
    /// 私房本段的注册数读数（清单没带 core 段或 `core: null` ⇒ `None`）。
    pub core_agents: Option<i64>,
    pub extras: usize,
    pub extras_restored: usize,
    pub extras_skipped: usize,
}

/// 校验两份备份与清单。**同一条判据同时服务无确认与有确认两条路径**——
/// 有确认那一步再调一次，不是"确认之后就信清单"。
pub fn check(manifest: &Path) -> Result<RestoreCheck, StoreError> {
    check_with_key(manifest, None)
}

pub fn check_with_key(manifest: &Path, key: Option<&BackupKey>) -> Result<RestoreCheck, StoreError> {
    let doc = read_manifest(manifest)?;
    let checked = check_doc(manifest, &doc, key)?;
    Ok(checked.check)
}

fn read_manifest(manifest: &Path) -> Result<BackupManifest, StoreError> {
    let bytes = std::fs::read(manifest)?;
    serde_json::from_slice(&bytes).map_err(|_| {
        StoreError::Io("the backup manifest is unreadable".to_string())
    })
}

struct CheckedDoc {
    check: RestoreCheck,
    /// 解出来的临时文件靠它活着；drop 之后 extras/transcript 路径就失效。
    _temp: Option<TempDir>,
    transcript: PathBuf,
    memory: PathBuf,
    ledger: Option<PathBuf>,
    core: Option<PathBuf>,
    extras: Vec<(String, PathBuf)>,
}

fn check_doc(
    manifest: &Path,
    doc: &BackupManifest,
    key: Option<&BackupKey>,
) -> Result<CheckedDoc, StoreError> {
    if doc.format_version != crate::backup::MANIFEST_FORMAT_VERSION {
        return Err(StoreError::Io(
            "the backup manifest has an unknown format".to_string(),
        ));
    }
    // 生产侧只在加密备份里收集 extras（`backup.rs` 的 `collect_extras` 调用点）。
    // 明文清单带 extras ⇒ 那不是本工具写的清单，直接拒绝：否则"备份目录可写"就等于
    // "能覆盖恢复目标的 llm.toml / 密钥箱"，而明文清单连解密这一道成本都没有。
    if doc.encryption.is_none() && !doc.extras.is_empty() {
        return Err(StoreError::Io(
            "a plaintext backup manifest must not carry extras".to_string(),
        ));
    }
    let dir = manifest.parent().unwrap_or_else(|| Path::new(""));
    let temp = if doc.encryption.is_some() {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let base = temp.as_ref().map(|t| t.path()).unwrap_or(dir);
    let transcript = materialize(dir, base, &doc.transcript, doc.encryption.as_ref(), key, "transcript.db")?;
    let memory = materialize(dir, base, &doc.memory, doc.encryption.as_ref(), key, "memory.db")?;
    let turns = check_transcript(&transcript)?;
    let memory_rows = check_memory(&memory, doc)?;
    // W28 两段：清单带了才校验。旧清单（缺格）与新清单的 `core: null` 在这里同形跳过。
    let ledger = match &doc.ledger {
        Some(segment) => {
            let path = materialize(
                dir,
                base,
                &segment.file,
                doc.encryption.as_ref(),
                key,
                "ledger.db",
            )?;
            check_ledger(&path, doc)?;
            Some(path)
        }
        None => None,
    };
    let core = match &doc.core {
        Some(segment) => {
            let path = materialize(
                dir,
                base,
                &segment.file,
                doc.encryption.as_ref(),
                key,
                "core.db",
            )?;
            check_core(&path, doc)?;
            Some(path)
        }
        None => None,
    };
    let extras = materialize_extras(dir, base, doc, key)?;
    Ok(CheckedDoc {
        check: RestoreCheck {
            turns,
            memory_rows,
            ledger_rows: doc.ledger.as_ref().map(|l| l.rows).unwrap_or(0),
            core_agents: doc.core.as_ref().map(|c| c.agents),
            extras: extras.len(),
            // 校验本身不写任何东西；写没写由 `restore_with_options` 按开关填。
            extras_restored: 0,
            extras_skipped: 0,
        },
        _temp: temp,
        transcript,
        memory,
        ledger,
        core,
        extras,
    })
}

fn materialize_extras(
    dir: &Path,
    base: &Path,
    doc: &BackupManifest,
    key: Option<&BackupKey>,
) -> Result<Vec<(String, PathBuf)>, StoreError> {
    let mut extras = Vec::with_capacity(doc.extras.len());
    for (i, extra) in doc.extras.iter().enumerate() {
        if !plain_file_name(&extra.source) || !plain_file_name(&extra.stored) {
            return Err(StoreError::Io(
                "a backup extra names a file outside its directory".to_string(),
            ));
        }
        // 恢复目标只接受三类名字，且 `hub.key`/`FENCED`/`REQUIRE_EPOCH` 明确拒绝——
        // 这三个不靠"后缀不在白名单里"这一条意外兜住，免得将来放宽后缀就把它们放进来。
        if !restorable_source(&extra.source) {
            return Err(StoreError::Io(
                "a backup extra has an unsafe restore name".to_string(),
            ));
        }
        // `stored` 必须是**本组**的名字：清单不能把本组指向别组的密文或目录里别的文件。
        if extra.stored != extra_stored_name(doc.created_at_ms, &extra.source) {
            return Err(StoreError::Io(
                "a backup extra is not stored under this backup set".to_string(),
            ));
        }
        let path = materialize(
            dir,
            base,
            &extra.stored,
            doc.encryption.as_ref(),
            key,
            &format!("extra-{i}"),
        )?;
        let bytes = std::fs::read(&path)?;
        if bytes.len() as u64 != extra.bytes
            || rc_net::protocol::sha256_hex(&bytes) != extra.sha256
        {
            return Err(StoreError::Io(
                "a backup extra does not check out".to_string(),
            ));
        }
        extras.push((extra.source.clone(), path));
    }
    Ok(extras)
}

fn materialize(
    dir: &Path,
    base: &Path,
    name: &str,
    encryption: Option<&BackupEncryption>,
    key: Option<&BackupKey>,
    plain_name: &str,
) -> Result<PathBuf, StoreError> {
    let source = checked_path(dir, name)?;
    if encryption.is_none() {
        return Ok(source);
    }
    let Some(key) = key else {
        return Err(StoreError::Io("this backup needs a backup key".to_string()));
    };
    let dest = base.join(plain_name);
    decrypt_file(&source, &dest, key)
        .map_err(|_| StoreError::Io("the backup file cannot be decrypted".to_string()))?;
    Ok(dest)
}

/// 清单里的文件名只能是**纯文件名**：`file_name()` 与原文相等，且不含任何分隔符。
/// 拒绝 `..`、绝对路径与 `a/b`——清单被人改过时，`join` 不许把恢复引到别的目录。
fn checked_path(dir: &Path, raw: &str) -> Result<PathBuf, StoreError> {
    if !plain_file_name(raw) {
        return Err(StoreError::Io(
            "the backup manifest names a file outside its directory".to_string(),
        ));
    }
    let path = dir.join(raw);
    if !path.is_file() {
        return Err(StoreError::Io(
            "a backup file named by the manifest is not there".to_string(),
        ));
    }
    Ok(path)
}

fn plain_file_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw != "."
        && raw != ".."
        && !raw.contains('/')
        && !raw.contains('\\')
        && !raw.contains('\0')
        && Path::new(raw).file_name().and_then(|n| n.to_str()) == Some(raw)
}

fn open_read_only(path: &Path) -> Result<rusqlite::Connection, StoreError> {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| StoreError::Sql(e.to_string()))
}

/// 备份侧的转录版本门。**只接受当前版本与 v4**（S1b-C1）：v5 只给 `sessions` 加了 7 个
/// 可空归属列，旧备份的 6 列形状可以就地补默认值（见 `overwrite_transcript`），
/// 所以 v4 备份仍然可恢复；更旧/更新的版本一律拒绝，理由不变（形状不可知）。
fn check_transcript(path: &Path) -> Result<i64, StoreError> {
    /// S1b-C1 之前产出的转录备份：`sessions` 只有 6 列、没有归属索引。
    const LEGACY_TRANSCRIPT_VERSION: i32 = 4;
    let conn = open_read_only(path)?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != rc_store::ddl::TRANSCRIPT_VERSION && version != LEGACY_TRANSCRIPT_VERSION {
        return Err(StoreError::Io(
            "the transcript backup schema is not the current one".to_string(),
        ));
    }
    Ok(conn.query_row("SELECT count(*) FROM turns", [], |r| r.get(0))?)
}

fn check_memory(path: &Path, doc: &BackupManifest) -> Result<i64, StoreError> {
    let conn = open_read_only(path)?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != rc_store::ddl::MEMORY_VERSION {
        return Err(StoreError::Io(
            "the memory backup schema is not the current one".to_string(),
        ));
    }
    // S1b-C2a：摘要/行数只对备份**实际存在**的表算（旧 25 表备份缺两张归属侧表）。
    // `logical_digest` 逐表独立编码、按表名排序，所以同 25 表算出的字节与旧清单完全一致；
    // 反过来，少一张旧表的坏备份会算出不同摘要 ⇒ 被拒。
    let present = present_memory_tables(&conn)?;
    if present.is_empty() {
        return Err(StoreError::Io(
            "the memory backup schema is not the current one".to_string(),
        ));
    }
    let digest = rc_store::snapshot::logical_digest(&conn, &present)?;
    if digest != doc.memory_digest {
        return Err(StoreError::Io(
            "the memory backup does not match the manifest digest".to_string(),
        ));
    }
    let rows = memory_rows_of(&conn, &present)?;
    if rows != doc.memory_rows {
        return Err(StoreError::Io(
            "the memory backup does not match the manifest row count".to_string(),
        ));
    }
    Ok(rows)
}

/// 备份侧的账本版本门 + present-only 行数/摘要核对。
///
/// 只接受当前账本版本：带 ledger 段的备份是 W28 之后产的，当时 `LEDGER_VERSION` 已是
/// 7；没有"旧版本 ledger 段"这种东西，形状不可知的版本照旧拒绝。行数与摘要都从
/// **副本**按 present-only 集合重算（与生产侧同一算法）——清单是未签名 JSON，
/// "这份账本是不是清单说的那一份"必须可验证。
fn check_ledger(path: &Path, doc: &BackupManifest) -> Result<i64, StoreError> {
    let segment = doc
        .ledger
        .as_ref()
        .ok_or_else(|| StoreError::Io("the manifest has no ledger segment".to_string()))?;
    let conn = open_read_only(path)?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != rc_store::ddl::LEDGER_VERSION {
        return Err(StoreError::Io(
            "the ledger backup schema is not the current one".to_string(),
        ));
    }
    let tables = present_tables_of(&conn)?;
    if tables.is_empty() {
        return Err(StoreError::Io(
            "the ledger backup schema is not the current one".to_string(),
        ));
    }
    let digest = digest_of_tables(&conn, &tables)?;
    if digest != segment.digest {
        return Err(StoreError::Io(
            "the ledger backup does not match the manifest digest".to_string(),
        ));
    }
    let rows = sum_rows_of(&conn, &tables)?;
    if rows != segment.rows {
        return Err(StoreError::Io(
            "the ledger backup does not match the manifest row count".to_string(),
        ));
    }
    Ok(rows)
}

/// 备份侧的私房本核对。core.db **没有** `user_version`（rc-core 不立版本），
/// 内容身份完全靠 agents 读数 + 全表摘要钉；摘要按 present-only 集合算，
/// 存量孤表（`core_sync_state` 一族）因此是合法状态，不会把旧库的备份拒掉。
fn check_core(path: &Path, doc: &BackupManifest) -> Result<i64, StoreError> {
    let segment = doc
        .core
        .as_ref()
        .ok_or_else(|| StoreError::Io("the manifest has no core segment".to_string()))?;
    let conn = open_read_only(path)?;
    let agents: i64 = conn
        .query_row("SELECT count(*) FROM core_agents", [], |r| r.get(0))
        .map_err(|_| {
            StoreError::Io("the core backup schema is not the current one".to_string())
        })?;
    if agents != segment.agents {
        return Err(StoreError::Io(
            "the core backup does not match the manifest agent count".to_string(),
        ));
    }
    let tables = present_tables_of(&conn)?;
    let digest = digest_of_tables(&conn, &tables)?;
    if digest != segment.digest {
        return Err(StoreError::Io(
            "the core backup does not match the manifest digest".to_string(),
        ));
    }
    Ok(agents)
}

/// 校验、替换两份备份、按清单设蒸馏停点，再记数/审计。
///
/// 顺序是判定的一部分：记忆库回填（含判定族）与水位设置都必须在任何 `cycle` 之前；
/// 索引重建排在记忆库替换之后，否则影子表指向的是被删掉的那一批 id。
pub fn restore(
    root: &mut Root,
    manifest: &Path,
    now_ms: i64,
) -> Result<RestoreCheck, StoreError> {
    restore_with_options(root, manifest, now_ms, None, true)
}

/// 带 backup key 的恢复。**保留写回 extras 的旧语义**：这一层是库内调用方显式选择的行为，
/// 操作员面的开关在 CLI（`restore --with-extras`）——那里默认**不写**。
pub fn restore_with_key(
    root: &mut Root,
    manifest: &Path,
    now_ms: i64,
    key: Option<&BackupKey>,
) -> Result<RestoreCheck, StoreError> {
    restore_with_options(root, manifest, now_ms, key, true)
}

/// `write_extras` 是安全边界上的那一格：清单未签名，"校验通过"只证明这份密文与清单自洽，
/// 不证明它是操作员要的那份配置。于是 extras 始终**校验**（解密、sha256、字节数），
/// 但只有显式要求时才落回 `data_dir`；不写时 `extras_skipped` 如实回报份数。
pub fn restore_with_options(
    root: &mut Root,
    manifest: &Path,
    now_ms: i64,
    key: Option<&BackupKey>,
    write_extras: bool,
) -> Result<RestoreCheck, StoreError> {
    let doc = read_manifest(manifest)?;
    let mut checked = check_doc(manifest, &doc, key)?;
    overwrite_transcript(root.transcript(), &checked.transcript)?;
    overwrite_memory(root.memory(), &checked.memory)?;
    rc_index::rebuild(root.memory(), now_ms).map_err(|_| {
        StoreError::Sql("the memory index could not be rebuilt".to_string())
    })?;
    // W38a 刀 B（工单 4）：转录影子层随记忆索引同刻重建——刀 A 之前的旧备份没有
    // turn_shadow 行，恢复后 turns 在而影子空，检索面只剩 coverage 缺口。
    rc_ledger::rebuild_turn_shadow(root.transcript()).map_err(|_| {
        StoreError::Sql("the transcript shadow index could not be rebuilt".to_string())
    })?;
    // W28：账本整替（清单带 ledger 段时）。**必须排在水位设置之前**——
    // `water::replace_distill_cursors` 写的就是账本的 `job_watermark` 表，
    // 顺序反了刚设好的停点会被整替抹掉。旧清单没有这一段 ⇒ 跳过，活库账本原封。
    if let Some(path) = &checked.ledger {
        overwrite_ledger(root.ledger(), path)?;
    }
    let cursors: Vec<(String, Cursor)> = doc
        .distill_watermarks
        .iter()
        .map(|w| {
            (
                w.scope.clone(),
                Cursor {
                    at_ms: w.at_ms,
                    turn_uid: w.turn_uid.clone(),
                },
            )
        })
        .collect();
    water::replace_distill_cursors(root.ledger(), &cursors, now_ms)?;
    // W28：私房本文件替换（清单带 core 段时）。core.db 是懒库快照，整文件回放；
    // 它不写任何"恢复后必须重设"的状态，排在账本整替之后、extras 之前没有顺序耦合。
    if let Some(path) = &checked.core {
        replace_core_file(root.data_dir(), path)?;
    }
    if write_extras {
        restore_extras(root.data_dir(), &checked.extras)?;
        checked.check.extras_restored = checked.check.extras;
    } else {
        checked.check.extras_skipped = checked.check.extras;
    }
    rc_ledger::counters::bump(
        root.ledger_mut(),
        "maintenance",
        "restore",
        "restored_turns",
        checked.check.turns,
        now_ms,
    )?;
    rc_ledger::device::append_audit(
        root.ledger_mut(),
        now_ms,
        "operator",
        "restore",
        None,
        "restored",
    )?;
    Ok(checked.check)
}

/// 恢复目标里的合法名字：`llm.toml`、`*.enc`、`*.pass`。
///
/// `hub.key` / `FENCED` / `REQUIRE_EPOCH` 本来就不在后缀白名单里，这里**显式**再拒一次：
/// 这三样一旦被 extras 覆盖，恢复出来的就是一个"钥匙或门禁被别人换过"的中心。
fn restorable_source(source: &str) -> bool {
    !matches!(source, "hub.key" | "FENCED" | "REQUIRE_EPOCH")
        && (source == "llm.toml" || source.ends_with(".enc") || source.ends_with(".pass"))
}

/// 把随组的 LLM 配置/密钥箱落回 data_dir。只接受恢复目标里的三类名字：
/// `llm.toml`、`*.enc`、`*.pass`——清单被人改过时，不能借 extras 覆盖 `hub.key`/`FENCED`。
fn restore_extras(data_dir: &Path, extras: &[(String, PathBuf)]) -> Result<(), StoreError> {
    for (source, path) in extras {
        if !plain_file_name(source) || !restorable_source(source) {
            return Err(StoreError::Io(
                "a backup extra has an unsafe restore name".to_string(),
            ));
        }
        let bytes = std::fs::read(path)?;
        rc_secret::write_private_file(&data_dir.join(source), &bytes)
            .map_err(|_| StoreError::Io("a backup extra could not be restored".to_string()))?;
    }
    Ok(())
}

/// 覆盖转录层：同一条连接上的 `ATTACH` + 两表整替，**不是文件替换**。
/// Windows 上被打开的库文件不能被替换，而事务里的删除/插入失败会整体回滚；
/// 备份文件始终只读。`ATTACH` 的文件名用 `?1` 绑定（本机实测可用），不拼 SQL。
fn overwrite_transcript(t: &TranscriptHandle, backup: &Path) -> Result<(), StoreError> {
    let path = backup
        .to_str()
        .ok_or_else(|| StoreError::Io("the backup path is not valid UTF-8".to_string()))?;
    let conn = t.conn();
    conn.execute("ATTACH DATABASE ?1 AS backup", [path])?;
    // S1b-C1：`sessions` 的列数随备份版本变（v4 = 6 列、v5 = 13 列），所以这里不再
    // `SELECT *`，而是**显式列名 + 形状自证**：不看 `user_version`，而是问 attached
    // 库里到底有哪几列——只把 user_version 改成 4 的"假旧库"与真 v4 备份必须分得开。
    let ownership_columns: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('sessions', 'backup')
         WHERE name IN ('owner_principal_id','owner_agent_id','scope_kind','scope_key',
                        'visibility','retention','ownership_src')",
        [],
        |r| r.get(0),
    )?;
    let has_ownership = ownership_columns == 7;
    let tx = conn.unchecked_transaction()?;
    // 恢复出来的是中心态：备份侧 apply 的游标/去重表不属于恢复结果。
    tx.execute_batch(
        "DROP TABLE IF EXISTS replication_state;
         DROP TABLE IF EXISTS replicated_commit;",
    )?;
    tx.execute("DELETE FROM ingest_log", [])?;
    tx.execute("DELETE FROM tombstone", [])?;
    tx.execute("DELETE FROM center_state", [])?;
    // 回执的发送者是备份自己：恢复出来的是另一个中心，旧回执不能跟着复活。
    tx.execute("DELETE FROM replica_ack", [])?;
    tx.execute("DELETE FROM turns", [])?;
    tx.execute("DELETE FROM sessions", [])?;
    if has_ownership {
        tx.execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                                  last_saved_at_ms, owner_principal_id, owner_agent_id,
                                  scope_kind, scope_key, visibility, retention, ownership_src)
             SELECT session_key, device_id, source_agent, machine, project_hint,
                    last_saved_at_ms, owner_principal_id, owner_agent_id,
                    scope_kind, scope_key, visibility, retention, ownership_src
             FROM backup.sessions",
            [],
        )?;
    } else {
        // v4 备份没有归属：按设计 §1.2 的"未知 ⇒ 最保守"补字面量。
        // `ownership_src='legacy'` 让运维分得清"迁移补的"与"v3 适配器写的"。
        tx.execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                                  last_saved_at_ms, owner_principal_id, owner_agent_id,
                                  scope_kind, scope_key, visibility, retention, ownership_src)
             SELECT session_key, device_id, source_agent, machine, project_hint,
                    last_saved_at_ms,
                    NULL, source_agent, 'project', COALESCE(project_hint, session_key),
                    'private_agent', 'default', 'legacy'
             FROM backup.sessions",
            [],
        )?;
    }
    tx.execute("INSERT INTO turns SELECT * FROM backup.turns", [])?;
    tx.execute("INSERT INTO center_state SELECT * FROM backup.center_state", [])?;
    tx.execute("INSERT INTO ingest_log SELECT * FROM backup.ingest_log", [])?;
    tx.execute("INSERT INTO tombstone SELECT * FROM backup.tombstone", [])?;
    tx.commit()?;
    // 提交之后脱离：DETACH 不能在事务里跑。脱离失败不影响已提交的数据，
    // 而且进程随后退出，连接跟着消失。
    let _ = conn.execute("DETACH DATABASE backup", []);
    Ok(())
}

/// 覆盖记忆库：`MEMORY_TABLES` **逐张全替**（含判定族），一张不落。
///
/// 删除对当前库的全部 `MEMORY_TABLES` 执行（恢复是整替）；插入则逐表先看备份里有没有
/// 这张表——旧 25 表备份缺 `resource_owner`/`resource_grant`，跳过它们、让活库里的表
/// 保持空（活库的表由 `Root::open` 的 `MEMORY_SQL` 以 `IF NOT EXISTS` 保证存在）。缺**旧**表仍然不可能
/// 走到这里：`check_memory` 的摘要/行数已经在前面把它拒了。
///
/// 顺序直接取 `MEMORY_TABLES`：它按字典序，`alias` 因此在 `anchor` 之后；删除倒着走。
/// `defer_foreign_keys` 让外键检查推到提交时，于是"整替"这件事不依赖行插入顺序——
/// 顺序只是为了让中间态也尽量可读。
pub(crate) fn overwrite_memory(m: &MemoryHandle, backup: &Path) -> Result<(), StoreError> {
    let path = backup
        .to_str()
        .ok_or_else(|| StoreError::Io("the backup path is not valid UTF-8".to_string()))?;
    let conn = m.conn();
    conn.execute("ATTACH DATABASE ?1 AS backup_mem", [path])?;
    let mut present: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    {
        let mut st = conn
            .prepare("SELECT name FROM backup_mem.sqlite_master WHERE type='table'")?;
        let mut rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for name in &mut rows {
            present.insert(name?);
        }
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys=ON")?;
    for table in MEMORY_TABLES.iter().rev() {
        tx.execute(&format!("DELETE FROM \"{table}\""), [])?;
    }
    for table in MEMORY_TABLES {
        if !present.contains(*table) {
            continue;
        }
        tx.execute(
            &format!("INSERT INTO \"{table}\" SELECT * FROM backup_mem.\"{table}\""),
            [],
        )?;
    }
    tx.commit()?;
    // 与转录层同一个理由：DETACH 在事务外，失败不影响已提交的数据。
    let _ = conn.execute("DETACH DATABASE backup_mem", []);
    Ok(())
}

/// 覆盖账本：与记忆库同一条 present-only 整替——备份里有什么表就全替什么表，
/// 活库的表由 `Root::open` 的 `LEDGER_SQL` 以 `IF NOT EXISTS` 保证存在。ledger 的
/// 表集合随版本漂移且没有静态清单，按备份现读的表名走（`sqlite_%` 内部表除外）。
///
/// ledger 没有 `MEMORY_TABLES` 那份字典序清单，表名直接从 `backup.sqlite_master`
/// 取，**拼进 SQL 前一律过 `safe_table_name`**：名字来自备份文件（清单未签名），
/// 引号转义之外还必须是不含分隔符的普通标识符。
fn overwrite_ledger(l: &rc_store::LedgerHandle, backup: &Path) -> Result<(), StoreError> {
    let path = backup
        .to_str()
        .ok_or_else(|| StoreError::Io("the backup path is not valid UTF-8".to_string()))?;
    let conn = l.conn();
    conn.execute("ATTACH DATABASE ?1 AS backup_ledger", [path])?;
    let mut tables: Vec<String> = Vec::new();
    {
        let mut st = conn.prepare(
            "SELECT name FROM backup_ledger.sqlite_master
             WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let mut rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for name in &mut rows {
            let name = name?;
            if !safe_table_name(&name) {
                let _ = conn.execute("DETACH DATABASE backup_ledger", []);
                return Err(StoreError::Io(
                    "the ledger backup names a table that cannot be restored".to_string(),
                ));
            }
            tables.push(name);
        }
    }
    let tx = conn.unchecked_transaction()?;
    for table in &tables {
        tx.execute(&format!("DELETE FROM \"{table}\""), [])?;
    }
    for table in &tables {
        tx.execute(
            &format!("INSERT INTO \"{table}\" SELECT * FROM backup_ledger.\"{table}\""),
            [],
        )?;
    }
    tx.commit()?;
    let _ = conn.execute("DETACH DATABASE backup_ledger", []);
    Ok(())
}

/// 把私房本备份**整文件替换**回 `<data_dir>/core.db`。
///
/// 为什么不是 ATTACH 整替：`core_note_<h>` 的 append-only 是两个触发器
/// （`BEFORE UPDATE/DELETE … RAISE(ABORT)`），整替路径的 `DELETE` 会被它拦下；
/// 拆触发器等于在本函数里复刻 `CoreStore` 的 DDL。而 restore 进程里没有任何
/// 打开的 core 连接（`Root` 不含 core 柄），文件替换可行且语义最直白：快照回放。
/// 旧库的 `-wal`/`-shm` 必须随主文件一起走——WAL 配错主文件是把旧页叠上新库，
/// 比"没恢复"更坏。
fn replace_core_file(data_dir: &Path, backup: &Path) -> Result<(), StoreError> {
    let target = data_dir.join("core.db");
    for side in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(data_dir.join(format!("core.db{side}")));
    }
    // 复制不移动：备份原件归轮转管，恢复侧一个字节都不消耗它。
    std::fs::copy(backup, &target).map_err(|_| {
        StoreError::Io("the core backup could not be restored".to_string())
    })?;
    Ok(())
}

/// ledger 整替的表名闸：纯标识符（字母/数字/下划线），且不是 SQLite 内部名。
/// 备份文件是外来的（清单未签名），表名拼进 SQL 前在这里收口。
fn safe_table_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with("sqlite_")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
