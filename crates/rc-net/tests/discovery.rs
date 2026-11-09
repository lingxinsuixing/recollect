//! S4c1：发现应答的往返、验签与"坏包不应答"。
//!
//! 每条用例都从一个真的 responder 线程出发（`127.0.0.1:0` 现绑后取 `local_addr`），
//! 客户端**只**用 `probe_payload` 发问、`verify_announce` 收账——不另写一份测试版协议。

use rc_net::discovery::{
    announce_canonical, discover, encode_announce, new_nonce, probe_payload, serve_udp,
    verify_announce, DiscoveryError, PROBE_MAX,
};
use rc_net::error::NetError;
use rc_net::invite::{hub_fingerprint, HubIdentity, IDENTITY_TTL_MS};
use rc_net::key::HubKey;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 签名用例的固定"现在"：这些用例自己造字节、自己给 now，与系统钟无关。
const NOW: i64 = 1_700_000_000_000;
const NONCE: &str = "0123456789abcdef";
const TLS_FP: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const OTHER_TLS_FP: &str = "4444444444444444444444444444444444444444444444444444444444444444";

/// 与真中心同一个取钟法：`discover` 判窗口用的是**调用方给的**钟（库里不取钟），所以本档
/// 把它取在这里、既喂应答者也喂扫描——两边必须活在同一个钟面上，否则一条刚签的公告会被
/// 自己判成过期（固定常量 1_700_000_000_000 早就过了）。
fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the unix epoch")
        .as_millis() as i64
}

fn hub() -> (tempfile::TempDir, HubKey) {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    (dir, key)
}

/// 中心会交给 responder 的那份模板：时间两格是占位（每次应答现算），`sig` 为空。
/// 这里的窗口是**合法**的：验签用例要证明的是别的格子，一份已经过期的公告会在窗口
/// 那一格就先死掉，测不到它们。
fn template(key: &HubKey) -> HubIdentity {
    let hub_pub = key.public_b64();
    HubIdentity {
        v: 1,
        hub_fp: hub_fingerprint(&hub_pub),
        hub_pub,
        tls_fp: TLS_FP.into(),
        epoch: 7,
        endpoints: vec!["https://127.0.0.1:7420".into()],
        issued_at_ms: NOW,
        expires_at_ms: NOW + IDENTITY_TTL_MS,
        sig: String::new(),
    }
}

/// `serve_udp` 是无限循环，所以起一条 detached 线程；线程活到测试进程结束。
fn responder(key: HubKey, template: HubIdentity) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let base = system_now_ms();
    std::thread::spawn(move || serve_udp(socket, key, template, base));
    addr
}

fn client(timeout: Duration) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(timeout)).unwrap();
    socket
}

fn ask(addr: SocketAddr, client: &UdpSocket, nonce: &str) -> Vec<u8> {
    client.send_to(&probe_payload(nonce), addr).unwrap();
    let mut buf = [0u8; 8192];
    let (n, _) = client.recv_from(&mut buf).expect("responder must answer a good probe");
    buf[..n].to_vec()
}

/// "没有应答"：两个平台把这叫两种错误名（Linux `WouldBlock` / Windows `TimedOut`），
/// 统一按"这条网线上什么都没回"判。
fn assert_silent(socket: &UdpSocket) {
    let mut buf = [0u8; 8192];
    match socket.recv_from(&mut buf) {
        Ok((n, _)) => panic!("responder answered a datagram it must ignore ({n} bytes)"),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        Err(e) => panic!("unexpected recv error: {e:?}"),
    }
}

#[test]
fn a_probe_roundtrips_to_a_signed_announce() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let base = system_now_ms();
    let addr = responder(key, tpl.clone());

    let bytes = ask(addr, &client(Duration::from_secs(2)), NONCE);
    assert!(bytes.len() <= 4096, "公告超过 4 KiB：{}", bytes.len());
    let (nonce, identity) =
        verify_announce(&bytes, system_now_ms()).expect("一条刚签的公告必须验得过");

    assert_eq!(nonce, NONCE, "nonce 回显不一致");
    assert_eq!(identity.v, tpl.v);
    assert_eq!(identity.hub_pub, tpl.hub_pub);
    assert_eq!(identity.hub_fp, tpl.hub_fp);
    assert_eq!(identity.tls_fp, tpl.tls_fp);
    assert_eq!(identity.epoch, tpl.epoch);
    assert_eq!(identity.endpoints, tpl.endpoints);
    // 有效期由 responder 现算：起点是线程的起跑钟（加已跑过的毫秒），窗口就是公告 TTL。
    assert!(
        identity.issued_at_ms >= base && identity.issued_at_ms < base + 60_000,
        "issued_at_ms 不是现算的：{}",
        identity.issued_at_ms
    );
    assert_eq!(identity.expires_at_ms - identity.issued_at_ms, IDENTITY_TTL_MS);

    // 信封形状：五个键、没有把 probe 原文或来源地址回显进来。
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut keys: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["identity", "nonce", "sig", "type", "v"]);
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("\"probe\""), "公告回显了请求原文：{text}");
}

