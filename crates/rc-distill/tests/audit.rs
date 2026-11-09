//! 十二条分四组：**调用形状**（三票三次、三个问题，P-11 + P-39）、**权力分配**（只有票 1 消音）、
//! **证据面**（票 1 读转录原文、证据**缺行**只降档不消音而**缺表**是错误，P-43 + P-71）、**形状错与游标**（P-36 + §2.15）。
mod common;
use common::{one_card_payload, seed};
use rc_distill::audit::{build_case, parse_vote, run_audit, AuditError, AuditOutcome, Vote, VOTES};
use rc_distill::extract::{run_extract, select_batch, MAX_TURNS_PER_BATCH};
use rc_distill::prompt::audit_user;
use rc_distill::DistillError;
use rc_ledger::water;
use rc_llm::{mock::MockBackend, LlmBackend, LlmFailure, Prompt};
use rc_memory::{lookup, owner::{OwnerCtx, Visibility}, scope_of, set_verdict, unreviewed_rows,
                writers_x::{put_batch_with_owner, BatchCtx, WriteItem},
                AnchorRole, FactContent, MemKey, MemTable, Provenance, RowFacts, Scope, ScopeKind,
                Verdict, put_card};
use rc_store::Root;

const TRUE_REPLY: &str = r#"{"holds":true}"#;

/// 一次真抽取之后的库：一行未审卡片，出处指向真实存在的人工轮。
/// 审核的两条前置（"行已落库"与"证据读得到"）只有走这条链才同时成立，
/// 所以本文件的每一条都从这里起步——直接 `put_card` 造出来的行没有可重读的证据。
fn extracted(dir: &std::path::Path) -> Root {
    let mut root = Root::open(dir).unwrap();
    let uids = seed(&mut root, 2, 1_000);
    let plan = select_batch(root.transcript(), root.ledger(), MAX_TURNS_PER_BATCH).unwrap().unwrap();
    let backend = MockBackend::scripted(vec![Ok(one_card_payload(&uids))]);
    run_extract(root.memory(), root.ledger(), &plan, &backend, 3_000).unwrap();
    root
}

fn the_row(root: &Root) -> RowFacts {
    unreviewed_rows(root.memory(), 10).unwrap().remove(0)
}

/// 判同与票 3 的邻居池（C5c 起）按 `resource_owner` 的
/// `(owner_principal_id, owner_agent_id)` 过滤：邻居必须与被打分的那一行**同一个归属人**。
/// `put_card` 的 owner 恒为合成 agent（`synthetic-agent`），与抽取批次写出的行不是一个人，
/// 所以夹具把归属从那一行自己读回来再照抄过去——硬编码同样是在猜。
fn card_as_owner_of(root: &Root, like: &RowFacts, title: &str, body: &str, at: i64) -> MemKey {
    let scope = scope_of(root.memory(), &like.key).unwrap();
    let (principal, agent): (Option<String>, String) = root.memory().conn().query_row(
        "SELECT owner_principal_id, owner_agent_id FROM resource_owner
          WHERE mem_table='card' AND mem_id=?1",
        rusqlite::params![like.key.id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let items = vec![WriteItem {
        table: MemTable::Card,
        kind: "lesson".into(),
        title: title.into(),
        body: body.into(),
        scope,
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }],
    }];
    let ctx = BatchCtx { source_agent: agent.clone(), device_id: "dev-audit-1".into() };
    let owner = OwnerCtx {
        owner_principal_id: principal,
        owner_agent_id: agent,
        visibility: Visibility::PrivateAgent,
        retention: "default".into(),
    };
    put_batch_with_owner(root.memory(), &items, &ctx, &owner, at, &[])
        .unwrap().remove(0).unwrap()
}

