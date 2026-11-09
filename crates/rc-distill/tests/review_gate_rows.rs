//! S3c-1：`/v1/rows` 的 **fact 行**程序闸逐码用例（设计 `2026-10-26-review-queue-s3c.md`
//! §4、§6；派发档 `S3c-1-rows-gate.md` §4）。
//!
//! 夹具走真库：`seed_bodies` 把轮真导入转录库，锚用测试内 `insert_anchor` 直插 `anchor` 表
//! （与 `review_gate.rs` 同一套写法），不为测试改生产 API。每条用例只制造一个违规，
//! 断言只看 `code` 与 `passed`——断言信息里只有码，不打印候选正文、锚 id 或 `turn_uid`。
//!
//! 三组用例值得单独说明：
//! - `number_*`：C1 的口径是"数字对**证据轮**"，所以既有"邻居轮有、证据轮没有 ⇒ 拒"，
//!   也有"证据轮有、邻居轮没有 ⇒ 过"——后者证明读数取的是 `evidence_turn_uid` 而不是
//!   `turn_uids[0]`。
//! - `mode_*`：S3c-fix 起闸门判的是 `/v1/rows` 的**写库口径**（只收 `stated`），DDL 闭集里
//!   的 `inferred`/`reported` 也必须拒——放行它们同样是"闸过、写库另有口径"。
//! - `双错载荷_缺锚先于退场`：§3 的 7/8 是"先全查存在、再全查退场"，否则码序会取决于
//!   哪个锚排在前面。
//!
//! S3c-fix2 补的是 DDL 的 `NOT NULL` 面：`id`/`scope_kind`/`scope_key`/`source_agent`/
//! `device_id`/`created_at_ms` 逐列缺失/空串/错类型都必须落在 `payload_shape`，
//! 可空 `world_id` 非 null 必须是字符串（见 §9e）。

use rc_distill::review::{gate_rows, RowsCandidate, RowsGateOutcome, ROWS_GATE_CODES};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::{Scope, ScopeKind};
use rc_store::Root;
use serde_json::{json, Map, Value};

mod common;
use common::seed_bodies;

const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const NOW_MS: i64 = 1_800_000_000_000;

/// 证据轮的正文：含 `12`/`3` 两段数字，不含 `2026`。
const BODY: &str = "张三决定把发布推迟到 12 月 3 日";
/// 邻居轮（非证据）的正文：含 `2026`，不含 `12`/`3` 这两段数字。
const OTHER_BODY: &str = "发布会临时改到 2026 年";

fn owner(agent: &str) -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: None,
        owner_agent_id: agent.to_string(),
        visibility: Visibility::PrivateAgent,
        retention: "default".to_string(),
    }
}

/// 一行 fact 的最小合法列值：主体锚、闭集内的三格、无数字的正文、空参与者、
/// 必填的 `norm_version`（S3c-fix：`INTEGER NOT NULL`，闸必判）、以及 S3c-fix2 补齐的
/// `TEXT NOT NULL` 五格 + `created_at_ms`（`INTEGER NOT NULL`）。
/// 各条用例只改它需要改的那一格——"只有一处违规"。
fn base_values(evidence: &str) -> Map<String, Value> {
    json!({
        "id": "fact-base",
        "subject_anchor_id": "anchor-zhang",
        "predicate": "decide",
        "category": "decision",
        "mode": "stated",
        "text": "发布推迟",
        "evidence_turn_uid": evidence,
        "participants_json": "[]",
        "scope_kind": "session",
        "scope_key": "sess-batch-A/dev-batch-A",
        "source_agent": "qoder",
        "device_id": "dev-batch-A",
        "created_at_ms": 1_700_000_000_000i64,
        "norm_version": 1
    })
    .as_object()
    .expect("夹具的列值必须是对象")
    .clone()
}

fn set(values: &mut Map<String, Value>, key: &str, value: Value) {
    values.insert(key.to_string(), value);
}

fn clear(values: &mut Map<String, Value>, key: &str) {
    assert!(values.remove(key).is_some(), "夹具必须真有这一列：{key}");
}

