//! C6b：抽取输入渲染 ±1 邻轮（CTX），并守住「CTX 不是证据」。
//!
//! 派发档 `2026-10-28-ctx-render` 的五条判据各一条用例：
//! a. 有邻轮时渲染出现 `CTX [c1]` 段，且它**不**是 TARGET（邻轮没有 t 编号、不在 TARGET 段）；
//! b. 邻轮正文里的句子不能作为 `evidence`/`quote` 通过契约，`c*` 也不是合法 `turn`；
//! c. 压缩块只出现在 CTX 段；
//! d. 没有邻轮时出现 `CTX: (none)`；
//! e. CTX 增加后 `turns` 读数不变、邻轮不进 `pass_state`，且跨批边界也取得到。
//!
//! 正反例只用通用占位句（张三/对账单/配置），不含任何语料专名。

use rc_distill::batch::{Batch, CtxSide, CtxTurn, ExtractCtx, TurnRow};
use rc_distill::pass_fact::select_fact_batch;
use rc_distill::pass_prompt::{
    event_user, fact_user, is_compaction_body, CTX_NOTE, FactCandidate, FactFamily,
};
use rc_distill::{EventPassError, FactPassError};
use rc_store::Root;

mod common;
use common::seed_bodies;

/// 2026-10-26T14:05+08:00：与其余 prompt 用例同一个时间锚。
const NOW: i64 = 1_792_994_700_000;

fn uid(index: usize) -> String {
    format!("{:064x}", index + 1)
}

fn row(index: usize, role: &str, body: &str) -> TurnRow {
    TurnRow {
        turn_uid: uid(index),
        role: role.into(),
        body: body.into(),
        created_at_ms: NOW + index as i64 * 60_000,
        ingested_at_ms: NOW + index as i64 * 60_000,
        speaker: None,
        speaker_anchor_id: None,
    }
}

fn batch(rows: Vec<TurnRow>) -> Batch {
    Batch {
        session_key: "sess-ctx".into(),
        device_id: "dev-ctx".into(),
        source_agent: "agent-ctx".into(),
        project_hint: None,
        rows,
    }
}

fn candidate(index: usize, canonical: &str) -> FactCandidate {
    FactCandidate {
        reference: format!("a{}", index + 1),
        kind: "person".into(),
        canonical: canonical.into(),
        anchor_id: format!("{:064x}", 0xa0 + index),
        mark: None,
    }
}

/// 两条目标轮 + 各自一条邻轮：前一轮挨着 t1，后一轮挨着 t2。
fn with_neighbors() -> (Batch, ExtractCtx, Vec<FactCandidate>) {
    let b = batch(vec![
        row(0, "human", "好的，收到"),
        row(1, "agent", "我下周三之前把对账单发给你"),
    ]);
    let ctx = ExtractCtx {
        turns: vec![
            CtxTurn {
                target_turn_uid: uid(0),
                side: CtxSide::Before,
                row: TurnRow {
                    turn_uid: uid(90),
                    role: "human".into(),
                    body: "上一轮说了：这批报价只发李四一份".into(),
                    created_at_ms: NOW - 60_000,
                    ingested_at_ms: NOW - 60_000,
                    speaker: None,
                    speaker_anchor_id: None,
                },
            },
            CtxTurn {
                target_turn_uid: uid(1),
                side: CtxSide::After,
                row: TurnRow {
                    turn_uid: uid(91),
                    role: "human".into(),
                    body: "下一轮说了：那就先这样".into(),
                    created_at_ms: NOW + 120_000,
                    ingested_at_ms: NOW + 120_000,
                    speaker: None,
                    speaker_anchor_id: None,
                },
            },
        ],
    };
    (b, ctx, vec![candidate(0, "张三")])
}

