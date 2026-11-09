//! G1 刀 A：现网库对照测量（派发档"对照测量"段）。**环境变量 gated**——
//! 不设 `RECOLLECT_G1A_MEASURE_DB` 时这条用例打印一行跳过即 pass（CI/默认全量跑不碰现网）。
//!
//! 复现：
//! 1. 只读备份现网库到临时目录（SQLite backup API，绝不写现网）：
//!    ```python
//!    import sqlite3, tempfile, os
//!    src = sqlite3.connect("file:D:/example/.local/node-data/memory.db?mode=ro", uri=True)
//!    d = tempfile.mkdtemp(prefix="g1a_measure_"); dst = sqlite3.connect(os.path.join(d,"memory.db"))
//!    src.backup(dst); dst.close(); src.close(); print(d)
//!    ```
//! 2. `RECOLLECT_G1A_MEASURE_DB=<那个临时目录> cargo test -p rc-distill --test g1a_measure -- --nocapture`
//!
//! 安全 belt：路径里含 `node-data`（现网目录名）直接 panic——测量只许在临时副本上跑。
//! 副本是可写的（回填要盖章），现网库从头到尾只读。
//!
//! 尺子（独立于章，避免循环论证）：主循环一行是"施工流水" ⇔ 它是卡行且其镜像 fact 的
//! 归一化谓词 ∈ `CONSTRUCTION_PREDICATES`（与预闸同一份闭集常量）。占比 = 施工卡行 / 卡行
//! （锚行没有谓词，不算流水也不进卡行分母；另报含锚总行数供透明对照）。
//!
//! 三段读数：①预闸+回填前后 16 条审计探针的主循环施工占比；②靶向 decision/promise 卡
//! 逐卡零丢失；③阴性对照——回填盖了章的流水行必须真从主循环降下去。

use rc_agent::recall::{recall, RecallRequest};
use rc_memory::reader::RecallFilter;
use rc_memory::value_gate::CONSTRUCTION_PREDICATES;
use rc_store::AgentRoot;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// 审计档 `design/2026-11-07-recall-audit.md` §② 的 16 条探针（逐字）。
const QUERIES: [&str; 16] = [
    "示例主人是谁", "示例昵称是什么", "W31 是什么状态", "上线进展",
    "为什么砍掉调用配额", "batch8 为什么放弃", "今天早上发生了什么", "昨天的抽干结果",
    "用户拍板过什么", "谁负责备份", "还有什么没修", "下一步计划",
    "rc=2 修复和什么有关", "水位漂移怎么发现的", "最近发生了什么", "毛姆的小说",
];

fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// 主循环一行：id + 是否卡行 + 卡行谓词是否命中闭集。
struct RowRead {
    id: String,
    is_card: bool,
    construction: bool,
}

/// 跑一条探针，读 json 面主循环（rows 数组）。Err（no_rows 等）当 0 行。
fn probe(m: &rc_store::MemoryHandle, q: &str, now_ms: i64, pred: &HashMap<String, String>, closed: &HashSet<String>)
    -> (Vec<RowRead>, i64, i64, String)
{
    let req = RecallRequest::Query(q.to_string());
    let text = match recall(m, &req, &RecallFilter::empty(), now_ms) {
        Ok(tj) => tj.text().to_string(),
        Err(e) => return (Vec::new(), 0, 0, format!("ERR:{e}")),
    };
    let v: Value = serde_json::from_str(&text).unwrap();
    let archived = v["value_mix"]["archived"].as_i64().unwrap_or(0);
    let main = v["value_mix"]["main"].as_i64().unwrap_or(0);
    let mut rows = Vec::new();
    for r in v["rows"].as_array().unwrap() {
        let id = r["id"].as_str().unwrap().to_string();
        let is_card = id.starts_with("card:");
        let construction = if is_card {
            pred.get(&id[5..]).map(|p| closed.contains(&norm(p))).unwrap_or(false)
        } else {
            false
        };
        rows.push(RowRead { id, is_card, construction });
    }
    (rows, main, archived, String::new())
}

fn aggregate(rows: &[RowRead]) -> (usize, usize, usize) {
    let total = rows.len();
    let cards = rows.iter().filter(|r| r.is_card).count();
    let construction = rows.iter().filter(|r| r.construction).count();
    (total, cards, construction)
}

