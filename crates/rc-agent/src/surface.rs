use crate::error::ToolError;
use crate::recall::TZ_OFFSET_MIN;
use rc_contract::text::format_cn_ms;
use rc_search::gate::{Form, ProbeView};

/// V3：围栏 = **每行前缀**。它同时是阶段 2 抽取侧剪除注入块的标记，所以字符串只有一个家。
pub const FENCE: &str = "[recollect] ";

/// **W-0（派发档 I-5b §0.5）：MF-1 在注入面的最后一个消费口。**
///
/// 一次装载 = 判定 + 渲染要的全部数据。它存在的原因是 `rc-search/src/gate.rs` 的 `Decision.facts` 那条纪律：
/// **渲染层不许回库再 SELECT 一遍**——`Decision.facts` 已经带着渲染要的快照，判定与渲染必须
/// 是同一份数据（两份读数的后果是"块里的行与决定块的判定不是同一刻的库"，而没人会报错）。
/// 所以 `time_confidence` 这一格**在这里读一次**（`probe_via_search` 手里有句柄，`probe_block`
/// 没有），再由渲染按 `card id` 查这一份快照。
///
/// 为什么不是一个 `Vec<&str>` 按序对齐：`forms` 里有 `anchor:` 行（`ROW_TABLES` 含 Anchor，
/// 而锚那张表根本没有 `time_confidence` 这一列），按序对齐就得给锚造一个假读数。
/// `(id, slug)` 的键控形状让"这一行没有时间档"与"这一行的时间档是 unknown"是两件事。
///
/// `Debug` 是**必需的而不是装饰**：`body_canary.rs` 的 `probe_kind_for_canary` 那一条对 `probe_via_search(..)` 的
/// 失败臂用 `unwrap_err()`，而它要求 `Ok` 那一型也带 `Debug`。
#[derive(Debug)]
pub struct ProbeSnapshot {
    view: ProbeView,
    /// `fit` 之后那几行（最多 `MAX_FORM_LINES` 行 = 3 行）各自的 `card.time_confidence`
    /// **归一后**的读数（`Vec` 而不是 `HashMap`：三元素表，线性找比引一个哈希容器诚实）。
    time_confidence: Vec<(String, &'static str)>,
    /// 同一批卡各自的 `card.time_json` 事件时间（债 #9 · 修法甲）。`None` = 那一行没有
    /// 事件时间 ⇒ 渲染沿用既有各支；事件支与 `time_confidence` 支的优先级判据在 [`time_cell`]。
    event_time: Vec<(String, Option<i64>)>,
}

impl ProbeSnapshot {
    /// 装载 = 拿 `probe_view` 的结果，再为**已经开口的那几行**各补一次时间档。
    /// 那一条批量 SQL 只点 `card` 表、只查 fit 之后的 ≤3 个 id，而且**在判定之后**才发：
    /// 默认路径上被闸挡掉的行一次都不点（不是每行一条，是 0 或 1 条）。
    fn load(m: &rc_store::MemoryHandle, view: ProbeView) -> Result<ProbeSnapshot, ToolError> {
        let ids: Vec<String> = view.forms.iter()
            .filter(|d| d.key.table == rc_memory::keys::MemTable::Card)
            .map(|d| d.key.id.clone())
            .collect();
        let modes = rc_memory::reader::card_modes(m, &ids).map_err(ToolError::from)?;
        // 归一判据的唯一家是 `recall::time_confidence_slug`（派发档 §0.5 那句"不许再造第二套
        // slug 判据"）：这里只做"读列 → 归一 → 按键收进快照"，值域、NULL、域外串怎么算全在那一格。
        let time_confidence: Vec<(String, &'static str)> = modes.iter()
            .map(|cm| (cm.id.clone(), crate::recall::time_confidence_slug(
                cm.time_confidence.as_deref())))
            .collect();
        // 债 #9（修法甲）：同一批 id 再解一次 `card.time_json`（同一笔批量侧查的账，1 条 SQL）。
        // 没有事件时间的行不在返回里 ⇒ `get` 不到就是 `None`，渲染走既有支。
        let events = rc_memory::reader::card_event_times(m, &ids).map_err(ToolError::from)?;
        let event_time: Vec<(String, Option<i64>)> = modes.iter()
            .map(|cm| (cm.id.clone(),
                       events.iter().find(|e| e.id == cm.id).map(|e| e.start_ms)))
            .collect();
        Ok(ProbeSnapshot { view, time_confidence, event_time })
    }
}

/// `ProbeSnapshot`  derefs 到 `ProbeView`：调用方（`hook` 的 `view.forms`、
/// `stamp_cooldown(..., &view, ...)`）因此一个字都不必改——这一面新增的是**一格读数**，
/// 不是一条新的调用形状。
impl std::ops::Deref for ProbeSnapshot {
    type Target = ProbeView;
    fn deref(&self) -> &ProbeView { &self.view }
}

/// 渲染层能看到的两份数据源：**带时间档的快照**（生产路径）与**裸 `ProbeView`**
/// （手工拼 `ProbeView` 的既有渲染用例）。
///
/// 为什么要这一层 trait 而不是直接给 `probe_block` 加第二个形参：`probe_block(&view)` 这个
/// 调用形状有五个既有读者（`hook/src/main.rs` 的 `run_probe` 与 `rc-agent/tests/{block_cap,
/// sentence_boundary,attestation_summary,body_canary}.rs` 的若干处），它们都不在本轮的文件集里
/// ⇒ 签名必须保持"`&view` 一个参数"。泛型 + 默认 `None` 是**唯一**能同时满足
/// "调用点一字不改"和"生产路径带得上时间档"的形状；`ProbeView` 那一支的读数恒为 `None`，
/// 而 `None` 走的正是 BASE 的那两个字面量——手工拼表的用例因此仍然比的是**旧字节**。
pub trait ProbeSource {
    /// 判定结果（表单序 = `fit` 之后的序，渲染只许逐行画它）。
    fn view(&self) -> &ProbeView;
    /// 这一行（`card` 的 id）的时间档；**未知与缺席同形**（`None` ⇒ 沿用既有那一支）。
    fn time_confidence(&self, _card_id: &str) -> Option<&'static str> {
        None
    }
    /// 这一行（`card` 的 id）的 `time_json` 事件时间（债 #9 · 修法甲）。
    /// `None` = 没有这一格或没有事件时间 ⇒ 沿用既有各支（既有字节一字不动）。
    /// 裸 [`ProbeView`] 的读数恒 `None`（与 `time_confidence` 同一条纪律：手工拼表的
    /// 既有渲染用例仍然比旧字节）。
    fn event_time_ms(&self, _card_id: &str) -> Option<i64> {
        None
    }
}

impl ProbeSource for ProbeView {
    fn view(&self) -> &ProbeView { self }
}

impl ProbeSource for ProbeSnapshot {
    fn view(&self) -> &ProbeView { &self.view }
    fn time_confidence(&self, card_id: &str) -> Option<&'static str> {
        self.time_confidence.iter().find(|(id, _)| id == card_id).map(|(_, slug)| *slug)
    }
    fn event_time_ms(&self, card_id: &str) -> Option<i64> {
        self.event_time.iter().find(|(id, _)| id == card_id).and_then(|(_, ms)| *ms)
    }
}

