//! 每日备份：**转录层 + 记忆库 + 账本 + agent 私房本（存在才备）+ 一份清单**。
//!
//! W28 备份段拆分（2026-11-07 备份审计 + core.db 定位裁定）：备份按
//! **「可复制段 / 不可复制段」**分——
//! - **库文件可复制**：`ledger.db`（账本：pass 状态/水位/设备/审查队列/审计）**必备**；
//!   `core.db`（agent 私房本，按 agent 建表的懒库）**存在才备**——缺席是合法状态，
//!   清单/报告如实记 `core: null`，不造假文件。
//! - **身份密钥永不进组**：`keys/core_<h>.key` 一类身份文件一个字节都不进备份集。
//!   裁定原文：**「身份类密钥不下发的既有裁定，多一份静态拷贝 = 多一类出机器面」**。
//!   库文件进组是"可复制段"，密钥文件是"身份"——两回事，见 `collect_extras` 的扫描面。
//!
//! 记忆库另有快照那条路，但快照是"下行给节点的副本"，不是"中心自己回到某一天"的把手；
//! 账本另有复制面（`/v1/replicate/ledger` 把设备/能力/审计送给 standby），但那是复制
//! 不是本地的冷备——本文件产出的组里 ledger.db 是第一份**本地**快照。
//!
//! 7 **组**轮转：留最近 7 组（transcript + memory + ledger + core(可选) + manifest 五件
//! 一套），旧的整组删。**文件名不含设备名、不含路径**——`restore` 不许打印备份文件的
//! 完整路径，而文件名会被人读成"这是谁的备份"。

use rc_store::{Root, StoreError};
use rc_secret::backup::{encrypt_file, BackupKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 保留组数。**7 是立值**：一周。
pub const RETAIN_BACKUPS: usize = 7;

/// 清单自身的形状版本（不是转录层/记忆库的 schema 版本）。
pub const MANIFEST_FORMAT_VERSION: u32 = 1;
pub const BACKUP_ENCRYPTION_VERSION: u32 = 1;

pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

/// 转录备份文件名。**只有时间戳**，没有设备名、没有路径。
pub fn backup_name(now_ms: i64) -> String {
    format!("transcript-{now_ms}.db")
}

fn memory_name(now_ms: i64) -> String {
    format!("memory-{now_ms}.db")
}

/// 账本备份文件名。ledger.db 是**必备**段：备份组里没有它就是失败，没有"跳过"。
fn ledger_name(now_ms: i64) -> String {
    format!("ledger-{now_ms}.db")
}

/// agent 私房本备份文件名。core.db 是懒库：**存在才备**，缺席不产文件（`core: null`）。
fn core_name(now_ms: i64) -> String {
    format!("core-{now_ms}.db")
}

fn manifest_name(now_ms: i64) -> String {
    format!("backup-{now_ms}.json")
}

/// extras 在备份目录里的密文名：**只有本组时间戳 + source**，没有路径。
///
/// 生产侧与恢复侧共用这一个式子：清单不能把本组指向别组的密文、也不能指向目录里
/// 别的文件——`stored` 必须逐字等于 `extra_stored_name(清单的 created_at_ms, source)`。
pub fn extra_stored_name(now_ms: i64, source: &str) -> String {
    format!("extra-{now_ms}-{source}.enc")
}

/// 清单里的一条蒸馏水位。`scope` 是 `water::distill_scope` 的键形。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistillWatermark {
    pub scope: String,
    pub at_ms: i64,
    pub turn_uid: String,
}

