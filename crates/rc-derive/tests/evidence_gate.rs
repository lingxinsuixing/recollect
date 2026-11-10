//! derive 的证据闸（派发档 I-2 · Phase 2.4 任务 2.4）。
//!
//! 计划 §8.2 的那一句：**"阴性夹具必须红：共现、时间相邻、同实体无机制 ⇒ 不生成因果边"**，
//! 加上本仓那条"阴性结论要有阳性对照"的规矩：同一个夹具里必须有一条**带着原文机制句**的链
//! **落得进库**，否则那三条阴性只是"闸太松 / 管道整个断了"的副产品，不携带任何信息。
//!
//! 三条阴性各自钉一道**确定性**判据（不是靠措辞猜）：
//! (a) 纯同轮共现 —— 链**一条依据行都没点名**（`basis: []`）⇒ 证据闸拒收（说不出凭什么 ⇒ 不落）；
//! (b) 时间相邻无机制 —— 机制句整串就是套话 `"时间相邻"` ⇒ 机制闸拒收；
//! (c) 同实体无机制句 —— 机制句整串就是套话 `"同一个实体"` ⇒ 机制闸拒收。
//!
//! **为什么阴性不落在 `fact_link` 上**（P-3 的那一格已知条件）：`put_edges_anchored` 的 I4 门槛
//! 判据①"两端共享 mention"，而 `memory_mention` 里今天**没有 card/anchor 行** ⇒ 对 card 端点恒为空集，
//! `part_of` 必被 drop。那条闸的有无与本闸无关，混在一条用例里就分不清"红是因为证据闸还是因为 mention 空集"。
//! 因果链走的是 `put_chains_merged`（**没有** I4 门槛），所以这里的红只能来自证据闸；
//! 写侧 `fact_link` 那一层的三条阴性与阳性对照在 `crates/rc-memory/tests/edges_evidence.rs`
//! （那里把 I4 的锚条件**先满足**，再让闸单独决定落不落）。
//!
//! 每条阴性跑**两层**：parse 层（答复进得来吗）+ `derive_pass` 层（库里落没落行）。
//! 变异证明（T-5）删掉 parse 那一支的两道新闸门 ⇒ 恰好 (a)(b)(c) 三条红、阳性对照照绿。

use rc_derive::derive::derive_pass;
use rc_derive::parse::{parse_derive_output, REJECT_NO_EVIDENCE, REJECT_NO_MECHANISM};
use rc_llm::backend::{LlmBackend, LlmFailure, Prompt};
use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_entity, EntityContent, FactContent};
use rc_memory::{reader, Provenance};
use rc_store::Root;
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-evidence".into() } }
fn prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }]
}
fn card(title: &str, body: &str) -> FactContent {
    FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                  scope: scope(), valid_to_ms: None }
}

/// 脚本化后端：按调用顺序消费预制回复（与 `derive_pass.rs` 同一形状）。
struct Scripted { replies: Vec<String>, i: usize }
impl Scripted {
    fn new(reply: serde_json::Value) -> Self {
        Scripted { replies: vec![reply.to_string()], i: 0 }
    }
}
impl LlmBackend for Scripted {
    fn complete(&self, _p: &Prompt) -> Result<String, LlmFailure> {
        self.replies.get(self.i).cloned().ok_or(LlmFailure::SchemaMismatch)
    }
}

/// 三条真卡 + 一条实体锚（锚只是为了让 I6 的补铸有得可铸，不参与本闸的判定）。
/// 返回**按批序排列的 `RowFacts`**：位置别名 r1 / r2 / r3 就是这三条。
/// 为什么是三条：`provenance` 的主键是 `(mem_table, mem_id, turn_uid)`，而链的环节行先落
/// （`put_chains_merged` 里 C-3 那句承重墙）⇒ 环节同名的那几条依据会被 `INSERT OR IGNORE` 吞掉。
/// 要让阳性对照在库里**看得见一条 `role='basis'` 的出处行**，依据里就得有一条不是环节的句柄。
fn seeded(root: &Root) -> Vec<rc_memory::RowFacts> {
    let m = root.memory();
    put_entity(m, &EntityContent { kind: "concept".into(), title: "额度封顶".into(),
                                   scope: scope(), aliases: vec![] }, &prov(), 900).unwrap();
    let k1 = put_card(m, &card("额度封顶先落地",
                               "额度封顶在 11-04 先落地，压缩触发点因此不再按 384K 走。"),
                      &prov(), 1000).unwrap();
    let k2 = put_card(m, &card("回退链不再触发",
                               "回退链的 384K 触发点被顶掉，改由额度封顶决定。"),
                      &prov(), 2000).unwrap();
    let k3 = put_card(m, &card("封顶的原文出处",
                               "额度封顶这一条写在原文里，是上面两行共同的出处。"),
                      &prov(), 3000).unwrap();
    vec![reader::lookup(m, &k1).unwrap().unwrap(),
         reader::lookup(m, &k2).unwrap().unwrap(),
         reader::lookup(m, &k3).unwrap().unwrap()]
}

