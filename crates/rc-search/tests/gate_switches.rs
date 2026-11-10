//! §8 的闸门开关逐条点名（任务 6 前半 · Step 1）。
//!
//! 这一档防的是"把所有沉默都归给低分"那一族：每条用例都**同时**断
//! ①"关掉这个开关 ⇒ 沉默（或不降级就不许沉默）"与②"沉默的原因 = 它对应的那一格"。
//! 只断①的话，实现者可以让 `decide` 永远返回 `LowScore` 而全绿。
//!
//! 夹具全部合成（守卫 12）：正文是编造的中文短句，没有 CANARY、没有路径、没有密钥。

use rc_memory::keys::MemKey;
use rc_memory::{
    attest, bump_cooldown, clear_expired, put_card, resolve_retirement, retire, set_verdict,
    supersede, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Stance, Verdict,
};
use rc_search::gate::{decide, Form, GateCell, Switches};
use rc_search::rank::{rank, Scored};
use rc_store::{MemoryHandle, Root};

/// 六开关全绿那一条行的查询词（正文里必含它，见 `body`）。
const QUERY: &str = "开关载体用例 同分合成词甲";
/// 只查共享前缀词的那一半：多条行同池时用它的臂（见 `开关_not_stale` 等）。
const QUERY_SHORT: &str = "同分合成词甲";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}
fn human_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }]
}
fn agent_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Agent }]
}
fn body(title: &str) -> FactContent {
    FactContent {
        kind: "decision".into(),
        title: title.into(),
        body: "开关载体用例 同分合成词甲 一句足够短的话".into(),
        scope: scope(),
        valid_to_ms: None,
    }
}

/// `crates/rc-search/tests/order.rs:14` 与 `crates/rc-index/tests/rebuild.rs:7` 的同一个 helper 口径：
/// `TempDir` 必须先于句柄活着、由调用方绑定，所以这里交回**所有权**而不是借来的 `&MemoryHandle`
/// （交回所有权还顺带满足本档 `开关_strong_anchor` 那条：它要在换文件之前把连接关掉）。
fn memory_handle(dir: &std::path::Path) -> MemoryHandle {
    Root::open(dir).unwrap().into_memory()
}

/// 一条**六开关全绿**的行：人工锚、未过期、未被否、未冷却、有正面表态、能命中。
/// 所有开关用例都从它出发"关掉一个"，这样每个用例的差异是单变量的。
/// 偏离 brief：原文返回 `Result<MemKey, MemError>`，而它内部最后一步 `rc_index::rebuild(..)?`
/// 的错误类型是 `IndexError`（两支互不转换），那个 `?` 编译不过（实测 E0277）。
/// 夹具建不起行就是用例直接失败，所以这里就地 `unwrap` 并把类型收成 `MemKey`。
fn eligible(dir: &std::path::Path, now: i64, title: &str) -> MemKey {
    let m = memory_handle(dir);
    let k = put_card(&m, &body(title), &human_prov(), now).unwrap();
    attest(&m, &k, Stance::Confirmed, Some(&"h".repeat(64)), None, now).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    k
}

/// 按标题取**唯一**一条候选：命中数不是 1 就红，因为那意味着夹具污染了别的行。
/// 偏离 brief：去掉原文的 `now` 形参——它在这个 helper 里没有任何用处（编译期 `unused_variable`）。
fn only(dir: &std::path::Path, title: &str) -> Scored {
    let m = memory_handle(dir);
    let got: Vec<_> = rank(&m, None, NOW, QUERY)
        .unwrap()
        .into_iter()
        .filter(|s| s.facts.title == title)
        .collect();
    // 偏离 brief（M-1 / 守卫 12）：原文的消息是 `{got:?}`，而 `Scored` 派生了 `Debug` ⇒
    // 一旦命中数不是 1，整行的 `facts.body` 会连同类目一起吐进 panic 输出。
    // 夹具今天是合成的，但任务 11 引入真实缺席样本后这个形状就变成泄漏面，所以载荷换成
    // **只有身份值**（标题 + 句柄串）。
    let ids: Vec<String> = got.iter().map(|s| s.key.key_string()).collect();
    assert_eq!(got.len(), 1, "标题 {title} 命中 {} 行，句柄：{ids:?}", got.len());
    got.into_iter().next().unwrap()
}

