use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

use rc_memory::{issues, put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-doctor")
}

fn doctor(dir: &Path, now_ms: Option<i64>) -> Value {
    let mut cmd = Command::new(bin());
    cmd.args(["--capabilities", "--json", "--data-dir"]).arg(dir);
    if let Some(n) = now_ms {
        cmd.arg("--now-ms").arg(n.to_string());
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "doctor rc={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn seed_card(m: &rc_store::MemoryHandle, title: &str, now_ms: i64) {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-diag".into() };
    put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: title.into(),
            body: format!("diag body {title}"),
            scope,
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        now_ms,
    )
    .unwrap();
}

#[test]
fn the_backup_reading_reports_the_age_in_hours() {
    let dir = tempfile::tempdir().unwrap();
    let now = 10_000_000_000_i64;
    {
        let root = Root::open(dir.path()).unwrap();
        rc_ledger::counters::set_one(
            root.ledger(), "maintenance", "", "backup_ok_at_ms",
            now - 2 * 3_600_000, now,
        )
        .unwrap();
        rc_ledger::counters::set_one(
            root.ledger(), "maintenance", "", "backup_fail_at_ms",
            now - 60_000, now,
        )
        .unwrap();
    }
    let v = doctor(dir.path(), Some(now));
    assert_eq!(v["backup"]["last_ok_at_ms"].as_i64(), Some(now - 2 * 3_600_000), "{v}");
    assert_eq!(v["backup"]["age_hours"].as_i64(), Some(2), "{v}");
    assert_eq!(v["backup"]["last_fail_at_ms"].as_i64(), Some(now - 60_000), "{v}");
}

#[test]
fn a_machine_without_a_node_has_no_connectivity_block() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert!(
        v.get("node_sync").is_none(),
        "没有节点状态文件 ⇒ 不许给一份零值读数：{v}"
    );
}

#[test]
fn a_node_machine_reports_its_current_path_and_rate() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    // SyncState 没有 serde 默认值，必须给全 13 个字段。
    let st = json!({
        "paired_hub_pub": "",
        "push_cursor": {"at_ms": 0, "turn_uid": ""},
        "pending_turns": 3,
        "current_manifest": {"epoch": 1, "seq": 1, "sha256": "", "created_at_ms": 777},
        "current_endpoint": "lan-ipv6",
        "last_rate_bps": 2_500_000_u64,
        "resume_count": 2_u64,
        "last_push_at_ms": 5,
        "last_pull_at_ms": 6,
        "last_push_accepted": 0,
        "last_push_rejected": 0,
        "last_error": "",
        "partial": {"epoch": 0, "seq": 0, "received": 0}
    });
    std::fs::write(dir.path().join("node-state.json"), st.to_string()).unwrap();
    let v = doctor(dir.path(), None);
    assert_eq!(v["node_sync"]["endpoint"], "lan-ipv6", "{v}");
    assert_eq!(v["node_sync"]["last_rate_bps"].as_u64(), Some(2_500_000), "{v}");
    assert_eq!(v["node_sync"]["resume_count"].as_u64(), Some(2), "{v}");
    assert_eq!(v["node_sync"]["pending_turns"].as_i64(), Some(3), "{v}");
    assert_eq!(v["node_sync"]["as_of_ms"].as_i64(), Some(777), "{v}");
}

#[test]
fn a_reconcile_mismatch_raises_one_alert() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        seed_card(root.memory(), "one", 1);
        rc_index::rebuild(root.memory(), 2).unwrap();
        seed_card(root.memory(), "two", 3);
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["reconcile"]["delta"].as_i64(), Some(-1), "{v}");
    let hit = v["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "index_content_mismatch")
        .collect::<Vec<_>>();
    assert_eq!(hit.len(), 1, "对不上账必须恰一条告警：{v}");
    assert_eq!(hit[0]["delta"], -1, "{v}");
}

/// S5：推导休眠告警的**接线**那一半（纯函数的判定两向住 `rc-diag/tests/derive_dormant.rs`）。
/// 没有这一条的话，`main` 里 `alerts.extend(derive_dormant_alerts(..))` 那句被整个删掉，
/// 全仓仍然零红——纯函数测不到接线，这与 `halt_alerts` 那条注释点名的坑同形。
///
/// 正支：记忆层有一行活行、水位账里一行 derive 都没有（从没成功推进过）⇒ 恰一条
/// `derive_dormant`，且 `lag_rows` 是它唯一的数（不指认是哪几行）。
/// 反支：把 derive 水位推到这一行之后 ⇒ 余量归零、`last_success_at_ms` 也有值，告警消失。
#[test]
fn a_never_derived_library_with_rows_raises_the_dormant_alert() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        seed_card(root.memory(), "pending", 1);
    }
    let v = doctor(dir.path(), None);
    let hit: Vec<&Value> = v["alerts"].as_array().unwrap().iter()
        .filter(|a| a["kind"] == "derive_dormant").collect();
    assert_eq!(hit.len(), 1, "池里有行而推导从没跑过 ⇒ 恰一条：{v}");
    assert_eq!(hit[0]["lag_rows"].as_i64(), Some(1), "告警要报余量：{v}");

    {
        let root = Root::open(dir.path()).unwrap();
        rc_ledger::water::advance_settled(
            root.ledger(), "derive", "",
            &rc_ledger::water::Cursor { at_ms: 2, turn_uid: "x".into() }, 3,
        ).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert!(v["alerts"].as_array().unwrap().iter().all(|a| a["kind"] != "derive_dormant"),
            "追平之后仍报休眠（判据两格缺一就不许响）：{v}");
}

