//! `hub derive run` 的黑盒面（派发档 I-1 任务 2.2 / 2.3 的判据）。
//!
//! 五条口径由真进程钉住：
//! ① 每批一行 JSONL 读数，字段名与 `DeriveReadout` 同名字段一套，不自创第二套；
//! ② **收批的判据落在游标上，不落在 rows 上**——`Ok{rows:0}` 同时是"空批"与"整批被
//!    `INSERT OR IGNORE` 吞掉"，只有游标能分开两者（R-A 登记债第 5 条，控制器已定调）；
//! ③ 连续 `Fault` 到 `FAIL_STREAK`（默认 3，读 `RECOLLECT_DRAIN_FAIL_STREAK`）就熔断，
//!    退出码沿用 hub 既有约定（`Fault` ⇒ 运维失败 3）；
//! ④ P-5 的守卫：推导池的表域 ⊆ {card, anchor}，落库的边端点也必属该域（本轮不扩池到 fact）；
//! ⑤ 空批是合法读数（rc 0），不是失败。
use rc_distill::derive::run_derive;
use rc_ledger::water;
use rc_llm::mock::MockBackend;
use rc_memory::reader::{count_rows_since, rows_since};
use rc_memory::{AnchorRole, EntityContent, FactContent, MemTable, Provenance, Scope, ScopeKind,
                put_card, put_entity, reader};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NOW: i64 = 1_700_000_000_000;
/// 合格的"这批没有可推导内容"：四键里前三键齐全、三数组全空 ⇒ `ZeroOutput` ⇒ 走 `Ok` 并**推游标**。
const ZERO_OUTPUT: &str = r#"{"relationships":[],"chains":[],"timeline":[]}"#;
/// 契约缺键（少 `chains`/`timeline`）⇒ 整批丢 + `note_attempt`，第三次隔离。
const BAD_CONTRACT: &str = r#"{"relationships":[]}"#;

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }

fn data_dir(dir: &Path) -> PathBuf { dir.join("data") }

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-derive-run".into() }
}

fn prov(uid: &str) -> Vec<Provenance> {
    vec![Provenance { turn_uid: uid.to_owned(), anchor_role: AnchorRole::Human }]
}

/// 64 位十六进制的轮身份（契约那一层的形状），`i` 决定内容 ⇒ 一批里条条不同址。
fn turn_uid(i: usize) -> String {
    format!("{:0>64x}", i + 1)
}

fn card(title: &str, body: &str) -> FactContent {
    FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                  scope: scope(), valid_to_ms: None }
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .arg("--data-dir")
        .arg(data_dir(dir))
        .output()
        .expect("hub binary")
}

fn derive_run(dir: &Path, replies: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<&str> = vec!["derive", "run", "--replies"];
    let replies_s = replies.to_str().unwrap();
    args.push(replies_s);
    args.extend(extra);
    run(dir, &args)
}

fn code(out: &Output) -> i32 { out.status.code().unwrap() }

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// stdout 按行拆成 JSON 值（每批一行的契约）。空 stdout ⇒ 空向量。
fn lines(out: &Output) -> Vec<Value> {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l)
            .unwrap_or_else(|e| panic!("读数必须是逐行 JSON：{l:?}（{e}）")))
        .collect()
}

fn first(out: &Output) -> Value {
    let v = lines(out);
    assert_eq!(v.len(), 1, "应当恰好一行读数：{v:?}");
    v.into_iter().next().unwrap()
}

/// 写一个 `--replies` 脚本：`n` 份同一句答复。
fn script(dir: &Path, name: &str, reply: &str, n: usize) -> PathBuf {
    let p = dir.join(name);
    let arr: Vec<Value> = (0..n).map(|_| Value::String(reply.to_owned())).collect();
    std::fs::write(&p, Value::Array(arr).to_string()).unwrap();
    p
}

fn fault_script(dir: &Path, name: &str, slug: &str, n: usize) -> PathBuf {
    let p = dir.join(name);
    let arr: Vec<Value> = (0..n).map(|_| json!({"fail": slug})).collect();
    std::fs::write(&p, Value::Array(arr).to_string()).unwrap();
    p
}

