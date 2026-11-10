//! doctor 的运维面读数：读数形状、退出码分界、零正文/零路径两张网都要能跑红。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-doctor")
}

/// cargo 只给**本包**的 bin 定义 `CARGO_BIN_EXE_*`：`CARGO_BIN_EXE_recollect-hub` 在这里未定义，
/// 把 hub 写成 dev 依赖也不行（它是无 lib 的 bin 包，cargo 直接忽略并警告）。两个 bin 同在一个
/// target 目录，所以按邻居找——并断它不比 hub 的源码旧：`cargo test -p doctor` 不会重建 hub，
/// 一条上周的产物能让这条端到端断言测到旧行为。
fn hub_bin() -> PathBuf {
    let exe = Path::new(bin())
        .parent()
        .unwrap()
        .join(format!("recollect-hub{}", std::env::consts::EXE_SUFFIX));
    assert!(exe.is_file(), "找不到同目录的 recollect-hub 产物：请跑 cargo test --workspace");
    let built = exe.metadata().unwrap().modified().unwrap();
    let hub_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hub");
    if let Some(newest) = newest_under(&hub_dir) {
        assert!(newest <= built, "hub 产物早于 hub 源码改动：先跑 cargo test --workspace");
    }
    exe
}

fn newest_under(dir: &Path) -> Option<SystemTime> {
    let mut best = None::<SystemTime>;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let t = if p.is_dir() {
            newest_under(&p)
        } else {
            p.metadata().ok().and_then(|m| m.modified().ok())
        };
        if let Some(t) = t {
            best = Some(best.map_or(t, |b| b.max(t)));
        }
    }
    best
}

/// 用 hub 造一个有内容的库：空库证不出"正文没被带出来"。
fn import(dir: &Path, name: &str, body_json: &str) -> std::process::ExitStatus {
    let f = dir.join(name);
    std::fs::write(&f, body_json).unwrap();
    let data = dir.join("data");
    Command::new(hub_bin())
        .args([
            "import",
            f.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ])
        .status()
        .unwrap()
}

/// 诊断路径从 P1-5 起**只读**：库得先存在（`Root::open` 是唯一建库入口）。
/// 需要"空库读数"的用例用它铺底，别再指望 doctor 顺手建库。
fn empty_store(dir: &Path) {
    rc_store::Root::open(&dir.join("data")).unwrap();
}