fn candidate(uids: &[&str], values: &Map<String, Value>) -> RowsCandidate {
    RowsCandidate {
        owner: owner("agent-batch-A"),
        scope: Scope {
            kind: ScopeKind::Project,
            key: "proj-rows".to_string(),
        },
        session_key: SESSION.to_string(),
        device_id: DEVICE.to_string(),
        turn_uids: uids.iter().map(|uid| (*uid).to_string()).collect(),
        values: values.clone(),
    }
}

/// 一个**对夹具 owner 可见**的锚：`anchor` 行 + `resource_owner` 行（owner 就是候选的
/// owner `agent-batch-A`，private_agent）。没有归属行的锚在新闸下按 fail-closed 拒收
/// （`anchor_not_visible`），所以夹具必须同时写归属——与生产里命名 pass 的写入形状一致。
fn insert_anchor(root: &Root, id: &str, status: &str) {
    insert_owned_anchor(root, id, status, "agent-batch-A", "private_agent", "project", "proj-rows");
}

fn insert_owned_anchor(
    root: &Root,
    id: &str,
    status: &str,
    owner_agent: &str,
    visibility: &str,
    scope_kind: &str,
    scope_key: &str,
) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, 'person', ?1, 'real', 'project', 'p', ?2, 1, 1, 1)",
            rusqlite::params![id, status],
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, NULL, ?2, ?2, ?3, 'default', ?4, ?5, ?2, ?6, 1)",
            rusqlite::params![id, owner_agent, visibility, scope_kind, scope_key, DEVICE],
        )
        .unwrap();
}

fn run(root: &Root, cand: &RowsCandidate) -> RowsGateOutcome {
    gate_rows(root.memory(), root.transcript(), cand, NOW_MS).unwrap()
}

fn assert_code(root: &Root, cand: &RowsCandidate, expected: &'static str) {
    let outcome = run(root, cand);
    assert!(!outcome.passed, "应当失败（期望 {expected}）");
    assert_eq!(outcome.code, Some(expected));
}

fn assert_passes(root: &Root, cand: &RowsCandidate) {
    let outcome = run(root, cand);
    assert!(outcome.passed, "应当通过（code={:?}）", outcome.code);
    assert_eq!(outcome.code, None);
}

fn setup() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[BODY], 1_000);
    (dir, root, uids)
}

/// 两轮：`uids[0]` = 邻居轮（含 `2026`），`uids[1]` = 证据轮（含 `12`/`3`）。
fn setup_two() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[OTHER_BODY, BODY], 1_000);
    (dir, root, uids)
}

// ── 1. payload_shape ─────────────────────────────────────────────────────────

#[test]
fn payload_shape_缺_text_时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    clear(&mut values, "text");
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "payload_shape",
    );
}

#[test]
fn payload_shape_证据轮不在轮清单时拒收() {
    let (_dir, root, uids) = setup_two();
    let values = base_values(&uids[1]);
    // 轮清单只给邻居轮：证据轮不在其中。
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "payload_shape",
    );
}

#[test]
fn payload_shape_轮清单为空时拒收() {
    let (_dir, root, uids) = setup();
    let values = base_values(&uids[0]);
    assert_code(&root, &candidate(&[], &values), "payload_shape");
}

#[test]
fn payload_shape_value_json_是数组时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    set(&mut values, "value_json", json!(["不是标量"]));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "payload_shape",
    );
}

#[test]
fn payload_shape_参与者不是_json_数组时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    set(&mut values, "participants_json", json!("not-json"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "payload_shape",
    );
}

// ── 2. text_missing ─────────────────────────────────────────────────────────

#[test]
fn text_missing_正文只含空白时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    set(&mut values, "text", json!("  \t\n "));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "text_missing",
    );
}

// ── 3/4/5. 轮存在、会话/设备、墓碑 ─────────────────────────────────────────────

#[test]
fn turn_missing_幽灵轮时拒收() {
    let (_dir, root, _uids) = setup();
    let ghost = "a".repeat(64);
    let values = base_values(&ghost);
    assert_code(&root, &candidate(&[ghost.as_str()], &values), "turn_missing");
}

