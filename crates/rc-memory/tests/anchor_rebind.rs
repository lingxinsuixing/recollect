//! 锚点一键换绑（D27）：把挂在旧锚上的关联数据搬到新锚，旧锚降为"这个实体曾被这样称呼"的属性。
//!
//! 为什么必须有（用户 11-01）：**锚是在证据不全时被命名的**——"我同事"、"老方"、"那个客户"。
//! 锚不可换，后来的认知就回写到不了已经落库的判断上，描述会永久停在第一次猜的位置。
//!
//! 这片钉的是四件事：
//! 1. **重键而不是改列**：`state_assertion`/`fact`/`mention` 的行 id 都把主体/锚算进哈希，
//!    所以换绑必须按新锚重算 id、`INSERT OR IGNORE`（同一件事重复 ⇒ 自然合并、不长第二行）、
//!    再迁出处、删旧行。只 UPDATE 一列会让"id 可从内容复算"这条不变式失效。
//! 2. **零悬空**：12 个外键列之外，`memory_mention.anchor_id` 没有 FK 保护，
//!    按 `(mem_table, mem_id)` 存的 `provenance`/`resource_owner`/`attention` 也必须跟着走。
//! 3. **旧锚出声不消失**：`status='merged'`、它的正名并进新锚的别名，整笔记在
//!    `memory_op(op='MERGE')` 的 before/after 里——这就是"原锚点作为关联属性"的落点。
//!    （`attribute` 表到今天没有生产者、id 规则未核实 ⇒ 这里不往它写，见 §10.143。）
//! 4. **默认只看不动**：`--dry-run` 一格不写；没核实过 id 规则的表若有行，直接拒绝 apply。
use rc_memory::identity::{compute_mem_id, compute_state_id};
use rc_memory::keys::MemTable;
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::display_name;
use rc_memory::rebind::{rebind_apply, rebind_plan, RebindError};
use rc_memory::writers_state::{put_state_rows, StateWriteItem};
use rc_memory::BatchCtx;
use rc_memory::writers_v4::{write_a1_with_owner, A1Anchor, A1Batch};
use rc_memory::Provenance;
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_500_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SCOPE: &str = "proj-rebind";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: SCOPE.into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-rebind".into()),
        owner_agent_id: "agent-rebind".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-rebind".into(), device_id: "dev-rebind".into() }
}

fn uid(index: usize) -> String {
    format!("{:064x}", 0xa0 + index)
}

/// 建一格锚（走生产的 A1 写入口：正名、别名、出处、归属一次到位），返回锚 id。
fn anchor(m: &MemoryHandle, canonical: &str, aliases: &[&str], turn: usize) -> String {
    let batch = A1Batch {
        anchors: vec![A1Anchor {
            reference: "a1".into(),
            kind: "person".into(),
            canonical: canonical.into(),
            aliases: aliases.iter().map(|a| (*a).to_string()).collect(),
            reality_status: "real".into(),
            evidence: vec![Provenance { turn_uid: uid(turn), anchor_role: AnchorRole::Human }],
            stable: true,
        }],
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: Vec::new(),
    };
    let out = write_a1_with_owner(m, &batch, &ctx(), &owner(), &scope(), "run-rebind", NOW).unwrap();
    out.by_ref["a1"].id.clone()
}

/// 在指定主体锚上落一条状态行。
fn state_on(m: &MemoryHandle, subject: &str, dimension: &str, value: &str, turn: usize) -> String {
    let item = StateWriteItem {
        subject_anchor_id: subject.into(),
        dimension: dimension.into(),
        value_after: value.into(),
        mode: "expressed".into(),
        text: value.to_string(),
        evidence: vec![(uid(turn), AnchorRole::Human)],
        scope: scope(),
        source_ms: SRC_MS,
    };
    put_state_rows(m, &[item], &ctx(), &owner(), NOW).unwrap();
    compute_state_id(subject, dimension, value, &uid(turn), &scope())
}

