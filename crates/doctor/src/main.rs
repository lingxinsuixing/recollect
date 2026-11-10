use clap::Parser;
use rc_store::Root;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "recollect-doctor",
    version,
    about = "capability readings — zero body, zero path, zero secret"
)]
struct Args {
    /// 打印能力读数（阶段 0 只有这一种读数）
    #[arg(long)]
    capabilities: bool,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器主语：轮转态那一节按它取行（`llm_state` 一台机器一行）。
    /// 不给就走 `RECOLLECT_DEVICE_ID` 兜底，两者都没有 ⇒ 那一节是 `none`（不是失败，见下面那段注释）。
    #[arg(long)]
    device_id: Option<String>,
    /// "距上次成功备份 N 小时"里的"现在"。**缺省取系统时钟**（入口取钟是允许的）；
    /// 传了就用它，便于测试与复现。
    #[arg(long)]
    now_ms: Option<i64>,
}

// 退出码：0 有读数 / 3 没有读数（用法错误、没给 --capabilities、库打不开）。
// 3 而不是 2：2 在 hub 那边专指"整批拒收"，诊断面不该借用。
fn main() -> ExitCode {
    let a = match Args::try_parse() {
        Ok(a) => a,
        // help/version 也被 clap 当成 Err，`use_stderr()` 是这两类与真用法错误的分界；
        // 用法文本自己拼：clap 的原串会回显操作员敲进来的那个 token，而它常常是个路径。
        Err(e) if e.use_stderr() => {
            return operational("invalid invocation — usage: recollect-doctor --capabilities [--json] [--data-dir DIR] [--device-id ID] [--now-ms MS]");
        }
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(0);
        }
    };
    if !a.capabilities {
        // "无读数可比成功更像失败"：仍要说清这里有哪些旗。
        return operational("no reading requested — flags: --capabilities [--json] [--data-dir DIR] [--device-id ID] [--now-ms MS]");
    }
    // `--now-ms` 在这里先取：下面的 `a.device_id` 被 `or_else` 移走之后，整份 `a` 就借不出来了。
    let now = now_ms(&a);
    // ---- 机器主语（§4.8 ④ 的那只手）----
    // 形状**逐字照 `crates/mcp/src/main.rs` 的既有先例**，一字不新造：`--device-id` 旗标 +
    // `RECOLLECT_DEVICE_ID` 兜底，过形用 `rc_contract::safe_token(&d) == d` 的**相等比较**
    // 而不是采信 `safe_token` 的返回值（那边那句注释的理由在这里原样有效：它的拒绝值自己不是合法 token）。
    // 这是全仓第三个 `RECOLLECT_DEVICE_ID` 读取点，命名空间仍只有一个：判据 35 那条 `zero`
    // 扫的是"另一个机器号命名空间"那一族字面（它扫全仓 `*.rs`，**连注释都算命中**，所以这里不能把
    // 那个名字写出来），而本节给操作员看的 JSON 键叫 `llm_state`（表名同源），也不是那一族。
    let machine = match a.device_id.or_else(|| std::env::var("RECOLLECT_DEVICE_ID").ok()) {
        Some(d) if rc_contract::safe_token(&d) == d => Some(d),
        // **用法错误**那一族，不是"缺身份"那一族（两种后果，处置不同）。
        Some(_) => return operational("device id is not a safe token"),
        None => None,
    };
    // P1-5：诊断路径**只读**。这里曾经用 `Root::open`，而它会跑迁移——审计实测跑一次
    // doctor 就在现网新建了空 `card_verdict_event`、把 WAL 从 218,392 B 撑到 255,472 B。
    // 读一眼不该改库：只读装配不建目录、不建表、不迁移；库缺失或 schema 不是当前版本 ⇒
    // 照旧走 `operational`（退出码 3），doctor 不修库。
    let root = match Root::open_readonly(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // E2（A31）的迁移提示**不在这里了**：本入口不再开可写句柄，迁移只发生在 hub/mcp 的
    // `Root::open` 上。"谁先撞上谁负责说"那条纪律仍然成立，只是 doctor 不再是"谁"；
    // `Root::open_readonly` 的 `migration_report()` 因此恒为默认值，没有可打印的回拨。
    let cap = match rc_store::capability::probe(root.transcript(), root.ledger(), root.memory()) {
        Ok(c) => c,
        Err(_) => return operational("capability probe failed"),
    };
    let rows = match rc_ledger::counters::read_all(root.ledger()) {
        Ok(r) => r,
        Err(_) => return operational("cannot read usage counters"),
    };

    let mut obj = match serde_json::to_value(&cap) {
        Ok(Value::Object(m)) => m,
        _ => return operational("cannot render reading"),
    };
    let mut counters = Map::new();
    // `attention` 的读数是下面那一节告警的第三个输入，所以它与 `quota_alerts` / `halt_alerts`
    // 住在同一段里（它自己读的是记忆库，不依赖 `rows`，但"告警要在 `rows` 被移走之前算完"这条
    // 顺序对它同样成立——分两处写就会有人以为它可以挪到 `for` 之后）。
    // 读不到就走 `operational`（退出码 3）：本文件里每一条"读不到"都是这个形状，不静默折成 0——
    // `0` 是"库自洽"这个**读数**，拿它当读失败的兜底就是把最坏的失败渲染成好消息。
    let attention_open = match rc_memory::anomaly::open_count(root.memory()) {
        Ok(n) => n,
        Err(_) => return operational("cannot read attention rows"),
    };
    // 告警要在 `rows` 被下面那个 `for` 移动之前算出来：同一份读数的两个用法，不是两次查询。
    let mut alerts = quota_alerts(&rows);
    alerts.extend(halt_alerts(root.ledger()));
    alerts.extend(attention_alerts(attention_open));
    // S5：推导休眠（从没成功推进过、池里还有活行）与配额告警同一段算完——两者都只读
    // 账与库、都不占用 `rows`，但"告警要在 `alerts` 被移走之前算完"这条纪律对它同样成立。
    alerts.extend(derive_dormant_alerts(root.ledger(), root.memory()));
    // W32-A2：review 轴的两本账对账。`review_watermark` 表（水位视角，`hub pending` 的
    // `review.pending` 读的就是它）与 `review_item` 实表（实表视角，`hub review stats` 的
    // `pending_review`）必须恒等——enqueue 补写水位（`rows.rs` 的 A2 写点）之前，入队不
    // 动水位，两本账会漂移，读数失真且只能靠脆弱的补偿链救。两格都取当前全量值：
    // 水位是各 pass `pending_review` 的跨 pass 求和（与 `hub pending` 同一把尺子，
    // `review_mark::all`），实表是 `rc_memory::review::stats` 的六态计数那一格。
    // 不等 ⇒ 恰一条告警（kind=review_watermark_mismatch，两格原值都在载荷里）；对得上
    // 也印 `review_reconcile` 块（`agree: true` 是读数），与上面 `reconcile` 那一条同形。
    // 两处读失败都走 `operational(3)`：不把"读不到"折成 0——0 是"两本账都空"这个读数，
    // 拿它当读失败的兜底就是把最坏的失败渲染成好消息。
    let watermark_pending = match rc_ledger::review_mark::all(root.ledger()) {
        Ok(marks) => marks.iter().map(|m| m.pending_review).sum::<i64>(),
        Err(_) => return operational("cannot read review watermark"),
    };
    let table_pending = match rc_memory::review::stats(root.memory(), now) {
        Ok(s) => s.pending_review,
        Err(_) => return operational("cannot read review queue stats"),
    };
    if watermark_pending != table_pending {
        alerts.push(serde_json::json!({
            "kind": "review_watermark_mismatch",
            "watermark_pending": watermark_pending,
            "table_pending": table_pending,
        }));
    }
    obj.insert(
        "review_reconcile".into(),
        serde_json::json!({
            "watermark_pending": watermark_pending,
            "table_pending": table_pending,
            "agree": watermark_pending == table_pending,
        }),
    );
    // ---- 备份读数与对账（阶段 4 T5）----
    // 两者都吃 `rows`，所以都在 `for r in rows` 之前算。`reconcile` 的告警也要在这一刻 push：
    // 下面 `obj.insert("alerts", Value::Array(alerts))` 会把 `alerts` 移走。
    let backup = backup_reading(&rows, now);
    let reconcile = rc_diag::doctor::reconcile(root.memory());
    if let Some(rec) = &reconcile {
        if !rec.agrees() {
            alerts.push(serde_json::json!({
                "kind": "index_content_mismatch",
                "delta": rec.delta()
            }));
        }
    }
    // 轮号同理：`rounds_reading` 只是又读一遍同一份 `rows`，所以它也必须在移动之前算。
    let (rounds, last_full) = rounds_reading(&rows);
    // B5b：密钥归档那一块也吃 `rows`，同一条"在 `for` 之前算"的纪律。
    let key_archive = key_archive_reading(&rows, &a.data_dir);
    // A45：率是显示层除法，不落库、不进任何表的列。
    let extract_gate = extract_gate(&rows);
    for r in rows {
        counters.insert(
            format!("{}:{}:{}", r.subject_kind, r.subject_id, r.metric),
            serde_json::json!(r.count),
        );
    }
    obj.insert("counters".into(), Value::Object(counters));
    if let Some(g) = extract_gate {
        obj.insert("extract_gate".into(), g);
    }
    // S3b/R4 + W32-A5：按 pass 的终态分桶（只读显示）。旧口径是
    // `count_terminal_by_pass` 的一个总数——`TERMINAL_STATES` 含 `quarantined`，1949 条隔离
    // 格被算成"已覆盖"，卡死读成健康。现在拆 `done/empty/rejected/quarantined/stale` 五桶
    // （`count_by_state` 的行数，按桶各数各的），另给一格派生读数 `covered` =
    // done+empty+rejected+stale：**隔离格单独成桶、不计入覆盖**。`TERMINAL_STATES` 本身
    // 一个字不动（`status.distill_lag_turns` 等读者依赖它），改的只是 doctor 这一面的读法。
    // 口径住在 `rc_ledger::pass_state::count_by_state` 一处（它数格/行，不是去重轮：
    // fact 同一轮的三族各一行——分桶要回答"卡在哪一支"，格比轮说得清）。没有五桶任何
    // 一行的 pass 不出现（`running`/`pending` 不是覆盖，与旧口径同一条界）。空表 ⇒ `{}`
    // （"还没有任何 pass 跑完过"是读数），读失败 ⇒ `operational(3)`：与本节其它读数
    // 同一条纪律，不把"读不到"折成"没有"。
    let mut pass_coverage = Map::new();
    for pass_id in rc_ledger::pass_state::PASS_IDS {
        let states = match rc_ledger::pass_state::count_by_state(root.ledger(), pass_id) {
            Ok(rows) => rows,
            Err(_) => return operational("cannot read pass coverage"),
        };
        let count = |want: rc_ledger::pass_state::PassState| {
            states
                .iter()
                .find(|(state, _)| *state == want)
                .map(|(_, n)| *n)
                .unwrap_or(0)
        };
        let done = count(rc_ledger::pass_state::PassState::Done);
        let empty = count(rc_ledger::pass_state::PassState::Empty);
        let rejected = count(rc_ledger::pass_state::PassState::Rejected);
        let quarantined = count(rc_ledger::pass_state::PassState::Quarantined);
        let stale = count(rc_ledger::pass_state::PassState::Stale);
        let covered = done + empty + rejected + stale;
        if covered + quarantined > 0 {
            pass_coverage.insert(
                pass_id.to_string(),
                serde_json::json!({
                    "done": done,
                    "empty": empty,
                    "rejected": rejected,
                    "quarantined": quarantined,
                    "stale": stale,
                    "covered": covered,
                }),
            );
        }
    }
    obj.insert("pass_coverage".into(), Value::Object(pass_coverage));
    // P0-1 的两格（只给数，不给正文）：`claims` 是**有效章**覆盖的会话数（口径在
    // `rc_ledger::transcript::principal_claim_sessions`：现行章且身份已知），
    // `facts_with_agent_speaker_but_human_evidence` 是"用户说的被记成 agent 说的"的行数
    // （口径在 `rc_memory::reader` 那一个实现点）。两个读失败都走 `operational(3)`：
    // `0` 是"身份通道干净"这个**读数**，拿它当读失败的兜底就是把最坏的失败渲染成好消息。
    let claim_sessions = match rc_ledger::transcript::principal_claim_sessions(root.transcript()) {
        Ok(n) => n,
        Err(_) => return operational("cannot read principal claims"),
    };
    let agent_speaker = match rc_memory::reader::facts_with_agent_speaker_but_human_evidence(
        root.memory(),
    ) {
        Ok(n) => n,
        Err(_) => return operational("cannot read fact speakers"),
    };
    // W20（P1-3）：闸 6c 在**哪些会话**处于 fail-open。主语是"有 human 轮、但
    // `resolve_principal` 解析不出主人"的会话（口径住
    // `rc_ledger::transcript::principal_fail_open_sessions`，与闸门入口同一条）；
    // `subject_gate.mode` 是同一件事的全局读法，三值：
    // - `fail_open`：一条有效章都没有 ⇒ 整库 fail-open；
    // - `mixed`：有有效章，但还有会话解析不出主人（收紧的只是有章那部分）；
    // - `strict`：有有效章，且每个有 human 轮的会话都说得出主人。
    // 读失败同样走 `operational(3)`。
    let fail_open = match rc_ledger::transcript::principal_fail_open_sessions(root.transcript()) {
        Ok(n) => n,
        Err(_) => return operational("cannot read principal fail-open sessions"),
    };
    let subject_gate_mode = if claim_sessions == 0 {
        "fail_open"
    } else if fail_open > 0 {
        "mixed"
    } else {
        "strict"
    };
    obj.insert(
        "principal".into(),
        serde_json::json!({
            "claims": claim_sessions,
            "facts_with_agent_speaker_but_human_evidence": agent_speaker,
            "fail_open_sessions": fail_open,
            "subject_gate": {"mode": subject_gate_mode, "claim_sessions": claim_sessions},
        }),
    );
    // W14e（审计 P0-3）：两个悬空桶分开读数，**恒在场、零填充**。
    // `0` 是"这一桶干净"这个读数（与 `attention` 那一节同一条纪律），不是"这一格没有"；
    // 读失败仍走 `operational(3)`，不折成 0。两个桶的口径住 `rc_memory::issues::unresolved_surface_counts`
    // 一处（`resolved_at_ms IS NULL`），doctor 只做渲染。
    let (surface_open, shape_open) =
        match rc_memory::issues::unresolved_surface_counts(root.memory()) {
            Ok(counts) => counts,
            Err(_) => return operational("cannot read extraction issue counts"),
        };
    obj.insert(
        "issues".into(),
        serde_json::json!({
            "unresolved_surface": surface_open,
            "unresolved_surface_shape": shape_open,
        }),
    );
    // W15a（审计 P1-2 两套账）：`passed` 卡的背书来源，**恒在场、零填充**。
    // 三格的口径住 `rc_memory::issues::verdict_source_counts` 一处（最新事件 `max(id)`；
    // 没有事件的 passed 卡 = `passed_unbacked`，那是"来源未知"这个读数，不是读失败）。
    // 读失败仍走 `operational(3)`，不折成 0——与上面 `issues` 一节同一条纪律。
    let (pass_audit, pass_review_apply, passed_unbacked) =
        match rc_memory::issues::verdict_source_counts(root.memory()) {
            Ok(counts) => counts,
            Err(_) => return operational("cannot read verdict sources"),
        };
    obj.insert(
        "verdict_source".into(),
        serde_json::json!({
            "pass_audit": pass_audit,
            "pass_review_apply": pass_review_apply,
            "passed_unbacked": passed_unbacked,
        }),
    );
    // W15b2（审计 P1-1 的第二刀）：库里还有多少组"同 scope 同正名、两个不同 kind 的活锚"，
    // **恒在场、零填充**。写侧已不再添第二格，但存量那几对不在这里合并——这一格只回答
    // "还剩几对"，处理是数据动作，另排。口径住 `rc_memory::issues::cross_kind_duplicate_groups`
    // 一处；`0` 是读数，读失败仍走 `operational(3)`，不折成 0。
    let cross_kind = match rc_memory::issues::cross_kind_duplicate_groups(root.memory()) {
        Ok(n) => n,
        Err(_) => return operational("cannot read cross-kind anchor groups"),
    };
    obj.insert("cross_kind_duplicate_groups".into(), serde_json::json!(cross_kind));
    // W20（P3-2）：休眠的临时锚——**恒在场、零填充**。口径住
    // `rc_memory::issues::dormant_anchor_count`（`provisional` + 活行 + 门槛前建立的 +
    // 零 mention + 零 provenance + 零环境属性）；`0` 是读数，读失败仍走 `operational(3)`，
    // 不折成 0。**只读数、不 retire**：锚的退场是用户/操作员的动作（守卫 13），doctor 不动库。
    let dormant = match rc_memory::issues::dormant_anchor_count(root.memory(), now) {
        Ok(n) => n,
        Err(_) => return operational("cannot read dormant anchors"),
    };
    obj.insert("anchor_dormant".into(), serde_json::json!(dormant));
    // W22a（审计 P2-3 第一半）：每一条活锚的行 id 是否等于它自己的 preimage，**恒在场、零填充**。
    // 口径住 `rc_memory::issues::anchor_id_audit`（只查活锚；scope 由行上那两列构造，这里只渲染）。
    // `mismatch` 是读数不是故障：发现 id 对不上的行只报数，**不在这里修**——回拨/重键/退休是
    // 运维看过数之后的动作，doctor 不写记忆库（守卫 13 的读法）。
    // 读失败仍走 `operational(3)`，不折成 0——与本节其它读数同一条纪律。
    let (anchor_checked, anchor_mismatch) =
        match rc_memory::issues::anchor_id_audit(root.memory()) {
            Ok(counts) => counts,
            Err(_) => return operational("cannot read anchor id audit"),
        };
    obj.insert(
        "anchor_identity".into(),
        serde_json::json!({"checked": anchor_checked, "mismatch": anchor_mismatch}),
    );
    // W20（P3-3）：待复核的告警行——**恒在场、零填充**。口径住
    // `rc_memory::recheck::recheck_pending`（未解决且没盖过复核戳）。
    // `closable_by` 把解除条件写进读数本身：这一格不由谁点按钮解除，而是"之后又有一条落在
    // 同一行上的表态"（`rc_memory::recheck::recheck_attested` 那一支）——操作员不必去读代码。
    let recheck = match rc_memory::recheck::recheck_pending(root.memory()) {
        Ok(n) => n,
        Err(_) => return operational("cannot read recheck backlog"),
    };
    obj.insert(
        "recheck_pending".into(),
        serde_json::json!({
            "open": recheck,
            "closable_by": RECHECK_CLOSABLE_BY,
        }),
    );
    // 键的形状是架构 §9 的告警区承诺；探针读数接入时才会再往里填。
    obj.insert("alerts".into(), Value::Array(alerts));
    let mut rounds_obj = Map::new();
    rounds_obj.insert("current".into(), serde_json::json!(rounds));
    rounds_obj.insert("last_full_scan".into(), serde_json::json!(last_full));
    obj.insert("rounds".into(), Value::Object(rounds_obj));
    // ---- 三块读数（阶段 4 T5）----
    // 备份：取不到 ⇒ 这一格不出现（不是 0 小时）。
    if let Some(b) = backup {
        obj.insert("backup".into(), Value::Object(b));
    }
    // 密钥归档：`hub.key` 与导出计数都没有 ⇒ 整块不出现（不造恒告警/恒 0 的格）。
    if let Some(k) = key_archive {
        obj.insert("key_archive".into(), Value::Object(k));
    }
    // 连通性：**不是节点就没有这一格**——"这台机器不是节点"与"节点连不上"是两种后果。
    if let Some(c) = rc_diag::connectivity::read_connectivity(&a.data_dir) {
        obj.insert("node_sync".into(), serde_json::to_value(c).unwrap_or(Value::Null));
    }
    // V4 分片读数：只有 `<data-dir>/inject` 存在时才印。**没有目录 ⇒ 整块不出现**：
    // "还没开分片"与"开了但零"是两种后果，不造恒零的格。`reading` 只读、永不失败。
    if rc_store::shards::dir(&a.data_dir).is_dir() {
        let r = rc_store::shards::reading(&a.data_dir);
        obj.insert(
            "inject_shards".into(),
            serde_json::json!({"shards": r.shards, "evicted": r.evicted}),
        );
    }
    // 对账：对得上也印这一格（`agrees: true` 是读数）；告警只在 `!agrees` 时已 push。
    if let Some(rec) = reconcile {
        obj.insert("reconcile".into(), serde_json::to_value(rec).unwrap_or(Value::Null));
    }

    // ---- ④ 当前服务者那一节（§4.8 ④ + 裁定 E）----
    // **两格，不建第三格**：`serving` 取 `llm_state::Row.slug` 原样，`written_at` 取 `Row.written_at_ms`。
    // `slug` 不许换成"对端回传的 `model` 字段"：10-17 实测抓到 deepseek 请求 `deepseek-chat` 回传
    // `"model":"deepseek-flash"`（台账 §10(41) 登记①），照回显显示会把一家显示成一个表上不存在的名字。
    // **"本轮前进次数"这一格本批刻意不建**（不是漏了）：它的读数是进程内的 `Serving.failed_over`，
    // 跨进程没有生产者 ⇒ 按 §4.7 那条纪律"先不建格"。§4.8 ④ 原文那句"显示三件"因此只落两件，
    // 同一句纪律还规定 `inherited: bool` 也不开（§4.6 原文，原样有效）。
    let mut serving = Map::new();
    match machine.as_deref() {
        // 缺身份 ⇒ 这一节是 `none` 而**不是** `operational(3)`（裁定 E ③）：hub 要写这张表，
        // 拿不到身份必须响亮失败；doctor 只是把表读给操作员看，而它的全部价值在"半配置的机器上也能跑"。
        None => {}
        Some(m) => match rc_ledger::llm_state::read(root.ledger(), m) {
            // 这台机器还没写过行 ⇒ 同样 `none`，这也不是失败（与上面那支同一个渲染、不同到达路径）。
            Ok(None) => {}
            Ok(Some(row)) => {
                serving.insert("serving".into(), serde_json::json!(row.slug));
                serving.insert("written_at".into(), serde_json::json!(row.written_at_ms));
            }
            // 有身份却读不到 ⇒ `operational(3)` + 类别串。本文件里每一条"读不到"都是这个形状
            // （见上面 `attention_open` 那句注释）：不许静默折成 `none`——"没有服务者"是个好消息，
            // 拿它当读失败的兜底就是把最坏的失败渲染成最好的读数。
            Err(_) => return operational("cannot read llm serving state"),
        },
    }
    obj.insert("llm_state".into(), Value::Object(serving));

    let text = if a.json {
        match serde_json::to_string(&Value::Object(obj)) {
            Ok(t) => t,
            Err(_) => return operational("cannot render reading"),
        }
    } else {
        human_lines(&obj).join("\n")
    };
    emit(&text);
    ExitCode::from(0)
}

