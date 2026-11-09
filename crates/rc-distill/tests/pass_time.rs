//! S2d-1：时间 pass 的确定性半（设计 `2026-10-26-s2-prompt-v0` §2.4、§7）。
//!
//! 两块各钉一件事：
//!
//! 1. [`parse_time_text`]：v0 闭集里的短语必须归一成**确定的毫秒区间 + 精度 + 依据 + 类别**，
//!    闭集外/非法/空 ⇒ `start/end=None, precision=unknown, basis=none`，**不猜**。
//!    本地日历与时区固定 +08:00（`rc_agent::recall::TZ_OFFSET_MIN`），存储仍是 UTC 毫秒。
//! 2. [`resolve_pending_time_expr`]：只扫 `basis='none'` 的行；解析出 start 才更新该行并回填
//!    `fact` 的对应列（deadline→due、event_time→occurred、validity→valid 区间）；
//!    未解析的行原地不动（下一轮还会扫到），已填过的列不覆盖，重跑第二遍 facts=0。
//!
//! **不跑真实 LLM、不碰 holdout。** 正反例只用通用占位名（张三/李四/某项目/某机房）。

use rc_distill::pass_time::{parse_time_text, resolve_pending_time_expr, run_time, select_time_turns};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::{AnchorRole, BatchCtx, Scope, ScopeKind};
use rc_store::{LedgerHandle, MemoryHandle, Root};

/// 2026-10-26T14:05:00+08:00 = 2026-10-26T06:05:00Z（周一）。所有相对短语以它为准。
const TURN: i64 = 1_792_994_700_000;

