//! C2（`2026-10-28-duty-split-and-code-fixes` §2 C2）：time 闭集扩容 + `time_text` 接地 +
//! 解析失败可重跑。
//!
//! **每一条用例的短语与期望值都取自 `gold/time.jsonl`**（36 条真实短语判定）。参照钟一律是
//! 证据轮的 `created_at_ms`（+08:00），不是运行钟；期望毫秒由 gold 的 `expected.start_ms/
//! end_ms/precision/kind` 逐字抄来。
//!
//! 两条**故意不跟 gold**：
//! - `周五下班前`：gold 给了"18:00 假设"的值，但「下班」依赖个人作息 ⇒ 本实现留 unknown
//!   （派发档硬约束：依赖个人作息的不要猜）。
//! - 里程碑/整句（`H5 之前`、`H7c`、`先核…再进 H1c`）：正确输出是 unknown，且**不该建行**。
//!
//! 不跑 LLM、不碰 holdout。正反例只用通用占位名。

use rc_distill::pass_event_contract::parse_event_output;
use rc_distill::pass_fact_contract::parse_fact_output;
use rc_distill::pass_prompt::{FactCandidate, FactFamily};
use rc_distill::pass_time::{parse_time_text, run_time, select_time_turns, TIME_PIPELINE};
use rc_distill::{Batch, TurnRow};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{is_time_phrase, put_fact_rows, FactWriteItem};
use rc_memory::{compute_mem_id, AnchorRole, BatchCtx, MemTable, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};
use serde_json::{json, Value};

/// 2026-10-26T14:05:00+08:00（周一）：未标注的用例统一用它当参照钟。
const MON_1405: i64 = 1_792_994_700_000;
/// 2026-10-26T19:36:34.597+08:00：`20:00 前` / `20:00 一过` 的证据轮（gold ref 3）。
const MON_1936: i64 = 1_793_014_594_597;
/// 2026-10-26T16:49:15.692+08:00：`13:07` 的证据轮（gold 扩充样本）。
const MON_1649: i64 = 1_793_004_555_692;
/// 2026-10-26T15:33:20.200+08:00：`下周一上午十点前` 的证据轮（gold ref 1）。
const MON_1533: i64 = 1_793_000_000_200;
/// 2026-10-27T18:27:22.436+08:00：`接下来 5 天` 的证据轮。
const TUE_1827: i64 = 1_793_096_842_436;
/// 2026-10-26T20:00:00+08:00：多数用例的边界值。
const MON_2000: i64 = 1_793_016_000_000;