fn read(dir: &Path, extra: &[&str]) -> std::process::Output {
    let data = dir.join("data");
    let mut args: Vec<&str> = vec!["--capabilities"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--data-dir", data.to_str().unwrap()]);
    Command::new(bin()).args(&args).output().unwrap()
}

#[test]
fn capabilities_reports_versions_and_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    // 先造一个有内容的库，才能证明正文没被带出来
    std::fs::write(dir.path().join("ex.json"),
        r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-1","source_agent":"a","turns":[{"role":"human","body":"CANARY-7741","created_at_ms":1}]}]}"#).unwrap();
    let status = Command::new(hub_bin())
        .args(["import", dir.path().join("ex.json").to_str().unwrap(), "--data-dir", dir.path().join("data").to_str().unwrap()])
        .status().unwrap();
    assert!(status.success());

    let out = Command::new(bin())
        .args(["--capabilities", "--json", "--data-dir", dir.path().join("data").to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["fts5"], true);
    assert_eq!(v["norm_version"], 1);
    assert_eq!(v["transcript_schema"], rc_store::ddl::TRANSCRIPT_VERSION);
    // R12-e：这一项必须**在场**（`v["memory_schema"]` 对不存在的键也返回 Null，所以"键在不在"
    // 与"值是什么"分两条断）。任务 1 起 Root::open 迁移三库 ⇒ 记忆层报版本，不再是 null。
    assert!(v.get("memory_schema").is_some(), "memory_schema 这一项必须在");
    // 不写死版本号：这一格测的是 doctor **把库里的版本渲染出来**，不是"当前设计版本是几"。
    // 版本号本身的硬地板留在 `phase3_ddl::fresh_db_is_v5`（那里 `== 5` 是设计值）。
    assert_eq!(v["memory_schema"], rc_store::ddl::MEMORY_VERSION);
    assert!(v["counters"]["device:dev-1:ingest_turns_accepted"].as_i64().unwrap() >= 1);
    assert!(!text.contains("CANARY-7741"), "诊断输出泄露正文");
    assert!(!text.contains(&dir.path().display().to_string()), "诊断输出泄露路径");
    // 上一条在 Windows 上会漏：JSON 把 `\` 转义成 `\\`，整串就不再是子串。路径里那个唯一的
    // 临时目录组件名不含分隔符，转义不动它——所以用它再断一次。
    let tmp_component = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "诊断输出泄露临时目录：{text}");
    assert!(!text.contains("data_dir"), "读数里不该有目录这一项：{text}");
}

/// 架构 §9 把告警区列进 doctor 字段清单，所以键的形状是承诺；阶段 0 没有任何告警条件，
/// 空数组是诚实的读法——"这一层存在，但没有东西触发"。
#[test]
fn alerts_field_is_present_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    empty_store(dir.path());
    let out = read(dir.path(), &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert!(v.get("alerts").is_some(), "告警区的键必须在");
    assert_eq!(v["alerts"].as_array().unwrap().len(), 0, "阶段 0 不许发明告警条件");
    assert_eq!(v["counters"].as_object().unwrap().len(), 0, "没导入过就不该有账");
    assert!(v.get("extract_gate").is_none(), "没跑过抽取闸门就不许造这一节");
}

/// 打不开库是运维失败（可重试），而 StoreError 的原串带着 data-dir 路径——所以这条网
/// 断的是失败路径：成功路径的金丝雀覆盖不到它。
#[test]
fn data_dir_pointing_at_a_regular_file_exits_3_and_leaks_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let out = Command::new(bin())
        .args(["--capabilities", "--data-dir", blocker.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let so = String::from_utf8_lossy(&out.stdout).to_string();
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(so.is_empty(), "运维失败不印半份读数：{so}");
    assert_eq!(se.trim(), "error: cannot open data store", "只许报类别：{se}");
    let p = dir.path().display().to_string();
    assert!(!so.contains(&p) && !se.contains(&p), "stderr 泄露路径：{se}");
    // `--data-dir` 的值是操作员亲手给的，rusqlite 的原串会把它整条抄回来；而这条失败在
    // 单个 Windows 临时目录上，`contains(&p)` 防不住 JSON/错误串里的分隔符转义。
    // 形状断言与下面的转义无关：分隔符原样出现在失败文本里即为泄漏。
    assert!(
        !se.contains('/') && !se.contains('\\'),
        "失败路径泄露分隔符形状：{se}"
    );
    let tmp_component = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!se.contains(&tmp_component), "stderr 泄露临时目录名：{se}");
}

/// 另一条打不开的形状，也是零路径**真咬**的那一条：`data-dir/transcript.db` 是个目录时，
/// rusqlite 的原串实测带着完整路径（`unable to open database file: <dir>\transcript.db`），
/// `Root::open(&dir)?` 会把 `StoreError` 的 Display 直接抄给操作员。
#[test]
fn unopenable_database_file_exits_3_and_leaks_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("transcript.db")).unwrap();
    let out = Command::new(bin())
        .args(["--capabilities", "--data-dir", data.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let so = String::from_utf8_lossy(&out.stdout).to_string();
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(so.is_empty(), "运维失败不印半份读数：{so}");
    assert_eq!(se.trim(), "error: cannot open data store", "只许报类别：{se}");
    let p = dir.path().display().to_string();
    assert!(!se.contains(&p), "stderr 泄露路径：{se}");
    // 类别行里连分隔符都不该有：实测原串是 `…: <tmp>\data\transcript.db`。
    assert!(
        !se.contains('/') && !se.contains('\\') && !se.contains(".db"),
        "stderr 里出现了路径形状：{se}"
    );
}

/// P1-5：诊断路径只读 ⇒ 库缺失时不再"顺手建一个空库"，而是运维失败（不做修复）。
/// 这条同时钉住"不建目录"：失败之后那条路径上什么都不许多出来。
#[test]
fn a_missing_store_is_operational_instead_of_being_created() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = Command::new(bin())
        .args(["--capabilities", "--data-dir", data.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty(), "失败路径不印半份读数");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "error: cannot open data store",
        "只许报类别"
    );
    assert!(!data.exists(), "doctor 在建库失败的路径上建了目录：{data:?}");
}

/// 没给 `--capabilities` 时无读数可产——这和"打不开库"同属运维失败，不是另一种成功。
#[test]
fn no_capability_flag_exits_3_and_names_the_flags_that_exist() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["--data-dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "没有读数不等于成功");
    let so = String::from_utf8_lossy(&out.stdout).to_string();
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(so.is_empty(), "无读数时 stdout 必须空：{so}");
    assert!(se.contains("--capabilities"), "类别行要说清有哪些旗：{se}");
    assert!(!se.contains(&dir.path().display().to_string()), "stderr 泄露路径：{se}");
}

