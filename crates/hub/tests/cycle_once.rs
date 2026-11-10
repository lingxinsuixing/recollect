// crates/hub/tests/cycle_once.rs
//! 十七条全部走真进程：CLI 是本任务的**唯一**出口面，装配漏了一支的时候，
//! 只有黑盒调用看得见（单元测试里"忘了调用 run_dedupe"不红——它红在stdout 少了读数那一格，
//! 而那一格必须由子进程的 stdout 才读得出来）。
//! `use rc_contract::compute_turn_uid` 是本文件唯一的 uid 来源：**不许**在这里重抄一遍哈希
//! （守卫 3——那个函数的家在生产侧 `crates/rc-contract/src/identity.rs`，测试用它是引用不是复制）。
use rc_contract::compute_turn_uid;
use rc_ledger::counters;
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::writers_a::attest;
use rc_memory::writers_c::retire;
use rc_memory::writers_x::{put_card, put_entity, EntityContent, FactContent};
use rc_memory::{MemKey, Provenance, Stance};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }
fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}
/// `cycle once` 的来源两格由调用方给，`--data-dir` 指到新临时目录。
fn cycle_raw(dir: &Path, source_args: &[&str]) -> std::process::Output {
    let dd = dir.join(DATA);
    let dd = dd.to_str().unwrap();
    let mut args: Vec<&str> = vec!["cycle", "once"];
    args.extend_from_slice(source_args);
    args.extend_from_slice(&["--data-dir", dd]);
    run(&args)
}
/// 带环境变量跑的 `cycle once`。**先把 `RECOLLECT_DEVICE_ID` 从子进程环境里摘掉**，再按 `env`
/// 加回去：不摘的话，本机那个变量的 ambient 值会让"缺身份"那一支在别人的终端上根本跑不到
/// （用例要的缺席必须由用例自己造）。摘与加都走 `Command` 的 builder，父进程的环境一个字节不动。
fn run_env(dir: &Path, source_args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.env_remove("RECOLLECT_DEVICE_ID");
    for (k, v) in env { cmd.env(k, v); }
    cmd.args(["cycle", "once"]);
    cmd.args(source_args);
    let dd = dir.join(DATA);
    cmd.args(["--data-dir", dd.to_str().unwrap()]);
    cmd.output().expect("hub binary")
}
fn code(out: &std::process::Output) -> i32 { out.status.code().unwrap() }
fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}
fn stderr_text(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

const DEV: &str = "dev-cli";
const SESS: &str = "sess-cli";
const DATA: &str = "data";
/// 卡片正文里的记号：第 8 条用例断它**不出现在** stdout（守卫 12 的零正文）。
const CANARY: &str = "CLI-CARD-BODY-7731";
const YES: &str = r#"{"holds":true}"#;
/// 判同的两种答复。`r1` 恒为探针（渲染顺序在判同那一层的 prompt 里定死），
/// 所以"留 r1"就是"留探针、顶掉邻居"，"same:false"就是这一个方向不并。
const JUDGE_KEEP_PROBE: &str = r#"{"same":true,"keep":"r1"}"#;
const JUDGE_NOT_SAME: &str = r#"{"same":false}"#;
/// 推导家族在脚本中的"跑通但不落行"答复：三键齐、`chains` 里给一条**单环节**链。
/// **为什么不是空三键**：裁定 12 写了"写零行 = 错误对 rc-derive 原样适用（非空批三键皆空 ⇒
/// 按失败处置、不推水位）"，所以 `{"relationships":[],"chains":[],"timeline":[]}` 是
/// `ZeroOutput` ⇒ `BatchDropped` ⇒ exit 2 + attempts +1，拿它当 happy path 的填充会把三条
/// 既有用例从 exit 0 改红。批次③ 段 B 起 chains 这一族有了生产写入口（`derive_pass` 的
/// `put_chains` 一支），这条答复"不落行"的原因换成了 parse 的第一道拒收：**环节数 < 2 ⇒
/// 条目级拒收**，单环节永远走不到落库那支 ⇒ 读数照旧：`derive_rows = 0`、影子行数不变、
/// `derive_rejected = 1`、游标照推。它就是那道拒收在装配面上的唯一证人，与下面的
/// `DERIVE_CHAIN`（落库证人）各钉一格、互不顶替。
const DERIVE_QUIET: &str = r#"{"relationships":[],"chains":[{"from":["r1"],"mechanism":"requires","basis":["r1"]}],"timeline":[]}"#;
/// 推导家族"真落链"的答复：四键齐、`chains` 里一条**两环节**链，`basis:["r1"] ⊆ from`
/// （输出契约的示例形状本身如此，工单 C-3）。`type` 是批次 M 起 `chain` 的必填三值键——
/// 缺它会被 parse 单条拒收、装配面读数从 1 变 0。它过得了 parse 的闸门，从 `put_chains` 落库：
/// 头行 `event(kind='causal')` + `ord = 0/1` 的两行环节；basis 那一跳与 r1 的环节行撞
/// `(mem_table, mem_id, turn_uid)` 主键 ⇒ 被 `INSERT OR IGNORE` 忽略，库里留的是环节那一行
/// （执法用例见 `两环节的链答复经装配面真落库并带着影子抬高`）。
const DERIVE_CHAIN: &str = r#"{"relationships":[],"chains":[{"from":["r1","r2"],"mechanism":"先立的那条决定把后来的排期顶掉了。","basis":["r1"],"type":"causes"}],"timeline":[]}"#;
/// 批次 M 段 C 的"只给 mention"答复（四键齐、三键空 + 一条合法 mention ⇒ **不是** `ZeroOutput`，
/// 见 R-M-1）。名字逐字出现在被引行的 title 里（verbatim 闸门），行别名 `r2` 落到批里那张
/// 消费行——白盒播种的实体锚在 `r1`，而锚行提到自己的那一格被 `put_mentions` 的 I6 自指闸门
/// 跳过 ⇒ mention 必须从消费行发出才写得成。它钉的是"段 B 的三个 mention 计数器真被送到 cycle
/// 的读数面"：`derive_rows = 1`（mention 实际插入行数进 `rows_written`）、
/// `derive_mentions_written = 1`、`derive_mentions_unresolved = 0`、`derive_mentions_missing = 0`。
const DERIVE_MENTION_ONLY: &str = r#"{"relationships":[],"chains":[],"timeline":[],"mentions":[{"row":"r2","name":"Hub证甲"}]}"#;
/// 审核票 1 的不成立答复。它是 `verdict=failed` + 退场账的唯一生产者（`audit::audit_row`），
/// 与 `YES` 只差一个词——这个差就是守卫 13 那条抑制账的开关。
const NO: &str = r#"{"holds":false}"#;

fn turn(role: &str, body: &str, at: i64) -> Value {
    json!({"role": role, "body": body, "created_at_ms": at})
}
fn uid(role: &str, body: &str, at: i64) -> String {
    compute_turn_uid(DEV, SESS, role, body, at)
}

/// 导入件。`name` 是为了同一目录下能放第二份（第 5 条用例要两个批次）。
fn import_file(dir: &Path, name: &str, turns: Vec<Value>) -> std::path::PathBuf {
    let p = dir.join(format!("exchange-{name}.json"));
    let doc = json!({"format_version": 1, "sessions": [{
        "session_key": SESS, "device_id": DEV, "source_agent": "agent-cli", "turns": turns }]});
    std::fs::write(&p, doc.to_string()).unwrap();
    p
}
fn import(dir: &Path, name: &str, turns: Vec<Value>) {
    let p = import_file(dir, name, turns);
    let out = run(&["import", p.to_str().unwrap(), "--data-dir", dir.join(DATA).to_str().unwrap()]);
    assert!(out.status.success(), "导入夹具必须成功：{}", stderr_text(&out));
}

/// `--replies` 文件由 `serde_json` 现写（**不是**手写转义 JSON）：数组元素是字符串时，
/// 那一条 LLM 回复本身就得再转义一层，手写的版本一定会漂。
fn cycle(dir: &Path, replies: &[Value]) -> std::process::Output {
    let p = dir.join("replies.json");
    std::fs::write(&p, Value::Array(replies.to_vec()).to_string()).unwrap();
    run(&["cycle", "once", "--replies", p.to_str().unwrap(),
          "--data-dir", dir.join(DATA).to_str().unwrap()])
}
fn reply(text: &str) -> Value { Value::String(text.to_owned()) }
fn fail(slug: &str) -> Value { json!({"fail": slug}) }

/// 五键齐全、一条卡片、出处指向本轮那条人工轮。
fn extract_ok(u: &str) -> String {
    json!({"entities": [], "events": [], "assertions": [],
           "cards": [{"kind": "lesson", "title": "上线前先跑测试",
                      "body": format!("{CANARY} 每次合并之前先跑全量测试"),
                      "valid_to_ms": null, "provenance": [u]}],
           "worthless_turn_uids": []}).to_string()
}
/// 判同开口的夹具：两条**同 kind、正文逐字相同、标题只差一个词**的卡片，出处分指同一会话的两条 turn。
/// 两个条件都在 `compute_mem_id` 那一侧：标题归一后不同 ⇒ 两个句柄（同一条内容的重传走的是
/// 抑制账，不是判同）；正文同词 ⇒ `score_row` 的 `matched >= 1` 过 `FLOOR`（那一个是任何非零命中
/// 都过线的取值），于是 `scored_siblings` 两个方向都不空、判同必须开口。
fn dup_cards(u1: &str, u2: &str) -> String {
    json!({"entities": [], "events": [], "assertions": [],
           "cards": [
               {"kind": "lesson", "title": "上线前先验证",
                "body": format!("{CANARY} 每次合并之前先跑全量测试"),
                "valid_to_ms": null, "provenance": [u1]},
               {"kind": "lesson", "title": "上线前需要验证",
                "body": format!("{CANARY} 每次合并之前先跑全量测试"),
                "valid_to_ms": null, "provenance": [u2]}],
           "worthless_turn_uids": []}).to_string()
}
/// 缺 `cards` 键 ⇒ `ContractError::MissingKey` ⇒ `reason_slug()` 是 `Some("schema")`（P-40 的输入）。
const CONTRACT_BAD: &str = r#"{"entities":[],"events":[],"assertions":[],"worthless_turn_uids":[]}"#;

fn open(dir: &Path) -> Root { Root::open(&dir.join(DATA)).unwrap() }
/// `attempts` 没有 CLI 读数面，也没有 getter（任务 3 的 `water` 只暴露游标与停轮位）。
/// 所以 P-32 那两条判据只能直接读那一列——这是本文件带 `rusqlite` dev-dependency 的全部理由。
fn attempts(dir: &Path) -> i64 {
    open(dir).ledger().conn().query_row(
        "SELECT sum(attempts) FROM job_watermark WHERE family='distill'", [], |r| r.get(0)).unwrap_or(0)
}
fn cursor_uid(dir: &Path) -> String {
    open(dir).ledger().conn().query_row(
        "SELECT cursor_turn_uid FROM job_watermark WHERE family='distill' LIMIT 1", [],
        |r| r.get(0)).unwrap_or_default()
}
fn unreviewed(dir: &Path) -> i64 {
    open(dir).memory().conn().query_row(
        "SELECT count(*) FROM card WHERE verdict='unreviewed'", [], |r| r.get(0)).unwrap()
}
fn llm_job_ids(dir: &Path) -> Vec<String> {
    let root = open(dir);
    counters::read_all(root.ledger()).unwrap().iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls")
        .map(|r| r.subject_id.clone()).collect()
}
/// P-33 的那条差值在这一层只有"总量"可读：`sum(llm_calls) == 递进去的脚本长度`。
/// 脚本长度是本文件自己写的（每条用例都数得清），所以这条断言不是修辞而是算术。
fn llm_calls_total(dir: &Path) -> i64 {
    let root = open(dir);
    counters::read_all(root.ledger()).unwrap().iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls")
        .map(|r| r.count).sum()
}
/// 单个家族的 `llm_calls`。P-40 那道闸门在这一层要按**增量**执法：计数器行是一进场就是整表读数，
/// 第一轮判同开过口之后 `dedupe` 那一行永远在场，"这一轮的 ids 不含 dedupe"就成了一条**未变异也红**的
/// 断言（恒假和恒真一样没有执法力，方向别记反）。另有第二形可用——`UsageRow.last_at_ms` 配上
/// "零增量不建行"，读成"这一轮那个家族的行没被碰过"，但它要比系统时钟，所以这里用增量。
fn llm_calls_for(dir: &Path, family: &str) -> i64 {
    let root = open(dir);
    counters::read_all(root.ledger()).unwrap().iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls" && r.subject_id == family)
        .map(|r| r.count).sum()
}
/// S3a：生产侧写入的"最近一次运行是否触顶"那一格（`llm_job:<subject>`）。
/// 这一格是 `status.quota_exhausted` 与 `doctor` 告警的共同来源，所以这里断的是
/// **生产者真写了**，而不是读侧自己推出来的结论。
fn quota_stopped_for(dir: &Path, subject: &str) -> Option<i64> {
    let root = open(dir);
    counters::read_all(root.ledger()).unwrap().iter()
        .find(|r| r.subject_kind == "llm_job"
                 && r.metric == counters::QUOTA_STOPPED_METRIC
                 && r.subject_id == subject)
        .map(|r| r.count)
}
/// 判同落笔的库内形状：`(card 总行数, 带 superseded_by 的行数, 指针指向另一条活行的行数, 指针悬空的行数)`。
/// `merged` 那一格必须有第二个证人：打印出来的那个数是作业自报的，而"自报了几条"与"库里真有几条
/// 被顶掉"不是同一件事。**真正只有库内形状能接住的失效是顶替指针落错目标**（把输家的指针写成
/// 指向它自己、或指向一条已经死了的行）——那种形状下 `apply_merges` 照样返回 `merged = 1`，
/// 只有"指针指向活行 / 指针悬空"这两格会红。
/// （`apply_merges` 整段换成 `let _ = plans;` 时先红的是退出码与 `merged` 两格，这条证人今天走不到，
/// 但它管的是更窄的那一种：数对了、账错了。）
fn merge_ledger(dir: &Path) -> (i64, i64, i64, i64) {
    let root = open(dir);
    let m = root.memory();
    let count = |sql: &str| -> i64 { m.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    (
        count("SELECT count(*) FROM card"),
        count("SELECT count(*) FROM card WHERE superseded_by IS NOT NULL"),
        count("SELECT count(*) FROM card c WHERE c.superseded_by IS NOT NULL \
               AND EXISTS (SELECT 1 FROM card w WHERE w.id=c.superseded_by \
                           AND w.superseded_by IS NULL)"),
        count("SELECT count(*) FROM card c WHERE c.superseded_by IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM card w WHERE w.id=c.superseded_by)"),
    )
}
/// 未解决的退场账的 reason 清单。守卫 13 的抑制与 §2.19 的消音归因读的就是这一张表。
fn retirement_reasons(dir: &Path) -> Vec<String> {
    let root = open(dir);
    let m = root.memory();
    let mut st = m.conn().prepare(
        "SELECT reason FROM retirement WHERE resolved_at_ms IS NULL ORDER BY reason").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<_, _>>().unwrap()
}

/// 两条座位来源的开关面（§4.8 ②④ 段 A）。
/// 夹具**一律在 `tempfile` 里内联写 TOML**：不许让用例去读仓根那份 `llm.toml.example`，
/// 否则"改了样例文件"会静默改掉测试输入（样例给操作员看，夹具给判据看，两个主语）。
fn write_cfg(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}
/// 形状合法、**一家都不出网**的两座配置：preset 表上的前两名，`secret_ref` 是 `箱:格` 的占位形状。
/// key 的原文一个字节都不在这里（也不在仓里任何地方）。
fn cfg_two_preset_seats() -> String {
    String::from(
        "[[providers]]\nprovider = \"deepseek\"\nmodel = \"deepseek-chat\"\nsecret_ref = \"kms:deepseek_key\"\n\n\
         [[providers]]\nprovider = \"sensenova\"\nmodel = \"sensenova-6.8-flash-lite\"\nsecret_ref = \"kms:sensenova_key\"\n",
    )
}

#[test]
fn missing_replies_is_exit_3_and_the_usage_string_names_both_flags() {
    // §4.8 判据那一格点名的**用例语义反转**（原句留着）：`--replies` 从必填变可选之后，
    // 缺它不再是用法错误，而是"两条座位来源都没给"的运维失败。
    // 原来这一条断的是 P-34 + P-06：clap 自己那个 2 被 `operational()` 改写成 3，用法串点名必填旗标。
    // 今天仍然退 3、仍然要点名两个来源，但**主语换了**：
    // ① 那一串不再来自 `invalid invocation` 那一支（它是用法错误专用，走的是 clap），
    //    而是来自 hub 自己的一句"没给来源"；② 它点的两个旗标是 `--replies` 与 `--llm-config`。
    // 断言方向掉头的那一刻这条必须能红：实现前它是红的（clap 仍然拦成用法错误、仍然没有 --llm-config）。
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["cycle", "once", "--data-dir", dir.path().join(DATA).to_str().unwrap()]);
    assert_eq!(code(&out), 3, "两条来源都不给 ⇒ 运维失败（3），不是用法错误（clap 的 2 被改写那一族）");
    let err = stderr_text(&out).to_lowercase();
    assert!(!err.contains("invalid invocation"),
        "缺 `--replies` 还被当成用法错误 ⇒ 反转没落地：{err}");
    assert!(err.contains("--replies"), "失败串没点 --replies：拼错的人以为它是可选的：{err}");
    assert!(err.contains("--llm-config"), "失败串没点另一条来源 --llm-config：他会以为 live 不需要配置：{err}");
    assert!(err.contains("cycle once"), "失败串没点 cycle once：他会一直以为 `cycle` 不存在：{err}");
    assert_eq!(out.stdout, b"", "运维失败不许有读数");
}

#[test]
fn misspelled_subcommand_shows_the_same_usage_string() {
    let out = run(&["cycel", "once"]);
    assert_eq!(code(&out), 3);
    assert!(stderr_text(&out).contains("cycle once"), "{}", stderr_text(&out));
    assert_eq!(out.stdout, b"");
}

#[test]
fn a_bad_toml_config_is_a_different_failure_than_no_config() {
    // §4.8 ② 末段那三件里的第三件：`rc_llm_cfg::load()` 的 `Ok(None)` 与 `Err` 两条分支
    // 在 hub 这一侧必须是**两种后果**（与 `--replies` 那条"刻意不给 default_value"同一条理由）。
    // 两种都不许静默继续：都退 3、stdout 都空，但类别串必须不同——操作员要能从这一句分清
    // "这台机器还没配" 与 "配了但坏了"，后者的动作是改文件，前者的动作是补文件。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let tmp_component = d.file_name().unwrap().to_str().unwrap().to_owned();
    // ① 配坏了：TOML 本体就解析不动（`load()` 的 Err 那一支）。
    let bad = write_cfg(d, "bad.toml", "providers = [ this is not toml");
    let out = cycle_raw(d, &["--llm-config", bad.to_str().unwrap()]);
    assert_eq!(code(&out), 3, "坏 TOML 不许静默继续：{}", stderr_text(&out));
    let malformed = stderr_text(&out);
    assert!(malformed.contains("llm config is malformed"), "报的不是'配坏了'那一类：{malformed}");
    assert_eq!(out.stdout, b"", "运维失败不许有读数");
    // ② 缺文件：`load()` 的 `Ok(None)`（"这台机器还没轮到配 live"）。
    let gone = d.join("nope.toml");
    let out = cycle_raw(d, &["--llm-config", gone.to_str().unwrap()]);
    assert_eq!(code(&out), 3, "缺配置也不许静默继续：{}", stderr_text(&out));
    let missing = stderr_text(&out);
    assert!(missing.contains("no llm reply source"), "缺文件被报成了别的一类：{missing}");
    assert_eq!(out.stdout, b"");
    // 两种后果的**差**才是这一条的全部理由：同一条串 = 反转没落地、又收回成一种静默。
    assert!(!missing.contains("malformed"), "缺文件与坏 TOML 共用了同一句：{missing}");
    assert!(!malformed.contains("no llm reply source"), "两句互换了主语：{malformed}");
    // 守卫 12：两条都不许带配置文件的路径（临时目录那一格就是路径证人）。
    for text in [malformed, missing] {
        assert!(!text.contains(&tmp_component), "运维失败外带了配置文件的绝对路径：{text}");
        assert!(!text.contains("bad.toml"), "运维失败外带了操作员给的文件名：{text}");
    }
}

#[test]
fn an_unknown_preset_seat_fails_at_startup_without_the_config_path() {
    // 设计节 1 那条接缝的第一格：`llm.toml` 里的 provider 查不到 preset 键 ⇒ **启动即响亮失败**，
    // 不许回落默认（回落出来的请求会打到一条没人要它打的路上，`Live::new` 的注释同律）。
    // "启动即"在这条上的可证形式是：一个读数都没打印（`CycleOut` 那十七格一格都不许出现）、
    // 库根本没开。把这一支换成"查不到就跳过那一座" ⇒ 本条红在退出码 0。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let tmp_component = d.file_name().unwrap().to_str().unwrap().to_owned();
    let cfg = write_cfg(d, "llm.toml",
        "[[providers]]\nprovider = \"not-a-preset\"\nmodel = \"nope-not-on-the-table\"\nsecret_ref = \"kms:k\"\n");
    let out = cycle_raw(d, &["--llm-config", cfg.to_str().unwrap()]);
    assert_eq!(code(&out), 3, "preset 查不到却跑成功了 ⇒ 静默回落：{}", stderr_text(&out));
    let err = stderr_text(&out);
    assert!(err.contains("preset table"), "报的不是那一条类别：{err}");
    assert!(!err.contains("no llm reply source"), "配置明明给了却被报成没给：{err}");
    assert_eq!(out.stdout, b"", "启动即失败不许有读数");
    assert!(!err.contains(&tmp_component), "失败消息外带了配置文件的绝对路径：{err}");
    assert!(!err.contains("not-a-preset"), "失败消息外带了配置里的值：{err}");
}

#[test]
fn the_shipped_example_is_a_seat_that_reaches_the_secret_hand() {
    // §4.8 末段那一件"仓内一份 `llm.toml.example`"要是没人解析它，它就是一张美图：
    // 抄不动的例子比没有例子更糟（操作员照抄 ⇒ 得到一条与配置无关的失败，然后怀疑 hub）。
    // 这一条把仓根那一份真解析一遍：它必须**过得了** `load()`、**过得了** preset 那一关，
    // 一路走到裁定 A 那一句（拿不到 key）才停 —— 停下之后 stdout 仍然一个字节都没有。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../llm.toml.example");
    let out = run_env(d, &["--llm-config", example], &[("RECOLLECT_DEVICE_ID", "dev-laptop")]);
    assert_eq!(code(&out), 3, "{}", stderr_text(&out));
    let err = stderr_text(&out);
    assert!(err.contains("llm secret"),
             "仓根的例子没能走到「拿不到 key」那一步 ⇒ 它自己坏了或名字不在表上：{err}");
    assert!(!err.contains("malformed"), "例子被 load() 拒了：抄它的人只会得到「配坏了」：{err}");
    assert!(!err.contains("preset table"), "例子里某个 (provider, model) 不在 preset 表上：{err}");
    assert_eq!(out.stdout, b"", "非 TTY 一律不提示输入：例子跑得通也不许开口问");
}

#[test]
fn device_identity_from_the_environment_decides_which_loud_failure() {
    // §4.8 ①：hub 的第一条环境变量读取，名字逐字 `RECOLLECT_DEVICE_ID`（判据 35 的那个命名空间），
    // 取到的值立刻过既有的 `device_subject` —— 坏形状与没给**同归"缺身份"**。
    // 这一条同时是那三条红线的执法面：非 TTY 不提示输入（stdout 必须零字节）、
    // 不崩溃（退出码是 3 那一族，不是 101）、不许开出第三个身份名字。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let cfg = write_cfg(d, "llm.toml", &cfg_two_preset_seats());
    let cfg = cfg.to_str().unwrap();
    // ① 合法形状的身份 ⇒ 过得了 `device_subject` ⇒ 往前走到裁定 A 那一句（拿不到 key）。
    let ok = run_env(d, &["--llm-config", cfg], &[("RECOLLECT_DEVICE_ID", "dev-laptop")]);
    assert_eq!(code(&ok), 3, "{}", stderr_text(&ok));
    let err = stderr_text(&ok);
    assert!(err.contains("llm secret"), "合法身份下报的不是'拿不到 key'那一类：{err}");
    assert!(!err.contains("device identity"), "合法形状被当成缺身份了：{err}");
    // ② 没给 ⇒ "缺身份"。环境变量从子进程里摘掉，这条才不受本机 ambient 值的摆布。
    let none = run_env(d, &["--llm-config", cfg], &[]);
    assert_eq!(code(&none), 3, "{}", stderr_text(&none));
    let err = stderr_text(&none);
    assert!(err.contains("device identity missing"), "缺身份那一句没报出来：{err}");
    assert!(err.contains("RECOLLECT_DEVICE_ID"), "缺身份却没点名那个变量名，操作员不知道该设哪一格：{err}");
    assert_eq!(none.stdout, b"", "非 TTY 一律不提示输入：stdout 必须一个字节都没有");
    // ③ 坏形状 ⇒ 与没给同归"缺身份"，**不是**崩溃（101）、**不是**提示。
    // 这一支就是判据 35 那一族的阳性对照：走的是 `device_subject` 的既有形状规则，不是新写的校验。
    for bad in ["  ", "a/b/c", "C:\\Users\\me", "dev laptop"] {
        let out = run_env(d, &["--llm-config", cfg], &[("RECOLLECT_DEVICE_ID", bad)]);
        let err = stderr_text(&out);
        assert_eq!(code(&out), 3, "坏形状的身份把 hub 打崩了（{bad}）：{err}");
        assert!(err.contains("device identity missing"), "坏形状没归到缺身份那一类（{bad}）：{err}");
        assert!(!err.contains("llm secret"), "坏形状却走到了拿 key 那一步（{bad}）：{err}");
        assert_eq!(out.stdout, b"", "坏形状也不许提示输入（{bad}）");
    }
}

#[test]
fn giving_replies_takes_the_replay_seat_even_when_a_config_is_present() {
    // §4.8 ② 的优先序：给了 `--replies` ⇒ 回放座位，配置那一条支**根本不进**。
    // 失效形状是把优先序反过来（或"两边都读、坏配置挡住一切"）：那时这一条红在退出码 3。
    // 这一条同时是 `Metered` 拆成"账本 + 来源"之后的接线证人：`llm_job` 的账（`extract` 那一行）
    // 由账本出、回复由来源出，两边缺一格这一条就红——账本还在但来源没递进去时，
    // 抽取拿不到回复 ⇒ `Timeout` ⇒ 3；来源递进去了但账本没接上时 ⇒ `llm_job_ids` 是空表。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000), turn("agent", "好的，跑过了", 1_001)]);
    let u = uid("human", "上线前要先跑测试", 1_000);
    let replies = d.join("replies.json");
    std::fs::write(&replies, Value::Array(vec![
        reply(&extract_ok(&u)), reply(YES), reply(YES), reply(YES), reply(DERIVE_QUIET),
    ]).to_string()).unwrap();
    // 配置里那一家查不到 preset 键：优先序对了就根本不会被读。
    let cfg = write_cfg(d, "llm.toml",
        "[[providers]]\nprovider = \"not-a-preset\"\nmodel = \"nope\"\nsecret_ref = \"kms:k\"\n");
    let dd = d.join(DATA);
    let out = run(&["cycle", "once", "--replies", replies.to_str().unwrap(),
                    "--llm-config", cfg.to_str().unwrap(), "--data-dir", dd.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "给了 --replies 却被配置挡住：{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((v["turns_scanned"].as_i64().unwrap(), v["rows_written"].as_i64().unwrap()), (2, 1));
    assert!(llm_job_ids(d).contains(&"extract".to_owned()),
            "账本没接到这一轮：{:?}", llm_job_ids(d));
    assert_eq!(llm_calls_total(d), 5, "P-33 的算术在拆分之后仍然成立：脚本五条、调用五次");
}

#[test]
fn empty_store_runs_and_calls_nothing() {
    // P-19：空库不是错误。空脚本本身就是探针——任何一次 LLM 调用都会拿到 Timeout ⇒ 3。
    let dir = tempfile::tempdir().unwrap();
    let out = cycle(dir.path(), &[]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((v["turns_scanned"].as_i64().unwrap(), v["rows_written"].as_i64().unwrap()), (0, 0));
    assert_eq!(llm_job_ids(dir.path()), Vec::<String>::new(),
               "一次都没开口 ⇒ 一行 `llm_calls` 都不许落账（收尾覆盖写的 quota 格子不在此列）");
}

/// S3a 阴性（收尾那一半）：一次没触顶的运行收尾必须把这一格写成 0，并且覆盖掉上一次留下的 1
/// （`set_one` 是覆盖不是累加）。
///
/// 这条**不**负责杀"删掉收尾那一句"（那一种变异由 `quota_exhausted_reply_sets_the_cell_and_still_exits_3`
/// 杀：清 0 会把这格写回 0，所以只有"触顶那一轮必须留 1"能看见收尾）。它杀的是另外两型：
/// 收尾写成累加（1 + 0 还是 1 ⇒ 红）与收尾搬到清 0 之前（被清 0 覆盖 ⇒ 红）。
#[test]
fn a_completed_run_overwrites_a_stale_quota_cell_with_zero() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    {
        let root = open(d);
        counters::set_one(root.ledger(), "llm_job", "cycle",
                          counters::QUOTA_STOPPED_METRIC, 1, 1_000).unwrap();
    }
    // 空库 + 空脚本：这一轮一次模型都不开 ⇒ 没发生过配额事件。
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(quota_stopped_for(d, "cycle"), Some(0),
        "上一次的 1 必须被这一轮的收尾覆盖成 0（覆盖语义，不是累加）");
}

/// S3a 阴性（评审 I-1 的 clear 半边）：cycle 的早退路径也必须把上一次的 1 清掉。
///
/// 为什么挑"轮转态读失败"这一支：cycle 的收尾是无条件的，正常路径上 clear 与 record 都写 0，
/// 分不开。清 0 那一行只对**清 0 之后、收尾之前**就返回的那几支有意义，而它们在 CLI 面上唯一
/// 能被夹具逼出来的一支就是这里——给它一台"有身份、但它那一行读不出来"的机器。
/// 变异电池：删掉 `run_cycle_with_slots` 开头那句 `clear_quota_stopped` ⇒ 这一格留在 1 ⇒ 红。
#[test]
fn an_early_exit_clears_the_previous_runs_quota_cell() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    {
        let root = open(d);
        counters::set_one(root.ledger(), "llm_job", "cycle",
                          counters::QUOTA_STOPPED_METRIC, 1, 1_000).unwrap();
        // 轮转态那一行读不出来：整数列里放文本（SQLite 不做静态类型检查，`read` 的
        // `r.get::<_, i64>` 因此当场失败）⇒ `FAULT_LLM_STATE_READ` 早退。
        root.ledger().conn().execute(
            "INSERT INTO llm_state(machine, slug, probe_due_at_ms, probe_ok, probe_failures, written_at_ms)
             VALUES('dev-laptop','m',1000,0,0,'oops')", []).unwrap();
    }
    let replies = d.join("replies.json");
    std::fs::write(&replies, "[]").unwrap();
    let out = run_env(d, &["--replies", replies.to_str().unwrap()],
                      &[("RECOLLECT_DEVICE_ID", "dev-laptop")]);
    assert_eq!(code(&out), 3, "轮转态读失败是运维故障：{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("llm rotation state unreadable"),
        "早退的原因必须是轮转态那一支：{}", stderr_text(&out));
    assert_eq!(quota_stopped_for(d, "cycle"), Some(0),
        "早退的运行也必须把上一次的 1 清掉（否则 status 会一直报上次的触顶）");
}

#[test]
fn happy_path_prints_seventeen_readouts_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000), turn("agent", "好的，跑过了", 1_001)]);
    // 五条回复 = 1 次抽取 + 3 票审核 + 1 次推导。判同在这一轮**不开口**：库里只有一条卡片，
    // `scored_siblings` 给空表 ⇒ 任务 7 定的 `continue` 那一支。
    let u = uid("human", "上线前要先跑测试", 1_000);
    let out = cycle(d, &[reply(&extract_ok(&u)), reply(YES), reply(YES), reply(YES), reply(DERIVE_QUIET)]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v.as_object().unwrap().len(), 17, "十七个读数一个不多一个不少：{v}");
    for k in ["schema_version", "turns_scanned", "rows_written", "batches_dropped",
              "quota_exhausted", "halted", "merged", "suppressed",
              "derive_rows", "derive_rejected", "derive_dropped",
              "derive_mentions_written", "derive_mentions_unresolved", "derive_mentions_missing",
              "entities_seen", "kinds_rejected", "aliases_dropped"] {
        assert!(v.get(k).is_some(), "少一格 {k}：{v}");
    }
    // T9-4 的第一格：这一格以前只断过"键在场"。等式右边是生产侧写进 `CycleOut` 的那个值
    // （`CapReport::schema_version()` 的返回值，本机读数 1），与 `import_cli`/`rebuild_cli`
    // 那两条面上已有的写法同形——断值不断形状，中心面的三个读数版本才算一把尺子。
    assert_eq!(v["schema_version"], 1, "cycle 的读数版本与另外两个运维面漂了：{v}");
    assert_eq!((v["turns_scanned"].as_i64().unwrap(), v["rows_written"].as_i64().unwrap()), (2, 1));
    assert_eq!((&v["quota_exhausted"], &v["halted"], &v["merged"], &v["suppressed"]),
               (&json!(false), &json!(false), &json!(0), &json!(0)));
    // S3a 阴性：没触顶的一轮也必须落一格 0（"跑过且没触顶"），而不是把这一格留空——
    // 留空会被读成 null（"还没有运行记录"），status 与 doctor 的三态就分不开了。
    assert_eq!(quota_stopped_for(d, "cycle"), Some(0),
        "跑完的一轮不许把触顶格留空：null 会冒充'还没跑过'");
    // 下面这两格截至段 B 前原先全 workspace 只有"键在场"（上面那个清单）与丢批那条**断 0** 的值断言，
    // 于是 `run_cycle_once` 里 `out.derive_rows += dr.rows;` 与 `out.derive_rejected += dr.rejected;`
    // 两行整句删掉全 workspace 照绿。而 DERIVE_QUIET 的答复上头那句注释早就写明它读出来是
    // "`derive_rows = 0`、影子行数不变、`derive_rejected = 1`、游标照推"——注释说的那个
    // **非零**读数此前一条用例都没钉住。这里补的就是那一条。
    assert_eq!((&v["derive_rows"], &v["derive_rejected"]), (&json!(0), &json!(1)),
        "环节数不足 ⇒ parse 条目级拒收，不落库但计 rejected：{v}");
    assert_eq!(unreviewed(d), 0, "三票全真 ⇒ 那一行不再是 unreviewed（审核真的被装配上了）");
    // 守卫 12：出口只有计数与身份，正文与路径一个字节都不许多。
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains(CANARY), "输出含正文：{text}");
    let tmp_component = d.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "输出含临时目录组件：{text}");
    assert_eq!(llm_job_ids(d), vec!["audit".to_owned(), "derive".to_owned(), "extract".to_owned()],
        "计数器主语只有开过口的家族");
    assert_eq!(llm_calls_total(d), 5, "P-33 的算术：脚本五条、调用五次，一条不多一条不少");
    // T9-3：cycle 写完行之后影子必须跟着。十七格里没有影子格，所以这一拍的唯一证人是**索引面自己**
    // 的读数——删掉 `rc_index::rebuild` 调用，cycle 的十七格一个字节都不变，只有这里读得到。
    // 先断 `rows_indexed` 的精确值 1（本轮那一行）：它非零、又等于本轮该被索引的行数，
    // 所以"重建被挪到抽取之前"（读成 0）与"根本没重建"（marker 缺席 ⇒ `.as_u64()` 给 None）都红。
    let st = run(&["index", "status", "--data-dir", d.join(DATA).to_str().unwrap()]);
    assert_eq!(code(&st), 0, "cycle 之后连影子读数都取不到：{}", stderr_text(&st));
    let sv = stdout_json(&st);
    assert_eq!(sv["rows_indexed"].as_u64(), Some(1), "本轮一行 ⇒ 影子一行，且这个数非零：{sv}");
    assert_ne!(sv["rebuilt_at_ms"], json!(null), "marker 不在 = cycle 末尾没重建过这一层：{sv}");
    // 上面那格是**共证人不是独立判据**：`index status` 在 marker 缺席时整份 JSON 只剩
    // `rebuilt_at_ms: null` 一个键，于是"根本没重建"这一形先红的是 `rows_indexed` 那格。
    // 留它的理由是说得出名字的那一格：`rebuilt_at_ms` 是 status 面向运维渲染的字段
    // （`rc-agent::tools::status` 就带它），而"这一层什么时候重建过"正是查影子时要看的东西。
}

