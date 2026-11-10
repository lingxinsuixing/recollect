//! H4：交接条**不是**记忆。写进去之后，它不许出现在记忆侧任何一处落盘面。
//!
//! 判据是"这个词在盘上根本不存在"，不是"接口没返回它"：
//! 1. `transcript.db` / `memory.db` / `ledger.db` 及其 WAL —— 交接正文一个字节都不许有；
//! 2. `core.db` 自己也不许有明文 —— 载荷是 AEAD，marker 只以密文形态存在；
//! 3. `DATA_FILES` 这张清单本身不含 `core.db`，所以快照/复制/备份天然看不到它。
//!
//! 四个库文件**必须都真实读到**（[`REQUIRED_DBS`]）：读不到文件不是"文件里没有"的证据。
//!
//! 用独特 marker 而不是抽表：任何"新增了一张表又忘了排除"的坏法都会当场红，
//! 不依赖测试作者事先知道那张表叫什么。

use rc_core::CoreStore;
use std::fs;

/// 独特且不太可能自然出现的串；用中英混排，确保不会被别的夹具撞上。
const MARKER: &str = "交接隔离-marker-4e1f-Q7";

/// 落盘面的四个库文件：记忆侧三件 + 交接条的家 `core.db`。
/// **缺一个就是失败**：读不到文件从来不是"文件里没有这个词"的证据，
/// 一条 `if let Ok` 会让"新库根本没建出来"这件事装成通过。
const REQUIRED_DBS: [&str; 4] = ["transcript.db", "memory.db", "ledger.db", "core.db"];

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_handoff_note_never_reaches_the_memory_side_or_plain_text_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    // 三个库都要真实存在：与生产同一个入口（`Root::open` 建 transcript/memory/ledger）。
    let _root = rc_store::Root::open(dir.path()).unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    store.register_agent("agent-a", 1_000).unwrap();

    let written = store
        .handoff_write("agent-a", "continue", MARKER, None, "dev-test", 1_000)
        .unwrap();
    // 先证明它真的写进去了（否则下面的"盘上没有"是空断言）。
    let listed = store.handoff_list("agent-a", 1_000).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].text, MARKER);
    assert_eq!(listed[0].id, written.id);

    let marker = MARKER.as_bytes();
    // 四个库文件逐个读：主库文件**必须存在**（缺了就是失败），`-wal` 只在还有未 checkpoint 的页时才在。
    for name in REQUIRED_DBS {
        for suffix in ["", "-wal"] {
            let path = dir.path().join(format!("{name}{suffix}"));
            let bytes = if suffix.is_empty() {
                fs::read(&path)
                    .unwrap_or_else(|e| panic!("{name} 必须存在（读不到就不是“盘上没有这个词”）：{e}"))
            } else {
                match fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                }
            };
            assert!(
                !contains(&bytes, marker),
                "交接正文落进了 {name}{suffix}：记忆侧三件不该有它，core.db 里也只许是密文"
            );
        }
    }

    // 快照/复制/备份的清单只有这三件，core.db 不在里面（H1 的"不进快照"由这张清单兑现）。
    assert!(
        !rc_store::DATA_FILES.contains(&"core.db"),
        "DATA_FILES 里出现了 core.db：交接条会被快照/复制带走"
    );
    assert_eq!(REQUIRED_DBS.len(), rc_store::DATA_FILES.len() + 1, "清单漂了");
}