#[test]
fn an_announce_signed_by_another_key_is_rejected() {
    let (_dir, key) = hub();
    let (_dir_b, other) = hub();
    let tpl = template(&key);

    // 字段全对、canonical 也对，只是签名来自另一把私钥（另一台中心）。
    let sig = other.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
    let bytes = encode_announce(&tpl, NONCE, &sig);
    assert_eq!(verify_announce(&bytes, NOW), Err(DiscoveryError::Signature));
}

#[test]
fn tampering_any_signed_byte_breaks_the_signature() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let sig = key.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
    let good = encode_announce(&tpl, NONCE, &sig);
    assert!(verify_announce(&good, NOW).is_ok(), "基线公告本身没验过");

    let doc: serde_json::Value = serde_json::from_slice(&good).unwrap();
    let mut mutations = Vec::new();
    // nonce 回显在 canonical 的第一行；身份里的每一格在它下面。
    let mut nonce = doc.clone();
    nonce["nonce"] = serde_json::json!("fedcba9876543210");
    mutations.push(nonce);
    let mut tls = doc.clone();
    tls["identity"]["tls_fp"] = serde_json::json!(OTHER_TLS_FP);
    mutations.push(tls);
    let mut epoch = doc.clone();
    epoch["identity"]["epoch"] = serde_json::json!(8);
    mutations.push(epoch);
    let mut endpoints = doc.clone();
    endpoints["identity"]["endpoints"] = serde_json::json!(["https://10.9.9.9:7420"]);
    mutations.push(endpoints);

    for tampered in mutations {
        let bytes = serde_json::to_vec(&tampered).unwrap();
        assert_eq!(
            verify_announce(&bytes, NOW),
            Err(DiscoveryError::Signature),
            "改过一格的公告仍然验过：{tampered}"
        );
    }

    // 签名串本身改一个字符也是 Signature——这是"签名不是装饰"的最小证据。
    // `rfind` 找的是外层那个 `sig`（内层 identity 里还有一个同名空字段）。
    let text = String::from_utf8(good.clone()).unwrap();
    let at = text.rfind("\"sig\":\"").unwrap() + "\"sig\":\"".len();
    let mut raw = good;
    raw[at] = if raw[at] == b'A' { b'B' } else { b'A' };
    assert_eq!(verify_announce(&raw, NOW), Err(DiscoveryError::Signature));
}

/// 公告只有一个签名（外层）。内层 `identity.sig` 被填上任何东西都只能是一份
/// "不在这条 canonical 里"的签名，读的人会误以为它验过，所以直接判 `Signature`。
#[test]
fn an_inner_identity_signature_is_refused() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let sig = key.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
    let good = encode_announce(&tpl, NONCE, &sig);
    let mut doc: serde_json::Value = serde_json::from_slice(&good).unwrap();
    doc["identity"]["sig"] = serde_json::json!("AAAA");
    let bytes = serde_json::to_vec(&doc).unwrap();
    assert_eq!(verify_announce(&bytes, NOW), Err(DiscoveryError::Signature));

    // 反过来：encode 会把调用方带进来的内层签名清掉，线上只有外层那一个。
    let mut with_inner = tpl.clone();
    with_inner.sig = "not-a-signature".into();
    let wire = encode_announce(&with_inner, NONCE, &sig);
    assert!(!String::from_utf8(wire).unwrap().contains("not-a-signature"));
}

#[test]
fn a_correctly_signed_announce_with_a_bad_nonce_is_a_nonce_error() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    for bad in [
        String::new(),
        "short".to_string(),
        "0123456789abcde".to_string(),
        "0123456789abcde ".to_string(),
        "0123456789abcdef\n".to_string(),
        "a".repeat(65),
    ] {
        let sig = key.sign_b64(announce_canonical(&tpl, &bad).as_bytes());
        let bytes = encode_announce(&tpl, &bad, &sig);
        assert_eq!(
            verify_announce(&bytes, NOW),
            Err(DiscoveryError::Nonce),
            "这把私钥真的签了它，但 nonce 不是一条 safe token：{bad:?}"
        );
    }
}

