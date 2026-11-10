//! 端点顺序：上一次成功的路先试，其余按配置顺序。
//! 真实 LAN / 虚拟 LAN 切换后，这条顺序决定同步是“先等一下死路”还是“立刻走通”。

use rc_net::node::{order_endpoints, Endpoint};

fn ep(label: &str, host: &str) -> Endpoint {
    Endpoint {
        label: label.into(),
        host: host.into(),
        port: 8477,
        tls_fp: None,
    }
}

fn labels<'a>(endpoints: &'a [&'a Endpoint]) -> Vec<&'a str> {
    endpoints.iter().map(|e| e.label.as_str()).collect()
}

#[test]
fn last_good_moves_to_the_front_and_keeps_the_rest_in_order() {
    let endpoints = vec![
        ep("mesh", "198.51.100.3"),
        ep("lan", "192.0.2.209"),
        ep("tunnel", "127.0.0.1"),
    ];
    assert_eq!(
        labels(&order_endpoints(&endpoints, "lan")),
        vec!["lan", "mesh", "tunnel"]
    );
    assert_eq!(
        labels(&order_endpoints(&endpoints, "tunnel")),
        vec!["tunnel", "mesh", "lan"]
    );
}

#[test]
fn empty_or_unknown_last_good_keeps_the_configured_order() {
    let endpoints = vec![ep("lan", "192.0.2.209"), ep("mesh", "198.51.100.3")];
    for last in ["", "gone"] {
        assert_eq!(
            labels(&order_endpoints(&endpoints, last)),
            vec!["lan", "mesh"]
        );
    }
}

#[test]
fn a_single_endpoint_is_unchanged() {
    let endpoints = vec![ep("lan", "192.0.2.209")];
    assert_eq!(labels(&order_endpoints(&endpoints, "lan")), vec!["lan"]);
    assert_eq!(labels(&order_endpoints(&endpoints, "mesh")), vec!["lan"]);
}