fn count(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

fn has_hex64(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.windows(64).any(|window| window.iter().all(|c| c.is_ascii_hexdigit()))
}

// ── a. CTX 段在 TARGET 之前，且邻轮不是 TARGET ────────────────────────────────

#[test]
fn ctx_renders_before_targets_and_never_as_a_target() {
    let (b, ctx, c) = with_neighbors();
    for user in [
        fact_user(&b, &c, FactFamily::Promise, &ctx),
        event_user(&b, &c, &ctx),
    ] {
        let first_target = user.find("TARGET [t").expect("TARGET 段必须在场");
        let head = &user[..first_target];
        assert!(head.contains("CTX [c1] before t1"), "前一轮的 CTX 段头：{user}");
        assert!(head.contains("CTX [c2] after t2"), "后一轮的 CTX 段头：{user}");
        assert!(head.contains(CTX_NOTE), "段头的那一句固定说明必须在场：{user}");
        assert!(head.contains("上一轮说了：这批报价只发李四一份"), "前一轮正文在 CTX 段：{user}");
        assert!(head.contains("下一轮说了：那就先这样"), "后一轮正文在 CTX 段：{user}");
        // 判据 a 的"不出现 TARGET [t"：CTX 段里没有 TARGET 标记——邻轮没有被提成目标轮。
        assert!(!head.contains("TARGET [t"), "CTX 段里不许出现 TARGET 标记：{user}");

        // 目标段只有批里的两轮；邻轮正文一次都不出现在目标段里。
        let targets = &user[first_target..];
        assert_eq!(count(targets, "TARGET [t"), b.rows.len(), "TARGET 只数目标轮：{user}");
        assert!(!targets.contains("上一轮说了：这批报价只发李四一份"), "邻轮不许混进 TARGET 段");
        assert!(!targets.contains("下一轮说了：那就先这样"), "邻轮不许混进 TARGET 段");
        // 邻轮没有 t 编号：`t3` 在整段渲染里不存在（批只有两轮）。
        assert!(!user.contains("TARGET [t3]"), "邻轮不许抢走 t 编号：{user}");
        // 与既有渲染同一条纪律：uid / 毫秒字段名一个字都不出现。
        assert!(!user.contains(&uid(90)) && !user.contains(&uid(91)));
        assert!(!has_hex64(&user));
        assert!(!user.contains("_ms"));
    }
}

// ── b. 邻轮的句子不是证据，`c*` 也不是引用面 ─────────────────────────────────

#[test]
fn ctx_neighbor_text_cannot_become_evidence() {
    let (b, ctx, c) = with_neighbors();
    let user = fact_user(&b, &c, FactFamily::Promise, &ctx);
    // 前件：邻轮的话确实在 prompt 里（否则下面的拒收证明不了任何事）。
    assert!(user.contains("这批报价只发李四一份"));

    // 拿邻轮的句子当 quote：逐字检查只对照**目标轮**正文 ⇒ 拒。
    let quoted_neighbor = r#"{"schema_version":0,"pass":"fact","family":"promise","facts":[{
           "ref":"f1","turn":"t1","subject":"a1","value":"只发李四一份",
           "category":"promise","predicate":"promise","mode":"stated",
           "quote":"这批报价只发李四一份","evidence":["t1"],
           "text":"张三承诺这批报价只发李四一份"}],"empty_turns":["t2"]}"#;
    assert_eq!(
        rc_distill::parse_fact_output(quoted_neighbor, &b, &c, FactFamily::Promise).unwrap_err(),
        FactPassError::QuoteNotVerbatim("f1".into()),
        "邻轮的句子不能当目标轮的引文"
    );

    // 目标轮的引文合法、但 evidence 写 `c1`：`c*` 不在目标表里 ⇒ evidence_not_target。
    let cites_ctx = r#"{"schema_version":0,"pass":"fact","family":"promise","facts":[{
           "ref":"f1","turn":"t1","subject":"a1","value":"把对账单发给李四",
           "category":"promise","predicate":"promise","mode":"stated",
           "quote":"我下周三之前把对账单发给你","evidence":["t1","c1"],
           "text":"张三在下周三之前把对账单发给你"}],"empty_turns":[]}"#;
    assert_eq!(
        rc_distill::parse_fact_output(
            // t1 的目标轮正文必须真有引文（这条用例打的不是 quote，是 evidence 的引用面）。
            cites_ctx,
            &batch(vec![
                row(0, "agent", "我下周三之前把对账单发给你"),
                row(1, "human", "好的，收到"),
            ]),
            &c,
            FactFamily::Promise,
        )
        .unwrap_err(),
        FactPassError::EvidenceNotTarget("c1".into()),
        "CTX 的 c* 不是 evidence 的引用面"
    );

    // 把邻轮直接当成本条的 turn：`c*`/邻轮不在 targets 里 ⇒ unknown_turn。
    let turn_is_ctx = r#"{"schema_version":0,"pass":"fact","family":"promise","facts":[{
           "ref":"f1","turn":"c1","subject":"a1","value":"x1",
           "category":"promise","predicate":"promise","mode":"stated",
           "quote":"这批报价只发李四一份","evidence":["c1"],"text":"x1"}],
           "empty_turns":["t1","t2"]}"#;
    assert_eq!(
        rc_distill::parse_fact_output(turn_is_ctx, &b, &c, FactFamily::Promise).unwrap_err(),
        FactPassError::UnknownTurn("c1".into())
    );

    // event 侧同一条边界：邻轮的句子过不了 event 的逐字检查。
    let event_from_ctx = r#"{"schema_version":0,"pass":"event","events":[{
           "ref":"e1","turn":"t1","category":"event","actor":"a1",
           "action":"send_quote","trigger":"发","object_text":"对账单","participants":["a1"],
           "result":"done","mode":"stated","quote":"这批报价只发李四一份",
           "evidence":["t1"],"text":"张三把对账单发给了李四"}],"empty_turns":["t2"]}"#;
    assert_eq!(
        rc_distill::parse_event_output(event_from_ctx, &b, &c).unwrap_err(),
        EventPassError::QuoteNotVerbatim("e1".into()),
        "邻轮的句子也不能当 event 的引文"
    );
}