#[test]
fn doctor_reports_the_shard_count_and_evictions() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    // 没有 `inject/` ⇒ 整块不出现（不造恒零读数）。
    let v = doctor(dir.path(), None);
    assert!(v.get("inject_shards").is_none(), "没有分片目录却给了 inject_shards：{v}");

    let keys = vec!["card:one".to_string(), "card:two".to_string()];
    rc_store::shards::record(dir.path(), "sess-one", &keys, 1_000).unwrap();
    rc_store::shards::record(dir.path(), "sess-two", &keys, 2_000).unwrap();
    // 淘汰计数是文件（不进 usage_counter）：fixture 直接摆一个可读的十进制数。
    std::fs::write(rc_store::shards::dir(dir.path()).join("evicted"), b"7").unwrap();

    let v = doctor(dir.path(), None);
    assert_eq!(v["inject_shards"]["shards"].as_u64(), Some(2), "{v}");
    assert_eq!(v["inject_shards"]["evicted"].as_u64(), Some(7), "{v}");
}

#[test]
fn the_three_counters_have_readings() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        rc_ledger::counters::bump(root.ledger(), "llm_job", "extract", "llm_calls", 7, 1).unwrap();
        rc_ledger::counters::bump(
            root.ledger(),
            "device",
            "dev-counters",
            "ingest_turns_accepted",
            3,
            1,
        )
        .unwrap();
        rc_ledger::counters::bump(
            root.ledger(),
            "agent_tool",
            "recollect_recall",
            "tool_call",
            2,
            1,
        )
        .unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["counters"]["llm_job:extract:llm_calls"].as_i64(), Some(7), "{v}");
    assert_eq!(
        v["counters"]["device:dev-counters:ingest_turns_accepted"].as_i64(),
        Some(3),
        "{v}"
    );
    assert_eq!(
        v["counters"]["agent_tool:recollect_recall:tool_call"].as_i64(),
        Some(2),
        "{v}"
    );
}

/// S3b/R4：`count_terminal_by_pass` 的运维读者。lag 换源到 `extract_pass_state` 之后，
/// "落后多少"只剩一个总数；这一块把"每个 pass 覆盖了多少轮"摊给操作员，否则卡在哪一支
/// 只能自己去查库。空表给 `{}`（"没有一个 pass 有终态行"是读数），不是缺块。
///
/// **W32-A5 语义变化（只动断言臂，测试名不动）**：`pass_coverage` 从"每 pass 一个终态
/// 轮数"拆成 `done/empty/rejected/quarantined/stale` 五桶 + 派生格 `covered`；
/// `quarantined` **不再计入 covered**——`TERMINAL_STATES` 含隔离态，旧口径把 1949 条
/// 隔离格读成"已覆盖"。夹具里那条 quarantined 行就是验收形状：它单独成桶、`covered`
/// 不含它。单位从"去重轮"变为"状态行"（`count_by_state` 数格：fact 同一轮的多族各算
/// 一行——分桶要回答"卡在哪一支"，格比轮说得清）。
#[test]
fn doctor_reports_terminal_turn_coverage_by_pass() {
    use rc_ledger::pass_state::{mark_running, mark_terminal, PassKey, PassState};
    let key = |turn_uid: &'static str, pass_id: &'static str, family: &'static str| PassKey {
        turn_uid,
        pass_id,
        family,
    };
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let l = root.ledger();
        // naming 覆盖两轮；同一轮的两个 pass 各记一轮（按 pass 分别去重）。
        mark_terminal(l, key("turn-a", "naming", ""), PassState::Done, None, 1).unwrap();
        mark_terminal(l, key("turn-b", "naming", ""), PassState::Empty, None, 2).unwrap();
        mark_terminal(l, key("turn-a", "fact", "decision"), PassState::Done, None, 3).unwrap();
        // A5：一条隔离格——单独成桶，且不许被 `covered` 顺带数进去。
        mark_terminal(l, key("turn-d", "time", ""), PassState::Quarantined, None, 5).unwrap();
        // running 不是覆盖：这一格一个数都不该出现。
        mark_running(l, key("turn-c", "event", ""), "fp", "run", 4).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["pass_coverage"]["naming"]["done"].as_i64(), Some(1), "{v}");
    assert_eq!(v["pass_coverage"]["naming"]["empty"].as_i64(), Some(1), "{v}");
    assert_eq!(v["pass_coverage"]["naming"]["covered"].as_i64(), Some(2), "{v}");
    assert_eq!(v["pass_coverage"]["naming"]["quarantined"].as_i64(), Some(0), "{v}");
    assert_eq!(v["pass_coverage"]["fact"]["done"].as_i64(), Some(1), "{v}");
    assert_eq!(v["pass_coverage"]["fact"]["covered"].as_i64(), Some(1), "{v}");
    assert_eq!(v["pass_coverage"]["time"]["quarantined"].as_i64(), Some(1), "{v}");
    assert_eq!(v["pass_coverage"]["time"]["covered"].as_i64(), Some(0),
        "隔离格不算已覆盖：time 只有这条隔离行，covered 必须是 0：{v}");
    assert!(v["pass_coverage"].get("event").is_none(),
        "running 行不是覆盖，不许出现在这一块：{v}");

    let empty = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(empty.path()).unwrap();
    }
    let v = doctor(empty.path(), None);
    assert_eq!(v["pass_coverage"], json!({}),
        "没有终态行 ⇒ 空对象（读数），不是缺块：{v}");
}

