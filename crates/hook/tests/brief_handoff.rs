//! W34 续接包（「上一棒交接」固定段）的 hook 面用例。判据照派发档验收原文两条：
//! 有未闭 handoff 的夹具 ⇒ brief 输出含交接段与条目；无 ⇒ 该段整体缺席（阴性臂钉住）。
//! 夹具走 `tests/common/mod.rs` 的既有形状（真二进制 + 真库），core 侧直接用 `rc-core`
//! 建 fixture——hook 二进制怎么读 core.db，只有从 CLI 出口看才算数。

mod common;
use common::*;

use rc_core::CoreStore;

/// 建 core fixture：注册 `agent-a` 并写一张未闭 continue 条。返回 data 路径。
fn seed_core(data: &std::path::Path) {
    let core = CoreStore::open(data).unwrap();
    core.register_agent("agent-a", NOW).unwrap();
    core.handoff_write(
        "agent-a",
        "continue",
        "接着把水位闸的读数补进台账",
        None,
        "dev",
        NOW,
    )
    .unwrap();
}

fn brief_with_agent(data: &std::path::Path, agent: &str) -> String {
    let out = hook(&[
        "brief",
        "--data-dir",
        data.to_str().unwrap(),
        "--agent-id",
        agent,
        &now_arg(),
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn 有未闭交接_段出现且含条目() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed_core(&data);
    let text = brief_with_agent(&data, "agent-a");
    assert!(text.contains("[上一棒交接]"), "{text}");
    assert!(text.contains("接着把水位闸的读数补进台账"), "{text}");
    assert!(text.contains("- 接着做"), "{text}");
    // 状态块仍在段前：第 1 行还是围栏 JSON。
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("[recollect] {"), "{first}");
}

#[test]
fn 段的每一行都带围栏前缀() {
    // V3：围栏 = 每行前缀。段多了几行，不破"每行前缀"的承诺。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed_core(&data);
    let text = brief_with_agent(&data, "agent-a");
    for line in text.trim_end().lines() {
        assert!(line.starts_with("[recollect] "), "段行缺围栏前缀：{line}");
    }
}

#[test]
fn 无未闭交接且无读数_段整体缺席() {
    // 阴性臂：注册了但没写过 handoff、审核队列也为 0 ⇒ 缺席的是整段（不是空标题），
    // 且输出退化回既有的"一行围栏 JSON"形状。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let core = CoreStore::open(&data).unwrap();
    core.register_agent("agent-a", NOW).unwrap();
    let text = brief_with_agent(&data, "agent-a");
    assert!(text.contains("[recollect] "), "{text}");
    assert!(!text.contains("[上一棒交接]"), "{text}");
    assert_eq!(text.trim_end().lines().count(), 1, "{text}");
}

#[test]
fn 不给agent_id_段缺席_即使库里有未闭交接() {
    // `--agent-id` 是这一段的开关：不给就维持既有形状——p5 那条"brief 必须一行"的世界不变。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed_core(&data);
    let out = hook(&["brief", "--data-dir", data.to_str().unwrap(), &now_arg()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("[上一棒交接]"), "{text}");
    assert_eq!(text.trim_end().lines().count(), 1, "{text}");
}

#[test]
fn core_db不存在_段缺席且不建库() {
    // 读路径不许把 `CoreStore::open` 的建库副作用带进来：没碰过 core 的根，brief 完不该
    // 凭空长出 core.db。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let text = brief_with_agent(&data, "agent-a");
    assert!(!text.contains("[上一棒交接]"), "{text}");
    assert!(!data.join("core.db").exists(), "读路径建了库");
}

#[test]
fn 未注册身份_段缺席() {
    // core.db 在，但查的身份没注册：合法的空，不是故障（退出码 0、输出一行）。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let core = CoreStore::open(&data).unwrap();
    core.register_agent("agent-b", NOW).unwrap();
    let text = brief_with_agent(&data, "agent-a");
    assert!(!text.contains("[上一棒交接]"), "{text}");
    assert_eq!(text.trim_end().lines().count(), 1, "{text}");
}

#[test]
fn 有待审读数_段含读数行() {
    // 读数键 `pending_review`（rc_memory::review::stats，与 doctor 对账、hub pending 同源）：
    // 有账才报数。夹具入队一条 pending_review，段里出现那一格。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let core = CoreStore::open(&data).unwrap();
    core.register_agent("agent-a", NOW).unwrap();
    let root = rc_store::AgentRoot::open(&data).unwrap();
    rc_memory::review::enqueue(
        root.memory(),
        &rc_memory::review::NewItem {
            source: "rows".into(),
            pass_id: "rows".into(),
            family: String::new(),
            session_key: "sess-synthetic".into(),
            device_id: "dev".into(),
            turn_uids_json: "[\"t-1\"]".into(),
            owner_json: "[]".into(),
            payload_json: "{\"kind\":\"synthetic\"}".into(),
            batch_key: String::new(),
        },
        NOW,
    )
    .unwrap();
    let text = brief_with_agent(&data, "agent-a");
    assert!(text.contains("[上一棒交接]"), "{text}");
    assert!(text.contains("审核队列待审 1 条"), "{text}");
}

#[test]
fn 坏agent_id_退出码2() {
    // 输入不可用（safe_token 过不了）⇒ 2，不是 3：落 3 会把操作员引去跑 doctor。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let core = CoreStore::open(&data).unwrap();
    core.register_agent("agent-a", NOW).unwrap();
    let out = hook(&[
        "brief",
        "--data-dir",
        data.to_str().unwrap(),
        "--agent-id",
        "bad/agent",
        &now_arg(),
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}
