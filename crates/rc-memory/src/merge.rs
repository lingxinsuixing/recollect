//! 实体合并：把"同一实体的两格锚"收成一格（canonical 同名 / alias 指到别的锚）。
//!
//! 与 [`crate::rebind`] 的分工：rebind 是**一格到一格**的换绑（操作员指名 from/into，
//! 校验、重键、别名归并、`status='merged'`、`memory_op` 台账都在那里）；本模块只多做三件
//! 换绑不该知道的事：
//!
//! 1. **发现**：扫活跃锚（`status IN ('active','provisional')`）的正名与别名，按
//!    `lower(trim(name))` 建索引，取"同一名字被两格以上认领"的连通分量。名字相等是**词面**
//!    判定，不做任何相似度推断（`MiniCPM-V-35B` 与 `MiniCPM-V-4.7-35B` 不会因为长得像而合并）。
//! 2. **分类**（三档，只有 A 可自动）：
//!    - **A** 同 kind、同 scope、恰好两格、名字把它们精确连起来 ⇒ `action=merge`；
//!    - **B** 跨 kind 同名 ⇒ 只出人工清单（"person 老方"与"work 老方"是重新命名，不是换绑）；
//!    - **C** 同 kind 但目标不唯一（同一名字被三格以上认领）或 scope 不同 ⇒ 只出清单。
//!
//!    连通分量里只要混进一个跨 kind 的成员，整组按 B 处理——"先合一部分"会留下半个实体。
//! 3. **落账**：A 类的写动作**整笔复用** `rebind::rebind_apply_tx`（事务内那一半），再在
//!    **同一个事务**里写 `anchor_merge(from_id,to_id,run_id,reason,created_at_ms)`。合并成功而
//!    审计行丢失，等于事后无法回答"这一格为什么不见了"，所以两者不许分两个事务。
//!
//! 旧锚的"退役形状"就是换绑既有的那一格：`status='merged'` + 正名并进新锚的 `alias` +
//! `memory_op(op='MERGE')`。本模块**不**物理删锚，也不把 `status` 改成 `retired`——`retired`
//! 在库里没有生产者、也没有读者，写它等于把"合并"这件可解释的事换成一句没人认领的标签。
//!
//! 幂等：plan 只看活跃锚，合并过的 from 立刻退出索引 ⇒ 第二次运行同一批候选为空、
//! `merged=0`。`anchor_merge` 再补一条 `WHERE NOT EXISTS` 兜底，重复调用不会长出第二行。
//!
//! 可逆性（必须写在明面上）：换绑是**内容寻址重键**（`mention.id`、`state_assertion.id`
//! 都把锚算进哈希）。前向一旦发生 id 重键、或 `provenance`/`resource_owner` 撞车被
//! `INSERT OR IGNORE` 丢掉，**仅凭 `anchor_merge` 一行无法反向还原**：原始 mention 的
//! `(turn, surface, span)` 与撞车前的归属行都不在审计行里。要能回滚，只有两条路——
//! ① 前向之前留库级快照/备份；② 降级为"只归并 alias + 写审计、不重键"。本模块选前者
//! 作为操作前提（报告里写明），不假装 `anchor_merge` 本身可逆。
use crate::rebind::{rebind_apply_tx, rebind_plan, RebindError, RebindReport};
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};

/// 三档分类（闭集；顺序就是清单里的顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MergeClass {
    /// 同 kind、同 scope、名字精确相连 ⇒ 可自动合并。
    A,
    /// 跨 kind 同名 ⇒ 人工（重新命名实体，走抽取线）。
    B,
    /// 同名目标不唯一 / scope 不同 ⇒ 人工。
    C,
}

impl MergeClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }

    /// `--only` 的取值解析（大小写不敏感）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "A" => Some(Self::A),
            "B" => Some(Self::B),
            "C" => Some(Self::C),
            _ => None,
        }
    }
}

/// 清单里一行会做什么。`Skip` 留给"查到了但这一轮不动"的形状；当前 plan 不产生它
/// （已合并的锚直接退出索引，不占一行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeAction {
    Merge,
    Manual,
    Skip,
}

impl MergeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Manual => "manual",
            Self::Skip => "skip",
        }
    }
}

/// 一格锚身上的引用分项（给报告与"为什么这一格是 from"的排序用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefCount {
    pub mention: usize,
    pub memory_mention: usize,
    pub state: usize,
    pub fact: usize,
    pub provenance: usize,
    pub owner: usize,
    pub attention: usize,
}