/// W32-A2：review 轴的两本账（水位表 vs `review_item` 实表）在 doctor 里对账。
/// 空库两本账都是 0 ⇒ `agree: true` 且无告警；水位动了、实表没跟上 ⇒ `agree: false`
/// 且恰一条 `review_watermark_mismatch`，两个原值都在告警与读数块里。
#[test]
fn the_review_reconcile_cell_compares_watermark_against_the_table() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["review_reconcile"]["watermark_pending"].as_i64(), Some(0), "{v}");
    assert_eq!(v["review_reconcile"]["table_pending"].as_i64(), Some(0), "{v}");
    assert_eq!(v["review_reconcile"]["agree"], json!(true), "{v}");
    let mismatch = v["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "review_watermark_mismatch")
        .count();
    assert_eq!(mismatch, 0, "空库两本账都是 0，不许告警：{v}");

    // 水位表有一行 pending=2，实表没有对应待审项 ⇒ 失配恰一条告警。
    {
        let root = Root::open(dir.path()).unwrap();
        rc_ledger::review_mark::put(
            root.ledger(),
            &rc_ledger::review_mark::ReviewWatermark {
                pass_id: "naming".into(),
                max_decision_id: 0,
                pending_review: 2,
                quarantined: 0,
                applied: 0,
                updated_at_ms: 1,
            },
        )
        .unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["review_reconcile"]["watermark_pending"].as_i64(), Some(2), "{v}");
    assert_eq!(v["review_reconcile"]["table_pending"].as_i64(), Some(0), "{v}");
    assert_eq!(v["review_reconcile"]["agree"], json!(false), "{v}");
    let hits: Vec<&serde_json::Value> = v["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "review_watermark_mismatch")
        .collect();
    assert_eq!(hits.len(), 1, "失配必须恰一条告警：{v}");
    assert_eq!(hits[0]["watermark_pending"].as_i64(), Some(2), "{v}");
    assert_eq!(hits[0]["table_pending"].as_i64(), Some(0), "{v}");
}

// ── P0-1：身份通道的两格读数 ────────────────────────────────────────────────

/// 空库上两格都是 `0`（不是缺席）：`0` 在这里是"身份通道还没有任何一条章 / 还没有一条
/// 把用户的话记成 agent 的话"这个**读数**，与"读不到"是两种后果。
#[test]
fn principal_两格在空库上是零而不是缺席() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["claims"].as_i64(), Some(0), "空库两格必须印出来：{v}");
    assert_eq!(
        v["principal"]["facts_with_agent_speaker_but_human_evidence"].as_i64(),
        Some(0),
        "{v}"
    );
    // 人类可读面同样两行（`k=v` 摊平）：运维 grep 的是主语。
    let mut cmd = Command::new(bin());
    cmd.args(["--capabilities", "--data-dir"]).arg(dir.path());
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("principal.claims=0"), "{text}");
    assert!(text.contains("principal.facts_with_agent_speaker_but_human_evidence=0"), "{text}");
}