/// `--help`/`--version` 是给人看的，不是运维失败：clap 把这两类也算 Err，那一支必须走 0。
#[test]
fn help_and_version_exit_0_and_print_to_stdout() {
    for flag in ["--help", "-h", "--version", "-V"] {
        let out = Command::new(bin()).args([flag]).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{flag} 被当成运维失败了：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty(), "{flag} 的输出该给看终端的人：stdout 空");
        // 只有 version 这支值得断"无路径形状"：help 里 `--data-dir` 的默认值 `./data`
        // 本来就带分隔符，那是 flag 文档不是泄漏。
        if flag.contains("version") || flag == "-V" {
            let so = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(
                !so.contains('/') && !so.contains('\\'),
                "{flag} 的输出里有路径形状：{so}"
            );
        }
    }
}

/// 用法错误也不能占 2（那是 hub 的"整批拒收"），且不许回显操作员敲的那个 token——它常常是个路径。
#[test]
fn usage_error_exits_3_and_does_not_echo_the_offending_token() {
    let out = Command::new(bin())
        .args(["--capabilities", "C:/Users/victim/.ssh/id_rsa"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("invalid invocation"), "类别要报得出来：{text}");
    for leak in ["id_rsa", "/Users", "victim"] {
        assert!(!text.contains(leak), "用法错误回显了操作员的路径：{text}");
    }
    assert!(out.stdout.is_empty(), "用法错误的文本走 stderr：{text}");
}

/// R12-b 的端到端那条网：形状像路径的 `device_id` claim 必须在**写入侧**就被归到 unknown，
/// doctor 只是回声。~~doctor 不依赖 rc-contract，所以它自己无从过滤~~（**10-17 就地更正，原句留着**：
/// 裁定 E 之后 doctor 依赖 `rc-contract` 了，但它只用自己的 `safe_token` 过**操作员给的**那个主语，
/// 转录里 claim 来的 `device_id` 仍不归它过滤 ⇒ 这句的结论不变、理由换了）。缺这条用例，泄漏只是理论上关掉了。
#[test]
fn path_shaped_device_claim_never_reaches_the_reading() {
    let dir = tempfile::tempdir().unwrap();
    let hostile = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"C:/Users/victim/.ssh/id_rsa","source_agent":"a","turns":[{"role":"human","body":"CANARY-7741","created_at_ms":1}]}]}"#;
    assert!(import(dir.path(), "hostile.json", hostile).success(), "怪署名 claim 不该让整份文件失败");

    let out = read(dir.path(), &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v["counters"]["device:unknown:ingest_turns_accepted"].as_i64().unwrap() >= 1);
    for leak in ["id_rsa", "/Users", "victim", "CANARY-7741"] {
        assert!(!text.contains(leak), "读数里出现了 {leak}：{text}");
    }
    // 分隔符会被 JSON 转义，所以用不含分隔符的那个临时目录组件名再断一次（见上用例的注释）。
    let tmp_component = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "读数里出现了本机目录：{text}");
    let keys: Vec<&str> = v["counters"].as_object().unwrap().keys().map(String::as_str).collect();
    assert!(
        !keys.iter().any(|k| k.contains('/') || k.contains('\\')),
        "计数器主语还是路径形状：{keys:?}"
    );
}

