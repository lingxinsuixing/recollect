use crate::{LedgerHandle, MemoryHandle, StoreError, TranscriptHandle};
use rusqlite::Connection;

pub const TRANSCRIPT_VERSION: i32 = 5;

/// 转录检索影子表的名字（W38a 刀 A，设计 2026-11-08-w38a-transcript-search §二.1）。
/// 名字的唯一家在这里：rc-ledger 的写面与 rc-search 的读面都引用这个常量，
/// 不许各自抄字面量（两份拼写就是两处漂移的机会）。
pub const TURN_SHADOW: &str = "turn_shadow";
// 阶段 4：+llm_state（轮转态落盘，节 4 第 7 条）；E2：游标列改中性名 `cursor_at_ms`；
// distill 轴换 ingested_at（v3→v4 的唯一迁移动作，见 `backfill_ledger_v4`）。
// 阶段 4 T1：+device/pair_code/audit_log（v4→v5 只加表，不动既有行）。
// 阶段 5 B3a：+pre_distilled_turn（v5→v6 只加表，不写 backfill——空表即可）。
// v6→v7：+device_capability（配对设备的能力面，只加表）。
pub const LEDGER_VERSION: i32 = 7;
// canonical 基线（32 表 = 冻结的 25 表 + S1b 的 resource_owner/resource_grant 侧表
// + S3a 的 review_item/review_decision 审查侧表 + W15a 的 card_verdict_event 事件表
// + G1 刀 B 的 card_sink 归档章表 + 波二刀 A 的 card_reuse_event 复用事件表；
// 前 25 表逐字照 `design/2026-10-24-memory-schema.md` §3/§4，归属侧表照
// `design/2026-10-26-ownership-persistence.md` §2.1，审查侧表照
// `design/2026-10-26-review-queue-s3.md` §3.1/§3.2）。
// v≤5 的一律拒绝重建，没有迁移路径（无生产数据）；侧表只靠 `IF NOT EXISTS` 增量补齐，不 bump。
pub const MEMORY_VERSION: i32 = 6;

const TRANSCRIPT_SQL: &[&str] = &[
    "PRAGMA journal_mode=WAL",
    "PRAGMA foreign_keys=ON",
    "CREATE TABLE IF NOT EXISTS sessions(
        session_key TEXT NOT NULL,
        device_id   TEXT NOT NULL,
        source_agent TEXT NOT NULL,
        machine     TEXT,
        project_hint TEXT,
        last_saved_at_ms INTEGER,
        -- S1b-C1：会话级资源归属（设计 ownership-persistence §1.1）。**可空、无 DEFAULT**：
        -- 旧行自然是 NULL（= 未知 ⇒ 读侧最保守），新行由 `insert_inner` 写保守默认值。
        -- 这里不许写 CHECK：SQLite 的 ALTER 不能带 CHECK，写了就让迁移库与 fresh 库形状分叉；
        -- 值域由契约（ingest_v3 的 ScopeKind / visibility / valid_retention）+ 测试执法。
        owner_principal_id TEXT,   -- NULL = 未登记自然人（S1b 恒 NULL）
        owner_agent_id     TEXT,   -- NULL = 旧行；新行 = source_agent
        scope_kind         TEXT,
        scope_key          TEXT,
        visibility         TEXT,
        retention          TEXT,
        ownership_src      TEXT,   -- v3|default|operator|legacy：这 7 列谁写的
        PRIMARY KEY(session_key, device_id)
     )",
    // 归属扫描/回填的第一跳：按 owner 找会话。索引不靠版本——`apply` 每次都会
    // `CREATE INDEX IF NOT EXISTS` ⇒ 老库在 v4→v5 那次 open 就有。
    "CREATE INDEX IF NOT EXISTS sessions_by_owner ON sessions(owner_agent_id, owner_principal_id)",
    // 身份层 I1（设计 `2026-10-27-principal-identity.md` §3.1）：会话主人身份的**追加式**侧表。
    // `sessions` 是 append-only（下面第 44 行那条注释），而"改盖"天然是 UPDATE ⇒ 章住这里：
    // 每次盖章追加一行，`(session_key, device_id)` 内 `claimed_at_ms` 最大的一条即当前有效章
    // （读侧唯一入口 `rc_ledger::transcript::resolve_principal`）。
    // 三处形状是拍板的：主键三元组、没有 `id` 列（行 id 就是 SQLite 的 rowid，`supersedes` 指它）、
    // `source` **不设 CHECK**（闭集执法在 `claim_principal`，与 audit_log.action 同一条取向：
    // 新来源是常量扩展，不是建表迁移）。
    // **不 bump TRANSCRIPT_VERSION**：与 memory 侧 resource_owner/review_item 同先例——
    // `apply` 每次都重放全部 SQL ⇒ 旧库开库即自愈。
    // 它**不进** `MEMORY_TABLES`：这是 transcript 侧表，回答"这个会话是谁的"，不是记忆内容。
    "CREATE TABLE IF NOT EXISTS session_principal_claim(
        session_key    TEXT NOT NULL,
        device_id      TEXT NOT NULL,
        principal_id   TEXT NOT NULL,
        principal_name TEXT,
        source         TEXT NOT NULL,
        claimed_by     TEXT NOT NULL,
        claimed_at_ms  INTEGER NOT NULL,
        supersedes     INTEGER,
        reason         TEXT,
        PRIMARY KEY(session_key, device_id, claimed_at_ms)
     )",
    "CREATE INDEX IF NOT EXISTS claim_by_session
        ON session_principal_claim(session_key, device_id, claimed_at_ms DESC)",
    // append-only：没有 UPDATE 路径（运维面 purge 是唯一例外，阶段 4）。
    // 这里不许出现任何派生列——tests\ddl.rs 逐条断言。
    "CREATE TABLE IF NOT EXISTS turns(
        turn_uid TEXT PRIMARY KEY,
        session_key TEXT NOT NULL,
        device_id TEXT NOT NULL,
        role TEXT NOT NULL CHECK(role IN ('human','agent','system')),
        body TEXT NOT NULL,
        body_fp TEXT NOT NULL,
        turn_key_tier INTEGER NOT NULL CHECK(turn_key_tier IN (1,2,3)),
        project_hint TEXT,
        created_at_ms INTEGER NOT NULL,
        ingested_at_ms INTEGER NOT NULL,
        commit_seq INTEGER NOT NULL DEFAULT 0
     )",
    "CREATE INDEX IF NOT EXISTS turns_by_session ON turns(session_key, created_at_ms)",
    // E2：`select_batch` 的翻页轴换成到达钟（`ingested_at_ms`），旧索引不再覆盖那条 ORDER。
    // `apply` 每次都会执行 `CREATE INDEX IF NOT EXISTS` ⇒ 老库自动补上，不 bump TRANSCRIPT_VERSION。
    "CREATE INDEX IF NOT EXISTS turns_by_session_ingested ON turns(session_key, ingested_at_ms)",
    // v2：中心复制轴。`commit_seq` 是中心全局单调提交号；`0` 表示旧行/合成行尚未分配。
    // 索引按 `(commit_seq, turn_uid)` 排列，备份按 `commit_seq > since` 顺序拉取。
    "CREATE INDEX IF NOT EXISTS turns_by_commit ON turns(commit_seq, turn_uid)",
    "CREATE TABLE IF NOT EXISTS center_state(
        id INTEGER PRIMARY KEY CHECK(id=1),
        last_commit_seq INTEGER NOT NULL DEFAULT 0,
        epoch INTEGER NOT NULL DEFAULT 1
     )",
    "INSERT OR IGNORE INTO center_state(id, last_commit_seq, epoch) VALUES(1, 0, 1)",
    // 每次成功导入的一条提交记录。`commit_seq` 与 turns.commit_seq 同事务写入；
    // 只记计数/哈希/时间，不记正文。
    "CREATE TABLE IF NOT EXISTS ingest_log(
        commit_seq INTEGER PRIMARY KEY,
        epoch INTEGER NOT NULL,
        op TEXT NOT NULL CHECK(op IN ('import')),
        session_key TEXT NOT NULL,
        device_id TEXT NOT NULL,
        turns_count INTEGER NOT NULL,
        content_sha256 TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
     )",
    // v3：purge 的墓碑与导入共用 center_state.commit_seq。单独一张表，避免重写 v2
    // ingest_log 的 import-only CHECK；复制导出把两张表按 commit_seq 合并。
    "CREATE TABLE IF NOT EXISTS tombstone(
        commit_seq INTEGER PRIMARY KEY,
        epoch INTEGER NOT NULL,
        scope_kind TEXT NOT NULL CHECK(scope_kind IN ('session','device')),
        scope_key TEXT NOT NULL,
        turns_removed INTEGER NOT NULL,
        content_sha256 TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
     )",
    "CREATE INDEX IF NOT EXISTS tombstone_scope ON tombstone(scope_kind, scope_key)",
    // v4：备份回执。`replicated_seq` 回答“至少有一份备份已经落地到哪一格”，
    // 设备据此决定何时可以清理本地原始副本。`epoch` 让提升前的旧回执自动失效；
    // 恢复（restore）不带走这张表——恢复出来的中心必须让备份重新回执。
    "CREATE TABLE IF NOT EXISTS replica_ack(
        device_id TEXT PRIMARY KEY,
        applied_seq INTEGER NOT NULL,
        epoch INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
     )",
    // W38a 刀 A（设计 2026-11-08-w38a-transcript-search §二.1）：转录检索的 FTS5 影子表。
    // `IF NOT EXISTS` + **不 bump TRANSCRIPT_VERSION**：与 session_principal_claim 同先例，
    // `apply` 每次重放全部 SQL ⇒ 老库开库自愈（它也不进任何版本迁移梯子）。
    // `tokenize='unicode61'` 显式写出（R20-4 同一条：不靠随 SQLite 版本漂的默认值）。
    // 三列与 rc-index 的 rc_shadow 同一家族形状：`turn_uid` 是回 JOIN turns 的钥匙（UNINDEXED），
    // `tokens` 是 `rc_index::analyzer::row_doc("", body)` 的词串（turns 无标题列），`row_len` 是词数。
    // 写入面只有两处，都在 rc-ledger：`insert_inner`（增量，与 turns 行同事务同生共死）与
    // `rebuild_turn_shadow`（全量重建）。读面在 rc-search::transcript，一律 JOIN turns 权威面：
    // purge/restore 留下的孤儿影子行自然缺席，coverage 读数显形（细案 §二.1）。
    // 派生层可整层丢弃（DROP 不伤 turns）——它不是正文，是可重建的索引。
    "CREATE VIRTUAL TABLE IF NOT EXISTS turn_shadow USING fts5(
        turn_uid UNINDEXED, tokens, row_len UNINDEXED, tokenize='unicode61')",
];

