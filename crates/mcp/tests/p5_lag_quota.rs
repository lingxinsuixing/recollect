//! P5 的线上判据。这一档存在的理由是 P-55 那句话：`Quota` 是**另一个进程**里的内存对象，
//! 所以"配额烧完"这件事只有一个地方能证——往水位库里真 `bump` 一条 `llm_job:*:llm_calls`，
//! 然后让**另一个进程**（这个子进程）读回来。进程内的 `Tap` 证不了跨进程。
//! 夹具来自任务 11 迁出的 `crates/mcp/tests/common/mod.rs`（P-65），本文件一行 `CARGO_BIN_EXE` 都不写。
mod common;
use common::*;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
/// 本任务自带的合成正文：**不用 `tools.rs` 那个 `CANARY-…` 串**，因为判据要靠"输出里没有它"
/// 断守卫 12，而那条金丝雀在别的用例里是会被合法召回的正文。
const LAG_BODY: &str = "落后面合成词 一条还没被蒸的 agent 轮";

/// 起一个会话、发 initialize、把句柄交回调用方（四条用例都在 init 之后还要再发工具调用）。
fn booted(dir: &std::path::Path) -> Session {
    let mut s = Session::start(dir);
    let _ = s.ask(&init(1, "qoder-test"));
    s
}

/// 把 `recollect_status` 的回包拆成 JSON 值。三格都以它为断言对象，所以抽成函数。
fn status_json(s: &mut Session, id: i64) -> Value {
    serde_json::from_str(&tool_text(&s.ask(&call(id, "recollect_status", json!({}))))).unwrap()
}

/// 转录库里已有的 turn_uid，按写入时刻升序。`turns(dir)` 只交回 `(role, body)` 两列（本机已核
/// `crates/mcp/tests/common/mod.rs:152`），而这一格的两个用例都要按 uid 去写状态行，所以在这里查一次。
fn turn_uids(data: &std::path::Path) -> Vec<String> {
    let root = rc_store::Root::open(data).unwrap();
    let got = root.transcript().conn()
        .prepare("SELECT turn_uid FROM turns ORDER BY created_at_ms")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap()
        .filter_map(|x| x.ok())
        .collect::<Vec<_>>();
    got
}

#[test]
fn 生命周期累计不等于配额耗尽() {
    // S3a：旧口径把生命周期 `llm_calls` 求和（88）与**单轮**上限（40）相比 ⇒ 恒 true。
    // 新口径只读"最近一次运行收尾写了什么"：88 次累计 + stopped=0 ⇒ false。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        let l = root.ledger();
        rc_ledger::counters::bump(l, "llm_job", "naming", "llm_calls", 28, NOW).unwrap();
        rc_ledger::counters::bump(l, "llm_job", "fact", "llm_calls", 45, NOW).unwrap();
        rc_ledger::counters::bump(l, "llm_job", "event", "llm_calls", 15, NOW).unwrap();
        rc_ledger::counters::set_one(
            l, "llm_job", "naming",
            rc_ledger::counters::QUOTA_STOPPED_METRIC, 0, NOW,
        ).unwrap();
    }
    let mut s = booted(dir.path());
    let body = status_json(&mut s, 2);
    assert_eq!(body["quota_exhausted"], json!(false), "88 累计不等于本轮触顶：{body}");
}

#[test]
fn 最近一次运行触顶时_status_报_true() {
    // 阳性：累计只有 1 次（远低于 40），但最近一次运行收尾写了 stopped=1 ⇒ true。
    // 这一条同时钉住另一半：判定来自运行记录，**不来自**累计。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        let l = root.ledger();
        rc_ledger::counters::bump(l, "llm_job", "event", "llm_calls", 1, NOW).unwrap();
        rc_ledger::counters::set_one(
            l, "llm_job", "event",
            rc_ledger::counters::QUOTA_STOPPED_METRIC, 1, NOW,
        ).unwrap();
    }
    let mut s = booted(dir.path());
    let out = status_json(&mut s, 2);
    assert_eq!(out["quota_exhausted"], json!(true), "1 次累计 + stopped=1 ⇒ 必须报触顶：{out}");
}

