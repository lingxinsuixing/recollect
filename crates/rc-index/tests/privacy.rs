use rc_index::rebuild::rebuild;
use rc_index::IndexError;
use rc_memory::synth;
use rc_store::MemoryHandle;

fn memory_handle() -> (tempfile::TempDir, MemoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    (dir, root.into_memory())
}

#[test]
fn 影子表只有三列且没有分数列() {
    let (_d, m) = memory_handle();
    rebuild(&m, 1).unwrap();
    let cols: Vec<String> = m
        .conn()
        .prepare("SELECT name FROM pragma_table_info('rc_shadow') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(cols, ["key", "tokens", "row_len"]);
    for banned in ["score", "rank", "bm25", "importance", "body", "title"] {
        assert!(!cols.iter().any(|c| c == banned), "影子表出现越界列 {banned}");
    }
}

#[test]
fn 影子表存的是句柄与词面_不含正文原串() {
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rebuild(&m, now).unwrap();
    let doc: String = m
        .conn()
        .query_row("SELECT group_concat(tokens, '|') FROM rc_shadow", [], |r| r.get(0))
        .unwrap();
    // 合成夹具的正文是一句通顺的话；影子表里只许出现被切开后的词。
    // 这一串逐字取自 `rc-memory/src/synth.rs` 的 `tie_body('甲')`（夹具是唯一真相源，测试跟着它走）。
    let whole_body = "合成正文等长一句甲";
    assert!(!doc.contains(whole_body), "影子表存了未分词的正文原串");
    let keys: String = m
        .conn()
        .query_row("SELECT group_concat(key, '|') FROM rc_shadow", [], |r| r.get(0))
        .unwrap();
    assert!(keys.contains(':'), "key 应是 \"<table>:<id>\" 形状");
}

#[test]
fn 重建报告零正文() {
    // 守卫 12：运维面输出只许有计数与版本。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    let json = serde_json::to_string(&rebuild(&m, now).unwrap()).unwrap();
    assert!(!json.contains("同分"), "报告里出现正文：{json}");
    assert!(!json.contains(':') || json.contains("\"norm_version\":1"), "报告形状异常：{json}");
}

#[test]
fn sql错误渲染必须过消毒_不带磁盘路径() {
    // 裁定（守卫 12）：`IndexError::Sql(#[from] rusqlite::Error)` 是一条没消毒的出口。
    // `rusqlite::Error::InvalidPath(p)` 的 Display 就是磁盘路径本身——`hub index rebuild` 失败面
    // 会把这条 IndexError 打给操作员，路径就漏了。From<rusqlite::Error> 必须在构造点削平它，
    // 与 rc-memory 的 sqlite_text 同规则（首行 / 120 字符上界 / 路径形状→占位符）。
    let raw = rusqlite::Error::InvalidPath(std::path::PathBuf::from(r"C:\Users\synthetic\secret\memory.db"));
    // 先确认这条夹具错误【本身】带磁盘路径——否则这条守卫从没红过，等于没守卫。
    let raw_text = raw.to_string();
    assert!(
        raw_text.contains('\\') && raw_text.to_lowercase().contains(".db"),
        "夹具的 rusqlite 错误本身不含磁盘路径，消毒无从证明：{raw_text}"
    );

    let sanitized = IndexError::from(raw).to_string();
    // 消毒后的渲染里既没有反斜杠、也没有正斜杠、没有盘符冒号后的路径片段、没有库文件名、没有目录名。
    assert!(!sanitized.contains('\\'), "消毒后仍带反斜杠路径：{sanitized}");
    assert!(!sanitized.contains('/'), "消毒后仍带正斜杠路径：{sanitized}");
    assert!(!sanitized.contains("C:"), "消毒后仍带盘符：{sanitized}");
    assert!(!sanitized.to_lowercase().contains(".db"), "消毒后仍带库文件名：{sanitized}");
    assert!(!sanitized.contains("secret"), "消毒后仍带目录名：{sanitized}");
}
