//! 水位层（§7 的三个作业家族 + §6.5 P2 的每 turn 抽取态）。
//! 三条纪律住在这一层，不是住在调用方：① 倒退 ⇒ 停轮且游标不动；② 停轮期间一切推进都拒；
//! ③ `attempts` 与 `last_success_at_ms` 是两列（原设计 §6"故障期不计 attempts"要能表达）。
use rc_store::{LedgerHandle, StoreError, TranscriptHandle};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

/// 封闭家族清单。与 `job_watermark.family` 的 CHECK 一对镜像，`tests/water.rs` 逐字对拍。
pub const FAMILIES: [&str; 3] = [DISTILL, "derive", "anomaly_scan"];

/// `FAMILIES` 的第一项。**它是一个名字，不是把字面量重复一遍**：`FAMILIES` 那张清单是合法性的家，
/// 而 `distill` 这一族的名字今天起有四个读者（任务 6 的游标、任务 9 的 `charge_counters`、
/// 本任务的 `last_extract_at_ms` 与 `mcp` 的缝）。加这一条常量的同时把 `FAMILIES` 的初值改成
/// `[DISTILL, "derive", "anomaly_scan"]`——三个值一个都没动，所以任务 3 那条
/// `assert_eq!(water::FAMILIES, ["distill", "derive", "anomaly_scan"])` 逐字仍绿。
pub const DISTILL: &str = "distill";

/// 游标的**轴由 family 决定**：`distill` 是 `turns.ingested_at_ms`（到达钟），
/// `derive` 是记忆行的 `created_at_ms`；`anomaly_scan` 是扫描时刻的 `now_ms`
/// （它没有"待扫的记忆行"这个轴——扫的是全量，戳记的是这一轮扫过的时间）。
/// 字段名因此是 `at_ms`：写 `ingested_at_ms` 会让后两族背一条不成立的承诺，
/// 写 `created_at_ms` 会让 distill 与 anomaly_scan 各背另一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub at_ms: i64,
    pub turn_uid: String,
}

/// 一个 turn 的抽取态（§6.5 P2 的四格）。`parse` / `slug` 往返是这一列唯一的安全读法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractState {
    Pending,
    Extracted,
    Rejected,
    Worthless,
}

impl ExtractState {
    pub fn slug(self) -> &'static str {
        match self {
            ExtractState::Pending => "pending",
            ExtractState::Extracted => "extracted",
            ExtractState::Rejected => "rejected",
            ExtractState::Worthless => "worthless",
        }
    }
    pub fn parse(s: &str) -> Option<ExtractState> {
        match s {
            "pending" => Some(ExtractState::Pending),
            "extracted" => Some(ExtractState::Extracted),
            "rejected" => Some(ExtractState::Rejected),
            "worthless" => Some(ExtractState::Worthless),
            _ => None,
        }
    }
}

fn check_family(family: &str) -> Result<(), StoreError> {
    if !FAMILIES.contains(&family) {
        return Err(StoreError::Sql(format!("water: unknown family {family}")));
    }
    Ok(())
}