/// 备份清单。字段就是 `restore` 校验与回填所需的全部内容身份：
/// `memory_digest` 是 `logical_digest`（方案原文的 `memory_seq` 在库里不存在），
/// `memory_rows` 与它一起把"这份记忆库是不是清单说的那一份"变成可验证的读数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format_version: u32,
    pub created_at_ms: i64,
    pub transcript: String,
    pub memory: String,
    pub transcript_max_ingested_at: Option<i64>,
    pub memory_rows: i64,
    pub memory_digest: String,
    pub memory_version: i32,
    #[serde(default)]
    pub distill_watermarks: Vec<DistillWatermark>,
    /// 加密备份才有；旧清单缺这一格 ⇒ 明文备份。
    #[serde(default)]
    pub encryption: Option<BackupEncryption>,
    /// 账本段（W28）：组内文件名 + 备份点的行读数与内容摘要。本工单之前产的清单
    /// 缺这一格 ⇒ restore 按旧备份处理（账本不在组里，照旧恢复其余三件）。
    #[serde(default)]
    pub ledger: Option<BackupLedger>,
    /// agent 私房本段（W28）。`None` 两义（行为相同，restore 都跳过）：
    /// 旧清单缺格；新清单里 `core.db` 合法缺席（懒库没建过）。core 备份**没有**
    /// `user_version`（rc-core 不立版本），内容身份靠 agents 读数 + 摘要钉。
    #[serde(default)]
    pub core: Option<BackupCore>,
    /// 随组走的 LLM 配置/密钥箱；明文备份为空。
    #[serde(default)]
    pub extras: Vec<BackupExtra>,
}

/// 清单里的账本段。读数与记忆库同构：行数 + 内容摘要，都能从副本重算——
/// "这份账本是不是清单说的那一份"是可验证的，不是只信清单自己写下的值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupLedger {
    pub file: String,
    /// 备份里实际存在的表（present-only，见 `present_tables_of`）的 `count(*)` 之和。
    pub rows: i64,
    pub digest: String,
}

/// 清单里的 agent 私房本段。`agents` 是 `core_agents` 的注册行数；
/// 摘要覆盖全部 present 表（含 per-agent 动态表，AEAD 密文行原样编入）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupCore {
    pub file: String,
    pub agents: i64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEncryption {
    pub format_version: u32,
    /// 16 位十六进制 key 指纹；不是 key 本身。
    pub key_id: String,
    pub cipher: String,
}

/// 随数据一起走的配置文件/密钥箱。**只在加密备份里出现**：
/// `source` 是恢复时落回 data_dir 的文件名，`stored` 是备份目录里的密文名。
/// 两者的 sha/字节都是**明文**读数，恢复时先解密再核对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupExtra {
    pub source: String,
    pub stored: String,
    pub sha256: String,
    pub bytes: u64,
}

/// 一次成功备份的读数。**都不是路径**：CLI 只印文件名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    pub transcript: String,
    pub memory: String,
    /// 账本备份文件名（必备段）。
    pub ledger: String,
    /// agent 私房本备份文件名；懒库缺席时为 `None`（清单里记 `core: null`）。
    pub core: Option<String>,
    pub memory_rows: i64,
    /// 随组走的配置/密钥箱份数（明文备份恒为 0）。
    pub extras: usize,
}

/// 记忆库 `MEMORY_TABLES` 的 `count(*)` 之和。
///
/// 表清单只从 `rc_memory::schema` 现读，不在这里抄第二份；单双引号只是 SQL 标识符的
/// 转义，表名都是编译期常量。**备份生产侧走这一条（严格）**：当前表清单里少一张就报错，
/// "跳过缺失表"只属于恢复侧的旧备份兼容（见 [`present_memory_tables`]）。
pub(crate) fn memory_rows(conn: &rusqlite::Connection) -> Result<i64, StoreError> {
    memory_rows_of(conn, rc_memory::schema::MEMORY_TABLES)
}

/// 给定表集的 `count(*)` 之和。恢复侧按 [`present_memory_tables`] 的 present-only 表集调用：
/// 旧备份缺的侧表因此不算进行数，与旧清单读数逐字一致。
pub(crate) fn memory_rows_of(
    conn: &rusqlite::Connection,
    tables: &[&str],
) -> Result<i64, StoreError> {
    let mut total = 0i64;
    for table in tables {
        let n: i64 = conn.query_row(
            &format!("SELECT count(*) FROM \"{table}\""),
            [],
            |r| r.get(0),
        )?;
        total += n;
    }
    Ok(total)
}

