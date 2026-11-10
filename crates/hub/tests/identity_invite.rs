//! S4b1：`/v1/identity` 与 `/v1/pair` 的票分支（Handler 层，真 `Root`）。
//!
//! 每个 fixture 都是"一台真的中心"：真库、真 hub key、真 TLS 指纹（写死一份 64 hex，
//! 这条用例面不握手——握手的用例在 rc-net/tests/tls.rs）。票由测试自己的那把手签，
//! 与被测的 `HubState` 从同一个 `hub.key` 文件加载 ⇒ 两者是同一个身份。

use hub::hub_cfg::HubConfig;
use hub::serve::HubState;
use rc_net::http::{Handler, Request};
use rc_net::invite::{HubIdentity, InviteTicket, TICKET_VERSION};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

const NOW: i64 = 1_000;
const TLS_FP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_FP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    key_path: PathBuf,
    state: HubState,
    hub_pub: String,
    /// 这台中心读到的"现在"。`0` 就是"钟在 unix epoch 之前"那一格。
    now: i64,
}

impl Fixture {
    fn new(tls_fp: Option<&str>) -> Fixture {
        Fixture::with_now(tls_fp, NOW)
    }

    fn with_now(tls_fp: Option<&str>, now: i64) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let root = Root::open(&data).unwrap();
        let key_path = dir.path().join("hub.key");
        let key = HubKey::load_or_create(&key_path).unwrap();
        let hub_pub = key.public_b64();
        let cfg = HubConfig {
            bind: "127.0.0.1:7420".into(),
            endpoints: vec!["https://127.0.0.1:7420".into()],
            max_snapshot_bps: 4096,
        };
        let state = HubState::new(root, key, cfg, &data, Box::new(move || now))
            .with_tls_fingerprint(tls_fp.map(str::to_string));
        Fixture { _dir: dir, data, key_path, state, hub_pub, now }
    }

    /// 同一把私钥的第二个句柄（`HubState` 已经拿走了第一个）。
    fn key(&self) -> HubKey {
        HubKey::load_or_create(&self.key_path).unwrap()
    }

    fn handle(&mut self, req: &Request) -> rc_net::http::Response {
        self.state.handle(req)
    }

    fn get(&mut self, path: &str) -> rc_net::http::Response {
        self.handle(&Request::new("GET", path, Vec::new(), Vec::new()))
    }

    fn post(&mut self, path: &str, body: Vec<u8>) -> rc_net::http::Response {
        self.handle(&Request::new(
            "POST",
            path,
            vec![("Content-Type".into(), "application/json".into())],
            body,
        ))
    }
}

fn error_code(resp: &rc_net::http::Response) -> String {
    let doc: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    doc["error"].as_str().unwrap_or_default().to_string()
}

/// 出票：账本登记 + 签 canonical + 拼文本（与 `hub invite` 同一套调用）。
fn issue(f: &mut Fixture, tls_fp: &str, expires_at_ms: i64) -> (String, String) {
    let id = format!("{:032x}", rand_suffix());
    let now = f.now;
    let mut root = Root::open(&f.data).unwrap();
    rc_ledger::invite::issue(root.ledger_mut(), &id, now, expires_at_ms).unwrap();
    let t = InviteTicket {
        v: TICKET_VERSION,
        id: id.clone(),
        hub_fp: rc_net::invite::hub_fingerprint(&f.hub_pub),
        tls_fp: tls_fp.to_string(),
        endpoints: vec!["https://127.0.0.1:7420".into()],
        expires_at_ms,
    };
    let sig = f.key().sign_b64(rc_net::invite::invite_canonical(&t).as_bytes());
    (rc_net::invite::encode_ticket(&t, &sig), id)
}

/// 票 id 只需在本进程内互不相同：用文件级计数器，不引新依赖。
fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn ticket_body(ticket: &str, dev: &DeviceKey) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "ticket": ticket,
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap()
}

fn code_body(code: &str, dev: &DeviceKey) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "code": code,
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap()
}

fn pair_with_ticket(f: &mut Fixture, ticket: &str, dev: &DeviceKey) -> rc_net::http::Response {
    f.post("/v1/pair", ticket_body(ticket, dev))
}

// ── /v1/identity ──────────────────────────────────────────────────────────

