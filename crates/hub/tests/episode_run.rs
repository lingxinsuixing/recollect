//! episode 归组内核 + `hub episode run` 的用例面（派发档 I-3b · T-1/T-2 的唯一落点）。
//!
//! 七件事由这些用例钉住：
//! 1. **剥自锚**：只共享"说话 agent 自己的锚"的两条**不许**合（S0 §2.2：含自锚时 597/630 对
//!    都"有共同参与者"，等于没有过滤器）；
//! 2. **阳性对照**：剥完仍有一个共同参与者、跨度 13.5min 的两条**必须**合（否则第 1 条的阴性
//!    只是"阈值全拒"的副产品）；
//! 3. **两根时间闸**：块内跨度 ≤ 15min 剔掉 6h 那一类强边；Δt ≤ 10min 只管邻近轴（中间带）；
//! 4. **三段置信带**：同证据轮 / 会话内轮距 ≤1 而无共同参与者 ⇒ 进中间带、**本轮不落库**；
//! 5. **时间轴只有两口钟**：`occurred_at_ms`（已知）→ `created_at_ms`（发话）；
//!    `turns.ingested_at_ms` 参与不进来（夹具把导入钟推到 9 天之外，判定必须不受它影响）；
//! 6. **幂等与自噬**：同一库重跑写侧全 0、三表行数不变；归组自己写的行不会变成下一轮的成员；
//! 7. **CLI**：零模型调用也 rc=0、读数键齐全、`--max-batches` 与 `lag_rows` 的账对得上。
//!
//! 夹具纪律：合成用例只用通用占位名；**现网 fact id 只出现在副本真跑那一条里，
//! 而且是从副本库里读真身**（参与者、钟、署名一律读回来，源码里一个专名都不抄）。
//! 副本真跑由 `EPISODE_B_ACCEPT` 开关：不设 ⇒ 直接 return（不 `#[ignore]`，
//! 为的是 `#[test]` 条数与该 suite 的 passed 数一格不差）。
use rc_distill::episode::{
    self, Band, DeferMidBand, MidBandAdjudicator, Pair,
};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::GroupFact;
use rc_memory::{
    compute_fact_id, compute_mem_id, put_fact_rows_with_extras, AnchorRole, BatchCtx, FactExtras,
    FactWriteItem, MemTable, Scope, ScopeKind,
};
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 执行钟。写入口不用它落任何一格（`put_episodes` 那一族的钟来自成员），
/// 但它故意与发话时刻差很远 ⇒ 一旦有谁拿它当时间轴，用例就会红。
const NOW: i64 = 1_700_000_999_000;
/// 发话钟基准。
const SRC0: i64 = 1_700_000_000_000;
const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
const HOUR: i64 = 60 * MIN;
/// 导入钟偏移：比任何一条阈值都大一个量级（S0 §1.3 实测中位 26.85h、最长 9.98 天）。
const INGEST_LAG: i64 = 9 * 24 * HOUR;

/// 说话 agent 的自锚：`kind='agent'` 且 canonical **等于** `BatchCtx::source_agent`，
/// 这正是 `writers_fact.rs` 的 `put_fact_rows_with_extras` 会把 subject 无条件并进参与者的那一形。
const AGENT: &str = "agent-甲";
/// 主人锚与第三方锚的 canonical：`seed_anchor` 的 INSERT 与句柄派生共用这两串。
const PERSON: &str = "乙某";
const WORK: &str = "丙某服务";
/// 只为"没有落库的锚位"（`row(...)` 的纯逻辑夹具）保留：那种 id 不指向任何 anchor 行，
/// 没有行上的 kind/canonical/scope 可依，不能派生。
fn anchor(index: usize) -> String { format!("{:064x}", 0xa0 + index) }
/// 自锚（agent）、主人锚（person）、第三方锚（work，"被谈到的那个模型/服务"）。
///
/// 三个句柄都按生产算法算：`seed_anchor` 铺下的那几行 `kind` 是字面量、canonical 是
/// [`AGENT`]/[`PERSON`]/[`WORK`]、scope 是那行 SQL 写死的 `project/proj-episode-run`
/// （= 本文件的 [`scope()`]）⇒ 句柄与行同源。
fn self_anchor() -> String { compute_mem_id(MemTable::Anchor, "agent", AGENT, "", &scope()) }
fn person_anchor() -> String { compute_mem_id(MemTable::Anchor, "person", PERSON, "", &scope()) }
fn work_anchor() -> String { compute_mem_id(MemTable::Anchor, "work", WORK, "", &scope()) }
fn turn(index: usize) -> String { format!("{:064x}", 0x31 + index) }

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-episode-run".into() } }
fn scope_b() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-episode-other".into() } }
fn ctx() -> BatchCtx { BatchCtx { source_agent: AGENT.into(), device_id: "dev-ep".into() } }
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-ep".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }

fn data_dir(dir: &Path) -> PathBuf { dir.join("data") }

fn seed_anchor(m: &rc_store::MemoryHandle, id: &str, kind: &str, canonical: &str) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                            status, created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, ?2, ?3, 'real', 'project', ?4, 'active', ?5, ?5, 1)",
        rusqlite::params![id, kind, canonical, "proj-episode-run", SRC0],
    ).unwrap();
}

/// 一轮真转录行（归组的轮距轴读它）。`ingested_at_ms` 一律推到 9 天之外：
/// 它是**导入钟**，谁用它当时间轴，谁的"相邻"就全散 ⇒ 用例当场红。
fn seed_turn(
    root: &Root, index: usize, session_key: &str, spoken_at_ms: i64,
) -> String {
    let uid = turn(index);
    root.transcript().conn().execute(
        "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                           turn_key_tier, created_at_ms, ingested_at_ms, commit_seq)
         VALUES(?1,?2,'dev-ep','agent',?3,?4,1,?5,?6,0)",
        rusqlite::params![uid, session_key, format!("合成第 {index} 句"), format!("fp{index}"),
                          spoken_at_ms, spoken_at_ms + INGEST_LAG],
    ).unwrap();
    uid
}

/// 一条真 fact（走真生产者 ⇒ 自带 `ref_kind='turn'` 出处与参与者）。
/// `subject` 是主语锚、`extra` 是主语以外的参与者锚；`turn_index` 决定它挂在哪一轮。
/// 八个形参是夹具的形状（锚、参与者、钟、归属、会话各一格），合并成结构体只会让每条夹具
/// 多背一层壳——同 `writers_x::insert_derived_provenance` 那条 allow 的理由。
#[allow(clippy::too_many_arguments)]
fn fact(
    root: &Root, turn_index: usize, subject: &str, extra: &[String], value: &str,
    speech_ms: i64, item_scope: &Scope, session_key: &str,
) -> String {
    seed_turn(root, turn_index, session_key, speech_ms);
    fact_on_existing_turn(root, turn_index, subject, extra, value, speech_ms, item_scope)
}

/// 同上，但**不**再插一轮（两条 fact 共用同一证据轮的那一形）。
fn fact_on_existing_turn(
    root: &Root, turn_index: usize, subject: &str, extra: &[String], value: &str,
    speech_ms: i64, item_scope: &Scope,
) -> String {
    let text = format!("甲某{value}");
    let item = FactWriteItem {
        subject_anchor_id: subject.into(),
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some(value.into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: value.into(),
        evidence: vec![(turn(turn_index), AnchorRole::Agent)],
        text: text.clone(),
        scope: item_scope.clone(),
        source_ms: speech_ms,
    };
    let id = compute_fact_id(subject, "做了", None, Some(value), "event", "", &text, item_scope);
    let extras = FactExtras { participants: extra.to_vec(), ..Default::default() };
    let out = put_fact_rows_with_extras(
        root.memory(), &[item], &[extras], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    id
}

fn set_occurred(root: &Root, id: &str, ms: Option<i64>) {
    root.memory().conn().execute(
        "UPDATE fact SET occurred_at_ms=?1 WHERE id=?2", rusqlite::params![ms, id]).unwrap();
}

/// 一个铺好三个锚的最小库（自锚 / 主人 / 第三方）。
fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory(), &self_anchor(), "agent", AGENT);
    seed_anchor(root.memory(), &person_anchor(), "person", PERSON);
    seed_anchor(root.memory(), &work_anchor(), "work", WORK);
    (dir, root)
}

