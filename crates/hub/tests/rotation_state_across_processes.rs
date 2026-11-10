// crates/hub/tests/rotation_state_across_processes.rs
//! §4.8 判据那一格的**真进程两条**（设计节 5 逐字：同一临时库上跑两次 `hub cycle once`，
//! 第一次主家坏 ⇒ 落到备用座位；第二次主家仍坏 ⇒ 仍由备座服务）。
//!
//! 为什么必须是真子进程：单元测试里连调两次 `Rotating` 测的是那台状态机自己，不是**落盘**。
//! "读回来的是哪一行、写下去的是哪一行"这两格只有换一个进程才看得见（作废规则①④的落点都在
//! 进程边界之外：一个进程内既没有"上一次"，也没有"另一台机器"）。
//!
//! 本段（段 B）读"服务者是谁、那行写于何时"这两格**直接从库里读那一行**
//! （`rc_ledger::llm_state::read`）——`doctor` 那一格是段 C，这里不碰。
//!
//! 夹具形状照 `cycle_once.rs`：`run_env` 先把 ambient 的 `RECOLLECT_DEVICE_ID` 摘掉再按参数加回，
//! 于是"缺身份"那一支在本机与 CI 上都是用例自己造出来的缺席，而不是碰巧环境里没有。
use rc_contract::compute_turn_uid;
use rc_ledger::{counters, llm_state};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEV: &str = "dev-cli";
const SESS: &str = "sess-cli";
const DATA: &str = "data";
/// 本机身份（过 `device_subject` 的占位 token，不是任何真值；密钥值的字节一个都不在这里）。
const BOX: &str = "box-laptop";
/// 别的机器写的那一行（作废规则①的另一造：`read` 的 `WHERE machine=?1` 承担，不靠 hub 判断）。
const OTHER_BOX: &str = "box-desktop";