/// `n` 张活卡（各带一条人工轮出处 ⇒ 进推导池）。
fn seed_cards(data: &Path, n: usize) -> Vec<rc_memory::MemKey> {
    let root = Root::open(data).unwrap();
    let keys = (0..n).map(|i| {
        put_card(root.memory(), &card(&format!("row {i:02}"), &format!("body {i:02}")),
                 &prov(&turn_uid(i)), NOW + i as i64).unwrap()
    }).collect();
    drop(root);
    keys
}

/// 边夹具：一个真锚 + 两张正文里逐字提到它的卡（`put_edges_anchored` 的共享 mention 门槛）。
/// 返回 `(锚 key, 卡1 key, 卡2 key)`，调用方按池里的位置别名（r1..rn）把它们接进答复。
fn seed_shared_entity_store(data: &Path) -> [rc_memory::MemKey; 3] {
    let root = Root::open(data).unwrap();
    let anchor = put_entity(root.memory(), &EntityContent {
        kind: "concept".into(), title: "Postgres".into(), scope: scope(), aliases: vec![],
    }, &prov(&turn_uid(200)), 900).unwrap();
    let c1 = put_card(root.memory(),
                      &card("ship behind a flag", "Until the migration finishes. Postgres stays."),
                      &prov(&turn_uid(201)), 1000).unwrap();
    let c2 = put_card(root.memory(),
                      &card("flagged rollout", "Same plan reworded; Postgres first."),
                      &prov(&turn_uid(202)), 2000).unwrap();
    drop(root);
    [anchor, c1, c2]
}

/// 池里的位置别名：`select_batch` 就是 `reader::rows_since(None, 40)`，所以编号 = 池里的序位 + 1。
fn alias_of(data: &Path, key: &rc_memory::MemKey) -> String {
    let root = Root::open(data).unwrap();
    let pool = rows_since(root.memory(), None, 40).unwrap();
    drop(root);
    let k = key.key_string();
    let idx = pool.iter().position(|r| r.key.key_string() == k)
        .unwrap_or_else(|| panic!("{k} 必在推导池里：{pool:?}"));
    format!("r{}", idx + 1)
}

/// 一行读数的公共形状（字段名一套不许有第二家）。
fn assert_readout_shape(v: &Value) {
    assert_eq!(v["schema_version"], json!(1), "{v}");
    assert_eq!(v["job"], json!("derive"), "{v}");
    assert!(v["batch"].is_number(), "{v}");
    assert!(v["rows"].is_number(), "{v}");
    assert!(v["rejected"].is_number(), "{v}");
    assert!(v["dropped"].is_boolean(), "{v}");
    assert!(v["quarantined"].is_number(), "{v}");
    assert!(v.is_object() && v.get("issue_id").is_some(), "issue_id 这一格必须在（没隔离就是 null）：{v}");
    assert!(v["cursor_moved"].is_boolean(), "{v}");
    assert!(v.get("lag_rows").is_some(), "{v}");
    // 观测面两格（W33-A10 · 评审缺陷②补线）：键必须在。值不在这里钉 0——回放脚本
    // 显式声明的 Timeout 会进 `timeouts`（故障链臂实测），0 的钉子住在用干净脚本的
    // `review_run_cli.rs` 与日路径的排期闸里。
    assert!(v.get("max_call_ms").is_some(), "{v}");
    assert!(v.get("timeouts").is_some(), "{v}");
}

// ── 判据 ①：每批一行读数 ────────────────────────────────────────────────────────

