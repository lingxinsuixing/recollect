use rc_store::Root;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn write_fixture(dir: &Path, name: &str, body_json: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body_json).unwrap();
    p
}

const OK: &str = r#"{"format_version":1,"sessions":[{"session_key":"ses-A","device_id":"dev-1","source_agent":"qoder","turns":[{"role":"human","body":"CANARY-BODY-8842","created_at_ms":1730000000000}]}]}"#;

#[test]
fn import_accepts_and_reports_counts_only() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "ok.json", OK);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["turns_accepted"], 1);
    assert_eq!(v["turns_rejected"], 0);
    assert_eq!(v["schema_version"], 1);
    // 守卫 12 的金丝雀：正文一个字都不许出现在输出里
    assert!(!text.contains("CANARY-BODY-8842"), "输出泄露正文：{text}");
    assert!(!text.contains(&dir.path().display().to_string()), "输出泄露路径：{text}");
    // 上一条在 Windows 上会漏：JSON 把 `\` 转义成 `\\`，整串就不再是子串。路径里那个唯一的
    // 临时目录组件名不含分隔符，转义不动它——所以用它再断一次。
    let tmp_component = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "输出泄露临时目录：{text}");
}

/// 导入必须把读侧要的索引层留下。§10.148 的现场：中心与新建臂的 `index status` 都是
/// `rebuilt_at_ms:null`、shadow 表压根不存在，任何召回都吃 `index_not_built`——因为建索引的
/// 四条路径（`cycle once`、复制接收、恢复、手动 `hub index rebuild`）里没有一条挂在导入上。
#[test]
fn an_import_leaves_the_read_side_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    // `index status` 本身就打印 JSON（USAGE 的 `index rebuild|status [--data-dir DIR]` 没有 --json 这一格）
    let status = |args: &[&str]| -> serde_json::Value {
        let out = Command::new(bin())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap()
    };
    // 对照（阴性）：一座只被打开过、没导入过的库，读数必须是"没建索引"。
    let before = status(&["index", "status", "--data-dir", dd]);
    assert!(before["rebuilt_at_ms"].is_null(), "空库该是没建索引：{before}");

    let f = write_fixture(dir.path(), "ok.json", OK);
    let imported = status(&["import", f.to_str().unwrap(), "--data-dir", dd, "--json"]);
    assert_eq!(imported["turns_accepted"], 1);

    // 实验：导入之后同一支读数必须非空 ⇒ 读侧不用再等人手动 rebuild。
    let after = status(&["index", "status", "--data-dir", dd]);
    assert!(
        !after["rebuilt_at_ms"].is_null(),
        "导入之后索引层必须已经建好（否则召回要人手动 rebuild 才有结果）：{after}"
    );
    // `rows_indexed` 这里**可以是 0**：索引的是记忆行（卡/事实/事件），而这条夹具只进了一轮
    // 转录、还没抽取过任何记忆行。所以"影子层真的建出来了"才是可判的那一条。
    assert!(after["rows_indexed"].as_i64().is_some(), "读数得给出行数这一格：{after}");
    let layer = rc_index::shadow_tables(Root::open(dir.path()).unwrap().memory().conn()).unwrap();
    assert!(!layer.is_empty(), "影子层没建出来：{after}");
}

#[test]
fn duplicate_turn_uid_exits_2_and_names_the_code() {
    let dir = tempfile::tempdir().unwrap();
    let dup = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"d","source_agent":"a","turns":[
        {"role":"human","body":"x","created_at_ms":1,"turn_uid":"u1","tier":"id_bearing"},
        {"role":"human","body":"y","created_at_ms":2,"turn_uid":"u1","tier":"id_bearing"}]}]}"#;
    let f = write_fixture(dir.path(), "dup.json", dup);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["batch_reject"], "duplicate_turn_uid");
    assert_eq!(v["turns_accepted"], 0);
}

#[test]
fn second_import_of_same_file_reports_all_duplicates_and_still_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "ok.json", OK);
    let dd = dir.path().to_str().unwrap();
    for _ in 0..2 {
        let out = Command::new(bin()).args(["import", f.to_str().unwrap(), "--data-dir", dd]).output().unwrap();
        assert!(out.status.success());
    }
    let out = Command::new(bin()).args(["import", f.to_str().unwrap(), "--data-dir", dd, "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["turns_accepted"], 0);
    assert_eq!(v["skipped_duplicate"], 1, "幂等命中不是错误，也不是丢数据");
}

#[test]
fn data_dir_pointing_at_a_regular_file_exits_3_and_leaks_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "ok.json", OK);
    // --data-dir 指着一个已存在的普通文件：Root::open 必炸，这是运维失败（可重试），不是坏文件。
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", f.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let so = String::from_utf8_lossy(&out.stdout).to_string();
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    let p = dir.path().display().to_string();
    assert!(!so.contains(&p), "stdout 泄露路径：{so}");
    assert!(!se.contains(&p), "stderr 泄露路径：{se}");
    assert!(so.is_empty(), "运维失败不印报告：{so}");
}