#[test]
fn 两环节的链答复经装配面真落库并带着影子抬高() {
    // 工单 §5 步骤 3（批次③ 段 C-2）：`DERIVE_CHAIN` 走一次真进程 cycle，把"装配面真调用
    // `put_chains`"钉成进程面读数——此前本文件对 chains 只有"拒收 / 落 0 行"两格，
    // 落库那一支在 hub 黑盒面上零证人。
    // 白盒播种两条**活实体**：P-30 定过实体不进审核游标（`unreviewed_rows` 只吃三张候选表），
    // 而实体在推导池里（`rows_since` 走四张表）⇒ 批 = 两行 = 答复里的 r1/r2 有着落；
    // 本轮 LLM 恰好开口一次（与 `cycle_把重影作业挂在全量扫描那一支上` 同一条推理）。
    // 变异电池 M5：把 `derive.rs` 的 `put_chains` 调用摘掉 ⇒ 本条红在 `derive_rows` 那格。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    {
        let root = open(d);
        let m = root.memory();
        let pv = || vec![rc_memory::Provenance { turn_uid: "t-hub-chain".into(),
                                                anchor_role: rc_memory::AnchorRole::Human }];
        let ec = |title: &str| rc_memory::EntityContent {
            kind: "person".into(), title: title.into(),
            scope: rc_memory::Scope { kind: rc_memory::ScopeKind::Person, key: "p-hub-chain".into() },
            aliases: vec![] };
        rc_memory::put_entity(m, &ec("Hub链甲"), &pv(), 1).unwrap();
        rc_memory::put_entity(m, &ec("Hub链乙"), &pv(), 1).unwrap();
    }
    let out = cycle(d, &[reply(DERIVE_CHAIN)]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((&v["derive_rows"], &v["derive_rejected"], &v["derive_dropped"]),
               (&json!(1), &json!(0), &json!(0)),
               "装配面真调用 `put_chains` 的那一格读数：{v}");
    // 库面证人：一行因果头行 + 两行带序环节（basis 那一跳与 r1 撞主键被忽略，不重复成行）。
    let root = open(d);
    let m = root.memory();
    let count = |sql: &str| -> i64 { m.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT count(*) FROM card WHERE kind='causal'"), 1, "链头行必须在场");
    assert_eq!(count("SELECT count(*) FROM provenance WHERE role='link'"), 2,
        "两环节 = 两行带 ord 的 link 行（C-3：撞主键的 basis 不覆盖环节行）");
    let head: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    let ords: Vec<i64> = {
        let mut st = m.conn().prepare(
            "SELECT ord FROM provenance WHERE mem_table='card' AND mem_id=?1 \
             AND role='link' ORDER BY ord").unwrap();
        st.query_map([head.as_str()], |r| r.get(0))
            .unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(ords, vec![0, 1], "环节序号从 0 起连续——「顺序读得回来」在装配面上的形状");
    // 影子跟着抬：cycle 末尾的重建要把链头行一起带进索引面。`rows_indexed` 的精确值是
    // **实测**写死的（两实体 + 链头 = 3），不照抄 happy_path 的 1——抄那个数就钉不出新那一行。
    let st = run(&["index", "status", "--data-dir", d.join(DATA).to_str().unwrap()]);
    assert_eq!(code(&st), 0, "cycle 之后连影子读数都取不到：{}", stderr_text(&st));
    let sv = stdout_json(&st);
    assert_eq!(sv["rows_indexed"].as_u64(), Some(3),
        "两实体 + 一条链头 = 影子三行；链头没进影子就是重建那层漏了它（实测值）：{sv}");
    assert_eq!(llm_calls_total(d), 1, "本轮只有推导开了一次口：抽取无新批、审核不见实体、判同无邻居");
}

#[test]
fn mentions_only_reply_flows_through_the_assembly_readouts() {
    // 批次 M 段 C：段 B 落在 `DeriveOutcome` 的三个 mention 计数器在**装配面**上的证人。
    // 此前那三格在 hub 黑盒面上零读数——`derive_mentions_written`/`unresolved`/`missing` 三个
    // `CycleOut` 格子整段删掉全 workspace 照绿。夹具与 `两环节的链答复经装配面真落库并带着影子抬高`
    // 同形：白盒播种一条活实体（实体在推导池里 ⇒ 批 = 一行 = 答复里的 r1 有着落），
    // 本轮 LLM 恰好开口一次（抽取无新批、审核不见实体、判同无邻居）。
    // 变异电池 M-C3：删 `out.derive_mentions_written += dr.mentions_written;` ⇒ 本条红在那一格。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    {
        let root = open(d);
        let m = root.memory();
        let pv = || vec![rc_memory::Provenance { turn_uid: "t-hub-mention".into(),
                                                anchor_role: rc_memory::AnchorRole::Human }];
        let ec = |title: &str| rc_memory::EntityContent {
            kind: "person".into(), title: title.into(),
            scope: rc_memory::Scope { kind: rc_memory::ScopeKind::Person, key: "p-hub-mention".into() },
            aliases: vec![] };
        rc_memory::put_entity(m, &ec("Hub证甲"), &pv(), 1).unwrap();
        // 批里第二行：消费行（`r2`）。mention 从它发出才不被 I6 的自指闸门吞掉。
        put_card(m, &FactContent {
                kind: "decision".into(), title: "Hub证甲 排期".into(),
                body: "Hub证甲说下周上线".into(),
                scope: rc_memory::Scope { kind: rc_memory::ScopeKind::Project,
                                          key: "p-hub-mention".into() },
                valid_to_ms: None },
            &pv(), 2).unwrap();
    }
    let out = cycle(d, &[reply(DERIVE_MENTION_ONLY)]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    // 读数面：mention 实际插入 1 行（进 `derive_rows`）、零未解析、零缺键。
    assert_eq!((&v["derive_rows"], &v["derive_mentions_written"]), (&json!(1), &json!(1)),
        "装配面没把 mention 的写入数送到读数面：{v}");
    assert_eq!((&v["derive_mentions_unresolved"], &v["derive_mentions_missing"]),
               (&json!(0), &json!(0)),
               "一条合法 mention 不该被读成未解析或缺键：{v}");
    // 库面证人：那一行 mention 真的落库了（读数自报与库内形状必须同一件事）。
    let root = open(d);
    let m = root.memory();
    let n: i64 = m.conn().query_row("SELECT count(*) FROM memory_mention", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "答复里一条合法 mention ⇒ 库里恰好一行");
    assert_eq!(llm_calls_total(d), 1, "本轮只有推导开了一次口：抽取无新批、审核不见实体、判同无邻居");
}

#[test]
fn contract_discard_skips_dedupe_and_still_audits() {
    // P-40 + P-32 两条判据在这一条里同时跑（它们共用一个库状态，拆开就得各造一遍两轮）。
    // 第一轮：抽取成功（写 1 行、推一格游标），审核的第一票拿到 Timeout ⇒ exit 3，
    // 而 `audit_row` 的"三票全回来才动笔"让那一行**留在 unreviewed**（任务 8 的 P-36）。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000)]);
    let u1 = uid("human", "上线前要先跑测试", 1_000);
    let out = cycle(d, &[reply(&extract_ok(&u1)), fail("timeout")]);
    assert_eq!(code(&out), 3);
    assert_eq!(attempts(d), 0, "LLM 故障族不许计 attempts（任务 1 的 `counts_attempt()`）");
    assert_eq!(unreviewed(d), 1);
    let cursor_after_first = cursor_uid(d);

    // 第二轮：新导入 ⇒ 新批次，回复是缺键的契约违规件。
    import(d, "b", vec![turn("human", "第二个批次的第一句话", 2_000), turn("agent", "第二句", 2_001)]);
    // 四条回复 = 1 次抽取（契约拒） + 3 票审核（第一轮那一行）。
    let out = cycle(d, &[reply(CONTRACT_BAD), reply(YES), reply(YES), reply(YES)]);
    assert_eq!(code(&out), 2, "整批丢弃是 2：与 import 的整批拒收同一条语义（§2.19）");
    let v = stdout_json(&out);
    assert_eq!((v["batches_dropped"].as_i64().unwrap(), v["merged"].as_i64().unwrap()), (1, 0));
    // 判同没开口 ⇒ `llm_job/dedupe` 那一行**根本不存在**（零增量的行不创建，与 import 同一条纪律）。
    // 三支分开断而不是断一个整表：`extract` 与 `audit` 在场证明装配没漏装（失效形状 ①）。
    // 下面那条 `dedupe` 缺席**不是 P-40 的判据**——这一条的第一轮只写一行、判同本来就不开口，
    // 把 `main.rs` 的闸门拆掉它照旧绿（复审实测：拆闸后既有的九条全绿）。它真正的执法对象是
    // "零增量的行不许创建"：拆掉 `charge_counters` 里那句 `if n == 0 { continue; }` 它就红。
    // P-40 的判据住在 `a_contract_discard_after_a_dedupe_round_adds_no_dedupe_calls` 的**增量**那一格。
    let ids = llm_job_ids(d);
    assert!(ids.contains(&"audit".to_owned()) && ids.contains(&"extract".to_owned()),
            "计数器主语少了开过口的家族：{ids:?}");
    assert!(!ids.contains(&"dedupe".to_owned()), "判同在这一轮开口了 = P-40：派生表里躺的是上一批的行");
    assert!(cursor_uid(d) == cursor_after_first, "游标一动不动 = 同一批下一轮还会被选中（活锁）");
    assert_eq!(attempts(d), 1, "契约丢弃计 attempts：那是运维看见活锁的唯一一格");
    assert_eq!(unreviewed(d), 0, "审核在这一轮照跑：它的游标是库内的 verdict 列，与派生表无关");
    // 两轮各喂满：2 + 4 = 6 次，**再加一次** = 7。加的那一次是 §4.8 ③ 段 B 的读数：
    // 递进 `run_cycle_once` 的那一支今天确实是 `Rotating`（单座位也是一个槽位），
    // 而它自己的 Timeout 语义是"先原样重试一次"（`rc-llm::rotating` 的 `VISITS_PER_CALL`，
    // Task 6 封板的形状）。脚本耗尽的那一次重试**真开过口**（`Seat` 又 pop 了一次、又记了一次），
    // 所以 P-33 这条算术要把它的账照记 —— 计数器记的是调用次数，不是 cycle 轮数。
    assert_eq!(llm_calls_total(d), 7, "计数器记的是调用次数，不是 cycle 轮数");
}

#[test]
fn a_near_duplicate_pair_makes_dedupe_speak_and_merge_land() {
    // 失效形状 ①（本任务自称要拦的那一族）第一次真的有牙：以前九条用例整场只有一条卡片，
    // `scored_siblings` 恒空 ⇒ 判同一次都不开口 ⇒ 删掉"判同 + 并条"那 13 行也全绿。
    // 家族序列 extract → dedupe → dedupe → audit ×3，合计 6 次调用：
    // 两条卡片互为本轮探针的邻居（两个方向都要问），并条之后只剩活下来的那一行可审。
    // 第二次判定答 `same:false`，于是计划只落一条 `MergePlan`——两个方向都答"留 r1"会写出
    // 互相顶掉的两条计划，那是 `apply_merges` 逐条重读要拒的形状。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000), turn("agent", "上线前要先验证一遍", 1_001)]);
    let u1 = uid("human", "上线前要先跑测试", 1_000);
    let u2 = uid("agent", "上线前要先验证一遍", 1_001);
    let out = cycle(d, &[reply(&dup_cards(&u1, &u2)),
                         reply(JUDGE_KEEP_PROBE), reply(JUDGE_NOT_SAME),
                         reply(YES), reply(YES), reply(YES), reply(DERIVE_QUIET)]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((v["turns_scanned"].as_i64().unwrap(), v["rows_written"].as_i64().unwrap()), (2, 2));
    assert_eq!(v["schema_version"], 1, "读数版本在这一支上也得与另外两个运维面一致：{v}");
    // `merged` 从"全档只能读成 0"变成能被点亮的格子（本轮没有抑制账，所以 `suppressed` 是 0）。
    assert_eq!((&v["merged"], &v["suppressed"], &v["batches_dropped"]),
               (&json!(1), &json!(0), &json!(0)));
    assert_eq!((&v["quota_exhausted"], &v["halted"]), (&json!(false), &json!(false)));
    assert_eq!(llm_job_ids(d),
               vec!["audit".to_owned(), "dedupe".to_owned(), "derive".to_owned(), "extract".to_owned()],
               "整表等式第一次把 dedupe 的**在场**当成可红的事");
    assert_eq!(llm_calls_for(d, "dedupe"), 2, "两个方向的探针各问一次");
    assert_eq!(llm_calls_total(d), 7, "P-33 的算术：脚本七条、调用七次");
    assert_eq!(merge_ledger(d), (2, 1, 1, 0), "库里两条卡片、一条被顶掉、指针指向另一条活行");
    assert_eq!(unreviewed(d), 1, "输的那一行不再进审核游标（被顶掉），活下来的那一行已经审完");
    // 守卫 12 在这一支上重跑一遍：并条把两行的标题都读进过内存，出口仍然只许有计数。
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains(CANARY), "输出含正文：{text}");
    let tmp_component = d.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "输出含临时目录组件：{text}");
}

#[test]
fn a_contract_discard_after_a_dedupe_round_adds_no_dedupe_calls() {
    // P-40 那道闸门的反例。闸门以前不可证伪：上一轮判同根本没开口，把 `!contract_discarded`
    // 改成 `if true` 也全绿（本轮在同一棵树上跳过新增三条、只跑原有九条复现了那个读数）。
    // 这一条把第一轮改成**真的会开口**的夹具，闸门第一次能红。
    // 第二轮只喂一条回复（那份契约违规件）：闸门掉了 ⇒ 判同拿着 `rc_extract_row` 里那批上一轮的行
    // 重新开口（被顶掉那一行仍能被 `lookup` 成探针，它的邻居是活行）⇒ 计数器多一格，且脚本当场
    // 耗尽 ⇒ Timeout ⇒ stderr 那句 `llm timeout — retry this round later`、退出码 3。
    // 实测的第一现场是下面那条增量（3 对 2），退出码那一格是第二证人（3 对 2）。
    // 判据写成**增量**而不是"第二轮的 ids 不含 dedupe"：计数器行第一轮已经创建，
    // 那一形在这条夹具下恒真、挡不住任何事。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000), turn("agent", "上线前要先验证一遍", 1_001)]);
    let u1 = uid("human", "上线前要先跑测试", 1_000);
    let u2 = uid("agent", "上线前要先验证一遍", 1_001);
    let first = cycle(d, &[reply(&dup_cards(&u1, &u2)),
                           reply(JUDGE_KEEP_PROBE), reply(JUDGE_NOT_SAME),
                           reply(YES), reply(YES), reply(YES), reply(DERIVE_QUIET)]);
    assert_eq!(code(&first), 0, "{}", stderr_text(&first));
    assert_eq!(llm_calls_for(d, "dedupe"), 2, "反例的前提：第一轮判同确实开过口，闸门才有东西可挡");
    let cursor_after_first = cursor_uid(d);

    // 第二轮：新导入 ⇒ 新批次，回复是一份缺键的契约违规件，**一条都不多喂**。
    import(d, "b", vec![turn("human", "第二个批次的第一句话", 2_000), turn("agent", "第二句", 2_001)]);
    let out = cycle(d, &[reply(CONTRACT_BAD)]);
    // 增量排在退出码之前：闸门掉了的第一现场就是这一格从 2 变 3，它是 P-40 唯一直接的读数证人。
    assert_eq!(llm_calls_for(d, "dedupe"), 2, "P-40：丢弃轮一个 dedupe 增量都不许有");
    assert_eq!(code(&out), 2, "丢弃轮重新开口就会把脚本耗尽 ⇒ 3：{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((v["batches_dropped"].as_i64().unwrap(), v["merged"].as_i64().unwrap()), (1, 0));
    assert_eq!(llm_calls_total(d), 8, "第二轮只烧掉抽取那一次调用：7 + 1");
    assert!(cursor_uid(d) == cursor_after_first, "游标一动不动 = 同一批下一轮还会被选中（活锁）");
    assert_eq!(attempts(d), 1, "契约丢弃计 attempts：那是运维看见活锁的唯一一格");
    assert_eq!(llm_job_ids(d),
               vec!["audit".to_owned(), "dedupe".to_owned(), "derive".to_owned(), "extract".to_owned()],
               "主语清单不因丢弃轮而多出一格");
}

#[test]
fn a_resupplied_dead_row_is_counted_in_suppressed() {
    // T9-4 的第二格。`suppressed` 以前只被断过 0 ⇒ 把 `out.suppressed += o.suppressed` 整句删掉
    // 全档照旧绿（复审点名的那一格推理型缺口，本条把它做成可红的）。
    // 形状 = 票 1 投 false ⇒ 审核写下未解决的退场账；第二轮抽取**重新产出同一条内容** ⇒
    // `put_batch` 的写前查账命中 ⇒ 这一格读到 1，而 `rows_written` 读到 0。
    // 归因不住在这一格：它住 `retirement.reason`（下面直接读那张表），这一格只有算术。
    // 家族序列：第一轮 extract → audit ×3（一条卡片没有邻居 ⇒ 判同不开口），第二轮只剩 extract。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000)]);
    let u1 = uid("human", "上线前要先跑测试", 1_000);
    let out = cycle(d, &[reply(&extract_ok(&u1)), reply(NO), reply(YES), reply(YES)]);
    assert_eq!(code(&out), 0, "票 1 不成立是一次成功的审核，不是运维失败：{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((&v["rows_written"], &v["suppressed"], &v["merged"]),
               (&json!(1), &json!(0), &json!(0)));
    assert_eq!(retirement_reasons(d), vec!["audit:vote1".to_owned()],
        "消音的归因在退场账那一侧：CycleOut 的这一格只有计数");

    // 第二轮：新批次、同一条内容（`compute_mem_id` 哈希六段——表前缀、kind、归一标题、归一正文、
    // 归属类型与归一归属键，这六段逐字不变 ⇒ 同一个句柄），出处换成新那一轮的 uid。
    import(d, "b", vec![turn("human", "把那条经验原样再说一遍", 2_000)]);
    let u2 = uid("human", "把那条经验原样再说一遍", 2_000);
    let out = cycle(d, &[reply(&extract_ok(&u2))]);
    assert_eq!(code(&out), 0, "整批被抑制是一次成功，不是 ZeroOutput：{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!((v["turns_scanned"].as_i64().unwrap(), v["rows_written"].as_i64().unwrap()), (1, 0));
    assert_eq!((&v["suppressed"], &v["merged"], &v["batches_dropped"]),
               (&json!(1), &json!(0), &json!(0)));
    assert_eq!(merge_ledger(d), (1, 0, 0, 0), "抑制那一格没有写成行：库里还是那一条已判死的行");
    assert_eq!(llm_job_ids(d), vec!["audit".to_owned(), "extract".to_owned()],
        "证据表只记写成的行 ⇒ 抑制轮没有探针，判同不开口");
    assert_eq!(llm_calls_total(d), 5, "4 + 1：抑制轮只烧掉抽取那一次调用");
}

#[test]
fn a_dead_basis_is_suppressed_on_a_cycle_with_no_derive_batch() {
    // 段 0 的**可达性**用例（E40）。它测的是挂点不是语义——语义由 `crates/rc-memory/tests/orphans.rs`
    // 的十二条钉；这一格钉的是"空批轮次也跑"。
    // 红形（把 `run_cycle_once` 顶部那一段挪回 `derive_pass`）：推导批为空 ⇒ `run_derive` 提前
    // return ⇒ 段 0 一次都不跑 ⇒ `retirement` 表里没有 `basis_dead` 这一行。
    // **为什么必须走真进程**：`CycleOut.suppressed` 那一格是**抽取**的读数，段 0 的三格本阶段
    // 不升 `CycleOut` ⇒ 段 0 在进程外唯一的可观察面就是库。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    {
        // 句柄必须在子进程起来之前放掉：读-写-放-再起子进程，这个顺序才与生产同形。
        let root = open(d);
        let m = root.memory();
        let scope = Scope { kind: ScopeKind::Project, key: "p-hub".into() };
        let target = put_entity(m, &EntityContent {
                kind: "concept".into(), title: "hub-段0-目标".into(),
                scope: scope.clone(), aliases: vec![] },
            &[Provenance { turn_uid: "turn-seed".into(), anchor_role: AnchorRole::Human }], 100)
            .unwrap().id;
        let cited = put_card(m, &FactContent {
                kind: "decision".into(), title: "hub-段0-依据".into(), body: "引用上面那个".into(),
                scope, valid_to_ms: None },
            &[Provenance { turn_uid: "turn-seed".into(), anchor_role: AnchorRole::Human }], 100)
            .unwrap().id;
        // 派生出处行（`ref_kind='anchor'`：被引目标在 canonical 里是 `anchor` 表）直接 insert：
        // 生产侧唯一的写入点是
        // `writers_x::insert_derived_provenance`，那是另一条路径，段 0 的语义不依赖产生方式。
        m.conn().execute(
            "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role, ref_kind)
             VALUES('card',?1,?2,'human','anchor')",
            rusqlite::params![cited, target]
        ).unwrap();
        retire(m, &MemKey { table: MemTable::Anchor, id: target }, "manual", 200).unwrap();
        // 这张卡片必须**先判否再入题**（`Stance::Rejected` 落进 `attestation` 表）：否则它是活行，
        // 而活行既进推导池（`derive_pool_union`）也可能进审核队列 ⇒ 这一轮会去调 LLM、没有回复
        // 就超时退 3，而那个退 3 会盖住本用例真正要断的那一格（红在退出码而不是退场表）。
        // 判否之后两个消费面都看不到它，这一轮一次 LLM 都不开口 ⇒ 退不退出场就只由段 0 说了算。
        // 段 0 不看引用行自己的生死，只看它指向的依据死没死 ⇒ 判否的卡片照样会被退场。
        // **别拿 `verdict` 列代替**（E40 实测栽过）：`alive_where` 的三支是退场账 /
        // `attestation` 否决 / 被顶掉，`verdict` 不在其中——它只管审核队列，设了它推导池照样
        // 看得到这一行，退出码还是 3。
        attest(m, &MemKey { table: MemTable::Card, id: cited }, Stance::Rejected, None, None, 100)
            .unwrap();
    }
    assert_eq!(retirement_reasons(d), vec!["manual".to_owned()],
        "夹具自己只退过那一条目标行，段 0 一个字都还没跑");

    // 一轮**没有任何新对话**的 cycle：抽取批为空 ⇒ 推导批也为空。
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 0, "空批不是错误：{}", stderr_text(&out));
    assert_eq!(retirement_reasons(d), vec!["basis_dead".to_owned(), "manual".to_owned()],
        "空批轮次也要跑段 0——它挂在 cycle 开头，不挂在『有没有活行』上");
}

#[test]
fn llm_timeout_exits_3_but_still_reports_real_rows_written() {
    // §2.19 给 `run_cycle_once` 定下 `(CycleOut, CycleExit)` 而不是 `Result` 的那条执法用例。
    // 写成 `Result` 的话，非零码这一支就把 `rows_written` 一起丢了——而抽取那一行是真写进库了的。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000)]);
    let u = uid("human", "上线前要先跑测试", 1_000);
    let out = cycle(d, &[reply(&extract_ok(&u)), fail("timeout")]);
    assert_eq!(code(&out), 3);
    let v = stdout_json(&out);
    assert_eq!((v["turns_scanned"].as_i64().unwrap(), v["rows_written"].as_i64().unwrap()), (1, 1),
        "退出码 3 不许把已经发生的读数一起吞掉");
    assert_eq!((&v["quota_exhausted"], &v["halted"]), (&json!(false), &json!(false)));
    assert!(stderr_text(&out).contains("timeout"), "类别串要读得出是哪一族：{}", stderr_text(&out));
}

#[test]
fn quota_exhausted_reply_sets_the_cell_and_still_exits_3() {
    // `CycleOut.quota_exhausted` 有两条生产者（脚本声明 / 后端回的额度耗尽）。CLI 路径只能跑到
    // 第一条：本地调用次数上限已砍掉（2026-11-06 的裁定），所以一趟回放永远撞不到"本进程数出来的"
    // 那一支。第二条住在 `replay.rs` 与 `rc-cycle` 的单元测试里，两条都在收据上点名这一格。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000)]);
    let u = uid("human", "上线前要先跑测试", 1_000);
    let out = cycle(d, &[reply(&extract_ok(&u)), fail("quota_exhausted")]);
    assert_eq!(code(&out), 3);
    let v = stdout_json(&out);
    assert_eq!((&v["quota_exhausted"], &v["rows_written"]), (&json!(true), &json!(1)));
    // S3a 阳性：生产侧真的把"最近一次运行触顶"写进了账（读侧 `status`/`doctor` 靠这一格）。
    // 这一句同时挡住"只在 CycleOut 里报 true、账上一个字没写"那一型——那种实现下
    // `status.quota_exhausted` 会永远是 null/false。
    assert_eq!(quota_stopped_for(d, "cycle"), Some(1),
        "本轮触顶必须落账，否则 status 读不到它");
}

#[test]
fn derive_dropped_increments_dropped_and_keeps_cursor() {
    // 推导契约丢批的接线测试：3 格里 `derive_dropped = 1`、游标不动、attempts +1、
    // 下轮同一批仍被选中（与 P-32 同律：活锁唯一可见格）。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // 白盒播种那一格走抽取：转录里有一条人工轮 ⇒ 抽取写一行 ⇒ 审核审一行（3 票）⇒
    // 推导拿到的批里有一行活卡。判同不开口（库里只有一条卡片，没有邻居）。
    import(d, "a", vec![turn("human", "先跑测试再合并", 1_000)]);
    // 喂一份缺键的推导件（只有 relationships 键、缺 chains 与 timeline）
    const DERIVE_BAD: &str = r#"{"relationships":[]}"#;
    let out = cycle(d, &[reply(&extract_ok(&uid("human", "先跑测试再合并", 1_000))),
                         reply(YES), reply(YES), reply(YES), reply(DERIVE_BAD)]);
    assert_eq!(code(&out), 2, "推导丢批也走整批拒收码：{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["derive_dropped"].as_i64().unwrap(), 1, "丢批读数：{v}");
    assert_eq!(v["derive_drop_reason"], "schema", "丢批必须带封闭类别：{v}");
    assert!(stderr_text(&out).contains("derive batch dropped: schema"),
        "stderr 也要有一句可诊断的类别：{}", stderr_text(&out));
    assert_eq!(v["derive_rows"].as_i64().unwrap(), 0);
    assert_eq!(v["derive_rejected"].as_i64().unwrap(), 0);
    // 游标停在初始（job_watermark 上 family='derive' 那行 cursor_at_ms = 0）
    let derive_cursor = open(d).ledger().conn().query_row(
        "SELECT cursor_at_ms, cursor_turn_uid FROM job_watermark WHERE family='derive'",
        [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).unwrap();
    assert_eq!(derive_cursor.0, 0, "丢批 ⇒ 游标不动：{derive_cursor:?}");
    // attempts 计数（裁定 8 同律：契约丢批 +1）
    let attempts: i64 = open(d).ledger().conn().query_row(
        "SELECT attempts FROM job_watermark WHERE family='derive'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(attempts, 1, "推导丢批计 attempts（活锁唯一可见格）");
    // 计数器面**有** derive 一行：LLM 确实被调用了一次并交回了那份缺键的答复，契约拒收发生在
    // 下游的 parse 层、调用本身已经发生 ⇒ `charge_counters` 照 P-33 记那一次。
    // 这一格不许因为"丢批"就消失——丢了就等于把一次真发生的调用读成"没开过口"。
    let ids = llm_job_ids(d);
    assert!(ids.contains(&"derive".to_owned()), "丢批的那一次调用也是调用（P-33 同律）：{ids:?}");
    assert_eq!(llm_calls_total(d), 5, "脚本五条、调用五次：抽取 1 + 审核 3 + 推导 1");
}

#[test]
fn every_bad_replies_file_is_one_category_and_exit_3() {
    // §2.16 第 2 条：四种读侧失败全部映到**同一条**类别串。分开写就是把"拼错一个词"
    // 播报成四种不同的运维动作，而操作员要做的都是"重做那个文件"。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let cases: [(&str, &str); 4] = [
        ("顶层不是数组", "{}"),
        ("元素既不是串也不是对象", "[7]"),
        ("对象里没有 fail 键", r#"[{"nope":"x"}]"#),
        ("fail slug 认不出", r#"[{"fail":"timed_out"}]"#),
    ];
    for (what, body) in cases {
        let p = d.join("bad.json");
        std::fs::write(&p, body).unwrap();
        let out = run(&["cycle", "once", "--replies", p.to_str().unwrap(),
                        "--data-dir", d.join(DATA).to_str().unwrap()]);
        assert_eq!(code(&out), 3, "{what}");
        assert_eq!(stderr_text(&out).trim(), "error: cannot parse --replies file", "{what}");
        assert_eq!(out.stdout, b"", "{what} ⇒ 文件坏在开库之前，一个读数都不许有");
    }
    // 第五种"读不到"用不存在的文件测。
    let out = run(&["cycle", "once", "--replies", d.join("nope.json").to_str().unwrap(),
                    "--data-dir", d.join(DATA).to_str().unwrap()]);
    assert_eq!(code(&out), 3);
    assert_eq!(stderr_text(&out).trim(), "error: cannot parse --replies file");
}

#[test]
fn the_three_job_entrances_have_the_signed_shapes() {
    // §2.17：守卫 1 在本阶段的执法形状是**签名不收会话柄**，不是 trybuild fixture。
    // 探针是类型等式：给 `run_extract` 的第一位塞一个 `TranscriptHandle`，或给任何一位塞一个
    // `CapReport`（守卫 14 的另一半：业务层不再判资格），这一条用例当场编译不过。
    // 形状拆成 `type` 别名是 clippy `type_complexity` 要求的拆法；等式本身一格不省。
    type ExtractFn = fn(&rc_store::MemoryHandle, &rc_store::LedgerHandle,
                        &rc_distill::extract::BatchPlan, &dyn rc_llm::LlmBackend, i64)
        -> Result<rc_distill::extract::ExtractOutcome, rc_distill::DistillError>;
    type DedupeFn = fn(&rc_store::MemoryHandle, &rc_store::LedgerHandle,
                       &rc_distill::extract::BatchPlan, &dyn rc_llm::LlmBackend, i64)
        -> Result<rc_distill::dedupe::DedupeOutcome, rc_distill::DistillError>;
    type AuditFn = fn(&rc_store::TranscriptHandle, &rc_store::MemoryHandle,
                      &dyn rc_llm::LlmBackend, i64)
        -> Result<rc_distill::audit::AuditOutcome, rc_distill::DistillError>;
    let _extract: ExtractFn = rc_distill::extract::run_extract;
    let _dedupe: DedupeFn = rc_distill::dedupe::run_dedupe;
    let _audit: AuditFn = rc_distill::audit::run_audit;
}

/// `anomaly_scan` 家族的水位读数。判"这一轮扫描到底跑成了没"的唯一办法是看游标推没推，
/// 而这一族不配对轮次（`advance` 喂空 `turn_uid`），所以只读 `last_success_at_ms`。
/// 用 `max()` 而不是 `LIMIT 1`：这一族只有一行（空 `scope_key`），但 `max` 对空集也回一行 NULL，
/// 于是 `Option` 那一支不必靠 `QueryReturnedNoRows` 的 `unwrap_or` 兜——
/// "读 SQL 出错折成 None"与"从没扫过折成 None"是两件事，这里要的只有后者。
fn anomaly_scan_at_ms(dir: &Path) -> Option<i64> {
    open(dir).ledger().conn().query_row(
        "SELECT max(last_success_at_ms) FROM job_watermark WHERE family='anomaly_scan'", [],
        |r| r.get::<_, Option<i64>>(0)).unwrap_or(None)
}
/// 同一族的 `attempts` 总和：失败轮的计数（游标不动、只加它）——"失败不前进"那两条断言的第二条。
fn anomaly_attempts(dir: &Path) -> i64 {
    open(dir).ledger().conn().query_row(
        "SELECT sum(attempts) FROM job_watermark WHERE family='anomaly_scan'", [],
        |r| r.get(0)).unwrap_or(0)
}

/// `seed_contradiction` 的锚：那张卡唯一的人工轮出处 uid。正面表态必须复用它，否则
/// `attest` 当场拒（锚不是该行的人工轮出处时写不进去）。
const ANCHOR_UID: &str = "t-hub-anomaly";

/// 条款②那条矛盾，四笔**公开写入口**造出来（守卫 3：测试引用生产算法，不重抄 INSERT 列清单）。
/// 形状：判失败 ⇒ 退场 ⇒ 更晚的正面表态（C24-N1：同一事务里解除退场账）⇒
/// 行按 `alive_where` 复活，但 `verdict='failed'` ⇒ 条款②成立。
/// **另加一笔 derive 水位**：这条卡现在按 `alive_where` 是活行，不推水位就会进推导批，
/// 而本文件的多条用例给的是零回复 ⇒ 推导 stage 报 `Timeout`，测的就不是扫描/告警了。
/// 水位推到 `(k.created_at, k.key_string())`，`rows_since` 的严格游标语义把它排除。
/// 正文必须带 `CANARY`：`告警行里一个字节正文都没有` 断的是"告警行不含正文"，
/// 正文里没那个记号时那条断言对任何串都恒真（空过的测试不是测试）。
/// 返回那张卡的键，好让后面的任务能对着**同一条行**补表态；本文件那四条把它当语句用、
/// 不接返回值，`MemKey` 不是 `#[must_use]`，所以不报警告。
fn seed_contradiction(dir: &Path) -> rc_memory::MemKey {
    use rc_memory::{attest, put_card, retire, set_verdict, AnchorRole, FactContent,
                    Provenance, Scope, ScopeKind, Stance, Verdict};
    let root = open(dir);
    let m = root.memory();
    let k = put_card(m, &FactContent {
        kind: "note".into(), title: "上线前先跑测试".into(),
        body: format!("{CANARY} 合成正文，不含任何生产文本"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: ANCHOR_UID.to_string(), anchor_role: AnchorRole::Human }], 1).unwrap();
    set_verdict(m, &k, Verdict::Failed, 1).unwrap();
    retire(m, &k, "manual", 2).unwrap();
    attest(m, &k, Stance::Confirmed, Some(ANCHOR_UID), None, 3).unwrap();
    rc_ledger::water::advance(
        root.ledger(), "derive", "",
        &rc_ledger::water::Cursor { at_ms: 1, turn_uid: k.key_string() }, 1,
    ).unwrap();
    k
}

#[test]
fn 首轮_cycle_就落下_anomaly_心跳() {
    // 证的不是"扫描能跑"，是"新库第一轮的心跳就不是缺席的"。零回复是故意的：
    // 扫描不调 LLM，所以抽取那边拿不到回复也不该挡住它（挡住就是把它错挂到配额家族的成功上）。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_contradiction(d);
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 0, "零回复的轮不许把 cycle 打死：{}", stderr_text(&out));
    assert!(anomaly_scan_at_ms(d).is_some(), "首轮全量扫描必须推 anomaly_scan 水位");
}

#[test]
fn 扫描把库里的矛盾落成_attention_行() {
    // 黑盒这一条只证"hub 真的调了 sweep 并且账落到了表上"——判据本身的正反两面在
    // `crates/rc-memory/tests/anomaly.rs`，那里是白盒，这里重复判据没有意义。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_contradiction(d);
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let n: i64 = open(d).memory().conn().query_row(
        "SELECT count(*) FROM attention WHERE kind='contradiction' AND resolved_at_ms IS NULL",
        [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "首轮全量扫描应把这条矛盾升成一条未解决告警");
}

#[test]
fn 告警行里一个字节正文都没有() {
    // 守卫 12 落在 attention 这一族上的位置：告警只有 `(kind, mem_table, mem_id)` 三个指认。
    // 读的是**列的拼接**而不是整行，因为要挡的是"有一天有人把 title/body 塞进告警"——
    // 那种行一旦存在，`status` 与 doctor 就只是隔一层的问题，而这里先让它在本层就红。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_contradiction(d);
    cycle(d, &[]);
    let row: String = open(d).memory().conn().query_row(
        "SELECT kind || '/' || mem_table || '/' || mem_id FROM attention LIMIT 1", [],
        |r| r.get(0)).unwrap();
    assert!(!row.contains(CANARY), "告警行外带记忆正文：{row}");
}

#[test]
fn 扫描失败不动游标_只加一次_attempts() {
    // §6 失败矩阵的"失败不前进"：先 sweep 后 advance 那个顺序的唯一证据。
    // 失效要**确定**，所以注入的是"升告警那一条 INSERT 必失败"，而不是删表：
    // `attention` 表删了会回来——`ddl::apply` 无条件重放 `CREATE TABLE IF NOT EXISTS`，
    // 于是第二轮扫描正常跑完、`exit=0`，这条用例测的就是一个不存在的行为。
    // `BEFORE INSERT … RAISE(ABORT)` 触发器只在 INSERT 真的产行时触发，所以下面必须先
    // `DELETE FROM attention`：首轮留下的那条未解决告警会让 `raise_attention` 的
    // `WHERE NOT EXISTS` 直接不放行（零行 ⇒ 触发器根本不 fire ⇒ 扫描"成功"），
    // 删掉它才逼得出那一次真 INSERT。
    // 水位这一族只动 `last_full_scan_round`（拨回 0）：不拨的话第二轮 `scan_due(2, 1)` 为假
    // ⇒ 扫描压根不跑 ⇒ 游标没动、attempts 没加，两条断言都在"没跑"上空绿。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_contradiction(d);
    let first = cycle(d, &[]);
    assert_eq!(code(&first), 0, "{}", stderr_text(&first));
    let cursor = anomaly_scan_at_ms(d).expect("前置：首轮成功扫描要留下水位");
    let attempts_before = anomaly_attempts(d);
    open(d).memory().conn().execute(
        "CREATE TRIGGER raise_attention_boom BEFORE INSERT ON attention
         BEGIN SELECT RAISE(ABORT, 'injected scan failure'); END", []).unwrap();
    open(d).memory().conn().execute("DELETE FROM attention", []).unwrap();
    open(d).ledger().conn().execute(
        "UPDATE usage_counter SET count=0
         WHERE subject_kind='maintenance' AND subject_id='' AND metric='last_full_scan_round'",
        []).unwrap();
    let second = cycle(d, &[]);
    assert_eq!(code(&second), 3, "扫描失败必须折成运维故障退出码：{}", stderr_text(&second));
    // 类别串也断：只断退出码的话，把 `FAULT_ANOMALY` 换成 `FAULT_COUNTERS` 全仓无人报警，
    // 而那一步的取舍是"报 counters 会把操作员支到错误的面上"（`scan_rounds` 那一支同理）。
    assert!(stderr_text(&second).contains("attention scan failed"),
        "扫描失败必须报 anomaly 那一族，不是别的故障类别：{}", stderr_text(&second));
    assert_eq!(anomaly_scan_at_ms(d), Some(cursor), "失败轮不许推游标（先 sweep 后 advance 的顺序判据）");
    assert_eq!(anomaly_attempts(d), attempts_before + 1, "失败轮必须记一次 attempt");
}

/// 轮号累加（`bump(rounds)`）失败必须可见。这一条是"本任务把 T5 那句 `let _ = …` 改成可见故障"
/// 的**唯一**证人：实测撤掉那一支（把 `exit = CycleExit::Fault(FAULT_COUNTERS)` 换成空体）
/// 之后 hub 的全部用例仍绿，而轮号是 `scan_due` 唯一的轮源——轮号停走时
/// `attention_open: 0` 看起来仍是好消息。
/// 注入只筛 `subject_kind='maintenance'`：`charge_counters` 写的是 `llm_job`，
/// 不加这一句就把两支故障混成一次注入，测不出红在哪一支。
#[test]
fn 轮号累加失败_报_counters_类别_且本轮不跑扫描() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    open(d);
    open(d).ledger().conn().execute(
        "CREATE TRIGGER rounds_bump_boom BEFORE INSERT ON usage_counter
         FOR EACH ROW WHEN NEW.subject_kind='maintenance'
         BEGIN SELECT RAISE(ABORT, 'injected rounds bump failure'); END", []).unwrap();
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 3, "轮号写不进去不许静默成功：{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("usage counter write failed"),
        "这一支报的是 counters 类别串：{}", stderr_text(&out));
    assert!(anomaly_scan_at_ms(d).is_none(), "已判死的轮不许再动扫描（挂点在 `!matches!(Fault)` 之内）");
}

/// 复核那一列的读数。用 `max` 与 `anomaly_scan_at_ms` 同法：空集回一行 NULL，
/// 于是 `None` 只许是"没盖过"，不混进 SQL 错误。
fn recheck_stamp(dir: &Path) -> Option<i64> {
    open(dir).memory().conn().query_row(
        "SELECT max(rechecked_at_ms) FROM attention", [],
        |r| r.get::<_, Option<i64>>(0)).unwrap_or(None)
}

/// 种一个"更晚的表态"：比 hub 真时钟晚一整代的那个时刻。
/// 不读系统时钟（判据 12a：取时钟只许住在三个 bin 的 `main.rs`，测试也不例外），
/// 而告警那一行的 `created_at_ms` 是 hub 用真时钟盖的，所以这里只能往未来取一个常数。
/// 常数的失效日写在原位：`4_102_444_800_000` = 2100-01-01T00:00:00Z（`date -u -d @4102444800`），
/// 真时钟跨过那一天之后 `cycle_本轮新升的告警_本轮就被盖戳` 会红在"盖了 0 条"上——换常数，别改判据。
const RECHECK_LATER: i64 = 4_102_444_800_000;

fn attest_later(dir: &Path, k: &rc_memory::MemKey) {
    let root = open(dir);
    attest(root.memory(), k, Stance::Corroborated, Some(ANCHOR_UID), None, RECHECK_LATER).unwrap();
}

#[test]
fn cycle_本轮新升的告警_本轮就被盖戳() {
    // 顺序证人，也是接线证人。形状：矛盾在**首轮之前**就齐了（`seed_contradiction` 的退场账 +
    // 一笔比 hub 真时钟更晚的正面表态），首轮 `run_anomaly_scan` 里 sweep 先升出那一行、
    // recheck 紧接着盖它 ⇒ 一轮之内两个动作都看得见。
    // 为什么这样写才有牙（E57 实测，库侧同形状）：先跑 recheck 后跑 sweep ⇒ 0 行可盖（那时
    // 告警行还不存在），戳要白等一整轮；把 recheck 那行同型换成 `recheck_pending` ⇒ 永远不盖。
    // 于是"挂点挪到 sweep 之前"和"挂点根本没接上"两个变异各自只红在这一条上，而初稿那条两轮
    // 写法两个变异都放行。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let k = seed_contradiction(d);
    attest_later(d, &k);
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert!(recheck_stamp(d).unwrap_or(0) > 0,
            "本轮升出来的告警必须本轮就拿到戳：{}", stderr_text(&out));
}

#[test]
fn cycle_没有更晚的表态_复核一个都不盖() {
    // 反向半边：`seed_contradiction` 自带的表态在第 3 毫秒、比 hub 首轮的真时钟早得多 ⇒
    // `recheck_attested` 的 `a.created_at_ms > attention.created_at_ms` 不成立 ⇒ 不许盖。
    // 少了这一条，"首轮盖了戳"就可能只是"把每一行都盖了"的同义反复（牙：把那一支 EXISTS
    // 删掉 ⇒ 本条红，见变异 ②）。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_contradiction(d);
    let out = cycle(d, &[]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert!(live_alert(d) > 0, "前置：告警真的升出来了，否则这一条是空过的");
    assert_eq!(recheck_stamp(d), None, "库里没有更晚的表态 ⇒ 那一列必须还是空的");
}

/// 未解决告警的行数。第二条用例的前置用它：`None` 既可能是"没盖"也可能是"根本没行可盖"。
fn live_alert(dir: &Path) -> i64 {
    open(dir).memory().conn().query_row(
        "SELECT count(*) FROM attention WHERE resolved_at_ms IS NULL", [],
        |r| r.get(0)).unwrap()
}

/// 某一族挂着未解决的告警数。只数 kind 与 resolved，不带正文（守卫 12 在测试侧同样成立）。
fn open_attention(dir: &Path, kind_slug: &str) -> i64 {
    open(dir).memory().conn().query_row(
        "SELECT count(*) FROM attention WHERE kind=?1 AND resolved_at_ms IS NULL",
        [kind_slug], |r| r.get(0)).unwrap()
}

/// 别名表全量行数——"只报不动"与"闭包补了一行"在这一格上是两个相反的读数。
fn alias_rows(dir: &Path) -> i64 {
    open(dir).memory().conn().query_row("SELECT count(*) FROM alias", [], |r| r.get(0)).unwrap()
}

#[test]
fn cycle_把重影作业挂在全量扫描那一支上() {
    // 这一条证的不是三条检测 SQL（`ghosts.rs` 那 8 条已经证了），是**接线**：
    // 挂点没进 `run_anomaly_scan`，或者整支挂在 `scan_due` 判定之外，这里两个数都是 0。
    // 夹具一律 `entity` 表：P-30 定过实体永不进**审核**游标（`unreviewed_rows` 只吃三张候选表），
    // 所以这一条不需要 T12 那句 `set_verdict(Passed)` 前置。但实体**在推导池里**
    // （`derive_pool_union` 的臂就是四张表，`reader.rs` 的 `ROW_TABLES`），四条活实体行 = 一个
    // 非空推导批 = 恰好一次 LLM 调用，所以这里必须喂一份答复：`&[]` 会折成 exit 3
    // （本机实测："error: llm timeout — retry this round later"）。答复取既有的 `DERIVE_QUIET`
    // （跑通但不落行）；取"三键皆空"会撞裁定 12 的 `ZeroOutput` ⇒ exit 2，那是另一条既有判据。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let u = "t-hub-ghost";
    {
        let root = open(d);
        let m = root.memory();
        let pv = || vec![rc_memory::Provenance { turn_uid: u.into(), anchor_role: rc_memory::AnchorRole::Human }];
        let ec = |title: &str, kind: &str, aliases: Vec<String>| rc_memory::EntityContent {
            kind: kind.into(), title: title.into(),
            scope: rc_memory::Scope { kind: rc_memory::ScopeKind::Person, key: "p-hub-ghost".into() },
            aliases };
        // 闭包对：'Hub甲' 认领 'Hub乙'，而 'Hub乙' 自成一行
        rc_memory::put_entity(m, &ec("Hub甲", "person", vec!["Hub乙".into()]), &pv(), 1).unwrap();
        rc_memory::put_entity(m, &ec("Hub乙", "person", vec![]), &pv(), 1).unwrap();
        // 同题对：kind 抖动造出的两行（A39 的报告口径）
        rc_memory::put_entity(m, &ec("Hub丙", "person", vec![]), &pv(), 1).unwrap();
        rc_memory::put_entity(m, &ec("Hub丙", "concept", vec![]), &pv(), 1).unwrap();
    }
    assert_eq!(alias_rows(d), 1, "夹具只有一条显式别名");
    let out = cycle(d, &[reply(DERIVE_QUIET)]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(alias_rows(d), 2, "一轮 cycle 之后闭包补上了反向那一条");
    assert_eq!(open_attention(d, "duplicate_title"), 2, "同题两行都升成告警");
    assert_eq!(open_attention(d, "alias_ambiguity"), 0, "单边态不该报歧义（一家认领）");
}

#[test]
fn cycle_out_carries_extract_gate_counts() {
    // A45 三数在**装配面**上的非零 witness（E1 段 B）。十七格里那三格此前只有"键在场"，
    // 而 "键在场" 对"`out.entities_seen += o.entities_seen;` 整行删掉"恒真：抽取闸门的三个
    // 计数器在 `rc-distill` 侧有生产者（`entity_gate` 五条钉住），但 hub 不把它们搬上读数面
    // 时全 workspace 照绿。这一条把三格都断成**非零且有确切值**，删任意一行累加即红。
    // 夹具一批里同时给四种形状：三条合法实体（第一条带一个好别名 + 三个不在原文里的别名，
    // 另两条无别名、标题互不相同且都不等于卡片标题）、两条非法 kind 实体（`human` / `mood`）、
    // 一条卡片。于是 分母 5 / 拒收 2 / 坏别名 3 / 落行 4 四个数互不相同，四处接线任一对调都红
    // （全 0 或全 1 的夹具分不出是哪一格坏了；后两格若撞成同值，`rows_written` 与
    // `aliases_dropped` 那两条同形累加线右值对调也测不出来）。
    // 别名那一格要三个坏别名而不是两个：「测试」逐字在出处原文里（`上线前要先跑测试`）⇒ 那是
    // **被留下**的那一个，两个坏别名只会读出 2（= kinds_rejected），四数就撞成两对。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000), turn("agent", "好的，跑过了", 1_001)]);
    let u = uid("human", "上线前要先跑测试", 1_000);
    let extract = json!({
        "entities": [
            {"kind":"project","title":"上线前跑测试",
             "aliases":["测试","不存在1","不存在2","不存在3"],"provenance":[&u]},
            {"kind":"project","title":"上线前先验证","aliases":[],"provenance":[&u]},
            {"kind":"project","title":"上线前先评审","aliases":[],"provenance":[&u]},
            {"kind":"human","title":"非法实体甲","aliases":[],"provenance":[&u]},
            {"kind":"mood","title":"非法实体乙","aliases":[],"provenance":[&u]}
        ],
        "cards": [{"kind":"lesson","title":"上线前先跑测试",
                   "body": format!("{CANARY} 每次合并之前先跑全量测试"),
                   "valid_to_ms": null, "provenance": [&u]}],
        "events": [], "assertions": [], "worthless_turn_uids": []
    }).to_string();
    let out = cycle(d, &[reply(&extract), reply(YES), reply(YES), reply(YES), reply(DERIVE_QUIET)]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["entities_seen"], 5, "五条实体都该进分母：{v}");
    assert_eq!(v["kinds_rejected"], 2, "human / mood 各被单条丢弃：{v}");
    assert_eq!(v["aliases_dropped"], 3, "三个不在出处原文里的别名各丢一个（「测试」被留下）：{v}");
    assert_eq!(v["rows_written"], 4, "三条合法实体 + card：{v}");
}
