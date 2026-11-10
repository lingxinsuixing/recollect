use rc_ledger::water::{self, Cursor, ExtractState};
use rc_ledger::pass_state::{self, PassKey, PassState};
use rc_store::{LedgerHandle, Root, TranscriptHandle};

fn cursor(ms: i64) -> Cursor {
    Cursor { at_ms: ms, turn_uid: format!("{:04}{}", ms, "b".repeat(60)) }
}

/// 新读数那两条用例的夹具。`Root::open` 是这两个句柄的**唯一**合法构造路（守卫 4），
/// 而句柄是 `Root` 借出去的，所以这里让两个薄壳 `Rc` 共享同一份 `Root`，并把 `TempDir`
/// 挂在旁边保活——否则夹具在建完库之后就把目录删了。
struct Fresh {
    _dir: tempfile::TempDir,
    root: Root,
}

/// `fresh_ledger` / `fresh_pair` 的第一返回值。`Deref` 到账本句柄，所以用例里那一行
/// `water::halted_scopes_count(&l)` 与 `water::advance(&l, …)` 逐字就是真句柄的形状。
struct LRef(std::rc::Rc<Fresh>);
impl std::ops::Deref for LRef {
    type Target = LedgerHandle;
    fn deref(&self) -> &LedgerHandle { self.0.root.ledger() }
}

/// `fresh_pair` 的第二返回值（转录侧）。
struct TRef(std::rc::Rc<Fresh>);
impl std::ops::Deref for TRef {
    type Target = TranscriptHandle;
    fn deref(&self) -> &TranscriptHandle { self.0.root.transcript() }
}

fn fresh() -> std::rc::Rc<Fresh> {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    std::rc::Rc::new(Fresh { _dir: dir, root })
}

fn fresh_ledger() -> LRef {
    LRef(fresh())
}

/// 转录与账本**两个独立连接**（守卫 6 那个形状的夹具）：返回同一份 `Root` 的两个壳。
fn fresh_pair() -> (LRef, TRef) {
    let p = fresh();
    (LRef(p.clone()), TRef(p))
}

#[test]
fn absent_cursor_reads_none_and_family_list_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert!(water::read(root.ledger(), "distill", "sess-a").unwrap().is_none());
    assert_eq!(water::FAMILIES, ["distill", "derive", "anomaly_scan"]);
    // 家族名不在封闭清单里 ⇒ 报错，不新增行（§9.5"计数器表不许为不存在的工具建立正读数"同型）。
    assert!(water::read(root.ledger(), "bogus", "sess-a").is_err());

    // 封闭清单的两侧逐字对拍 DDL 的 CHECK——`water.rs` 那句"一对镜像"的注释得是真的事实。
    let l = root.ledger();
    let ddl: String = l.conn()
        .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='job_watermark'",
                   [], |r| r.get(0)).unwrap();
    let needle = format!("CHECK(family IN ({}))",
        water::FAMILIES.iter().map(|f| format!("'{f}'")).collect::<Vec<_>>().join(","));
    assert!(ddl.contains(&needle), "DDL 的家族清单与 `water::FAMILIES` 漂移：{ddl}");
    assert_eq!(ddl.matches("CHECK(").count(), 1, "family 那列多出第二个 CHECK 就是第二个清单");

    // 坑 P-71：坏游标（INTEGER 列装了 TEXT，亲和性存不下 ⇒ 读它是类型错误）必须报错，
    // 不许折成"没有游标"再被 upsert 静默覆盖。断两件事：报错了，且那一格没被改掉。
    l.conn().execute(
        "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                   attempts, last_success_at_ms, halted_at_ms)
         VALUES('distill','sess-broken','not-an-int','',0,NULL,NULL)", []).unwrap();
    assert!(water::advance(l, "distill", "sess-broken", &cursor(5_000), 6_000).is_err(),
        "游标读不出来必须报错，不许当成'没有游标'");
    let stored: String = l.conn()
        .query_row("SELECT typeof(cursor_at_ms) FROM job_watermark WHERE scope_key='sess-broken'",
                   [], |r| r.get(0)).unwrap();
    assert_eq!(stored, "text", "读失败的那一次不许把游标改掉");
}