#[test]
fn reject_counter_attributes_to_the_rejecting_sessions_device() {
    let dir = tempfile::tempdir().unwrap();
    let two = r#"{"format_version":1,"sessions":[
        {"session_key":"ses-A","device_id":"dev-1","source_agent":"qoder","turns":[{"role":"human","body":"CANARY-ATTR-A","created_at_ms":1}]},
        {"session_key":"ses-B","device_id":"dev-2","source_agent":"qoder","turns":[{"role":"wizard","body":"CANARY-ATTR-B","created_at_ms":2}]}]}"#;
    let f = write_fixture(dir.path(), "two.json", two);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let rows = usage(dir.path());
    let find = |dev: &str, metric: &str| -> Option<i64> {
        rows.iter()
            .find(|r| r.subject_kind == "device" && r.subject_id == dev && r.metric == metric)
            .map(|r| r.count)
    };
    assert_eq!(find("dev-1", "ingest_turns_accepted"), Some(1));
    assert_eq!(find("dev-2", "ingest_turns_rejected"), Some(1), "拒收必须归到真正犯错的设备");
    assert_eq!(find("dev-1", "ingest_turns_rejected"), None, "别人的错不记到 dev-1 头上");
    assert_eq!(find("dev-2", "ingest_turns_accepted"), None);
    assert!(!rows.iter().any(|r| r.subject_id == "unknown"), "可归属时不许退化到 unknown");
    assert!(
        !rows.iter().any(|r| r.subject_id.contains("CANARY")),
        "subject_id 里出现了正文：{rows:?}"
    );
}

#[test]
fn truly_empty_body_is_a_whole_batch_reject_counted_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let empty = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-x","source_agent":"a","turns":[{"role":"human","body":"","created_at_ms":1}]}]}"#;
    let f = write_fixture(dir.path(), "empty.json", empty);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["turns_rejected"], 1);
    assert_eq!(v["batch_reject"], "body_empty");
    let rows = usage(dir.path());
    let rej: i64 = rows
        .iter()
        .filter(|r| r.subject_id == "dev-x" && r.metric == "ingest_turns_rejected")
        .map(|r| r.count)
        .sum();
    assert_eq!(rej, 1, "parse 平面已 exit，normalize 平面不可能再叠一次");
    assert!(!rows.iter().any(|r| r.metric == "ingest_turns_accepted"), "无 accepted 却造出 accepted 行");
}

#[test]
fn whitespace_only_body_is_a_per_item_reject_on_the_other_plane() {
    let dir = tempfile::tempdir().unwrap();
    let ws = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-y","source_agent":"a","turns":[{"role":"human","body":"   ","created_at_ms":1}]}]}"#;
    let f = write_fixture(dir.path(), "ws.json", ws);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "归一后为空走 normalize 平面：逐条拒收");
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["turns_rejected"], 1);
    assert_eq!(v["batch_reject"], "");
    assert_eq!(v["rejects"][0]["code"], "body_empty");
    let rows = usage(dir.path());
    assert_eq!(
        rows.iter()
            .filter(|r| r.subject_id == "dev-y" && r.metric == "ingest_turns_rejected")
            .map(|r| r.count)
            .sum::<i64>(),
        1
    );
    assert!(!rows.iter().any(|r| r.count == 0), "delta 为 0 的 bump 不该留下幽灵行");
}

#[test]
fn repo_fixture_imports_cleanly_and_second_pass_skips_all() {
    let dir = tempfile::tempdir().unwrap();
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/exchange/synthetic_basic.json");
    let dd = dir.path().to_str().unwrap();
    let run = || {
        let out = Command::new(bin())
            .args(["import", fx.to_str().unwrap(), "--data-dir", dd, "--json"])
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    };
    let (code, text1) = run();
    assert_eq!(code, Some(0), "{text1}");
    let v: serde_json::Value = serde_json::from_str(&text1).unwrap();
    assert_eq!(v["turns_accepted"], 3);
    assert_eq!(v["turns_rejected"], 0);
    assert_eq!(v["skipped_duplicate"], 0);
    let (code, text2) = run();
    assert_eq!(code, Some(0), "{text2}");
    let v: serde_json::Value = serde_json::from_str(&text2).unwrap();
    assert_eq!(v["turns_accepted"], 0);
    assert_eq!(v["skipped_duplicate"], 3);
    let tmp_component = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
    for text in [&text1, &text2] {
        for canary in ["hello", "world", "先跑测试"] {
            assert!(!text.contains(canary), "夹具正文出现在输出里：{canary}");
        }
        assert!(!text.contains(&dir.path().display().to_string()), "输出泄露路径：{text}");
        // 同上：JSON 转义会绕过整串子串断言，用不含分隔符的临时目录组件名再断一次。
        assert!(!text.contains(&tmp_component), "输出泄露临时目录：{text}");
    }
}

