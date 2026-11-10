//! `llm.toml` 的读取与校验（A47 节 3）。
//!
//! 这一族用例的共同主题：**坏配置必须在出网之前拒掉，且拒的消息不能把坏值原样带出去**。

use rc_llm_cfg::{keys, load, Config, Seat};
use std::path::Path;

fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join("llm.toml");
    std::fs::write(&path, body).unwrap();
    path
}

fn err_at(dir: &tempfile::TempDir, body: &str) -> String {
    match load(&write(dir, body)) {
        Err(e) => e,
        Ok(other) => panic!("expected a rejection, got {other:?}"),
    }
}

/// 缺文件 = "这台机器还没轮到配 live"，回放照跑；它与"配坏了"是两种后果，不许合并。
#[test]
fn 缺文件是_没配_而不是_配坏了() {
    let dir = tempfile::tempdir().unwrap();
    let missing: &Path = &dir.path().join("llm.toml");
    assert!(matches!(load(missing), Ok(None)));
}

/// 最小完整形状：一项主 provider（preset 表的键）+ 一项 custom 通道。
/// 顺序必须留着——它就是 fallback 链的顺序。
#[test]
fn 有序列表原样保留_主家在第一项() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = load(&write(
        &dir,
        r#"
[[providers]]
provider = "agnes"
secret_ref = "kms:agnes_key"

[[providers]]
model = "qwen3-local"
secret_ref = "env:LOCAL_KEY"
[providers.endpoint]
base_url = "http://127.0.0.1:11434/v1"
protocol = "openai-chat"
"#,
    ))
    .unwrap()
    .expect("configured");
    let Config { seats } = &cfg;
    assert_eq!(seats.len(), 2);
    assert_eq!(seats[0].provider.as_deref(), Some("agnes"));
    assert!(seats[0].endpoint.is_none());
    assert_eq!(seats[1].provider, None);
    assert_eq!(seats[1].model.as_deref(), Some("qwen3-local"));
    assert_eq!(seats[1].secret_ref, "env:LOCAL_KEY");
    let ep = seats[1].endpoint.as_ref().unwrap();
    assert_eq!(ep.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(ep.protocol, "openai-chat");
}

/// `providers` 是列表这一事实由节 3 钉着：缺这一格 ⇒ 响亮失败，不许当成"空列表"再往下走。
#[test]
fn 没有_providers_数组_与_空数组_都是失败() {
    let dir = tempfile::tempdir().unwrap();
    assert!(err_at(&dir, "").contains("no `providers` array"));
    assert!(err_at(&dir, "providers = []").contains("list is empty"));
}

/// 非法 TOML 与非法形状分开报：前者修语法、后者修座位，混成一条会让人改错地方。
#[test]
fn 语法错误报语法() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(err_at(&dir, "[[providers\nprovider = \"x\""), "llm.toml is not valid TOML");
}

/// provider 与 endpoint 二选一、互斥且必居其一：**两个都给**是最危险的一形
/// （它会静默用掉 preset 那一行而把端点那一段当注释），所以它必须报错而不是取第一个。
#[test]
fn 一个座位只能是一种通道() {
    let dir = tempfile::tempdir().unwrap();
    let both = r#"
[[providers]]
provider = "agnes"
secret_ref = "kms:agnes_key"
[providers.endpoint]
base_url = "http://127.0.0.1:1"
protocol = "openai-chat"
"#;
    assert!(err_at(&dir, both).contains("not both"));
    assert!(err_at(&dir, "[[providers]]\nsecret_ref = \"kms:a\"\n").contains("not both"));
}

/// custom 那一侧没有 preset 行可查 ⇒ 模型名只能来自配置，缺它就只能带着默认名出网。
#[test]
fn custom_座位必须自带模型名() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"
[[providers]]
secret_ref = "env:LOCAL_KEY"
[providers.endpoint]
base_url = "http://127.0.0.1:11434/v1"
protocol = "openai-chat"
"#;
    assert!(err_at(&dir, body).contains("needs `model`"));
}

/// 协议名逐字（`openai-chat` / `anthropic-messages`）：拼错不许回落默认（节 1 那条接缝）。
#[test]
fn 协议名只许那两个且逐字() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"
[[providers]]
model = "m"
secret_ref = "env:K"
[providers.endpoint]
base_url = "http://127.0.0.1:1"
protocol = "OpenAI-Chat"
"#;
    assert!(err_at(&dir, body).contains("unknown protocol name"));
    assert!(keys::check_protocol("anthropic-messages").is_ok());
}

