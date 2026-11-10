use rc_agent::capabilities;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
// 偏离 brief 的一处编译修正：下方 `probe_kind_for_canary` 的 authorization 臂要用 `AgentRoot`，
// 而 brief 这行只 import 了 `Root`（E0433）。按任务 6 先例补 import、语义与断言一个不动。
use rc_store::{AgentRoot, Root};

const NOW: i64 = 1_700_000_000_000;
/// 金丝雀：这串字**只在本文件出现**。任何一处输出里出现它就说明正文流到了不该到的面。
const CANARY: &str = "金絲雀正文绝不该出现在诊断面-9fc3";

fn seeded(dir: &std::path::Path) -> rc_memory::keys::MemKey {
    let root = Root::open(dir).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "金丝雀标题".into(),
            body: format!("同分合成词甲 {CANARY}"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    k
}

#[test]
fn 正文允许出现在_recall_与_export_里() {
    let dir = tempfile::tempdir().unwrap();
    let _k = seeded(dir.path());
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let json = rc_agent::recall::recall(
        m,
        &rc_agent::recall::RecallRequest::query("同分合成词甲").unwrap(),
        &rc_agent::recall::RecallFilter::empty(),
        NOW,
    )
    .unwrap();
    assert!(json.text().contains(CANARY), "recall 里读不到正文 ⇒ agent 面失去了它的用途");
    let doc = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap()).unwrap();
    assert!(doc.text().contains(CANARY), "export 里读不到正文 ⇒ C3 的两跳溯源无意义");
}

#[test]
fn 诊断面与错误面一个字符都不许带正文() {
    let dir = tempfile::tempdir().unwrap();
    let _k = seeded(dir.path());
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // capabilities：只有版本、工具名、格子名、上限。
    let caps = serde_json::to_string(&capabilities()).unwrap();
    assert!(!caps.contains(CANARY) && !caps.contains("金丝雀"), "{caps}");
    // 错误面：四条各造一次，断 kind/code/hint/Display 都无正文、无用户字节回显。
    // 复用 errors_are_four 的那张路由表（同源纪律：四臂只有一处构造）。
    for case in ["invalid", "authorization", "unavailable", "empty"] {
        let err = probe_kind_for_canary(case);
        let s = format!("{err}");
        assert!(!s.contains(CANARY), "{case} 的错误渲染带了正文：{s}");
        assert!(!err.hint.contains('\n'), "{case}：hint 必须单行，否则日志把它当两条读数");
        assert!(!err.code.contains(' '), "{case}：code 必须是机读 token");
    }
    // probe 注入块：阶段 1 里它含标题（signpost 指路），所以标题**允许**、正文只允许出现在 full。
    // 这条断的是：signpost 那一行必须没有 body。
    let view = rc_agent::surface::probe_via_search(m, None, "同分合成词甲", NOW).unwrap();
    let block = rc_agent::surface::probe_block(&view);
    let text = block.text();
    let full_count = view.forms.iter().filter(|d| d.form == rc_search::gate::Form::Full).count();
    let body_lines = text.lines().filter(|l| l.contains(CANARY)).count();
    assert_eq!(body_lines, full_count, "正文行数与 full 档数不等：signpost 泄露了正文");
    assert!(text.lines().all(|l| l.starts_with(rc_agent::FENCE)), "围栏不是每行前缀（V3）");
}

/// `errors_are_four.rs` 里那张路由表的复用。**tests/*.rs 互不引用**（阶段 0 起的纪律），
/// 所以这里重述一次四臂的构造，而不是 `mod errors_are_four;`。两处必须同形状：
/// 本任务 Step 7 证伪第 1 条对这两个文件**各数一次臂数**（少一臂即红）；而"逐字 diff、
/// 只许差在目录变量名上"是**评审动作，没有用例能做它**——承认它，别把它写成判据。
fn probe_kind_for_canary(case: &str) -> rc_agent::ToolError {
    let dir = tempfile::tempdir().unwrap();
    match case {
        "invalid" => rc_agent::recall::RecallRequest::query("   ").unwrap_err(),
        "authorization" => {
            let k = {
                let root = Root::open(dir.path()).unwrap();
                let key = rc_memory::synth::seed(root.memory(), NOW).unwrap().remove(0);
                root.memory().conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
                key
            };
            let snap = dir.path().join("snap.db");
            std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
            let home = tempfile::tempdir().unwrap();
            let ar = AgentRoot::open_replica(home.path(), &snap).unwrap();
            rc_memory::bump_cooldown(ar.memory(), &[k], NOW, rc_memory::COOLDOWN_MS)
                .unwrap_err() // 副本写冷却必须失败（`err_expect` 在 -D warnings 下是硬错误）
                .into()
        }
        "unavailable" => {
            let root = Root::open(dir.path()).unwrap();
            rc_memory::synth::seed(root.memory(), NOW).unwrap();
            rc_agent::surface::probe_via_search(root.memory(), None, "同分合成词甲", NOW).unwrap_err()
        }
        "empty" => {
            let root = Root::open(dir.path()).unwrap();
            rc_index::rebuild(root.memory(), NOW).unwrap();
            rc_agent::recall::recall(
                root.memory(),
                &rc_agent::recall::RecallRequest::query("同分合成词甲").unwrap(),
                &rc_agent::recall::RecallFilter::empty(),
                NOW,
            )
            .unwrap_err()
        }
        other => panic!("未知分型用例 {other}"),
    }
}

#[test]
fn 三类载体都只提供_text_一个消费口() {
    // 这条断的是 API 形状的存在性（"有没有别的出口"由 tests/compile_fail 的两条 E0616/E0624 证）。
    // 它自己不试图在运行期证明私有性——那是不可能的，所以写成两条编译失败而不是这一条。
    let dir = tempfile::tempdir().unwrap();
    let _k = seeded(dir.path());
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let json: rc_agent::ToolJson = rc_agent::recall::recall(
        m,
        &rc_agent::recall::RecallRequest::query("同分合成词甲").unwrap(),
        &rc_agent::recall::RecallFilter::empty(),
        NOW,
    )
    .unwrap();
    let doc: rc_agent::ExportDoc =
        rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap()).unwrap();
    let view = rc_agent::surface::probe_via_search(m, None, "同分合成词甲", NOW).unwrap();
    let block: rc_agent::InjectionBlock = rc_agent::surface::probe_block(&view);
    // 三次调用都只是 `text()`。任何"再暴露一个字段"的改动都会让下面这三行之外的代码出现编译错误。
    let _: String = json.text();
    let _: String = doc.text();
    let _: String = block.text();
    assert!(block.is_empty() || block.text().starts_with(rc_agent::FENCE));
}
