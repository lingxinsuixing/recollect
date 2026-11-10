//! B2b-3：连通性读数带四个离线格（判据 2㊹）。
//! B4a：再带 detached / 确认时刻 / 自备份三格（判据 3⑭）。
//!
//! 造法把三段真实形状串起来：状态文件（`node::state::save` 的真序列化）+ 一个带标记行的
//! 暂存层（`node::staging::prepare` 建层，标记行照生产者的表形插入）+ 一次 `read_connectivity`。
//! "这台机器不是节点"那一支（没有状态文件 ⇒ `None`）同时钉在这里：`None` 不是零值读数。

use node::state::{self, CurrentManifest, SelfDistill, SyncState};
use rc_store::Root;

#[test]
fn the_offline_readings_travel_with_the_connectivity() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("replica")).unwrap();

    // 底座副本：一份拷过来的记忆库（照 `offline_cycle.rs` 的夹具，WAL 先落回主文件再拷）。
    {
        let centre = tempfile::tempdir().unwrap();
        let root = Root::open(centre.path()).unwrap();
        drop(root);
        std::fs::copy(centre.path().join("memory.db"), data.join("replica/snapshot-1-1.db"))
            .unwrap();
    }
    let manifest = CurrentManifest { epoch: 1, seq: 1, sha256: "0".repeat(64), created_at_ms: 1 };
    node::staging::prepare(&data, &manifest, 1).unwrap();
    // 一行行标。键形与生产者（`selfdistill` 的差集）一致：`表:句柄`。
    // 写柄走 `AgentRoot::open_replica_rw`（`open_replica` 是只读的读面柄）。
    {
        let root = rc_store::AgentRoot::open_replica_rw(&data, &node::staging::path(&data)).unwrap();
        root.memory()
            .conn()
            .execute(
                "INSERT INTO staging_new_row(id, table_name) VALUES('card:deadbeef', 'card')",
                [],
            )
            .unwrap();
    }

    let st = SyncState {
        self_distill: SelfDistill::OfflineDistilling,
        offline_elapsed_ms: 86_400_000,
        llm_key: Some("unlocked".into()),
        // R9：读数面要能说"我钉的是谁 / 现在还有没有错 / 重发现问过几次"。
        paired_hub_pub: "hub-pub-b64".into(),
        paired_tls_fp: Some("a".repeat(64)),
        last_error: "tls_failed".into(),
        last_error_at_ms: 1_700_000_000_123,
        rediscover_ok: 2,
        rediscover_miss: 3,
        ..SyncState::default()
    };
    state::save(&state::state_path(&data), &st).unwrap();

    let c = rc_diag::connectivity::read_connectivity(&data).expect("状态文件在 ⇒ 必须有读数");
    assert_eq!(c.self_distill, "offline_distilling", "状态机的原样串，不是另起一套名字");
    assert_eq!(c.offline_elapsed_ms, 86_400_000);
    assert_eq!(
        c.staging_rows,
        node::staging::new_rows(&data).len(),
        "读数与暂存层的真行数必须一致"
    );
    assert!(c.staging_rows >= 1, "这一层里有一行标记：{}", c.staging_rows);
    assert_eq!(c.llm_key.as_deref(), Some("unlocked"));
    assert_eq!(c.hub_fp, st.hub_fp(), "短名由状态同一份公式算");
    assert_eq!(c.hub_fp.len(), 64);
    assert_eq!(c.tls_fp, "a".repeat(64));
    assert_eq!(c.transport, "tls", "有指纹就是一档 TLS");
    assert_eq!(c.last_error, "tls_failed");
    assert_eq!(c.last_error_at_ms, 1_700_000_000_123);
    assert_eq!((c.rediscover_ok, c.rediscover_miss), (2, 3));

    // `None` ⇒ doctor 那一格不出现（照现有形状省略，不是打出一个 null）。
    let mut bare = c.clone();
    bare.llm_key = None;
    let v = serde_json::to_value(&bare).unwrap();
    assert!(v.get("llm_key").is_none(), "None 的格必须整格消失：{v}");

    // 没有状态文件 ⇒ `None`（"这台机器不是节点"与"节点连不上"是两种后果）。
    let empty = tempfile::tempdir().unwrap();
    assert!(rc_diag::connectivity::read_connectivity(empty.path()).is_none());
}

#[test]
fn connectivity_carries_the_detached_readings() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let st = SyncState {
        self_distill: SelfDistill::DetachedDistilling,
        reconcile_confirmed_at_ms: 1_700_000_000_000,
        dr_backup_last_at_ms: 1_700_000_100_000,
        dr_backup_count: 7,
        local_turn_highwater_ms: 1_700_000_050_000,
        ..SyncState::default()
    };
    state::save(&state::state_path(&data), &st).unwrap();

    let c = rc_diag::connectivity::read_connectivity(&data).expect("状态文件在 ⇒ 必须有读数");
    assert!(c.detached, "detached_distilling 必须让这一格为 true");
    assert_eq!(c.self_distill, "detached_distilling");
    assert_eq!(c.reconcile_confirmed_at_ms, 1_700_000_000_000);
    assert_eq!(c.dr_backup_last_at_ms, 1_700_000_100_000);
    assert_eq!(c.dr_backup_count, 7);
    assert_eq!(c.local_turn_highwater_ms, 1_700_000_050_000);
}