/// 解析结果的五格，省去在每条用例里重复写字段名。
type Grid = (Option<i64>, Option<i64>, &'static str, &'static str, &'static str);

fn p(text: &str, category: &str) -> Grid {
    let got = parse_time_text(text, TURN, category);
    (got.start_ms, got.end_ms, got.precision, got.basis, got.kind)
}

// ── ① 绝对日期/时间 ⇒ basis=explicit ─────────────────────────────────

#[test]
fn iso_date_is_the_whole_local_day() {
    // 2026-11-05（周四）00:00 .. 23:59:59.999 +08:00
    assert_eq!(
        p("2026-11-05", "promise"),
        (Some(1_793_808_000_000), Some(1_793_894_399_999), "day", "explicit", "validity"),
    );
}

#[test]
fn iso_date_with_time_is_an_hour() {
    // 18:30 .. 18:30:59.999：带时分的精度是 hour，不是 day。
    let expected = (Some(1_793_874_600_000), Some(1_793_878_199_999), "hour", "explicit", "validity");
    assert_eq!(p("2026-11-05 18:30", "promise"), expected);
    assert_eq!(p("2026-11-05T18:30", "promise"), expected, "T 分隔符同一条路");
}

#[test]
fn chinese_year_month_day_is_a_day() {
    assert_eq!(
        p("2027年3月1日", "promise"),
        (Some(1_803_830_400_000), Some(1_803_916_799_999), "day", "explicit", "validity"),
    );
}

#[test]
fn month_day_uses_the_turn_local_year() {
    // 短语只给月日 ⇒ 年份取 turn 的本地年（2026），不是"下一个 3 月 1 日"。
    assert_eq!(
        p("3月1日", "promise"),
        (Some(1_772_294_400_000), Some(1_772_380_799_999), "day", "explicit", "validity"),
    );
    // 1 月的轮上写 12 月 31 日同样是"当年"（12-31 在 1-01 之前，也不回滚到上一年）。
    let jan = parse_time_text("12月31日", 1_767_240_000_000 /* 2026-01-01T12:00+08 */, "promise");
    assert_eq!(jan.start_ms, Some(1_798_646_400_000) /* 2026-12-31 00:00 +08 */);
}

// ── ② 以轮为参照的日/晚/月 ────────────────────────────────────────────

#[test]
fn relative_days_shift_from_the_turn_local_day() {
    // 今天 = 10-26；昨天 10-25；明天 10-27；前天 10-24；后天 10-28（均为本地日 00:00 起）。
    assert_eq!(
        p("今天", "promise"),
        (Some(1_792_944_000_000), Some(1_793_030_399_999), "day", "turn_ts", "validity"),
    );
    assert_eq!(
        p("今天", "other"),
        (Some(1_792_944_000_000), Some(1_793_030_399_999), "day", "turn_ts", "validity"),
    );
    assert_eq!(
        p("昨天", "promise"),
        (Some(1_792_857_600_000), Some(1_792_943_999_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("明天", "promise"),
        (Some(1_793_030_400_000), Some(1_793_116_799_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("前天", "promise"),
        (Some(1_792_771_200_000), Some(1_792_857_599_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("后天", "promise"),
        (Some(1_793_116_800_000), Some(1_793_203_199_999), "day", "inferred", "validity"),
    );
}

#[test]
fn evenings_are_twenty_hundred_local() {
    // 昨晚 = 10-25 20:00（精度 hour）；今晚/明晚同一口径。
    assert_eq!(
        p("昨晚", "event"),
        (Some(1_792_929_600_000), Some(1_792_933_199_999), "hour", "inferred", "event_time"),
    );
    assert_eq!(
        p("今晚", "other"),
        (Some(1_793_016_000_000), Some(1_793_019_599_999), "hour", "turn_ts", "validity"),
    );
    assert_eq!(
        p("明晚", "other"),
        (Some(1_793_102_400_000), Some(1_793_105_999_999), "hour", "inferred", "validity"),
    );
}

#[test]
fn months_are_whole_local_months() {
    // 本月 = 2026-10-01 00:00 .. 10-31 23:59:59.999；下个月 = 2026-11 整月。
    assert_eq!(
        p("本月", "promise"),
        (Some(1_790_784_000_000), Some(1_793_462_399_999), "month", "turn_ts", "validity"),
    );
    assert_eq!(
        p("这个月", "promise"),
        (Some(1_790_784_000_000), Some(1_793_462_399_999), "month", "turn_ts", "validity"),
    );
    assert_eq!(
        p("下个月", "promise"),
        (Some(1_793_462_400_000), Some(1_796_054_399_999), "month", "inferred", "validity"),
    );
}

// ── ③ 周（周一为一周之首，用本地日历）────────────────────────────────

#[test]
fn weekday_uses_the_local_week() {
    // 10-26 是周一：本周一 = 10-26，本周日 = 11-01，上周一 = 10-19。
    assert_eq!(
        p("周一", "other"),
        (Some(1_792_944_000_000), Some(1_793_030_399_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("本周日", "other"),
        (Some(1_793_462_400_000), Some(1_793_548_799_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("上周一", "other"),
        (Some(1_792_339_200_000), Some(1_792_425_599_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("星期天", "other"),
        (Some(1_793_462_400_000), Some(1_793_548_799_999), "day", "inferred", "validity"),
        "星期天 = 周日 = 本周第七天",
    );
}

#[test]
fn next_week_weekday_before_is_a_deadline_ending_that_day() {
    // 下周三 = 2026-11-04；`之前` ⇒ kind=deadline、end=当日 23:59:59.999。
    assert_eq!(
        p("下周三之前", "promise"),
        (Some(1_793_721_600_000), Some(1_793_807_999_999), "day", "inferred", "deadline"),
    );
    // 同一短语在 event 类别下：修饰词优先 ⇒ 仍是 deadline（不是 event_time）。
    assert_eq!(
        p("下周三之前", "event"),
        (Some(1_793_721_600_000), Some(1_793_807_999_999), "day", "inferred", "deadline"),
    );
}

// ── ④ 偏移与修饰 ─────────────────────────────────────────────────────

#[test]
fn day_and_hour_offsets_are_inferred() {
    // 3 天后 = 10-29 整天；2 小时前 = 轮时刻 - 2h（精度 hour）。
    assert_eq!(
        p("3天后", "promise"),
        (Some(1_793_203_200_000), Some(1_793_289_599_999), "day", "inferred", "validity"),
    );
    assert_eq!(
        p("2小时前", "promise"),
        (Some(1_792_987_500_000), Some(1_792_991_099_999), "hour", "inferred", "validity"),
    );
    // `N天之前` / `N天之后` 是偏移，不是 before/after 修饰：**区间**按偏移算（3 天前 = 10-23
    // 整天、3 天后 = 10-29 整天）。kind 仍过 `time_kind_for` 同一把尺子，所以"3天之前"在
    // 承诺类上带上了 `之前` 这个截止词 ⇒ deadline（这是写库层与解析层共用一把尺子的代价，
    // 已登记在交付报告里；区间本身没有被它影响）。
    assert_eq!(
        p("3天之前", "promise"),
        (Some(1_792_684_800_000), Some(1_792_771_199_999), "day", "inferred", "deadline"),
    );
    assert_eq!(
        p("3天之后", "promise"),
        (Some(1_793_203_200_000), Some(1_793_289_599_999), "day", "inferred", "validity"),
    );
}

#[test]
fn before_makes_a_deadline_and_after_opens_a_validity() {
    // 截止/以前/之前 ⇒ deadline，end=当日末；起/以后 ⇒ validity，end 开口。
    assert_eq!(
        p("2026-10-01 之前", "promise"),
        (Some(1_790_784_000_000), Some(1_790_870_399_999), "day", "explicit", "deadline"),
    );
    assert_eq!(
        p("下月起", "promise"),
        (Some(1_793_462_400_000), None, "month", "inferred", "validity"),
    );
}

// ── ⑤ 本地日跨越 UTC 日界 ─────────────────────────────────────────────

#[test]
fn local_day_wins_over_the_utc_day() {
    // 2026-10-26T17:00Z = 2026-10-27 01:00 +08。UTC 日期还是 26 号，本地已是 27 号。
    let late = 1_793_034_000_000;
    let today = parse_time_text("今天", late, "promise");
    assert_eq!(today.start_ms, Some(1_793_030_400_000), "本地 10-27 的 00:00");
    assert_eq!(today.end_ms, Some(1_793_116_799_999));

    // 2026-10-26T23:30+08（UTC 仍是 26 号 15:30）：明天只比轮时刻晚 1.8 秒。
    let night = 1_793_028_600_000;
    let tomorrow = parse_time_text("明天", night, "promise");
    assert_eq!(tomorrow.start_ms, Some(1_793_030_400_000));
    assert!(tomorrow.start_ms.unwrap() - night == 1_800_000, "跨日只差 30 分钟");
}

// ── ⑥ 不猜 ───────────────────────────────────────────────────────────

#[test]
fn unknown_phrases_and_empty_text_resolve_to_nothing() {
    for text in ["那阵子", "最近", "改天", "回头再说", "今天明天", "2026-02-30",
                 "2026-13-01", "2026-10-32", "", "   ", "\r\n"] {
        let got = p(text, "promise");
        assert_eq!(
            got,
            (None, None, "unknown", "none", "validity"),
            "短语 {text:?} 解析不出 ⇒ 三格留空、precision=unknown、basis=none，不猜",
        );
    }
    // 失败时 kind 仍按类别取：事件类不因为解析失败退化成 validity。
    assert_eq!(p("那阵子", "event"), (None, None, "unknown", "none", "event_time"));
    // 失败时若短语里有截止词，kind 保留 deadline 的意图（时间两格仍是空）。
    assert_eq!(p("那阵子截止", "promise"), (None, None, "unknown", "none", "deadline"));
}

#[test]
fn every_category_defaults_to_its_own_kind_when_there_is_no_modifier() {
    let cases = [
        ("下周三", "promise", "validity"),
        ("下周三", "plan", "validity"),
        ("下周三", "decision", "validity"),
        ("下周三", "event", "event_time"),
        ("下周三", "state_change", "event_time"),
        ("下周三", "observation", "event_time"),
        ("下周三", "other", "validity"),
        ("下周三截止", "plan", "deadline"),
    ];
    for (text, category, kind) in cases {
        assert_eq!(p(text, category).4, kind, "{category} / {text} 的 kind");
    }
}

// ── ⑦ 回填（resolve_pending_time_expr）────────────────────────────────

const UID: &str = "0000000000000000000000000000000000000000000000000000000000000021";
const ANCHOR: &str = "00000000000000000000000000000000000000000000000000000000000000a1";

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
            rusqlite::params![ANCHOR, TURN, rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
    // S3a-3c：runner 现在要把解析结果过待审队列（程序闸查转录用 origin/会话/墓碑），
    // 所以夹具补一条会话与两行源轮。时间 pass 不消费正文，body 只是占位。
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
    for (uid, body) in [(UID, "轮一的源句"), (UID2, "轮二的源句")] {
        root.transcript()
            .conn()
            .execute(
                "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                                   turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                                   commit_seq)
                 VALUES(?1,'sess-time','dev-time','human',?2,?3,1,NULL,?4,?4,0)",
                rusqlite::params![uid, body, format!("fp-{uid}"), TURN],
            )
            .unwrap();
    }
    (dir, root)
}

/// 一条 fact（time_text 由调用方给）。写库层同时落一条未解析的 `time_expr`。
fn write_fact(m: &MemoryHandle, category: &str, time_text: &str) {
    let text = format!("某正文：{category}/{time_text}");
    write_fact_on(m, UID, category, time_text, &text);
}

/// 同一形状，但证据轮与正文由调用方给：S2d-2 的 runner 用例要按轮区分行。
fn write_fact_on(m: &MemoryHandle, uid: &str, category: &str, time_text: &str, text: &str) {
    let it = FactWriteItem {
        subject_anchor_id: ANCHOR.into(),
        object_anchor_id: None,
        predicate: "承诺".into(),
        value: Some("把对账单发出去".into()),
        category: category.into(),
        mode: "stated".into(),
        time_text: time_text.into(),
        quote: "我下周三之前把对账单发给你".into(),
        evidence: vec![(uid.into(), AnchorRole::Human)],
        text: text.into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-time".into() },
        source_ms: TURN,
    };
    let ctx = BatchCtx { source_agent: "agent-time".into(), device_id: "dev-time".into() };
    let owner = OwnerCtx {
        owner_principal_id: Some("p-time".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    };
    put_fact_rows(m, &[it], &ctx, &owner, TURN).unwrap();
}

fn fact_col(m: &MemoryHandle, category: &str, time_text: &str, column: &str) -> Option<i64> {
    let text = format!("某正文：{category}/{time_text}");
    m.conn()
        .query_row(&format!("SELECT {column} FROM fact WHERE text=?1"), [text], |r| r.get(0))
        .unwrap()
}

fn time_row(m: &MemoryHandle, text: &str) -> (Option<i64>, Option<i64>, String, String, String) {
    m.conn()
        .query_row(
            "SELECT start_ms, end_ms, precision, basis, kind FROM time_expr WHERE text=?1",
            [text],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
}

#[test]
fn deadline_backfills_due_at() {
    let (_dir, root) = seeded();
    let m = root.memory();
    write_fact(m, "promise", "下周三之前");

    let report = resolve_pending_time_expr(m, TURN + 1_000).unwrap();
    assert_eq!((report.expressions, report.resolved, report.facts), (1, 1, 1));

    let row = time_row(m, "下周三之前");
    assert_eq!(
        row,
        (Some(1_793_721_600_000), Some(1_793_807_999_999),
         "day".into(), "inferred".into(), "deadline".into()),
    );
    assert_eq!(
        fact_col(m, "promise", "下周三之前", "due_at_ms"),
        Some(1_793_807_999_999),
        "deadline 回填 due_at_ms = end",
    );
    // 其余时间列不动。
    for column in ["occurred_at_ms", "occurred_end_ms", "valid_from_ms", "valid_to_ms"] {
        assert_eq!(fact_col(m, "promise", "下周三之前", column), None, "{column} 不该被写");
    }
}

#[test]
fn event_time_backfills_occurred() {
    let (_dir, root) = seeded();
    let m = root.memory();
    write_fact(m, "event", "昨晚");

    let report = resolve_pending_time_expr(m, TURN).unwrap();
    assert_eq!((report.expressions, report.resolved, report.facts), (1, 1, 1));
    assert_eq!(time_row(m, "昨晚").4, "event_time");
    assert_eq!(fact_col(m, "event", "昨晚", "occurred_at_ms"), Some(1_792_929_600_000));
    assert_eq!(fact_col(m, "event", "昨晚", "occurred_end_ms"), Some(1_792_933_199_999));
    assert_eq!(fact_col(m, "event", "昨晚", "due_at_ms"), None);
}

#[test]
fn validity_backfills_the_open_interval() {
    let (_dir, root) = seeded();
    let m = root.memory();
    write_fact(m, "promise", "下月起");

    let report = resolve_pending_time_expr(m, TURN).unwrap();
    assert_eq!((report.expressions, report.resolved, report.facts), (1, 1, 1));
    assert_eq!(
        fact_col(m, "promise", "下月起", "valid_from_ms"),
        Some(1_793_462_400_000),
        "validity 回填区间左端",
    );
    assert_eq!(
        fact_col(m, "promise", "下月起", "valid_to_ms"),
        None,
        "`起` 是开口区间：右端仍是 NULL（不是 0，也不是假装解析）",
    );
}

#[test]
fn unknown_phrase_leaves_the_row_and_the_fact_alone() {
    let (_dir, root) = seeded();
    let m = root.memory();
    write_fact(m, "promise", "那阵子");

    let report = resolve_pending_time_expr(m, TURN).unwrap();
    assert_eq!(
        (report.expressions, report.resolved, report.facts),
        (1, 0, 0),
        "扫到一行但没解析出 ⇒ 一行不改",
    );
    assert_eq!(
        time_row(m, "那阵子"),
        (None, None, "unknown".into(), "none".into(), "validity".into()),
        "未解析行原地不动（下一轮还会扫到）",
    );
    for column in ["occurred_at_ms", "occurred_end_ms", "valid_from_ms", "valid_to_ms",
                   "due_at_ms"] {
        assert_eq!(fact_col(m, "promise", "那阵子", column), None, "{column} 不该被写");
    }
}

#[test]
fn already_filled_columns_are_not_overwritten() {
    let (_dir, root) = seeded();
    let m = root.memory();
    write_fact(m, "promise", "下周三之前");
    m.conn()
        .execute("UPDATE fact SET due_at_ms=42 WHERE text=?1", ["某正文：promise/下周三之前"])
        .unwrap();

    let report = resolve_pending_time_expr(m, TURN).unwrap();
    assert_eq!((report.expressions, report.resolved, report.facts), (1, 1, 0),
               "句子仍被解析（resolved=1），但没有 fact 行需要改");
    assert_eq!(fact_col(m, "promise", "下周三之前", "due_at_ms"), Some(42), "先写的值不被覆盖");
}

#[test]
fn one_kind_only_touches_its_own_categories() {
    let (_dir, root) = seeded();
    let m = root.memory();
    // 同一轮、同一短语：promise（deadline）与 event（event_time 的意图）各一条 fact。
    write_fact(m, "promise", "下周三之前");
    write_fact(m, "event", "下周三之前");

    let report = resolve_pending_time_expr(m, TURN).unwrap();
    assert_eq!(report.expressions, 2, "两条 fact 各带一条时间行（kind 不同 ⇒ id 不同）");
    assert_eq!(report.resolved, 2);
    assert_eq!(report.facts, 1, "只有 promise 那一行被 deadline 回填");
    assert_eq!(fact_col(m, "promise", "下周三之前", "due_at_ms"), Some(1_793_807_999_999));
    assert_eq!(
        fact_col(m, "event", "下周三之前", "due_at_ms"),
        None,
        "deadline 只回填 promise/plan/decision",
    );
    assert_eq!(
        fact_col(m, "event", "下周三之前", "occurred_at_ms"),
        None,
        "截止短语不是事件发生时刻，不回填 occurred",
    );
}

#[test]
fn second_run_is_a_noop() {
    let (_dir, root) = seeded();
    let m = root.memory();
    write_fact(m, "promise", "下周三之前");

    let first = resolve_pending_time_expr(m, TURN).unwrap();
    assert_eq!((first.expressions, first.resolved, first.facts), (1, 1, 1));
    let second = resolve_pending_time_expr(m, TURN + 60_000).unwrap();
    assert_eq!(
        (second.expressions, second.resolved, second.facts),
        (0, 0, 0),
        "已解析的行不再是 basis='none'，第二遍扫不到也改不到",
    );

    // 未解析的行第二遍仍在扫描面上（不是"扫不到"而是"扫到但不改"）。
    write_fact(m, "promise", "那阵子");
    let third = resolve_pending_time_expr(m, TURN + 120_000).unwrap();
    assert_eq!((third.expressions, third.resolved, third.facts), (1, 0, 0));
}

// ── ⑧ runner（select_time_turns / run_time）────────────────────────────
//
// S2d-2 把 S2d-1 的两块接成一条 pass：选批只认 `(turn, 'time', '')` 与
// `time_expr.basis='none'`，回填委托给 `resolve_pending_time_expr`，终态由程序写。
// 这里钉五件事：可解析的轮被填并 done、未知短语仍 none 但轮也 done（不猜）、
// 重跑空批、未知不拖垮同批、pending 到期才重选。

const UID2: &str = "0000000000000000000000000000000000000000000000000000000000000022";

/// 一轮的时间 pass 状态：`(state, reason)`，键是 `(turn, 'time', '')`。
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

/// 按行正文取 fact 的某一列（`write_fact_on` 的行不是按短语唯一）。
fn fact_col_of(m: &MemoryHandle, text: &str, column: &str) -> Option<i64> {
    m.conn()
        .query_row(&format!("SELECT {column} FROM fact WHERE text=?1"), [text], |r| r.get(0))
        .unwrap()
}

#[test]
fn run_time_fills_a_deadline_and_an_event_then_marks_both_done() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    // 轮 1：承诺（`下周三之前` ⇒ deadline ⇒ due_at_ms）；
    // 轮 2：事件（`昨晚` ⇒ event_time ⇒ occurred/occurred_end）。
    write_fact_on(m, UID, "promise", "下周三之前", "轮一：下周三之前发对账单");
    write_fact_on(m, UID2, "event", "昨晚", "轮二：昨晚断电");

    let out = run_time(m, l, root.transcript(), 0, TURN + 1_000).unwrap();
    assert_eq!(
        (out.turns, out.expressions, out.resolved, out.facts),
        (2, 2, 2, 2),
        "两轮、两条未解析行、两条解析成功、两条 fact 被回填",
    );
    assert_eq!(out.state.slug(), "done");

    assert_eq!(
        fact_col_of(m, "轮一：下周三之前发对账单", "due_at_ms"),
        Some(1_793_807_999_999),
        "deadline 回填 due_at_ms = 当日末",
    );
    assert_eq!(
        fact_col_of(m, "轮二：昨晚断电", "occurred_at_ms"),
        Some(1_792_929_600_000),
        "event_time 回填 occurred_at_ms = 昨晚 20:00",
    );
    assert_eq!(fact_col_of(m, "轮二：昨晚断电", "occurred_end_ms"), Some(1_792_933_199_999));

    let row = time_row(m, "下周三之前");
    assert_eq!(
        (row.3.as_str(), row.4.as_str()),
        ("inferred", "deadline"),
        "time_expr 的 basis/kind 已被回填",
    );
    assert_eq!(time_row(m, "昨晚").3, "inferred");

    assert_eq!(pass_state(l, UID), ("done".into(), None));
    assert_eq!(pass_state(l, UID2), ("done".into(), None));
}

#[test]
fn an_unknown_phrase_stays_none_and_the_turn_is_stale() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact(m, "promise", "改天");

    let out = run_time(m, l, root.transcript(), 0, TURN).unwrap();
    assert_eq!(
        (out.turns, out.expressions, out.resolved, out.facts),
        (1, 1, 0, 0),
        "扫到一轮，解析不出，零回填",
    );
    // C2：`basis='none'` 不再与 done 等价——"已处理、结论未知"是 `stale(unresolved)`，
    // 解析器升版后会被重新选中（`tests/time_closed.rs` 钉了这条重跑路径）。
    assert_eq!(out.state.slug(), "stale", "解析不出 ⇒ 不是 done");
    assert_eq!(
        time_row(m, "改天"),
        (None, None, "unknown".into(), "none".into(), "validity".into()),
        "不猜：行原地不动（basis 仍是 none）",
    );
    for column in ["due_at_ms", "occurred_at_ms", "occurred_end_ms",
                   "valid_from_ms", "valid_to_ms"] {
        assert_eq!(fact_col(m, "promise", "改天", column), None, "{column} 不该被写");
    }
    assert_eq!(
        pass_state(l, UID),
        ("stale".into(), Some("unresolved".into())),
        "轮级终态带 unresolved 理由",
    );
}

#[test]
fn a_second_run_selects_nothing_and_writes_nothing() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact(m, "promise", "下周三之前");

    let first = run_time(m, l, root.transcript(), 0, TURN).unwrap();
    assert_eq!((first.turns, first.facts), (1, 1));

    assert!(select_time_turns(m, l, 0, TURN + 10_000).unwrap().is_empty(),
            "已解析的行不再是 basis='none' ⇒ 不再选中");
    let second = run_time(m, l, root.transcript(), 0, TURN + 10_000).unwrap();
    assert_eq!(
        (second.turns, second.expressions, second.resolved, second.facts),
        (0, 0, 0, 0),
        "空批：一行不写",
    );
    assert_eq!(second.state.slug(), "empty");
    assert_eq!(fact_col(m, "promise", "下周三之前", "due_at_ms"), Some(1_793_807_999_999));
}

#[test]
fn an_unknown_turn_does_not_fail_the_batch_with_a_resolvable_one() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    write_fact_on(m, UID, "promise", "下周三之前", "可解析轮");
    write_fact_on(m, UID2, "promise", "改天", "未知轮");

    // limit=1 时只选一轮（顺序按 turn_uid ⇒ 先 21 号轮）。
    assert_eq!(select_time_turns(m, l, 1, TURN).unwrap(), vec![UID.to_string()]);
    assert_eq!(select_time_turns(m, l, 0, TURN).unwrap().len(), 2);

    let out = run_time(m, l, root.transcript(), 0, TURN).unwrap();
    assert_eq!(
        (out.turns, out.expressions, out.resolved, out.facts),
        (2, 2, 1, 1),
        "一轮可解析、一轮未知：整批仍成功",
    );
    assert_eq!(pass_state(l, UID), ("done".into(), None));
    assert_eq!(
        pass_state(l, UID2),
        ("stale".into(), Some("unresolved".into())),
        "未知轮落 stale(unresolved)，不是 done",
    );
    assert_eq!(time_row(m, "改天").3, "none");
}

#[test]
fn a_pending_row_is_reselected_only_when_it_is_due() {
    let (_dir, root) = seeded();
    let (m, l) = (root.memory(), root.ledger());
    // 未知短语 ⇒ 回填后 time_expr 仍是 basis='none'，重选判据只剩状态行这一条。
    write_fact(m, "promise", "改天");
    let out = run_time(m, l, root.transcript(), 0, TURN).unwrap();
    assert_eq!(out.state.slug(), "stale");
    assert!(select_time_turns(m, l, 0, TURN + 10_000).unwrap().is_empty(),
            "同指纹的 stale(unresolved) 不重选");

    // 手工改成 pending + 未到期 ⇒ 不选；到期（<= now）⇒ 选。
    let set_pending = |due: Option<i64>| {
        l.conn()
            .execute(
                "UPDATE extract_pass_state SET state='pending', next_retry_at_ms=?2
                 WHERE turn_uid=?1 AND pass_id='time' AND family=''",
                rusqlite::params![UID, due],
            )
            .unwrap();
    };
    set_pending(Some(TURN + 60_000));
    assert!(select_time_turns(m, l, 0, TURN + 59_999).unwrap().is_empty(), "未到期不选");
    assert_eq!(select_time_turns(m, l, 0, TURN + 60_000).unwrap(), vec![UID.to_string()]);
    set_pending(None);
    assert_eq!(select_time_turns(m, l, 0, TURN).unwrap(), vec![UID.to_string()],
               "pending 无到期时间 ⇒ 可跑");
}