#[test]
fn identity_is_signed_by_the_hub_and_names_this_hub() {
    let mut f = Fixture::new(Some(TLS_FP));
    let resp = f.get("/v1/identity");
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));

    // 节点读到什么就用什么验：解出来的结构体既是被验的对象，也是 canonical 的输入。
    let id: HubIdentity = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(id.v, 1);
    assert_eq!(id.hub_pub, f.hub_pub);
    assert_eq!(id.hub_fp, rc_net::invite::hub_fingerprint(&f.hub_pub));
    assert_eq!(id.tls_fp, TLS_FP);
    assert_eq!(id.endpoints, vec!["https://127.0.0.1:7420".to_string()]);
    assert_eq!(id.epoch, 1);
    assert_eq!(id.issued_at_ms, NOW);
    assert_eq!(id.expires_at_ms, NOW + rc_net::invite::IDENTITY_TTL_MS);
    assert!(
        rc_net::key::verify_b64(
            &id.hub_pub,
            rc_net::invite::identity_canonical(&id).as_bytes(),
            &id.sig
        ),
        "公告的签名验不过"
    );
}

#[test]
fn identity_reports_an_empty_tls_fingerprint_in_plaintext_mode() {
    let mut f = Fixture::new(None);
    let resp = f.get("/v1/identity");
    assert_eq!(resp.status, 200);
    let id: HubIdentity = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(id.tls_fp, "");
    assert!(rc_net::key::verify_b64(
        &id.hub_pub,
        rc_net::invite::identity_canonical(&id).as_bytes(),
        &id.sig
    ));
}

/// 身份端点**不**要求签名/nonce，被 fence 的中心也要答：发现层问的是"你是谁"，
/// 不是"你还能不能写"。写面则照旧 409。
#[test]
fn identity_answers_even_when_the_center_is_fenced() {
    let mut f = Fixture::new(Some(TLS_FP));
    hub::promote::write_fence(&f.data, 1, NOW).unwrap();

    let resp = f.get("/v1/identity");
    assert_eq!(resp.status, 200, "fenced 之后身份端点不该拒答");
    let id: HubIdentity = serde_json::from_slice(&resp.body).unwrap();
    assert!(rc_net::key::verify_b64(
        &id.hub_pub,
        rc_net::invite::identity_canonical(&id).as_bytes(),
        &id.sig
    ));

    let dev = DeviceKey::generate().unwrap();
    let (ticket, _) = issue(&mut f, TLS_FP, NOW + 60_000);
    let resp = pair_with_ticket(&mut f, &ticket, &dev);
    assert_eq!(resp.status, 409, "fenced 中心仍然配对成功了");
    assert_eq!(error_code(&resp), "center_fenced");
}

#[test]
fn identity_has_no_signature_or_nonce_requirement() {
    let mut f = Fixture::new(None);
    // 完全没有头部的 GET：不加 Authorization、不加 nonce。旧实现若把这条路由
    // 挂进认证链，这里会是 401。
    assert_eq!(f.get("/v1/identity").status, 200);
    assert_eq!(f.get("/v1/identity?probe=1").status, 200);
    // 方法不对仍是 405，不是 404：这条路由存在，只是不接受写。
    let resp = f.post("/v1/identity", Vec::new());
    assert_eq!(resp.status, 405);
}

// ── /v1/pair：票 ─────────────────────────────────────────────────────────

#[test]
fn a_ticket_pairs_a_device_and_the_token_works() {
    let mut f = Fixture::new(Some(TLS_FP));
    let dev = DeviceKey::generate().unwrap();
    let (ticket, _id) = issue(&mut f, TLS_FP, NOW + 600_000);
    let resp = pair_with_ticket(&mut f, &ticket, &dev);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    let token = doc["token"].as_str().unwrap();
    assert!(!token.is_empty());
    assert_eq!(doc["hub_pub"], f.hub_pub);
    assert_eq!(doc["hub_fp"], rc_net::invite::hub_fingerprint(&f.hub_pub));
    assert_eq!(doc["tls_fingerprint"], TLS_FP);
    assert_eq!(doc["epoch"], 1);

    // token 是真 token：拿它换 nonce（第二道认证）必须 200。
    let resp = f.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
}

