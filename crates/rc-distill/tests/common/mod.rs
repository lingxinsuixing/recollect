#![allow(dead_code)]
//! 任务 6 三套 rc-distill 用例（`provenance.rs` / `zero_output.rs` / `batch_bounds.rs`）
//! 共用的五件夹具。`pub` 不是风格：`mod common;` 把这份文件挂成测试 crate 的子模块，而私有项
//! 只有**自己模块与后代**能看见，兄弟测试文件里的 `use common::seed;` 会撞 E0603。
//! 任务 7 的 `dedupe.rs` 与任务 8 的 `audit.rs` 是它的第二个、第三个消费者。
//! 第一行的 `allow(dead_code)`：每个测试 bin 各自编译一遍 `common`，这个 bin 用不到的那一个
//! 函数在它自己的 bin 里就是死码，而 `pub` 不挡 `dead_code` lint——Step 4 那条
//! `clippy -D warnings` 会把它升成错误。
use rc_contract::{normalize, parse};
use rc_ledger::insert_batch;
use rc_store::Root;
use serde_json::json;

/// 把 `n` 条合成 turn 真导入会话库：`parse` → `normalize` → `insert_batch`，与
/// `crates/rc-ledger/tests/ingest.rs` 同一条链。测试里裸写 SQL 会让 provenance 覆盖率
/// 这条判据测不到本阶段的写入面（守卫 11 的写入面就是这条链）。
///
/// 会话/设备/agent 三个值**故意与 `BatchCtx::synthetic()` 不同**（坑 P-29）：
/// 记忆行上的两列若还写成合成常量，`content_rows_carry_the_batch_context` 当场红。
pub fn seed(root: &mut Root, n: usize, from_ms: i64) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-batch-A",
            "device_id": "dev-batch-A",
            "source_agent": "agent-batch-A",
            "turns": (0..n).map(|i| json!({
                "role": if i % 2 == 0 { "human" } else { "agent" },
                "body": format!("合成第 {} 句话，讲的是项目进度", from_ms + i as i64),
                "created_at_ms": from_ms + i as i64,
            })).collect::<Vec<_>>()
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, from_ms + 100_000);
    let mut uids = Vec::new();
    for (i, a) in rep.accepted.iter().enumerate() {
        // E2：`select_batch` 的翻页轴是到达钟（`ingested_at_ms`）。这里的夹具让到达顺序
        // 与 created_at 的顺序一致（第 i 条在第 from_ms+i 毫秒到达），否则整批同毫秒
        // 落地会退化成 uid 字典序，`uids[i]` 这类"第 i 条"的断言读的就不再是到达序。
        insert_batch(root.transcript_mut(), &a.session, std::slice::from_ref(&a.turn),
                     from_ms + i as i64).unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), n, "夹具的条数必须真是 n，否则下面的断言读的是别的东西");
    uids
}

/// 与 `seed` 同一条导入链，但正文由调用方给：压缩摘要/自定义标记的用例靠它。
pub fn seed_bodies(root: &mut Root, bodies: &[&str], from_ms: i64) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-batch-A",
            "device_id": "dev-batch-A",
            "source_agent": "agent-batch-A",
            "turns": bodies.iter().enumerate().map(|(i, b)| json!({
                "role": if i % 2 == 0 { "human" } else { "agent" },
                "body": b,
                "created_at_ms": from_ms + i as i64,
            })).collect::<Vec<_>>()
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, from_ms + 100_000);
    let mut uids = Vec::new();
    for (i, a) in rep.accepted.iter().enumerate() {
        insert_batch(root.transcript_mut(), &a.session, std::slice::from_ref(&a.turn),
                     from_ms + i as i64).unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), bodies.len(), "夹具的条数必须真是 bodies.len()");
    uids
}

/// `n` 条 turn，**到达顺序与 created_at 顺序相反**：用来钉住“prompt 按 created_at、游标按 ingested”。
pub fn seed_created_reversed(root: &mut Root, n: usize, from_ms: i64) -> Vec<String> {    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-batch-A",
            "device_id": "dev-batch-A",
            "source_agent": "agent-batch-A",
            "turns": (0..n).map(|i| json!({
                "role": if i % 2 == 0 { "human" } else { "agent" },
                "body": format!("倒序第 {} 句，讲的是项目进度", i),
                "created_at_ms": from_ms + (n - 1 - i) as i64,
            })).collect::<Vec<_>>()
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, from_ms + 100_000);
    let mut uids = Vec::new();
    for (i, a) in rep.accepted.iter().enumerate() {
        insert_batch(root.transcript_mut(), &a.session, std::slice::from_ref(&a.turn),
                     from_ms + i as i64).unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), n);
    uids
}

/// `n` 条 turn、`created_at_ms` **全部相同**、正文各不相同（⇒ uid 各不相同、字典序可预测）。
/// 与 `seed` 共用同一对会话/设备值：它测的是 tie-break，不是 P-29 的归属对撞。
pub fn seed_same_ms(root: &mut Root, n: usize, ms: i64) -> Vec<String> {
    let doc = json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-batch-A",
            "device_id": "dev-batch-A",
            "source_agent": "agent-batch-A",
            "turns": (0..n).map(|i| json!({
                "role": if i % 2 == 0 { "human" } else { "agent" },
                "body": format!("同一毫秒的第 {} 句，讲的是项目进度", i),
                "created_at_ms": ms,
            })).collect::<Vec<_>>()
        }]
    });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, ms + 100_000);
    let mut uids = Vec::new();
    for a in &rep.accepted {
        insert_batch(root.transcript_mut(), &a.session, std::slice::from_ref(&a.turn), ms).unwrap();
        uids.push(a.turn.turn_uid.clone());
    }
    assert_eq!(uids.len(), n, "同毫秒夹具也必须真落下 n 行（正文互不相同 ⇒ uid 互不相同）");
    uids
}