const LEDGER_SQL: &[&str] = &[
    "PRAGMA journal_mode=WAL",
    "CREATE TABLE IF NOT EXISTS import_log(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at_ms INTEGER NOT NULL,
        finished_at_ms INTEGER,
        device_id TEXT,
        exchange_sha256 TEXT NOT NULL,
        sessions_in INTEGER NOT NULL,
        turns_accepted INTEGER NOT NULL,
        turns_rejected INTEGER NOT NULL,
        batch_reject_code TEXT
     )",
    // 架构 §9.5：三个用量计数器共用一张窄表，零正文。
    "CREATE TABLE IF NOT EXISTS usage_counter(
        subject_kind TEXT NOT NULL,
        subject_id   TEXT NOT NULL,
        metric       TEXT NOT NULL,
        count        INTEGER NOT NULL DEFAULT 0,
        last_at_ms   INTEGER NOT NULL,
        PRIMARY KEY(subject_kind, subject_id, metric)
     )",
    // 架构 §7：抽取 / 推导 / 异常扫描三个家族各一游标，**互不共用**。
    // family 用封闭 CHECK：新家族是一次 schema 变更（同 D7 对枚举的取向），不是一个字符串参数。
    // scope_key：distill 是 `water::distill_scope` 给出的 `device_id/session_key`（每会话独立蒸），
    // derive 与 anomaly_scan 是全局 ''。
    // 游标语义由**家族**决定，与 `rc_ledger::water::Cursor` 的三轴描述同源：
    // distill = turns.ingested_at_ms（到达钟）；derive = 记忆行的 created_at_ms；
    // anomaly_scan = 扫描时刻的 now_ms（它没有"待扫的记忆行"这个轴——扫的是全量，
    // 戳记的是这一轮扫过的时间，`hub::run_anomaly_scan` 的 `advance` 就喂这一格）。
    // 列名因此用中性的 `at_ms`——把某一族的轴写进列名，另一族就背着一条不成立的承诺。
    "CREATE TABLE IF NOT EXISTS job_watermark(
        family TEXT NOT NULL CHECK(family IN ('distill','derive','anomaly_scan')),
        scope_key TEXT NOT NULL,
        cursor_at_ms INTEGER NOT NULL DEFAULT 0,
        cursor_turn_uid TEXT NOT NULL DEFAULT '',
        attempts INTEGER NOT NULL DEFAULT 0,
        last_success_at_ms INTEGER,
        halted_at_ms INTEGER,
        PRIMARY KEY(family, scope_key))",
    // §6.5 P2 的数据源：每个 turn 的抽取态。它住水位库而不是记忆库，
    // 因为它是"P 独占的过程读数"，不是记忆内容（守卫 12）。
    // reason 只许封闭短语（schema / fence / quota / halt），长度在测试里钉死 ≤32。
    // S1：`extraction_fingerprint` 记"这一轮是谁产出的"（管线版本 + 模型 + 两段 prompt 的
    // 64 位 hex）。**可空**：升级前的行与节点自蒸写下的行没有这一格（NULL = 没记过，
    // 与"空串"是两件事）。它不 bump `LEDGER_VERSION`——旧库靠列存在性驱动的补列自愈。
    "CREATE TABLE IF NOT EXISTS extract_state(
        turn_uid TEXT PRIMARY KEY,
        state TEXT NOT NULL CHECK(state IN ('pending','extracted','rejected','worthless')),
        reason TEXT,
        at_ms INTEGER NOT NULL,
        extraction_fingerprint TEXT)",
    // S2a-1（设计 2026-10-26-s2-prompt-v0 §3）：多 pass 抽取的**按 (turn, pass, family)** 状态。
    // 它是 `extract_state` 的旁表，不是它的替代：旧表的主键与语义一个字节都不动（legacy v3 轮级
    // 汇总仍只读那一张）。`family` 只对 fact 取值（decision|promise|plan），其余 pass 写空串——
    // 这条"非 fact 只许空串"的形状住在 `rc_ledger::pass_state` 的 API 校验里，DDL 只钉闭集。
    // `stale` = 指纹变了、旧行的产出不再作数（重跑会覆盖），它不是终态之外的第四类。
    // 波三修 1：闭集加 `lesson`（第 6 pass 的终态行：空产出/契约拒收/审核完成）。
    // **不 bump `LEDGER_VERSION`**：旧库的 CHECK 由 `migrate_ledger` 的 `widen_closed_set_check`
    // 按 canonical 重建；新表靠 IF NOT EXISTS 补齐；本片也不把它塞进 ledger 快照（S4 前再补）。
    "CREATE TABLE IF NOT EXISTS extract_pass_state(
        turn_uid TEXT NOT NULL,
        pass_id TEXT NOT NULL CHECK(pass_id IN ('naming','fact','event','time','state','lesson')),
        family TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL CHECK(state IN
            ('pending','running','done','empty','rejected','quarantined','stale')),
        attempt INTEGER NOT NULL DEFAULT 0,
        next_retry_at_ms INTEGER,
        fingerprint TEXT,
        reason TEXT,
        run_id TEXT,
        updated_at_ms INTEGER NOT NULL,
        -- P0-2：只含 system prompt 的版本号（64 位小写 hex，算法在
        -- `rc_ledger::pass_state::prompt_version`）。指纹含整批 user 文本，批组成一变就全不等，
        -- 不能当「改判据」的信号；这一列只回答「这一格的产出是哪一版 system prompt 产的」。
        -- 可空：旧库升级后既有行是 NULL，NULL **不**触发重选（不自动全量重跑）。
        -- 列序放在末尾：与旧库 `ALTER TABLE ... ADD COLUMN` 的追加位置逐列相同。
        -- 同样不 bump `LEDGER_VERSION`：补列走 `backfill_ledger_prompt_version`（列存在性驱动）。
        prompt_version TEXT,
        PRIMARY KEY(turn_uid, pass_id, family))",
    // 选工作（"哪些轮缺哪个 pass"）按 pass_id + state 扫；没有它每次都是全表扫。
    "CREATE INDEX IF NOT EXISTS extract_pass_state_by_pass
        ON extract_pass_state(pass_id, state)",
    // S2e-3（dogfood-a-smoke 发现 3）：解析失败时模型原文的**限长副本**，让冒烟能直接看出
    // "为什么是 json/quote_not_verbatim"。只在事实/事件 pass 的抽取或审核解析失败时写一条；
    // 成功输出一个字节都不进这张表。`stage` 是拒收的两个现场（抽取/审核），`family` 只对
    // fact 取值（其余 pass 空串），`turn_uid` 取该批第一轮。截断到 64,000 字符由
    // `rc_ledger::pass_state::record_artifact` 做（DDL 不管长度），TTL/清理归 S3 维护服务。
    // 同样不 bump `LEDGER_VERSION`：`apply` 每次执行全部 SQL，新表靠 IF NOT EXISTS 补齐。
    "CREATE TABLE IF NOT EXISTS extract_pass_artifact(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        pass_id TEXT NOT NULL,
        family TEXT NOT NULL DEFAULT '',
        stage TEXT NOT NULL CHECK(stage IN ('extract','review')),
        run_id TEXT NOT NULL,
        turn_uid TEXT NOT NULL,
        raw TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL)",
    // "这个格子上一次拒收的原文是什么"：按 (pass_id, stage) 找最新一条，family 只作过滤。
    "CREATE INDEX IF NOT EXISTS extract_pass_artifact_latest
        ON extract_pass_artifact(pass_id, stage, created_at_ms)",
    // 节 4 第 7 条（A47-2）：轮转态落盘，一台机器一行。`machine` 就是既有的设备标识命名空间；
    // "缺身份"的处置（不读不写）在调用方那一侧，所以这一层不校验、不设 CHECK。
    // `probe_due_at_ms` 可空＝"没有待做探测"，与 0（随时可探测）是两件事（段 A 的 persisted() 靠这个区分）。
    "CREATE TABLE IF NOT EXISTS llm_state(
        machine         TEXT NOT NULL PRIMARY KEY,
        slug            TEXT NOT NULL,
        probe_due_at_ms INTEGER,
        probe_ok        INTEGER NOT NULL DEFAULT 0,
        probe_failures  INTEGER NOT NULL DEFAULT 0,
        written_at_ms   INTEGER NOT NULL)",
    // ── 阶段 4 · T1：设备身份与配对 ──
    // `token_hash` 唯一：一个 token 只对应一台设备。raw token 不落库。
    "CREATE TABLE IF NOT EXISTS device(
        device_id    TEXT PRIMARY KEY,
        pubkey       TEXT NOT NULL,
        token_hash   TEXT NOT NULL UNIQUE,
        paired_at_ms INTEGER NOT NULL,
        revoked_at_ms INTEGER)",
    // 一次性码：`used_at_ms` 非空即已用；`expires_at_ms` 到期即废。不存 raw 码。
    "CREATE TABLE IF NOT EXISTS pair_code(
        code_hash   TEXT PRIMARY KEY,
        created_at_ms INTEGER NOT NULL,
        expires_at_ms INTEGER NOT NULL,
        used_at_ms   INTEGER,
        used_by_device TEXT)",
    // S4b1：邀请票的**服务端**登记面。票本体（签过名的载荷）不落库——它由中心私钥
    // 每次现签；这里只记"这张票发过、能不能再用一次"。`id` 由出票方随机生成并写进票的
    // 签名体，所以重放一张旧票也撞同一行。
    // **不 bump `LEDGER_VERSION`**：与 review_watermark 同例，`apply` 每次执行全部 SQL，
    // 新表靠 IF NOT EXISTS 补齐。
    "CREATE TABLE IF NOT EXISTS invite(
        id           TEXT PRIMARY KEY,
        created_at_ms INTEGER NOT NULL,
        expires_at_ms INTEGER NOT NULL,
        used_at_ms   INTEGER,
        used_by_device TEXT,
        revoked_at_ms INTEGER)",
    // v7：配对设备的能力面。默认没有能力；`replicate` 是备份读取全量提交的显式授权，
    // 不是"配对即全库可读"。grant/revoke 都留审计。
    "CREATE TABLE IF NOT EXISTS device_capability(
        device_id    TEXT NOT NULL,
        capability   TEXT NOT NULL CHECK(capability IN ('replicate')),
        granted_at_ms INTEGER NOT NULL,
        revoked_at_ms INTEGER,
        PRIMARY KEY(device_id, capability))",
    // 审计：T1 写 pair / revoke / pair_code_issued；T5 追加 purge / restore；
    // S4b1 追加 invite_issued / invite_revoked；能力面追加 replicate_grant / replicate_revoke。
    // **action 不设 CHECK**：新动作是常量扩展，不是建表迁移（处置见计划 §Global）。
    "CREATE TABLE IF NOT EXISTS audit_log(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at_ms INTEGER NOT NULL,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        subject TEXT,
        detail TEXT)",
    // ── 阶段 5 · B3a：中心已吸收的节点自蒸轮 ──
    // 节点离线蒸过、中心按行吸收的那些 turn 住这张表。`select_batch` 在 `extract_state`
    // 的 `stated` 集合之外再并进这一张的 uid 集，中心因此**不重蒸**（A48 的裁定）。
    // `at_ms` 只在第一次写入时落值：重复标记是幂等的，不刷新时间戳。
    "CREATE TABLE IF NOT EXISTS pre_distilled_turn(
        turn_uid TEXT PRIMARY KEY,
        at_ms    INTEGER NOT NULL)",
    // H7c2：审查轴水位——每个 pass 一行，只有计数与最大判定 id，**零正文/零判定详情**
    // （记忆库的 `review_item`/`review_decision` 才是内容侧真相）。它随账本快照复制到
    // standby：接管后与本地 `review_decision` 的最大 id 对比，"审查差多少条"才答得出来。
    // **不 bump `LEDGER_VERSION`**：`apply` 每次执行全部 SQL，新表靠 IF NOT EXISTS 补齐。
    "CREATE TABLE IF NOT EXISTS review_watermark(
        pass_id TEXT PRIMARY KEY,
        max_decision_id INTEGER NOT NULL,
        pending_review INTEGER NOT NULL,
        quarantined INTEGER NOT NULL,
        applied INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
     )",
];