/// 阳性：一条有效章 + 一条"human 证据却挂在 agent 锚上"的 fact ⇒ 两格各 1。
///
/// 缺陷形状用**生产的写库函数**造（`ensure_agent_anchor` + `put_fact_rows`），只把
/// `provenance.anchor_role` 改回 `human`——那条列是程序值，真实缺陷正是"身份未知时
/// 写侧拿自锚顶替 human 轮"，用它把读数钉住比手拼一行 fact 更接近现场。
#[test]
fn principal_两格各自认自己的主语() {
    use rc_memory::keys::{AnchorRole as Role, Scope as Sc, ScopeKind as Kind};
    use rc_memory::owner::{OwnerCtx, Visibility};
    use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
    use rc_memory::BatchCtx;

    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000_000_i64;
    {
        let root = Root::open(dir.path()).unwrap();
        rc_ledger::transcript::claim_principal(
            root.transcript(),
            rc_ledger::transcript::NewPrincipalClaim {
                session_key: "ses-doc",
                device_id: "dev-doc",
                principal_id: "p-doc",
                principal_name: Some("示例主人"),
                source: "agent_runtime",
                claimed_by: "env:RECOLLECT_PRINCIPAL_NAME",
                reason: None,
            },
            now,
        )
        .unwrap();

        let m = root.memory();
        let scope = Sc { kind: Kind::Project, key: "proj-doc".into() };
        let ctx = BatchCtx { source_agent: "qoder".into(), device_id: "dev-doc".into() };
        let owner = OwnerCtx {
            owner_principal_id: None,
            owner_agent_id: "qoder".into(),
            visibility: Visibility::Project,
            retention: "default".into(),
        };
        let self_anchor = rc_memory::ensure_agent_anchor(m, &ctx, &owner, &scope, now)
            .unwrap()
            .id;
        let item = FactWriteItem {
            subject_anchor_id: self_anchor.clone(),
            object_anchor_id: None,
            predicate: "decide".into(),
            value: Some("把发布推迟一天".into()),
            category: "decision".into(),
            mode: "stated".into(),
            time_text: String::new(),
            quote: String::new(),
            // agent 证据 ⇒ 说话人是自锚（这就是缺陷形状：主语是用户，说话人却是 agent）。
            evidence: vec![("d".repeat(64), Role::Agent)],
            text: "决定把发布推迟一天".into(),
            scope: scope.clone(),
            source_ms: now,
        };
        put_fact_rows(m, std::slice::from_ref(&item), &ctx, &owner, now).unwrap();
        // 把出处标成 human：真实缺陷里 human 轮的 speaker 被顶成了 agent 锚。
        let changed = m
            .conn()
            .execute(
                "UPDATE provenance SET anchor_role='human' WHERE mem_table='fact'",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1, "夹具前提：恰好一条 fact 出处");
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["claims"].as_i64(), Some(1), "一条有效章：{v}");
    assert_eq!(
        v["principal"]["facts_with_agent_speaker_but_human_evidence"].as_i64(),
        Some(1),
        "human 证据 + agent 说话人的那一行必须被数到：{v}"
    );
}

/// W14e（审计 P0-3）：`issues` 一节恒在场、零填充。空库也要给两个 `0`——
/// "这一桶干净"是读数，"这一格没有"是另一种后果，doctor 不许把两者混成一个缺席。
#[test]
fn the_issues_block_is_present_and_zero_filled_on_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["issues"]["unresolved_surface"].as_i64(), Some(0), "{v}");
    assert_eq!(v["issues"]["unresolved_surface_shape"].as_i64(), Some(0), "{v}");
}

/// W14e：只有真名桶的副本 ⇒ 真名桶 1、形状桶 0；已解决的行两边都不计。
#[test]
fn the_issues_block_counts_only_the_name_bucket_when_that_is_all_there_is() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        issues::record(
            m,
            "entity",
            issues::UNRESOLVED_SURFACE_CODE,
            "run-a",
            "turn=t1; surface=老张; reason=批内没有落点; cand=0",
            1,
        )
        .unwrap();
        let closed = issues::record(
            m,
            "entity",
            issues::UNRESOLVED_SURFACE_CODE,
            "run-a",
            "turn=t2; surface=那台设备; reason=信息不足; cand=0",
            2,
        )
        .unwrap();
        assert!(issues::resolve(m, closed, 3).unwrap());
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["issues"]["unresolved_surface"].as_i64(), Some(1), "已解决的行不计：{v}");
    assert_eq!(v["issues"]["unresolved_surface_shape"].as_i64(), Some(0), "{v}");
}

