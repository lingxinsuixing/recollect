//! §9 的两条读者用例。三条表态各自的形态以原始行断言挂住，probe 侧以字面字串断言挂住。
//! §9 的两条读者用例。三条表态各自的形态以原始行断言挂住，probe 侧以字面字串断言挂住。

use rc_agent::recall::{RecallFilter, RecallRequest, recall};
use rc_agent::surface::probe_block;
use rc_memory::keys::{AnchorRole, Scope, ScopeKind, Stance};
use rc_memory::synth;
use rc_memory::writers_a;
use rc_memory::writers_x::{put_card, FactContent};
use rc_memory::Provenance;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

#[test]
fn 三条表态_各自的_created_与_resolved_字段_各对各() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let t_uid = "a".repeat(64);
    let scope = Scope { kind: ScopeKind::Project, key: "p-t9".into() };
    // 走公开写入口造出处行，**一条 SQL 都不写**：`rc-agent` 的依赖清单里没有 `rusqlite`
    //（守卫 3——SQL 住在 rc-memory 的读数实现里，本 crate 只消费它的结果）。
    // `put_card` 一次落下"行 + 一行 `anchor_role='human'` 的出处"，而 `attest` 的正面表态
    // 要核的正是这张出处表（V6）⇒ 这条出处的 uid 就是后面两条表态共用的锚。
    let key = put_card(root.memory(), &FactContent {
            kind: "decision".into(), title: "t9-被表态的那张卡".into(),
            body: "两次表态之间形态变过一次。".into(),
            scope: scope.clone(), valid_to_ms: None },
        &[Provenance { turn_uid: t_uid.clone(), anchor_role: AnchorRole::Human }],
        NOW).unwrap();

    writers_a::attest(root.memory(), &key, Stance::Rejected,
                      Some(&t_uid), Some("too vague"), NOW + 100).unwrap();
    writers_a::attest(root.memory(), &key, Stance::Confirmed,
                      Some(&t_uid), Some("after clarification"), NOW + 200).unwrap();

    let (req, _rej) = RecallRequest::handles(&[key.key_string()]).unwrap();
    let json = recall(root.memory(), &req, &RecallFilter::empty(), NOW + 300).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    let row = &v["rows"][0];
    let summary = row["attestation_summary"].as_array().expect("attestation_summary 是数组");
    assert_eq!(summary.len(), 2, "两条表态：rejected 在 t1、confirmed 在 t2");

    assert_eq!(summary[0]["stance"], "rejected");
    assert_eq!(summary[0]["actor"], "agent");
    assert_eq!(summary[0]["reason"], "too vague");
    // 第 6 格（E48 加）：锚是写入侧核过的那条人工轮出处，不暴露它等于把 V6 的校验藏起来。
    assert_eq!(summary[0]["anchor_turn_uid"], t_uid.as_str(),
               "表态锚着哪条人工轮必须读得出来——那是 V6 写入侧核过的那一条");
    assert_eq!(summary[0]["created_at_ms"], (NOW + 100).to_string());
    // **E50 裁定之后**：否决是终态，**不被**后续表态解除 ⇒ 这里是 null 而不是 t2。
    // "被否决之后又被确认就能复活"这条路今天**没有**产品形状——要复活走 C 的 retire 那一族。
    // 这条断言的旧版（期待 t2）就是让第二次投票复活整条记忆的那个读数。
    assert!(summary[0]["resolved_at_ms"].is_null(),
            "否决是终态：不该被后面那次 confirmed 解除（E50 裁定）");

    assert_eq!(summary[1]["stance"], "confirmed");
    assert_eq!(summary[1]["reason"], "after clarification");
    assert_eq!(summary[1]["created_at_ms"], (NOW + 200).to_string());
    assert!(summary[1]["resolved_at_ms"].is_null(),
            "confirmed 是在场态势，resolved_at_ms 必须 NULL");
}