/// G1 归档章表（`card_sink`）的 DDL **唯一家**（刀 A 落地时住 `rc_memory::value_gate`
/// 私有常量，刀 B 第 0 件搬进 canonical 重放列）。
///
/// 为什么必须开库即存在：`MEMORY_TABLES` 的备份行数/摘要读数是**严格制**——清单里有而
/// 库里没表，`logical_digest` 直接报 `no such table`，懒补表撑不住升清单。
/// 为什么必须进清单：章是**消费面状态**（召回主循环缺省不取），`hub::restore` 按清单逐表
/// 整替；LLM 价值轴的章（`reason='value_axis'`）不可再生，不随备份恢复就永久丢了。
///
/// `pub` 是为了让写路径的幂等补表（`value_gate::ensure`，双保险）重放**同一根字符串**，
/// 不另抄一份 DDL。`reason` 刻意不加 CHECK：程序章 `program_gate` 与 LLM 章
/// `value_axis` 同表共存，加 CHECK 就得重建表（SQLite 改不了 CHECK）。
pub const CARD_SINK_SQL: &str = "CREATE TABLE IF NOT EXISTS card_sink(
        card_id TEXT PRIMARY KEY REFERENCES card(id),
        sink TEXT NOT NULL CHECK(sink IN ('archive')),
        reason TEXT NOT NULL,
        at_ms INTEGER NOT NULL,
        norm_version INTEGER NOT NULL)";

/// 复用反馈事件表（`card_reuse_event`）的 DDL **唯一家**（波二刀 A，设计
/// `2026-11-08-g1-reuse-lesson-design.md` §波次二）。
///
/// 为什么走 canonical 正路而不是模块私有侧表：复用反馈是**不可再生的消费面状态**
/// （"这张卡帮没帮上忙"只发生过一次，没有任何程序能回填它）。`hub::restore` 按
/// `MEMORY_TABLES` **逐表整替**，清单外的表恢复后回到空库形状——反馈行不随备份走
/// 就永久丢了。与 `card_sink` 升清单的理由逐字相同（G1 刀 B 第 0 件）。
///
/// 为什么**不**挤 `card_verdict_event`：那张表是**背书来源账**（"谁盖了 verdict"，
/// `source ∈ audit|review_apply`），本表是**使用质量账**（"用起来怎么样"）。
/// 两轴语义不同源（表态=真假，复用=帮没帮上忙），混一张表就得在一格里装两义。
///
/// `outcome` 的闭集与 `rc_memory::reuse::ReuseOutcome` 的 `slug()` 逐个同词
/// （写口与工具面的值域家都在那一处，这里只是 schema 落点）。
/// **append-only**：每次反馈追加一行，绝不 upsert/覆盖（`id` 是自增行 id，不是卡 id）；
/// 同一张卡同一种 outcome 写两次 = 两行，那正是"用过两次"的读数。
///
/// `pub` 是为了让写路径的幂等补表（`reuse::ensure`，双保险）重放**同一根字符串**，
/// 不另抄一份 DDL。**不 bump `MEMORY_VERSION`**：`apply` 每次重放 `CREATE TABLE IF NOT EXISTS`，
/// 老库开库即自愈。
pub const CARD_REUSE_EVENT_SQL: &str = "CREATE TABLE IF NOT EXISTS card_reuse_event(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        card_id TEXT NOT NULL REFERENCES card(id),
        outcome TEXT NOT NULL CHECK(outcome IN ('worked','failed','partial','mismatch','stale')),
        note TEXT,
        at_ms INTEGER NOT NULL,
        by_agent TEXT NOT NULL,
        norm_version INTEGER NOT NULL)";

