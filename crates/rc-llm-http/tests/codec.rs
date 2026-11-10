//! codec + Live 的判据（A47 节 5）。零网络：出网点是计数桩。
//!
//! 响应体一律用 `tests/fixtures/` 下的**真实形状**（各家实测抓回来的骨架，键序、旁枝字段、
//! `usage` 明细都留着；零真 key、零批次原文）。手写小 JSON 只能证明我的解析器认识我以为的形状，
//! 证明不了它认识厂家发的形状——这一批的三处判据改错（`content` 整个键不见、`length` + 半句、
//! `stop` + 空正文）全是只有真实形状才暴露得出来的。

use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_llm_http::codec::{build_body, classify, Verdict};
use rc_llm_http::preset::{
    custom_shape, custom_target, find, BudgetField, ErrorBody, Preset, Protocol, ReasoningKey, ROWS,
};
use rc_llm_http::{Egress, Live, Raw, MEASURED_CEILINGS, MAX_BUDGET, MAX_SENDS_PER_CALL};
use std::cell::RefCell;
use std::rc::Rc;

/// 每次真实发出的 `(url, 鉴权头)` 一对（别名只为过 clippy 的 `type_complexity`）。
type Calls = Vec<(String, Vec<(String, String)>)>;


const REPLY_OK: &str = include_str!("fixtures/openai-chat-200-reply.json");
const TOOL_CALLS: &str = include_str!("fixtures/agnes-200-tool-calls.json");
const HALF_SENTENCE: &str = include_str!("fixtures/agnes-200-length-half-sentence.json");
const NO_CONTENT_KEY: &str = include_str!("fixtures/sensenova-200-length-without-content.json");
const MINIMAX_INBAND_FAIL: &str = include_str!("fixtures/minimax-200-base-resp-failure.json");
const MINIMAX_EMPTY_AT_STOP: &str = include_str!("fixtures/minimax-200-stop-empty-content.json");
const M3_SPLIT_REPLY: &str = include_str!("fixtures/minimax-m3-200-split-reply.json");
const AGNES_MODEL_NOT_FOUND: &str = include_str!("fixtures/agnes-503-model-not-found.json");
const DEEPSEEK_BUSY_503: &str = include_str!("fixtures/deepseek-503-busy-fast.json");
const DEEPSEEK_401_BARE: &str = include_str!("fixtures/deepseek-401-bare-text.txt");
const GATEWAY_504_HTML: &str = include_str!("fixtures/gateway-504-html.html");
const ANTHROPIC_REPLY: &str = include_str!("fixtures/anthropic-messages-200-reply.json");

#[derive(Clone)]
struct Scripted {
    replies: Rc<RefCell<Vec<(u16, String, i64)>>>,
    bodies: Rc<RefCell<Vec<String>>>,
    /// 桩记下来的每次发出。
    calls: Rc<RefCell<Calls>>,
}

impl Scripted {
    fn new(replies: Vec<(u16, String, i64)>) -> Scripted {
        Scripted {
            replies: Rc::new(RefCell::new(replies)),
            bodies: Rc::new(RefCell::new(Vec::new())),
            calls: Rc::new(RefCell::new(Vec::new())),
        }
    }
    fn bodies(&self) -> Vec<String> {
        self.bodies.borrow().clone()
    }
    /// 最后一次真实发出的 `(url, headers, body)`。custom 通道那两条用例判的就是这一组，
    /// 因为那一侧唯一的新东西是"地址从配置里来"。
    fn last_call(&self) -> (String, Vec<(String, String)>, String) {
        let calls = self.calls.borrow();
        let bodies = self.bodies.borrow();
        let (url, headers) = calls.last().expect("一次调用都没发生");
        (url.clone(), headers.clone(), bodies.last().unwrap().clone())
    }
}

impl Egress for Scripted {
    fn post(&self, url: &str, headers: &[(String, String)], body: &str) -> Result<Raw, LlmFailure> {
        assert!(url.ends_with("/chat/completions") || url.ends_with("/messages"), "{url}");
        assert!(!headers.is_empty(), "鉴权头必须随协议走");
        self.calls.borrow_mut().push((url.to_string(), headers.to_vec()));

        self.bodies.borrow_mut().push(body.to_string());
        let (status, text, slow) = self
            .replies
            .borrow_mut()
            .drain(..1)
            .next()
            .unwrap_or_else(|| panic!("桩脚本只有 {} 条响应", MAX_SENDS_PER_CALL));
        Ok(Raw { status, body: text, slow_ms: slow })
    }
}