/// 写失败（下游把管道关了）不是读数失败：`println!` 在那一刻 panic，
/// 退出码就变成 101，而操作员读的是退出码。
fn emit(text: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out
        .write_all(text.as_bytes())
        .and_then(|_| out.write_all(b"\n"))
        .and_then(|_| out.flush());
}

fn operational(msg: &str) -> ExitCode {
    // 只报类别：StoreError 的原串带 data-dir 路径，零路径规则不许把它抄到运维面上。
    let _ = std::io::stderr().write_all(format!("error: {msg}\n").as_bytes());
    ExitCode::from(3)
}

fn now_ms(a: &Args) -> i64 {
    a.now_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// `recheck_pending` 那一格写下的**解除条件**（同词是 `rc_memory::recheck` 那条谓词的口径：
/// "之后又有一条落在同一行上的表态"）。它住在常量里而不是散在渲染串里，是因为它是给
/// 操作员读的承诺，与 `open` 同一格、同一份 JSON。
const RECHECK_CLOSABLE_BY: &str = "later_attestation";

/// 备份读数。两格：上次成功、上次失败。**取不到就不给这一格**。
/// `rows` 是 `main` 已经读好的那一份，不再读第二次。
fn backup_reading(
    rows: &[rc_ledger::counters::UsageRow],
    now_ms: i64,
) -> Option<Map<String, Value>> {
    let get = |metric: &str| {
        rows.iter()
            .find(|r| r.metric == metric && r.subject_kind == "maintenance")
            .map(|r| r.count)
    };
    let ok_at = get("backup_ok_at_ms")?;
    let mut m = Map::new();
    m.insert("last_ok_at_ms".into(), serde_json::json!(ok_at));
    m.insert(
        "age_hours".into(),
        serde_json::json!((now_ms - ok_at).max(0) / 3_600_000),
    );
    if let Some(f) = get("backup_fail_at_ms") {
        m.insert("last_fail_at_ms".into(), serde_json::json!(f));
    }
    Some(m)
}

/// `maintenance:key_archive:key_archive_exported_at_ms` 的那一行 —— B5b 的中心密钥归档读数。
const KEY_ARCHIVE_METRIC: &str = "key_archive_exported_at_ms";

/// `key_archive` 块：`last_at_ms`（0 = 从未导出）与 `fingerprint`（没有可用指纹就是 null）。
///
/// 块只在 `<data-dir>/hub.key` 在**或**导出计数在时出现：两样都没有的机器不造一个恒 0 的格。
///
/// 指纹的来路是 `<data-dir>/hub.pub`：hub 在每次读到/生成中心私钥之后用它自己的
/// `HubKey::load_or_create(..).public_b64()` 算出 `device_id_of`，连同 `hub.key` 的
/// (字节数, mtime 毫秒) 一起落盘。doctor 只做两件事——核对戳、念指纹——所以它
/// 不需要 `rc-net`（它的正式依赖清单里没有这一项，而 Ed25519 公钥推不出来），
/// 也**从不打开私钥本体**。戳对不上（钥匙被换掉而缓存没刷新）⇒ null：
/// 把上一把钥匙的指纹念成这一把的，比说"不知道"更坏。
fn key_archive_reading(
    rows: &[rc_ledger::counters::UsageRow],
    data_dir: &Path,
) -> Option<Map<String, Value>> {
    let exported = rows
        .iter()
        .find(|r| {
            r.subject_kind == "maintenance"
                && r.subject_id == "key_archive"
                && r.metric == KEY_ARCHIVE_METRIC
        })
        .map(|r| r.count);
    let key_path = data_dir.join("hub.key");
    let key_present = key_path.is_file();
    if !key_present && exported.is_none() {
        return None;
    }
    let fingerprint = if key_present {
        cached_fingerprint(data_dir, &key_path)
    } else {
        None
    };
    let mut m = Map::new();
    m.insert("last_at_ms".into(), serde_json::json!(exported.unwrap_or(0)));
    m.insert("fingerprint".into(), serde_json::json!(fingerprint));
    Some(m)
}

/// 读 hub 落下的公开指纹缓存；戳与当前 `hub.key` 的 (字节数, mtime) 必须逐字相等。
fn cached_fingerprint(data_dir: &Path, key_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join("hub.pub")).ok()?;
    let stamp: Value = serde_json::from_str(&text).ok()?;
    let size = stamp.get("key_size")?.as_u64()?;
    let mtime_ms = stamp.get("key_mtime_ms")?.as_i64()?;
    let fingerprint = stamp.get("fingerprint")?.as_str()?;
    let meta = std::fs::metadata(key_path).ok()?;
    let modified = meta.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    if meta.len() != size || since_epoch.as_millis() as i64 != mtime_ms {
        return None;
    }
    if !is_fingerprint_shape(fingerprint) {
        return None;
    }
    Some(fingerprint.to_owned())
}