const MEMORY_SQL: &[&str] = &[
    "PRAGMA journal_mode=WAL",
    "PRAGMA foreign_keys=ON",
    // canonical 基线（25 表 + 2 张归属侧表 + 2 张审查侧表 + W15a 的 1 张事件表
    // + G1 刀 B 的 1 张归档章表 + 波二刀 A 的 1 张复用事件表 = 32）：前 25 表逐字照
    // `design/2026-10-24-memory-schema.md` §3（本体表）与 §4（消费/派生/审计表），
    // 归属侧表照 `design/2026-10-26-ownership-persistence.md` §2.1，
    // 审查侧表照 `design/2026-10-26-review-queue-s3.md` §3.1/§3.2。
    // H1a 给 `review_item` 末尾追加 `claimed_by`/`claim_expires_at_ms` 两列（队列占位/租约）；
    // H1c 再追加 `batch_key`（批次身份，同批重跑走顶替而不是并存）；
    // H4b 给 `review_decision` 末尾追加 `review_input_sha256`（送审指纹，与载荷指纹分列）。
    // 旧库由 `backfill_memory_review_columns` 按列存在性补齐，`MEMORY_VERSION` 不 bump。
    // 列、类型、DEFAULT、CHECK、索引一处不增不减。
    // 一条纪律：**加表/加列**必须同时登记 rc-memory/src/schema.rs 的归属表（ownership 双向对拍）。
    // ── §3 本体表 ──
    "CREATE TABLE IF NOT EXISTS anchor(
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK(kind IN
            ('person','agent','org','place','object','animal','work','event','concept')),
        canonical TEXT NOT NULL,
        reality_status TEXT NOT NULL DEFAULT 'unknown'
            CHECK(reality_status IN ('real','fictional','virtual','abstract','hypothetical','unknown')),
        world_id TEXT,
        scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active'
            CHECK(status IN ('active','provisional','merged','retired')),
        profile_text TEXT,
        created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
        norm_version INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS anchor_lookup ON anchor(kind, canonical)",
    "CREATE INDEX IF NOT EXISTS anchor_world ON anchor(world_id, scope_kind, scope_key)",
    "CREATE TABLE IF NOT EXISTS alias(
        anchor_id TEXT NOT NULL REFERENCES anchor(id) ON DELETE CASCADE,
        alias TEXT NOT NULL,
        valid_from_ms INTEGER, valid_to_ms INTEGER,
        source_turn_uid TEXT,
        PRIMARY KEY(anchor_id, alias))",
    "CREATE INDEX IF NOT EXISTS alias_lookup ON alias(alias)",
    // 轮级指称：原文哪一轮怎么称呼它（旧行级语义已迁到 memory_mention）。
    "CREATE TABLE IF NOT EXISTS mention(
        id TEXT PRIMARY KEY,
        anchor_id TEXT NOT NULL REFERENCES anchor(id) ON DELETE CASCADE,
        turn_uid TEXT NOT NULL,
        surface TEXT NOT NULL,
        span_start INTEGER, span_end INTEGER,
        speaker_anchor_id TEXT,
        discourse_role TEXT CHECK(discourse_role IN ('speaker','addressee','mentioned','quoted')),
        resolution TEXT NOT NULL CHECK(resolution IN ('resolved','provisional','unresolved')),
        confidence REAL NOT NULL DEFAULT 0.8,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS mention_turn ON mention(turn_uid)",
    "CREATE INDEX IF NOT EXISTS mention_anchor ON mention(anchor_id, turn_uid)",
    "CREATE TABLE IF NOT EXISTS attribute(
        id TEXT PRIMARY KEY,
        anchor_id TEXT NOT NULL REFERENCES anchor(id) ON DELETE CASCADE,
        key TEXT NOT NULL,
        value_json TEXT NOT NULL,
        value_text TEXT NOT NULL,
        value_type TEXT NOT NULL,
        unit TEXT,
        valid_from_ms INTEGER, valid_to_ms INTEGER, superseded_by TEXT,
        confidence REAL NOT NULL DEFAULT 0.8,
        mode TEXT NOT NULL CHECK(mode IN ('stated','inferred','reported')),
        text TEXT NOT NULL,
        world_id TEXT, scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS attribute_anchor ON attribute(anchor_id, key, valid_to_ms)",
    // W14c：读路径按**属性值**找锚（`literal 甲某随行 → 环境锚`），所以值那一列也要一格索引。
    // `CREATE INDEX IF NOT EXISTS` 在每次 open 时重放 ⇒ 老库自动补上，`MEMORY_VERSION` 不 bump。
    "CREATE INDEX IF NOT EXISTS attribute_value ON attribute(value_text)",
    "CREATE TABLE IF NOT EXISTS relation(
        id TEXT PRIMARY KEY,
        subject_anchor_id TEXT NOT NULL REFERENCES anchor(id) ON DELETE CASCADE,
        predicate TEXT NOT NULL CHECK(predicate IN
            ('part_of','depends_on','owns','located_in','role','spouse',
             'parent_of','child_of','works_for','member_of','friend_of')),
        object_anchor_id TEXT NOT NULL REFERENCES anchor(id) ON DELETE CASCADE,
        valid_from_ms INTEGER, valid_to_ms INTEGER, superseded_by TEXT,
        mode TEXT NOT NULL CHECK(mode IN ('stated','inferred','reported')),
        confidence REAL NOT NULL DEFAULT 0.8,
        text TEXT NOT NULL,
        evidence_turn_uid TEXT,
        world_id TEXT, scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL,
        CHECK(subject_anchor_id <> object_anchor_id))",
    "CREATE INDEX IF NOT EXISTS relation_subject ON relation(subject_anchor_id, predicate, valid_to_ms)",
    "CREATE INDEX IF NOT EXISTS relation_object ON relation(object_anchor_id, predicate, valid_to_ms)",
    "CREATE TABLE IF NOT EXISTS fact(
        id TEXT PRIMARY KEY,
        subject_anchor_id TEXT NOT NULL REFERENCES anchor(id) ON DELETE CASCADE,
        predicate TEXT NOT NULL,
        object_anchor_id TEXT REFERENCES anchor(id) ON DELETE SET NULL,
        value_json TEXT, value_text TEXT,
        category TEXT NOT NULL CHECK(category IN
            ('event','promise','plan','decision','observation','state_change','other')),
        participants_json TEXT NOT NULL DEFAULT '[]',
        place_anchor_id TEXT REFERENCES anchor(id) ON DELETE SET NULL,
        speaker_anchor_id TEXT REFERENCES anchor(id) ON DELETE SET NULL,
        occurred_at_ms INTEGER, occurred_end_ms INTEGER,
        speech_time_ms INTEGER,
        valid_from_ms INTEGER, valid_to_ms INTEGER,
        due_at_ms INTEGER, status TEXT CHECK(status IN ('pending','done','cancelled','expired')),
        mode TEXT NOT NULL CHECK(mode IN ('stated','inferred','reported')),
        confidence REAL NOT NULL DEFAULT 0.8,
        text TEXT NOT NULL,
        evidence_turn_uid TEXT NOT NULL,
        superseded_by TEXT,
        world_id TEXT, scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS fact_subject ON fact(subject_anchor_id, occurred_at_ms)",
    "CREATE INDEX IF NOT EXISTS fact_time ON fact(occurred_at_ms)",
    "CREATE INDEX IF NOT EXISTS fact_due ON fact(status, due_at_ms)",
    "CREATE INDEX IF NOT EXISTS fact_scope ON fact(world_id, scope_kind, scope_key)",
    "CREATE TABLE IF NOT EXISTS state_assertion(
        id TEXT PRIMARY KEY,
        subject_kind TEXT NOT NULL CHECK(subject_kind IN ('anchor','relation')),
        subject_anchor_id TEXT REFERENCES anchor(id) ON DELETE CASCADE,
        subject_relation_id TEXT REFERENCES relation(id) ON DELETE CASCADE,
        dimension TEXT NOT NULL CHECK(dimension IN
            ('emotion','mood','sentiment','affect_episode','trust','relationship','health','config','progress')),
        value_before TEXT, value_after TEXT NOT NULL,
        valid_from_ms INTEGER, valid_to_ms INTEGER, superseded_by TEXT,
        cause_fact_id TEXT REFERENCES fact(id) ON DELETE SET NULL,
        text TEXT NOT NULL,
        mode TEXT NOT NULL CHECK(mode IN ('expressed','inferred')),
        confidence REAL NOT NULL DEFAULT 0.8,
        evidence_turn_uid TEXT,
        world_id TEXT, scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL,
        CHECK((subject_kind='anchor') = (subject_anchor_id IS NOT NULL)),
        CHECK((subject_kind='relation') = (subject_relation_id IS NOT NULL)))",
    "CREATE INDEX IF NOT EXISTS state_subject ON state_assertion(subject_anchor_id, dimension, valid_to_ms)",
    "CREATE INDEX IF NOT EXISTS state_relation ON state_assertion(subject_relation_id, dimension, valid_to_ms)",
    "CREATE TABLE IF NOT EXISTS utterance(
        id TEXT PRIMARY KEY,
        speaker_anchor_id TEXT REFERENCES anchor(id) ON DELETE SET NULL,
        addressee_anchor_id TEXT REFERENCES anchor(id) ON DELETE SET NULL,
        act TEXT NOT NULL,
        quote TEXT NOT NULL,
        quote_hash TEXT NOT NULL,
        gist TEXT NOT NULL,
        fact_id TEXT REFERENCES fact(id) ON DELETE SET NULL,
        turn_uid TEXT NOT NULL,
        mode TEXT NOT NULL CHECK(mode IN ('stated','inferred','reported')),
        confidence REAL NOT NULL DEFAULT 0.9,
        world_id TEXT, scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL,
        superseded_by TEXT)",
    "CREATE UNIQUE INDEX IF NOT EXISTS utterance_dedup ON utterance(turn_uid, quote_hash, act)",
    "CREATE INDEX IF NOT EXISTS utterance_fact ON utterance(fact_id)",
    "CREATE TABLE IF NOT EXISTS episode(
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        narrative TEXT NOT NULL,
        start_ms INTEGER, end_ms INTEGER,
        participants_json TEXT NOT NULL DEFAULT '[]',
        status TEXT NOT NULL CHECK(status IN ('open','resolved','dormant','superseded')),
        world_id TEXT, scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL,
        superseded_by TEXT)",
    "CREATE TABLE IF NOT EXISTS episode_fact(
        episode_id TEXT NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
        fact_id TEXT NOT NULL REFERENCES fact(id) ON DELETE CASCADE,
        ord INTEGER NOT NULL,
        PRIMARY KEY(episode_id, fact_id))",
    "CREATE INDEX IF NOT EXISTS episode_fact_ord ON episode_fact(episode_id, ord)",
    // ── §4 消费、派生与审计表 ──
    "CREATE TABLE IF NOT EXISTS card(
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        title TEXT NOT NULL,
        text TEXT NOT NULL,
        time_json TEXT,
        pointers_json TEXT NOT NULL DEFAULT '[]',
        scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL, world_id TEXT,
        as_of_ms INTEGER NOT NULL,
        valid_to_ms INTEGER, superseded_by TEXT,
        confidence REAL NOT NULL DEFAULT 0.8,
        mode TEXT NOT NULL DEFAULT 'stated',
        verdict TEXT NOT NULL DEFAULT 'unreviewed',
        cooldown_until_ms INTEGER,
        causal_type TEXT CHECK(causal_type IS NULL OR causal_type IN ('causes','enables','prevents')),
        time_anchor_ms INTEGER, time_anchor_id TEXT,
        time_confidence TEXT,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS card_asof ON card(as_of_ms)",
    "CREATE INDEX IF NOT EXISTS card_scope ON card(world_id, scope_kind, scope_key)",
    // W15a：`card.verdict` 的**背书来源**。追加型事件表——每次落笔追加一行，
    // 绝不 upsert 覆盖，历史因此留得住；"谁盖的"由 `source` 闭集区分。
    // 与 `review_watermark` / `resource_owner` 同一条先例：`apply` 每次重放
    // `CREATE TABLE IF NOT EXISTS`，老库开库即自愈 ⇒ `MEMORY_VERSION` 不 bump。
    // 闭集在 `writers_c` 只写一次（两个常量），这里是它的 schema 落点；两处必须同词。
    "CREATE TABLE IF NOT EXISTS card_verdict_event(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        card_id TEXT NOT NULL,
        verdict TEXT NOT NULL,
        source TEXT NOT NULL CHECK(source IN ('audit','review_apply')),
        at_ms INTEGER NOT NULL,
        norm_version INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS card_verdict_event_by_card ON card_verdict_event(card_id, id)",
    // G1 刀 B：归档章表升 canonical 正路（开库即存在 ⇒ 备份/整替/副本带着章走）。
    // DDL 的唯一家是 [`CARD_SINK_SQL`]，这里只引用它，不抄第二份。
    CARD_SINK_SQL,
    // 波二刀 A：复用反馈事件表。与 `card_sink` 同一条正路（开库即存在、进清单、随备份整替）：
    // 反馈行不可再生。DDL 的唯一家是 [`CARD_REUSE_EVENT_SQL`]，这里只引用它。
    // 索引按 `(card_id, id)`：刀 B 的按卡聚合（worked 计数）与"最近一条反馈"都从这一条走；
    // `id` 入索引是为了同一张卡内按发生序取尾。
    CARD_REUSE_EVENT_SQL,
    "CREATE INDEX IF NOT EXISTS card_reuse_event_by_card ON card_reuse_event(card_id, id)",
    // 派生索引：哪条记忆行提到哪个锚（旧行级 mention 的 canonical 落点）。
    "CREATE TABLE IF NOT EXISTS memory_mention(
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        anchor_id TEXT NOT NULL,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL,
        PRIMARY KEY(mem_table, mem_id, anchor_id))",
    "CREATE INDEX IF NOT EXISTS memory_mention_anchor ON memory_mention(anchor_id)",
    // 记忆行之间的边，可撤销；端点存 "<table>:<id>" 键（当前主要是 card:<id>）。
    "CREATE TABLE IF NOT EXISTS fact_link(
        id TEXT PRIMARY KEY,
        from_id TEXT NOT NULL, to_id TEXT NOT NULL,
        relation TEXT NOT NULL CHECK(relation IN
            ('causes','enables','prevents','responds_to','part_of','depends_on','owns','located_in','role')),
        basis TEXT NOT NULL, evidence_json TEXT NOT NULL,
        decided_by TEXT,
        scope_kind TEXT NOT NULL, scope_key TEXT NOT NULL, world_id TEXT,
        source_agent TEXT NOT NULL, device_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, norm_version INTEGER NOT NULL,
        CHECK(from_id <> to_id))",
    "CREATE INDEX IF NOT EXISTS fact_link_from ON fact_link(from_id)",
    "CREATE INDEX IF NOT EXISTS fact_link_to ON fact_link(to_id)",
    // ── S1b-C2a：资源归属侧表（设计 ownership-persistence §2.1）──
    // 侧表而不是给 25 张表各加 4 列：一处写点 + 一处闸点，且 `apply` 的
    // `CREATE TABLE IF NOT EXISTS` 天然幂等 ⇒ MEMORY_VERSION 不 bump（旧备份靠缺表兼容）。
    // `scope_kind` 是记忆行自己的 ScopeKind（person|project|entity），**不是**契约五值。
    "CREATE TABLE IF NOT EXISTS resource_owner(
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        owner_principal_id TEXT, owner_agent_id TEXT NOT NULL,
        created_by_agent TEXT NOT NULL,
        visibility TEXT NOT NULL CHECK(visibility IN ('private_agent','person','project','org','public')),
        retention TEXT NOT NULL DEFAULT 'default'
            CHECK(retention IN ('default','keep') OR retention LIKE 'days:%'),
        -- 记忆行自己的 ScopeKind（person|project|entity），不是契约五值
        scope_kind TEXT NOT NULL CHECK(scope_kind IN ('person','project','entity')),
        scope_key TEXT NOT NULL,
        source_agent TEXT, device_id TEXT,
        created_at_ms INTEGER NOT NULL,
        PRIMARY KEY(mem_table, mem_id))",
    "CREATE INDEX IF NOT EXISTS resource_owner_agent
      ON resource_owner(owner_agent_id, owner_principal_id, visibility)",
    "CREATE INDEX IF NOT EXISTS resource_owner_scope
      ON resource_owner(scope_kind, scope_key)",
    "CREATE TABLE IF NOT EXISTS resource_grant(
        id TEXT PRIMARY KEY,
        from_principal_id TEXT, from_agent_id TEXT NOT NULL,
        to_agent_id TEXT, to_principal_id TEXT,
        capability TEXT NOT NULL CHECK(capability IN ('recall','export','secret_use')),
        data_class TEXT NOT NULL CHECK(data_class IN ('memory','transcript','secret')),
        scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global','person','project','entity')),
        scope_key TEXT NOT NULL,
        purpose TEXT NOT NULL, granted_at_ms INTEGER NOT NULL,
        expires_at_ms INTEGER, revoked_at_ms INTEGER,
        CHECK((to_agent_id IS NULL) <> (to_principal_id IS NULL)),
        CHECK(to_agent_id IS NULL OR to_agent_id <> from_agent_id),
        CHECK(expires_at_ms IS NULL OR expires_at_ms > granted_at_ms))",
    "CREATE INDEX IF NOT EXISTS resource_grant_to_agent
      ON resource_grant(to_agent_id, revoked_at_ms, expires_at_ms)",
    "CREATE INDEX IF NOT EXISTS resource_grant_to_principal
      ON resource_grant(to_principal_id, revoked_at_ms, expires_at_ms)",
    "CREATE TABLE IF NOT EXISTS embedding(
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        model TEXT NOT NULL, dim INTEGER NOT NULL, vector BLOB NOT NULL,
        norm_version INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
        PRIMARY KEY(mem_table, mem_id, model))",
    "CREATE TABLE IF NOT EXISTS provenance(
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL, turn_uid TEXT NOT NULL,
        anchor_role TEXT NOT NULL CHECK(anchor_role IN ('human','agent','system')),
        ref_kind TEXT NOT NULL DEFAULT 'turn'
            CHECK(ref_kind IN ('turn','card','fact','anchor','relation','state_assertion','utterance','episode','attribute','mention')),
        ord INTEGER,
        role TEXT NOT NULL DEFAULT 'basis' CHECK((role='link')=(ord IS NOT NULL)),
        PRIMARY KEY(mem_table, mem_id, turn_uid))",
    "CREATE INDEX IF NOT EXISTS provenance_turn ON provenance(turn_uid)",
    "CREATE TABLE IF NOT EXISTS attestation(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        stance TEXT NOT NULL CHECK(stance IN ('confirmed','corroborated','rejected','retired')),
        actor TEXT NOT NULL CHECK(actor IN ('agent','center')),
        anchor_turn_uid TEXT, reason TEXT,
        created_at_ms INTEGER NOT NULL, resolved_at_ms INTEGER)",
    "CREATE INDEX IF NOT EXISTS attestation_target ON attestation(mem_table, mem_id, resolved_at_ms)",
    "CREATE TABLE IF NOT EXISTS retirement(
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL, reason TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, resolved_at_ms INTEGER,
        PRIMARY KEY(mem_table, mem_id))",
    "CREATE TABLE IF NOT EXISTS attention(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        kind TEXT NOT NULL CHECK(kind IN
            ('contradiction','alias_ambiguity','duplicate_title','state_conflict','quote_conflict')),
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL, resolved_at_ms INTEGER, rechecked_at_ms INTEGER)",
    "CREATE INDEX IF NOT EXISTS attention_target ON attention(mem_table, mem_id, resolved_at_ms)",
    "CREATE TABLE IF NOT EXISTS memory_op(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id TEXT,
        op TEXT NOT NULL CHECK(op IN ('ADD','UPDATE','NOOP','RETIRE','MERGE','SPLIT','REBUILD')),
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        before_json TEXT, after_json TEXT,
        reason TEXT NOT NULL,
        actor TEXT NOT NULL CHECK(actor IN ('extractor','program','center','agent','user')),
        created_at_ms INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS memory_op_target ON memory_op(mem_table, mem_id, created_at_ms)",
    "CREATE TABLE IF NOT EXISTS anchor_merge(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        from_id TEXT NOT NULL, to_id TEXT NOT NULL,
        run_id TEXT, reason TEXT NOT NULL, created_at_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS memory_stats(
        mem_table TEXT NOT NULL, mem_id TEXT NOT NULL,
        occurrence_count INTEGER NOT NULL DEFAULT 1,
        mention_count INTEGER NOT NULL DEFAULT 1,
        access_count INTEGER NOT NULL DEFAULT 0,
        distinct_session_count INTEGER NOT NULL DEFAULT 1,
        distinct_speaker_count INTEGER NOT NULL DEFAULT 1,
        first_mentioned_ms INTEGER, last_mentioned_ms INTEGER, last_accessed_ms INTEGER,
        emotional_intensity REAL NOT NULL DEFAULT 0,
        open_loop_weight REAL NOT NULL DEFAULT 0,
        salience REAL NOT NULL DEFAULT 0.5,
        salience_updated_ms INTEGER NOT NULL,
        muted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(mem_table, mem_id))",
    "CREATE INDEX IF NOT EXISTS memory_stats_salience ON memory_stats(salience DESC, last_mentioned_ms DESC)",
    "CREATE INDEX IF NOT EXISTS memory_stats_mentioned ON memory_stats(last_mentioned_ms)",
    "CREATE TABLE IF NOT EXISTS time_expr(
        id TEXT PRIMARY KEY,
        turn_uid TEXT NOT NULL,
        text TEXT NOT NULL,
        start_ms INTEGER, end_ms INTEGER,
        precision TEXT NOT NULL CHECK(precision IN ('hour','day','week','month','unknown')),
        basis TEXT NOT NULL CHECK(basis IN ('turn_ts','explicit','inferred','none')),
        kind TEXT NOT NULL CHECK(kind IN ('event_time','speech_time','validity','deadline','recurrence')),
        created_at_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS extraction_run(
        run_id TEXT PRIMARY KEY,
        dataset TEXT NOT NULL, turn_start TEXT NOT NULL, turn_end TEXT NOT NULL,
        model TEXT NOT NULL, prompt_version TEXT NOT NULL, schema_version INTEGER NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('running','ok','partial','failed')),
        started_at_ms INTEGER NOT NULL, finished_at_ms INTEGER,
        usage_json TEXT, notes TEXT)",
    "CREATE TABLE IF NOT EXISTS extraction_batch(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id TEXT NOT NULL REFERENCES extraction_run(run_id) ON DELETE CASCADE,
        pass TEXT NOT NULL CHECK(pass IN ('role','entity','time','event','state','utterance','relation','assemble')),
        attempt INTEGER NOT NULL DEFAULT 1,
        raw_json TEXT NOT NULL, schema_ok INTEGER NOT NULL DEFAULT 0,
        errors_json TEXT, created_at_ms INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS extraction_batch_run ON extraction_batch(run_id, pass)",
    "CREATE TABLE IF NOT EXISTS extraction_issue(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id TEXT NOT NULL, pass TEXT NOT NULL, code TEXT NOT NULL,
        detail TEXT NOT NULL, resolved_at_ms INTEGER, created_at_ms INTEGER NOT NULL)",
    // ── S3a-1：双审队列（设计 `2026-10-26-review-queue-s3` §3.1/§3.2）──
    // 两张表住在记忆库而不是 ledger：待审载荷是**内容**（同 `extraction_batch.raw_json`
    // 的内容先例），判定台账必须与它同生共死；ledger 只存过程读数（守卫 12）。
    // **不 bump `MEMORY_VERSION`**：`apply` 每次都重放 `CREATE TABLE IF NOT EXISTS`，
    // 旧库开库即自愈——与 `extract_pass_state` 的先例同一条（见本文件 ledger 段）。
    // 载荷 64k/键闭集/判定码的校验住在 `rc-memory::review` 的 API 里，DDL 只钉 CHECK 闭集；
    // 列归属见 `rc-memory/src/schema.rs`（`payload_json` 归 X，判定表的
    // verdict/reason_code 归 C、`detail` 归 X，其余归 P）。
    "CREATE TABLE IF NOT EXISTS review_item(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        source TEXT NOT NULL CHECK(source IN ('distill','rows','report','reuse')),
        pass_id TEXT NOT NULL CHECK(pass_id IN ('naming','fact','event','time','rows','state','report','reuse','lesson')),
        family TEXT NOT NULL DEFAULT '',
        session_key TEXT NOT NULL DEFAULT '',
        device_id TEXT NOT NULL DEFAULT '',
        turn_uids_json TEXT NOT NULL,
        owner_json TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        payload_sha256 TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN
            ('pending_review','approved','applied','rejected','quarantined','stale')),
        program_code TEXT,
        conflict_json TEXT,
        attempt INTEGER NOT NULL DEFAULT 0,
        next_retry_at_ms INTEGER,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        applied_at_ms INTEGER,
        -- H1a：队列占位/租约。NULL = 未占用 / 无租约（旧行天然是这两格）。
        -- 列序放在末尾：与旧库 `ALTER TABLE ... ADD COLUMN` 的追加位置逐列相同。
        claimed_by TEXT,
        claim_expires_at_ms INTEGER,
        -- H1c：批次身份（sha256 hex，公式见 `rc_memory::review::batch_key`）。
        -- NULL = 旧行/调用方未提供 ⇒ 不参与顶替。列序同样在末尾。
        batch_key TEXT)",
    "CREATE UNIQUE INDEX IF NOT EXISTS review_item_payload
        ON review_item(pass_id, family, payload_sha256)",
    // H1c：同键在途至多一条。部分索引只约束三个在途态——终态（applied/rejected/stale）
    // 不占位，历史因此可以并存；`batch_key IS NULL` 的旧行不进索引。
    "CREATE UNIQUE INDEX IF NOT EXISTS review_item_batch_inflight
        ON review_item(pass_id, family, batch_key)
        WHERE batch_key IS NOT NULL
          AND state IN ('pending_review','quarantined','approved')",
    "CREATE INDEX IF NOT EXISTS review_item_by_state
        ON review_item(state, pass_id, id)",
    // 只追加判定台账：每次状态变化与一条判定同事务。不设外键——item 的载荷可被 TTL
    // 清理，判定要留；`detail` 只放码/计数（≤512 字符由 API 校验）。
    "CREATE TABLE IF NOT EXISTS review_decision(
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        item_id INTEGER NOT NULL,
        stage TEXT NOT NULL CHECK(stage IN ('program','review','apply','human')),
        verdict TEXT NOT NULL CHECK(verdict IN ('pass','reject','uncertain')),
        reason_code TEXT NOT NULL,
        rules_version TEXT NOT NULL,
        model TEXT NOT NULL DEFAULT '',
        prompt_version TEXT NOT NULL DEFAULT '',
        input_sha256 TEXT NOT NULL,
        decider TEXT NOT NULL CHECK(decider IN ('program','center','human')),
        detail TEXT NOT NULL DEFAULT '',
        created_at_ms INTEGER NOT NULL,
        -- H4b：送审输入指纹（`extraction_fingerprint`）。只有真正送审的 review 行非空；
        -- `input_sha256` 从此恒为载荷指纹（`review_item.payload_sha256`）。
        -- 列序放在末尾：与旧库 `ALTER TABLE ... ADD COLUMN` 的追加位置相同。
        review_input_sha256 TEXT NOT NULL DEFAULT '')",
    "CREATE INDEX IF NOT EXISTS review_decision_by_item
        ON review_decision(item_id, id)",
    // 索引层（rc_shadow / rc_index_marker）不在这里建：派生物的 DDL 只住在唯一能命名它的
    // rc-index::shadow（守卫 2），由 rebuild 自己建、可整层丢弃、幂等重建。
    // MEMORY_SQL 若也建它，每次 open 都会把空表复活，"整层可丢弃"这条性质在结构上就不成立。
    // 见控制器裁定 R15。归属表（rc-memory/src/schema.rs）因此也不登记它的列（见 V9）。
];