/// 一条链的答复：机制句与依据行由调用方给（阴性/阳性只差这两格），环节固定 r1→r2。
fn reply_with(mechanism: &str, basis: Vec<&str>) -> serde_json::Value {
    json!({"relationships": [], "timeline": [],
           "chains": [{"from": ["r1", "r2"], "mechanism": mechanism,
                       "type": "causes", "basis": basis}]})
}

/// 库面上"这条链落了吗"的唯一读数：`card(kind='causal')` 的行数。
fn causal_rows(root: &Root) -> i64 {
    root.memory().conn().query_row("SELECT count(*) FROM card WHERE kind='causal'", [],
                                   |r| r.get(0)).unwrap()
}

/// (a) 纯同轮共现：机制句只是"两条在同一轮里一起被提到"，而 `basis` 一格依据行都没点名。
/// "共现"本身说不出凭什么，而**说不出依据行的链在库里没有可重算的出处** ⇒ 证据闸必须拦。
#[test]
fn a_chain_that_only_shares_the_same_turn_is_rejected_and_lands_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let rows = seeded(&root);
    let reply = reply_with("两条都在同一轮里被提到", vec![]);

    let p = parse_derive_output(&reply.to_string(), &rows).unwrap();
    assert!(p.chains.is_empty(), "没点依据行的链不许穿过 parse，拿到 {:?}", p.chains);
    assert_eq!(p.rejected, 1, "共现链计一次拒收");
    assert_eq!(p.rejected_codes, vec![REJECT_NO_EVIDENCE],
        "拒收必须**分诊得出来**：这一条是'一条依据行都没点名'，不是'机制句是套话'");

    let out = derive_pass(root.memory(), &rows, &Scripted::new(reply), NOW).unwrap();
    assert_eq!(out.rejected, 1, "`rejected` 读数必须看见这一拒（不是静默丢弃）");
    assert_eq!(causal_rows(&root), 0, "被证据闸拒收的链一个字节都不许落库");
}

/// (b) 时间相邻无机制：依据行点了（`basis: ["r1"]`），但机制句整串就是套话"时间相邻"。
/// A 在 B 之前只说明顺序，不证明因果（计划 D3）⇒ 套话机制必须被拒。
#[test]
fn a_chain_that_states_time_adjacency_as_its_mechanism_is_rejected_and_lands_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let rows = seeded(&root);
    let reply = reply_with("时间相邻", vec!["r1"]);

    let p = parse_derive_output(&reply.to_string(), &rows).unwrap();
    assert!(p.chains.is_empty(), "套话机制的链不许穿过 parse，拿到 {:?}", p.chains);
    assert_eq!(p.rejected, 1, "套话机制计一次拒收");
    assert_eq!(p.rejected_codes, vec![REJECT_NO_MECHANISM],
        "这一条的码是'交不出机制句'——它点了依据行，被拦的是那句套话本身");

    let out = derive_pass(root.memory(), &rows, &Scripted::new(reply), NOW).unwrap();
    assert_eq!(out.rejected, 1, "`rejected` 读数必须看见这一拒");
    assert_eq!(causal_rows(&root), 0, "时间相邻不是机制，被拒的链不落库");
}

/// (c) 同实体无机制句：两行确实讲同一个实体，机制句整串却是"同一个实体"。
/// 同现实体是去重、不是关系（反例四支第 2 条）⇒ 同一道闸。
#[test]
fn a_chain_about_the_same_entity_without_a_mechanism_sentence_is_rejected_and_lands_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let rows = seeded(&root);
    let reply = reply_with("同一个实体", vec!["r1", "r2"]);

    let p = parse_derive_output(&reply.to_string(), &rows).unwrap();
    assert!(p.chains.is_empty(), "同实体套话的链不许穿过 parse，拿到 {:?}", p.chains);
    assert_eq!(p.rejected, 1, "同实体套话计一次拒收");
    assert_eq!(p.rejected_codes, vec![REJECT_NO_MECHANISM],
        "同实体那一支与被点名依据行的那一支是同一道闸（机制句本身说不出事情）");

    let out = derive_pass(root.memory(), &rows, &Scripted::new(reply), NOW).unwrap();
    assert_eq!(out.rejected, 1, "`rejected` 读数必须看见这一拒");
    assert_eq!(causal_rows(&root), 0, "同实体不是机制，被拒的链不落库");
}