/// W14e：两桶都有的副本 ⇒ 两桶各数各的（这是 P0-3 要的那一格：
/// "倾倒口"与"真歧义"再也不能混成一个计数）。
#[test]
fn the_issues_block_counts_the_two_buckets_separately() {
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        issues::record(
            m,
            "entity",
            issues::UNRESOLVED_SURFACE_CODE,
            "run-b",
            "turn=t1; surface=测试组的李伟; reason=同名但没解析到; cand=1",
            1,
        )
        .unwrap();
        for (index, shape) in ["hex", "path"].into_iter().enumerate() {
            issues::record(
                m,
                "entity",
                issues::UNRESOLVED_SURFACE_SHAPE_CODE,
                "run-b",
                &format!("turn=t{}; surface=s{index}; shape={shape}", index + 2),
                2 + index as i64,
            )
            .unwrap();
        }
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["issues"]["unresolved_surface"].as_i64(), Some(1), "{v}");
    assert_eq!(v["issues"]["unresolved_surface_shape"].as_i64(), Some(2), "{v}");
}

/// W15a（审计 P1-2）：`verdict_source` 一节恒在场、零填充。空库也要给三个 `0`。
#[test]
fn the_verdict_source_block_is_present_and_zero_filled_on_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["verdict_source"]["pass_audit"].as_i64(), Some(0), "{v}");
    assert_eq!(v["verdict_source"]["pass_review_apply"].as_i64(), Some(0), "{v}");
    assert_eq!(v["verdict_source"]["passed_unbacked"].as_i64(), Some(0), "{v}");
}

/// W15a：三种分布各断言一次——只有未背书、再加 audit、再加 review_apply。
/// 每加一张卡都重读一次，三格里各自的"这一桶真能非零"与"别的桶没被顺带数进去"都钉住。
#[test]
fn the_verdict_source_block_splits_passed_cards_by_who_stamped_them() {
    let dir = tempfile::tempdir().unwrap();
    let now = 9_000_i64;
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        // ① 存量形状：列上 passed、账上没有事件（W15a 之前 audit 盖的那 36 张同形）。
        let legacy = seed_card_key(m, "legacy", now);
        rc_memory::set_verdict(m, &legacy, rc_memory::Verdict::Passed, now).unwrap();
    }
    let v = doctor(dir.path(), Some(now));
    assert_eq!(v["verdict_source"]["pass_audit"].as_i64(), Some(0), "{v}");
    assert_eq!(v["verdict_source"]["pass_review_apply"].as_i64(), Some(0), "{v}");
    assert_eq!(v["verdict_source"]["passed_unbacked"].as_i64(), Some(1), "{v}");

    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        // ② audit 自盖：来源行 `audit`。
        let by_audit = seed_card_key(m, "by-audit", now + 1);
        rc_memory::set_verdict_from(
            m, &by_audit, rc_memory::Verdict::Passed, rc_memory::VERDICT_SOURCE_AUDIT, now + 1,
        )
        .unwrap();
    }
    let v = doctor(dir.path(), Some(now));
    assert_eq!(v["verdict_source"]["pass_audit"].as_i64(), Some(1), "{v}");
    assert_eq!(v["verdict_source"]["pass_review_apply"].as_i64(), Some(0), "{v}");
    assert_eq!(v["verdict_source"]["passed_unbacked"].as_i64(), Some(1), "{v}");

    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        // ③ 审核队列背书：put_card 出厂 unreviewed ⇒ promote 的 CAS 动一行。
        let by_review = seed_card_key(m, "by-review", now + 2);
        assert_eq!(rc_memory::promote_verdict(m, &by_review, now + 2).unwrap(), 1);
    }
    let v = doctor(dir.path(), Some(now));
    assert_eq!(v["verdict_source"]["pass_audit"].as_i64(), Some(1), "{v}");
    assert_eq!(v["verdict_source"]["pass_review_apply"].as_i64(), Some(1), "{v}");
    assert_eq!(v["verdict_source"]["passed_unbacked"].as_i64(), Some(1), "{v}");
}

/// 与 `seed_card` 同一份载荷，另外把句柄交回来（上面那条要拿它去改判）。
fn seed_card_key(m: &rc_store::MemoryHandle, title: &str, now_ms: i64) -> rc_memory::MemKey {
    let scope = Scope { kind: ScopeKind::Project, key: "proj-diag".into() };
    put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: title.into(),
            body: format!("diag body {title}"),
            scope,
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        now_ms,
    )
    .unwrap()
}

// ── W20：四格新读数（P3-2 休眠锚 / P3-3 待复核 / P1-3 6c 的 fail-open 会话）────────────

const DAY: i64 = 24 * 3_600_000;

/// 一条旧 `provisional` 锚：`created_at_ms` 直接写，因为"建了 30 天以上"只有直插造得出来
/// （没有生产入口会往回盖建锚时刻）。
fn old_provisional_anchor(dir: &Path, name: &str, created_at_ms: i64) {
    let root = Root::open(dir).unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,status,
                                created_at_ms,updated_at_ms,norm_version)
             VALUES(?1,'person',?2,'real','project','proj-diag','provisional',?3,?3,1)",
            (format!("a-{name}"), name, created_at_ms),
        )
        .unwrap();
}

