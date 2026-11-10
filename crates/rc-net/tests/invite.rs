//! S4b1：邀请票的编解码、canonical 与验签面。
//!
//! 每条用例都从"一张中心真的会签出来的票"出发（`HubKey` 现生成），
//! 只改一处再看结果——改的是哪一个字节，断言就盯哪一格的错。

use rc_net::invite::{
    decode_ticket, encode_ticket, hub_fingerprint, identity_canonical, invite_canonical,
    verify_ticket, HubIdentity, InviteError, InviteTicket, IDENTITY_TTL_MS, TICKET_VERSION,
};
use rc_net::key::HubKey;

const HUB_FP: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const HUB_FP_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const TLS_FP: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn hub() -> (tempfile::TempDir, HubKey) {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    (dir, key)
}

fn ticket(hub_fp: &str, expires_at_ms: i64) -> InviteTicket {
    InviteTicket {
        v: TICKET_VERSION,
        id: "0123456789abcdef0123456789abcdef".into(),
        hub_fp: hub_fp.to_string(),
        tls_fp: TLS_FP.to_string(),
        endpoints: vec!["https://127.0.0.1:7420".into()],
        expires_at_ms,
    }
}

/// 中心侧出票的那两步（签 canonical → 拼信封），测试里也只走这两步，
/// 不另写一份"测试版出票"。
fn issue(key: &HubKey, t: &InviteTicket) -> String {
    let sig = key.sign_b64(invite_canonical(t).as_bytes());
    encode_ticket(t, &sig)
}

#[test]
fn ticket_roundtrips_through_text_form() {
    let (_dir, key) = hub();
    let original = ticket(&hub_fingerprint(&key.public_b64()), 1_700_000_000_000);
    let text = issue(&key, &original);
    assert!(text.starts_with("rc1."), "文本票前缀错了：{text}");
    assert_eq!(text.split('.').count(), 3, "必须是三段：{text}");

    let (decoded, sig) = decode_ticket(&text).unwrap();
    assert_eq!(decoded, original, "信封往返改了内容");
    assert_eq!(encode_ticket(&decoded, &sig), text, "同一份内容编不出同一串文本");

    assert_eq!(verify_ticket(&text, &key.public_b64()).unwrap(), original);
}

#[test]
fn canonical_is_byte_stable_and_field_order_is_part_of_the_signature() {
    let t = ticket(HUB_FP, 42);
    assert_eq!(
        invite_canonical(&t),
        format!(
            "recollect-invite-v1\n1\n0123456789abcdef0123456789abcdef\n{HUB_FP}\n{TLS_FP}\n\
             https://127.0.0.1:7420\n42"
        )
    );
    // 两次调用的字节必须一样：canonical 里出现任何"随调用变化"的东西
    // （时间、map 序）都会让同一张票在两边算出不同的签名体。
    assert_eq!(invite_canonical(&t), invite_canonical(&t.clone()));

    let mut multi = ticket(HUB_FP, 42);
    multi.endpoints = vec!["https://10.0.0.2:7420".into(), "https://10.0.0.3:7420".into()];
    assert!(invite_canonical(&multi).ends_with("https://10.0.0.2:7420,https://10.0.0.3:7420\n42"));
}

#[test]
fn identity_canonical_covers_every_field_in_the_documented_order() {
    let id = HubIdentity {
        v: 1,
        hub_pub: "pk".into(),
        hub_fp: HUB_FP.into(),
        tls_fp: TLS_FP.into(),
        epoch: 7,
        endpoints: vec!["https://10.0.0.2:7420".into()],
        issued_at_ms: 100,
        expires_at_ms: 100 + IDENTITY_TTL_MS,
        sig: "sig".into(),
    };
    assert_eq!(
        identity_canonical(&id),
        format!(
            "recollect-identity-v1\n1\npk\n{HUB_FP}\n{TLS_FP}\n7\nhttps://10.0.0.2:7420\n100\n{}",
            100 + IDENTITY_TTL_MS
        )
    );
}

#[test]
fn tampering_any_signed_byte_breaks_the_signature() {
    let (_dir, key) = hub();
    let own_fp = hub_fingerprint(&key.public_b64());
    let good = ticket(&own_fp, 1_700_000_000_000);
    let sig = key.sign_b64(invite_canonical(&good).as_bytes());

    // 三项各改一格：终端指纹、hub 指纹、过期时刻。
    for tampered in [
        InviteTicket { tls_fp: "4444444444444444444444444444444444444444444444444444444444444444".into(), ..good.clone() },
        InviteTicket { hub_fp: HUB_FP_B.into(), ..good.clone() },
        InviteTicket { expires_at_ms: good.expires_at_ms + 1, ..good.clone() },
        InviteTicket { endpoints: vec!["https://127.0.0.1:7421".into()], ..good.clone() },
    ] {
        let text = encode_ticket(&tampered, &sig);
        assert_eq!(
            verify_ticket(&text, &key.public_b64()),
            Err(InviteError::Signature),
            "改过内容的票验签仍然通过：{text}"
        );
    }
}

