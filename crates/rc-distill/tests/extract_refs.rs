//! W23a：旧抽取面的**批内引用**（`t1..tN`）。
//!
//! 旧 prompt 让模型逐字抄 64 位 `turn_uid`——那是把身份值塞进一句要模型复制的文本里，
//! 既贵（每引用一次 64 个 hex）又脆（抄错一位就是整批丢弃）。现在方括号里是位置别名，
//! 映射回真实轮由程序做；**兼容期**仍收老回放夹具与老节点缓存里的 64 位 reply。
//!
//! 三臂（派发档 §2 的三条判据）：
//! ① 新引用：`t1`/`t2` ⇒ 解析出的 provenance 是真实 uid，且落库通过；
//! ② 未知引用：`t9` ⇒ 既有的 batch/契约错误，不静默；
//! ③ legacy：真实 64 位 uid 仍被接受，但**新 prompt 文本里它出现 0 次**。

mod common;

use rc_distill::batch::{ref_index, turn_ref, Batch, TurnRow};
use rc_distill::contract::{parse_output, ContractError};
use rc_distill::extract::{run_extract, select_batch, state_of, MAX_TURNS_PER_BATCH};
use rc_ledger::water::ExtractState;
use rc_llm::mock::MockBackend;
use rc_store::Root;
use serde_json::{json, Value};

fn row(uid_hex: &str, role: &str, body: &str) -> TurnRow {
    TurnRow {
        turn_uid: uid_hex.to_string(),
        role: role.into(),
        body: body.into(),
        created_at_ms: 1,
        ingested_at_ms: 1,
        speaker: None,
        speaker_anchor_id: None,
    }
}

/// 三行一批：`u[0]`/`u[1]`/`u[2]` 是 64 位 hex（生产形状，夹具不另立一套）。
fn batch_of_three() -> (Batch, [String; 3]) {
    let uids = [format!("{:064x}", 11), format!("{:064x}", 12), format!("{:064x}", 13)];
    let b = Batch {
        session_key: "s".into(),
        device_id: "d".into(),
        source_agent: "a".into(),
        project_hint: None,
        rows: vec![
            row(&uids[0], "human", "第一条"),
            row(&uids[1], "agent", "第二条"),
            row(&uids[2], "human", "第三条"),
        ],
    };
    (b, uids)
}

fn reply(provenance: Value, worthless: Value) -> String {
    json!({
        "entities": [],
        "cards": [{"kind":"lesson","title":"甲","body":"乙","valid_to_ms":null,
                   "provenance": provenance}],
        "events": [],
        "assertions": [],
        "worthless_turn_uids": worthless
    })
    .to_string()
}

/// 臂 ①（渲染侧）：新 prompt 里每一行给的是 `t1..tN`，64 位身份值**一次都不出现**。
///
/// 这条断言是"短 id"那张单子的可观测面：只要有人把渲染改回 `r.turn_uid`，
/// 这里立刻红，而不必等模型抄错一位。
#[test]
fn the_new_prompt_carries_batch_refs_and_no_64_hex_identity_value() {
    let (b, uids) = batch_of_three();
    let user = rc_distill::prompt::extract_user(&b);

    let i1 = user.find("[t1]").expect("第一行要渲染成 [t1]");
    let i2 = user.find("[t2]").expect("第二行要渲染成 [t2]");
    let i3 = user.find("[t3]").expect("第三行要渲染成 [t3]");
    assert!(i1 < i2 && i2 < i3, "编号顺序 = batch.rows 顺序：{user}");

    for uid in &uids {
        assert_eq!(user.matches(uid.as_str()).count(), 0, "身份值不许出现在 prompt 里：{user}");
    }
    // 指令侧也要跟着换：模型被要求抄的是 ref，不是 id。
    assert!(user.contains("Copy the bracketed ref(s) verbatim into provenance"), "{user}");
    assert!(!user.contains("bracketed id"), "旧的复制指令还在：{user}");
}

/// 臂 ①（解析侧）：`t1`/`t2`/`t3` 按 `rows` 顺序映回真实 uid，四类清单都走同一条映射。
#[test]
fn t_refs_map_back_to_the_real_uids_in_every_list() {
    let (b, uids) = batch_of_three();
    let payload = json!({
        "entities": [{"kind":"person","title":"张三","aliases":[],"provenance":["t1"]}],
        "cards":    [{"kind":"lesson","title":"甲","body":"乙","valid_to_ms":null,"provenance":["t2"]}],
        "events":   [{"kind":"milestone","title":"丙","body":"丁","valid_to_ms":null,"provenance":["t3","t1"]}],
        "assertions":[{"kind":"decision","title":"戊","body":"己","valid_to_ms":null,"provenance":["t3"]}],
        "worthless_turn_uids": ["t2"]
    })
    .to_string();
    let out = parse_output(&payload, &b).unwrap();

    assert_eq!(out.entities[0].uids, vec![uids[0].clone()], "entity 出处要映回真实 uid");
    assert_eq!(out.cards[0].uids, vec![uids[1].clone()]);
    assert_eq!(out.events[0].uids, vec![uids[2].clone(), uids[0].clone()], "顺序 = 首次出现序");
    assert_eq!(out.assertions[0].uids, vec![uids[2].clone()]);
    assert_eq!(out.worthless, vec![uids[1].clone()], "worthless 也要映射，不许留 ref");
    assert_eq!(out.content_items, 4);
}

