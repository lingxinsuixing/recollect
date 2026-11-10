//! B2a-3：节点读箱的两条——值读得到、五类失败分得开。
use node::llm_key::{read_llm_key, LlmKeyFault};
use rc_secret::SecretsBox;
use std::path::Path;

const PASS: &[u8] = b"node-test-passphrase-123";

/// 节点 data-dir 里的箱；`slot` 为 `None` 时箱是空的（缺格那一支）。
fn seed_box(data: &Path, slot: Option<(&str, &str)>) {
    std::fs::create_dir_all(data).unwrap();
    {
        let mut b = SecretsBox::create(&data.join("secrets.enc"), PASS).unwrap();
        if let Some((slot, value)) = slot {
            b.put(slot, value).unwrap();
        }
    }
    std::fs::write(data.join("secrets.pass"), b"node-test-passphrase-123\n").unwrap();
}

#[test]
fn a_key_is_read_from_the_nodes_own_box() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed_box(&data, Some(("openai", "sk-node-test")));
    assert_eq!(
        read_llm_key(&data, "secrets:openai").unwrap().as_str(),
        "sk-node-test",
        "节点自己 data-dir 里的箱读不出值 ⇒ B2b 的离线自蒸没有钥匙"
    );
}

#[test]
fn bad_ref_and_the_four_box_failures_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    // `secret_ref` 没有 `:`：值的位置不合法，开箱之前就拒。
    assert_eq!(read_llm_key(&data, "noslot").unwrap_err(), LlmKeyFault::BadRef);
    // 空目录：箱还没建。
    assert_eq!(
        read_llm_key(&data, "secrets:openai").unwrap_err(),
        LlmKeyFault::BoxMissing
    );
    // 只有箱：口令文件没落。
    {
        SecretsBox::create(&data.join("secrets.enc"), PASS).unwrap();
    }
    assert_eq!(
        read_llm_key(&data, "secrets:openai").unwrap_err(),
        LlmKeyFault::PassMissing
    );
    // 箱与口令都在，格缺。
    std::fs::write(data.join("secrets.pass"), b"node-test-passphrase-123\n").unwrap();
    assert_eq!(
        read_llm_key(&data, "secrets:openai").unwrap_err(),
        LlmKeyFault::SlotMissing
    );
    // 箱在、口令文件在，但口令是错的。
    std::fs::write(data.join("secrets.pass"), b"not-the-passphrase\n").unwrap();
    assert_eq!(
        read_llm_key(&data, "secrets:openai").unwrap_err(),
        LlmKeyFault::BoxOpen
    );
}
