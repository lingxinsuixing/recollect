//! S3c：`status` 顶层那格 `freshness` 的三条判据（`last_extract_age_s` /
//! `newest_memory_age_s` / `pending_turns`）。
//!
//! 为什么这一档只测 `rc-agent` 的函数面、不在 `mcp` 再测一遍线上形状：`mcp` 的
//! `recollect_status` 把 `tools::status` 的 JSON 原样吐出去（live 面连一层包装都不加，
//! `read_view` 只在节点面才改写它），所以"键在不在、值是什么"只有一处分叉点，就是这里。
//! 两格的**生产源头**（`extract_pass_state` 的终态行）也在这层的缝后面：本 crate 依守卫 6
//! 拿不到 `rc-ledger`，所以夹具用假缝把 `CycleHealth` 的两个输入递进来——与
//! `tools_shape.rs` 里 P5/P6 同一个做法。

use rc_agent::tools::{status, StatusOptions};
use rc_agent::{CycleHealth, CycleRead, ToolError, TurnExtraction};
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;

/// 只编排 `CycleHealth` 的假缝。递进来的两格各自是一个生产读数：`lag_turns` 在生产侧是
/// `转录轮数 − extract_pass_state 终态行`，`last_success_at_ms` 是终态行 `updated_at_ms` 的
/// 最大值（S3b 换源后的口径）。本文件只关心 `status` 怎么把它们折成秒。
struct Seam {
    lag_turns: i64,
    last_success_at_ms: Option<i64>,
}

impl CycleRead for Seam {
    fn turn_extraction(&self, _turn_uid: &str) -> Result<Option<TurnExtraction>, ToolError> {
        Ok(None)
    }
    fn cycle_health(&self) -> Result<CycleHealth, ToolError> {
        Ok(CycleHealth {
            lag_turns: self.lag_turns,
            last_success_at_ms: self.last_success_at_ms,
            llm_calls_used: 0,
            last_derive_at_ms: None,
            derive_cursor: None,
            last_anomaly_scan_at_ms: None,
            quota_stopped: Some(false),
            llm_calls_total: Some(0),
        })
    }
    fn halted_scopes(&self) -> Result<i64, ToolError> { Ok(0) }
    fn lag_scopes(&self) -> Result<Vec<(String, i64)>, ToolError> { Ok(Vec::new()) }
}

fn v(s: String) -> Value {
    serde_json::from_str(&s).expect("返回不是合法 JSON")
}

/// 写一张 `as_of_ms = at_ms` 的活卡：`put_card` 的第四个参数就是这一行的 `as_of`。
/// 标题各自不同，否则内容寻址会把两张卡折成同一个 id。
fn card_at(dir: &std::path::Path, title: &str, at_ms: i64) {
    let root = Root::open(dir).unwrap();
    put_card(root.memory(), &FactContent {
        kind: "decision".into(),
        title: title.into(),
        body: "freshness 判据的合成正文。".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-freshness".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: "f".repeat(64), anchor_role: AnchorRole::Human }], at_ms).unwrap();
}

#[test]
fn freshness_空库三格全_null() {
    // 阴性那一半（"从来没有"）：空库 + 缝缺席 ⇒ 三格各是 `null`，**不是** 0。
    // 0 会被读成"刚刚抽过 / 一秒前写过一张卡 / 零条待抽"，那是三个截然不同的运维结论，
    // 而这一面其实什么都没发生过（P-54 那一族最像成功的失败）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    let f = out["freshness"].as_object().expect("freshness 必须是顶层对象");
    for k in ["last_extract_age_s", "newest_memory_age_s", "pending_turns"] {
        assert!(f.contains_key(k), "键 {k} 不在场 ⇒ 形状又少了一格：{out}");
        assert_eq!(f[k], Value::Null, "空库/缝缺席时 {k} 必须是 null 而不是 0：{out}");
    }
}

#[test]
fn freshness_写入后两格年龄取最新而不是最旧() {
    // 阳性那一半：两张不同时刻的卡 + 一条"60 秒前收尾的终态 pass 行"。
    let dir = tempfile::tempdir().unwrap();
    card_at(dir.path(), "较新的那张卡", NOW - 120_000);
    card_at(dir.path(), "较旧的那张卡", NOW - 600_000);
    let root = Root::open(dir.path()).unwrap();
    let seam = Seam { lag_turns: 2, last_success_at_ms: Some(NOW - 60_000) };
    let out = v(status(root.memory(), Some(&seam), &StatusOptions::default(), NOW).unwrap().text());
    let f = &out["freshness"];
    assert!(f.is_object(), "freshness 必须是顶层对象：{out}");
    // 整格精确相等，而不是逐格取下界：`now_ms` 是**调用方递进来的形参**（`status` 的第四个
    // 参数），三格的输入全是本文件亲手喂的固定时刻，所以 120/60/2 与墙钟无关、不会漂。
    // 整格比较还让 null 回归直接打出 left/right 两边（`as_i64().unwrap()` 只会 panic 一句
    // "called on None"，把"折成 null 还是折成 0"这个诊断吞掉）。
    // 取的是**最新**那一行：两张卡分别 120 秒/600 秒，读成 `min`（最旧）⇒ 600 ≠ 120 必红。
    // 不声称能咬住"取排序后第一条"：`facts` 的 `ORDER BY tbl, id` 里 id 是内容寻址哈希，
    // 两张卡谁在前由 sha256 决定，那种变异红不红是碰运气，写进断言只会变成假承诺。
    assert_eq!(f, &json!({
        "last_extract_age_s": 60,
        "newest_memory_age_s": 120,
        "pending_turns": 2,
    }), "60 秒前的终态行 + 最新一张卡在 120 秒前：{out}");
    // 与 lag **同源**：同一个值、同一个来处，不是第二次读账。
    assert_eq!(f["pending_turns"], out["distill_lag_turns"],
               "pending_turns 必须与 distill_lag_turns 同源：{out}");
}

#[test]
fn freshness_未来时刻的行把年龄饱和到零() {
    // 时钟回拨/跨机写入会让 `as_of` 落在"未来"：直减会给出负数，而负的年龄没有读法。
    // 饱和到 0（"刚刚"）是唯一不撒谎的档——不是 `null`：那一行确实在库里。
    let dir = tempfile::tempdir().unwrap();
    card_at(dir.path(), "时刻在未来的一张卡", NOW + 60_000);
    let root = Root::open(dir.path()).unwrap();
    let seam = Seam { lag_turns: 0, last_success_at_ms: Some(NOW + 5_000) };
    let out = v(status(root.memory(), Some(&seam), &StatusOptions::default(), NOW).unwrap().text());
    // 整格相等而不是两格逐取：三格都在同一个 `now_ms` 形参下算，0 是精确值，不随墙钟漂。
    assert_eq!(out["freshness"], json!({
        "last_extract_age_s": 0,
        "newest_memory_age_s": 0,
        "pending_turns": 0,
    }), "未来的 as_of 不许折成负年龄：{out}");
    assert_eq!(out["freshness"]["pending_turns"], out["distill_lag_turns"], "{out}");
}