/// `device_id_of` 的形状：`dev-` + 十六位小写 hex。缓存文件是自己写的，坏形状仍要拦下：
/// 它是盘上的数据，不是本进程的变量。
fn is_fingerprint_shape(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("dev-") else { return false };
    rest.len() == 16
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 摊平成 `k=v` 一行一条：操作员 grep 的是主语，不是嵌套层级。
///
/// 嵌套对象也摊平（`k.ik=v` / `k.ik.jk=v`），**不把 JSON 原串抄到人类面**：
/// `plain` 对 `Value::Object` 走的是 `to_string()`，那会把引号带进终端输出，而
/// "人类模式不许出现 JSON 引号"是 `capabilities.rs` 那条用例钉着的形状。
/// W20 的 `principal.subject_gate` 是第一个**空库也会出现**的嵌套块（`node_sync`
/// 只在节点机器上出现），于是这条规则从"碰巧没人踩"变成"必须成立"。
fn human_lines(obj: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(obj.len());
    flatten_into("", obj, &mut out);
    out
}

/// `human_lines` 的递归体：`prefix` 是父层已经攒下的 `k.`（顶层是空串）。
fn flatten_into(prefix: &str, obj: &Map<String, Value>, out: &mut Vec<String>) {
    for (k, v) in obj {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            // 空的一节也要留一行：读不出"没有账"和"这一节没印出来"是两件事（与 memory_schema 同理）。
            Value::Object(inner) if inner.is_empty() => out.push(format!("{key}=none")),
            Value::Object(inner) => flatten_into(&key, inner, out),
            Value::Array(items) if items.is_empty() => out.push(format!("{key}=none")),
            other => out.push(format!("{key}={}", plain(other))),
        }
    }
}