fn prompt() -> Prompt {
    Prompt { system: "S".to_string(), user: "U".to_string(), derive: false }
}

/// 表上的行的**形状**（classify 与 build_body 只要形状；地址在 `Live` 那一侧）。
fn shape(provider: &str, slug: &str) -> &'static Preset {
    find(provider, slug).expect("preset 表里必须有这一行").shape
}

fn live(provider: &str, model: &str, replies: Vec<(u16, String, i64)>) -> (Live, Scripted) {
    let stub = Scripted::new(replies);
    let backend = Live::new(
        provider,
        model,
        "sk-test-key-value-must-not-appear".to_string(),
        100,
        None,
        Box::new(stub.clone()),
    )
    .expect("preset 表里有的行必须构造得起来");
    (backend, stub)
}

fn ok_json(text: &str) -> String {
    format!(r#"{{"choices":[{{"message":{{"content":"{text}"}},"finish_reason":"stop"}}]}}"#)
}

fn truncated_empty() -> String {
    r#"{"choices":[{"message":{"content":"","role":"assistant"},"finish_reason":"length"}]}"#.to_string()
}

// ---- 三条成功判据各走各的手段（节 4） ----

#[test]
fn truncated_raises_the_budget_and_resends() {
    let (backend, stub) =
        live("agnes", "agnes-3.0-flash", vec![(200, truncated_empty(), 900), (200, REPLY_OK.into(), 900)]);
    assert!(backend.complete(&prompt()).unwrap().contains("t-a1"));
    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("\"max_tokens\":100"), "{}", bodies[0]);
    assert!(bodies[1].contains("\"max_tokens\":200"), "抬预算那一族只认抬预算：{}", bodies[1]);
}

/// SenseNova 实测形状：预算被思考吃满时 `content` **整个键不见**（`message` 只有
/// `{role, reasoning}`）。把它读成"落点读不出"⇒ 判据 1 在这条形上永远不触发，
/// 抬预算那一支变成死路，症状却是"这一家偶尔回 Malformed"（不换家、不前进）。
#[test]
fn a_response_without_a_content_key_still_raises_the_budget() {
    // 阳性对照：夹具里 `content` 这个键确实不存在，否则这条用例是空的。
    assert!(!NO_CONTENT_KEY.contains("\"content\""), "夹具已经带 content 了");
    assert!(NO_CONTENT_KEY.contains("\"finish_reason\": \"length\""));
    let (backend, stub) = live(
        "sensenova",
        "sensenova-6.8-flash-lite",
        vec![(200, NO_CONTENT_KEY.into(), 900), (200, REPLY_OK.into(), 900)],
    );
    // 重发这一次用的是同一形状的另一条路：`complete()` 交出的是正文，不是 reasoning。
    let reply = backend.complete(&prompt()).unwrap();
    assert_eq!(stub.bodies().len(), 2, "必须走到抬预算重发");
    assert!(stub.bodies()[1].contains("\"max_completion_tokens\":200"));
    assert!(!reply.contains("budget is gone"), "思考内容漏进正文：{reply}");
}

/// Agnes 实测形状：`max_tokens` 小于一个词 ⇒ `finish_reason=length` + **半句**正文（"Hel"）。
/// 半句不是非法 JSON 那一族的"逃逸"：拿同一个预算原样重发，回来的还是同一个半句，白烧一次出网。
/// （若判据 1 还带"且正文为空"那个条件，这一条形会落进判据 2 ⇒ 两次 body 相同 ⇒ 本条红。）
#[test]
fn a_half_sentence_at_length_is_truncation_not_an_escape() {
    let (backend, stub) = live(
        "agnes",
        "agnes-3.0-flash",
        vec![(200, HALF_SENTENCE.into(), 900), (200, REPLY_OK.into(), 900)],
    );
    assert!(backend.complete(&prompt()).unwrap().contains("t-a1"));
    let bodies = stub.bodies();
    assert_eq!(bodies.len(), 2);
    assert_ne!(bodies[0], bodies[1], "截断那一族必须**改预算**重发，不是原样重发");
    assert!(bodies[1].contains("\"max_tokens\":200"), "{}", bodies[1]);
}