/// 沉默 = 空串。这里产出的 `InjectionBlock` 内部就是那个空串——
/// V3 的等价关系（沉默 ⇔ `stdout.is_empty()`）在这一层成立，`hook` 只是把它原样写出去。
///
/// **`speech` 那一支在注入面的形状（W-0）**：episode 镜像卡在成员全无可知事件时间时，
/// `as_of_ms` 落的是**发话时刻**（`writers_x::episode_time_arm` 第二支，库里那一格是
/// `'speech'`）。BASE 上这一面只读 `as_of_ms`，于是 hook 的每一行都在说"这件事的时间 = 那个人
/// 说话的时间"。现在那一行的时间格换成 `as_of=<ms>（发话时刻，事件时间未知）`——
/// 说的是"这一格是发话钟"，而不是"它是事件钟"。
///
/// 三支的字节账（派发档 §0.5 硬线"anchored 与 unknown/NULL 两支持现字节不许变"）：
/// - `anchored` ⇒ `as_of=<ms>`，**与 BASE 逐字节相同**（那一格本来就是事件时间）；
/// - `unknown` / NULL / 值域外的串 ⇒ `as_of=<ms>`，**逐字节相同**（`time_confidence_slug`
///   已经把后两者收进 `unknown` 那一支，而既有那一支本来就是按 `as_of_ms` 在场与否分支的）；
/// - `speech` ⇒ 多出尾部那一句补语。**行首前缀、行数、`| as_of=` 这个机读键都不动**，
///   所以 `Decision::lines()` 的行账、`fit` 的上限、抽取侧剪注入块的 FENCE 判据都不受影响。
///
/// 用例在 `tests/surface_time_labels.rs`，改前后两面的原文对照在报告里。
pub fn probe_block<S: ProbeSource + ?Sized>(src: &S) -> crate::InjectionBlock {
    let mut lines: Vec<String> = Vec::new();
    for d in &src.view().forms {
        match d.form {
            Form::Silent => {}
            // signpost = 指路：句柄 + 标题原样 + 时间。**没有正文**。
            Form::Signpost => lines.push(format!(
                "{FENCE}{} | {} | {}",
                d.key.key_string(),
                d.facts.title,
                time_cell(src, d),
            )),
            // full = 代言：≤1 句正文 + 句柄 + 时间。句子在**这里**截，不在渲染下游截
            // （截断动作离"每档占几行"的定义越近，两处漂移的机会越小）。
            Form::Full => {
                let first = first_sentence(&d.facts.body);
                lines.push(format!("{FENCE}{first}"));
                lines.push(format!("{FENCE}{} | {}", d.key.key_string(), time_cell(src, d)));
            }
        }
    }
    crate::InjectionBlock::from_lines(lines)
}