/// 按句柄从短查询的候选池里取一条；不在池里就点名是哪条臂丢了行。
fn hit(dir: &std::path::Path, key: &MemKey) -> Scored {
    let m = memory_handle(dir);
    rank(&m, None, NOW, QUERY_SHORT)
        .unwrap()
        .into_iter()
        .find(|x| &x.key == key)
        .unwrap_or_else(|| panic!("{} 不在候选池", key.key_string()))
}

const NOW: i64 = 1_700_000_000_000;

#[test]
fn 基线_六开关全绿的行必须开口() {
    let dir = tempfile::tempdir().unwrap();
    let k = eligible(dir.path(), NOW, "基线全绿卡");
    let s = only(dir.path(), "基线全绿卡");
    let sw = Switches::of(&s, NOW);
    assert!(sw.opens_mouth(), "基线行就该沉默，后面的用例全部失去参照：{sw:?}");
    let d = decide(&s, NOW);
    assert_eq!(d.key.key_string(), k.key_string());
    assert_eq!(d.cell, None);
    // V5：单次 confirmed 只给 signpost，不给 full。
    assert_eq!(d.form, Form::Signpost);
}

#[test]
fn 开关_attested_的载体是表态表_关掉它只降级不沉默() {
    // V10 的执法点。关掉它必须得到 signpost 而不是 Silent——
    // 出现一个 cell=None 却 form=Silent 的行，就等于造出了"读不懂的沉默"。
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let k = put_card(&m, &body("无表态卡"), &human_prov(), NOW).unwrap();
    let k2 = put_card(&m, &body("佐证升级卡"), &human_prov(), NOW).unwrap();
    rc_index::rebuild(&m, NOW).unwrap();

    let s = only(dir.path(), "无表态卡");
    assert!(!Switches::of(&s, NOW).attested);
    let d = decide(&s, NOW);
    assert_eq!(d.form, Form::Signpost, "未表态必须降级不许沉默");
    assert_eq!(d.cell, None);
    // 偏离 brief（搬载体，配合 Step 5 第 6 条）：brief 说把 `attested` 挪进 `opens_mouth` 时这条用例也该红，
    // 但它通篇只读 `decide` 的产物——而 `decide` 不调 `opens_mouth`，所以那个变异在这儿**无牙**
    // （实测 12 条里只有 `合取性质` 红）。V10 说"attested 不驱动沉默"，这句话在开关侧的形状就是
    // "关掉它 opens_mouth 仍为真"，于是这里补上它——两处执法（这条 + 镜像用例）从此都有载体。
    assert!(Switches::of(&s, NOW).opens_mouth(), "无表态行必须仍能开口（V10：attested 只降级，不沉默）");
    // 证据升级 ⇒ 才开最高档。两条臂各占一行，免得一条行同时被两个证据撑着
    // （brief 原文把第二条臂挂在已经 Passed 的行上，那一臂无论怎样都绿）。
    set_verdict(&m, &k, Verdict::Passed, NOW).unwrap();
    let s = only(dir.path(), "无表态卡");
    assert_eq!(decide(&s, NOW).form, Form::Full, "Passed 必须开 full");

    // 臂②：一次 corroborated 表态，行仍没被中心判过（verdict 保持 unreviewed）。
    // 偏离 brief：原文这一行是 `let _ = attest(...)`——`let _ =` 是 G7 禁的形状（错误不许折成
    // "什么都没发生"），且它不接任何断言。这里改成 unwrap + 独立一行的断言。
    attest(&m, &k2, Stance::Corroborated, Some(&"h".repeat(64)), None, NOW).unwrap();
    let s2 = only(dir.path(), "佐证升级卡");
    assert_eq!(s2.facts.verdict, Verdict::Unreviewed, "佐证臂必须与 Passed 臂分处两行");
    assert_eq!(s2.facts.attestation, Some(Stance::Corroborated));
    assert_eq!(decide(&s2, NOW).form, Form::Full, "corroborated 必须开 full");
}

#[test]
fn 开关_verdict_ok_只否_failed_low_与_unreviewed_都过() {
    // §8 原文要 `Low` 也放行：它是中心复核给出的"质量偏低"读数，不是否决。
    // 把 Low 当成否，症状是"低质量的真记忆永远不再被提起"，而那属于 G6 判据③的反面。
    let dir = tempfile::tempdir().unwrap();
    let k = eligible(dir.path(), NOW, "判决三态卡");
    let m = memory_handle(dir.path());
    for (v, expect_silent) in [
        (Verdict::Unreviewed, false),
        (Verdict::Low, false),
        (Verdict::Failed, true),
    ] {
        set_verdict(&m, &k, v, NOW).unwrap();
        let s = only(dir.path(), "判决三态卡");
        assert_eq!(Switches::of(&s, NOW).verdict_ok, !expect_silent, "{v:?} 判错");
        let d = decide(&s, NOW);
        assert_eq!(d.form == Form::Silent, expect_silent, "{v:?}");
        assert_eq!(d.cell, if expect_silent { Some(GateCell::Rejected) } else { None }, "{v:?} 归错格");
    }
}

