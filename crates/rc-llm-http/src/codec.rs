//! 两套协议 codec + 三条成功判据（A47 节 4）。
//!
//! 三条判据**顺序固定、不许合并**：`length` 那一族只有抬预算才有效，
//! 瞬时契约逃逸那一族只有原样重发才自愈（实测 3/78 ≈ 3.8%）——共用一条重试逻辑必错。

use crate::preset::{Preset, Protocol, Target};
use rc_llm::{LlmFailure, Prompt};
use serde_json::{json, Value};

/// 一次调用的分型结果。三条判据各占一个变体 ⇒ "共用一条重试逻辑必错"在类型层就共不了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 三条判据全过：正文就是要交给下游的原文。
    Reply(String),
    /// `finish` 合法但正文不是 JSON ⇒ 瞬时契约逃逸，**原样**重发、**不许**抬预算。
    Escape,
    /// `finish = length` 且正文空 ⇒ 思考吃满预算那一族，**抬预算**重发才有效。
    Truncated,
    /// `finish = tool_calls`：正文在第二个落点，不是"库里没有"（守卫 8 一族的假空）。
    ToolChannel,
    /// 落点读不出、白名单之外，或解析成功但正文空 ⇒ 算脱约，不算"库里没有"。
    Malformed,
    /// 传输层或业务层失败，已归一化成四态。
    Failed(LlmFailure),
}

/// 实现律④：请求体只发**该协议文档列出的**字段名（Agnes 实测：多发一个同义名 ⇒ 上限被静默改写、
/// 无任何警告）。所以这里没有"两个都发以防万一"那一支。第 15 格那个开关**是**该协议列出的字段，
/// 因此它由表带着走、不发第二家的名字。
///
/// `thinking_default` 为 `true` 的那一行**按格判定**（节 2 第 15/16 格，设计节 4 第 6.4 条末尾）：
/// 缺分离开关 ⇒ 思考混进正文，过不了"整条正文必须是且只是那一个 JSON 对象"那一判；
/// 缺已验上界或这次预算超过它 ⇒ 实测形态是 `finish_reason=length` 且 `message` 键集里根本没有
/// `content`（2026-10-16 两臂里的对照臂）。两种症状都是"答案莫名变空"，所以响亮失败。
pub fn build_body(
    p: &Preset,
    target: &Target,
    prompt: &Prompt,
    budget: u32,
    temperature: Option<f64>,
    reasoning_effort: Option<&str>,
) -> Result<String, &'static str> {
    if p.thinking_default {
        if p.split_field.is_none() {
            return Err("this line thinks by default but names no split field: thinking would land in the reply body");
        }
        match p.budget_ceiling_verified {
            None => {
                return Err("this line thinks by default with no verified budget ceiling; the wallclock chain is not wired")
            }
            Some(ceiling) if budget > ceiling => {
                return Err("this line's requested budget exceeds its verified ceiling; the body comes back empty at length")
            }
            Some(_) => {}
        }
    }
    // 模型名逐字来自 `Target`（表上的行或 `llm.toml`），这里不做任何大小写处理（第 3 格）。
    let mut body = json!({ "model": target.model_slug });
    match p.protocol {
        Protocol::OpenAiChat => {
            body["messages"] = json!([
                { "role": "system", "content": prompt.system },
                { "role": "user", "content": prompt.user },
            ]);
        }
        Protocol::AnthropicMessages => {
            // Anthropic 形：`system` 是顶层字段、输出预算那一格**必填**（Agnes 的 Messages 页逐字）。
            body["system"] = json!(prompt.system);
            body["messages"] = json!([{ "role": "user", "content": prompt.user }]);
        }
    }
    body[p.budget_field.name()] = json!(budget);
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    // 第 15 格的**兑现点**：没有这三行，那一格只是一格没人读的数据（判据 32 守的就是它）。
    if let Some(field) = p.split_field {
        body[field] = json!(true);
    }
    // 思考档位：只接 OpenAI 兼容形（`reasoning_effort`）。Anthropic 形的档位在
    // `output_config.effort`，本表没有那一格的兑现点 ⇒ 配了就是配置事故，响亮拒绝。
    if let Some(effort) = reasoning_effort {
        match p.protocol {
            Protocol::OpenAiChat => body["reasoning_effort"] = json!(effort),
            Protocol::AnthropicMessages => {
                return Err("reasoning_effort is only wired for the openai-chat protocol")
            }
        }
    }
    Ok(body.to_string())
}