#[test]
fn advance_forward_then_backwards_halts_without_moving_the_cursor() {
    // 坑 P-03 的正面执法：倒退必须"停轮 + 不回拨"，不是"回拨 + 记日志"。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    water::advance(l, "distill", "sess-a", &cursor(2_000), 9_000).unwrap();
    let bad = water::advance(l, "distill", "sess-a", &cursor(1_000), 9_500);
    assert!(bad.is_err(), "水位倒退必须报错");
    let c = water::read(l, "distill", "sess-a").unwrap().unwrap();
    assert_eq!(c.at_ms, 2_000, "报错的那一次不许把游标拨回去");
    assert!(water::halted(l, "distill", "sess-a").unwrap());
    // 停轮之后前进仍然可走（`clear_halt` 由运维面显式叫，任务 9 不自动清）。
    assert!(water::advance(l, "distill", "sess-a", &cursor(3_000), 9_600).is_err(),
            "停轮期间一切推进都拒，直到人清掉 halt（否则 halt 只是个装饰）");
    water::clear_halt(l, "distill", "sess-a", 9_700).unwrap();
    assert!(!water::halted(l, "distill", "sess-a").unwrap());
    water::advance(l, "distill", "sess-a", &cursor(3_000), 9_800).unwrap();
    assert_eq!(water::read(l, "distill", "sess-a").unwrap().unwrap().at_ms, 3_000);
}

#[test]
fn equal_cursor_is_idempotent_not_a_halt() {
    // 重传同一批（阶段 0/1 的幂等语义）会推着同样的 cursor 再来一次：那是幂等命中，不是倒退。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    water::advance(l, "distill", "s", &cursor(5_000), 1).unwrap();
    water::advance(l, "distill", "s", &cursor(5_000), 2).unwrap();
    assert!(!water::halted(l, "distill", "s").unwrap());
}

#[test]
fn attempts_and_heartbeat_are_separate_columns() {
    // 原设计 §6：故障期不推水位、**不计 attempts**；而 §9"周期心跳"要 `last_success_at` 每次成功都写。
    // 两个读数挤在一列的话，"这一轮失败"与"这一轮成功但零产出"就分不开。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    water::note_attempt(l, "distill", "s").unwrap();
    water::note_attempt(l, "distill", "s").unwrap();
    water::advance(l, "distill", "s", &cursor(3_000), 3_000).unwrap();
    let row: (i64, Option<i64>, Option<i64>) = l.conn().query_row(
        "SELECT attempts, last_success_at_ms, halted_at_ms FROM job_watermark WHERE family='distill' AND scope_key='s'",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(row.0, 2);
    assert_eq!(row.1, Some(3_000), "成功推进必须盖心跳");
    assert_eq!(row.2, None);
}

#[test]
fn extract_state_roundtrips_all_four_and_rejects_a_fifth() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    for (i, st) in [ExtractState::Pending, ExtractState::Extracted, ExtractState::Rejected, ExtractState::Worthless].iter().enumerate() {
        let uid = format!("{:02}{}", i, "c".repeat(62));
        let reason = if *st == ExtractState::Rejected { Some("schema") } else { None };
        water::set_extract_state(l, &uid, *st, reason, 1_000 + i as i64).unwrap();
        assert_eq!(water::get_extract_state(l, &uid).unwrap(), Some(*st));
    }
    // P2 判据原文：pending 与 rejected 必须在同一个查询面上可区分（都返回"查不到"是最像成功的失败）。
    let unknown = "f".repeat(64);
    assert_eq!(water::get_extract_state(l, &unknown).unwrap(), None, "没有行就是没有行，不许默认成 pending");
    // 第五个值进不了 DDL CHECK：
    let e = l.conn().execute(
        "INSERT INTO extract_state(turn_uid, state, reason, at_ms) VALUES('zz','magically_done',NULL,1)", []);
    assert!(e.is_err(), "state 有 CHECK 封闭清单");

    // 状态的封闭清单也逐字对拍 DDL 的 CHECK，并让四个 slug 各自站进去一次（改 slug 必红）。
    let ddl: String = l.conn()
        .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='extract_state'",
                   [], |r| r.get(0)).unwrap();
    let needle = "CHECK(state IN ('pending','extracted','rejected','worthless'))";
    assert!(ddl.contains(needle), "DDL 的状态清单与这四个 slug 漂移：{ddl}");
    for st in [ExtractState::Pending, ExtractState::Extracted, ExtractState::Rejected, ExtractState::Worthless] {
        assert!(needle.contains(&format!("'{}'", st.slug())), "{st:?} 的 slug 不在 CHECK 那一列里");
    }

    // 坑 P-71 的另一支：整张表读不出来 ≠ 这一轮没有状态。`Ok(None)` 不许由 `.ok()` 折出来。
    l.conn().execute("ALTER TABLE extract_state RENAME TO extract_state_gone", []).unwrap();
    assert!(water::get_extract_state(l, &unknown).is_err(),
        "SQL 错误必须上抛，不许读成'查无此轮'");
}