/// 四条内容（实体 / 卡片 / 事件 / 断言各一）+ 零 worthless。出处全指向批内 uid。
/// 实体 kind 取抽取六词与 canonical `anchor` 词表的交集 `org`（旧夹具有的 `project` 不在 anchor CHECK 内）。
pub fn payload(uids: &[String]) -> String {
    json!({
        "entities": [{"kind":"org","title":"项目进度","aliases":["这个项目"],
                      "provenance":[uids[0]]}],
        "cards":    [{"kind":"lesson","title":"先看进度","body":"合成正文一",
                      "valid_to_ms":null,"provenance":[uids[1 % uids.len()]]}],
        "events":   [{"kind":"milestone","title":"合成事件","body":"合成正文二",
                      "valid_to_ms":null,"provenance":[uids[uids.len()-1]]}],
        "assertions":[{"kind":"state","title":"在赶进度","body":"合成正文三",
                       "valid_to_ms":null,"provenance":[uids[0], uids[uids.len()-1]]}],
        "worthless_turn_uids": []
    }).to_string()
}

/// 五键齐全、四条内容全空、但 worthless 逐条表态。这不是 ZeroOutput（§7 的那条分界）。
pub fn worthless_payload(uids: &[String]) -> String {
    json!({"entities":[],"cards":[],"events":[],"assertions":[],
           "worthless_turn_uids": uids}).to_string()
}

/// 五键齐全、**只有一条卡片**、其余三臂全空。审核的用例靠它把未审行数定成 1 ⇒
/// "每行三次调用"才是可数的；判同的 `no_candidates_means_no_llm_call_at_all` 用它测"没有邻居不开口"。
/// 正文与 `payload` 那四条不同词面，所以它不会在本不该有邻居的用例里长出邻居。
pub fn one_card_payload(uids: &[String]) -> String {
    serde_json::json!({"entities":[],
        "cards":[{"kind":"lesson","title":"唯一的一条","body":"没有邻居的正文",
                  "valid_to_ms":null,"provenance":[uids[0]]}],
        "events":[], "assertions":[], "worthless_turn_uids":[]}).to_string()
}

/// 测试夹具：把一张表里**已有的全部归属行**改写成给定会话的归属。
///
/// `put_entity`（旧夹具写入面）给锚盖的是合成 owner（`synthetic-agent`、private_agent），
/// 而候选池/程序闸从隐私修复起要求"锚对该会话 owner 可见"——真实链路里命名 pass 用会话
/// 归属建锚，夹具却用合成归属，两者必须在这里对齐。只 UPDATE `resource_owner`（不改行、
/// 不改表结构），所以"归属行数"类断言不受影响。
pub fn adopt_rows(
    root: &Root,
    mem_table: &str,
    owner_agent: &str,
    principal: Option<&str>,
    visibility: &str,
    scope_kind: &str,
    scope_key: &str,
) {
    root.memory()
        .conn()
        .execute(
            "UPDATE resource_owner
                SET owner_agent_id=?1, owner_principal_id=?2, visibility=?3,
                    scope_kind=?4, scope_key=?5
              WHERE mem_table=?6",
            rusqlite::params![owner_agent, principal, visibility, scope_kind, scope_key, mem_table],
        )
        .unwrap();
}

/// 直插一个锚 + 它的归属行（隐私/可见性用例的夹具：owner/visibility/scope 由调用方指定）。
///
/// 一份实现供 `pass_event` 与 `pass_fact` 两处共用：此前是复制粘贴的两份，参数化后又各改一遍
/// 只会让它们继续漂。每一格都显式进参数，**没有任何字段在函数体里写死**。
///
/// `owner_principal_id` 与 `device_id` 都是 `Option`：主人锚与"正文里那个人名锚"在归属行上
/// 的唯一区别就是后者带设备、前者不带（生产侧见 `writers_v4` 的两个 `ensure_*_anchor`）。
/// 夹具若只会造"带设备"的行，说话人那一格的主人臂在用例里就永远走不到。
#[allow(clippy::too_many_arguments)]
pub fn insert_anchor_full(
    m: &rc_store::MemoryHandle,
    id: &str,
    canonical: &str,
    kind: &str,
    reality_status: &str,
    status: &str,
    owner_agent: &str,
    owner_principal_id: Option<&str>,
    device_id: Option<&str>,
    visibility: &str,
    scope_kind: &str,
    scope_key: &str,
    created_at_ms: i64,
) {
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 1)",
            rusqlite::params![
                id, kind, canonical, reality_status, scope_kind, scope_key, status, created_at_ms
            ],
        )
        .unwrap();
    m.conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, ?2, ?3, ?3, ?4, 'default', ?5, ?6, ?3, ?7, ?8)",
            rusqlite::params![
                id,
                owner_principal_id,
                owner_agent,
                visibility,
                scope_kind,
                scope_key,
                device_id,
                created_at_ms
            ],
        )
        .unwrap();
}

/// canonical 两张表（`anchor` / `card`）的行数一次读回来（`rows_written` 的独立读数；断行数不断布尔，§10 明令）。
/// 旧四表（entity/card/event/assertion）的物理行全落这两张表，求和口径不变。
pub fn content_rows(root: &Root) -> i64 {
    let one = |t: &str| root.memory().conn()
        .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap();
    one("anchor") + one("card")
}
