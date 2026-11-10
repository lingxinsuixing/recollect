use node::node_cfg;

#[test]
fn a_missing_config_is_none_not_a_default() {
    let dir = tempfile::tempdir().unwrap();
    assert!(node_cfg::load(&dir.path().join("node.toml")).unwrap().is_none());
}

#[test]
fn a_broken_config_is_an_error_with_a_closed_phrase() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(&p, b"data_dir = 1\n").unwrap();
    let e = node_cfg::load(&p).unwrap_err();
    assert!(!e.contains("1"), "错误消息不许回显用户字节：{e}");
}

/// S4b2：端点**可以缺席/为空**——票会把地址带来。于是"没有端点"不再是配置错误，
/// 而是 `pair`/`sync` 那一步的 `no_endpoint`（见 `invite_e2e.rs` 的最后一条）。
#[test]
fn a_config_without_endpoints_loads_but_sync_requires_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    // 完全不写 `endpoints` 键。
    std::fs::write(&p, b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n").unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("没有端点也是能读的配置");
    assert!(cfg.endpoints.is_empty(), "缺省就是空清单：{:?}", cfg.endpoints);

    // 写一个空数组：同一件事的另一种写法。
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\nendpoints = []\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("空数组也是能读的配置");
    assert!(cfg.endpoints.is_empty());

    // 但**形状坏了**仍然是配置错误：`endpoints` 只能是一个数组。
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\nendpoints = \"127.0.0.1:8787\"\n",
    )
    .unwrap();
    let e = node_cfg::load(&p).unwrap_err();
    assert!(e.contains("endpoints"), "{e}");
}

/// 空清单放行了，**单项校验一个都不许松**：端口越界、label 不是安全 token 仍要拒。
#[test]
fn a_config_with_an_illegal_endpoint_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    let bad_entries = [
        // 端口 0
        "[[endpoints]]\nlabel = \"lan\"\nhost = \"127.0.0.1\"\nport = 0\n",
        // 端口越界
        "[[endpoints]]\nlabel = \"lan\"\nhost = \"127.0.0.1\"\nport = 70000\n",
        // label 不是安全 token（含空格）
        "[[endpoints]]\nlabel = \"my lan\"\nhost = \"127.0.0.1\"\nport = 8787\n",
        // 缺 host
        "[[endpoints]]\nlabel = \"lan\"\nport = 8787\n",
        // 不是表
        "endpoints = [1]\n",
    ];
    for bad in bad_entries {
        std::fs::write(
            &p,
            format!("data_dir = \"./d\"\ndevice_key_path = \"./k\"\n{bad}"),
        )
        .unwrap();
        assert!(node_cfg::load(&p).is_err(), "收下了非法端点：{bad:?}");
    }
}

#[test]
fn a_good_config_keeps_the_endpoint_order() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\nhub_pub = \"\"\nallow_plaintext = true\n\
          [[endpoints]]\nlabel = \"lan-ipv6\"\nhost = \"fd00::1\"\nport = 8787\n\
          [[endpoints]]\nlabel = \"lan-ipv4\"\nhost = \"192.0.2.10\"\nport = 8787\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    let labels: Vec<&str> = cfg.endpoints.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(labels, vec!["lan-ipv6", "lan-ipv4"]);
    assert!(cfg.hub_pub.is_none(), "空串 = 不钉");
}

#[test]
fn llm_fields_default_to_locked_and_no_config() {
    // 两个键都不写：`llm_config=None`（B2b 没有可读的配置）、`auto_unlock=false`
    // （B1a 的默认值原样保留——不自蒸）。
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\nallow_plaintext = true\n\
          [[endpoints]]\nlabel = \"lan\"\nhost = \"fd00::1\"\nport = 8787\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    assert_eq!(cfg.llm_config, None);
    assert!(!cfg.auto_unlock, "缺省必须是锁定：默认自蒸会把额度烧在一台没人点过头的节点上");
}

