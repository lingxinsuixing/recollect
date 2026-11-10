//! 协议层。**唯一的分帧知识住处**：`main.rs` 只管"这条请求该回什么"，
//! 一行进一行出由这里保证。
use serde_json::{json, Value};

pub const PARSE: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32603;

/// 一行输入读出来的四种结局。**没有第五种**：读不成消息的一律 `Malformed`，
/// 而它带的是一个常量串——它会原样回到宿主面前（守卫 12：错误面零正文零路径）。
#[derive(Debug, PartialEq)]
pub enum InMsg {
    Request { id: Value, method: String, params: Value },
    /// 没有 `id` 的消息。**这个变体没有字段是有意的**：本实现对它唯一的动作是"不回包"，
    /// 留着 `method`/`params` 会让 `dead_code` 报"字段从未被读"——那条警告说的正是"这里没做过决定"。
    Notification,
    /// 空行：宿主配置里多出来的东西。不回包（回它等于在 stdout 上放一行没有主人的字）。
    Empty,
    Malformed(&'static str),
}

pub fn decode(line: &str) -> InMsg {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return InMsg::Empty;
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return InMsg::Malformed("not json");
    };
    // batch 数组：JSON-RPC 允许，本实现明确不支持。不支持要说出来，不许看起来像成功。
    if v.is_array() {
        return InMsg::Malformed("batch not supported");
    }
    if v.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return InMsg::Malformed("missing or wrong jsonrpc version");
    }
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return InMsg::Malformed("missing method");
    };
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match v.get("id") {
        // `id: null` 也算请求：JSON-RPC 里 null 是合法 id，把它读成通知等于丢一条永久收不到回执的请求。
        Some(id) => InMsg::Request { id: id.clone(), method: method.to_string(), params },
        None => InMsg::Notification,
    }
}

pub fn result_line(id: &Value, result: Value) -> String {
    // `serde_json::to_string` 转义所有控制字符，所以"物理一行"由序列化保证而不是断言保证；
    // 上面那条测试断的是**这件事有人看着**，不是它常常成立。
    serde_json::to_string(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
        .unwrap_or_else(|_| fallback_error(id))
}

pub fn error_line(id: &Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}
    }))
    .unwrap_or_else(|_| fallback_error(id))
}

fn fallback_error(id: &Value) -> String {
    // 连错误都编不出来时唯一诚实的说法。id 走 serde 而不是插值：插值进来的字符串
    // 可能含裸换行，而那正是我们要防的那一件事。
    let id = serde_json::to_string(id).unwrap_or_else(|_| "null".to_string());
    format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{INTERNAL},"message":"serialization failed"}}}}"#)
}

// crates/mcp/src/rpc.rs —— 单元测试写在同文件末尾。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 有id是请求_无id是通知_空行是第三种() {
        assert_eq!(
            decode(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
            InMsg::Request { id: json!(1), method: "tools/list".into(), params: json!(null) }
        );
        assert_eq!(decode(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), InMsg::Notification);
        assert_eq!(decode("   "), InMsg::Empty);
    }

    #[test]
    fn 未知字段必须被忽略_c2演进规则的协议侧() {
        // §6.5 纪律 1："消费者必须忽略未知字段"。协议层是第一消费者，
        // 所以它先在 `jsonrpc`/`id`/`method`/`params` 之外能容忍任意东西，承诺面才敢只增不减。
        let line = r#"{"jsonrpc":"2.0","id":"a1","method":"tools/list","params":{},"_future_capability":{"x":[1,2]},"trace":"synthetic"}"#;
        match decode(line) {
            InMsg::Request { id, method, .. } => {
                assert_eq!(id, json!("a1"));
                assert_eq!(method, "tools/list");
            }
            other => panic!("未知字段把请求吃掉了：{other:?}"),
        }
    }

    #[test]
    fn 六种坏输入都是malformed而不是panic() {
        // 六对 `(输入, 期望字面量)` 而不是 `matches!(.., Malformed(_))`：四条常量各有读者
        // （`"not json"` 一个、`"missing or wrong jsonrpc version"` 三个、`"batch not supported"` 一个、
        // `"missing method"` 一个），把其中任何一条塌成同一条都会当场红——旧的循环写法对此全绿。
        // 期望值逐条取自 `decode` 的分支顺序（空行 ⇒ `Empty`，不进这张表）。
        for (bad, want) in [
            ("not json", "not json"),
            (r#"{"id":1,"method":"x"}"#, "missing or wrong jsonrpc version"),                 // 缺 jsonrpc
            (r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#, "missing or wrong jsonrpc version"), // 版本不对
            (r#"[{"jsonrpc":"2.0","id":1,"method":"x"}]"#, "batch not supported"),            // batch：本实现不支持，且不猜
            (r#"{"jsonrpc":"2.0","id":1}"#, "missing method"),                                 // 有 id 无 method
            // 合法 JSON（一个字符串值）⇒ 过得了 `from_str`，但 `Value::String` 上 `get("jsonrpc")`
            // 是 None ⇒ 落版本那一支。填 `"not json"` 正是这条断言要拦住的手势。
            (r#""just a string""#, "missing or wrong jsonrpc version"),                       // 不是对象
        ] {
            assert_eq!(decode(bad), InMsg::Malformed(want), "{bad}");
        }
    }

    #[test]
    fn 编码是物理一行且换行被转义() {
        // stdout 的分帧就是换行：正文里出现一个裸换行等于把一条响应劈成两条。
        let line = result_line(&json!(1), json!({"text": "甲\n乙"}));
        assert_eq!(line.lines().count(), 1, "输出必须物理一行：{line}");
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["result"]["text"], "甲\n乙");
    }

    #[test]
    fn 错误码是四个固定值且id原样回声() {
        assert_eq!(PARSE, -32700);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL, -32603);
        for id in [json!("z"), json!(7), json!(null)] {
            let e = error_line(&id, INVALID_PARAMS, "bad params");
            let v: Value = serde_json::from_str(&e).unwrap();
            assert_eq!(v["error"]["code"], -32602);
            assert_eq!(v["id"], id, "id 要原样回声：{e}");
        }
    }
}