/// `attention` 未解决告警的一行。与 `quota_alerts` 同一个**条件形状**：`0` 不产任何一行。
/// 那一半不是风格：判据 43 钉的是"空库 `doctor --json` 的 `alerts` 必须是 `[]`"，
/// 这里若无条件产一条 `"attention: 0"`，那条判据当场红（`scripts/check-phase2.sh` 里那条
/// 判据的字面量就是这个数组）。
///
/// 行里只有 `kind` 与 `open` 两个数加一条动作指引，**没有任何记忆内容，也没有指认**（守卫 12 的
/// 读法：能进运维面的只有数和分型）。"是哪几条"住在记忆库的 `attention` 表里，那三列
/// `(kind, mem_table, mem_id)` 是指认的本体——把 id 念进这一行不会多给操作员任何信息，
/// 只会给"外带记忆库内部标识"开一个面。所以 doctor 与 `status` 两处都只报**有多少条要处理**，
/// 谁都不报是哪几条；要落到具体行，唯一的去处是记忆库那张表——`action` 就写这一件事。
///
/// `action` 里那两个名字都得实测过才许写进来，因为这一句是给操作员执行的，不是描述：
/// `recollect_status` 是 MCP 侧的工具名（`crates/mcp/src/main.rs` 的分派表），它输出的顶层格
/// 正是 `attention_open`；而"哪几条"今天**没有任何产品面读者**——`anomaly::open_attention_targets`
/// 是私有的、`find_contradictions` 在 src 侧零调用者，所以指认只能自己查库读 `attention` 表。
/// 初稿在这里写的是 `recollect-mcp status`，实测退出码 3（`error: invalid invocation`，那个 bin
/// 只接 `--data-dir` / `--device-id`，没有子命令），是一句照着工具名拼出来的命令行谎话：
/// 前半句把 MCP 工具名写成了 CLI 子命令，后半句 "then recall the flagged rows" 更假——
/// `recollect_recall` 要的是句柄，而从告警到句柄之间今天没有那一步。
fn attention_alerts(open: usize) -> Vec<Value> {
    if open == 0 { return Vec::new(); }
    vec![serde_json::json!({"kind": "attention", "open": open,
               "action": "recollect_status -> attention_open; the flagged targets live only in \
                          memory.db table attention"})]
}

