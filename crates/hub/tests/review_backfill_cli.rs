// crates/hub/tests/review_backfill_cli.rs
//! T1：`hub review backfill-verdict` 的真进程面。
//!
//! 这一条命令只做一件事：把**历史**已 applied 的 fact/event 载荷，按内容寻址重算出卡 id，
//! 命中且仍是 `unreviewed` 的卡用 `promote_verdict` 单向 CAS 提升成 `passed`——S4 只回填
//! 当次 `RunOutcome.card_ids`，这 35 张卡停在 `unreviewed` 等 audit 兜底。
//!
//! 用例只钉边界，不测存储层（`promote_verdict` 自己的三态在 `rc-memory/tests/writers.rs`）：
//! - 干跑只读：清单里 `promote/already/absent` 三档齐，库里一个字节都不动；
//! - 真跑只动命中且 `unreviewed` 的卡；`rejected` 的 item 即使算得出同一张卡也不许碰；
//! - 幂等：第二次 `promoted=0`；
//! - 回滚只碰清单内的 id，清单外的 passed 卡原样不动；
//! - `--pass` 只认 fact/event；naming/time 是用法错误。
//!
//! 载荷形状与写库点 `crates/rc-memory/src/writers_fact.rs:464-467` 同源：
//! `kind = item.category`、`title = text` 前 30 个 char、`scope = payload.owner.{scope_kind,scope_key}`。
use rc_memory::review::{
    enqueue, record_decision, Decider, Decision, NewItem, ReviewStage, ReviewVerdict,
};
use rc_memory::{put_card, AnchorRole, FactContent, MemKey, Provenance, Scope, ScopeKind, Verdict};
use rc_store::Root;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DATA: &str = "data";
const NOW: i64 = 1_700_000_000_000;
const SCOPE_KEY: &str = "proj-backfill";
/// 长于 30 个 char 的正文：卡 title 是 `text.chars().take(30)`，重算必须逐字照抄这一格。
const LONG_TEXT: &str = "历史已审核卡回填必须只认内容寻址重算出来的那个 id，不许自带一份规范化";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON")
}

fn data_dir(dir: &Path) -> String {
    dir.join(DATA).to_str().unwrap().to_string()
}

fn open(dir: &Path) -> Root {
    Root::open(&dir.join(DATA)).unwrap()
}

fn scope() -> Scope {
    Scope {
        kind: ScopeKind::Project,
        key: SCOPE_KEY.to_string(),
    }
}

fn title_of(text: &str) -> String {
    text.chars().take(30).collect()
}

fn provenance() -> Vec<Provenance> {
    vec![Provenance {
        turn_uid: "a".repeat(64),
        anchor_role: AnchorRole::Human,
    }]
}

/// 造一张卡：id 由写库层自己算（与 `backfill` 重算的是同一条公式）。
fn make_card(root: &Root, category: &str, text: &str) -> MemKey {
    put_card(
        root.memory(),
        &FactContent {
            kind: category.to_string(),
            title: title_of(text),
            body: text.to_string(),
            scope: scope(),
            valid_to_ms: None,
        },
        &provenance(),
        NOW,
    )
    .unwrap()
}

/// 载荷：只带 backfill 读得懂的几格（`items[].category` / `items[].text` 与 `owner`）。
fn payload(pass_id: &str, family: &str, items: &[(&str, &str)]) -> String {
    let items: Vec<Value> = items
        .iter()
        .map(|(category, text)| json!({"category": category, "text": text, "quote": "q"}))
        .collect();
    json!({
        "schema": "review.pass.v1",
        "pass_id": pass_id,
        "family": family,
        "run_id": "run-backfill",
        "batch": {"turn_uids": ["a".repeat(64)], "source_agent": "agent-cli",
                  "device_id": "dev-cli", "session_key": "sess-cli", "source_ms": NOW},
        "owner": {"owner_principal_id": null, "owner_agent_id": "agent-cli",
                  "visibility": "private_agent", "retention": "default",
                  "scope_kind": "project", "scope_key": SCOPE_KEY},
        "items": items,
    })
    .to_string()
}

