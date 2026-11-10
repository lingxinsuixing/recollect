use rc_distill::derive::{run_derive, DeriveExit};
use rc_ledger::water;
use rc_llm::mock::MockBackend;
use rc_memory::{FactContent, Provenance, Scope, ScopeKind, AnchorRole, MemKey, put_card,
                put_entity, EntityContent};
use rc_store::Root;
use std::path::Path;

const NOW: i64 = 1_700_000_000_000;

fn open(p: &Path) -> Root { Root::open(p).unwrap() }

fn seed_two_cards(d: &Path) -> (MemKey, MemKey) {
    let root = open(d);
    // I4：两端共享实体「全量测试」⇒ 别名补铸先落两条 mention，part_of 才过锚门槛。
    put_entity(root.memory(), &EntityContent {
        kind: "concept".into(), title: "全量测试".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        aliases: vec![],
    }, &[Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }], NOW - 10).unwrap();
    let a = put_card(root.memory(), &FactContent {
        kind: "decision".into(), title: "上线前先跑测试".into(),
        body: "任何合并之前先跑全量测试。".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }], NOW).unwrap();
    let b = put_card(root.memory(), &FactContent {
        kind: "decision".into(), title: "上线前需要验证".into(),
        body: "任何合并之前先跑全量测试。".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }], NOW).unwrap();
    (a, b)
}

#[test]
fn run_derive_pushes_cursor_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (a, _b) = seed_two_cards(d);
    let root = open(d);
    let backend = MockBackend::scripted(vec![Ok(
        // 一条**真能落库**的关系：三键齐、kind 在封闭枚举内、两端（r2/r3 = 两张 card）共享
        // 实体「全量测试」补铸出的 mention（I4）。r1 是更早的实体行，不是这条边的端点。
        r#"{"relationships":[{"from":"r2","to":"r3","kind":"part_of","reason":"r2 是 r1 的前置检查","basis":["r2","r3"]}],"chains":[],"timeline":[]}"#.to_string()
    )]);
    let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert!(matches!(exit, DeriveExit::Ok), "成功的批次不是 Ok：{exit:?}");
    assert_eq!(readout.rows, 3, "一条关系 + 两端各一条别名 mention（I4/I6）");
    assert_eq!(readout.rejected, 0);
    assert!(!readout.dropped);
    // 推游标：cursor 停在 (NOW, 批末那一行的 key_string)
    let cur = water::read(root.ledger(), "derive", "").unwrap().unwrap();
    assert_eq!(cur.at_ms, NOW);
    assert!(!cur.turn_uid.is_empty());
    // 上面两格断不出"推到批末"：两条种子行的 `created_at_ms` **同为 NOW**，`turn_uid` 非空对
    // 批首批末是同一句话，所以 `run_derive` 里那个 `batch.last()` 换成 `batch.first()` 时
    // 本用例逐字仍绿。而"推批末"正是游标推进契约本身——推成批首 ⇒ 批里最后一行永远被重选，
    // `derive_lag_rows` 永远归不了零。这一格把批末那一行的 key 点名钉住。
    //
    // 批末是 **`a` 而不是 `b`**：`rows_since` 的全序是 `(created_at_ms ASC, tbl ASC, id ASC)`，
    // 同 ms 时落到内容寻址 id 的字典序上——不是写入次序（`a` 先写，可它并不先入选）。
    // 本机实测：本用例下 `a` 的 id 排在 `b` 之前，所以批是 `[b, a]`、末行是 `a`。
    // 绑成 `b` 会当场红，而红的是这条断言本身（不是生产代码坏了）。
    assert_eq!(cur.turn_uid, a.key_string(),
        "游标落在批末那一行的 key 上——推成批首会让那一行永远重选");
}

