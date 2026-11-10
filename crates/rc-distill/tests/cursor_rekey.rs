//! A24/A31：迟到行 = created_at 早、ingested_at 晚。
//! 旧键（created_at）会把它漏在游标之后；新键（ingested_at）必须选中它。
use rc_distill::extract::select_batch;
use rc_ledger::water::{self, Cursor};
use rc_store::Root;
use serde_json::json;

/// 导入一条 turn，created_at 与 ingested_at 分开给。
fn seed_one(root: &mut Root, body: &str, created_at_ms: i64, ingested_at_ms: i64) -> String {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "ses-late",
            "device_id": "dev-origin",
            "source_agent": "agent-late",
            "turns": [{"role": "human", "body": body, "created_at_ms": created_at_ms}]
        }]
    });
    let file = rc_contract::parse(doc.to_string().as_bytes()).unwrap();
    let rep = rc_contract::normalize(file, ingested_at_ms);
    let a = &rep.accepted[0];
    rc_ledger::insert_batch(root.transcript_mut(), &a.session,
                            std::slice::from_ref(&a.turn), ingested_at_ms).unwrap();
    a.turn.turn_uid.clone()
}

#[test]
fn 迟到行旧键漏_新键选中() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let first = seed_one(&mut root, "第一条", 1_000, 2_000);
    let _second = seed_one(&mut root, "第二条", 1_001, 2_000);
    let scope = water::distill_scope("dev-origin", "ses-late");
    // 模拟前两条已蒸过：游标停在 ingested_at=2000 的第二条之前/之后都行，
    // 这里用第一条的 ingested_at 作下界，第二条仍在游标之后（同 ms 按 uid 定序）。
    water::advance(root.ledger(), water::DISTILL, &scope,
        &Cursor { at_ms: 2_000, turn_uid: first.clone() }, 3_000).unwrap();
    // 迟到的行：created_at 早于游标，ingested_at 晚于游标
    let late = seed_one(&mut root, "迟到的行", 500, 4_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let uids: Vec<&str> = plan.batch.rows.iter().map(|r| r.turn_uid.as_str()).collect();
    assert!(uids.contains(&late.as_str()),
        "ingested_at 比游标新的迟到行必须被选中；实际批 = {uids:?}");
    // 第二条与迟到行都在新键之后；旧键会漏掉迟到行
    assert!(!plan.batch.rows.iter().any(|r| r.turn_uid == first),
        "游标之前的第一条不许再进来");
}

#[test]
fn 同_ingested_at_用_turn_uid_定序() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let a = seed_one(&mut root, "甲", 1_000, 5_000);
    let b = seed_one(&mut root, "乙", 1_001, 5_000);
    let plan = select_batch(root.transcript(), root.ledger(), 40).unwrap().unwrap();
    let mut got: Vec<String> = plan.batch.rows.iter().map(|r| r.turn_uid.clone()).collect();
    let mut want = vec![a, b];
    got.sort(); want.sort();
    assert_eq!(got, want, "同一 ingested_at 的两条都必须进批，且按 uid 可确定地翻页");
}

#[test]
fn eta_用到达钟_不是设备钟() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let _a = seed_one(&mut root, "甲", 100, 10_000);
    let _b = seed_one(&mut root, "乙", 200, 10_001);
    let c = seed_one(&mut root, "丙", 300, 10_002);
    let t = root.transcript();
    assert_eq!(
        rc_ledger::transcript::count_between(t, "ses-late", 10_000, 10_002).unwrap(),
        1,
        "按到达钟只数中间一条；按设备钟 (100..300) 会是 0"
    );
    let origin = rc_ledger::transcript::turn_origin(t, &c).unwrap().unwrap();
    assert_eq!(origin.device_id, "dev-origin");
    assert_eq!(origin.ingested_at_ms, 10_002);
}