#[test]
fn turn_session_mismatch_会话对不上时拒收() {
    let (_dir, root, uids) = setup();
    let values = base_values(&uids[0]);
    let mut cand = candidate(&[uids[0].as_str()], &values);
    cand.session_key = "sess-other".to_string();
    assert_code(&root, &cand, "turn_session_mismatch");
}

#[test]
fn turn_session_mismatch_设备对不上时拒收() {
    let (_dir, root, uids) = setup();
    let values = base_values(&uids[0]);
    let mut cand = candidate(&[uids[0].as_str()], &values);
    cand.device_id = "dev-other".to_string();
    assert_code(&root, &cand, "turn_session_mismatch");
}

#[test]
fn turn_tombstoned_墓碑命中时拒收() {
    let (_dir, root, uids) = setup();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO tombstone(commit_seq, epoch, scope_kind, scope_key, turns_removed,
                                   content_sha256, created_at_ms)
             VALUES(1, 1, 'session', ?1, 1, 'deadbeef', 1)",
            [SESSION],
        )
        .unwrap();
    let values = base_values(&uids[0]);
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "turn_tombstoned",
    );
}

// ── 6. compaction_evidence ──────────────────────────────────────────────────

#[test]
fn compaction_evidence_证据轮是摘要块时拒收() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(
        &mut root,
        &["[[compact_summary]] 张三决定把发布推迟到 12 月 3 日"],
        1_000,
    );
    let values = base_values(&uids[0]);
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "compaction_evidence",
    );
}

// ── 7. ref_missing ─────────────────────────────────────────────────────────

#[test]
fn ref_missing_客体锚不存在时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "object_anchor_id", json!("anchor-missing-object"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "ref_missing",
    );
}

#[test]
fn ref_missing_参与者锚不存在时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(
        &mut values,
        "participants_json",
        json!(r#"["anchor-zhang","anchor-ghost"]"#),
    );
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "ref_missing",
    );
}

// ── 8. ref_retired ─────────────────────────────────────────────────────────

#[test]
fn ref_retired_主体锚已退场时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "retired");
    let values = base_values(&uids[0]);
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "ref_retired",
    );
}

#[test]
fn ref_retired_合并态锚也拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_anchor(&root, "anchor-merged", "merged");
    let mut values = base_values(&uids[0]);
    set(&mut values, "object_anchor_id", json!("anchor-merged"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "ref_retired",
    );
}

// ── 8b. anchor_not_visible：引用锚必须对行 owner 可见 ──────────────────────────

/// 行的主语锚是**另一个 owner 的私有锚**：闸门以前只查"锚在不在"就放行，于是
/// 行 owner=qoder、主语锚 owner=别人也能入库。现在按 `anchor_not_visible` 拒收。
#[test]
fn anchor_not_visible_主体是他人的私有锚时拒收() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-private-other",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "proj-rows",
    );
    let mut values = base_values(&uids[0]);
    set(&mut values, "subject_anchor_id", json!("anchor-private-other"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "anchor_not_visible",
    );
}

/// 参与者 / speaker 是他人私有锚：与 subject 同一条规则（引用格一个不落）。
#[test]
fn anchor_not_visible_参与者是他人的私有锚时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_owned_anchor(
        &root,
        "anchor-private-other",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "proj-rows",
    );
    let mut values = base_values(&uids[0]);
    set(
        &mut values,
        "participants_json",
        json!(r#"["anchor-zhang","anchor-private-other"]"#),
    );
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "anchor_not_visible",
    );
}

/// 没有 `resource_owner` 行的锚：fail-closed ⇒ `anchor_not_visible`（不是放行）。
#[test]
fn anchor_not_visible_无归属行的锚拒收() {
    let (_dir, root, uids) = setup();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES('anchor-orphan', 'person', '无归属锚', 'real', 'project', 'proj-rows',
                    'active', 1, 1, 1)",
            [],
        )
        .unwrap();
    let mut values = base_values(&uids[0]);
    set(&mut values, "subject_anchor_id", json!("anchor-orphan"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "anchor_not_visible",
    );
}