#[test]
fn another_hubs_key_cannot_verify_this_ticket() {
    let (_dir, key) = hub();
    let (_dir_b, other) = hub();
    let text = issue(&key, &ticket(&hub_fingerprint(&key.public_b64()), 1_700_000_000_000));
    assert_eq!(
        verify_ticket(&text, &other.public_b64()),
        Err(InviteError::Signature),
        "别人的钥匙不该验过这张票"
    );
}

/// `hub_fp` 与签发钥匙必须对得上：一把真钥匙签出一张"声称是另一台中心"的票，
/// 只有在实现有 bug 或被改过时才可能发生——但那是票的核心断言，必须有一格测试盯着。
#[test]
fn a_ticket_that_names_another_hub_is_reported_as_hub_mismatch() {
    let (_dir, key) = hub();
    let text = issue(&key, &ticket(HUB_FP_B, 1_700_000_000_000));
    assert_eq!(
        verify_ticket(&text, &key.public_b64()),
        Err(InviteError::HubMismatch)
    );
}

#[test]
fn unsupported_version_is_rejected_after_the_signature_check() {
    let (_dir, key) = hub();
    let mut t = ticket(&hub_fingerprint(&key.public_b64()), 1_700_000_000_000);
    t.v = 2;
    // 版本是签名覆盖的一格 ⇒ 真钥匙签它，验签过得去，落到 Version。
    assert_eq!(verify_ticket(&issue(&key, &t), &key.public_b64()), Err(InviteError::Version));

    // 没有签名的 v=2 只是坏签名，不是"新版本"：版本判定不许抢在验签前面。
    let forged = encode_ticket(&t, "AAAA");
    assert_eq!(verify_ticket(&forged, &key.public_b64()), Err(InviteError::Signature));
}

#[test]
fn malformed_text_is_format_error_and_never_panics() {
    let (_dir, key) = hub();
    let good = issue(&key, &ticket(&hub_fingerprint(&key.public_b64()), 1_700_000_000_000));
    let payload = good.split('.').nth(1).unwrap();

    for bad in [
        String::new(),
        ".".into(),
        "rc1".into(),
        "rc1.".into(),
        format!("rc1.{payload}"),
        format!("rc1.{payload}.sig.extra"),
        format!("rc2.{payload}.sig"),
        "rc1.!!!.sig".to_string(),
        "rc1.bm90IGpzb24.sig".to_string(),
    ] {
        assert_eq!(
            decode_ticket(&bad),
            Err(InviteError::Format),
            "这段输入该判格式错：{bad:?}"
        );
    }
    // 库大小写不敏感地拒：非 `rc1` 前缀一律 Format。
    assert_eq!(decode_ticket(&good.to_uppercase()), Err(InviteError::Format));
}

#[test]
fn json_with_a_missing_field_is_a_format_error() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let json = format!("{{\"v\":1,\"id\":\"x\",\"hub_fp\":\"{HUB_FP}\",\"tls_fp\":\"{TLS_FP}\"}}");
    let text = format!("rc1.{}.sig", URL_SAFE_NO_PAD.encode(json.as_bytes()));
    assert_eq!(decode_ticket(&text), Err(InviteError::Format));
}

#[test]
fn hub_fingerprint_is_64_lowercase_hex_and_not_the_public_key() {
    let (_dir, key) = hub();
    let pub_b64 = key.public_b64();
    let fp = hub_fingerprint(&pub_b64);
    assert_eq!(fp.len(), 64);
    assert!(fp.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_ne!(fp, pub_b64);
    assert_eq!(fp, hub_fingerprint(&pub_b64), "同一个公钥必须算出同一个指纹");
    let (_dir_b, other) = hub();
    assert_ne!(fp, hub_fingerprint(&other.public_b64()), "两台中心的指纹撞了");
}

/// 票里**不含**私钥材料，也不含 token：它只是"允许配一次"的凭证。
/// 这一格盯的是形状，不是内容——多写一个字段就多一条泄露面。
#[test]
fn ticket_json_has_exactly_the_documented_fields() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let (_dir, key) = hub();
    let text = issue(&key, &ticket(&hub_fingerprint(&key.public_b64()), 1));
    let json = URL_SAFE_NO_PAD.decode(text.split('.').nth(1).unwrap()).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let mut keys: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["endpoints", "expires_at_ms", "hub_fp", "id", "tls_fp", "v"]
    );
}