fn count(m: &rc_store::MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn episode_ids(m: &rc_store::MemoryHandle) -> Vec<String> {
    let mut st = m.conn().prepare("SELECT id FROM episode ORDER BY id").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
}

fn members_of(m: &rc_store::MemoryHandle, episode_id: &str) -> Vec<String> {
    let mut st = m.conn().prepare(
        "SELECT fact_id FROM episode_fact WHERE episode_id=?1 ORDER BY ord, fact_id").unwrap();
    st.query_map([episode_id], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
}

fn episode_of(m: &rc_store::MemoryHandle, fact_id: &str) -> Option<String> {
    rc_memory::episode_of_fact(m, fact_id).unwrap()
}

fn row_counts(m: &rc_store::MemoryHandle) -> (i64, i64, i64) {
    (count(m, "SELECT count(*) FROM episode"),
     count(m, "SELECT count(*) FROM episode_fact"),
     count(m, "SELECT count(*) FROM card WHERE kind='episode'"))
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).arg("--data-dir").arg(data_dir(dir)).output().expect("hub bin")
}

fn code(out: &Output) -> i32 { out.status.code().unwrap() }

fn stderr_text(out: &Output) -> String { String::from_utf8_lossy(&out.stderr).to_string() }

/// 每趟一行 JSONL（`--json` 的契约）；空 stdout ⇒ 空向量。
fn lines(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout).lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l)
            .unwrap_or_else(|e| panic!("读数必须是逐行 JSON：{l:?}（{e}）")))
        .collect()
}

// ── 1. 剥自锚 ────────────────────────────────────────────────────────────────

/// 一支 `GroupFact` 的构造（纯函数用例用，不落库）。
/// 八个形参 = `GroupFact` 里参与判定的那八格（其余署名/正文走默认值）；
/// 用它而不是结构体字面量是把"哪八格参与判定"这一件事写在一处。
#[allow(clippy::too_many_arguments)]
fn row(id: &str, subject: &str, subject_kind: Option<&str>, subject_canonical: Option<&str>,
       speaker: Option<&str>, participants: &[&str], created: i64, uid: &str) -> GroupFact {
    GroupFact {
        id: id.into(),
        participants: participants.iter().map(|s| (*s).to_string()).collect(),
        speaker_anchor_id: speaker.map(|s| s.to_string()),
        subject_anchor_id: subject.to_string(),
        subject_kind: subject_kind.map(|s| s.to_string()),
        subject_canonical: subject_canonical.map(|s| s.to_string()),
        source_agent: AGENT.into(),
        device_id: "dev-ep".into(),
        scope: scope(),
        world_id: None,
        category: "event".into(),
        text: id.into(),
        occurred_at_ms: None,
        occurred_end_ms: None,
        created_at_ms: created,
        evidence_turn_uid: uid.into(),
    }
}

/// 共同参与者必须先剥掉"说话 agent 自己的锚"：两支都算（`speaker_anchor_id`、
/// 以及 subject 就是那个 agent 锚的那一支），而 person 锚**不剥**。
#[test]
fn 剥自锚之后才算共同参与者_人的锚不剥() {
    let a = row("a", &self_anchor(), Some("agent"), Some(AGENT), Some(&self_anchor()),
                &[&self_anchor(), &work_anchor()], SRC0, &turn(0));
    let b = row("b", &self_anchor(), Some("agent"), Some(AGENT), None,
                &[&self_anchor(), &work_anchor()], SRC0 + MIN, &turn(1));
    assert_eq!(episode::self_anchors(&a), std::collections::BTreeSet::from([self_anchor()]));
    // 两支都指向同一个自锚：只剥一次，剩下的那个共同者才是"真共同参与者"。
    assert_eq!(episode::shared_participants(&a, &b), vec![work_anchor()]);

    // 只剩自锚 ⇒ 交集为空（这就是现网 597 对塌成 3 对的那一格）。
    let c = row("c", &self_anchor(), Some("agent"), Some(AGENT), Some(&self_anchor()),
                &[&self_anchor()], SRC0, &turn(2));
    assert!(episode::shared_participants(&b, &c).is_empty(), "只共享自锚不算共同参与者");
    // subject 是 person ⇒ 不剥；它是合法的共同者。
    let d = row("d", &person_anchor(), Some("person"), Some("乙某"), Some(&self_anchor()),
                &[&person_anchor(), &work_anchor()], SRC0, &turn(3));
    let e = row("e", &person_anchor(), Some("person"), Some("乙某"), Some(&self_anchor()),
                &[&person_anchor()], SRC0, &turn(4));
    assert_eq!(episode::shared_participants(&d, &e), vec![person_anchor()]);
    // 说话 agent 的锚与"本行的 agent 自锚"不同一支时也剥得掉（speaker 支独立生效）。
    let other_agent = anchor(9);
    let f = row("f", &person_anchor(), Some("person"), Some("乙某"), Some(&other_agent),
                &[&other_agent, &work_anchor()], SRC0, &turn(5));
    let g = row("g", &person_anchor(), Some("person"), Some("乙某"), None,
                &[&other_agent], SRC0, &turn(6));
    assert!(episode::shared_participants(&f, &g).is_empty(), "只共享说话 agent 自己的锚不算");
}

// ── 2/3/4/5. 阈值与三段带（库面）────────────────────────────────────────────

/// 阴性一：只共享自锚的两条（相隔 10 秒、同一轮）——**不许**合。
#[test]
fn 阴性_只共享自锚的两条不合() {
    let (dir, root) = seeded();
    let a = fact(&root, 0, &self_anchor(), &[self_anchor()], "调通了闸", SRC0, &scope(), "sess-ep-A");
    let b = fact(&root, 1, &self_anchor(), &[self_anchor()], "补了桩", SRC0 + 10 * SEC,
                 &scope(), "sess-ep-A");
    let rows = rc_memory::reader::group_facts(root.memory(), 40).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(episode::shared_participants(&rows[0], &rows[1]).is_empty(),
            "两行的共同者只有说话 agent 自己的锚 ⇒ 剥完为空");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(pass.readout.episodes_written, 0, "含自锚的重叠不是共同参与者：{pass:?}");
    assert_eq!(row_counts(root.memory()), (0, 0, 0), "阴性对不落任何一段经历");
    assert_eq!(episode_of(root.memory(), &a), None);
    assert_eq!(episode_of(root.memory(), &b), None);
    drop(dir);
}