// ── c. 压缩块只出现在 CTX 段 ────────────────────────────────────────────────

#[test]
fn compaction_block_renders_only_in_the_ctx_section() {
    let summary = "[[compact_summary]] 本段由适配器压出：用户昨天决定了报价口径。";
    assert!(is_compaction_body(summary), "夹具本身要是压缩块");
    let b = batch(vec![row(0, "human", "好的，收到")]);
    let ctx = ExtractCtx {
        turns: vec![CtxTurn {
            target_turn_uid: uid(0),
            side: CtxSide::Before,
            row: TurnRow {
                turn_uid: uid(92),
                role: "system".into(),
                body: summary.into(),
                created_at_ms: NOW - 60_000,
                ingested_at_ms: NOW - 60_000,
                speaker: None,
                speaker_anchor_id: None,
            },
        }],
    };
    for user in [
        fact_user(&b, &[], FactFamily::Decision, &ctx),
        event_user(&b, &[], &ctx),
    ] {
        let first_target = user.find("TARGET [t").expect("TARGET 段必须在场");
        let head = &user[..first_target];
        assert!(head.contains("CTX [c1] before t1 [compact summary]"), "摘要要在 CTX 段并标明：{user}");
        assert!(head.contains(summary), "摘要正文只许在 CTX 段：{user}");
        let targets = &user[first_target..];
        assert!(!targets.contains("[[compact_summary]]"), "压缩块不许进 TARGET 段：{user}");
        assert!(!targets.contains("用户昨天决定了报价口径"), "压缩块正文不许进 TARGET 段：{user}");
    }
}

// ── d. 没有邻轮 ⇒ 明写 (none) ────────────────────────────────────────────────