#[test]
fn a_ticket_is_single_use_for_the_same_and_for_other_devices() {
    let mut f = Fixture::new(Some(TLS_FP));
    let dev_a = DeviceKey::generate().unwrap();
    let (ticket, _) = issue(&mut f, TLS_FP, NOW + 600_000);
    assert_eq!(pair_with_ticket(&mut f, &ticket, &dev_a).status, 200);

    let dev_b = DeviceKey::generate().unwrap();
    let resp = pair_with_ticket(&mut f, &ticket, &dev_b);
    assert_eq!(resp.status, 409, "同一张票被第二台设备用了");
    assert_eq!(error_code(&resp), "invite_used");

    let resp = pair_with_ticket(&mut f, &ticket, &dev_a);
    assert_eq!(resp.status, 409, "同一台设备重放同一张票");
    assert_eq!(error_code(&resp), "invite_used");
}

#[test]
fn an_expired_ticket_is_refused_and_stays_unused() {
    let mut f = Fixture::new(None);
    let dev = DeviceKey::generate().unwrap();
    let (ticket, id) = issue(&mut f, "", NOW - 1);
    let resp = pair_with_ticket(&mut f, &ticket, &dev);
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_expired");

    let used: Option<i64> = Root::open(&f.data)
        .unwrap()
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM invite WHERE id=?1", [&id], |r| r.get(0))
        .unwrap();
    assert_eq!(used, None, "过期票被拒时不许把它标成已用");
}

/// S4fix-A R13：钟在 unix epoch 之前（`now == 0`）⇒ 每一张票都按过期拒，
/// 而且是**现有**那个码（`invite_expired`），不新增码。
/// 账本那一步的判据是 `now_ms <= 0 || expires_at_ms < now_ms`，预检放行的票也死在这一步。
#[test]
fn a_clock_before_the_epoch_refuses_every_ticket_as_expired() {
    let mut f = Fixture::with_now(None, 0);
    let dev = DeviceKey::generate().unwrap();
    // 有效期远在未来：只看 `expires_at_ms < now` 会读成"还能用"。
    let (ticket, id) = issue(&mut f, "", 60_000);
    let resp = pair_with_ticket(&mut f, &ticket, &dev);
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_expired");

    let used: Option<i64> = Root::open(&f.data)
        .unwrap()
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM invite WHERE id=?1", [&id], |r| r.get(0))
        .unwrap();
    assert_eq!(used, None, "坏钟下的拒票把票标成已用了");
}

#[test]
fn a_revoked_ticket_is_refused() {
    let mut f = Fixture::new(None);
    let dev = DeviceKey::generate().unwrap();
    let (ticket, id) = issue(&mut f, "", NOW + 600_000);
    assert!(rc_ledger::invite::revoke(Root::open(&f.data).unwrap().ledger_mut(), &id, NOW).unwrap());
    let resp = pair_with_ticket(&mut f, &ticket, &dev);
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_revoked");
}

