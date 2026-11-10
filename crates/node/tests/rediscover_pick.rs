//! S4d1：`pick_rediscovered` 的**纯函数**用例（不起进程、不发一个包）。
//!
//! 这一层要钉住的是三道匹配门：公钥对上、证书指纹对上、地址能走。门外的候选一律
//! 静默跳过；**同一把钥匙、另一枚证书**则是 `identity_mismatch`——那是要人去查证书轮换
//! 的那一响，不能和"网上没有我那台中心"共用一个码。

use node::sync::{pick_rediscovered, SyncError};
use rc_net::discovery::Candidate;

const FP_OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FP_OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// 一条公告的可读投影。`hub_fp` 按中心那一侧同一公式由公钥现算（本层不用它，
/// 但用真值构造能把"将来拿它做键"的改动挡在形状之外）。
fn cand(hub_pub: &str, tls_fp: &str, endpoints: &[&str]) -> Candidate {
    Candidate {
        hub_fp: rc_net::invite::hub_fingerprint(hub_pub),
        tls_fp: tls_fp.to_string(),
        epoch: 1,
        endpoints: endpoints.iter().map(|s| s.to_string()).collect(),
        hub_pub: hub_pub.to_string(),
        seen_from: "127.0.0.1:37420".parse().unwrap(),
    }
}

#[test]
fn my_hub_is_picked_with_the_paired_label() {
    let got = pick_rediscovered("hub-A", FP_OLD, &[cand("hub-A", FP_OLD, &["https://127.0.0.1:9443"])])
        .expect("对上公钥与指纹的候选必须收下");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].label, "paired-1", "路名与配对/`sync` 用同一套序号");
    assert_eq!(got[0].host, "127.0.0.1");
    assert_eq!(got[0].port, 9443);
    assert_eq!(got[0].tls_fp.as_deref(), Some(FP_OLD), "地址必须跟着钉死的指纹走");
}

#[test]
fn another_hub_is_ignored_even_if_it_is_the_only_one() {
    // 网上只有它一台也不算数：绝不因为"看起来没人跟我抢"就改用别的中心。
    let got = pick_rediscovered("hub-A", FP_OLD, &[cand("hub-B", FP_OTHER, &["https://192.0.2.9:9443"])])
        .expect("外来中心不是错误，只是不采纳");
    assert!(got.is_empty(), "外来中心必须被静默跳过：{got:?}");
}

#[test]
fn a_borrowed_certificate_does_not_buy_trust() {
    // 冒充者：公钥不是我的，指纹却是我的。指纹是公开的，撞上它不构成任何信任。
    let got = pick_rediscovered("hub-A", FP_OLD, &[cand("hub-B", FP_OLD, &["https://192.0.2.9:9443"])])
        .expect("只撞指纹不是错误");
    assert!(got.is_empty(), "只撞上指纹的候选必须被跳过：{got:?}");
}

#[test]
fn the_same_key_with_another_certificate_is_a_loud_failure() {
    // 同一把中心钥匙、另一枚证书：中心换了证书或有人在冒充。下一步是查证书轮换，
    // 所以这里**一个地址都不返回**（重试新地址等于把响铃按掉）。
    let err = pick_rediscovered("hub-A", FP_OLD, &[cand("hub-A", FP_OTHER, &["https://127.0.0.1:9443"])])
        .expect_err("同钥匙换证书必须响亮失败");
    assert_eq!(err, SyncError::IdentityMismatch);
    assert_eq!(err.code(), "identity_mismatch");
}

#[test]
fn a_matching_certificate_later_in_the_table_still_wins() {
    // R15：先扫完整张表。同键两条公告里"错指纹在前、对指纹在后"——
    // 前一条不能把后一条的地址一起判死，否则中心同时公告两份身份时永远收敛不了。
    let got = pick_rediscovered(
        "hub-A",
        FP_OLD,
        &[
            cand("hub-A", FP_OTHER, &["https://127.0.0.1:9443"]),
            cand("hub-A", FP_OLD, &["https://127.0.0.1:9444"]),
        ],
    )
    .expect("表里有匹配的证书就不该报错");
    assert_eq!(got.len(), 1, "只采纳指纹对上的那一条：{got:?}");
    assert_eq!(got[0].port, 9444);
    assert_eq!(got[0].tls_fp.as_deref(), Some(FP_OLD));
}

#[test]
fn bad_addresses_are_dropped_and_the_good_one_stays() {
    let got = pick_rediscovered(
        "hub-A",
        FP_OLD,
        &[cand(
            "hub-A",
            FP_OLD,
            &[
                "http://127.0.0.1:9443",   // 非 https：票钉的是指纹，明文走不了这一支
                "https://hub.local:9443",  // 主机名：发现不替人做 DNS
                "https://127.0.0.1:0",     // 端口 0
                "https://127.0.0.1:9443",  // 唯一能走的一条
            ],
        )],
    )
    .expect("坏地址不是同步错误");
    assert_eq!(got.len(), 1, "只该留下能走的那一条：{got:?}");
    assert_eq!((got[0].host.as_str(), got[0].port), ("127.0.0.1", 9443));
}

#[test]
fn the_same_host_and_port_is_kept_once_in_input_order() {
    // 同一台中心的两条公告（不同网口/两次单播）会带来重复地址：按输入顺序去重，
    // 第一条留下，序号不跳号。
    let got = pick_rediscovered(
        "hub-A",
        FP_OLD,
        &[
            cand("hub-A", FP_OLD, &["https://127.0.0.1:9443"]),
            cand("hub-A", FP_OLD, &["https://127.0.0.1:9443", "https://127.0.0.1:9444"]),
        ],
    )
    .expect("重复地址不是错误");
    assert_eq!(got.len(), 2, "同 host:port 只留一条：{got:?}");
    let ports: Vec<u16> = got.iter().map(|e| e.port).collect();
    assert_eq!(ports, vec![9443, 9444], "顺序同输入");
    let labels: Vec<&str> = got.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(labels, vec!["paired-1", "paired-2"], "序号不跳号：{labels:?}");
}
