use rc_contract::{compute_body_fp, compute_turn_uid, normalize_body, NormalizedTurn, SessionId, TurnTier};
use rc_contract::{normalize, parse, ExchangeFile};
use rc_ledger::transcript::{count_between, turn_origin};
use rc_ledger::{insert_batch, log_import};
use rc_store::Root;
use serde_json::json;

fn file(one_body: &str) -> ExchangeFile {
    parse(
        &serde_json::to_vec(&json!({
            "format_version": 1,
            "sessions": [{"session_key": "ses-A", "device_id": "dev-1", "source_agent": "qoder",
                          "turns": [{"role": "human", "body": one_body, "created_at_ms": 1730000000000i64}]}]
        }))
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn same_file_twice_is_idempotent_by_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let rep = normalize(file("先跑测试"), 1);
    let a = insert_batch(root.transcript_mut(), &rep.accepted[0].session, std::slice::from_ref(&rep.accepted[0].turn), 10).unwrap();
    let b = insert_batch(root.transcript_mut(), &rep.accepted[0].session, std::slice::from_ref(&rep.accepted[0].turn), 20).unwrap();
    assert_eq!((a.inserted, a.skipped_duplicate), (1, 0));
    assert_eq!((b.inserted, b.skipped_duplicate), (0, 1), "重复导入由同一机制吸收，不是错误");
    assert_eq!(rc_ledger::count_turns_on(root.transcript()), 1);
}

#[test]
fn sessions_row_is_upserted_with_last_saved_at() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let rep = normalize(file("甲"), 1);
    insert_batch(root.transcript_mut(), &rep.accepted[0].session, std::slice::from_ref(&rep.accepted[0].turn), 10).unwrap();
    let n: i64 = rc_ledger::count_sessions(root.transcript());
    assert_eq!(n, 1);
}

#[test]
fn import_log_absent_after_crash_makes_reimport_safe() {
    // 崩溃窗口：turns 已写、import_log 未写 ⇒ 重跑不报错、不产生第二行
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let rep = normalize(file("乙"), 1);
    insert_batch(root.transcript_mut(), &rep.accepted[0].session, std::slice::from_ref(&rep.accepted[0].turn), 10).unwrap();
    log_import(root.ledger_mut(), 1, 2, Some("dev-1"), &"aa".repeat(32), &rep).unwrap();
    insert_batch(root.transcript_mut(), &rep.accepted[0].session, std::slice::from_ref(&rep.accepted[0].turn), 30).unwrap();
    assert_eq!(rc_ledger::count_import_rows(root.ledger()), 1);
    assert_eq!(rc_ledger::count_turns_on(root.transcript()), 1, "重跑不落第二行正文");
}

