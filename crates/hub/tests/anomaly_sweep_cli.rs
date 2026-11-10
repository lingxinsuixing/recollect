//! W26：`hub anomaly sweep` 的黑盒面——日保底档那一枪（`daily-drain.sh --full`）的产物。
//!
//! 这一面钉的是三件事，每件都在派发档 §3 里：
//! 1. **升**：两条活锚共享一串别名 ⇒ `alias_ambiguity` 这一族升起告警（逐 id 断言，不是"非空"）；
//! 2. **解**：把其中一条并走（真走 `anchor rebind`：`status='merged'` + 别名并进新锚）之后再扫
//!    ⇒ 这一族 `raised=0`、`resolved` 恰好收掉剩下的那一行——判据是**机械**的，不需要谁去认证；
//! 3. **四格 JSON**：`{"contradiction":{"raised","resolved"},"ghosts":{"raised","resolved"}}`
//!    逐格相等（多一个键、少一个键、值错一格都红），且 stdout 恰好一行。
//!
//! 一条口径要先说清（它不是缺陷，是 `attention` 的粒度）：**一行告警 = 一个认领方**。
//! `ambiguous_alias_claimants` 按 `(anchor_id, alias)` 出格，`alias_ambiguity` 的升/解也都按
//! `(kind, mem_table, mem_id)` 定位 ⇒ 两条活锚共享一串别名时升的是**两行**（每家一行），
//! `rc-memory/tests/ghosts.rs` 的 `t3` 钉着同一件事（两 claimant ⇒ 两行）。所以下面第一条用例
//! 断的是"两家各一行"，而不是派发档措辞里那个按**别名串**数的"一条"。
use rc_memory::keys::{AnchorRole, MemKey, Scope, ScopeKind};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::Provenance;
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NOW: i64 = 1_700_000_000_000;
/// 两家认领的**同一串**别名。它不是任何一条锚的正名——正名相同就是"同一格"，
/// 走不到歧义那一族（那条路是 `anchor merge` 的发现面）。
const SHARED: &str = "王工";

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }

fn data_dir(dir: &Path) -> PathBuf { dir.join("data") }

fn run(data: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).arg("--data-dir").arg(data).output().expect("hub binary")
}

fn code(out: &Output) -> i32 { out.status.code().unwrap() }

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

fn stderr_text(out: &Output) -> String { String::from_utf8_lossy(&out.stderr).to_string() }

/// 一条合成锚：`kind`/归属固定，只有正名与别名是变量。归属钉成常数是为了让"共享别名"这一个轴
/// 自己说话——跨归属同名在 `ambiguous_alias_claimants` 里**不分域**（它的 GROUP BY 只看别名串），
/// 但测试不该依赖那条口径：夹具先把它做成同域，判据就只剩歧义这一件事。
fn ent(m: &rc_store::MemoryHandle, title: &str, aliases: &[&str], at: i64) -> MemKey {
    put_entity(m, &EntityContent {
        kind: "person".into(),
        title: title.into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-w26".into() },
        aliases: aliases.iter().map(|s| (*s).to_owned()).collect(),
    }, &[Provenance { turn_uid: "t-w26".repeat(8), anchor_role: AnchorRole::Human }], at).unwrap()
}

/// 两条活锚共享 `SHARED`：`老王` 与 `老张` 都把它挂成自己的别名。
/// 第三格不存在（没有一条锚的正名叫 `SHARED`）⇒ 闭包那一支无事可做，歧义是这一轮唯一的产出。
fn two_claimants(data: &Path) -> (MemKey, MemKey) {
    let root = Root::open(data).expect("建库");
    let a = ent(root.memory(), "老王", &[SHARED], NOW);
    let c = ent(root.memory(), "老张", &[SHARED], NOW);
    (a, c)
}

fn ids_of(rows: &[(String, String)]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
    v.sort();
    v
}