/// 阳性对照（本仓规矩：阴性结论要有阳性对照）：**机制句是从两行原文里复述出来的**、
/// 并且点名的依据行 ⇒ 必须落库，而且落的那一格是"证据"本身——
/// 机制句进链头行的正文（`card.text`），依据行进 `provenance` 的 `role='basis'` 行。
/// 这一条绿，(a)(b)(c) 的红才只可能是闸造成的，而不是"管道整个断了"。
#[test]
fn a_chain_quoting_a_mechanism_sentence_from_the_rows_lands_with_its_basis_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let rows = seeded(&root);
    // 这一句是"从两行原文复述出来的机制"，而**不是**任何一行的正文本身——`put_chains_merged`
    // 那道 I5 文本降权（与任一环节正文的字符二元组 Jaccard ≥ 0.7 ⇒ 判为复述）不该吃掉阳性对照。
    let mechanism = "先立的那条封顶决定取消了旧的容量阈值，于是回退不再按容量触发";
    let reply = reply_with(mechanism, vec!["r1", "r3"]);

    let p = parse_derive_output(&reply.to_string(), &rows).unwrap();
    assert_eq!(p.chains.len(), 1, "有机制句又有依据行的链必须穿过 parse");
    assert_eq!(p.rejected, 0, "阳性对照不该被闸误伤");
    assert!(p.rejected_codes.is_empty(), "阳性对照不许留下一支拒收码：{:?}", p.rejected_codes);
    assert_eq!(p.chains[0].basis_ids.len(), 2, "两条依据行都要解回真实行 id");

    let out = derive_pass(root.memory(), &rows, &Scripted::new(reply), NOW).unwrap();
    assert_eq!(out.rejected, 0, "这一轮的 `rejected` 必须是 0：闸没有吃掉合法的链");
    assert_eq!(causal_rows(&root), 1, "合法的链必须落一条因果头行");
    let head: String = root.memory().conn().query_row(
        "SELECT id FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    let body: String = root.memory().conn().query_row(
        "SELECT text FROM card WHERE id=?1", [&head], |r| r.get(0)).unwrap();
    assert_eq!(body, mechanism, "机制句就是链头行的正文（P-1：证据落在被人读的格子上）");
    let basis_rows: i64 = root.memory().conn().query_row(
        "SELECT count(*) FROM provenance WHERE mem_table='card' AND mem_id=?1 AND role='basis'",
        [&head], |r| r.get(0)).unwrap();
    assert_eq!(basis_rows, 1,
        "环节 r1/r2 先以 role='link' 占了出处行，依据里的 r3 不是环节 ⇒ `role='basis'` 恰好一格");
}

/// prompt 侧必须**问得出**这句话（只加程序闸、不问模型 = 闸天天红、模型天天答不上来）：
/// 机制句 + 至少一条依据行是链的必答项，且"套话机制"点名是反例。
/// 撤掉 prompt 里那一句 ⇒ 本条红（与 (a)(b)(c) 的闸变异是两个不同的变异，分开跑）。
#[test]
fn the_prompt_asks_for_a_mechanism_sentence_and_a_named_basis_row() {
    let s = rc_derive::prompt::derive_system();
    assert!(s.contains("MECHANISM SENTENCE"),
            "prompt 没有把机制句钉成链的必答项：{s}");
    assert!(s.contains("at least one basis row"),
            "prompt 没有要求链点名依据行：{s}");
    assert!(s.contains("A placeholder mechanism"),
            "prompt 没有把套话机制列为反例：{s}");
}