/// 篡改 = 改内容但签名不动（攻击者没有私钥，只能这么干）。
/// 三种改法都必须在验签那一格死掉，而且是**静态**的 `invite_invalid`，不回显票。
#[test]
fn tampered_tickets_are_refused_without_echoing_them() {
    let mut f = Fixture::new(Some(TLS_FP));
    let hub_pub = f.hub_pub.clone();
    let id = "tamper-0000";
    rc_ledger::invite::issue(Root::open(&f.data).unwrap().ledger_mut(), id, NOW, NOW + 600_000)
        .unwrap();
    let good = InviteTicket {
        v: TICKET_VERSION,
        id: id.into(),
        hub_fp: rc_net::invite::hub_fingerprint(&hub_pub),
        tls_fp: TLS_FP.into(),
        endpoints: vec!["https://127.0.0.1:7420".into()],
        expires_at_ms: NOW + 600_000,
    };
    let sig = f.key().sign_b64(rc_net::invite::invite_canonical(&good).as_bytes());

    let variants = [
        // 改 hub 指纹的一格 hex
        InviteTicket { hub_fp: OTHER_FP.into(), ..good.clone() },
        // 改终端指纹
        InviteTicket { tls_fp: OTHER_FP.into(), ..good.clone() },
        // 改过期时刻（试图把短票续命）
        InviteTicket { expires_at_ms: good.expires_at_ms + 1, ..good.clone() },
        // 改端点
        InviteTicket { endpoints: vec!["https://192.0.2.9:7420".into()], ..good.clone() },
    ];
    for t in &variants {
        let text = rc_net::invite::encode_ticket(t, &sig);
        let dev = DeviceKey::generate().unwrap();
        let resp = pair_with_ticket(&mut f, &text, &dev);
        assert_eq!(resp.status, 403, "改过的票被接下了：{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(error_code(&resp), "invite_invalid");
        let body = String::from_utf8_lossy(&resp.body).to_string();
        assert!(!body.contains(&text[..20]), "错误响应回显了票：{body}");
    }

    // 签名本身改一格：同样是 `invite_invalid`。
    let good_text = rc_net::invite::encode_ticket(&good, &sig);
    let mut chars: Vec<char> = good_text.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
    let broken_sig: String = chars.into_iter().collect();
    let resp = pair_with_ticket(&mut f, &broken_sig, &DeviceKey::generate().unwrap());
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_invalid");

    // 上面那些失败一发都不许烧票：原票仍然可用。
    let resp = pair_with_ticket(&mut f, &good_text, &DeviceKey::generate().unwrap());
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
}

/// 票是**另一台中心**出的：签名用另一把钥匙 ⇒ 本中心验不过。
#[test]
fn a_ticket_from_another_hub_is_refused() {
    let mut f = Fixture::new(None);
    let other_dir = tempfile::tempdir().unwrap();
    let other = HubKey::load_or_create(&other_dir.path().join("hub.key")).unwrap();
    let t = InviteTicket {
        v: TICKET_VERSION,
        id: "foreign".into(),
        hub_fp: rc_net::invite::hub_fingerprint(&other.public_b64()),
        tls_fp: String::new(),
        endpoints: vec!["https://192.0.2.10:7420".into()],
        expires_at_ms: NOW + 600_000,
    };
    let sig = other.sign_b64(rc_net::invite::invite_canonical(&t).as_bytes());
    let resp = pair_with_ticket(
        &mut f,
        &rc_net::invite::encode_ticket(&t, &sig),
        &DeviceKey::generate().unwrap(),
    );
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_invalid");

    // 本中心的钥匙签一张"声称是别家"的票：只有实现有 bug 才会发生，但仍要单独点名。
    let t = InviteTicket { hub_fp: OTHER_FP.into(), ..t };
    let sig = f.key().sign_b64(rc_net::invite::invite_canonical(&t).as_bytes());
    let resp = pair_with_ticket(
        &mut f,
        &rc_net::invite::encode_ticket(&t, &sig),
        &DeviceKey::generate().unwrap(),
    );
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_hub_mismatch");
}

/// 票里钉的终端指纹与**这一次会话**的终端必须一致：两侧任一为空/不同都拒。
/// 这一格就是"发现到冒充中心"的服务端回声——票是给 TLS 档签的，明文档不收。
#[test]
fn tls_fingerprint_must_match_the_running_center() {
    let mut f = Fixture::new(Some(TLS_FP));
    let (plain, _) = issue(&mut f, "", NOW + 600_000);
    let resp = pair_with_ticket(&mut f, &plain, &DeviceKey::generate().unwrap());
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_tls_mismatch");

    let (other, _) = issue(&mut f, OTHER_FP, NOW + 600_000);
    let resp = pair_with_ticket(&mut f, &other, &DeviceKey::generate().unwrap());
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_tls_mismatch");

    // 反向：明文中心收到一张钉了指纹的票，同样拒。
    let mut plain_center = Fixture::new(None);
    let (pinned, _) = issue(&mut plain_center, TLS_FP, NOW + 600_000);
    let resp = pair_with_ticket(&mut plain_center, &pinned, &DeviceKey::generate().unwrap());
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "invite_tls_mismatch");
}

/// 已经被一台**活**设备占着的 id：409，且票不被消费——换一台设备同一张票还能用。
#[test]
fn an_occupied_device_id_does_not_burn_the_ticket() {
    let mut f = Fixture::new(None);
    let dev = DeviceKey::generate().unwrap();
    let (first, _) = issue(&mut f, "", NOW + 600_000);
    assert_eq!(pair_with_ticket(&mut f, &first, &dev).status, 200);

    let (second, second_id) = issue(&mut f, "", NOW + 600_000);
    let resp = pair_with_ticket(&mut f, &second, &dev);
    assert_eq!(resp.status, 409);
    assert_eq!(error_code(&resp), "device_exists");

    let used: Option<i64> = Root::open(&f.data)
        .unwrap()
        .ledger()
        .conn()
        .query_row("SELECT used_at_ms FROM invite WHERE id=?1", [&second_id], |r| r.get(0))
        .unwrap();
    assert_eq!(used, None, "device_exists 之后票被烧了（事务没回滚）");

    let other_dev = DeviceKey::generate().unwrap();
    assert_eq!(pair_with_ticket(&mut f, &second, &other_dev).status, 200);
}