fn scalar(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 一格锚上某条别名的 `(valid_from_ms, source_turn_uid)`（"这名字当初在哪一轮被说过"）。
fn alias_row(m: &MemoryHandle, anchor_id: &str, alias: &str) -> (Option<i64>, Option<String>) {
    m.conn()
        .query_row(
            "SELECT valid_from_ms, source_turn_uid FROM alias WHERE anchor_id=?1 AND alias=?2",
            rusqlite::params![anchor_id, alias],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// 库里还指向某一格锚的引用总数（12 个外键列 + 无 FK 的 memory_mention + 三张按 id 存的表）。
fn references_to(m: &MemoryHandle, anchor_id: &str) -> i64 {
    let cols = [
        "alias.anchor_id",
        "mention.anchor_id",
        "attribute.anchor_id",
        "relation.subject_anchor_id",
        "relation.object_anchor_id",
        "fact.subject_anchor_id",
        "fact.object_anchor_id",
        "fact.place_anchor_id",
        "fact.speaker_anchor_id",
        "state_assertion.subject_anchor_id",
        "utterance.speaker_anchor_id",
        "utterance.addressee_anchor_id",
        "memory_mention.anchor_id",
    ];
    let mut total = 0i64;
    for col in cols {
        let (table, column) = col.split_once('.').unwrap();
        let n: i64 = m
            .conn()
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
                rusqlite::params![anchor_id],
                |r| r.get(0),
            )
            .unwrap();
        total += n;
    }
    for table in ["provenance", "resource_owner", "attention"] {
        total += m
            .conn()
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE mem_table='anchor' AND mem_id=?1"),
                rusqlite::params![anchor_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
    }
    total
}

/// 1：重键 + 零悬空 + 旧锚出声。
#[test]
fn rebinding_moves_every_reference_and_rekeys_the_state_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &["老王"], 1);
    let winner = anchor(m, "张三", &[], 2);
    let old_state = state_on(m, &loser, "mood", "很烦", 3);

    let plan = rebind_plan(m, &loser, &winner).unwrap();
    assert_eq!(plan.states, 1, "预演要报出会动几行");
    assert!(plan.blockers.is_empty(), "这一形没有阻塞项：{:?}", plan.blockers);
    // 预演一格都不写。
    assert_eq!(rebind_plan(m, &loser, &winner).unwrap().states, 1);
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor"), 2);

    let report = rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();
    assert_eq!(report.states_moved, 1);

    // 旧行不再存在，新行按新主体可复算
    let (kept_id, subject, value): (String, String, String) = m
        .conn()
        .query_row(
            "SELECT id, subject_anchor_id, value_after FROM state_assertion ORDER BY id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(subject, winner, "状态行现在属于新锚");
    assert_eq!(value, "很烦");
    assert_eq!(
        kept_id,
        compute_state_id(&winner, "mood", "很烦", &uid(3), &scope()),
        "换绑后的 id 必须等于按新主体重算出来的那个（id 可从内容复算这条不变式）"
    );
    assert!(old_state != kept_id, "旧 id 不再被引用");

    // 出处跟着走：那一轮仍是这条状态的证据
    let proven: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM provenance WHERE mem_table='state_assertion' AND mem_id=?1 AND turn_uid=?2",
            rusqlite::params![kept_id, uid(3)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(proven, 1, "换绑不许把出处丢了");

    // 旧锚：不删、盖章、并进新锚的别名、留一笔 MERGE
    let status: String = m
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", rusqlite::params![loser], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "merged", "旧锚转成 merged——这是它的第一生产者");
    let aliases: Vec<String> = m
        .conn()
        .prepare("SELECT alias FROM alias WHERE anchor_id=?1 ORDER BY alias")
        .unwrap()
        .query_map([&winner], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(aliases.contains(&"我同事".to_string()), "旧锚的正名并进新锚别名：{aliases:?}");
    assert!(aliases.contains(&"老王".to_string()), "旧锚原有的别名一起搬：{aliases:?}");
    // 旧锚的"身份残留"落在**别名 + 台账**两处：`attribute` 表到今天没有生产者，
    // 它的 id 算法没被核实过 ⇒ 这里不发明一套（那正是静默损坏的入口）。
    let ops_json: String = m
        .conn()
        .query_row(
            "SELECT before_json || '|' || after_json FROM memory_op
              WHERE op='MERGE' AND mem_table='anchor' AND mem_id=?1",
            rusqlite::params![loser],
            |r| r.get(0),
        )
        .unwrap();
    assert!(ops_json.contains("我同事"), "MERGE 台账要写下旧锚的正名：{ops_json}");
    assert!(ops_json.contains(&loser), "还要写下来自哪一格：{ops_json}");
    assert!(ops_json.contains(&winner), "以及去了哪一格：{ops_json}");
    let ops: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM memory_op WHERE op='MERGE' AND mem_table='anchor' AND mem_id=?1",
            rusqlite::params![loser],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ops, 1, "整笔要有痕迹");

    // 零悬空：没有任何一行还指着旧锚
    assert_eq!(references_to(m, &loser), 0, "换绑后不许还有引用挂在 merged 锚上");
    assert!(references_to(m, &winner) > 0);
    // 锚的 kind 不参与归属变更
    assert_eq!(
        compute_mem_id(MemTable::Anchor, "person", "张三", "", &scope()),
        winner,
        "新锚的身份一格没动（换绑不改身份）"
    );
}

/// 2：同一件事重复 ⇒ 合并不长第二行，且报出"合并掉几行"。
#[test]
fn a_rebind_that_collides_with_an_identical_row_merges_instead_of_duplicating() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "老方", &[], 1);
    let winner = anchor(m, "方哲", &[], 2);
    state_on(m, &loser, "mood", "很累", 3);
    // 新锚上同一维度同值同证据轮 ⇒ 换绑过去就是同一格
    state_on(m, &winner, "mood", "很累", 3);
    assert_eq!(scalar(m, "SELECT count(*) FROM state_assertion"), 2);

    let plan = rebind_plan(m, &loser, &winner).unwrap();
    assert_eq!(plan.collapse, 1, "预演就该报出会合并掉一行");
    rebind_apply(m, &loser, &winner, "同一人", "agent", NOW).unwrap();
    assert_eq!(
        scalar(m, "SELECT count(*) FROM state_assertion"),
        1,
        "同主体·同维度·同值·同证据轮 ⇒ 合并成一格当前状态（不是并排两行）"
    );
    assert_eq!(references_to(m, &loser), 0);
}

/// 3：拒绝形状（闭集码）——跨 kind / 跨 scope / 自己换自己 / 目标已 merged / 链式换绑。
#[test]
fn the_rebind_refuses_every_shape_that_would_damage_the_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let person = anchor(m, "张三", &[], 1);
    let concept = {
        let batch = A1Batch {
            anchors: vec![A1Anchor {
                reference: "a1".into(),
                kind: "concept".into(),
                canonical: "砍价".into(),
                aliases: Vec::new(),
                reality_status: "abstract".into(),
                evidence: vec![Provenance { turn_uid: uid(2), anchor_role: AnchorRole::Human }],
                stable: true,
            }],
            mentions: Vec::new(),
            issues: Vec::new(),
            unresolved: Vec::new(),
        };
        write_a1_with_owner(m, &batch, &ctx(), &owner(), &scope(), "run-x", NOW)
            .unwrap()
            .by_ref["a1"]
            .id
            .clone()
    };
    assert_eq!(rebind_plan(m, &person, &concept).unwrap_err(), RebindError::KindMismatch);
    assert_eq!(rebind_plan(m, &person, &person).unwrap_err(), RebindError::SameAnchor);
    assert_eq!(
        rebind_plan(m, &person, &"f".repeat(64)).unwrap_err(),
        RebindError::Missing
    );

    // 跨 scope 的格：身份域不同，两格并存是合法的，不换。
    let other_scope = Scope { kind: ScopeKind::Project, key: "proj-other".into() };
    let far = {
        let batch = A1Batch {
            anchors: vec![A1Anchor {
                reference: "a1".into(),
                kind: "person".into(),
                canonical: "李四".into(),
                aliases: Vec::new(),
                reality_status: "real".into(),
                evidence: vec![Provenance { turn_uid: uid(3), anchor_role: AnchorRole::Human }],
                stable: true,
            }],
            mentions: Vec::new(),
            issues: Vec::new(),
            unresolved: Vec::new(),
        };
        write_a1_with_owner(m, &batch, &ctx(), &owner(), &other_scope, "run-s", NOW)
            .unwrap()
            .by_ref["a1"]
            .id
            .clone()
    };
    assert_eq!(rebind_plan(m, &person, &far).unwrap_err(), RebindError::ScopeMismatch);

    // 换过一次就别再换：merged 锚既不能当 from 也不能当 into（链式换绑会让"谁是本人"说不清）。
    let loser = anchor(m, "我同事", &[], 4);
    rebind_apply(m, &loser, &person, "同一人", "user", NOW).unwrap();
    assert_eq!(rebind_plan(m, &loser, &person).unwrap_err(), RebindError::AlreadyMerged);
    let third = anchor(m, "王五", &[], 5);
    assert_eq!(rebind_plan(m, &third, &loser).unwrap_err(), RebindError::TargetMerged);
}