/// P5 的告警：最近一次抽取运行触顶时**恰一条**，否则**零条**（P-70）。
///
/// 为什么条件化而不是每次追加一条 `exhausted: false`：`doctor` 今天有两条存量用例把 `alerts`
/// 钉成恒空（`alerts_field_is_present_and_empty` 与人类可读面那条 `alerts=none`），
/// 那是阶段 1"这一节还不该有内容"的钉子，不在本任务的预算里。条件化让它们在"没触顶"的库上
/// 逐字仍绿，而本任务新那条单元测试管"触顶时它真出现"。
///
/// 为什么告警对象里没有 `exhausted`：一条只在触顶时才存在的告警里再带一个恒 `true`
/// 的布尔，就是 P-44 那一族的反面——恒真的字段不报任何东西，它只是把"这条为什么在场"重新藏起来。
///
/// S3a：判定与 `status.quota_exhausted` **同源**——两边都读
/// `rc_ledger::counters::quota_stopped`（经 `rc_diag::doctor::quota_exhausted_alerts` 这一处），
/// `used` 仍然渲进 payload，但**不用它判告警**：累计 88 次只是成本读数。
/// 2026-11-06：本地调用配额已砍掉，于是载荷里那格单轮上限一并不再存在（没有上限可报）。
fn quota_alerts(rows: &[rc_ledger::counters::UsageRow]) -> Vec<Value> {
    let used: i64 = rows.iter()
        .filter(|r| r.metric == rc_llm::ALERT_METRIC)
        .map(|r| r.count)
        .sum();
    // 决策（含"最近一次运行触没触顶"与"这一族失败是否有停抽取后果"）在 rc-diag；
    // 这里只留数字的渲染。
    if rc_diag::doctor::quota_exhausted_alerts(rows).is_empty() {
        return Vec::new();
    }
    vec![serde_json::json!({
        "kind": "quota",
        "metric": rc_llm::ALERT_METRIC,
        "used": used,
    })]
}

