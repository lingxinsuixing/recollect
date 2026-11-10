//! G19 体验刀的工具层判据（刀 1 counts 面 / 刀 4 的分型与键名）。
//!
//! 缝载荷与三段计数的**线上半边**在 `mcp/tests/g19_selfserve.rs`（那里握着两个真库）；
//! 本文件证的是工具面自己的三件事：
//! ① `view=counts` 无实体名/工具名、缺省面不变；
//! ② `ingest_status` 的 no_rows 分型与回执键名（三段计数的算术在这里是**派生读数**
//!    ——`pending = max(0, total − distilled)` 的钳零相减住在 `tools::ingest_status`，
//!    所以桩给 `distilled > total` 的反常输入时，正确的读数是 0，不是负数也不是报错）；
//! ③ 缺省缝实现（没接这一格的面）是响亮的 Unavailable，不是 `None`——两种"没有"
//!    不许混成一格（P-54 的缝面形状）。
//!
//! 本 crate 依赖清单里没有 `rc-ledger`（守卫 6），所以这里的桩只喂裸读数——与
//! `tools_shape.rs::Tap` 的同一取舍：字面量进桩不进 src。

use rc_agent::tools::{ingest_status, status, StatusOptions, StatusView};
use rc_agent::{AgentErrorKind, CycleHealth, CycleRead, IngestCounts, ToolError, TurnExtraction};
use rc_memory::{put_entity, EntityContent, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
const TURN: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

/// 计数面用例的记忆库：一条实体 + 一次建索引——目录在缺省面非空，
/// "counts 面把它们拿掉了"才是可观察的断言，而不是"本来就没有"。
fn seeded_memory() -> &'static rc_store::MemoryHandle {
    use std::cell::RefCell;
    thread_local! {
        static MEM: RefCell<Option<&'static rc_store::MemoryHandle>> = const { RefCell::new(None) };
    }
    MEM.with(|slot| {
        if slot.borrow().is_none() {
            let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
            let root = Box::leak(Box::new(Root::open(dir.path()).unwrap()));
            put_entity(
                root.memory(),
                &EntityContent {
                    kind: "person".into(),
                    title: "目录夹具的老王".into(),
                    scope: Scope { kind: ScopeKind::Project, key: "proj-g19".into() },
                    aliases: vec![],
                },
                &[Provenance { turn_uid: TURN.into(), anchor_role: rc_memory::AnchorRole::Human }],
                NOW,
            )
            .unwrap();
            rc_index::rebuild(root.memory(), NOW).unwrap();
            *slot.borrow_mut() = Some(root.memory());
        }
        slot.borrow().unwrap()
    })
}