pub fn schema_version(conn: &Connection) -> Result<i32, StoreError> {
    let v: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v)
}

fn apply(conn: &Connection, sql: &[&str], target: i32, db: &'static str) -> Result<(), StoreError> {
    let found = schema_version(conn)?;
    if found > target {
        return Err(StoreError::SchemaTooNew { db, found, supported: target });
    }
    for s in sql {
        conn.execute_batch(s)?;
    }
    if found < target {
        conn.pragma_update(None, "user_version", target)?;
    }
    Ok(())
}

/// 一次 `Root::open` 的迁移读数。启动面据此响亮打印，**不改退出码**。
#[derive(Debug, Clone, Copy, Default)]
pub struct MigrationReport {
    /// 本次迁移把多少个 distill scope 的游标归零（只用于启动时响亮打印）。
    pub ledger_cursor_reset: usize,
}

impl MigrationReport {
    /// 迁移提示的唯一文本（零正文、零路径）。`None` = 这次没有回拨。
    ///
    /// 文本只住这一处：hub / doctor / mcp 三个入口都开库、都可能先撞上 v3→v4，
    /// 谁先撞上谁负责说出来；若每个入口各拼一句，第四个入口出现时必然漏掉一条。
    pub fn ledger_warning(&self) -> Option<String> {
        (self.ledger_cursor_reset > 0).then(|| format!(
            "watermarks reset: {} distill scopes \
             (cursor_created_at -> cursor_at_ms; created_at -> ingested_at)",
            self.ledger_cursor_reset))
    }
}

