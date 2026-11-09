use crate::keys::{MemTable, Scope};
use rc_contract::normalize_body;
use sha2::{Digest, Sha256};

/// 句柄 = sha256(表前缀 · kind · 归一标题 · 归一正文 · 归属类型 · 归一归属键)，hex。
///
/// 三条都是 §6 的承诺面语义逼出来的，不是风格：
/// ① **不含 created_at / as_of / 出处** —— 派生重建必须算出同一个 id，否则跨会话写下的
///    表态会锚到别的东西上，而悬空表态比没有表态更坏（它让我以为自己确认过某件事）。
/// ② **表前缀进 preimage** —— 一条断言与一张卡内容是同两行字时不许互相顶掉。
/// ③ **正文与标题先 normalize_body** —— 与阶段 0 的 turn 身份共用同一把归一尺子，
///    否则 CRLF 与 NFC 的差异会造出"同一条记忆的两个 id"。
/// 每段之间插一个 0u8 分隔：`["ab","c"]` 与 `["a","bc"]` 必须算出不同的 id。
pub fn compute_mem_id(table: MemTable, kind: &str, title: &str, body: &str, scope: &Scope) -> String {
    let mut h = Sha256::new();
    for part in [table.prefix(), kind, &normalize_body(title), &normalize_body(body),
                 scope.kind.as_str(), &normalize_body(&scope.key)] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 边 id = sha256("relationship" · from_id · kind · to_id)，hex。
/// **理由不进 id** ⇒ 同 (from, kind, to) 重写幂等（理由变化是新边的内容而不是新边的身份）。
/// 段间 0u8 分隔防拼接碰撞：`("a","bc","d")` 与 `("ab","c","d")` 无分隔时都拼成 `abcd`，
/// 必须靠分隔算出不同的 id（Step 14 的变异就是撤掉 `h.update([0u8]);`，用例要能为它红）。
pub fn compute_edge_id(from_id: &str, kind: &str, to_id: &str) -> String {
    let mut h = Sha256::new();
    for part in ["relationship", from_id, kind, to_id] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 因果链头行的 id = sha256("causal" · 归属类型 · 归一归属键 · 归一机制句 · **有序环节 id 逐个**)，hex。
///
/// **为什么不走 `compute_mem_id`**（批次③ C-1，承重）：那一条的 preimage 只有
/// `(table, kind, title, body, scope)`，**环节列表根本不在里面**；而链头行的正文按 C-2 就是机制句本身
/// ⇒ "同一句机制、两组环节"的两条链会算出**同一个 id**，第二条撞上头行那句 `INSERT OR IGNORE`
/// 就被静默吞掉——环节全部丢失、不报错、不红（P-44 那一族"读数没有生产者"的写入侧镜像）。
/// 把环节逐个进 preimage 才是那条链的身份：换了环节就是另一条链。
/// 形状照同文件的 `compute_edge_id`（"理由不进 id"那一档处理）：
/// ① 表位写死 `"causal"`（链头只落 `event(kind='causal')`，它不是第四张表）；
/// ② **段间 0u8 分隔**防拼接碰撞——`["a","bc"]` 与 `["ab","c"]` 无分隔时都拼成 `ab c`，
///    而这两组环节是两条不同的链；机制句与首个环节 id 之间同样需要这道分隔；
/// ③ 机制句与归属键先过 `normalize_body`，与 `compute_mem_id` 同一把归一尺子（CRLF/NFC 差异
///    不许造出"同一条链的两个 id"）；环节 id 是库内已有的十六进制句柄，原样进 preimage。
pub fn compute_chain_id(scope: &Scope, mechanism: &str, ordered_ids: &[String]) -> String {
    let mut h = Sha256::new();
    let normalized_key = normalize_body(&scope.key);
    let normalized_mechanism = normalize_body(mechanism);
    let mut parts: Vec<&str> = vec!["causal", scope.kind.as_str(), &normalized_key,
                                    &normalized_mechanism];
    for id in ordered_ids { parts.push(id.as_str()); }
    for part in parts {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 软边 id：有序两端进 preimage，`basis` / `evidence_json` 不进（同边重写幂等）。
/// 与 `compute_edge_id` 同族，但**前缀不同**：两张表的 id 空间不共用，防止一条边冒充一条软边。
pub fn compute_event_link_id(a_id: &str, b_id: &str) -> String {
    let mut h = Sha256::new();
    for part in ["event_link", a_id, b_id] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 轮级 mention 的 id = sha256("mention" · 轮 · 锚 · 归一表面名 · span_start · span_end)。
///
/// 为什么不像消费行那样复用 `compute_mem_id`：mention 不是内容行，它的身份是
/// "某一轮用某个表面名称呼了某个锚"。同一轮同一锚同一表面名重复出现时合成一行
/// （A1 不产出字符 span，两格都传 `None`）——重放因此幂等；换了表面名就是另一条提及。
pub fn compute_mention_id(turn_uid: &str, anchor_id: &str, surface: &str,
                          span_start: Option<i64>, span_end: Option<i64>) -> String {
    let start = span_start.map(|v| v.to_string()).unwrap_or_default();
    let end = span_end.map(|v| v.to_string()).unwrap_or_default();
    let normalized = normalize_body(surface);
    let mut h = Sha256::new();
    for part in ["mention", turn_uid, anchor_id, &normalized, &start, &end] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// fact 行的 id = sha256("fact" · 主语 · 谓词 · 宾语? · 值? · 类别 · 归一 time_text ·
/// 归一正文 · 归属类型 · 归一归属键)，hex。
///
/// **为什么不用 `compute_mem_id`**（S2b-2a）：fact 的身份是"谁在什么时间下承诺/决定了什么"，
/// 它的 preimage 必须包含 subject/object/predicate/value/category/time_text 这六格；
/// `compute_mem_id` 只有 `(table, kind, title, body, scope)`，两条同正文不同宾语的承诺会撞成
/// 一行——`INSERT ... ON CONFLICT DO NOTHING` 把第二条静默吞掉，不报错、不红。
/// 形状照同文件的 `compute_chain_id`：段间 0u8 分隔（`("ab","c")` 与 `("a","bc")` 不许撞）。
///
/// 三条细节：
/// ① `text` / `time_text` / `scope.key` 先过 `normalize_body`，与阶段 0 的 turn 身份、
///    `compute_mem_id` 共用同一把归一尺子（CRLF 与 NFC 的差异不许造出"同一条事实的两个 id"）；
/// ② `object` / `value` 的缺席**不是空段**：`None` 写标记字节 0x01，`Some(s)` 写 0x02 + `s`。
///    若把缺席写成空段，`None` 与 `Some("")` 会算出同一个 id（"没有对象"与"对象是空串"
///    是两条不同的行）；若写成 `"None"` 字面量，一个真叫 `None` 的值就能冒充缺席；
/// ③ 段间分隔是 `0u8`，所以字段边界不同必算出不同 id。
#[allow(clippy::too_many_arguments)]
pub fn compute_fact_id(
    subject_anchor_id: &str, predicate: &str, object_anchor_id: Option<&str>,
    value: Option<&str>, category: &str, time_text: &str, text: &str, scope: &Scope,
) -> String {
    let mut h = Sha256::new();
    for part in ["fact", subject_anchor_id, predicate] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    push_optional(&mut h, object_anchor_id);
    push_optional(&mut h, value);
    let normalized_time = normalize_body(time_text);
    let normalized_text = normalize_body(text);
    let normalized_key = normalize_body(&scope.key);
    for part in [category, &normalized_time, &normalized_text,
                 scope.kind.as_str(), &normalized_key] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 可选段的编码：缺席 = 单字节 0x01，在席 = 0x02 + 值。两枚标记都不是 UTF-8 文本里会出现的
/// 可见字符 ⇒ 缺席不会被某个字面量（例如 `"None"`）冒充，空串（0x02 + 空 + 0x00）也不会被
/// 当成缺席（0x01 + 0x00）。与 `compute_mem_id` 的 0u8 段间分隔同一档：它们防的是"写错了
/// 也看不出来"的那一类碰撞，不是逆向不可伪造。
fn push_optional(h: &mut Sha256, part: Option<&str>) {
    match part {
        None => h.update([0x01u8]),
        Some(s) => { h.update([0x02u8]); h.update(s.as_bytes()); }
    }
    h.update([0u8]);
}

/// `time_expr` 行的 id = sha256("time_expr" · 轮 · 归一短语 · kind)，hex（S2d-1）。
///
/// 一条时间行的身份是「**哪一轮的哪句短语、按哪种意图读**」：同一句"下周三之前"在同一轮里
/// 既有 deadline 读法也可能有别的读法，两种读法是两行；换一轮就是另一行（短语的参照钟跟着轮走）。
/// `start_ms/end_ms/precision/basis` **不进** preimage——它们是解析结果，本来就要被回填改写；
/// 若进 id，一次回填就会换来一个新 id ⇒ 同一句短语在库里长出第二行。
///
/// 三条细节与同族一致：段间 `0u8` 分隔（`("ab","c")` 与 `("a","bc")` 不许撞）；短语先过
/// `normalize_body`（CRLF/首尾空白/NFC 不造"同一条短语的两个 id"）；表位写死 `"time_expr"`，
/// 与 `compute_edge_id`/`compute_event_link_id` 同一档前缀，不与别的句柄空间共用。
pub fn compute_time_expr_id(turn_uid: &str, text: &str, kind: &str) -> String {
    let normalized_text = normalize_body(text);
    let mut h = Sha256::new();
    for part in ["time_expr", turn_uid, &normalized_text, kind] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// `state_assertion` 行的 id = sha256("state_assertion" · 主体锚 · 维度 · 归一值 · 证据轮 ·
/// 归属类型 · 归一归属键)，hex（P2 状态 pass，方案 `2026-10-31-state-pass-p2.md` §4）。
///
/// 一条状态行的身份是「**谁说、在某一轮里、把哪个主体的哪个维度直陈成哪个值**」：
/// - `valid_from_ms` / `created_at_ms` **不进** preimage——它们取证据轮时刻，是列值不是身份；
/// - `value_before` **不进** preimage——它是库里既有行的读数（程序补），同一句直陈换个时刻
///   重放仍是同一行；进了它，重放会因库里多了一条旧状态而长出第二行；
/// - `mode` / `confidence` / `superseded_by` 同理不进：前者由引文判、后者由审核后写。
/// - 证据轮进 id：同一句"我很焦虑"在两个不同轮里是两条记忆（两次说过 = 两次发生），
///   与 [`compute_fact_id`] 把 `text`（含轮次语义）进 id 的同一条读法一致。
///
/// 段间 `0u8` 分隔、可选段走 [`push_optional`]，与同族其余 id 函数同一把尺子。
pub fn compute_state_id(
    subject_anchor_id: &str, dimension: &str, value_after: &str,
    evidence_turn_uid: &str, scope: &Scope,
) -> String {
    let normalized_value = normalize_body(value_after);
    let normalized_key = normalize_body(&scope.key);
    let mut h = Sha256::new();
    for part in [
        "state_assertion", subject_anchor_id, dimension, &normalized_value,
        evidence_turn_uid, scope.kind.as_str(), &normalized_key,
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// `episode` 结构行的 id = sha256("episode" · "row" · 归属类型 · 归一归属键 · **有序成员 fact id 逐个**)。
/// 它的镜像消费卡走 [`compute_episode_card_id`]（同一组输入、只差侧标）。
///
/// **为什么不走 `compute_mem_id`**（派发档 I-3a 交付 1，母档 D3 承重）：那一条的 preimage 只有
/// `(table, kind, title, body, scope)`，**成员列表根本不在里面**；而"一段经历"的身份就是
/// "哪几条 fact 在一起"——标题与叙事只是它的说法。用标题做 id 会带来两件事：
/// ① 同标题不同成员的两段经历撞成一行（`INSERT OR IGNORE` 静默吞掉成员，与 `compute_chain_id`
///    那一格点名的形状同源）；② 归组层换个措辞重写叙事就换句柄，而成员一个没动。
/// 母档**明令不许**改成"只哈希 scope + 标题"，理由就是 ①。
///
/// **这一串"有序成员"是谁给的序**（派发档 I-3e · FIX-2 改判，函数本身没动、动的是调用方）：
/// 是写入口**准入之后**、按 [`crate::writers_x::event_order`] 排好的那一批 fact id，
/// 不是归组层当初递进来的那一串。修前递的是后者 ⇒ 被拦掉的成员也进 preimage，
/// 于是"同一个句柄对应两套内容"（落库的成员行却是准入后的另一套），配 `INSERT OR IGNORE`
/// 就是第一遍的内容被永久冻住、重放不收敛（I-3d 的 T-7 实测：两成员与消音后一成员的
/// `episode.id` 都是 `87bd79ca0b39…`）。改判之后 id 与 `episode_fact`/卡上指针**同源**，
/// 句柄说的就是库里那一段经历的内容。
///
/// 三条细节与同族一致：
/// ① 归属键过 `normalize_body`（CRLF/NFC 不造"同一段经历的两个 id"），成员 id 是库内已有的
///    十六进制句柄，原样进 preimage；
/// ② **段间 `0u8` 分隔**：`["ab","c"]` 与 `["a","bc"]` 两组成员必须算出不同 id；
/// ③ **成员顺序进 preimage**：哈希仍然含序，所以"事件序不同 ⇒ 句柄不同"这一格没变
///    （D3 接受它）；变的是**谁决定那个序**——它现在由成员自己的时间列决定，
///    归组层换个调用序交同一段经历，算出来的是同一个句柄（那正是内容寻址要的）。
///    要证明哈希真含序，见 `rc-memory/tests/episode.rs` 的
///    `句柄由归属与有序成员算出_换序即换句柄_卡与行同源不碰撞`（变异 T-5① 打的就是这一格）；
///    要证明写入口交进去的是准入后的事件序，见同文件的
///    `句柄取准入后按事件序的成员_少一个成员就换一个句柄`。
pub fn compute_episode_id(scope: &Scope, ordered_fact_ids: &[String]) -> String {
    episode_handle("row", scope, ordered_fact_ids)
}


/// `episode` 镜像卡（`card(kind='episode')`）的 id = sha256("episode" · "card" · 归属类型 ·
/// 归一归属键 · **有序成员 fact id 逐个**)。
///
/// **为什么句柄不扩 `MemTable` 而是镜像一张卡**（派发档 I-3a 设计要点）：`keys.rs` 的
/// `MemTable` 只有 `Anchor|Card`（全仓数百处引用），`episode` 不是可指认句柄；消费面要的
/// 那个稳定句柄因此只能是卡，`episode`/`episode_fact` 只做结构源。
/// 与 [`compute_episode_id`] 的关系是**同源 1:1**：同一组 `(scope, 有序成员)` 必配成一对、
/// 两侧各一；而 `side` 那一段不同 ⇒ 两串 hex 必不相同 ⇒ 镜像卡不可能冒充结构行，
/// 反之 `table_of()` 那种"两张可指认表各查一次"的读法也不会被搅浑。
/// 与母档 D6 同一格：episode 之间若日后要连边，端点必须落在 `card:`，用的就是这一串。
pub fn compute_episode_card_id(scope: &Scope, ordered_fact_ids: &[String]) -> String {
    episode_handle("card", scope, ordered_fact_ids)
}

/// 两个 episode 句柄的**唯一**preimage 实现（守卫 3：结构行与镜像卡不许各写一份成员哈希）。
/// `side` 取 `"row"` / `"card"`；值域由上面两个公开入口把守，本函数不校验——多一个侧标就是
/// 多一个句柄空间，那要连着改 `rc-memory/tests/episode.rs` 的"同源不碰撞"那一条。
fn episode_handle(side: &str, scope: &Scope, ordered_fact_ids: &[String]) -> String {
    let mut h = Sha256::new();
    let normalized_key = normalize_body(&scope.key);
    let mut parts: Vec<&str> = vec!["episode", side, scope.kind.as_str(), &normalized_key];
    for id in ordered_fact_ids { parts.push(id.as_str()); }
    for part in parts {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}