/// 备份里**实际存在的** canonical 表，保持 `MEMORY_TABLES` 顺序。
///
/// 这是恢复侧唯一的 present-only 读数点：S1b-C2a 的两张归属侧表是后来才加进当前表清单的，
/// 产出于此前的 25 表记忆备份不含它们。摘要/行数只对 present 表算 ⇒ 旧清单照旧对得上；
/// 反之，旧表缺一张时 present 集合就少一张，摘要必然对不上（不是"缺什么都放过"）。
pub(crate) fn present_memory_tables(
    conn: &rusqlite::Connection,
) -> Result<Vec<&'static str>, StoreError> {
    let mut st = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let names: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rc_memory::schema::MEMORY_TABLES
        .iter()
        .copied()
        .filter(|table| names.iter().any(|n| n == table))
        .collect())
}

/// 给定库**实际存在的**用户表（排除 `sqlite_%` 内部表），名字排序。
///
/// ledger/core 没有像 `MEMORY_TABLES` 那样的一份静态表清单——两者的表集合随版本
/// 漂移（DDL 大量"不 bump 版本、`IF NOT EXISTS` 幂等补表"的切片），core 还带
/// per-agent 动态表与存量孤表（`core_sync_state` 的孤表是合法状态）。所以
/// ledger/core 的行数/摘要一律 present-only：备份里有什么表，就读数什么表，
/// 恢复侧对同一文件重算同一集合——自洽是可验证的，缺表会在摘要上现形。
pub(crate) fn present_tables_of(conn: &rusqlite::Connection) -> Result<Vec<String>, StoreError> {
    let mut st = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let mut names: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

/// 给定表集的 `count(*)` 之和（ledger/core 的 present-only 行数口径）。
pub(crate) fn sum_rows_of(
    conn: &rusqlite::Connection,
    tables: &[String],
) -> Result<i64, StoreError> {
    let mut total = 0i64;
    for table in tables {
        let n: i64 = conn.query_row(
            &format!("SELECT count(*) FROM \"{table}\""),
            [],
            |r| r.get(0),
        )?;
        total += n;
    }
    Ok(total)
}

/// 给定表集的逻辑内容摘要（`&[String]` → `logical_digest` 的 `&[&str]` 口径桥）。
pub(crate) fn digest_of_tables(
    conn: &rusqlite::Connection,
    tables: &[String],
) -> Result<String, StoreError> {
    let names: Vec<&str> = tables.iter().map(|s| s.as_str()).collect();
    rc_store::snapshot::logical_digest(conn, &names)
}

/// 做一次备份并置位成功读数。返回三件套的文件名（**不是路径**）。
///
/// 两次 `VACUUM INTO` 都落在同一个 `backups/`：转录层一份、记忆库一份。随后写清单。
/// 任一环失败 ⇒ 删掉**本次**已经落下的文件（目标已存在时一个字节都不动旧组），
/// 由调用方置失败时刻并退出 3。
pub fn backup(root: &mut Root, data_dir: &Path, now_ms: i64) -> Result<BackupReport, StoreError> {
    backup_with_options(
        root,
        data_dir,
        now_ms,
        BackupOptions {
            key: None,
            llm_config: None,
        },
    )
}

pub struct BackupOptions<'a> {
    pub key: Option<&'a BackupKey>,
    /// `llm.toml` 的路径；缺省时尝试 `<data_dir>/llm.toml`。
    /// 只在加密备份里随组走；明文备份不带配置/密钥。
    pub llm_config: Option<&'a Path>,
}

/// 加密备份：独立 backup key 不在 data_dir 内；明文 `VACUUM INTO` 文件随后被删，
/// 目录里只留 `.db.enc`。清单只记 key 指纹，不记 key。
pub fn backup_with_key(
    root: &mut Root,
    data_dir: &Path,
    now_ms: i64,
    key: Option<&BackupKey>,
) -> Result<BackupReport, StoreError> {
    backup_with_options(
        root,
        data_dir,
        now_ms,
        BackupOptions {
            key,
            llm_config: None,
        },
    )
}