#[test]
fn a_wrong_version_or_shape_is_a_format_error() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let sig = key.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
    let good = encode_announce(&tpl, NONCE, &sig);
    assert!(verify_announce(&good, NOW).is_ok());

    let mut doc: serde_json::Value = serde_json::from_slice(&good).unwrap();
    doc["v"] = serde_json::json!(2);
    let v2 = serde_json::to_vec(&doc).unwrap();
    assert_eq!(verify_announce(&v2, NOW), Err(DiscoveryError::Version));

    let mut doc: serde_json::Value = serde_json::from_slice(&good).unwrap();
    doc["type"] = serde_json::json!("probe");
    assert_eq!(verify_announce(&serde_json::to_vec(&doc).unwrap(), NOW), Err(DiscoveryError::Format));

    let mut doc: serde_json::Value = serde_json::from_slice(&good).unwrap();
    doc["extra"] = serde_json::json!(1);
    assert_eq!(verify_announce(&serde_json::to_vec(&doc).unwrap(), NOW), Err(DiscoveryError::Format));

    assert_eq!(verify_announce(b"", NOW), Err(DiscoveryError::Format));
    assert_eq!(verify_announce(b"not json", NOW), Err(DiscoveryError::Format));
    assert_eq!(verify_announce(&[b' '; 4097], NOW), Err(DiscoveryError::Format));
}

/// 坏包一律不应答，而 responder 在这些坏包之后**还活着**：接着发一条好 probe 仍然收到公告。
/// 这一条同时盯住两件事：沉默的判据（读超时）与"一次坏输入不等于线程结束"。
#[test]
fn bad_datagrams_get_no_answer_and_do_not_kill_the_responder() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let addr = responder(key, tpl.clone());
    let client = client(Duration::from_millis(250));

    let good_probe = probe_payload(NONCE);
    let mut oversize = good_probe.clone();
    oversize.extend(vec![b' '; PROBE_MAX]);
    assert!(oversize.len() > PROBE_MAX);

    let bad: Vec<Vec<u8>> = vec![
        b"not json".to_vec(),
        b"{}".to_vec(),
        // 类型不对：announce 是中心说的话，不是节点问的话。
        br#"{"v":1,"type":"announce","nonce":"0123456789abcdef"}"#.to_vec(),
        // 版本不对。
        br#"{"v":2,"type":"probe","nonce":"0123456789abcdef"}"#.to_vec(),
        // nonce 太短。
        br#"{"v":1,"type":"probe","nonce":"short"}"#.to_vec(),
        // 多一个字段。
        br#"{"v":1,"type":"probe","nonce":"0123456789abcdef","x":1}"#.to_vec(),
        // 一条合法 probe 加上超过 PROBE_MAX 的填充。
        oversize,
    ];
    for packet in &bad {
        client.send_to(packet, addr).unwrap();
        assert_silent(&client);
    }

    // 坏包之后仍然服务：同一条线程、同一个 nonce 窗口。
    let bytes = ask(addr, &client, NONCE);
    let (nonce, identity) =
        verify_announce(&bytes, system_now_ms()).expect("坏包之后的公告仍然要验得过");
    assert_eq!(nonce, NONCE);
    assert_eq!(identity.hub_fp, tpl.hub_fp);
}

/// 端口上同时来两条 probe（不同 nonce）时各答各的：公告是逐条签的，不是缓存的。
#[test]
fn each_probe_is_answered_with_its_own_nonce() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let addr = responder(key, tpl);
    let client = client(Duration::from_secs(2));

    for nonce in ["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbb"] {
        let (echoed, _) = verify_announce(&ask(addr, &client, nonce), system_now_ms()).unwrap();
        assert_eq!(echoed, nonce);
    }
}

// ---- S4c2：客户端扫描 ---------------------------------------------------------------------

/// 一台"照 probe 回话、但身份是伪造的"应答者：nonce 回显正确，签名只能用 `signer` 那把
/// 私钥。两种坏都由它造：签名来自另一把钥匙（对端不是它自称的中心），
/// 以及指纹不是由自己的公钥算出来的（对端给自己起了别人的名字）。
fn counterfeiter(signer: HubKey, identity: HubIdentity) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf) else { continue };
            let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else { continue };
            let Some(nonce) = doc["nonce"].as_str() else { continue };
            let sig = signer.sign_b64(announce_canonical(&identity, nonce).as_bytes());
            let _ = socket.send_to(&encode_announce(&identity, nonce, &sig), peer);
        }
    });
    addr
}