/// 三个句柄一起迁移：装配点只有 `Root::open` 一个，所以"三库齐全"是一句话的事。
/// 记忆库这一头单独可叫（`AgentRoot::open` 只要它），故拆成 `migrate_memory`。
///
/// 水位库的顺序是**先 backfill、后 apply**：`apply` 会把 `user_version` 抬到 `LEDGER_VERSION`，
/// 一旦它先落盘而 backfill 没跑完，下次 open 就会跳过迁移却读不到新列（不可自愈）。
/// 反过来则两种崩溃都可重放：backfill 的写全在一个 IMMEDIATE 事务里，事务未 commit ⇒
/// 回滚后旧列还在；已 commit 而 apply 未跑 ⇒ 旧列已不在，backfill 下次直返 0，apply 补版本。
/// 记忆库没有这一层：canonical 是单基线，旧 schema 在 `check_memory_version` 就被拒。
pub(crate) fn migrate_all(t: &TranscriptHandle, l: &LedgerHandle, m: &MemoryHandle)
    -> Result<MigrationReport, StoreError>
{
    migrate_transcript(t)?;
    let ledger_cursor_reset = migrate_ledger(l)?;
    migrate_memory(m)?;
    Ok(MigrationReport { ledger_cursor_reset })
}

/// 只迁移会话库。节点侧的装配点用它——节点没有可写的记忆库，跑不了 `migrate_all`。
///
/// v1→v2 只加列/加表：`turns.commit_seq` 用 `ALTER TABLE` 补，其余表由 `apply` 幂等建。
/// v4→v5 只给 `sessions` 补 7 个归属列（`backfill_transcript_v5`）。
/// **先补列、后 apply**：`apply` 会把 user_version 抬到目标版本，若先抬版本而列没补，下次 open
/// 会跳过迁移却读到缺列的表（不可自愈）。补列本身按列存在性驱动，崩溃可重放。
pub(crate) fn migrate_transcript(t: &TranscriptHandle) -> Result<(), StoreError> {
    let found = schema_version(t.conn())?;
    if found > TRANSCRIPT_VERSION {
        return Err(StoreError::SchemaTooNew {
            db: "transcript.db",
            found,
            supported: TRANSCRIPT_VERSION,
        });
    }
    backfill_transcript_v2(t.conn())?;
    backfill_transcript_v5(t.conn())?;
    apply(t.conn(), TRANSCRIPT_SQL, TRANSCRIPT_VERSION, "transcript.db")
}

/// v1→v2：给 turns 补 `commit_seq`。列存在性驱动，fresh 库/已迁移库直返。
fn backfill_transcript_v2(conn: &Connection) -> Result<(), StoreError> {
    let has_table = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='turns'",
            [],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    };
    let has_col = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('turns') WHERE name='commit_seq'",
            [],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    };
    if !has_table(conn)? || has_col(conn)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 进事务后重探：两个进程同时首开时，后拿到写锁的一方看到列已补。
    if has_table(&tx)? && !has_col(&tx)? {
        tx.execute_batch("ALTER TABLE turns ADD COLUMN commit_seq INTEGER NOT NULL DEFAULT 0")?;
    }
    tx.commit()?;
    Ok(())
}