#[test]
fn state_write_is_idempotent_per_turn_and_never_grows_two_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    let uid = "1".repeat(64);
    water::set_extract_state(l, &uid, ExtractState::Pending, None, 1).unwrap();
    water::set_extract_state(l, &uid, ExtractState::Extracted, None, 2).unwrap();
    let n: i64 = l.conn().query_row("SELECT count(*) FROM extract_state WHERE turn_uid=?1", [&uid], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "一个 turn 一条状态：重写而不是长第二条");
    assert_eq!(water::get_extract_state(l, &uid).unwrap(), Some(ExtractState::Extracted));
}

#[test]
fn count_states_is_the_p5_denominator_and_carries_no_body() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let l = root.ledger();
    for (i, st) in [ExtractState::Extracted, ExtractState::Worthless, ExtractState::Rejected].iter().enumerate() {
        water::set_extract_state(l, &format!("{:02}{}", i, "d".repeat(62)), *st, None, 1).unwrap();
    }
    let got = water::count_states(l).unwrap();
    // 序由读口自己的 `ORDER BY state` 给，不在测试里再 sort 一遍：那样要么掩盖读口的序，
    // 要么为一条用例逼出一个没有读者的 `Ord`。这里直接断序，判据才有牙。
    assert_eq!(got.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
               vec![ExtractState::Extracted, ExtractState::Rejected, ExtractState::Worthless]);
    assert!(got.iter().all(|(_, n)| *n == 1), "三个 turn 三种状态：每格各一条");
    // 零正文：`reason` 这一列只许装封闭短语，用例把夹具里的正文长度当场钉住。
    let longest: i64 = l.conn().query_row(
        "SELECT coalesce(max(length(reason)),0) FROM extract_state", [], |r| r.get(0)).unwrap();
    assert!(longest <= 32, "reason 列成了自由文本 = 守卫 12 的破口，得到 {longest}");
}

#[test]
fn an_old_ledger_db_gets_the_new_tables_on_open() {
    // 坑 P-02：apply() 是"IF NOT EXISTS + 只在 found<target 时写版本"，
    // 所以旧库升级靠的是同一段 SQL 幂等重放。这条用例证的就是这件事真的发生。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        root.ledger().conn().pragma_update(None, "user_version", 1).unwrap();
        root.ledger().conn().execute_batch("DROP TABLE job_watermark; DROP TABLE extract_state").unwrap();
    }
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(rc_store::ddl::schema_version(root.ledger().conn()).unwrap(),
               rc_store::ddl::LEDGER_VERSION);
    water::advance(root.ledger(), "distill", "s", &cursor(1), 1).unwrap();
}

#[test]
fn only_the_three_families_can_have_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let e = water::advance(root.ledger(), "purge", "s", &cursor(1), 1).unwrap_err();
    assert!(format!("{e}").contains("unknown family"), "{e}");
}

#[test]
fn watermark_tables_have_no_content_bearing_column() {
    // 守卫 12 在 schema 层：两张水位表的列名逐个落在白名单里，白名单里没有一列能装记忆正文。
    // （§10 生命周期侧③"这张表里逐行断言没有一列能装下正文"的同型用例。）
    // `reason` 装的是封闭短语，它的长度在 extract_state 那条用例里钉死。
    const ALLOWED: [&str; 12] = [
        "family", "scope_key", "cursor_at_ms", "cursor_turn_uid", "attempts",
        "last_success_at_ms", "halted_at_ms", "turn_uid", "state", "reason", "at_ms",
        // S1：抽取指纹是**哈希**（64 位小写 hex），不是正文——加列时在这里表态。
        "extraction_fingerprint",
    ];
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for table in ["job_watermark", "extract_state"] {
        let mut st = root.ledger().conn()
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid")).unwrap();
        let cols: Vec<String> = st.query_map([], |r| r.get(0)).unwrap()
            .collect::<Result<_, _>>().unwrap();
        assert!(!cols.is_empty(), "{table} 没建出来");
        for name in &cols {
            assert!(ALLOWED.contains(&name.as_str()), "{table}.{name} 不在白名单里 ⇒ 有人加了列没表态");
        }
    }
}

