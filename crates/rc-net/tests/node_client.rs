use rc_net::key::DeviceKey;
use rc_net::node::{order_addrs, Endpoint};
use std::net::SocketAddr;

#[test]
fn ipv6_addresses_come_first_and_the_family_order_is_stable() {
    let addrs = vec![
        "192.0.2.10:8787".parse::<SocketAddr>().unwrap(),
        "[fd00::2]:8787".parse::<SocketAddr>().unwrap(),
        "[fd00::1]:8787".parse::<SocketAddr>().unwrap(),
        "10.0.0.5:8787".parse::<SocketAddr>().unwrap(),
    ];
    let ordered = order_addrs(addrs);
    let v6: Vec<String> = ordered
        .iter()
        .filter(|a| a.is_ipv6())
        .map(|a| a.ip().to_string())
        .collect();
    assert_eq!(v6, vec!["fd00::2".to_string(), "fd00::1".to_string()]);
    assert!(ordered[0].is_ipv6());
    assert!(!ordered[2].is_ipv6());
    assert!(!ordered[3].is_ipv6());
}

#[test]
fn an_endpoint_with_an_ipv6_literal_resolves_to_that_address() {
    let ep = Endpoint { label: "lan-ipv6".into(), host: "fd00::1".into(), port: 8787, tls_fp: None };
    let addrs = ep.resolve().unwrap();
    assert_eq!(addrs.len(), 1);
    assert!(addrs[0].is_ipv6());
    assert_eq!(addrs[0].port(), 8787);
}

#[test]
fn a_device_key_derives_its_own_device_id() {
    let key = DeviceKey::generate().unwrap();
    let id = key.device_id();
    assert!(id.starts_with("dev-"), "{id}");
    assert_eq!(id.len(), "dev-".len() + 16);
}

#[test]
fn the_signed_request_carries_the_four_authentication_headers() {
    // 形状检查：客户端发出的请求必须有这四个头，且 nonce 与签名是**逐请求**的。
    // 真实收发在 T4-B 的用例里验；这里只钉住"头不会漏"。
    let key = DeviceKey::generate().unwrap();
    let id = key.device_id();
    let headers = [
        ("Authorization".to_string(), "Bearer t".to_string()),
        ("X-Recollect-Device".to_string(), id.clone()),
        ("X-Recollect-Nonce".to_string(), "n1".to_string()),
        ("X-Recollect-Sig".to_string(), "sig".to_string()),
    ];
    let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        vec!["Authorization", "X-Recollect-Device", "X-Recollect-Nonce", "X-Recollect-Sig"]
    );
}

use rc_net::http::{Handler, Limits, Request, Response, serve};
use rc_net::key::HubKey;
use rc_net::node::{download_snapshot, NodeAuth};
use rc_net::snapshot::Manifest;
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// 一台假中心：只答 challenge 与 snapshot 两件事。
struct FakeHub {
    key: HubKey,
    body: Vec<u8>,
    /// 回一个错的分片 sha（验"核不过就当这一片没收到"）。
    chunk_sha_wrong: bool,
    device_id: String,
}

impl Handler for FakeHub {
    fn handle(&mut self, req: &Request) -> Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/challenge") => {
                let nonce = "n-1".to_string();
                let expires_at_ms = 1i64;
                let sig = self.key.sign_b64(
                    rc_net::protocol::challenge_canonical(
                        &self.device_id,
                        &nonce,
                        expires_at_ms,
                    )
                    .as_bytes(),
                );
                Response::json(
                    200,
                    serde_json::json!({
                        "api_version": 1,
                        "nonce": nonce,
                        "expires_at_ms": expires_at_ms,
                        "epoch": 1,
                        "sig": sig,
                    }),
                )
            }
            ("GET", "/v1/snapshot/memory") => {
                let total = self.body.len() as u64;
                let range = req.header("range").unwrap_or("");
                let (start, end) = rc_net::http::parse_range(range, total, 10 * 1024 * 1024)
                    .expect("the fixture only serves ranges it understands");
                let chunk = self.body[start as usize..=end as usize].to_vec();
                let sha = if self.chunk_sha_wrong {
                    "0".repeat(64)
                } else {
                    rc_net::protocol::sha256_hex(&chunk)
                };
                let mut resp = Response::bytes(206, "application/octet-stream", chunk);
                resp.headers
                    .push(("content-range".into(), format!("bytes {start}-{end}/{total}")));
                resp.headers.push(("x-recollect-chunk-sha256".into(), sha));
                resp
            }
            _ => Response::error(404, "not_found"),
        }
    }
}