/// P-70 同族（条件化、不每次追加一条 `halted: false`）。"注入 halted 行才出现"——
/// `doctor` 的存量用例把 `alerts` 钉成恒空（`alerts_field_is_present_and_empty` 与人类可读面
/// `alerts=none`），本条以条件化让它们逐字仍绿。
///
/// 编译修正（brief 原文写的是 `&rc_ledger::LedgerHandle`）：`rc-ledger` **不 re-export**
/// `LedgerHandle`（`rc_store` 才是它的家，实测 E0603），而 `root.ledger()` 给的正是
/// `&rc_store::LedgerHandle` ⇒ 形参路径改正，语义一字未动。
fn halt_alerts(l: &rc_store::LedgerHandle) -> Vec<Value> {
    let n = match rc_ledger::water::halted_scopes_count(l) {
        Ok(n) => n,
        Err(_) => return Vec::new(),   // ledger 读不到 ⇒ 不响响——doctor 是运维面，治库的事交给 hub
    };
    if n == 0 { return Vec::new(); }
    vec![serde_json::json!({"kind": "halt", "halted_scopes": n})]
}

/// S5：推导家族休眠的告警（判定在 `rc_diag::doctor::derive_dormant_alert`，这里只搬数与渲染）。
///
/// 两格输入的来源与 `status` 同源、不同道：`last_derive_at_ms` 与游标对都读水位账
/// （`water::last_derive_at_ms` / `water::read("derive","")`），余量用同一把
/// `rc_memory::reader::count_rows_since` 现算——那是 `status.derive_lag_rows` 的唯一生产者，
/// 这里不再写第二份判据（守卫 3）。
///
/// 三处读失败都 ⇒ 零条，与 `halt_alerts` 同一条纪律：doctor 是运维面，读不到不等于故障；
/// 而且条件化形状是判据 43（空库 `alerts` 必须是 `[]`）的护身符——空库上余量为 0，纯函数
/// 本来就不产告警，读失败再折成一条告警会把"读不到"渲染成"休眠"。
fn derive_dormant_alerts(
    l: &rc_store::LedgerHandle,
    m: &rc_store::MemoryHandle,
) -> Vec<Value> {
    let last = match rc_ledger::water::last_derive_at_ms(l) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // 游标整对搬走：`cursor_turn_uid` 存的正是"批末行的 key_string"（`tbl_prefix:id`），
    // 形状就是 `count_rows_since` 要的 `(ms, key)`（与 mcp 那条缝同一句读法）。
    let cursor = match rc_ledger::water::read(l, "derive", "") {
        Ok(c) => c.map(|c| (c.at_ms, c.turn_uid)),
        Err(_) => return Vec::new(),
    };
    let lag = match rc_memory::reader::count_rows_since(m, cursor) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let slugs = rc_diag::doctor::derive_dormant_alert(last, lag);
    match slugs.first() {
        // `lag_rows` 是这一行唯一的数：告警只报"还压着几行"，不指认是哪几行（守卫 12 的读法）。
        Some(kind) => vec![serde_json::json!({"kind": kind, "lag_rows": lag})],
        None => Vec::new(),
    }
}