pub fn backup_with_options(
    root: &mut Root,
    data_dir: &Path,
    now_ms: i64,
    options: BackupOptions<'_>,
) -> Result<BackupReport, StoreError> {
    let key = options.key;
    let dir = backups_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let transcript = backup_name(now_ms);
    let memory = memory_name(now_ms);
    let ledger = ledger_name(now_ms);
    let core = core_name(now_ms);
    let manifest = manifest_name(now_ms);
    // 同一时间戳的组已经存在 ⇒ 一个都不覆盖：`VACUUM INTO` 的"目标必须不存在"是硬要求，
    // 而先探测**五件**名字（transcript/memory/ledger/core/manifest）能让"失败的那一次
    // 不残留第二组"这件事与文件系统的竞态无关。
    for name in [&transcript, &memory, &ledger, &core, &manifest] {
        if dir.join(name).exists() {
            return Err(StoreError::Io(
                "a backup set with this timestamp already exists".to_string(),
            ));
        }
    }

    // core.db 是懒库：**存在才备**，缺席是合法状态（清单/报告如实记 `core: null`）。
    // 不能用 `CoreStore::open` 探存在性——它对着不存在的路径会顺手建库，正是
    // "不造假文件"要挡的那只手。这里只开只读连接：备份是文件级快照（`VACUUM INTO`），
    // 不读任何表内容，不碰密文、不拼动态表名——hub 不绕过 CoreStore 读 core 的载荷，
    // 而备份根本不需要读载荷。
    let core_src = data_dir.join("core.db");
    let core_conn = if core_src.is_file() {
        Some(rusqlite::Connection::open_with_flags(
            &core_src,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    } else {
        None
    };

    let transcript_dest = dir.join(&transcript);
    let memory_dest = dir.join(&memory);
    let ledger_dest = dir.join(&ledger);
    let core_dest = dir.join(&core);
    let manifest_dest = dir.join(&manifest);
    // 本次已经落组、失败必须带走的文件名（当前形态）。加密成功后整体换名。
    let mut built: Vec<String> = Vec::new();
    let wipe = |built: &[String]| {
        for name in built {
            let _ = std::fs::remove_file(dir.join(name));
        }
    };

    rc_store::snapshot::vacuum_into(root.transcript().conn(), &transcript_dest)?;
    built.push(transcript.clone());
    if let Err(e) = rc_store::snapshot::vacuum_into(root.memory().conn(), &memory_dest) {
        wipe(&built);
        return Err(e);
    }
    built.push(memory.clone());
    // 账本是必备段：vacuum 失败 ⇒ 整组失败（没有"跳过 ledger 继续"那一档）。
    if let Err(e) = rc_store::snapshot::vacuum_into(root.ledger().conn(), &ledger_dest) {
        wipe(&built);
        return Err(e);
    }
    built.push(ledger.clone());
    let has_core = core_conn.is_some();
    if let Some(conn) = &core_conn {
        if let Err(e) = rc_store::snapshot::vacuum_into(conn, &core_dest) {
            wipe(&built);
            return Err(e);
        }
        built.push(core.clone());
    }

    let mut transcript_name = transcript.clone();
    let mut memory_name = memory.clone();
    let mut ledger_name = ledger.clone();
    let mut core_name_out = has_core.then(|| core.clone());
    let mut encryption = None;
    if let Some(key) = key {
        // 加密变体**成组**：四个库文件各自成密文，任何一枚失败 ⇒ 删掉本次全部
        // 明文与已产密文，不残留半组。
        let enc_names: Vec<String> = built.iter().map(|n| format!("{n}.enc")).collect();
        let mut done: Vec<String> = Vec::new();
        let mut key_info = None;
        let mut enc_result = Ok(());
        for (plain, enc) in built.iter().zip(enc_names.iter()) {
            match encrypt_file(&dir.join(plain), &dir.join(enc), key) {
                Ok(info) => key_info = Some(info),
                Err(_) => {
                    enc_result = Err(());
                    break;
                }
            }
            done.push(enc.clone());
        }
        if enc_result.is_err() {
            for name in done {
                let _ = std::fs::remove_file(dir.join(&name));
            }
            wipe(&built);
            return Err(StoreError::Io("backup encryption failed".to_string()));
        }
        for name in &built {
            let _ = std::fs::remove_file(dir.join(name));
        }
        built = enc_names;
        transcript_name = format!("{transcript}.enc");
        memory_name = format!("{memory}.enc");
        ledger_name = format!("{ledger}.enc");
        core_name_out = has_core.then(|| format!("{core}.enc"));
        encryption = Some(BackupEncryption {
            format_version: BACKUP_ENCRYPTION_VERSION,
            key_id: key_info.expect("encryption succeeded but key id is missing").key_id,
            cipher: "xchacha20poly1305-chunked".into(),
        });
    }

    // 配置/密钥箱只在加密备份里随组走；明文备份一个字节都不带。
    let mut extras = Vec::new();
    if let Some(key) = key {
        match collect_extras(data_dir, &dir, now_ms, key, options.llm_config) {
            Ok(list) => extras = list,
            Err(e) => {
                wipe(&built);
                return Err(e);
            }
        }
    }

    let manifest_built = build_manifest(
        root,
        core_conn.as_ref(),
        now_ms,
        SegmentNames {
            transcript: transcript_name.clone(),
            memory: memory_name.clone(),
            ledger: ledger_name.clone(),
            core: core_name_out.clone(),
        },
        encryption,
        extras.clone(),
    );
    let manifest_doc = match manifest_built {
        Ok(doc) => doc,
        Err(e) => {
            wipe(&built);
            for extra in &extras {
                let _ = std::fs::remove_file(dir.join(&extra.stored));
            }
            return Err(e);
        }
    };
    if let Err(e) = write_manifest(&manifest_dest, &manifest_doc) {
        wipe(&built);
        for extra in &extras {
            let _ = std::fs::remove_file(dir.join(&extra.stored));
        }
        return Err(e);
    }
    if let Err(e) = retain(&dir, RETAIN_BACKUPS) {
        wipe(&built);
        for extra in &extras {
            let _ = std::fs::remove_file(dir.join(&extra.stored));
        }
        let _ = std::fs::remove_file(&manifest_dest);
        return Err(e);
    }
    mark_ok(root, now_ms)?;
    Ok(BackupReport {
        transcript: transcript_name,
        memory: memory_name,
        ledger: ledger_name,
        core: core_name_out,
        memory_rows: manifest_doc.memory_rows,
        extras: extras.len(),
    })
}

/// 把 LLM 配置与 data_dir 根下的密钥箱加密进本组。
///
/// W28 密钥裁定原文：**「身份类密钥不下发的既有裁定，多一份静态拷贝 = 多一类出机器面」**。
/// 所以 `keys/` 目录（`hub.key`、`core_<h>.key` 的家）一个字节都不进备份组：
/// 扫描面只落 `data_dir` **根**下的单文件（`read_dir` 不递归，`is_file()` 把目录整个
/// 跳过），将来也不许为它加递归。core.db 进组是**库文件可复制段**，keys/ 是**身份**——
/// 两回事；私房本的密文进了组，开它的钥匙绝不跟着走。
///
/// 只在有 backup key 时调用：`<box>.pass` 是明文口令，和它的 `.enc` 一起进
/// **加密**备份才是自洽的；明文备份里放它们等于把 LLM key 明文留在盘上。
fn collect_extras(
    data_dir: &Path,
    dir: &Path,
    now_ms: i64,
    key: &BackupKey,
    llm_config: Option<&Path>,
) -> Result<Vec<BackupExtra>, StoreError> {
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    if let Some(path) = llm_config {
        if !path.is_file() {
            return Err(StoreError::Io("the llm config is missing".to_string()));
        }
        // 恢复目标固定叫 llm.toml：`hub cycle --llm-config <data-dir>/llm.toml` 直接可用。
        sources.push(("llm.toml".to_string(), path.to_path_buf()));
    } else {
        let candidate = data_dir.join("llm.toml");
        if candidate.is_file() {
            sources.push(("llm.toml".to_string(), candidate));
        }
    }
    let mut boxes: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if matches!(name, "llm.toml" | "hub.key" | "FENCED" | "REQUIRE_EPOCH") {
            continue;
        }
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("enc") | Some("pass")
        ) {
            boxes.push((name.to_string(), path));
        }
    }
    boxes.sort_by(|a, b| a.0.cmp(&b.0));
    sources.extend(boxes);

    let mut extras: Vec<BackupExtra> = Vec::with_capacity(sources.len());
    for (source, path) in sources {
        let bytes = std::fs::read(&path)?;
        let sha256 = rc_net::protocol::sha256_hex(&bytes);
        let stored = extra_stored_name(now_ms, &source);
        let dest = dir.join(&stored);
        if encrypt_file(&path, &dest, key).is_err() {
            let _ = std::fs::remove_file(&dest);
            for extra in &extras {
                let _ = std::fs::remove_file(dir.join(&extra.stored));
            }
            return Err(StoreError::Io(
                "backup extras encryption failed".to_string(),
            ));
        }
        extras.push(BackupExtra {
            source,
            stored,
            sha256,
            bytes: bytes.len() as u64,
        });
    }
    Ok(extras)
}

