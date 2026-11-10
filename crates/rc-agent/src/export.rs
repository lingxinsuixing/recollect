//! `export`（C3）：人类面的可读导出件。它是正文离开库的**第一道**门（另一道是 agent 的 `recall`）。
//!
//! 两条承诺立在这里：① 被抑制的行**也**进导出件，并写明它当时为什么沉默（原因格子）；
//! ② 超限时明写"还有 N 行未列出"。溯源有两档形状：**一跳**（事实 → 出处轮次）与
//! **两跳**（任务 7：事实 → 依据行 → 那条依据行的出处轮次），字段形状按"加跳是加法"来定。
//!
//! S1b-C4b1 起它还有第三道门：**读闸**（`rc_memory::gate`，设计 §4.2）。权限不是第三格
//! "沉默原因"——被拒的行在导出件里**不出现、不解释**，因为它与"库里没有"必须是同一个形状。
//! C4b2b 起这道门也管 `## edges`：两端**任一**不可见，整条边就不出现（边没有"沉默原因"格，
//! 连 `## edges` 段都可能整个不出现）。

use crate::error::ToolError;
use crate::recall::TZ_OFFSET_MIN;
use rc_contract::text::format_cn_ms;
use rc_memory::reader;
use rc_search::gate::{cells_of, GateCell};
use rc_store::MemoryHandle;

/// C3 的人类面承诺：**被抑制的行 + 原因格子 + "还有 N 行未列出"**。
pub struct ExportRequest {
    pub now_ms: i64,
    /// 本阶段固定 `None`（全量）。`Some` 留给阶段 3 的"按实体导出"，以及
    /// `tests/export_doc.rs` 那条把它跑活的用例（生产路径上它此刻是死的，见那里的第 6 条证伪）。
    pub limit: Option<usize>,
    /// 读闸的读者身份（S1b-C4b1）。`None` = 不过闸（中心 CLI/测试/旧调用）；
    /// mcp 的中心面给 `Some(Viewer::human(宿主 agent))`，被拒的行**不出现、不解释**。
    /// 它住请求里而不另开一个形参：`limit` 与"谁能读"必须同时到取数点，否则
    /// "截断"与"权限"两套口径会在同一个循环里打架（§4.2）。
    pub viewer: Option<rc_memory::gate::Viewer>,
}

impl ExportRequest {
    pub fn all(now_ms: i64) -> Result<Self, ToolError> {
        Ok(ExportRequest { now_ms, limit: None, viewer: None })
    }
}

fn render_steps(out: &mut String, steps: Vec<rc_memory::ProvenanceStep>) {
    for step in steps {
        match step {
            rc_memory::ProvenanceStep::Turn { turn_uid, role } => {
                out.push_str(&format!("    ← turn {turn_uid} ({})\n", role.as_str()));
            }
            rc_memory::ProvenanceStep::Basis { ref_kind, target_id, role, basis_exists } => {
                if basis_exists {
                    out.push_str(&format!("    ← {ref_kind}:{target_id} ({})\n", role.as_str()));
                } else {
                    // missing 那一支**不渲 role**：`anchor_role` 是写入时按两端取的最强值，
                    // 行都没了还并列一个可核的锚角色，读的人会以为那条依据还在。
                    out.push_str(&format!("    ← {ref_kind}:{target_id} (missing)\n"));
                }
            }
            rc_memory::ProvenanceStep::BasisTurn { turn_uid, role } => {
                out.push_str(&format!("        ← turn {turn_uid} ({})\n", role.as_str()));
            }
        }
    }
}