#[test]
fn derive_run_prints_one_json_line_per_batch_and_moves_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let [_, c1, c2] = seed_shared_entity_store(&data);
    let replies = script(dir.path(), "replies-1.json", &json!({
        "relationships": [
            {"from": alias_of(&data, &c1), "to": alias_of(&data, &c2),
             "kind": "part_of", "reason": "same plan reworded", "basis": ["r1", "r2", "r3"]}
        ],
        "chains": [], "timeline": []
    }).to_string(), 1);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "1", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = first(&out);
    assert_readout_shape(&v);
    assert_eq!(v["batch"], json!(1), "批号从 1 起：{v}");
    assert!(v["rows"].as_u64().unwrap() >= 1, "边 + 补铸 mention 至少一行：{v}");
    assert_eq!(v["cursor_moved"], json!(true), "非空批必须推游标：{v}");
    assert_eq!(v["dropped"], json!(false), "{v}");
    assert_eq!(v["quarantined"], json!(0), "{v}");
    assert_eq!(v["issue_id"], Value::Null, "没有隔离就没有 id：{v}");
    assert!(v["lag_rows"].is_number(), "余量读数必须是数字：{v}");
    // 游标真的动了：库里的水位行不再是"从没跑过"。
    let root = Root::open(&data).unwrap();
    let cur = water::read(root.ledger(), "derive", "").unwrap();
    assert!(cur.is_some(), "一轮成功的 derive 必须写下水位");
    assert!(cur.unwrap().turn_uid.contains(':'), "游标是 key_string（带表前缀）");
}

#[test]
fn derive_run_has_a_human_readable_form_too() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    seed_cards(&data, 2);
    let replies = script(dir.path(), "replies-text.json", ZERO_OUTPUT, 1);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "1"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("batch=1") && text.contains("cursor_moved=true"), "{text}");
}

// ── 判据 ②：收批的判据落在游标上 ───────────────────────────────────────────────

#[test]
fn derive_run_stops_after_two_empty_unmoved_batches_with_rc_zero() {
    let dir = tempfile::tempdir().unwrap();
    // 空库：没有活行 ⇒ `run_derive` 在选批为空时提前返回，游标一动不动。
    let root = Root::open(&data_dir(dir.path())).unwrap();
    drop(root);
    let replies = script(dir.path(), "replies-empty.json", ZERO_OUTPUT, 4);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "0", "--json"]);
    assert_eq!(code(&out), 0, "空批是合法读数，不是失败：{}", stderr_text(&out));
    let v = lines(&out);
    assert_eq!(v.len(), 2, "空批且游标未动连续两次就收批：{v:?}");
    for (i, line) in v.iter().enumerate() {
        assert_readout_shape(line);
        assert_eq!(line["batch"], json!(i + 1), "{line}");
        assert_eq!(line["rows"], json!(0), "{line}");
        assert_eq!(line["cursor_moved"], json!(false), "{line}");
    }
}

/// 这一条钉的是本任务最容易踩的坑：`Ok{rows:0}` 同时是"空批"与"整批被 `INSERT OR IGNORE`
/// 吞掉 / 模型答了零行"。**41 张卡 = 两批**（`DERIVE_MAX_ROWS_PER_BATCH` = 40），前两轮每轮
/// rows 都是 0 而游标每轮都在动 ⇒ 只按 rows 收批的实现会在这里少一行。
#[test]
fn derive_run_keeps_going_when_a_zero_row_batch_still_moves_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    seed_cards(&data, 41);
    let replies = script(dir.path(), "replies-41.json", ZERO_OUTPUT, 3);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "3", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = lines(&out);
    assert_eq!(v.len(), 3, "三批都要有读数（rows=0 但游标在动的那两批不许被当成跑空）：{v:?}");
    assert_eq!(v[0]["cursor_moved"], json!(true), "{v:?}");
    assert_eq!(v[1]["cursor_moved"], json!(true), "{v:?}");
    assert_eq!(v[2]["cursor_moved"], json!(false), "第三批之后池子空了：{v:?}");
    assert_eq!(v[0]["rows"], json!(0));
    assert_eq!(v[1]["rows"], json!(0));
}

#[test]
fn derive_run_stops_at_the_batch_cap_even_when_work_remains() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    seed_cards(&data, 41);
    let replies = script(dir.path(), "replies-cap.json", ZERO_OUTPUT, 1);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "1", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(lines(&out).len(), 1, "--max-batches 1 就只跑一批");
    let root = Root::open(&data).unwrap();
    let left = count_rows_since(root.memory(),
                                water::read(root.ledger(), "derive", "").unwrap()
                                    .map(|c| (c.at_ms, c.turn_uid))).unwrap();
    assert!(left > 0, "批上限到了不等于队列空了，余量必须还在：{left}");
}

