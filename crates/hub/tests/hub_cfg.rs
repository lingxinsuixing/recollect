use hub::hub_cfg::{load, HubConfig};
use std::path::Path;

fn write(dir: &Path, body: &str) -> std::path::PathBuf {
    let p = dir.join("hub.toml");
    std::fs::write(&p, body).unwrap();
    p
}

const GOOD: &str = r#"
[hub]
bind = "100.64.0.10:7420"
endpoints = ["http://100.64.0.10:7420", "http://100.64.0.11:7420"]
max_snapshot_bps = 4096
"#;

#[test]
fn missing_config_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(&dir.path().join("absent.toml")).unwrap().is_none());
}

#[test]
fn a_valid_config_round_trips_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let c: HubConfig = load(&write(dir.path(), GOOD)).unwrap().unwrap();
    assert_eq!(c.bind, "100.64.0.10:7420");
    assert_eq!(c.endpoints, vec!["http://100.64.0.10:7420", "http://100.64.0.11:7420"]);
    assert_eq!(c.max_snapshot_bps, 4096);
}

#[test]
fn endpoints_accept_http_and_https_host_port() {
    let dir = tempfile::tempdir().unwrap();
    // 两种 scheme 都是合法公告：https 是给节点的信号（配 `hub_tls_fp` 走 TLS），
    // http 留给回环开发面。中心自己不消费 scheme。
    let body = "[hub]\nbind = \"100.64.0.10:7420\"\nendpoints = [\"https://100.64.0.10:7420\", \"http://100.64.0.11:7420\"]\n";
    let c = load(&write(dir.path(), body)).unwrap().unwrap();
    assert_eq!(
        c.endpoints,
        vec!["https://100.64.0.10:7420", "http://100.64.0.11:7420"]
    );
}

#[test]
fn endpoints_must_be_http_or_https_host_port_without_a_path() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        "http://100.64.0.10:7420/v1",
        "https://100.64.0.10:7420/v1",
        "http://hub.example:7420",
        "https://hub.example:7420",
        "http://100.64.0.10",
        "ftp://100.64.0.10:7420",
        "100.64.0.10:7420",
        "",
    ] {
        let body = format!("[hub]\nbind = \"100.64.0.10:7420\"\nendpoints = [\"{bad}\"]\n");
        assert!(load(&write(dir.path(), &body)).is_err(), "收下了非法端点：{bad}");
    }
}

#[test]
fn bind_must_parse_as_a_socket_address() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["localhost:7420", "100.64.0.10", ""] {
        let body = format!("[hub]\nbind = \"{bad}\"\nendpoints = [\"http://100.64.0.10:7420\"]\n");
        assert!(load(&write(dir.path(), &body)).is_err(), "收下了非法 bind：{bad}");
    }
}

#[test]
fn zero_rate_limit_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let body = "[hub]\nbind = \"100.64.0.10:7420\"\nendpoints = [\"http://100.64.0.10:7420\"]\nmax_snapshot_bps = 0\n";
    assert!(load(&write(dir.path(), body)).is_err(), "0 被读成「不限速」是配置面的静默改判");
}