/// 4：没核实过 id 规则的表一旦有行，apply 必须拒绝（预演仍给数），而不是悄悄 UPDATE 坏了重放。
#[test]
fn a_table_without_a_verified_id_rule_blocks_the_apply_rather_than_guessing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &[], 1);
    let winner = anchor(m, "张三", &[], 2);
    m.conn()
        .execute(
            "INSERT INTO relation(id, subject_anchor_id, predicate, object_anchor_id, mode,
                                 confidence, text, scope_kind, scope_key, source_agent, device_id,
                                 created_at_ms, norm_version)
             VALUES('r1',?1,'role',?2,'stated',0.8,'我同事当的介绍人','project',?3,
                    'agent-rebind','dev-rebind',?4,1)",
            rusqlite::params![loser, winner, SCOPE, NOW],
        )
        .unwrap();

    let plan = rebind_plan(m, &loser, &winner).unwrap();
    assert!(
        plan.blockers.contains(&"relation_unverified_id_rule"),
        "relation 有行 ⇒ apply 必须被挡住：{:?}",
        plan.blockers
    );
    assert_eq!(
        rebind_apply(m, &loser, &winner, "同一人", "user", NOW).unwrap_err(),
        RebindError::Blocked
    );
    assert_eq!(
        scalar(m, "SELECT count(*) FROM state_assertion"),
        0,
        "被挡下时一格都不该动"
    );
}