/// 臂 ①（写侧）：整条链跑一次——ref 回复经 `run_extract` 落库，状态行挂在**真实** uid 上。
///
/// 契约层映对了但写侧拿 ref 去查角色/查行，结果一样是错的；这一条把两端钉在一起。
#[test]
fn a_ref_reply_writes_rows_under_the_real_uids() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = common::seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH)
        .unwrap()
        .expect("两条 turn 必须选得出来");
    assert_eq!(plan.rows_ref().len(), 2, "夹具的前提是两条都进批");

    let payload = json!({
        "entities": [], "events": [], "assertions": [],
        "cards": [{"kind":"lesson","title":"引用落库","body":"两条轮一起作证",
                   "valid_to_ms":null,"provenance":["t1","t2"]}],
        "worthless_turn_uids": []
    })
    .to_string();
    let backend = MockBackend::scripted(vec![Ok(payload)]);
    let out = run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();

    assert_eq!(out.rows_written, 1, "ref 回复必须落一行，不是整批被拒");
    for uid in &uids {
        assert_eq!(state_of(root.ledger(), uid).unwrap(), Some(ExtractState::Extracted),
                   "{uid} 的状态行要挂在真实 uid 上");
    }
    // 出处落库的是真实 uid（写侧与契约层同一条映射，不各算一遍）。
    let prov: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM provenance WHERE turn_uid IN (?1, ?2)",
        [&uids[0], &uids[1]], |r| r.get(0)).unwrap();
    assert_eq!(prov, 2, "一行两轮出处：{prov}");
}

/// 臂 ②：未知引用（`t9`）走**既有**契约错误——不静默、不折成空串、不落某个默认行。
#[test]
fn an_unknown_ref_is_the_existing_contract_error() {
    let (b, _uids) = batch_of_three();
    // provenance：越界引用。错误载荷里回显 ref 本身（形状是程序定的、长度 ≤5），
    // 运维靠它定位到模型引错了哪一格。
    let e = parse_output(&reply(json!(["t9"]), json!([])), &b).unwrap_err();
    assert_eq!(e, ContractError::UnlistedUid { uid: "t9".into() });
    assert!(e.to_string().contains("t9"), "载荷要把引错的 ref 说出来：{e}");

    // worthless 走另一支，但同样是错误而不是"静默丢弃"。
    let e2 = parse_output(&reply(json!(["t1"]), json!(["t9"])), &b).unwrap_err();
    assert_eq!(e2, ContractError::WorthlessUnlisted { uid: "t9".into() });

    // 语法边界：0 不是合法下标、`t01` 不认（一种行只有一种拼法）、大小写不宽松。
    for bad in ["t0", "t01", "T1", "t", "t1x", "tt1"] {
        let e = parse_output(&reply(json!([bad]), json!([])), &b).unwrap_err();
        assert!(matches!(e, ContractError::UnlistedUid { .. }), "{bad} 必须报错，得到 {e:?}");
    }
    // 未知**形状**的文本仍只回显长度（守卫 12 的模型输入侧不变）。
    let leak = "我在赶进度，别把这段正文回显到运维面".repeat(8);
    let e = parse_output(&reply(json!([leak.clone()]), json!([])), &b).unwrap_err();
    assert!(!e.to_string().contains("我在赶进度"), "正文从 Display 漏出去了：{e}");
    assert!(e.to_string().contains("not-a-uid"), "{e}");
}

/// 臂 ③（兼容期）：真实 64 位 uid 仍被接受——老回放夹具与老节点缓存的回复就是这个形状。
#[test]
fn a_legacy_64_hex_uid_is_still_accepted() {
    let (b, uids) = batch_of_three();
    let out = parse_output(&reply(json!([uids[1].clone()]), json!([uids[2].clone()])), &b).unwrap();
    assert_eq!(out.cards[0].uids, vec![uids[1].clone()]);
    assert_eq!(out.worthless, vec![uids[2].clone()]);

    // 两种拼法混在一格：映回之后是同一行 ⇒ 去重成一格（不然写侧撞 provenance 主键）。
    let out = parse_output(&reply(json!(["t1", uids[0].clone()]), json!([])), &b).unwrap();
    assert_eq!(out.cards[0].uids, vec![uids[0].clone()], "同一轮的两种拼法只占一格");
}

/// 编号规则只有一个家（`batch::turn_ref` / `Batch::uid_of_ref`），且只有一种拼法。
#[test]
fn the_ref_grammar_has_one_home_and_one_spelling_per_row() {
    let (b, uids) = batch_of_three();
    assert_eq!(turn_ref(0), "t1");
    assert_eq!(turn_ref(2), "t3");
    // 渲染与解析是同一条编号：`turn_ref(i)` 映出来的就是 `uid_of_ref` 回的那一行。
    for (i, uid) in uids.iter().enumerate() {
        assert_eq!(b.uid_of_ref(&turn_ref(i)), Some(uid.as_str()), "第 {i} 行的两种说法要互指");
    }
    assert_eq!(b.uid_of_ref("t4"), None, "越界 ⇒ 没有这一行，不许折叠到末行");
    assert_eq!(b.uid_of_ref("t0"), None, "下标从 1 起");
    assert_eq!(ref_index("t01"), None, "前导零不是同一条编号规则");
    assert_eq!(ref_index("t12"), Some(12));
    // legacy 身份值不是位置别名：ref 那一支对它恒 None，接住它的是 legacy 那一支。
    assert_eq!(b.uid_of_ref(&"f".repeat(64)), None);
}
