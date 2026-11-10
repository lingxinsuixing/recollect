//! 阶段 0 的零正文出口哨兵（守卫 12）：拒收原因里不许出现正文。
//! 每条被拒 turn 的 body 都埋同一串合成金丝雀；`normalize` 产出的任何
//! `Reject`——逐条拒与整批拒都算——其渲染结果（code + detail + loc 全含）
//! 里这串金丝雀必须一个字都不出现。

use rc_contract::format::{ExchangeFile, RawSession, RawTurn};
use rc_contract::{normalize, Locator, Reject, TurnTier};

const CANARY: &str = "leak-me-not-8f4e2a7c9b";

fn turn(role: &str, tier: Option<TurnTier>, uid: Option<&str>) -> RawTurn {
    RawTurn {
        turn_uid: uid.map(str::to_string),
        seq: None,
        tier,
        role: role.into(),
        // 金丝雀只许死在正文里；拒收原因若把它抄出来任何一角，就是泄露。
        body: format!("{CANARY} 先跑测试"),
        created_at_ms: Some(1730000000000),
        speaker: None,
    }
}

fn session(turns: Vec<RawTurn>) -> ExchangeFile {
    ExchangeFile {
        format_version: 1,
        sessions: vec![RawSession {
            session_key: "s-expose".into(),
            device_id: "dev-synthetic-1".into(),
            source_agent: "a".into(),
            machine: None,
            project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None,
            principal_name: None,
            principal_source: None,
            turns,
        }],
    }
}

/// Reject 的全部可见出口：detail 原串、Debug 渲染、serde 渲染（含 code 的 snake_case 名）。
fn assert_no_body(rejects: &[Reject]) {
    assert!(!rejects.is_empty(), "样本没产生任何拒收，哨兵空转");
    for r in rejects {
        let rendered = format!(
            "{}|{:?}|{}",
            r.detail,
            r,
            serde_json::to_string(r).expect("reject renders to json")
        );
        assert!(
            !rendered.contains(CANARY),
            "正文金丝雀泄露进了拒收原因: {rendered}"
        );
    }
}

#[test]
fn per_item_and_batch_rejects_never_echo_the_body() {
    // 逐条拒（RoleUnknown）与整批拒（TierClaimUnfalsifiable）同批出现
    let rep = normalize(
        session(vec![
            turn("robot", None, None),
            turn("human", Some(TurnTier::IdBearing), None),
        ]),
        1730000009999,
    );
    assert!(rep.accepted.is_empty(), "整批拒收 ⇒ 一条都不许落库");
    assert_no_body(&rep.rejects);
}

#[test]
fn duplicate_uid_batch_reject_never_echoes_the_body() {
    let rep = normalize(
        session(vec![
            turn("human", Some(TurnTier::IdBearing), Some("dup-1")),
            turn("agent", Some(TurnTier::IdBearing), Some("dup-1")),
        ]),
        1730000009999,
    );
    assert!(rep.accepted.is_empty());
    assert_no_body(&rep.rejects);
}

/// role 的值是生产者可控文本：它可以是正文的一段，也可以长成一条路径。
/// 拒收原因只需要说"哪个字段不合分型"，不需要抄回它的值。
#[test]
fn role_unknown_never_echoes_the_claimed_role() {
    let rep = normalize(
        session(vec![turn("CANARY-ROLE leak-me-not-8f4e2a7c9b C:/Users/alice/.ssh/id_ed25519", None, None)]),
        1730000009999,
    );
    assert_no_body(&rep.rejects);
    let rendered = serde_json::to_string(&rep.rejects).unwrap();
    assert!(rendered.contains("role_unknown"), "分型还得报得出来：{rendered}");
    assert!(!rendered.contains("CANARY-ROLE"), "role 的值被抄进了拒收原因：{rendered}");
    assert!(!rendered.contains("/Users"), "role 的值可以是路径：{rendered}");
}

/// 第三个同类出口：`DuplicateTurnUid` 原来把生产者给的 `turn_uid` 原样抄进 detail。
/// uid 本该是不透明标识，但它同样是可控文本——同一个过滤规则，不留第二条缝。
#[test]
fn path_shaped_duplicate_turn_uid_is_redacted_but_still_counted() {
    let hostile_uid = "C:/Users/victim/.ssh/id_rsa";
    let rep = normalize(
        session(vec![
            turn("human", Some(TurnTier::IdBearing), Some(hostile_uid)),
            turn("human", Some(TurnTier::IdBearing), Some(hostile_uid)),
        ]),
        1730000009999,
    );
    assert_eq!(rep.batch_reject, Some(rc_contract::RejectCode::DuplicateTurnUid));
    let rendered = serde_json::to_string(&rep.rejects).unwrap();
    assert!(rendered.contains("duplicate_turn_uid"), "分型要报得出来：{rendered}");
    assert!(!rendered.contains("id_rsa") && !rendered.contains("/Users"), "uid 泄进了拒收原因：{rendered}");
    assert_eq!(rep.rejected_turn_count(), 1, "重复对只算一条被拒的 turn");
}

/// 形状正常的 uid（uuid / hex）必须照旧可回显，否则排查时等于自废。
#[test]
fn identifier_shaped_uid_still_echoes() {
    let rep = normalize(
        session(vec![
            turn("human", Some(TurnTier::IdBearing), Some("0f6c3a9e-2b41-4c8d-9a77-1e5d2c8b4a30")),
            turn("human", Some(TurnTier::IdBearing), Some("0f6c3a9e-2b41-4c8d-9a77-1e5d2c8b4a30")),
        ]),
        1730000009999,
    );
    let rendered = serde_json::to_string(&rep.rejects).unwrap();
    assert!(rendered.contains("0f6c3a9e-2b41-4c8d-9a77-1e5d2c8b4a30"), "合法 uid 被误隐：{rendered}");
}
/// 未知字段的**键名**同样是可控文本（实测 `"C:/Users/x/.ssh/id_rsa"` 会原样回显）。
/// 越界键必须仍然被认出并定位，只是不许把键名本身抄进输出。
#[test]
fn path_shaped_unknown_field_key_is_redacted_but_still_located() {
    let hostile = br#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"d","source_agent":"a","C:/Users/x/.ssh/id_rsa":"v","turns":[{"role":"human","body":"x","created_at_ms":1}]}]}"#;
    let errs = rc_contract::parse(hostile).expect_err("越界键必须整批拒收");
    assert!(!errs.is_empty(), "样本没产生拒收，哨兵空转");
    let rendered = errs
        .iter()
        .map(|r| format!("{}|{}|{}", r.code.slug(), r.detail, serde_json::to_string(r).unwrap()))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("unknown field"), "越界键仍要被认出来：{rendered}");
    assert_eq!(errs[0].loc, Locator { session_index: 0, turn_index: None }, "层级定位不许一起丢：{:?}", errs[0].loc);
    assert!(!rendered.contains("id_rsa"), "键名泄进了输出：{rendered}");
    assert!(!rendered.contains("/Users"), "键名是一条路径：{rendered}");
}