/// 一行时间格的写法。**判据的家不在这里**：`src.time_confidence(...)` 给的已经是
/// `recall::time_confidence_slug` 归一后的读数（NULL、域外串都在那一格被收成 `unknown`），
/// `src.event_time_ms(...)` 给的是 `card.time_json` 解出的事件时间（债 #9 · 修法甲，判据在
/// `rc_memory::reader::card_event_times`），这里只按归一结果分三支：
///
/// - 事件时间在场 ⇒ `as_of=<列值>（事件发生于 <中文戳>）`——`as_of=` 这个机读键与值保持
///   列语义不动（与 anchored/unknown 两支同一分工），补语把事件钟说出来；
/// - `speech` ⇒ `as_of=<列值>（发话时刻，事件时间未知）`（W-0 的既有形状）；
/// - 其余 ⇒ `as_of=<列值>`（BASE 原文）。
///
/// 事件支优先于 `speech` 支：契约 1 是"`time_json` 里真有事件时间的卡不得再说'事件时间未知'"。
/// 为什么这一支不说"发生在 X"而说"发话于"：计划 `:59,124` 那条纪律——**时间只用事件时间**，
/// 缺了就说未知，不许拿 `created_at`/发话钟顶成事件时间。
fn time_cell<S: ProbeSource + ?Sized>(src: &S, d: &rc_search::gate::Decision) -> String {
    let is_card = d.key.table == rc_memory::keys::MemTable::Card;
    let event = if is_card { src.event_time_ms(&d.key.id) } else { None };
    if let Some(ms) = event {
        return format!("as_of={}（事件发生于 {}）", d.facts.as_of_ms,
                       format_cn_ms(ms, TZ_OFFSET_MIN));
    }
    let speech = is_card && src.time_confidence(&d.key.id) == Some("speech");
    if speech {
        // `as_of=` 这个键名保留：它说的是"这一行自带的那口钟"，补语说的是那口钟是什么性质。
        format!("as_of={}（发话时刻，事件时间未知）", d.facts.as_of_ms)
    } else {
        format!("as_of={}", d.facts.as_of_ms)
    }
}