#[test]
fn 开关_not_stale_的三条臂一条都不能少() {
    // 载体 = retirement 未解决 ∨ superseded_by 非空 ∨ valid_to_ms 已过期（§10 生命周期①②）。
    // 只断 valid_to 是实现者最容易写的版本，所以三条各来一遍，且都要求同一个格名。
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());

    // 臂①：valid_to 过期。
    let expired = FactContent { valid_to_ms: Some(NOW - 1), ..body("过期臂卡") };
    let k1 = put_card(&m, &expired, &human_prov(), NOW).unwrap();
    // 臂②：被取代。
    let k2 = put_card(&m, &body("被取代臂卡"), &human_prov(), NOW).unwrap();
    supersede(&m, &k2, &"f".repeat(64), NOW).unwrap();
    // 臂③：退场（抑制账）。
    let k3 = put_card(&m, &body("退场臂卡"), &human_prov(), NOW).unwrap();
    retire(&m, &k3, "合成理由", NOW).unwrap();

    rc_index::rebuild(&m, NOW).unwrap();
    for (label, k) in [("valid_to", &k1), ("superseded_by", &k2), ("retirement", &k3)] {
        let s = hit(dir.path(), k);
        assert!(!Switches::of(&s, NOW).not_stale, "{label} 没让 not_stale 变假");
        let d = decide(&s, NOW);
        assert_eq!(d.form, Form::Silent, "{label}");
        assert_eq!(d.cell, Some(GateCell::Stale), "{label} 归错格");
    }
    // 可逆的一半：resolve 退场后必须回来（守卫 13 的第一段）。
    resolve_retirement(&m, &k3, NOW + 1).unwrap();
    let s = hit(dir.path(), &k3);
    assert!(Switches::of(&s, NOW + 1).not_stale, "resolve 后仍被当过期");
    assert_ne!(decide(&s, NOW + 1).form, Form::Silent);
}

#[test]
fn valid_to_为空等于永不过期而不是看起来过期() {
    // §10 生命周期②。默认值方向错了会让处境类记忆集体沉默，
    // 而那和"库里没有"字节相同——最难发现的一类。
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let forever = put_card(&m, &body("永不过期卡"), &human_prov(), NOW).unwrap();
    let future = FactContent { valid_to_ms: Some(NOW + 86_400_000), ..body("未到期卡") };
    let soon = put_card(&m, &future, &human_prov(), NOW).unwrap();
    rc_index::rebuild(&m, NOW).unwrap();
    for k in [forever, soon] {
        let s = hit(dir.path(), &k);
        assert!(Switches::of(&s, NOW + 1000).not_stale, "{} 被判过期", k.key_string());
        assert_ne!(decide(&s, NOW + 1000).cell, Some(GateCell::Stale));
    }
}

#[test]
fn 开关_strong_anchor_的载体是出处角色_不在会话库里现查() {
    let dir = tempfile::tempdir().unwrap();
    let weak;
    {
        let m = memory_handle(dir.path());
        weak = put_card(&m, &body("弱锚卡"), &agent_prov(), NOW).unwrap();
        rc_index::rebuild(&m, NOW).unwrap();
    } // 连接在这里关掉：Windows 上给一个正被打开的库文件改名不是可靠操作（关掉再换才测得到事实）
    let s = hit(dir.path(), &weak);
    assert_eq!(s.facts.anchor_role, AnchorRole::Agent);
    assert!(!Switches::of(&s, NOW).strong_anchor);
    let d = decide(&s, NOW);
    assert_eq!(d.form, Form::Silent);
    assert_eq!(d.cell, Some(GateCell::WeakAnchor));
    // V6 的另一半：这条判据**不读 transcript.db**——把会话库换掉再判一次，结果必须一致。
    // 偏离 brief（等价加强）：原文对 `transcript.db` 改名后仍用**改名前就握着的那只连接**，
    // 那只连接握着旧文件的句柄，换名字根本测不到"现查"这一路。这里先关连接、把会话库移走，
    // 于是重新 `Root::open` 拿到的是一个**空的** transcript.db：
    // 会话库里一条轮次都没有、而这一行仍然沉默在 WeakAnchor ⇒ 载体确实是抄进行的那一列。
    std::fs::rename(dir.path().join("transcript.db"), dir.path().join("hidden.db")).unwrap();
    let s2 = hit(dir.path(), &weak);
    assert_eq!(s2.facts.anchor_role, AnchorRole::Agent);
    assert_eq!(decide(&s2, NOW).cell, Some(GateCell::WeakAnchor));
    assert!(!Switches::of(&s2, NOW).strong_anchor);
    assert!(dir.path().join("hidden.db").exists(), "会话库没被移走 ⇒ 上面那半什么都没测");
    assert_eq!(rc_memory::provenance_of(&memory_handle(dir.path()), &weak).unwrap().len(), 1,
        "出处行的口径变了：这一行的锚只有那一条 agent 轮");
}