/// 守卫 12 的账侧那一半（R4）：`import_log` 的每一列只许是计数/时间戳/设备号/哈希/档位码。
/// 做法是放一个**唯一的合成正文标记**走完摄入路径，再逐列渲染 `import_log` 的全部内容，
/// 断言标记不在任何一列里出现。同一条扫描先证明自己能读到合法值（`dev-1`），
/// 否则"什么都没读到"也会被当成"没有泄漏"。
#[test]
fn import_log_columns_carry_no_body_marker() {
    const MARKER: &str = "SYNTH-MARKER-勿入日志-8f2c";
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let f: ExchangeFile = parse(
        &serde_json::to_vec(&json!({
            "format_version": 1,
            "sessions": [
                {"session_key": "ses-A", "device_id": "dev-1", "source_agent": "qoder",
                 "turns": [{"role": "human", "body": MARKER, "created_at_ms": 1730000000000i64}]},
                {"session_key": "ses-B", "device_id": "dev-1", "source_agent": "qoder",
                 "turns": [{"role": "human", "body": "正文乙", "created_at_ms": 1730000000001i64},
                           {"role": "wizard", "body": "正文丙", "created_at_ms": 1730000000002i64}]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let rep = normalize(f, 1);
    // 前置条件：两行被接受（含带标记的那一行）、一行逐条拒收、整批没被拒。
    assert_eq!((rep.accepted.len(), rep.rejects.len()), (2, 1));
    assert_eq!(rep.batch_reject, None);
    for acc in &rep.accepted {
        insert_batch(
            root.transcript_mut(),
            &acc.session,
            std::slice::from_ref(&acc.turn),
            10,
        )
        .unwrap();
    }
    log_import(root.ledger_mut(), 1, 2, Some("dev-1"), &"bb".repeat(32), &rep).unwrap();

    // 标记确实进过这条路径：它在转录侧的正文列里（正文的家），所以金丝雀不是空跑。
    let body: String = root
        .transcript()
        .conn()
        .query_row(
            "SELECT body FROM turns WHERE session_key='ses-A'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body, MARKER);

    // 逐列渲染账侧的每一张表的每一列（守卫 2 的思路：清单现算，不抄名字）。
    let conn = root.ledger().conn();
    let mut dump = String::new();
    for table in ["import_log", "usage_counter"] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for (i, name) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = row.get(i).unwrap();
                dump.push_str(&format!("{table}.{name}={v:?}|"));
            }
        }
    }
    assert!(dump.contains("dev-1"), "金丝雀失效：账侧连合法值都没读到 {dump}");
    assert!(!dump.contains(MARKER), "正文泄漏到账侧 {dump}");
    assert!(!dump.contains("wizard"), "拒收 detail 的输入值泄漏到账侧 {dump}");
}

// ── 任务 10（P2 读缝的两个转录侧读口）：手工造轮的夹具 + 五条新用例 ──────────────

/// 手工造一轮：`insert_batch` 的入参形状与 `mcp` 的 `append_agent_turn` 完全一致，
/// 走契约而不是走文件解析，为的是**控制 `created_at_ms`**（区间开闭是本文件的判据）。
fn one(device: &str, session: &str, body: &str, at: i64) -> NormalizedTurn {
    let body = normalize_body(body);
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: compute_turn_uid(device, session, "human", &body, at),
        body_fp: compute_body_fp(&body),
        body,
        role: "human".into(),
        created_at_ms: at,
    }
}
fn sid(device: &str, session: &str) -> SessionId {
    SessionId { session_key: session.into(), device_id: device.into(), source_agent: "qoder".into(),
                machine: None, project_hint: None, last_saved_at_ms: Some(1),
                owner_principal_id: None, principal_name: None, principal_source: None }
}
fn put(root: &mut Root, device: &str, session: &str, body: &str, at: i64) -> String {
    let n = one(device, session, body, at);
    let uid = n.turn_uid.clone();
    insert_batch(root.transcript(), &sid(device, session), &[n], at).unwrap();
    uid
}

#[test]
fn count_between_is_left_open_right_open() {
    // P-44 的区间含义整个压在这一条上：`after` 是游标指向的那一条（**已蒸过**，不算排队），
    // `before` 是问的那一条本身（**也不算**）。实现里出现任何一个 `+1`/`-1` 就是有人在猜边界。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    for i in 0..5 {
        put(&mut root, "dev-1", "ses-A", &format!("第{i}条"), 100 + i);
    }
    let t = root.transcript();
    assert_eq!(count_between(t, "ses-A", 100, 104).unwrap(), 3, "两端都不含：101..=103");
    assert_eq!(count_between(t, "ses-A", 100, 101).unwrap(), 0, "相邻两端之间没有行");
    assert_eq!(count_between(t, "ses-A", 104, 100).unwrap(), 0, "倒区间给 0，不给 panic 也不给负数");
}

#[test]
fn count_between_does_not_cross_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    for i in 0..3 { put(&mut root, "dev-1", "ses-A", &format!("A{i}"), 100 + i); }
    for i in 0..4 { put(&mut root, "dev-1", "ses-B", &format!("B{i}"), 100 + i); }
    assert_eq!(count_between(root.transcript(), "ses-A", 99, 200).unwrap(), 3,
               "跨会话串了 ⇒ 队列位置是两台设备之和");
}

#[test]
fn count_between_on_an_empty_range_is_zero_not_none() {
    // "库里没有排队"与"读不到库"必须分得开（分型族那条），所以这里的返回类型是 `u64` 不是 `Option`。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(count_between(root.transcript(), "ses-none", 0, i64::MAX).unwrap(), 0);
}

#[test]
fn turn_origin_returns_the_session_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uid = put(&mut root, "dev-9", "ses-W", "定位用的一条", 1234);
    assert_eq!(
        turn_origin(root.transcript(), &uid).unwrap().as_ref().map(|o| (o.session_key.as_str(), o.created_at_ms)),
        Some(("ses-W", 1234_i64)),
        "会话键与时间是 `eta_turns` 的两个端点，少一个就算不出队列位置"
    );
}