impl RefCount {
    pub fn total(self) -> usize {
        self.mention
            + self.memory_mention
            + self.state
            + self.fact
            + self.provenance
            + self.owner
            + self.attention
    }
}

/// 清单的一行。`from` 是要消失的那一格，`to` 是留下的那一格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCandidate {
    pub from_id: String,
    pub to_id: String,
    pub from_canonical: String,
    pub to_canonical: String,
    pub from_kind: String,
    pub to_kind: String,
    /// 这一行会搬走的引用总数（from 侧；口径见 [`RefCount`]）。
    pub refs: usize,
    pub class: MergeClass,
    pub action: MergeAction,
    /// 闭集/半闭集理由词：`same_kind_name_match`、`cross_kind`、`ambiguous_target`、
    /// `scope_mismatch`、`blocked:<code>`、`refused:<code>`。
    pub reason: String,
    pub from_status: String,
    pub to_status: String,
    pub from_refs: RefCount,
    pub to_refs: RefCount,
}

/// 一格锚的合并面。
#[derive(Debug, Clone)]
struct Face {
    id: String,
    kind: String,
    canonical: String,
    status: String,
    scope_kind: String,
    scope_key: String,
    created_at_ms: i64,
}

/// 名字归一：只做 `trim` + 小写。**刻意不做**全半角/标点/相似度归一——
/// 多走一步就是在替人判断"这两个名字是不是同一个实体"。
fn norm(name: &str) -> String {
    name.trim().to_lowercase()
}

