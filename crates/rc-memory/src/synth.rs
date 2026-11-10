//! 合成播种：只给测试用的夹具工厂（本模块每一项都是 `#[doc(hidden)]`，
//! 生产代码引用它就是任务 11 判据 12b/12c 要红的那件事）。
//!
//! 两条纪律：① 内容全是编造的中文短句，没有任何一句来自真实会话，也没有测试金丝雀串那一类的外部样本；
//! ② 播种一律走 `writers_*` 的公开入口，不裸写 SQL——否则这些夹具就成了绕过守卫 11 的旁路，
//! 而任务 11 那一千行档测的就不再是本阶段的写入面。

use crate::keys::{AnchorRole, MemKey, Scope, ScopeKind};
use crate::writers_a::attest;
use crate::writers_x::{put_assertion, put_card, put_entity, put_event, EntityContent, FactContent};
use crate::{bump_cooldown, MemError, Provenance, Stance, COOLDOWN_MS};
use rc_store::MemoryHandle;

/// 任务 5 的"人为同分"靠这三个标题：同长、共享前缀词，差别只有末尾那一个字。
const TIE_PREFIX: &str = "同分合成词";
const TIE_TAILS: [char; 3] = ['甲', '乙', '丙'];
/// 任务 6 的五格夹具共用的查询词。
const CELL_WORD: &str = "格子测试合成词";
/// 一天（毫秒）。处境类断言的保质期用它的长度，为的是"在 now 之后还活着"。
const ONE_DAY_MS: i64 = 86_400_000;

/// 第 i 行的出处 uid：`{:02}` + 62 个 `a`，正好 64 个 hex 字符（与内容寻址的 id 同形）。
fn uid(i: usize) -> String {
    format!("{:02}{}", i, "a".repeat(62))
}

fn prov(role: AnchorRole, i: usize) -> Vec<Provenance> {
    vec![Provenance { turn_uid: uid(i), anchor_role: role }]
}

fn project_scope(key: &str) -> Scope {
    Scope { kind: ScopeKind::Project, key: key.to_string() }
}

fn tie_body(tail: char) -> String {
    format!("合成正文等长一句{tail}")
}

fn tie_card(tail: char) -> FactContent {
    FactContent {
        kind: "decision".into(),
        title: format!("{TIE_PREFIX}{tail}"),
        body: tie_body(tail),
        scope: project_scope("proj-synthetic"),
        valid_to_ms: None,
    }
}

fn cell_card(tail: &str, body: &str) -> FactContent {
    FactContent {
        kind: "situation".into(),
        title: format!("{CELL_WORD}{tail}"),
        body: body.to_string(),
        scope: project_scope("proj-synthetic"),
        valid_to_ms: None,
    }
}

/// 七行：三张同分卡 + 一条带保质期的断言 + 一条事件 + 两个实体。
/// 出处角色按行号分：前五行 human（强锚臂），第六行 agent（弱锚臂），第七行 system。
/// 返回句柄清单，用例不许去猜 id。
#[doc(hidden)]
pub fn seed(m: &MemoryHandle, now_ms: i64) -> Result<Vec<MemKey>, MemError> {
    seed_rows(m, now_ms)
}

/// 同内容、只换 P 时间戳：`now_ms` 从调用方进来，而 id 是内容的函数（见 `identity.rs`），
/// 时钟不进 preimage，所以两次播种的每一个句柄都相同 ⇒ 它的用途是证"重复播种不增行"。
#[doc(hidden)]
pub fn seed_at(m: &MemoryHandle, now_ms: i64) -> Result<Vec<MemKey>, MemError> {
    seed_rows(m, now_ms)
}

fn seed_rows(m: &MemoryHandle, now_ms: i64) -> Result<Vec<MemKey>, MemError> {
    let mut keys = Vec::new();
    for (i, tail) in TIE_TAILS.iter().enumerate() {
        keys.push(put_card(m, &tie_card(*tail), &prov(AnchorRole::Human, i), now_ms)?);
    }
    // 断言那一行带保质期，而且必须在 now 之后才到期：它是"活着且被复核过"的那一档样本。
    keys.push(put_assertion(m, &FactContent {
        kind: "claim".into(),
        title: "处境类断言一行".into(),
        body: "那条断言到明天还成立".into(),
        scope: project_scope("proj-synthetic"),
        valid_to_ms: Some(now_ms + ONE_DAY_MS),
    }, &prov(AnchorRole::Human, 3), now_ms)?);
    keys.push(put_event(m, &FactContent {
        kind: "meeting".into(),
        title: "会议记录归档".into(),
        body: "会上记录了一条决定".into(),
        scope: project_scope("proj-synthetic"),
        valid_to_ms: None,
    }, &prov(AnchorRole::Human, 4), now_ms)?);
    keys.push(put_entity(m, &EntityContent {
        kind: "person".into(),
        title: "老王".into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-synthetic".into() },
        aliases: vec!["老王".into()],
    }, &prov(AnchorRole::Agent, 5), now_ms)?);
    keys.push(put_entity(m, &EntityContent {
        // anchor.kind 的 CHECK 没有 "project" ⇒ 用域内的 "org"（夹具只测去重形状）。
        kind: "org".into(),
        title: "合成项目".into(),
        scope: project_scope("proj-synthetic"),
        aliases: vec!["proj-synthetic".into()],
    }, &prov(AnchorRole::System, 6), now_ms)?);
    Ok(keys)
}