/// 别人的锚、visibility=project、scope 与候选全等 ⇒ 共享可见，照常通过。
#[test]
fn anchor_共享可见_他人的项目锚通过() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-shared",
        "active",
        "agent-other",
        "project",
        "project",
        "proj-rows",
    );
    let mut values = base_values(&uids[0]);
    set(&mut values, "subject_anchor_id", json!("anchor-shared"));
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
}

/// 私有锚 scope 全等也不可见：private_agent 是 owner-only。
#[test]
fn anchor_not_visible_私有锚_scope_相同也拒收() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-private-same-scope",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "proj-rows",
    );
    let mut values = base_values(&uids[0]);
    set(
        &mut values,
        "subject_anchor_id",
        json!("anchor-private-same-scope"),
    );
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "anchor_not_visible",
    );
}

/// 双错：主体是他人私有锚（可见性）**且** category 越界（闭集）⇒ 可见性先报。
#[test]
fn 双错载荷_可见性先于闭集() {
    let (_dir, root, uids) = setup();
    insert_owned_anchor(
        &root,
        "anchor-private-other",
        "active",
        "agent-other",
        "private_agent",
        "project",
        "proj-rows",
    );
    let mut values = base_values(&uids[0]);
    set(&mut values, "subject_anchor_id", json!("anchor-private-other"));
    set(&mut values, "category", json!("bogus"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "anchor_not_visible",
    );
}

// ── 9. 闭集：category / predicate / mode ────────────────────────────────────

#[test]
fn category_不在七值闭集时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "category", json!("bogus"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "category");
}

#[test]
fn predicate_去空白后为空时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "predicate", json!("   "));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "predicate");
}

/// `predicate` 没有闭集（与 fact 写库层同口径：只要求非空）——写一个闭集外但非空的谓词
/// 必须**通过**。这条用例挡住"顺手给 predicate 加一份 family 闭集"的改动。
#[test]
fn predicate_没有闭集_任意非空值都过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "predicate", json!("postpone-release"));
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
}

#[test]
fn mode_不在三值闭集时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "mode", json!("bogus"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "mode");
}

/// 闸门判的是**写库口径**：`/v1/rows` 只收 `stated`（与 fact 写库层 `FACT_MODE_STATED`
/// 同一个值，S3c-fix 复审 P1-a）。`inferred`/`reported` 是 DDL 闭集里的词，但写入口不收，
/// 放行它们就是"闸过、写库另有口径"的死循环面——所以必须按 `mode` 拒。
#[test]
fn mode_三值内的_inferred_也拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "mode", json!("inferred"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "mode");
}

#[test]
fn mode_三值内的_reported_也拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "mode", json!("reported"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "mode");
}

// ── 9b. status（S3c-fix：DDL 闭集，可空）─────────────────────────────────────

#[test]
fn status_不在四值闭集时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "status", json!("bogus"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "status");
}

/// 空串不是"没给"：`status` 的 DDL 只有四值 + NULL，空串会被 CHECK 拒 ⇒ 闸必须先拒。
#[test]
fn status_空串也拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "status", json!(""));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "status");
}

#[test]
fn status_四值逐个通过且_null_也通过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    for status in [json!("pending"), json!("done"), json!("cancelled"), json!("expired"), Value::Null] {
        let mut values = base_values(&uids[0]);
        set(&mut values, "status", status.clone());
        assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    }
}

// ── 9c. S3c-fix：放行即 503 的形状列（confidence/norm_version/毫秒）────────────

#[test]
fn payload_shape_confidence_是字符串时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    set(&mut values, "confidence", json!("0.9"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
}

#[test]
fn payload_shape_confidence_是布尔时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    set(&mut values, "confidence", json!(true));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
}

#[test]
fn confidence_数字与_null_都通过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    for confidence in [json!(0.8), json!(1), Value::Null] {
        let mut values = base_values(&uids[0]);
        set(&mut values, "confidence", confidence);
        assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    }
}

