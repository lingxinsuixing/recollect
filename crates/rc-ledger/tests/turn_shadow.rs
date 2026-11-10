//! W38a 刀 A 的测试钉（设计 2026-11-08-w38a-transcript-search §五 + 工单 6）：
//! ① 闸红线（含变异证明的目标）；② 增量；③ 幂等；④ snippet；⑤ 空读数与闸拒可分型。
//! 另落刀 A 两个读数函数（session_turns / visible_sessions）的"同闸同语义"对拍。
//!
//! 夹具纪律：agent / principal / 会话名 / 正文全部自造（合成词），不引用任何 holdout 语料。

use rc_contract::{NormalizedTurn, SessionId, TurnTier};
use rc_ledger::{insert_batch, rebuild_turn_shadow};
use rc_memory::gate::Viewer;
use rc_search::transcript::{
    search_turns, session_turns, turn_window, visible_sessions, TranscriptSearchError, Trust,
    MAX_WINDOW_SIDE,
};
use rc_store::Root;

/// 会话甲/乙的命中词（自造合成词，两个会话共享，让 principal 臂成为唯一区分）。
const MARKER: &str = "quokka";
/// 会话丙（异 principal）独有的命中词。
const MARKER_OTHER: &str = "wallaby";
/// 哪里都没有的词（空读数的零对照）。
const ABSENT: &str = "ptarmigan";

fn root() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

fn session(key: &str, agent: &str, principal: &str) -> SessionId {
    SessionId {
        session_key: key.to_string(),
        device_id: "dev-1".to_string(),
        source_agent: agent.to_string(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: Some(principal.to_string()),
        principal_name: None,
        principal_source: None,
    }
}

fn turn(uid: &str, body: &str, created_ms: i64) -> NormalizedTurn {
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: uid.to_string(),
        body: body.to_string(),
        body_fp: format!("fp-{uid}"),
        role: "human".to_string(),
        created_at_ms: created_ms,
    }
}

fn put(root: &mut Root, key: &str, agent: &str, principal: &str, uid: &str, body: &str, created_ms: i64) {
    insert_batch(
        root.transcript_mut(),
        &session(key, agent, principal),
        &[turn(uid, body, created_ms)],
        created_ms,
    )
    .unwrap();
}

