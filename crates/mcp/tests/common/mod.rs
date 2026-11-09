//! mcp 集成测试的**共享夹具**（P-65：Task 10 把 `tools.rs` 第 24–143 行的非用例代码原样搬进这里，
//! 再为 P2/P3/P4/P5 四个新文件补 `call_json`/`call_error_text` 一对适配器）。
//! `tests/<目录>/mod.rs` 不是独立的 test target ⇒ 这次迁移的**唯一收据**是 mcp 的 22 条读数一格不动。
//!
//! 夹具的两条纪律（原文住在 `tools.rs` 的档头，这里逐字跟着搬）：
//! - **一次 spawn 验多格**：`Session` 是一个进程里的一串往返，不是每条断言一个进程。
//! - **读之前必须 flush**：请求停在用户态缓冲里时 `read_line` 会把测试挂死，
//!   那是用例唯一一种"看起来在跑"的失败。
//!
//! 播种是白盒的（`seed` 直接往 `memory.db` 写行 + 建索引）："recall 有东西可召回"
//! 不是用例要证的事，那是 `rc-search` / `rc-index` 各自的用例。
// 共享夹具的**每个宿主 target** 只用其中一部分（`tools.rs` 不碰 `call_json`/`call_error_text`，
// `p2_extract_state.rs` 不碰 `run_cli`/`seed`/`turns`……）：`tests/<目录>/mod.rs` 不是独立 target，
// 于是其余 target 里这些项按 `dead_code` 报。allow 收在模块级而不是逐项——逐项 allow 会让
// "哪些项还有读者"变成逐条考古，而 P-65 的判据是条数与 `CARGO_BIN_EXE` 落点，不是这份清单。
#![allow(dead_code)]
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

pub const TOK: &str = "工具面合成词";
pub const BODY: &str = "CANARY-AGENT-BODY-9931 带工具面合成词的一条合成记录";

pub struct Session {
    pub child: Child,
    pub stdin: ChildStdin,
    pub out: BufReader<std::process::ChildStdout>,
}

impl Session {
    pub fn start(data: &Path) -> Session {
        Session::start_with(data, &["--device-id", "dev-synthetic"])
    }

    /// 同一套往返，额外参数由调用方给（节点模式的 `--replica` 从这里进）。
    pub fn start_with(data: &Path, extra: &[&str]) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_recollect-mcp"))
            .args(["--data-dir", data.to_str().unwrap()])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("mcp binary");
        Session {
            stdin: child.stdin.take().unwrap(),
            out: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }

    /// 发一行、读一行。**读之前必须 flush**，否则请求还停在用户态缓冲里，
    /// `read_line` 会把测试挂死——那是本夹具唯一一种"看起来在跑"的失败。
    pub fn ask(&mut self, msg: &Value) -> Value {
        self.stdin.write_all(format!("{}\n", serde_json::to_string(msg).unwrap()).as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        let mut got = String::new();
        let n = self.out.read_line(&mut got).unwrap();
        assert!(n > 0, "服务端在没有回包的情况下关闭了 stdout");
        serde_json::from_str(&got).unwrap_or_else(|e| panic!("回包不是 JSON：{got} / {e}"))
    }

    /// 发一行**裸文本**（用来喂 `Malformed` 那一支）。
    pub fn ask_raw(&mut self, raw: &str) -> Value {
        self.stdin.write_all(format!("{raw}\n").as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        let mut got = String::new();
        assert!(self.out.read_line(&mut got).unwrap() > 0, "坏输入没回包");
        serde_json::from_str(&got).unwrap_or_else(|e| panic!("回包不是 JSON：{got} / {e}"))
    }

    /// 关掉 stdin ⇒ 服务端读到 EOF 退出；此时缓冲区里若有多余行会全部露出来。
    pub fn drain(self) -> Vec<String> {
        let Session { child, stdin, mut out } = self;
        drop(stdin);
        let _ = child; // Child 不持有已经移出去的 stdin/stdout
        let mut v = Vec::new();
        let mut buf = String::new();
        while out.read_line(&mut buf).unwrap_or(0) > 0 {
            v.push(std::mem::take(&mut buf));
        }
        v
    }
}

/// 单独起一个进程、喂指定的 `--device-id`（或 `RECOLLECT_DEVICE_ID`）、**不等回包**。
/// 用于启动期就该失败的输入：`stdin` 走 `Stdio::null()`，所以服务端若错误地活到读循环，
/// 它会读到 EOF 而**退 0**——那正好被"退出码必须是 3"抓住，而不是把测试挂死。
pub fn run_cli(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_recollect-mcp"));
    c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("mcp binary")
}

pub fn init(id: i64, client_name: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"initialize",
           "params":{"protocolVersion":"2025-03-26","clientInfo":{"name":client_name,"version":"0"}}})
}
pub fn call(id: i64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}
/// 工具返回的那段文本。五个工具全走 `content[0].text`，所以这一步抽成函数。
pub fn tool_text(r: &Value) -> String {
    r["result"]["content"][0]["text"].as_str().unwrap().to_string()
}

