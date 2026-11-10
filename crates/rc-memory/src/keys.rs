//! 跨模块的句柄形状（§6）：类型只在这里定义一次，文本形状是唯一来源。
//! 带 SQL CHECK 的那几列（`anchor_role`/`stance`/`actor`）与枚举的对拍由 `tests\ddl_shape.rs` 执法；
//! `Verdict` 与 `ScopeKind` 按裁定 R8 故意没有 CHECK（自由 TEXT），
//! 它们的守卫是 `parse(as_str())` 往返加写侧的类型化 `set_verdict`，列归属在 `tests\ownership.rs`。

/// **`fact_link` 不在这里**：软边不是可指认句柄（裁定 3），它没有 `MemKey`、
/// 不进 `ROW_TABLES`、不进推导池；读侧走 `reader::event_link_of`，撤销走 `retire_event_link`。
/// canonical 之后只剩两张可指认的表：`anchor`（本体锚）与 `card`（消费行）。
/// 旧 `event`/`assertion` 都是消费行，统一折叠进 `card`（`kind` 是自由文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemTable { Anchor, Card }

impl MemTable {
    pub fn prefix(self) -> &'static str {
        match self {
            MemTable::Anchor => "anchor",
            MemTable::Card => "card",
        }
    }
    pub fn parse(s: &str) -> Option<MemTable> {
        match s {
            "anchor" => Some(MemTable::Anchor),
            "card" => Some(MemTable::Card),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemKey {
    pub table: MemTable,
    pub id: String,
}

impl MemKey {
    /// "<table>:<id>"——宿主 agent 写进笔记、跨会话指认的唯一形状。
    pub fn key_string(&self) -> String {
        format!("{}:{}", self.table.prefix(), self.id)
    }

    /// `key_string()` 的逆：文本形状的唯一来源因此有两个方向、却只有一个家（裁定 R16）。
    /// 按**第一个** `':'` 切分——正文/id 里可以带冒号，只有第一个是分隔点，其余原样留在 id 里。
    /// 前缀必须过 `MemTable::parse`；认不出、或没有分隔点、或冒号后为空 ⇒ `None`。
    /// 与 `MemTable::parse` 同形（`Option`，不新增错误类型）。
    pub fn parse(s: &str) -> Option<MemKey> {
        let (table, id) = s.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        Some(MemKey { table: MemTable::parse(table)?, id: id.to_string() })
    }
}

// PartialEq 按 (table, id)，Ord 手写按 key_string 字典序（§8 消费侧②的 tie-break 靠它），不许 derive。
impl PartialEq for MemKey {
    fn eq(&self, other: &Self) -> bool {
        self.table == other.table && self.id == other.id
    }
}
impl Eq for MemKey {}
impl PartialOrd for MemKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MemKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key_string().cmp(&other.key_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind { Person, Project, Entity }

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Person => "person",
            ScopeKind::Project => "project",
            ScopeKind::Entity => "entity",
        }
    }
    pub fn parse(s: &str) -> Option<ScopeKind> {
        match s {
            "person" => Some(ScopeKind::Person),
            "project" => Some(ScopeKind::Project),
            "entity" => Some(ScopeKind::Entity),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Scope {
    pub kind: ScopeKind,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorRole { Human, Agent, System }

impl AnchorRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AnchorRole::Human => "human",
            AnchorRole::Agent => "agent",
            AnchorRole::System => "system",
        }
    }
    pub fn parse(s: &str) -> Option<AnchorRole> {
        match s {
            "human" => Some(AnchorRole::Human),
            "agent" => Some(AnchorRole::Agent),
            "system" => Some(AnchorRole::System),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict { Unreviewed, Passed, Low, Failed }

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Unreviewed => "unreviewed",
            Verdict::Passed => "passed",
            Verdict::Low => "low",
            Verdict::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Verdict> {
        match s {
            "unreviewed" => Some(Verdict::Unreviewed),
            "passed" => Some(Verdict::Passed),
            "low" => Some(Verdict::Low),
            "failed" => Some(Verdict::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance { Confirmed, Corroborated, Rejected, Retired }

impl Stance {
    /// 正向票：`Confirmed | Corroborated`。
    pub fn is_positive(self) -> bool {
        matches!(self, Stance::Confirmed | Stance::Corroborated)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Stance::Confirmed => "confirmed",
            Stance::Corroborated => "corroborated",
            Stance::Rejected => "rejected",
            Stance::Retired => "retired",
        }
    }
    pub fn parse(s: &str) -> Option<Stance> {
        match s {
            "confirmed" => Some(Stance::Confirmed),
            "corroborated" => Some(Stance::Corroborated),
            "rejected" => Some(Stance::Rejected),
            "retired" => Some(Stance::Retired),
            _ => None,
        }
    }
}

/// provenance.ref_kind 字面量集合（裁定 2）：逐字等于 canonical DDL 的
/// `provenance.ref_kind` CHECK（`turn` 加上九张可被引用的记忆表）。
/// **`fact_link` 不在值域**——`ref_kind` 命名的是**被引用行所在的表**，而边不是可指认句柄
/// （裁定 3）；边可以作为 `provenance.mem_table='fact_link'` 出现，但不会被别的行当作依据引用。
/// ddl_shape 测试用这个常量对拍 SQL CHECK 字面量，零命中即红。
pub const REF_KINDS: [&str; 10] = [
    "turn", "card", "fact", "anchor", "relation", "state_assertion",
    "utterance", "episode", "attribute", "mention",
];

/// provenance.role 字面量集合（阶段 4 · 批次③ 因果链）：'basis' = 依据，'link' = 因果链的环节。
/// 它是 DDL 里那条跨列 CHECK `(role='link')=(ord IS NOT NULL)` 的 Rust 侧对手，但与 `REF_KINDS`
/// 有一处实差，别把它当成同一种守卫：那条 CHECK 锁的是 role 与 ord 的**耦合**（环节必带序、
/// 依据必不带序），不是 role 的值域 ⇒ 'bogus' 配 NULL ord 落得进库，这一格由 rc-store 的
/// phase3_ddl 钉成事实。值域那一半由批次③ 段 B 的写侧把守，与 `relationship.from_kind`
/// 不建 CHECK 同档（残值交给读侧判）。
/// 'basis' 同时是那一列的 DEFAULT ⇒ 存量出处行与不带 role 的插入都落 'basis'，事实族形状零变化。
/// ddl_shape 测试用这个常量对拍 `provenance` 建表原文里 role 那一格的字面量，零命中即红。
pub const PROVENANCE_ROLES: [&str; 2] = ["basis", "link"];

/// `attention.kind` 那一列的封闭集。**三个 slug 必须逐字等于 `rc-store` 的 DDL 里 `attention`
/// 表 `kind` 那一格 CHECK 的三项**：CHECK 那一侧由 `tests/ddl_shape.rs` 的
/// `attention_kind_check_is_exactly_the_three_closed_enums` 钉住，Rust 这一侧靠"真的把它插进表"
/// 来钉——`anomaly::sweep` 经 `as_str()` 写 `contradiction`，slug 写错一个字母 `INSERT` 就被
/// CHECK 拒掉、那条用例当场红，所以这里不再抄一遍对拍（两处对拍同一件事会打架）。
///
/// 每一格都必须有真实生产者：`Contradiction` 的家是 `anomaly::sweep`，`AliasAmbiguity` 与
/// `DuplicateTitle` 的家是 `anomaly::sweep_ghosts`（两族各走一支 `sweep_family`，互不解除）。
/// 三格今天都有写者，所以库里可以也有它们的行；`status` 的 `attention_open` 数的是一切族里
/// `resolved_at_ms IS NULL` 的行，新格落表就自动被数进去。
/// 日后加第四格 = 同时改 DDL 的 CHECK、上面点名的那条对拍与本 crate 的判据，
/// 三处漏一处就会写出 CHECK 里没有的词，或被 CHECK 拒掉。
///
/// 但要说清这一句的**执法时机是延迟的**，别把它当成"漏了就当场红"：只动 Rust 这一侧
/// （枚举、`ALL`、`as_str` 三处各补一格而 DDL 不动）今天全仓不红——实测补一个第四格
/// `Ghost` 后 `cargo test -p rc-memory` 是 **0 failed**（判据只取这一半；条数从补测那一轮的
/// rc-memory 基线一路在涨，别把它当门）。理由很直白：CHECK 只在
/// `INSERT` 时说话，而新格没有写者就没有 `INSERT`。真正会红的那一刻在它被真行写进表的那一轮，
/// 所以加格的人得自己保证三处一起改，不能指望现场报警。反向那一侧倒是有牙的、且当场就红：
/// 只把 DDL 的 CHECK 加一格，`attention_kind_check_is_exactly_the_three_closed_enums` FAILED
/// （实测：往 CHECK 里塞 `'ghost'` ⇒ 该文件 6 passed / 1 failed）。
/// 只 derive 到 `Eq`：全仓对 `AttentionKind` 没有任何排序或比序的读者（写侧按格点名、
/// 读侧 `as_str()` 与 `parse`），所以这里不给 `Ord`——`find_contradictions` 要的文本序来自
/// `MemTable::prefix()`，与 kind 无关，给 kind 补一个按声明序的 `Ord` 就是再造一个假序（见 `anomaly.rs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind { Contradiction, AliasAmbiguity, DuplicateTitle }

impl AttentionKind {
    pub const ALL: [AttentionKind; 3] = [
        AttentionKind::Contradiction,
        AttentionKind::AliasAmbiguity,
        AttentionKind::DuplicateTitle,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            AttentionKind::Contradiction => "contradiction",
            AttentionKind::AliasAmbiguity => "alias_ambiguity",
            AttentionKind::DuplicateTitle => "duplicate_title",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}