/// 活跃锚（`merged`/`retired` 不进候选：已合并的 from 不许再出现）。
fn alive(m: &MemoryHandle) -> Result<Vec<Face>, MemError> {
    let mut st = m.conn().prepare(
        "SELECT id, kind, canonical, status, scope_kind, scope_key, created_at_ms
           FROM anchor WHERE status IN ('active','provisional')
          ORDER BY id",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(Face {
                id: r.get(0)?,
                kind: r.get(1)?,
                canonical: r.get(2)?,
                status: r.get(3)?,
                scope_kind: r.get(4)?,
                scope_key: r.get(5)?,
                created_at_ms: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 某格锚的引用分项（只读；12 个外键列里参与 id 的和不参与的在这里**不分**，
/// 因为这是"会被搬走多少"的读数，不是可搬性判定——可搬性只看 [`rebind_plan`] 的 blockers）。
pub fn reference_counts(m: &MemoryHandle, id: &str) -> Result<RefCount, MemError> {
    let conn = m.conn();
    let one = |sql: &str| -> Result<usize, MemError> {
        let n: i64 = conn.query_row(sql, [id], |r| r.get(0))?;
        Ok(n as usize)
    };
    Ok(RefCount {
        mention: one("SELECT count(*) FROM mention WHERE anchor_id=?1")?,
        memory_mention: one("SELECT count(*) FROM memory_mention WHERE anchor_id=?1")?,
        state: one("SELECT count(*) FROM state_assertion WHERE subject_anchor_id=?1")?,
        fact: one(
            "SELECT count(*) FROM fact
              WHERE subject_anchor_id=?1 OR object_anchor_id=?1
                 OR place_anchor_id=?1 OR speaker_anchor_id=?1",
        )?,
        provenance: one("SELECT count(*) FROM provenance WHERE mem_table='anchor' AND mem_id=?1")?,
        owner: one("SELECT count(*) FROM resource_owner WHERE mem_table='anchor' AND mem_id=?1")?,
        attention: one("SELECT count(*) FROM attention WHERE mem_table='anchor' AND mem_id=?1")?,
    })
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[rb] = ra;
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "active" => 0,
        _ => 1,
    }
}

/// 只读发现：算清单，一格都不写。已合并的锚不出现。
pub fn merge_plan(m: &MemoryHandle) -> Result<Vec<MergeCandidate>, MemError> {
    let anchors = alive(m)?;
    if anchors.len() < 2 {
        return Ok(Vec::new());
    }
    let index: BTreeMap<&str, usize> = anchors
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.as_str(), i))
        .collect();

    // 名字 -> 认领它的锚（正名与别名同一张索引：alias 指到别的锚正是这里要发现的重复）。
    let mut claims: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (i, a) in anchors.iter().enumerate() {
        claims.entry(norm(&a.canonical)).or_default().insert(i);
    }
    {
        let mut st = m.conn().prepare("SELECT anchor_id, alias FROM alias")?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (anchor_id, alias) in rows {
            // 指向 merged/retired 锚的别名行不参与发现（那些锚已退出）。
            if let Some(&i) = index.get(anchor_id.as_str()) {
                claims.entry(norm(&alias)).or_default().insert(i);
            }
        }
    }

    let mut parent: Vec<usize> = (0..anchors.len()).collect();
    for owners in claims.values() {
        if owners.len() > 1 {
            let first = *owners.iter().next().expect("non-empty");
            for &other in owners.iter().skip(1) {
                union(&mut parent, first, other);
            }
        }
    }

    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..anchors.len() {
        let root = find(&mut parent, i);
        components.entry(root).or_default().push(i);
    }

    let mut out = Vec::new();
    for (_root, members) in components {
        if members.len() < 2 {
            continue;
        }
        let kinds: BTreeSet<&str> = members.iter().map(|&i| anchors[i].kind.as_str()).collect();
        let scopes: BTreeSet<(&str, &str)> = members
            .iter()
            .map(|&i| (anchors[i].scope_kind.as_str(), anchors[i].scope_key.as_str()))
            .collect();

        let (class, base_reason) = if kinds.len() > 1 {
            (MergeClass::B, "cross_kind")
        } else if scopes.len() > 1 {
            (MergeClass::C, "scope_mismatch")
        } else if members.len() > 2 {
            (MergeClass::C, "ambiguous_target")
        } else {
            (MergeClass::A, "same_kind_name_match")
        };

        // 方向：active 优先于 provisional；同状态看引用多的一侧留下；再按入库时刻、id 定序。
        // （reference 多的一侧留下，是因为搬过去的 id 重键撞车概率更低。）
        let mut refs: BTreeMap<usize, RefCount> = BTreeMap::new();
        for &i in &members {
            refs.insert(i, reference_counts(m, &anchors[i].id)?);
        }
        let mut order = members.clone();
        order.sort_by(|&x, &y| {
            let (ax, ay) = (&anchors[x], &anchors[y]);
            status_rank(&ax.status)
                .cmp(&status_rank(&ay.status))
                .then_with(|| refs[&y].total().cmp(&refs[&x].total()))
                .then_with(|| ax.created_at_ms.cmp(&ay.created_at_ms))
                .then_with(|| ax.id.cmp(&ay.id))
        });
        let to = order[0];
        for &from in order.iter().skip(1) {
            let mut action = if class == MergeClass::A {
                MergeAction::Merge
            } else {
                MergeAction::Manual
            };
            let mut reason = base_reason.to_string();
            if class == MergeClass::A {
                // 预演也走一遍换绑的校验：搬不动的引用在这里就降级为人工，
                // 免得清单说"会合并"、apply 却整笔拒绝。
                match rebind_plan(m, &anchors[from].id, &anchors[to].id) {
                    Ok(p) if p.blockers.is_empty() => {}
                    Ok(p) => {
                        action = MergeAction::Manual;
                        reason = format!("blocked:{}", p.blockers.join(","));
                    }
                    Err(e) => {
                        action = MergeAction::Manual;
                        reason = format!("refused:{}", rebind_code(&e));
                    }
                }
            }
            let from_refs = refs[&from];
            out.push(MergeCandidate {
                from_id: anchors[from].id.clone(),
                to_id: anchors[to].id.clone(),
                from_canonical: anchors[from].canonical.clone(),
                to_canonical: anchors[to].canonical.clone(),
                from_kind: anchors[from].kind.clone(),
                to_kind: anchors[to].kind.clone(),
                refs: from_refs.total(),
                class,
                action,
                reason,
                from_status: anchors[from].status.clone(),
                to_status: anchors[to].status.clone(),
                from_refs,
                to_refs: refs[&to],
            });
        }
    }

    out.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then_with(|| norm(&a.from_canonical).cmp(&norm(&b.from_canonical)))
            .then_with(|| a.from_id.cmp(&b.from_id))
    });
    Ok(out)
}

/// 一次真正落库的合并。
#[derive(Debug, Clone)]
pub struct AppliedMerge {
    pub candidate: MergeCandidate,
    pub run_id: String,
    /// 换绑那一侧的读数（搬了几条 mention/别名/出处）。
    pub rebind: RebindReport,
}