/// 5：`fact` 的 `place_anchor_id` / `speaker_anchor_id` 不是 id 输入 ⇒ 换绑要**就地改列**，
/// 而不是留在 merged 锚上。留着的后果正是用户说的那句话：关于锚点的描述偏离实体的真实位置。
#[test]
fn fact_columns_that_are_not_id_inputs_follow_the_anchor_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "那个客户", &[], 1);
    let winner = anchor(m, "张三", &[], 2);
    let subject = anchor(m, "李四", &[], 3);
    // 直接落一行事实：主体是第三方，旧锚占的是"地点"和"说话人"两格。
    m.conn()
        .execute(
            "INSERT INTO fact(id, subject_anchor_id, predicate, category, mode, text,
                              evidence_turn_uid, place_anchor_id, speaker_anchor_id,
                              scope_kind, scope_key, source_agent, device_id,
                              created_at_ms, norm_version)
             VALUES('f1',?1,'met','event','stated','李四在那个客户那儿见了张三',?2,?3,?4,
                    'project',?5,'agent-rebind','dev-rebind',?6,1)",
            rusqlite::params![subject, uid(4), loser, loser, SCOPE, NOW],
        )
        .unwrap();
    // 出处按 (mem_table, mem_id) 存 ⇒ 事实行 id 不变，它就不该跟着搬（这里钉住"不搬"）。
    m.conn()
        .execute(
            "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
             VALUES('fact','f1',?1,'human')",
            rusqlite::params![uid(4)],
        )
        .unwrap();

    let plan = rebind_plan(m, &loser, &winner).unwrap();
    assert!(plan.blockers.is_empty(), "地点/说话人不是 id 输入，不该把整笔挡掉：{:?}", plan.blockers);
    assert_eq!(plan.fact_refs, 2, "预演要报出有几列跟着改");

    let report = rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();
    assert_eq!(report.fact_refs_moved, 2);

    let (place, speaker, fact_subject): (Option<String>, Option<String>, String) = m
        .conn()
        .query_row(
            "SELECT place_anchor_id, speaker_anchor_id, subject_anchor_id FROM fact WHERE id='f1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(place.as_deref(), Some(winner.as_str()), "地点跟着换到新锚");
    assert_eq!(speaker.as_deref(), Some(winner.as_str()), "说话人跟着换到新锚");
    assert_eq!(fact_subject, subject, "主体不是它 ⇒ 那一格一格没动（事实行 id 因此不变）");
    let proven: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM provenance WHERE mem_table='fact' AND mem_id='f1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(proven, 1, "事实行没重键，它的出处一格都不该动");
    assert_eq!(references_to(m, &loser), 0, "换绑后不许还有引用挂在 merged 锚上");
}