// ── 任务 10（§2.22 单读口 + P-63 键形唯一家）：三条新用例 ──────────────────────

#[test]
fn extract_record_reads_state_and_reason_as_one_row() {
    // §2.22 的全部理由在这一条上：一行的两列**必须**同读。分成两把 getter 时这两句仍然绿，
    // 所以牙不在"读得到"，在"只有一条 SQL"——那条判据在 Step 4 的 grep 上。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    water::set_extract_state(root.ledger(), &"a".repeat(64), ExtractState::Rejected, Some("合成理由甲"), 10).unwrap();
    water::set_extract_state(root.ledger(), &"b".repeat(64), ExtractState::Extracted, None, 11).unwrap();
    let r = water::extract_record(root.ledger(), &"a".repeat(64)).unwrap().unwrap();
    assert_eq!((r.state, r.reason.as_deref()), (ExtractState::Rejected, Some("合成理由甲")));
    let n = water::extract_record(root.ledger(), &"b".repeat(64)).unwrap().unwrap();
    assert_eq!((n.state, n.reason.as_deref()), (ExtractState::Extracted, None), "无理由是合法读数");
    assert_eq!(water::extract_record(root.ledger(), &"c".repeat(64)).unwrap(), None, "缺行是读数不是错误");
}

#[test]
fn get_extract_state_is_the_same_row_read_as_extract_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    water::set_extract_state(root.ledger(), &"d".repeat(64), ExtractState::Worthless, Some("合成理由乙"), 12).unwrap();
    assert_eq!(water::get_extract_state(root.ledger(), &"d".repeat(64)).unwrap(),
               water::extract_record(root.ledger(), &"d".repeat(64)).unwrap().map(|r| r.state));
}

#[test]
fn distill_scope_is_the_only_key_shaper() {
    // P-63：键形漂一个字符**不报错**，它只是让 `read` 读回 `None`，于是 `eta_turns` 少报。
    // 所以这条断的是形状本身，而"两处生产者同一形状"由 Step 4 的 grep 执法。
    assert_eq!(water::distill_scope("dev-1", "ses-A"), "dev-1/ses-A");
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let scope = water::distill_scope("dev-1", "ses-A");
    water::advance(root.ledger(), "distill", &scope, &cursor(100), 200).unwrap();
    assert!(water::read(root.ledger(), "distill", &water::distill_scope("dev-1", "ses-A")).unwrap().is_some(),
            "同一对零件两次算出同一个键");
}

#[test]
fn last_extract_at_ms_is_none_before_any_success() {
    // 从没成功过的家族 ⇒ `None`，不是 0。理由与 `cursor_row` 那句"不替调用方造一个 0"同一条：
    // "最后一次成功在 0 毫秒"和"从来没有成功"是两件事，而后者是 `status` 那一格今天的真读数。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    assert_eq!(water::last_extract_at_ms(root.ledger(), water::DISTILL).unwrap(), None);
    // 未知家族响亮拒绝，而不是安静给 `None`：给 `None` 就分不清"没成功过"和"家族名拼错了"。
    assert!(water::last_extract_at_ms(root.ledger(), "nope").is_err());
}

