use rc_index::rebuild::{index_status, rebuild, search, IndexError};
use rc_memory::keys::MemTable;
use rc_memory::{reader, synth};
use rc_store::MemoryHandle;

// tests/*.rs 互不引用（任务 0 起的纪律），所以这个四行构造器在本文件里就地写一遍。
fn memory_handle() -> (tempfile::TempDir, MemoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    (dir, root.into_memory())
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn 重建是幂等的() {
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    let a = rebuild(&m, now).unwrap();
    let b = rebuild(&m, now + 1).unwrap();
    assert_eq!(a.rows_indexed, b.rows_indexed);
    // 幂等的可观测面在影子表自己的行数，不在 `rows_indexed`：后者是记忆行的计数（`rows.len()`），
    // 少了 `DELETE FROM rc_shadow` 它两次都一样，翻倍的是影子。没有这一句，那条 DELETE 就是没守卫。
    assert_eq!(count(&m, "rc_shadow"), a.rows_indexed as i64, "两次重建后影子行数翻倍");
    let hits1 = search(&m, "同分合成词甲").unwrap();
    let hits2 = search(&m, "同分合成词甲").unwrap();
    assert_eq!(
        hits1.iter().map(|h| h.key.key_string()).collect::<Vec<_>>(),
        hits2.iter().map(|h| h.key.key_string()).collect::<Vec<_>>()
    );
    // 裁定 R20-1：任务 5 的两个输入（`SearchHit.tokens` / `SearchHit.row_len`）必须有零值断言。
    // 从前把 `tokens` 换成"查询时重新切一遍"（正是 `rebuild.rs` 排序注释禁止的口径漂移）、
    // 或把 `row_len` 写成 `0`，135 条全绿。现在直接拿影子表本体对拍，两个方向都跑不掉。
    assert!(!hits1.is_empty(), "夹具没命中，下面的对拍会恒真");
    for hit in &hits1 {
        let (doc, len): (String, i64) = m
            .conn()
            .query_row(
                "SELECT tokens, row_len FROM rc_shadow WHERE key = ?1",
                rusqlite::params![hit.key.key_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let want: Vec<String> = doc.split(' ').filter(|s| !s.is_empty()).map(String::from).collect();
        assert_eq!(hit.tokens, want, "tokens 不是影子表自己的口径：key={}", hit.key.key_string());
        assert_eq!(hit.row_len, len as usize, "row_len 不等于 rc_shadow.row_len：key={}", hit.key.key_string());
        // 只断 `tokens` 非空：`row_len >= 1` 由 `rebuild.rs:216` 的 `len.max(1)` 构造性恒真，
        // 留着是一条永假告不了的断言；真正的牙在上面两句影子表对拍里。
        assert!(!hit.tokens.is_empty(), "命中字段被写成了零值：{hit:?}");
    }
}

#[test]
fn 重建不改任何一行记忆() {
    // 守卫 11 的反方向：索引层（D）不许伸进别人的列，哪怕是"顺手刷个时间戳"。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    let keys = synth::seed(&m, now).unwrap();
    // canonical 之后可指认的表只剩 `anchor` 与 `card`：旧 `entity` 落 `anchor`，
    // 旧 `event`/`assertion`/`card` 三者合并落 `card`，旧 `entity_alias` 落 `alias`。
    // 清单仍是六张互不相同的表，覆盖播种写到的每一个落点。
    const MEMORY_COUNTS: [&str; 6] =
        ["anchor", "card", "provenance", "attestation", "retirement", "alias"];
    let before: Vec<u64> = MEMORY_COUNTS.iter().map(|t| count(&m, t) as u64).collect();
    let cooldown_before: Vec<Option<i64>> = reader::facts(&m)
        .unwrap()
        .into_iter()
        .map(|r| r.cooldown_until_ms)
        .collect();
    rebuild(&m, now + 999).unwrap();
    let after: Vec<u64> = before
        .iter()
        .map(|_| 0u64)
        .collect::<Vec<_>>()
        .into_iter()
        .enumerate()
        .map(|(i, _)| count(&m, MEMORY_COUNTS[i]) as u64)
        .collect();
    assert_eq!(before, after, "重建增删了记忆行");
    let cooldown_after: Vec<Option<i64>> = reader::facts(&m)
        .unwrap()
        .into_iter()
        .map(|r| r.cooldown_until_ms)
        .collect();
    assert_eq!(cooldown_before, cooldown_after, "重建写了 P 的冷却列");
    // 且重建后仍可检索：这一句把"没动数据"与"数据还在服务里"分开断。
    assert!(!search(&m, "同分合成词甲").unwrap().is_empty());
    assert!(!keys.is_empty());
}

#[test]
fn 只读句柄拒绝被重建() {
    let (_d, m) = memory_handle();
    synth::seed(&m, 1_700_000_000_000).unwrap();
    let dir = _d.path().to_path_buf();
    // 副本形状只能由 `open_replica` 给出：`AgentRoot::open` 给的是可写柄，
    // 用它测"只读拒绝"会测到一个假绿（断言恒真）。副本文件就是刚建好的 memory.db。
    let agent = rc_store::AgentRoot::open_replica(&dir, &dir.join("memory.db")).unwrap();
    let ro = agent.memory();
    assert!(ro.is_read_only());
    let err = rebuild(ro, 1).expect_err("副本句柄必须拒写");
    assert!(matches!(err, IndexError::ReadOnly(_) | IndexError::Store(rc_store::StoreError::ReadOnly(_))));
}

#[test]
fn 索引层整层消失后_recall_仍可检索_但_probe_响亮报错() {
    // 守卫 3 的双源形状：同一份查询实现不许把"索引没了"读成"库是空的"。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    m.conn().execute("DROP TABLE rc_shadow", []).unwrap();
    m.conn()
        .execute("DROP TABLE rc_index_marker", [])
        .unwrap();
    let err = search(&m, "同分合成词甲").expect_err("缺表必须报错不许返回空");
    assert!(matches!(
        err,
        IndexError::Store(_) | IndexError::Sql(_) | IndexError::NotBuilt
    ));
    // 事实行还在，直查那条路（任务 7 的 recall）不受影响。计数按 `synth::seed` 的 canonical 形状钉死：
    // 三张同分卡 + 一条旧断言 + 一条旧事件 = 5 行 `card`（`event`/`assertion` 都折进了 `card`），
    // 两个实体 = 2 行 `anchor`。换成分母不同的夹具时这里当场红，而不是被一个宽松的 `>=` 吞掉。
    assert_eq!(count(&m, "card"), 5);
    assert_eq!(count(&m, "anchor"), 2);
    assert!(reader::facts(&m).unwrap().iter().any(|r| r.title.contains("同分合成词甲")));
}

#[test]
fn 没建过索引不是空库() {
    let (_d, m) = memory_handle();
    synth::seed(&m, 1_700_000_000_000).unwrap();
    let err = search(&m, "同分合成词甲").expect_err("未建索引必须报 NotBuilt");
    assert!(matches!(err, IndexError::NotBuilt));
    assert_eq!(index_status(&m).unwrap(), None);
}

#[test]
fn 逐行读数坏掉必须报错_不许伪装成没有记忆() {
    // 裁定 R18-1（证伪 #1 的载体）：从前 `search` 里那一行 `.filter_map(|x| x.ok())` 把逐行
    // step 期的每一个错误折进 `Ok(vec![])` ⇒ 本 crate 唯一的读数面对"索引层坏了"回答"没有记忆"。
    // 夹具走真实形状：`row_len` 被改成 TEXT（被人动过的影子层），每一行的 `get::<i64>(2)` 当场失败。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    assert!(!search(&m, "同分合成词甲").unwrap().is_empty(), "夹具本身要能命中，否则下面的断言恒真");
    let changed = m.conn().execute("UPDATE rc_shadow SET row_len = '坏掉的列'", []).unwrap();
    assert!(changed > 0, "夹具没能把 row_len 改成非 INTEGER，这条守卫从没红过");
    let err = search(&m, "同分合成词甲").expect_err("step 期失败必须报错，不许 Ok(空)");
    assert!(matches!(err, IndexError::LayerUnreadable), "错误面贴错了：{err:?}");
    // 守卫 12：这一支**不带载荷**——step 期错误多半来自 FTS5，原串会回显用户查询片段。
    assert_eq!(err.to_string(), "index layer unreadable");
}

#[test]
fn 影子行句柄不成形必须报错_不许静默丢候选() {
    // 裁定 R18-3（证伪 #2 的载体）：影子里每把句柄都是 `rebuild` 用 `key_string()` 写进去的，
    // 解析不回来就等价于"这一层被人动过"。从前这里 `.filter_map` 静默丢掉整条候选，
    // 注释引用的 `no_facts_for_key` 全仓不存在（grep 只命中那条评论自己）。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    let hits = search(&m, "同分合成词甲").unwrap();
    let key = hits[0].key.key_string();
    // 词面与长度照抄一把真句柄 ⇒ 这行**一定**被 MATCH 命中，只有 key 是不成形的。
    let (doc, len): (String, i64) = m
        .conn()
        .query_row(
            "SELECT tokens, row_len FROM rc_shadow WHERE key = ?1",
            rusqlite::params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    m.conn()
        .execute(
            "INSERT INTO rc_shadow(key, tokens, row_len) VALUES('bogus', ?1, ?2)",
            rusqlite::params![doc, len],
        )
        .unwrap();
    let err = search(&m, "同分合成词甲").expect_err("不成形的句柄必须报错，不许被丢掉");
    assert!(matches!(err, IndexError::LayerUnreadable), "错误面贴错了：{err:?}");
    assert_eq!(err.to_string(), "index layer unreadable");
}

#[test]
fn 前缀相似的外来表不许算进索引层清单() {
    // 裁定 R20-3（证伪 #3 的载体，编号以 `task-4-report.md` §2 为准）：`name LIKE 'rc_shadow_%'` 里的 `_` 是 LIKE 的**单字符通配符**，
    // 于是 `rc_shadowXtmp` 会被算进索引层，而 `rc-memory/tests/ownership.rs:49` 的 `starts_with`
    // 不认它 ⇒ "唯一一份清单"漂成两份。改 `GLOB` 后 `_` 是字面量。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    m.conn().execute("CREATE TABLE rc_shadowXtmp(x INTEGER)", []).unwrap();
    let listed = rc_index::shadow_tables(m.conn()).unwrap();
    assert!(!listed.contains(&"rc_shadowXtmp".to_string()), "外来表被算进索引层：{listed:?}");
    // 另一半：清单里真正的 fts5 伴生表必须还在，否则上一条会因为"清单空了"而恒真。
    assert!(listed.contains(&"rc_shadow_config".to_string()), "伴生表也没了，夹具走样：{listed:?}");
    // 这个 7 是刻意钉住的快照：SQLite 换实现时这里会假红，届时按 `shadow.rs` 的
    // `index_layer_names` 谓词现算的重量重新对表，而不是把断言改松。
    assert_eq!(listed.len(), 7, "索引层条目数不是 7：{listed:?}");
}

#[test]
fn 旧_ddl_形状读成没建过_重建把它建回规范形状() {
    // 裁定 R21-2（证伪 #4 的载体，编号以 `task-4-report.md` §2 为准）：R15 之后没有任何东西能造出错误形状的表，
    // `reconcile_shape` + `marker_is_canonical` 因此杀不死（整段删掉 135 条全绿）。
    // 钉住它的夹具就用**旧 DDL 的真实形状**——逐字取自 `crates/rc-store/src/ddl.rs`@330094e
    // 的 MEMORY_SQL（R15 之前）：两列的 `rc_shadow(key, toks)` + 一列的 `rc_index_marker(norm_version)`。
    // 这一轮冲突的来源正是这两张旧表，拿来当夹具名正言顺。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    m.conn()
        .execute_batch(
            "CREATE VIRTUAL TABLE rc_shadow USING fts5(key UNINDEXED, toks, tokenize='unicode61');
             CREATE TABLE rc_index_marker(norm_version INTEGER NOT NULL);
             INSERT INTO rc_index_marker(norm_version) VALUES(1);",
        )
        .unwrap();
    // ① 读数面：不是本 crate 写的形状 ⇒ "从没建过"，而不是让 "no such column" 冒充读数。
    assert_eq!(index_status(&m).unwrap(), None, "旧 marker 形状被当成已建索引的读数");
    // ② 检索面：响亮地报"没建索引"。
    let err = search(&m, "同分合成词甲").expect_err("旧形状不许被读成一次正常检索");
    assert!(matches!(err, IndexError::NotBuilt), "错误面贴错了：{err:?}");
    // ③ 重建面：`reconcile_shape` 删掉两张异物形状的表并按规范形状重建，且真的能用。
    let rep = rebuild(&m, now).unwrap();
    assert_eq!(rep.rows_indexed, reader::facts(&m).unwrap().len() as u64, "重建行数与事实行不符");
    assert_eq!(index_status(&m).unwrap().unwrap().rows_indexed, rep.rows_indexed);
    assert!(!search(&m, "同分合成词甲").unwrap().is_empty(), "重建后检索没恢复");
    // 形状对拍：旧的 `toks` 列必须已经换成规范三列（`tests/privacy.rs` 撞的是同一个常量）。
    let cols: Vec<String> = m
        .conn()
        .prepare("SELECT name FROM pragma_table_info('rc_shadow') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(cols, ["key", "tokens", "row_len"], "reconcile_shape 没把旧影子换成规范形状");
}

#[test]
fn marker_形状规范但值读不出_不许贴成没建过() {
    // 裁定 R18-4 的新分支必须真的被执行到（`src/rebuild.rs:166-174`）：`index_status` 报错时，
    // 只有**事实层面两张主表都不存在**（判据 `shadow::index_layer_missing`，`src/shadow.rs:119-121`）
    // 才贴 NotBuilt，否则原样 `Err(e)` 抛出——绝不靠错误文本或代码去猜。
    // 夹具：marker 的列名/列数一字未动（形状仍然 canonical），但 `rows_indexed` 被写成 TEXT。
    // INTEGER 亲和性对无法无损转换的文本是偏好不是强制（SQLite 文档：转换不了就按原样存），
    // 存进去的仍是 TEXT ⇒ `index_status` 的逐行 `get::<i64>` 当场失败 ⇒ `Err(Sql(_))`。
    // 对照组（两张主表都不存在 ⇒ NotBuilt）已由本档 `没建过索引不是空库` 钉死，不重复写。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    let changed = m
        .conn()
        .execute("UPDATE rc_index_marker SET rows_indexed = 'x'", [])
        .unwrap();
    assert!(changed > 0, "夹具没能把 rows_indexed 写成 TEXT，这条守卫从没红过");
    let err = search(&m, "同分合成词甲").expect_err("形状规范却读不出来，不许被重贴成没建过");
    assert!(matches!(err, IndexError::Sql(_)), "错误面贴错了：{err:?}");
}

#[test]
fn marker_记着它是哪一版尺子建出来的() {
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    let rep = rebuild(&m, now).unwrap();
    assert_eq!(rep.norm_version, rc_store::capability::NORM_VERSION);
    assert_eq!(rep.schema_version, rc_store::ddl::MEMORY_VERSION);
    assert_eq!(rep.rebuilt_at_ms, now);
    assert_eq!(index_status(&m).unwrap().unwrap().rows_indexed, rep.rows_indexed);
    // 尺子改了 ⇒ 旧影子必须拒用（这条是 NORM_VERSION +1 时"必须全量重建"的实际牙齿）。
    m.conn()
        .execute("UPDATE rc_index_marker SET norm_version = 900", [])
        .unwrap();
    let err = search(&m, "同分合成词甲").expect_err("尺子不符必须拒用");
    assert!(matches!(err, IndexError::NormMismatch { marker: 900, .. }));
}

#[test]
fn 空查询词不许伪装成无记忆() {
    let (_d, m) = memory_handle();
    synth::seed(&m, 1_700_000_000_000).unwrap();
    rebuild(&m, 1).unwrap();
    let err = search(&m, " ！！！ ").expect_err("无词查询要报错不许 Ok(空)");
    assert!(matches!(err, IndexError::NoQueryTokens));
}

#[test]
fn 句柄前缀不参与匹配() {
    // `key` 是 UNINDEXED 的判据：否则 "card:" 会成为一个可命中的词，检索结果被句柄形状污染。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    assert!(search(&m, "card").unwrap().is_empty(), "句柄前缀被索引了");
    assert!(search(&m, MemTable::Card.prefix()).unwrap().is_empty());
}

#[test]
fn 重建后归属过滤滤掉的是真实存在的索引层条目() {
    // 裁定 R15：MEMORY_SQL 不再建索引层 ⇒ `rc-memory/tests/ownership.rs` 的 `is_index_layer`
    // 前缀过滤从"刚 open 时无表可滤"的死代码，变成"重建之后确实滤到东西"的活代码。
    // 这条走 ownership 方向 ② 的那套形状，对拍的对象是真在 rc-index 这一侧建出来的表。
    let is_index_layer = |n: &str| n == "rc_shadow" || n.starts_with("rc_shadow_") || n == "rc_index_marker";
    let listed = |m: &MemoryHandle| -> Vec<String> {
        let mut st = m
            .conn()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
    };

    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();

    // 刚 open、还没建索引：一条索引层条目都不该有（R15 的核心形状，也是它让过滤变活的前提）。
    assert!(listed(&m).iter().all(|n| !is_index_layer(n)), "刚 open 的记忆库里出现了索引层条目");

    rebuild(&m, now).unwrap();
    let after = listed(&m);

    // 重建之后 fts5 影子表 rc_shadow 的伴生条目（_config/_content/_data/_docsize/_idx，共 5 张），
    // 再加 rc_shadow 本身与 rc_index_marker 一共 7 条被前缀过滤掉。
    let mut layer: Vec<String> = after.iter().filter(|n| is_index_layer(n)).cloned().collect();
    layer.sort();
    // 裁定 R20-2：清单只有一个家，那就**调公开函数对拍**，而不是在本档用第三种算法重算一遍。
    // 这一句同时钉住 `lib.rs` 的包装器与 `index_layer_names` 的两条谓词（`= ?2` 与 `GLOB ?3`）：
    // 包装器漏转发、谓词写错、`ORDER BY` 丢了——都红在这里。
    assert_eq!(
        rc_index::shadow_tables(m.conn()).unwrap(),
        layer,
        "公开的索引层清单与本用例现算的清单漂移（清单只有一个家）"
    );
    // 下面这份字面量清单是刻意钉住的快照（同 `前缀相似的外来表不许算进索引层清单` 里的 7 计数、
    // `src/rebuild.rs` 的 LAYER）：SQLite 换实现时这里会假红，届时按 `shadow.rs` 的
    // `index_layer_names` 谓词现算的重量重新对表。
    assert_eq!(
        layer,
        [
            "rc_index_marker",
            "rc_shadow",
            "rc_shadow_config",
            "rc_shadow_content",
            "rc_shadow_data",
            "rc_shadow_docsize",
            "rc_shadow_idx"
        ],
        "重建之后索引层的条目形状与 ownership 过滤的口径不一致（这条过滤滤的是真存在的东西）"
    );

    // 滤掉索引层后剩下的必须逐位等于归属表登记的每一张——这就是 ownership 方向 ② 的对拍。
    let real: Vec<String> = after.into_iter().filter(|n| !is_index_layer(n)).collect();
    assert_eq!(real, rc_memory::schema::MEMORY_TABLES.to_vec(), "过滤后的清单与归属表不一致");
}

#[test]
fn 重建与检索都不需要会话库存在() {
    // V6 的牙：出处把锚角色抄进行里，于是检索侧连 transcript.db 都不用打开。
    // 实现者若把角色查询挪回 transcript 侧，这一条会红在"打不开 memory.db 之外"的位置。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    std::fs::rename(_d.path().join("transcript.db"), _d.path().join("hidden.db")).unwrap();
    std::fs::rename(_d.path().join("ledger.db"), _d.path().join("hidden2.db")).unwrap();
    rebuild(&m, now).unwrap();
    assert!(!search(&m, "同分合成词甲").unwrap().is_empty());
    // R3 定下的口径：断"另两个库没被碰"必须断**文件仍然不存在**，不能只断"没报错"——
    // SQLite 在读写 open 时会当场把文件建出来，一次无错的 open 恰恰是要抓的那个动作。
    for name in ["transcript.db", "ledger.db"] {
        assert!(!_d.path().join(name).exists(), "索引路径碰了另两个库：{name} 被重新建出来了");
    }
    // 另一半：改名后的两个文件还在，所以上面那两条"不存在"不是因为整个目录空了（恒真的守卫等于没守卫）。
    for name in ["hidden.db", "hidden2.db"] {
        assert!(_d.path().join(name).exists(), "夹具走样：{name} 不见了，前一组断言成了恒真");
    }
}

#[test]
fn rc_index_的依赖里没有_ledger_llm_net() {
    // 守卫 6 在库层的投影。用 cargo tree 的元数据而不是读 Cargo.toml：
    // 后者能被一句 `dev-dependencies` 绕过，而这条断言要的是真实链接图。
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let ws = manifest.parent().unwrap().parent().unwrap();
    let out = std::process::Command::new(env!("CARGO"))
        .args(["tree", "-p", "rc-index", "--prefix", "none"])
        .current_dir(ws)
        .output()
        .expect("cargo tree");
    let s = String::from_utf8_lossy(&out.stdout);
    for banned in ["rc-ledger", "rc-llm", "rc-net", "rc-agent", "rc-search"] {
        assert!(!s.split_whitespace().any(|w| w == banned), "rc-index 依赖了 {banned}:\n{s}");
    }
    assert!(out.status.success(), "cargo tree 失败：{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn row_len_是词数不是唯一词数() {
    // 计划 Step 8 第 1 条点名要在本档补的断言。`row_doc` 若改成 `HashSet` 去重，`cjk.rs`
    // 那三条仍然全绿——去重发生在两侧共用的 `cut` 之后，词面形状一个字都没变，尺子断言抓不到它。
    // 唯一抓得住的地方就是 `row_len` 的语义：它是任务 5 长度归一的分母，数的是**出现次数**不是种类数。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed_padded(&m, now, 40).unwrap();
    rebuild(&m, now).unwrap();
    let (doc, row_len): (String, i64) = m
        .conn()
        .query_row("SELECT tokens, row_len FROM rc_shadow ORDER BY row_len DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let toks: Vec<&str> = doc.split(' ').filter(|s| !s.is_empty()).collect();
    assert_eq!(toks.len() as i64, row_len, "row_len 必须等于影子词串的词数：doc={doc} row_len={row_len}");
    let kinds = toks.iter().copied().collect::<std::collections::HashSet<_>>().len() as i64;
    assert!(row_len >= 40, "正文垫进去的 40 个「记录」都该进分母，实际 row_len={row_len}");
    assert!(row_len > kinds, "row_len 等于唯一词数（{row_len}）⇒ 分词侧被去重，长行在长度归一里看起来变短");
}

#[test]
fn match_语法字符只是词面() {
    // 计划 Step 8 第 3 条点名的断言。`match_string` 给每个词面套双引号，`+`/`*`/`-`/`NEAR`
    // 就只是词面；去掉引号 ⇒ `c++` 里的 `+` 变成 FTS5 语法，一次正常查询红成 `IndexError::Sql`，
    // "查询词可以长成任何样子"这条判据当场失效（本档"一开始就该红"的两条之一）。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    // jieba 把 `C++` 当一整块切，词面 `c++` 带着 FTS5 认得的符号进来。
    let no_hit = search(&m, "C++ 部署").expect("带语法字符的查询必须能查，不许红成 Sql");
    assert!(no_hit.is_empty(), "不认识的词就该不命中，但必须是 Ok：{no_hit:?}");
    // 同一串里混着认识的词时，语法字符不许把整批候选带走——这才叫"只是词面"。
    let hit = search(&m, "C++ 同分合成词甲").expect("语法字符不许让整条查询报错");
    assert!(!hit.is_empty(), "语法字符把匹配本身也弄坏了：{hit:?}");
}