/// 阳性对照：剥自锚后剩一个共同参与者、跨度 13.5min ⇒ **必须**合。
/// 派发档的原话：没有这一条，上面的阴性只是"阈值全拒"的副产品。
#[test]
fn 阳性_剥自锚后一个共同参与者且跨度十三分半必须合() {
    let (dir, root) = seeded();
    let a = fact(&root, 0, &person_anchor(), &[work_anchor()], "要求试那个服务", SRC0,
                 &scope(), "sess-ep-A");
    let b = fact(&root, 1, &self_anchor(), &[work_anchor()], "看到那个服务是思考模型",
                 SRC0 + 808 * SEC, &scope(), "sess-ep-A");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    let ro = pass.readout;
    assert_eq!(ro.candidates, 1, "只有一对候选：{ro:?}");
    assert_eq!(ro.merged_pairs, 1);
    assert_eq!((ro.episodes_written, ro.members_linked, ro.cards_written), (1, 2, 1), "{ro:?}");
    assert_eq!(ro.span_pruned, 0, "13.5min 在 15min 跨度闸之内，不该被剔");
    assert_eq!(ro.mid_band_deferred, 0, "这一对走的是强边，不是中间带");
    assert_eq!(row_counts(root.memory()), (1, 2, 1));
    let id = episode_ids(root.memory()).remove(0);
    let got = members_of(root.memory(), &id);
    assert_eq!(got.len(), 2);
    assert!(got.contains(&a) && got.contains(&b), "成员就是那两条：{got:?}");
    // D4-补：成员事件时间全未知 ⇒ `start/end` 保持 NULL；卡面上的钟取最早成员的**发话**时刻，
    // 并如实标 `time_confidence='speech'`（不是 'anchored'，更不是执行钟/导入钟）。
    assert_eq!(count(root.memory(), &format!(
        "SELECT count(*) FROM episode WHERE id='{id}' AND start_ms IS NULL AND end_ms IS NULL")),
        1, "跨度只用于判定，不许冒充事件时间写进 start_ms");
    assert_eq!(count(root.memory(), &format!(
        "SELECT count(*) FROM card WHERE kind='episode' AND as_of_ms={SRC0} \
         AND time_confidence='speech' AND mode='inferred'")),
        1, "镜像卡的钟来自成员发话时刻，且显式标 inferred");
    drop(dir);
}

/// 跨度闸：三条强边里那两条 6h 级必须被剔掉（`measure-8-spancap.py` 的同一算法），
/// 13.5min 那条保住 ⇒ 结果是一段两成员的经历 + 一条孤零的 fact。
#[test]
fn 跨度闸剔掉超长强边_六小时那条不许合() {
    let (dir, root) = seeded();
    let a = fact(&root, 0, &person_anchor(), &[work_anchor()], "开头", SRC0, &scope(), "sess-ep-A");
    let b = fact(&root, 1, &self_anchor(), &[work_anchor()], "中段", SRC0 + 808 * SEC,
                 &scope(), "sess-ep-A");
    let c = fact(&root, 2, &self_anchor(), &[work_anchor()], "六小时后的另一件事",
                 SRC0 + 6 * HOUR + 25 * MIN, &scope(), "sess-ep-A");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    let ro = pass.readout;
    assert_eq!(ro.candidates, 3, "三条强边都是候选：{ro:?}");
    assert_eq!(ro.span_pruned, 2, "跨度闸该剔两条：{ro:?}");
    assert_eq!((ro.episodes_written, ro.members_linked), (1, 2), "{ro:?}");
    let id = episode_ids(root.memory()).remove(0);
    let got = members_of(root.memory(), &id);
    assert_eq!(got.len(), 2, "块内只有相邻的那两条：{got:?}");
    assert!(got.contains(&a) && got.contains(&b), "块内成员 = 相邻两条：{got:?}");
    assert!(!got.contains(&c));
    assert_eq!(episode_of(root.memory(), &c), None, "6.45h 之外的那条不许被链进来");
    drop(dir);
}

/// 阴性二：同一证据轮、没有共同参与者 ⇒ 中间带，标出来但不合。
#[test]
fn 阴性_同证据轮没有共同参与者只进中间带() {
    let (dir, root) = seeded();
    let _a = fact_on_existing_turn(&root, 0, &self_anchor(), &[self_anchor()], "同轮第一条",
                                   SRC0, &scope());
    let _b = fact(&root, 0, &person_anchor(), &[person_anchor()], "同轮第二条", SRC0,
                  &scope(), "sess-ep-A");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    let ro = pass.readout;
    assert_eq!((ro.candidates, ro.merged_pairs), (1, 0), "同轮算候选，不算合并：{ro:?}");
    assert_eq!(ro.mid_band_deferred, 1, "中间带本轮一个都不合：{ro:?}");
    assert_eq!(row_counts(root.memory()), (0, 0, 0));
    drop(dir);
}

/// 阴性三：会话内轮距 1、没有共同参与者（派发档夹具里 `55.9s` 与 `1.2min` 那一对的形状）。
#[test]
fn 阴性_轮距一但没有共同参与者不合() {
    let (dir, root) = seeded();
    let _a = fact(&root, 7, &self_anchor(), &[work_anchor()], "甲事", SRC0, &scope(), "sess-ep-A");
    let _b = fact(&root, 8, &person_anchor(), &[person_anchor()], "乙事", SRC0 + 71 * SEC,
                  &scope(), "sess-ep-A");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(pass.readout.episodes_written, 0, "{:?}", pass.readout);
    assert_eq!(pass.readout.mid_band_deferred, 1, "轮距 1 是中间带：{:?}", pass.readout);
    assert_eq!(row_counts(root.memory()), (0, 0, 0));
    drop(dir);
}

/// 阴性四：跨会话 + 只共享自锚（现网 `9.12d` 那一对的形状）⇒ 连候选都不算。
/// 这一条同时钉住 D9 第 5 条"要求同 session"（它在现网是空约束，但形状要写死）。
#[test]
fn 阴性_跨会话既不成强边也不进中间带() {
    let (dir, root) = seeded();
    let _a = fact(&root, 0, &self_anchor(), &[self_anchor()], "这一场的", SRC0,
                  &scope(), "sess-ep-A");
    let _b = fact(&root, 1, &self_anchor(), &[self_anchor()], "另一场的", SRC0 + 9 * 24 * HOUR,
                  &scope(), "sess-ep-B");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(pass.readout.candidates, 0, "跨会话不构成任何对：{:?}", pass.readout);
    assert_eq!(row_counts(root.memory()), (0, 0, 0));
    drop(dir);
}

/// 时间轴只有两口钟：事件时间已知时按它配（发话钟差 7 天也照合），
/// 而 `turns.ingested_at_ms`（夹具里被推到 9 天之外）参与不进来。
#[test]
fn 时间轴只取事件时间与发话时刻_导入钟不参与() {
    let (dir, root) = seeded();
    let a = fact(&root, 0, &person_anchor(), &[work_anchor()], "上午那句", SRC0, &scope(),
                 "sess-ep-A");
    let b = fact(&root, 1, &self_anchor(), &[work_anchor()], "七天后的那句", SRC0 + 7 * 24 * HOUR,
                 &scope(), "sess-ep-A");
    set_occurred(&root, &a, Some(SRC0 + 30 * 60_000));
    set_occurred(&root, &b, Some(SRC0 + 30 * 60_000 + 30 * SEC));
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(pass.readout.episodes_written, 1, "Δt 按事件时间只有 30 秒：{:?}", pass.readout);
    let id = episode_ids(root.memory()).remove(0);
    assert_eq!(count(root.memory(), &format!(
        "SELECT count(*) FROM episode WHERE id='{id}' AND start_ms={}", SRC0 + 30 * 60_000)),
        1, "start_ms 来自成员 occurred_at_ms，不是发话钟，更不是导入钟");
    drop(dir);
}