// ── /v1/pair：请求形状与旧路 ─────────────────────────────────────────────

#[test]
fn pair_requires_exactly_one_credential_and_a_derived_device_id() {
    let mut f = Fixture::new(None);
    let dev = DeviceKey::generate().unwrap();
    let (ticket, _) = issue(&mut f, "", NOW + 600_000);
    let (code, _) = rc_net::pair::new_pair_code().unwrap();

    // 两个都给
    let both = serde_json::to_vec(&json!({
        "ticket": ticket,
        "code": code,
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap();
    let resp = f.post("/v1/pair", both);
    assert_eq!(resp.status, 400);
    assert_eq!(error_code(&resp), "bad_pair_request");

    // 一个都不给
    let neither = serde_json::to_vec(&json!({
        "device_id": dev.device_id(),
        "pubkey": dev.public_b64(),
    }))
    .unwrap();
    assert_eq!(f.post("/v1/pair", neither).status, 400);

    // device_id 不是从 pubkey 导出的
    let lying = serde_json::to_vec(&json!({
        "ticket": ticket,
        "device_id": "dev-0000000000000000",
        "pubkey": dev.public_b64(),
    }))
    .unwrap();
    assert_eq!(f.post("/v1/pair", lying).status, 400);

    // 上面三次都没碰票
    assert_eq!(pair_with_ticket(&mut f, &ticket, &dev).status, 200);
}

/// 旧 code 路径原样保留（弃用但兼容）：响应同样带 `hub_fp`/`tls_fingerprint`，
/// 失败码也仍是 `pair_code_*`。
#[test]
fn the_legacy_pair_code_still_works() {
    let mut f = Fixture::new(None);
    let dev = DeviceKey::generate().unwrap();
    let (code, hash) = rc_net::pair::new_pair_code().unwrap();
    rc_ledger::device::issue_pair_code(
        Root::open(&f.data).unwrap().ledger_mut(),
        &hash,
        NOW,
        NOW + 300_000,
    )
    .unwrap();

    let resp = f.post("/v1/pair", code_body(&code, &dev));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    assert!(!doc["token"].as_str().unwrap().is_empty());
    assert_eq!(doc["hub_fp"], rc_net::invite::hub_fingerprint(&f.hub_pub));
    assert_eq!(doc["tls_fingerprint"], Value::Null, "明文档的 tls_fingerprint 必须是 null");

    // 一次性、且失败码不变
    let dev_b = DeviceKey::generate().unwrap();
    let resp = f.post("/v1/pair", code_body(&code, &dev_b));
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "pair_code_used");

    // 认不出的码
    let resp = f.post("/v1/pair", code_body("ZZZZZ-ZZZZZ", &DeviceKey::generate().unwrap()));
    assert_eq!(resp.status, 403);
    assert_eq!(error_code(&resp), "pair_code_unknown");
}

// ── CLI ──────────────────────────────────────────────────────────────────

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().unwrap()
}

fn cli_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let tls_dir = data.join("tls");
    let addr = "127.0.0.1:7420".parse().unwrap();
    let id = rc_net::tls::ensure_identity(&tls_dir, addr).unwrap();
    std::fs::write(
        data.join("hub.toml"),
        "[hub]\nbind = \"127.0.0.1:7420\"\nendpoints = [\"https://127.0.0.1:7420\"]\n",
    )
    .unwrap();
    assert_eq!(rc_net::tls::fingerprint_at(&tls_dir).unwrap(), id.fingerprint);
    (dir, data)
}