/// 一个"没有任何人听"的地址：先绑再放，端口在本进程里随即空出来。
/// Windows 上往它发 probe 会收到一次 ICMP 回灌（ECONNRESET），这一条同时钉住
/// "没人听不是扫描失败"。
fn silent_port() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    addr
}

#[test]
fn a_nonce_is_a_fresh_16_byte_base64url_token() {
    let a = new_nonce().unwrap();
    let b = new_nonce().unwrap();
    assert_ne!(a, b, "两次扫描的 nonce 不许相同——相同就等于让人回放旧公告");
    for nonce in [&a, &b] {
        assert_eq!(nonce.len(), 22, "16 字节 base64url 应落在 22 字符：{nonce}");
        assert!(
            nonce.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "nonce 不是一条 base64url token：{nonce}"
        );
    }
}

#[test]
fn discover_returns_the_candidate_the_responder_signed() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let addr = responder(key, tpl.clone());

    let got = discover(&[addr], Duration::from_millis(800), system_now_ms()).expect("扫描本身不该失败");
    assert_eq!(got.len(), 1, "一个应答者只该产出一个候选：{got:?}");
    let c = &got[0];
    assert_eq!(c.hub_fp, tpl.hub_fp);
    assert_eq!(c.hub_pub, tpl.hub_pub);
    assert_eq!(c.tls_fp, TLS_FP);
    assert_eq!(c.epoch, 7);
    assert_eq!(c.endpoints, tpl.endpoints);
    // `seen_from` 是这条公告的来源地址：本机直连时就是应答 socket 自己。
    assert_eq!(c.seen_from, addr);
}

#[test]
fn two_hubs_come_back_sorted_by_fingerprint() {
    let (_dir_a, key_a) = hub();
    let (_dir_b, key_b) = hub();
    let tpl_a = template(&key_a);
    let tpl_b = template(&key_b);
    let addr_a = responder(key_a, tpl_a.clone());
    let addr_b = responder(key_b, tpl_b.clone());

    let got = discover(&[addr_a, addr_b], Duration::from_millis(800), system_now_ms()).unwrap();
    assert_eq!(got.len(), 2, "两台不同钥匙的中心应产出两个候选：{got:?}");
    assert!(got[0].hub_fp < got[1].hub_fp, "候选必须按 hub_fp 升序：{got:?}");
    let mut want = vec![tpl_a.hub_fp, tpl_b.hub_fp];
    want.sort();
    assert_eq!(got.iter().map(|c| c.hub_fp.clone()).collect::<Vec<_>>(), want);
}

/// 坏公告不列出来，而且**一次坏包不影响同一轮扫描里好中心的候选**：
/// 扫描是"广播之后听一段"，不是"一问一答"。
#[test]
fn a_bad_signature_or_a_borrowed_fingerprint_is_not_listed() {
    let (_dir, victim) = hub();
    let victim_tpl = template(&victim);

    // ① 签名假：身份、nonce 全对，签名来自另一把私钥。
    let (_dir_b, signer) = hub();
    let liar = counterfeiter(signer, victim_tpl.clone());
    // ② 指纹借用：用自己的公钥与私钥，只把 hub_fp 填成受害中心的那一枚。
    let (_dir_c, forger_key) = hub();
    let mut borrowed = template(&forger_key);
    borrowed.hub_fp = victim_tpl.hub_fp.clone();
    let forger = counterfeiter(forger_key, borrowed);
    // ③ 一台真的好中心，用来证明坏包只是被跳过。
    let (_dir_d, good_key) = hub();
    let good_tpl = template(&good_key);
    let good = responder(good_key, good_tpl.clone());

    let got = discover(&[liar, forger, good], Duration::from_millis(800), system_now_ms()).unwrap();
    assert_eq!(got.len(), 1, "只有签名与指纹都对得上的那台该被列出：{got:?}");
    assert_eq!(got[0].hub_fp, good_tpl.hub_fp);
}

/// 没有人应答 ⇒ 空表、不报错。发现命令的常态是"这片网上暂时没有中心"，
/// 把它做成错误等于让第一次开机必须一次成功。
#[test]
fn no_answer_is_an_empty_list_not_an_error() {
    let got = discover(&[silent_port()], Duration::from_millis(300), system_now_ms())
        .expect("没有应答不是扫描失败");
    assert!(got.is_empty(), "空端口不该产出候选：{got:?}");
}