/// 人类可读模式也得把"这一节没有东西"和"读失败"分开：空的一节写成 none，有版本的地方写成数字。
/// （任务 1 起三库齐全 ⇒ memory_schema 报版本；null → none 那条渲染规则留给阶段 4 的副本半装状态。）
#[test]
fn human_mode_prints_versions_and_keeps_empty_sections_as_none() {
    let dir = tempfile::tempdir().unwrap();
    empty_store(dir.path());
    let out = read(dir.path(), &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    // 同 JSON 那一格：版本号从 `MEMORY_VERSION` 现取，不在测试里写死（写死的话每次 bump 都要
    // 手改一次，且测不到"渲染的版本"）。
    assert!(text.contains(&format!("memory_schema={}", rc_store::ddl::MEMORY_VERSION)),
            "记忆层的版本读数没印出来：{text}");
    assert!(text.contains("alerts=none"), "空的一节要读得出\"没有\"：{text}");
    assert!(text.contains("counters"), "读数行没带上账目：{text}");
    assert!(!text.contains(&dir.path().display().to_string()), "人类模式泄露路径：{text}");
    assert!(!text.contains("null"), "人类模式别把 JSON 的 null 抄过来：{text}");
    assert!(text.contains("fts5=true"), "布尔读数不许带引号：{text}");
    assert!(!text.contains('"'), "人类模式不许出现 JSON 引号：{text}");
}

/// 造一个账本：给 `n` 个 scope 各推一次水位、再各倒退一次 ⇒ `n` 个停轮 scope。
/// 倒退的那一次必须报错——停轮是"报错"这一支落笔的，造法变了这条用例得重写。
fn ledger_with_n_halted_scopes(data_dir: &Path, n: i64) {
    let root = rc_store::Root::open(data_dir).unwrap();
    let l = root.ledger();
    for i in 0..n {
        let scope = format!("dev-a/s-{i}");
        let cur = |ms: i64, uid: &str| rc_ledger::water::Cursor { at_ms: ms, turn_uid: uid.into() };
        rc_ledger::water::advance(l, "distill", &scope, &cur(2_000, "t"), 1).unwrap();
        let back = rc_ledger::water::advance(l, "distill", &scope, &cur(1_000, "u"), 2);
        assert!(back.is_err(), "水位倒退必须报错，停轮才有落笔处");
    }
    assert_eq!(rc_ledger::water::halted_scopes_count(l).unwrap(), n, "夹具没造出停轮行");
}

fn halt_alerts_in(alerts: &serde_json::Value) -> Vec<&serde_json::Value> {
    alerts.as_array().unwrap().iter().filter(|a| a["kind"] == "halt").collect()
}

/// `kind: halt` 这条告警的**正反两支**都钉：停轮 scope 为正 ⇒ 恰好一条、`halted_scopes`
/// 等于停轮数；为零 ⇒ 一条都没有（P-70 同族的条件化形状，与 `quota_alerts` 对称）。
///
/// 上一轮只有**反面**有钉子（存量那两条把 `alerts` 钉成恒空），所以把 `halt_alerts`
/// 的正向那一支改成永返 `Vec::new()` 全仓零红——正面那一支今天恒不出现也没有人发现。
///
/// **为什么这一条走子进程而不是纯函数递一次**：`doctor` 这个包只有 `[[bin]]`、没有 lib
/// 目标（`Cargo.toml` 里没有 `[lib]`，也没有 `src/lib.rs`），而 `halt_alerts` 是私有的，
/// 形参还是账本句柄（它读 `job_watermark`，本来就不是纯函数）⇒ 集成测试链不上那个函数，
/// 只能走同一个二进制的 `--capabilities --json` 那条路，把 `alerts` 区取出来断。
#[test]
fn halt_alert_shows_exactly_one_entry_when_scopes_are_halted_and_none_otherwise() {
    // 反支：有水位行、但一条停轮都没有 ⇒ `alerts` 区里不许出现 `kind == "halt"`。
    let quiet = tempfile::tempdir().unwrap();
    {
        let root = rc_store::Root::open(&quiet.path().join("data")).unwrap();
        rc_ledger::water::advance(root.ledger(), "distill", "dev-a/s-a",
            &rc_ledger::water::Cursor { at_ms: 2_000, turn_uid: "t".into() }, 1).unwrap();
        assert_eq!(rc_ledger::water::halted_scopes_count(root.ledger()).unwrap(), 0);
    }
    let out = read(quiet.path(), &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(halt_alerts_in(&v["alerts"]).len(), 0,
               "没有停轮 scope 却报了停轮告警：{:?}", v["alerts"]);

    // 正支：两个停轮 scope ⇒ 恰好一条，数字逐字对拍（不是"有就行"）。
    let halted = tempfile::tempdir().unwrap();
    ledger_with_n_halted_scopes(&halted.path().join("data"), 2);
    let out = read(halted.path(), &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let halts = halt_alerts_in(&v["alerts"]);
    assert_eq!(halts.len(), 1, "两个停轮 scope 恰好一条告警，多了少了都不行：{:?}", v["alerts"]);
    assert_eq!(halts[0]["halted_scopes"], 2,
               "告警里的数字是停轮 scope 数，不是第一行：{}", halts[0]);
}

// ---- ④「当前服务者」那一节（§4.8 ④ + 裁定 E；Task 7 段 C）----

/// 轮转态的一行**从写侧进**：生产路径上唯一的写者是 `hub cycle once`，而那一趟要跑完整一轮
/// 还得配 `--replies` 夹具；这一节断的是 doctor 的**读**（`WHERE machine=?1` 那一只手），
/// 所以直接调 `rc_ledger::llm_state::write`——与上面 `ledger_with_n_halted_scopes` 同一条路。
/// 句柄要在块内 drop，否则子进程打开同一个库时锁还没放。
fn write_state(dir: &Path, machine: &str, slug: &str, written_at_ms: i64) {
    let root = rc_store::Root::open(&dir.join("data")).unwrap();
    rc_ledger::llm_state::write(root.ledger(), &rc_ledger::llm_state::Row {
        machine: machine.to_owned(),
        slug: slug.to_owned(),
        probe_due_at_ms: None,
        probe_ok: 0,
        probe_failures: 0,
        written_at_ms,
    })
    .unwrap();
}

/// 两格都在场，且**逐字是表上那一格**：写 `reply-2` 读出 `reply-2`（不是 preset 的 `model_slug`，
/// 更不是对端回显——台账 §10(41) 抓到过 deepseek 请求 `deepseek-chat` 回传 `deepseek-flash`）。
/// 后半段是**反恒真**：同一台机器改成 `reply-1` 再读一次，读数必须跟着表走，
/// 否则"写死一个 reply-2"的实现也能绿第一条。
#[test]
fn the_serving_section_prints_the_two_cells_from_the_row() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), "dev-1", "reply-2", 1234);
    let out = read(dir.path(), &["--json", "--device-id", "dev-1"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["llm_state"]["serving"], "reply-2", "服务者要取表上那一格：{text}");
    assert_eq!(v["llm_state"]["written_at"], 1234, "「写于何时」要取表上那一格：{text}");
    // 既有读数不许因为新增这一节而消失。
    assert_eq!(v["fts5"], true, "--capabilities 的既有读数不在了");
    let tmp_component = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "服务者那一节泄露临时目录：{text}");
    assert!(!text.contains("data_dir"), "读数里不该有目录这一项：{text}");
    // 键名纪律由判据 35 那条 `zero` 在全仓 `*.rs` 上守（它连注释都扫，所以这里不能写出那个字面）：
    // 这一节的键逐字是 `llm_state`，与表名同源，不开第二个机器号命名空间。
    assert!(v.get("llm_state").is_some(), "这一节的键要逐字是表名：{text}");

    write_state(dir.path(), "dev-1", "reply-1", 5678);
    let out = read(dir.path(), &["--json", "--device-id", "dev-1"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["llm_state"]["serving"], "reply-1", "读数没跟着表走，这一节是写死的");
    assert_eq!(v["llm_state"]["written_at"], 5678, "同上：时刻那一格也要重读");
}

/// 缺身份 ⇒ 那一节是 `none`、退出码 0（裁定 E ③）。与下面那条 `Ok(None)` 用例是**同一个渲染、
/// 不同的到达路径**，两条都要在：把第一支折成 `operational(3)` 只红这一条，
/// 把第二支折成失败只红那一条。
#[test]
fn a_machine_without_identity_still_runs_and_renders_none() {
    let dir = tempfile::tempdir().unwrap();
    empty_store(dir.path());
    let json = Command::new(bin())
        .args(["--capabilities", "--json", "--data-dir",
               dir.path().join("data").to_str().unwrap()])
        // 测试进程可能带着 ambient 的 RECOLLECT_DEVICE_ID 跑，兜底那一只手必须真的为空。
        .env_remove("RECOLLECT_DEVICE_ID")
        .output()
        .unwrap();
    assert!(json.status.success(), "缺身份不等于失败：{}", String::from_utf8_lossy(&json.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(json.stdout).unwrap()).unwrap();
    assert!(v["llm_state"].as_object().unwrap().is_empty(), "缺身份却报了服务者：{v:?}");
    assert_eq!(v["fts5"], true, "半配置的机器上 doctor 仍要能跑：既有读数没了");

    let human = Command::new(bin())
        .args(["--capabilities", "--data-dir", dir.path().join("data").to_str().unwrap()])
        .env_remove("RECOLLECT_DEVICE_ID")
        .output()
        .unwrap();
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("llm_state=none"), "空的一节要读得出\"没有\"：{text}");
    assert!(!text.contains("null"), "人类模式不许把 JSON 的 null 抄过来：{text}");
}

/// 有身份、这台机器还没写过行 ⇒ 同样 `none`，同样不是失败（`read` 的 `Ok(None)` 那一支）。
#[test]
fn an_identity_with_no_row_written_yet_is_none_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    empty_store(dir.path());
    let out = read(dir.path(), &["--json", "--device-id", "dev-1"]);
    assert!(out.status.success(), "还没写过行不是运维失败：{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert!(v.get("llm_state").is_some(), "这一节的键必须在场");
    assert!(v["llm_state"].as_object().unwrap().is_empty(), "表里没行却报了服务者：{v:?}");
}

/// 机器主语是真的过滤器：表里有两行、写给别的机器 ⇒ 这一台读不出（`read` 那句 `WHERE machine=?1`）。
/// 反支同一条用例：主语对上时那一格读得出来，否则"恒不匹配"的实现也能绿。
#[test]
fn rows_written_by_other_machines_are_not_readable_here() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), "dev-1", "reply-2", 111);
    write_state(dir.path(), "dev-2", "reply-7", 222);

    let out = read(dir.path(), &["--json", "--device-id", "dev-3"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert!(v["llm_state"].as_object().unwrap().is_empty(), "别的机器写的行被这一台读出来了：{v:?}");

    let mine = read(dir.path(), &["--json", "--device-id", "dev-2"]);
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(mine.stdout).unwrap()).unwrap();
    assert_eq!(v["llm_state"]["serving"], "reply-7", "主语匹配时那一格要读得出来");
    assert_eq!(v["llm_state"]["written_at"], 222);
}