/// 一条未解决、没盖过复核戳的告警行（`recheck_pending` 的唯一主语）。
fn open_attention(dir: &Path, mem_id: &str, created_at_ms: i64) {
    let root = Root::open(dir).unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO attention(kind,mem_table,mem_id,created_at_ms,resolved_at_ms,
                                   rechecked_at_ms)
             VALUES('contradiction','fact',?1,?2,NULL,NULL)",
            (mem_id, created_at_ms),
        )
        .unwrap();
}

fn session(key: &str, device: &str) -> rc_contract::SessionId {
    rc_contract::SessionId {
        session_key: key.into(),
        device_id: device.into(),
        source_agent: "qoder".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    }
}

/// 一个带 human 轮的会话：走生产唯一写入口（`insert_batch`），role 逐字是 `human`。
fn human_session(dir: &Path, key: &str, device: &str) {
    let body = "一句人工写的正文";
    let turn = rc_contract::NormalizedTurn {
        tier: rc_contract::TurnTier::ContentOnly,
        turn_uid: rc_contract::compute_turn_uid(device, key, "human", body, 1),
        body: body.into(),
        body_fp: rc_contract::compute_body_fp(body),
        role: "human".into(),
        created_at_ms: 1,
    };
    let root = Root::open(dir).unwrap();
    rc_ledger::insert_batch(root.transcript(), &session(key, device), &[turn], 10).unwrap();
}

/// 盖一章（`source=human_claim` 是闭集里的常规值，与 `rc-ledger` 的既有用例同一档）。
fn claim(dir: &Path, key: &str, device: &str, principal_id: &str, now_ms: i64) {
    let root = Root::open(dir).unwrap();
    rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: key,
            device_id: device,
            principal_id,
            principal_name: None,
            source: "human_claim",
            claimed_by: "qoder",
            reason: None,
        },
        now_ms,
    )
    .unwrap();
}

/// 会话行上的旧式主人列（回落支的夹具）：一行 SQL，因为写侧刻意不写这一列。
fn owner_column(dir: &Path, key: &str, device: &str, principal_id: &str) {
    let root = Root::open(dir).unwrap();
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=?1 WHERE session_key=?2 AND device_id=?3",
            (principal_id, key, device),
        )
        .unwrap();
}

/// W22a（审计 P2-3 第一半）：`anchor_identity` 恒在场、零填充，且真的把 id 对不上的那一行
/// 数出来——两条锚（一条 id = 自己的 preimage，一条是别的 64 位 hex）⇒ checked 2 / mismatch 1。
///
/// 这格是**只读**读数：doctor 不许因为它报 1 就去改库，用例因此只看 JSON 与人类可读面，
/// 事后也不回写（那一半属于运维的动作，不属这一单）。
#[test]
fn the_anchor_identity_cell_is_present_and_counts_id_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["anchor_identity"]["checked"].as_i64(), Some(0),
               "空库这一格也必须印出来，两个 0 是读数：{v}");
    assert_eq!(v["anchor_identity"]["mismatch"].as_i64(), Some(0), "{v}");

    let scope = Scope { kind: ScopeKind::Project, key: "proj-diag".into() };
    let good = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor, "person", "对得上的锚", "", &scope);
    let bad = "c".repeat(64);
    {
        let root = Root::open(dir.path()).unwrap();
        for (id, canonical) in [(&good, "对得上的锚"), (&bad, "对不上的锚")] {
            root.memory()
                .conn()
                .execute(
                    "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,
                                        status,created_at_ms,updated_at_ms,norm_version)
                     VALUES(?1,'person',?2,'real','project','proj-diag','active',1,1,1)",
                    (id, canonical),
                )
                .unwrap();
        }
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["anchor_identity"]["checked"].as_i64(), Some(2), "{v}");
    assert_eq!(v["anchor_identity"]["mismatch"].as_i64(), Some(1),
               "两行里恰一行 id 与它自己的 preimage 对不上：{v}");

    let out = Command::new(bin())
        .args(["--capabilities", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("anchor_identity.checked=2"), "人类可读面也要有这一格：{text}");
    assert!(text.contains("anchor_identity.mismatch=1"), "人类可读面也要有读数：{text}");
}

/// P3-2：`anchor_dormant` 恒在场、零填充，且真的数得出那条旧 `provisional` 锚。
#[test]
fn the_anchor_dormant_cell_is_present_and_counts_old_provisional_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let now = 1_700_000_000_000_i64;
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), Some(now));
    assert_eq!(v["anchor_dormant"].as_i64(), Some(0),
               "空库这一格也必须印出来，0 是读数：{v}");

    old_provisional_anchor(dir.path(), "没人提过", now - 40 * DAY);
    // 门槛内的那一格不许被顺带数进去（30 天那条边界正是这一刀）。
    old_provisional_anchor(dir.path(), "刚建出来", now - 2 * DAY);

    let v = doctor(dir.path(), Some(now));
    assert_eq!(v["anchor_dormant"].as_i64(), Some(1),
               "只数门槛外那条临时锚：{v}");

    let out = Command::new(bin())
        .args(["--capabilities", "--data-dir"])
        .arg(dir.path())
        .arg("--now-ms")
        .arg(now.to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("anchor_dormant=1"), "人类可读面也要有这一格：{text}");
}