/// 入队一条（`tag` 进 session/turn 身份：`batch_key` 不去重才长得成第二条 item）。
fn enqueue_item(root: &Root, pass_id: &str, family: &str, payload_json: &str) -> (i64, String) {
    enqueue_tagged(root, "a", pass_id, family, payload_json)
}

fn enqueue_tagged(
    root: &Root,
    tag: &str,
    pass_id: &str,
    family: &str,
    payload_json: &str,
) -> (i64, String) {
    let item = NewItem {
        source: "distill".into(),
        pass_id: pass_id.into(),
        family: family.into(),
        session_key: format!("sess-cli-{tag}"),
        device_id: "dev-cli".into(),
        turn_uids_json: format!("[\"turn-cli-{tag}\"]"),
        owner_json: "{\"owner_agent_id\":\"agent-cli\"}".into(),
        payload_json: payload_json.into(),
        batch_key: String::new(),
    };
    let out = enqueue(root.memory(), &item, NOW).unwrap();
    (out.id, out.payload_sha256)
}

fn decision(
    stage: ReviewStage,
    verdict: ReviewVerdict,
    reason: &str,
    decider: Decider,
    sha: &str,
) -> Decision {
    Decision {
        stage,
        verdict,
        reason_code: reason.into(),
        rules_version: "review-gate-v0.1".into(),
        model: String::new(),
        prompt_version: String::new(),
        input_sha256: sha.into(),
        review_input_sha256: String::new(),
        decider,
        detail: "test".into(),
    }
}

/// 三闸全过 ⇒ `applied`（终态）。
fn to_applied(root: &Root, id: i64, sha: &str) {
    for (stage, verdict, reason) in [
        (ReviewStage::Program, ReviewVerdict::Pass, "program_pass"),
        (ReviewStage::Review, ReviewVerdict::Pass, "review_pass"),
        (ReviewStage::Apply, ReviewVerdict::Pass, "apply_pass"),
    ] {
        let decider = match stage {
            ReviewStage::Review => Decider::Center,
            _ => Decider::Program,
        };
        record_decision(
            root.memory(),
            id,
            &decision(stage, verdict, reason, decider, sha),
            NOW,
        )
        .unwrap();
    }
}

/// 程序侧"不确定" ⇒ `quarantined`（合法边只有 pending/approved 起点）。
fn to_quarantined(root: &Root, id: i64, sha: &str) {
    record_decision(
        root.memory(),
        id,
        &decision(
            ReviewStage::Program,
            ReviewVerdict::Uncertain,
            "review_uncertain",
            Decider::Program,
            sha,
        ),
        NOW,
    )
    .unwrap();
}

fn to_rejected(root: &Root, id: i64, sha: &str) {
    record_decision(
        root.memory(),
        id,
        &decision(
            ReviewStage::Program,
            ReviewVerdict::Reject,
            "turn_missing",
            Decider::Program,
            sha,
        ),
        NOW,
    )
    .unwrap();
}

fn verdict_of(root: &Root, id: &str) -> Option<String> {
    root.memory()
        .conn()
        .query_row("SELECT verdict FROM card WHERE id=?1", [id], |r| r.get(0))
        .ok()
}

/// 全库快照：三张会被这条命令"理论上可能碰到"的表逐行读出。干跑前后必须逐字节相等。
fn snapshot(root: &Root) -> (Vec<(String, String)>, i64, i64, i64) {
    let mut cards: Vec<(String, String)> = root
        .memory()
        .conn()
        .prepare("SELECT id, verdict FROM card ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    cards.sort();
    let count = |sql: &str| -> i64 {
        root.memory()
            .conn()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    };
    (
        cards,
        count("SELECT count(*) FROM review_item"),
        count("SELECT count(*) FROM review_decision"),
        count("SELECT count(*) FROM resource_owner"),
    )
}

fn verdict_map(root: &Root) -> BTreeMap<String, String> {
    snapshot(root).0.into_iter().collect()
}

fn actions(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["action"].as_str().unwrap().to_string())
        .collect()
}

fn count_action(rows: &[Value], action: &str) -> usize {
    rows.iter().filter(|r| r["action"] == action).count()
}