/// `maintenance:rounds` / `maintenance:last_full_scan_round` 两计数 → JSON 对象。
/// 这两行**只读不写**。写侧是两个不同的生产者，说清谁写才不至于让人在这里找错地方：
/// `maintenance:rounds` 由 hub 的 `run_cycle` 每轮 `bump` 一次（裁定 8）；
/// `maintenance:last_full_scan_round` 走 `set_one`（覆盖不是累加），它的生产者是 hub 的
/// `run_anomaly_scan`——**只在全量扫描成功的那一轮**写，所以这一格落后于 `current` 是正常的
/// （每 N 轮才追平一次），而它**从未被写过**时读出来是 0：那是"这一层还没跑过"的读数，
/// 不是读失败，也不许拿它去推别的事。
fn rounds_reading(rows: &[rc_ledger::counters::UsageRow]) -> (i64, i64) {
    let get = |metric: &str| -> i64 {
        rows.iter().find(|r| r.metric == metric && r.subject_kind == "maintenance"
                          && r.subject_id.is_empty())
            .map(|r| r.count).unwrap_or(0)
    };
    (get("rounds"), get("last_full_scan_round"))
}

/// A45：抽取闸门三数 + 一个**只在这一层**算出来的率。
/// 三格的主语是 `llm_job:extract`，生产者是 `rc_distill::extract::run_extract` 的那三次 `bump`。
/// `kinds_reject_rate` 是显示层除法：它不落库、不进任何表的列，也不进 `usage_counter`——
/// 率一旦落库就成了第二个真相，两个真相迟早对不上（而分母是累计值，率本身没有稳态）。
/// 分母缺席时返回 `None` 而不是造一个 `0/0`：空库的 doctor JSON 必须与旧形状**逐键一致**，
/// 否则"这一层还没跑过"会被读成"跑过且很干净"。
fn extract_gate(rows: &[rc_ledger::counters::UsageRow]) -> Option<Value> {
    let get = |metric: &str| rows.iter()
        .find(|r| r.subject_kind == "llm_job" && r.subject_id == "extract" && r.metric == metric)
        .map(|r| r.count)
        .unwrap_or(0);
    let seen = get("entities_seen");
    if seen == 0 {
        return None;   // 没有生产者就不造 0/0；空库的 doctor JSON 仍与旧形状一致
    }
    let rejected = get("kinds_rejected");
    let aliases = get("aliases_dropped");
    Some(serde_json::json!({
        "entities_seen": seen,
        "kinds_rejected": rejected,
        "aliases_dropped": aliases,
        // 显示层除法：这两格不落任何表
        "kinds_reject_rate": rejected as f64 / seen as f64,
        "aliases_drop_rate": aliases as f64 / seen as f64,
    }))
}