/// MiniMax 实测形状：`finish_reason=stop`、`base_resp.status_code=0`（业务成功）、`output_sensitive=true`
/// 而正文是空串。判据 3 必须排在判据 2 **前面**：空串本身不是合法 JSON，反过来排会把它读成
/// "瞬时契约逃逸"⇒ 原样重发一次永远不会变的东西（而这一族的真相是涉敏拒答，换家也一样）。
#[test]
fn an_empty_reply_at_stop_is_neither_an_escape_nor_an_answer() {
    let (backend, stub) = live("minimax", "MiniMax-M3", vec![(200, MINIMAX_EMPTY_AT_STOP.into(), 900)]);
    assert_eq!(backend.complete(&prompt()).unwrap_err(), LlmFailure::SchemaMismatch);
    assert_eq!(stub.bodies().len(), 1, "脱约不许重发：一次都没第二次");
}

#[test]
fn escape_resends_identically_without_raising_the_budget() {
    let (backend, stub) =
        live("agnes", "agnes-3.0-flash", vec![(200, ok_json("not json at all"), 900), (200, REPLY_OK.into(), 900)]);
    assert!(backend.complete(&prompt()).unwrap().contains("t-a1"));
    let bodies = stub.bodies();
    assert_eq!(bodies[0], bodies[1], "瞬时契约逃逸必须**原样**重发，预算一个字都不许动");
}

#[test]
fn tool_channel_is_not_read_as_an_empty_answer() {
    // 这一条是守卫 8 那一族（假空）的形状：`tool_calls` 时 content 是空串，
    // 把它当"库里没有"就等于替用户判了"这条不存在"。
    assert_eq!(
        classify(shape("agnes", "agnes-3.0-flash"), 200, TOOL_CALLS),
        Verdict::ToolChannel,
        "真实夹具：正文在 `tool_calls` 那个第二个落点"
    );
    let (backend, stub) = live("agnes", "agnes-3.0-flash", vec![(200, TOOL_CALLS.into(), 900)]);
    let e = backend.complete(&prompt()).unwrap_err();
    assert_eq!(e, LlmFailure::SchemaMismatch, "工具通道不接线 ⇒ 脱约，且**不换家**");
    assert_eq!(stub.bodies().len(), 1, "工具通道也不是重发的手段");
}

#[test]
fn sends_are_bounded_so_a_broken_provider_cannot_loop() {
    // 桩里**多喂一条**：上限被拆掉时症状是"发出数 = 3"，而不是"桩脚本用完了"那个 panic。
    // （自指地按 `MAX_SENDS_PER_CALL` 喂，等于让被测常量自己当判据 —— 那是空过的守卫。）
    let replies = vec![(200u16, truncated_empty(), 900i64); MAX_SENDS_PER_CALL + 1];
    let (backend, stub) = live("agnes", "agnes-3.0-flash", replies);
    assert!(backend.complete(&prompt()).is_err());
    assert_eq!(stub.bodies().len(), MAX_SENDS_PER_CALL, "一次 complete() 最多发这么多个请求");
}

// ---- 本地校验先于出网（节 2 实现律③） ----

#[test]
fn temperature_outside_domain_fails_before_any_send() {
    let stub = Scripted::new(vec![]);
    let e = match Live::new("sensenova", "kimi-k3", "k".into(), 100, Some(0.0), Box::new(stub.clone())) {
        Ok(_) => panic!("kimi-k3 只许 temperature=1，构造点必须拒"),
        Err(e) => e,
    };
    assert!(e.contains("temperature"), "{e}");
    assert_eq!(stub.bodies().len(), 0, "\"返回了错误\"不算有牙：必须一次都没发出");
}

#[test]
fn unknown_preset_key_fails_loudly_and_sends_nothing() {
    let stub = Scripted::new(vec![]);
    assert!(Live::new("agnes", "agnes-9.9-nonexistent", "k".into(), 100, None, Box::new(stub.clone())).is_err());
    assert_eq!(stub.bodies().len(), 0);
}

// ---- 503 二因：同码两判，可读的见证只有带内 code ----

#[test]
fn two_503s_from_one_provider_split_on_inband_code() {
    let p = shape("agnes", "agnes-3.0-flash");
    // 两条都是从 Agnes 实测到的体形状（前者即夹具 `agnes-503-model-not-found.json`）。
    let busy = r#"{"error":{"code":"server_error","message":"Upstream service is busy","type":"server_error"}}"#;
    assert_eq!(classify(p, 503, AGNES_MODEL_NOT_FOUND), Verdict::Failed(LlmFailure::SchemaMismatch));
    assert_eq!(classify(p, 503, busy), Verdict::Failed(LlmFailure::BackendError));
    // 状态码相同、带内 code 不同 ⇒ 结论不同：除了 code 没有第二样可读的东西能分出这一判。
}