/// v4→v5：给 `sessions` 补 7 个归属列（S1b-C1，设计 ownership-persistence §1.2）。
///
/// **列存在性驱动**，不看 `user_version`：版本号与真实列形状可能因旧实现的崩溃窗口分叉。
/// 表不存在（fresh 库）⇒ 直返：`apply` 会用新形状建表，`ALTER` 在这里会撞"no such table"。
/// 与 `backfill_transcript_v2` 同形：开一个 IMMEDIATE 写事务、事务内**逐列重探**，缺哪个补哪个；
/// 两个进程同时首开时只有一个能补列，补列本身原子，崩在任何位置都可重放。
/// 列一律可空、不带 DEFAULT（SQLite 的 ALTER 也不能带 CHECK）⇒ 迁移库与 fresh 库形状一致。
fn backfill_transcript_v5(conn: &Connection) -> Result<(), StoreError> {
    // 顺序就是 `TRANSCRIPT_SQL` 里 7 个新列的追加顺序：两侧 `pragma_table_info` 必须逐列相等。
    const OWNERSHIP_COLUMNS: [&str; 7] = [
        "owner_principal_id",
        "owner_agent_id",
        "scope_kind",
        "scope_key",
        "visibility",
        "retention",
        "ownership_src",
    ];
    let has_table = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sessions'",
            [],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    };
    let has_col = |conn: &Connection, name: &str| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('sessions') WHERE name=?1",
            [name],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    };
    if !has_table(conn)? {
        return Ok(());
    }
    let mut missing = false;
    for name in OWNERSHIP_COLUMNS {
        if !has_col(conn, name)? {
            missing = true;
            break;
        }
    }
    if !missing {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 进事务后重探：并发首开时，后拿到写锁的一方在这里逐列看到已补的列。
    if has_table(&tx)? {
        for name in OWNERSHIP_COLUMNS {
            if !has_col(&tx, name)? {
                tx.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {name} TEXT"))?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// 只迁移水位库，返回"被归零的 distill 游标数"（节点入口也要报这条迁移提示）。
/// **backfill 在 apply 之前**：理由与 `migrate_all` 同一条，两次崩在不同的位置都能自愈。
pub(crate) fn migrate_ledger(l: &LedgerHandle) -> Result<usize, StoreError> {
    let reset = backfill_ledger_v4(l.conn())?;
    backfill_ledger_fingerprint(l.conn())?;
    backfill_ledger_prompt_version(l.conn())?;
    // P2：`extract_pass_state.pass_id` 的 CHECK 多了 `state`（CHECK 只能重建表才能改）。
    // 波三修 1：再加 `lesson`（第 6 pass 的终态行）。探针取该表 CHECK 的 canonical 全文：
    // 在 ⇒ 已是新形状；不在 ⇒ 重建。
    widen_closed_set_check(
        l.conn(),
        LEDGER_SQL,
        "extract_pass_state",
        &["CHECK(pass_id IN ('naming','fact','event','time','state','lesson'))"],
    )?;
    apply(l.conn(), LEDGER_SQL, LEDGER_VERSION, "ledger.db")?;
    Ok(reset)
}

/// v3→v4：列改名 + 只归零 distill 游标。**列存在性驱动**，不看 `user_version`——
/// 版本号与真实列形状可能因旧实现的崩溃窗口而分叉（v4 + 旧列），此时按列自愈。
///
/// `job_watermark` 不存在或没有旧列 ⇒ fresh 库 / 已迁移库，直返 0。
/// 否则开一个 IMMEDIATE 写事务（句柄只给 `&Connection`，故用等价构造
/// `Transaction::new_unchecked`；它发 `BEGIN IMMEDIATE`、drop 即回滚）：
/// 事务内**重新探测**列，两个进程同时首开时只有一个能看到旧列并完成改名 + 归零。
/// 旧列在、新列也在（半迁移坏库）⇒ 不 rename，继续归零——新列已是读侧唯一真相。
fn backfill_ledger_v4(conn: &Connection) -> Result<usize, StoreError> {
    let has_col = |conn: &Connection, table: &str, column: &str| -> Result<bool, StoreError> {
        let mut st = conn.prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1"))?;
        let n: i64 = st.query_row([column], |r| r.get(0)).unwrap_or(0);
        Ok(n == 1)
    };
    if !has_col(conn, "job_watermark", "cursor_created_at_ms")? {
        return Ok(0);
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 进事务后重探：并发首开时，后拿到写锁的一方在这里看到旧列已不在。
    if !has_col(&tx, "job_watermark", "cursor_created_at_ms")? {
        tx.commit()?;
        return Ok(0);
    }
    if !has_col(&tx, "job_watermark", "cursor_at_ms")? {
        tx.execute_batch(
            "ALTER TABLE job_watermark RENAME COLUMN cursor_created_at_ms TO cursor_at_ms")?;
    }
    // 只重置 distill：derive / anomaly_scan 的轴没有变，重置它们等于白烧一轮推导。
    // 先数**非空游标**再归零：空游标本就没有旧轴值，不该进迁移提示的条数。
    let n: i64 = tx.query_row(
        "SELECT count(*) FROM job_watermark WHERE family='distill' AND cursor_turn_uid != ''",
        [], |r| r.get(0))?;
    tx.execute(
        "UPDATE job_watermark SET cursor_at_ms=0, cursor_turn_uid='' WHERE family='distill'",
        [],
    )?;
    tx.commit()?;
    Ok(n as usize)
}

/// S1：给 `extract_state` 补 `extraction_fingerprint`。**列存在性驱动**，不看 `user_version`——
/// 本任务不 bump `LEDGER_VERSION`，所以版本号在这条迁移上不是信号（旧库的版本号已经是 7）。
///
/// 表不存在（fresh 库）⇒ 直返：`apply` 会用新形状建表，`ALTER` 在这里会撞"no such table"。
/// 与 `backfill_ledger_v4` 同形：开一个 IMMEDIATE 写事务、事务内**重探**，两个进程同时首开时
/// 只有一个能补列；补列本身是原子的，崩在任何位置都可重放。
fn backfill_ledger_fingerprint(conn: &Connection) -> Result<(), StoreError> {
    let has_table = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='extract_state'",
            [], |r| r.get(0))?;
        Ok(n == 1)
    };
    let has_col = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('extract_state') \
             WHERE name='extraction_fingerprint'",
            [], |r| r.get(0))?;
        Ok(n == 1)
    };
    if !has_table(conn)? || has_col(conn)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 进事务后重探：并发首开时，后拿到写锁的一方在这里看到列已补。
    if has_table(&tx)? && !has_col(&tx)? {
        tx.execute_batch(
            "ALTER TABLE extract_state ADD COLUMN extraction_fingerprint TEXT")?;
    }
    tx.commit()?;
    Ok(())
}

/// P0-2：给 `extract_pass_state` 补 `prompt_version`（改判据后旧轮自动可重跑的信号）。
/// **列存在性驱动**，不看 `user_version`——本切片不 bump `LEDGER_VERSION`，版本号在这里
/// 不是信号（旧库的版本号已经是当前值）。形状与 [`backfill_ledger_fingerprint`] 逐行同构：
/// 表不存在（fresh 库）⇒ 直返（`apply` 会用新形状建表）；列已在 ⇒ 直返（迁移幂等）。
/// 否则开 IMMEDIATE 写事务、事务内重探，并发首开时只有一个能补列；补列原子，崩在哪都可重放。
/// 新列可空、无 DEFAULT ⇒ 旧行天然是 NULL（= "不知道是哪一版产的"，选批**不**据此重选）。
fn backfill_ledger_prompt_version(conn: &Connection) -> Result<(), StoreError> {
    let has_table = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='extract_pass_state'",
            [], |r| r.get(0))?;
        Ok(n == 1)
    };
    let has_col = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('extract_pass_state') \
             WHERE name='prompt_version'",
            [], |r| r.get(0))?;
        Ok(n == 1)
    };
    if !has_table(conn)? || has_col(conn)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 进事务后重探：并发首开时，后拿到写锁的一方在这里看到列已补。
    if has_table(&tx)? && !has_col(&tx)? {
        tx.execute_batch(
            "ALTER TABLE extract_pass_state ADD COLUMN prompt_version TEXT")?;
    }
    tx.commit()?;
    Ok(())
}

/// 记忆库的版本门（可写与副本**共用**：副本也不能带旧 schema 进来）。
/// `0` = 空库（fresh，从零建）；`6` = canonical 基线；`>6` = 太新，拒绝降级；
/// `1..=5` = 旧 12 表形状，**没有迁移** ⇒ `SchemaRebuildRequired`，由操作员删除/归档后重建。
fn check_memory_version(found: i32) -> Result<(), StoreError> {
    if found > MEMORY_VERSION {
        return Err(StoreError::SchemaTooNew { db: "memory.db", found, supported: MEMORY_VERSION });
    }
    if found > 0 && found < MEMORY_VERSION {
        return Err(StoreError::SchemaRebuildRequired {
            db: "memory.db", found, supported: MEMORY_VERSION,
        });
    }
    Ok(())
}

/// 可写记忆库：先过版本门，再补 H1a/H1c/H4b 的追加列，最后 `apply`（`0`/`6` 都走重放 ⇒ 幂等；
/// 建表全是 `IF NOT EXISTS`）。
/// canonical 是单基线：旧 12 表形状一律拒绝而不是就地迁。`review_item` 的三列与
/// `review_decision` 的 `review_input_sha256` 例外——它们不 bump `MEMORY_VERSION`，
/// 靠列存在性在 v6 库上自愈（与 ledger 的 `extract_pass_state` 先例同一条）。
pub fn migrate_memory(m: &MemoryHandle) -> Result<(), StoreError> {
    if m.is_read_only() {
        return Err(StoreError::ReadOnly("memory.db (replica)"));
    }
    check_memory_version(schema_version(m.conn())?)?;
    // 先补列、后 apply：apply 负责把 fresh 库按新形状建好；旧库建表是 no-op，列只能在这补。
    // `review_item_batch_inflight` 这个部分唯一索引也在 apply 里，所以 batch_key 必须先补。
    backfill_memory_review_columns(m.conn())?;
    // A9（2026-11-07 W34）：`review_item` 的 source 与 pass_id 两个 CHECK 各多了 `report`
    // （消费者报告某条记忆有误的最小入口）。两条 CHECK 的 canonical 全文各是一根探针：
    // 任一缺失即重建——重建走 canonical 全文，一次把两条 CHECK 拓到新形状；
    // "只退回一条"的夹具形状与"两条都旧"的真旧库都能被认出来。
    // 波二刀 B：两条 CHECK 再各加 `reuse`（复用反馈的 failed/mismatch 入复核队列，
    // `source='reuse'` / `pass_id='reuse'`，与 `report` 同一支：引擎 `supported_pass`
    // 不认它 ⇒ 只走人工判定链）。探针同步换新——只改 canonical 不改探针，旧库不会重建。
    widen_closed_set_check(
        m.conn(),
        MEMORY_SQL,
        "review_item",
        &[
            "CHECK(source IN ('distill','rows','report','reuse'))",
            "CHECK(pass_id IN ('naming','fact','event','time','rows','state','report','reuse','lesson'))",
        ],
    )?;
    apply(m.conn(), MEMORY_SQL, MEMORY_VERSION, "memory.db")
}

