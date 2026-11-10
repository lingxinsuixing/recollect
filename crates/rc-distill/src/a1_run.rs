//! A1 的作业面：同一批跑两次、各自过契约、返回两份草稿。
//!
//! 两次调用之间不做任何写入：stable 交集与落库由调用方在两次都成功之后做
//!（`contract_v4::plan_a1` + `rc_memory::write_a1`）。这样"第一跑成功、第二跑坏了"
//! 不会在库里留下半套锚——第二跑失败时这一批整体不落，重试从干净状态开始。

use crate::batch::Batch;
use crate::contract_v4::{parse_a1, A1Output};
use crate::prompt_v4::{a1_system, a1_user, AnchorCandidate};
use crate::DistillError;
use rc_llm::{LlmBackend, Prompt};

/// 单跑 A1：渲染 prompt → 调模型 → 过契约。
///
/// `declared` 是**这批话的主人**（P0-1）：`resolve_principal` 的读数经
/// [`crate::pass_naming`] 传进来，渲染成 `a1_user` 的第一行。`None` ⇒ 渲染 `NONE`
/// （身份未知），不是"这一格不存在"。
pub fn run_a1_once(
    backend: &dyn LlmBackend,
    batch: &Batch,
    candidates: &[AnchorCandidate],
    declared: Option<&str>,
) -> Result<A1Output, DistillError> {
    let raw = backend.complete(&Prompt {
        system: a1_system(),
        user: a1_user(batch, candidates, declared),
        derive: false,
    })?;
    Ok(parse_a1(&raw, batch)?)
}

/// 同一批跑两次。顺序执行而不是并发：抽取座位有速率限制，且"第一跑坏 ⇒ 不再烧第二跑"
/// 是这里要保留的行为。两跑用**同一份** `declared`：两跑之间主人身份变了，那是另一批。
pub fn run_a1_pair(
    backend: &dyn LlmBackend,
    batch: &Batch,
    candidates: &[AnchorCandidate],
    declared: Option<&str>,
) -> Result<(A1Output, A1Output), DistillError> {
    let first = run_a1_once(backend, batch, candidates, declared)?;
    let second = run_a1_once(backend, batch, candidates, declared)?;
    Ok((first, second))
}