// ── 派发档 I-2b · B-1：分诊五格的**产地**（不经子进程，就在 `derive_pass` 这一层）────
//
// `hub derive run` 的 JSONL 五格是从这里来的，所以这一条钉的是"每一型进自己那一格"，
// 而总格 `rejected` 的口径一字未动（母档 P-5）。它与 hub 那条用例的分工：
// 那一条看运维面（键在不在、等式在进程边界上成不成立），这一条看产地（分格的语义对不对）。
// 两型残差格子（`rejected_parse_other` / `rejected_chain_merged`）也必须有证人：
// 没有它们，运维面那句"三类相加 = 总格"就只在残差为 0 的那一次答复里成立（那是巧合不是恒等式）。
#[test]
fn derive_pass_puts_every_rejection_shape_in_its_own_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut rows = seeded(&root);
    // 第四条卡**不提那个实体** ⇒ 它与 r1 既无共享 mention 也无逐字同实体（I4 只可能挡这一支）。
    let k4 = put_card(root.memory(), &card("另一件不相干的事",
                                           "这一行讲的是排期，与上面三行没有共同的被提及对象。"),
                      &prov(), 4000).unwrap();
    rows.push(rc_memory::reader::lookup(root.memory(), &k4).unwrap().unwrap());

    // 五型各一条：套话机制 / 无依据行 / 锚门槛 / 枚举外 kind（无码）/ 同序链的第二条（合并）。
    let real = "第二次审核把口径改成按调用计数，于是旧的行数预算不再约束这一族";
    let reply = json!({
        "relationships": [
            {"from": "r1", "to": "r4", "kind": "part_of",
             "reason": "两端没有共同提及的结构归属", "basis": ["r1", "r4"]},
            {"from": "r1", "to": "r2", "kind": "co_occurs",
             "reason": "两条一起出现", "basis": ["r1"]}
        ],
        "chains": [
            {"from": ["r1", "r2"], "mechanism": "时间相邻", "type": "causes", "basis": ["r1"]},
            {"from": ["r2", "r3"], "mechanism": real, "type": "enables", "basis": []},
            {"from": ["r1", "r2"], "mechanism": real, "type": "causes", "basis": ["r3"]},
            {"from": ["r1", "r2"], "mechanism": real, "type": "causes", "basis": ["r3"]}
        ],
        "timeline": [], "mentions": []
    });

    let out = derive_pass(root.memory(), &rows, &Scripted::new(reply), NOW).unwrap();
    assert_eq!((out.rejected_no_mechanism, out.rejected_no_evidence, out.rejected_anchor_gate),
               (1, 1, 1), "证据闸两道与锚门槛各一条，必须落在各自那一格：{out:?}");
    assert_eq!((out.rejected_parse_other, out.rejected_chain_merged), (1, 1),
               "枚举外 kind 与同序链的第二条是另两型拒收，不许混进上面三格：{out:?}");
    assert_eq!(out.rejected, 5, "总格的口径未变：三格 parse 拒收 + 1 条锚门槛 + 1 条合并");
    assert_eq!(out.rejected, out.rejected_no_mechanism + out.rejected_no_evidence
                            + out.rejected_anchor_gate + out.rejected_parse_other
                            + out.rejected_chain_merged,
        "分计之和必须恰好等于总格（少一格就会在这里露出来）：{out:?}");
    assert_eq!(causal_rows(&root), 1, "只有那条合法的链落库（第二条同序的被合并）");
    let links: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    assert_eq!(links, 0, "两条关系答复一条被锚门槛挡、一条被枚举挡 ⇒ 边一行都不许落");
}

/// 残差格不许吃掉证据闸那两格：同一型拒收**出现两次**时分计要跟着到 2（防止有人把分计
/// 写成 `min(1, …)` 之类的"有没有"而不是"几条"）。
#[test]
fn the_evidence_gate_buckets_count_rejections_rather_than_only_recording_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let rows = seeded(&root);
    let reply = json!({
        "relationships": [], "timeline": [], "mentions": [],
        "chains": [
            {"from": ["r1", "r2"], "mechanism": "时间相邻", "type": "causes", "basis": ["r1"]},
            {"from": ["r2", "r3"], "mechanism": "同一个实体", "type": "enables",
             "basis": ["r2"]},
            {"from": ["r1", "r3"], "mechanism": "两条都在同一轮里被提到", "type": "prevents",
             "basis": []}
        ]
    });
    let out = derive_pass(root.memory(), &rows, &Scripted::new(reply), NOW).unwrap();
    assert_eq!(out.rejected_no_mechanism, 2, "两支套话机制 ⇒ 计数到 2，不是记一件有此事");
    assert_eq!(out.rejected_no_evidence, 1, "第三条被前一道闸拦下的码是 no_evidence");
    assert_eq!(out.rejected, 3);
    assert_eq!(out.rejected_parse_other + out.rejected_anchor_gate + out.rejected_chain_merged,
               0, "这一轮没有第四、第五型：{out:?}");
    assert_eq!(causal_rows(&root), 0, "三条都不落库");
}