// ── 判据 ③：连续失败熔断 ───────────────────────────────────────────────────────

#[test]
fn derive_run_breaks_the_fault_streak_and_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    seed_cards(&data, 2);
    let replies = fault_script(dir.path(), "replies-timeout.json", "timeout", 5);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "0", "--json"]);
    assert_eq!(code(&out), 3, "熔断走 hub 既有的运维失败码：{}", stderr_text(&out));
    let v = lines(&out);
    assert_eq!(v.len(), 3, "默认 FAIL_STREAK = 3：{v:?}");
    for line in &v {
        assert_readout_shape(line);
        assert_eq!(line["cursor_moved"], json!(false), "Fault 不推水位：{line}");
    }
    assert!(stderr_text(&out).contains("error: llm timeout"), "{}", stderr_text(&out));
    let root = Root::open(&data).unwrap();
    assert!(water::read(root.ledger(), "derive", "").unwrap().is_none(),
           "故障轮一行水位都不许写");
}

#[test]
fn derive_run_reads_the_fail_streak_from_the_drain_env() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    seed_cards(&data, 2);
    let replies = fault_script(dir.path(), "replies-timeout1.json", "timeout", 5);
    let out = Command::new(bin())
        .args(["derive", "run", "--replies", replies.to_str().unwrap(),
               "--max-batches", "0", "--json"])
        .arg("--data-dir").arg(&data)
        .env("RECOLLECT_DRAIN_FAIL_STREAK", "1")
        .output().unwrap();
    assert_eq!(code(&out), 3, "{}", stderr_text(&out));
    assert_eq!(lines(&out).len(), 1, "env 把熔断压到 1 次：{:?}", lines(&out));
}

// ── 判据 ④：隔离与丢批走既有码 ─────────────────────────────────────────────────

#[test]
fn derive_run_quarantines_a_poison_batch_and_reports_the_issue() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    seed_cards(&data, 2);
    let replies = script(dir.path(), "replies-bad.json", BAD_CONTRACT, 3);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "3", "--json"]);
    let v = lines(&out);
    assert_eq!(v.len(), 3, "{v:?}");
    assert_eq!(v[0]["dropped"], json!(true), "{v:?}");
    assert_eq!(v[0]["cursor_moved"], json!(false), "未到隔离上限不越批：{v:?}");
    assert_eq!(v[0]["issue_id"], Value::Null, "{v:?}");
    assert_eq!(v[2]["quarantined"], json!(1), "第三次隔离：{v:?}");
    assert!(v[2]["issue_id"].is_i64(), "隔离轮要给出 issue id：{v:?}");
    assert_eq!(v[2]["cursor_moved"], json!(true), "隔离必须越过这一批：{v:?}");
    // 丢批/隔离轮的终局按 hub 既有码：整批拒收 = 2。
    assert_eq!(code(&out), 2, "{}", stderr_text(&out));
    let status = run(dir.path(), &["derive", "status", "--json"]);
    assert_eq!(code(&status), 0, "{}", stderr_text(&status));
    assert_eq!(first(&status)["open_issues"], json!(1), "隔离的行要能被 status 看见");
}

// ── 判据 ⑤：用法形状 ───────────────────────────────────────────────────────────

#[test]
fn derive_run_rejects_a_zero_llm_timeout_before_touching_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let replies = script(dir.path(), "replies-zero-timeout.json", ZERO_OUTPUT, 1);
    let out = derive_run(dir.path(), &replies,
                        &["--max-batches", "1", "--llm-timeout-ms", "0", "--json"]);
    assert_eq!(code(&out), 3, "{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("--llm-timeout-ms"), "{}", stderr_text(&out));
    // 库根本没被开起来：`data` 那一格都不该存在。
    assert!(!data_dir(dir.path()).exists(), "旗标形状错就先于开库收口");
}

#[test]
fn derive_run_without_a_seat_source_is_an_operational_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["derive", "run", "--max-batches", "1", "--json"]);
    assert_eq!(code(&out), 3, "{}", stderr_text(&out));
}

