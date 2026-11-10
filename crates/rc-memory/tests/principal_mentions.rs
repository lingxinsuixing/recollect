//! D31：human 轮正文里的「我 / 我们」→ 主人锚 mention（程序侧，writer 级）。
//!
//! 判据（派发档 §一 / §四）：
//! 1. 「我」⇒ `discourse_role='speaker'`、`resolution='resolved'`、`confidence=1.0`、
//!    `speaker_anchor_id` = 主人锚（它确实是说话人）；
//! 2. 「我们」⇒ `provisional` / `0.7` / `speaker_anchor_id IS NULL`（集合不是权威说话人）；
//! 3. 同轮两个表面 ⇒ 两行；同轮多次「我」⇒ 一行；
//! 4. span 两列一律 NULL；同一批重放 ⇒ 0 行（内容寻址 + `INSERT OR IGNORE`）。
//!
//! 夹具照 `speaker_bind.rs` / `subagent_speaker.rs`：主人锚走**生产的**
//! `ensure_principal_anchor`（断言比的就是那条内容寻址算法算出来的格）。
use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::turn_spoken_by;
use rc_memory::writers_v4::{ensure_principal_anchor, record_principal_mentions};
use rc_memory::BatchCtx;
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_500_000;
const SOURCE_AGENT: &str = "agent-d31";
const PRINCIPAL: &str = "p-d31";

fn uid(n: usize) -> String {
    format!("{:064x}", 0xd3 + n)
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-d31".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: SOURCE_AGENT.into(), device_id: "dev-d31".into() }
}

fn owner(principal: Option<&str>) -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: principal.map(str::to_string),
        owner_agent_id: SOURCE_AGENT.into(),
        visibility: Visibility::Project,
        retention: "default".into(),
    }
}

/// 生产的建锚入口：身份已知时必然返回一格主人锚。
fn ensure_principal(m: &MemoryHandle) -> String {
    ensure_principal_anchor(m, PRINCIPAL, Some("某人"), &owner(Some(PRINCIPAL)), &scope(), NOW)
        .unwrap()
        .expect("身份已知时必须建主人锚")
        .id
}

/// `mention` 的断言面（只取本片关心的列；surface 排序保证「我」在「我们」前）。
struct MentionRow {
    surface: String,
    anchor_id: String,
    discourse_role: String,
    resolution: String,
    confidence: f64,
    speaker_anchor_id: Option<String>,
    span_start: Option<i64>,
    span_end: Option<i64>,
}

fn mentions(m: &MemoryHandle) -> Vec<MentionRow> {
    let mut st = m
        .conn()
        .prepare(
            "SELECT surface, anchor_id, discourse_role, resolution, confidence,
                    speaker_anchor_id, span_start, span_end
               FROM mention
              ORDER BY surface",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok(MentionRow {
            surface: r.get(0)?,
            anchor_id: r.get(1)?,
            discourse_role: r.get(2)?,
            resolution: r.get(3)?,
            confidence: r.get(4)?,
            speaker_anchor_id: r.get(5)?,
            span_start: r.get(6)?,
            span_end: r.get(7)?,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn anchor_kind(m: &MemoryHandle, anchor_id: &str) -> String {
    m.conn()
        .query_row("SELECT kind FROM anchor WHERE id=?1", [anchor_id], |r| r.get(0))
        .unwrap()
}

// ── ① 「我」= resolved 的说话人 mention ──────────────────────────────────────

#[test]
fn 我说_落一条resolved的speaker_mention指向主人锚() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal = ensure_principal(m);

    let turns = [(uid(1), "我下周三把报价单发给你".to_string())];
    let written = record_principal_mentions(m, &ctx(), &turns, &principal, NOW).unwrap();
    assert_eq!(written, 1, "一条 human 轮的「我」应写一行 mention");

    let rows = mentions(m);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.surface, "我", "surface 存原文代词");
    assert_eq!(row.anchor_id, principal, "mention 指向本会话解析出的主人锚");
    assert_eq!(row.discourse_role, "speaker");
    assert_eq!(row.resolution, "resolved");
    assert_eq!(row.confidence, 1.0);
    assert_eq!(
        row.speaker_anchor_id.as_deref(),
        Some(principal.as_str()),
        "「我」确实是说话人"
    );
    assert!(row.span_start.is_none() && row.span_end.is_none(), "span 两列一律 NULL");
    assert_eq!(anchor_kind(m, &principal), "person", "主人锚是 kind=person");
    assert!(
        turn_spoken_by(m, &uid(1), &principal).unwrap(),
        "读侧出口 turn_spoken_by 必须认这一行"
    );
}

// ── ② 「我们」降级：集合不是权威说话人 ─────────────────────────────────────

#[test]
fn 我们说_落一条provisional且speaker为空() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal = ensure_principal(m);

    let turns = [(uid(2), "我们下周再谈".to_string())];
    assert_eq!(
        record_principal_mentions(m, &ctx(), &turns, &principal, NOW).unwrap(),
        1
    );

    let rows = mentions(m);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.surface, "我们");
    assert_eq!(row.anchor_id, principal);
    assert_eq!(row.discourse_role, "speaker");
    assert_eq!(row.resolution, "provisional", "「我们」不能当 resolved 的权威说话人");
    assert_eq!(row.confidence, 0.7);
    assert!(
        row.speaker_anchor_id.is_none(),
        "「我们」是集合：speaker_anchor_id 必须 NULL（也避免放宽闸 6c 第 3 支）"
    );
    assert!(
        !turn_spoken_by(m, &uid(2), &principal).unwrap(),
        "「我们」不该被读成主人一个人在说"
    );
}

// ── ③ 同轮两个表面 ⇒ 两行；同轮多次「我」⇒ 一行 ──────────────────────────

#[test]
fn 同轮我和我们_两行都写() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal = ensure_principal(m);

    let turns = [(uid(3), "我先把报价单发你，我们再约时间".to_string())];
    assert_eq!(
        record_principal_mentions(m, &ctx(), &turns, &principal, NOW).unwrap(),
        2,
        "两个不同表面 ⇒ 两行"
    );

    let rows = mentions(m);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].surface, "我");
    assert_eq!(rows[1].surface, "我们");
    for row in &rows {
        assert_eq!(row.anchor_id, principal);
    }
    assert_eq!(rows[0].resolution, "resolved");
    assert_eq!(rows[1].resolution, "provisional");
}

#[test]
fn 同轮多次我_仍只有一行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal = ensure_principal(m);

    let turns = [(uid(4), "我明天来，我也带上合同".to_string())];
    assert_eq!(
        record_principal_mentions(m, &ctx(), &turns, &principal, NOW).unwrap(),
        1
    );
    assert_eq!(mentions(m).len(), 1, "同轮同锚同表面只合成一行");
}

// ── ④ 幂等：同一批重放 0 行 ────────────────────────────────────────────────

#[test]
fn 同一批重放_返回0且总行数不变() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal = ensure_principal(m);

    let turns = [(uid(5), "我下周把报价单发给你".to_string())];
    assert_eq!(
        record_principal_mentions(m, &ctx(), &turns, &principal, NOW).unwrap(),
        1
    );
    assert_eq!(
        record_principal_mentions(m, &ctx(), &turns, &principal, NOW).unwrap(),
        0,
        "内容寻址 + INSERT OR IGNORE：重放不增行"
    );
    assert_eq!(mentions(m).len(), 1);
}