/// 与 `seed` 的第一张同分卡同形状、只在正文尾部追加 `v{i}` ⇒ 内容不同 ⇒ 一个新句柄。
/// 任务 6 与任务 11 靠它把候选池撑到两百 / 一千行（一次调用一行、一次提交）。
#[doc(hidden)]
pub fn seed_variants(m: &MemoryHandle, now_ms: i64, i: usize) -> Result<Vec<MemKey>, MemError> {
    let mut c = tie_card(TIE_TAILS[0]);
    c.body = format!("{} v{i}", tie_body(TIE_TAILS[0]));
    Ok(vec![put_card(m, &c, &prov(AnchorRole::Human, i % 7), now_ms)?])
}

/// 五格各一条（任务 6 的 `五格各一条_每条沉默恰好一个原因` 与 `归格必须与载体列一致`）。
/// 铁律是**每条只带它那一格的证据**：一行既过期又低分的话，那一格就永远数不到，
/// 而格子优先级（否决 > 过期 > 冷却 > 弱锚 > 低分）会把另一个原因盖住——那是混格，不是省事。
#[doc(hidden)]
pub fn seed_cells(m: &MemoryHandle, now_ms: i64) -> Result<Vec<MemKey>, MemError> {
    let mut keys = Vec::new();
    // ① 被否：`attestation` 里有一条未解决的 rejected（免锚那一型，理由是人写的）。
    let rejected = put_card(m, &cell_card("被否卡", "一条被判决否掉的短正文"),
                            &prov(AnchorRole::Human, 0), now_ms)?;
    attest(m, &rejected, Stance::Rejected, None, Some("合成：与最新结论冲突"), now_ms)?;
    keys.push(rejected);
    // ② 过期：保质期落在 now 之前。载体是 `valid_to_ms` 列，不是标题里的字。
    let mut stale = cell_card("过期卡", "一条过了保质期的短正文");
    stale.valid_to_ms = Some(now_ms - 1);
    keys.push(put_card(m, &stale, &prov(AnchorRole::Human, 1), now_ms)?);
    // ③ 冷却：`cooldown_until_ms` 真的还没到期（窗口用生产常数，不在这里自定一个）。
    let cooling = put_card(m, &cell_card("冷却卡", "一条这一轮刚被提起过的短正文"),
                           &prov(AnchorRole::Human, 2), now_ms)?;
    bump_cooldown(m, std::slice::from_ref(&cooling), now_ms, COOLDOWN_MS)?;
    keys.push(cooling);
    // ④ 弱锚：出处只有 agent 轮，没有一条 human 轮（V6 的载体）。
    keys.push(put_card(m, &cell_card("弱锚卡", "一条只有机器出处的短正文"),
                       &prov(AnchorRole::Agent, 3), now_ms)?);
    // ⑤ 低分：超长行。整数匹配度是 `1024 * matched / row_len`，要整除到 0 就得
    //    `row_len > 1024 · matched`；查询词最多切成四五个 token，所以这里把正文垫到约六千 token，
    //    留三倍以上余量——垫少了这条格子就变成"分数恰好是 1"，任务 6 的判据会随分词器抖。
    let long: String = (0..1500).map(|i| format!("{CELL_WORD} 记录{i} ")).collect();
    keys.push(put_card(m, &cell_card("超长卡", &long), &prov(AnchorRole::Human, 4), now_ms)?);
    Ok(keys)
}

/// 任务 5 用它拿 `score == 0`：正文是 `extra_tokens` 个 `记录`，标题固定。
#[doc(hidden)]
pub fn seed_padded(m: &MemoryHandle, now_ms: i64, extra_tokens: usize) -> Result<MemKey, MemError> {
    let body = vec!["记录"; extra_tokens].join(" ");
    put_card(m, &FactContent {
        kind: "note".into(),
        title: "长行低分卡".into(),
        body,
        scope: project_scope("proj-synthetic"),
        valid_to_ms: None,
    }, &prov(AnchorRole::Human, 0), now_ms)
}