/// 记下每一次请求的**用户段**。`MockBackend` 只数次数、不存请求文本，而 P-11 与 P-39 两条判据
/// 要断的都是请求内容（三次调用 + 三段互不相同的 user），所以这里自己实现一个后端。
/// 它必须与 `MockBackend` 用同一份脚本形状：`Vec<Result<String, LlmFailure>>` 按序消费、耗尽即 `Timeout`。
/// 存的是 `prompt.user.clone()` 而不是 `prompt.clone()`——Recorder 的接口只欠用户段，
/// 而这两条判据一个都不需要 system 段（system 那条的缺席由 `prompt_shape.rs` 的 canary 用例测）。
struct Recorder {
    replies: std::cell::RefCell<Vec<Result<String, LlmFailure>>>,
    users: std::cell::RefCell<Vec<String>>,
}
impl Recorder {
    fn new(replies: Vec<Result<String, LlmFailure>>) -> Recorder {
        Recorder { replies: std::cell::RefCell::new(replies), users: std::cell::RefCell::new(vec![]) }
    }
    fn calls(&self) -> usize { self.users.borrow().len() }
}
impl LlmBackend for Recorder {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        self.users.borrow_mut().push(prompt.user.clone());
        let mut q = self.replies.borrow_mut();
        if q.is_empty() { return Err(LlmFailure::Timeout); }
        q.remove(0)
    }
}

#[test]
fn three_calls_three_questions_and_three_writings() {
    // P-11 挡住"一次调用三个 bool"，这一条同时挡住"三次调用同一段文本"（P-39）。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let rec = Recorder::new(vec![Ok(TRUE_REPLY.into()), Ok(TRUE_REPLY.into()), Ok(TRUE_REPLY.into())]);
    let out = run_audit(root.transcript(), root.memory(), &rec, 4_000).unwrap();
    assert_eq!((out.rows_audited, out.passed, rec.calls()), (1, 1, 3), "三票必须是三次调用");
    let users = rec.users.borrow().clone();
    assert_eq!(users.len(), 3);
    assert_ne!(users[0], users[1], "票 1 与票 2 问的是同一件事 ⇒ 三票退化成一票乘三");
    assert_ne!(users[1], users[2], "票 2 与票 3 同文 ⇒ 同上");
    assert_ne!(users[0], users[2], "票 1 与票 3 同文 ⇒ 同上");
    // 各自的票号只出现在自己那一段（`Vote::slug()` 是唯一的票号家）。
    for (i, vote) in VOTES.iter().enumerate() {
        assert!(users[i].contains(vote.slug()), "{:?} 段里没有自己的票号：{}", vote, users[i]);
        for other in VOTES.iter().filter(|v| *v != vote) {
            assert!(!users[i].contains(other.slug()), "{:?} 段里出现了 {:?} 的票号", vote, other);
        }
    }
}

#[test]
fn only_vote_one_has_the_power_to_mute() {
    // §2.18 连带 1：票 2 / 票 3 不成立只降档。三型各跑一次，只有 i==0 那一次有账。
    for i in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let root = extracted(dir.path());
        // 句柄在跑审核**之前**抄下来：跑完之后这一行已经不是 unreviewed，`unreviewed_rows` 里读不到它。
        let key = the_row(&root).key.clone();
        let mut replies: Vec<Result<String, LlmFailure>> = (0..3).map(|_| Ok(TRUE_REPLY.to_string())).collect();
        replies[i] = Ok(r#"{"holds":false}"#.to_string());
        let backend = MockBackend::scripted(replies);
        let out = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
        let want = if i == 0 { Verdict::Failed } else { Verdict::Low };
        assert_eq!(out.failed + out.low, 1);
        // `lookup` 仍然读得到那一行（本阶段没有删除路径），读的是 verdict 列与账。
        let row = lookup(root.memory(), &key).unwrap()
            .expect("行还在：筛掉它的是账，不是 DELETE");
        assert_eq!(row.verdict, want, "票 {} 的落库档位不对", i + 1);
        assert_eq!(row.retired, i == 0, "票 {} 被当成了消音权", i + 1);
        let n: i64 = root.memory().conn()
            .query_row("SELECT count(*) FROM retirement", [], |r| r.get(0)).unwrap();
        assert_eq!(n, if i == 0 { 1 } else { 0 }, "消音的账只可能来自票 1");
    }
}