/// 组内各段的**最终**文件名（加密后是 `.db.enc`，明文组是 `.db`）。
/// `core` 是懒库缺席时的 `None`——这一格没有文件，也没有读数。
struct SegmentNames {
    transcript: String,
    memory: String,
    ledger: String,
    core: Option<String>,
}

fn build_manifest(
    root: &Root,
    core_conn: Option<&rusqlite::Connection>,
    now_ms: i64,
    names: SegmentNames,
    encryption: Option<BackupEncryption>,
    extras: Vec<BackupExtra>,
) -> Result<BackupManifest, StoreError> {
    let transcript_max_ingested_at: Option<i64> = root
        .transcript()
        .conn()
        .query_row("SELECT max(ingested_at_ms) FROM turns", [], |r| r.get(0))?;
    let memory_rows = memory_rows(root.memory().conn())?;
    let memory_digest = rc_store::snapshot::logical_digest(
        root.memory().conn(),
        rc_memory::schema::MEMORY_TABLES,
    )?;
    // 账本段（必备）：present-only 行数 + 内容摘要，恢复侧从副本重算同一集合来对。
    let ledger_segment = {
        let conn = root.ledger().conn();
        let tables = present_tables_of(conn)?;
        Some(BackupLedger {
            file: names.ledger,
            rows: sum_rows_of(conn, &tables)?,
            digest: digest_of_tables(conn, &tables)?,
        })
    };
    // agent 私房本段（存在才备）：`core_agents` 注册行数 + 全表摘要。core.db 没有
    // user_version，内容身份完全靠这两格钉；懒库缺席 ⇒ `None`，清单如实记 null。
    let core_segment = match core_conn {
        Some(conn) => {
            let tables = present_tables_of(conn)?;
            let agents: i64 =
                conn.query_row("SELECT count(*) FROM core_agents", [], |r| r.get(0))?;
            Some(BackupCore {
                file: names.core.expect("core segment without a file name"),
                agents,
                digest: digest_of_tables(conn, &tables)?,
            })
        }
        None => None,
    };
    let mut distill_watermarks = Vec::new();
    for scope in rc_ledger::water::scopes(root.ledger(), "distill")? {
        if let Some(cursor) = rc_ledger::water::read(root.ledger(), "distill", &scope)? {
            distill_watermarks.push(DistillWatermark {
                scope,
                at_ms: cursor.at_ms,
                turn_uid: cursor.turn_uid,
            });
        }
    }
    Ok(BackupManifest {
        format_version: MANIFEST_FORMAT_VERSION,
        created_at_ms: now_ms,
        transcript: names.transcript,
        memory: names.memory,
        transcript_max_ingested_at,
        memory_rows,
        memory_digest,
        memory_version: rc_store::ddl::MEMORY_VERSION,
        distill_watermarks,
        ledger: ledger_segment,
        core: core_segment,
        encryption,
        extras,
    })
}