#[test]
fn probe_不出现_attestation_summary_键() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    synth::seed(root.memory(), NOW).unwrap();
    // 影子索引不在场时 `probe_view` 报 `IndexError::NotBuilt`（与 `hook/tests/probe_cli.rs`
    // 同一条实测）⇒ 先建索引，这条用例才走得到真正的生产路径。
    rc_index::rebuild(root.memory(), NOW).unwrap();
    // **走生产路径**（`rc_search::gate::probe_view` → `rc_agent::surface::probe_block`），
    // 不手搓 `ProbeView`：那个结构是 `forms: Vec<Decision>` + `silent: Cells` +
    // `candidates: usize` 三字段，手搓它等于在测试里复刻一遍装配，而判据要断的是
    // "围栏那一行里有没有这个键"——那是渲染面的事。
    // 查询词必须**命中种子**（`synth::seed` 播的是中文 `同分合成词…`）：查不中就是
    // `forms=0` ⇒ 围栏压根不渲染 ⇒ 下面两句在空串上恒真，那不是守卫（E49 记的第四张空牌）。
    let view = rc_search::gate::probe_view(root.memory(), None, "同分合成词", NOW).unwrap();
    let text = probe_block(&view).text();
    assert!(!text.contains("attestation_summary"),
        "probe 围栏不允许出现 attestation_summary：{text}");
    // **防空那一格**：先断围栏**真的被渲染了**。查询词查不中就是 `forms=0`、`text=""`
    // ⇒ 上面那句 `!contains` 在空串上恒真（E49 实测：这是本任务第一张空牌）。
    // E50 把上一版那句 `lines().count() <= 3` 删了——`from_lines` 硬 `.take(3)`、结构恒真，
    // 而它的失败消息"加了这个字段就会破"恰好被变异二证伪（围栏加到 6 行仍绿）。
    assert!(text.contains("同分合成词"),
        "围栏压根没被渲染（forms=0）⇒ 上面那句负断言在空串上恒真，那不是守卫：{text:?}");
    assert!(!view.forms.is_empty(), "查不中就是空围栏，判据无处落脚：forms={}", view.forms.len());
}

#[test]
fn 再投一票_不会把被否决的记忆复活() {
    // E50 M1 的钉子：UPDATE-precursor 若把 `rejected` 也解除，`alive_where` 的第三支
    // （`NOT EXISTS (… stance='rejected' AND resolved_at_ms IS NULL)`）就不再命中 ⇒
    // 记忆静默复活。实测那一票是 `retired`——而 `Retired` 按既有设计只落表态行、
    // **不落抑制账**（`tools.rs` 的 `suppresses()` 只有 Rejected 为真），所以它不但没让
    // 记忆死，还把上一张否决账清了。**这是可达生产路径**（`VoteStance::ALL` 含 retired）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let uid = "c".repeat(64);
    let scope = Scope { kind: ScopeKind::Project, key: "p-t9b".into() };
    let key = put_card(root.memory(), &FactContent {
            kind: "decision".into(), title: "t9-否决后又被投票的那张卡".into(),
            body: "第二次投票不许让它复活。".into(), scope, valid_to_ms: None },
        &[Provenance { turn_uid: uid.clone(), anchor_role: AnchorRole::Human }], NOW).unwrap();

    writers_a::attest(root.memory(), &key, Stance::Rejected, Some(&uid), Some("太含糊"), NOW + 100).unwrap();
    let before = rc_memory::lookup(root.memory(), &key).unwrap().unwrap();
    assert!(before.rejected, "投了否决就该是死的");

    writers_a::attest(root.memory(), &key, Stance::Retired, None, Some("判为无价值"), NOW + 200).unwrap();
    let after = rc_memory::lookup(root.memory(), &key).unwrap().unwrap();
    assert!(after.rejected,
        "再投一票就把否决账解除了 ⇒ 被 agent 判死的记忆静默复活（E50 M1）");
    // 判定史两格都在：被否决那条**不被解除**（终态），后一条是在场的新表态。
    let hist = rc_memory::reader::attestations_of(root.memory(), &key).unwrap();
    assert_eq!(hist.len(), 2, "两次表态都进史");
    assert_eq!(hist[0].stance, Some(Stance::Rejected));
    assert!(hist[0].resolved_at_ms.is_none(), "否决是终态：不被后续表态解除");
}

#[test]
fn 脏表态slug读成_null_而不是被折成否决() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let key = put_card(root.memory(), &FactContent {
            kind: "decision".into(), title: "t9-脏表态".into(), body: "库外写进来的 slug。".into(),
            scope: Scope { kind: ScopeKind::Project, key: "p-t9".into() }, valid_to_ms: None },
        &[Provenance { turn_uid: "d".repeat(64), anchor_role: AnchorRole::Human }], NOW).unwrap();
    // `attestation.stance` 有 CHECK；用本连接的开关模拟"旧库/库外写入留下的脏 slug"。
    // 开关是连接级 PRAGMA，必须逐条 execute（带 `?1` 的语句走不了 execute_batch）。
    root.memory().conn().execute("PRAGMA ignore_check_constraints=ON", []).unwrap();
    root.memory().conn().execute(
        "INSERT INTO attestation(mem_table,mem_id,stance,actor,created_at_ms)
         VALUES('card', ?1, 'maybe', 'agent', 1)", [&key.id]).unwrap();
    root.memory().conn().execute("PRAGMA ignore_check_constraints=OFF", []).unwrap();
    let hist = rc_memory::reader::attestations_of(root.memory(), &key).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].stance, None, "认不出的 slug 必须读成 None，不是最强否定");

    let (req, _rej) = RecallRequest::handles(&[key.key_string()]).unwrap();
    let json = recall(root.memory(), &req, &RecallFilter::empty(), NOW + 10).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    assert!(v["rows"][0]["attestation_summary"][0]["stance"].is_null(),
            "agent 面必须看见 null，不许看见 rejected：{}", json.text());
}