#[test]
fn last_extract_at_ms_takes_the_max_over_scopes_and_ignores_other_families() {
    // 这一格答的是"抽取家族上一次真推进是什么时候"，所以它是**跨会话的 max**，
    // 而且**只**取一个家族：`derive` 与 `anomaly_scan` 的成功时刻混进来就是第二个口径。
    // 三个 scope 的次序是反着写的，这样 `max` 若被写成"最后一行"或"第一行"都会红。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let l = root.ledger();
    water::advance(l, water::DISTILL, "s1", &cursor(300), 300).unwrap();
    water::advance(l, water::DISTILL, "s2", &cursor(100), 100).unwrap();
    water::advance(l, water::DISTILL, "s3", &cursor(500), 500).unwrap();
    assert_eq!(water::last_extract_at_ms(l, water::DISTILL).unwrap(), Some(500));
    // 另一个家族自己成功过一次，更早——它不许把 max 拉低，也不许被读进 DISTILL 那一格。
    water::advance(l, "derive", "s9", &cursor(450), 450).unwrap();
    assert_eq!(water::last_extract_at_ms(l, water::DISTILL).unwrap(), Some(500),
               "别的家族的时刻漏进来了 ⇒ 这一格不再是抽取家族的读数");
    assert_eq!(water::last_extract_at_ms(l, "derive").unwrap(), Some(450));
    // 有一条游标行但从未成功（`note_attempt` 之后、`advance` 之前那种形状）不许污染 max：
    // `advance` 才盖 `last_success_at_ms`，而 `attempts` 那一支把时间同伴列留成 NULL。
    water::note_attempt(l, water::DISTILL, "s4").unwrap();
    assert_eq!(water::last_extract_at_ms(l, water::DISTILL).unwrap(), Some(500),
               "NULL 被当成 0 读进来了 ⇒ SQL 少了一层 Option");
}

// ── 任务 8（停轮与滞后两面可见）：三条新用例 ─────────────────────────────────

#[test]
fn halted_scopes_count_returns_zero_when_no_row_halted() {
    let l = fresh_ledger();
    assert_eq!(water::halted_scopes_count(&l).unwrap(), 0);
}

#[test]
fn halted_scopes_count_counts_only_distill_family_halted_rows() {
    let l = fresh_ledger();
    water::advance(&l, "distill", "dev-a/s-a", &Cursor { at_ms: 1, turn_uid: "t".into() }, 100).unwrap();
    water::advance(&l, "derive",  "dev-a/s-a", &Cursor { at_ms: 1, turn_uid: "t".into() }, 100).unwrap();
    // 让 distill 倒退一次 ⇒ halt 落笔
    let _ = water::advance(&l, "distill", "dev-a/s-a", &Cursor { at_ms: 0, turn_uid: "".into() }, 200);
    assert_eq!(water::halted_scopes_count(&l).unwrap(), 1, "derive 行 halt 不计入");
}

#[test]
fn lag_by_scope_returns_zero_for_session_with_state_rows() {
    let (l, _t) = fresh_pair();
    // scope 里有 0 turn ⇒ 0 lag（不返负）
    let lag = water::lag_by_scope(&l, &_t).unwrap();
    assert!(lag.is_empty() || lag.iter().all(|(_, n)| *n >= 0), "lag 不许为负");
}

/// 合成一行转录（转录连接）。夹具一律合成，不碰真实导出——与 `rc-store` 的 ddl 用例同一条纪律。
/// `turn_uid` 按位编号，所以"哪几轮有状态行"在下面几行里一眼可数。
fn synth_turn(t: &TranscriptHandle, i: u32, device_id: &str, session_key: &str) {
    synth_turn_with_role(t, i, device_id, session_key, "human");
}

/// 同上，但角色可指定：`system` 轮是压缩块/CTX，只作上下文，永不作抽取目标，
/// 所以它**不进 lag 的分母**——这条口径必须有夹具能造出 system 轮来钉。
fn synth_turn_with_role(
    t: &TranscriptHandle,
    i: u32,
    device_id: &str,
    session_key: &str,
    role: &str,
) {
    let uid = format!("{:064x}", i + 1);
    t.conn().execute(
        "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                           turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
         VALUES(?1, ?3, ?2, ?4, '合成正文', '合成指纹', 1, NULL, 1000, 1001)",
        rusqlite::params![uid.as_str(), device_id, session_key, role],
    ).unwrap();
}

