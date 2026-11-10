//! `RecallRequest::Handles` 那一支的三条判据：坏句柄**逐条拒收并计数**（一条坏串不许带走整批，
//! 也不许被静默丢掉），以及**显式查不到不许返回最近邻**（§10 消费侧③）。
//! 后一条是 Step 7 证伪第 4 项唯一的牙 —— 没有这个文件，`None => absent.push(..)` 改成
//! "拿 `all_titles` 里最接近的标题凑一行"不会让任何测试变红。
//!
//! 任务 8 Step 5 往里加了三条：幽灵句柄在**写入面**（`vote`）的那一道门（先认行再写，
//! 不留悬空表态），以及裁定 R45 点名的 `verdict` **取值**读者（`recall_cap.rs:126` 只对撞了
//! "与库里那一列同源"，而全仓夹具那一列恒为 `unreviewed` ⇒ 写成常量也不红，本文件补真读数）。
//! Step 5 的用例引用 Step 6 的产物（`rc_agent::tools::{vote, VoteStance}`），Step 5 交付时
//! 本文件编译不过是设计好的红；Step 6 落地后恢复绿。
//! 实测登记与控制器裁定 R46②③：brief 逐字给的三处期望与已落地的 `MemKey::parse` 口径相互矛盾
//! （`card:zz` 形状合法 ⇒ `bad_n` 读出 1 而不是 2、`vote` 给 `row_absent` 而不是 `bad_handle`），
//! 逐字论证见 `task-8-report.md` 第二段 §4；第三段按裁定把两处期望与一个实参改写为落地的**两类分型**
//! 口径（改写点各自有用例内注释点名），其余逐字未动。
//! 修复轮 1（工单 R51）在本文件加了一条 `status_的悬空表态读数_…`：给 `status` 的
//! `dangling_attestations` 那格补**非零**读者（此前它只有 `tools_shape.rs:191` 的零读数一条），
//! 同时给 `reader::dangling_attestations` 的 event 支补第一个夹具。

use rc_agent::recall::{recall, RecallFilter, RecallRequest};
use rc_agent::tools::{status, vote, StatusOptions, VoteStance};
use rc_agent::AgentErrorKind;
use rc_memory::keys::MemKey;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

/// 一行合成 card，返回它的句柄（顺带把索引建起来，好让 `lookup` 走在一条已建索引的库上）。
fn one_card(dir: &std::path::Path, title: &str) -> MemKey {
    let root = Root::open(dir).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "note".into(),
            title: title.into(),
            body: "同分合成词甲 的一句正文".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    k
}

/// `+Nnot_listed` 是 `unresolved_handles` 里的**唯一非句柄项**，所以断"含哪几条句柄"要先把它滤掉。
/// 这里每一步都是响亮的：形状不对就 panic，不 `unwrap_or_default`。
fn unresolved_handles(v: &serde_json::Value) -> Vec<String> {
    let arr = v["unresolved_handles"].as_array().expect("unresolved_handles 必须是数组");
    let mut out = Vec::new();
    for x in arr {
        let s = x.as_str().expect("每一项都是字符串");
        if !s.starts_with('+') {
            out.push(s.to_owned());
        }
    }
    out
}

#[test]
fn 查不到的句柄进_unresolved_handles_且绝不返回最近邻() {
    let dir = tempfile::tempdir().unwrap();
    let _near = one_card(dir.path(), "最接近的一条");
    let root = Root::open(dir.path()).unwrap();

    // `"card:zz"` 形状合法（`MemKey::parse` 只认 `<table>:<非空 id>`）⇒ 进合法批、lookup 查不到；
    // `"nonsense:a"` 表名认不出 ⇒ 在构造请求时就被拒收并计数（不许带走整批）。
    let raw = vec!["card:zz".to_string(), "nonsense:a".to_string()];
    let (req, rejected) = rc_agent::recall::RecallRequest::handles(&raw).unwrap();
    assert_eq!(rejected, 1, "坏句柄没有被单独计数：它被静默丢掉了");
    let rc_agent::recall::RecallRequest::Handles(ks) = &req else {
        panic!("handles() 构造出了非 Handles 臂的请求");
    };
    assert_eq!(ks.len(), 1, "合法的那一条被坏串带走了");
    assert_eq!(ks[0].key_string(), "card:zz");

    let json = rc_agent::recall::recall(root.memory(), &req, &RecallFilter::empty(), NOW).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    assert_eq!(v["rows"].as_array().expect("rows 必须是数组").len(), 0, "查不到却返回了行 ⇒ 最近邻冒充发生了");
    assert_eq!(unresolved_handles(&v), vec!["card:zz"], "查不到的句柄没被逐条点名");
}

