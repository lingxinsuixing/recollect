//! P3 的 hook 半边。两条都是**对撞**而不是断字面量：期望值来自**当场**在同一份播种好的库里
//! 调 `rc_agent::tools::status(…, None, &StatusOptions::default(), NOW)`（`rc-agent` 是 hook 的正式依赖）。
//! `hook brief` **不加 `--entity`**（P-53 的定案：那是把 agent 面的查询参数往运维面上搬，
//! 而运维面只有 `--data-dir`/`--now-ms`/`--replica` 三个输入），所以这里对撞的是"无过滤"那一条路径。
mod common;
use common::*;
use rc_agent::tools::{status, StatusOptions};

#[test]
fn brief里的目录三格与进程内逐字节相同() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed_person(&data, NOW);                                // 本文件用的夹具：与 mcp 侧同形状的**一份**
                                                            // （跨 crate 没有共享 tests/common，Cargo 的测试隔离决定的；
                                                            //  它漂了也不会让两面**互相**同意，只会让各自那一面对自己的期望红）

    let out = hook(&["brief", "--data-dir", data.to_str().unwrap(), &now_arg()]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let line = text.lines().next().unwrap();
    let body = line.strip_prefix(rc_agent::FENCE).expect("brief 少了围栏");

    let root = rc_store::Root::open(&data).unwrap();
    let want = status(root.memory(), None, &StatusOptions::default(), NOW).unwrap();
    let got: serde_json::Value = serde_json::from_str(body).unwrap();
    let want_v: serde_json::Value = serde_json::from_str(&want.text()).unwrap();
    assert_eq!(got["entities"], want_v["entities"], "节点面的目录漂开了（守卫 3 / P-53）");
    assert_eq!(got["entities"].to_string(), want_v["entities"].to_string());   // P-64 的双比

    // **武装**这条对撞：目录若两面都缺席，`Value == Value` 会拿 `null == null` 绿过去。
    assert!(got["entities"].is_array() && !got["entities"].as_array().unwrap().is_empty());
    assert_eq!(got["entities"][0]["refs"], 1, "夹具没连上出处轮 ⇒ 对撞的是两份空读数");
}

#[test]
fn 节点面的目录里零正文零路径() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed_person(&data, NOW);
    // 正文 canary 用 `probe_cli` 那条既有夹具的卡片正文（守卫 12 在线上半边的原样尺子）。
    rc_memory::put_card(rc_store::Root::open(&data).unwrap().memory(),
        &rc_memory::FactContent { kind: "lesson".into(), title: "会召回的行".into(),
            body: "带这个词的行会被提示面召回。".into(),
            scope: rc_memory::Scope { kind: rc_memory::ScopeKind::Project, key: "proj-A".into() },
            valid_to_ms: None },
        &[rc_memory::Provenance { turn_uid: "a".repeat(64), anchor_role: rc_memory::AnchorRole::Human }],
        NOW).unwrap();

    let out = hook(&["brief", "--data-dir", data.to_str().unwrap(), &now_arg()]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("带这个词的行会被提示面召回"), "节点面的目录漏了正文（守卫 12）");
    assert!(!text.contains("title\"") && !text.contains("\"body\""), "目录不该长出一个叫 body 的格子");
    assert!(!text.contains('\\') && !text.contains(".db"), "运维/诊断面零路径（守卫 12）");
    assert_eq!(text.lines().count(), 1, "brief 永远只占一行");
}