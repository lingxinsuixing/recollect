//! W20（P3-2）：休眠的临时锚读数（`issues::dormant_anchor_count`）。
//!
//! 六条用例各钉一支条件：门槛外的 provisional 无引用 ⇒ 1；有 mention ⇒ 0；
//! 有 provenance ⇒ 0；有环境属性（W20b）⇒ 0 且同一夹具里零引用的仍算 1；`active` ⇒ 0；
//! 门槛内 ⇒ 0；空库 ⇒ 0。外加一条已处置（退场）⇒ 0，那一支守的是 `reader::alive_where`
//! 的三支。
use rc_memory::issues::dormant_anchor_count;
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;
const DAY: i64 = 24 * 3_600_000;

fn fresh() -> (tempfile::TempDir, Root) {
    let d = tempfile::tempdir().unwrap();
    let r = Root::open(d.path()).unwrap();
    (d, r)
}

fn anchor(m: &MemoryHandle, id: &str, status: &str, created_at_ms: i64) {
    m.conn()
        .execute(
            "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,status,
                                created_at_ms,updated_at_ms,norm_version)
             VALUES(?1,'person',?2,'real','project','proj-x',?3,?4,?4,1)",
            (id, format!("锚-{id}"), status, created_at_ms),
        )
        .unwrap();
}

/// 一条 mention：这是"还被提过"的唯一凭据。
fn mention(m: &MemoryHandle, anchor_id: &str) {
    m.conn()
        .execute(
            "INSERT INTO mention(id,anchor_id,turn_uid,surface,resolution,source_agent,device_id,
                                 created_at_ms,norm_version)
             VALUES(?1,?2,?3,'某个说法','resolved','qoder','dev',?4,1)",
            (format!("m-{anchor_id}"), anchor_id, format!("t-{anchor_id}"), NOW),
        )
        .unwrap();
}

/// 一条锚出处：`mem_table='anchor'` 那一格是这一支的主语。
fn provenance(m: &MemoryHandle, anchor_id: &str) {
    m.conn()
        .execute(
            "INSERT INTO provenance(mem_table,mem_id,turn_uid,anchor_role)
             VALUES('anchor',?1,?2,'human')",
            (anchor_id, format!("t-prov-{anchor_id}")),
        )
        .unwrap();
}

/// 一条环境属性：W14c 那一类锚可以只有这一格引用（没有正文出处、没人再提名）。
fn attribute(m: &MemoryHandle, anchor_id: &str) {
    m.conn()
        .execute(
            "INSERT INTO attribute(id,anchor_id,key,value_json,value_text,value_type,mode,text,
                                   scope_kind,scope_key,source_agent,device_id,created_at_ms,
                                   norm_version)
             VALUES(?1,?2,'location','\"办公室\"','办公室','string','stated','所在环境：办公室',
                    'project','proj-x','qoder','dev',?3,1)",
            (format!("at-{anchor_id}"), anchor_id, NOW),
        )
        .unwrap();
}

fn retire(m: &MemoryHandle, anchor_id: &str) {
    m.conn()
        .execute(
            "INSERT INTO retirement(mem_table,mem_id,reason,created_at_ms,resolved_at_ms)
             VALUES('anchor',?1,'test',?2,NULL)",
            (anchor_id, NOW),
        )
        .unwrap();
}

#[test]
fn an_old_provisional_anchor_with_no_references_is_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    anchor(m, "old", "provisional", NOW - 40 * DAY);
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 1,
               "门槛外、无 mention、无出处的临时锚就是要报的那一格");
}

#[test]
fn a_mentioned_anchor_is_not_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    anchor(m, "talked", "provisional", NOW - 40 * DAY);
    mention(m, "talked");
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 0,
               "还被提过的锚不是休眠锚");
}

#[test]
fn an_anchor_with_provenance_is_not_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    anchor(m, "sourced", "provisional", NOW - 40 * DAY);
    provenance(m, "sourced");
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 0,
               "有锚出处的行不是休眠锚（这一支的牙：去掉它这条当场红）");
}

/// W20b：只有环境属性的锚**不是**休眠锚——它没有正文出处、也没人再把它当名字提，
/// 但 `attribute` 那一行就是"还在被引用"。同一夹具里那条零引用的仍算 1：
/// 两个数一起看，才说明排除的是属性那一支而不是把整格数丢了。
#[test]
fn an_anchor_with_environment_attributes_is_not_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    anchor(m, "env", "provisional", NOW - 40 * DAY);
    attribute(m, "env");
    anchor(m, "bare", "provisional", NOW - 40 * DAY);
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 1,
               "有环境属性的锚不算休眠，零引用的那条仍算 1（这一支的牙：删掉排除条件 ⇒ 这条红）");
}

#[test]
fn an_active_anchor_is_not_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    anchor(m, "confirmed", "active", NOW - 40 * DAY);
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 0,
               "`active` 是确认过的名字面，不在这格的主语里");
}

#[test]
fn a_fresh_provisional_anchor_is_not_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    // 29 天：门槛是 30 天，所以这一格故意**不**引用那个常量——门槛被改小（甚至改成 0）时，
    // 这条必须红；拿常量当夹具的话，夹具会跟着门槛一起动，边界就永远测不到。
    anchor(m, "fresh", "provisional", NOW - 29 * DAY);
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 0,
               "门槛内的临时锚本来就还没被引用过，报它只是噪声");
}

#[test]
fn a_retired_provisional_anchor_is_not_dormant() {
    let (_d, root) = fresh();
    let m = root.memory();
    anchor(m, "gone", "provisional", NOW - 40 * DAY);
    retire(m, "gone");
    assert_eq!(dormant_anchor_count(m, NOW).unwrap(), 0,
               "已处置（退场账未解除）的行不算休眠：`alive_where` 那一支的牙");
}

#[test]
fn an_empty_store_reads_zero() {
    let (_d, root) = fresh();
    assert_eq!(dormant_anchor_count(root.memory(), NOW).unwrap(), 0,
               "0 是「没有休眠锚」这个读数，不是读失败");
}
