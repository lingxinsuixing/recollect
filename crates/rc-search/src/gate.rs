//! §8 的闸门：把"能不能开口"与"以什么强度开口"分成两件事。
//!
//! 混成一团的后果是可测的：所有沉默都归到"低分"，于是导出件里看不出任何异常，
//! 而真实原因可能是那条被否了。所以这里有两条各自独立的形状：
//! `Switches::opens_mouth`（五个沉默开关的**合取**，`attested` 不在内，V10）
//! 与 `decide` 的五段格序（否决 > 过期 > 冷却 > 弱锚 > 低分，V8）。
//! 两者互为镜像这条判据的载体在 `tests/gate_switches.rs`（逐行核对 + 逐臂点名）。
//! **唯一例外是 G11**（2026-11-07 召回审计 + W35）：弱锚格里 `verdict=Passed` 且
//! kind=event 的卡降为 `Signpost` 出声一格——开关合取与格子归属都不变，降级只动出声形态。
//!
//! 这一层不碰 SQL、不碰会话库：它只读 `Scored` 已经带在内存里的那份 `RowFacts`（守卫 3——
//! 闸门有第二个数据源的那一刻起，判定与渲染就可以给出两个答案）。

use crate::rank::Scored;
use rc_memory::keys::MemKey;
use rc_memory::{AnchorRole, Stance, Verdict};
use serde::Serialize;

/// C2 输出块的硬上限。V3：它数的是 **stdout 的物理行**，不是条数——
/// 所以"沉默"的实现必须是 `stdout.is_empty()`，而不是某个标志位。
/// 上限在这里定义，在 `rc-agent::surface::probe_block` 执行；两处共享同一个常量（不是两份数字）。
pub const MAX_FORM_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    Silent,
    Signpost,
    Full,
}

/// 沉默的五格。**一格一个原因，混格即红**（§10 阈值侧③）。
/// `Ord` 按声明序，`counts()` 用它把五格定序输出，于是导出件的格子顺序不依赖插入顺序。
/// 声明序就是判定优先级（V8）：`decide` 按同序逐段检查，两处不许各排一遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateCell {
    /// 中心判 Failed（`verdict`）或 agent/中心表态 rejected（`attestation`）。人的否决压过一切机械读数。
    Rejected,
    /// retirement 未解决 ∨ `superseded_by` 非空 ∨ `valid_to_ms` 已过期。
    Stale,
    /// D4：同一会话内已注入且未被引用 ⇒ `cooldown_until_ms` 未到。
    Cooldown,
    /// 出处只有 agent/system 轮，没有人工轮（V6）。
    WeakAnchor,
    /// 整数匹配度低于 `FLOOR`。
    LowScore,
}

impl GateCell {
    pub const ALL: [GateCell; 5] = [
        GateCell::Rejected,
        GateCell::Stale,
        GateCell::Cooldown,
        GateCell::WeakAnchor,
        GateCell::LowScore,
    ];
    /// 与 `RejectCode::slug()` 同一个理由：运维面读数和导出件里的分型必须只有一个渲染处。
    pub fn slug(self) -> String {
        serde_json::to_value(self)
            .expect("closed enum with derived Serialize")
            .as_str()
            .expect("GateCell serializes to a string")
            .to_owned()
    }
}