/// `norm_version` 是 `INTEGER NOT NULL`：缺这一格是最典型的"闸放行、写库必拒"。
#[test]
fn payload_shape_缺_norm_version_时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    clear(&mut values, "norm_version");
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
}

#[test]
fn payload_shape_norm_version_不是整数时拒收() {
    let (_dir, root, uids) = setup();
    for bad in [json!(1.5), json!("1"), json!(true), Value::Null] {
        let mut values = base_values(&uids[0]);
        set(&mut values, "norm_version", bad.clone());
        assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
    }
}

/// 六个毫秒列：可空；非空必须是 integer（`INTEGER` 列，浮点/字符串/布尔都进不去）。
#[test]
fn payload_shape_毫秒列不是整数时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    for column in [
        "occurred_at_ms",
        "occurred_end_ms",
        "speech_time_ms",
        "valid_from_ms",
        "valid_to_ms",
        "due_at_ms",
    ] {
        for bad in [json!(1.5), json!("1"), json!(true)] {
            let mut values = base_values(&uids[0]);
            set(&mut values, column, bad.clone());
            assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
        }
        // 同一列给合法值（整数或 null）必须通过。
        let mut values = base_values(&uids[0]);
        set(&mut values, column, json!(1_000));
        assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
        set(&mut values, column, Value::Null);
        assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    }
}

// ── 9d. speaker_anchor_id（S3c-fix：并入第 7/8 步的锚两遍扫）─────────────────

#[test]
fn ref_missing_speaker_锚不存在时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "speaker_anchor_id", json!("anchor-missing-speaker"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "ref_missing");
}

#[test]
fn ref_retired_speaker_锚已退场时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_anchor(&root, "anchor-speaker", "retired");
    let mut values = base_values(&uids[0]);
    set(&mut values, "speaker_anchor_id", json!("anchor-speaker"));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "ref_retired");
}

#[test]
fn speaker_锚在且未退场时通过_空串按不存在的锚拒() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    insert_anchor(&root, "anchor-speaker", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "speaker_anchor_id", json!("anchor-speaker"));
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    // 空串不是"没给"：写库的 FK 会拒一个不存在的 id ⇒ 闸必须先按 `ref_missing` 拒，
    // 否则就是"闸过、写库 503"的死循环（复审 P1-a）。
    set(&mut values, "speaker_anchor_id", json!(""));
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "ref_missing");
    // null / 缺键才是"没给"。
    set(&mut values, "speaker_anchor_id", Value::Null);
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    clear(&mut values, "speaker_anchor_id");
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
}

// ── 9e. S3c-fix2：NOT NULL 列（id/作用域/来源/created_at_ms）与可空 world_id ────

/// `fact` 的 `TEXT NOT NULL` 五格：缺键 ⇒ `payload_shape`（放行就是写库必拒）。
#[test]
fn payload_shape_必填文本列缺失时拒收() {
    let (_dir, root, uids) = setup();
    for column in ["id", "scope_kind", "scope_key", "source_agent", "device_id"] {
        let mut values = base_values(&uids[0]);
        clear(&mut values, column);
        assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
    }
}

/// 同一五格的空串：`NOT NULL` 拦不住空串，语义上就是"没给"，所以闸必须按形状拒。
#[test]
fn payload_shape_必填文本列为空串时拒收() {
    let (_dir, root, uids) = setup();
    for column in ["id", "scope_kind", "scope_key", "source_agent", "device_id"] {
        let mut values = base_values(&uids[0]);
        set(&mut values, column, json!(""));
        assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
    }
}

/// 同一五格的非字符串（数字/布尔/数组）：进不了 TEXT 列 ⇒ `payload_shape`。
#[test]
fn payload_shape_必填文本列不是字符串时拒收() {
    let (_dir, root, uids) = setup();
    for column in ["id", "scope_kind", "scope_key", "source_agent", "device_id"] {
        for bad in [json!(7), json!(true), json!(["x"])] {
            let mut values = base_values(&uids[0]);
            set(&mut values, column, bad.clone());
            assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
        }
    }
}