#[test]
fn run_derive_drops_the_batch_and_leaves_the_cursor_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_two_cards(d);
    let root = open(d);
    // **缺键** = 答复不合格 ⇒ 契约族，整批丢。空三键不再属于这一支（见下一条用例）：
    // 它是模型对"没有可推导内容"的合格答复，照推游标。
    let backend = MockBackend::scripted(vec![Ok(
        r#"{"relationships":[]}"#.to_string()
    )]);
    let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert_eq!(exit, DeriveExit::BatchDropped, "缺键属契约族，不是运维故障");
    assert!(readout.dropped, "丢批那一格是 dropped：{readout:?}");
    assert_eq!(readout.drop_reason, Some("schema"), "丢批必须带封闭类别：{readout:?}");
    assert_eq!((readout.rows, readout.rejected), (0, 0));
    // 游标不动：整批被拒收 ⇒ 下一轮还会选中同一批（游标是活锁的唯一解药，不许在这里前进）。
    //
    // **这一格的形状与 Step 1 原稿的 `water::read(..).is_none()` 不同，是原稿那一格本身写错了**：
    // `note_attempt` 是 INSERT（`job_watermark` 那张表上 `family='derive'` / `scope_key=''`
    // 的行由它**建出来**，游标两列取初值 0 / ''），所以丢批之后这行**在场**而不是缺席。
    // brief 的 Step 9（hub 侧 `derive_dropped_increments_dropped_and_keeps_cursor`，已落地并通过）
    // 给的是同一件事的另一个形状：查行、断言 `cursor_at_ms == 0`。
    // 两处只能留一个真相，而 Step 9 那一侧是树上已经在跑的那一侧 ⇒ 这里取它的形状。
    // 判据的口径没变：游标两列必须停在初值，前进了就等于把这批永远跳过。
    let cur = water::read(root.ledger(), "derive", "").unwrap().unwrap();
    assert_eq!((cur.at_ms, cur.turn_uid.as_str()), (0, ""),
        "丢批之后游标仍停在初值——前进了就等于把这批永远跳过");
    // attempts +1：运维看见活锁的唯一一格（`note_attempt` 只涨计数、不动游标两列）
    let attempts: i64 = root.ledger().conn().query_row(
        "SELECT attempts FROM job_watermark WHERE family='derive' AND scope_key=''",
        [], |r| r.get(0)).unwrap();
    assert_eq!(attempts, 1, "丢批计 attempts：不计的话活锁在运维面上完全不可见");
}

#[test]
fn run_derive_accepts_an_explicit_empty_output_and_advances() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_two_cards(d);
    let root = open(d);
    // 四键齐全、四数组全空 = 合格的空答复（P1 的 cron 摘要批就是这个形状）。
    // 它必须推游标；否则同一批每轮重选、attempts 无限增长，derive 永远停在这一批。
    let backend = MockBackend::scripted(vec![Ok(
        r#"{"relationships":[],"chains":[],"timeline":[],"mentions":[]}"#.to_string()
    )]);
    let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    assert_eq!(exit, DeriveExit::Ok, "空答复不是丢批：{exit:?}");
    assert!(!readout.dropped);
    assert_eq!((readout.rows, readout.rejected), (0, 0));
    assert_eq!(readout.drop_reason, None);
    let cur = water::read(root.ledger(), "derive", "").unwrap().unwrap();
    assert_eq!(cur.at_ms, NOW, "空答复也要推游标，否则这一批永远重选");
    assert!(!cur.turn_uid.is_empty());
}

#[test]
fn run_derive_reports_a_fault_when_the_attempt_cannot_be_written() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    seed_two_cards(d);
    let root = open(d);
    // 让 `note_attempt` 的 INSERT 真失败、而**读侧仍通**——这是本用例的全部难点。
    //
    // 第一版用 `ALTER TABLE job_watermark RENAME TO jw_gone`，那条**打不出红**：改名连
    // `water::read` 一起弄坏，`run_derive` 的第 1 步就返 `Fault(FAULT_READ)` 了，而
    // `Fault` 恰好就是本用例断言要的那个形状 ⇒ 假绿（变异实测：`3 passed; 0 failed`）。
    // 这就是"守卫在真路径上"与"守卫被别处短路"的分界线。
    //
    // 触发器只拦 INSERT、读侧不动，于是唯一能让它失败的就是 `note_attempt` 那一支。
    root.ledger().conn().execute_batch(
        "CREATE TRIGGER t_c4 BEFORE INSERT ON job_watermark BEGIN
             SELECT RAISE(ABORT, 'watermark is sealed');
         END").unwrap();
    // 先证明读侧确实还通（否则这一格又是空过）：把批读出来
    assert_eq!(water::read(root.ledger(), "derive", "").unwrap(), None,
        "触发器只许拦 INSERT；读侧要是也不通，本用例就短路成 FAULT_READ 的假绿");
    // 缺键才会走 `note_attempt`；空三键现在是合格答复，不会碰水位写口。
    let backend = MockBackend::scripted(vec![Ok(
        r#"{"relationships":[]}"#.to_string()
    )]);
    let (readout, exit) = run_derive(root.memory(), root.ledger(), &backend, NOW);
    // 上抛为 Fault：`BatchDropped` 会把"水位库写失败"读成"这批被模型拒收"，
    // 而 attempts 一格没涨 ⇒ 活锁在运维面上完全不可见（裁定 12 / P-32 要防的正是这件事）。
    assert!(matches!(exit, DeriveExit::Fault(_)),
        "水位写失败不许报 BatchDropped——那样 attempts 没涨而运维看见的是一次契约拒收：{exit:?}");
    assert!(!readout.dropped, "Fault 不是丢批：{readout:?}");
}