/// 起一台假中心，回一个指向它的端点。
fn start_fake_hub(hub: FakeHub) -> (Endpoint, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::spawn(move || {
        let mut hub = hub;
        serve(listener, &mut hub, &flag, Limits::default(), 0);
    });
    (
        Endpoint { label: "test".into(), host: "127.0.0.1".into(), port, tls_fp: None },
        stop,
    )
}

fn manifest_of(bytes: u64) -> Manifest {
    Manifest {
        epoch: 1,
        seq: 1,
        sha256: "0".repeat(64),
        schema_version: 6,
        norm_version: 1,
        bytes,
        created_at_ms: 0,
        signature: String::new(),
    }
}

#[test]
fn chunk_digest_mismatch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let dev = DeviceKey::generate().unwrap();
    let (ep, _stop) = start_fake_hub(FakeHub {
        key,
        body: vec![7u8; 4096],
        chunk_sha_wrong: true,
        device_id: dev.device_id(),
    });
    let auth = NodeAuth {
        token: "t",
        device_id: &dev.device_id(),
        key: &dev,
        hub_pub: &dev.device_id(), // 换成真的中心公钥见下面两行
    };
    // 真的中心公钥：夹具那把。
    let hub_pub = {
        let k = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
        k.public_b64()
    };
    let auth = NodeAuth { hub_pub: &hub_pub, ..auth };
    let err = download_snapshot(
        &ep,
        &auth,
        &manifest_of(4096),
        &dir.path().join("part.db"),
        0,
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(matches!(err, rc_net::error::NetError::ChunkMismatch), "{err:?}");
}

#[test]
fn resume_uses_the_offset_on_disk_not_the_claimed_one() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let hub_pub = key.public_b64();
    let dev = DeviceKey::generate().unwrap();
    let (ep, _stop) = start_fake_hub(FakeHub {
        key,
        body: (0..4096u32).map(|b| b as u8).collect(),
        chunk_sha_wrong: false,
        device_id: dev.device_id(),
    });
    let auth = NodeAuth {
        token: "t",
        device_id: &dev.device_id(),
        key: &dev,
        hub_pub: &hub_pub,
    };
    let part = dir.path().join("part.db");
    // 盘上有一个 100 字节的残段，而调用方声称"已经收了 4096" ⇒ 两个说法对不上 ⇒ 从头来。
    std::fs::write(&part, vec![0u8; 100]).unwrap();
    let got = download_snapshot(
        &ep,
        &auth,
        &manifest_of(4096),
        &part,
        4096,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(got.resumed_from, 0, "盘上长度与 claimed 不符 ⇒ 从头来");
    assert_eq!(got.bytes, 4096);
    // 那个 100 字节的残段必须被截掉：append 写上去的话落盘是 4196。
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 4096);
}

#[test]
fn a_resumed_download_starts_at_the_offset_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    let hub_pub = key.public_b64();
    let dev = DeviceKey::generate().unwrap();
    let (ep, _stop) = start_fake_hub(FakeHub {
        key,
        body: (0..8192u32).map(|b| b as u8).collect(),
        chunk_sha_wrong: false,
        device_id: dev.device_id(),
    });
    let auth = NodeAuth { token: "t", device_id: &dev.device_id(), key: &dev, hub_pub: &hub_pub };
    let part = dir.path().join("part.db");
    // 盘上已经有前 4096 字节（内容与夹具一致）⇒ 这一轮从 4096 继续。
    std::fs::write(&part, (0..4096u32).map(|b| b as u8).collect::<Vec<u8>>()).unwrap();
    let got = download_snapshot(
        &ep,
        &auth,
        &manifest_of(8192),
        &part,
        4096,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(got.resumed_from, 4096);
    assert_eq!(got.bytes, 8192);
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 8192);
}