#[test]
fn g1a_现网对照测量() {
    let Ok(dir) = std::env::var("RECOLLECT_G1A_MEASURE_DB") else {
        eprintln!("G1A_MEASURE_SKIP: 未设 RECOLLECT_G1A_MEASURE_DB（临时副本目录），跳过现网对照测量。");
        return;
    };
    assert!(!dir.contains("node-data"),
        "测量只许在临时副本上跑，绝不指向现网目录（node-data）：{dir}");
    let root = AgentRoot::open(std::path::Path::new(&dir)).unwrap();
    let m = root.memory();

    // 复位到"功能上线前"形状：丢掉副本上可能已有的章（前一次测量跑留下的），
    // 保证 BEFORE 真的是无归档基线、且本用例可重复跑。只动临时副本，现网库从头到尾只读。
    m.conn().execute_batch("DROP TABLE IF EXISTS card_sink").unwrap();

    // 钟：取库里最新写入时刻，复刻"截至最近一次落卡"的召回（stale/cooldown 按它判）。
    let now_ms: i64 = m.conn().query_row(
        "SELECT COALESCE(max(created_at_ms), 0) FROM card", [], |r| r.get(0)).unwrap();
    rc_index::rebuild(m, now_ms).unwrap();

    let closed: HashSet<String> = CONSTRUCTION_PREDICATES.iter().map(|w| norm(w)).collect();
    // 卡 → 镜像 fact 谓词（一张卡可能指多条 fact，取第一条；施工判定只要谓词命中闭集）。
    let pred: HashMap<String, String> = {
        let mut st = m.conn().prepare(
            "SELECT c.id, f.predicate FROM card c
               JOIN json_each(c.pointers_json) p
               JOIN fact f ON f.id = substr(p.value, 6)
              WHERE p.value LIKE 'fact:%'").unwrap();
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap();
        let mut map = HashMap::new();
        for kv in rows { let (k, v) = kv.unwrap(); map.entry(k).or_insert(v); }
        map
    };

    println!("=== G1A MEASURE BEGIN ===");
    println!("db_dir: {dir}");
    println!("now_ms: {now_ms}");
    println!("closed_set_size: {}", closed.len());
    println!("card_predicate_map: {}", pred.len());

    // ── BEFORE ──────────────────────────────────────────────────────────────
    println!("--- BEFORE (per query) ---");
    let mut before_rows: Vec<Vec<RowRead>> = Vec::new();
    let (mut b_total, mut b_cards, mut b_constr) = (0usize, 0usize, 0usize);
    for (i, q) in QUERIES.iter().enumerate() {
        let (rows, main, archived, err) = probe(m, q, now_ms, &pred, &closed);
        let (total, cards, constr) = aggregate(&rows);
        b_total += total; b_cards += cards; b_constr += constr;
        let ratio = if cards > 0 { format!("{:.1}%", 100.0 * constr as f64 / cards as f64) } else { "n/a".into() };
        println!("[q{:02}] {q} | rows={total} cards={cards} construction={constr} ratio={ratio} | value_mix.main={main} archived={archived} {err}", i + 1);
        before_rows.push(rows);
    }
    let b_ratio_cards = if b_cards > 0 { 100.0 * b_constr as f64 / b_cards as f64 } else { 0.0 };
    let b_ratio_rows = if b_total > 0 { 100.0 * b_constr as f64 / b_total as f64 } else { 0.0 };
    println!("--- BEFORE aggregate ---");
    println!("total_rows={b_total} total_cards={b_cards} total_construction={b_constr} ratio_cards={b_ratio_cards:.1}% ratio_rows={b_ratio_rows:.1}%");

    // ── 靶向 BEFORE（逐卡可召回性，供零丢失对照）──────────────────────────────
    let targets: Vec<(String, String, String)> = {
        let mut st = m.conn().prepare(
            "SELECT id, kind, title FROM card WHERE kind IN ('decision','promise') AND mode='stated' ORDER BY kind, id").unwrap();
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        rows.map(|x| x.unwrap()).collect()
    };
    let recallable = |m: &rc_store::MemoryHandle, id: &str, title: &str| -> bool {
        let req = RecallRequest::Query(title.to_string());
        match recall(m, &req, &RecallFilter::empty(), now_ms) {
            Ok(tj) => {
                let v: Value = serde_json::from_str(&tj.text()).unwrap();
                v["rows"].as_array().unwrap().iter().any(|r| r["id"].as_str() == Some(&format!("card:{id}")))
            }
            Err(_) => false,
        }
    };
    let before_target: Vec<bool> = targets.iter().map(|(id, _k, t)| recallable(m, id, t)).collect();

    // ── BACKFILL ─────────────────────────────────────────────────────────────
    let report = rc_distill::value_backfill::backfill_program_archive(m, now_ms).unwrap();
    println!("--- BACKFILL ---");
    println!("scanned={} hits={} newly_marked={}", report.scanned, report.hits, report.newly_marked);
    assert!(report.newly_marked > 0, "现网应有施工流水可归档，否则测量空转");
    // 幂等：第二遍不新增章。
    let again = rc_distill::value_backfill::backfill_program_archive(m, now_ms).unwrap();
    println!("second_run newly_marked={} (幂等钉)", again.newly_marked);
    assert_eq!(again.newly_marked, 0, "回填幂等：第二遍零新增");

    // ── AFTER ────────────────────────────────────────────────────────────────
    println!("--- AFTER (per query) ---");
    let mut after_rows: Vec<Vec<RowRead>> = Vec::new();
    let (mut a_total, mut a_cards, mut a_constr) = (0usize, 0usize, 0usize);
    for (i, q) in QUERIES.iter().enumerate() {
        let (rows, main, archived, err) = probe(m, q, now_ms, &pred, &closed);
        let (total, cards, constr) = aggregate(&rows);
        a_total += total; a_cards += cards; a_constr += constr;
        let ratio = if cards > 0 { format!("{:.1}%", 100.0 * constr as f64 / cards as f64) } else { "n/a".into() };
        println!("[q{:02}] {q} | rows={total} cards={cards} construction={constr} ratio={ratio} | value_mix.main={main} archived={archived} {err}", i + 1);
        after_rows.push(rows);
    }
    let a_ratio_cards = if a_cards > 0 { 100.0 * a_constr as f64 / a_cards as f64 } else { 0.0 };
    let a_ratio_rows = if a_total > 0 { 100.0 * a_constr as f64 / a_total as f64 } else { 0.0 };
    println!("--- AFTER aggregate ---");
    println!("total_rows={a_total} total_cards={a_cards} total_construction={a_constr} ratio_cards={a_ratio_cards:.1}% ratio_rows={a_ratio_rows:.1}%");
    println!("--- RATIO VERDICT ---");
    println!("before_ratio_cards={b_ratio_cards:.1}% after_ratio_cards={a_ratio_cards:.1}% target(<20%)_met={}", a_ratio_cards < 20.0);
    assert!(a_ratio_cards < b_ratio_cards, "回填后施工占比必须真降：{b_ratio_cards:.1}% -> {a_ratio_cards:.1}%");

    // ── 靶向零丢失 ───────────────────────────────────────────────────────────
    println!("--- TARGET (decision/promise zero-loss) ---");
    let sink_target: i64 = m.conn().query_row(
        "SELECT count(*) FROM card_sink cs JOIN card c ON c.id = cs.card_id
          WHERE c.kind IN ('decision','promise','plan')", [], |r| r.get(0)).unwrap();
    println!("card_sink_target_count={sink_target} (豁免类别不许被盖章，必须 0)");
    assert_eq!(sink_target, 0, "decision/promise/plan 卡零归档");
    let mut lost = 0usize;
    let mut rec_before = 0usize;
    let mut rec_after = 0usize;
    for (idx, (id, kind, title)) in targets.iter().enumerate() {
        let after = recallable(m, id, title);
        let before = before_target[idx];
        if before { rec_before += 1; }
        if after { rec_after += 1; }
        if before && !after { lost += 1; }
        println!("card:{id} {kind} before={before} after={after} title={title}");
        assert_eq!(before, after, "靶向卡可召回性不许因回填改变（零丢失）：card:{id}");
    }
    println!("target_total={} recallable_before={rec_before} recallable_after={rec_after} lost={lost}", targets.len());
    assert_eq!(lost, 0, "靶向 decision/promise 卡零丢失");

    // ── 阴性对照：盖了章的流水行必须真降 ──────────────────────────────────────
    println!("--- NEGATIVE CONTROL ---");
    let archived_ids: HashSet<String> = rc_memory::value_gate::archived_card_ids(m).unwrap();
    println!("archived_total={}", archived_ids.len());
    // BEFORE 主循环里出现过的施工卡行（唯一 id）。
    let before_constr_ids: HashSet<String> = before_rows.iter().flatten()
        .filter(|r| r.construction).map(|r| r.id[5..].to_string()).collect();
    // 其中被盖章的（阴性集）：AFTER 同探针主循环里必须一个都不剩。
    let negative_set: HashSet<String> = before_constr_ids.intersection(&archived_ids).cloned().collect();
    let after_all_ids: HashSet<String> = after_rows.iter().flatten()
        .filter(|r| r.is_card).map(|r| r.id[5..].to_string()).collect();
    let survived: Vec<&String> = negative_set.iter().filter(|id| after_all_ids.contains(*id)).collect();
    println!("construction_in_before_main(unique)={}", before_constr_ids.len());
    println!("negative_set(archived∩before)={} survived_after={} (必须 0)", negative_set.len(), survived.len());
    assert!(survived.is_empty(), "阴性对照失败：盖章流水行仍在主循环：{survived:?}");
    // 选施工命中最多的一条探针做原始读数示例。
    let ex = (0..QUERIES.len()).max_by_key(|&i| before_rows[i].iter().filter(|r| r.construction).count()).unwrap();
    let b_ids: Vec<&str> = before_rows[ex].iter().map(|r| r.id.as_str()).collect();
    let a_ids: Vec<&str> = after_rows[ex].iter().map(|r| r.id.as_str()).collect();
    let b_c: Vec<&str> = before_rows[ex].iter().filter(|r| r.construction).map(|r| r.id.as_str()).collect();
    println!("example_query=[q{:02}] {}", ex + 1, QUERIES[ex]);
    println!("example_before_construction_ids={b_c:?}");
    println!("example_before_row_ids={b_ids:?}");
    println!("example_after_row_ids={a_ids:?}");
    println!("=== G1A MEASURE END ===");
}