/// 非空库夹具：`lag_by_scope` 的函数主体今天第一次被跑到。
///
/// 上一轮那条用例用的是**空库**：`assert!(lag.is_empty() || …)` 的左支当场成立，
/// 于是"在返回前插一行 `out.clear()`"与"删掉降序那一行"全仓零红——**分支一行没跑到**，
/// 不是分支恒真。所以这里换一个真装了行的库，并且断言形状是**逐字对拍整个返回**：
/// 多一行、少一行、顺序反了，三种改坏都当场红。左支不再是"也放过"，只剩右支。
#[test]
fn lag_by_scope_counts_turns_without_a_state_row_and_orders_descending() {
    let (l, t) = fresh_pair();
    // scope `dev-a/s-a`：5 轮转录，其中 2 轮在 `extract_pass_state` 里有终态行。
    // 用例名保留（它是判据锚），只换夹具：S3b 起 lag_scopes 与 status 同源，legacy
    // `extract_state` 不再是读口——它今天没有写者，"有一行"算已蒸的旧口径会把
    // 一条从没蒸过的轮读成完成。两种终态都用上，证明减数是"终态集合"不是某一个 state。
    for i in 0..5 { synth_turn(&t, i, "dev-a", "s-a"); }
    pass_state::mark_terminal(
        &l, PassKey { turn_uid: &format!("{:064x}", 1), pass_id: "naming", family: "" },
        PassState::Done, None, 10).unwrap();
    pass_state::mark_terminal(
        &l, PassKey { turn_uid: &format!("{:064x}", 2), pass_id: "event", family: "" },
        PassState::Empty, None, 11).unwrap();
    // 阴性对照：legacy 表里给另一轮写 extracted ⇒ 读旧表会把这轮算成已蒸，lag 变 2。
    water::set_extract_state(&l, &format!("{:064x}", 3), ExtractState::Extracted, None, 12).unwrap();
    // scope `dev-b/s-b`：4 轮全部有终态 ⇒ lag 0。"没有落后"不是诊断信号，不许出现在返回里。
    for i in 5..9 { synth_turn(&t, i, "dev-b", "s-b"); }
    for i in 5..9 {
        pass_state::mark_terminal(
            &l, PassKey { turn_uid: &format!("{:064x}", i + 1), pass_id: "naming", family: "" },
            PassState::Done, None, 13).unwrap();
    }

    assert_eq!(
        water::lag_by_scope(&l, &t).unwrap(),
        vec![("dev-a/s-a".to_owned(), 3i64)],
        "lag = 没有终态行的可抽取轮（5 − 2）；legacy extracted 不参与；lag 0 的 scope 不返回"
    );

    // 再播一个 lag 更大的 scope ⇒ 两条都在场，且必须按 lag 降序。
    // 降序那一行被删掉时返回是字典序（`dev-a` 排在 `dev-c` 前），与期望正相反 ⇒ 当场红。
    for i in 9..15 { synth_turn(&t, i, "dev-c", "s-c"); }
    assert_eq!(
        water::lag_by_scope(&l, &t).unwrap(),
        vec![("dev-c/s-c".to_owned(), 6i64), ("dev-a/s-a".to_owned(), 3i64)],
        "降序是读口给的，不是调用方自己 sort 的"
    );
}

#[test]
fn lag_by_scope_只数可抽取轮_且减数只认_pass_终态() {
    // S3b 漏项：`lag_scopes` 仍读 legacy `extract_state`，且分母含 system 轮。
    // 两条都必须红。夹具：4 条可抽取轮 + 1 条 system；1 条可抽取轮有 pass 终态；
    // 再把一条**别的**可抽取轮写进 legacy 表作阴性对照。
    let (l, t) = fresh_pair();
    for i in 0..4 { synth_turn(&t, i, "dev-a", "s-a"); }
    synth_turn_with_role(&t, 4, "dev-a", "s-a", "system");
    let first = format!("{:064x}", 1);
    pass_state::mark_terminal(
        &l,
        PassKey { turn_uid: &first, pass_id: "naming", family: "" },
        PassState::Done, None, 20,
    ).unwrap();
    water::set_extract_state(&l, &format!("{:064x}", 2), ExtractState::Extracted, None, 10).unwrap();

    assert_eq!(
        water::lag_by_scope(&l, &t).unwrap(),
        vec![("dev-a/s-a".to_owned(), 3i64)],
        "分母 = 4 条可抽取轮（system 不计）；减数 = 1 条 pass 终态（legacy extracted 不参与）"
    );
}

// ── S1：抽取指纹（算法、写口、读口、聚合读数、迁移）────────────────────────────
//
// 四件事各一条牙：① 算法对字段边界敏感（长度前缀）；② 旧行读出来是 None；③ 盖章只更新
// 已存在的行；④ 聚合读数把 NULL 组排在最后。迁移那条单独放在末尾，它要造一个旧形状的库。