#[test]
fn llm_fields_parse_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n\
          llm_config = \"llm.toml\"\nauto_unlock = true\nallow_plaintext = true\n\
          [[endpoints]]\nlabel = \"lan\"\nhost = \"fd00::1\"\nport = 8787\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    assert_eq!(cfg.llm_config, Some(std::path::PathBuf::from("llm.toml")));
    assert!(cfg.auto_unlock);
}

#[test]
fn hub_tls_fp_is_lowercased_and_attached_to_every_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    let upper = "A".repeat(64);
    std::fs::write(
        &p,
        format!(
            "data_dir = \"./d\"\ndevice_key_path = \"./k\"\nhub_tls_fp = \"{upper}\"\n\
             [[endpoints]]\nlabel = \"lan-ipv6\"\nhost = \"fd00::1\"\nport = 8787\n\
             [[endpoints]]\nlabel = \"lan-ipv4\"\nhost = \"192.0.2.10\"\nport = 8787\n"
        ),
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    let want = "a".repeat(64);
    assert_eq!(cfg.endpoints.len(), 2);
    for e in &cfg.endpoints {
        assert_eq!(e.tls_fp.as_deref(), Some(want.as_str()), "每个端点都必须被钉住：{e:?}");
    }
}

#[test]
fn a_malformed_hub_tls_fp_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    // 空串不是"不钉"：不钉的前提是**根本不写**这个键，写一个空值是配坏了。
    let bad_values = [
        "".to_string(),
        "abc".to_string(),
        "g".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        format!("{} ", "a".repeat(64)),
    ];
    for bad in bad_values {
        std::fs::write(
            &p,
            format!(
                "data_dir = \"./d\"\ndevice_key_path = \"./k\"\nhub_tls_fp = \"{bad}\"\n\
                 [[endpoints]]\nlabel = \"lan\"\nhost = \"fd00::1\"\nport = 8787\n"
            ),
        )
        .unwrap();
        assert!(
            node_cfg::load(&p).is_err(),
            "收下了非法 hub_tls_fp（len={}）：{bad:?}",
            bad.len()
        );
    }
    // 非字符串（这里是整数）同样拒绝。
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\nhub_tls_fp = 1\n\
          [[endpoints]]\nlabel = \"lan\"\nhost = \"fd00::1\"\nport = 8787\n",
    )
    .unwrap();
    assert!(node_cfg::load(&p).is_err(), "hub_tls_fp 必须是一个字符串");
}

#[test]
fn a_config_without_hub_tls_fp_means_plaintext_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n\
          [[endpoints]]\nlabel = \"lan\"\nhost = \"127.0.0.1\"\nport = 8787\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    assert_eq!(cfg.endpoints.len(), 1);
    assert_eq!(cfg.endpoints[0].tls_fp, None, "没写 pin = 明文（既有回环 E2E 不变）");
}

#[test]
fn a_missing_box_does_not_make_the_config_bad() {
    // 配置只点名"去哪儿读"（路径与开关），不点名箱里有没有值：路径指向不存在的
    // `llm.toml`（更别说它背后的箱）时 `load` 仍是 Ok——B2b 才在离线路径读箱，
    // `sync`/`pair`/`status` 不受影响。
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n\
          llm_config = \"absent/llm.toml\"\nauto_unlock = true\nallow_plaintext = true\n\
          [[endpoints]]\nlabel = \"lan\"\nhost = \"fd00::1\"\nport = 8787\n",
    )
    .unwrap();
    assert!(!dir.path().join("absent/llm.toml").exists());
    let cfg = node_cfg::load(&p).unwrap().expect("a missing box is not a config error");
    assert_eq!(cfg.llm_config, Some(std::path::PathBuf::from("absent/llm.toml")));
    assert!(cfg.auto_unlock);
}