#[test]
fn 没有运行记录时_quota_exhausted_是_null() {
    // 阴性：账上只有累计、没有任何一次运行收尾写过 quota_stopped ⇒ `null`。
    // `false` 会被读成"运行过且没触顶"，而那是一次都没记录过。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_ledger::counters::bump(root.ledger(), "llm_job", "extract", "llm_calls", 40, NOW).unwrap();
    }
    let mut s = booted(dir.path());
    let out = status_json(&mut s, 2);
    assert!(out.as_object().unwrap().contains_key("quota_exhausted"), "{out}");
    assert_eq!(out["quota_exhausted"], Value::Null,
        "没有 quota_stopped 行时这一格是 null，不是 false：{out}");
    // S3a-fix（评审 M-3）：旧夹具末句"空库 ⇒ 落后 0"在改写时被删掉、没搬过来。
    // 空库上落后是**真读数** 0（两行数相减），不是 null——这一句补回原来那条断言。
    assert_eq!(out["distill_lag_turns"], json!(0), "空库上的落后是真读数 0：{out}");
    // S3c（评审 M-2）：中心面空库的 `freshness` 三元组整格钉在这里。两格年龄是 `null`
    // （没有卡、也没有终态 pass 行 ⇒ "从来没有"），而 `pending_turns` 是 **0**——它是
    // `distill_lag_turns` 的同一个变量，在中心面是一个真读数（controller 裁定：0 正确）。
    // 整格相等同时挡住"把 0 折成 null 去凑三格全 null"的改法。
    assert_eq!(out["freshness"], json!({
        "last_extract_age_s": null,
        "newest_memory_age_s": null,
        "pending_turns": 0,
    }), "中心面空库：年龄两格 never，待抽是真读数 0：{out}");
}

#[test]
fn llm_calls_total_是累计成本读数() {
    // R3：累计数另名 `llm_calls_total`，值等于所种三行之和（28+45+15=88）。
    // 它与旁边那格 `quota_exhausted` 互不代言：同一份账上 stopped=0 ⇒ 成本读数 88、判定 false。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        let l = root.ledger();
        rc_ledger::counters::bump(l, "llm_job", "naming", "llm_calls", 28, NOW).unwrap();
        rc_ledger::counters::bump(l, "llm_job", "fact", "llm_calls", 45, NOW).unwrap();
        rc_ledger::counters::bump(l, "llm_job", "event", "llm_calls", 15, NOW).unwrap();
        rc_ledger::counters::set_one(
            l, "llm_job", "event",
            rc_ledger::counters::QUOTA_STOPPED_METRIC, 0, NOW,
        ).unwrap();
    }
    let mut s = booted(dir.path());
    let out = status_json(&mut s, 2);
    assert_eq!(out["llm_calls_total"], json!(88), "累计数必须等于三行之和：{out}");
    assert_eq!(out["quota_exhausted"], json!(false), "成本读数不许参与触顶判定：{out}");
}

#[test]
fn 账上没有_llm_calls_行时_llm_calls_total_是_null() {
    // 阴性：没有 usage 行 ⇒ `null`（`0` 会被读成"跑过且一次调用都没花"）。
    let dir = tempfile::tempdir().unwrap();
    let mut s = booted(dir.path());
    let out = status_json(&mut s, 2);
    assert!(out.as_object().unwrap().contains_key("llm_calls_total"), "{out}");
    assert_eq!(out["llm_calls_total"], Value::Null,
        "没有任何 llm_calls 行时累计读数是 null：{out}");
}

