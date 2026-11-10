use rc_contract::{compute_body_fp, compute_turn_uid, normalize_body};
use std::path::Path;

fn golden() -> serde_json::Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../golden/turn_uid.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn golden_cases_match_normalized_bodies() {
    for c in golden()["cases"].as_array().unwrap() {
        let got = normalize_body(c["body_raw"].as_str().unwrap());
        assert_eq!(got, c["expect_body"].as_str().unwrap(), "{} body", c["name"]);
    }
}

#[test]
fn golden_cases_match_hashes() {
    let mut pending = vec![];
    for c in golden()["cases"].as_array().unwrap() {
        let body = normalize_body(c["body_raw"].as_str().unwrap());
        let ts = c["created_at_ms"].as_i64().unwrap();
        let uid = compute_turn_uid(
            c["device_id"].as_str().unwrap(),
            c["session_key"].as_str().unwrap(),
            c["role"].as_str().unwrap(),
            &body, ts,
        );
        let fp = compute_body_fp(&body);
        for (field, got) in [("expect_turn_uid", &uid), ("expect_body_fp", &fp)] {
            let want = c[field].as_str().unwrap();
            if want == "FILL" {
                pending.push(format!("{} {field} = {got}", c["name"]));
            } else {
                assert_eq!(want.len(), 64);
                assert_eq!(want, got.as_str(), "{} {field}", c["name"]);
            }
        }
    }
    if !pending.is_empty() {
        panic!("把下面这些 hex 填进 golden/turn_uid.json：\n{}", pending.join("\n"));
    }
}
