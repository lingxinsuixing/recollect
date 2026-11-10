//! 配对码与 token 的展示面：两次生成不同、哈希不回显原码。
//! nonce 的三条用例住在 `tests/nonce.rs`（判据 69⑧ 点名的是那个测试目标里的 `nonce_is_single_use`）。
use rc_net::pair::{new_pair_code, new_token};

#[test]
fn generated_codes_differ_and_hash_hides_the_code() {
    let (a, ha) = new_pair_code().unwrap();
    let (b, hb) = new_pair_code().unwrap();
    assert_ne!(a, b, "两枚码撞了：随机源坏了");
    assert_ne!(ha, hb);
    assert!(!ha.contains(&a), "哈希里出现原码");
    assert_eq!(ha.len(), 64);
    let (t1, _) = new_token().unwrap();
    let (t2, _) = new_token().unwrap();
    assert_ne!(t1, t2);
}