type Grid = (Option<i64>, Option<i64>, &'static str, &'static str, &'static str);

fn p(text: &str, turn_ms: i64, category: &str) -> Grid {
    let got = parse_time_text(text, turn_ms, category);
    (got.start_ms, got.end_ms, got.precision, got.basis, got.kind)
}

// ── ① ISO 带偏移（gold 3 条 wrong_value：text 与字段自相矛盾）──────────────

#[test]
fn iso_with_offset_is_an_instant_not_unknown() {
    // gold：2026-10-27T08:04+08:00 ⇒ [1793059440000, 1793059440000] hour / event_time。
    assert_eq!(
        p("2026-10-27T08:04+08:00", MON_1405, "event"),
        (Some(1_793_059_440_000), Some(1_793_059_440_000), "hour", "explicit", "event_time"),
    );
    assert_eq!(
        p("2026-10-27T08:37+08:00", MON_1405, "event"),
        (Some(1_793_061_420_000), Some(1_793_061_420_000), "hour", "explicit", "event_time"),
    );
    assert_eq!(
        p("2026-10-27T08:54+08:00", MON_1405, "event"),
        (Some(1_793_062_440_000), Some(1_793_062_440_000), "hour", "explicit", "event_time"),
    );
    // 秒与 `Z` 也收；`+0800` 紧凑写法同一结果。
    assert_eq!(
        p("2026-10-27T08:04:32+08:00", MON_1405, "event").0,
        Some(1_793_059_440_000 + 32_000),
    );
    assert_eq!(
        p("2026-10-27T00:04Z", MON_1405, "event").0,
        Some(1_793_059_440_000),
    );
    assert_eq!(
        p("2026-10-27T08:04+0800", MON_1405, "event").0,
        Some(1_793_059_440_000),
    );
}

// ── ② 裸时刻 + 前/后（gold 4 条 wrong_value）──────────────────────────────

#[test]
fn bare_clock_before_is_an_open_left_deadline() {
    // gold：`20:00 前` ⇒ deadline / hour / start=null / end=2026-10-26T20:00+08:00。
    let expected = (None, Some(MON_2000), "hour", "inferred", "deadline");
    assert_eq!(p("20:00 前", MON_1936, "promise"), expected);
    assert_eq!(p("20:00前", MON_1936, "promise"), expected, "空格不影响");
    // 不加"跨天猜测"：20:05 说的 `20:00 前` 仍落当日（gold 口径：方向词只决定 start/end）。
    assert_eq!(p("20:00 前", MON_2000 + 300_000, "promise").1, Some(MON_2000));
}

#[test]
fn bare_clock_after_is_an_open_right_validity() {
    // gold：`20:00 一过` / `20:00 额度重置后` ⇒ validity / hour / start=20:00 / end=null。
    let expected = (Some(MON_2000), None, "hour", "inferred", "validity");
    assert_eq!(p("20:00 一过", MON_1936, "promise"), expected);
    assert_eq!(p("20:00 额度重置后", MON_1936, "promise"), expected);
}

// ── ③ 现在/刚才（gold 7 条 wrong_value）───────────────────────────────────

#[test]
fn now_resolves_to_the_turn_clock() {
    for turn in [
        1_793_072_142_186_i64, // 2026-10-27T11:35:42.186
        1_793_025_720_979,     // 2026-10-26T22:42:00.979
        1_793_022_967_333,     // 2026-10-26T21:56:07.333
        1_793_076_179_682,     // 2026-10-27T12:42:59.682
    ] {
        assert_eq!(
            p("现在", turn, "other"),
            (Some(turn), Some(turn), "hour", "turn_ts", "speech_time"),
            "`现在` 的指称就是这一轮的时刻",
        );
    }
    // `刚才` 同一口径（gold 的 event 样本用它）。
    assert_eq!(p("刚才", MON_1405, "event").0, Some(MON_1405));
    assert_eq!(p("刚才", MON_1405, "event").4, "speech_time");
}

// ── ④ 下周X 上午/下午 H 点前（gold wrong_value）───────────────────────────

#[test]
fn next_weekday_morning_is_a_point_deadline() {
    // gold：`下周一上午十点前` ⇒ deadline / hour / [2026-11-02T10:00, 同一刻]。
    assert_eq!(
        p("下周一上午十点前", MON_1533, "promise"),
        (Some(1_793_584_800_000), Some(1_793_584_800_000), "hour", "inferred", "deadline"),
    );
    // 下午 + 阿拉伯数字 + 半：`下周三下午 3 点半`（不依赖 gold，只钉语法）。
    let wed = parse_time_text("下周三下午3点半", MON_1405, "promise");
    assert_eq!(wed.start_ms, Some(1_793_777_400_000), "2026-11-04T15:30+08:00");
    assert_eq!(wed.precision, "hour");
}

// ── ⑤ 裸钟点 = 当日那一刻（gold 扩充样本 3 条）────────────────────────────

#[test]
fn bare_clock_is_a_point_on_the_turn_day() {
    // gold：`13:07`（ref 16:49）⇒ 2026-10-26T13:07+08:00 —— 同日更早，不是"明天"。
    assert_eq!(
        p("13:07", MON_1649, "event"),
        (Some(1_792_991_220_000), Some(1_792_991_220_000), "hour", "inferred", "event_time"),
    );
    // gold：`今天 13:04` 同一天。
    assert_eq!(
        p("今天 13:04", MON_1649, "event"),
        (Some(1_792_991_040_000), Some(1_792_991_040_000), "hour", "inferred", "event_time"),
    );
    // gold：`今天20:00左右`（`左右` 不跨桶）。
    assert_eq!(
        p("今天20:00左右", MON_1936, "event"),
        (Some(MON_2000), Some(MON_2000), "hour", "inferred", "event_time"),
    );
}

// ── ⑤b 秒级钟面（T33）：`HH:MM:SS` 与 `HH:MM` 同一桶，秒进毫秒 ─────────────

#[test]
fn clock_with_seconds_is_a_point_on_the_turn_day() {
    // 现网两条 stale 的原文就是带秒的钟面（`08:59:42`）：2026-10-26T08:59:42+08:00。
    const EIGHT_5959_42: i64 = 1_792_976_382_000;
    assert_eq!(
        p("08:59:42", MON_1649, "event"),
        (Some(EIGHT_5959_42), Some(EIGHT_5959_42), "hour", "inferred", "event_time"),
    );
    // `13:55` 的行为一字不变；`13:55:30` 同桶、值多 30 秒（精度闭集里没有 minute）。
    assert_eq!(
        p("13:55", MON_1649, "event"),
        (Some(1_792_994_100_000), Some(1_792_994_100_000), "hour", "inferred", "event_time"),
    );
    assert_eq!(p("13:55:30", MON_1649, "event").0, Some(1_792_994_130_000));
    // 秒写 0 与两位写法是同一刻，不因多一节而漂移。
    assert_eq!(p("13:55:00", MON_1649, "event").0, Some(1_792_994_100_000));
    // 日词/星期前缀那条路（`parse_clock_whole`）也认秒。
    assert_eq!(p("今天 08:59:42", MON_1649, "event").0, Some(EIGHT_5959_42));
}

#[test]
fn malformed_clocks_are_not_guessed() {
    // 越界、缺位、坏格式、空串：一律 unknown/none，不截断、不补零。
    for text in ["25:00", "13:5", "13:55:60", "13:55:3", "13:55:", "", "  "] {
        assert_eq!(
            p(text, MON_1649, "event"),
            (None, None, "unknown", "none", "event_time"),
            "{text:?} 不在闭集里，正确输出就是 unknown",
        );
    }
}

// ── ⑥ 周期（gold 3 条 wrong_precision）──────────────────────────────────

#[test]
fn weekly_and_daily_are_recurrences() {
    assert_eq!(
        p("每周", MON_1405, "other"),
        (None, None, "week", "explicit", "recurrence"),
    );
    assert_eq!(p("每天", MON_1405, "other"), (None, None, "day", "explicit", "recurrence"));
    assert_eq!(p("每天定时", MON_1405, "other"), (None, None, "day", "explicit", "recurrence"));
}

// ── ⑦ 接下来 N 天（gold 扩充样本）───────────────────────────────────────

#[test]
fn next_n_days_spans_from_the_turn_clock() {
    assert_eq!(
        p("接下来 5 天", TUE_1827, "promise"),
        (
            Some(1_793_096_842_436),
            Some(1_793_528_842_436),
            "day",
            "inferred",
            "validity",
        ),
    );
}

// ── ⑧ 不该解析的一律 unknown，且不建行 ───────────────────────────────────

#[test]
fn milestones_and_sentence_fragments_stay_unknown() {
    // kind 取 `time_kind_for`（类别 + 截止词）的既有口径，与 gold 的 draft 逐条一致。
    let cases = [
        ("H5 之前", "deadline"),
        ("H7b 前", "validity"),
        ("H7c", "validity"),
        ("先核 commit 与新增用例，再进 H1c", "validity"),
        ("先核提交与关键读数，然后进 H2", "validity"),
    ];
    for (phrase, kind) in cases {
        assert_eq!(
            p(phrase, MON_1405, "promise"),
            (None, None, "unknown", "none", kind),
            "{phrase}：没有日历锚，正确输出就是 unknown",
        );
        assert!(!is_time_phrase(phrase), "{phrase} 不该产生 time_expr 行");
    }
}

#[test]
fn friday_off_work_is_not_guessed() {
    // 「下班」依赖个人作息：不猜 18:00（gold 的 18:00 是假设值，本实现不跟）。
    // kind 与 gold 的 draft 同形（`time_kind_for` 只认 `之前|以前|截止`，裸「前」不升级 kind）。
    assert_eq!(
        p("周五下班前", MON_1533, "promise"),
        (None, None, "unknown", "none", "validity"),
        "落 unknown：不猜下班时刻，也不猜工作日",
    );
    assert!(is_time_phrase("周五下班前"), "短语本身仍值得建一行（留给以后有作息表时）");
}

// ── ⑨ 写库层：H7c 不建 time_expr 行 ─────────────────────────────────────

const UID: &str = "0000000000000000000000000000000000000000000000000000000000000041";
/// 锚行的 canonical：INSERT 与句柄派生共用这一串（`person/张三/proj-time`）。
fn anchor() -> String {
    compute_mem_id(
        MemTable::Anchor,
        "person",
        "张三",
        "",
        &Scope { kind: ScopeKind::Project, key: "proj-time".into() },
    )
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person','张三','unknown',NULL,'project','proj-time','active',NULL,?2,?2,?3)",
            rusqlite::params![anchor(), MON_1405, rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, owner_principal_id,
                                  owner_agent_id, scope_kind, scope_key, visibility, retention,
                                  ownership_src)
             VALUES('sess-time','dev-time','agent-time','p-time','agent-owner','project',
                    'proj-time','project','keep','operator')",
            [],
        )
        .unwrap();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                               commit_seq)
             VALUES(?1,'sess-time','dev-time','human','轮一的源句','fp-time',1,NULL,?2,?2,0)",
            rusqlite::params![UID, MON_1405],
        )
        .unwrap();
    (dir, root)
}

