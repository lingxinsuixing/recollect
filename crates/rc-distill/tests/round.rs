//! P0-4a：`round_once` 的契约——**空批是合法结果**。
//!
//! 这一条钉两件事，正好是"空批不写状态、不调模型"的全部含义：
//! 1. naming/fact/event 的选批为空 ⇒ `Ok(None)`；
//! 2. 这一轮一条 `extract_pass_state` 都不写（那三族"没活干"的读数由 hub 打 `state="none"`）。
//!
//! time 不在这一条里：它的空批是 `state="empty"` 的完整读数（选批与回填共用 `run_time`
//! 的扫描面，审查水位照写），所以它返回 `Ok(Some(..))`——这个差异单独钉在下面。

use rc_distill::pass_prompt::FactFamily;
use rc_distill::review::ReviewMode;
use rc_distill::round::{round_once, Family, RoundInput, RoundOut};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_store::Root;

/// 空批永远不会问到模型：真被问到就说明选批这一步漏了。
struct DeadSeat;

impl LlmBackend for DeadSeat {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        panic!("空批不许调模型");
    }
}

fn state_rows(root: &Root) -> i64 {
    root.ledger()
        .conn()
        .query_row("SELECT COUNT(*) FROM extract_pass_state", [], |r| r.get(0))
        .expect("账本必须回答这一格")
}

#[test]
fn empty_batch_is_none_and_writes_no_pass_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let seat = DeadSeat;
    let now_ms = 1_792_994_700_000;

    // 空库：没有会话、没有轮、没有任何表态行。
    let families = [Family::Naming, Family::Fact(FactFamily::Decision), Family::Event];
    for family in families {
        let got = round_once(
            root.transcript(),
            root.ledger(),
            root.memory(),
            RoundInput {
                family,
                batch_turns: 0,
                now_ms,
                backend: Some(&seat),
                review_mode: ReviewMode::Inline,
                retry_empty: 0,
            },
        );
        let got = got.unwrap_or_else(|e| panic!("空批不是错误：{family:?} -> {e:?}"));
        assert!(got.is_none(), "{family:?} 的空批必须是 Ok(None)，不写读数");
        assert_eq!(state_rows(&root), 0, "{family:?} 的空批一条状态行都不许写");
    }
}

#[test]
fn time_empty_batch_is_an_empty_readout_not_none() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();

    let got = round_once(
        root.transcript(),
        root.ledger(),
        root.memory(),
        RoundInput {
            family: Family::Time,
            batch_turns: 0,
            now_ms: 1_792_994_700_000,
            // time 没有模型面：一个座位都不需要。
            backend: None,
            review_mode: ReviewMode::Inline,
            retry_empty: 0,
        },
    )
    .expect("time 的空批不是错误");

    match got {
        Some(RoundOut::Time(out)) => {
            assert_eq!(out.turns, 0, "空批没有选中轮");
            assert_eq!(out.state.slug(), "empty", "time 的空批是 empty，不是 none");
        }
        other => panic!("time 必须交回 TimeOutcome，实际 {other:?}"),
    }
    // 与其余三族的分界：time 的空批照样跑完一轮，所以它写过 `extract_pass_state` 也无妨；
    // 这里只钉"没有选中轮 ⇒ 没有 running 行"。
    let running: i64 = root
        .ledger()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM extract_pass_state WHERE state='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(running, 0, "空批不许落 running");
}