/// 某一族当前的 `(mem_id, created_at_ms)`（只取未解除的行；降序无关，这里只排序后比集合）。
fn open_rows(data: &Path, kind: &str) -> Vec<(String, String)> {
    let root = Root::open(data).unwrap();
    let mut st = root.memory().conn().prepare(
        "SELECT mem_id, mem_table FROM attention
          WHERE kind=?1 AND resolved_at_ms IS NULL ORDER BY mem_id").unwrap();
    st.query_map([kind], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap().collect::<Result<Vec<_>, _>>().unwrap()
}

/// 某一族的全量行数（含已解除）——用来判"解除过的没有被删，只是盖了戳"。
fn family_total(data: &Path, kind: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory().conn().query_row(
        "SELECT count(*) FROM attention WHERE kind=?1", [kind], |r| r.get(0)).unwrap()
}

fn anchor_status(data: &Path, id: &str) -> String {
    let root = Root::open(data).unwrap();
    root.memory().conn().query_row(
        "SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0)).unwrap()
}

fn sweep_json(data: &Path) -> Value {
    let out = run(data, &["anomaly", "sweep", "--json"]);
    assert_eq!(code(&out), 0, "anomaly sweep 必须退出 0：{}", stderr_text(&out));
    stdout_json(&out)
}

/// ① 升：两条活锚共享一串别名 ⇒ `alias_ambiguity` 每家一行，指认的正是那两条锚。
/// 牙：把 `anomaly::sweep_ghosts` 的歧义那一支换回"只报一条"或漏掉一个 claimant，逐 id 断言会红。
#[test]
fn two_live_anchors_sharing_an_alias_raise_one_row_each() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let (a, c) = two_claimants(&data);

    let v = sweep_json(&data);
    assert_eq!(v["contradiction"], json!({"raised": 0, "resolved": 0}),
        "这一夹具里没有矛盾（只有别名歧义）：{v}");
    // 重影那一族 = 别名歧义 + 同题多行；两家的正名不同，同题那一族是 0。
    assert_eq!(v["ghosts"], json!({"raised": 2, "resolved": 0}),
        "两条活锚共享一串别名 ⇒ 每家一行告警（`attention` 的粒度是认领方，不是别名串）：{v}");

    let open = open_rows(&data, "alias_ambiguity");
    assert_eq!(ids_of(&open), {
        let mut v = vec![a.id.clone(), c.id.clone()];
        v.sort();
        v
    }, "告警必须指认两家，一家都不漏、一家都不多");
    assert!(open.iter().all(|(_, tbl)| tbl == "anchor"), "告警点落在锚表：{open:?}");

    // 幂等：同一份库再扫一次，三格必须纹丝不动（升的查重、解的面按"不再成立"收）。
    let again = sweep_json(&data);
    assert_eq!(again, json!({
        "contradiction": {"raised": 0, "resolved": 0},
        "ghosts": {"raised": 0, "resolved": 0},
    }), "同一状态重放一轮不该再升：{again}");
    assert_eq!(family_total(&data, "alias_ambiguity"), 2, "重放不许多出第三行");
}

/// ② 解：把其中一条并走（`status='merged'`）之后，歧义不再成立 ⇒ 新一轮 `raised=0`、
/// `resolved` 收掉剩下的那一行。
///
/// 为什么第一轮是 2 行而这一轮只 `resolved=1`：`anchor rebind` 在换绑的**同一个事务**里
/// 就把被并走那一格的告警解了（`rebind.rs` 第 5 步，`report.attention_resolved`），
/// 所以留给下一轮扫描的只有仍在世的那一格。两处解的不是同一行，数目也就各是各的。
/// 牙：把 `sweep_family` 的解除支撤掉 ⇒ 这一轮的 `resolved` 是 0、`open` 仍是 1 行。
#[test]
fn merging_one_claimant_resolves_the_rest_on_the_next_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let (a, c) = two_claimants(&data);

    let first = sweep_json(&data);
    assert_eq!(first["ghosts"], json!({"raised": 2, "resolved": 0}), "{first}");

    // 真走产品的换绑面（不是手写 UPDATE status）：别名并进新锚、旧锚转 `merged`、
    // 自己那一行告警当场解除。派发档说的"置 status='merged'"落在这一条命令的落点上。
    let out = run(&data, &[
        "anchor", "rebind", "--from", &c.id, "--into", &a.id, "--reason", "w26-merged",
    ]);
    assert_eq!(code(&out), 0, "换绑应成功：{}", stderr_text(&out));
    assert_eq!(anchor_status(&data, &c.id), "merged", "被并走的那一格必须留下 merged 这个出声的形状");

    let second = sweep_json(&data);
    assert_eq!(second["contradiction"], json!({"raised": 0, "resolved": 0}), "{second}");
    assert_eq!(second["ghosts"], json!({"raised": 0, "resolved": 1}),
        "只剩在世那一行的告警要由本轮扫描解掉（另一行已由换绑自己解了）：{second}");
    assert!(open_rows(&data, "alias_ambiguity").is_empty(), "这一族必须清空");
    assert_eq!(family_total(&data, "alias_ambiguity"), 2,
        "解除是盖戳不是删行：两行都还在，只是都已解除");
}

/// ③ 四格键面：空库上逐格相等（对象相等 ⇒ 多键/少键/错值都红），且 stdout 恰好一行 JSON。
/// 这把"四格"钉成契约而不是恰好能读出来的三个数：`ghosts` 是**聚合**格（歧义 + 同题），
/// 空库上它照样必须在、照样是 0。
#[test]
fn json_keys_are_exactly_the_four_cells_and_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    Root::open(&data).expect("建空库");

    let out = run(&data, &["anomaly", "sweep", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(stdout_json(&out), json!({
        "contradiction": {"raised": 0, "resolved": 0},
        "ghosts": {"raised": 0, "resolved": 0},
    }));
    let raw = String::from_utf8(out.stdout).unwrap();
    assert_eq!(raw.trim_end().lines().count(), 1, "stdout 必须恰好一行：{raw}");

    // 人读面：同四个数、`k=v` 两族各一对（脚本与人读的是同一份读数，不各说各话）。
    let human = run(&data, &["anomaly", "sweep"]);
    assert_eq!(code(&human), 0, "{}", stderr_text(&human));
    let line = String::from_utf8(human.stdout).unwrap();
    assert!(line.contains("contradiction raised=0 resolved=0"), "{line}");
    assert!(line.contains("ghosts raised=0 resolved=0"), "{line}");
}

/// ④ 缺库 ⇒ 运维错误（退出 3），且**不许**把库建出来。
/// 这一面是写库的作业，所以它不会走只读装配；但"库不在"仍然是一次读不出来，
/// 不许被念成"扫过了、库是干净的"（那样每天保底档都会在空目录上写出一份全 0 的读数）。
#[test]
fn a_missing_store_is_an_operational_error_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let out = run(&data, &["anomaly", "sweep", "--json"]);
    assert_eq!(code(&out), 3, "缺库必须是运维失败：{}", stderr_text(&out));
    assert!(!stderr_text(&out).contains("invalid invocation"),
        "这不是用法错误：{}", stderr_text(&out));
    assert!(!data.join("memory.db").exists(), "失败的扫描不许把库建出来");
}
