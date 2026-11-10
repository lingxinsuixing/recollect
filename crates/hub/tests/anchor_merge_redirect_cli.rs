//! `hub anchor merge --from --to`（D8 方案二指向合并）的 CLI 面：默认预演一格不写、
//! `--apply` 事务落库、JSON 形状、拒绝码、断链读数退 3。A/B/C 发现合并的既有面
//! （不带 `--from`）不归本文件，见 `anchor_merge_cli.rs`。
use rc_store::Root;
use rusqlite::Connection;
use serde_json::Value;
use std::process::Output;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    std::process::Command::new(bin()).args(args).output().expect("hub binary")
}

fn text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn err(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

/// `insert_anchor` 铺下的四格 canonical：INSERT 与句柄派生共用这四串。
const TO_CANON: &str = "幸存组织";
const FROM_CANON: &str = "旧名组织";
const OTHER_TO_CANON: &str = "另一幸存";
const OTHER_FROM_CANON: &str = "另一旧名";

/// 生产算法：`kind='org'`、scope 是 `insert_anchor` 写死的 `project/proj-cli`。
fn anchor_id(canonical: &str) -> String {
    rc_memory::compute_mem_id(
        rc_memory::keys::MemTable::Anchor, "org", canonical, "",
        &rc_memory::Scope { kind: rc_memory::ScopeKind::Project, key: "proj-cli".into() },
    )
}

fn insert_anchor(conn: &Connection, id: &str, canonical: &str, status: &str) {
    conn.execute(
        "INSERT INTO anchor(id, kind, canonical, scope_kind, scope_key, status,
                            created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, 'org', ?2, 'project', 'proj-cli', ?3, 1700000000000, 1700000000000, 1)",
        rusqlite::params![id, canonical, status],
    )
    .unwrap();
}

/// 返回 (from, to)。
fn seed(dir: &std::path::Path) -> (String, String) {
    let root = Root::open(dir).unwrap();
    let conn = root.memory().conn();
    let (from, to) = (anchor_id(FROM_CANON), anchor_id(TO_CANON));
    insert_anchor(conn, &to, TO_CANON, "active");
    insert_anchor(conn, &from, FROM_CANON, "provisional");
    (from, to)
}