/// H1a/H1c/H4b：给 `review_item` 补 `claimed_by`/`claim_expires_at_ms`/`batch_key`，
/// 给 `review_decision` 补 `review_input_sha256`。
///
/// **列存在性驱动**，不看 `user_version`（这些切片都不 bump `MEMORY_VERSION`，版本号在这里
/// 不是信号）。表不存在（fresh 库）⇒ 直返：`apply` 会用新形状建表，`ALTER` 在这里会撞
/// "no such table"。与 `backfill_transcript_v5` 同形：开一个 IMMEDIATE 写事务、事务内
/// **逐列重探**，缺哪个补哪个；两个进程同时首开时只有一个能补列，补列本身原子，崩在任何
/// 位置都可重放。review_item 的三列一律可空、不带 DEFAULT ⇒ 旧行天然是 NULL
/// （= 未占用 / 无租约 / 未提供批次身份）；review_decision 的送审指纹是
/// `NOT NULL DEFAULT ''` ⇒ 旧行天然是空串（H4b 之前没有这一列，空串表示"未记录"）。
fn backfill_memory_review_columns(conn: &Connection) -> Result<(), StoreError> {
    // 顺序 = `MEMORY_SQL` 里这些新列的追加顺序。
    backfill_late_columns(
        conn,
        "review_item",
        &[
            ("claimed_by", "TEXT"),
            ("claim_expires_at_ms", "INTEGER"),
            ("batch_key", "TEXT"),
        ],
    )?;
    backfill_late_columns(
        conn,
        "review_decision",
        &[("review_input_sha256", "TEXT NOT NULL DEFAULT ''")],
    )
}

/// 按列存在性给一张表补列（列清单 = `(name, type)`，类型串逐字进 `ALTER`）。
///
/// 表名与列名只来自本文件的字面量（不接外部输入）；表不存在 ⇒ 直返（fresh 库由 `apply`
/// 按新形状建）。IMMEDIATE 事务 + 事务内逐列重探：并发首开/半途崩溃都可重放。
fn backfill_late_columns(
    conn: &Connection,
    table: &str,
    late_columns: &[(&str, &str)],
) -> Result<(), StoreError> {
    let has_table = |conn: &Connection| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    };
    let has_col = |conn: &Connection, name: &str| -> Result<bool, StoreError> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info(?1) WHERE name=?2",
            [table, name],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    };
    if !has_table(conn)? {
        return Ok(());
    }
    let mut missing = false;
    for (name, _) in late_columns {
        if !has_col(conn, name)? {
            missing = true;
            break;
        }
    }
    if !missing {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 进事务后重探：并发首开时，后拿到写锁的一方在这里逐列看到已补的列。
    if has_table(&tx)? {
        for (name, ty) in late_columns {
            if !has_col(&tx, name)? {
                tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"))?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// 副本侧只校验版本，绝不迁移：整替进来的文件必须已经是本产物认得的形状。
/// 旧 schema 的副本同样拒绝——它读到的是 v≤5 的 12 表形状，没有一列对得上 canonical。
pub fn require_memory_schema(m: &MemoryHandle) -> Result<(), StoreError> {
    check_memory_version(schema_version(m.conn())?)
}

/// 只读装配的版本门：**必须恰好是当前版本**，`0`（空文件/未建表）也算读不了。
///
/// 与 [`require_memory_schema`] 的差别只在 `0` 那一格：它在可写路径上意思是"从零建"，
/// 只读装配没有"建"的机会 ⇒ 读到空库只能响亮失败。`> target` 仍是 `SchemaTooNew`
/// （本产物太旧、拒绝降级）；`< target` 用 `Sql` 带一段自解释的文本——不为一个入口新增
/// 变体：`rc-agent` 的 `StoreError` 分型表**逐变体点名、没有 `_` 兜底**，新增变体会在
/// 那个 crate 里强制多一条处置决定，而这一单的射程只有诊断路径。
pub(crate) fn require_current_version(
    conn: &Connection,
    db: &'static str,
    target: i32,
) -> Result<(), StoreError> {
    let found = schema_version(conn)?;
    if found > target {
        return Err(StoreError::SchemaTooNew { db, found, supported: target });
    }
    if found != target {
        return Err(StoreError::Sql(format!(
            "{db} 的 schema 版本 {found} 不是当前的 {target}：只读装配不做迁移，先用可写入口打开一次"
        )));
    }
    Ok(())
}

/// 三库一起核（只读装配用）：`Root::open_readonly` 是唯一调用点。
/// 顺序与 `migrate_all` 一致，只是为了报错时报出同一份"先会话后水位再记忆"的次序。
pub(crate) fn require_all_schemas(
    t: &TranscriptHandle,
    l: &LedgerHandle,
    m: &MemoryHandle,
) -> Result<(), StoreError> {
    require_current_version(t.conn(), "transcript.db", TRANSCRIPT_VERSION)?;
    require_current_version(l.conn(), "ledger.db", LEDGER_VERSION)?;
    require_current_version(m.conn(), "memory.db", MEMORY_VERSION)
}

/// P2：把某张表的 CHECK 加宽到 canonical 的形状——**按 canonical 原文重建这张表**。
///
/// 为什么不能 `ALTER TABLE`：CHECK 是表定义的一部分，SQLite 没有"改约束"的语句。
/// 为什么不走 `user_version`：`apply` 每次执行全部 SQL，`CREATE TABLE IF NOT EXISTS` 对
/// 已存在的表是 no-op ⇒ 旧库不会因为版本号而拿到新约束。判据因此是**存在性驱动**：
/// 读 `sqlite_master` 里那张表的原文，canonical 的每一条被加宽 CHECK 全文都在就直返，
/// 缺任一条才重建。
///
/// 形状与 `backfill_late_columns` 同一条纪律：表不存在（fresh 库）⇒ 直返，由 `apply` 按新
/// 形状建；全部写在一个 IMMEDIATE 事务里，事务未 commit ⇒ 回滚后旧表还在，崩在任何位置
/// 都可重放（重建是新表名，旧表在 rename 之前一直可读）。
///
/// 拷数据按**两侧列名的交集**（旧库可能少几列，交集之外的一律不写，新列取 DEFAULT/NULL），
/// 所以补列迁移与本次重建互不依赖顺序。索引按 canonical 原文重建，不自己拼形状。
/// 探针判据：canonical 里该表**每一条**被加宽的 CHECK 全文都在 ⇒ 已是新形状，跳过；
/// 任一缺失 ⇒ 重建。为什么按 CHECK 全文而不按值字面量（`'report'`）：一张表的多条 CHECK
/// 同住一条 CREATE，夹具/真库都可能"这条已加宽、那条没加宽"，值字面量分不清是哪条。
fn all_present(current: &str, needles: &[&str]) -> bool {
    needles.iter().all(|n| current.contains(n))
}

fn widen_closed_set_check(
    conn: &Connection,
    sql: &[&str],
    table: &str,
    needles: &[&str],
) -> Result<(), StoreError> {
    let prefix = format!("CREATE TABLE IF NOT EXISTS {table}(");
    let Some(create) = sql.iter().find(|statement| statement.starts_with(prefix.as_str())) else {
        return Ok(());
    };
    let indexes: Vec<&str> = sql
        .iter()
        .filter(|statement| {
            statement.starts_with("CREATE") && statement.contains(&format!("ON {table}"))
        })
        .copied()
        .collect();
    if !table_exists(conn, table)? {
        return Ok(());
    }
    let current: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        )
        .ok();
    let Some(current) = current else { return Ok(()); };
    if all_present(&current, needles) {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // 事务内重探：两个进程同时首开时只有一个能看到旧约束。
    let again: String = tx.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    if all_present(&again, needles) {
        tx.commit()?;
        return Ok(());
    }
    let staged = format!("{table}__p2");
    let staged_create = create.replacen(
        prefix.as_str(),
        &format!("CREATE TABLE IF NOT EXISTS {staged}("),
        1,
    );
    tx.execute_batch(&staged_create)?;
    let shared = shared_columns(&tx, table, &staged)?;
    if shared.is_empty() {
        return Err(StoreError::Sql(format!("ddl: {table} has no columns in common")));
    }
    let column_list = shared.join(",");
    tx.execute_batch(&format!(
        "INSERT INTO {staged}({column_list}) SELECT {column_list} FROM {table}"
    ))?;
    tx.execute_batch(&format!("DROP TABLE {table}"))?;
    tx.execute_batch(&format!("ALTER TABLE {staged} RENAME TO {table}"))?;
    for statement in &indexes {
        tx.execute_batch(statement)?;
    }
    tx.commit()?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n == 1)
}

/// 两张表的列名交集（保持**新表**的列顺序）。旧库缺的列不拷，新库多的列取默认值。
fn shared_columns(conn: &Connection, old: &str, new: &str) -> Result<Vec<String>, StoreError> {
    let columns = |conn: &Connection, table: &str| -> Result<Vec<String>, StoreError> {
        let mut st = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    };
    let old_columns = columns(conn, old)?;
    let new_columns = columns(conn, new)?;
    Ok(new_columns
        .into_iter()
        .filter(|name| old_columns.iter().any(|other| other == name))
        .collect())
}