fn hex64(tag: &str) -> String {
    // 测试里的"指纹"不是真哈希：写口/读口只搬运字符串，形状由 `extraction_fingerprint` 自己钉。
    // 但仍然做成 64 位小写 hex——它是库里那一列的真实形状，夹具不许比生产宽。
    let base: String = tag.bytes().map(|b| format!("{b:02x}")).collect();
    base.chars().cycle().take(64).collect()
}

#[test]
fn extraction_fingerprint_is_64_lowercase_hex_and_field_boundary_aware() {
    assert_eq!(water::EXTRACT_PIPELINE_VERSION, "extract-v1");
    let fp = water::extraction_fingerprint("extract-v1", "model-a", "SYS", "USER");
    assert_eq!(fp.len(), 64, "指纹是 64 位 hex，得到 {fp}");
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "只许小写 hex：{fp}");
    // 同一套输入逐字节相同（同一模型同一 prompt 同一输入 ⇒ 同一指纹）。
    assert_eq!(fp, water::extraction_fingerprint("extract-v1", "model-a", "SYS", "USER"));
    // 任一段变一个字符 ⇒ 指纹变。
    for (p, m, s, u) in [
        ("extract-v2", "model-a", "SYS", "USER"),
        ("extract-v1", "model-b", "SYS", "USER"),
        ("extract-v1", "model-a", "SYS!", "USER"),
        ("extract-v1", "model-a", "SYS", "USER!"),
    ] {
        assert_ne!(fp, water::extraction_fingerprint(p, m, s, u),
                   "换了 ({p},{m},{s},{u}) 指纹却没变");
    }
    // 长度前缀的意义：把边界挪一格不许撞（朴素拼接会撞）。
    assert_ne!(water::extraction_fingerprint("ab", "c", "", ""),
               water::extraction_fingerprint("a", "bc", "", ""),
               "字段拼接有歧义 ⇒ 两套不同的输入得到同一个指纹");
    assert_ne!(water::extraction_fingerprint("a", "b", "c", ""),
               water::extraction_fingerprint("a", "b", "", "c"));
}