/// 清单先写临时名再改名：半个 JSON 被 `restore` 读成"清单坏了"之前，它根本不会叫清单。
fn write_manifest(dest: &Path, manifest: &BackupManifest) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(manifest).map_err(|_| {
        StoreError::Io("the backup manifest could not be encoded".to_string())
    })?;
    let tmp = dest.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// 轮转：留最近 `keep` 组。**按 `backup-<ts>.json` 的时间戳排**，认不出的名字一律不碰。
/// 只删由**认识的清单名**推导出的组内文件（W28 起含 ledger/core 段的名字）；
/// 目录里多出来的任何别名都不是这里的东西。
fn retain(dir: &Path, keep: usize) -> Result<(), StoreError> {
    let mut groups: Vec<(i64, String)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(ms) = name
            .strip_prefix("backup-")
            .and_then(|s| s.strip_suffix(".json"))
            .and_then(|s| s.parse::<i64>().ok())
        {
            groups.push((ms, name));
        }
    }
    groups.sort();
    let excess = groups.len().saturating_sub(keep);
    for (ms, manifest) in groups.into_iter().take(excess) {
        let manifest_path = dir.join(&manifest);
        // 清单先读再删：本组的 extras/ledger/core 名字只有清单知道；读不出来就退回按时间戳推名。
        let doc: Option<BackupManifest> = std::fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        match doc {
            Some(doc) => {
                let segment_names = doc.ledger.into_iter().map(|l| l.file).chain(
                    doc.core.into_iter().map(|c| c.file),
                );
                let names = std::iter::once(doc.transcript)
                    .chain(std::iter::once(doc.memory))
                    .chain(segment_names)
                    .chain(doc.extras.into_iter().map(|e| e.stored));
                for name in names {
                    if safe_backup_name(&name) {
                        let _ = std::fs::remove_file(dir.join(name));
                    }
                }
            }
            None => {
                // 推名兜底也盖 W28 的两段：旧清单（读不出来）的时代没有这两段，
                // 但清单本身可能坏掉的新组会有——兜底按本组时间戳把认识的名字都走一遍。
                for base in [
                    backup_name(ms),
                    memory_name(ms),
                    ledger_name(ms),
                    core_name(ms),
                ] {
                    for name in [base.clone(), format!("{base}.enc")] {
                        let _ = std::fs::remove_file(dir.join(name));
                    }
                }
            }
        }
        let _ = std::fs::remove_file(manifest_path);
    }
    Ok(())
}

/// 只认纯文件名：清单/轮转都不许把删除引到目录外。
fn safe_backup_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw != "."
        && raw != ".."
        && !raw.contains('/')
        && !raw.contains('\\')
        && !raw.contains('\0')
        && Path::new(raw).file_name().and_then(|n| n.to_str()) == Some(raw)
}

/// 备份成功：**置位**（不是累加）——"上次成功是几点"是一个时刻，不是一个次数。
pub fn mark_ok(root: &mut Root, now_ms: i64) -> Result<(), StoreError> {
    rc_ledger::counters::set_one(
        root.ledger_mut(), "maintenance", "backup", "backup_ok_at_ms", now_ms, now_ms,
    )
}

/// 备份失败：同样置位。**失败也要留时刻**——只留"上次成功"的话，
/// 一次连续失败会让那一格停在旧时刻，而旧时刻会被读成"很久没备份"，
/// 那恰好是对的方向，但分不清"没备份"与"备份了但失败"。
pub fn mark_failed(root: &mut Root, now_ms: i64) -> Result<(), StoreError> {
    rc_ledger::counters::set_one(
        root.ledger_mut(), "maintenance", "backup", "backup_fail_at_ms", now_ms, now_ms,
    )
}