#[test]
fn 全部句柄都坏是_validation_不是_empty() {
    let dir = tempfile::tempdir().unwrap();
    one_card(dir.path(), "一条真实记录");
    let raw = vec!["nonsense:a".to_string(), "card:".to_string(), "没有冒号".to_string()];
    // 全是坏句柄 ⇒ 必须报错（`unwrap_err` 而不是 `.err().expect(..)`：clippy 的 `err_expect`
    // 在本仓 `-D warnings` 下是硬错误）。
    let err = rc_agent::recall::RecallRequest::handles(&raw).unwrap_err();
    assert_eq!(err.kind, rc_agent::AgentErrorKind::Validation);
    assert_eq!(err.code, "no_valid_handles", "批里一条合法都没有 ⇒ 该改的是输入，不是承认库空");
}

#[test]
fn 合法句柄与查不到的混在一批时两条都要报() {
    let dir = tempfile::tempdir().unwrap();
    let k = one_card(dir.path(), "库里有的那条");
    let root = Root::open(dir.path()).unwrap();
    let raw = vec![k.key_string(), "card:zz".to_string()];
    let (req, rejected) = rc_agent::recall::RecallRequest::handles(&raw).unwrap();
    assert_eq!(rejected, 0, "两条都合法却被计数了");
    let json = rc_agent::recall::recall(root.memory(), &req, &RecallFilter::empty(), NOW).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json.text()).unwrap();
    assert_eq!(v["rows"].as_array().expect("rows 必须是数组").len(), 1, "合法的那条没回来");
    assert_eq!(unresolved_handles(&v), vec!["card:zz"], "同批里的坏句柄被顺手丢掉了");
    // 裁定 R41：`row_json` 的 `form` / `verdict` 两个键在句柄臂的读者（另一半在 `recall_cap.rs`）。
    // 刚写下的卡片出处是 human、没被否、没过期、没冷却 ⇒ 它必须**开口**，而且档位读得出来。
    // 这里不写"取不到就当没有"：`as_str().expect(..)` 是刻意的——`form` 键一旦从生产码里删掉，
    // 这条必须炸（模块头那句"G7 在消费侧的落点"没有读者就是假承诺）。
    let row = &v["rows"].as_array().expect("rows 必须是数组")[0];
    // 断言消息只带身份值（守卫 12）：句柄与标题，不 dump 整行（行里有正文）。
    let id = row["id"].as_str().expect("每行都带句柄");
    assert_eq!(id, k.key_string(), "回来的行不是点名的那条句柄");
    let form = row["form"].as_str().expect("form 必须是字符串（缺键 = agent 面读不出形态）");
    assert!(form == "full" || form == "signpost", "新鲜卡片却开不了口：{form} ⇒ {id}");
    assert_eq!(row["verdict"].as_str().expect("verdict 必须是字符串"), "unreviewed", "{id}");
    assert!(row["silence_reason"].is_null(), "开口的行带了沉默原因：{id} ⇒ {}", row["silence_reason"]);
    // 裁定 R37 的第三个数据点：句柄臂没有截断这回事，`unresolved_handles` 却非空——
    // 旧判据（按 `+` 前缀猜）在这里给出 `true`，即"契约字段带着与名字相反的真值"。
    assert_eq!(
        v["truncated"],
        serde_json::json!(false),
        "句柄臂没有截断，truncated 却跟着数组形状走：未列出读数={:?}",
        unresolved_handles(&v)
    );
}