/// 把 turns 全表逐行渲染成一个串（幂等测试的"一字不动"判据）。
fn dump_turns(root: &Root) -> String {
    let mut st = root.transcript().conn().prepare(
        "SELECT turn_uid, session_key, device_id, role, body, body_fp, turn_key_tier,
                project_hint, created_at_ms, ingested_at_ms, commit_seq
         FROM turns ORDER BY turn_uid",
    )
    .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok(format!(
                "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, u8>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, i64>(10)?
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect::<Vec<_>>().join("\n")
}

// ── 钉①：闸红线（变异证明的目标，见最终报告） ─────────────────────────────

#[test]
fn cross_principal_session_is_invisible_to_search_and_the_gate_mutation_flips_this_test() {
    let (_d, mut root) = root();
    // 同 principal 的一对：alpha 的甲、beta 的乙；异 principal 的：beta 的丙。
    put(&mut root, "ses-same-a", "agent-alpha", "p-one", "t-a1", "alpha session keeps the quokka marker", 1000);
    put(&mut root, "ses-same-b", "agent-beta", "p-one", "t-b1", "beta session also keeps the quokka marker", 1001);
    put(&mut root, "ses-cross", "agent-beta", "p-two", "t-c1", "beta other keeps the quokka marker elsewhere", 1002);

    let hits_of = |root: &Root, v: &Viewer| -> Vec<String> {
        search_turns(root.transcript(), Trust::Ownership(v), MARKER, 20)
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.session_key)
            .collect()
    };
    let alpha = Viewer::agent("agent-alpha").with_principal("p-one");
    let seen = hits_of(&root, &alpha);
    assert!(seen.contains(&"ses-same-a".to_string()), "自己的会话必须可见：{seen:?}");
    assert!(
        seen.contains(&"ses-same-b".to_string()),
        "同 principal 互读（principal 判定臂保护的那一格）没成立：{seen:?}"
    );
    assert!(
        !seen.contains(&"ses-cross".to_string()),
        "红线：异 principal 的会话被搜到了：{seen:?}"
    );

    // 无 principal 的 viewer fail-closed：只见自己 agent 的会话。
    let seen = hits_of(&root, &Viewer::agent("agent-alpha"));
    assert_eq!(seen, vec!["ses-same-a".to_string()], "无 principal 必须 fail-closed：{seen:?}");

    // 人面 viewer 同判，不享受特权：principal 命中同 principal 的两个，吃不到丙。
    let human = Viewer::human("human-observer").with_principal("p-one");
    let seen = hits_of(&root, &human);
    assert!(seen.contains(&"ses-same-a".to_string()) && seen.contains(&"ses-same-b".to_string()), "{seen:?}");
    assert!(!seen.contains(&"ses-cross".to_string()), "人面 viewer 看到了异 principal 的会话：{seen:?}");

    // 对称半边：beta（p-one）看得见自己的乙与丙，也读得到 alpha 的甲（同 principal）。
    let beta = Viewer::agent("agent-beta").with_principal("p-one");
    assert_eq!(hits_of(&root, &beta).len(), 3, "{seen:?}");
}

// ── 钉②：增量 ─────────────────────────────────────────────────────────────

#[test]
fn insert_batch_is_searchable_immediately_without_rebuild() {
    let (_d, mut root) = root();
    put(&mut root, "ses-inc", "agent-alpha", "p-one", "t-inc",
        "the increment pin keeps the quokka marker visible", 1000);
    // 不调 rebuild_turn_shadow——影子行随 turns 行同事务落库，检索当场命中。
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");
    let out = search_turns(root.transcript(), Trust::Ownership(&viewer), MARKER, 10).unwrap();
    assert_eq!(out.hits.len(), 1, "增量写入后、重建之前就必须能搜到：{out:?}");
    assert_eq!(out.hits[0].turn_uid, "t-inc");
    assert_eq!(
        (out.coverage.indexed, out.coverage.total),
        (1, 1),
        "影子与权威面零缺口：{out:?}"
    );
    // 影子行本身：tokens 是索引尺子的输出（含命中词），row_len 是词数不是 0。
    let (tokens, row_len): (String, i64) = root
        .transcript()
        .conn()
        .query_row("SELECT tokens, row_len FROM turn_shadow WHERE turn_uid='t-inc'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(tokens.split(' ').any(|w| w == MARKER), "影子词串不含命中词：{tokens}");
    assert!(row_len >= 1, "row_len 必须是词数：{row_len}");
}

/// 引号逃逸语义的对拍（与 rc-index 的 `match_语法字符只是词面` 同族）：
/// `+` 之类 FTS 语法字符必须只是词面——用户字节永远只经 `?1` 绑定进 MATCH。
#[test]
fn match_syntax_characters_are_word_faces_not_operators() {
    let (_d, mut root) = root();
    put(&mut root, "ses-ops", "agent-alpha", "p-one", "t-ops",
        "build with c++ and the quokka marker", 1000);
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");
    let out = search_turns(root.transcript(), Trust::Ownership(&viewer), "c++", 10).unwrap();
    assert_eq!(out.hits.len(), 1, "语法字符被当成了 FTS 操作符：{out:?}");
}

// ── 钉③：幂等 ─────────────────────────────────────────────────────────────

#[test]
fn rebuild_twice_is_idempotent_and_turns_rows_are_untouched() {
    let (_d, mut root) = root();
    put(&mut root, "ses-idem", "agent-alpha", "p-one", "t-1", "first turn keeps the quokka marker", 1000);
    put(&mut root, "ses-idem", "agent-alpha", "p-one", "t-2", "second turn keeps the quokka marker too", 1001);

    let n1 = rebuild_turn_shadow(root.transcript()).unwrap();
    let before = dump_turns(&root);
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");
    let first = search_turns(root.transcript(), Trust::Ownership(&viewer), MARKER, 20).unwrap();

    let n2 = rebuild_turn_shadow(root.transcript()).unwrap();
    assert_eq!(n1, n2, "重建连跑两次读数漂了：{n1} vs {n2}");
    assert_eq!(dump_turns(&root), before, "重建动了 turns 行（权威面一字不许动）");
    let second = search_turns(root.transcript(), Trust::Ownership(&viewer), MARKER, 20).unwrap();
    assert_eq!(first, second, "重建前后检索读数不变");
    assert_eq!(second.coverage.indexed, second.coverage.total, "重建后零缺口：{second:?}");
    assert_eq!(second.coverage.total, 2);
}

// ── 钉④：snippet ──────────────────────────────────────────────────────────

#[test]
fn snippet_caps_at_240_chars_with_ellipsis_and_limit_one_returns_one_row() {
    let (_d, mut root) = root();
    let filler = "filler ".repeat(160); // 1120 字符
    let long_body = format!("{filler}{MARKER} {filler}");
    put(&mut root, "ses-snip", "agent-alpha", "p-one", "t-long", &long_body, 1000);
    put(&mut root, "ses-snip", "agent-alpha", "p-one", "t-second", "another quokka here", 1001);
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");

    // limit=1 只回一行，且如实报截断。
    let one = search_turns(root.transcript(), Trust::Ownership(&viewer), MARKER, 1).unwrap();
    assert_eq!(one.hits.len(), 1, "limit=1 必须只回一行：{:?}", one.hits.len());
    assert!(one.truncated, "命中不止一行却没报截断：{one:?}");

    // >240 字符的命中轮：snippet 截到硬上限、含省略号、以命中词为中心。
    let both = search_turns(root.transcript(), Trust::Ownership(&viewer), MARKER, 20).unwrap();
    assert_eq!(both.hits.len(), 2, "limit 上限之内应全回：{:?}", both.hits.len());
    assert!(!both.truncated, "limit=20 只有两条命中却报了截断：{both:?}");
    let long = both.hits.iter().find(|h| h.turn_uid == "t-long").expect("长正文命中在场");
    let chars = long.snippet.chars().count();
    assert!(chars <= 240, "snippet 超过硬上限：{chars}");
    assert!(long.snippet.contains('…'), "被截断的 snippet 必须有省略号：{}", long.snippet);
    assert!(long.snippet.contains(MARKER), "snippet 必须含命中词本身：{}", long.snippet);

    // 短正文不出省略号（未截断的对照）。
    let short = both.hits.iter().find(|h| h.turn_uid == "t-second").unwrap();
    assert!(!short.snippet.contains('…'), "短正文不该有省略号：{}", short.snippet);
}

// ── 钉⑤：空读数与闸拒可分型 ───────────────────────────────────────────────

#[test]
fn no_hits_and_gate_denied_are_both_ok_but_distinguishable() {
    let (_d, mut root) = root();
    put(&mut root, "ses-a", "agent-alpha", "p-one", "t-a", "alpha keeps the quokka marker", 1000);
    put(&mut root, "ses-c", "agent-beta", "p-two", "t-c", "beta keeps the wallaby marker", 1001);
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");

    // 闸拒形状：影子里有命中（wallaby 只在丙里），但全部关在闸外。合法读数，不是 Err。
    let denied = search_turns(root.transcript(), Trust::Ownership(&viewer), MARKER_OTHER, 10).unwrap();
    assert_eq!(denied.hits.len(), 0, "闸外的命中不许回来：{denied:?}");
    assert!(denied.matched > 0, "闸拒必须可分型：matched={}", denied.matched);

    // 空读数形状：索引里根本没有这个词。同为 Ok（isError=false 形状），matched == 0。
    let empty = search_turns(root.transcript(), Trust::Ownership(&viewer), ABSENT, 10).unwrap();
    assert_eq!(empty.hits.len(), 0);
    assert_eq!(empty.matched, 0, "空读数的 matched 必须是 0：{empty:?}");

    // 两个信封可分型（D32 教训）。
    assert_ne!(denied.matched, empty.matched);

    // 空查询词不是空读数：响亮的 NoQueryTokens（守卫 8 同一族）。
    let e = search_turns(root.transcript(), Trust::Ownership(&viewer), "的", 10).unwrap_err();
    assert!(matches!(e, TranscriptSearchError::NoQueryTokens), "{e:?}");
}

// ── 读数函数的闸对拍（session_turns / visible_sessions） ───────────────────

#[test]
fn session_turns_replays_only_visible_turns_and_honors_the_cursor() {
    let (_d, mut root) = root();
    put(&mut root, "ses-same-b", "agent-beta", "p-one", "t-b1", "first turn of beta", 1000);
    put(&mut root, "ses-same-b", "agent-beta", "p-one", "t-b2", "second turn of beta", 1001);
    put(&mut root, "ses-same-b", "agent-beta", "p-one", "t-b3", "fourth turn of beta", 1003);
    put(&mut root, "ses-cross", "agent-beta", "p-two", "t-c1", "third turn of beta", 1002);
    let alpha = Viewer::agent("agent-alpha").with_principal("p-one");

    // 同 principal 互读：回放乙会话，按 (created_at_ms, turn_uid) 升序。
    let replay = session_turns(root.transcript(), Trust::Ownership(&alpha), "ses-same-b", 200, None)
        .unwrap()
        .expect("可见会话必须回一个信封");
    // W39 刀 E：归属随原文走——信封带 device 与两列 owner（取自第一条可见轮的 sessions 行）。
    assert_eq!(replay.session_key, "ses-same-b");
    assert_eq!(replay.device_id.as_deref(), Some("dev-1"));
    assert_eq!(replay.owner_agent_id.as_deref(), Some("agent-beta"));
    assert_eq!(replay.owner_principal_id.as_deref(), Some("p-one"));
    let uids: Vec<&str> = replay.turns.iter().map(|r| r.turn_uid.as_str()).collect();
    assert_eq!(uids, vec!["t-b1", "t-b2", "t-b3"], "回放顺序漂了：{uids:?}");
    // 正文回放要带原文。
    assert_eq!(replay.turns[0].body, "first turn of beta");
    assert_eq!(replay.turns[0].role, "human");

    // 游标：t-b1 之后只剩两条。
    let replay = session_turns(root.transcript(), Trust::Ownership(&alpha), "ses-same-b", 200, Some("t-b1"))
        .unwrap()
        .expect("游标之后的页仍有可见轮");
    let uids: Vec<&str> = replay.turns.iter().map(|r| r.turn_uid.as_str()).collect();
    assert_eq!(uids, vec!["t-b2", "t-b3"], "游标之后应只剩后两条：{uids:?}");

    // limit 截断。
    let replay = session_turns(root.transcript(), Trust::Ownership(&alpha), "ses-same-b", 2, None)
        .unwrap()
        .expect("limit=2 的页仍有可见轮");
    assert_eq!(replay.turns.len(), 2, "limit=2 只许两行");

    // 不可见的会话 ⇒ None（与查无同形，不泄露存在性）。
    let replay = session_turns(root.transcript(), Trust::Ownership(&alpha), "ses-cross", 200, None).unwrap();
    assert!(replay.is_none(), "闸外的会话必须与查不到同形：{replay:?}");
}

#[test]
fn visible_sessions_lists_only_visible_sessions_with_counts_and_range() {
    let (_d, mut root) = root();
    put(&mut root, "ses-same-a", "agent-alpha", "p-one", "t-a1", "alpha one", 1000);
    put(&mut root, "ses-same-a", "agent-alpha", "p-one", "t-a2", "alpha two", 1005);
    put(&mut root, "ses-same-b", "agent-beta", "p-one", "t-b1", "beta one", 1001);
    put(&mut root, "ses-cross", "agent-beta", "p-two", "t-c1", "beta hidden", 1002);
    let alpha = Viewer::agent("agent-alpha").with_principal("p-one");

    let list = visible_sessions(root.transcript(), Trust::Ownership(&alpha)).unwrap();
    let keys: Vec<&str> = list.iter().map(|s| s.session_key.as_str()).collect();
    assert!(keys.contains(&"ses-same-a") && keys.contains(&"ses-same-b"), "{keys:?}");
    assert!(!keys.contains(&"ses-cross"), "闸外的会话不许出现在清单里：{keys:?}");
    let a = list.iter().find(|s| s.session_key == "ses-same-a").unwrap();
    assert_eq!(a.turns, 2, "轮数读数：{a:?}");
    assert_eq!((a.first_ms, a.last_ms), (Some(1000), Some(1005)), "时间范围读数：{a:?}");
    assert_eq!(a.source_agent, "agent-alpha");

    // 无 principal 的 viewer 只见自己 agent 的会话。
    let list = visible_sessions(root.transcript(), Trust::Ownership(&Viewer::agent("agent-alpha"))).unwrap();
    let keys: Vec<&str> = list.iter().map(|s| s.session_key.as_str()).collect();
    assert_eq!(keys, vec!["ses-same-a"], "fail-closed 的清单：{keys:?}");
}

// ── turn_window（W39 刀 A：按 turn_uid 取同会话窗口） ──────────────────────

#[test]
fn turn_window_returns_target_and_neighbours_in_order() {
    let (_d, mut root) = root();
    for i in 1..=5 {
        put(
            &mut root,
            "ses-win",
            "agent-alpha",
            "p-one",
            &format!("t-win-{i}"),
            &format!("window body {i}"),
            1000 + i,
        );
    }
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");

    let w = turn_window(root.transcript(), Trust::Ownership(&viewer), "t-win-3", 1, 1)
        .unwrap()
        .expect("可见会话里的轮必须取得到窗口");
    let uids: Vec<&str> = w.turns.iter().map(|r| r.turn_uid.as_str()).collect();
    assert_eq!(uids, vec!["t-win-2", "t-win-3", "t-win-4"], "窗口必须升序、目标在中间：{uids:?}");
    assert_eq!(w.turns[1].body, "window body 3");
    assert_eq!((w.session_key.as_str(), w.device_id.as_str()), ("ses-win", "dev-1"));
    assert_eq!(w.owner_agent_id.as_deref(), Some("agent-alpha"));

    // before=0 after=0 ⇒ 只有目标轮。
    let w = turn_window(root.transcript(), Trust::Ownership(&viewer), "t-win-3", 0, 0).unwrap().unwrap();
    let uids: Vec<&str> = w.turns.iter().map(|r| r.turn_uid.as_str()).collect();
    assert_eq!(uids, vec!["t-win-3"], "before=0 after=0 只回目标轮：{uids:?}");
}

#[test]
fn turn_window_caps_and_truncation_flags() {
    let (_d, mut root) = root();
    // 5 轮小会话：取中间轮、两侧各要上限（10），现有邻居不够 ⇒ 不报截断。
    for i in 1..=5 {
        put(
            &mut root,
            "ses-cap",
            "agent-alpha",
            "p-one",
            &format!("t-cap-{i}"),
            &format!("cap body {i}"),
            2000 + i,
        );
    }
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");
    let w = turn_window(root.transcript(), Trust::Ownership(&viewer), "t-cap-3", MAX_WINDOW_SIDE, MAX_WINDOW_SIDE)
        .unwrap()
        .unwrap();
    assert_eq!(w.turns.len(), 5, "邻居不够上限时全给：{w:?}");
    assert!(!w.truncated_before && !w.truncated_after, "会话只有 5 轮，两侧不该报截断：{w:?}");

    // 30 轮长会话：before=2 ⇒ 截断标 true、只回 2 条前邻居。
    for i in 1..=30 {
        put(
            &mut root,
            "ses-long",
            "agent-alpha",
            "p-one",
            &format!("t-long-{i:02}"),
            &format!("long body {i}"),
            3000 + i,
        );
    }
    let w = turn_window(root.transcript(), Trust::Ownership(&viewer), "t-long-15", 2, 0).unwrap().unwrap();
    let uids: Vec<&str> = w.turns.iter().map(|r| r.turn_uid.as_str()).collect();
    assert!(w.truncated_before, "更长会话里 before=2 必须报截断：{w:?}");
    assert_eq!(uids, vec!["t-long-13", "t-long-14", "t-long-15"], "只回 2 条前邻居：{uids:?}");
    assert!(w.truncated_after, "after=0 而后面还有轮，必须如实报后侧截断：{w:?}");

    // 请求超过上限 ⇒ 侧向收口到 MAX_WINDOW_SIDE。
    let w = turn_window(root.transcript(), Trust::Ownership(&viewer), "t-long-20", 100, 0).unwrap().unwrap();
    assert_eq!(w.turns.len(), MAX_WINDOW_SIDE + 1, "before 超过上限必须收口：{}", w.turns.len());
    assert!(w.truncated_before, "收口后仍要如实报截断：{w:?}");
}

#[test]
fn turn_window_is_invisible_across_principals_and_absent_is_the_same_shape() {
    let (_d, mut root) = root();
    put(&mut root, "ses-other", "agent-beta", "p-two", "t-other-1", "other principal body", 4000);
    put(&mut root, "ses-own", "agent-alpha", "p-one", "t-own-1", "own body", 4001);
    let alpha = Viewer::agent("agent-alpha").with_principal("p-one");

    // 异 principal 会话里的 uid ⇒ Ok(None)（与查无同形，不泄露存在性）。
    let hidden = turn_window(root.transcript(), Trust::Ownership(&alpha), "t-other-1", 1, 1).unwrap();
    assert!(hidden.is_none(), "异 principal 的轮必须与查无同形：{hidden:?}");

    // 不存在的 64 位 hex uid ⇒ 同一个 Ok(None)。
    let absent = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(absent.len(), 64);
    let missing = turn_window(root.transcript(), Trust::Ownership(&alpha), absent, 1, 1).unwrap();
    assert!(missing.is_none(), "不存在的轮必须是 Ok(None)：{missing:?}");

    // 同 agent 自读 ⇒ Some。
    let own = turn_window(root.transcript(), Trust::Ownership(&alpha), "t-own-1", 1, 1).unwrap();
    assert!(own.is_some(), "自己的会话必须取得到：{own:?}");
}

#[test]
fn turn_window_tie_on_created_at_orders_by_turn_uid() {
    let (_d, mut root) = root();
    // 同一时刻两轮：先后只由 turn_uid 决定。
    put(&mut root, "ses-tie", "agent-alpha", "p-one", "t-tie-a", "tie a", 5000);
    put(&mut root, "ses-tie", "agent-alpha", "p-one", "t-tie-b", "tie b", 5000);
    let viewer = Viewer::agent("agent-alpha").with_principal("p-one");
    let w = turn_window(root.transcript(), Trust::Ownership(&viewer), "t-tie-b", 1, 0).unwrap().unwrap();
    let uids: Vec<&str> = w.turns.iter().map(|r| r.turn_uid.as_str()).collect();
    assert_eq!(uids, vec!["t-tie-a", "t-tie-b"], "同 created_at_ms 时按 turn_uid 决定先后：{uids:?}");
}
