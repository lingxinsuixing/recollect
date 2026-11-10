//! C1：说话人落库（`fact.speaker_anchor_id` 与 `mention.speaker_anchor_id` 是**程序列**）。
//!
//! 这一片钉四件事：
//! 1. agent 证据轮的 fact/event ⇒ 会话自锚（`ensure_agent_anchor` 那一格）；
//! 2. human 证据轮 + 身份已知 ⇒ 主人锚（`ensure_principal_anchor` 那一格）；
//! 3. human 证据轮 + 身份未知（含 `unknown`）⇒ NULL，且**不造锚**（行数与入场时逐格相同）；
//! 4. mention 的说话人由程序覆盖：人类轮 ⇒ 主人锚、agent 轮 ⇒ 自锚，模型递的 `speaker_ref`
//!    在有程序值时一律不落库。
//!
//! 夹具取向：主人锚与自锚都走**生产的** `ensure_*_anchor`（不自拼 id——写库层那一支比的正是
//! 这两条内容寻址算法算出来的格），person 主体锚与 naming 一样直插（它的 id 与断言无关）。
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, put_fact_rows_with_extras, FactExtras, FactWriteItem};
use rc_memory::writers_v4::{
    ensure_agent_anchor, ensure_principal_anchor, write_a1_with_owner, A1Anchor, A1Batch, A1Mention,
};
use rc_memory::{BatchCtx, Provenance};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_500_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SOURCE_AGENT: &str = "agent-speaker";
const PRINCIPAL: &str = "p-speaker";

fn uid(index: usize) -> String {
    format!("{:064x}", 0x51 + index)
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-speaker".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: "dev-speaker".into() }
}

/// 会话的归属声明。`principal` 就是 `resolve_principal` 的读数（写库层只看得见这一格）。
fn owner(principal: Option<&str>) -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: principal.map(str::to_string),
        owner_agent_id: SOURCE_AGENT.into(),
        visibility: Visibility::Project,
        retention: "default".into(),
    }
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

/// 主体锚（`fact.subject_anchor_id` 的外键目标）：naming pass 先写的那一格。
fn seed_subject(m: &MemoryHandle, canonical: &str) -> String {
    let id = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor, "person", canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person',?2,'real',NULL,'project','proj-speaker','active',NULL,?3,?3,?4)",
            rusqlite::params![id, canonical, SRC_MS,
                              rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
    id
}

fn ensure_self(m: &MemoryHandle) -> String {
    ensure_agent_anchor(m, &ctx(), &owner(None), &scope(), NOW).unwrap().id
}

fn ensure_principal(m: &MemoryHandle, principal_id: &str, name: &str) -> String {
    ensure_principal_anchor(m, principal_id, Some(name), &owner(Some(principal_id)), &scope(), NOW)
        .unwrap()
        .expect("身份已知时必须建主人锚")
        .id
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

// ── ① agent 轮 ⇒ 会话自锚 ─────────────────────────────────────────────────────

#[test]
fn agent证据轮_fact的speaker是会话自锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let self_anchor = ensure_self(m);
    let subject = seed_subject(m, "张三");

    let item = fact_item(&subject, vec![(uid(1), AnchorRole::Agent)]);
    put_fact_rows(m, std::slice::from_ref(&item), &ctx(), &owner(None), NOW).unwrap();

    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(self_anchor.as_str()),
        "agent 轮的说话人就是本 agent：speaker 必须落在自锚那一格"
    );
}

#[test]
fn agent证据轮_event的speaker也是会话自锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let self_anchor = ensure_self(m);
    let subject = seed_subject(m, "张三");

    let mut item = fact_item(&subject, vec![(uid(2), AnchorRole::Agent)]);
    item.category = "event".into();
    item.predicate = "action".into();
    put_fact_rows_with_extras(
        m,
        std::slice::from_ref(&item),
        &[FactExtras::default()],
        &ctx(),
        &owner(None),
        NOW,
    )
    .unwrap();

    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(self_anchor.as_str()),
        "event 与 fact 是同一条写入路径：说话人规则不许分叉"
    );
}

// ── ② human 轮 + 身份已知 ⇒ 主人锚 ───────────────────────────────────────────

#[test]
fn human证据轮_有身份_fact的speaker是主人锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal_anchor = ensure_principal(m, PRINCIPAL, "某人");
    let subject = seed_subject(m, "张三");

    let item = fact_item(&subject, vec![(uid(3), AnchorRole::Human)]);
    put_fact_rows(
        m,
        std::slice::from_ref(&item),
        &ctx(),
        &owner(Some(PRINCIPAL)),
        NOW,
    )
    .unwrap();

    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(principal_anchor.as_str()),
        "human 轮的说话人是会话主人：speaker 必须落在主人锚那一格"
    );
}

#[test]
fn 混证据_human陈述加agent回执_speaker是主人锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal_anchor = ensure_principal(m, PRINCIPAL, "某人");
    let subject = seed_subject(m, "张三");

    // 任一 human ⇒ 主人：这条事实是用户提出来的，agent 的回执只是第二条证据。
    let item = fact_item(
        &subject,
        vec![(uid(4), AnchorRole::Human), (uid(5), AnchorRole::Agent)],
    );
    put_fact_rows(
        m,
        std::slice::from_ref(&item),
        &ctx(),
        &owner(Some(PRINCIPAL)),
        NOW,
    )
    .unwrap();

    assert_eq!(
        speaker_of(m, &subject).as_deref(),
        Some(principal_anchor.as_str())
    );
}

