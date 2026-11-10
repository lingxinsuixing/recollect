//! 声明式列归属表（守卫 11）：每列一个写入者，未登记 ⇒ 默认拒绝。
//! 列序 = `rc_store::ddl::MEMORY_SQL` 的列序；注释里的 P/X/A/C 就是 `Actor` 四值。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor { Program, Extractor, Center, Agent }

#[derive(Debug, Clone, Copy)]
pub struct ColumnOwner {
    pub table: &'static str,
    pub column: &'static str,
    pub actor: Actor,
}

/// 不含索引层（守卫 2：影子表名在 rc-index 之外不可命名）。那一层在 `sqlite_master` 里
/// 是 1 + 5 + 1 个条目——`rc_shadow` 这张 fts5 虚表、它自带的五个 `rc_shadow_*` 伴生条目
/// （`_config`/`_content`/`_data`/`_docsize`/`_idx`）、再加 `rc_index_marker` 这一个 marker，
/// 都不是归属表里的关系。顺序必须按表名字典序——
/// `tests\ownership.rs` 方向②拿它跟 `sqlite_master ... ORDER BY name` 的现算清单逐位对拍。
/// canonical 基线（32 表 = 冻结的 25 表 + S1b-C2a 的 resource_owner/resource_grant 归属侧表
/// 再加 S3a 的 review_decision/review_item 审查侧表，再加 W15a 的 card_verdict_event
/// 背书事件表，再加 G1 刀 B 的 card_sink 归档章表，再加波二刀 A 的 card_reuse_event
/// 复用事件表）：前 25 表逐字照
/// `design/2026-10-24-memory-schema.md` §2 的冻结字典，归属侧表照
/// `design/2026-10-26-ownership-persistence.md` §2.1（资源生命周期数据，不含记忆内容），
/// 审查侧表照 `design/2026-10-26-review-queue-s3.md` §3.1/§3.2。
///
/// `card_sink` 为什么进清单（刀 A 时它是模块私有侧表）：章是**消费面状态**，而
/// `hub::restore` 按这张清单逐表整替——清单外的表恢复后回到空库形状。程序章可回填再生，
/// LLM 价值轴章（`reason='value_axis'`）不可再生，必须随备份恢复。
///
/// `card_reuse_event` 为什么进清单（波二刀 A）：复用反馈是**使用质量账**，一次都没法
/// 程序回填（"这张卡帮没帮上忙"只发生过一次）。同一条理由：不进清单，restore 之后
/// 全部反馈行蒸发。它与 `card_verdict_event` 同表族不同轴（背书=真假，复用=有没有用），
/// 因此是**新表**而不是那张表多一列。
///
/// 例外（不进这张清单）：§7.3a 的 `review_verdict` 是审查侧的**证据附件**，只由
/// `crate::verdict` 写、只给 §7.3 的重跑队列读。它进清单会同时改到 `hub::rows` 的白名单/
/// `INSERT_ORDER` 与 `hub::restore` 的整替面——那是另一条工单的面；DDL 因此住在
/// `crate::verdict`（唯一能命名这张表的模块），写入面由该文件的 API 钉死。
/// 它是**可重跑的证据**，与 card_sink 的消费面状态性质不同，所以例外只留给它。
pub const MEMORY_TABLES: &[&str] = &[
    "alias", "anchor", "anchor_merge", "attention", "attestation", "attribute",
    "card", "card_reuse_event", "card_sink", "card_verdict_event", "embedding", "episode",
    "episode_fact", "extraction_batch", "extraction_issue", "extraction_run", "fact", "fact_link",
    "memory_mention", "memory_op", "memory_stats", "mention", "provenance", "relation",
    "resource_grant", "resource_owner", "retirement", "review_decision",
    "review_item", "state_assertion", "time_expr", "utterance",
];

const fn o(table: &'static str, column: &'static str, actor: Actor) -> ColumnOwner {
    ColumnOwner { table, column, actor }
}