/// 取第一句。中文句号 / 问号 / 叹号与它们的半角形式都算边界；找不到边界时**按字符数截**
/// （不是按字节，否则中文会在 UTF-8 中间断掉而 panic ——那是这一层唯一真实的崩溃形状）。
///
/// **ASCII `.` 是唯一的例外**：它同时是句末标点和型号名/小数里的连接符。本机库里这一格的
/// 实测分布（控制器与实施者各自独立用只读 SQL 复算）：**6/35 张卡**的标题或正文含 ASCII 点，
/// 而这 6 张里全部 10 个点都落在**两侧都是数字**的位置（`Qwen3.8-27B`、`V4.1`、`V-4.7-35B`）
/// ⇒ 判据①今天已覆盖全部真数据，判据②不为现存数据服务、由 `tests/sentence_boundary.rs` 的
/// M3 变异证明它不是冗余。附录里那个"21/35"是把锚的 `canonical`（23/60 含点）混进了卡的口径，
/// 而锚不经 `first_sentence`、不受这一判据影响。判据一句话——
/// **点号两侧都是数字、或点号落在一个不含空白的 token 内部时，不许断句**；
/// 其余形状（点左侧是空白、点右侧是空白或串尾）照旧是句末。
/// 这条判据的代价写在 `tests/sentence_boundary.rs` 的最后一臂：ASCII 点直接贴着中文时
/// （`…V2.6-Flash.它比前一档稳。`）不再断开，那一串靠中文标点或 120 字符上限收口。
/// 放宽只朝一个方向：**边界仍然有三种**（中文标点、`?`/`!`、"点 + 空白/串尾"），
/// 所以这里产不出"整段正文"，`Form::Full` 的两行形状与 `Decision::lines()` 的行账不受影响。
pub const MAX_SENTENCE_CHARS: usize = 120;

fn first_sentence(body: &str) -> String {
    // 偏离 brief 的一处形态（真值表逐位相同）：brief 写 `while let Some(c) = chars.next()`，
    // `clippy::while_let_on_iterator` 在本仓 `-D warnings` 的 gate 下是硬错误（实测）。
    // 判据现在要**前后各看一个字符**，所以先把 body 落成 `Vec<char>`：按字节前后看会在
    // UTF-8 中间踩到非字符边界而 panic（上面注释点名的同一个崩溃形状），按 char 看不会。
    // `for ….enumerate()` 与 `while let` 的差别只在迭代器耗尽后不需要再判 `None`——而这里
    // 每次提前退出都走 `break`，两个形状产出的串逐字符相同。与任务 6 的 nonminimal_bool 改写同判据。
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().copied().enumerate() {
        out.push(c);
        let n = i + 1;
        if ends_sentence(&chars, i) && n >= 2 {
            break;
        }
        if n >= MAX_SENTENCE_CHARS {
            break;
        }
    }
    out
}

/// 位置 `i` 上的字符是不是句末。中文三个标点与半角 `?`/`!` 无条件算；半角 `.` 要再过一道
/// `is_token_interior_dot`（判据见 `first_sentence` 的 doc）。
fn ends_sentence(chars: &[char], i: usize) -> bool {
    match chars[i] {
        '。' | '？' | '！' | '?' | '!' => true,
        '.' => !is_token_interior_dot(chars, i),
        _ => false,
    }
}

/// 点号**不许**当句末的两种形状：
/// ① 两侧都是 ASCII 数字——版本号/小数（`Qwen3.8-27B` 中间那个点）；
/// ② 两侧都不是空白，即点落在一个不含空白的 token 内部（`DeepSeek-V4.1-Flash` 的两个点、
/// `MiMo-V2.6-Flash.它比…` 里贴着中文的那个点）。
/// ①已被②包含，逐字留着是因为派发档的判据就写了两半。两条都有独立读者（实测）：
/// 只按①判 ⇒ `MiMo-V2.6-Flash.它…` 那一臂红；把②的"两侧都非空白"写成"任一侧非空白"
/// ⇒ `见 p. 12 页` 那一臂红。用例在 `tests/sentence_boundary.rs`。
/// 串首（`i == 0`）与串尾（`get(i + 1)` 为空）都不成立 ⇒ 点是句末：`见 p.` 这种结尾照旧断。
fn is_token_interior_dot(chars: &[char], i: usize) -> bool {
    let prev = if i == 0 { None } else { Some(chars[i - 1]) };
    let next = chars.get(i + 1).copied();
    let digit_on_both =
        prev.is_some_and(|c| c.is_ascii_digit()) && next.is_some_and(|c| c.is_ascii_digit());
    let inside_token =
        prev.is_some_and(|c| !c.is_whitespace()) && next.is_some_and(|c| !c.is_whitespace());
    digit_on_both || inside_token
}