/// **`secret_ref` 用 `:` 不用 `/`（节 3 那条推论的落点）**：
/// A43 那句"挡 `openai/../../..`"要落成码，判据就是这一条。
#[test]
fn 斜杠形状的_secret_ref_一律拒() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        "openai/../../..",
        "kms/agnes_key",
        "kms",
        "kms:",
        ":agnes_key",
        "kms:ag nes",
        "kms:agnes_key:extra",
    ] {
        let body = format!("[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"{bad}\"\n");
        let e = err_at(&dir, &body);
        assert!(e.contains("secret_ref"), "{bad} ⇒ {e}");
    }
    // 反斜杠走 TOML 的字面量字符串（单引号），否则它先在语法那一层就死了。
    let bs = err_at(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nsecret_ref = 'kms:agnes\\key'\n",
    );
    assert!(bs.contains("secret_ref"), "{bs}");
    assert!(keys::check_secret_ref("kms:agnes_key").is_ok());
    assert!(keys::check_secret_ref("env:LOCAL_KEY").is_ok());
    assert!(keys::check_secret_ref("box:a.b-c:d_e").is_err(), "两个冒号分三段");
}

/// 缺 `secret_ref` 的座位：这一格不能省——省了就等于"去箱子里随便拿一把"。
#[test]
fn 缺_secret_ref_直接拒() {
    let dir = tempfile::tempdir().unwrap();
    assert!(err_at(&dir, "[[providers]]\nprovider = \"agnes\"\n").contains("no `secret_ref`"));
}

/// 被拒的值**不许原样进消息**，且死在哪一条要说清（两条分支的分工是阳性对照）：
/// 整串没有 `:` ⇒ "形状"那一支，消息里根本不含值；半边有斜杠 ⇒ "safe token"那一支，
/// 消息里必须是 `(redacted)`。这一条防的是"校验器自己变成一条下行路径"——被拦下的正是长成路径的串。
#[test]
fn 拒值的消息只带脱敏形状与序号() {
    let dir = tempfile::tempdir().unwrap();
    let no_colon = "openai/../../etc/passwd";
    let e = err_at(&dir, &seat_with(no_colon));
    assert!(!e.contains(no_colon) && !e.contains("passwd"), "原串泄漏进消息：{e}");
    assert!(e.contains("box:slot"), "该报形状而不是别的：{e}");
    assert!(e.contains("providers[0]"), "序号是唯一的定位：{e}");

    let half = "kms:openai/../../etc/passwd";
    let e = err_at(&dir, &seat_with(half));
    assert!(e.contains("(redacted)"), "脱敏占位必须在：{e}");
    assert!(!e.contains(half) && !e.contains("passwd"), "半边串也不许留：{e}");
    assert!(!e.contains("openai"), "箱名以外的半边同样不许留：{e}");

    // 阳性对照两件：合法值绝不会被这条规则误伤，且序号跟着位置走
    //（第一条好、第二条坏 ⇒ 报的是 [1] 不是 [0]）。
    let good_then_bad = format!(
        "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:agnes_key\"\n{}",
        seat_with(no_colon)
    );
    let e2 = err_at(&dir, &good_then_bad);
    assert!(e2.contains("providers[1]"), "{e2}");
    assert!(!e2.contains(no_colon), "原串泄漏进消息：{e2}");
}

fn seat_with(secret_ref: &str) -> String {
    format!("[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"{secret_ref}\"\n")
}

/// `Seat` 的字段集**恰好**是"位置/参数集"：这一格是形状自检，字面量必须穷举所有字段。
/// 下一个人若在这里加一个 `api_key: String`（把凭据的值装进配置对象），本用例编译失败。
#[test]
fn seat_的字段集就是位置集() {
    let seat = Seat {
        provider: Some("agnes".to_string()),
        model: None,
        secret_ref: "kms:agnes_key".to_string(),
        endpoint: None,
        budget: None,
        derive_budget: None,
        temperature: None,
        reasoning_effort: None,
    };
    assert!(format!("{seat:?}").contains("kms:agnes_key"), "位置要读得出来");
}