/// §8 的闸门开关 + D4 的冷却（V10：六个字段，`attested` 只驱动阶梯）。
/// 每个字段的存在理由都是**一列可点名的载体**——注释里点名它，测试里断它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    /// 载体：`attestation` 表最新一条正面表态（由 `reader::facts` 汇成 `RowFacts.attestation`）。
    pub attested: bool,
    /// 载体：`retirement.resolved_at_ms` / `superseded_by` / `valid_to_ms`（三条臂）
    /// **加上 `card_reuse_event` 的 `stale` 反馈（刀 B 的第四臂）**。
    /// 第四臂是"读取侧的 retire 候选"：消费方报过 `stale`（这张卡已失效）时，读取面
    /// **不改 card 行、不写 retirement 账**（那是 user/operator 的裁定，见 `writers_c::retire`），
    /// 只把这一行判进 `GateCell::Stale`——与"退场账已成立"在检索面同形（静默），
    /// 但退场账本身仍然只有人来结。形状理由见设计 §波次二第 3 条的"stale 卡直接进 stale 判定"。
    pub not_stale: bool,
    /// 载体：`provenance.anchor_role` 抄进行的那一列（V6）。
    pub strong_anchor: bool,
    /// 载体：否决的**两列**——`verdict` 列（只有 `Failed` 是否决，`Low`/`Unreviewed` 放行）与
    /// `attestation` 里那条未解决的 rejected（`RowFacts.rejected`）：只读一列会让 `opens_mouth()`
    /// 与 `decide` 对被表态否决的行各说一套，本档那条镜像判据当场成空话。
    /// 字段名沿用 brief 的六字段声明面（R31 已裁：不改名、不新增第七个开关），装两列是刻意的。
    pub verdict_ok: bool,
    /// 载体：`RowFacts` 上没有它——**分数只在内存里**（守卫 7），所以这条断的是"库里查不到分数列"。
    pub above_floor: bool,
    /// 载体：`cooldown_until_ms` 列（P 写，V4/V7）。
    pub not_cooling_down: bool,
}

impl Switches {
    pub fn of(s: &Scored, now_ms: i64) -> Switches {
        let f = &s.facts;
        // 两条时间比较写成 `is_none_or` 而不是 brief 原文的 `!…is_some_and(…)`：
        // `clippy::nonminimal_bool` 在 `-D warnings` 下拦后者，而两者真值表逐位相同
        // （`!(Some(v) && v < now)` ⇔ `v >= now 或没有上界`）。空值那一支的方向就是本档那条
        // `valid_to_为空等于永不过期` 用例钉住的东西，改写没有动它。
        Switches {
            attested: matches!(f.attestation, Some(Stance::Confirmed) | Some(Stance::Corroborated)),
            not_stale: !f.retired
                && f.superseded_by.is_none()
                && f.valid_to_ms.is_none_or(|v| v >= now_ms)
                // 刀 B 的第四臂：`card_reuse_event` 里有 `stale` 反馈 ⇒ 读取侧判过期。
                // 信号由 `rank` 从复用表读一次装在 `Scored` 上（闸门不查库，守卫 3）。
                && !s.reuse.stale,
            strong_anchor: f.anchor_role == AnchorRole::Human,
            verdict_ok: f.verdict != Verdict::Failed && !f.rejected,
            above_floor: s.above_floor(),
            not_cooling_down: f.cooldown_until_ms.is_none_or(|t| t <= now_ms),
        }
    }

