use rc_contract::{
    assign_tier, compute_body_fp, compute_turn_uid, normalize, Locator, RejectCode, TurnTier,
};
use rc_contract::format::{ExchangeFile, RawSession, RawTurn};

fn turn(tier: Option<TurnTier>, uid: Option<&str>, seq: Option<i64>) -> RawTurn {
    RawTurn {
        turn_uid: uid.map(|s| s.to_string()),
        seq,
        tier,
        role: "human".into(),
        body: "先跑测试".into(),
        created_at_ms: Some(1730000000000),
        speaker: None,
    }
}

#[test]
fn undeclared_tier_falls_to_lowest() {
    // 缺省落最低档，禁猜高档位（架构 §5）
    let (t, err) = assign_tier(&turn(None, None, None));
    assert_eq!(t, TurnTier::ContentOnly);
    assert!(err.is_none());
}

#[test]
fn declared_id_bearing_without_uid_is_unfalsifiable_claim() {
    let (t, err) = assign_tier(&turn(Some(TurnTier::IdBearing), None, None));
    assert_eq!(err, Some(RejectCode::TierClaimUnfalsifiable));
    assert_eq!(t, TurnTier::IdBearing, "判定档用于报告，落库档由 normalize 决定");
}

#[test]
fn batch_reject_when_a_tier_claim_is_unfalsifiable() {
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![RawSession {
            session_key: "s".into(), device_id: "dev-synthetic-1".into(),
            source_agent: "a".into(), machine: None, project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None, principal_name: None, principal_source: None,
            turns: vec![turn(Some(TurnTier::IdBearing), Some("dup-1"), None),
                        turn(Some(TurnTier::IdBearing), Some("dup-1"), None)],
        }],
    };
    let rep = normalize(file, 1730000009999);
    assert!(rep.accepted.is_empty(), "整批拒收 ⇒ 一条都不许落库");
    assert_eq!(rep.batch_reject, Some(RejectCode::DuplicateTurnUid));
}

#[test]
fn content_only_tier_still_produces_a_stable_uid() {
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![RawSession {
            session_key: "s".into(), device_id: "dev-synthetic-1".into(),
            source_agent: "a".into(), machine: None, project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None, principal_name: None, principal_source: None,
            turns: vec![turn(None, None, None)],
        }],
    };
    let rep = normalize(file.clone(), 1);
    let rep2 = normalize(file, 999_999);
    assert_eq!(rep.accepted[0].turn.turn_uid, rep2.accepted[0].turn.turn_uid);
    assert_eq!(rep.accepted[0].turn.tier, TurnTier::ContentOnly);
}

#[test]
fn blank_only_body_is_a_per_item_reject_not_an_empty_row() {
    // parse 的 BodyEmpty 只看原始串；纯空白串得过 parse 后被 normalize_body 削成 ""。
    // 归一后为空的 turn 不许落库（空正文产不出可读的记忆），逐条拒收进报告。
    let mut blank = turn(None, None, None);
    blank.body = "   \n  \t ".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![RawSession {
            session_key: "s".into(), device_id: "dev-synthetic-1".into(),
            source_agent: "a".into(), machine: None, project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None, principal_name: None, principal_source: None,
            turns: vec![turn(None, None, None), blank],
        }],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, None, "空正文是逐条拒收，不是整批拒收");
    assert_eq!(rep.rejects.len(), 1);
    assert_eq!(rep.rejects[0].code, RejectCode::BodyEmpty);
    assert_eq!(rep.rejects[0].loc, Locator { session_index: 0, turn_index: Some(1) });
    assert_eq!(rep.rejects[0].detail, "body", "拒收原因只许放字段名，不许抄正文");
    assert_eq!(rep.accepted.len(), 1, "兄弟 turn 不受逐条拒收牵连");
    assert!(rep.accepted.iter().all(|a| !a.turn.body.is_empty()), "accepted 里一条空正文都不许有");
}

#[test]
fn stable_seq_claim_with_seq_is_accepted_and_sibling_reject_isolated() {
    // StableSeq 声明可证伪（有 seq）⇒ 落库档 StableSeq、uid 由 compute_turn_uid 计算
    // （seq 不进 preimage，R1 字节冻结）；同会话里 role 非法的兄弟 turn 只逐条拒，不牵连它。
    let mut bad_role = turn(None, None, None);
    bad_role.role = "robot".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![RawSession {
            session_key: "s".into(), device_id: "dev-synthetic-1".into(),
            source_agent: "a".into(), machine: Some("mach-synthetic-1".into()),
            project_hint: Some("hint-synthetic".into()), last_saved_at_ms: Some(1730000005000),
            owner_principal_id: None, principal_name: None, principal_source: None,
            turns: vec![turn(Some(TurnTier::StableSeq), None, Some(7)), bad_role],
        }],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, None);
    assert_eq!(rep.rejects.len(), 1, "role 非法只逐条拒");
    assert_eq!(rep.rejects[0].code, RejectCode::RoleUnknown);
    assert_eq!(rep.rejects[0].loc, Locator { session_index: 0, turn_index: Some(1) });
    assert_eq!(rep.accepted.len(), 1, "被逐条拒的兄弟不许把 accepted 一起带走");
    let acc = &rep.accepted[0];
    assert_eq!(acc.turn.tier, TurnTier::StableSeq);
    assert_eq!(
        acc.turn.turn_uid,
        compute_turn_uid("dev-synthetic-1", "s", "human", "先跑测试", 1730000000000),
        "StableSeq 的 uid 走计算路径，且 preimage 里没有 seq"
    );
    assert_eq!(acc.turn.body, "先跑测试");
    assert_eq!(acc.turn.body_fp, compute_body_fp("先跑测试"));
    assert_eq!(acc.turn.role, "human");
    assert_eq!(acc.turn.created_at_ms, 1730000000000);
    assert_eq!(acc.session.session_key, "s");
    assert_eq!(acc.session.device_id, "dev-synthetic-1");
    assert_eq!(acc.session.source_agent, "a");
    assert_eq!(acc.session.machine.as_deref(), Some("mach-synthetic-1"));
    assert_eq!(acc.session.project_hint.as_deref(), Some("hint-synthetic"));
    assert_eq!(acc.session.last_saved_at_ms, Some(1730000005000));
}