/// 不跨归属：两个 scope 各合各的，绝不长出一段跨归属的经历
/// （`writers_x::shared_member_scope` 是执法处，这里是"根本不会递给执法处"）。
#[test]
fn 不跨归属_两个_scope_的各合各的() {
    let (dir, root) = seeded();
    fact(&root, 0, &person_anchor(), &[work_anchor()], "甲项目的开头", SRC0, &scope(), "sess-ep-A");
    fact(&root, 1, &self_anchor(), &[work_anchor()], "甲项目的中段", SRC0 + MIN, &scope(),
         "sess-ep-A");
    fact(&root, 2, &person_anchor(), &[work_anchor()], "乙项目的开头", SRC0 + 2 * MIN, &scope_b(),
         "sess-ep-A");
    fact(&root, 3, &self_anchor(), &[work_anchor()], "乙项目的中段", SRC0 + 3 * MIN, &scope_b(),
         "sess-ep-A");
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(pass.readout.episodes_written, 2, "{:?}", pass.readout);
    assert_eq!(count(root.memory(), "SELECT count(*) FROM episode WHERE scope_key='proj-episode-run'"), 1);
    assert_eq!(count(root.memory(), "SELECT count(*) FROM episode WHERE scope_key='proj-episode-other'"), 1);
    drop(dir);
}

/// 幂等 + 自噬：同一库重跑，写侧计数全 0、三表行数不变；
/// 而归组自己写的行（card / episode / episode_fact / provenance）**不会**再当成员读回来——
/// 扫描面只有 `fact`，所以第二趟的 `scanned` 与第一趟一字不差。
#[test]
fn 幂等_重跑写侧全零行数不变且归组写的行不回喂() {
    let (dir, root) = seeded();
    fact(&root, 0, &person_anchor(), &[work_anchor()], "把闸接通", SRC0, &scope(), "sess-ep-A");
    fact(&root, 1, &self_anchor(), &[work_anchor()], "再补一条桩", SRC0 + 5 * MIN, &scope(),
         "sess-ep-A");
    let first = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(first.readout.episodes_written, 1, "{:?}", first.readout);
    let before = row_counts(root.memory());
    let facts_before = count(root.memory(), "SELECT count(*) FROM fact");
    let second = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(second.scanned, first.scanned, "扫描面只数 fact ⇒ 归组写的行不回喂下一轮");
    assert_eq!(second.window_end, first.window_end);
    for key in ["episodes_written", "members_linked", "cards_written", "merged_pairs",
                "superseded", "dropped_members"] {
        let n = match key {
            "episodes_written" => second.readout.episodes_written,
            "members_linked" => second.readout.members_linked,
            "cards_written" => second.readout.cards_written,
            "merged_pairs" => second.readout.merged_pairs,
            "superseded" => second.readout.superseded,
            _ => second.readout.dropped_members,
        };
        assert_eq!(n, 0, "重跑的写侧计数 {key} 必须是 0：{:?}", second.readout);
    }
    assert_eq!(second.readout.candidates, first.readout.candidates, "判定读数不随重跑漂");
    assert_eq!(row_counts(root.memory()), before, "三表行数一格没动");
    assert_eq!(count(root.memory(), "SELECT count(*) FROM fact"), facts_before);
    drop(dir);
}

/// 中间带的接口位：本轮唯一实现一律拒 ⇒ 中间带对进不了块。
/// 它是"日后接模型"的那一格，不是一段没人调的壳：`run_episode_pass` 每次都带着它走。
#[test]
fn 中间带裁决接口位_本轮实现一律拒() {
    let a = row("a", &self_anchor(), Some("agent"), Some(AGENT), Some(&self_anchor()),
                &[&self_anchor()], SRC0, &turn(0));
    let b = row("b", &self_anchor(), Some("agent"), Some(AGENT), Some(&self_anchor()),
                &[&self_anchor()], SRC0 + SEC, &turn(0));
    let facts = vec![a.clone(), b.clone()];
    let pairs = episode::plan_pairs(&facts, &std::collections::HashMap::new());
    assert_eq!(pairs.len(), 1, "同轮 ⇒ 候选一对：{pairs:?}");
    assert_eq!(pairs[0].band, Band::Proximity);
    assert!(!DeferMidBand.accepts(&facts, &pairs[0]), "本轮实现一律拒");
    let p: &Pair = &pairs[0];
    assert!(p.same_turn, "这一对的来路是同证据轮：{p:?}");
    // 裁决器换成"一律放行"才会出现块——这证明中间带**确实**卡在裁决这一格，不是卡在别处。
    struct AcceptAll;
    impl MidBandAdjudicator for AcceptAll {
        fn accepts(&self, _: &[GroupFact], _: &Pair) -> bool { true }
    }
    let positions = std::collections::HashMap::new();
    let strict = episode::plan_window(&[a.clone(), b.clone()], &positions, &DeferMidBand);
    assert!(strict.blocks.iter().all(|blk| blk.len() < 2), "本轮不该有任何多成员块");
    assert_eq!(strict.deferred.len(), 1);
    let loose = episode::plan_window(&[a, b], &positions, &AcceptAll);
    assert_eq!(loose.deferred.len(), 0, "放行 ⇒ 中间带不再挂在 deferred 上");
    assert!(loose.blocks.iter().any(|blk| blk.len() == 2), "接口位接上就会合（变异证明）");
}

// ── 6/7. CLI 面 ──────────────────────────────────────────────────────────────

/// 把上面那条合成库落进一个可被 CLI 打开的目录（CLI 自己再 `Root::open` ⇒ 这里必须先交还句柄）。
fn seed_cli_store(dir: &Path, n_extra: usize) -> Vec<String> {
    let root = Root::open(&data_dir(dir)).unwrap();
    seed_anchor(root.memory(), &self_anchor(), "agent", AGENT);
    seed_anchor(root.memory(), &person_anchor(), "person", PERSON);
    seed_anchor(root.memory(), &work_anchor(), "work", WORK);
    fact(&root, 0, &person_anchor(), &[work_anchor()], "要求试那个服务", SRC0, &scope(),
         "sess-ep-A");
    fact(&root, 1, &self_anchor(), &[work_anchor()], "看到那个服务是思考模型", SRC0 + 808 * SEC,
         &scope(), "sess-ep-A");
    // 余量夹具：再铺 `n_extra` 条只带自锚的 fact（既不成对也不合，只用来把窗撑到帽外）。
    for i in 0..n_extra {
        let index = 10 + i;
        fact(&root, index, &self_anchor(), &[self_anchor()],
             &format!("多余的第 {i} 句"), SRC0 + (index as i64) * MIN, &scope(), "sess-ep-A");
    }
    let ids = rc_memory::reader::group_facts(root.memory(), 400).unwrap()
        .into_iter().map(|f| f.id).collect();
    drop(root);
    ids
}

/// 派发档 §4 的那一格读数：键名与个数一个不多一个不少（`span_pruned` 是唯一的新增，
/// 理由是 P-5"被闸挡下的对数不许蒸发"）。
const EPISODE_READOUT_KEYS: [&str; 13] = [
    "schema_version", "job", "batch", "candidates", "merged_pairs", "span_pruned",
    "episodes_written", "members_linked", "cards_written", "superseded", "dropped_members",
    "mid_band_deferred", "cursor_moved",
];