/// `probe_via_search` = "检索 + 判定 + 分型映射"的一次完成。
/// 它存在于 `surface` 而不是 `hook`/`mcp`，因为**两个二进制必须走同一条判定路径**
/// （守卫 3：一份实现两个数据源；两臂各判一次 = 同一句查询在两个面上给不同答案，而那没人会报错）。
///
/// 返回的是 [`ProbeSnapshot`] 而不是裸 `ProbeView`：`time_confidence` 这一格**只有在这里**
/// 拿得到库句柄，而渲染层不许回库补查（`rc-search/src/gate.rs` 的 `Decision.facts` 那条纪律）。它是 `ProbeView` 的
/// `Deref` 目标，所以既有调用方（`hook` 的 `view.forms` 与 `stamp_cooldown(.., &view, ..)`）
/// 一个字都不必改。
pub fn probe_via_search(
    m: &rc_store::MemoryHandle,
    viewer: Option<&crate::recall::Viewer>,
    query: &str,
    now_ms: i64,
) -> Result<ProbeSnapshot, ToolError> {
    // W37-A1：viewer 原样透传（`None` = 不过读闸的旧调用）。hook 侧给的是
    // 无 principal 的 agent viewer——读闸 fail-closed，五格与候选过滤照常生效。
    let view = rc_search::gate::probe_view(m, viewer, query, now_ms).map_err(ToolError::from)?;
    ProbeSnapshot::load(m, view)
}

/// 状态块：`hook brief` 的 stdout 与 `mcp` 每会话首次返回里的 `bootstrap` 字段
/// **共用这一个构造器**（§6.5 P1 那句"同一构造器"的字面兑现；它的对撞用例在任务 10 的
/// `framing.rs`——那边比字节，这边比形状）。
///
/// 传进来的必须已经是 `tools::status(...).text()` 那串紧凑 JSON——它没有物理换行，
/// 但 `one_line` 仍然要过一遍，因为它就在 `from_lines` 里：**"一行"这个承诺的执行点在载体
/// `InjectionBlock`，不在构造它的这段代码**。今天靠上游保证是侥幸，明天换个上游就不是。
pub fn status_block(json: &str) -> crate::InjectionBlock {
    crate::InjectionBlock::from_lines(vec![format!("{FENCE}{json}")])
}

/// `hook brief` 的完整载体（W34 续接包）：围栏 JSON 行在前，可选的固定段（rc-core 渲染面
/// 出的「上一棒交接」）在后。`section` 为空串 ⇒ 块与 [`status_block`] 逐字节相同——
/// 段缺席，不是空标题（沉默匹配视图内容）。
///
/// 段的每一行都落围栏前缀：V3 的"围栏 = 每行前缀"不因段而破。抽取侧对 `role='system'`
/// 轮本就整段不选批（`batch::SYSTEM_ROLE` 的候选 SQL），围栏只是让邻轮上下文里的
/// 注入行照旧可被 `prepare` 剪掉——注入内容不进蒸馏，与 probe 同一条。
///
/// 不走 [`status_block`] 那条 `from_lines`：那是 probe 面的 3 行额度载体；brief 的行预算
/// 住 rc-core 的分区上限，载体只兑现"控制字符扁平化 + 沉默 ⇔ 空串"两条承诺
/// （`from_brief_lines`）。
pub fn brief_block(json: &str, section: &str) -> crate::InjectionBlock {
    let mut lines = vec![format!("{FENCE}{json}")];
    for line in section.lines() {
        lines.push(format!("{FENCE}{line}"));
    }
    crate::InjectionBlock::from_brief_lines(lines)
}