const YES: &str = r#"{"holds":true}"#;
/// 推导家族"跑通但不落行"的答复（与 `cycle_once.rs` 同一形状：非空三键才不触发 `ZeroOutput`）。
const DERIVE_QUIET: &str = r#"{"relationships":[],"chains":[{"from":["r1"],"mechanism":"requires","basis":["r1"]}],"timeline":[]}"#;

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }
fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}
/// 带环境变量跑 `cycle once`。`source_args` 就是那串 `--replies`（顺序 = 降级顺序）。
/// 形状是 `&[String]` 而不是 `&[&str]`（`cycle_once.rs` 的那一对是 `&[&str]`）：本文件的旗标
/// 由 `two_flags` / `one_flags` 现拼，`&[&str]` 会把临时 `Vec<String>` 卡在语句边界上（E0716），
/// 而"忘记设环境变量"这一族必须由用例自己造，不能靠借用技巧。
fn run_env(dir: &Path, source_args: &[String], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.env_remove("RECOLLECT_DEVICE_ID");
    for (k, v) in env { cmd.env(k, v); }
    cmd.args(["cycle", "once"]);
    cmd.args(source_args);
    let dd = dir.join(DATA);
    cmd.args(["--data-dir", dd.to_str().unwrap()]);
    cmd.output().expect("hub binary")
}
/// 拿到身份的那一轮（段 B 的读写两支都开着）。
fn with_box(dir: &Path, source_args: &[String]) -> std::process::Output {
    run_env(dir, source_args, &[("RECOLLECT_DEVICE_ID", BOX)])
}
fn code(out: &std::process::Output) -> i32 { out.status.code().unwrap() }
fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}
fn stderr_text(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn turn(role: &str, body: &str, at: i64) -> Value {
    json!({"role": role, "body": body, "created_at_ms": at})
}
fn uid(role: &str, body: &str, at: i64) -> String {
    compute_turn_uid(DEV, SESS, role, body, at)
}
fn reply(text: &str) -> Value { Value::String(text.to_owned()) }
fn fail(slug: &str) -> Value { json!({"fail": slug}) }

fn import(dir: &Path, name: &str, turns: Vec<Value>) {
    let p = dir.join(format!("exchange-{name}.json"));
    let doc = json!({"format_version": 1, "sessions": [{
        "session_key": SESS, "device_id": DEV, "source_agent": "agent-cli", "turns": turns }]});
    std::fs::write(&p, doc.to_string()).unwrap();
    let out = run(&["import", p.to_str().unwrap(), "--data-dir",
                    dir.join(DATA).to_str().unwrap()]);
    assert!(out.status.success(), "导入夹具必须成功：{}", stderr_text(&out));
}

/// 抽取答复：标题与正文由调用方给，**第二轮必须换一个词都不重的**，否则判同开口、
/// 座位脚本的消费序列漂移，"这一轮开了一次口"就不再等于"备座服务"。
fn card_reply(u: &str, title: &str, body: &str) -> String {
    json!({"entities": [], "events": [], "assertions": [],
           "cards": [{"kind": "lesson", "title": title, "body": body,
                      "valid_to_ms": null, "provenance": [u]}],
           "worthless_turn_uids": []}).to_string()
}
/// 一座"能吃满一整轮"的回放脚本：抽取 1 + 审核 3 票 + 推导 1（与 `happy_path` 那条同一算术）。
/// 长度是**判据**而不是余量：多一条会被读成"这一轮真的开了六次口"。
/// 第二轮的卡片必须与第一轮**一个词都不重**（`scored_siblings` 命中就 ⇒ 判同开口 ⇒ 后面每一格
/// 答复都错位一格），所以第二轮用英文标题/正文：跨文字的 token 集合天然不相交，
/// 这一条算术才不依赖阈值的具体取值。
fn full_round(u: &str, title: &str, body: &str) -> Vec<Value> {
    vec![reply(&card_reply(u, title, body)), reply(YES), reply(YES), reply(YES), reply(DERIVE_QUIET)]
}
/// 第 `n` 座（从 1 数起）的脚本落成 `--replies` 要吃的那个文件。
fn write_seat(dir: &Path, n: usize, script: &[Value]) -> PathBuf {
    let p = dir.join(format!("replies-{n}.json"));
    std::fs::write(&p, Value::Array(script.to_vec()).to_string()).unwrap();
    p
}
/// 两个座位的旗标：顺序就是降级顺序（`reply-1` 先、`reply-2` 后）。
fn two_flags(first: &Path, second: &Path) -> Vec<String> {
    vec!["--replies".to_owned(), first.to_string_lossy().into_owned(),
         "--replies".to_owned(), second.to_string_lossy().into_owned()]
}
fn one_flags(first: &Path) -> Vec<String> {
    vec!["--replies".to_owned(), first.to_string_lossy().into_owned()]
}

fn open(dir: &Path) -> Root { Root::open(&dir.join(DATA)).unwrap() }
/// 段 B 的两格读数面：**直接从库里读那一行**（服务者是谁、那行写于何时）。
fn row_of(dir: &Path, machine: &str) -> Option<llm_state::Row> {
    llm_state::read(open(dir).ledger(), machine).unwrap()
}
/// 手工造一行（跨机器那一造、以及"上一次写的值"这一造）：走的是同一个 `write`，
/// 所以夹具不是绕过 DDL 的裸 SQL。
fn put_row(dir: &Path, machine: &str, slug: &str, due: Option<i64>, ok: u32, nf: usize, at: i64) {
    let root = open(dir);
    llm_state::write(root.ledger(), &llm_state::Row {
        machine: machine.to_owned(), slug: slug.to_owned(), probe_due_at_ms: due,
        probe_ok: ok, probe_failures: nf, written_at_ms: at }).unwrap();
}
fn llm_calls_total(dir: &Path) -> i64 {
    counters::read_all(open(dir).ledger()).unwrap().iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls")
        .map(|r| r.count).sum()
}
fn llm_calls_for(dir: &Path, family: &str) -> i64 {
    counters::read_all(open(dir).ledger()).unwrap().iter()
        .filter(|r| r.subject_kind == "llm_job" && r.metric == "llm_calls" && r.subject_id == family)
        .map(|r| r.count).sum()
}

#[test]
fn the_backup_seat_serves_the_next_process_not_the_primary() {
    // 真进程第一条（判据 1 的第一格）：第一轮主座坏 ⇒ 库里那一行是备座；
    // 第二轮同样两个旗标 ⇒ **仍然由备座服务**——主座一次都没开口。
    // 谁是服务者的证人不是 `slug`（两条路径最后都能写回 reply-2），而是**抽取家族开口的次数**：
    // 继承成功 = 1 次（备座直接顶上），没继承 = 2 次（主座坏掉一次 + 备座一次）。
    // 把 B2 那句 `llm_state::read` 注释掉 ⇒ 本条当场红在 `extract` 的增量上。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    let u1 = uid("human", "上线前要先跑测试", 1_000);
    let s1 = write_seat(d, 1, &[fail("backend_error")]);
    let s2 = write_seat(d, 2, &full_round(&u1, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let flags = two_flags(&s1, &s2);
    let first = with_box(d, &flags);
    assert_eq!(code(&first), 0, "主座坏、备座补上 ⇒ 这一轮要跑完：{}", stderr_text(&first));
    let r1 = row_of(d, BOX).expect("第一轮落盘 ⇒ 本机那一行必须在场");
    assert_eq!((r1.machine.as_str(), r1.slug.as_str()), (BOX, "reply-2"),
               "换座之后没把服务者写进表里：{r1:?}");
    assert!(r1.written_at_ms > 0, "那一行必须带本轮的时刻：{r1:?}");
    assert_eq!(llm_calls_for(d, "extract"), 2, "主座坏那一次也算真开过口（P-33 同律）");
    assert_eq!(llm_calls_total(d), 6, "脚本 1+5 条、调用 6 次：抽取两次 + 审核三票 + 推导一次");

    // 第二轮：换一批新对话（否则这一轮根本没有活要问 LLM，服务者是谁就读不出来），
    // 座位旗标一字不改。
    import(d, "b", vec![turn("human", "备份要先练一遍", 2_000),
                        turn("agent", "恢复流程走过了", 2_001)]);
    let u2 = uid("human", "备份要先练一遍", 2_000);
    write_seat(d, 2, &full_round(&u2, "Backup drill", "Run the restore handbook every quarter"));
    let before_total = llm_calls_total(d);
    let before_extract = llm_calls_for(d, "extract");
    let second = with_box(d, &flags);
    assert_eq!(code(&second), 0, "第二次同样两个旗标却跑不完：{}", stderr_text(&second));
    let v = stdout_json(&second);
    assert_eq!(v["turns_scanned"].as_i64(), Some(2), "第二轮要真的有新批次，否则这一条是空的：{v}");
    assert_eq!(llm_calls_for(d, "extract") - before_extract, 1,
        "抽取只开了一次口 ⇒ 备座服务；开了两次说明主座又被问了一遍（那一行没读回来）");
    assert_eq!(llm_calls_total(d) - before_total, 5, "整轮 5 次调用，主座一次都没沾到");
    let r2 = row_of(d, BOX).expect("第二轮写完 ⇒ 那一行还在");
    assert_eq!(r2.slug, "reply-2", "第二次服务的还是那家：{r2:?}");
    assert!(r2.written_at_ms >= r1.written_at_ms,
        "写于何时那一格被写坏了（本机钟倒退也算）：{:?}", (r1.written_at_ms, r2.written_at_ms));
}

#[test]
fn an_unrecognized_seat_in_the_row_is_not_a_downgrade() {
    // 真进程那条的补刀（作废规则②）：表里那一行写着 `reply-2`，可这一轮只给了**一个**
    // `--replies`（它的标签按序位是 `reply-1`）⇒ 认不到的座位 = 当作没降级 ⇒ 回到 `reply-1`
    // 服务，且那一行被**改写**为 `reply-1`。
    // 变异：把 `Rotating::seeded` 换成 `Rotating::new` ⇒ 本条仍然绿（都不降级是同一后果），
    // 所以真正的牙在后两格的断言上；把 `seeded` 换成"照 slug 找、找不到就 panic 或回落 0 之外
    // 的动作"才红。这一条与上一条共用"抽取开口次数"那一格当证人。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    let u1 = uid("human", "上线前要先跑测试", 1_000);
    let s1 = write_seat(d, 1, &[fail("backend_error")]);
    let s2 = write_seat(d, 2, &full_round(&u1, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let flags = two_flags(&s1, &s2);
    assert_eq!(code(&with_box(d, &flags)), 0, "前置那一轮跑不完 ⇒ 后面那条\"认不到\"就没有对手");
    assert_eq!(row_of(d, BOX).unwrap().slug, "reply-2", "前置：先让表里那一行是备座");

    // 第三轮只给一个座位：表里那行认不到 ⇒ 这一座就是主家，且不留探测态。
    import(d, "b", vec![turn("human", "备份要先练一遍", 2_000),
                        turn("agent", "恢复流程走过了", 2_001)]);
    let u2 = uid("human", "备份要先练一遍", 2_000);
    let only = write_seat(d, 1, &full_round(&u2, "Backup drill", "Run the restore handbook every quarter"));
    let before_extract = llm_calls_for(d, "extract");
    let solo = one_flags(&only);
    let out = with_box(d, &solo);
    assert_eq!(code(&out), 0, "认不到的座位被当成了故障：{}", stderr_text(&out));
    assert_eq!(llm_calls_for(d, "extract") - before_extract, 1, "回到 reply-1 服务 ⇒ 抽取恰好开一次口");
    let r = row_of(d, BOX).unwrap();
    assert_eq!(r.slug, "reply-1", "那一行没被改写回主家：{r:?}");
    assert_eq!((r.probe_due_at_ms, r.probe_ok, r.probe_failures), (None, 0, 0),
        "主家没有待做的探测：这三格不清零就是把 0 号座位写成了降级态");
}

#[test]
fn a_row_written_by_another_machine_is_not_readable_here() {
    // 判据 1 的第二格：表里那一行是**别的机器**写的 ⇒ 读不出来。
    // 承担它的是 `read` 的 `WHERE machine=?1`（作废规则①），所以这一条不测 hub 的判断，
    // 测的是 hub 有没有把身份递对：本机那一行带着可辨认的探测值（1/1/Some(111)），
    // 别机那一行带着另一组（2/3/Some(9999999)），**没读到任何一行**则是第三组（0/0/None）。
    // 三组互不相同 ⇒ 写错主语（读全表 / 拿另一台的行 / 干脆不读）分别红在不同的断言上。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    put_row(d, OTHER_BOX, "reply-2", Some(9_999_999), 2, 3, 1);
    put_row(d, BOX, "reply-2", Some(111), 1, 1, 1);
    let u = uid("human", "上线前要先跑测试", 1_000);
    let s1 = write_seat(d, 1, &[fail("backend_error")]);
    let s2 = write_seat(d, 2, &full_round(&u, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let flags = two_flags(&s1, &s2);
    let out = with_box(d, &flags);
    assert_eq!(code(&out), 0, "本机有行、形状合法却跑挂了：{}", stderr_text(&out));
    assert_eq!(llm_calls_for(d, "extract"), 1,
        "主座被问了 ⇒ 别机那一行读进来了（读不出才是对的）");
    let mine = row_of(d, BOX).unwrap();
    assert_eq!((mine.slug.as_str(), mine.probe_due_at_ms, mine.probe_ok, mine.probe_failures),
               ("reply-2", Some(111), 1, 1), "本机那一行的探测三格没被原样带回来：{mine:?}");
    assert!(mine.written_at_ms > 1, "写于何时那一格没被本轮的时刻覆盖：{mine:?}");
    let theirs = row_of(d, OTHER_BOX).unwrap();
    assert_eq!((theirs.probe_due_at_ms, theirs.probe_ok, theirs.probe_failures, theirs.written_at_ms),
               (Some(9_999_999), 2, 3, 1), "别机那一行被本机改写了 ⇒ 一台机器一行的规矩破了");
}

#[test]
fn a_missing_identity_neither_reads_nor_writes_and_the_round_still_completes() {
    // 判据 1 的第三格 + 作废规则④的落点：缺身份 ⇒ 既不读也不写，而这一支必须是**能跑完的一轮**。
    // 造法：先把本机那一行写成"上一次的值"（written_at_ms = 1，一个任何真钟都不会写的值），
    // 再以缺身份跑一轮 ⇒ 那一行必须逐列还是旧的，而 stdout 照旧是十七个读数、退出码 0。
    // 反证（阳性对照）放在最后：同一份旗标**带上身份**再跑一次 ⇒ 那一行被改写 ⇒
    // 上面那句"还是旧的"不是因为根本没人能写这一张表。
    // 评审 T1（甲案）的两格挂在这同一条上：缺身份那一轮要在运维面**响亮一句**（`warning` 通道），
    // 而带上身份那一轮**不许**有那一句。补两格而不是新起一条用例：这两轮本来就在这里跑，
    // 再跑一轮只是把"真进程 + 落盘"的那笔时间重复付一次。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    put_row(d, BOX, "reply-2", Some(111), 1, 1, 1);
    let u = uid("human", "上线前要先跑测试", 1_000);
    let s1 = write_seat(d, 1, &[fail("backend_error")]);
    let s2 = write_seat(d, 2, &full_round(&u, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let flags = two_flags(&s1, &s2);

    let out = run_env(d, &flags, &[]);
    assert_eq!(code(&out), 0, "缺身份被当成了失败（它该是不读不写、照常跑完）：{}", stderr_text(&out));
    // 响亮一句（甲案）：退出码仍是 0，但"这一轮的轮转态没在继承"必须读得出来，
    // 且要点名到操作员能自己修的那一格（变量名）。前缀是 `warning` 不是 `error`——判定没坏。
    let err = stderr_text(&out);
    assert!(err.contains("no device identity"), "缺身份这一支没在运维面说话：{err}");
    assert!(err.contains("RECOLLECT_DEVICE_ID"),
            "那一句没点名变量名，操作员不知道该设哪一格：{err}");
    assert!(err.contains("llm rotation state"), "那一句说的是轮转态而不是别的事：{err}");
    let v = stdout_json(&out);
    assert_eq!(v.as_object().unwrap().len(), 17, "缺身份那一支的读数面要一字不少（十七格）：{v}");
    assert_eq!((v["turns_scanned"].as_i64(), v["rows_written"].as_i64()), (Some(2), Some(1)));
    // 那一行 JSON 之外不许有第二个字节：非 TTY 一律不提示输入（用户的红线），
    // 而 stdout 多一行"请输入设备号"在这种形状下会被读成"读数坏了"而不是"提示输入"。
    assert_eq!(std::str::from_utf8(&out.stdout).unwrap().lines().count(), 1,
               "缺身份那一支不许往 stdout 写第二行");
    let r = row_of(d, BOX).unwrap();
    assert_eq!((r.slug.as_str(), r.probe_due_at_ms, r.probe_ok, r.probe_failures, r.written_at_ms),
               ("reply-2", Some(111), 1, 1, 1), "缺身份却把那一行写回去了：{r:?}");

    // 缺身份那一轮的换座照常发生（不读不写 ≠ 不降级），且账本照记：主座坏一次 + 备座五次。
    assert_eq!(llm_calls_for(d, "extract"), 2, "缺身份不等于不服务：换座仍然要跑");
    assert_eq!(llm_calls_total(d), 6);

    write_seat(d, 2, &full_round(&u, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let seeded = with_box(d, &flags);
    assert_eq!(code(&seeded), 0, "阳性对照这一支要跑成：{}", stderr_text(&seeded));
    assert_ne!(row_of(d, BOX).unwrap().written_at_ms, 1, "带上身份也没写 ⇒ 上面那句\"还是旧的\"是恒真");
    // 反向证明：那一句是**缺身份**才打的，不是每轮都打。写成无条件的那一格就红在这里。
    assert!(!stderr_text(&seeded).contains("no device identity"),
            "有身份的一轮也报了缺身份：{}", stderr_text(&seeded));
}

/// 作废规则③（本机钟倒退 ⇒ 整行作废）在生产路径上的**唯一一次执行**（评审 S1）。
/// 造法：往表里写一个**未来的** `written_at_ms = i64::MAX` ⇒ `Rotating::seeded` 的合取项
/// `seed.written_at_ms <= now_ms`（`crates/rc-llm/src/rotating.rs:98`）为假 ⇒ 整行作废 ⇒
/// 从 `reply-1` 起服务 ⇒ 主座坏一次 + 备座一次 ⇒ 抽取**开两次口**、整轮六次。
/// 反证（过去那一趟）：同一个形状只把 `at` 换成 `1` ⇒ 继承成功 ⇒ 抽取**只开一次口**、整轮五次。
/// **两趟必须在同一条里**：只留未来那一趟，一个"永远不继承"的实现也能绿。
/// 这一条的牙正对着把 `main.rs` 那一处 `now_ms` 换成 `i64::MAX` 的那个变异：那样做未来趟
/// 会跟着继承成功（`i64::MAX <= i64::MAX` 为真）⇒ 红在两趟读数之差这一格，而既有那几条照绿。
/// 事后 `row_of(d, BOX).unwrap().written_at_ms` 必须不等于 `i64::MAX`：坏值既不许被原样绕回去
/// 毒下一轮，也不许被 clamp 成一个像样的值（裁定 C：`llm_state::read` 不加 clamp），
/// 写回去的那一格构造上是本轮真钟（`row_of_persisted` 的 `written_at_ms` 纪律）。
#[test]
fn a_seed_written_in_the_future_is_discarded_and_rewritten() {
    // ---- 未来那一趟 ----
    let fdir = tempfile::tempdir().unwrap();
    let f = fdir.path();
    import(f, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    put_row(f, BOX, "reply-2", Some(111), 1, 1, i64::MAX);
    let uf = uid("human", "上线前要先跑测试", 1_000);
    let fs1 = write_seat(f, 1, &[fail("backend_error")]);
    let fs2 = write_seat(f, 2, &full_round(&uf, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let fflags = two_flags(&fs1, &fs2);
    let future = with_box(f, &fflags);
    assert_eq!(code(&future), 0, "未来那一趟要跑成能跑完的一轮（作废≠故障）：{}", stderr_text(&future));
    assert_eq!(llm_calls_for(f, "extract"), 2,
        "未来的种没被整行作废 ⇒ 主座一次都没被问（抽取该开两次口：主座坏一次 + 备座一次）");
    assert_eq!(llm_calls_total(f), 6, "未来那一趟的整轮调用数：脚本 1+5 条、抽取两次 + 审核三票 + 推导一次");
    let frow = row_of(f, BOX).expect("未来那一趟照样要落盘");
    assert_eq!(frow.slug, "reply-2", "换过座之后服务者要写回表上：{frow:?}");
    assert_ne!(frow.written_at_ms, i64::MAX,
               "那个坏值被原样绕回去毒下一轮了（也不许 clamp 成一个像样的值）：{frow:?}");

    // ---- 过去那一趟（反证：同一个形状，只有 `at` 不同）----
    let pdir = tempfile::tempdir().unwrap();
    let p = pdir.path();
    import(p, "a", vec![turn("human", "备份要先练一遍", 2_000),
                        turn("agent", "恢复流程走过了", 2_001)]);
    put_row(p, BOX, "reply-2", Some(111), 1, 1, 1);
    let up = uid("human", "备份要先练一遍", 2_000);
    let ps1 = write_seat(p, 1, &[fail("backend_error")]);
    let ps2 = write_seat(p, 2, &full_round(&up, "Backup drill", "Run the restore handbook every quarter"));
    let pflags = two_flags(&ps1, &ps2);
    let past = with_box(p, &pflags);
    assert_eq!(code(&past), 0, "过去那一趟要跑完：{}", stderr_text(&past));
    assert_eq!(llm_calls_for(p, "extract"), 1,
        "过去那一格的种没被继承 ⇒ 备座又被从主座开始走了一遍");
    assert_eq!(llm_calls_total(p), 5, "整轮 5 次调用：主座一次都没沾到");
    // 两趟的差就是这一条的全部理由（读数不同 ⇒ 这一条测的是种的那个戳，不是别的什么）。
    assert_ne!(llm_calls_for(f, "extract"), llm_calls_for(p, "extract"),
        "未来与过去两趟读数相同 ⇒ hub 根本没读种，这一条空过");
    let prow = row_of(p, BOX).expect("过去那一趟也要落盘");
    assert_eq!(prow.slug, "reply-2", "继承成功 ⇒ 服务的还是备座：{prow:?}");
    assert_ne!(prow.written_at_ms, 1, "写回的那一格没被本轮的钟覆盖：{prow:?}");
}

/// 段 D 的 D3②：`llm_state::read` 失败那一支的 witness（此前测试侧对两条落盘类别串零命中，
/// 唯一相关断言含的是 `rotation state`——两条串都含它 ⇒ 互换不红）。四格：退出码 3、
/// 报的是**读**失败那一类（`unreadable`）而**不是**写失败那一类（`write failed`，这一格就是
/// "两条串不许互换"的牙）、stdout 零字节（读失败 ⇒ 整轮不跑）、计数器一格都没落
/// （`llm_calls_total` 加出来是 0，不是"不存在"：这一支在 `run_cycle_once` **之前**就 `return`，
/// 那一轮根本没开过口，而夹具的 import 记的是 import 主语、进不了这个过滤器）。
/// 造法用**改列名**而不是 `DROP TABLE`：建表那句是 `CREATE TABLE IF NOT EXISTS`，
/// 每次 `Root::open` 都重跑那一族 SQL ⇒ 新进程一开库就把表补回来，`read` 返回 `Ok(None)`
/// 而不是 `Err`，那一支根本到不了。列名改掉后表还在 ⇒ 那句整条跳过，而 `read` 的
/// `SELECT … written_at_ms` 在 `prepare` 就失败（rusqlite `bundled` 里的 SQLite ≥ 3.25 支持 `RENAME COLUMN`）。
/// 身份必须给（`with_box`）：缺身份走到的是 D1 那一条 `warning` 而不是这一支。
#[test]
fn a_failed_state_read_stops_the_round_and_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    open(d).ledger().conn().execute_batch(
        "ALTER TABLE llm_state RENAME COLUMN written_at_ms TO gone_written_at_ms").unwrap();
    let u = uid("human", "上线前要先跑测试", 1_000);
    let s1 = write_seat(d, 1, &[fail("backend_error")]);
    let s2 = write_seat(d, 2, &full_round(&u, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let flags = two_flags(&s1, &s2);
    let out = with_box(d, &flags);
    assert_eq!(code(&out), 3, "读失败却不降判定 ⇒ 轮转态坏了没人知道：{}", stderr_text(&out));
    let err = stderr_text(&out);
    assert!(err.contains("unreadable"), "读失败报的不是那一类：{err}");
    assert!(!err.contains("write failed"), "两条落盘类别串被互换了（读失败报成了写失败）：{err}");
    // 反泄漏两格（整批评审门·登记 (4)）：这一支把 `Err(_)` 整个丢掉 ⇒ 结构上带不出原串，
    // 但"结构上不会"不是断言。锚点是实测出来的：把那一处改成 `format!("…: {e} …")` 之后
    // stderr 读作 `sqlite: no such column: written_at_ms in SELECT slug, … llm_state WHERE machine=?1
    // at offset 56` ⇒ 第一格当场红；把第一格删掉再跑，第二格也红（逐格验过，见台账变异收据）。
    // 守的是守卫 12（类别串零插值）；路径那一族在这一支够不着——`read` 拿到的是已经开好的
    // conn，它的错误来自 prepare/step，带的是 SQL 文本而不是库路径。
    assert!(!err.contains("SELECT"), "SQL 原串漏进运维面了（守卫 12）：{err}");
    assert!(!err.contains("written_at_ms"), "rusqlite 的列名读数漏进运维面了（守卫 12）：{err}");
    assert_eq!(out.stdout, b"", "读失败那一支要把整轮挡下来：stdout 一个字节都不许有");
    assert_eq!(llm_calls_total(d), 0, "读失败的一轮不该有任何 LLM 调用落账：{err}");
}

#[test]
fn a_failed_state_write_downgrades_the_round_but_keeps_the_readouts() {
    // 判据 1 的最后一格（B3 那一格的去向照 `charge_counters` 的现成先例）：
    // `llm_state::write` 失败 ⇒ 判定降成 3，但 stdout 的读数照旧在场——那一轮的抽取真发生了。
    // 造法用触发器（`BEFORE INSERT ... RAISE(ABORT)`）而不是把库设成只读：只读会把
    // `Root::open` 与计数器写一起挡掉，那时红的就不是这一格。
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    import(d, "a", vec![turn("human", "上线前要先跑测试", 1_000),
                        turn("agent", "好的，跑过了", 1_001)]);
    open(d).ledger().conn().execute_batch(
        "CREATE TRIGGER llm_state_boom BEFORE INSERT ON llm_state
         BEGIN SELECT RAISE(ABORT, 'boom-canary'); END;").unwrap();
    let u = uid("human", "上线前要先跑测试", 1_000);
    let only = write_seat(d, 1, &full_round(&u, "上线前先跑测试", "每次合并之前先跑全量测试"));
    let flags = one_flags(&only);
    let out = with_box(d, &flags);
    assert_eq!(code(&out), 3, "写失败却不降判定 ⇒ 落盘坏了没人知道");
    let err = stderr_text(&out);
    // 精确子串（段 D 的 D3①）：原来断的是 `rotation state`，而读失败与写失败**两条类别串都含它**
    // ⇒ 两条互换不红。这一格改成只认写失败那一条才有的 `write failed`。
    assert!(err.contains("write failed"), "写失败报的不是那一类：{err}");
    assert!(!err.contains("boom-canary"), "SQL 原串漏进运维面了（守卫 12）：{err}");
    assert!(!err.contains(DATA), "类别串里带了库的路径名（守卫 12）：{err}");
    let v = stdout_json(&out);
    assert_eq!(v.as_object().unwrap().len(), 17, "故障分支把读数一起吞了（十七格）：{v}");
    assert_eq!((v["turns_scanned"].as_i64(), v["rows_written"].as_i64()), (Some(2), Some(1)));
    assert_eq!(llm_calls_total(d), 5, "计数器那一格在写失败之后照旧落账（同一层的先例）");
}