/// 6（#22 第 1 条）：并别名要把这条称呼**自己的出处**带上，而不是盖成合并时刻。
/// `alias.source_turn_uid` 是"这名字当初在哪一轮被说过"，`valid_from_ms` 是它那次的时间；
/// 换绑不是它的生产时刻，补 `now_ms` 等于伪造日期。
#[test]
fn a_moved_alias_keeps_the_provenance_it_was_created_with() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &["老王"], 1);
    let winner = anchor(m, "张三", &[], 2);
    // 这条别名当初被说的那一轮：时间在过去、来源是 uid(7)。
    m.conn()
        .execute(
            "UPDATE alias SET valid_from_ms=?1, source_turn_uid=?2
              WHERE anchor_id=?3 AND alias='老王'",
            rusqlite::params![NOW - 5_000, uid(7), loser],
        )
        .unwrap();
    let before = alias_row(m, &loser, "老王");
    assert_eq!(before, (Some(NOW - 5_000), Some(uid(7))), "建场：旧行两列都在");

    rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();

    let after = alias_row(m, &winner, "老王");
    assert_eq!(
        after, before,
        "并别名要把 (valid_from_ms, source_turn_uid) 原样带上（NULL 就 NULL，有值就有值）"
    );
    assert_ne!(after.0, Some(NOW), "valid_from_ms 不许被伪造成合并时刻（NOW）");
}

/// 7（#22 第 2 条）：旧锚的正名并成新锚的别名时，`valid_from_ms = NULL`（正常别名的生产者
/// `writers_v4` 就不写这一列），`source_turn_uid` = 旧锚在 `provenance` 里**最早**的那一轮。
#[test]
fn the_old_canonical_carries_the_earliest_turn_the_old_anchor_was_named_in() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &[], 1);
    let winner = anchor(m, "张三", &[], 2);
    // 旧锚后来又被说过一轮：期望值必须是**最早**那一轮，不是"随便一轮"。
    m.conn()
        .execute(
            "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
             VALUES('anchor',?1,?2,'human')",
            rusqlite::params![loser, uid(5)],
        )
        .unwrap();
    // 期望值先独立查一次（不从 merge 结果反推）。
    let expected: String = m
        .conn()
        .query_row(
            "SELECT turn_uid FROM provenance WHERE mem_table='anchor' AND mem_id=?1
              ORDER BY rowid LIMIT 1",
            rusqlite::params![loser],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(expected, uid(1), "建场：最早的一轮是建锚那一轮");

    rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();

    let row = alias_row(m, &winner, "我同事");
    assert_eq!(row.0, None, "旧正名并成别名不发明日期：valid_from_ms 必须是 NULL");
    assert_eq!(
        row.1.as_deref(),
        Some(expected.as_str()),
        "source_turn_uid 要取旧锚在 provenance 里最早的那一轮"
    );
    let later = uid(5);
    assert_ne!(row.1.as_deref(), Some(later.as_str()), "不是后来又被说的那一轮");
}