#[test]
fn 开关_above_floor_的载体是内存里的整数分数_库里没有它的列() {
    let dir = tempfile::tempdir().unwrap();
    let _k = eligible(dir.path(), NOW, "阈值开关卡");
    let s = only(dir.path(), "阈值开关卡");
    assert!(s.above_floor());
    assert!(Switches::of(&s, NOW).above_floor);
    // 关掉它的唯一办法是造一个 0 分行（长行），所以这里断的是"分数与格子一致"而不是"能关掉"。
    let m = memory_handle(dir.path());
    // 偏离 brief 的魔数：`seed_padded(.., 3000)` 落在 `score == 1 == FLOOR`（实测
    // `1024*5/3004 == 1`），`above_floor` 依然为真 ⇒ 那条臂的 `assert!(!long.above_floor())` 红。
    // 0 分的条件是 `row_len > 1024 · matched`，即 extra_tokens 必须 > 约 5120；
    // 这里取 6000（约 1.17 倍余量），与 `rc-memory/src/synth.rs:144` 那条超长卡同一个理由。
    rc_memory::synth::seed_padded(&m, NOW, 6000).unwrap();
    rc_index::rebuild(&m, NOW).unwrap();
    let long = rank(&m, None, NOW, "长行低分卡 记录")
        .unwrap()
        .into_iter()
        .find(|x| x.facts.title == "长行低分卡")
        .unwrap();
    assert!(!long.above_floor(), "0 分臂没造出来：score={} matched={} row_len={}",
        long.score, long.matched, long.row_len);
    assert!(!Switches::of(&long, NOW).above_floor);
    assert_eq!(decide(&long, NOW).cell, Some(GateCell::LowScore));
}

#[test]
fn 第六个开关_not_cooling_down_的载体是冷却列() {
    // D4 带来的；它不在 §8 原文五条里，所以必须单独点名——否则实现者会认为"没要求测它"。
    let dir = tempfile::tempdir().unwrap();
    let k = eligible(dir.path(), NOW, "冷却开关卡");
    let m = memory_handle(dir.path());
    let stamped = bump_cooldown(&m, std::slice::from_ref(&k), NOW, rc_memory::COOLDOWN_MS).unwrap();
    assert_eq!(stamped, 1, "冷却没写进行");
    rc_index::rebuild(&m, NOW).unwrap();
    let s = hit_cooldown_pool(dir.path(), &k);
    assert!(!Switches::of(&s, NOW).not_cooling_down);
    let d = decide(&s, NOW);
    assert_eq!(d.form, Form::Silent);
    assert_eq!(d.cell, Some(GateCell::Cooldown));
    // 未来的行不许被清：清账的时钟还在窗口里 ⇒ 回执 0 行。
    // 偏离 brief 的实参：原文这里是 `clear_expired(m, later)`，而 `later == 冷却到期点 + 1`，
    // 那一行按 `cooldown_until_ms <= now_ms` 已经**到期**了（实测回执 1 行，断言红）。
    // "不许清未来的行"这句意图只有拿窗口内的时钟才测得到，所以传 `NOW`。
    assert_eq!(clear_expired(&m, NOW).unwrap(), 0, "clear_expired 不许清未来的行");
    // 窗口过了就回来，且**是同一行**（句柄不变 ⇒ 宿主笔记里的引用仍然有效）。
    let later = NOW + rc_memory::COOLDOWN_MS + 1;
    let s2 = hit_cooldown_pool(dir.path(), &k);
    assert!(Switches::of(&s2, later).not_cooling_down);
    // V5 的镜像：这一行从建立起就是"单次 confirmed"，冷却解除后回到它原本那一档（signpost），
    // 不是 full。偏离 brief：原文在这里断 `Full` 并注"Passed 之后应回到 full"，
    // 但 `eligible` 从没写过 verdict=Passed，`attest` 写的是 confirmed ⇒ 断 Full 就是断错了档
    // （而基线那条用例在同样的行上断的正是 Signpost，两条会互相矛盾）。
    assert_eq!(decide(&s2, later).form, Form::Signpost, "冷却解除后必须回到基线那一档");
    // 想要 full 就补上证据：这一句同时保住 brief 想说的"Passed ⇒ full"。
    set_verdict(&m, &k, Verdict::Passed, later).unwrap();
    let s3 = hit_cooldown_pool(dir.path(), &k);
    assert_eq!(decide(&s3, later).form, Form::Full, "Passed 之后应回到 full");
}