/// 身份形状坏（给了但不是合法 token）是**用法错误那一族**，与上面"缺身份"是两种后果：
/// 退出码 3、stdout 零字节（非 TTY 一律不提示输入），且不回显操作员敲进来的那个 token。
#[test]
fn a_malformed_device_id_is_operational_and_echoes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["--capabilities", "--device-id", "C:/Users/victim/.ssh/id_rsa",
               "--data-dir", dir.path().join("data").to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "形状坏的身份要响亮失败");
    assert!(out.stdout.is_empty(), "失败路径不印半份读数");
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(se.contains("device id"), "类别要说得出口：{se}");
    for leak in ["id_rsa", "/Users", "victim"] {
        assert!(!se.contains(leak), "stderr 回显了操作员的 token：{se}");
    }
    assert!(!se.contains('/') && !se.contains('\\'), "stderr 里出现了路径形状：{se}");
}

/// 四支处置的**第三支**（工单 §4.8 ④：有身份、表也在、就是读不出来 ⇒ `operational(3)`）
/// 此前的 witness 数是零：`cannot read` 那两个字面在测试侧只命中 `cannot open data store`。
/// 造法与 hub 侧那条同源：改列名（`DROP TABLE` 会被下一次开库的 `CREATE TABLE IF NOT EXISTS`
/// 补回一张空表 ⇒ `read` 返回 `Ok(None)`，那是第二支、不是这一支），句柄用同一台的
/// `rc_store::Root::open`，不新写第二套开库逻辑。
/// 反证格在前：**同一个库、同一个 `--device-id`、不做 ALTER** ⇒ 退出码 0 且那一节读得出 `reply-2`。
/// 没有这一格，"永远返回 Err 的实现"也能绿第一格。
#[test]
fn an_unreadable_serving_state_is_operational_not_none() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), "dev-1", "reply-2", 1234);

    let ok = read(dir.path(), &["--json", "--device-id", "dev-1"]);
    assert!(ok.status.success(), "反证：表正常时这一支要读得出：{}", String::from_utf8_lossy(&ok.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(ok.stdout).unwrap()).unwrap();
    assert_eq!(v["llm_state"]["serving"], "reply-2", "反证格要证明故障是 ALTER 造出来的，不是本来就读不出：{v:?}");

    rc_store::Root::open(&dir.path().join("data")).unwrap().ledger().conn().execute_batch(
        "ALTER TABLE llm_state RENAME COLUMN written_at_ms TO gone_written_at_ms").unwrap();

    let out = read(dir.path(), &["--json", "--device-id", "dev-1"]);
    assert_eq!(out.status.code(), Some(3), "读失败被折成 none 或 0 ⇒ 最坏的失败渲染成最好的读数");
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(se.contains("cannot read llm serving state"), "报的不是读失败那一类：{se}");
    assert!(out.stdout.is_empty(), "运维失败不印半份读数：{}", String::from_utf8_lossy(&out.stdout));
}