#[test]
fn turn_origin_on_a_ghost_uid_is_ok_none() {
    // **`None` 是读数**（P-62 的前提）。这里不许写成 `.ok()`：那把 SQL 错误也折成"查无此轮"，
    // 于是"磁盘坏了"被报成"你那句话不存在"——正是 §11 D5 点名的那一族。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(turn_origin(root.transcript(), &"f".repeat(64)).unwrap(), None);
    assert!(turn_origin(root.transcript(), "not-a-uid").unwrap().is_none(), "形状错的输入也是查无，不是错误");
}

// ── S1b-C1：会话归属的 7 列（新行保守默认 + 重传不清空） ─────────────────────

type Ownership = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// 按 `TRANSCRIPT_SQL` 的列序读一行的 7 个归属列。
fn ownership(root: &Root, session: &str, device: &str) -> Ownership {
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, scope_kind, scope_key,
                    visibility, retention, ownership_src
             FROM sessions WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![session, device],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap()
}

fn owned(
    principal: Option<&str>,
    agent: &str,
    kind: &str,
    key: &str,
    visibility: &str,
    retention: &str,
    src: &str,
) -> Ownership {
    (
        principal.map(str::to_string),
        Some(agent.to_string()),
        Some(kind.to_string()),
        Some(key.to_string()),
        Some(visibility.to_string()),
        Some(retention.to_string()),
        Some(src.to_string()),
    )
}

/// 新行只写保守默认：owner 是产出者自己、private、default；`scope_key` 与
/// `Batch::scope_key()` 同口径（`project_hint` 优先，缺省退回会话键）。
#[test]
fn a_new_session_carries_the_conservative_ownership_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let mut with_hint = sid("dev-1", "ses-A");
    with_hint.project_hint = Some("proj-1".into());
    let n = one("dev-1", "ses-A", "带 hint 的一条", 10);
    insert_batch(root.transcript_mut(), &with_hint, &[n], 10).unwrap();
    let n = one("dev-1", "ses-B", "不带 hint 的一条", 20);
    insert_batch(root.transcript_mut(), &sid("dev-1", "ses-B"), &[n], 20).unwrap();

    assert_eq!(
        ownership(&root, "ses-A", "dev-1"),
        owned(None, "qoder", "project", "proj-1", "private_agent", "default", "default"),
        "project_hint 在场时 scope_key 必须取它"
    );
    assert_eq!(
        ownership(&root, "ses-B", "dev-1"),
        owned(None, "qoder", "project", "ses-B", "private_agent", "default", "default"),
        "没有 project_hint 时 scope_key 退回会话键"
    );
    // owner_principal_id 是 NULL（S1b 不登记自然人），不是空串。
    let principal: Option<String> = root
        .transcript()
        .conn()
        .query_row("SELECT owner_principal_id FROM sessions WHERE session_key='ses-A'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(principal, None, "未登记自然人是 NULL，不是空串");
}