/// P3-3：`recheck_pending` 的两个键逐字钉住（`open` + `closable_by`），并跟着库走。
#[test]
fn the_recheck_pending_block_pins_its_two_keys() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    let block = v["recheck_pending"].as_object().unwrap_or_else(|| panic!("缺这一块：{v}"));
    assert_eq!(block.len(), 2, "这一块恰两个键（open + closable_by）：{v}");
    assert_eq!(v["recheck_pending"]["open"].as_i64(), Some(0), "{v}");
    assert_eq!(v["recheck_pending"]["closable_by"].as_str(), Some("later_attestation"),
               "解除条件要写进读数：{v}");

    open_attention(dir.path(), "f-1", 100);
    let v = doctor(dir.path(), None);
    assert_eq!(v["recheck_pending"]["open"].as_i64(), Some(1),
               "一条未解决、没盖戳的告警 ⇒ 1：{v}");
}

/// P1-3：有 human 轮、一条章都没有 ⇒ 那个会话的 6c 是 fail-open，整库也是。
#[test]
fn a_human_session_without_a_claim_is_fail_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(0),
               "一个会话都没有 ⇒ 零填充：{v}");
    assert_eq!(v["principal"]["subject_gate"]["mode"].as_str(), Some("fail_open"),
               "一条有效章都没有 ⇒ 全局 fail-open：{v}");

    human_session(dir.path(), "ses-a", "dev-a");
    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(1),
               "这个会话的 6c 是 fail-open：{v}");
    assert_eq!(v["principal"]["subject_gate"]["mode"].as_str(), Some("fail_open"), "{v}");
    assert_eq!(v["principal"]["subject_gate"]["claim_sessions"].as_i64(), Some(0), "{v}");
    assert_eq!(v["principal"]["claims"].as_i64(), Some(0), "{v}");
}

/// P1-3：有有效章的会话 ⇒ 该会话不在 fail-open 里，闸门整体是 `strict`。
#[test]
fn a_valid_claim_makes_the_subject_gate_strict() {
    let dir = tempfile::tempdir().unwrap();
    human_session(dir.path(), "ses-a", "dev-a");
    claim(dir.path(), "ses-a", "dev-a", "acct-1", 5);

    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(0),
               "现行章说得出主人 ⇒ 不算 fail-open：{v}");
    assert_eq!(v["principal"]["subject_gate"]["mode"].as_str(), Some("strict"), "{v}");
    assert_eq!(v["principal"]["subject_gate"]["claim_sessions"].as_i64(), Some(1), "{v}");
}

/// P1-3 的第三臂：**现行章**才是判据。先盖有效章、后盖 `unknown` 的会话，6c 对它是
/// fail-open——只看"曾经有过一条有效章"的实现会把它漏掉，这条当场红。
#[test]
fn a_later_unknown_claim_makes_that_session_fail_open_again() {
    let dir = tempfile::tempdir().unwrap();
    human_session(dir.path(), "ses-a", "dev-a");
    human_session(dir.path(), "ses-b", "dev-b");
    claim(dir.path(), "ses-a", "dev-a", "acct-1", 5);
    claim(dir.path(), "ses-b", "dev-b", "acct-2", 6);
    // ses-b 的现行章：后来那次"答了不知道"。
    claim(dir.path(), "ses-b", "dev-b", "unknown", 7);

    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(1),
               "ses-b 的现行章是 unknown ⇒ 它是那一个 fail-open；ses-a 不是：{v}");
    assert_eq!(v["principal"]["subject_gate"]["mode"].as_str(), Some("mixed"),
               "有章但仍有一个会话解析不出主人 ⇒ mixed：{v}");
    assert_eq!(v["principal"]["subject_gate"]["claim_sessions"].as_i64(), Some(1), "{v}");
}

/// W20b（P1-3 的回落支）：没有章、但会话列填了主人的会话**不是** fail-open——
/// `resolve_principal` 回落到那一列，闸门拿得到 known。只查 claim 的实现会把它误记成
/// fail-open，这条当场红。
#[test]
fn a_session_column_owner_spares_the_session_from_fail_open() {
    let dir = tempfile::tempdir().unwrap();
    human_session(dir.path(), "ses-a", "dev-a");
    owner_column(dir.path(), "ses-a", "dev-a", "p1");

    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(0),
               "无章但回落列有主人 ⇒ 不算 fail-open：{v}");
    assert_eq!(v["principal"]["claims"].as_i64(), Some(0),
               "回落列不是章，claims 仍是 0：{v}");
    assert_eq!(v["principal"]["subject_gate"]["mode"].as_str(), Some("fail_open"),
               "一条章都没有 ⇒ 全局仍是 fail_open（这一格只读章）：{v}");
}