// ── P-5 的守卫：推导池的表域 ⊆ {card, anchor}，边端点同域 ──────────────────────

/// 往 `fact` 表插一行活 fact（本轮不扩池到 fact，但守卫必须**不是空判**：
/// 库里得真有一条 fact 行，它进不了池这件事才有牙齿）。
fn seed_fact_row(data: &Path, subject_anchor_id: &str) {
    let root = Root::open(data).unwrap();
    root.memory().conn().execute(
        "INSERT INTO fact (id, subject_anchor_id, predicate, category, mode, text,
                           evidence_turn_uid, scope_kind, scope_key, source_agent,
                           device_id, created_at_ms, norm_version)
         VALUES ('f00000000000000000000000000000000000000000000000000000000000derive',
                 ?1, 'lives_in', 'observation', 'stated', 'a fact row',
                 ?2, 'project', 'proj-derive-run', 'rc-test', 'dev-test', 1500, 1)",
        rusqlite::params![subject_anchor_id, turn_uid(300).as_str()],
    ).unwrap();
    drop(root);
}

#[test]
fn derive_pool_domain_is_exactly_card_and_anchor_and_ignores_facts() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let [anchor, _, _] = seed_shared_entity_store(&data);
    seed_fact_row(&data, &anchor.id);

    let root = Root::open(&data).unwrap();
    // 夹具非空判：那条 fact 行确实活在库里（读侧认得它，只是推导池不要它）。
    assert_eq!(reader::fact_rows(root.memory(), None, None, None, 10).unwrap().len(), 1,
               "夹具的 fact 行必须是活行，否则下面那几条断言在空集上成立");
    let pool = rows_since(root.memory(), None, 100).unwrap();
    // ① 表域 ⊆ {card, anchor}
    for r in &pool {
        assert!(matches!(r.key.table, MemTable::Card | MemTable::Anchor),
                "推导池里出现了 {}/{} —— P-4 本轮不扩池到 fact", r.key.table.prefix(), r.key.id);
    }
    // ② 两个臂都在（域不是"只剩 card"，撤掉任一臂也红）
    assert!(pool.iter().any(|r| r.key.table == MemTable::Card), "card 臂不在池里：{pool:?}");
    assert!(pool.iter().any(|r| r.key.table == MemTable::Anchor), "anchor 臂不在池里：{pool:?}");
    // ③ 臂与臂之间不重复投递同一行（多一个臂 = 行限被悄悄减半）
    let mut keys: Vec<String> = pool.iter().map(|r| r.key.key_string()).collect();
    let n = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), n, "推导池里有重复行 ⇒ 臂表被加了重复项：{keys:?}");
    // ④ 那条 fact 行没进池
    assert!(!pool.iter().any(|r| r.key.id == "f00000000000000000000000000000000000000000000000000000000000derive"),
            "fact 行进了推导池：{pool:?}");
    // ⑤ 余量读数与选批同一口径（同一个臂表、同一个游标谓词）
    assert_eq!(count_rows_since(root.memory(), None).unwrap(), n as i64);
}

#[test]
fn derived_edge_endpoints_stay_inside_the_card_anchor_domain() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let [anchor, c1, c2] = seed_shared_entity_store(&data);
    seed_fact_row(&data, &anchor.id);
    let replies = script(dir.path(), "replies-edge.json", &json!({
        "relationships": [
            {"from": alias_of(&data, &c1), "to": alias_of(&data, &c2),
             "kind": "part_of", "reason": "same plan reworded", "basis": ["r2", "r3"]}
        ],
        "chains": [], "timeline": []
    }).to_string(), 1);
    // 池里还有那条 fact 行吗？没有（上一用例钉住），所以这里不需要它在批内。
    let out = derive_run(dir.path(), &replies, &["--max-batches", "1", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert!(first(&out)["rows"].as_u64().unwrap() >= 1, "{:?}", lines(&out));

    let root = Root::open(&data).unwrap();
    let mut st = root.memory().conn().prepare(
        "SELECT from_id, to_id, relation FROM fact_link ORDER BY from_id, to_id").unwrap();
    let edges = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                                         r.get::<_, String>(2)?)))
        .unwrap().map(|x| x.unwrap()).collect::<Vec<_>>();
    assert!(!edges.is_empty(), "边一行都没落 ⇒ 这一条断言是在空集上成立");
    for (from, to, relation) in &edges {
        for id in [from, to] {
            let table = id.split(':').next().unwrap_or("");
            assert!(table == "card" || table == "anchor",
                    "端点 {id}（relation={relation}）落在推导池域外");
        }
    }
}