#[test]
fn invite_cli_issues_a_verifiable_ticket_and_revokes_it() {
    let (_dir, data) = cli_fixture();
    let data_s = data.to_str().unwrap();
    let out = run(&["invite", "--data-dir", data_s, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let doc: Value = serde_json::from_str(text.trim()).unwrap();
    let ticket = doc["ticket"].as_str().unwrap();
    assert!(ticket.starts_with("rc1."), "不是文本票：{ticket}");

    let key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
    let verified = rc_net::invite::verify_ticket(ticket, &key.public_b64()).unwrap();
    assert_eq!(verified.id, doc["id"].as_str().unwrap());
    assert_eq!(verified.hub_fp, doc["hub_fp"].as_str().unwrap());
    assert_eq!(verified.hub_fp, rc_net::invite::hub_fingerprint(&key.public_b64()));
    assert_eq!(verified.tls_fp, doc["tls_fp"].as_str().unwrap());
    assert_eq!(verified.tls_fp, rc_net::tls::fingerprint_at(&data.join("tls")).unwrap());
    assert_eq!(verified.endpoints, vec!["https://127.0.0.1:7420".to_string()]);
    assert_eq!(verified.expires_at_ms, doc["expires_at_ms"].as_i64().unwrap());
    assert_eq!(verified.id.len(), 32, "票 id 该是 16 字节 hex");
    assert!(verified.id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!text.contains(data_s), "出票的输出带了路径：{text}");

    let out = run(&["invite", "revoke", &verified.id, "--data-dir", data_s]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["revoked"], true);

    // 幂等：再作废一次仍是成功，且票在账本里是撤销态。
    let out = run(&["invite", "revoke", &verified.id, "--data-dir", data_s]);
    assert!(out.status.success());
    let revoked: Option<i64> = Root::open(&data)
        .unwrap()
        .ledger()
        .conn()
        .query_row("SELECT revoked_at_ms FROM invite WHERE id=?1", [&verified.id], |r| r.get(0))
        .unwrap();
    assert!(revoked.is_some(), "revoke 没有落库");

    // 不存在的 id ⇒ 退出 3（运维失败），不是成功。
    let out = run(&["invite", "revoke", "nope", "--data-dir", data_s]);
    assert_eq!(out.status.code(), Some(3));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(data_s));
}

#[test]
fn invite_cli_refuses_without_a_certificate_and_without_leaking_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-canary-invite");
    let out = run(&["invite", "--data-dir", data.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3), "没有证书时必须退出 3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no tls certificate"), "静态话变了：{stderr}");
    assert!(!stderr.contains("data-canary-invite"), "stderr 带了路径：{stderr}");
    assert!(out.stdout.is_empty(), "失败路径不该有 stdout");

    // `0`/负 TTL 是用法错误，不是"永不过期"。
    let (_d, data) = cli_fixture();
    let out = run(&["invite", "--ttl-ms", "0", "--data-dir", data.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let out = run(&["invite", "--ttl-ms", "-5", "--data-dir", data.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
}

/// 出票要先把 `hub.toml` 找对：缺省取 `<data-dir>/hub.toml`，`--hub-config` 可覆盖。
#[test]
fn invite_cli_needs_a_hub_config_for_endpoints() {
    let (_dir, data) = cli_fixture();
    std::fs::remove_file(data.join("hub.toml")).unwrap();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("hub config not found"));

    let cfg = data.join("hub.toml");
    std::fs::write(&cfg, "[hub]\nbind = \"127.0.0.1:7420\"\nendpoints = [\"https://127.0.0.1:7421\"]\n")
        .unwrap();
    let out = run(&[
        "invite",
        "--hub-config",
        cfg.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let ticket = String::from_utf8(out.stdout).unwrap();
    let key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
    let verified = rc_net::invite::verify_ticket(ticket.trim(), &key.public_b64()).unwrap();
    assert_eq!(verified.endpoints, vec!["https://127.0.0.1:7421".to_string()]);
}

/// 票是秘密：它只出现在 stdout 的那一行里，不进 stderr、不进审计任何一行。
/// 这一条走的是纯文本输出（不带 `--json`），顺带钉住"票仍是唯一一条 stdout 行"。
#[test]
fn invite_cli_prints_only_the_ticket_on_stdout() {
    let (_dir, data) = cli_fixture();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap()]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "出票的 stdout 不止一行：{text}");
    assert!(text.trim().starts_with("rc1."));
}

/// S4fix-A R1：TLS 中心的票里不许出现 `http://` 端点——节点会因此拒掉**整张**票，
/// 而错误会指向"票被伪造"。所以出票这一步就退出 3 并点名第几条。
#[test]
fn invite_cli_refuses_plaintext_endpoints_on_a_tls_center() {
    let (_dir, data) = cli_fixture();
    // 非回环 bind ⇒ 与 serve 同一条判定：默认 TLS。
    std::fs::write(
        data.join("hub.toml"),
        "[hub]\nbind = \"100.64.0.10:7420\"\nendpoints = [\"https://100.64.0.10:7420\", \"http://100.64.0.10:7420\"]\n",
    )
    .unwrap();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3), "TLS 中心给 http:// 端点出票了");
    assert!(out.stdout.is_empty(), "失败路径不该有 stdout");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("hub.endpoints[1]"), "没点名第几条：{err}");
    assert!(err.contains("http://100.64.0.10:7420"), "没回显那一条（地址不是秘密）：{err}");
    assert!(!err.contains("rc1."), "错误串带了票：{err}");

    // 全 https:// ⇒ 照常出票。
    std::fs::write(
        data.join("hub.toml"),
        "[hub]\nbind = \"100.64.0.10:7420\"\nendpoints = [\"https://100.64.0.10:7420\"]\n",
    )
    .unwrap();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).trim().starts_with("rc1."));
}