/// W20b：现行章是 `unknown` ⇒ 它**遮住**回落列。会话列填了主人也拿不到 known，
/// 闸门对这一格就是 fail-open。回落优先于章的实现在这条当场红。
#[test]
fn an_unknown_claim_shadows_the_session_column_owner() {
    let dir = tempfile::tempdir().unwrap();
    human_session(dir.path(), "ses-a", "dev-a");
    owner_column(dir.path(), "ses-a", "dev-a", "p1");
    claim(dir.path(), "ses-a", "dev-a", "unknown", 5);

    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(1),
               "unknown 章遮住回落列 ⇒ 这个会话仍算 fail-open：{v}");
}

/// W20b：三值里的 `mixed` 单独钉一条——有有效章（ses-a），另有一个无章无回落列的会话
/// （ses-b）⇒ claims>0 且 fail_open>0。
#[test]
fn the_subject_gate_mode_is_mixed_while_some_sessions_say_nothing() {
    let dir = tempfile::tempdir().unwrap();
    human_session(dir.path(), "ses-a", "dev-a");
    claim(dir.path(), "ses-a", "dev-a", "acct-1", 5);
    human_session(dir.path(), "ses-b", "dev-b");

    let v = doctor(dir.path(), None);
    assert_eq!(v["principal"]["fail_open_sessions"].as_i64(), Some(1), "{v}");
    assert_eq!(v["principal"]["subject_gate"]["mode"].as_str(), Some("mixed"), "{v}");
    assert_eq!(v["principal"]["subject_gate"]["claim_sessions"].as_i64(), Some(1), "{v}");
}

/// W15b2（审计 P1-1）：跨 kind 双锚的组数——恒在场、零填充，且同 kind 的重复不算。
///
/// 阳性的两对用生产的写库函数造（不同 kind ⇒ 不同 id）；"同 kind 同名同 scope"的两行
/// 内容寻址算不出两个 id，只有直插造得出来，负例那一半因此走一行 SQL。
#[test]
fn cross_kind_duplicate_groups_counts_pairs_of_two_kinds_only() {
    use rc_memory::writers_x::{put_entity, EntityContent};

    let dir = tempfile::tempdir().unwrap();
    {
        let _root = Root::open(dir.path()).unwrap();
    }
    let v = doctor(dir.path(), None);
    assert_eq!(
        v["cross_kind_duplicate_groups"].as_i64(),
        Some(0),
        "空库这一格也必须印出来，0 是读数：{v}"
    );

    let now = 1_700_000_000_000_i64;
    let prov = [Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }];
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let anchor = |kind: &str, canonical: &str, scope: &Scope| {
            put_entity(
                m,
                &EntityContent {
                    kind: kind.into(),
                    title: canonical.into(),
                    scope: scope.clone(),
                    aliases: vec![],
                },
                &prov,
                now,
            )
            .unwrap()
        };
        // 第一对：同一正名两种 kind。
        let proj_a = Scope { kind: ScopeKind::Project, key: "proj-a".into() };
        anchor("work", "老周", &proj_a);
        anchor("agent", "老周", &proj_a);
        // 第二对：另一个 scope、另一个正名，仍是两种 kind。
        let proj_b = Scope { kind: ScopeKind::Project, key: "proj-b".into() };
        anchor("work", "老李", &proj_b);
        anchor("person", "老李", &proj_b);
        // 负例：同 kind、同正名、同 scope 的两行——直插制造，两行都在库里。
        for (id, created) in [("anchor-dup-1", now - 2), ("anchor-dup-2", now - 1)] {
            m.conn()
                .execute(
                    "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind,
                                        scope_key, status, created_at_ms, updated_at_ms,
                                        norm_version)
                     VALUES(?1, 'work', '老王', 'real', 'project', 'proj-a', 'active', ?2, ?2, 1)",
                    (&id, created),
                )
                .unwrap();
        }
        let rows: i64 = m
            .conn()
            .query_row("SELECT count(*) FROM anchor WHERE canonical='老王'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2, "夹具前提：同 kind 的两行都真在库里");
    }
    let v = doctor(dir.path(), None);
    assert_eq!(
        v["cross_kind_duplicate_groups"].as_i64(),
        Some(2),
        "两对跨 kind 双锚 ⇒ 2；同 kind 的重复不算：{v}"
    );
}