/// 冷却那条行的池子按完整查询词取（它的标题正文都只在 `QUERY` 里被命中）。
fn hit_cooldown_pool(dir: &std::path::Path, key: &MemKey) -> Scored {
    let m = memory_handle(dir);
    rank(&m, None, NOW, QUERY)
        .unwrap()
        .into_iter()
        .find(|x| &x.key == key)
        .unwrap_or_else(|| panic!("{} 不在候选池", key.key_string()))
}

#[test]
fn 分数放大十倍不许改变形态() {
    // G6 判据③的可执行版本：匹配度决定"开不开口"，不决定"用哪一档"。
    // 造一个 Scored 副本把分数乘 10，形态与格子必须逐位相同。
    let dir = tempfile::tempdir().unwrap();
    let _k = eligible(dir.path(), NOW, "放大十倍卡");
    let s = only(dir.path(), "放大十倍卡");
    let a = decide(&s, NOW);
    let mut boosted = s.clone();
    boosted.score *= 10;
    let b = decide(&boosted, NOW);
    assert_eq!(a.form, b.form, "形态被分数驱动了：{:?} vs {:?}", a.form, b.form);
    assert_eq!(a.cell, b.cell);
    assert!(b.score > a.score, "放大没生效，用例什么都没断");
}

#[test]
fn 合取性质_六个开关里关掉任意一个都必须沉默且格子唯一() {
    // §8 的"合取"不是修辞：同一份行数据，逐个开关造反例，
    // 断 opens_mouth 与 decide 在每个反例上都同意（两套实现不许漂）。
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let cases = ["关attested", "关not_stale", "关strong_anchor", "关verdict_ok", "关above_floor", "关cooldown"];
    let expect: [Option<GateCell>; 6] = [
        None,                       // attested 关掉只降级，不沉默（V10）
        Some(GateCell::Stale),
        Some(GateCell::WeakAnchor),
        Some(GateCell::Rejected),
        Some(GateCell::LowScore),
        Some(GateCell::Cooldown),
    ];
    // 每一臂**只该**关掉哪些开关字段（与 `cases` 同下标）。第 ② 臂有两个：弱锚那一行按 V6 不许
    // 有人工出处，而 `attest` 的正面表态又必须挂在人工轮上（`rc-memory/src/writers_a.rs:19`），
    // 所以它同时是"没表态"的那一条——`attested` 不驱动沉默，格子仍然是 WeakAnchor。
    const SWITCH_OF_CASE: [&[&str]; 6] = [
        &["attested"],
        &["not_stale"],
        &["attested", "strong_anchor"],
        &["verdict_ok"],
        &["above_floor"],
        &["not_cooling_down"],
    ];
    let mut made: Vec<(usize, MemKey)> = Vec::new();
    for (i, label) in cases.iter().enumerate() {
        let title = format!("合取反例卡{i}");
        // 关阈值的那条必须是"能被 FTS 命中、但因为极长而整除到 0"——
        // 正文里没有查询词的行根本不进候选池，用它测 LowScore 等于什么都没测。
        // 长度口径同 `开关_above_floor`：`1024 * matched / row_len` 要为 0 ⇒ row_len > 1024·matched。
        let content = match *label {
            "关not_stale" => FactContent { valid_to_ms: Some(NOW - 1), ..body(&title) },
            "关above_floor" => FactContent {
                body: (0..6000).map(|_| "同分合成词甲 记录 ").collect(),
                ..body(&title)
            },
            _ => body(&title),
        };
        let prov = match *label {
            "关strong_anchor" => agent_prov(),
            _ => human_prov(),
        };
        let k = put_card(&m, &content, &prov, NOW).unwrap();
        // 偏离 brief：原文条件是 `*label != "关attested"`，于是它也试图给**弱锚那条行**做正面表态，
        // 而 `attest` 的正面表态必须解析到该行的**人工轮**出处（writers_a.rs:19-26）——
        // 那行只有 agent 出处，`.unwrap()` 当场 `AnchorNotHuman`  panic。
        // 弱锚那一格不需要表态来参与（attestation 不驱动沉默），所以这里跳过它。
        if *label != "关attested" && *label != "关strong_anchor" {
            attest(&m, &k, Stance::Confirmed, Some(&"h".repeat(64)), None, NOW).unwrap();
        }
        if *label == "关verdict_ok" {
            set_verdict(&m, &k, Verdict::Failed, NOW).unwrap();
        }
        if *label == "关cooldown" {
            bump_cooldown(&m, std::slice::from_ref(&k), NOW, rc_memory::COOLDOWN_MS).unwrap();
        }
        made.push((i, k));
    }
    rc_index::rebuild(&m, NOW).unwrap();
    let all = rank(&m, None, NOW, QUERY).unwrap();
    assert!(!all.is_empty(), "候选池空了，用例失去意义");
    for s in &all {
        let sw = Switches::of(s, NOW);
        let d = decide(s, NOW);
        // G11 改写后的镜像：出声 ⇔ 开关合取 ∨ 弱锚降级那一臂（Passed ∧ kind=event 的
        // WeakAnchor 格）。本用例的夹具没有 event 卡 ⇒ 右侧第二支对所有行恒假，
        // 断言对这批夹具与改前逐字等价；全量降级用例在 `tests/g11_weak_anchor_downgrade.rs`。
        let mouth = sw.opens_mouth()
            || (d.cell == Some(GateCell::WeakAnchor)
                && s.facts.verdict == Verdict::Passed
                && s.facts.kind == "event");
        assert_eq!(
            mouth,
            d.form != Form::Silent,
            "{} 的开关合取与 decide 不一致",
            s.facts.title
        );
        // 这一条不动（工单拍板）：`cell.is_none() == opens_mouth()`——opens_mouth 是开关合取，
        // 格位归属不受 G11 降级影响（降级行的 cell 照旧是 Some(WeakAnchor)）。
        assert_eq!(sw.opens_mouth(), d.cell.is_none(), "{} 的格子与开关合取不一致", s.facts.title);
    }
    // 逐臂点名：每个反例都必须落进它对应的那一格（缺这一半，"六臂各测一个开关"就退化成了"有一条会沉默"）。
    for (i, k) in made.iter() {
        let s = all.iter().find(|x| &x.key == k).unwrap_or_else(|| panic!("第 {i} 臂的行不在候选池"));
        let d = decide(s, NOW);
        match expect[*i] {
            Some(cell) => assert_eq!(d.cell, Some(cell), "{} 归错格", cases[*i]),
            None => assert_eq!(d.cell, None, "{} 不该沉默（V10）", cases[*i]),
        }
        // 偏离 brief 多加的这一半：格子之外还要断"那一臂关掉的正是它点名的那个开关"。
        // 只断格子的话，某臂明明**两个**开关都被关掉，格子会按优先级报出先检查的那一个，
        // 测试就看不出第二个开关也被人弄坏了（单变量这一半才是"逐臂点名"的意思）。
        let sw = Switches::of(s, NOW);
        let off: Vec<&str> = [
            ("attested", sw.attested),
            ("not_stale", sw.not_stale),
            ("strong_anchor", sw.strong_anchor),
            ("verdict_ok", sw.verdict_ok),
            ("above_floor", sw.above_floor),
            ("not_cooling_down", sw.not_cooling_down),
        ]
        .iter()
        .filter(|(_, on)| !*on)
        .map(|(n, _)| *n)
        .collect();
        assert_eq!(
            off.join(","),
            SWITCH_OF_CASE[*i].join(","),
            "{} 关掉的开关不是它点名的那一个",
            cases[*i]
        );
    }
}