fn sess(session_key: &str, turns: Vec<RawTurn>) -> RawSession {
    RawSession {
        session_key: session_key.into(),
        device_id: "dev-synthetic-1".into(),
        source_agent: "a".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
        turns,
    }
}

/// T1 的证伪：同一会话里同一序号被不同 role 声明 ⇒ 它标识的不是 turn，声明是假的。
/// 这条判据存在的全部理由：没有它，给每条都填 `seq=0` 的弱源可以永远伪装成 T1，
/// 而"声明档 vs 实际档"的落差要等第一次重排导出才暴露（spec §4）。
#[test]
fn same_ordinal_across_roles_falsifies_the_stable_seq_claim() {
    let mut agent = turn(Some(TurnTier::StableSeq), None, Some(0));
    agent.role = "agent".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![sess("s", vec![turn(Some(TurnTier::StableSeq), None, Some(0)), agent])],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, Some(RejectCode::SeqNotStable));
    assert!(rep.accepted.is_empty(), "整批拒收 ⇒ 一条都不许落库");
    let r = &rep.rejects[rep.rejects.len() - 1];
    assert_eq!(r.code, RejectCode::SeqNotStable);
    assert_eq!(r.loc, Locator { session_index: 0, turn_index: Some(1) });
    // 守卫 12：detail 只许放数字与字段名。正文"先跑测试"两条 turn 都有，一个字都不许出现。
    assert_eq!(r.detail, "ordinal=0", "序号是身份值，正文不是");
    assert!(rep.rejects.iter().all(|x| !x.detail.contains("先跑测试")));
}

/// 判据不许咬合法数据：同序号 + 同 role + 不同正文是**重生成/分支**，架构 §5 明写两行都保留。
/// 把它拒了等于用"去重"的名义吃掉真实历史。
#[test]
fn same_ordinal_same_role_is_a_revision_not_a_falsified_claim() {
    let mut second = turn(Some(TurnTier::StableSeq), None, Some(3));
    second.body = "改成先写文档".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![sess("s", vec![turn(Some(TurnTier::StableSeq), None, Some(3)), second])],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, None, "重生成不是身份冲突");
    assert_eq!(rep.accepted.len(), 2, "同组两条都要保留，蒸馏阶段才决定取哪条");
    assert_ne!(
        rep.accepted[0].turn.turn_uid, rep.accepted[1].turn.turn_uid,
        "正文不同 ⇒ uid 不同（preimage 不含序号，R1 字节冻结）"
    );
}

/// 序号是**会话内**的承诺，跨会话重复是常态（每个会话都从 0 数起）。
/// 判据的作用域写错就会把最常见的合法导出整批拒收。
#[test]
fn same_ordinal_in_different_sessions_is_not_a_collision() {
    let mut agent = turn(Some(TurnTier::StableSeq), None, Some(0));
    agent.role = "agent".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![sess("s-1", vec![turn(Some(TurnTier::StableSeq), None, Some(0))]), sess("s-2", vec![agent])],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, None);
    assert_eq!(rep.accepted.len(), 2);
}

/// 只有**声明 T1** 的 turn 参与序号登记：T0 的 `seq` 是普通属性（spec §3.1"ordinal 只是普通属性"），
/// 拿它做标识性判据会把首批两个真实源（自带 uuid、同时带序号）整批拒收。
#[test]
fn id_bearing_turns_are_not_subject_to_the_ordinal_check() {
    let mut agent = turn(Some(TurnTier::IdBearing), Some("u-2"), Some(0));
    agent.role = "agent".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![sess("s", vec![turn(Some(TurnTier::IdBearing), Some("u-1"), Some(0)), agent])],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, None, "T0 的序号不参与证伪它的身份");
    assert_eq!(rep.accepted.len(), 2);
}

/// 两个平面不许串：role 非法是**逐条**拒收，不该因为它的序号和兄弟 turn 撞上就升级成整批拒收。
/// 一个坏 role 值本身可能就是生产者笔误，把它算进身份判据会让一条毒 turn 毁掉整份导出。
#[test]
fn a_role_invalid_turn_does_not_enter_the_ordinal_registry() {
    let mut robot = turn(Some(TurnTier::StableSeq), None, Some(0));
    robot.role = "robot".into();
    let file = ExchangeFile {
        format_version: 1,
        sessions: vec![sess("s", vec![turn(Some(TurnTier::StableSeq), None, Some(0)), robot])],
    };
    let rep = normalize(file, 1730000009999);
    assert_eq!(rep.batch_reject, None, "逐条拒收的 turn 不许升级为整批拒收");
    assert_eq!(rep.rejects.len(), 1);
    assert_eq!(rep.rejects[0].code, RejectCode::RoleUnknown);
    assert_eq!(rep.accepted.len(), 1);
}