/// `created_at_ms` 是 `INTEGER NOT NULL`：缺这一格与 `norm_version` 同一条失败面。
#[test]
fn payload_shape_缺_created_at_ms_时拒收() {
    let (_dir, root, uids) = setup();
    let mut values = base_values(&uids[0]);
    clear(&mut values, "created_at_ms");
    assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
}

#[test]
fn payload_shape_created_at_ms_不是整数时拒收() {
    let (_dir, root, uids) = setup();
    for bad in [json!(1.5), json!("600"), json!(true), Value::Null] {
        let mut values = base_values(&uids[0]);
        set(&mut values, "created_at_ms", bad.clone());
        assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
    }
}

/// `world_id` 是 DDL 里的可空 TEXT：非 null 必须是字符串（数字/布尔/数组形状错）。
#[test]
fn payload_shape_world_id_不是字符串时拒收() {
    let (_dir, root, uids) = setup();
    for bad in [json!(1), json!(true), json!(["w"]), json!({"w": 1})] {
        let mut values = base_values(&uids[0]);
        set(&mut values, "world_id", bad.clone());
        assert_code(&root, &candidate(&[uids[0].as_str()], &values), "payload_shape");
    }
}

#[test]
fn world_id_字符串_null_缺省都通过() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "world_id", json!("world-a"));
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    set(&mut values, "world_id", Value::Null);
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
    clear(&mut values, "world_id");
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
}

// ── 10. number_not_grounded（C1：只看证据轮）──────────────────────────────────

#[test]
fn number_not_grounded_正文数字不在证据轮时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "text", json!("2026 年发布新版本"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "number_not_grounded",
    );
}

/// C1 的边界：`2026` 只出现在**邻居轮**（provenance 里、但不是证据轮），证据轮没有 ⇒ 必须拒。
/// 这条用例钉住"数字对证据轮"，不是"对全部 provenance 轮"。
#[test]
fn number_not_grounded_邻居轮有但证据轮没有也拒收() {
    let (_dir, root, uids) = setup_two();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[1]);
    set(&mut values, "text", json!("2026 年发布新版本"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str(), uids[1].as_str()], &values),
        "number_not_grounded",
    );
}

/// 反面：证据轮有 `2026`、轮清单里的**第一轮**没有 ⇒ 通过。读数取 `evidence_turn_uid`
/// 那一轮，不是 `turn_uids[0]`。
#[test]
fn number_只看证据轮_邻居轮没有也算过() {
    let (_dir, root, uids) = setup_two();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "text", json!("2026 年发布会"));
    assert_passes(
        &root,
        &candidate(&[uids[0].as_str(), uids[1].as_str()], &values),
    );
}

// ── 11. owner_unresolved ───────────────────────────────────────────────────

#[test]
fn owner_unresolved_空_agent_时拒收() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let values = base_values(&uids[0]);
    let mut cand = candidate(&[uids[0].as_str()], &values);
    cand.owner = owner("");
    assert_code(&root, &cand, "owner_unresolved");
}

// ── 通过基线 ───────────────────────────────────────────────────────────────

