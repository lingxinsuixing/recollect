//! P-54 的装配半边：节点面今天**就是**缝缺席，而这不是 hook 判的，是它拿不到水位库这件事的形状。
//! 夹具来自任务 11 迁出的 `crates/hook/tests/common/mod.rs`（P-65），本文件一行 `CARGO_BIN_EXE` 都不写。
mod common;
use common::*;
use serde_json::Value;

/// 把 `brief` 那一行围栏 JSON 拆成值。`brief` 的载荷就是 `status(…, None, …).text()`，
/// 所以这三格在节点面上长什么样，只有从这里读才算数。
fn brief_json(data: &std::path::Path) -> Value {
    let out = hook(&["brief", "--data-dir", data.to_str().unwrap(), &now_arg()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "brief 必须一行：{text}");
    let payload = text.trim().strip_prefix("[recollect] ").expect("缺围栏");
    serde_json::from_str(payload).unwrap()
}

#[test]
fn 节点面的三格是_null_且三个键名都在场() {
    // 断的是**装配事实**（hook 递 None），不是函数行为（函数行为在 tools_shape 那条）。
    // 有人把 hook 那一行的 `None` 换成别的东西之前，得先给 hook 加 `rc-ledger` 依赖，
    // 而那会撞守卫 6 的编译失败用例——这两句一起才是 P-54 的完整执法。
    let dir = tempfile::tempdir().unwrap();
    let out = brief_json(&data_of(&dir));
    for k in ["distill_lag_turns", "last_extract_at_ms", "quota_exhausted"] {
        assert!(out.as_object().unwrap().contains_key(k), "节点面少了键 {k}：{out}");
        assert_eq!(out[k], Value::Null, "{k} 在节点面给出了非 null 读数，它的来源是什么？");
    }
    // 能力声明那一格仍然两面同值（P-54 后半段在节点面上的那一半）。
    assert_eq!(out["capabilities"]["extract_state"], "queryable",
               "节点面把自己的不可读写进了能力声明：{out}");
}

#[test]
fn 运维读数不进_probe_面() {
    // 守卫 12 在 P5 上的那一格。`brief` 与 `probe` 是节点面的两条出口，各有唯一一种载荷：
    // 前者是运维读数（今天就有 `rows` 与 `index`），后者是记忆行。P5 的三格属于前者，
    // **不许**出现在 probe 的每一行里——那一行是要进模型上下文的。
    // 这一条把初稿那句"P5 的三格不进注入块"改成了它能被兑现的形状：`brief` 的输出**就是**
    // 注入载体（`status_block` 那一行围栏 JSON），所以"不进注入块"在节点面上无从成立；
    // 真正要守的是"不进记忆面"。
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let out = hook(&["probe", "--data-dir", data.to_str().unwrap(), &now_arg(), "--query", TOK]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("[recollect] "), "夹具召回没出声，本条就成了空转：{text}");
    for banned in ["quota_exhausted", "distill_lag_turns", "last_extract_at_ms"] {
        assert!(!text.contains(banned), "probe 面（进上下文那一面）出现运维键 {banned}：{text}");
    }
}