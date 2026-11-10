//! 节点自己的读箱：`secret_ref`（`箱:格`）→ 值。
//!
//! **只给 B2b 的离线自蒸用**：`sync` / `pair` / `status` 一行都不碰这个模块（B2a-3 的测试
//! 证明这一点）。这一层只做两件事——把 `secret_ref` 的形状校验交给
//! `rc_llm_cfg::keys::check_secret_ref`（不新写第二套），把读箱交给
//! `rc_secret::read_secret`（不复制文件格式）。
//!
//! 不变式与箱同源：值只经过不留下。返回 `Zeroizing<String>`，调用方只交给执行面；
//! 错误面是封闭枚举，不带路径、不带值、不带 slot 名。

use std::path::Path;
use zeroize::Zeroizing;

/// 读箱失败的封闭分类。`BadRef` 是"值的位置不合法"（配置事故），其余五类是读箱事故。
#[derive(Debug, PartialEq, Eq)]
pub enum LlmKeyFault {
    BadRef,
    BadBox,
    BoxMissing,
    PassMissing,
    BoxOpen,
    SlotMissing,
}

/// 读一格。`BadRef`（`check_secret_ref` 失败）在开箱之前；其余映射见 [`map_fault`]。
pub fn read_llm_key(data_dir: &Path, secret_ref: &str) -> Result<Zeroizing<String>, LlmKeyFault> {
    if rc_llm_cfg::keys::check_secret_ref(secret_ref).is_err() {
        return Err(LlmKeyFault::BadRef);
    }
    let (box_name, slot) = secret_ref.split_once(':').ok_or(LlmKeyFault::BadRef)?;
    rc_secret::read_secret(data_dir, box_name, slot).map_err(map_fault)
}

/// 一个变体一条去向；`SecretError` 增变体时这里编译不过，逼着表态。
/// `AlreadyExists` 不从读路径来（读箱只 `open`），为穷尽性归到 `BoxOpen`。
fn map_fault(e: rc_secret::SecretError) -> LlmKeyFault {
    use rc_secret::SecretError as E;
    match e {
        E::BadBox => LlmKeyFault::BadBox,
        E::BoxMissing => LlmKeyFault::BoxMissing,
        E::PassMissing => LlmKeyFault::PassMissing,
        E::OpenFailed | E::BadFormat | E::Io(_) | E::Permission | E::BadParams
        | E::AlreadyExists | E::BackupKey | E::BackupFormat | E::BackupAuth => {
            LlmKeyFault::BoxOpen
        }
        E::MissingSlot => LlmKeyFault::SlotMissing,
        E::BadSlot => LlmKeyFault::BadRef,
    }
}