#[test]
fn three_votes_in_order_write_exactly_once() {
    // §2.18 连带 3 的形状：不是三次 UPDATE 折一次，是只发一次。
    // 读的是 `AuditOutcome` 的分配律（四格算术），而不是"最后一次 verdict 对不对"——
    // 后者会被"连写三次最后停在 Passed"那种实现蒙过去。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let backend = MockBackend::scripted(vec![Ok(TRUE_REPLY.into()); 3]);
    let out: AuditOutcome = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!((out.rows_audited, out.passed, out.low, out.failed), (1, 1, 0, 0));
    assert_eq!(out.passed + out.low + out.failed, out.rows_audited, "一行只进一个格子");
    assert_eq!(backend.calls(), 3, "票 1 已经 false 也要跑完三票：短路让配额读数失去算术意义");
}

#[test]
fn a_false_vote_two_does_not_stop_vote_three() {
    // P-11 的反方向：短路。票 1 真、票 2 假 ⇒ 仍必须三次调用（那一行降档，但第三票照问）。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let backend = MockBackend::scripted(vec![Ok(TRUE_REPLY.into()),
                                             Ok(r#"{"holds":false}"#.into()),
                                             Ok(TRUE_REPLY.into())]);
    let out = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!((out.low, backend.calls()), (1, 3));
}

#[test]
fn vote_one_reads_the_cited_turns_and_vote_three_reads_the_neighbours() {
    // P-43 的执法点：**证据进没进 prompt 是用例读的，不是注释声称的**。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    // 行与归属都在加邻居**之前**抄：邻居的 `as_of_ms` 更早，`the_row` 的首行会被它顶掉；
    // 而归属必须从那一行自己读（`scope_of`），硬编码 `"sess-batch-A"` 是猜——猜错了这条用例
    // 断的是"一条不属于本行的邻居"，票 3 那一侧读的是空气。
    let row = the_row(&root);
    // 邻居的标题**照抄那一行**：票 3 的候选来自 `scored_siblings` 的词面重叠门槛，
    // 自造一个不相干的标题会让这条邻居根本进不了候选。正文里那串独有的是**只有邻居才有**的标记。
    // 归属（scope 与归属人）从那一行自己读回来：C5c 起候选池按 `resource_owner` 过滤邻居。
    card_as_owner_of(&root, &row, &row.title, &format!("{} 邻居独有串", row.body), 1_500);
    let case = build_case(root.transcript(), root.memory(), &row).unwrap();
    assert_eq!(case.evidence.len(), 1, "出处只有一条人工轮 ⇒ 证据恰好一条");
    assert_eq!(case.siblings.len(), 1, "票 3 的邻居是从记忆库直接读回来的那一条");

    // 被引证的那条轮，从**那一行自己的出处**读回来（不是"表里按 uid 排第一"那种猜）。
    let uid = root.memory().conn().query_row::<String, _, _>(
        "SELECT turn_uid FROM provenance WHERE mem_table='card' AND mem_id=?1",
        rusqlite::params![row.key.id], |r| r.get(0)).unwrap();
    let cited = root.transcript().conn().query_row::<String, _, _>(
        "SELECT body FROM turns WHERE turn_uid=?1", rusqlite::params![uid], |r| r.get(0)).unwrap();

    let v1 = audit_user(&case, Vote::Provable);
    let v2 = audit_user(&case, Vote::LongLived);
    let v3 = audit_user(&case, Vote::Consistent);
    assert!(v1.contains(&cited), "票 1 的 user 段没有转录原文 ⇒ 那一票在结构上无法回答");
    assert!(!v1.contains("邻居独有串"), "票 1 不看邻居：它只回答能不能重读出来");
    assert!(!v2.contains(&cited) && !v2.contains("邻居独有串"),
            "票 2 只问一行之后的事：带上下文会让它滑回可复核性");
    assert!(v3.contains("邻居独有串"), "票 3 的 user 段没有邻居 ⇒ 矛盾无从判");
    assert!(!v3.contains(&cited), "票 3 不读转录：它比的是行与行");
}

