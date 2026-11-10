//! 波三刀 B：lesson 卡的**召回**与**复用反馈**收尾检查（**环境变量 gated**）。
//!
//! 不设 `RECOLLECT_G1C_LESSON_DB` 时这条用例打印一行跳过即 pass（CI/默认全量跑不碰现网）。
//!
//! 复现（正式路径那一发在 CLI 上跑，本用例只读回执）：
//! 1. 只读备份现网库到临时目录（SQLite backup API，绝不写现网）：
//!    ```python
//!    import sqlite3, os
//!    d = r"D:/example/.local/lesson-sample/base"; os.makedirs(d, exist_ok=True)
//!    for name in ("memory.db", "transcript.db", "ledger.db"):
//!        src = sqlite3.connect(f"file:D:/example/.local/node-data/{name}?mode=ro", uri=True)
//!        dst = sqlite3.connect(os.path.join(d, name)); src.backup(dst); dst.close(); src.close()
//!    ```
//! 2. 在副本上真跑 lesson（生产路径，含抽取 + 独立审核 + 落卡）：
//!    `recollect-hub.exe distill once --pass lesson --batch-turns 10 --data-dir <副本>
//!     --llm-config <副本>/llm.toml --llm-timeout-ms 300000`
//! 3. `RECOLLECT_G1C_LESSON_DB=<副本> cargo test -p rc-distill --test g1c_lesson_sample -- --nocapture`
//!
//! 安全 belt：路径里含 `node-data`（现网目录名）直接 panic——检查只许在临时副本上跑。
//!
//! 本用例会往**副本**里写两样东西（都不碰现网）：索引重建（`rc_index::rebuild`，召回的前提）
//! 与一条 `card_reuse_event`（复用反馈走的就是生产同一个写口 `record_reuse`）。
//! 它回答的是判据里那两问：lesson 卡**落库后能不能被 recall 读到**、**能不能被
//! `recollect_reuse` 反馈**（MCP 工具的写口就是这里的 `record_reuse`）。

use rc_agent::recall::{recall, RecallRequest};
use rc_memory::keys::MemKey;
use rc_memory::reader::RecallFilter;
use rc_memory::reuse::{record_reuse, reuse_counts};
use rc_store::AgentRoot;
use serde_json::Value;
use std::path::Path;

#[test]
fn g1c_lesson_card_recall_and_reuse() {
    let Ok(dir) = std::env::var("RECOLLECT_G1C_LESSON_DB") else {
        eprintln!(
            "G1C_LIVE_SKIP: 未设 RECOLLECT_G1C_LESSON_DB（临时副本目录），跳过 lesson 卡召回/复用检查。"
        );
        return;
    };
    assert!(
        !dir.contains("node-data"),
        "检查只许在临时副本上跑，绝不指向现网目录（node-data）：{dir}"
    );
    let root = AgentRoot::open(Path::new(&dir)).unwrap();
    let m = root.memory();

    // 钟取库里最新落卡时刻：冻结"截至最近一次落卡"的召回（与 g1a/g1b 同口径）。
    let now_ms: i64 = m
        .conn()
        .query_row(
            "SELECT COALESCE(max(created_at_ms), 0) FROM card",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // 索引重建是召回的前提：副本从现网 backup 出来，FTS 表里没有本轮新落的 lesson 卡。
    rc_index::rebuild(m, now_ms).unwrap();

    let cards: Vec<(String, String)> = {
        let mut st = m
            .conn()
            .prepare("SELECT id, title FROM card WHERE kind='lesson' ORDER BY created_at_ms, id")
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.filter_map(|x| x.ok()).collect()
    };
    println!("G1C lesson_cards={}", cards.len());
    assert!(!cards.is_empty(), "副本里没有 kind='lesson' 的卡：先在副本上跑一发 lesson");

    for (id, title) in &cards {
        // ① 句柄召回：`recollect_recall(handles=["card:<id>"], view="card")` 的底层同一条路。
        let key = format!("card:{id}");
        let Some(mem_key) = MemKey::parse(&key) else {
            println!("G1C handle_parse_fail key={key}");
            continue;
        };
        let req = RecallRequest::Handles(vec![mem_key]);
        match recall(m, &req, &RecallFilter::empty(), now_ms) {
            Ok(tj) => {
                let text = tj.text();
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let rows = v["rows"].as_array().cloned().unwrap_or_default();
                let hit = rows.iter().any(|row| row["id"].as_str() == Some(key.as_str()));
                println!(
                    "G1C handle_recall key={key} rows={} hit={} bytes={}",
                    rows.len(),
                    hit,
                    text.len()
                );
            }
            Err(e) => println!("G1C handle_recall key={key} error={e}"),
        }

        // ② 查询召回：用卡标题当查询词，看它是否进主循环（`value_mix` 一并读，避免"归档了
        //    却读成没命中"那一族）。
        let req = RecallRequest::Query(title.clone());
        match recall(m, &req, &RecallFilter::empty(), now_ms) {
            Ok(tj) => {
                let text = tj.text();
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let rows = v["rows"].as_array().cloned().unwrap_or_default();
                let hit = rows.iter().any(|row| row["id"].as_str() == Some(key.as_str()));
                let mix = v["value_mix"].clone();
                println!(
                    "G1C query_recall title={:?} rows={} hit={} value_mix={}",
                    title,
                    rows.len(),
                    hit,
                    mix
                );
            }
            Err(e) => println!("G1C query_recall title={:?} error={e}", title),
        }

        // ③ 复用反馈：`recollect_reuse` 的写口（只把卡是否存在当资格，不按 kind 过滤）。
        match record_reuse(m, id, "worked", Some("g1c lesson sample"), now_ms, "g1c-sample") {
            Ok(event_id) => {
                let counts = reuse_counts(m, id).unwrap();
                println!(
                    "G1C reuse_write card={id} event_id={event_id} worked={} failed={} partial={} mismatch={} stale={}",
                    counts.worked, counts.failed, counts.partial, counts.mismatch, counts.stale
                );
            }
            Err(e) => println!("G1C reuse_write card={id} error={e}"),
        }
    }
}