#[test]
fn 格子优先级_一格都不许被下一格冒充() {
    // V8 的优先级（否决 > 过期 > 冷却 > 弱锚 > 低分）在单变量用例里是**测不到**的：
    // `合取性质` 那六臂各自只关一个开关，任何两段格序互换都照样全绿。
    // 所以这里专门造"一行同时坏两格"的样本，每一臂都先断第二个原因**确实也在**，
    // 再断报出来的是高优先级那一格——少了前一半，这条用例就只是"报了对的格"的空话。
    // 偏离 brief 的新增用例（brief 的 Step 1 六条里没有它；它属于本档的优先级形状，不是 Step 3 的 `causes`）。
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let long_body: String = (0..6000).map(|_| "同分合成词甲 记录 ").collect();

    // ① 过期 + 低分 ⇒ Stale。
    let k1 = put_card(
        &m,
        &FactContent { body: long_body.clone(), valid_to_ms: Some(NOW - 1), ..body("过期且低分卡") },
        &human_prov(),
        NOW,
    )
    .unwrap();
    // ② 否决 + 过期 ⇒ Rejected（人的判决压过机械读数）。
    let k2 = put_card(
        &m,
        &FactContent { body: long_body.clone(), valid_to_ms: Some(NOW - 1), ..body("被否且过期卡") },
        &human_prov(),
        NOW,
    )
    .unwrap();
    set_verdict(&m, &k2, Verdict::Failed, NOW).unwrap();
    // ③ 冷却 + 弱锚 ⇒ Cooldown。
    let k3 = put_card(&m, &body("冷却且弱锚卡"), &agent_prov(), NOW).unwrap();
    bump_cooldown(&m, std::slice::from_ref(&k3), NOW, rc_memory::COOLDOWN_MS).unwrap();
    // ④ 过期 + 冷却 ⇒ Stale（R32 补的相邻段：`gate.rs` 的过期段与冷却段此前没有任何
    // "一行同时坏两格"的载体，互换那两支全绿）。
    let k4 = put_card(
        &m,
        &FactContent { valid_to_ms: Some(NOW - 1), ..body("过期且冷却卡") },
        &human_prov(),
        NOW,
    )
    .unwrap();
    // R30：`bump_cooldown` 不开事务、`NotFound` 早退不回滚 ⇒ 逐句柄生效；这一行的冷却列必须真的写上
    // （下面 `!sw4.not_cooling_down` 就是它的回执，不在这里把返回值当失败）。
    bump_cooldown(&m, std::slice::from_ref(&k4), NOW, rc_memory::COOLDOWN_MS).unwrap();
    // ⑤ 弱锚 + 低分 ⇒ WeakAnchor（R32 补的另一对相邻段：弱锚段与低分段）。
    // 长度口径同 `开关_above_floor`：`row_len > 1024 · matched` 才整除到 0，故复用 `long_body`。
    let k5 = put_card(
        &m,
        &FactContent { body: long_body.clone(), ..body("弱锚且低分卡") },
        &agent_prov(),
        NOW,
    )
    .unwrap();
    rc_index::rebuild(&m, NOW).unwrap();

    let s1 = only(dir.path(), "过期且低分卡");
    assert_eq!(s1.key, k1, "①判定的行不是刚写进去的那条");
    let sw1 = Switches::of(&s1, NOW);
    assert!(!sw1.not_stale && !sw1.above_floor, "①的两个原因必须同时成立：{sw1:?}");
    assert_eq!(decide(&s1, NOW).cell, Some(GateCell::Stale), "既过期又低分必须报过期");

    let s2 = only(dir.path(), "被否且过期卡");
    assert_eq!(s2.key, k2, "②判定的行不是刚写进去的那条");
    let sw2 = Switches::of(&s2, NOW);
    assert!(!sw2.verdict_ok && !sw2.not_stale, "②的两个原因必须同时成立：{sw2:?}");
    assert_eq!(decide(&s2, NOW).cell, Some(GateCell::Rejected), "被否的行不许被报成过期");

    let s3 = only(dir.path(), "冷却且弱锚卡");
    assert_eq!(s3.key, k3, "③判定的行不是刚写进去的那条");
    let sw3 = Switches::of(&s3, NOW);
    assert!(!sw3.not_cooling_down && !sw3.strong_anchor, "③的两个原因必须同时成立：{sw3:?}");
    assert_eq!(decide(&s3, NOW).cell, Some(GateCell::Cooldown), "刚被提起过的行不许被报成弱锚");
    // 窗口过去之后，同一行的原因必须换成下一格（弱锚）——证明"报哪一格"取的是当时成立的那个最高优先级。
    let later = NOW + rc_memory::COOLDOWN_MS + 1;
    assert_eq!(decide(&s3, later).cell, Some(GateCell::WeakAnchor));

    let s4 = only(dir.path(), "过期且冷却卡");
    assert_eq!(s4.key, k4, "④判定的行不是刚写进去的那条");
    let sw4 = Switches::of(&s4, NOW);
    assert!(!sw4.not_stale && !sw4.not_cooling_down, "④的两个原因必须同时成立：{sw4:?}");
    assert_eq!(decide(&s4, NOW).cell, Some(GateCell::Stale), "既过期又被冷却的行不许被报成冷却");

    let s5 = only(dir.path(), "弱锚且低分卡");
    assert_eq!(s5.key, k5, "⑤判定的行不是刚写进去的那条");
    let sw5 = Switches::of(&s5, NOW);
    assert!(!sw5.strong_anchor && !sw5.above_floor, "⑤的两个原因必须同时成立：{sw5:?}");
    assert_eq!(decide(&s5, NOW).cell, Some(GateCell::WeakAnchor), "既弱锚又低分的行不许被报成低分");
}

