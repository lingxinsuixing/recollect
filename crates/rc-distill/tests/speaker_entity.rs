//! D 子 agent 接线的抽取侧：轮级说话人锚进候选、进 TARGET 行。
//!
//! 数据侧早就有这一格：转换器把子 agent 的报告写成 `role=agent` + `speaker="subagent:…"`，
//! 入库时 `record_turn_speakers` 为该名字建 `kind='agent'` 锚并写一条
//! `mention(discourse_role='speaker', resolution='resolved')`。抽取侧从前不读它，
//! 渲染的 `speaker=` 只按 role 推 ⇒ 子 agent 的报告被写成主 agent 自己说的。
//!
//! 三条判据（派发档 `2026-10-30-s1-speaker-entity`）：
//! 1. 有 speaker mention 的轮 ⇒ TARGET 渲染出 `<该锚的 a-ref>`，且该锚在候选里；
//! 2. **没有 speaker 的轮 ⇒ 渲染与改动前逐字一致**（`no_speaker_turn_renders_verbatim`）；
//! 3. 两格程序锚仍倒数第二/最后，`a*` 按最终位置连续重编。
//!
//! 夹具只用通用占位名（张三/李四/填充锚NN），不含语料专名；不跑真实 LLM。

use rc_distill::pass_event::{select_event_batch, EventPlan};
use rc_distill::pass_prompt::{event_user, MARK_PRINCIPAL, MARK_SELF};
use rc_memory::writers_x::{put_entity, EntityContent};
use rc_memory::Scope;
use rc_memory::{AnchorRole, Provenance, ScopeKind};
use rc_store::Root;

mod common;
use common::{adopt_rows, seed_bodies};

const NOW: i64 = 1_700_000_000_000;
const SESSION: &str = "sess-batch-A";
const DEVICE: &str = "dev-batch-A";
const SOURCE_AGENT: &str = "agent-batch-A";
const PRINCIPAL: &str = "p1";
/// 子 agent 的轮级说话人（转换器写的 `speaker` 字面量形状）。
const SUBAGENT: &str = "subagent:call-01";
/// 目标轮正文：不点任何候选名，免得 C3 的证据内预筛改顺序。
const HUMAN_BODY: &str = "我昨晚把配置改回去，服务就恢复了";
const AGENT_BODY: &str = "刚才某打印机卡纸，我拆开清理后恢复";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-x".into() }
}

/// 会话归属改成 `p1 / alice / project / keep`，scope 改成 `project/proj-x`。
fn adopt_session(root: &mut Root) {
    root.transcript_mut()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p1', owner_agent_id='alice',
                    visibility='project', retention='keep',
                    scope_kind='project', scope_key='proj-x', ownership_src='operator'
             WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![SESSION, DEVICE],
        )
        .unwrap();
}

fn put(root: &Root, kind: &str, canonical: &str, created_at_ms: i64) -> String {
    let key = put_entity(
        root.memory(),
        &EntityContent {
            kind: kind.into(),
            title: canonical.into(),
            scope: scope(),
            aliases: vec![],
        },
        &[Provenance {
            turn_uid: format!("prov-{canonical}"),
            anchor_role: AnchorRole::Human,
        }],
        created_at_ms,
    )
    .unwrap();
    key.id
}

/// 夹具 A：两轮（human + agent）、没有 speaker mention。候选 = 张三/李四 + 两格程序锚。
fn fixture_without_speakers() -> (tempfile::TempDir, Root, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[HUMAN_BODY, AGENT_BODY], 1_000);
    adopt_session(&mut root);
    put(&root, "person", "张三", NOW - 1_000);
    put(&root, "person", "李四", NOW - 2_000);
    adopt_rows(&root, "anchor", "alice", Some(PRINCIPAL), "project", "project", "proj-x");
    (dir, root, uids)
}