/// 空白 device_id 能过 `parse`（它不是空串），但绝不能当计数器主语——否则账上多一行
/// 主语是三个空格、谁也查不到的真行数。
#[test]
fn blank_device_id_is_attributed_to_unknown_on_both_planes() {
    let dir = tempfile::tempdir().unwrap();
    let blank = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"   ","source_agent":"a","turns":[{"role":"human","body":"CANARY-BLANK","created_at_ms":1},{"role":"wizard","body":"x","created_at_ms":2}]}]}"#;
    let f = write_fixture(dir.path(), "blank.json", blank);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "一条 accepted、一条逐条拒");
    let rows = usage(dir.path());
    let subjects: Vec<&str> = rows.iter().map(|r| r.subject_id.as_str()).collect();
    assert!(!subjects.iter().any(|s| s.trim().is_empty()), "幽灵主语进了账库：{subjects:?}");
    for metric in ["ingest_turns_accepted", "ingest_turns_rejected"] {
        assert_eq!(
            rows.iter().filter(|r| r.subject_id == "unknown" && r.metric == metric).count(),
            1,
            "{metric} 该归到 unknown"
        );
    }
    assert!(!String::from_utf8_lossy(&out.stdout).contains("CANARY-BLANK"));
}

/// 形状像路径的 `device_id` claim 能过 `parse`（身份字段逐字节照抄、不清洗），但它不是一个
/// 可归因的主语：两个平面的账都落 unknown，审计行的署名留空。导入本身照旧成功——
/// 猜错的署名不该让一份好文件失败。
#[test]
fn path_shaped_device_claim_lands_on_unknown_on_both_planes() {
    let dir = tempfile::tempdir().unwrap();
    let hostile = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"C:/Users/victim/.ssh/id_rsa","source_agent":"a","turns":[{"role":"human","body":"CANARY-CLAIM","created_at_ms":1},{"role":"wizard","body":"x","created_at_ms":2}]}]}"#;
    let f = write_fixture(dir.path(), "claim.json", hostile);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "一条 accepted、一条逐条拒");
    let rows = usage(dir.path());
    for metric in ["ingest_turns_accepted", "ingest_turns_rejected"] {
        assert_eq!(
            rows.iter().filter(|r| r.subject_id == "unknown" && r.metric == metric).count(),
            1,
            "{metric} 该归到 unknown"
        );
    }
    let subjects: Vec<&str> = rows.iter().map(|r| r.subject_id.as_str()).collect();
    assert!(
        !subjects.iter().any(|s| s.contains('/') || s.contains('\\') || s.contains(".ssh")),
        "路径形状的主语进了账库：{subjects:?}"
    );
    assert_eq!(import_log(dir.path())[0].0, None, "审计署名不许是路径");
    let so = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!so.contains("id_rsa") && !so.contains("CANARY-CLAIM"), "报告泄露 claim：{so}");
}

/// `--help` 不是运维失败：clap 把 help/version 也当 Err 返回，那一支必须走 0。
#[test]
fn help_and_version_exit_0_and_print_to_stdout() {
    for flag in ["--help", "-h", "--version", "-V"] {
        let out = Command::new(bin()).args([flag]).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{flag} 被当成运维失败了：{}", String::from_utf8_lossy(&out.stderr));
        assert!(!out.stdout.is_empty(), "{flag} 的输出该给看终端的人：stdout 空");
        // 只有 version 这支断"无路径形状"：help 里 `--data-dir` 的默认值 `./data` 自带分隔符。
        if flag.contains("version") || flag == "-V" {
            let so = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(!so.contains('/') && !so.contains('\\'), "{flag} 的输出里有路径形状：{so}");
        }
    }
}

