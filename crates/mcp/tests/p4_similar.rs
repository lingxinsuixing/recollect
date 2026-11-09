//! P4 的线上判据：`remember` 在同一次调用里既写转录又读影子索引。
//! 这一档存在的唯一理由是 `RefCell<Root>`——本文件的第 1 条在旧形状下**不是红断言，是 panic**
//! （`already borrowed: BorrowMutError`），那正是坑 P-48 的原文。撤掉 `RefCell` 之后它绿，
//! 而"构造不出那种 panic"从此是类型层面的事实。
//!
//! W37-A1（G16）：`similar_hint` 带读者身份过读闸。本文件的宿主自报名统一用
//! `common::seed` 的署名 agent（`synthetic-agent`）——P4 这一族判的是"写与读在同一次调用"，
//! 读闸那一维不许掺变量；跨 agent 的那一线在线上有专属用例（`similar_跨agent的行不进提示`）。
mod common;
use common::*;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
/// 与 `common::seed` 那条 card 的标题重合到必然命中的程度（`TOK` 就在那个标题里）。
const P4_BODY: &str = "再说一次工具面合成词的口径";

#[test]
fn remember_在同一次调用里写一次并读一次() {
    // P-48 的收据（Step 3b 那一格 panic 的对照面）。两半都要断：
    // ① 提示是**数字**（说明记忆柄真的被读了）；② 转录真的留下一行（说明写缝也真的跑了）。
    // 在 `RefCell<Root>` 还活着的时候这一条走不到任何断言：子进程在读到回包之前就 panic 退掉，
    // `call_json` 会先喊"服务端在没有回包的情况下关闭了 stdout"。
    let dir = tempfile::tempdir().unwrap();
    let k = seed(dir.path(), NOW);
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "synthetic-agent"));
    let out = call_json(&mut s, 2, "recollect_remember",
                        json!({"body": P4_BODY, "session_key": "sess-p4"}));
    assert_eq!(out["similar_n"], 1, "{out}");
    assert_eq!(out["similar"][0]["id"], k);
    let wrote = turns(dir.path());
    assert_eq!(wrote.len(), 1, "写缝被降级路径一起撤掉了（P-61）：{wrote:?}");
    assert_eq!(wrote[0].1, P4_BODY);
}

#[test]
fn 门面上的降级读数是_null_不是_0() {
    // P-59 的线上半边 + P-61：**回执仍然是成功的**（`call_json` 里那句 `isError` 断言
    // 就是这条，用例只管两格）。这里故意不跑 `seed`（它建索引），于是索引从没建过。
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "qoder-test"));
    let out = call_json(&mut s, 2, "recollect_remember",
                        json!({"body": P4_BODY, "session_key": "sess-p4"}));
    assert_eq!(out["similar_n"], Value::Null);
    assert_eq!(out["similar"], Value::Null);
    assert_eq!(turns(dir.path()).len(), 1, "读缝失败 ⇒ 写缝也不许失败");
}

#[test]
fn 门面回执顶层恒八格_没有分数也没有正文() {
    // C2 的线上尺子（与 `tools_shape.rs` 那条同型，但它是 agent 真正读到的那一层）：
    // 宿主按字段名取值，字段名与格数漂了就是协议破坏。`common::BODY` 是 `seed` 那条 card
    // 的正文里**没有**的词，而 `seed` 写进去的正文是"召回面只断形状。"——所以这一条断的是
    // 整串回包里不出现那条卡片的正文。
    let dir = tempfile::tempdir().unwrap();
    let _k = seed(dir.path(), NOW);
    let mut s = Session::start(dir.path());
    let _ = s.ask(&init(1, "synthetic-agent"));
    let out = call_json(&mut s, 2, "recollect_remember",
                        json!({"body": P4_BODY, "session_key": "sess-p4"}));
    let keys: Vec<String> = out.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, vec!["accepted_chars", "eta_turns", "extract_state", "reason",
                          "schema_version", "similar", "similar_n", "turn_uid"]);
    let s = out.to_string();
    assert!(!s.contains("召回面只断形状"), "提示把正文带上去了（守卫 12）：{s}");
    for w in ["score", "matched", "row_len"] {
        assert!(!s.contains(w), "分数或它的中间量上了线：{w}");
    }
}

#[test]
fn similar_跨agent的行不进提示_同principal的经4b进() {
    // W37-A1（G16 的线上半边）：`remember` 的相似提示带读者身份过读闸。
    // 夹具卡的归属补上 principal（`common::seed` 的白盒卡 owner_agent=synthetic-agent、
    // principal=NULL，4b 的判据两边都要非空）；宿主以 `agent-reader` 起进程——
    // 无 `--principal-id` ⇒ 4b 永不命中（fail-closed），提示里没有别人的行；
    // `--principal-id p2` ⇒ principal 不等，同样没有；
    // `--principal-id p1` ⇒ 同 principal 异 agent 经 4b 放行，提示里恰一条。
    let dir = tempfile::tempdir().unwrap();
    let k = seed(dir.path(), NOW);
    {
        // 白盒补章：把 seed 卡的归属 principal 写成 p1（模拟"行有章"的库）。
        let root = rc_store::Root::open(dir.path()).unwrap();
        root.memory().conn().execute(
            "UPDATE resource_owner SET owner_principal_id='p1'
              WHERE mem_table='card' AND owner_agent_id='synthetic-agent'",
            [],
        ).unwrap();
    }

    for extra in [&[][..], &["--principal-id", "p2"][..]] {
        let mut args = vec!["--device-id", "dev-synthetic", "--agent-id", "agent-reader"];
        args.extend_from_slice(extra);
        let mut s = Session::start_with(dir.path(), &args);
        let _ = s.ask(&init(1, "agent-reader"));
        let out = call_json(&mut s, 2, "recollect_remember",
                            json!({"body": P4_BODY, "session_key": "sess-g16"}));
        assert_eq!(out["similar_n"], 0, "extra={extra:?}：别人的 private 行泄进了相似提示：{out}");
        assert!(!out.to_string().contains(&k), "泄漏了句柄：extra={extra:?} {out}");
    }

    let mut s = Session::start_with(dir.path(),
        &["--device-id", "dev-synthetic", "--agent-id", "agent-reader", "--principal-id", "p1"]);
    let _ = s.ask(&init(1, "agent-reader"));
    let out = call_json(&mut s, 2, "recollect_remember",
                        json!({"body": P4_BODY, "session_key": "sess-g16"}));
    assert_eq!(out["similar_n"], 1, "同 principal 的异 agent 没经 4b 读到：{out}");
    assert_eq!(out["similar"][0]["id"], k, "{out}");
}