/// 单座预算覆盖：缺省是 `None`（走 32_000），显式值时必须是正整数。
#[test]
fn seat_budget_is_optional_and_must_be_positive() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = load(&write(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nmodel = \"agnes-3.0-flash\"\nsecret_ref = \"kms:agnes_key\"\nbudget = 524288\n",
    ))
    .unwrap()
    .expect("configured");
    assert_eq!(cfg.seats[0].budget, Some(524_288));

    let zero = err_at(&dir, "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\nbudget = 0\n");
    assert!(zero.contains("positive u32"), "{zero}");
    let wrong = err_at(&dir, "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\nbudget = \"big\"\n");
    assert!(wrong.contains("must be an integer"), "{wrong}");

    let tuned = load(&write(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:agnes_key\"\nderive_budget = 524288\n",
    ))
    .unwrap()
    .expect("configured");
    assert_eq!(tuned.seats[0].derive_budget, Some(524_288));
    assert_eq!(tuned.seats[0].budget, None);
    let bad = err_at(&dir, "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\nderive_budget = 0\n");
    assert!(bad.contains("derive_budget"), "{bad}");
}

/// 温度是座位的一格：写了就原样读出来，不写就是 `None`（provider 默认）——**不许补成 0.0**，
/// 那条静默会把"这台机器没配温度"变成一次行为改变。
#[test]
fn a_seat_may_carry_a_temperature() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = load(&write(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nmodel = \"agnes-3.0-flash\"\n\
         secret_ref = \"kms:agnes_key\"\ntemperature = 0.0\n",
    ))
    .unwrap()
    .expect("configured");
    assert_eq!(cfg.seats[0].temperature, Some(0.0));

    let unset = load(&write(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nmodel = \"agnes-3.0-flash\"\n\
         secret_ref = \"kms:agnes_key\"\n",
    ))
    .unwrap()
    .expect("configured");
    assert_eq!(unset.seats[0].temperature, None, "没配就是 None（provider 默认），不是 0.0");

    // 上界与下界按 `0.0..=2.0` 闭区间收（边界值单独钉一次，免得有人把 `<=` 写成 `<`）。
    for edge in ["0.0", "2.0"] {
        let ok = load(&write(
            &dir,
            &format!(
                "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\ntemperature = {edge}\n"
            ),
        ))
        .unwrap()
        .expect("boundary is legal");
        assert!(ok.seats[0].temperature.is_some(), "{edge} 在值域内被拒了");
    }
}

/// 越界与 `budget` 同一条处理方式：配置错，带序号，**不静默截到最近的合法值**。
#[test]
fn an_out_of_range_temperature_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["3.0", "-0.1"] {
        let e = err_at(
            &dir,
            &format!(
                "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\ntemperature = {bad}\n"
            ),
        );
        assert!(e.contains("temperature"), "{bad} ⇒ {e}");
        assert!(e.contains("providers[0]"), "序号是唯一的定位：{e}");
        assert!(!e.contains(bad), "坏值原样进消息了：{e}");
    }
}

/// 类型错也走坏配置那条路（`"hot"` 不许被当成"没配"而静默回落 provider 默认）。
#[test]
fn a_non_numeric_temperature_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let e = err_at(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\ntemperature = \"hot\"\n",
    );
    assert!(e.contains("temperature"), "{e}");
    assert!(e.contains("providers[0]"), "{e}");
}

/// 思考档位：缺省是 None（不发这一格）；`none` 是 SenseNova 的"关思考"，属于闭集；
/// `disabled`/大小写漂移仍是配置事故。
#[test]
fn reasoning_effort_is_optional_and_closed_set() {
    let dir = tempfile::tempdir().unwrap();
    let ok = load(&write(
        &dir,
        "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\nreasoning_effort = \"low\"\n",
    ))
    .expect("闭集内必须读得出来").expect("文件存在");
    assert_eq!(ok.seats[0].reasoning_effort.as_deref(), Some("low"));

    let none = load(&write(
        &dir,
        "[[providers]]\nprovider = \"sensenova\"\nmodel = \"sensenova-6.8-flash-lite\"\nsecret_ref = \"kms:a\"\nreasoning_effort = \"none\"\n",
    ))
    .expect("`none`（关思考）必须可读").expect("文件存在");
    assert_eq!(none.seats[0].reasoning_effort.as_deref(), Some("none"));

    let unset = load(&write(&dir, &seat_with("kms:a"))).expect("没配必须仍可读").expect("文件存在");
    assert_eq!(unset.seats[0].reasoning_effort, None, "没配就是 None，不是 low/max");

    for bad in ["disabled", "LOW"] {
        let e = err_at(
            &dir,
            &format!(
                "[[providers]]\nprovider = \"agnes\"\nsecret_ref = \"kms:a\"\nreasoning_effort = \"{bad}\"\n"
            ),
        );
        assert!(e.contains("reasoning_effort"), "{bad} ⇒ {e}");
        assert!(e.contains("providers[0]"), "{e}");
    }
}