/// 每一张 canonical 表的每一列恰好一条登记。分配规则（T2 派发档）：
/// 内容 X、身份/时间/范围/版本 P、判定 C；沿用旧表已有的 Actor 分配
/// （card/provenance/attestation/retirement/attention），新表按同一把尺子展开。
pub const OWNERSHIP: &[ColumnOwner] = &[
    // alias：锚的别名。FK 与出处轮归 P，称呼与有效期归 X。
    o("alias", "anchor_id", Actor::Program),        o("alias", "alias", Actor::Extractor),
    o("alias", "valid_from_ms", Actor::Extractor),  o("alias", "valid_to_ms", Actor::Extractor),
    o("alias", "source_turn_uid", Actor::Extractor),
    // anchor：本体锚。身份/范围/生命周期归 P；kind/canonical/reality_status/profile 是内容。
    o("anchor", "id", Actor::Program),              o("anchor", "kind", Actor::Extractor),
    o("anchor", "canonical", Actor::Extractor),     o("anchor", "reality_status", Actor::Extractor),
    o("anchor", "world_id", Actor::Program),        o("anchor", "scope_kind", Actor::Program),
    o("anchor", "scope_key", Actor::Program),       o("anchor", "status", Actor::Program),
    o("anchor", "profile_text", Actor::Extractor),  o("anchor", "created_at_ms", Actor::Program),
    o("anchor", "updated_at_ms", Actor::Program),   o("anchor", "norm_version", Actor::Program),
    // anchor_merge：合并审计，全列归 P。
    o("anchor_merge", "id", Actor::Program),        o("anchor_merge", "from_id", Actor::Program),
    o("anchor_merge", "to_id", Actor::Program),     o("anchor_merge", "run_id", Actor::Program),
    o("anchor_merge", "reason", Actor::Program),    o("anchor_merge", "created_at_ms", Actor::Program),
    // attention 7 列全归 C（写侧 = 维护批）。
    o("attention", "id", Actor::Center),            o("attention", "kind", Actor::Center),
    o("attention", "mem_table", Actor::Center),     o("attention", "mem_id", Actor::Center),
    o("attention", "created_at_ms", Actor::Center), o("attention", "resolved_at_ms", Actor::Center),
    o("attention", "rechecked_at_ms", Actor::Center),
    // attestation：表态内容归 A，身份/时间归 P（沿用旧分配）。
    o("attestation", "id", Actor::Program),     o("attestation", "mem_table", Actor::Program),
    o("attestation", "mem_id", Actor::Program), o("attestation", "stance", Actor::Agent),
    o("attestation", "actor", Actor::Program),  o("attestation", "anchor_turn_uid", Actor::Agent),
    o("attestation", "reason", Actor::Agent),   o("attestation", "created_at_ms", Actor::Program),
    o("attestation", "resolved_at_ms", Actor::Program),
    // attribute：一元键值。owner 身份/范围/版本归 P，键值内容与有效期归 X。
    o("attribute", "id", Actor::Program),           o("attribute", "anchor_id", Actor::Program),
    o("attribute", "key", Actor::Extractor),        o("attribute", "value_json", Actor::Extractor),
    o("attribute", "value_text", Actor::Extractor), o("attribute", "value_type", Actor::Extractor),
    o("attribute", "unit", Actor::Extractor),       o("attribute", "valid_from_ms", Actor::Extractor),
    o("attribute", "valid_to_ms", Actor::Extractor), o("attribute", "superseded_by", Actor::Program),
    o("attribute", "confidence", Actor::Extractor), o("attribute", "mode", Actor::Extractor),
    o("attribute", "text", Actor::Extractor),       o("attribute", "world_id", Actor::Program),
    o("attribute", "scope_kind", Actor::Program),   o("attribute", "scope_key", Actor::Program),
    o("attribute", "source_agent", Actor::Program), o("attribute", "device_id", Actor::Program),
    o("attribute", "created_at_ms", Actor::Program), o("attribute", "norm_version", Actor::Program),
    // card：消费行（旧 card/event/assertion 的合并落点）。身份/范围/版本/时间归 P，
    // 内容与 event 独有的 causal_type/时间锚列归 X，verdict 归 C（沿用旧分配）。
    o("card", "id", Actor::Program),                o("card", "kind", Actor::Extractor),
    o("card", "title", Actor::Extractor),           o("card", "text", Actor::Extractor),
    o("card", "time_json", Actor::Extractor),       o("card", "pointers_json", Actor::Extractor),
    o("card", "scope_kind", Actor::Program),        o("card", "scope_key", Actor::Program),
    o("card", "world_id", Actor::Program),          o("card", "as_of_ms", Actor::Program),
    o("card", "valid_to_ms", Actor::Extractor),     o("card", "superseded_by", Actor::Program),
    o("card", "confidence", Actor::Extractor),      o("card", "mode", Actor::Extractor),
    o("card", "verdict", Actor::Center),            o("card", "cooldown_until_ms", Actor::Program),
    o("card", "causal_type", Actor::Extractor),     o("card", "time_anchor_ms", Actor::Extractor),
    o("card", "time_anchor_id", Actor::Extractor),  o("card", "time_confidence", Actor::Extractor),
    o("card", "source_agent", Actor::Program),      o("card", "device_id", Actor::Program),
    o("card", "created_at_ms", Actor::Program),     o("card", "norm_version", Actor::Program),
    // card_reuse_event（波二刀 A）：复用反馈的**使用质量账**。账列（行 id/指向的卡/时间/
    // 版本）归 P；反馈内容（`outcome`/`note`/`by_agent`）归 C——"帮没帮上忙"是裁定，
    // 与 `card.verdict`/`card_sink.sink` 同一把尺子（都是消费面的判定，不是记忆正文）。
    o("card_reuse_event", "id", Actor::Program),
    o("card_reuse_event", "card_id", Actor::Program),
    o("card_reuse_event", "outcome", Actor::Center),
    o("card_reuse_event", "note", Actor::Center),
    o("card_reuse_event", "at_ms", Actor::Program),
    o("card_reuse_event", "by_agent", Actor::Center),
    o("card_reuse_event", "norm_version", Actor::Program),
    // card_sink（G1 刀 B）：召回消费面的**归档章**。身份/时间/版本归 P；判定本身
    // （`sink`/`reason`）归 C——与 `card.verdict`/`card_verdict_event.verdict` 同一把尺子
    // （章是"这条还进不进召回主循环"的裁定，不是记忆内容）。两族 reason 同表：
    // `program_gate`（程序预闸）与 `value_axis`（审核 LLM 价值轴），先写者赢。
    o("card_sink", "card_id", Actor::Program),
    o("card_sink", "sink", Actor::Center),
    o("card_sink", "reason", Actor::Center),
    o("card_sink", "at_ms", Actor::Program),
    o("card_sink", "norm_version", Actor::Program),
    // card_verdict_event（W15a）：判决的**背书来源**。四条账列归 P；判定本身
    // （`verdict`/`source`）归 C——与 `card.verdict` 同一个写入者（writers_c）。
    o("card_verdict_event", "id", Actor::Program),
    o("card_verdict_event", "card_id", Actor::Program),
    o("card_verdict_event", "verdict", Actor::Center),
    o("card_verdict_event", "source", Actor::Center),
    o("card_verdict_event", "at_ms", Actor::Program),
    o("card_verdict_event", "norm_version", Actor::Program),
    // embedding：向量索引，全列归 P。
    o("embedding", "mem_table", Actor::Program),    o("embedding", "mem_id", Actor::Program),
    o("embedding", "model", Actor::Program),        o("embedding", "dim", Actor::Program),
    o("embedding", "vector", Actor::Program),       o("embedding", "norm_version", Actor::Program),
    o("embedding", "created_at_ms", Actor::Program),
    // episode：叙事分组。标题/叙事/时间/参与者归 X；生命周期与范围归 P。
    o("episode", "id", Actor::Program),             o("episode", "title", Actor::Extractor),
    o("episode", "narrative", Actor::Extractor),    o("episode", "start_ms", Actor::Extractor),
    o("episode", "end_ms", Actor::Extractor),       o("episode", "participants_json", Actor::Extractor),
    o("episode", "status", Actor::Program),         o("episode", "world_id", Actor::Program),
    o("episode", "scope_kind", Actor::Program),     o("episode", "scope_key", Actor::Program),
    o("episode", "source_agent", Actor::Program),   o("episode", "device_id", Actor::Program),
    o("episode", "created_at_ms", Actor::Program),  o("episode", "norm_version", Actor::Program),
    o("episode", "superseded_by", Actor::Program),
    // episode_fact：叙事成员，全列归 P。
    o("episode_fact", "episode_id", Actor::Program), o("episode_fact", "fact_id", Actor::Program),
    o("episode_fact", "ord", Actor::Program),
    // extraction_batch：抽取流水审计。身份/时间归 P，pass/原始输出归 X。
    o("extraction_batch", "id", Actor::Program),      o("extraction_batch", "run_id", Actor::Program),
    o("extraction_batch", "pass", Actor::Extractor),  o("extraction_batch", "attempt", Actor::Extractor),
    o("extraction_batch", "raw_json", Actor::Extractor), o("extraction_batch", "schema_ok", Actor::Extractor),
    o("extraction_batch", "errors_json", Actor::Extractor), o("extraction_batch", "created_at_ms", Actor::Program),
    // extraction_issue：问题审计。resolved 是判定戳。
    o("extraction_issue", "id", Actor::Program),      o("extraction_issue", "run_id", Actor::Program),
    o("extraction_issue", "pass", Actor::Extractor),  o("extraction_issue", "code", Actor::Extractor),
    o("extraction_issue", "detail", Actor::Extractor), o("extraction_issue", "resolved_at_ms", Actor::Center),
    o("extraction_issue", "created_at_ms", Actor::Program),
    // extraction_run：一次抽取的元数据。status/时间/版本归 P，数据集与模型输出归 X。
    o("extraction_run", "run_id", Actor::Program),       o("extraction_run", "dataset", Actor::Extractor),
    o("extraction_run", "turn_start", Actor::Extractor), o("extraction_run", "turn_end", Actor::Extractor),
    o("extraction_run", "model", Actor::Extractor),      o("extraction_run", "prompt_version", Actor::Extractor),
    o("extraction_run", "schema_version", Actor::Program), o("extraction_run", "status", Actor::Program),
    o("extraction_run", "started_at_ms", Actor::Program), o("extraction_run", "finished_at_ms", Actor::Program),
    o("extraction_run", "usage_json", Actor::Extractor), o("extraction_run", "notes", Actor::Extractor),
    // fact：本体事实（M3 才写入）。subject/object/place/speaker 的 id 列与谓词/值归 X。
    o("fact", "id", Actor::Program),                    o("fact", "subject_anchor_id", Actor::Extractor),
    o("fact", "predicate", Actor::Extractor),           o("fact", "object_anchor_id", Actor::Extractor),
    o("fact", "value_json", Actor::Extractor),          o("fact", "value_text", Actor::Extractor),
    o("fact", "category", Actor::Extractor),            o("fact", "participants_json", Actor::Extractor),
    o("fact", "place_anchor_id", Actor::Extractor),     o("fact", "speaker_anchor_id", Actor::Extractor),
    o("fact", "occurred_at_ms", Actor::Extractor),      o("fact", "occurred_end_ms", Actor::Extractor),
    o("fact", "speech_time_ms", Actor::Extractor),      o("fact", "valid_from_ms", Actor::Extractor),
    o("fact", "valid_to_ms", Actor::Extractor),         o("fact", "due_at_ms", Actor::Extractor),
    o("fact", "status", Actor::Extractor),              o("fact", "mode", Actor::Extractor),
    o("fact", "confidence", Actor::Extractor),          o("fact", "text", Actor::Extractor),
    o("fact", "evidence_turn_uid", Actor::Extractor),   o("fact", "superseded_by", Actor::Program),
    o("fact", "world_id", Actor::Program),              o("fact", "scope_kind", Actor::Program),
    o("fact", "scope_key", Actor::Program),             o("fact", "source_agent", Actor::Program),
    o("fact", "device_id", Actor::Program),             o("fact", "created_at_ms", Actor::Program),
    o("fact", "norm_version", Actor::Program),
    // fact_link：记忆行之间的边。端点存 "<table>:<id>" 键；链接内容归 X，判定者归 C。
    o("fact_link", "id", Actor::Program),            o("fact_link", "from_id", Actor::Extractor),
    o("fact_link", "to_id", Actor::Extractor),       o("fact_link", "relation", Actor::Extractor),
    o("fact_link", "basis", Actor::Extractor),       o("fact_link", "evidence_json", Actor::Extractor),
    o("fact_link", "decided_by", Actor::Center),     o("fact_link", "scope_kind", Actor::Program),
    o("fact_link", "scope_key", Actor::Program),     o("fact_link", "world_id", Actor::Program),
    o("fact_link", "source_agent", Actor::Program),  o("fact_link", "device_id", Actor::Program),
    o("fact_link", "created_at_ms", Actor::Program), o("fact_link", "norm_version", Actor::Program),
    // memory_mention：记忆行 → 锚 的派生索引。身份/范围归 P，被指锚归 X。
    o("memory_mention", "mem_table", Actor::Program), o("memory_mention", "mem_id", Actor::Program),
    o("memory_mention", "anchor_id", Actor::Extractor), o("memory_mention", "source_agent", Actor::Program),
    o("memory_mention", "device_id", Actor::Program), o("memory_mention", "created_at_ms", Actor::Program),
    o("memory_mention", "norm_version", Actor::Program),
    // memory_op：维护审计。身份/actor/时间归 P，操作内容归 X。
    o("memory_op", "id", Actor::Program),            o("memory_op", "run_id", Actor::Program),
    o("memory_op", "op", Actor::Extractor),          o("memory_op", "mem_table", Actor::Program),
    o("memory_op", "mem_id", Actor::Program),        o("memory_op", "before_json", Actor::Extractor),
    o("memory_op", "after_json", Actor::Extractor),  o("memory_op", "reason", Actor::Extractor),
    o("memory_op", "actor", Actor::Program),         o("memory_op", "created_at_ms", Actor::Program),
    // memory_stats：使用侧统计，全列归 P（派生读数，不是内容）。
    o("memory_stats", "mem_table", Actor::Program),          o("memory_stats", "mem_id", Actor::Program),
    o("memory_stats", "occurrence_count", Actor::Program),   o("memory_stats", "mention_count", Actor::Program),
    o("memory_stats", "access_count", Actor::Program),       o("memory_stats", "distinct_session_count", Actor::Program),
    o("memory_stats", "distinct_speaker_count", Actor::Program), o("memory_stats", "first_mentioned_ms", Actor::Program),
    o("memory_stats", "last_mentioned_ms", Actor::Program),  o("memory_stats", "last_accessed_ms", Actor::Program),
    o("memory_stats", "emotional_intensity", Actor::Program), o("memory_stats", "open_loop_weight", Actor::Program),
    o("memory_stats", "salience", Actor::Program),           o("memory_stats", "salience_updated_ms", Actor::Program),
    o("memory_stats", "muted", Actor::Program),
    // mention：轮级指称。指称内容归 X，身份/范围/版本归 P。
    o("mention", "id", Actor::Program),              o("mention", "anchor_id", Actor::Program),
    o("mention", "turn_uid", Actor::Extractor),      o("mention", "surface", Actor::Extractor),
    o("mention", "span_start", Actor::Extractor),    o("mention", "span_end", Actor::Extractor),
    o("mention", "speaker_anchor_id", Actor::Extractor), o("mention", "discourse_role", Actor::Extractor),
    o("mention", "resolution", Actor::Extractor),    o("mention", "confidence", Actor::Extractor),
    o("mention", "source_agent", Actor::Program),    o("mention", "device_id", Actor::Program),
    o("mention", "created_at_ms", Actor::Program),   o("mention", "norm_version", Actor::Program),
    // provenance 段：出处列归 P，turn_uid 归 X（沿用旧分配）。
    o("provenance", "mem_table", Actor::Program), o("provenance", "mem_id", Actor::Program),
    o("provenance", "turn_uid", Actor::Extractor), o("provenance", "anchor_role", Actor::Program),
    o("provenance", "ref_kind", Actor::Program),
    o("provenance", "ord", Actor::Program),       o("provenance", "role", Actor::Program),
    // relation：锚到锚。subject/object 与谓词归 X，范围/版本归 P。
    o("relation", "id", Actor::Program),                o("relation", "subject_anchor_id", Actor::Extractor),
    o("relation", "predicate", Actor::Extractor),       o("relation", "object_anchor_id", Actor::Extractor),
    o("relation", "valid_from_ms", Actor::Extractor),   o("relation", "valid_to_ms", Actor::Extractor),
    o("relation", "superseded_by", Actor::Program),     o("relation", "mode", Actor::Extractor),
    o("relation", "confidence", Actor::Extractor),      o("relation", "text", Actor::Extractor),
    o("relation", "evidence_turn_uid", Actor::Extractor), o("relation", "world_id", Actor::Program),
    o("relation", "scope_kind", Actor::Program),        o("relation", "scope_key", Actor::Program),
    o("relation", "source_agent", Actor::Program),      o("relation", "device_id", Actor::Program),
    o("relation", "created_at_ms", Actor::Program),     o("relation", "norm_version", Actor::Program),
    // resource_grant：授权关系（S1b-C2a 侧表，无记忆内容）。整张表的每一列都由程序写：
    // 模型侧结构上写不出授权——这是"节点永不许上传归属"的登记面。
    o("resource_grant", "id", Actor::Program),                o("resource_grant", "from_principal_id", Actor::Program),
    o("resource_grant", "from_agent_id", Actor::Program),     o("resource_grant", "to_agent_id", Actor::Program),
    o("resource_grant", "to_principal_id", Actor::Program),   o("resource_grant", "capability", Actor::Program),
    o("resource_grant", "data_class", Actor::Program),        o("resource_grant", "scope_kind", Actor::Program),
    o("resource_grant", "scope_key", Actor::Program),         o("resource_grant", "purpose", Actor::Program),
    o("resource_grant", "granted_at_ms", Actor::Program),     o("resource_grant", "expires_at_ms", Actor::Program),
    o("resource_grant", "revoked_at_ms", Actor::Program),
    // resource_owner：一行记忆的归属（S1b-C2a 侧表，无记忆内容）。全列归 P：
    // owner/visibility/retention 是资源控制权，抽取器与 agent 都写不出来。
    o("resource_owner", "mem_table", Actor::Program),        o("resource_owner", "mem_id", Actor::Program),
    o("resource_owner", "owner_principal_id", Actor::Program), o("resource_owner", "owner_agent_id", Actor::Program),
    o("resource_owner", "created_by_agent", Actor::Program), o("resource_owner", "visibility", Actor::Program),
    o("resource_owner", "retention", Actor::Program),        o("resource_owner", "scope_kind", Actor::Program),
    o("resource_owner", "scope_key", Actor::Program),        o("resource_owner", "source_agent", Actor::Program),
    o("resource_owner", "device_id", Actor::Program),        o("resource_owner", "created_at_ms", Actor::Program),
    // retirement：退场账（沿用旧分配）。
    o("retirement", "mem_table", Actor::Program), o("retirement", "mem_id", Actor::Program),
    o("retirement", "reason", Actor::Center),     o("retirement", "created_at_ms", Actor::Program),
    o("retirement", "resolved_at_ms", Actor::Center),
    // review_decision：只追加判定台账（S3a 审查侧表）。verdict/reason_code 是判定归 C；
    // detail 只放码/计数、来自上游审核输出，归 X；身份/阶段/版本/时间归 P。
    o("review_decision", "id", Actor::Program),            o("review_decision", "item_id", Actor::Program),
    o("review_decision", "stage", Actor::Program),         o("review_decision", "verdict", Actor::Center),
    o("review_decision", "reason_code", Actor::Center),    o("review_decision", "rules_version", Actor::Program),
    o("review_decision", "model", Actor::Program),         o("review_decision", "prompt_version", Actor::Program),
    o("review_decision", "input_sha256", Actor::Program),  o("review_decision", "decider", Actor::Program),
    o("review_decision", "detail", Actor::Extractor),      o("review_decision", "created_at_ms", Actor::Program),
    // H4b：送审输入指纹（与 `input_sha256` 的载荷指纹分列）。由引擎现算、程序写，归 P。
    o("review_decision", "review_input_sha256", Actor::Program),
    // review_item：待审区（S3a 审查侧表）。payload_json 是上游抽取器的内容，归 X；
    // 其余全部是程序侧的身份/键/状态/时间，归 P（owner_json 由服务端推导，不进模型面）。
    o("review_item", "id", Actor::Program),             o("review_item", "source", Actor::Program),
    o("review_item", "pass_id", Actor::Program),        o("review_item", "family", Actor::Program),
    o("review_item", "session_key", Actor::Program),    o("review_item", "device_id", Actor::Program),
    o("review_item", "turn_uids_json", Actor::Program), o("review_item", "owner_json", Actor::Program),
    o("review_item", "payload_json", Actor::Extractor), o("review_item", "payload_sha256", Actor::Program),
    o("review_item", "state", Actor::Program),          o("review_item", "program_code", Actor::Program),
    o("review_item", "conflict_json", Actor::Program),  o("review_item", "attempt", Actor::Program),
    o("review_item", "next_retry_at_ms", Actor::Program), o("review_item", "created_at_ms", Actor::Program),
    o("review_item", "updated_at_ms", Actor::Program),  o("review_item", "applied_at_ms", Actor::Program),
    // H1a：队列占位/租约（claimed_by/claim_expires_at_ms）。写侧只有 claim/release 与
    // 迁移补列，全部由程序写；它们不是记忆内容，归 P。
    o("review_item", "claimed_by", Actor::Program),     o("review_item", "claim_expires_at_ms", Actor::Program),
    // H1c：批次身份（batch_key）。由 `review::batch_key` 现算、`enqueue` 写；不是记忆内容，归 P。
    o("review_item", "batch_key", Actor::Program),
    // state_assertion：带有效期的状态断言。主体/维度/值/证据归 X，范围/版本归 P。
    o("state_assertion", "id", Actor::Program),                 o("state_assertion", "subject_kind", Actor::Extractor),
    o("state_assertion", "subject_anchor_id", Actor::Extractor), o("state_assertion", "subject_relation_id", Actor::Extractor),
    o("state_assertion", "dimension", Actor::Extractor),        o("state_assertion", "value_before", Actor::Extractor),
    o("state_assertion", "value_after", Actor::Extractor),      o("state_assertion", "valid_from_ms", Actor::Extractor),
    o("state_assertion", "valid_to_ms", Actor::Extractor),      o("state_assertion", "superseded_by", Actor::Program),
    o("state_assertion", "cause_fact_id", Actor::Extractor),    o("state_assertion", "text", Actor::Extractor),
    o("state_assertion", "mode", Actor::Extractor),             o("state_assertion", "confidence", Actor::Extractor),
    o("state_assertion", "evidence_turn_uid", Actor::Extractor), o("state_assertion", "world_id", Actor::Program),
    o("state_assertion", "scope_kind", Actor::Program),         o("state_assertion", "scope_key", Actor::Program),
    o("state_assertion", "source_agent", Actor::Program),       o("state_assertion", "device_id", Actor::Program),
    o("state_assertion", "created_at_ms", Actor::Program),      o("state_assertion", "norm_version", Actor::Program),
    // time_expr：时间表达归一审计。身份/时间归 P，表达内容归 X。
    o("time_expr", "id", Actor::Program),            o("time_expr", "turn_uid", Actor::Extractor),
    o("time_expr", "text", Actor::Extractor),        o("time_expr", "start_ms", Actor::Extractor),
    o("time_expr", "end_ms", Actor::Extractor),      o("time_expr", "precision", Actor::Extractor),
    o("time_expr", "basis", Actor::Extractor),       o("time_expr", "kind", Actor::Extractor),
    o("time_expr", "created_at_ms", Actor::Program),
    // utterance：关键原话。说话人/内容归 X，quote_hash/范围/版本归 P。
    o("utterance", "id", Actor::Program),               o("utterance", "speaker_anchor_id", Actor::Extractor),
    o("utterance", "addressee_anchor_id", Actor::Extractor), o("utterance", "act", Actor::Extractor),
    o("utterance", "quote", Actor::Extractor),          o("utterance", "quote_hash", Actor::Program),
    o("utterance", "gist", Actor::Extractor),           o("utterance", "fact_id", Actor::Extractor),
    o("utterance", "turn_uid", Actor::Extractor),       o("utterance", "mode", Actor::Extractor),
    o("utterance", "confidence", Actor::Extractor),     o("utterance", "world_id", Actor::Program),
    o("utterance", "scope_kind", Actor::Program),       o("utterance", "scope_key", Actor::Program),
    o("utterance", "source_agent", Actor::Program),     o("utterance", "device_id", Actor::Program),
    o("utterance", "created_at_ms", Actor::Program),    o("utterance", "norm_version", Actor::Program),
    o("utterance", "superseded_by", Actor::Program),
];