#[test]
fn 被表态否决的行也沉默且与开关合取一致() {
    // Rejected 那一格的载体有**两列**（`GateCell::Rejected` 的文档自己说的）：
    // 中心的 `verdict='failed'` 与 agent/中心的未解决 rejected 表态。brief 的 `Switches::verdict_ok`
    // 注释只点了 verdict 列，而 brief 的 `decide` 两支都读——两支一旦各读一半，
    // `opens_mouth()` 会说"能开口"而 `decide` 报 `Rejected`，上面那条镜像用例正是为它准备的，
    // 但它只在 `verdict` 那一列上跑过。这一条把第二列补成载体。
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let k = put_card(&m, &body("被表态否决卡"), &human_prov(), NOW).unwrap();
    attest(&m, &k, Stance::Confirmed, Some(&"h".repeat(64)), None, NOW).unwrap();
    attest(&m, &k, Stance::Rejected, None, Some("合成：与最新结论冲突"), NOW).unwrap();
    rc_index::rebuild(&m, NOW).unwrap();
    let s = only(dir.path(), "被表态否决卡");
    assert!(s.facts.rejected, "rejected 表态没抄进行：闸门就没了载体");
    assert_eq!(s.facts.verdict, Verdict::Unreviewed, "这一行的否决必须只来自表态列");
    let sw = Switches::of(&s, NOW);
    assert!(!sw.verdict_ok, "表态否决没进 verdict_ok ⇒ 合取会答'能开口'");
    assert!(!sw.opens_mouth());
    let d = decide(&s, NOW);
    assert_eq!(d.form, Form::Silent);
    assert_eq!(d.cell, Some(GateCell::Rejected));
}