#[test]
fn 三格在中心面上都是真值_不是_null() {
    // P-54 的中心面半边。三格各有各的生产者，所以三种形状一次测齐：
    // 落后 1（两条 remember 减一条状态行）、最后成功时刻（一次真 `advance`）、
    // 未触顶（一次运行收尾写了 stopped=0）。S3a 起第三格读的是运行记录，不再是累计和。
    let dir = tempfile::tempdir().unwrap();
    let _k = seed(dir.path(), NOW);
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_ledger::counters::bump(root.ledger(), "llm_job", "extract", "llm_calls", 3, NOW).unwrap();
        rc_ledger::counters::set_one(root.ledger(), "llm_job", "extract",
            rc_ledger::counters::QUOTA_STOPPED_METRIC, 0, NOW).unwrap();
    }
    let mut s = booted(dir.path());
    for i in 0..2 {
        let _ = s.ask(&call(10 + i, "recollect_remember",
                            json!({"body": format!("{LAG_BODY} 第 {} 条", i), "session_key": "ses-lag"})));
    }
    // 其中一条"被蒸过了"：S3b 起 lag 只认 `extract_pass_state` 的终态行，所以夹具要写
    // **这一张表**（legacy `extract_state` 今天的生产路径一行都不写，写它测出来的是假绿）。
    // 时间刻意取 `NOW + 7` 且不写 `job_watermark`：`last_extract_at_ms` 必须等于这一行自己的
    // `updated_at_ms`，否则"读的还是旧表"会以 `NOW` 这个巧合值混过去。
    let first = turn_uids(dir.path()).remove(0);
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_ledger::pass_state::mark_terminal(
            root.ledger(),
            rc_ledger::pass_state::PassKey { turn_uid: &first, pass_id: "naming", family: "" },
            rc_ledger::pass_state::PassState::Done, None, NOW + 7,
        ).unwrap();
    }
    let out = status_json(&mut s, 20);
    assert_eq!(out["distill_lag_turns"], 1, "两条轮、一条终态 pass 行 ⇒ 落后 1：{out}");
    assert_eq!(out["last_extract_at_ms"], NOW + 7,
        "这一格是终态行自己的 updated_at_ms，不是水位游标/legacy 行的时间：{out}");
    assert_eq!(out["quota_exhausted"], false, "stopped=0 ⇒ 没触顶：{out}");
    assert!(!out.to_string().contains(LAG_BODY), "运维读数里带了正文（守卫 12）：{out}");
}

#[test]
fn 落后格读的是转录减状态行_不是_pending_行数() {
    // P-69 的**错表守卫**（关键阴性）。用例名是判据 30 的锚（`scripts/check-phase2.sh` 按它
    // `--exact` 点名），S3b 换源不改名；"状态行"今天指 `extract_pass_state` 的终态行——
    // legacy `extract_state` 在生产路径上是 0 行，若 lag 仍读它，一条那里的 extracted
    // 就会把一轮读成已蒸。夹具：3 条轮 + 1 条 legacy `extracted` + pass 表 0 行 ⇒
    // 正确读数仍是 3（读旧表会报 2，读 pending 那一格会报 0）。
    let dir = tempfile::tempdir().unwrap();
    let _k = seed(dir.path(), NOW);
    let mut s = booted(dir.path());
    for i in 0..3 {
        let _ = s.ask(&call(10 + i, "recollect_remember",
                            json!({"body": format!("{LAG_BODY} 第 {} 条", i), "session_key": "ses-lag"})));
    }
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_ledger::water::set_extract_state(root.ledger(), &turn_uids(dir.path()).remove(0),
            rc_ledger::water::ExtractState::Extracted, None, NOW).unwrap();
    }
    let out = status_json(&mut s, 20);
    assert_eq!(out["distill_lag_turns"], 3,
        "legacy 表的 extracted 行不算已蒸；3 条轮 − 0 条 pass 终态 = 3（报 2 就是在读错表）：{out}");
    assert_eq!(out["last_extract_at_ms"], Value::Null,
        "pass 表没有终态行 ⇒ 最后抽取时刻是 null，不许拿 legacy 行的时间顶替：{out}");
    // 夹具自证：legacy 那一行真在（否则上面的 3 是空断言），而 pass 表真的一行都没有。
    let states: Vec<(rc_ledger::water::ExtractState, i64)> = {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_ledger::water::count_states(root.ledger()).unwrap()
    };
    assert_eq!(states, vec![(rc_ledger::water::ExtractState::Extracted, 1)],
        "夹具本来就该只有一条 legacy extracted 行");
    let pass_rows: i64 = {
        let root = rc_store::Root::open(dir.path()).unwrap();
        root.ledger().conn().query_row("SELECT count(*) FROM extract_pass_state", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(pass_rows, 0, "这一条的分母正是 pass 表的 0 行");
}

#[test]
fn 有退场账时_status_的_open_retirements_读到它() {
    // A17 那条"效果有出口"的落点（§2.9 表里留的债，本任务在这里还）。断的是**效果有出口**
    // 而不是**计数有出口**：审核票 1 不成立 ⇒ 写抑制账 ⇒ `open_retirements` 动。
    // cycle 那一侧的因果由任务 9 的 `cycle_once.rs` 整档在脚本判据 42 里跑；这一条管中心面读得到。
    let dir = tempfile::tempdir().unwrap();
    let key = seed(dir.path(), NOW);
    let out0 = status_json(&mut booted(dir.path()), 2);
    assert_eq!(out0["open_retirements"], 0, "播种只有一条卡：{out0}");
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        rc_memory::retire(root.memory(), &rc_memory::keys::MemKey::parse(&key).unwrap(),
                          "本条为出口判据而写", NOW).unwrap();
    }
    let out = status_json(&mut booted(dir.path()), 2);
    assert_eq!(out["open_retirements"], 1,
               "退场账写了而 status 读不出 ⇒ A17 那条'效果有出口'不成立：{out}");
    assert_eq!(out["distill_lag_turns"], 0, "退场与落后是两格，一格不许替另一格说话");
}