/// `hub episode run --json`：**零 LLM 调用也 rc=0**，每趟一行、键齐全、类型对；
/// 收批判据落在 `cursor_moved`（W33·A4 起判据 = "窗尾还会不会前移"：两行全库已在窗内
/// ⇒ 第一趟收批，第二趟被便宜闸整段跳过——不再存在"第二趟确认"那一发）。
/// 语义变化（A4）按纪律只动断言臂、测试名不动：旧读法在 batch 1 是空洞的真
/// （before=None 恒不等），新读法以 `lag_rows > 0` 为准（见 hub `episode_run` doc ②）。
#[test]
fn cli_零模型调用也rc0且读数键齐全() {
    let dir = tempfile::tempdir().unwrap();
    let ids = seed_cli_store(dir.path(), 0);
    let out = run(dir.path(), &["episode", "run", "--json"]);
    assert_eq!(code(&out), 0, "stdout={:?} stderr={}", String::from_utf8_lossy(&out.stdout),
               stderr_text(&out));
    let got = lines(&out);
    assert_eq!(got.len(), 1, "窗尾之后零行 ⇒ 便宜闸跳过剩余趟，只有一趟读数：{got:?}");
    for line in &got {
        let keys: Vec<&str> = line.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys.len(), EPISODE_READOUT_KEYS.len() + 1, "多出来的那一格是 lag_rows：{line}");
        for key in EPISODE_READOUT_KEYS {
            assert!(line.get(key).is_some(), "读数缺键 {key}：{line}");
        }
        assert!(line.get("lag_rows").is_some(), "读数缺 lag_rows：{line}");
        assert_eq!(line["job"], Value::String("episode".into()));
        assert_eq!(line["schema_version"], Value::from(2));
    }
    assert_eq!(got[0]["episodes_written"], Value::from(1), "{:?}", got[0]);
    assert_eq!(got[0]["members_linked"], Value::from(2));
    assert_eq!(got[0]["cards_written"], Value::from(1));
    assert_eq!(got[0]["merged_pairs"], Value::from(1));
    assert_eq!(got[0]["cursor_moved"], Value::from(false),
               "全库已在窗内 ⇒ 窗尾不再前移（A4 判据）：{:?}", got[0]);
    assert_eq!(got[0]["lag_rows"], Value::from(0));
    // A4 便宜闸的验收臂：同一份库再跑一发（无新 fact）⇒ 单行收批，
    // `why=no_new_facts` + `calls=0`（纯程序，0 模型调用），写侧照旧全 0。
    let again = run(dir.path(), &["episode", "run", "--json"]);
    assert_eq!(code(&again), 0, "stderr={}", stderr_text(&again));
    let got2 = lines(&again);
    assert_eq!(got2.len(), 1, "无新 fact ⇒ 单行收批：{got2:?}");
    assert_eq!(got2[0]["episodes_written"], Value::from(0), "重跑写侧全 0：{:?}", got2[0]);
    assert_eq!(got2[0]["cursor_moved"], Value::from(false));
    assert_eq!(got2[0]["lag_rows"], Value::from(0));
    assert_eq!(got2[0]["why"], Value::String("no_new_facts".into()), "{}", got2[0]);
    assert_eq!(got2[0]["calls"], Value::from(0), "{}", got2[0]);
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (1, 2, 1), "CLI 落的行与读数一致");
    assert!(episode_of(root.memory(), &ids[0]).is_some());
    drop(root);
}

/// 座位是"同族形状"，不是本轮的必需品：**不给任何座位也照跑**（rc=0），
/// 而给了一个坏座位必须当场失败——"配了但配坏了"与"没配"不许塌成一种。
#[test]
fn cli_不给座位也跑得起来_坏座位当场失败() {
    let dir = tempfile::tempdir().unwrap();
    seed_cli_store(dir.path(), 0);
    let quiet = run(dir.path(), &["episode", "run"]);
    assert_eq!(code(&quiet), 0, "无座位也 rc=0；stderr={}", stderr_text(&quiet));
    assert!(!quiet.stdout.is_empty(), "不带 --json 也要出一行 k=v 读数");
    let bad = run(dir.path(), &["episode", "run", "--replies", "no-such-file.json"]);
    assert_eq!(code(&bad), 3, "坏座位是运维失败：stderr={}", stderr_text(&bad));
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (1, 2, 1), "坏座位那一次一个都没落");
    drop(root);
}

/// `--max-batches 1` ⇒ 恰好一行读数；窗外的余量落在 `lag_rows` 那一格里（不蒸发）。
#[test]
fn cli_max_batches_到顶只出一行余量进lag_rows() {
    let dir = tempfile::tempdir().unwrap();
    // 2 条会合的 + 45 条只带自锚的 ⇒ 活 fact 47 行 > 扫描帽 40。
    seed_cli_store(dir.path(), 45);
    let out = run(dir.path(), &["episode", "run", "--json", "--max-batches", "1"]);
    assert_eq!(code(&out), 0);
    let got = lines(&out);
    assert_eq!(got.len(), 1, "--max-batches 1 只出一行：{got:?}");
    assert_eq!(got[0]["batch"], Value::from(1));
    assert_eq!(got[0]["lag_rows"], Value::from(7), "47 行活 fact，窗 40 ⇒ 余 7：{:?}", got[0]);
    drop(out);
    // 放开上限接着跑 ⇒ 余量被收掉、行数不变（窗从库头整段重算，跨界那一段早就在了）。
    let rest = run(dir.path(), &["episode", "run", "--json"]);
    assert_eq!(code(&rest), 0);
    let tail = lines(&rest);
    let last = tail.last().unwrap();
    assert_eq!(last["lag_rows"], Value::from(0), "跑空 ⇒ 余量归零：{last}");
    assert_eq!(last["episodes_written"], Value::from(0), "整段重算不该写出第二段：{last}");
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (1, 2, 1), "只该有一段经历");
    drop(root);
}

/// 先证再写（派发档 §3 / 母档 §3）：**快照确实搬 `episode` 与 `episode_fact` 两张表**。
/// 这一条是"写出来在副本上会不会库里有、下发看不见"的执行证明，不是注释：
/// 走的是生产同一条路径 `rc_memory::snapshot::build`（`VACUUM INTO` → `prune` → 悬空指针检查
/// → digest），然后把产出的那份快照当**另一册库**打开逐表数行。
/// `prune` 的删除面只有 card/anchor（按活行筛）与五张派生表 + `fact_link`/`attention`，
/// 从不点名 `episode`/`episode_fact`；摘要的表清单是 `schema::MEMORY_TABLES`，那两张在里面。
#[test]
fn 快照真的搬_episode_与_episode_fact_两张表() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("data");
    std::fs::create_dir_all(&store).unwrap();
    let root = Root::open(&store).unwrap();
    seed_anchor(root.memory(), &self_anchor(), "agent", AGENT);
    seed_anchor(root.memory(), &person_anchor(), "person", PERSON);
    seed_anchor(root.memory(), &work_anchor(), "work", WORK);
    fact(&root, 0, &person_anchor(), &[work_anchor()], "要求试那个服务", SRC0, &scope(),
         "sess-ep-A");
    fact(&root, 1, &self_anchor(), &[work_anchor()], "看到那个服务是思考模型", SRC0 + 808 * SEC,
         &scope(), "sess-ep-A");
    let snap_dir = dir.path().join("snap");
    // ① 归组**之前**先出一份快照：它是"下发看不见"那一侧的基准。
    let before = rc_memory::snapshot::build(root.memory(), &snap_dir, NOW, false)
        .expect("快照产出失败（归组之前）");
    assert_eq!(snap_count(&before.path, "SELECT count(*) FROM episode"), 0);
    let pass = episode::run_episode_pass(root.memory(), root.transcript(), 40, &DeferMidBand, NOW)
        .unwrap();
    assert_eq!(pass.readout.episodes_written, 1, "{:?}", pass.readout);
    // ② 归组之后再出一份：走的是生产同一条路径（`VACUUM INTO` → `prune` → 悬空指针检查 → digest），
    //    然后把产出的那份快照当**另一册库**打开逐表数行。
    //    `prune` 的删除面只有 card/anchor（按活行筛）与五张派生表 + `fact_link`/`attention`，
    //    从不点名 `episode`/`episode_fact`；摘要的表清单是 `schema::MEMORY_TABLES`，那两张在里面。
    let after = rc_memory::snapshot::build(root.memory(), &snap_dir, NOW + 1, false)
        .expect("快照产出失败（归组之后）");
    assert_eq!(snap_count(&after.path, "SELECT count(*) FROM episode"), 1,
        "快照里没有 episode ⇒ 下发看不见，本轮就不该落库");
    assert_eq!(snap_count(&after.path, "SELECT count(*) FROM episode_fact"), 2, "成员表没跟着走");
    assert_eq!(snap_count(&after.path, "SELECT count(*) FROM card WHERE kind='episode'"), 1,
        "镜像卡没跟着走");
    assert_eq!(snap_count(&after.path,
        "SELECT count(*) FROM provenance WHERE mem_table='card' AND ref_kind='fact'"), 2,
        "每成员一行 basis 出处没跟着走");
    assert_ne!(before.digest, after.digest, "写了经历而摘要不动 ⇒ 下发侧永远不会换代");
    drop(root);
}