/// R1：明文是开发面。非回环 host + 配置无 `hub_tls_fp` + 未写 `allow_plaintext`
/// ⇒ 加载即配置错。错误是**静态句**：地址不是秘密，但错误面没有理由多印一份。
#[test]
fn a_non_loopback_endpoint_without_a_pin_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n\
          [[endpoints]]\nlabel = \"mesh\"\nhost = \"192.0.2.6\"\nport = 8787\n",
    )
    .unwrap();
    let e = node_cfg::load(&p).unwrap_err();
    assert_eq!(
        e, "node.toml endpoint without hub_tls_fp must be loopback (plaintext is dev-only)",
        "错误必须是这一句静态话：{e}"
    );
    assert!(!e.contains("192.0.2.6"), "不许回显地址：{e}");
    // 主机名同样按非回环处理：明文只许在本机，不交给一次解析结果决定。
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n\
          [[endpoints]]\nlabel = \"mesh\"\nhost = \"hub.example\"\nport = 8787\n",
    )
    .unwrap();
    assert!(node_cfg::load(&p).is_err(), "主机名不是回环");
}

/// 显式写 `allow_plaintext = true` 才放行非回环明文；`status.transport` 仍如实报
/// `plaintext`（读数是事实，不是许可）。
#[test]
fn allow_plaintext_lets_a_non_loopback_endpoint_through() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\nallow_plaintext = true\n\
          [[endpoints]]\nlabel = \"mesh\"\nhost = \"192.0.2.6\"\nport = 8787\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("显式接受明文");
    assert!(cfg.allow_plaintext);
    assert_eq!(cfg.endpoints[0].tls_fp, None);
}

/// 同一份非回环地址 + `hub_tls_fp` ⇒ 合法：有 pin 就走 TLS。
#[test]
fn a_non_loopback_endpoint_with_a_pin_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    let pin = "a".repeat(64);
    std::fs::write(
        &p,
        format!(
            "data_dir = \"./d\"\ndevice_key_path = \"./k\"\nhub_tls_fp = \"{pin}\"\n\
             [[endpoints]]\nlabel = \"mesh\"\nhost = \"192.0.2.6\"\nport = 8787\n"
        ),
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("有 pin 就走 TLS");
    assert_eq!(cfg.hub_tls_fp.as_deref(), Some(pin.as_str()));
    assert_eq!(cfg.endpoints[0].tls_fp.as_deref(), Some(pin.as_str()));
}

/// R7.7：发现默认开着（窗口 600ms），并可以被显式关掉。
#[test]
fn discovery_is_on_by_default_and_can_be_switched_off() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(&p, b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\n").unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    assert!(cfg.discovery, "缺省必须开：自动重连不能只靠改代码关");
    assert_eq!(cfg.discovery_timeout_ms, 600);
    assert!(!cfg.allow_plaintext, "缺省不接受非回环明文");

    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\ndiscovery = false\ndiscovery_timeout_ms = 50\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("the config is there");
    assert!(!cfg.discovery);
    assert_eq!(cfg.discovery_timeout_ms, 50, "边界 50 合法");
}

#[test]
fn an_out_of_range_discovery_timeout_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    for bad in [49, 10001] {
        std::fs::write(
            &p,
            format!("data_dir = \"./d\"\ndevice_key_path = \"./k\"\ndiscovery_timeout_ms = {bad}\n"),
        )
        .unwrap();
        assert!(node_cfg::load(&p).is_err(), "越界的窗口必须拒：{bad}");
    }
}

/// R16：发现层只绑 `0.0.0.0`。IPv6 目标在这里就是配置错，
/// 而不是"中心不可达"（后者会被离线窗口读成中心离线）。
#[test]
fn an_ipv6_discovery_target_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("node.toml");
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\ndiscovery_target = \"::1\"\n",
    )
    .unwrap();
    let e = node_cfg::load(&p).unwrap_err();
    assert!(e.contains("IPv4"), "错误要说清只收 IPv4：{e}");
    // 阳性对照：IPv4 目标照常能读。
    std::fs::write(
        &p,
        b"data_dir = \"./d\"\ndevice_key_path = \"./k\"\ndiscovery_target = \"127.0.0.1\"\n",
    )
    .unwrap();
    let cfg = node_cfg::load(&p).unwrap().expect("IPv4 目标合法");
    assert_eq!(cfg.discovery_target.as_deref(), Some("127.0.0.1"));
}