/// apply 的读数。
#[derive(Debug, Clone, Default)]
pub struct MergeReport {
    pub merged: Vec<AppliedMerge>,
    /// 计划里说 merge、实做前复查被挡下的行（`from, to, 原因码`）。
    pub refused: Vec<(String, String, String)>,
}

impl MergeReport {
    pub fn merged_count(&self) -> usize {
        self.merged.len()
    }
}

/// 合并失败（预演不返回它：预演只有存储错误）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    ReadOnly,
    Store(String),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnly => f.write_str("handle is a read-only replica"),
            Self::Store(s) => write!(f, "{s}"),
        }
    }
}

impl From<MemError> for MergeError {
    fn from(e: MemError) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<rusqlite::Error> for MergeError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(MemError::from(e).to_string())
    }
}

/// 换绑拒绝面的闭集词。与 `hub` 的打印逐字同源；本模块拿它拼 `refused:<code>`。
pub fn rebind_code(e: &RebindError) -> &'static str {
    match e {
        RebindError::SameAnchor => "same_anchor",
        RebindError::Missing => "missing_anchor",
        RebindError::KindMismatch => "kind_mismatch",
        RebindError::ScopeMismatch => "scope_mismatch",
        RebindError::AlreadyMerged => "already_merged",
        RebindError::TargetMerged => "target_merged",
        RebindError::RedirectCycle => "redirect_cycle",
        RebindError::Blocked => "blocked",
        RebindError::ReadOnly => "read_only",
        RebindError::Store(_) => "store_error",
    }
}

/// 执行 A 类合并。`only` 之外（B/C）一行不写；`action != merge` 的行也一行不写。
///
/// 每个候选：复跑 `rebind_plan` 复查（plan 与 apply 之间可能有人改库）→ 事务内搬引用 →
/// 同一事务写 `anchor_merge` → 提交。任一候选失败**只回滚它自己**，其余继续，失败原因进
/// `report.refused`（"能搬多少搬多少"在这里不成立：单笔内部仍是一格都不半途而废）。
pub fn merge_apply(
    m: &MemoryHandle,
    reason: &str,
    actor: &str,
    now_ms: i64,
    only: MergeClass,
) -> Result<MergeReport, MergeError> {
    if m.is_read_only() {
        return Err(MergeError::ReadOnly);
    }
    let run_id = format!("merge-{now_ms}");
    let mut report = MergeReport::default();
    for c in merge_plan(m)? {
        if c.action != MergeAction::Merge || c.class != only {
            continue;
        }
        // 复查：plan 是只读快照，写之前再问一次"现在还搬得动吗"。
        match rebind_plan(m, &c.from_id, &c.to_id) {
            Ok(p) if p.blockers.is_empty() => {}
            Ok(p) => {
                report.refused.push((
                    c.from_id.clone(),
                    c.to_id.clone(),
                    format!("blocked:{}", p.blockers.join(",")),
                ));
                continue;
            }
            Err(e) => {
                report.refused.push((
                    c.from_id.clone(),
                    c.to_id.clone(),
                    format!("refused:{}", rebind_code(&e)),
                ));
                continue;
            }
        }
        let tx = m.conn().unchecked_transaction()?;
        let rebind = match rebind_apply_tx(&tx, &c.from_id, &c.to_id, reason, actor, now_ms) {
            Ok(r) => r,
            Err(e) => {
                // 事务在这里析构 = 回滚；本候选一行不留，继续下一组。
                report.refused.push((
                    c.from_id.clone(),
                    c.to_id.clone(),
                    format!("refused:{}", rebind_code(&e)),
                ));
                continue;
            }
        };
        // 审计行与换绑同一事务：合并发生了就必须有账，账在就必须真合并过。
        tx.execute(
            "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
             SELECT ?1, ?2, ?3, ?4, ?5
              WHERE NOT EXISTS(SELECT 1 FROM anchor_merge WHERE from_id=?1 AND to_id=?2)",
            params![c.from_id, c.to_id, run_id, reason, now_ms],
        )?;
        // 旧锚的退役形状 = 换绑已盖的 `merged`（这里再写一遍是幂等的显式声明：
        // 不管 rebind 将来怎么变，本命令的落点都是"不物理删、只退场"）。
        tx.execute(
            "UPDATE anchor SET status='merged', updated_at_ms=?1 WHERE id=?2",
            params![now_ms, c.from_id],
        )?;
        tx.commit()?;
        report.merged.push(AppliedMerge { candidate: c, run_id: run_id.clone(), rebind });
    }
    Ok(report)
}