    /// 开口判据：五个开关的合取（`attested` 故意不在内，理由见本节 V10）。
    /// 这条函数与 `decide` 里的格序必须互为镜像——`tests/gate_switches.rs` 最后一条用例逐行核对它们的一致性。
    /// **G11 之后的例外**：`decide` 可以在 WeakAnchor 格上让一行出声（Passed ∧ kind=event 降级），
    /// 此时 `opens_mouth()` 仍答否——它只答开关合取，不答出声形态。
    pub fn opens_mouth(&self) -> bool {
        self.not_stale
            && self.strong_anchor
            && self.verdict_ok
            && self.above_floor
            && self.not_cooling_down
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub key: MemKey,
    pub form: Form,
    /// **格位不变量（G11 改写）**：cell 是它**为什么被闸/被降级**的格位。
    /// `cell.is_none() == opens_mouth()`（开关合取的镜像，载体仍是 `tests/gate_switches.rs`）
    /// 这一条不动；`cell = Some(..)` 默认 `form == Silent`，**唯一例外**是
    /// `cell = Some(WeakAnchor) ∧ form == Signpost` = G11 降级出声（Passed 的 event 卡，
    /// 一行出声、格子照旧记 WeakAnchor——cells 是"判定落点"诊断，form 才是出声形态）。
    /// 破掉镜像的形态就是"沉默了但没人知道为什么"。
    pub cell: Option<GateCell>,
    /// 只给 `doctor`/`export` 的读数用；**不许进 agent 面输出**（守卫 12 之外的另一条：分数不出内存）。
    pub score: i64,
    /// 渲染要的快照（任务 7 的 `probe_block` 读它）。带着它 = 判定与渲染同一份数据；
    /// 让渲染层回库再 SELECT 一遍 = 第二个数据源，那正是守卫 3 禁的东西。
    pub facts: rc_memory::reader::RowFacts,
}

impl Decision {
    /// 它在 C2 块里占几个物理行（V3）。这个函数让"上限"可以在**正文还不存在时**就被证明——
    /// 于是 `fit` 只依赖格子形状，不依赖任何一条真实文本的长度。
    pub fn lines(&self) -> usize {
        match self.form {
            Form::Silent => 0,
            Form::Signpost => 1,
            Form::Full => 2,
        }
    }
}

/// 判定顺序 = 格的优先级：**否决 > 过期 > 冷却 > 弱锚 > 低分**。
/// 这个顺序不是审美。一条既过期又低分的行必须报"过期"（§10 生命周期①：三格混淆即红），
/// 因为"冷却"与"低分"都意味着"它本来还能被提起"，而人的判决和事实失效不是。
pub fn decide(s: &Scored, now_ms: i64) -> Decision {
    let sw = Switches::of(s, now_ms);
    let cell = if !sw.verdict_ok {
        Some(GateCell::Rejected)
    } else if !sw.not_stale {
        Some(GateCell::Stale)
    } else if !sw.not_cooling_down {
        Some(GateCell::Cooldown)
    } else if !sw.strong_anchor {
        Some(GateCell::WeakAnchor)
    } else if !sw.above_floor {
        Some(GateCell::LowScore)
    } else {
        None
    };
    let form = match cell {
        // G11（2026-11-07 召回审计 + W35）：弱锚格吞掉了 hook 注入面约 2/3 探针——真正
        // 复核通过（Passed）的 event 卡也被静默。拍板只动这一格：Passed 且 kind=event 的卡
        // 从 Silent 降为 Signpost（一行出声），其余 WeakAnchor 行照旧沉默。
        // **判定顺序（否决 > 过期 > 冷却 > 弱锚 > 低分）与格子归属一字不动**：cell 仍是
        // WeakAnchor，`cells_of` 的统计因此照旧含它——cells 是"判定落点"诊断，form 才是
        // 出声形态。fact/state 的弱锚行不进白名单，等读数。
        Some(GateCell::WeakAnchor)
            if s.facts.verdict == Verdict::Passed && s.facts.kind == "event" =>
        {
            Form::Signpost
        }
        Some(_) => Form::Silent,
        // V5：full 只由可核查证据打开——corroborated（中心记为已佐证）或 verdict=Passed（复核通过）。
        // 单次 confirmed 只到 signpost。
        None if matches!(s.facts.attestation, Some(Stance::Corroborated))
            || s.facts.verdict == Verdict::Passed =>
        {
            Form::Full
        }
        None => Form::Signpost,
    };
    Decision { key: s.key.clone(), form, cell, score: s.score, facts: s.facts.clone() }
}

/// 装得下的前缀。**溢出即停（`break`），不跳过再塞**：
/// 跳过高位置的 full 去塞低位置的 signpost，等于用"行数"换"相关性"，
/// 而顺序是相关性唯一的表达方式。这条理由要写在代码里，因为下一个人一定会想改成 `continue`。
pub fn fit(decisions: &[Decision]) -> Vec<Decision> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for d in decisions {
        if d.form == Form::Silent {
            continue;
        }
        if used + d.lines() > MAX_FORM_LINES {
            break;
        }
        used += d.lines();
        out.push(d.clone());
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct Cells(std::collections::BTreeMap<GateCell, usize>);

impl Cells {
    pub fn record(&mut self, c: GateCell) {
        *self.0.entry(c).or_default() += 1;
    }
    /// **唯一的沉默计数出口**：`export`（C3）与 `status`（读数）都从这里取。
    /// 两处各算一次就是两个"沉默计数"，那属于 §11 D9 点名过的"两个真相"那一族。
    /// 返回值按 `GateCell::ALL` 定序，缺格补 0——补 0 是刻意的：
    /// 导出件里"某一格不存在"和"某一格为 0"必须是同一件事，否则读的人要猜。
    pub fn counts(&self) -> [(GateCell, usize); 5] {
        GateCell::ALL.map(|c| (c, self.0.get(&c).copied().unwrap_or(0)))
    }
    pub fn total(&self) -> usize {
        self.0.values().sum()
    }
}

/// G11：降级出声的行（`cell = Some(WeakAnchor)` ∧ `form = Signpost`）照旧在这里记账——
/// cells 数的是"判定落点"，不是"沉默条数"（沉默与否由 form 承担）。
pub fn cells_of(decisions: &[Decision]) -> Cells {
    let mut c = Cells::default();
    for d in decisions {
        if let Some(cell) = d.cell {
            c.record(cell);
        }
    }
    c
}

#[derive(Debug, Clone)]
pub struct ProbeView {
    /// 已按 `fit` 截断；渲染层（任务 8）只许逐行画它，不许再增删。
    pub forms: Vec<Decision>,
    pub silent: Cells,
    /// 候选池大小。`status` 用它做"检索到过但都没开口"的读数（G7：沉默要能自证）。
    /// S2 起它是**剔除锚之后**的池子：锚不是注入对象，把它算进"候选"就等于承认它占额度。
    /// 于是这个数与 `silent` 的五格计数都不再包含锚——被剔除的行既不是候选，也不是"沉默的候选"。
    pub candidates: usize,
}

/// `hook probe` 的全部逻辑就这一次调用：**判定在库里，时钟由调用方给**（可复现性），
/// 冷却写入不在这里（那是 `hook` 的事，因为它是唯一有 P 写权限的读路径，V7）。
///
/// W37-A1（hook probe 补 viewer）：`viewer` 是 probe 这一次读的读者身份。
/// `None` = 不过读闸（不带 `--agent-id` 的旧调用，行为与改动前逐位相同）；
/// `Some(v)` ⇒ 读闸在 `rank` 里先走一遍——读者看不见的行根本不进候选池，
/// `candidates` 与五格计数都只数这位读者看得见的行。hook 侧构造的是
/// **无 principal 的 agent viewer**：读闸对它保持 fail-closed（4b 永不命中）。
///
/// S2：候选里**剔除锚**。锚是导航，不是要往提示面注入的记忆；它们进候选的后果是
/// 可测的——实体行只有标题 ⇒ `row_len` 比带正文的卡小 ⇒ 同分下排得更前，于是
/// `MAX_FORM_LINES` 那 3 行额度被锚占满、真卡被 `fit` 溢出即停地挤掉。
/// 剔除发生在 `decide` **之前**，而 `rank` 原样不动：这与"低分项仍可 recall"（G6 判据③）
/// 同一条纪律——过滤在候选（"谁算注入对象"），不在检索（"库里有什么"）。
/// 显式寻址不走这里（`recall(handles=[anchor:…])` / `view=json`），那条能力不丢。
pub fn probe_view(
    m: &rc_store::MemoryHandle,
    viewer: Option<&rc_memory::gate::Viewer>,
    query: &str,
    now_ms: i64,
) -> Result<ProbeView, crate::SearchError> {
    let ranked: Vec<_> = crate::rank::rank(m, viewer, now_ms, query)?
        .into_iter()
        .filter(|s| s.facts.key.table != rc_memory::MemTable::Anchor)
        .collect();
    let decisions: Vec<Decision> = ranked.iter().map(|s| decide(s, now_ms)).collect();
    let silent = cells_of(&decisions);
    let candidates = decisions.len();
    Ok(ProbeView { forms: fit(&decisions), silent, candidates })
}