/// 用法错误里不许回显操作员敲的那个 token（它通常就是一个路径）。
#[test]
fn usage_error_does_not_echo_the_offending_token() {
    let out = Command::new(bin()).args(["C:/Users/victim/.ssh/id_rsa"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("invalid invocation"), "类别要报得出来：{text}");
    for leak in ["id_rsa", "/Users", "victim"] {
        assert!(!text.contains(leak), "用法错误回显了操作员的路径：{text}");
    }
    assert!(out.stdout.is_empty(), "用法错误的文本走 stderr：{text}");
}

fn usage(dir: &Path) -> Vec<rc_ledger::counters::UsageRow> {
    let root = rc_store::Root::open(dir).unwrap();
    rc_ledger::counters::read_all(root.ledger()).unwrap()
}

fn import_log(dir: &Path) -> Vec<(Option<String>, String, i64, i64)> {
    let root = rc_store::Root::open(dir).unwrap();
    let mut st = root
        .ledger()
        .conn()
        .prepare(
            "SELECT device_id, batch_reject_code, turns_accepted, turns_rejected
             FROM import_log ORDER BY id",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

/// 敲错参数不是"这批文件被整批拒收"：2 会让下游把好文件当毒文件，所以用法错误走 3。
#[test]
fn invocation_errors_exit_3_and_print_no_report() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let cases: Vec<Vec<String>> = vec![
        vec![],
        vec!["import".into()],
        vec!["import".into(), "--data-dir".into(), dd.into()],
        vec!["--bogus".into()],
        vec!["nope".into()],
    ];
    for args in cases {
        let out = Command::new(bin()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "args={args:?} stderr={}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "用法错误不许伪造一份导入报告：args={args:?}");
    }
}

/// parse 平面的整批拒收同样要留审计行，且 code 与 stdout 用同一个 snake_case 字符串。
#[test]
fn whole_batch_reject_on_the_parse_plane_still_writes_an_audit_row() {
    let dir = tempfile::tempdir().unwrap();
    let empty = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-x","source_agent":"a","turns":[{"role":"human","body":"","created_at_ms":1}]}]}"#;
    let f = write_fixture(dir.path(), "empty.json", empty);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let rows = import_log(dir.path());
    assert_eq!(rows.len(), 1, "被拒的导入在审计里必须留痕");
    assert_eq!(rows[0].1, v["batch_reject"], "审计行的 code 要和当场报告一致");
    assert_eq!(rows[0].0.as_deref(), Some("dev-x"), "单会话文件的归因设备");
    assert_eq!(rows[0].3, 1);
}

/// JSON 本体就读不出时没有会话可归因——这一条是 R11-b 的 `unknown` 出口，缺它等于没测到。
#[test]
fn unattributable_reject_falls_back_to_unknown_subject() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "garbage.json", "{ this is not json");
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let rows = usage(dir.path());
    let unknown = rows
        .iter()
        .find(|r| r.subject_kind == "device" && r.subject_id == "unknown" && r.metric == "ingest_turns_rejected")
        .expect("归不了因的拒收要落在 unknown 上，而不是消失");
    assert_eq!(unknown.count, 1);
    assert_eq!(import_log(dir.path()).len(), 1);
}

/// 一条 turn 同时缺 created_at_ms 又是空正文 ⇒ 两个分型，但只有一条被拒的 turn。
#[test]
fn two_defects_on_one_turn_count_as_one_rejected_turn() {
    let dir = tempfile::tempdir().unwrap();
    let both = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-d","source_agent":"a","turns":[{"role":"human","body":""}]}]}"#;
    let f = write_fixture(dir.path(), "both.json", both);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["rejects"].as_array().unwrap().len(), 2, "两个原因都不许吞");
    assert_eq!(v["turns_rejected"], 1, "计数读数是 turn 条数，不是分型条数");
    let rows = usage(dir.path());
    assert_eq!(
        rows.iter()
            .filter(|r| r.subject_id == "dev-d" && r.metric == "ingest_turns_rejected")
            .map(|r| r.count)
            .sum::<i64>(),
        1
    );
    assert_eq!(import_log(dir.path())[0].3, 1, "审计行与报告同一个口径");
}

/// 拒收原因里的可控文本（role 的值、越界键的键名）可能是正文或路径——CLI 侧的最后一道网。
#[test]
fn input_controlled_reject_text_never_reaches_the_operator() {
    let dir = tempfile::tempdir().unwrap();
    let hostile = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-h","source_agent":"a","C:/Users/victim/.ssh/id_rsa":"v","turns":[{"role":"CANARY-ROLE secret-of-the-user","body":"x","created_at_ms":1}]}]}"#;
    let f = write_fixture(dir.path(), "hostile.json", hostile);
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "越界键必须整批拒收");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("json_malformed"), "分型还得报得出来：{text}");
    for leak in ["secret-of-the-user", "id_rsa", "/Users", "victim"] {
        assert!(!text.contains(leak), "输出泄露可控文本 {leak}：{text}");
    }
    let rows = usage(dir.path());
    assert!(
        !rows.iter().any(|r| r.subject_id.contains("/Users") || r.subject_id.contains(' ')),
        "subject_id 里出现了路径或空白串：{rows:?}"
    );
}