/// `run_derive` 自己那条"空批不推游标"的老律在 CLI 循环里仍然成立（库面直调，不经进程）。
#[test]
fn empty_batch_never_advances_the_watermark_even_through_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let root = Root::open(&data).unwrap();
    let backend = MockBackend::scripted(vec![Ok(ZERO_OUTPUT.to_string())]);
    let (ro, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert_eq!(ro.rows, 0);
    assert!(matches!(exit, rc_distill::derive::DeriveExit::Ok));
    assert!(water::read(root.ledger(), "derive", "").unwrap().is_none());
}

// ── 派发档 I-2b · B-1：拒收码上读数面（`hub derive run` 的 JSONL）─────────────────

/// 读一格计数。**不用 `unwrap_u64()` 之外的宽容写法**：键缺席时 `v[k]` 是 `Value::Null`，
/// `as_u64()` 给 `None` ⇒ 这里必须响亮红，而不是把"没有这一格"读成"这一格是 0"。
/// 这条律的来由：B-1 要的正是"运维看得见是谁拒的"，一个缺席的键与一个 0 在分诊上是两回事。
fn count(v: &Value, key: &str) -> u64 {
    v.get(key)
        .unwrap_or_else(|| panic!("读数里少了 `{key}` 这一格：{v}"))
        .as_u64()
        .unwrap_or_else(|| panic!("`{key}` 必须是计数：{v}"))
}

/// 一条**真机制句**（不是 `PLACEHOLDER_MECHANISMS` 的整串，也不是任何环节行的正文）。
const REAL_MECHANISM: &str = "先立的那次改动把旧的容量口径顶掉了，于是回退不再按那条阈值触发";

fn count_links_and_causal(data: &Path) -> (i64, i64) {
    let root = Root::open(data).unwrap();
    let links: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    let causal: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    (links, causal)
}