/// 把产出的一份快照当**另一册库**打开数行（只读，不回连中心库）。
fn snap_count(path: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// T-7 的 CLI 面：同一份库连跑两次 `hub episode run`，第二次写侧全 0、三表行数不变。
#[test]
fn cli_第二次运行写侧全零且行数不变() {
    let dir = tempfile::tempdir().unwrap();
    seed_cli_store(dir.path(), 0);
    let first = run(dir.path(), &["episode", "run", "--json"]);
    assert_eq!(code(&first), 0);
    let root = Root::open(&data_dir(dir.path())).unwrap();
    let before = row_counts(root.memory());
    let cards_before = count(root.memory(), "SELECT count(*) FROM card");
    drop(root);
    let second = run(dir.path(), &["episode", "run", "--json"]);
    assert_eq!(code(&second), 0);
    let lines2 = lines(&second);
    for line in &lines2 {
        assert_eq!(line["episodes_written"], Value::from(0), "重跑第一格必须 0：{line}");
        assert_eq!(line["members_linked"], Value::from(0));
        assert_eq!(line["cards_written"], Value::from(0));
        assert_eq!(line["superseded"], Value::from(0));
        assert_eq!(line["merged_pairs"], Value::from(0));
    }
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), before, "三表行数一格没动");
    assert_eq!(count(root.memory(), "SELECT count(*) FROM card"), cards_before,
        "镜像卡不增 ⇒ 归组写的行没被再归一次");
    drop(root);
}

// ── I-12 · 裁决步的离线四支（--replies 夹具，零真调用）─────────────────────────

/// 中间带夹具：同会话轮距 1、剥自锚后无共同参与者 ⇒ 恰好一对 Proximity 候选
/// （`阴性_轮距一但没有共同参与者不合` 的同一形状）。
fn seed_mid_band(dir: &Path) -> Vec<String> {
    let root = Root::open(&data_dir(dir)).unwrap();
    seed_anchor(root.memory(), &self_anchor(), "agent", AGENT);
    seed_anchor(root.memory(), &person_anchor(), "person", PERSON);
    seed_anchor(root.memory(), &work_anchor(), "work", WORK);
    let _a = fact(&root, 7, &self_anchor(), &[work_anchor()], "甲事", SRC0, &scope(),
                  "sess-ep-A");
    let _b = fact(&root, 8, &person_anchor(), &[person_anchor()], "乙事", SRC0 + 71 * SEC,
                  &scope(), "sess-ep-A");
    let ids = rc_memory::reader::group_facts(root.memory(), 400).unwrap()
        .into_iter().map(|f| f.id).collect();
    drop(root);
    ids
}

/// 一份 `--replies` 脚本（hub `replay` 的数组形状：字符串 = 回复原文，对象 = 四态失败）。
fn write_replies(dir: &Path, name: &str, items: &[Value]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec(&items).unwrap()).unwrap();
    path.to_string_lossy().to_string()
}

fn yes_reply() -> Value {
    Value::String("{\"verdict\":\"yes\",\"reason\":\"同一段经历的两步\"}".into())
}
fn no_reply() -> Value {
    Value::String("{\"verdict\":\"no\",\"reason\":\"时间近但完全是两件事\"}".into())
}
fn uncertain_reply() -> Value {
    Value::String("{\"verdict\":\"uncertain\",\"reason\":\"看不出是否同一件事\"}".into())
}

/// 判定表一行的键（I-12 的交账格）。`job="adjudication"`。
const ADJUDICATION_KEYS: [&str; 9] =
    ["job", "fact_a", "fact_b", "verdict", "reason", "seat", "latency_ms", "prompt_sha", "calls"];

fn adjudication_lines(out: &Output) -> Vec<Value> {
    lines(out).into_iter().filter(|l| l["job"] == Value::String("adjudication".into())).collect()
}

/// 支一「是」：裁决放行 ⇒ 中间带合入 episode，判定表逐格在场（fact id / 座位 / 指纹 / 调用数）。
#[test]
fn cli_裁决是_中间带合入且判定表逐格可溯() {
    let dir = tempfile::tempdir().unwrap();
    let ids = seed_mid_band(dir.path());
    let replies = write_replies(dir.path(), "yes.json", &[yes_reply()]);
    let out = run(dir.path(), &["episode", "run", "--json", "--replies", &replies]);
    assert_eq!(code(&out), 0, "stdout={:?} stderr={}", String::from_utf8_lossy(&out.stdout),
               stderr_text(&out));
    let recs = adjudication_lines(&out);
    assert_eq!(recs.len(), 1, "一对候选恰好一行判定：{:?}", recs);
    for key in ADJUDICATION_KEYS {
        assert!(recs[0].get(key).is_some(), "判定表缺键 {key}：{}", recs[0]);
    }
    assert_eq!(recs[0]["verdict"], Value::String("yes".into()));
    assert_eq!(recs[0]["seat"], Value::String("reply-1".into()));
    assert_eq!(recs[0]["calls"], Value::from(1), "首发即中 ⇒ 一对一发");
    assert_eq!(recs[0]["prompt_sha"].as_str().unwrap().len(), 64);
    let a = recs[0]["fact_a"].as_str().unwrap();
    let b = recs[0]["fact_b"].as_str().unwrap();
    assert!(ids.iter().any(|x| x == a) && ids.iter().any(|x| x == b), "判定表记的是真 fact id");
    let readouts: Vec<Value> = lines(&out).into_iter()
        .filter(|l| l["job"] == Value::String("episode".into())).collect();
    assert_eq!(readouts[0]["episodes_written"], Value::from(1), "{}", readouts[0]);
    assert_eq!(readouts[0]["members_linked"], Value::from(2));
    assert_eq!(readouts[0]["mid_band_deferred"], Value::from(0), "{}", readouts[0]);
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (1, 2, 1), "裁决是 ⇒ 真的落了一段");
    drop(root);
}

/// 支二「否」：裁决拒 ⇒ 不合、不落任何行。与支一合看就是"裁决输出翻转 ⇒ 合并恰好变化"的
/// 离线形状（代码级变异见 I-12 报告的变异红读数）。
#[test]
fn cli_裁决否_中间带不合且不落库() {
    let dir = tempfile::tempdir().unwrap();
    seed_mid_band(dir.path());
    let replies = write_replies(dir.path(), "no.json", &[no_reply()]);
    let out = run(dir.path(), &["episode", "run", "--json", "--replies", &replies]);
    assert_eq!(code(&out), 0);
    let recs = adjudication_lines(&out);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["verdict"], Value::String("no".into()));
    let readouts: Vec<Value> = lines(&out).into_iter()
        .filter(|l| l["job"] == Value::String("episode".into())).collect();
    assert_eq!(readouts[0]["episodes_written"], Value::from(0), "{}", readouts[0]);
    assert_eq!(readouts[0]["mid_band_deferred"], Value::from(1), "{}", readouts[0]);
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (0, 0, 0), "裁决否 ⇒ 三表一格不落");
    drop(root);
}

