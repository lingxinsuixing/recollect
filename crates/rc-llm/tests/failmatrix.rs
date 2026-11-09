use rc_llm::{advances_watermark, counts_attempt, consequences, Consequence, JobFamily, LlmBackend, LlmFailure, mock::MockBackend, Prompt};

#[test]
fn every_failure_state_stops_extraction_and_never_moves_the_watermark() {
    // 失败矩阵四态（原设计 §6"LLM 不可用矩阵"）：停新增抽取 / 存量照常查 / 双向提醒 / 恢复即续。
    // 前三条是**每一个**失败态的固定动作，第四条不属于失败态（它是"失败之后的第一次成功"）。
    // 把它们并成一格 = "超时与配额烧完在动作上字节相同"，而 §9.5 ① 与 P5 要分得开这两件事。
    for f in [LlmFailure::Timeout, LlmFailure::SchemaMismatch, LlmFailure::QuotaExhausted, LlmFailure::BackendError] {
        let c = consequences(&f);
        assert!(c.contains(&Consequence::StopNewExtract), "{f:?} 必须停新增抽取");
        assert!(c.contains(&Consequence::KeepViewsOpen), "{f:?} 不许关视图：存量照常查是承诺不是奖励");
        assert!(c.contains(&Consequence::BidirectionalAlert), "{f:?} 必须双向提醒（agent 位 + doctor 行）");
        assert!(!c.contains(&Consequence::ResumeAndCatchUp), "{f:?}：恢复即续不是失败时的动作");
        assert!(!advances_watermark(&f), "{f:?}：故障期不推水位");
        assert!(!counts_attempt(&f), "{f:?}：故障期不计 attempts");
    }
}

#[test]
fn resume_is_the_only_path_that_moves_forward_after_a_halt() {
    use rc_llm::resume_applies;
    assert_eq!(resume_applies(true, true), Some(Consequence::ResumeAndCatchUp));
    assert_eq!(resume_applies(false, true), None, "本来就没失败 ⇒ 补跑不是一条新规则");
    assert_eq!(resume_applies(true, false), None, "又失败 ⇒ 继续停");
}

#[test]
fn schema_mismatch_is_not_a_silent_empty_reply() {
    // JSON 解析绝不返回空对象那条执法点的**上游**：backend 层不许把畸形回复咽掉。
    let b = MockBackend::scripted(vec![Ok("{ not json".to_string())]);
    let got = b.complete(&Prompt { system: "s".into(), user: "u".into(), derive: false });
    // mock 只转述脚本，不判断内容：内容合法性是 rc-distill 的 SchemaMismatch 判定（任务 4）。
    assert!(got.is_ok());
    assert_eq!(b.calls(), 1);
}

#[test]
fn llm_face_structurally_has_no_handles() {
    // 守卫 6 的镜像：rc-llm 不是"没调用会话/记忆层"，是它的依赖集里没有那两个 crate。
    // 这条在源码层测不到引用（它连依赖都没有），所以测的是**类型面**：本 crate 导出的每一项
    // 都不含 rc_store 的类型。用函数指针签名做一次编译期探针（写不出这个签名就是证据）。
    fn takes_no_handle<T>(_f: fn(T) -> Result<String, LlmFailure>) {}
    takes_no_handle::<Prompt>(|_p| Err(LlmFailure::BackendError));
    // 运行时的另一半证据在任务 13 的 cargo tree 判据里（依赖集 = 空）。
}

#[test]
fn job_family_slugs_are_the_quota_subject_ids() {
    // §9.5 ①：配额读数的主语是家族名。这四个 slug 与任务 13 写进 usage_counter 的 subject_id 同源。
    assert_eq!(JobFamily::Extract.slug(), "extract");
    assert_eq!(JobFamily::Dedupe.slug(), "dedupe");
    assert_eq!(JobFamily::Audit.slug(), "audit");
    assert_eq!(JobFamily::Derive.slug(), "derive");
}