#[test]
fn review_未提升的卡只给_signpost_提升之后才_full() {
    // S4 的闸门形态（**只钉不改**）：review apply 的 CAS 之前卡停在 `unreviewed`，
    // 而 `decide` 的 full 只由 corroborated 或 `verdict=Passed` 打开 ⇒ `unreviewed` 必须停在
    // Signpost。这条用例同时钉住另一端：`promote_verdict` 真的把它推到 Full 的那一档，
    // 于是"提升有没有意义"与"闸门有没有恒开"在同一个载体上各有一个读数。
    let dir = tempfile::tempdir().unwrap();
    let k = eligible(dir.path(), NOW, "未提升卡");

    let s = only(dir.path(), "未提升卡");
    assert_eq!(s.facts.verdict, Verdict::Unreviewed, "eligible 的默认判决必须是 unreviewed");
    let d = decide(&s, NOW);
    assert_eq!(d.cell, None, "unreviewed 不是否决，不许归进 Rejected");
    assert_eq!(d.form, Form::Signpost, "unreviewed 开了 full ⇒ 闸门恒开");

    let m = memory_handle(dir.path());
    assert_eq!(rc_memory::promote_verdict(&m, &k, NOW).unwrap(), 1, "提升必须命中那一行");
    let s = only(dir.path(), "未提升卡");
    assert_eq!(s.facts.verdict, Verdict::Passed);
    assert_eq!(decide(&s, NOW).form, Form::Full, "提升之后才允许 full（V5 的另一端）");
}