/// 一个真实 fact 行候选：subject/object/place/speaker/participant 五个锚都存在且未退场，
/// 正文里的数字在证据轮，类别/模式/状态在闭集内，owner 有值，**全部列**（含 S3c-fix 新纳入
/// 闸的 `status`/`confidence`/六个毫秒列/`norm_version`，以及 S3c-fix2 的 `id`/作用域/来源
/// 五格、`created_at_ms`、可空 `world_id`）按合法形状给全。
/// 这条是"全列合法通过"的基线：新增列若把合法值判错，会先红在这里。
#[test]
fn pass_基线_真实_fact_行通过() {
    let (_dir, root, uids) = setup();
    for (id, status) in [
        ("anchor-zhang", "active"),
        ("anchor-lisi", "provisional"),
        ("anchor-place", "active"),
        ("anchor-speaker", "active"),
        ("anchor-participant", "active"),
    ] {
        insert_anchor(&root, id, status);
    }
    let mut values = base_values(&uids[0]);
    set(&mut values, "text", json!("发布推迟到 12 月 3 日"));
    set(&mut values, "object_anchor_id", json!("anchor-lisi"));
    set(&mut values, "place_anchor_id", json!("anchor-place"));
    set(&mut values, "speaker_anchor_id", json!("anchor-speaker"));
    set(
        &mut values,
        "participants_json",
        json!(r#"["anchor-lisi","anchor-participant"]"#),
    );
    set(&mut values, "value_json", json!("12 月 3 日"));
    set(&mut values, "value_text", json!("12 月 3 日"));
    set(&mut values, "status", json!("done"));
    set(&mut values, "confidence", json!(0.9));
    set(&mut values, "occurred_at_ms", json!(1_700_000_000_000i64));
    set(&mut values, "speech_time_ms", json!(1_700_000_000_100i64));
    set(&mut values, "world_id", json!("world-a"));
    set(&mut values, "norm_version", json!(2));
    assert_passes(&root, &candidate(&[uids[0].as_str()], &values));
}

// ── 顺序与无泄漏 ───────────────────────────────────────────────────────────

/// 双错：subject 已退场（§3 第 8 步）**且** object 不存在（第 7 步）⇒ `ref_missing` 必须赢。
/// 一遍里"查一个判一个"的写法会报出 `ref_retired`。
#[test]
fn 双错载荷_缺锚先于退场() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "retired");
    let mut values = base_values(&uids[0]);
    set(&mut values, "object_anchor_id", json!("anchor-missing-object"));
    assert_code(
        &root,
        &candidate(&[uids[0].as_str()], &values),
        "ref_missing",
    );
}

/// 双错：缺 `text`（第 1 步）**且**幽灵轮（第 3 步）⇒ `payload_shape` 必须赢。
#[test]
fn 双错载荷_形状先于轮存在() {
    let (_dir, root, _uids) = setup();
    let ghost = "b".repeat(64);
    let mut values = base_values(&ghost);
    clear(&mut values, "text");
    assert_code(&root, &candidate(&[ghost.as_str()], &values), "payload_shape");
}

#[test]
fn 同一输入两次判定一致() {
    let (_dir, root, uids) = setup();
    insert_anchor(&root, "anchor-zhang", "active");
    let mut values = base_values(&uids[0]);
    set(&mut values, "text", json!("发布推迟到 12 月 3 日"));
    let cand = candidate(&[uids[0].as_str()], &values);
    assert_eq!(run(&root, &cand), run(&root, &cand));
}

/// 码表是封闭表：15 条、顺序即判定顺序、无重复。逐码用例断言的是这张表里的词。
#[test]
fn 码表是_15_条且不重复() {
    assert_eq!(ROWS_GATE_CODES.len(), 15);
    let unique: std::collections::BTreeSet<&str> = ROWS_GATE_CODES.iter().copied().collect();
    assert_eq!(unique.len(), ROWS_GATE_CODES.len(), "码不得重复");
    assert_eq!(ROWS_GATE_CODES[0], "payload_shape");
    assert_eq!(ROWS_GATE_CODES[14], "owner_unresolved");
    // S3c-fix 新码：`status` 排在 `mode` 之后、`number_not_grounded` 之前（第 9 步的闭集）。
    let mode_at = ROWS_GATE_CODES.iter().position(|c| *c == "mode").unwrap();
    assert_eq!(ROWS_GATE_CODES[mode_at + 1], "status");
    // 隐私修复的新码：排在"存在/退场"之后、闭集之前（引用锚判定的第三格）。
    let retired_at = ROWS_GATE_CODES.iter().position(|c| *c == "ref_retired").unwrap();
    assert_eq!(ROWS_GATE_CODES[retired_at + 1], "anchor_not_visible");
    assert_eq!(
        ROWS_GATE_CODES[retired_at + 2], "category",
        "可见性紧跟退场、先于闭集"
    );
}