/// 8（#22 第 3 条）：撞名（新锚已经有同名别名）只补 NULL，不覆盖已有值——
/// 与 `writers_v4` 重放那条 `WHERE source_turn_uid IS NULL` 同一条纪律。
#[test]
fn a_name_collision_only_fills_the_nulls_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &["老王", "老张"], 1);
    let winner = anchor(m, "张三", &[], 2);
    // 新锚上已经有同名别名：`老王` 两列都有值（不许被旧行覆盖），`老张` 两列都空（该补上）。
    m.conn()
        .execute(
            "INSERT INTO alias(anchor_id, alias, valid_from_ms, source_turn_uid)
             VALUES(?1,'老王',?2,?3)",
            rusqlite::params![winner, NOW - 9_000, uid(9)],
        )
        .unwrap();
    m.conn()
        .execute(
            "INSERT INTO alias(anchor_id, alias) VALUES(?1,'老张')",
            rusqlite::params![winner],
        )
        .unwrap();
    // 旧行两列都"更像现在"：撞名时若无条件覆盖，赢的就是这两行。
    m.conn()
        .execute(
            "UPDATE alias SET valid_from_ms=?1, source_turn_uid=?2 WHERE anchor_id=?3",
            rusqlite::params![NOW - 5_000, uid(7), loser],
        )
        .unwrap();

    rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();

    assert_eq!(
        alias_row(m, &winner, "老王"),
        (Some(NOW - 9_000), Some(uid(9))),
        "新锚那条别名两列都非 NULL ⇒ 一格都不覆盖"
    );
    assert_eq!(
        alias_row(m, &winner, "老张"),
        (Some(NOW - 5_000), Some(uid(7))),
        "新锚那条为空、旧行有值 ⇒ 两列都补上"
    );
}

/// 9（#22 第 4 条）：副作用读数——合并前后各读一次 `reader::display_name(winner)`。
///
/// **只量不说**：不把"显示名必须等于 canonical"写成断言（那是另一个决定，本轮只量）。
#[test]
fn display_name_before_and_after_the_merge_is_measured() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &["老王"], 1);
    let winner = anchor(m, "张三", &["三哥"], 2);
    // 新锚自己的可读名有一个真实的"现在"：比 NULL 新，但早于合并时刻。
    m.conn()
        .execute(
            "UPDATE alias SET valid_from_ms=?1 WHERE anchor_id=?2 AND alias='三哥'",
            rusqlite::params![NOW - 1_000, winner],
        )
        .unwrap();

    let before = display_name(m, &winner).unwrap();
    rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();
    let after = display_name(m, &winner).unwrap();
    eprintln!("#22 display_name(winner) before={before:?} after={after:?}");

    assert_eq!(before, "三哥", "建场：合并前显示的是新锚自己的称呼");
    // 功能面：旧称呼确实并过来了（这不是显示名断言）。
    let moved: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM alias WHERE anchor_id=?1 AND alias IN ('我同事','老王')",
            rusqlite::params![winner],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(moved, 2, "旧正名与旧别名都要落到新锚上");
}

/// 尾债④（W37 收尾）：rebind 成功后旧锚必须留一条 `anchor_merge` 指向行——
/// health scan 不再报 no_redirect，`redirect::resolve(from)` 也直接落到新锚
/// （此前只靠"引用已搬走 + Broken→self 兜底"撑着，解析面还要走一次回退）。
#[test]
fn a_rebind_leaves_a_pointing_row_so_resolution_lands_on_the_new_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let loser = anchor(m, "我同事", &["老王"], 1);
    let winner = anchor(m, "张三", &[], 2);
    state_on(m, &loser, "mood", "很烦", 3);

    rebind_apply(m, &loser, &winner, "认出是同一人", "user", NOW).unwrap();

    let pointing: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM anchor_merge WHERE from_id=?1 AND to_id=?2",
            rusqlite::params![loser, winner],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pointing, 1, "指向行恰好一条");
    let resolved = rc_memory::redirect::resolve(m, &loser).unwrap();
    assert_eq!(resolved.terminal_or_self(), winner, "解析直接落到新锚");
    // 幂等：同对 (from,to) 不长第二行。
    rebind_apply(m, &winner, &loser, "反向", "user", NOW).unwrap_err();
}