#[test]
fn no_neighbors_renders_the_none_marker() {
    let b = batch(vec![row(0, "human", "好的，收到")]);
    for user in [
        fact_user(&b, &[], FactFamily::Decision, &ExtractCtx::default()),
        event_user(&b, &[], &ExtractCtx::default()),
    ] {
        assert!(user.contains("CTX: (none)"), "没有邻轮要明写 (none)，不静默省略：{user}");
        assert!(!user.contains("CTX [c"), "没有邻轮就不许有 c* 段：{user}");
        assert!(!user.contains(CTX_NOTE), "没有 CTX 段就不带那句说明：{user}");
    }
}

// ── e. turns 读数不变、邻轮不进 pass_state、跨批边界也取得到 ─────────────────

#[test]
fn ctx_never_enters_batch_state_or_the_turns_readout() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let bodies = ["合成第 0 句", "合成第 1 句", "合成第 2 句", "合成第 3 句", "合成第 4 句", "合成第 5 句"];
    let uids = seed_bodies(&mut root, &bodies, 1_000);
    assert_eq!(uids.len(), 6);

    // 限 4 轮：批 = u0..u3，CTX = 紧随其后的 u4（会话还有 u5，但它不是 ±1）。
    let plan = select_fact_batch(root.transcript(), root.ledger(), root.memory(), 4,
                                 FactFamily::Promise, NOW)
        .unwrap()
        .expect("六轮都没跑过这一族");
    // `turns` 读数（hub 的 FactOut / event 的 EventOut）取的就是这一格：目标轮数，不含 CTX。
    assert_eq!(plan.batch.rows.len(), 4, "turns 读数仍只数目标轮");
    assert_eq!(plan.ctx_turns.turns.len(), 1, "只有末轮的后一轮在 ±1 之外：{:?}",
               plan.ctx_turns.turns.iter().map(|t| &t.row.body).collect::<Vec<_>>());
    let ctx_turn = &plan.ctx_turns.turns[0];
    assert_eq!(ctx_turn.row.turn_uid, uids[4]);
    assert_eq!(ctx_turn.side, CtxSide::After);
    assert_eq!(ctx_turn.target_turn_uid, uids[3]);
    assert!(!plan.batch.rows.iter().any(|r| r.turn_uid == ctx_turn.row.turn_uid),
            "CTX 轮不在批里");

    // 邻轮不进 pass_state：选批这一趟一行都不该为它写。
    let stated: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT COUNT(*) FROM extract_pass_state WHERE pass_id='fact'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stated, 0, "选批为一族还没跑过的会话写 pass_state，就是 CTX 漏了进来");

    // 渲染里看得见 u4，但它是 CTX 不是 TARGET。
    let user = fact_user(&plan.batch, &plan.candidates, FactFamily::Promise, &plan.ctx_turns);
    let first_target = user.find("TARGET [t").unwrap();
    assert!(user[..first_target].contains("CTX [c1] after t4"), "跨批边界要取到下一轮：{user}");
    assert!(user[..first_target].contains("合成第 4 句"));
    assert!(!user[first_target..].contains("合成第 4 句"), "CTX 轮不许混进 TARGET 段");

    // 跨批边界：把这一批标记完成后重选，新的首轮的前一轮是**上一批**的末轮。
    for uid in &uids[..4] {
        rc_ledger::pass_state::mark_terminal(
            root.ledger(),
            rc_ledger::pass_state::PassKey { turn_uid: uid, pass_id: "fact", family: "promise" },
            rc_ledger::pass_state::PassState::Done,
            None,
            NOW,
        )
        .unwrap();
    }
    let next = select_fact_batch(root.transcript(), root.ledger(), root.memory(), 0,
                                 FactFamily::Promise, NOW)
        .unwrap()
        .expect("还有两轮没跑");
    assert_eq!(next.batch.rows.len(), 2, "第二批只有 u4/u5");
    assert_eq!(next.ctx_turns.turns.len(), 1);
    let before = &next.ctx_turns.turns[0];
    assert_eq!(before.side, CtxSide::Before);
    assert_eq!(before.row.turn_uid, uids[3], "上一批的末轮是这一批首轮的前一轮");
    assert_eq!(before.target_turn_uid, uids[4]);
}