fn manifest_arg(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

fn write_manifest(path: &Path, body: &Value) {
    std::fs::write(path, serde_json::to_string_pretty(body).unwrap()).unwrap();
}

/// 夹具：两张卡（一 promotable、一 already passed）、一条 applied fact（三项：命中/已过/不存在），
/// 一条 rejected fact（其卡在库里且 unreviewed）、一条 quarantined event（同上）、一条 naming applied。
struct Fixture {
    _dir: tempfile::TempDir,
    root_dir: PathBuf,
    promotable: MemKey,
    already: MemKey,
    low: MemKey,
    rejected_card: MemKey,
    quarantined_card: MemKey,
    /// 不存在的卡对应的正文（`absent` 那一档）。
    absent_text: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root_dir = dir.path().to_path_buf();
    let root = open(&root_dir);
    let promotable = make_card(&root, "decision", LONG_TEXT);
    let already = make_card(&root, "plan", "这一张在历史里已经 passed");
    let low = make_card(
        &root,
        "decision",
        "这张卡被 audit 判成 low，review 不许翻案",
    );
    let rejected_card = make_card(&root, "decision", "被驳回的 item 算得出这张卡");
    let quarantined_card = make_card(&root, "action", "被隔离的 item 也算得出这张卡");
    rc_memory::set_verdict(root.memory(), &already, Verdict::Passed, NOW).unwrap();
    rc_memory::set_verdict(root.memory(), &low, Verdict::Low, NOW).unwrap();
    let absent_text = "这条 item 当年被 INSERT OR IGNORE 去重，卡根本没插进去".to_string();

    let (id, sha) = enqueue_item(
        &root,
        "fact",
        "decision",
        &payload(
            "fact",
            "decision",
            &[
                ("decision", LONG_TEXT),
                ("plan", "这一张在历史里已经 passed"),
                ("decision", &absent_text),
                ("decision", "这张卡被 audit 判成 low，review 不许翻案"),
            ],
        ),
    );
    to_applied(&root, id, &sha);

    let (id, sha) = enqueue_item(
        &root,
        "fact",
        "decision",
        &payload(
            "fact",
            "decision",
            &[("decision", "被驳回的 item 算得出这张卡")],
        ),
    );
    to_rejected(&root, id, &sha);

    let (id, sha) = enqueue_item(
        &root,
        "event",
        "",
        &payload("event", "", &[("action", "被隔离的 item 也算得出这张卡")]),
    );
    to_quarantined(&root, id, &sha);

    let (id, sha) = enqueue_item(
        &root,
        "naming",
        "",
        &payload("naming", "", &[("decision", LONG_TEXT)]),
    );
    to_applied(&root, id, &sha);

    Fixture {
        _dir: dir,
        root_dir,
        promotable,
        already,
        low,
        rejected_card,
        quarantined_card,
        absent_text,
    }
}

/// 1. 干跑：三档齐全、`rejected`/`quarantined`/`naming` 一条都不进清单，库里一字节不动。
#[test]
fn dry_run_lists_promote_already_and_absent_without_writing() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    let manifest = f.root_dir.join("dry.json");
    {
        let root = open(&f.root_dir);
        let before = snapshot(&root);
        let out = run(&[
            "review",
            "backfill-verdict",
            "--dry-run",
            "--json",
            "--manifest",
            &manifest_arg(&manifest),
            "--data-dir",
            &dd,
        ]);
        assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
        let rows = stdout_json(&out);
        let rows = rows.as_array().expect("--json 是数组");
        assert_eq!(
            actions(rows),
            vec!["promote", "already", "absent", "already"],
            "清单必须恰好是 applied fact 的四项：{rows:?}"
        );
        assert_eq!(
            rows[0]["title"],
            title_of(LONG_TEXT),
            "title 是 text 前 30 个 char"
        );
        assert_eq!(rows[0]["card_id"], f.promotable.id);
        assert_eq!(rows[0]["verdict_before"], "unreviewed");
        assert_eq!(rows[0]["item_index"], 0);
        assert!(rows[0]["review_item_id"].as_i64().unwrap() > 0);
        assert_eq!(rows[1]["card_id"], f.already.id);
        assert_eq!(rows[1]["verdict_before"], "passed");
        assert_eq!(
            rows[2]["verdict_before"],
            Value::Null,
            "absent 没有 verdict_before"
        );
        // audit 判过的 low：命中卡但**不是**提升目标，verdict_before 带着实值（不冒充 passed）。
        assert_eq!(rows[3]["card_id"], f.low.id);
        assert_eq!(rows[3]["verdict_before"], "low");
        // 被驳回/被隔离的 item 算得出真实存在的卡，但一条都不许出现在清单里。
        for row in rows {
            assert_ne!(
                row["card_id"], f.rejected_card.id,
                "rejected 的 item 进清单了：{row}"
            );
            assert_ne!(
                row["card_id"], f.quarantined_card.id,
                "quarantined 的 item 进清单了：{row}"
            );
        }
        // 干跑不写一个字节：card / review_item / review_decision / resource_owner 全部不动。
        assert_eq!(snapshot(&root), before, "干跑改动了库");
        assert_eq!(
            verdict_of(&root, &f.promotable.id).as_deref(),
            Some("unreviewed"),
            "干跑不许提升"
        );
    }
    // 清单落盘了（干跑的清单就是回滚清单）。
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), 4);
}