#[test]
fn 坏句柄逐条进_unresolved_一个最近邻都不许返回() {
    let dir = tempfile::tempdir().unwrap();
    let k = one_card(dir.path(), "先跑测试再合并");
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 裁定 R46②（改写 brief 的 `bad_n == 2` / `unresolved.len() == 2`，根因 Step 5 的 handles 段把两类坏串当一类）：
    // 落地的分型是**两类**——形状非法（`nonsense:a`，`MemKey::parse` 认不出表名）⇒ 构造期拒收并计数；
    // 形状合法但查不到（`card:zz`，parse 只要求前缀过 `MemTable::parse` 且冒号后非空）⇒ 进
    // `unresolved_handles`。与本文件 `查不到的句柄进_unresolved…` 与 `合法句柄与查不到的混在一批…`
    // 两条正在绿的用例同一口径（`:68` 断 rejected==1、`:100` 断 rejected==0）。
    let raw = vec![k.key_string(), "card:zz".to_string(), "nonsense:a".to_string()];
    let (req, bad_n) = RecallRequest::handles(&raw).unwrap();
    assert_eq!(bad_n, 1, "坏串的计数是这条判据的读数，不是注释（形状非法的那一条）");
    let out: serde_json::Value = serde_json::from_str(&recall(m, &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();
    assert_eq!(out["rows"].as_array().unwrap().len(), 1, "坏串被当成了别的行");
    // 形状合法但查不到的那一条单独点名进 unresolved；两类坏串各归各的读数，谁也不许静默。
    assert_eq!(out["unresolved_handles"].as_array().unwrap().len(), 1);
}

#[test]
fn 幽灵句柄返回空而不是一条相近的行() {
    // C2 那条硬语义的后半（架构 §6 消费侧②）："悬空表态比没有表态更坏"的前一道门。
    let dir = tempfile::tempdir().unwrap();
    one_card(dir.path(), "先跑测试再合并"); // 库里有一条标题相近的行
    let root = Root::open(dir.path()).unwrap();
    let ghost = format!("card:{}", "f".repeat(64));
    let (req, _) = RecallRequest::handles(std::slice::from_ref(&ghost)).unwrap();
    let out: serde_json::Value =
        serde_json::from_str(&recall(root.memory(), &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();
    assert_eq!(out["rows"].as_array().unwrap().len(), 0, "幽灵句柄返回了最近邻");
    assert_eq!(out["unresolved_handles"][0], ghost);
}

#[test]
fn 不存在的合法句柄_vote_拒收且不留悬空表态() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    one_card(dir.path(), "先跑测试再合并");
    let ghost = format!("card:{}", "e".repeat(64));
    let e = vote(m, &ghost, VoteStance::Rejected, None, Some("重复"), NOW).unwrap_err();
    assert_eq!(e.kind, AgentErrorKind::Empty, "{e:?}");
    assert_eq!(e.code, "row_absent");
    assert_eq!(rc_memory::reader::dangling_attestations(m).unwrap(), 0, "先写后查 ⇒ 悬空表态留下了");
    // 形状非法的句柄与不存在的句柄是两个码（改输入 / 承认它没了，人下一步做的不同）。
    // 裁定 R46③（改写 brief 的实参）：`card:zz` 形状**合法**（parse 只要求表名可认 + 冒号后非空），
    // 走的是上一条的 row_absent；真非法的串是 `nonsense:a`（表名认不出），它才落 `bad_handle`。
    // 根因 Step 5 的 handles 段把两类坏串当一类；brief Step 5 段照抄。下面两句按裁定原样未动即可达。
    let e2 = vote(m, "nonsense:a", VoteStance::Rejected, None, None, NOW).unwrap_err();
    assert_eq!(e2.code, "bad_handle");
    assert_ne!(e2.code, e.code);
}

#[test]
fn status_的悬空表态读数_读得出打在幽灵_card_句柄上的表态() {
    // 工单 R51（评审 Important #2）：`tools.rs:209` 那格 `dangling_attestations` 此前**没有非零读者**——
    // `tools_shape.rs:191` 断的是 0（那批夹具库里只有一张干净 card），本文件 `:175` 走的是
    // `reader::dangling_attestations` 直路不经 `status` ⇒ 把生产码写成常量 `0` 全仓绿。
    // 这一格是架构 §6"悬空表态比没有表态更坏"在**运维面**唯一可见的形状，所以这里给它一条活体读数。
    // 顺带：`reader::dangling_attestations` 那条 SQL 的 `card`/`anchor` 两支里，此前只有
    // 有行的那一支有夹具，本条是"句柄形状合法、行不存在"那一支的第一个夹具
    // （canonical 之前这一支用的是 `event` 表名，旧表已并入 `card`）。
    //
    // 为什么用 `rc_memory::attest` **直写**而不走 `vote`：agent 面构造不出悬空表态
    // （幽灵句柄在 `vote` 那道"先认行再写"的门上被 `row_absent` 拒收，正由上一条钉住）。
    // 绕过那道门才是本条要的形状 ⇒ 生产码一行不改，测试直写。
    let dir = tempfile::tempdir().unwrap();
    one_card(dir.path(), "先跑测试再合并");
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 形状合法的幽灵 card 句柄（表名可认 + 冒号后非空 ⇒ `MemKey::parse` 收），但行不存在。
    let ghost = MemKey::parse(&format!("card:{}", "d".repeat(64)))
        .expect("card:<64 hex> 是合法句柄形状；parse 认不出就是 keys.rs 漂了");
    // 免锚的负面表态：`attest` 只校验锚不校验行存在（那正是悬空表态能存在的根因）。
    rc_memory::attest(m, &ghost, rc_memory::Stance::Rejected, None, Some("重复"), NOW)
        .expect("免锚负面表态打在合法句柄上应当落库");
    // ① status 面（工单点名的非零读者）：常量 `0` 会红在这一句。
    let out: serde_json::Value =
        serde_json::from_str(&status(m, None, &StatusOptions::default(), NOW).unwrap().text()).expect("status 必须返回合法 JSON");
    assert_eq!(out["dangling_attestations"], 1, "status 没读出那条打在幽灵 card 上的表态");
    // ② 直路面（本文件 `:175` 只有零读数）：两条路必须给同一个数，否则 status 是抄的不是读的。
    assert_eq!(
        rc_memory::reader::dangling_attestations(m).expect("直路读数不许是错误"),
        1,
        "直路与 status 的分歧 ⇒ 其中一格是常量：{}",
        out["dangling_attestations"]
    );
}

#[test]
fn verdict_的取值在_recall_面读得出来_不是常量() {
    // 裁定 R45 第二段：`row_json` 里那个 `"verdict": f.verdict.as_str()` 有读者
    // （`recall_cap.rs:126` 断它"与库里那一列同源"），但**取值**没有——全仓走 `row_json` 的夹具
    // 那一列恒为 `Unreviewed`（`set_verdict` 在 `rc-agent` 只出现在 `block_cap.rs:78`，而那条走
    // `probe_view` 不进 `row_json`）⇒ 把 `src/recall.rs:83` 写成常量 `"unreviewed"` 全仓不红。
    // 那是任务 6 的 `opens_mouth` 同一族的形状：断言在，被测分支零读数。
    // 所以这里放**两行**：一行经生产 API `set_verdict` 判成 `Passed`，一行没被复核。
    // 两个期望值都是字面量 ⇒ "把那一列渲染成任何一个常量"必然红在其中一行上（两个方向都有牙）。
    let dir = tempfile::tempdir().unwrap();
    let passed = one_card(dir.path(), "被判过的那一条");
    let unreviewed = one_card(dir.path(), "没被复核的那一条");
    let root = Root::open(dir.path()).unwrap();
    rc_memory::set_verdict(root.memory(), &passed, rc_memory::Verdict::Passed, NOW).unwrap();
    let raw = vec![passed.key_string(), unreviewed.key_string()];
    let (req, rejected) = RecallRequest::handles(&raw).unwrap();
    assert_eq!(rejected, 0, "两条句柄都合法却被计数了");
    let out: serde_json::Value =
        serde_json::from_str(&recall(root.memory(), &req, &RecallFilter::empty(), NOW).unwrap().text()).unwrap();
    let rows = out["rows"].as_array().expect("rows 必须是数组");
    assert_eq!(rows.len(), 2, "夹具没被逐行召回，下面那两句就成了空的");
    // 逐行取 (句柄, verdict, form) 三个身份值（守卫 12：消息里只有句柄与这两个 slug，不带正文）。
    // 按句柄定位而不是按下标：句柄臂的行序不是本条的判据。
    let mut readings: Vec<(String, String, String)> = Vec::new();
    let mut silent_ids: Vec<String> = Vec::new();
    for r in rows {
        let id = r["id"].as_str().expect("每行都带句柄");
        let verdict = r["verdict"].as_str().expect("verdict 必须是字符串（缺键 = 复核结论读不出来）");
        let form = r["form"].as_str().expect("form 必须是字符串（缺键 = agent 面读不出形态）");
        readings.push((id.to_owned(), verdict.to_owned(), form.to_owned()));
        if !r["silence_reason"].is_null() {
            silent_ids.push(id.to_owned());
        }
    }
    let p = passed.key_string();
    let u = unreviewed.key_string();
    assert!(
        readings.contains(&(p.clone(), "passed".to_string(), "full".to_string())),
        "set_verdict(Passed) 在 recall 面没读出 passed/full：{readings:?}"
    );
    assert!(
        readings.contains(&(u.clone(), "unreviewed".to_string(), "signpost".to_string())),
        "没被复核的那一行读出了别的取值 ⇒ 上一句是常量喂出来的假绿：{readings:?}"
    );
    assert!(silent_ids.is_empty(), "两行都该开口，却有行带着沉默原因：{silent_ids:?}");
    // 刻意**不**断 `low_score` 在这一面可达（裁定 R42 (乙)：`row_json` 让分数恒过线，
    // 那一格只在注入面 `probe_view` 报）。写了就是自造一条永久红的用例。
}

#[test]
fn 低分沉默的行在_recall_的查询臂里仍可查() {
    // G6 判据③的后半（前半在任务 6 的 `低分沉默的行仍在_recall_面上可查`）。
    // 三行顺序是刻意的：先自证它在闸门上确实沉默（真分数），再断工具面查得到，最后断本面**不**替它报原因。
    // 少了第一行，这条用例退化成"recall 能查到东西"——那什么都没断。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 偏离 brief（与前半 `低分沉默的行仍在_recall_面上可查` 里那条已登记的偏离同一口径）：
    // 原文的 3000 落在 `score == 1 == FLOOR`（实测 `matched=5, row_len=3004 ⇒ 1024*5/3004 == 1`），
    // `above_floor` 仍为真 ⇒ 这一行根本不进 LowScore 格，下面两句（score == 0、格子 == LowScore）
    // 当场红在正确的实现上。0 分的条件是 `row_len > 1024·matched`，所以垫到 6000——
    // 与那条用例同一个数、同一个理由。这不是放宽判据：它要断的"低分行在工具面仍可查"一行没少。
    let k = rc_memory::synth::seed_padded(m, NOW, 6000).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let s = rc_search::rank::rank(m, None, NOW, "长行低分卡 记录")
        .unwrap()
        .into_iter()
        .find(|x| x.key == k)
        .expect("长行没进候选池");
    assert_eq!(s.score, 0, "先确认它确实满足低分");
    assert_eq!(
        rc_search::gate::decide(&s, NOW).cell,
        Some(rc_search::gate::GateCell::LowScore),
        "它在闸门上不落 LowScore，后面那句'仍可查'断的是别的东西"
    );

    let out: serde_json::Value = serde_json::from_str(
        &recall(m, &RecallRequest::query("长行低分卡 记录").unwrap(), &RecallFilter::empty(), NOW)
            .unwrap()
            .text(),
    )
    .unwrap();
    let hit = out["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str() == Some(k.key_string().as_str()))
        .expect("低分行在 recall 的工具面上消失了 ⇒ 过滤被挪进了 rc-agent 的查询臂");
    // 裁定 R44 的改写口径（承 R42 (乙)）：低分这一格**只在注入面 `probe_view` 报**，因为那里分数是
    // 真的；工具面让分数恒过线（`recall::decide_no_query` 的 `score: i64::MAX`），所以 `GateCell::LowScore`
    // 在本面结构上不可达。brief 逐字给的 `== "low_score"` / `== "silent"` 两句因此是**永久红**，
    // 不是实现坏了。这里断的是同一行在两个面上各自怎么被说出：
    // 闸门上它确实是 LowScore（上面两句用真分数核过），而工具面把它当普通新鲜行给——
    // 原因位恒空、形态与本面任何一条未否决新鲜行相同（同文件那条对账用例读到的同一个值）。
    // 牙：把真分数带进本面（改 `decide_no_query` 的 `score`）⇒ 第一句红；
    // 把闸门过滤挪进查询臂 ⇒ 上面那句 `expect` 或第二句红。"报原因 = low_score"那一半的家在
    // `rc-search/tests/causes.rs` 的 3a，本面再抄一遍就是两份尺子。
    assert_eq!(
        hit["silence_reason"],
        serde_json::Value::Null,
        "(乙) 口径：低分不在工具面报原因；这里读出非空就说明有人在两个面上各排了一遍闸门"
    );
    assert_eq!(
        hit["form"],
        "signpost",
        "低分行在本面被渲染成了别的形态 ⇒ 沉默判定被挪进了工具面"
    );
}
