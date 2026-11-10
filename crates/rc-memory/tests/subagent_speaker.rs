//! D 子 agent 接线：turn 级 `speaker` 驱动 `fact.speaker_anchor_id`。
//!
//! 四条判据：
//! - a. turn 带 `speaker` ⇒ fact 的说话人是**该 speaker 的 `kind='agent'` 锚**，锚被创建；
//! - b. turn 不带 `speaker` ⇒ 与 C1 逐字一致（agent 证据 ⇒ 会话自锚）；
//! - c. 同一 speaker 第二次写入不重复建锚（锚幂等、mention 内容寻址）；
//! - 轮级 speaker 优先于 C1 的"按证据轮 role 推导"（human 轮也不会把它拉回主人锚）。
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::OwnerCtx;
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_v4::{ensure_agent_anchor, record_turn_speakers};
use rc_memory::{BatchCtx, MemTable};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_500_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SOURCE_AGENT: &str = "qoder";
const SUBAGENT: &str = "subagent:call-01";

fn uid(n: usize) -> String {
    format!("{:064x}", 0x71 + n)
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-subagent".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: "dev-subagent".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx::synthetic(SOURCE_AGENT)
}

fn count(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 主体锚（naming pass 先写的那一格；id 与断言无关）。
fn seed_subject(m: &MemoryHandle, canonical: &str) -> String {
    let id = rc_memory::compute_mem_id(MemTable::Anchor, "person", canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person',?2,'real',NULL,'project','proj-subagent','active',NULL,?3,?3,?4)",
            rusqlite::params![id, canonical, SRC_MS,
                              rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
    id
}

fn fact_item(subject: &str, evidence: Vec<(String, AnchorRole)>) -> FactWriteItem {
    FactWriteItem {
        subject_anchor_id: subject.to_string(),
        object_anchor_id: None,
        predicate: "decide".into(),
        value: Some("把发布推迟一天".into()),
        category: "decision".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: String::new(),
        evidence,
        text: "决定把发布推迟一天".into(),
        scope: scope(),
        source_ms: SRC_MS,
    }
}

fn speaker_of(m: &MemoryHandle, subject: &str) -> Option<String> {
    m.conn()
        .query_row(
            "SELECT speaker_anchor_id FROM fact WHERE subject_anchor_id=?1",
            [subject],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

fn anchor_of(m: &MemoryHandle, canonical: &str) -> (String, String) {
    m.conn()
        .query_row(
            "SELECT id, kind FROM anchor WHERE canonical=?1 AND scope_kind='project'
                AND scope_key='proj-subagent'",
            [canonical],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap()
}

// ── a. turn 带 speaker ⇒ 子 agent 锚 ─────────────────────────────────────────

#[test]
fn turn带speaker_fact的speaker是子agent锚且锚被创建() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = seed_subject(m, "张三");

    let written = record_turn_speakers(
        m,
        &ctx(),
        &owner(),
        &scope(),
        &[(uid(1), SUBAGENT.to_string())],
        NOW,
    )
    .unwrap();
    assert_eq!(written, 1, "一条带 speaker 的轮应写一条轮级 mention");

    let (anchor_id, kind) = anchor_of(m, SUBAGENT);
    assert_eq!(kind, "agent", "子 agent 只能落 kind=agent 锚");

    let item = fact_item(&subject, vec![(uid(1), AnchorRole::Agent)]);
    put_fact_rows(m, std::slice::from_ref(&item), &ctx(), &owner(), NOW).unwrap();
    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(anchor_id.as_str()),
        "fact 的说话人必须是子 agent 锚，不是会话自锚"
    );
}

// ── b. turn 不带 speaker ⇒ C1 原样 ──────────────────────────────────────────

#[test]
fn turn不带speaker_仍按c1取会话自锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let self_anchor = ensure_agent_anchor(m, &ctx(), &owner(), &scope(), NOW)
        .unwrap()
        .id;
    let subject = seed_subject(m, "张三");

    let item = fact_item(&subject, vec![(uid(2), AnchorRole::Agent)]);
    put_fact_rows(m, std::slice::from_ref(&item), &ctx(), &owner(), NOW).unwrap();
    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(self_anchor.as_str()),
        "没有轮级 speaker 时，agent 证据仍落会话自锚（C1 行为一字不改）"
    );
}

// ── c. 幂等：同一 speaker 不重复建锚 ────────────────────────────────────────

#[test]
fn 同一speaker第二次写入不重复建锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();

    let turns = [(uid(3), SUBAGENT.to_string())];
    assert_eq!(record_turn_speakers(m, &ctx(), &owner(), &scope(), &turns, NOW).unwrap(), 1);
    // 不同轮、同一 speaker：锚必须复用。
    let turns2 = [(uid(4), SUBAGENT.to_string())];
    assert_eq!(record_turn_speakers(m, &ctx(), &owner(), &scope(), &turns2, NOW).unwrap(), 1);
    // 同一轮重放：mention 内容寻址，不增行。
    assert_eq!(record_turn_speakers(m, &ctx(), &owner(), &scope(), &turns, NOW).unwrap(), 0);

    assert_eq!(
        count(
            m,
            "SELECT count(*) FROM anchor WHERE kind='agent' AND canonical='subagent:call-01'"
        ),
        1,
        "同一 speaker 只有一格 agent 锚"
    );
    assert_eq!(
        count(m, "SELECT count(*) FROM mention WHERE discourse_role='speaker'"),
        2,
        "两条不同的轮各一条 mention；同轮重放不增行"
    );
}

// ── 优先级：轮级 speaker 高于 role 推导 ─────────────────────────────────────

#[test]
fn 轮级speaker优先于human证据的主人锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = seed_subject(m, "张三");
    // 主人身份已知：C1 会把"任一 human"判成主人锚；轮级 speaker 必须赢。
    let principal_id = "p-subagent";
    rc_memory::writers_v4::ensure_principal_anchor(
        m,
        principal_id,
        Some("某人"),
        &OwnerCtx {
            owner_principal_id: Some(principal_id.into()),
            owner_agent_id: SOURCE_AGENT.into(),
            visibility: rc_memory::owner::Visibility::Project,
            retention: "default".into(),
        },
        &scope(),
        NOW,
    )
    .unwrap();

    record_turn_speakers(
        m,
        &ctx(),
        &owner(),
        &scope(),
        &[(uid(5), SUBAGENT.to_string())],
        NOW,
    )
    .unwrap();
    let (anchor_id, _) = anchor_of(m, SUBAGENT);

    let item = fact_item(
        &subject,
        vec![(uid(5), AnchorRole::Human), (uid(6), AnchorRole::Agent)],
    );
    put_fact_rows(
        m,
        std::slice::from_ref(&item),
        &ctx(),
        &OwnerCtx {
            owner_principal_id: Some(principal_id.into()),
            owner_agent_id: SOURCE_AGENT.into(),
            visibility: rc_memory::owner::Visibility::Project,
            retention: "default".into(),
        },
        NOW,
    )
    .unwrap();
    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(anchor_id.as_str()),
        "轮级 speaker 高于按证据轮 role 的推导"
    );
}