/// 2. 真跑：只提升命中且 `unreviewed` 的卡；第二次 `promoted=0`（幂等）。
#[test]
fn a_real_run_promotes_only_unreviewed_cards_and_is_idempotent() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    let manifest = f.root_dir.join("real.json");
    let out = run(&[
        "review",
        "backfill-verdict",
        "--json",
        "--manifest",
        &manifest_arg(&manifest),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let rows = stdout_json(&out);
    let rows = rows.as_array().unwrap();
    assert_eq!(count_action(rows, "promote"), 1, "{rows:?}");
    assert_eq!(count_action(rows, "already"), 2);
    assert_eq!(count_action(rows, "absent"), 1);

    {
        let root = open(&f.root_dir);
        assert_eq!(
            verdict_of(&root, &f.promotable.id).as_deref(),
            Some("passed")
        );
        assert_eq!(verdict_of(&root, &f.already.id).as_deref(), Some("passed"));
        // 命中但被 audit 判成 low 的卡：review 不翻案（这一格是 set_verdict 变异体的靶）。
        assert_eq!(
            verdict_of(&root, &f.low.id).as_deref(),
            Some("low"),
            "low 被回填翻案了（用了 set_verdict？）"
        );
        // 被驳回/被隔离的卡不许被提升。
        assert_eq!(
            verdict_of(&root, &f.rejected_card.id).as_deref(),
            Some("unreviewed"),
            "rejected 的卡被碰了"
        );
        assert_eq!(
            verdict_of(&root, &f.quarantined_card.id).as_deref(),
            Some("unreviewed"),
            "quarantined 的卡被碰了"
        );
        // T0 的后果要在这里看得见：提升后的卡退出 audit 复查池（`unreviewed_rows`）。
        let pool: Vec<String> = rc_memory::unreviewed_rows(root.memory(), 100)
            .unwrap()
            .into_iter()
            .map(|r| r.key.id)
            .collect();
        assert!(
            !pool.contains(&f.promotable.id),
            "提升后的卡仍在 audit 复查池里"
        );
        assert!(
            pool.contains(&f.rejected_card.id),
            "未被提升的卡必须留在复查池里"
        );
    }

    let out = run(&["review", "backfill-verdict", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let rows = stdout_json(&out);
    let rows = rows.as_array().unwrap();
    assert_eq!(count_action(rows, "promote"), 0, "第二次还有提升：{rows:?}");
    assert_eq!(count_action(rows, "already"), 3);
    assert_eq!(count_action(rows, "absent"), 1);
}

/// 3. 真跑的清单只写"实际提升的 id"——回滚清单因此可以不看 action 直接用。
#[test]
fn the_real_run_manifest_carries_only_the_promoted_ids() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    let manifest = f.root_dir.join("real.json");
    let out = run(&[
        "review",
        "backfill-verdict",
        "--json",
        "--manifest",
        &manifest_arg(&manifest),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(
        saved,
        json!([f.promotable.id]),
        "真跑清单只装提升成功的卡 id"
    );
}

/// 4. `--pass` 只认 fact/event；naming/time 显式拒绝（用法错误，先于开库）。
#[test]
fn pass_selection_refuses_naming_and_time() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    for bad in ["naming", "time", "rows", "state", ""] {
        let out = run(&[
            "review",
            "backfill-verdict",
            "--pass",
            bad,
            "--dry-run",
            "--data-dir",
            &dd,
        ]);
        assert_eq!(
            code(&out),
            3,
            "--pass {bad} 应为用法错误：{}",
            stdout_text(&out)
        );
        assert!(
            stderr_text(&out).contains("review backfill-verdict --pass must be fact|event"),
            "--pass {bad} 的错误面不对：{}",
            stderr_text(&out)
        );
    }
    // `--pass fact` 只扫 fact：同一份夹具里 event 的 quarantined 卡本来就不该动，
    // 这里再多给一点：把 event 那条改成 applied，`--pass fact` 仍然不许碰它。
    let root = open(&f.root_dir);
    // 同一段正文、另一批（`run_id` 不同 ⇒ 载荷指纹不同 ⇒ 新 item，不是顶替）。
    let second_event = payload("event", "", &[("action", "被隔离的 item 也算得出这张卡")])
        .replace("run-backfill", "run-backfill-b");
    let (id, sha) = enqueue_tagged(&root, "b", "event", "", &second_event);
    to_applied(&root, id, &sha);
    drop(root);
    let out = run(&[
        "review",
        "backfill-verdict",
        "--pass",
        "fact",
        "--json",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let rows = stdout_json(&out);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 4, "--pass fact 混进了 event 的项：{rows:?}");
    let root = open(&f.root_dir);
    assert_eq!(
        verdict_of(&root, &f.quarantined_card.id).as_deref(),
        Some("unreviewed"),
        "--pass fact 碰了 event 产出的卡"
    );
    // 缺省是两个 pass 都扫：这一次 event 那一项会进清单。
    let out = run(&["review", "backfill-verdict", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 5);
}

/// 5. `--revert`：只碰清单内、当前 `passed` 的 id；清单外的 passed 卡原样不动；幂等。
#[test]
fn revert_only_touches_manifest_ids_and_is_idempotent() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    let manifest = f.root_dir.join("real.json");
    let out = run(&[
        "review",
        "backfill-verdict",
        "--json",
        "--manifest",
        &manifest_arg(&manifest),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));

    // 清单里带一张 audit 判过 low 的卡：它"当前不是 passed"⇒ 不回滚、不改写（`not_passed`）。
    // 这一格是"回滚用 set_verdict 而不是 CAS"的靶（那会把 low 顺手改成 unreviewed）。
    let mixed = f.root_dir.join("mixed.json");
    write_manifest(&mixed, &json!([f.low.id, f.promotable.id]));
    let out = run(&[
        "review",
        "backfill-verdict",
        "--revert",
        &manifest_arg(&mixed),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    // 清单里两项：一张 passed（退得动）+ 一张 low（不是回滚目标），另有 `already` 在清单外。
    assert!(
        stdout_text(&out).contains("reverted=1 not_passed=1"),
        "{}",
        stdout_text(&out)
    );
    {
        let root = open(&f.root_dir);
        assert_eq!(
            verdict_of(&root, &f.promotable.id).as_deref(),
            Some("unreviewed"),
            "清单内的 id 没退回"
        );
        assert_eq!(
            verdict_of(&root, &f.low.id).as_deref(),
            Some("low"),
            "回滚把 audit 的 low 改写了"
        );
        assert_eq!(
            verdict_of(&root, &f.already.id).as_deref(),
            Some("passed"),
            "清单外的 passed 卡被碰了"
        );
    }

    // 再回滚一次同一份清单：0 行，不是错误。
    let out = run(&[
        "review",
        "backfill-verdict",
        "--revert",
        &manifest_arg(&mixed),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "重复回滚不是错误：{}", stderr_text(&out));
    assert!(
        stdout_text(&out).contains("reverted=0 not_passed=2"),
        "{}",
        stdout_text(&out)
    );

    // 清单里混一个不存在的 64-hex：0 行、不算错误、也不许碰别的卡。
    let bogus = f.root_dir.join("bogus.json");
    write_manifest(&bogus, &json!(["b".repeat(64)]));
    let out = run(&[
        "review",
        "backfill-verdict",
        "--revert",
        &manifest_arg(&bogus),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        code(&out),
        0,
        "清单里的陌生 id 不许变成错误：{}",
        stderr_text(&out)
    );
    assert!(
        stdout_text(&out).contains("reverted=0"),
        "{}",
        stdout_text(&out)
    );
    let root = open(&f.root_dir);
    assert_eq!(verdict_of(&root, &f.already.id).as_deref(), Some("passed"));
}

/// 6. 清单形状不对 ⇒ 用法错误（先于开库），一个字节都不写。
#[test]
fn a_malformed_manifest_is_a_usage_error() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    let bad = f.root_dir.join("bad.json");
    write_manifest(&bad, &json!(["NOT-A-CARD-ID"]));
    let before = {
        let root = open(&f.root_dir);
        verdict_map(&root)
    };
    let out = run(&[
        "review",
        "backfill-verdict",
        "--revert",
        &manifest_arg(&bad),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 3, "{}", stdout_text(&out));
    assert!(
        stderr_text(&out).contains("review backfill-verdict --revert needs a manifest of card ids"),
        "{}",
        stderr_text(&out)
    );
    let root = open(&f.root_dir);
    assert_eq!(verdict_map(&root), before, "用法错误改动了库");
}

/// 7. `--revert` 与 `--dry-run`/`--manifest`/`--pass` 同给是用法错误：两件事都不做。
#[test]
fn revert_refuses_to_share_the_command_with_a_scan() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    let m = f.root_dir.join("m.json");
    write_manifest(&m, &json!([]));
    let m_arg = manifest_arg(&m);
    for extra in [
        vec!["--dry-run"],
        vec!["--manifest", "m2.json"],
        vec!["--pass", "fact"],
    ] {
        let mut args = vec![
            "review",
            "backfill-verdict",
            "--revert",
            m_arg.as_str(),
            "--data-dir",
            &dd,
        ];
        args.extend(extra.iter().copied());
        let out = run(&args);
        assert_eq!(
            code(&out),
            3,
            "{extra:?} 应与 --revert 冲突：{}",
            stdout_text(&out)
        );
        assert!(
            stderr_text(&out).contains("review backfill-verdict --revert cannot be combined"),
            "{extra:?} 的错误面不对：{}",
            stderr_text(&out)
        );
    }
}

/// 8. `--limit` 按 item 分批：第一批只扫一条，第二次把剩下的扫完（可续）。
#[test]
fn limit_takes_a_batch_of_items_and_the_next_run_resumes() {
    let f = fixture();
    let dd = data_dir(&f.root_dir);
    {
        // 第二条 applied fact（一项）：分批才有区别。
        let root = open(&f.root_dir);
        let (id, sha) = enqueue_item(
            &root,
            "fact",
            "promise",
            &payload("fact", "promise", &[("promise", &f.absent_text)]),
        );
        to_applied(&root, id, &sha);
    }
    let out = run(&[
        "review",
        "backfill-verdict",
        "--dry-run",
        "--json",
        "--limit",
        "1",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let rows = stdout_json(&out);
    assert_eq!(
        rows.as_array().unwrap().len(),
        4,
        "--limit 1 应只扫第一条 item：{rows:?}"
    );
    let out = run(&[
        "review",
        "backfill-verdict",
        "--dry-run",
        "--json",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(
        stdout_json(&out).as_array().unwrap().len(),
        5,
        "第二条 applied item 也应被扫到"
    );

    // 分批真跑：第一轮提升 1 张，第二轮 already。
    let out = run(&[
        "review",
        "backfill-verdict",
        "--json",
        "--limit",
        "1",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    assert_eq!(
        count_action(stdout_json(&out).as_array().unwrap(), "promote"),
        1
    );
    let out = run(&["review", "backfill-verdict", "--json", "--data-dir", &dd]);
    assert_eq!(code(&out), 0, "stderr: {}", stderr_text(&out));
    let rows = stdout_json(&out);
    assert_eq!(
        count_action(rows.as_array().unwrap(), "promote"),
        0,
        "{rows:?}"
    );
}