// ---- S4fix-A R4：公告的版本与窗口 ---------------------------------------------------------

/// 被签名覆盖的 `identity.v` 才说明"这条公告按哪套字段语义签发"：信封的 `v` 谁都能改，
/// 改了签名就碎；身份那一格改了只能由真私钥重签。
#[test]
fn an_announce_from_a_newer_identity_version_is_refused() {
    let (_dir, key) = hub();
    let tpl = HubIdentity { v: 2, ..template(&key) };
    let sig = key.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
    let bytes = encode_announce(&tpl, NONCE, &sig);
    assert_eq!(verify_announce(&bytes, NOW), Err(DiscoveryError::Version));
}

/// 窗口：过期与"未来票"都拒，容差只用来吸收时钟偏差。
#[test]
fn an_announce_outside_its_validity_window_is_refused() {
    let (_dir, key) = hub();
    let cases: [(i64, i64, Result<(), DiscoveryError>); 3] = [
        // 过期 61 秒：超出容差 ⇒ Expired。
        (NOW - 600_000, NOW - 61_000, Err(DiscoveryError::Expired)),
        // 签发时刻在未来 2 分钟：抓不到的未来票同样是"判不了窗口"。
        (NOW + 120_000, NOW + 400_000, Err(DiscoveryError::Expired)),
        // 差 1 毫秒到期仍在容差里：窗口判的是"过期"，不是"到期还早"。
        (NOW, NOW + 1, Ok(())),
    ];
    for (issued_at_ms, expires_at_ms, want) in cases {
        let tpl = HubIdentity { issued_at_ms, expires_at_ms, ..template(&key) };
        let sig = key.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
        let bytes = encode_announce(&tpl, NONCE, &sig);
        let got = verify_announce(&bytes, NOW).map(|_| ());
        assert_eq!(got, want, "issued={issued_at_ms} expires={expires_at_ms}");
    }

    // 反向：签发时刻很旧但还没过期 ⇒ 收。签发时刻不是判据，窗口才是。
    let tpl = HubIdentity { issued_at_ms: NOW - 600_000, expires_at_ms: NOW - 59_000, ..template(&key) };
    let sig = key.sign_b64(announce_canonical(&tpl, NONCE).as_bytes());
    assert!(verify_announce(&encode_announce(&tpl, NONCE, &sig), NOW).is_ok());
}

/// 一台"照 probe 回话、但公告窗口/版本由调用方指定"的应答者：用它把四类窗口放到
/// `discover` 那条路上判——验签通过、nonce 回对，唯独窗口/版本不对。
fn windowed_responder(key: HubKey, base: HubIdentity, issued_at_ms: i64, expires_at_ms: i64) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf) else { continue };
            let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else { continue };
            let Some(nonce) = doc["nonce"].as_str() else { continue };
            let identity = HubIdentity { issued_at_ms, expires_at_ms, ..base.clone() };
            let sig = key.sign_b64(announce_canonical(&identity, nonce).as_bytes());
            let _ = socket.send_to(&encode_announce(&identity, nonce, &sig), peer);
        }
    });
    addr
}

/// 端到端：坏窗口的公告不列出来，好窗口的列出来。四条各自一台应答者（同一把钥匙也不冲突：
/// 它们各占一个端口，而一表只收一条候选）。
#[test]
fn discover_drops_announces_outside_their_window_or_version() {
    let now = system_now_ms();
    let (dir, key) = hub();
    let tpl = template(&key);
    // 同一把钥匙的第二个句柄：四条用例各起一条线程，私钥只读、路径唯一。
    let key_at = |dir: &tempfile::TempDir| HubKey::load(&dir.path().join("hub.key")).unwrap();
    let expired = windowed_responder(key_at(&dir), tpl.clone(), now, now - 61_000);
    let got = discover(&[expired], Duration::from_millis(400), now).unwrap();
    assert!(got.is_empty(), "过期公告被列出来了：{got:?}");

    let future = windowed_responder(key_at(&dir), tpl.clone(), now + 120_000, now + 400_000);
    let got = discover(&[future], Duration::from_millis(400), now).unwrap();
    assert!(got.is_empty(), "未来公告被列出来了：{got:?}");

    let v2 = windowed_responder(
        key_at(&dir),
        HubIdentity { v: 2, ..tpl.clone() },
        now,
        now + IDENTITY_TTL_MS,
    );
    let got = discover(&[v2], Duration::from_millis(400), now).unwrap();
    assert!(got.is_empty(), "identity v=2 的公告被列出来了：{got:?}");

    // 还差 1 毫秒到期的那一份仍然要列：容差之内就是"没过期"。
    let alive = windowed_responder(key, tpl.clone(), now, now + 1);
    let got = discover(&[alive], Duration::from_millis(400), now).unwrap();
    assert_eq!(got.len(), 1, "窗口内的公告没被列出：{got:?}");
    assert_eq!(got[0].hub_fp, tpl.hub_fp);
}