fn scalar(dir: &std::path::Path, sql: &str) -> i64 {
    let root = Root::open(dir).unwrap();
    root.memory().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn status(dir: &std::path::Path, id: &str) -> String {
    let root = Root::open(dir).unwrap();
    root.memory()
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

/// 1：`--help` 出新旗标；默认预演一格不写；JSON 形状带 A/B/C 分类与断链读数。
#[test]
fn redirect_plan_is_read_only_and_reports_class_and_broken() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = seed(dir.path());
    let dd = dir.path().to_str().unwrap();

    let help = run(&["anchor", "merge", "--help"]);
    assert!(help.status.success());
    for flag in ["--from", "--to", "--apply", "--dry-run"] {
        assert!(text(&help).contains(flag), "缺 {flag}");
    }

    let out = run(&["anchor", "merge", "--from", &from, "--to", &to,
                    "--json", "--data-dir", dd]);
    assert!(out.status.success(), "{}", err(&out));
    let plan: Value = serde_json::from_str(text(&out).trim()).expect("预演是 JSON");
    assert_eq!(plan["op"], "anchor_merge_redirect");
    assert_eq!(plan["mode"], "plan");
    assert_eq!(plan["from_id"], from.as_str());
    assert_eq!(plan["to_id"], to.as_str());
    assert_eq!(plan["class"], "C", "名字不相连的一对按 C 报（人工裁决经 --apply 落地）：{plan}");
    assert_eq!(plan["class_reason"], "no_name_link");
    assert_eq!(plan["broken_redirects"], 0);

    // 一格没写：审计 0、状态不变、alias 不长。
    assert_eq!(scalar(dir.path(), "SELECT count(*) FROM anchor_merge"), 0);
    assert_eq!(status(dir.path(), &from), "provisional");
    assert_eq!(scalar(dir.path(), "SELECT count(*) FROM alias"), 0);
}

/// 2：`--apply` 事务落库——指向行、退场章、alias 来源三格、台账；幂等（二次 apply 拒）。
#[test]
fn redirect_apply_writes_pointer_status_alias_and_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = seed(dir.path());
    let dd = dir.path().to_str().unwrap();

    let out = run(&["anchor", "merge", "--from", &from, "--to", &to,
                    "--reason", "同一组织", "--apply", "--json", "--data-dir", dd]);
    assert!(out.status.success(), "{}", err(&out));
    let applied: Value = serde_json::from_str(text(&out).trim()).expect("apply 是 JSON");
    assert_eq!(applied["op"], "anchor_merge_redirect");
    assert_eq!(applied["mode"], "apply");
    assert_eq!(applied["aliases_merged"], 1);
    assert!(applied["broken_redirects"].as_array().unwrap().is_empty());

    assert_eq!(scalar(dir.path(), "SELECT count(*) FROM anchor_merge"), 1);
    assert_eq!(status(dir.path(), &from), "merged");
    assert_eq!(status(dir.path(), &to), "active");
    let alias_row: (Option<i64>, Option<i64>, Option<String>) = Root::open(dir.path())
        .unwrap().memory().conn().query_row(
            "SELECT valid_from_ms, valid_to_ms, source_turn_uid FROM alias
              WHERE anchor_id=?1 AND alias='旧名组织'",
            [&to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(alias_row, (None, None, None));
    assert_eq!(scalar(dir.path(),
        "SELECT count(*) FROM memory_op WHERE op='MERGE' AND mem_table='anchor'"), 1);

    // 二次 apply：from 已退场，显式拒绝、一行不多（拒绝码在 stderr，operational 下行）。
    let again = run(&["anchor", "merge", "--from", &from, "--to", &to,
                      "--apply", "--data-dir", dd]);
    assert_eq!(code(&again), 3, "stderr={}", err(&again));
    assert!(err(&again).contains("from_not_alive"), "{}", err(&again));
    assert_eq!(scalar(dir.path(), "SELECT count(*) FROM anchor_merge"), 1);
}

/// 3：拒绝面闭集逐词出声（same_anchor / missing_anchor）。
#[test]
fn redirect_refusals_print_closed_set_codes() {
    let dir = tempfile::tempdir().unwrap();
    let (from, to) = seed(dir.path());
    let dd = dir.path().to_str().unwrap();

    let self_merge = run(&["anchor", "merge", "--from", &from, "--to", &from,
                           "--apply", "--data-dir", dd]);
    assert_eq!(code(&self_merge), 3);
    assert!(err(&self_merge).contains("same_anchor"), "stderr={}", err(&self_merge));

    let missing = run(&["anchor", "merge", "--from", "nonexistent", "--to", &to,
                        "--apply", "--data-dir", dd]);
    assert_eq!(code(&missing), 3);
    assert!(err(&missing).contains("missing_anchor"), "stderr={}", err(&missing));

    // 预演同样报缺锚（class=missing）。
    let plan = run(&["anchor", "merge", "--from", "nonexistent", "--to", &to,
                     "--json", "--data-dir", dd]);
    assert!(plan.status.success(), "{}", err(&plan));
    let p: Value = serde_json::from_str(text(&plan).trim()).unwrap();
    assert_eq!(p["class"], "missing");
}

/// 4：断链读数可见——手塞一条指向不存在 to 的行（模拟坏数据），apply 后带断链清单、退 3。
#[test]
fn broken_redirects_are_visible_and_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (from, _to) = seed(dir.path());
    let dd = dir.path().to_str().unwrap();
    let ghost = format!("{:064x}", 9);
    {
        let root_tmp = Root::open(dir.path()).unwrap();
        let conn = root_tmp.memory().conn();
        conn.execute(
            "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
             VALUES(?1, ?2, 'bad', 'bad-data', 1700000000000)",
            rusqlite::params![&from, &ghost]).unwrap();
        // 断链的读面形状：**退场锚**的链走不到活锚（活锚带着死指针不会被读侧解析，
        // resolve 对活着直接返回）。把 from 盖章成 merged，模拟"合并后幸存锚被物理删"的坏数据。
        conn.execute("UPDATE anchor SET status='merged' WHERE id=?1", [&from]).unwrap();
    }
    let (from2, to2) = (anchor_id(OTHER_FROM_CANON), anchor_id(OTHER_TO_CANON));
    let root = Root::open(dir.path()).unwrap();
    insert_anchor(root.memory().conn(), &to2, OTHER_TO_CANON, "active");
    insert_anchor(root.memory().conn(), &from2, OTHER_FROM_CANON, "provisional");
    drop(root);

    let out = run(&["anchor", "merge", "--from", &from2, "--to", &to2,
                    "--apply", "--json", "--data-dir", dd]);
    // apply 本身成功（from2→to2 落库），但**断链读数非空 ⇒ 退 3**（读数可见，不静默）。
    assert_eq!(code(&out), 3, "stderr={}", err(&out));
    let applied: Value = serde_json::from_str(text(&out).trim()).unwrap();
    assert_eq!(applied["broken_redirects"].as_array().unwrap().len(), 1);
    assert_eq!(applied["broken_redirects"][0]["from_id"], from.as_str());
    assert_eq!(applied["broken_redirects"][0]["reason"], "missing_to");
    // 幸存对确实落库了。
    assert_eq!(scalar(dir.path(), "SELECT count(*) FROM anchor_merge"), 2);
}

/// 交付判据的 CLI 级证据：真 hub 二进制落指向合并，**真实消费者**（rc-memory 的读者，
/// 与 recall entity 过滤/名字渲染同一条路径）读数——
/// ① 合并后旧锚的行级 mention 按**幸存锚名字**过滤仍命中，且旧锚显示名渲成幸存锚；
/// ② 回滚：操作前 `hub backup` 留档 → 删 `anchor_merge` 行 ⇒ mention 命中消失
///    （回到"按 status 语义"），恢复备份文件后读数复原。
#[test]
fn consumer_reads_survive_the_merge_and_rollback_with_a_backup() {
    use rc_memory::identity::compute_mem_id;
    use rc_memory::keys::{MemTable, Scope, ScopeKind};

    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-cli".into() };
    let card_id = compute_mem_id(MemTable::Card, "note", "旧名组织提过的卡", "正文", &scope);
    let (from, to) = {
        let root = Root::open(dir.path()).unwrap();
        let conn = root.memory().conn();
        let to = anchor_id(TO_CANON);
        let from = anchor_id(FROM_CANON);
        insert_anchor(conn, &to, TO_CANON, "active");
        insert_anchor(conn, &from, FROM_CANON, "provisional");
        conn.execute(
            "INSERT INTO card(id, kind, title, text, as_of_ms, verdict, source_agent, device_id,
                              scope_kind, scope_key, created_at_ms, norm_version)
             VALUES(?1,'note','旧名组织提过的卡','正文',1700000000000,'unreviewed','agent','dev',
                    'project','proj-cli',1700000000000,1)",
            [&card_id]).unwrap();
        conn.execute(
            "INSERT INTO memory_mention(mem_table, mem_id, anchor_id, source_agent, device_id,
                                        created_at_ms, norm_version)
             VALUES('card', ?1, ?2, 'agent', 'dev', 1700000000000, 1)",
            rusqlite::params![&card_id, &from]).unwrap();
        (from, to)
    };

    // 合并前：按幸存锚名字过滤命中不了旧锚的 mention。
    let key = rc_memory::MemKey { table: MemTable::Card, id: card_id.clone() };
    assert!(!rc_memory::reader::row_mentions_any(
        Root::open(dir.path()).unwrap().memory(), &key, std::slice::from_ref(&to)).unwrap());

    // 真二进制落合并。
    let out = run(&["anchor", "merge", "--from", &from, "--to", &to,
                    "--reason", "同一组织", "--apply", "--data-dir", dd]);
    assert!(out.status.success(), "stderr={}", err(&out));

    // ① 旧锚的 mention 仍可见：按幸存锚名字的候选集命中；显示名渲成幸存锚。
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let ids = rc_memory::reader::entity_ids_for_name(m, "幸存组织").unwrap();
        assert!(ids.contains(&to), "{ids:?}");
        assert!(rc_memory::reader::row_mentions_any(m, &key, &ids).unwrap(),
                "合并后旧锚的 mention 仍可见（归属 to 锚）");
        // 渲染归属：旧锚 id 的名字面按幸存锚渲染（canonical 层；显示名层的
        // "最新有效别名优先"是既有规则，并入的旧正名成为幸存锚的别名）。
        let names = rc_memory::reader::anchor_names(m, &from).unwrap().unwrap();
        assert_eq!(names.canonical, "幸存组织");
        assert!(names.aliases.iter().any(|a| a == "旧名组织"), "{:?}", names.aliases);
    }

    // ② 回滚：操作前 hub backup 留档 → 删指向行 ⇒ 读数回到合并前。
    let backup = run(&["backup", "--data-dir", dd]);
    assert!(backup.status.success(), "stderr={}", err(&backup));
    assert!(scalar(dir.path(), "SELECT count(*) FROM anchor_merge") >= 1);
    {
        let root = Root::open(dir.path()).unwrap();
        root.memory().conn()
            .execute("DELETE FROM anchor_merge WHERE from_id=?1", [&from]).unwrap();
        let m = root.memory();
        let ids = rc_memory::reader::entity_ids_for_name(m, "幸存组织").unwrap();
        assert!(!rc_memory::reader::row_mentions_any(m, &key, &ids).unwrap(),
                "删指向行后旧锚 mention 回到不可见（按 status 语义）");
    }
}