/// 游标行的读取（内部）：不存在时返回 None（"从没蒸过"与"游标在 0"在两处判据里是两件事，
/// 所以这里不替调用方造一个 0）。
fn cursor_row(l: &LedgerHandle, family: &str, scope_key: &str)
    -> Result<Option<(Cursor, bool)>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT cursor_at_ms, cursor_turn_uid, halted_at_ms FROM job_watermark
         WHERE family=?1 AND scope_key=?2")?;
    let mut rows = st.query_map(params![family, scope_key], |r| {
        Ok((
            Cursor { at_ms: r.get(0)?, turn_uid: r.get(1)? },
            r.get::<_, Option<i64>>(2)?,
        ))
    })?;
    match rows.next() {
        Some(Ok((c, halted))) => Ok(Some((c, halted.is_some()))),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

pub fn read(l: &LedgerHandle, family: &str, scope_key: &str) -> Result<Option<Cursor>, StoreError> {
    check_family(family)?;
    Ok(cursor_row(l, family, scope_key)?.map(|(c, _)| c))
}

pub fn halted(l: &LedgerHandle, family: &str, scope_key: &str) -> Result<bool, StoreError> {
    check_family(family)?;
    Ok(cursor_row(l, family, scope_key)?.map(|(_, h)| h).unwrap_or(false))
}

/// 推进 = 前进或幂等；倒退 = 停轮。二者互斥，且倒退那一次**一个字节都不改游标**。
pub fn advance(l: &LedgerHandle, family: &str, scope_key: &str, to: &Cursor, now_ms: i64)
    -> Result<(), StoreError> {
    advance_inner(l, family, scope_key, to, now_ms, false)
}

/// `advance` 的"这一批已经结算"形：停轮/倒退守卫与 `advance` 逐字相同，额外把 `attempts=0`。
///
/// 它存在的理由：`attempts` 是"这条线**累计**失败数"，不是"这一批失败数"。成功推过一批之后
/// 不归零，偶发失败会跨批攒到上限，把下一批好批也误隔离。derive 的成功路径与隔离路径都走这里；
/// distill 那一族继续用 `advance`（语义不动，`attempts` 在那条线上仍是累计读数）。
pub fn advance_settled(l: &LedgerHandle, family: &str, scope_key: &str, to: &Cursor, now_ms: i64)
    -> Result<(), StoreError> {
    advance_inner(l, family, scope_key, to, now_ms, true)
}

fn advance_inner(l: &LedgerHandle, family: &str, scope_key: &str, to: &Cursor, now_ms: i64,
                 settled: bool) -> Result<(), StoreError> {
    check_family(family)?;
    let tx = l.conn().unchecked_transaction()?;
    // 坑 P-71：`None` 只许是"确认查无此游标"。`.ok()` 会把 SQL 错误一起折成 None，于是停轮检查
    // 与倒退检查两道**一起被跳过**，后面的 upsert 就把游标静默覆盖（含覆盖成倒退的方向）。
    let existing: Option<(i64, String, Option<i64>)> = match tx.query_row(
        "SELECT cursor_at_ms, cursor_turn_uid, halted_at_ms FROM job_watermark
         WHERE family=?1 AND scope_key=?2", params![family, scope_key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
        Ok(row) => Some(row),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(halted_at) = existing.as_ref().and_then(|e| e.2) {
        return Err(StoreError::Sql(format!(
            "water: {family}/{scope_key} is halted since {halted_at} — advance refused")));
    }
    if let Some((cur_ms, ref cur_uid, _)) = existing {
        if to.at_ms < cur_ms
            || (to.at_ms == cur_ms && !cur_uid.is_empty() && to.turn_uid < *cur_uid) {
            // 同 ms 时按 turn_uid 定序；空 cursor_turn_uid（初值）不算倒退，因为它是"还没有过任何行"。
            tx.execute(
                "UPDATE job_watermark SET halted_at_ms=?3 WHERE family=?1 AND scope_key=?2",
                params![family, scope_key, now_ms])?;
            tx.commit()?;
            return Err(StoreError::Sql(format!(
                "water: {family}/{scope_key} cursor regression refused (halt set, cursor untouched)")));
        }
    }
    // `settled` 只多一句 `attempts=0`：新行本来就从 0 起，所以 INSERT 那一半两形相同。
    let sql = if settled {
        "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                   attempts, last_success_at_ms, halted_at_ms)
         VALUES(?1,?2,?3,?4,0,?5,NULL)
         ON CONFLICT(family, scope_key) DO UPDATE SET
           cursor_at_ms=excluded.cursor_at_ms,
           cursor_turn_uid=excluded.cursor_turn_uid,
           last_success_at_ms=excluded.last_success_at_ms,
           attempts=0"
    } else {
        "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                   attempts, last_success_at_ms, halted_at_ms)
         VALUES(?1,?2,?3,?4,0,?5,NULL)
         ON CONFLICT(family, scope_key) DO UPDATE SET
           cursor_at_ms=excluded.cursor_at_ms,
           cursor_turn_uid=excluded.cursor_turn_uid,
           last_success_at_ms=excluded.last_success_at_ms"
    };
    tx.execute(sql, params![family, scope_key, to.at_ms, to.turn_uid, now_ms])?;
    tx.commit()?;
    Ok(())
}

/// 这条线累计的契约/作业失败数。行不存在 ⇒ 0（"从没失败过"与"行还没建出来"是同一个读数：
/// 两列游标也都不在场，`read` 那一侧已经把"有没有行"表达成 `None`）。
pub fn attempts(l: &LedgerHandle, family: &str, scope_key: &str) -> Result<i64, StoreError> {
    check_family(family)?;
    Ok(l.conn().query_row(
        "SELECT attempts FROM job_watermark WHERE family=?1 AND scope_key=?2",
        params![family, scope_key], |r| r.get(0),
    ).optional()?.unwrap_or(0))
}

/// **运维面的单批回拨入口**：把游标设回 `to`、`attempts=0`、`halted_at_ms=NULL`。
///
/// 它与 `advance` 的分工：`advance` 守的是"别把位置弄丢"，而重跑**按定义**要把游标往回拨
/// ——那正是这个函数的语义，不是倒退事故。它与 `rewind` 的分工：`rewind` 归零整条线，
/// 这里只回拨到一条 issue 记下的 `prev`（单批重跑）；唯一的调用方是 `hub derive retry`，
/// 不是任何自动路径。
///
/// `last_success_at_ms` 一个字节都不动：重跑不是一次成功，盖它会把"最后一次成功"说成现在。
pub fn retry_from(l: &LedgerHandle, family: &str, scope_key: &str, to: &Cursor, now_ms: i64)
    -> Result<(), StoreError>
{
    check_family(family)?;
    l.conn().execute(
        "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                   attempts, last_success_at_ms, halted_at_ms)
         VALUES(?1,?2,?3,?4,0,NULL,NULL)
         ON CONFLICT(family, scope_key) DO UPDATE SET
           cursor_at_ms=excluded.cursor_at_ms,
           cursor_turn_uid=excluded.cursor_turn_uid,
           attempts=0,
           halted_at_ms=NULL",
        params![family, scope_key, to.at_ms, to.turn_uid])?;
    let _ = now_ms; // 回拨不留时间戳（`last_success_at_ms` 只由成功路径盖，理由见上）
    Ok(())
}

/// 停轮的解除只有这一条路，且调用方是运维面（`hub cycle once` 不自动叫它，任务 9）。
/// 阶段 4 的 `restore` 是唯一被授权的回拨：它用同文件的 `rewind`（归零按定义是倒退，
/// `advance` 会拒绝并停轮），回拨入口仍然只有水位这一族。
pub fn clear_halt(l: &LedgerHandle, family: &str, scope_key: &str, now_ms: i64) -> Result<(), StoreError> {
    check_family(family)?;
    let n = l.conn().execute(
        "UPDATE job_watermark SET halted_at_ms=NULL WHERE family=?1 AND scope_key=?2",
        params![family, scope_key])?;
    if n > 1 {
        return Err(StoreError::Sql(format!("water: clear_halt touched {n} rows")));
    }
    let _ = now_ms;   // 解除不留时间戳列（DDL 只有一列 halted_at_ms；加列是 schema 变更，不是这里的决定）
    Ok(())
}

/// 列出某一族在 `job_watermark` 里的**全部** scope 键。
/// `lag_by_scope` 不够用：它只给"有落差的"那些 scope，而恢复要归零的是**所有** scope。
pub fn scopes(l: &LedgerHandle, family: &str) -> Result<Vec<String>, StoreError> {
    check_family(family)?;
    let mut st = l.conn().prepare(
        "SELECT scope_key FROM job_watermark WHERE family=?1 ORDER BY scope_key")?;
    let rows = st.query_map(params![family], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 只给 restore：把 distill 家族的全部 scope 替换成给定停点（删除 + 插入，允许双向）。
///
/// "按清单设停点"与 `advance` 的分工：`advance` 守的是"别把位置弄丢"，而清单里的停点
/// 可能比当前游标**早**（restore 回退）——这正是恢复的语义，不是倒退事故。
/// 它也不触发停轮：`halted_at_ms` 一律写 NULL，`attempts` 归 0、`last_success_at_ms` 记本次恢复。
/// 其它家族一个字节都不动。
pub fn replace_distill_cursors(
    l: &LedgerHandle,
    cursors: &[(String, Cursor)],
    now_ms: i64,
) -> Result<(), StoreError> {
    check_family(DISTILL)?;
    let tx = l.conn().unchecked_transaction()?;
    tx.execute("DELETE FROM job_watermark WHERE family=?1", params![DISTILL])?;
    {
        let mut st = tx.prepare(
            "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                       attempts, last_success_at_ms, halted_at_ms)
             VALUES(?1,?2,?3,?4,0,?5,NULL)")?;
        for (scope, cursor) in cursors {
            st.execute(params![DISTILL, scope, cursor.at_ms, cursor.turn_uid, now_ms])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// **阶段 4 的单 scope 归零入口**。B5a 起 `restore` 改走
/// [`replace_distill_cursors`]（按清单设停点，不是归零）；本函数留作水位层自己的
/// 单 scope 工具，生产侧今天没有调用者。
pub fn rewind(l: &LedgerHandle, family: &str, scope_key: &str, now_ms: i64)
    -> Result<(), StoreError>
{
    check_family(family)?;
    let tx = l.conn().unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE job_watermark
           SET cursor_at_ms=0, cursor_turn_uid='', halted_at_ms=NULL
         WHERE family=?1 AND scope_key=?2",
        params![family, scope_key])?;
    if changed == 0 {
        // 这个 scope 从来没有过水位行 ⇒ 归零是空操作，**不是**错误。
        tx.commit()?;
        return Ok(());
    }
    tx.commit()?;
    let _ = now_ms; // 归零不留时间戳列（那一列是 `last_success_at_ms`，由下一次 advance 盖）
    Ok(())
}

pub fn note_attempt(l: &LedgerHandle, family: &str, scope_key: &str) -> Result<(), StoreError> {
    check_family(family)?;
    // 没有 now_ms 形参：attempts 是一格计数，它的时间同伴列是 `last_success_at_ms`（由 advance 盖）。
    l.conn().execute(
        "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                   attempts, last_success_at_ms, halted_at_ms)
         VALUES(?1,?2,0,'',1,NULL,NULL)
         ON CONFLICT(family, scope_key) DO UPDATE SET attempts = attempts + 1",
        params![family, scope_key])?;
    Ok(())
}

/// 一个 turn 一条状态：重复写是**重写这一条**，不长第二条（与 `retirement` 同一条不变量形状）。
pub fn set_extract_state(l: &LedgerHandle, turn_uid: &str, state: ExtractState,
                         reason: Option<&str>, now_ms: i64) -> Result<(), StoreError> {
    let n = l.conn().execute(
        "INSERT INTO extract_state(turn_uid, state, reason, at_ms) VALUES(?1,?2,?3,?4)
         ON CONFLICT(turn_uid) DO UPDATE SET state=?2, reason=?3, at_ms=?4",
        params![turn_uid, state.slug(), reason, now_ms])?;
    if n != 1 {
        return Err(StoreError::Sql(format!("water: state write touched {n} rows")));
    }
    Ok(())
}

/// 抽取管线的版本号：prompt 之外的那一半身份。管线改形状（新增一道闸门、改了渲染顺序）时
/// 改这一串，于是旧指纹自动与新一轮分开——它不回答"为什么不同"，只回答"是不是同一套"。
pub const EXTRACT_PIPELINE_VERSION: &str = "extract-v1";

/// 字段的长度前缀编码：先写自己的字节长度（u64 小端），再写字节。
/// 没有它，`("ab","c")` 与 `("a","bc")` 会拼成同一段输入——**字段边界挪一格不该等于同一套管线**。
fn hash_field(h: &mut Sha256, value: &str) {
    h.update((value.len() as u64).to_le_bytes());
    h.update(value.as_bytes());
}

/// 一轮抽取的身份：管线版本 + 模型 + 两段 prompt，四段长度前缀后 SHA-256，**64 位小写 hex**。
///
/// 它是这一格的**唯一实现点**：cycle 层只调它、不自己拼第二套编码（同 `distill_scope` 的键形，
/// 两处生产者就会漂）。`model` 未知时调用方写 `"unknown"`（回放座位没有模型身份）。
pub fn extraction_fingerprint(
    pipeline: &str, model: &str, prompt_system: &str, prompt_user: &str,
) -> String {
    let mut h = Sha256::new();
    // 域分隔：同一段输入被另一个用途哈希时不会撞上本函数。
    h.update(b"rc.extract_fingerprint.v1");
    hash_field(&mut h, pipeline);
    hash_field(&mut h, model);
    hash_field(&mut h, prompt_system);
    hash_field(&mut h, prompt_user);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 给**已存在的**状态行盖指纹。`set_extract_state` 的签名因此一字不动（大量调用点不动），
/// 而"状态"与"谁产的"是两次写：状态在抽取后落、指纹在 cycle 层盖。
///
/// 0 行命中是**正常读数**：LLM 故障轮没有状态行，UPDATE 什么也不碰。这里不许把 0 折成错误——
/// 那会把一次后端故障播报成"写失败"（`FAULT_STORE_WRITE`），故障族就串了。
pub fn set_extraction_fingerprint(l: &LedgerHandle, turn_uid: &str, fingerprint: &str)
    -> Result<(), StoreError>
{
    l.conn().execute(
        "UPDATE extract_state SET extraction_fingerprint=?2 WHERE turn_uid=?1",
        params![turn_uid, fingerprint])?;
    Ok(())
}

/// 指纹的一个分组读数（运维面 `hub fingerprint status` 的唯一数据源）。零正文：
/// 指纹是哈希、`turns`/`first`/`last` 是计数与时刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintCount {
    /// `None` = legacy 组（升级前或节点自蒸写下的状态行，从没盖过章）。
    pub fingerprint: Option<String>,
    pub turns: i64,
    pub first_at_ms: i64,
    pub last_at_ms: i64,
}

/// 按 `extraction_fingerprint` 分组：值组按指纹升序，**NULL 组排最后**
///（`ORDER BY … IS NULL` 让"没有身份"永远在读数末尾，翻页/人眼都有一个稳定位置）。
/// `first/last` 取该组 `at_ms` 的 min/max——"这批是什么时候蒸的"只有一个口径。
pub fn fingerprint_counts(l: &LedgerHandle) -> Result<Vec<FingerprintCount>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT extraction_fingerprint, count(*), min(at_ms), max(at_ms)
         FROM extract_state
         GROUP BY extraction_fingerprint
         ORDER BY extraction_fingerprint IS NULL, extraction_fingerprint")?;
    let rows = st.query_map([], |r| {
        Ok(FingerprintCount {
            fingerprint: r.get(0)?,
            turns: r.get(1)?,
            first_at_ms: r.get(2)?,
            last_at_ms: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 标记一个 turn 已由节点自蒸、中心按行吸收。**幂等**：同一 uid 重复标记不报错，
/// 也不刷新 `at_ms`——第一次落下的时刻是"这一轮是什么时候进来的"的唯一读数，
/// 重试（B3b 的失败窗口）不许把它改成"最后一次重试的时刻"。
pub fn mark_pre_distilled(l: &LedgerHandle, turn_uid: &str, at_ms: i64) -> Result<(), StoreError> {
    l.conn().execute(
        "INSERT INTO pre_distilled_turn(turn_uid, at_ms) VALUES(?1,?2)
         ON CONFLICT(turn_uid) DO NOTHING",
        params![turn_uid, at_ms])?;
    Ok(())
}

pub fn pre_distilled(l: &LedgerHandle, turn_uid: &str) -> Result<bool, StoreError> {
    let n: i64 = l.conn().query_row(
        "SELECT count(*) FROM pre_distilled_turn WHERE turn_uid=?1",
        params![turn_uid], |r| r.get(0))?;
    Ok(n > 0)
}

/// 全量已回并 turn 的集合。`select_batch` 在会话循环之外**一次**读它，
/// 与 `extract_state` 的 `stated` 集合并进同一个差集。
pub fn pre_distilled_uids(l: &LedgerHandle)
    -> Result<std::collections::HashSet<String>, StoreError>
{
    let mut st = l.conn().prepare("SELECT turn_uid FROM pre_distilled_turn")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for row in rows { out.insert(row?); }
    Ok(out)
}

/// 签名一字不动（任务 3/6 的八个调用点因此零改动：五处直接调它，三处经 `state_of`）。
/// 函数体换成薄包装——这一步的意义是那条老的两列查询从文件里消失了：判据数的是
/// "只取状态那一列"的旧 SQL 归零，而"从状态表读"的字面仍是 2（这里一次、`count_states` 的
/// GROUP BY 一次），见 §2.22 的两条判据。`Ok(None)` 只许是"这一轮没有状态行"——
/// P-71 的逐臂 `match` 现在住在 `extract_record` 里，包装层不再自己碰 SQL。
pub fn get_extract_state(l: &LedgerHandle, turn_uid: &str) -> Result<Option<ExtractState>, StoreError> {
    Ok(extract_record(l, turn_uid)?.map(|r| r.state))
}

/// 蒸馏水位的 **scope 键的唯一算法**。它住在拥有 `job_watermark` 的这一侧，正如 `Cursor` 与
/// `ExtractState` 住这一侧；调用方（`rc-distill::select_batch` 与本 crate 之外的 `mcp` 缝）
/// 只有两个零件，所以键形一旦有两处就是 P-63：漂的那个字符不报错，它只让 `read` 读回 `None`。
/// **与记忆归属的 `scope_key` 不是一件事**（那是 `Batch::scope_key()`，落在记忆行的 P 列上）。
pub fn distill_scope(device_id: &str, session_key: &str) -> String {
    format!("{device_id}/{session_key}")
}

/// 一行抽取记录：状态 + 理由 + 产出它的那套管线指纹。**它是那一行的唯一读法**（§2.22）——
/// 再加一把 `get_extract_reason` 就是把同一行的两列交给两个读者，
/// 而第一个给 `extract_state` 加列的人只会加第三把。
///
/// `fingerprint` 是 S1 加的第三列：旧行（升级前的状态行、节点自蒸写下的行）读出来是 `None`，
/// 那不是一个错误读数——"这一轮是谁产的"在那些行上本来就没记过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractRecord {
    pub state: ExtractState,
    pub reason: Option<String>,
    pub fingerprint: Option<String>,
}

pub fn extract_record(l: &LedgerHandle, turn_uid: &str) -> Result<Option<ExtractRecord>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT state, reason, extraction_fingerprint FROM extract_state WHERE turn_uid=?1")?;
    let mut rows = st.query_map(rusqlite::params![turn_uid], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?))
    })?;
    match rows.next() {
        Some(Ok((raw, reason, fingerprint))) => {
            // 脏 slug 是**响亮**的：库里躺着 `extract_state` 读不出的状态 = 有人写了别的产物。
            // 这里不许折成 `Pending`（那会把"数据不一致"读成"还没蒸"，P-44 那一族）。
            let state = ExtractState::parse(&raw).ok_or_else(|| {
                StoreError::Sql(format!("water: unreadable state {raw}"))
            })?;
            Ok(Some(ExtractRecord { state, reason, fingerprint }))
        }
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// P5 的分母与 P2 的聚合读数。按 slug 字典序返回——`ORDER BY state` 就是这一序，
/// 本任务的用例直接断它，不在测试里二次排序。
pub fn count_states(l: &LedgerHandle) -> Result<Vec<(ExtractState, i64)>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT state, count(*) FROM extract_state GROUP BY state ORDER BY state")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(|(s, n)| {
        ExtractState::parse(&s).map(|p| (p, n)).ok_or_else(|| {
            StoreError::Sql(format!("water: unreadable state {s}"))
        })
    }).collect()
}

/// 一个家族**最后一次成功推进**的时刻，跨全部 scope 取 `max`（P5 的 `last_extract_at_ms`）。
/// 从没成功过 ⇒ `None`，不是 `Some(0)`——理由与 `cursor_row` 那句"不替调用方造一个 0"同一条。
///
/// 为什么返回 `Option`：SQL 的 `max()` 对空集与全 NULL 都回 NULL，而 `last_success_at_ms`
/// 本来就可空（任务 3 的 DDL 注释：故障期不计 `attempts` 那句要靠它表达）。把它折成 `Some(0)`
/// 就是把"没有"读成"很久以前"。
pub fn last_extract_at_ms(l: &LedgerHandle, family: &str) -> Result<Option<i64>, StoreError> {
    check_family(family)?;
    Ok(l.conn().query_row(
        "SELECT max(last_success_at_ms) FROM job_watermark WHERE family=?1",
        params![family], |r| r.get::<_, Option<i64>>(0))?)
}

/// 推导家族最后一次成功推进的 ms（P6 的 `last_derive_at_ms`）。
/// 本体不重写：`last_extract_at_ms` 早就是按 `family` 参数化的（它名字里的 "extract" 是 P5
/// 那格的旧称，不是函数约束），跨 scope 取 `max`、空集回 `None` 这两条语义只有一家（守卫 3）。
/// `check_family("derive")` 由它内部那条 `check_family(family)` 覆盖，这里不再重复。
pub fn last_derive_at_ms(l: &LedgerHandle) -> Result<Option<i64>, StoreError> {
    last_extract_at_ms(l, "derive")
}

/// 全库停轮的 scope 数。停轮 = `job_watermark.halted_at_ms IS NOT NULL`。
/// 仅统计 `family='distill'`（停轮只在蒸馏家族登记——段 0 不为 derive/anomaly_scan 设 halt）。
/// 运维读数：`status.halted_scopes` 与 `doctor alerts[kind:halt]` 共用本函数。
pub fn halted_scopes_count(l: &LedgerHandle) -> Result<i64, StoreError> {
    let n: i64 = l.conn().query_row(
        "SELECT count(*) FROM job_watermark
         WHERE family='distill' AND halted_at_ms IS NOT NULL",
        [], |r| r.get(0))?;
    Ok(n)
}

/// 每个 scope 的 lag = 该 scope **可抽取**轮数（`role <> 'system'`）− 该 scope 有**终态**
/// pass 行的轮数。按 lag 降序；lag ≤ 0 的 scope 不返（"没有落后"不是诊断信号）。
///
/// **跨库，而且是真的两个文件**（E45 订正）：`Root::open` 开的是 `transcript.db` 与
/// `ledger.db` **两个独立 SQLite 文件、两个独立连接**（`rc-store/src/lib.rs` 里
/// `TranscriptHandle::connect(data_dir.join("transcript.db"))` 与
/// `LedgerHandle::connect(data_dir.join("ledger.db"))` 两行）⇒ **一条 SQL 里跨库 JOIN 是
/// 跑不出来的**，账本连接上根本没有 `turns`/`sessions` 这两张表。
/// E40 那版把签名写成 `lag_by_scope(l, t)`、SQL 里却从头到尾没用 `t`，而本任务的 Interfaces
/// 明明写着"两个 conn 各读各的家，守卫 6"——**签名与实现互相矛盾，SQL 是照着"能 JOIN"写的**。
/// 现在按接口那一句落成**两次查询 + 内存合**：
///
/// 1. 转录侧一条：`SELECT device_id || '/' || session_key, turn_uid FROM turns
///    WHERE role <> 'system'` ——`turns` 自己带 `session_key` 与 `device_id` 两列（DDL 实测），
///    **不需要 JOIN `sessions`**（E40 那版的那个 JOIN 是多余的：`sessions` 只在要会话元数据
///    时才用得上）。`role <> 'system'` 与抽取候选 SQL 同判据：system 轮永不作 TARGET，
///    也永不会有终态行，算进分母就是一个恒正的假积压。
/// 2. 账本侧一条：`SELECT DISTINCT turn_uid FROM extract_pass_state WHERE state IN
///    (TERMINAL_STATES)`——哪些轮次真的蒸完了。**只认终态**（`running`/`pending` 不算），
///    且不并 legacy `extract_state`（今天没有写者；并进来会把"从没蒸过"读成"已蒸"）。
/// 3. 内存里合：逐 scope 数"终态轮次"，相减。
///
/// **为什么这一步可以放在内存**：N 是 scope 数（一台设备上几十量级），不是轮次数；
/// 而且守卫 6 的形状本来就是"两个 conn 各读各的家"——把 JOIN 塞进一条 SQL 反而要求某一个
/// 连接看得见另一个库，那才是破守卫。
pub fn lag_by_scope(l: &LedgerHandle, t: &TranscriptHandle) -> Result<Vec<(String, i64)>, StoreError> {
    use std::collections::{BTreeMap, BTreeSet};
    // 转录侧：scope → 它的**可抽取**轮次 uid。system 轮（工具调用/结果、压缩块）只作上下文，
    // 永不作 TARGET，也就不该进分母——与 `transcript::count_extractable_turns` 同一条判据。
    let mut by_scope: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut st = t.conn().prepare(
            "SELECT device_id || '/' || session_key, turn_uid FROM turns WHERE role <> 'system'")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (scope, uid) = row?;
            by_scope.entry(scope).or_default().push(uid);
        }
    }
    // 账本侧：已有**终态** pass 行的轮次集合。S3b 的换源同样适用这一格：legacy
    // `extract_state` 今天没有写者，读它会把"从没蒸过"读成"已蒸"（现网实测：4 个 scope
    // 的 lag 全等于各自总轮数，因为 legacy 表 0 行）。
    let mut done: BTreeSet<String> = BTreeSet::new();
    {
        let sql = format!(
            "SELECT DISTINCT turn_uid FROM extract_pass_state WHERE state IN {}",
            crate::pass_state::TERMINAL_STATES);
        let mut st = l.conn().prepare(&sql)?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows { done.insert(row?); }
    }
    let mut out: Vec<(String, i64)> = by_scope
        .into_iter()
        .map(|(scope, uids)| {
            let n = uids.len() as i64;
            let extracted = uids.iter().filter(|u| done.contains(*u)).count() as i64;
            (scope, n - extracted)
        })
        .filter(|(_, lag)| *lag > 0)
        .collect();
    // 降序是内存排而不是 SQL `ORDER BY`——这不是偏好，是跨库合进内存的必然结果
    //（E40 那版选 SQL `ORDER BY` 的理由"别把排序挤成测排序的代码"在同库时成立，跨库不成立）。
    // 排序键带 scope 名做 tie-break：同 lag 的两个 scope 顺序稳定，导出件/JSON 才不抖。
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}