/// 白盒播种：直接写记忆行 + 建索引（绕开 `mcp` 自己，"有东西可召回"不是用例要证的事）。
pub fn seed(data: &Path, now_ms: i64) -> String {
    use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
    use rc_store::Root;
    let root = Root::open(data).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: format!("{TOK} 的口径"),
            body: "召回面只断形状。".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        now_ms,
    )
    .unwrap();
    rc_index::rebuild(root.memory(), now_ms).unwrap();
    k.key_string()
}

/// 目录用的播种：一条实体（标题=别名之一）+ 一条**与它共享出处轮**的 card。
/// 返回 (`title`, `alias_hit`)，`alias_hit` 刻意取**别名**而不是标题——用例用它打的就是
/// "别名入口在认"这一半（P-51）；如果实现只按标题解析，那条用例会读到空目录而不是报错。
pub fn seed_person(data: &Path, now_ms: i64) -> (String, String) {
    use rc_memory::{put_card, put_entity, AnchorRole, EntityContent, FactContent,
                    Provenance, Scope, ScopeKind};
    use rc_store::Root;
    let root = Root::open(data).unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() };
    // 实体与 card **共享同一条出处轮**：目录的 `refs` 连的是出处而不是归属（P-56），
    // 这一串就是那条边的两端。
    let prov = vec![Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }];
    let title = "老王的口径".to_string();
    let alias_hit = "某老板".to_string();
    put_entity(root.memory(), &EntityContent {
        kind: "person".into(), title: title.clone(), scope: scope.clone(),
        aliases: vec![alias_hit.clone()],
    }, &prov, now_ms).unwrap();
    put_card(root.memory(), &FactContent {
        kind: "decision".into(), title: format!("{TOK} 的口径"), body: "目录面只断形状。".into(),
        scope, valid_to_ms: None,
    }, &prov, now_ms).unwrap();
    rc_index::rebuild(root.memory(), now_ms).unwrap();
    (title, alias_hit)
}

pub fn turns(data: &Path) -> Vec<(String, String)> {
    let root = rc_store::Root::open(data).unwrap();
    // 这里必须先把结果绑到局部变量再返回：作为尾表达式时 `prepare` 造出的那条 `Statement`
    // （它借用了 `root`）会在 `root` 之后才 drop，`E0597` 当场把 `tools.rs` 拦在编译期。
    let got = root
        .transcript()
        .conn()
        .prepare("SELECT role, body FROM turns")
        .unwrap()
        .query_map([], |x| Ok((x.get(0)?, x.get(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();
    got
}

/// 发一次工具调用、拿回解析后的 JSON。在 `tools.rs` 的既有用例里这是三条语句
/// （`ask` + `tool_text` + `from_str`），收成一家是为了四个新文件不各抄一遍。
/// 两句 `assert` 也住在这里：工具级失败**必须**走 `isError`，不许漂成 JSON-RPC error 对象
/// ——阶段 1 的 `工具面失败走isError不走JSONRPC错误` 断的是失败路径，这两句断的是
/// **成功路径**同样不许漂，于是四个新文件的每一次往返都自动复用这条协议纪律。
pub fn call_json(s: &mut Session, id: i64, tool: &str, args: Value) -> Value {
    let r = s.ask(&call(id, tool, args));
    assert!(r["error"].is_null(), "{tool} 走了 JSON-RPC error 而不是 isError：{r}");
    assert_eq!(r["result"]["isError"], json!(false), "{tool} 意外失败：{}", tool_text(&r));
    serde_json::from_str(&tool_text(&r)).unwrap()
}

/// 失败那一条路：`isError: true`，文本是 `kind:code hint` 三段（`error.rs` 的 `Display`）。
pub fn call_error_text(s: &mut Session, id: i64, tool: &str, args: Value) -> String {
    let r = s.ask(&call(id, tool, args));
    assert!(r["error"].is_null(), "{tool} 走了 JSON-RPC error 而不是 isError：{r}");
    assert_eq!(r["result"]["isError"], json!(true), "{tool} 期望 isError，回执却是成功：{r}");
    tool_text(&r)
}

/// **正常状态**那一条路：`isError: false`，文本仍是 `kind:code hint` 三段原样。
/// 它与 `call_error_text` 的**唯一**差别就是 `isError` 那一维：`recollect_recall` 的
/// `no_rows`（给了词、库里没命中）走这里（Task 10：那是正常状态，宿主不该把它当失败处理），
/// 而其余 `Empty` 码（`row_absent`/`turn_absent`/`not_found`）与运维态的 `no_memory`
/// **仍走 `call_error_text`**——前三种是"指认了一条不存在的东西"，最后一种是"这台还没有可用副本"。
pub fn call_normal_state_text(s: &mut Session, id: i64, tool: &str, args: Value) -> String {
    let r = s.ask(&call(id, tool, args));
    assert!(r["error"].is_null(), "{tool} 走了 JSON-RPC error 而不是 isError：{r}");
    assert_eq!(r["result"]["isError"], json!(false), "{tool} 期望正常状态，回执却标了失败：{r}");
    tool_text(&r)
}