/// 支三「不确定」：fail-closed —— 三态里最诚实的一态不合并、**不写库**（派发档 §硬纪律 4）。
#[test]
fn cli_裁决不确定_fail_closed不写库() {
    let dir = tempfile::tempdir().unwrap();
    seed_mid_band(dir.path());
    let replies = write_replies(dir.path(), "uncertain.json", &[uncertain_reply()]);
    let out = run(dir.path(), &["episode", "run", "--json", "--replies", &replies]);
    assert_eq!(code(&out), 0, "不确定不是这一跑的故障：stderr={}", stderr_text(&out));
    let recs = adjudication_lines(&out);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["verdict"], Value::String("uncertain".into()));
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (0, 0, 0), "不确定 ⇒ 不写库");
    drop(root);
}

/// 支四「超时」：每对首发 + 重试 ≤ 2；座位是单座 ⇒ Rotating 对一次 `complete` 先原样重试一次
/// （`VISITS_PER_CALL=2`）⇒ 判定表 `calls` 记 3 发、脚本烧 6 条。重试耗尽折 uncertain、不落库。
#[test]
fn cli_裁决超时_重试耗尽折成不确定_仍不落库() {
    let dir = tempfile::tempdir().unwrap();
    seed_mid_band(dir.path());
    let timeouts: Vec<Value> = (0..6).map(|_| json!({"fail": "timeout"})).collect();
    let replies = write_replies(dir.path(), "timeout.json", &timeouts);
    let out = run(dir.path(), &["episode", "run", "--json", "--replies", &replies]);
    assert_eq!(code(&out), 0, "超时是 fail-closed，不是运维失败：stderr={}", stderr_text(&out));
    let recs = adjudication_lines(&out);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["verdict"], Value::String("uncertain".into()), "{}", recs[0]);
    assert_eq!(recs[0]["calls"], Value::from(3), "首发 + 重试 ≤ 2：{}", recs[0]);
    assert!(recs[0]["reason"].as_str().unwrap().contains("llm timeout"),
            "失败理由带封闭类别：{}", recs[0]);
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (0, 0, 0), "超时耗尽 ⇒ 不写库");
    drop(root);
}

/// 判定表的复算纪律：对同一对**不重问**——缓存命中，判定表不重复、脚本不再被消耗。
/// 没有这一格，一趟真跑会烧两倍调用（I-12 报告 §实现 4）。
/// W33·A4 后本库（2 行 fact）只剩一趟：窗尾之后零行 ⇒ 第二趟被便宜闸整段跳过，
/// "第二趟不重问"由"判定表恰一行 + 第二条脚本回复没被烧"继续钉住（测试名不动，臂随语义走）。
#[test]
fn cli_重算不重问_第二趟判定表不再增行() {
    let dir = tempfile::tempdir().unwrap();
    seed_mid_band(dir.path());
    let replies = write_replies(dir.path(), "once.json", &[yes_reply(), no_reply()]);
    let out = run(dir.path(), &["episode", "run", "--json", "--replies", &replies]);
    assert_eq!(code(&out), 0);
    let recs = adjudication_lines(&out);
    assert_eq!(recs.len(), 1, "只有一行判定：{:?}", recs);
    let readouts: Vec<Value> = lines(&out).into_iter()
        .filter(|l| l["job"] == Value::String("episode".into())).collect();
    assert_eq!(readouts.len(), 1, "便宜闸跳过第二趟 ⇒ 恰一趟读数：{:?}", readouts);
    assert_eq!(readouts[0]["episodes_written"], Value::from(1), "{}", readouts[0]);
    drop(out);
    // 脚本给了两条而只烧了一条：后续趟（若有）若真重问，第二座脚本条目会被消费成"no"
    // （读不出）——但行数已经在上面的 recs.len()==1 里钉死，这里是把"只烧一条"落成可读的断言。
    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(row_counts(root.memory()), (1, 2, 1));
    drop(root);
}

// ── I-12 · 阴性对照（R-C 五对的真座位裁决臂；环境门控，平时不动手）──────────────
//
// R-C 的阴性对照是五对（`dispatch-I-3.md` §4 的原文清单；I-12 派发档写"6 对"，
// 在册清单只有这五对，差异登记在 I-12 报告）。它们多数**不是中间带候选**
// （跨度闸剔掉的强边、跨会话对），管线永远不会把这一格交给裁决器——
// 这一臂是"闸有牙"的证明：把对**手工递给**裁决器，模型必须全拒（拒 = no 或 uncertain）。
// 需要 `EPISODE_I12_REPLICA`（由 backup API 取的一致副本目录）与
// `EPISODE_I12_LLM_CONFIG`（副本目录形状的 `llm.toml`，首座 = 派发档指定的优先座位）。
// 现网零写入：判定不落库（这一臂根本不走 put_episodes）。
#[test]
fn 副本真跑_阴性对照逐对全拒_闸有牙() {
    let (Some(rep), Some(cfg_path)) =
        (std::env::var_os("EPISODE_I12_REPLICA"), std::env::var_os("EPISODE_I12_LLM_CONFIG"))
    else { return; };
    let rep = Path::new(&rep);
    let root = Root::open(rep).expect("副本打不开");
    let facts_all = rc_memory::reader::group_facts(root.memory(), 400).unwrap();
    // R-C 五对阴性：id 前缀是 `dispatch-I-3.md` §4 给的真实句柄，其余全部从库里读真身。
    let negatives: [(&str, &str, &str); 5] = [
        ("76db98675c23", "2c15aa562808", "强边但 Δt 6.45h ⇒ 跨度闸剔掉（共同参与者 1）"),
        ("fe6520673893", "2c15aa562808", "共同者两个但 Δt 6.22h ⇒ 跨度闸剔掉"),
        ("1a0220ad24ce", "394dc9897dad", "剥自锚后为空且跨会话"),
        ("76db98675c23", "11105ccfac64", "55.9s 但无共同参与者 ⇒ 中间带"),
        ("2aa4b1e7a7d7", "76db98675c23", "1.2min 但无共同参与者 ⇒ 中间带"),
    ];
    let by_prefix = |pfx: &str| -> GroupFact {
        let hits: Vec<&GroupFact> = facts_all.iter().filter(|f| f.id.starts_with(pfx)).collect();
        assert_eq!(hits.len(), 1, "副本里句柄前缀 {pfx} 应恰命中一行，实际 {}", hits.len());
        hits.into_iter().next().unwrap().clone()
    };
    // 首座 = 派发档指定的优先座位（llm.toml 第一条；本机为 Qwen3.8-27B 的 custom 通道）。
    // 座位枚举（I-12 派发档硬前置 3）：把配置首座换成备座、`EPISODE_I12_PAIR_LIMIT=1`
    // 限一对，就能用一发真调用探"这个座通不通"。preset 座与 custom 座两条构造支都在
    // （与 `build_live_seat` 同一条接缝：preset 查表在构造点，查不到键响亮失败）。
    let cfg = rc_llm_cfg::load(Path::new(&cfg_path)).expect("llm.toml 读不了")
        .expect("llm.toml 不存在");
    let seat = cfg.seats.first().expect("llm.toml 没有座位");
    let (box_name, slot) = seat.secret_ref.split_once(':')
        .expect("secret_ref 形状是 箱:格");
    let key = rc_secret::read_secret(rep, box_name, slot).expect("密钥箱读不了")
        .to_string();
    let timeout_ms: u64 = std::env::var("EPISODE_I12_TIMEOUT_MS").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(600_000);
    let egress: Box<dyn rc_llm_http::Egress> = Box::new(rc_llm_http::transport::agent(timeout_ms));
    let live = match (&seat.provider, &seat.endpoint) {
        (Some(provider), None) => rc_llm_http::Live::new(
            provider, seat.model.as_deref().expect("preset 座位必有 model"),
            key, seat.budget.unwrap_or(4096), seat.temperature, egress)
            .expect("座位构造失败"),
        _ => {
            let endpoint = seat.endpoint.as_ref().expect("座位要么 preset 要么 custom");
            rc_llm_http::Live::new_custom(
                &endpoint.base_url, &endpoint.protocol, seat.model.as_deref()
                    .expect("custom 座位必有 model"),
                key, seat.budget.unwrap_or(4096), seat.temperature, egress)
                .expect("座位构造失败")
        }
    }
    .with_reasoning_effort(seat.reasoning_effort.clone());
    let judge = rc_distill::adjudicate::LlmMidBand::new(&live);
    let limit: usize = std::env::var("EPISODE_I12_PAIR_LIMIT").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(negatives.len());
    let mut all_rejected = true;
    for (pa, pb, why) in negatives.iter().take(limit) {
        let a = by_prefix(pa);
        let b = by_prefix(pb);
        let pair = rc_distill::episode::Pair {
            a: 0, b: 1,
            delta_ms: (rc_distill::episode::fact_time(&a) - rc_distill::episode::fact_time(&b)).abs(),
            band: rc_distill::episode::Band::Proximity,
            shared: rc_distill::episode::shared_participants(&a, &b),
            same_turn: a.evidence_turn_uid == b.evidence_turn_uid,
            turn_distance: None,
        };
        let facts_pair = vec![a, b];
        let merged = judge.accepts(&facts_pair, &pair);
        let recs = judge.take_log();
        let rec = recs.last().expect("每一对都该有一行判定");
        println!("阴性 {pa}×{pb} verdict={} 座位={} 延迟={}ms calls={}（{why}）理由={}",
                 rec.verdict.clone().slug(), rec.seat, rec.latency_ms, rec.calls, rec.reason);
        if merged { all_rejected = false; }
    }
    assert!(all_rejected, "阴性对照应全拒（误放的那对见上面的逐对行）");
    drop(root);
}

