//! G1 刀 A：存量卡的程序回填（派发档语义裁定④）。
//!
//! 功能上线前落库的 fact 没有章——本入口拿**与写点同一个判定函数**
//! （[`rc_memory::value_gate::program_gate_hit`]）把存量行重新过一遍：命中的 fact
//! 找到它的镜像卡（`pointers_json` 含 `fact:<id>` 且 `mode='stated'`，与
//! `writers_x::member_cards_are_silenced` 同一把尺子），盖 `sink='archive'`。
//!
//! 口径：
//! - **幂等**：`INSERT OR IGNORE` 先写者赢，第二遍 `newly_marked == 0`，章集合不变。
//! - **同一把尺子**：自锚解析走 [`session_agent_anchor_id`]（与写点同一入口），
//!   human 出处走 [`rc_memory::value_gate::human_provenance_exists`]（查 fact 侧，
//!   与卡的镜像出处逐条同形），判定走 `program_gate_hit`——三处都不另写一份。
//! - **不碰卡面**：原文/verdict/vote 一个字节不动（裁定③）。
//! - **钟**：章是现在盖的 ⇒ `at_ms = now_ms`（回填时刻）；写点盖的章取源轮时刻，
//!   两者不混。重跑不覆盖既有章的钟（先写者赢）。
//! - 只读库拒写（现网库只读测量时，回填只在临时副本上跑）。

use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::value_gate::{self, GateInput, PROGRAM_GATE_REASON};
use rc_memory::writers_v4::session_agent_anchor_id;
use rc_memory::MemError;
use rc_store::MemoryHandle;

/// 回填读数（三格都是计数，不带正文——守卫 12）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackfillReport {
    /// 过了一遍判定的 stated fact 行数。
    pub scanned: usize,
    /// 判定命中的 fact 行数（一张 fact 可能有多张镜像卡，章数另计）。
    pub hits: usize,
    /// 本次真插入的章数（重放/二次回填 ⇒ 0）。
    pub newly_marked: usize,
}

/// 一条存量 fact 的判定输入（只取判定要用的列，正文一列不读）。
struct LegacyFact {
    id: String,
    subject_anchor_id: String,
    speaker_anchor_id: Option<String>,
    predicate: String,
    category: String,
    source_agent: String,
    scope_kind: String,
    scope_key: String,
}

/// 对全库 stated fact 跑一遍程序预闸，给命中行的镜像卡补章。整批一个事务。
pub fn backfill_program_archive(m: &MemoryHandle, now_ms: i64) -> Result<BackfillReport, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    // 先收齐输入（同一连接上不在写入事务里悬着读语句）。
    let facts: Vec<LegacyFact> = {
        let mut st = m.conn().prepare(
            "SELECT id, subject_anchor_id, speaker_anchor_id, predicate, category,
                    source_agent, scope_kind, scope_key
               FROM fact WHERE mode='stated' ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(LegacyFact {
                id: r.get(0)?,
                subject_anchor_id: r.get(1)?,
                speaker_anchor_id: r.get(2)?,
                predicate: r.get(3)?,
                category: r.get(4)?,
                source_agent: r.get(5)?,
                scope_kind: r.get(6)?,
                scope_key: r.get(7)?,
            })
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let norm_version = rc_store::capability::NORM_VERSION as i64;
    let mut report = BackfillReport::default();
    let tx = m.conn().unchecked_transaction()?;
    for f in &facts {
        report.scanned += 1;
        // scope 解析不出来 ⇒ 不命中（宁漏勿错；写侧 validate 保证合法行不会走到这一支）。
        let Some(kind) = ScopeKind::parse(&f.scope_kind) else {
            continue;
        };
        let scope = Scope { kind, key: f.scope_key.clone() };
        let self_anchor = session_agent_anchor_id(m, &f.source_agent, &scope)?;
        let human = value_gate::human_provenance_exists(&tx, "fact", &f.id)?;
        let hit = value_gate::program_gate_hit(&GateInput {
            subject_anchor_id: &f.subject_anchor_id,
            speaker_anchor_id: f.speaker_anchor_id.as_deref(),
            self_anchor_id: self_anchor.as_deref(),
            human_in_provenance: human,
            category: &f.category,
            predicate: &f.predicate,
        });
        if !hit {
            continue;
        }
        report.hits += 1;
        // 镜像卡：与 member_cards_are_silenced 同一把尺子（pointer + mode='stated'）。
        let pointer = format!("fact:{}", f.id);
        let card_ids: Vec<String> = {
            let mut st = tx.prepare(
                "SELECT c.id FROM card c, json_each(c.pointers_json) p
                  WHERE p.value=?1 AND c.mode='stated' ORDER BY c.id",
            )?;
            let rows = st.query_map([&pointer], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for card_id in card_ids {
            report.newly_marked += value_gate::mark_archive(
                &tx, &card_id, PROGRAM_GATE_REASON, now_ms, norm_version)?;
        }
    }
    tx.commit()?;
    Ok(report)
}