pub fn render_export(m: &MemoryHandle, req: &ExportRequest) -> Result<crate::ExportDoc, ToolError> {
    // 逐行判形态：与 probe 走同一个 decide（守卫 3）。export 里出现"某行为什么沉默"这件事，
    // 判据必须与探针当时的判断同源，否则导出件会变成一套独立的解释。
    let now = req.now_ms;
    // 读闸（S1b-C4b1，设计 §4.2）：`export` 是**人类面**，权限只经 `rc_memory::gate` 这一个家判。
    // 过滤**在循环之前**：`limit`、"还有 N 行未列出"、`listed of total` 三处读数因此全部
    // 基于可见集——被闸掉的行既不出现在清单里，也不进"未列出"的账（后者是"我能要却没给"，
    // 不是"我看不见"）。被拒行**不留任何痕迹**：导出件里不许出现"无权限"之类的解释，
    // 否则那一行存在与否本身就泄露了。
    //
    // C4b2b 起装载**一次**、两处共用：事实行走 `filter`（与 `facts_visible` 同一个实现），
    // 边两端走 `allows`（边没有行事实，只有句柄）。分两次装载会让"事实可见"与"边可见"
    // 各自读一次全量归属表，还可能读到不同快照。
    let gate = req
        .viewer
        .as_ref()
        .map(|v| rc_memory::gate::GateIndex::load(m, v, now))
        .transpose()
        .map_err(ToolError::from)?;
    let facts = reader::facts(m).map_err(ToolError::from)?;
    let facts = match &gate {
        Some(g) => g.filter(facts),
        None => facts,
    };
    // **MF-1（派发档 I-5a §0）：这一列在这里被读。** `time_confidence` 全仓有写者、有列归属、
    // 有列定义，但今天**零生产读者**——四个消费面（这里、`recall` 的三个视图、`surface` 的
    // 两行）一律只读 `as_of_ms`。episode 镜像卡在成员全无可知事件时间时，`as_of_ms` 落的就是
    // 发话时刻（`writers_x::episode_time_arm` 的 `'speech'` 那一支），照旧渲染就是把"说过"写成
    // "发生在"。这一面是**人类面**、逐字节 diff 是它的使用方式，所以：
    // - `speech` ⇒ 行尾多一格 `time=发话时刻（事件时间未知）`；
    // - 其余三支（`anchored` / `unknown` / NULL / 闭集外的串）⇒ **一字不加**（派发档明写
    //   unknown/NULL 沿用既有那一支；域外串走同一支且不回显原串）。
    // 于是既有导出件的 diff 面不受影响，而那一支谎报被堵住。
    // 读法是一次批量侧查（`reader::card_modes`）：逐行一次点查在这一面是全量导出，条数不是 20。
    let card_ids: Vec<String> = facts.iter()
        .filter(|f| f.key.table == rc_memory::MemTable::Card)
        .map(|f| f.key.id.clone())
        .collect();
    let card_modes = reader::card_modes(m, &card_ids).map_err(ToolError::from)?;
    // 债 #9（修法甲）：`card.time_json` 的事件时间与 `card_modes` 同一笔批量侧查（1 条 SQL）。
    // 事件支优先于 `speech` 支（契约 1：`time_json` 里真有事件时间的卡不得再说"事件时间未知"）。
    let card_events = reader::card_event_times(m, &card_ids).map_err(ToolError::from)?;
    let time_of = |f: &rc_memory::reader::RowFacts| -> &'static str {
        if f.key.table != rc_memory::MemTable::Card {
            // 锚行结构上没有这一列（`select_sql` 给它补字面量的同一族理由）⇒ 无信号 ⇒ unknown 支。
            return crate::recall::time_confidence_slug(None);
        }
        let raw = card_modes.iter().find(|c| c.id == f.key.id)
            .and_then(|c| c.time_confidence.as_deref());
        // 归一与 brief/card/expand 同一个家（`recall::time_confidence_slug`），不在这里重判。
        crate::recall::time_confidence_slug(raw)
    };
    let event_of = |f: &rc_memory::reader::RowFacts| -> Option<i64> {
        if f.key.table != rc_memory::MemTable::Card {
            return None;    // 锚行结构上没有 `time_json`（同上）
        }
        card_events.iter().find(|e| e.id == f.key.id).map(|e| e.start_ms)
    };
    let mut out = String::new();
    out.push_str(&format!("# recollect export (schema {})\n", crate::SCHEMA_VERSION));
    // 副本身份进导出件头部（架构 §6）：人会拿一周前的快照当现状，除非文件自己声明它是快照。
    // 它的读者：副本侧 `tools_shape.rs::副本面响亮标_replica_且写入一律拒`（`source: replica`），
    // live 侧 `tools_shape.rs::source_在_live_面上同样是读数`（`source: live`，R47）。
    out.push_str(&format!("> source: {}\n", crate::tools::source_of(m)));
    let mut cells = cells_of(&[]);
    let mut listed = 0usize;
    for f in &facts {
        // `limit` 到了就停：写进导出件的行数与末尾那句"还有 N 行未列出"必须是同一个真相
        // （brief 原文把 `listed` 加到全部行却仍报"未列出"，那两句会互相打脸；
        //  偏离记录在 task-7-report.md 后半的偏离清单）。
        if req.limit.is_some_and(|limit| listed >= limit) {
            break;
        }
        // 本面全量导出、**不经 `rank`**（数据来自 `reader::facts`），所以这里根本没有分数可用。
        // 判形态走 `recall::decide_no_query`（唯一家，见那里的文档）：`i64::MAX` 把"低分"这一开关
        // 显式设为通过 ⇒ `low_score` 格在导出件里恒 0（`tests/export_doc.rs` 那条 `- low_score: 0`
        // 的断言就是它），低分只由 `probe_view` 报。与 recall 面同一个家 = 同一个真相（守卫 3）。
        let d = crate::recall::decide_no_query(f, now);
        if let Some(c) = d.cell {
            cells.record(c);
        }
        // **沉默的行也进导出件**（C3 与 G6 判据③的合取：库里有的都要能看见）。
        // 行尾那一格 `time=…` 只在两支上出现（见上面 `time_of`/`event_of` 的注释）：
        // `time_json` 事件时间在场 ⇒ `time=事件发生于 …`（事件支优先，债 #9）；
        // `time_confidence='speech'` ⇒ `time=发话时刻（事件时间未知）`（既有形状）；
        // 其余三支一个字节都不加，既有导出件的 diff 面因此不变。
        let tc = time_of(f);
        let time_cell = match event_of(f) {
            Some(ms) => format!(", time=事件发生于 {}", format_cn_ms(ms, TZ_OFFSET_MIN)),
            None => if tc == "speech" { ", time=发话时刻（事件时间未知）".to_string() } else { String::new() },
        };
        out.push_str(&format!(
            "- `{}` {} — {} (as_of {}, form {}, reason {}{time_cell})\n",
            d.key.key_string(),
            f.kind,
            f.title,
            f.as_of_ms,
            // M-2（裁定 R43）：这里原来是 `unwrap_or("?")`——G7 的账目里不许留一个不可枚举的兜底值。
            // 与 `error.rs` 的 `slug()` 同族写法：封闭枚举 + derive Serialize 的渲染**结构上不会失败**，
            // 那就响亮地 expect，而不是给它一个假的读数。
            serde_json::to_value(d.form)
                .expect("Form has derived Serialize")
                .as_str()
                .expect("Form serializes to a string"),
            d.cell.map(|c: GateCell| c.slug()).unwrap_or_else(|| "shown".to_string()),
        ));
        // 偏离 brief 的一处（必要，载体是 Step 3 那份逐字金丝雀用例
        // `正文允许出现在_recall_与_export_里`：它断金丝雀样本串出现在导出件里，而那串住在**正文**，
        // brief 的行格式只渲染 `f.title` ⇒ 照抄会让那条用例永久红。C3 是人类面，
        // "库里那条东西说了什么"读得到才是导出件的意义，所以补的是一行正文，不是改那条用例。
        // 缩进两格：它不是列表项，`- ` 的行数仍然等于被导出的行数。
        out.push_str(&format!("    正文 {}\n", f.body));
        // 一跳溯源：句柄 → 出处轮次 uid（身份值，零正文，所以它可以出现在这里）。
        // **这里不写 SQL**：`provenance` 的列归 `rc-memory`（守卫 11），而 rc-agent 的依赖集里
        // **直接**没有 `rusqlite`——那不是风格而是可数的检查。判据（裁定 R39 改写，口径 = depth 1）：
        // `cargo tree -p rc-agent -e normal --depth 1 | grep rusqlite` 必须为空。
        // 为什么必须带 `--depth 1`：`rusqlite` 是 `rc-store` 的**非可选 normal 依赖**，而
        // `MemoryHandle` 就是本文件（与 `recall`）的入参类型 ⇒ 传递依赖必然在树里，不加 `--depth 1`
        // 的那条判据按字面**永远不可能为空**，grep 到的每一处都只说明"rc-store 在用 sqlite"，
        // 对"rc-agent 自己有没有伸进 SQL 层"一个字也不说。
        // 两跳溯源（任务 7）：`provenance_chain` 只给有序步骤，排版是这一层的职责。
        // 三种行各有形状：`    ← turn …` = 本行的出处轮次（既有形状，一字不改）；
        // `    ← <表>:<id> …` = 本行的依据行；`        ← turn …` = 那条依据行的出处轮次。
        // 依据行 missing ⇒ 明写 (missing) 且不渲第二跳——"没有'然后'就不能报"的反读，
        // 省略在这里等于"库里那条东西没有依据"，而事实是"有依据、依据已经不在了"。
        let chain = rc_memory::reader::provenance_chain(m, &f.key).map_err(ToolError::from)?;
        render_steps(&mut out, chain);
        // 第四形（批次③ 段 C）：**因果链头的有序环节**。链头 = `card(kind='causal')` 那一行
        // （canonical 之前它住 `event`，旧行已并入 `card`，见 `writers_x::put_chains`），
        // 它的环节落 `provenance` 的 `role='link'` 那一族；`provenance_chain` 现在把环节当
        // "环节不是依据"筛掉了（见 reader.rs 的 role 筛），所以环节在**这一支**单独按 `ord` 序渲出来。
        // 形状 `    ⇒ link <表>:<句柄> (<角色>)`：箭头 `⇒` 与既有三种的 `←` 区分开、`link` 一词点明
        // 它是环节而非依据，读的人一眼分得清"这是链条上的一步"而不是"这是它凭什么"。
        // **守卫 12 照旧**：只给被引行的表前缀、句柄与角色，正文一个字都不带——链头指向的那一行，
        // 其正文只会在**它自己那一行**（上面 `正文` 那一句）出现，绝不随环节泄进导出件。
        // 只有因果事件才试这一支：既避免给每张普通卡多打一次 `chain_links` 查询，也让
        // "kind='causal' 但不是链"（如 G4 手写那条）自然走空——它没有 `role='link'` 行 ⇒ 不渲环节。
        if f.key.table == rc_memory::MemTable::Card && f.kind == "causal" {
            // 链头类型（批次 M 段 C）：`    ⇒ type=<v>` 一行，位置在 `⇒ link` 之前。
            // NULL/非链 ⇒ `type=unknown`，与时间线 `time_confidence=unknown` 同族；形状只有一种
            // （四空格 + `⇒ type=` + 值），空值那一支不分叉。**守卫 12**：这一行只有类型值，不带正文。
            let ct = rc_memory::reader::causal_type_of(m, &f.key).map_err(ToolError::from)?;
            out.push_str(&format!("    ⇒ type={}\n", ct.as_deref().unwrap_or("unknown")));
            let links = rc_memory::reader::chain_links(m, &f.key).map_err(ToolError::from)?;
            for l in links {
                out.push_str(&format!("    ⇒ link {}:{} ({})\n",
                    l.ref_kind, l.target_id, l.anchor_role.as_str()));
            }
        }
        // A44 软边：时间行若锚在一条事件上，软边从**这一行**指向被锚事件。
        // 只渲身份值与 evidence（id 数组），`basis` 槽里存的就是这一行自己的正文，
        // 正文只在上面那一句出现一次（守卫 12 的同一条纪律）。
        if f.key.table == rc_memory::MemTable::Card && f.kind == "timeline" {
            if let Some(l) = reader::event_link_of(m, &f.key.id).map_err(ToolError::from)? {
                // `l.a_id` 已经是 canonical 的整句柄（`<table>:<id>`，本族两端都是 `card:`，
                // 见 `reader::EventLink` 的文档）：这里**不再自己拼前缀**——旧写法硬写
                // `event:` 会渲染出 `event:card:<id>` 这种双前缀，把"被锚事件的句柄"说错。
                // 形状仍是 `⇒ event_link <句柄> evidence=<json>`，只是句柄取自读数本身。
                out.push_str(&format!("    ⇒ event_link {} evidence={}\n",
                                      l.a_id, l.evidence_json));
            }
        }
        listed += 1;
    }
    let total = facts.len();
    out.push_str("\n## silence cells\n");
    for (c, n) in cells.counts() {
        out.push_str(&format!("- {}: {n}\n", c.slug()));
    }
    // 未列出数只在**这一个函数**里算（`crate::unlisted_count`）：`status`（任务 8）与这里
    // 各减一次就是两个真相。这里 `silent` 传 0——被沉默的行**已经列在上面了**，不属于"未列出"。
    let unlisted = crate::unlisted_count(total, listed, 0);
    // `req.limit == None` 时循环不可能提前停 ⇒ `unlisted` 恒为 0，这句话只跟着截断走。
    if req.limit.is_some() && unlisted > 0 {
        out.push_str(&format!("\n还有 {unlisted} 行未列出\n"));
    }
    out.push_str(&format!("\n{listed} rows listed of {total}.\n"));
    // C20 的边段：边不进事实行清单（裁定 3），所以单独一截；列表记号用 "* " 而不是 "- `"，
    // 免得把"列出的事实行数"那条既有读数（数 "- `" 开头的行）搅浑。
    // S1b-C4b2b：边**也过读闸**——两端**任一**不可见，整条边不出现（C4b1 登记的泄漏：
    // 事实行被闸住了，边行却把两张卡的句柄与关系渲了出来）。边不是行事实，所以它没有
    // "沉默原因"那一格：被拒的边与"库里没有这条边"必须同形，连 `## edges` 段都不进；
    // `edge_provenance` 只为**留下的**边查（被拒边的出处一行都不许读出来）。
    let edges = reader::edges(m).map_err(ToolError::from)?;
    let edges: Vec<_> = edges
        .into_iter()
        .filter(|e| match &gate {
            None => true,
            Some(g) => g.allows(&e.from_kind, &e.from_id) && g.allows(&e.to_kind, &e.to_id),
        })
        .collect();
    if !edges.is_empty() {
        out.push_str("\n## edges\n");
        for e in edges {
            out.push_str(&format!(
                "* `relationship:{}` {}:{} -> {}:{} ({}, scope {}:{}, retired {})\n",
                e.id, e.from_kind, e.from_id, e.to_kind, e.to_id, e.kind,
                e.scope.kind.as_str(), e.scope.key, e.retired));
            out.push_str(&format!("    因为 {}\n", e.reason));
            let steps = reader::edge_provenance(m, &e.id).map_err(ToolError::from)?;
            render_steps(&mut out, steps);
        }
    }
    Ok(crate::ExportDoc::from_markdown(out))
}