#[test]
fn vote_three_passes_only_when_the_row_is_free_of_contradiction() {
    // 问句极性 ↔ 判定档位 的绑定。`audit_system` 的 STEP 3 教的是"所述条件成立 ⇒ holds=true"，
    // 三票共用这一个契约，所以票 3 的"条件"必须是我们**期望成立**的那件事（不自相矛盾），
    // 算式那一侧才会是 `!holds[2] ⇒ Low`。
    // 两条断言缺一不可：`MockBackend` 按调用序号喂回复、不看问句内容，所以档位那一格在极性
    // 翻回负向时照旧全绿——只有第一格（读渲染出来的问句）能挡住它。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());

    let row = the_row(&root);
    let case = build_case(root.transcript(), root.memory(), &row).unwrap();
    let asked = audit_user(&case, Vote::Consistent);
    assert!(asked.contains("free of any flat contradiction"),
            "票 3 的问句不是正向条件陈述 ⇒ 它问的东西与 `holds` 契约相反：{asked}");

    // 第二格需要一个**真邻居**：票 3 的上下文来自 `scored_siblings` 的词面重叠门槛，
    // 标题照抄被审那一行才进得了候选（同上一条用例的形状）。
    // 邻居自己的 verdict 先按掉：`unreviewed_rows` 只收未审行，留着它本轮就审两行，
    // 三条回复的脚本会被第二行读空 ⇒ 红在错误上而不是红在档位上。
    let neighbour = card_as_owner_of(&root, &row, &row.title,
                                     &format!("{} 邻居独有串", row.body), 1_500);
    set_verdict(root.memory(), &neighbour, Verdict::Low, 1_500).unwrap();
    assert_eq!(unreviewed_rows(root.memory(), 10).unwrap().len(), 1, "本轮恰好只有那一行未审");

    // 票 1 真、票 2 真、票 3 假 ⇒ 与邻居打架的行只能降档。
    let backend = MockBackend::scripted(vec![Ok(TRUE_REPLY.into()), Ok(TRUE_REPLY.into()),
                                             Ok(r#"{"holds":false}"#.into())]);
    let out = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!(backend.calls(), 3, "三票一次都没问 ⇒ 这一条读的是\"没证据 ⇒ 降档\"那一支，不是票 3");
    assert_eq!((out.rows_audited, out.passed, out.low, out.failed), (1, 0, 1, 0),
        "票 3 不成立就被升成 passed = 把自相矛盾的行当定案；压成 failed 又等于给了它消音权");
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM retirement", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "§2.18 连带 1：消音权只在票 1");
    assert_eq!(lookup(root.memory(), &row.key).unwrap().unwrap().verdict, Verdict::Low);
}

#[test]
fn a_row_whose_evidence_is_gone_is_demoted_not_suppressed() {
    // P-43 的第三条 + §2.18 连带 0 的末尾：判不了 ≠ 证据反对它。
    // "没了证据"用真链路造：抽取之后把 turns 表清空（等价于 §4 的 purge 之后重跑审核）。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    root.transcript().conn().execute("DELETE FROM turns", []).unwrap();
    let backend = MockBackend::scripted(vec![]);   // 一旦被调用就是 Timeout ⇒ Err
    let out = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!((out.rows_audited, out.low, out.failed, backend.calls()), (1, 1, 0, 0));
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM retirement", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "没有证据就被写进抑制账 = 把「没读到」读成「证据反对它」");
    assert_eq!(root.memory().conn().query_row::<String, _, _>(
        "SELECT verdict FROM card LIMIT 1", [], |r| r.get(0)).unwrap(), "low");
}

#[test]
fn an_unreadable_turns_table_is_an_error_not_a_demoted_row() {
    // 与上一条成对：`DELETE FROM turns` 是"表在、行没了"⇒ 合法历史状态 ⇒ 降档；
    // `DROP TABLE turns` 是"表读不出来"⇒ 运维失败 ⇒ `Err`。把后者折成前者的代价是整批：
    // 每一个未审行都被永久写成 `low` 而 `verdict` 一旦离开 unreviewed 就永久离开审核游标
    // （坑 P-71 与坑 P-43 在同一格相遇）。折叠版（`.ok()`）在这里读出 `Ok(low = 1)`。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    root.transcript().conn().execute("DROP TABLE turns", []).unwrap();
    let backend = MockBackend::scripted(vec![]);   // 一旦开口就是 Timeout ⇒ 红在错误上而不是档位上
    let e = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap_err();
    assert!(matches!(&e, DistillError::Store(rc_store::StoreError::Sql(_))), "{e}");
    assert_eq!(backend.calls(), 0, "证据读不出来就一票都不问：问下去只会拿到凭空的判断");
    let tail = unreviewed_rows(root.memory(), 10).unwrap();
    assert_eq!(tail.len(), 1, "失败的那一行必须留在游标上，下一轮重新判");
    assert_eq!(lookup(root.memory(), &tail[0].key).unwrap().unwrap().verdict, Verdict::Unreviewed);
}

#[test]
fn a_shape_error_writes_nothing_at_all() {
    // P-36：形状错与"票不成立"分家。票 2 的回复是字符串不是布尔 ⇒ Err 且一行都不写。
    // 前提：`extracted` 的库里**只有一行**未审，所以这一条测的是"出错那一行没被写"，不是整轮原子。
    // 一轮里若有多行，先头那些已经落了库（`audit_row` 的写经 `writers_c` 即时提交），
    // 整轮的失败处置真正的消费者是任务 9 的 `run_cycle_once`——它收到 `Err` 之后不推水位。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let backend = MockBackend::scripted(vec![Ok(TRUE_REPLY.into()), Ok(r#"{"holds":"true"}"#.into())]);
    let e = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap_err();
    assert!(matches!(e, DistillError::Audit(_)), "{e}");
    assert_eq!(backend.calls(), 2, "形状错当场中止这一行：票 3 不该再问");
    let row = the_row(&root);
    assert_eq!(row.verdict, Verdict::Unreviewed, "verdict 还在 unreviewed ⇒ 下一轮重新判这一行");
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM retirement", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "一次 JSON 拼写错误不许让一条真记忆永久进抑制账");
    // 下一轮**照旧**读得到它：游标没被形状错推进。
    assert_eq!(unreviewed_rows(root.memory(), 10).unwrap().len(), 1);
}

#[test]
fn parse_vote_matrix_of_six_replies() {
    assert!(parse_vote(TRUE_REPLY).unwrap());
    assert!(!parse_vote(r#"{"holds":false}"#).unwrap());
    for (bad, want) in [
        ("nope", AuditError::NotAnObject),
        ("[1]", AuditError::NotAnObject),
        ("{}", AuditError::MissingHolds),
        (r#"{"holds":"true"}"#, AuditError::HoldsNotBool),
        (r#"{"holds":1}"#, AuditError::HoldsNotBool),
    ] {
        assert_eq!(parse_vote(bad).unwrap_err(), want, "{bad}");
    }
}

#[test]
fn the_pass_holds_at_max_per_pass_and_the_tail_is_not_lost() {
    // 与判同的 A16 对照：审核的游标就是 verdict 列 ⇒ 超出 `MAX_PER_PASS` 的那些行
    // **留在原地**，下一次 pass 继续。这一条测的是"下一轮补判"这条通路真的存在。
    // 六行**全部**用 `put_card` 直接造、出处指向库里不存在的 uid：这样每一行都走
    // "没证据 ⇒ Low、零调用"那一支。这一条要测的是行数上限，不是票，所以它刻意不走真抽取——
    // 走了抽取的那一行有证据，会向 `quiet` 那个空脚本开口，而空脚本回的是 `Timeout` ⇒ 整条用例红在错误上。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for i in 0..6 {
        put_card(root.memory(), &FactContent { kind: "note".into(), title: format!("垫行{i}"),
            body: format!("没有证据的正文{i}"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-tail".into() },
            valid_to_ms: None },
            &[Provenance { turn_uid: "d".repeat(64), anchor_role: AnchorRole::Human }], 2_000 + i)
            .unwrap();
    }
    assert_eq!(unreviewed_rows(root.memory(), 100).unwrap().len(), 6);
    let quiet = MockBackend::scripted(vec![]);   // 全部走"没证据 ⇒ Low"那一支，一次都不开口
    let first = run_audit(root.transcript(), root.memory(), &quiet, 4_000).unwrap();
    assert_eq!(first.rows_audited, 4, "= MAX_PER_PASS：一次 pass 的行数由这一个常量定");
    assert_eq!((first.low, quiet.calls()), (4, 0), "六行里一行都没开口：这一条测上限，不测票");
    assert_eq!(unreviewed_rows(root.memory(), 100).unwrap().len(), 2, "剩下的两行没被丢掉");
    let second = run_audit(root.transcript(), root.memory(), &quiet, 4_001).unwrap();
    assert_eq!((second.rows_audited, second.low), (2, 2), "下一轮把尾巴跑完：审核没有 A16 那一型");
}

#[test]
fn audit_moves_nothing_but_the_verdict_column() {
    // §2.15 的"三个作业各有各的推进，互不借道"：审核不动水位、不动 extract_state、不动 cooldown。
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let l = root.ledger();
    // `water::read` 收 `&LedgerHandle`，而 `root.ledger()` 已经是 `&LedgerHandle`——写成 `&l` 是 `&&_`，编译不过。
    let cursor_before = water::read(l, "distill", "dev-batch-A/sess-batch-A").unwrap();
    let state_before = l.conn().query_row::<i64, _, _>(
        "SELECT count(*) FROM extract_state", [], |r| r.get(0)).unwrap();
    // `card` 在 memory.db，不在水位库：这条计数必须走 `root.memory()`，
    // 写成 `l.conn()` 的那一版会在 SQL 层就报"no such table"，红得与判据无关。
    let cooldown_before = root.memory().conn().query_row::<i64, _, _>(
        "SELECT count(*) FROM card WHERE cooldown_until_ms IS NOT NULL", [], |r| r.get(0)).unwrap();
    let backend = MockBackend::scripted(vec![Ok(TRUE_REPLY.into()); 3]);
    run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!(water::read(l, "distill", "dev-batch-A/sess-batch-A").unwrap(), cursor_before,
        "审核推水位 = 把批次游标与行游标混成一件");
    assert_eq!(l.conn().query_row::<i64, _, _>("SELECT count(*) FROM extract_state", [], |r| r.get(0)).unwrap(),
               state_before);
    assert_eq!(cooldown_before, root.memory().conn().query_row::<i64, _, _>(
        "SELECT count(*) FROM card WHERE cooldown_until_ms IS NOT NULL", [], |r| r.get(0)).unwrap());
}

// ── R2-1：空邻居池不得被读成"没有矛盾" ────────────────────────────────────────

/// 探针**没有归属行** ⇒ 邻居池是"不可用"（`rows_in_scope` 的 `?6=1` 把整池清空），
/// 渲染必须明写一行，而不是让票 3 的 user 段看起来"下方没有邻居 = 没有矛盾"。
#[test]
fn a_probe_without_owner_renders_the_neighbour_pool_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let row = the_row(&root);
    // 归属行删掉：`rows_in_scope` 读探针归属落到 `(NULL, NULL, 0)`，SQL 自己就是空的。
    root.memory().conn()
        .execute("DELETE FROM resource_owner WHERE mem_table='card' AND mem_id=?1",
                 rusqlite::params![row.key.id])
        .unwrap();
    let case = build_case(root.transcript(), root.memory(), &row).unwrap();
    assert!(case.siblings_unavailable, "没有归属行 ⇒ 池**不可用**，不是'查过、没有'");
    assert!(case.siblings.is_empty(), "不可用的池当然取不出邻居");
    let v3 = audit_user(&case, Vote::Consistent);
    assert!(v3.contains("NEIGHBORS: unavailable (no owner for this probe)"),
            "票 3 的 user 段必须明写池不可用：{v3}");
    // 票 1 / 票 2 本来就不看邻居：这一行只属于票 3。
    assert!(!audit_user(&case, Vote::Provable).contains("NEIGHBORS: unavailable"));
    assert!(!audit_user(&case, Vote::LongLived).contains("NEIGHBORS: unavailable"));
}

/// 有归属、确实查过、没有相关邻居 ⇒ **普通空段**：不含 unavailable 标记。
/// 两种"没有"必须是两句话，否则这条修正只是换了个说法。
#[test]
fn a_probe_with_owner_and_no_neighbours_renders_a_plain_empty_pool() {
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let row = the_row(&root);
    let case = build_case(root.transcript(), root.memory(), &row).unwrap();
    assert!(!case.siblings_unavailable, "这一行有归属行：池可用，只是没有候选");
    assert!(case.siblings.is_empty(), "夹具里没有邻居");
    let v3 = audit_user(&case, Vote::Consistent);
    assert!(!v3.contains("unavailable"), "查过而没有 ≠ 不可用：{v3}");
    assert!(!v3.contains("NEIGHBOUR |"), "空池没有邻行可渲染：{v3}");
}

/// 行为用例：池不可用时模型仍答 `holds=true` ⇒ 程序**照契约收下**（不是硬拦）。
///
/// R2-1 的修正只在信息与判据两面：空池本身不等于 reject，程序侧不加闸。
#[test]
fn a_pass_on_an_unavailable_pool_is_still_accepted_by_the_program() {
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let row = the_row(&root);
    root.memory().conn()
        .execute("DELETE FROM resource_owner WHERE mem_table='card' AND mem_id=?1",
                 rusqlite::params![row.key.id])
        .unwrap();
    let rec = Recorder::new(vec![Ok(TRUE_REPLY.into()), Ok(TRUE_REPLY.into()), Ok(TRUE_REPLY.into())]);
    let out = run_audit(root.transcript(), root.memory(), &rec, 4_000).unwrap();
    assert_eq!((out.rows_audited, out.passed, out.low, out.failed), (1, 1, 0, 0),
               "空池本身不等于 reject：程序不加硬闸");
    let users = rec.users.borrow().clone();
    assert!(users[2].contains("NEIGHBORS: unavailable"), "票 3 的原文必须带着这一行：{}", users[2]);
}

/// W15a：audit 的 `passed` 必须留下一条 `source='audit'` 的背书事件——
/// 现网那 36 张"audit 自盖"的卡正是这一支盖的，来源从此可追溯。
#[test]
fn an_audit_pass_records_an_audit_backed_event() {
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    let backend = MockBackend::scripted(vec![Ok(TRUE_REPLY.into()), Ok(TRUE_REPLY.into()),
                                            Ok(TRUE_REPLY.into())]);
    let out = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!((out.rows_audited, out.passed), (1, 1));
    let (verdict, source, at_ms): (String, String, i64) = root.memory().conn().query_row(
        "SELECT c.verdict, e.source, e.at_ms FROM card c
           JOIN card_verdict_event e ON e.card_id = c.id
          ORDER BY e.id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((verdict.as_str(), source.as_str(), at_ms), ("passed", "audit", 4_000),
               "audit 盖的 passed 必须留下一条 audit 来源的事件");
    // 事件里的判决与列上的判决是同一档：两本账不许各说各的。
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM card_verdict_event", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "一次判定只许追加一条事件");
}

/// W15a：空证据的低档支同样留 `audit` 来源（`verdict='low'`）——降档也是 audit 盖的章。
#[test]
fn an_evidence_empty_demotion_records_an_audit_backed_low_event() {
    let dir = tempfile::tempdir().unwrap();
    let root = extracted(dir.path());
    root.transcript().conn().execute("DELETE FROM turns", []).unwrap();
    let backend = MockBackend::scripted(vec![]);
    let out = run_audit(root.transcript(), root.memory(), &backend, 4_000).unwrap();
    assert_eq!((out.low, backend.calls()), (1, 0));
    let (verdict, source): (String, String) = root.memory().conn().query_row(
        "SELECT c.verdict, e.source FROM card c
           JOIN card_verdict_event e ON e.card_id = c.id
          ORDER BY e.id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((verdict.as_str(), source.as_str()), ("low", "audit"));
}
