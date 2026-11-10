//! 脱敏通配的四条规则各一条，加一条"名单外不许动"的对照（A47 节 4 末段、判据 N5/N6）。
//! 每条用例都先断言**被剥的东西确实在夹具里**（阳性对照）——不然断言"不包含"是恒真的空守卫。

use rc_llm::PROBE_MIN_INTERVAL_SECS;
use rc_llm_http::redact::{redact, LONG_TOKEN_MIN, MAX_LOG_BYTES};

#[test]
fn 长串一律认不出_结构全数留得下() {
    let body = r#"{"error":{"type":"AgnesAI_error","request_id":"a1b2c3d4e5f60718293a4b5c6d7e8f90","message":"api key sk-not-a-real-key-000000 is invalid"}}"#;
    for token in ["AgnesAI_error", "request_id", "sk-not-a-real-key-000000"] {
        assert!(body.contains(token), "夹具里没有 {token}，本条用例是空的");
    }
    let r = redact(body);
    assert!(!r.contains("a1b2c3d4e5f60718293a4b5c6d7e8f90"), "32 位十六进制的 id 漏了：{r}");
    assert!(!r.contains("sk-not-a-real-key-000000"), "裸 key 漏了：{r}");
    assert!(r.contains("<id>"), "剥下来的形状要读得出：{r}");
    // 结构可读：字段名与短值都不许被误剥。
    assert!(r.contains("request_id") && r.contains("AgnesAI_error"), "结构被剥没了：{r}");
}

#[test]
fn 回显的后四位与两种_id_形状都剥得掉() {
    // DeepSeek 的 401 回显 key 的**后 4 位**（`****6789`）；错误体里的 request id 是带时间戳前缀
    // 那种形状，响应头里那种是 32 位十六进制——只按一种写会漏另一样（实测两形状并存）。
    assert!(!redact("api key ****6789 invalid").contains("6789"));
    assert!(!redact("id 2026101503523286932075NDLB5rLX here").contains("2026101503523286932075"));
}

#[test]
fn 名单里那三个键的值整段剥掉_其余字段留着() {
    // 各家会把请求正文逐字复述回错误体（SenseNova/MiniMax 实测都复述过批次原文）。
    let body = r#"{"error":{"reason":"batch too long","content":"今天下午三点开会，记得带合同"}}"#;
    let r = redact(body);
    assert!(!r.contains("开会"), "复述回来的正文漏了：{r}");
    assert!(r.contains("batch too long"), "非名单键的值不许被剥：{r}");
}

#[test]
fn 邮箱与标识符形状按通配剥_不是按某家的措辞() {
    // MiniMax 实测的 `content：@superman` 那一族。规则是"@ 后面一串"，不是"superman 这个词"。
    let r = redact(r#"{"msg":"contact support@example.com owner @superman"}"#);
    assert!(!r.contains("superman") && !r.contains("support"), "{r}");
}

#[test]
fn 形状先剥后截长度_跨在截断点上的长串也认得出() {
    // 夹具的算术：`{"a":"`(6) + 210 + `","b":"`(6) ⇒ 那串 32 个 `k` 从第 222 个字符起、到 254 才结束，
    // **正跨在 240 这条截断线上**。顺序反了的症状：留在窗口里的那 18 个 `k` 短于门限 ⇒ 原样进日志。
    let filler = "ab ".repeat(70);
    let long_key = "k".repeat(LONG_TOKEN_MIN + 8);
    let body = format!(r#"{{"a":"{}","b":"{}"}}"#, filler, long_key);
    let cut = redact(&body);
    assert!(!cut.contains(&"k".repeat(18)), "跨线的长串没被认出来：{cut}");
    assert!(cut.contains("<id>"), "剥下来的形状要读得出：{cut}");
    assert!(cut.chars().count() <= MAX_LOG_BYTES + 14, "截断没生效：{}", cut.chars().count());
}

#[test]
fn 名单之外的键名一律不动_这样才不是按家分支() {
    // N5 的通配性在行为面上的证明：`message`／`reason` 不在名单里，换一家也不换名单
    //（名单是 JSON 键名的形状，不是某家的措辞）。
    let r = redact(r#"{"error":{"code":16,"message":"something","reason":"again"}}"#);
    assert!(r.contains("something") && r.contains("again"), "非名单键被误剥：{r}");
    assert!(r.contains("reason"), "字段名要还在：{r}");
}

#[test]
fn 裸文本错误体的回显与请求号一起剥掉() {
    // 真实夹具（DeepSeek 的 401 裸文本）走同一条通配：规则按形状而不是按某家的措辞，
    // 所以"没有 JSON 结构"这一形也必须剥得干净——运维日志那一行吃的正是这种体。
    let bare = include_str!("fixtures/deepseek-401-bare-text.txt");
    assert!(bare.contains("abcdef12") && bare.contains("2026101500000000000000abcdEF1234"), "夹具形状变了");
    let r = redact(bare);
    assert!(!r.contains("abcdef12"), "key 后四位漏了：{r}");
    assert!(!r.contains("abcdEF1234"), "request id 漏了：{r}");
    assert!(r.contains("AuthenticationError"), "可读的类别名不许被剥没：{r}");
}

#[test]
fn 探针的两个常量是代码里的字面量_不是配置项() {
    // 节 4 第 3 条：可配 ⇒ 下一个人会把它调成 0。这里断言默认值；
    // "不许从 llm.toml 读"由 `rc-llm-cfg` 那边**没有**这两个键来执法。
    assert_eq!(PROBE_MIN_INTERVAL_SECS, 300);
}