/// 正文与 `finish` 的落点按协议取。思考内容**永不进返回值**：它可能逐字复述批次原文
/// （SenseNova/MiniMax 实测），一个字节都不许进 agent 面。
fn locate(p: &Preset, doc: &Value) -> Option<(String, String)> {
    match p.protocol {
        Protocol::OpenAiChat => {
            let choice = doc.get("choices")?.get(0)?;
            let finish = choice.get("finish_reason")?.as_str()?.to_string();
            // `content` **整个键不见**是实测形状（SenseNova 预算被思考吃满那一形：`message` 只有
            // `{role, reasoning}`，夹具 `sensenova-200-length-without-content.json`）。
            // 它必须读成空串而不是"读不出字段"，否则判据 1（`length` ⇒ 抬预算）在它身上永远不触发：
            // 会在判据之前先变成 Malformed。`null` 同理。
            // 类型不是字符串（数组那一形）⇒ 仍然算读不出，那是另一件事。
            let text = match choice.get("message")?.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(_) => return None,
            };
            Some((text, finish))
        }
        Protocol::AnthropicMessages => {
            let blocks = doc.get("content")?.as_array()?;
            let mut text = String::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(s) = b.get("text").and_then(Value::as_str) {
                        text.push_str(s);
                    }
                }
            }
            let finish = doc.get("stop_reason")?.as_str()?.to_string();
            Some((text, finish))
        }
    }
}

/// 思考键在不在：**键缺失**与**值为 null** 是两件事（我把前者误报成后者过一次）。
/// 唯一读者是那条固化这个区分的用例——它不许被当成"有正文可读"的判据用。
pub fn has_reasoning_key(p: &Preset, doc: &Value) -> bool {
    let key = match p.reasoning_key {
        Some(k) => k,
        None => return false,
    };
    match p.protocol {
        Protocol::OpenAiChat => doc
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get(key.name()))
            .is_some(),
        Protocol::AnthropicMessages => match doc.get("content").and_then(Value::as_array) {
            Some(blocks) => blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("thinking")),
            None => false,
        },
    }
}