#[test]
fn system_轮不计入落后_且_lag_scopes_读_pass_终态() {
    // 258 那个永久底的形状：转录里有 system（压缩块/CTX）轮，候选抽取在 SQL 层就
    // `role <> 'system'` 挡掉它们，于是它们永远不会有 pass 终态行；若 lag 的分母仍数
    // **全部**轮，lag 就恒等于 system 轮数。`lag_scopes` 另有一半毛病：它仍读 legacy
    // `extract_state`（今天 0 行），会把每个 scope 的全部轮都报成落后。
    let dir = tempfile::tempdir().unwrap();
    let _k = seed(dir.path(), NOW);
    let mut s = booted(dir.path());
    for i in 0..2 {
        let _ = s.ask(&call(10 + i, "recollect_remember",
                            json!({"body": format!("{LAG_BODY} 第 {} 条", i),
                                   "session_key": "ses-lag"})));
    }
    // 同会话插一条 system 轮：只作上下文，永不作 TARGET。
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        root.transcript().conn().execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             SELECT ?1, session_key, device_id, 'system', '压缩块', 'sys-fp', 1, NULL, 2000, 2001
               FROM turns WHERE session_key='ses-lag' LIMIT 1",
            ["f".repeat(64)],
        ).unwrap();
    }
    // 两条可抽取轮都写 pass 终态 ⇒ 可抽取集合里没有落后。
    let uids: Vec<String> = {
        let root = rc_store::Root::open(dir.path()).unwrap();
        // 与 common::turns 同一坑：尾表达式里的 `Statement` 借了 `root`，必须先绑局部再返回。
        let got = root.transcript().conn()
            .prepare("SELECT turn_uid FROM turns WHERE role <> 'system'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0)).unwrap()
            .filter_map(|r| r.ok()).collect();
        got
    };
    assert_eq!(uids.len(), 2, "夹具：两条 remember 轮");
    {
        let root = rc_store::Root::open(dir.path()).unwrap();
        for uid in &uids {
            rc_ledger::pass_state::mark_terminal(
                root.ledger(),
                rc_ledger::pass_state::PassKey { turn_uid: uid, pass_id: "naming", family: "" },
                rc_ledger::pass_state::PassState::Done, None, NOW + 7,
            ).unwrap();
        }
    }
    let out = status_json(&mut s, 30);
    assert_eq!(out["distill_lag_turns"], 0,
        "system 轮不进分母，两条可抽取轮都已终态 ⇒ 落后 0：{out}");
    assert_eq!(out["lag_scopes"], json!([]),
        "lag_scopes 与主格同口径（pass 终态 + 可抽取轮）⇒ 没有落后 scope：{out}");
}