/// 明文面不拦：回环 dev（`bind = 127.0.0.1`）与显式 `--insecure-http`。
#[test]
fn invite_cli_allows_plaintext_endpoints_on_a_dev_center() {
    let (_dir, data) = cli_fixture();
    std::fs::write(
        data.join("hub.toml"),
        "[hub]\nbind = \"127.0.0.1:7420\"\nendpoints = [\"http://127.0.0.1:7420\"]\n",
    )
    .unwrap();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap()]);
    assert!(out.status.success(), "回环 dev 面被拦了：{}", String::from_utf8_lossy(&out.stderr));

    // 非回环 + 显式 `--insecure-http`：出票照常。
    std::fs::write(
        data.join("hub.toml"),
        "[hub]\nbind = \"100.64.0.10:7420\"\nendpoints = [\"http://100.64.0.10:7420\"]\n",
    )
    .unwrap();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap(), "--insecure-http"]);
    assert!(out.status.success(), "--insecure-http 没生效：{}", String::from_utf8_lossy(&out.stderr));
}

/// 票是秘密：它只出现在 stdout 的那一行里，不进 stderr、不进审计任何一行。
///
/// S4fix-A R11：发行与撤销**必须**各留一条审计（`invite_issued`/`invite_revoked`，
/// subject = 票 id、detail 留空）。只断言"不含票"会漏掉"根本没有任何记录"这个更强的问题。
#[test]
fn the_ticket_never_lands_in_the_audit_log() {
    let (_dir, data) = cli_fixture();
    let out = run(&["invite", "--data-dir", data.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let issued: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ticket = issued["ticket"].as_str().unwrap().to_string();
    let id = issued["id"].as_str().unwrap().to_string();
    assert!(ticket.starts_with("rc1."));

    let audit_rows = |data: &std::path::Path| -> Vec<String> {
        let root = Root::open(data).unwrap();
        let mut stmt = root
            .ledger()
            .conn()
            .prepare("SELECT actor, action, subject, detail FROM audit_log ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            Ok(format!(
                "{}|{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default()
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };

    // 发行：恰好一条，点名票 id，detail 空。
    let rows = audit_rows(&data);
    assert_eq!(
        rows,
        vec![format!("operator|invite_issued|{id}|")],
        "发行的审计行不对：{rows:?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains(&ticket) && !row.contains(&ticket[..32])),
        "票（或它的前缀）被写进了审计：{rows:?}"
    );

    // 撤销：再加一条；重复撤销不再写（幂等）。
    for _ in 0..2 {
        let out = run(&["invite", "revoke", &id, "--data-dir", data.to_str().unwrap()]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let rows = audit_rows(&data);
    assert_eq!(
        rows,
        vec![
            format!("operator|invite_issued|{id}|"),
            format!("operator|invite_revoked|{id}|"),
        ],
        "撤销的审计行不对（或重复撤销写了第二行）：{rows:?}"
    );
    assert!(rows.iter().all(|row| !row.contains(&ticket)), "票被写进了审计：{rows:?}");
}