// ── ③ human 轮 + 身份未知 ⇒ NULL，且不造锚 ──────────────────────────────────

#[test]
fn human证据轮_无身份_speaker为null且不造锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let self_anchor = ensure_self(m);
    let subject = seed_subject(m, "张三");
    let anchors_before = count(m, "anchor");

    let item = fact_item(&subject, vec![(uid(6), AnchorRole::Human)]);
    put_fact_rows(m, std::slice::from_ref(&item), &ctx(), &owner(None), NOW).unwrap();

    assert_eq!(speaker_of(m, &subject), None, "没有身份 ⇒ 不猜说话人，也不拿自锚顶替");
    assert_eq!(
        count(m, "anchor"),
        anchors_before,
        "写库层不造锚：无身份的 human 轮不许长出一个无名用户（自锚早就在，不算新锚）"
    );
    assert!(
        m.conn()
            .query_row(
                "SELECT 1 FROM anchor WHERE id=?1",
                [&self_anchor],
                |r| r.get::<_, i64>(0)
            )
            .is_ok(),
        "自锚仍在（它由选批建，不由本层建）"
    );
}

#[test]
fn human证据轮_身份是unknown_speaker为null() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let subject = seed_subject(m, "张三");

    let item = fact_item(&subject, vec![(uid(7), AnchorRole::Human)]);
    put_fact_rows(
        m,
        std::slice::from_ref(&item),
        &ctx(),
        &owner(Some("unknown")),
        NOW,
    )
    .unwrap();

    assert_eq!(speaker_of(m, &subject), None, "章是 unknown ⇒ 不知道是谁，不造无名用户");
}

// ── ④ mention 的说话人由程序写 ───────────────────────────────────────────────

fn anchor_draft(reference: &str, canonical: &str, turn: char, role: AnchorRole) -> A1Anchor {
    A1Anchor {
        reference: reference.into(),
        kind: "person".into(),
        canonical: canonical.into(),
        aliases: Vec::new(),
        reality_status: "real".into(),
        evidence: vec![Provenance { turn_uid: turn.to_string().repeat(64), anchor_role: role }],
        stable: true,
    }
}

fn mention_draft(anchor_ref: &str, turn: char, speaker_ref: &str) -> A1Mention {
    A1Mention {
        anchor_ref: anchor_ref.into(),
        surface: "老张".into(),
        turn_uid: turn.to_string().repeat(64),
        speaker_ref: Some(speaker_ref.into()),
        discourse_role: "mentioned".into(),
        resolution: "resolved".into(),
        confidence: 0.9,
    }
}

fn batch(anchors: Vec<A1Anchor>, mentions: Vec<A1Mention>) -> A1Batch {
    A1Batch { anchors, mentions, issues: Vec::new(), unresolved: Vec::new() }
}

fn mention_speaker(m: &MemoryHandle, anchor_canonical: &str) -> Option<String> {
    m.conn()
        .query_row(
            "SELECT m.speaker_anchor_id FROM mention m JOIN anchor a ON a.id=m.anchor_id
              WHERE a.canonical=?1",
            [anchor_canonical],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

#[test]
fn agent轮mention的speaker被程序覆盖为自锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let self_anchor = ensure_self(m);

    // 模型说"说话人是 a1"（另一个 person 锚）：agent 轮的这一格必须被程序改写。
    let a1 = anchor_draft("a1", "老张", 'a', AnchorRole::Agent);
    let a2 = anchor_draft("a2", "李四", 'b', AnchorRole::Agent);
    write_a1_with_owner(
        m,
        &batch(vec![a1, a2], vec![mention_draft("a1", 'b', "a2")]),
        &ctx(),
        &owner(None),
        &scope(),
        "run-speaker",
        NOW,
    )
    .unwrap();

    assert_eq!(
        mention_speaker(m, "老张").as_deref(),
        Some(self_anchor.as_str()),
        "agent 轮的说话人是本 agent：模型写的 speaker_ref 不落库"
    );
}

#[test]
fn human轮mention的speaker被程序覆盖为主人锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal_anchor = ensure_principal(m, PRINCIPAL, "某人");

    let a1 = anchor_draft("a1", "老张", 'c', AnchorRole::Human);
    let a2 = anchor_draft("a2", "李四", 'd', AnchorRole::Human);
    write_a1_with_owner(
        m,
        &batch(vec![a1, a2], vec![mention_draft("a1", 'd', "a2")]),
        &ctx(),
        &owner(Some(PRINCIPAL)),
        &scope(),
        "run-speaker",
        NOW,
    )
    .unwrap();

    assert_eq!(
        mention_speaker(m, "老张").as_deref(),
        Some(principal_anchor.as_str()),
        "human 轮的说话人是会话主人：模型写的 speaker_ref 不落库"
    );
}