/// **B-1.2 钉的那条等式**（也是 T-5 变异的那唯一一条用例）：
/// 臂 A 让三道闸各拒一条、且**没有任何别的拒收形状**（残差两格 = 0）⇒
/// `rejected == rejected_no_mechanism + rejected_no_evidence + rejected_anchor_gate`。
/// 臂 B 再把另外两种今天同样折进 `rejected` 的形状（枚举外 kind、链合并）也各来一条 ⇒
/// 完整恒等式 `rejected == 五格之和`。两臂在**同一个用例**里：删掉三类分计的任意一类，
/// 红的就只有这一条（`assert_readout_shape` 是共享助手，故意**不**读新键）。
#[test]
fn derive_run_splits_the_three_rejection_kinds_and_they_sum_to_rejected() {
    // ── 臂 A：三类各一条，残差为 0 ⇒ 派发档那一句"三类相加 = rejected" ──────────────
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let keys = seed_cards(&data, 4);
    let alias: Vec<String> = keys.iter().map(|k| alias_of(&data, k)).collect();
    // (a) 机制句是套话 ⇒ `no_mechanism`；(b) 机制真但一条依据行都没点名 ⇒ `no_evidence`；
    // (c) 结构归属的边，但两端既无共享 mention 也无逐字同实体 ⇒ I4 锚门槛（`anchor_gate`）。
    // 库里只有 card 行、没有实体锚 ⇒ `memory_mention` 恒空（母档 P-3 的那一格已知条件），
    // 所以 `part_of` 只可能被锚门槛挡下；而门口那道证据闸打不到它（P-4：`part_of` 不是机制类）。
    let reply_a = json!({
        "relationships": [{"from": alias[2], "to": alias[3], "kind": "part_of",
                           "reason": "结构归属，但两端没有共同提及", "basis": [&alias[2], &alias[3]]}],
        "chains": [
            {"from": [&alias[0], &alias[1]], "mechanism": "时间相邻", "type": "causes",
             "basis": [&alias[0]]},
            {"from": [&alias[1], &alias[2]], "mechanism": REAL_MECHANISM, "type": "enables",
             "basis": []}
        ],
        "timeline": [], "mentions": []
    });
    let replies = script(dir.path(), "replies-gates.json", &reply_a.to_string(), 1);
    let out = derive_run(dir.path(), &replies, &["--max-batches", "1", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = first(&out);
    assert_readout_shape(&v);
    let nm = count(&v, "rejected_no_mechanism");
    let ne = count(&v, "rejected_no_evidence");
    let gate = count(&v, "rejected_anchor_gate");
    assert_eq!((nm, ne, gate), (1, 1, 1), "三道闸各拒一条，读数必须分别落在自己那一格：{v}");
    assert_eq!(count(&v, "rejected_parse_other"), 0, "这一臂没有第四种拒收形状：{v}");
    assert_eq!(count(&v, "rejected_chain_merged"), 0, "这一臂没有链被合并：{v}");
    assert_eq!(count(&v, "rejected"), nm + ne + gate,
        "总格 `rejected` 必须恰好等于三类之和（派发档 I-2b · B-1.2）：{v}");
    assert_eq!(count_links_and_causal(&data), (0, 0), "三条都被拒 ⇒ 库里一个字节都不许多");

    // ── 臂 B：再加上"没有码的单条拒收"与"链合并"两型 ⇒ 完整恒等式 ─────────────────
    let dir2 = tempfile::tempdir().unwrap();
    let data2 = data_dir(dir2.path());
    let keys2 = seed_cards(&data2, 4);
    let a2: Vec<String> = keys2.iter().map(|k| alias_of(&data2, k)).collect();
    let good = json!({"from": [&a2[0], &a2[1]], "mechanism": REAL_MECHANISM, "type": "causes",
                      "basis": [&a2[2]]});
    let reply_b = json!({
        "relationships": [
            {"from": &a2[2], "to": &a2[3], "kind": "part_of",
             "reason": "无共享 mention", "basis": [&a2[2], &a2[3]]},
            // 枚举外的 kind（共现不是关系）⇒ parse 单条拒收，但**没有**证据闸的码 ⇒ `parse_other`
            {"from": &a2[0], "to": &a2[1], "kind": "co_occurs",
             "reason": "两条一起出现", "basis": [&a2[0]]}
        ],
        "chains": [
            {"from": [&a2[0], &a2[1]], "mechanism": "同一个实体", "type": "causes",
             "basis": [&a2[0]]},
            {"from": [&a2[1], &a2[2]], "mechanism": REAL_MECHANISM, "type": "prevents",
             "basis": []},
            good.clone(), good.clone()   // 同一 causal_type + 同一顺序环节 ⇒ 第二条被合并
        ],
        "timeline": [], "mentions": []
    });
    let replies2 = script(dir2.path(), "replies-gates5.json", &reply_b.to_string(), 1);
    let out2 = derive_run(dir2.path(), &replies2, &["--max-batches", "1", "--json"]);
    assert_eq!(code(&out2), 0, "{}", stderr_text(&out2));
    let w = first(&out2);
    assert_readout_shape(&w);
    let (nm, ne, gate, other, merged) = (count(&w, "rejected_no_mechanism"),
                                         count(&w, "rejected_no_evidence"),
                                         count(&w, "rejected_anchor_gate"),
                                         count(&w, "rejected_parse_other"),
                                         count(&w, "rejected_chain_merged"));
    assert_eq!((nm, ne, gate, other, merged), (1, 1, 1, 1, 1), "五型各一条：{w}");
    assert_eq!(count(&w, "rejected"), nm + ne + gate + other + merged,
        "`rejected` 的每一条都要有一个分计格子接着，否则分类格与总格各说各话：{w}");
    assert_eq!(count_links_and_causal(&data2), (0, 1),
        "只有那条合法的链落库（因果头行 1 条），边一行都不许落：{w}");
}