// null 在这一行写成 none：读得出"这一层还不存在"，才不会和"读失败"混成一件事。
fn plain(v: &Value) -> String {
    match v {
        Value::Null => "none".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{attention_alerts, extract_gate, human_lines, quota_alerts};
    use serde_json::{Map, Value};

    #[test]
    fn attention_alerts_零告警不产行_有告警产一行() {
        assert!(attention_alerts(0).is_empty(), "0 ⇒ 空数组：判据 43 要空库的 alerts 是 []");
        let v = attention_alerts(3);
        assert_eq!(v.len(), 1, "非零 ⇒ 恰一行");
        assert_eq!(v[0]["open"], 3, "数要报出来，运维面只有数没有正文");
        assert_eq!(v[0]["kind"], "attention");
    }

    /// `action` 那一句的**所指**得对得上现实，因为它是给操作员照着执行的下一步。
    ///
    /// 为什么这里要测一个字面串：初稿那句写的是 `recollect-mcp status`，本机实跑
    /// `./target/debug/recollect-mcp.exe status` ⇒ 退出码 3（那个 bin 没有子命令）。字符串没有
    /// 类型、没有编译器管，`attention_alerts` 的其余断言也一眼看不见它——于是运维面上唯一的指引
    /// 可以长期是句执行不了的话而全仓不红。
    ///
    /// 这一条的形状是**从串里取出表名、拿它去问库**，不是"断言串里含 `attention` 再断言表存在"：
    /// 后者是恒绿的装饰（实测：把串里的表名改成 `attention_flagged`，那样写的版本照样 full green，
    /// 因为 `contains("attention")` 仍真、而 `sqlite_master` 那一句查的是我手写的字面量、不是串里那个）。
    /// 只有把串里的那个词当查询参数，"串指向一张不存在的表"才会红。
    #[test]
    fn action_点名的表真存在_且不冒充命令行子命令() {
        let v = attention_alerts(1);
        let action = v[0]["action"].as_str().unwrap();
        assert!(!action.contains("recollect-mcp"),
                "`recollect-mcp` 没有子命令，实跑退出码 3 ⇒ {action}");
        // 串以"… table <表名>"结尾：取最后一个 token 当表名，逐字拿去查库。
        let table = action.split_whitespace().last().unwrap().to_owned();
        let dir = tempfile::tempdir().unwrap();
        let root = rc_store::Root::open(dir.path()).unwrap();
        let n: i64 = root.memory().conn().query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [&table], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "`action` 点名的表 {table} 必须在记忆库里真实存在");
    }

    /// `Value::Null => "none"` 这条渲染规则的活体守卫。
    /// 任务 1 把 probe 收窄成只剩记忆柄之后，CLI 这条路已经造不出 null（tests/capabilities.rs
    /// 里那条 `!contains("null")` 因此看不见这个分支），所以这里直接递一个合成 map 给同一个
    /// 入口 `human_lines`：这一支被改成别的字面量、或整支删掉（落到 `other.to_string()`），
    /// 本用例当场红。
    #[test]
    fn null_渲染成_none_而不是_null() {
        let mut m = Map::new();
        m.insert("memory_schema".to_owned(), Value::Null);
        let text = human_lines(&m).join("\n");
        assert_eq!(text, "memory_schema=none", "null 那一行要读得出\"没有\"：{text}");
        assert!(!text.contains("null"), "人类可读面把 JSON 的 null 抄了出来：{text}");
    }

    /// `quota_alerts` 是纯函数，所以这一条不需要库、不需要子进程：几种输入各递一次同一个入口。
    /// 与同文件的 `null_渲染成_none_而不是_null` 同一条纪律、同一个理由：CLI 这条路造不出那个状态
    /// （要最近一次运行真的触顶得跑一轮配额用尽的 pass），而"造不出"不等于"不许测"。
    ///
    /// S3a 起告警的判据是**最近一次运行是否触顶**（`quota_stopped`），`llm_calls` 累计只作成本
    /// 读数：88 次累计 + stopped=0 这一支正是旧口径恒告警的回归证人。
    #[test]
    fn quota_alerts_只在烧完时给一条() {
        let row = |count: i64| rc_ledger::counters::UsageRow {
            subject_kind: "llm_job".into(),
            subject_id: "extract".into(),
            metric: rc_llm::ALERT_METRIC.into(),
            count,
            last_at_ms: 1,
        };
        let stopped = |v: i64| rc_ledger::counters::UsageRow {
            subject_kind: "llm_job".into(),
            subject_id: "cycle".into(),
            metric: rc_ledger::counters::QUOTA_STOPPED_METRIC.into(),
            count: v,
            last_at_ms: 2,
        };
        // 阴性：88 次累计 + stopped=0 ⇒ 空。这一支就是阶段 1 那两条存量钉子
        // （`alerts=none` / `len()==0`）的护身符，也是"累计不再代言触顶"的回归证人。
        assert_eq!(quota_alerts(&[row(88), stopped(0)]).len(), 0, "88 累计 ≠ 触顶：不许告警");
        assert_eq!(quota_alerts(&[row(12), row(3)]).len(), 0, "没有运行记录（null）不告警");
        assert_eq!(quota_alerts(&[]).len(), 0);
        // 正面：累计只有 12，但最近一次运行触顶 ⇒ 恰一条；`used` 仍是成本读数（12，不是上限）。
        let one = quota_alerts(&[row(12), stopped(1)]);
        assert_eq!(one.len(), 1, "stopped=1 必须出告警");
        assert_eq!(one[0]["used"], 12, "告警里的 used 是各家族累计之和，不是触顶的证据：{}", one[0]);
        assert!(one[0].get("limit").is_none(),
            "本地上限已砍掉：载荷里不许再有那一格：{}", one[0]);
        assert_eq!(one[0]["kind"], "quota");
        assert_eq!(one[0]["metric"], rc_llm::ALERT_METRIC);
        // 别的 metric 不许进这一条告警（`tool_call` 与 `probe_hit` 的计数与配额无关）。
        let other = rc_ledger::counters::UsageRow { metric: "tool_call".into(), count: 999, ..row(0) };
        assert_eq!(quota_alerts(&[other, stopped(0)]).len(), 0,
            "非 llm_calls 的计数被当成烧灼度：999 行 tool_call");
    }

    #[test]
    fn extract_gate_率是显示层除法() {
        let rows = vec![
            rc_ledger::counters::UsageRow { subject_kind: "llm_job".into(), subject_id: "extract".into(),
                metric: "entities_seen".into(), count: 4, last_at_ms: 0 },
            rc_ledger::counters::UsageRow { subject_kind: "llm_job".into(), subject_id: "extract".into(),
                metric: "kinds_rejected".into(), count: 1, last_at_ms: 0 },
            rc_ledger::counters::UsageRow { subject_kind: "llm_job".into(), subject_id: "extract".into(),
                metric: "aliases_dropped".into(), count: 2, last_at_ms: 0 },
        ];
        let v = extract_gate(&rows).expect("三数齐 ⇒ 必须有这一节");
        assert_eq!(v["entities_seen"], 4, "看见的实体数原样透出");
        assert_eq!(v["kinds_rejected"], 1, "拒掉的 kind 数原样透出");
        assert_eq!(v["kinds_reject_rate"], 0.25);
        assert_eq!(v["aliases_dropped"], 2);
        assert_eq!(v["aliases_drop_rate"], 0.5, "2/4：率是显示层除法");
    }
}