/// G19 的缝桩：只有 `ingest_counts` 有编排位，其余读数给合法零值——
/// status 的计数面用例要跑通纯 status 路径，`cycle_health` 必须在场。
struct G19Seam {
    ingest: Result<Option<IngestCounts>, ToolError>,
}
impl CycleRead for G19Seam {
    fn turn_extraction(&self, _turn_uid: &str) -> Result<Option<TurnExtraction>, ToolError> {
        Ok(Some(TurnExtraction {
            extract_state: "pending".into(),
            reason: None,
            eta_turns: 0,
        }))
    }
    fn cycle_health(&self) -> Result<CycleHealth, ToolError> {
        Ok(CycleHealth {
            lag_turns: 0,
            last_success_at_ms: None,
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
    fn ingest_counts(&self, _session_key: &str) -> Result<Option<IngestCounts>, ToolError> {
        self.ingest.clone()
    }
}

/// **不带** `ingest_counts` 覆写的缝：它走 trait 的缺省实现。用它与
/// `tools_shape.rs::Tap`（同样不覆写）一起钉住"缺省 = 响亮拒绝"。
struct BareSeam;
impl CycleRead for BareSeam {
    fn turn_extraction(&self, _turn_uid: &str) -> Result<Option<TurnExtraction>, ToolError> {
        Ok(None)
    }
    fn cycle_health(&self) -> Result<CycleHealth, ToolError> {
        Err(ToolError::unavailable("store_unreachable", "run: doctor"))
    }
    fn halted_scopes(&self) -> Result<i64, ToolError> { Ok(0) }
    fn lag_scopes(&self) -> Result<Vec<(String, i64)>, ToolError> { Ok(Vec::new()) }
}

fn v(s: String) -> Value {
    serde_json::from_str(&s).expect("返回不是合法 JSON")
}

// ── 刀 1：counts 面 ──────────────────────────────────────────────────────────

#[test]
fn status_counts_view_drops_the_directory_and_the_declaration() {
    let m = seeded_memory();
    // 缺省面（回归基准）：声明与目录都在——"拿掉了"的前提是"本来就有"。
    let full = v(status(m, None, &StatusOptions::default(), NOW).unwrap().text());
    assert!(full.get("capabilities").is_some(), "缺省面丢了能力声明：{full}");
    assert!(full["capabilities"]["tools"].as_array().is_some_and(|t| !t.is_empty()),
            "缺省面的工具名清单不在场：{full}");
    assert!(full.get("entities").is_some(), "缺省面丢了实体目录：{full}");
    assert_eq!(full["entities_total"], json!(1), "夹具的一条实体没进目录：{full}");

    let counts = v(status(m, None,
                          &StatusOptions { view: StatusView::Counts, ..Default::default() },
                          NOW).unwrap().text());
    assert!(counts.get("capabilities").is_none(), "counts 面带了工具能力声明：{counts}");
    assert!(counts.get("entities").is_none(), "counts 面带了实体名列表：{counts}");
    assert!(counts.get("entities_total").is_none(), "counts 面带了目录计数：{counts}");
    assert!(counts.get("entities_unlisted").is_none(), "counts 面带了未列出数：{counts}");
    // 计数格照旧在场：各表行数 + 滞后 + attention + freshness + 索引。
    for key in ["rows", "attention_open", "distill_lag_turns", "freshness", "index",
                "extract_state", "schema_version", "source"] {
        assert!(counts.get(key).is_some(), "counts 面丢了计数格 {key}：{counts}");
    }
    // 实体名一个字都不许出现（"无实体名"按内容断，不只按键名断）。
    assert!(!counts.to_string().contains("目录夹具的老王"), "counts 面带出了实体名：{counts}");
}

#[test]
fn status_counts_rejects_the_entity_filter_instead_of_silently_dropping_it() {
    let m = seeded_memory();
    let e = status(m, None,
                   &StatusOptions { view: StatusView::Counts, entity: Some("老王"),
                                    ..Default::default() },
                   NOW).unwrap_err();
    assert_eq!(e.kind, AgentErrorKind::Validation, "{e}");
    assert_eq!(e.code, "counts_and_entity", "{e}");
    // 静默忽略过滤 = 把"我筛过了"读成"全量计数"，所以响亮拒绝而不是丢掉输入。
}

#[test]
fn status_counts_keeps_the_per_turn_extraction_readout() {
    let m = seeded_memory();
    // turn 与 counts 不冲突：extraction 是每轮读数，不是目录（与 entity 的互斥分家）。
    let tap = G19Seam { ingest: Ok(None) };
    let out = v(status(m, Some(&tap),
                       &StatusOptions { view: StatusView::Counts, turn: Some(TURN),
                                        ..Default::default() },
                       NOW).unwrap().text());
    assert!(out.get("capabilities").is_none(), "counts 面带了声明：{out}");
    assert_eq!(out["extraction"]["extract_state"], json!("pending"), "{out}");
}

// ── 刀 4：ingest_status 的分型与键名 ────────────────────────────────────────

#[test]
fn ingest_status_a_ghost_session_is_no_rows_not_an_error() {
    let seam = G19Seam { ingest: Ok(None) };
    let e = ingest_status(&seam, "sess-ghost").unwrap_err();
    assert_eq!(e.kind, AgentErrorKind::Empty, "{e}");
    assert_eq!(e.code, "no_rows", "{e}");
    // mcp 面按**码**把 no_rows 翻成 isError=false（谁发都一样），所以这里只钉分型。
}

#[test]
fn ingest_status_shapes_the_two_phase_counts_with_saturating_pending() {
    let seam = G19Seam { ingest: Ok(Some(IngestCounts {
        turns_total: 5, distilled: 2, last_arrival_ms: Some(42),
    })) };
    let out = v(ingest_status(&seam, "sess-g19").unwrap().text());
    assert_eq!(out["session_key"], json!("sess-g19"), "{out}");
    assert_eq!(out["turns_total"], json!(5), "{out}");
    assert_eq!(out["turns_distilled"], json!(2), "{out}");
    assert_eq!(out["turns_pending"], json!(3), "{out}");
    assert_eq!(out["last_arrival_ms"], json!(42), "{out}");
    // 反常输入（残留终态行 > 轮数）：饱和到 0，不是负数、不是报错——
    // 与 `cycle_health.lag_turns` 的同一条规则。
    let seam = G19Seam { ingest: Ok(Some(IngestCounts {
        turns_total: 1, distilled: 3, last_arrival_ms: None,
    })) };
    let out = v(ingest_status(&seam, "sess-g19").unwrap().text());
    assert_eq!(out["turns_pending"], json!(0), "purge 残留行不许读成负积压：{out}");
    assert_eq!(out["last_arrival_ms"], Value::Null, "一条都没到是 null，不是 0：{out}");
}

/// 缺省缝实现（`hook` 一侧没接这一格）必须是响亮的 Unavailable——
/// "这面答不了"与"会话不在"（`Ok(None)`）是两种事实，不许折成一格。
#[test]
fn ingest_status_on_a_seam_without_the_reading_is_unavailable() {
    // `BareSeam` 不覆写 `ingest_counts` ⇒ 走 trait 的缺省实现（与 `Tap::default()` 同形）。
    let e = ingest_status(&BareSeam, "sess-g19").unwrap_err();
    assert_eq!(e.kind, AgentErrorKind::Unavailable, "{e}");
    assert_eq!(e.code, "extraction_not_readable", "{e}");
}