/// 夹具 B：`t2`（agent 轮）带一条轮级 speaker mention，锚是 `subagent:call-01`。
///
/// 候选池只收最近创建的 30 条：铺 30 个更晚创建的填充锚，把子 agent 锚挤出池外——
/// 它只可能由"轮级说话人补位"这一层进候选（P4a-1 的同一把尺子）。
fn fixture_with_speaker() -> (tempfile::TempDir, Root, Vec<String>, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let uids = seed_bodies(&mut root, &[HUMAN_BODY, AGENT_BODY], 1_000);
    adopt_session(&mut root);
    for i in 0..30 {
        put(&root, "person", &format!("填充锚{i:02}"), NOW - 1_000 + i as i64);
    }
    // 子 agent 锚：比全部填充锚都早 ⇒ 落在候选池的 30 格外。
    let sub_anchor = put(&root, "agent", SUBAGENT, NOW - 5_000);
    adopt_rows(&root, "anchor", "alice", Some(PRINCIPAL), "project", "project", "proj-x");
    // 轮级 speaker mention（真实写入面是 `record_turn_speakers`，这里直插按 DDL 必填列）。
    let mention_id = rc_memory::identity::compute_mention_id(&uids[1], &sub_anchor, SUBAGENT, None, None);
    root.memory()
        .conn()
        .execute(
            "INSERT INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                                 speaker_anchor_id, discourse_role, resolution, confidence,
                                 source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1,?2,?3,?4,NULL,NULL,?2,'speaker','resolved',1.0,?5,?6,?7,1)",
            rusqlite::params![
                mention_id,
                sub_anchor,
                uids[1],
                SUBAGENT,
                SOURCE_AGENT,
                DEVICE,
                NOW
            ],
        )
        .unwrap();
    (dir, root, uids, sub_anchor)
}

fn plan_of(root: &Root) -> EventPlan {
    select_event_batch(root.transcript(), root.ledger(), root.memory(), 0, NOW)
        .unwrap()
        .expect("夹具里的轮都还没跑过 event")
}

/// 判据 2 的 golden：**BASE 上**同一套夹具跑出来的整段 `event_user`，逐字节贴在这里。
///
/// 夹具里两条轮都没有 speaker mention（`rows[*].speaker` 全空），所以这段输出必须与
/// 改动前一字不差——它就是"没有 speaker 的轮渲染逐字不变"的证据。任何一格漂移
/// （`speaker=` 换成别的 a-ref、候选多/少一格、多一个空格）都会让本条用例红。
const NO_SPEAKER_RENDER: &str = "\
PASS event v0
CTX: (none)
TARGET [t1] 1970-01-01T08:00+08:00 human speaker=a4: 我昨晚把配置改回去，服务就恢复了
TARGET [t2] 1970-01-01T08:00+08:00 agent speaker=a3: 刚才某打印机卡纸，我拆开清理后恢复
说话人说明：human 轮的说话人是会话主人（候选表里 kind=person 且与会话主人绑定的那一格）；agent 轮的说话人是本 agent。说话人只是默认主语；只有引文本身点名了另一个人或组织时才换主语，引文里没有可挂的主语时不要把 event/work 之类顶上。
TARGET 行末尾的 `speaker=a*` 是程序解析出的说话人短 id：有这一格，它就是该轮的默认主语，不必再从候选表反推。
CANDIDATES (actor/object/participants/place 只能引用这些 a*):
- a1 | person | 张三
- a2 | person | 李四
- a3 | agent | agent-batch-A [self]
- a4 | person | p1 [principal]
";