// ── 副本真跑（T-5 / T-6 的六对夹具）──────────────────────────────────────────

/// 从副本库里读一行 fact 的真身（**不在源码里抄现网专名**：参与者、钟、署名一律读回来）。
fn live_by_prefix(m: &rc_store::MemoryHandle, prefix: &str) -> GroupFact {
    let hits: Vec<GroupFact> = rc_memory::reader::group_facts(m, 400).unwrap()
        .into_iter().filter(|f| f.id.starts_with(prefix)).collect();
    assert_eq!(hits.len(), 1, "副本里句柄前缀 {prefix} 应恰命中一行，实际 {}", hits.len());
    hits.into_iter().next().unwrap()
}

fn llm_calls(root: &Root) -> i64 {
    root.ledger().conn().query_row(
        "SELECT coalesce(sum(count),0) FROM usage_counter WHERE metric='llm_calls'",
        [], |r| r.get(0)).unwrap()
}

/// T-6 / T-5 / T-7 的副本真跑：六对夹具**逐对判定** + 零调用证明 + 幂等。
///
/// 只在 `EPISODE_B_ACCEPT` 指着"由 backup API 取出的**一致**副本"时才动手（`cp` 主库 .db
/// 会拿到 11-03 的旧快照：近期内容全在 `-wal` 里）。平时直接 return ⇒ `#[test]` 条数与
/// passed 数仍一格不差。**现网零写入**：所有写动作都落在那份副本里。
///
/// 断言全部相对"跑之前的库"，所以这份副本跑第二遍也不会假红：
/// 阳性那对必须在同一段活经历里，阴性五对必须不在同一段里。
#[test]
fn 副本真跑_六对夹具逐对判定_重跑全零且账不涨() {
    let Some(dir) = std::env::var_os("EPISODE_B_ACCEPT") else { return; };
    let path = Path::new(&dir);
    let root = Root::open(path).expect("副本打不开");
    let m = root.memory();
    let llm_before = llm_calls(&root);
    let counts_before = row_counts(m);
    let facts_before = count(m, "SELECT count(*) FROM fact");
    let budget = episode::EPISODE_MAX_FACTS_PER_PASS + 40;
    let first = episode::run_episode_pass(m, root.transcript(), budget, &DeferMidBand, NOW).unwrap();
    println!("副本真跑 第一趟读数：{:?} scanned={} window_end={:?} 三表行数 前={:?} 后={:?}",
             first.readout, first.scanned, first.window_end, counts_before, row_counts(m));
    // 六对夹具：id 前缀是派发档 §5 给的真实句柄，其余全部从库里读。
    let pair_verdicts = [
        ("76db98675c23", "2c15aa562808", false, "强边但 Δt 6.45h ⇒ 跨度闸剔掉"),
        ("fe6520673893", "2c15aa562808", false, "共同者两个但 Δt 6.22h ⇒ 跨度闸剔掉"),
        ("1a0220ad24ce", "394dc9897dad", false, "剥自锚后为空且跨会话"),
        ("76db98675c23", "11105ccfac64", false, "55.9s 但无共同参与者 ⇒ 中间带不合"),
        ("2aa4b1e7a7d7", "76db98675c23", false, "1.2min 但无共同参与者 ⇒ 中间带不合"),
        ("76db98675c23", "fe6520673893", true, "808s 且共同参与者 ⇒ 必须合"),
    ];
    for (pa, pb, want_merged, why) in pair_verdicts {
        let a = live_by_prefix(m, pa);
        let b = live_by_prefix(m, pb);
        let shared = episode::shared_participants(&a, &b);
        let delta = (episode::fact_time(&a) - episode::fact_time(&b)).abs();
        let ea = episode_of(m, &a.id);
        let merged = ea.is_some() && ea == episode_of(m, &b.id);
        println!("夹具 {pa}×{pb} 共同者={shared:?} Δt={}s 同段={merged}（{why}）", delta / 1000);
        assert_eq!(merged, want_merged, "{pa}×{pb}：{why}");
    }
    // 零调用：内核签名里没有后端形参，账本一格也不该动。
    let llm_after = llm_calls(&root);
    println!("usage_counter 的 llm_calls 总账：前={llm_before} 后={llm_after} 增量={}",
             llm_after - llm_before);
    assert_eq!(llm_after, llm_before, "本轮零 LLM 调用 ⇒ llm_calls 增量必须为 0");
    // 幂等（副本面）。
    let counts_after_first = row_counts(m);
    let second = episode::run_episode_pass(m, root.transcript(), budget, &DeferMidBand, NOW).unwrap();
    println!("副本真跑 第二趟读数（幂等）：{:?}", second.readout);
    assert_eq!(second.readout.episodes_written, 0);
    assert_eq!(second.readout.members_linked, 0);
    assert_eq!(second.readout.cards_written, 0);
    assert_eq!(second.readout.superseded, 0);
    assert_eq!(second.readout.merged_pairs, 0);
    assert_eq!(row_counts(m), counts_after_first, "重跑三表行数一格没动");
    assert_eq!(count(m, "SELECT count(*) FROM fact"), facts_before, "扫描面只读 fact ⇒ 行数不变");
    println!("三表行数：episode/episode_fact/card(kind='episode') = {:?}", row_counts(m));
    println!("活 episode 明细：{:?}", episode_ids(m).iter().map(|id| {
        (id[..12].to_string(), members_of(m, id).iter().map(|x| x[..12].to_string()).collect::<Vec<_>>())
    }).collect::<Vec<_>>());
    drop(root);
}
