//! 五条全部围绕 P-42：`apply_merges` 是**全阶段唯一**落 `superseded_by` 的地方，
//! 而它唯一的失效形状是"照单全收"。五张 plan 形状：一条正常、一对互指、一条追顶、一条跨表、一条指向幽灵。
// 本文件不需要 common 夹具：五条用例全部用 put_card / put_entity 直接造行

use rc_distill::audit::apply_merges;
use rc_distill::dedupe::MergePlan;
use rc_distill::DistillError;
use rc_memory::{lookup, put_card, put_entity, AnchorRole, EntityContent, FactContent, MemError,
                MemKey, Provenance, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 5_000;
fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-merge".into() } }
fn prov() -> Vec<Provenance> { vec![Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }] }
fn card(root: &Root, title: &str) -> MemKey {
    put_card(root.memory(), &FactContent { kind: "lesson".into(), title: title.into(),
        body: "一条独立的正文".into(), scope: scope(), valid_to_ms: None }, &prov(), NOW).unwrap()
}
fn plan(w: &MemKey, l: &MemKey) -> MergePlan { MergePlan { winner: w.clone(), loser: l.clone() } }
fn alive(root: &Root, k: &MemKey) -> bool { lookup(root.memory(), k).unwrap().unwrap().superseded_by.is_none() }

#[test]
fn one_plan_marks_the_loser_and_never_the_winner() {
    // P-37 的两半都在这一条里：loser 被顶、winner 不动，外加自指那一支被第二道闸挡下。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = card(&root, "留下");
    let b = card(&root, "被并");
    let rep = apply_merges(root.memory(), &[plan(&a, &b)], NOW).unwrap();
    assert_eq!((rep.merged, rep.refused), (1, 0));
    assert!(alive(&root, &a) && !alive(&root, &b));
    assert_eq!(lookup(root.memory(), &b).unwrap().unwrap().superseded_by.as_deref(), Some(a.id.as_str()));
    // 自指：构造点已经挡了（`parse_judge` 的 filter），这里是 `apply_merges` 自己的第二道。
    let rep = apply_merges(root.memory(), &[plan(&a, &a)], NOW).unwrap();
    assert_eq!((rep.merged, rep.refused), (0, 1), "自指的 plan 落笔就是\"被自己顶掉\"");
}

#[test]
fn a_mutually_superseding_plan_pair_leaves_exactly_one_row_alive() {
    // P-42 的执法点，也是本任务最坏那一档：两行一起消失且**永远回不来**
    // （`alive_where` 的第二支把它们两个都排除；写侧查账走 `dead_by_id_expr` 的第三支——行存在且
    //  `superseded_by` 非 NULL ⇒ 此后每一次重蒸都被算成命中抑制账而跳过，
    //  而 `ON CONFLICT DO NOTHING` 连一个错误都不报）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = card(&root, "甲");
    let b = card(&root, "乙");
    let rep = apply_merges(root.memory(), &[plan(&a, &b), plan(&b, &a)], NOW).unwrap();
    assert_eq!((rep.merged, rep.refused), (1, 1), "{rep:?}：两条都落 = 环");
    let alive_ids: Vec<String> = [a.clone(), b.clone()].iter()
        .filter(|k| alive(&root, k)).map(|k| k.id.clone()).collect();
    assert_eq!(alive_ids.len(), 1, "必须**恰有一行**还活着：{} 全消失了", alive_ids.len());
}

#[test]
fn a_second_plan_on_an_already_superseded_loser_is_refused() {
    // 重复记账的形状：同一个 loser 被两条 plan 顶两次 ⇒ 第二次必须被挡（它已经退场了）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let x = card(&root, "甲留");
    let y = card(&root, "乙留");
    let loser = card(&root, "被并");
    let rep = apply_merges(root.memory(), &[plan(&x, &loser), plan(&y, &loser)], NOW).unwrap();
    assert_eq!((rep.merged, rep.refused), (1, 1));
    assert_eq!(lookup(root.memory(), &loser).unwrap().unwrap().superseded_by.as_deref(),
               Some(x.id.as_str()), "第二顶把指针改向 y = 一条行两段退场账，谁顶的读不出来");
}

#[test]
fn a_cross_table_plan_is_refused() {
    // 同表这道闸的形状：`supersede` 把 winner 的 id 原样写进 **loser 那张表**，一条 card→anchor 的
    // plan 于是给那张卡片留下一个在 card 里指不到任何行的 `superseded_by`——而 `alive_where`
    // 的第二支只看 `IS NULL` ⇒ 那行永久消失。`dedupe.rs` 的两处构造点永远同表，所以这一道
    // 与自指那道闸同样只是纵深防御（P-37 的立场：不许假设上游守约）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let loser = card(&root, "被并的卡片");
    let winner = put_entity(root.memory(), &EntityContent { kind: "org".into(),
        title: "留下的实体".into(), scope: scope(), aliases: vec![] }, &prov(), NOW).unwrap();
    let rep = apply_merges(root.memory(), &[plan(&winner, &loser)], NOW).unwrap();
    assert_eq!((rep.merged, rep.refused), (0, 1),
        "{rep:?}：跨表落笔 = 给卡片写一个永远查不到对应行的顶替指针");
    // 有牙的是下面那一格：指针只会落在 loser 那张表上。上面这一格断的是实体仍可读到、
    // 没被当作 loser 顶掉——`anchor` 在 DDL 里根本没有 `superseded_by` 列（读侧 `select_sql`
    // 用字面量 NULL 补齐同名形状），所以它天然为 None，留在这里是为了"winner 侧没被动过"。
    assert_eq!(lookup(root.memory(), &winner).unwrap().unwrap().superseded_by, None);
    assert_eq!(lookup(root.memory(), &loser).unwrap().unwrap().superseded_by, None,
        "闸门被拆掉时这一格红：卡片带着一个 anchor 的 id 退场");
}

#[test]
fn a_plan_naming_a_row_that_is_not_there_is_an_error_not_a_refusal() {
    // refused 与 Err 的分界：refused 是**业务结论**（这一条 plan 不必落笔），
    // 句柄不存在是**库不一致**（判同刚说它与某行同物，而库里没这行）。后者必须响。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = card(&root, "甲");
    let ghost = MemKey { table: rc_memory::MemTable::Card, id: "9".repeat(64) };
    let e = apply_merges(root.memory(), &[plan(&a, &ghost)], NOW).unwrap_err();
    assert!(matches!(&e, DistillError::Memory(MemError::NotFound(k)) if k == &ghost), "{e}");
    assert!(alive(&root, &a), "Err 之前不许已经落笔：一个事务的语义在这里是逐条 plan 的先后");
    assert!(lookup(root.memory(), &ghost).unwrap().is_none());
}