/// 错误体里的 code。实测三种形状：`{"error":{"code":"model_not_found"}}`（字符串）、
/// `{"error":{"code":16}}`（**数字**、无 `type`）、`{"base_resp":{"status_code":1026}}`。
/// 全都要读得出来，读不出来返回 `None`（不等于"没有 code"，也不等于"成功"）。
pub fn error_code(doc: &Value) -> Option<String> {
    if let Some(c) = doc
        .get("base_resp")
        .and_then(|b| b.get("status_code"))
        .and_then(Value::as_i64)
    {
        return Some(c.to_string());
    }
    match doc.get("error").and_then(|e| e.get("code")) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// 响应体的形状先于内容：Kimi 的 504 返 HTML、DeepSeek 无 `Authorization` 时返裸文本且
/// `content-type` 为空。解析器**不许**在这里 panic。
pub fn parse_json(body: &str) -> Option<Value> {
    serde_json::from_str(body).ok()
}

/// 三条判据 + 带内 code 覆盖 + 带内业务状态。
pub fn classify(p: &Preset, status: u16, body: &str) -> Verdict {
    if !(200..300).contains(&status) {
        // 同码两判（503 在 Agnes 上同时承载"上游忙"与"这个模型无可用渠道"）。
        // **可读的见证只有带内 `error.code` 一条**（节 2 第 10 格）：命中覆盖名单 ⇒ 配置事故，
        // 换一家也一样 ⇒ `SchemaMismatch`（不前进）。
        //
        // 这里原本按"往返低于阈值"来分这一判，实测把它推翻了：DeepSeek 的 503 是**常态**，
        // 六次采样一半是 503、全部在 0.1–0.6 秒立回、体是标准 JSON 且 `error.code` 非空
        // （`server_error`）。按形状而不是按 code 判会把这一家一半的正常抖动读成"配置写错模型名"，
        // 于是它既不前进、又报成"回复形状不符"。⇒ 时延不进判据（它只进运维日志那一行，见 `crate::ops_line`）。
        // 那六次采样的原始取证件在**仓外**（`D:\recollect\.local\docs-audit-2026-10-15\response-formats.md`，
        // 不在 git 里、可能被清），所以这一族结论只认设计档那份更正：
        // `docs/plans/2026-10-15-a47-provider-access-surface-design.md` 节 4 与节 5 的"就地更正（2026-10-16）"两处。
        let doc = parse_json(body);
        if let Some(code) = doc.as_ref().and_then(error_code) {
            if crate::preset::code_is_overridden(p, &code) {
                return Verdict::Failed(LlmFailure::SchemaMismatch);
            }
        }
        // `retryable_status` 的语义在这里兑现（唯一兑现点）：名单内 = "这一族的失败是**这家**的状态、
        // 换一家可能成立" ⇒ `BackendError`（`Rotating` 据此前进）；名单外（400/404/422 那一族）
        // = 这个请求本身坏了，换到别家同样不成立 ⇒ `SchemaMismatch`（不前进，连续 3 次才前进）。
        // 名字沿用节 2 第 9 格，但**没有任何一层按它做同家重发**：一次发送对应账本一次计数，
        // 同家重发会把一轮预算静默搬成两次真调用（节 4"两本账不许互冒充"）。
        let f = if p.retryable_status.contains(&status) {
            LlmFailure::BackendError
        } else {
            LlmFailure::SchemaMismatch
        };
        return Verdict::Failed(f);
    }
    let doc = match parse_json(body) {
        Some(d) => d,
        // 2xx 却不是 JSON：这一族的成因只有"网关在成功码上回了 HTML"，是厂家故障不是我们的。
        None => return Verdict::Failed(LlmFailure::BackendError),
    };
    // 判据前置：HTTP 200 里塞业务失败（只有一家干，所以它是表的一格）。
    if let Some(code) = crate::preset::inband_failure(p, &doc) {
        return if crate::preset::code_is_overridden(p, &code) {
            Verdict::Failed(LlmFailure::SchemaMismatch)
        } else {
            // 限流／超时那一族是这家的状态 ⇒ 换家。这一支永不产生 `QuotaExhausted`（判据 N7 那一侧）。
            Verdict::Failed(LlmFailure::BackendError)
        };
    }
    let (text, finish) = match locate(p, &doc) {
        Some(x) => x,
        None => return Verdict::Malformed,
    };
    // 判据 1：白名单之外不尝试解析正文。
    if !p.finish_success_set.contains(&finish.as_str()) {
        return Verdict::Malformed;
    }
    if finish == p.protocol.tool_finish() {
        return Verdict::ToolChannel;
    }
    // 判据 1 的预算吃满那一格：**只有抬预算才有效**，且不分正文空不空。
    // 实测两形都出现——预算被思考吃满时 `content` 空或整个不见（SenseNova、转售路由上的 glm），
    // 预算小于一个词时 `content` 是半句（Agnes 的 `max_tokens=1` 探针回了 "Hello"）。
    // 把"半句"往下送会落进判据 2、拿同一个预算原样重发同一个半句，那是白烧一次出网。
    // 取值按协议（实现律⑤）：Anthropic 那一侧是 `max_tokens`，`length` 在那边不存在。
    if finish == p.protocol.truncated_finish() {
        return Verdict::Truncated;
    }
    // 判据 3 **排在判据 2 前面**：`finish=stop` + 正文空是 MiniMax 的合法失败形态（输出涉敏，
    // 文档轴上 `base_resp.status_code` 可以是 0）。它是脱约，不算"库里没有"（守卫 8 那一族的假空）；
    // 而空串本身不是合法 JSON，放在判据 2 之后会被读成"瞬时契约逃逸"、去重发一次永远不会变的东西。
    if text.trim().is_empty() {
        return Verdict::Malformed;
    }
    // 判据 2：finish 合法、正文非空、但不是 JSON ⇒ 瞬时契约逃逸（实测 3/78 原样重发即自愈）。
    // 契约字段级的那一层判定住在 `rc-distill`（它才认识输出契约），codec 只判"是不是 JSON"。
    if serde_json::from_str::<Value>(text.trim()).is_err() {
        return Verdict::Escape;
    }
    Verdict::Reply(text)
}
