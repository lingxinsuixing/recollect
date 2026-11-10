//! A1 执行面：两次调用、契约解析、plan → write 的离线闭环。
//! 模型用测试自带的记录型 backend（生产代码里没有它的引用点）。

use rc_distill::batch::{Batch, TurnRow};
use rc_distill::contract_v4::{plan_a1, A1Error};
use rc_distill::prompt::ANCHOR_CANARY;
use rc_distill::run_a1_pair;
use rc_distill::DistillError;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use std::cell::RefCell;

fn uid(c: char) -> String {
    c.to_string().repeat(64)
}

fn batch() -> Batch {
    Batch {
        session_key: "s".into(),
        device_id: "d".into(),
        source_agent: "a".into(),
        project_hint: None,
        rows: vec![
            TurnRow { turn_uid: uid('a'), role: "human".into(), body: "张三下周末搬家。".into(), created_at_ms: 1, ingested_at_ms: 1, speaker: None, speaker_anchor_id: None },
            TurnRow { turn_uid: uid('b'), role: "agent".into(), body: "老电脑重装完了，还差一次批量验证。".into(), created_at_ms: 2, ingested_at_ms: 2, speaker: None, speaker_anchor_id: None },
        ],
    }
}

fn reply(anchor_ref: &str, canonical: &str) -> String {
    format!(
        r#"{{"schema_version":4,"pass":"a1",
          "anchors":[{{"ref":"{anchor_ref}","kind":"object","canonical":"{canonical}",
                      "aliases":[],"reality_status":"real","evidence":["t2"],"reason":"原文点名"}}],
          "mentions":[{{"anchor_ref":"{anchor_ref}","surface":"{canonical}","turn_uid":"t2",
                       "speaker_ref":null,"discourse_role":"mentioned","resolution":"resolved","confidence":0.9}}],
          "issues":[],"unresolved_surfaces":[]}}"#
    )
}

struct Recorder {
    replies: RefCell<Vec<Result<String, LlmFailure>>>,
    seen: RefCell<Vec<Prompt>>,
}

impl Recorder {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Self {
        Recorder { replies: RefCell::new(replies), seen: RefCell::new(Vec::new()) }
    }
    fn calls(&self) -> usize {
        self.seen.borrow().len()
    }
}

impl LlmBackend for Recorder {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.seen.borrow_mut().push(Prompt {
            system: prompt.system.clone(),
            user: prompt.user.clone(),
            derive: prompt.derive,
        });
        self.replies.borrow_mut().remove(0)
    }
}

#[test]
fn pair_makes_two_calls_with_the_a1_prompt_and_no_derive_budget() {
    let backend = Recorder::new(vec![Ok(reply("a1", "老电脑")), Ok(reply("b1", "老电脑"))]);
    let (first, second) = run_a1_pair(&backend, &batch(), &[], None).unwrap();
    assert_eq!(backend.calls(), 2);
    assert_eq!(first.anchors.len(), 1);
    assert_eq!(second.anchors[0].reference, "b1");
    let seen = backend.seen.borrow();
    for p in seen.iter() {
        assert!(p.system.starts_with(ANCHOR_CANARY), "system 不是 A1 模板");
        assert!(p.user.contains("[t2]"), "user 缺短引用");
        assert!(!p.derive, "A1 走默认预算");
    }
}

#[test]
fn first_contract_failure_stops_before_the_second_call() {
    let backend = Recorder::new(vec![Ok("{}".into()), Ok(reply("a1", "老电脑"))]);
    let err = run_a1_pair(&backend, &batch(), &[], None).unwrap_err();
    assert!(matches!(err, DistillError::A1(A1Error::MissingKey(_))), "{err}");
    assert_eq!(backend.calls(), 1, "第一次就坏了，不该再烧一次调用");
}

#[test]
fn second_contract_failure_is_visible_and_writes_nothing() {
    let backend = Recorder::new(vec![Ok(reply("a1", "老电脑")), Ok("[]".into())]);
    let err = run_a1_pair(&backend, &batch(), &[], None).unwrap_err();
    assert!(matches!(err, DistillError::A1(A1Error::NotAnObject)), "{err}");
    assert_eq!(backend.calls(), 2);
}

#[test]
fn pair_to_plan_to_write_is_one_offline_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let backend = Recorder::new(vec![Ok(reply("a1", "老电脑")), Ok(reply("b1", "老电脑"))]);
    let (first, second) = run_a1_pair(&backend, &batch(), &[], None).unwrap();
    let plan = plan_a1(&first, &second, &batch());
    let out = rc_memory::write_a1(
        root.memory(),
        &plan,
        &rc_memory::BatchCtx { source_agent: "rc-distill".into(), device_id: "d".into() },
        &rc_memory::Scope { kind: rc_memory::ScopeKind::Person, key: "u1".into() },
        "run-1",
        1_700_000_000_000,
    )
    .unwrap();

    assert_eq!(out.anchors, 1, "两次指同一个对象 ⇒ 一个锚");
    assert_eq!(out.stable, 1, "双跑都出现 ⇒ stable");
    assert_eq!(out.mentions, 1, "两次同一条提及 ⇒ 去重");
    let status: String = root
        .memory()
        .conn()
        .query_row("SELECT status FROM anchor WHERE canonical='老电脑'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "active");
}