/// 重传不清空已归属行：`source_agent`/`project_hint` 变了，已写的归属不动，
/// `last_saved_at_ms` 仍取 max（旧行为不许被归属改造带坏）。
#[test]
fn reingesting_a_session_keeps_its_ownership_and_takes_the_later_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let mut first = sid("dev-1", "ses-A");
    first.project_hint = Some("proj-old".into());
    first.last_saved_at_ms = Some(100);
    let n = one("dev-1", "ses-A", "第一次", 10);
    insert_batch(root.transcript_mut(), &first, &[n], 10).unwrap();
    let before = ownership(&root, "ses-A", "dev-1");
    assert_eq!(
        before,
        owned(None, "qoder", "project", "proj-old", "private_agent", "default", "default")
    );

    let mut second = sid("dev-1", "ses-A");
    second.source_agent = "other-agent".into();
    second.project_hint = Some("proj-new".into());
    second.last_saved_at_ms = Some(50); // 更早：max 必须留住 100
    let n = one("dev-1", "ses-A", "第二次", 20);
    insert_batch(root.transcript_mut(), &second, &[n], 20).unwrap();

    assert_eq!(
        ownership(&root, "ses-A", "dev-1"),
        before,
        "重传改了归属列（owner/scope 被后来的默认值顶掉）"
    );
    let last_saved: i64 = root
        .transcript()
        .conn()
        .query_row(
            "SELECT last_saved_at_ms FROM sessions WHERE session_key='ses-A'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last_saved, 100, "last_saved_at_ms 必须取 max");
    assert_eq!(rc_ledger::count_sessions(root.transcript()), 1, "重传造出了第二行会话");
}

/// 已由运维/将来的 v3 适配器写下的非默认归属，重传时同样不许被默认值覆盖：
/// 冲突分支逐列 `COALESCE(旧, 新)`，旧值非空就赢。
#[test]
fn an_explicitly_owned_session_survives_reingestion() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let mut first = sid("dev-1", "ses-A");
    first.project_hint = Some("proj-old".into());
    let n = one("dev-1", "ses-A", "第一次", 10);
    insert_batch(root.transcript_mut(), &first, &[n], 10).unwrap();
    let explicit = owned(Some("p-1"), "agent-owner", "person", "person:k", "person", "keep", "operator");
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p-1', owner_agent_id='agent-owner',
                    scope_kind='person', scope_key='person:k', visibility='person',
                    retention='keep', ownership_src='operator'
             WHERE session_key='ses-A'",
            [],
        )
        .unwrap();

    let mut second = sid("dev-1", "ses-A");
    second.source_agent = "other-agent".into();
    second.project_hint = Some("proj-new".into());
    let n = one("dev-1", "ses-A", "第二次", 20);
    insert_batch(root.transcript_mut(), &second, &[n], 20).unwrap();
    assert_eq!(ownership(&root, "ses-A", "dev-1"), explicit, "重传覆盖了显式归属");
}

#[test]
fn count_turns_counts_rows_across_all_sessions() {
    // 落后那一格的被减数。它答的是"库里一共有几条轮"，所以**不许**按会话筛（那是 `count_between`
    // 的活，P2 用的那条），也不许返回 `Option`：零行是 0，不是"没有读数"（P-44 那一族）。
    // 返回 `i64` 而 `count_between` 返回 `u64`：这一支要和 `count_states` 的 `i64` 相减，
    // 而那一支的读者是队列位置。两种宽度各有各的家，不许为了"看起来一致"去改其中一边。
    let dir = tempfile::tempdir().unwrap();
    let mut root = rc_store::Root::open(dir.path()).unwrap();
    assert_eq!(rc_ledger::transcript::count_turns(root.transcript()).unwrap(), 0);
    let sid = rc_contract::SessionId {
        session_key: "ses-A".into(),
        device_id: "dev-synthetic".into(),
        source_agent: "qoder-test".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: Some(10),
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    // 两个会话各一条：跨会话不串这件事由"总数是 2"来表达，没有别的断言可写。
    for (i, key) in ["ses-A", "ses-B"].iter().enumerate() {
        let body = format!("落后面夹具第 {} 条", i);
        let turn = rc_contract::NormalizedTurn {
            tier: rc_contract::TurnTier::ContentOnly,
            turn_uid: rc_contract::compute_turn_uid("dev-synthetic", key, "human", &body, 10 + i as i64),
            body_fp: rc_contract::compute_body_fp(&body),
            body,
            role: "human".into(),
            created_at_ms: 10 + i as i64,
        };
        let mut s = sid.clone();
        s.session_key = (*key).to_string();
        rc_ledger::insert_batch(root.transcript_mut(), &s, std::slice::from_ref(&turn), 10).unwrap();
    }
    assert_eq!(rc_ledger::transcript::count_turns(root.transcript()).unwrap(), 2,
               "总数读不出 2 ⇒ 被减数错了，落后那一格会少报");
}