// ---- S4fix-A R5：应答限速 -----------------------------------------------------------------

/// 读到"这一轮没有更多应答"为止，返回条数。
fn drain(socket: &UdpSocket) -> usize {
    let mut buf = [0u8; 8192];
    let mut n = 0;
    loop {
        match socket.recv_from(&mut buf) {
            Ok(_) => n += 1,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return n
            }
            Err(e) => panic!("unexpected recv error: {e:?}"),
        }
    }
}

/// 一个源连发 20 条合法 probe ⇒ 最多收回 5 条应答；同 IP 的另一个端口算同一个源；
/// 另一个 loopback 地址是另一个源；窗口滑过之后同一个源又能拿到应答。
#[test]
fn one_source_gets_at_most_five_answers_per_window() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let addr = responder(key, tpl.clone());

    // 20 条各用不同 nonce：限速按月的是"源"，不是"这一条问话"。
    let first = client(Duration::from_millis(400));
    for i in 0..20u8 {
        let nonce = format!("{:016x}", i as u64 + 0x1000);
        first.send_to(&probe_payload(&nonce), addr).unwrap();
    }
    let answers = drain(&first);
    assert!(answers <= 5, "同一源在 1 秒窗口里拿到了 {answers} 条应答（上限 5）");
    assert!(answers >= 1, "一条应答都没有：限速把正常问话也挡了");

    // 同 IP、不同端口仍是同一个源：这一条也不该被答。
    let same_ip = client(Duration::from_millis(300));
    same_ip.send_to(&probe_payload("0123456789abcdea"), addr).unwrap();
    assert_silent(&same_ip);

    // 换一个源 IP：本机的第二个 loopback 地址（平台不允许绑定时只测上面的每源上限）。
    match UdpSocket::bind("127.0.0.2:0") {
        Ok(other) => {
            other.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            other.send_to(&probe_payload("0123456789abcdeb"), addr).unwrap();
            let mut buf = [0u8; 8192];
            let got = other.recv_from(&mut buf);
            assert!(got.is_ok(), "另一个源 IP 也被限速挡住了：{got:?}");
        }
        Err(_) => {
            // 127.0.0.2 绑不上：这一格留空，每源上限已由上面两条钉住。
        }
    }

    // 窗口滑过：1.1 秒之后同一个源重新可以拿到应答。
    std::thread::sleep(Duration::from_millis(1_100));
    let bytes = ask(addr, &first, "0123456789abcdec");
    let (echoed, identity) = verify_announce(&bytes, system_now_ms()).expect("窗口滑过后应答仍要验得过");
    assert_eq!(echoed, "0123456789abcdec");
    assert_eq!(identity.hub_fp, tpl.hub_fp);
}

// ---- S4fix-A R14：按目标降级与候选上限 -----------------------------------------------------

/// 一个 IPv6 目标在 IPv4 socket 上写不出去（地址族不对）：它只丢自己，
/// 同一次扫描里真 responder 的候选照收。
#[test]
fn an_unwritable_target_does_not_lose_the_other_targets() {
    let (_dir, key) = hub();
    let tpl = template(&key);
    let good = responder(key, tpl.clone());
    let v6: SocketAddr = "[::1]:7420".parse().unwrap();

    let got = discover(&[v6, good], Duration::from_millis(800), system_now_ms()).expect("一个坏目标不该毁掉整次扫描");
    assert_eq!(got.len(), 1, "好目标的候选丢了：{got:?}");
    assert_eq!(got[0].hub_fp, tpl.hub_fp);
}

/// 一个目标都发不出去 ⇒ 这次扫描确实没做成，必须是错误而不是空表。
#[test]
fn nothing_sent_at_all_is_an_io_error() {
    let v6: SocketAddr = "[::1]:7420".parse().unwrap();
    let got = discover(&[v6], Duration::from_millis(200), system_now_ms());
    assert!(
        matches!(got, Err(NetError::Io(_))),
        "一个目标都没发出去却不是错误：{got:?}"
    );
}