fn write_fact(m: &MemoryHandle, category: &str, time_text: &str) {
    let it = FactWriteItem {
        subject_anchor_id: anchor(),
        object_anchor_id: None,
        predicate: "承诺".into(),
        value: Some("把对账单发出去".into()),
        category: category.into(),
        mode: "stated".into(),
        time_text: time_text.into(),
        quote: "我下周三之前把对账单发给你".into(),
        evidence: vec![(UID.into(), AnchorRole::Human)],
        text: format!("某正文：{category}/{time_text}"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-time".into() },
        source_ms: MON_1405,
    };
    let ctx = BatchCtx { source_agent: "agent-time".into(), device_id: "dev-time".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p-time".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    put_fact_rows(m, &[it], &ctx, &owner, MON_1405).unwrap();
}

fn time_expr_count(m: &MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM time_expr", [], |r| r.get(0)).unwrap()
}

#[test]
fn h7c_builds_no_time_expr_row() {
    let (_dir, root) = seeded();
    let m = root.memory();
    // `H7c` 曾经因为"含 ASCII 数字"被当成时间表达（gold 第 20 条：行本身就该删）。
    write_fact(m, "promise", "H7c");
    assert_eq!(time_expr_count(m), 0, "批次编号不是时间短语");
    // 里程碑引用同样不建行。
    write_fact(m, "promise", "H5 之前");
    write_fact(m, "promise", "H7b 前");
    assert_eq!(time_expr_count(m), 0);
    // 真的时间短语照旧建行。
    write_fact(m, "promise", "20:00 前");
    assert_eq!(time_expr_count(m), 1);
}

// ── ⑩ 终态：`basis='none'` 不再等价于 done，且解析器升版后可重跑 ──────────

fn pass_state(l: &LedgerHandle, uid: &str) -> (String, Option<String>) {
    l.conn()
        .query_row(
            "SELECT state, reason FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='time' AND family=''",
            [uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn unresolved_is_a_separate_terminal_state_and_reruns_after_a_parser_bump() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact(m, "promise", "那阵子");

    let out = run_time(m, l, root.transcript(), 0, MON_1405).unwrap();
    assert_eq!((out.turns, out.resolved, out.facts), (1, 0, 0));
    assert_eq!(out.state.slug(), "stale", "解析不出 ⇒ 不是 done");
    assert_eq!(
        pass_state(l, UID),
        ("stale".into(), Some("unresolved".into())),
        "轮级终态带 unresolved 理由",
    );
    // 重选键就是这条指纹：pipeline 段取当前 [`TIME_PIPELINE`]，model 段恒 `program`，
    // 两个 prompt 段为空。bump 版本 ⇒ 指纹变 ⇒ `reselectable` 放行（下面手动改指纹是同一件事）。
    let fingerprint: String = l.conn()
        .query_row(
            "SELECT fingerprint FROM extract_pass_state
             WHERE turn_uid=?1 AND pass_id='time' AND family=''",
            [UID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        fingerprint,
        rc_ledger::water::extraction_fingerprint(TIME_PIPELINE, "program", "", ""),
        "stale 行的指纹必须由 TIME_PIPELINE 现算，否则 bump 常量不会触发重选",
    );

    // 同一版解析器：不重选（重跑只会得到同一个结论）。
    assert!(
        select_time_turns(m, l, 0, MON_1405 + 10_000).unwrap().is_empty(),
        "同指纹的 stale(unresolved) 不再选中",
    );

    // 模拟解析器升版（指纹变了）⇒ 这一轮重新进入选批：这就是"升级后回填旧 none"的路径。
    l.conn()
        .execute(
            "UPDATE extract_pass_state SET fingerprint='pass-time-old'
             WHERE turn_uid=?1 AND pass_id='time' AND family=''",
            [UID],
        )
        .unwrap();
    assert_eq!(
        select_time_turns(m, l, 0, MON_1405 + 20_000).unwrap(),
        vec![UID.to_string()],
        "解析器版本一变，旧的 unresolved 轮即可重跑",
    );
}

// ── ⑩b card.time_json：写卡时留空，由 time 回填（T33）────────────────────

fn card_time_json(m: &MemoryHandle, text: &str) -> Option<String> {
    m.conn()
        .query_row("SELECT time_json FROM card WHERE text=?1", [text], |r| r.get(0))
        .unwrap()
}

#[test]
fn card_time_json_is_written_when_the_time_resolves() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact(m, "state_change", "08:59:42");
    let text = "某正文：state_change/08:59:42";
    assert_eq!(
        card_time_json(m, text),
        None,
        "写卡时刻还没有解析结果：卡面时间留空，不拿源轮时刻冒充",
    );

    let out = run_time(m, l, root.transcript(), 0, MON_1405).unwrap();
    assert_eq!((out.resolved, out.facts), (1, 1));
    let got = card_time_json(m, text).expect("解析落地后卡面时间必须非空");
    let value: Value = serde_json::from_str(&got).expect("time_json 必须是可解析的 JSON");
    assert_eq!(value["kind"], "event_time");
    assert_eq!(value["field"], "occurred", "卡面时间要说清它镜像的是哪一组 fact 列");
    assert_eq!(value["start_ms"].as_i64(), Some(1_792_976_382_000));
    assert_eq!(value["end_ms"].as_i64(), Some(1_792_976_382_000));
    assert_eq!(value["precision"], "hour");
    assert_eq!(value["basis"], "inferred");

    // 幂等：第二次跑批没有可解析的行 ⇒ 值一个字节都不变，也不多卡、不多时间表达。
    let again = run_time(m, l, root.transcript(), 0, MON_1405 + 1_000).unwrap();
    assert_eq!(again.resolved, 0, "已解析的行不再进 apply");
    assert_eq!(card_time_json(m, text), Some(got));
    let (cards, exprs): (i64, i64) = m.conn()
        .query_row(
            "SELECT (SELECT count(*) FROM card), (SELECT count(*) FROM time_expr)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((cards, exprs), (1, 1), "重跑不重复写行");
}

#[test]
fn unresolved_time_leaves_card_time_json_null() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact(m, "promise", "那阵子");

    let out = run_time(m, l, root.transcript(), 0, MON_1405).unwrap();
    assert_eq!((out.resolved, out.facts), (0, 0));
    assert_eq!(
        card_time_json(m, "某正文：promise/那阵子"),
        None,
        "解析不出的短语不许在卡面上编一个时间",
    );
}

#[test]
fn card_time_json_follows_the_column_group_it_mirrors() {
    // 三条回填 SQL 各一条（event 那支见上一条用例）：`field` 说明卡片镜像的是哪一组 fact 列，
    // 两端可以单缺——`20:00 前` 只有右端（due），`20:00 一过` 只有左端（valid_from）。
    for (phrase, field, start, end) in [
        ("20:00 前", "due", None, Some(MON_2000)),
        ("20:00 一过", "valid", Some(MON_2000), None),
    ] {
        let (_dir, root) = seeded();
        let (m, l) = (root.memory(), root.ledger());
        write_fact(m, "promise", phrase);
        run_time(m, l, root.transcript(), 0, MON_1405).unwrap();
        let raw = card_time_json(m, &format!("某正文：promise/{phrase}"))
            .unwrap_or_else(|| panic!("{phrase} 解析成功，卡面时间必须非空"));
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["field"], field, "{phrase}");
        assert_eq!(value["start_ms"].as_i64(), start, "{phrase}");
        assert_eq!(value["end_ms"].as_i64(), end, "{phrase}");
    }
}

#[test]
fn a_resolved_turn_is_still_done() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact(m, "promise", "20:00 前");

    let out = run_time(m, l, root.transcript(), 0, MON_1405).unwrap();
    assert_eq!((out.turns, out.resolved), (1, 1));
    assert_eq!(out.state.slug(), "done");
    assert_eq!(pass_state(l, UID), ("done".into(), None));
    let (start, end, precision, basis, kind): (
        Option<i64>,
        Option<i64>,
        String,
        String,
        String,
    ) = m.conn()
        .query_row(
            "SELECT start_ms, end_ms, precision, basis, kind FROM time_expr WHERE text='20:00 前'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (start, end, precision.as_str(), basis.as_str(), kind.as_str()),
        (None, Some(MON_2000), "hour", "inferred", "deadline"),
    );
}

// ── ⑪ time_text 接地（fact / event 两个契约各一条正例 + 一条反例）────────

fn fact_batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-fact".into(),
        device_id: "dev-fact".into(),
        source_agent: "agent-fact".into(),
        project_hint: None,
        rows: bodies
            .iter()
            .enumerate()
            .map(|(index, body)| TurnRow {
                turn_uid: format!("{:064x}", index + 1),
                role: "human".into(),
                body: (*body).to_string(),
                created_at_ms: MON_1405 + index as i64 * 60_000,
                ingested_at_ms: MON_1405 + index as i64 * 60_000,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

fn candidates() -> Vec<FactCandidate> {
    vec![
        FactCandidate {
            reference: "a1".into(),
            kind: "person".into(),
            canonical: "张三".into(),
            anchor_id: format!("{:064x}", 0xa1),
            mark: None,
        },
        FactCandidate {
            reference: "a2".into(),
            kind: "person".into(),
            canonical: "李四".into(),
            anchor_id: format!("{:064x}", 0xa2),
            mark: None,
        },
    ]
}

fn fact_reply(time_text: &str) -> String {
    json!({
        "schema_version": 0,
        "pass": "fact",
        "family": "promise",
        "facts": [{
            "ref": "f1",
            "turn": "t1",
            "subject": "a1",
            "predicate": "promise",
            "object": "a2",
            "value": "把对账单发给你",
            "category": "promise",
            "mode": "stated",
            "time_text": time_text,
            "quote": "我下周三之前把对账单发给你",
            "evidence": ["t1"],
            "text": "张三承诺在下周三之前把对账单发给李四。"
        }],
        "empty_turns": []
    })
    .to_string()
}

#[test]
fn fact_time_text_must_be_grounded() {
    let b = fact_batch(&["我下周三之前把对账单发给你"]);
    let ok = parse_fact_output(&fact_reply("下周三之前"), &b, &candidates(), FactFamily::Promise)
        .expect("正文里有这个短语 ⇒ 收");
    assert_eq!(ok[0].time_text, "下周三之前");

    // C2：接地判定在闸（`gate.rs` 第 9 步，`time_text_ungrounded`）；契约层只做形状校验。
    let lenient =
        parse_fact_output(&fact_reply("明天上午"), &b, &candidates(), FactFamily::Promise)
            .expect("契约不判接地");
    assert_eq!(lenient[0].time_text, "明天上午");
}

fn event_batch(bodies: &[&str]) -> Batch {
    Batch {
        session_key: "sess-event".into(),
        device_id: "dev-event".into(),
        source_agent: "agent-event".into(),
        project_hint: None,
        rows: bodies
            .iter()
            .enumerate()
            .map(|(index, body)| TurnRow {
                turn_uid: format!("{:064x}", index + 1),
                role: "human".into(),
                body: (*body).to_string(),
                created_at_ms: MON_1405 + index as i64 * 60_000,
                ingested_at_ms: MON_1405 + index as i64 * 60_000,
                speaker: None,
                speaker_anchor_id: None,
            })
            .collect(),
    }
}

fn event_reply(time_text: &str, quote: &str) -> String {
    json!({
        "schema_version": 0,
        "pass": "event",
        "events": [{
            "ref": "e1",
            "turn": "t1",
            "category": "state_change",
            "actor": "a1",
            "trigger": "改回去",
            "action": "restore_config",
            "object": null,
            "object_text": "配置文件",
            "participants": ["a1"],
            "place": null,
            "result": "done",
            "mode": "stated",
            "time_text": time_text,
            "quote": quote,
            "evidence": ["t1"],
            "text": "张三把配置改回旧版本，服务恢复。"
        }],
        "empty_turns": []
    })
    .to_string()
}

#[test]
fn event_time_text_must_be_grounded() {
    let b = event_batch(&["昨晚我把配置改回去，服务就恢复了"]);
    let ok = parse_event_output(
        &event_reply("昨晚", "昨晚我把配置改回去，服务就恢复了"),
        &b,
        &candidates(),
    )
    .expect("时间落在引文里 ⇒ 收");
    assert_eq!(ok[0].time_text, "昨晚");

    // §7.5 切片一a：改前这一支是"契约不判接地、原样收下"，现在由契约硬拒。
    // 期望变更的理由与新旧值单列在台账 §10.60：引文外的「刚才」没有出处，
    // 与 fact 的 `value ⊂ quote` 是同一条原则。
    let err = parse_event_output(
        &event_reply("刚才", "我把配置改回去，服务就恢复了"),
        &b,
        &candidates(),
    );
    assert_eq!(err, Err(rc_distill::pass_event_contract::EventPassError::TimeNotInQuote("e1".into())));
}

/// 一个 Value 不是给编译器看的：它在这里只是防"用例被裁剪后 helper 变成死代码"。
#[allow(dead_code)]
fn _shape_guard(_: Value) {}