#[test]
fn a_legacy_state_row_reads_no_fingerprint() {
    let l = fresh_ledger();
    let uid = hex64("legacy");
    // 旧形状的写入口：只给四列，新列留 NULL。
    l.conn().execute(
        "INSERT INTO extract_state(turn_uid, state, reason, at_ms)
         VALUES(?1, 'extracted', NULL, 7)", [uid.as_str()]).unwrap();
    let r = water::extract_record(&l, &uid).unwrap().unwrap();
    assert_eq!(r.fingerprint, None, "旧行没有指纹 ⇒ None，不许造一个空串");
    assert_eq!(r.state, ExtractState::Extracted);
}

#[test]
fn stamping_a_fingerprint_updates_only_that_column_and_overwrites() {
    let l = fresh_ledger();
    let uid = hex64("stamp");
    water::set_extract_state(&l, &uid, ExtractState::Extracted, None, 10).unwrap();
    let first = hex64("a");
    water::set_extraction_fingerprint(&l, &uid, &first).unwrap();
    let r = water::extract_record(&l, &uid).unwrap().unwrap();
    assert_eq!(r.fingerprint.as_deref(), Some(first.as_str()));
    assert_eq!((r.state, r.reason.as_deref()), (ExtractState::Extracted, None),
               "盖章不许动 state/reason");
    let at: i64 = l.conn().query_row(
        "SELECT at_ms FROM extract_state WHERE turn_uid=?1", [uid.as_str()], |r| r.get(0)).unwrap();
    assert_eq!(at, 10, "盖章不许刷新 at_ms（那一列是状态落下的时刻）");

    // 重复盖章覆盖成新值（不是"第一条赢"，也不是长第二行）。
    let second = hex64("b");
    water::set_extraction_fingerprint(&l, &uid, &second).unwrap();
    assert_eq!(water::extract_record(&l, &uid).unwrap().unwrap().fingerprint.as_deref(),
               Some(second.as_str()));
    let n: i64 = l.conn().query_row(
        "SELECT count(*) FROM extract_state WHERE turn_uid=?1", [uid.as_str()], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn stamping_an_absent_turn_is_a_no_op_and_creates_no_row() {
    let l = fresh_ledger();
    let uid = hex64("absent");
    // LLM 故障轮没有状态行：盖章是 0 行命中的 UPDATE，正常结束而不是错误。
    water::set_extraction_fingerprint(&l, &uid, &hex64("a")).unwrap();
    let n: i64 = l.conn().query_row("SELECT count(*) FROM extract_state", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "盖章不许替没有状态行的轮造行");
    assert_eq!(water::extract_record(&l, &uid).unwrap(), None);
}

#[test]
fn fingerprint_counts_groups_by_fingerprint_with_null_last() {
    let l = fresh_ledger();
    let (fa, fb) = (hex64("a"), hex64("b"));
    // 播种顺序故意打散：读口的序必须由 SQL 给，不是插入序。
    for (tag, at) in [("n1", 5), ("b2", 50), ("a1", 30), ("n2", 60), ("b1", 10)] {
        water::set_extract_state(&l, &hex64(tag), ExtractState::Extracted, None, at).unwrap();
    }
    water::set_extraction_fingerprint(&l, &hex64("a1"), &fa).unwrap();
    water::set_extraction_fingerprint(&l, &hex64("b1"), &fb).unwrap();
    water::set_extraction_fingerprint(&l, &hex64("b2"), &fb).unwrap();

    assert_eq!(
        water::fingerprint_counts(&l).unwrap(),
        vec![
            water::FingerprintCount { fingerprint: Some(fa), turns: 1, first_at_ms: 30, last_at_ms: 30 },
            water::FingerprintCount { fingerprint: Some(fb), turns: 2, first_at_ms: 10, last_at_ms: 50 },
            water::FingerprintCount { fingerprint: None, turns: 2, first_at_ms: 5, last_at_ms: 60 },
        ],
        "按指纹升序、NULL 组排最后；first/last 是 min/max(at_ms)"
    );

    // 空库 ⇒ 空表（不是一条 turns=0 的幽灵行）。
    let empty = fresh_ledger();
    assert!(water::fingerprint_counts(&empty).unwrap().is_empty());
}

#[test]
fn an_old_shape_extract_state_gains_the_fingerprint_column_on_open() {
    // 旧形状 = 升级前那台机器的 ledger.db：extract_state 没有新列、user_version 已经是 7
    //（本任务**不** bump 版本，所以迁移只能是列存在性驱动）。
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        root.ledger().conn().execute_batch(
            "DROP TABLE extract_state;
             CREATE TABLE extract_state(
                turn_uid TEXT PRIMARY KEY,
                state TEXT NOT NULL CHECK(state IN ('pending','extracted','rejected','worthless')),
                reason TEXT,
                at_ms INTEGER NOT NULL);
             INSERT INTO extract_state(turn_uid, state, reason, at_ms)
                VALUES('legacy-turn', 'extracted', NULL, 42);").unwrap();
        assert_eq!(rc_store::ddl::schema_version(root.ledger().conn()).unwrap(),
                   rc_store::ddl::LEDGER_VERSION,
                   "前置条件：旧形状的库已经到 LEDGER_VERSION（版本号不能当迁移信号）");
    }
    let root = Root::open(dir.path()).unwrap();
    let n: i64 = root.ledger().conn().query_row(
        "SELECT count(*) FROM pragma_table_info('extract_state') WHERE name='extraction_fingerprint'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "Root::open 没把新列补上（列存在性驱动的迁移没跑）");
    let r = water::extract_record(root.ledger(), "legacy-turn").unwrap().unwrap();
    assert_eq!((r.state, r.fingerprint), (ExtractState::Extracted, None),
               "旧行必须还在、指纹为 None");
    // 新列可写（迁移不是只把列名放进 pragma 却写不进去）。
    water::set_extraction_fingerprint(root.ledger(), "legacy-turn", &hex64("a")).unwrap();
    assert_eq!(water::extract_record(root.ledger(), "legacy-turn").unwrap().unwrap().fingerprint,
               Some(hex64("a")));
    // 版本号一格没动。
    assert_eq!(rc_store::ddl::schema_version(root.ledger().conn()).unwrap(),
               rc_store::ddl::LEDGER_VERSION);
}