/// 判据 1：有 speaker mention 的轮 ⇒ TARGET 渲染该锚的 `a*`，且该锚在候选里。
#[test]
fn 有说话人mention的轮渲染成该锚的aref() {
    let (_dir, root, _uids, sub_anchor) = fixture_with_speaker();
    let plan = plan_of(&root);

    // 读：canonical 与锚 id 都来自 `mention`（不是从正文猜的）。
    assert_eq!(plan.batch.rows[1].speaker.as_deref(), Some(SUBAGENT));
    assert_eq!(
        plan.batch.rows[1].speaker_anchor_id.as_deref(),
        Some(sub_anchor.as_str())
    );
    assert_eq!(plan.batch.rows[0].speaker, None, "human 轮没有 mention ⇒ 不填");

    // 候选：子 agent 锚比 30 个填充锚都早 ⇒ 在池外，只可能由说话人补位进来。
    let at = plan
        .candidates
        .iter()
        .position(|candidate| candidate.anchor_id == sub_anchor)
        .expect("说话人锚必须被补进候选，否则模型没有 a-ref 可写");
    let reference = plan.candidates[at].reference.clone();
    assert_eq!(plan.candidates[at].kind, "agent");
    assert_eq!(plan.candidates[at].canonical, SUBAGENT);
    assert_eq!(plan.candidates[at].mark, None, "说话人锚不盖 [self]/[principal]");

    // 渲染：t2 的 speaker= 是子 agent 的 a-ref，不是主 agent 自锚。
    let user = event_user(&plan.batch, &plan.candidates, &plan.ctx_turns);
    let line = user
        .lines()
        .find(|line| line.starts_with("TARGET [t2] "))
        .expect("t2 必须在渲染里");
    let self_ref = plan
        .candidates
        .iter()
        .find(|candidate| candidate.mark == Some(MARK_SELF))
        .expect("自锚必须还在")
        .reference
        .clone();
    assert!(
        line.contains(&format!(" speaker={reference}: ")),
        "子 agent 的报告必须渲染成它自己的 a-ref（{reference}）：{line}"
    );
    assert_ne!(reference, self_ref, "不能塌回主 agent 自锚");
    assert!(user.contains(&format!("- {reference} | agent | {SUBAGENT}\n")), "{user}");
    // human 轮没有 mention，照旧走 role 推导（主人锚）。
    let principal_ref = plan
        .candidates
        .iter()
        .find(|candidate| candidate.mark == Some(MARK_PRINCIPAL))
        .expect("主人锚必须还在")
        .reference
        .clone();
    let human_line = user.lines().find(|line| line.starts_with("TARGET [t1] ")).unwrap();
    assert!(human_line.contains(&format!(" speaker={principal_ref}: ")), "{human_line}");
}

/// 判据 2：没有 speaker 的轮，渲染与改动前**逐字相同**（golden 来自 BASE）。
#[test]
fn 没有说话人的轮渲染逐字不变() {
    let (_dir, root, _uids) = fixture_without_speakers();
    let plan = plan_of(&root);
    assert!(
        plan.batch
            .rows
            .iter()
            .all(|row| row.speaker.is_none() && row.speaker_anchor_id.is_none()),
        "夹具里不该有任何 speaker mention"
    );
    assert_eq!(
        event_user(&plan.batch, &plan.candidates, &plan.ctx_turns),
        NO_SPEAKER_RENDER,
        "没有 speaker 的轮必须逐字回落到 role 推导"
    );
}

/// 判据 3：两格程序锚仍倒数第二/最后，新锚插在它们之前，`a*` 按最终位置连续。
#[test]
fn 两格程序锚仍在末尾且aref连续() {
    let (_dir, root, _uids, sub_anchor) = fixture_with_speaker();
    let plan = plan_of(&root);
    let n = plan.candidates.len();
    assert_eq!(n, 33, "30 个填充锚 + 说话人锚 + 自锚 + 主人锚");

    assert_eq!(plan.candidates[n - 2].mark, Some(MARK_SELF), "主 agent 锚仍倒数第二");
    assert_eq!(plan.candidates[n - 2].canonical, SOURCE_AGENT);
    assert_eq!(plan.candidates[n - 1].mark, Some(MARK_PRINCIPAL), "主人锚仍最后");
    assert_eq!(plan.candidates[n - 1].canonical, PRINCIPAL);

    let at = plan
        .candidates
        .iter()
        .position(|candidate| candidate.anchor_id == sub_anchor)
        .expect("说话人锚必须在候选里");
    assert_eq!(at, n - 3, "新锚插在两格程序锚之前");

    for (index, candidate) in plan.candidates.iter().enumerate() {
        assert_eq!(candidate.reference, format!("a{}", index + 1), "位置即契约：a* 连续");
    }
    let user = event_user(&plan.batch, &plan.candidates, &plan.ctx_turns);
    assert_eq!(
        user.lines().filter(|line| line.starts_with("- a")).count(),
        n,
        "每一格候选都渲染且只渲染一次"
    );
}