/// **这一条钉住一次就地更正**（`codec::classify` 的注释给了收据）：原先按"往返低于阈值"分 503
/// 那一判，实测把它推翻——DeepSeek 的 503 是**常态**（六次采样一半是 503）、0.1–0.6 秒立回、
/// JSON 体、`error.code` 非空 ⇒ 三个形状条件全中。按形状判会把这一家一半的正常抖动
/// 读成"配置写错模型名"，于是不前进、还报成"回复形状不符"。
/// 判据改完之后 `classify` 的签名里**没有**时延参数——这一条用例同时是那个签名的守卫。
#[test]
fn a_fast_json_503_with_a_nonempty_code_is_the_provider_being_busy() {
    assert!(DEEPSEEK_BUSY_503.contains("\"code\": \"server_error\""), "夹具里没有可读 code");
    assert_eq!(
        classify(shape("deepseek", "deepseek-chat"), 503, DEEPSEEK_BUSY_503),
        Verdict::Failed(LlmFailure::BackendError),
        "必须可前进"
    );
}

// ---- provider 侧额度与凭据绝不冒充本地闸门 ----

#[test]
fn provider_side_quota_never_becomes_quota_exhausted() {
    let p = shape("agnes", "agnes-3.0-flash");
    for status in [401u16, 402, 403, 429] {
        let v = classify(p, status, r#"{"error":{"code":"","message":"quota"}}"#);
        assert_eq!(v, Verdict::Failed(LlmFailure::BackendError), "{status} 必须能触发换家");
        assert_ne!(v, Verdict::Failed(LlmFailure::QuotaExhausted), "两本账不许互冒充");
    }
}

#[test]
fn non_retryable_status_does_not_advance() {
    let p = shape("deepseek", "deepseek-chat");
    assert_eq!(
        classify(p, 400, r#"{"error":{"code":"1001"}}"#),
        Verdict::Failed(LlmFailure::SchemaMismatch),
        "400 不在 retryable 名单里 ⇒ 这个请求本身坏了，换家也一样"
    );
    assert_eq!(
        classify(p, 404, "404 page not found"),
        Verdict::Failed(LlmFailure::SchemaMismatch),
        "404 同理（slug 大小写被归一那一族的落点）"
    );
}

// ---- 带内业务失败：200 不是成功 ----

#[test]
fn in_band_failure_at_200_is_not_a_reply() {
    let p = shape("minimax", "MiniMax-M3");
    assert_eq!(classify(p, 200, MINIMAX_INBAND_FAIL), Verdict::Failed(LlmFailure::BackendError), "1002 = 限流，换家可能成立");
    // 覆盖名单里的 code（涉敏）：换家也一样 ⇒ 不前进。
    let sensitive = r#"{"base_resp":{"status_code":1026,"status_msg":"risk"}}"#;
    assert_eq!(
        classify(p, 200, sensitive),
        Verdict::Failed(LlmFailure::SchemaMismatch),
        "1026 在 inband_code_overrides 里"
    );
}

// ---- 错误体三形都不 panic ----

#[test]
fn html_and_bare_text_bodies_are_both_normalized() {
    let ss = shape("sensenova", "sensenova-6.8-flash-lite");
    let ds = shape("deepseek", "deepseek-chat");
    // 三形各归各家：网关 HTML（SenseNova 的 504）、裸文本（DeepSeek 缺/坏 `Authorization`）、标准 JSON。
    assert_eq!(classify(ss, 504, GATEWAY_504_HTML), Verdict::Failed(LlmFailure::BackendError));
    assert_eq!(classify(ds, 401, DEEPSEEK_401_BARE), Verdict::Failed(LlmFailure::BackendError));
    assert_eq!(classify(ss, 500, r#"{"error":{"code":16}}"#), Verdict::Failed(LlmFailure::BackendError));
    // 裸文本没有可读 code ⇒ 只能按状态码走；401 在名单里（一家凭据不可用就是这家不可用）。
    assert!(DEEPSEEK_401_BARE.contains("****abcdef12"), "夹具必须带 key 尾数，否则下面那条 redact 用例是空的");
}

// ---- 思考键：缺失与 null 是两件事（我误报过一次），且思考永不进正文 ----

#[test]
fn missing_reasoning_key_differs_from_null_reasoning_key() {
    let p = shape("deepseek", "deepseek-chat");
    let absent: serde_json::Value = serde_json::from_str(
        r#"{"choices":[{"message":{"content":"{}"},"finish_reason":"stop"}]}"#)
        .unwrap();
    let nullv: serde_json::Value = serde_json::from_str(
        r#"{"choices":[{"message":{"content":"{}","reasoning_content":null},"finish_reason":"stop"}]}"#)
        .unwrap();
    assert!(!rc_llm_http::codec::has_reasoning_key(p, &absent), "键不存在");
    assert!(rc_llm_http::codec::has_reasoning_key(p, &nullv), "键存在且为 null —— 与上面不是一件事");
}

/// 两种协议各一条：思考**在场**（阳性对照用 `has_reasoning_key` 证）而返回值里没有它的一个字节。
/// SenseNova/MiniMax 实测会把批次原文逐字复述进思考里，所以"不许进 agent 面"这条不是洁癖。
#[test]
fn reasoning_never_leaves_through_the_reply() {
    let ds = shape("deepseek", "deepseek-chat");
    let with_rc = r#"{"choices":[{"message":{"content":"{}","role":"assistant","reasoning_content":"SECRET CHAIN"},"finish_reason":"stop"}]}"#;
    let doc: serde_json::Value = serde_json::from_str(with_rc).unwrap();
    assert!(rc_llm_http::codec::has_reasoning_key(ds, &doc), "夹具必须带思考键");
    match classify(ds, 200, with_rc) {
        Verdict::Reply(text) => {
            assert_eq!(text, "{}");
            assert!(!text.contains("SECRET"), "思考漏进返回值：{text}");
        }
        other => panic!("openai 形带 reasoning_content 的合法回复判成了 {other:?}"),
    }
    let an = custom_shape(Protocol::AnthropicMessages);
    assert!(rc_llm_http::codec::has_reasoning_key(an, &serde_json::from_str::<serde_json::Value>(ANTHROPIC_REPLY).unwrap()));
    match classify(an, 200, ANTHROPIC_REPLY) {
        Verdict::Reply(text) => {
            assert_eq!(text, r#"{"rows":[]}"#);
            assert!(!text.contains("hidden chain"), "thinking 块漏进返回值：{text}");
        }
        other => panic!("Anthropic 形的 text 块判成了 {other:?}"),
    }
}

// ---- 协议差异：鉴权头、路径、system 落点（实现律①②） ----

#[test]
fn anthropic_protocol_shapes_the_request_differently() {
    // custom 通道才有 Anthropic 形的行（表上四行全走 openai-chat），所以这里同时验两条通道的接缝。
    let an = custom_shape(Protocol::AnthropicMessages);
    let target = custom_target("http://127.0.0.1:11434", "claude-shaped-local");
    let body = build_body(an, &target, &prompt(), 256, None, None).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["system"], serde_json::json!("S"), "Anthropic 形里 system 是顶层字段");
    assert_eq!(v["messages"].as_array().unwrap().len(), 1, "user 一条，不许有 system 消息");
    assert_eq!(v["model"], serde_json::json!("claude-shaped-local"), "模型名逐字来自配置");
    let headers = Protocol::AnthropicMessages.auth_headers("kkk");
    assert!(headers.iter().any(|(k, _)| k == "x-api-key"));
    assert!(headers.iter().any(|(k, v2)| k == "anthropic-version" && v2 == "2023-06-01"));
    assert_eq!(Protocol::AnthropicMessages.path(), "/messages");

    // 整条通道走一遍：URL 是 base_url + 协议路径拼出来的，鉴权头逐字来自协议。
    let stub = Scripted::new(vec![(200, ANTHROPIC_REPLY.into(), 40)]);
    let b = Live::new_custom(
        "http://127.0.0.1:11434",
        "anthropic-messages",
        "claude-shaped-local",
        "kkk".into(),
        256,
        None,
        Box::new(stub.clone()),
    )
    .expect("custom 通道必须构造得起来");
    assert_eq!(b.complete(&prompt()).unwrap(), r#"{"rows":[]}"#);
    let (url, headers, _) = stub.last_call();    assert_eq!(url, "http://127.0.0.1:11434/messages");
    assert!(headers.iter().any(|(k, v2)| k == "x-api-key" && v2 == "kkk"), "{headers:?}");
}

/// 实现律⑤⑥的**行为面**证明：这两个取值若是硬编码的 `"length"`，Anthropic 形的截断会在
/// 判据 1 之前先被白名单打成 `Malformed` ⇒ 抬预算那一支在第二条协议上是死路，而这一条会红。
#[test]
fn the_truncation_and_tool_values_are_protocol_laws() {
    assert_eq!(Protocol::OpenAiChat.truncated_finish(), "length");
    assert_eq!(Protocol::AnthropicMessages.truncated_finish(), "max_tokens");
    assert_eq!(Protocol::OpenAiChat.tool_finish(), "tool_calls");
    assert_eq!(Protocol::AnthropicMessages.tool_finish(), "tool_use");

    let an = custom_shape(Protocol::AnthropicMessages);
    assert_eq!(
        classify(an, 200, r#"{"content":[{"type":"text","text":"{}"}],"stop_reason":"max_tokens"}"#),
        Verdict::Truncated,
        "Anthropic 侧的预算吃满必须走抬预算那一支"
    );
    // 反向：openai 形里 `max_tokens` 不是合法的 `finish_reason` ⇒ 白名单外，脱约。
    assert_eq!(
        classify(shape("deepseek", "deepseek-chat"), 200, r#"{"choices":[{"message":{"content":"{}"},"finish_reason":"max_tokens"}]}"#),
        Verdict::Malformed
    );
    // `tool_use` 不进 Anthropic 白名单（这一批不接工具通道）：它落在 `Malformed`，
    // 与 openai 形 `ToolChannel` 的上抛结果同族（都是 `SchemaMismatch`、都不换家）。
    assert_eq!(
        classify(an, 200, r#"{"content":[{"type":"tool_use"}],"stop_reason":"tool_use"}"#),
        Verdict::Malformed
    );
}

/// 节 2 第 5/15/16 三格的联动（设计节 4 第 6.4 条末尾）：开思考的行必须**同时**带分离开关与
/// 已验上界，且这次请求的预算不超过那个上界。三条各守一支，**顺序就是判定的顺序**：
/// 缺开关 ⇒ 思考混进正文、契约每轮判 `SchemaMismatch`；缺上界 ⇒ 症状是"答案莫名变空"；超上界 ⇒ 同上。
#[test]
fn a_thinking_row_without_a_split_field_refuses_to_egress() {
    let mut p = *shape("agnes", "agnes-3.0-flash");
    p.thinking_default = true;              // 表上没有任何一行取 true，只能就地改副本
    p.split_field = None;
    p.budget_ceiling_verified = Some(32_000);
    let e = build_body(&p, &p_target("agnes", "agnes-3.0-flash"), &prompt(), 256, None, None)
        .expect_err("没有分离开关的开思考行不许出网");
    assert!(e.contains("split"), "{e}");
}

#[test]
fn a_thinking_row_without_a_verified_ceiling_refuses_to_egress() {
    let mut p = *shape("agnes", "agnes-3.0-flash");
    p.thinking_default = true;
    p.split_field = Some("reasoning_split");
    p.budget_ceiling_verified = None;
    let e = build_body(&p, &p_target("agnes", "agnes-3.0-flash"), &prompt(), 256, None, None)
        .expect_err("没有已验上界的开思考行不许出网");
    assert!(e.contains("ceiling"), "{e}");
}

#[test]
fn a_thinking_row_over_its_verified_ceiling_refuses_to_egress() {
    let mut p = *shape("minimax", "MiniMax-M3");
    p.budget_ceiling_verified = Some(4_000); // 4_000 正是实测"正文 0 字"那一档
    let e = build_body(&p, &p_target("minimax", "MiniMax-M3"), &prompt(), 8_000, None, None)
        .expect_err("超过已验上界的那一发不许出网");
    assert!(e.contains("ceiling"), "{e}");
}

#[test]
fn the_default_budget_stays_under_the_smallest_measured_seat_ceiling() {
    // 32_000 与 65_536 之间只有一倍的余量，而这两个数一个是**会自己抬上去的常量**、一个是**对端写死的合法域**。
    // 抬 `MAX_BUDGET` 的人今天不会去看实施计划 §4.7 那张表，所以这一关必须长在测试里而不是长在注释里：
    // 越界的后果不是报错，是**前三次调用全钉在 SenseNova 那两行**（400 不在 `RETRYABLE` ⇒ `SchemaMismatch` 不前进）。
    // 变异 M-1：把 `MAX_BUDGET` 抬到 66_000 ⇒ 恰好这一条红。
    let smallest = MEASURED_CEILINGS.iter().map(|(_, c)| *c).min().unwrap_or(0);
    assert!(
        u64::from(MAX_BUDGET) < u64::from(smallest),
        "MAX_BUDGET = {MAX_BUDGET} 已经越过实测最小天花板 {smallest}：\
         先换掉 SenseNova 那两行或给 preset 加按座位分档，再抬这一档"
    );
}

#[test]
fn every_measured_ceiling_is_still_about_a_seat_that_is_on_the_table() {
    // 读数钉在座位名上才有意义：`MEASURED_CEILINGS` 里那条 65_536 说的是 `sensenova-6.8-flash-lite`，
    // 换座之后这个数属于另一只家（10-16 那次换座就是把整行换掉，读数跟着全部过期）。
    // 变异 M-2：改 `ROWS` 里任意一行的 `model_slug` ⇒ 恰好这一条红。
    for &(slug, ceiling) in &MEASURED_CEILINGS {
        assert!(
            ROWS.iter().any(|r| r.model_slug == slug),
            "{slug}（天花板 {ceiling}）已经不在 preset 表上了 ⇒ 这条读数属于一个不存在的座位"
        );
    }
}

/// 换座这件事在表上的落点，逐格钉死（不这么写的话，"格子被清空"那一类只有行为用例抓得到，
/// 而行为用例的 `expect_err` 在格子缺失时**照样拿到 Err** ⇒ 它会绿）。
#[test]
fn the_minimax_seat_is_m3_and_carries_both_new_cells() {
    let m = shape("minimax", "MiniMax-M3");
    assert!(m.thinking_default, "M3 自发思考，这一格必须是 true");
    assert_eq!(m.split_field, Some("reasoning_split"));
    assert_eq!(m.budget_ceiling_verified, Some(32_000));
    assert_eq!(m.reasoning_key, Some(ReasoningKey::ReasoningContent));
    assert_eq!(m.budget_field, BudgetField::MaxCompletionTokens);
    // "换掉"两个字的牙：旧座位必须查不到。
    assert!(find("minimax", "MiniMax-M3.1-Flash-Preview").is_none(), "M3.1 那行必须已经删除");
}

/// 第 15 格的兑现点：开关真的进了请求体，而那份"分居"回复读出来的是正文不是思考。
/// 夹具形状 = 2026-10-16 那一发受控读数（n=1；四个键齐、思考另住 `reasoning_content`、
/// `reasoning_details` 是文档未载的未知键），**正文与思考都是合成串**——批次原文不进仓。
#[test]
fn the_split_switch_is_sent_and_a_split_reply_yields_the_body() {
    let m = shape("minimax", "MiniMax-M3");
    let body = build_body(m, &p_target("minimax", "MiniMax-M3"), &prompt(), 8_000, None, None).unwrap();
    assert!(body.contains("\"reasoning_split\":true"), "{body}");
    match classify(m, 200, M3_SPLIT_REPLY) {
        Verdict::Reply(t) => assert_eq!(t, "{}", "取到的必须是 content 而不是那个 1,067 字符的思考"),
        other => panic!("分离档的合法回复判成了 {other:?}"),
    }
}

fn p_target(provider: &str, slug: &str) -> rc_llm_http::preset::Target {
    find(provider, slug).unwrap().target()
}

#[test]
fn budget_field_name_comes_from_the_table_not_from_a_const() {
    // 同义字段名同家都认、有优先级、静默压制（Agnes 实测）⇒ 一次只发一个，发哪个是数据。
    let d = shape("deepseek", "deepseek-chat");
    let m = shape("minimax", "MiniMax-M3");
    assert_eq!(d.budget_field, BudgetField::MaxTokens);
    assert_eq!(m.budget_field, BudgetField::MaxCompletionTokens);
    let td = p_target("deepseek", "deepseek-chat");
    let tm = p_target("minimax", "MiniMax-M3");
    let bd = build_body(d, &td, &prompt(), 7, None, None).unwrap();
    let bm = build_body(m, &tm, &prompt(), 7, None, None).unwrap();
    assert!(bd.contains("\"max_tokens\":7"), "{bd}");
    assert!(bm.contains("\"max_completion_tokens\":7"), "{bm}");
    assert!(!bd.contains("max_completion_tokens"), "两个同义名不许都发");
}

#[test]
fn error_body_kinds_are_recorded_for_the_three_shape_test() {
    // 第 14 格是**数据**：它不进 codec 的任何分支（解析器按形状通吃，见上面那条三形用例），
    // 但它必须留在表里，否则下一个人重新去踩"Kimi 的 504 是 HTML"这一脚。
    let rows: [&'static Preset; 4] = [
        shape("deepseek", "deepseek-chat"),
        shape("minimax", "MiniMax-M3"),
        shape("agnes", "agnes-3.0-flash"),
        shape("sensenova", "sensenova-6.8-flash-lite"),
    ];
    assert!(rows[0].error_body_kinds.contains(&ErrorBody::BareText));
    assert!(rows[1].error_body_kinds.contains(&ErrorBody::Json));
    assert!(rows[3].error_body_kinds.contains(&ErrorBody::Html));
    assert_eq!(rows[2].reasoning_key, Some(ReasoningKey::ReasoningContent));
    assert_eq!(rows[3].reasoning_key, Some(ReasoningKey::Reasoning));
    // 转售那一行与母行的差别只在参数合法域那一格（其余形状必须同值）。
    let kimi = shape("sensenova", "kimi-k3");
    assert_eq!(kimi.temperature_locked, Some(1.0));
    assert_eq!(kimi.reasoning_key, rows[3].reasoning_key);
    assert_eq!(kimi.budget_field, rows[3].budget_field);
    assert_eq!(kimi.finish_success_set, rows[3].finish_success_set);
}

// ---- custom 通道：地址来自配置，形状来自协议（节 1 接缝②） ----

#[test]
fn a_custom_seat_takes_its_address_from_the_config_and_its_shape_from_the_protocol() {
    let stub = Scripted::new(vec![(200, REPLY_OK.into(), 12)]);
    let b = Live::new_custom(
        "http://127.0.0.1:11434/v1",
        "openai-chat",
        "qwen3:8b-local",
        "kkk".into(),
        64,
        None,
        Box::new(stub.clone()),
    )
    .expect("custom 通道必须构造得起来");
    assert_eq!(b.label(), "custom", "日志与 doctor 的主语：custom 那一支不冒充厂家名");
    assert_eq!(b.shape().protocol, Protocol::OpenAiChat);
    assert!(b.complete(&prompt()).unwrap().contains("t-a1"));
    let (url, headers, body) = stub.last_call();
    assert_eq!(url, "http://127.0.0.1:11434/v1/chat/completions", "URL 逐字来自 llm.toml + 协议路径");
    assert!(headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer kkk"), "{headers:?}");
    assert!(body.contains("\"model\":\"qwen3:8b-local\""), "模型名要逐字来自配置：{body}");

    // 阳性对照：表上的行 label 是厂家名 ⇒ 两条通道的差别真的体现在 `label()` 上，不是恒等于 "custom"。
    let t = Live::new("agnes", "agnes-3.0-flash", "k".into(), 64, None, Box::new(Scripted::new(vec![])))
        .expect("表上的行必须构造得起来");
    assert_eq!(t.label(), "agnes");
    assert_eq!(t.shape().inband_code_overrides, &["model_not_found"]);
}

#[test]
fn a_custom_seat_with_a_typo_in_the_protocol_name_sends_nothing() {
    let stub = Scripted::new(vec![]);
    let e = match Live::new_custom(
        "http://127.0.0.1:11434/v1",
        "openai-chat-completion",
        "m",
        "k".into(),
        64,
        None,
        Box::new(stub.clone()),
    ) {
        Ok(_) => panic!("协议名拼错必须响亮失败，不许回落 openai-chat"),
        Err(e) => e,
    };
    assert!(e.contains("protocol"), "{e}");
    assert_eq!(stub.bodies().len(), 0, "回落默认通道的症状是「打到一条没人要它打的请求」");
}

// ---- 思考档位（reasoning_effort）：只接 openai-chat，且只是"可选加一格" ----

#[test]
fn reasoning_effort_lands_only_when_configured_and_only_on_openai_chat() {
    let m = custom_shape(Protocol::OpenAiChat);
    let t = custom_target("http://127.0.0.1:1", "MiniMax-M3.1-Flash");
    // 没配 ⇒ 请求体里没有这一格（与 temperature 同一条纪律：不补默认值）。
    let base: serde_json::Value =
        serde_json::from_str(&build_body(m, &t, &prompt(), 256, None, None).unwrap()).unwrap();
    assert!(base.get("reasoning_effort").is_none(), "没配就不许发：{base}");
    // 配了 low ⇒ 逐字发出去。
    let low: serde_json::Value =
        serde_json::from_str(&build_body(m, &t, &prompt(), 256, None, Some("low")).unwrap()).unwrap();
    assert_eq!(low["reasoning_effort"], serde_json::json!("low"));

    // Anthropic 形的档位在 output_config.effort，本表没有兑现点 ⇒ 响亮拒绝，不许静默丢掉。
    let an = custom_shape(Protocol::AnthropicMessages);
    let e = build_body(an, &t, &prompt(), 256, None, Some("low"))
        .expect_err("Anthropic 形不该静默接受 reasoning_effort");
    assert!(e.contains("openai-chat"), "{e}");
}
