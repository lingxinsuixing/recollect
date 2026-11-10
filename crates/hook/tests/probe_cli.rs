// 子进程夹具的家住在 `tests/common/mod.rs`（P-65 hook 半边，任务 11 迁出），
// 本文件的 17 条用例**逐字不动**——判据断的是 CLI 形状而非夹具。
mod common;
use common::*;
use rc_memory::reader::lookup;
use rc_memory::{
    put_card, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict,
};
use rc_store::AgentRoot;
// `.lines()` 在 `Output::stdout`（`Vec<u8>`）上的实现来自 `BufRead`，迁移后必须仍在本文件
// 显式带进来（公共名走 glob 不到 trait）——六处断言逐字未动。
use std::io::BufRead;
use std::process::Command;

#[test]
fn probe_出声是一行带围栏_退出码0() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let out = hook(&["probe", "--data-dir", data.to_str().unwrap(), &now_arg(), "--query", TOK]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "signpost 必须一行：{text}");
    assert!(text.starts_with("[recollect] "), "缺围栏：{text}");
    // 人工锚 + 无佐证 ⇒ signpost（指路），**不许带正文**。
    assert!(!text.contains("会被提示面召回"), "signpost 泄露正文：{text}");
}

#[test]
fn probe_沉默时stdout是零字节() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    // 负样本用 ASCII 合成词，不用中文短语：中文在 jieba 下必然切出与夹具共享的功能词
    // （`的`、`合成` 这类），而闸门下限是 1 个共享词元就出声 ⇒ 中文负样本对任何实现都永红。
    // ASCII 连续串与夹具的中文标题/正文在任何分词法下都没有公共词元。
    let out = hook(&[
        "probe", "--data-dir", data.to_str().unwrap(), &now_arg(),
        "--query", "zzqqxxsynthetic",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // V3 的等价关系：沉默 ⇔ 零字节。多写一个换行都会让宿主把"沉默"读成"空的一行"。
    assert_eq!(out.stdout, b"", "沉默必须零字节，实到：{:?}", out.stdout);
}

#[test]
fn 冷却端到端_第二次沉默_窗口过了又出声() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let k = seed(&data);
    let d = data.to_str().unwrap();

    let first = hook(&["probe", "--data-dir", d, &now_arg(), "--query", TOK]);
    assert_eq!(first.stdout.lines().count(), 1, "第一次必须出声");

    // 列真的写了：回执即判据（任务 3 证伪 5 同族），不看 stderr 看库。
    let root = AgentRoot::open(&data).unwrap();
    let f = lookup(root.memory(), &k).unwrap().unwrap();
    assert_eq!(f.cooldown_until_ms, Some(NOW + rc_memory::COOLDOWN_MS), "冷却列没写对");

    let second = hook(&["probe", "--data-dir", d, &now_arg(), "--query", TOK]);
    assert!(second.status.success());
    assert_eq!(second.stdout, b"", "冷却中的第二次必须沉默");

    let third = hook(&["probe", "--data-dir", d, &past_arg(), "--query", TOK]);
    assert_eq!(third.stdout.lines().count(), 1, "窗口过了要能重新出声");
}

#[test]
fn 索引未建时probe响亮退出3() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    // 只播种不 rebuild ⇒ `IndexError::NotBuilt`。
    let root = AgentRoot::open(&data).unwrap();
    put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: format!("{TOK} 未建索引"),
            body: "本条不重建索引。".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    drop(root);
    let out = hook(&["probe", "--data-dir", data.to_str().unwrap(), &now_arg(), "--query", TOK]);
    // 这条是"沉默即空输出"唯一的例外理由：索引没建与库里没东西必须字节可分（G7 / §11 D9）。
    assert_eq!(out.status.code(), Some(3), "索引未建必须运维失败");
    assert!(out.stdout.is_empty(), "运维失败时不许同时出声");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("index_not_built"), "stderr 要给稳定码：{err}");
    assert!(err.contains("hub index rebuild"), "stderr 要给下一条命令：{err}");
    let tmp = dir.path().display().to_string();
    assert!(!err.contains(&tmp), "stderr 泄露路径：{err}");
}

// 名字自任务 5 起只说对一半，按控制器裁定**保留原名**（仓里有按名 `--exact` 点用例的先例，
// 且 docs/plans/2026-10-08-phase1-retrieval-injection.md 嵌着旧用例原文，改名会孤儿化历史锚点）：
// `--query "   "` 那一臂（操作员显式给空白）**仍是 2**，宿主 stdin 那一臂（读成功却没词）
// **现在走沉默 0 + 零 stdout**。两臂各自的收据见体内注释与 task-5-report.md §6。
#[test]
fn 空查询两条入口都是2() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let d = data.to_str().unwrap();
    let by_flag = hook(&["probe", "--data-dir", d, &now_arg(), "--query", "   "]);
    assert_eq!(by_flag.status.code(), Some(2), "空查询是输入不可用，不是运维失败");
    assert!(by_flag.stdout.is_empty());
    let by_stdin = hook_stdin(&["probe", "--data-dir", d, &now_arg()], "   ");
    // 任务 5 起两条入口**不再同码**：`--query` 是操作员显式给的空白（"输入不可用"= 2 那一臂不动），
    // 而 stdin 那一臂是"宿主读成功却没词"= 宿主没话 ⇒ 沉默 0。用例名保留历史形状——名字里的
    // "都是 2"现在只对 `--query` 那一臂成立，改期望值的收据逐条列在 task-5-report.md §6。
    assert_eq!(by_stdin.status.code(), Some(0), "宿主 stdin 读成功而无词 = 沉默，不许阻断宿主");
    assert!(by_stdin.stdout.is_empty(), "沉默 = 零字节，实到：{:?}", by_stdin.stdout);
    // 2 与 3 必须可分（阶段 0 hub 那条理由原样成立）：坏提示重试一万次还是坏提示。
    let err = String::from_utf8_lossy(&by_flag.stderr).to_string();
    assert!(err.contains("empty_query"), "stderr 要给稳定码：{err}");
}

// 名字自任务 5 起只说对一半，按控制器裁定**保留原名**（同上：改名孤儿化历史锚点）。三臂现在的
// 读数：第一臂（宿主信封 + 有词 prompt）出声 rc 0；**第二臂（prompt 空串）现在走沉默 0 + 零字节**，
// 它不再有任何一臂是 2——"空 prompt 不退化成整块"这个判据从退出码换成了 stdout 字节（见那一臂上方）；
// 第三臂（非 JSON 裸文本兜底）出声 rc 0。仍然守 2 的是另外两条用例（`空查询两条入口都是2` 的
// `--query` 臂、`stdin解不开…` 的第一向），不在这个名字里。
#[test]
// 用例名逐字取 brief（本档 Step 4）的大写 `JSON` 形式：这个名字是派发契约点名的验收项
// （第二段与任务 11 按名 `--exact` 点它），所以放行的是 rustc 的 `non_snake_case` 风格 lint，
// 不是命名纪律。与 `rc-agent/tests/export_doc.rs`、`rc-agent/tests/tools_shape.rs` 里两处
// 同名判据的放行一致（本仓 `cargo clippy --workspace --all-targets -- -D warnings` 是 gate
// ⇒ 不放行就编译不过）。
#[allow(non_snake_case)]
fn 宿主stdin的JSON只取prompt字段() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let d = data.to_str().unwrap();
    // 正常宿主形状：整块是 JSON，提示词在 `prompt` 里。
    let blob = format!(
        r#"{{"session_id":"ses-synthetic","transcript_path":"/no/such/path.db","prompt":"{TOK} 该怎么走"}}"#
    );
    let out = hook_stdin(&["probe", "--data-dir", d, &now_arg()], &blob);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.lines().count(), 1, "取到 prompt 就该出声：{:?}", out.stdout);

    // `prompt` 是空串 ⇒ **不许**退化成"整块当查询"（那会把 session_id 和路径喂进分词器）。
    // 判据（任务 5 · 控制器裁定）：**这一臂测的是"不许把整块信封当查询"——退化实现会出声
    // （整块里的 TOK 命中播种的 card ⇒ stdout 非零），正确实现是沉默（零 stdout + rc 0），
    // 所以用字节分，不用 rc 分。** 旧写法断 `Some(2)`，那是把"空输入必被 2 挡出来"当代理读数；
    // 宿主语境下空输入现在本身就走沉默 0，于是 rc 在"正确沉默"与"退化成整块但整块没命中"
    // 之间不可分——只有"有没有出声"还能分，所以换成零字节，比原来更硬而不是更松。
    // 时钟必须走 past_arg：上面一臂已按 V7 给这条 card 盖了冷却，同 `--now-ms` 复跑的话
    // "没出声"说的是冷却而不是 stdin 解析，那一格就会偶然通过。
    // 命中词放在 `session_id` 里（不是 prompt），正是为了让"退化成整块"这一步可观测。
    let empty_blob = format!(r#"{{"session_id":"{TOK}","prompt":""}}"#);
    let empty = hook_stdin(&["probe", "--data-dir", d, &past_arg()], &empty_blob);
    assert_eq!(empty.status.code(), Some(0), "空 prompt 必须沉默，不许阻断宿主；stderr：{}", String::from_utf8_lossy(&empty.stderr));
    assert_eq!(empty.stdout, b"", "空 prompt 却出声了 = 整块信封被当成了查询：{:?}", String::from_utf8_lossy(&empty.stdout));

    // 不是 JSON ⇒ 整块原样当查询（兜底给"宿主直接喂裸文本"的那种配置）。
    // 时钟必须走到冷却窗外：第一次调用已按 V7 给这条 card 盖了冷却，同 `--now-ms` 复跑必然
    // 沉默，那一格沉默说的是冷却而不是 stdin 解析。
    let raw = hook_stdin(&["probe", "--data-dir", d, &past_arg()], TOK);
    assert!(raw.status.success(), "{}", String::from_utf8_lossy(&raw.stderr));
    assert_eq!(raw.stdout.lines().count(), 1, "裸文本兜底没出声：{:?}", raw.stdout);
}

#[test]
fn 另两个库是坏文件或被删掉时probe输出不变() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let d = data.to_str().unwrap();
    // 守卫 6 的运行时那一半：hook 只要去 open 这两个文件，`file is not a database` 当场红。
    std::fs::write(data.join("transcript.db"), b"definitely not sqlite").unwrap();
    std::fs::write(data.join("ledger.db"), b"definitely not sqlite").unwrap();
    let with_garbage = hook(&["probe", "--data-dir", d, &now_arg(), "--query", TOK]);
    assert!(with_garbage.status.success(), "{}", String::from_utf8_lossy(&with_garbage.stderr));
    assert_eq!(with_garbage.stdout.lines().count(), 1, "坏文件在场时第一次该出声");

    // 再把两个文件整个删掉，并越过冷却窗口让这一轮同样出声：**两次字节必须相同**。
    std::fs::remove_file(data.join("transcript.db")).unwrap();
    std::fs::remove_file(data.join("ledger.db")).unwrap();
    let without = hook(&["probe", "--data-dir", d, &past_arg(), "--query", TOK]);
    assert!(without.status.success(), "{}", String::from_utf8_lossy(&without.stderr));
    assert_eq!(without.stdout, with_garbage.stdout, "文件在不在改变了输出 ⇒ hook 打开过它们");
}

#[test]
// 名字里的大写 `CLI` 逐字取 brief（本档 Step 4）：它是这条判据的名字（C2 三行上限在 CLI 上成立），
// 改名等于把验收项换掉，所以放行 `non_snake_case`（同 `宿主stdin的JSON只取prompt字段` 那条的理由）。
#[allow(non_snake_case)]
fn 两条full在CLI上仍只占两行() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    {
        let root = AgentRoot::open(&data).unwrap();
        let m = root.memory();
        for i in 0..2 {
            let k = put_card(
                m,
                &FactContent {
                    kind: "decision".into(),
                    title: format!("{TOK} 的口径 第{i}条"),
                    body: "这是一条足够长以便看清两行形状的合成正文。".into(),
                    scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
                    valid_to_ms: None,
                },
                &[Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }],
                NOW,
            )
            .unwrap();
            set_verdict(m, &k, Verdict::Passed, NOW).unwrap();
        }
        rc_index::rebuild(m, NOW).unwrap();
    }
    let out = hook(&["probe", "--data-dir", data.to_str().unwrap(), &now_arg(), "--query", TOK]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // full = 2 行；两条 full 装不进 3 行 ⇒ `fit` 溢出即停 ⇒ 只有 2 行出声。
    assert_eq!(text.lines().count(), 2, "C2 上限在 CLI 上不成立：{text}");
    assert!(text.lines().all(|l| l.starts_with("[recollect] ")), "有行缺围栏：{text}");
}

#[test]
// 名字里的大写 `brief`/`JSON` 逐字取 brief（本档 Step 4）：`brief` 是这一档两条 stdout 面之一的
// 契约名，`JSON` 是它的输出形状，改名会丢掉验收点名的那一格 ⇒ 放行 `non_snake_case`。
#[allow(non_snake_case)]
fn brief是带围栏的一行JSON_索引未建也成功() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let out = hook(&["brief", "--data-dir", data.to_str().unwrap(), &now_arg()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "brief 必须一行：{text}");
    // 与 `rc_agent::surface::status_block` 同形。这是"两个门面共用一个构造器"在 CLI 侧的一半证据，
    // 另一半在任务 10（它拿 mcp 的 bootstrap 字节与这里对撞）。
    let payload = text.trim().strip_prefix("[recollect] ").expect("缺围栏");
    let v: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["index"]["built"], false, "没建索引时 brief 仍要报 built:false");
    for banned in ["\"title\"", "\"body\""] {
        assert!(!payload.contains(banned), "brief 出现正文字段 {banned}：{payload}");
    }
}

#[test]
fn 副本上probe照样出声_而冷却一格都没盖() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let k = seed(&data);
    let snap = dir.path().join("snapshot.db");
    std::fs::copy(data.join("memory.db"), &snap).unwrap();
    let out = hook(&[
        "probe", "--data-dir", data.to_str().unwrap(),
        "--replica", snap.to_str().unwrap(), &now_arg(), "--query", TOK,
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.lines().count(), 1, "副本读路径必须照常出声");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("replica_read_only"), "副本上没交代冷却为何跳过：{err}");
    // 原库一格都没动：证明"写跳过"不是"写到了别处"。
    let root = AgentRoot::open(&data).unwrap();
    assert_eq!(lookup(root.memory(), &k).unwrap().unwrap().cooldown_until_ms, None);
}

/// 裁定 R56 的新增用例（本段对工单的第二处偏离，逐字点名在 `task-9b1-report.md` §偏离 D2）。
///
/// **S2 之后这条用例的判据换了，强度没有降**。R56 当时可达的坏形状是：实体形态被注入
/// ⇒ `stamp_cooldown` 的 `keys` 里有 `MemTable::Anchor` ⇒ 而 `bump_cooldown` 撞见它是
/// **整批 `return Err(MemError::NotFound)`**（`crates/rc-memory/src/writers_p.rs`：
/// 是 `return` 不是 `continue`，本体表没有 `cooldown_until_ms` 列）
/// ⇒ 同批 card 的冷却一格都没盖、每轮重复注入。
/// 可达链逐环：`reader::facts` 的 `ROW_TABLES` 含 Anchor ⇒ `rc_index::rebuild` 把每一行
/// 都灌进影子索引（实体"标题即全部可检索内容"）⇒ `decide` 把 `s.key` 原样带出。
///
/// `probe_view` 不再放锚进候选之后，这条链在 CLI 上**已经走不到**（`view.forms` 里不会有
/// anchor）——所以旧夹具前提"实体排批首"随之失效，本条如实改成新契约的三个读数：
/// ① 同一次查询同时命中实体与 card 时，注入块**只有 card 一行**：锚不吃那 3 行额度
///    （修复前这一格是 3 行锚、真卡被 `fit` 溢出即停地挤出去）；
/// ② card 的冷却照旧盖上（混着实体也不许把同批的写打成整批失败）；
/// ③ 第二轮零字节：锚不进候选之后，"第二次沉默"才第一次对本夹具成立
///    （旧注释里"实体第二轮照旧出声"是旧契约，已不成立）。
#[test]
fn entity_形态混在批里时_card_的冷却照盖() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let k = seed(&data);
    let ek = {
        let root = AgentRoot::open(&data).unwrap();
        let m = root.memory();
        let ek = rc_memory::put_entity(
            m,
            &rc_memory::EntityContent {
                kind: "person".into(),
                // 标题即全部可检索内容：查询词就点在这里。
                title: TOK.into(),
                scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
                aliases: vec![],
            },
            &[Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
            NOW,
        )
        .unwrap();
        rc_index::rebuild(m, NOW).unwrap();
        ek
    };
    let d = data.to_str().unwrap();

    // ① 第一次：退出码 0，且注入块里只有 card 一行（anchors 不进候选）。
    let first = hook(&["probe", "--data-dir", d, &now_arg(), "--query", TOK]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let text = String::from_utf8(first.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "锚占了注入额度：{lines:?}");
    assert!(lines[0].contains(&k.key_string()), "那一行必须是 card：{lines:?}");
    assert!(!text.contains("anchor:"), "锚行进了注入块：{text}");
    assert!(!text.contains(&ek.key_string()), "实体句柄进了注入块：{text}");

    // ② 冷却列真盖上了：这一格就是"同批的写没有被整批打回"的读数。
    let root = AgentRoot::open(&data).unwrap();
    let f = lookup(root.memory(), &k).unwrap().unwrap();
    assert_eq!(
        f.cooldown_until_ms,
        Some(NOW + rc_memory::COOLDOWN_MS),
        "冷却列没写对：同批若有锚句柄，bump_cooldown 会整批 Err(NotFound) ⇒ card 一格都没盖"
    );
    drop(root);

    // ③ 第二次：那条 card 不再被重复注入。这里是**零字节**（V3 的沉默形状），
    // 因为本夹具里唯一能出声的那一行（card）已经在冷却里，锚又不在候选里。
    let second = hook(&["probe", "--data-dir", d, &now_arg(), "--query", TOK]);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(
        second.stdout, b"",
        "第二次仍出声（冷却没生效或锚又回了候选）：{:?}",
        String::from_utf8_lossy(&second.stdout)
    );
}

/// S2 的 CLI 读数：**锚不吃 3 行额度**。
/// 夹具是缺陷现场的最小复现：1 张卡 + 3 个同名前缀的锚，查询词同时点中两者。
/// 修复前 `rank` 把锚排在前（只有标题 ⇒ `row_len` 小 ⇒ 同分靠前），3 行额度被 3 个锚占满，
/// 真卡连一行都拿不到；所以这条用例的红点有两格，且互相独立：
/// ① `!stdout.contains("anchor:")`（锚句柄进了注入块）；
/// ② `stdout.contains(<card 句柄>)`（卡被 `fit` 挤掉时它是空的）。
/// ② 不可省：只断言"没有 anchor:"的话，把注入面整条关掉也能让它变绿。
#[test]
fn probe_查询点中锚时_三行额度不被锚吃掉() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let card = {
        let root = AgentRoot::open(&data).unwrap();
        let m = root.memory();
        let scope = Scope { kind: ScopeKind::Project, key: "proj-anchor-budget".into() };
        let prov = [Provenance { turn_uid: "d".repeat(64), anchor_role: AnchorRole::Human }];
        let card = put_card(
            m,
            &FactContent {
                kind: "decision".into(),
                title: "锚预算合成词 的口径".into(),
                body: "锚预算正文 这一句只在卡里。".into(),
                scope: scope.clone(),
                valid_to_ms: None,
            },
            &prov,
            NOW,
        )
        .unwrap();
        for name in ["锚预算合成词", "锚预算合成词-prod", "锚预算合成词-ops"] {
            rc_memory::put_entity(
                m,
                &rc_memory::EntityContent {
                    kind: "place".into(),
                    title: name.into(),
                    scope: scope.clone(),
                    aliases: vec![name.into()],
                },
                &prov,
                NOW,
            )
            .unwrap();
        }
        rc_index::rebuild(m, NOW).unwrap();
        card
    };
    let d = data.to_str().unwrap();

    let out = hook(&["probe", "--data-dir", d, &now_arg(), "--query", "锚预算合成词"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    // 3 行是**硬**额度（`MAX_FORM_LINES`）：锚行曾经把它占满，真卡被 `fit` 挤掉。
    assert!(text.lines().count() <= 3, "注入块超过 3 行：{text}");
    assert!(!text.contains("anchor:"), "锚行进了注入块：{text}");
    assert!(
        text.contains(&card.key_string()),
        "真卡被锚挤出了注入块：{text}"
    );

    // 后半：卡片自己的词照常出声——过滤只认"锚不进候选"，不是把这一面整体关掉。
    // 时钟走到冷却窗外：第一次调用已按 V7 给这张卡盖了冷却，同 `--now-ms` 复跑必然沉默，
    // 那一格沉默说的是冷却而不是过滤（与 `冷却端到端` 同一个理由）。
    let card_only = hook(&["probe", "--data-dir", d, &past_arg(), "--query", "锚预算正文"]);
    assert!(card_only.status.success(), "{}", String::from_utf8_lossy(&card_only.stderr));
    let card_text = String::from_utf8(card_only.stdout).unwrap();
    assert!(
        card_text.contains(&card.key_string()),
        "卡片词查不到那张卡（过滤误伤了卡）：{card_text}"
    );
}

// ---- R74–R81（任务 9 修复轮新增的六条：给注入面那些"沉默的臂"第一批读者）----

/// R74（评审 I-1 最低档）：`store unreachable — run: doctor` 这一支（brief/probe 各一处）
/// 在全仓 `tests/**` 里零命中 ⇒ "守卫 12 的零路径"在这一支上从来没有读数。
/// 夹具用**普通文件当 `--data-dir`**——`AgentRoot::open` 撞上的就是这一支。
#[test]
fn data_dir是个普通文件时probe响亮退3_零路径() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_dir = dir.path().join("afile");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let out = hook(&[
        "probe", "--data-dir", not_a_dir.to_str().unwrap(), &now_arg(), "--query", TOK,
    ]);
    // ① 库开不通是运维失败（3），不是"输入不可用"（2）：坏提示重试一万次还是坏提示，
    //    而库开不通重试可能就是好的。
    assert_eq!(out.status.code(), Some(3), "实到 {:?}，stderr：{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    // ② 运维失败**绝不同时出声**（V3 的沉默 ⇔ 零字节在失败面上同样成立）。
    assert!(out.stdout.is_empty(), "运维失败却出声了，实到：{:?}", out.stdout);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    // ③ 稳定串：这一支的 next_action 是"查库/跑 doctor"，不是"改提示词"。
    assert!(err.contains("store unreachable"), "stderr 没落到 store unreachable：{err}");
    // ④ 守卫 12：类别名而已，`StoreError` 的原串可能带路径，不许外传（同 `索引未建` 那条探针）。
    let tmp = dir.path().display().to_string();
    assert!(!err.contains(&tmp), "stderr 泄露路径：{err}");
}

/// R75（评审 I-2）：18 次 CLI 调用全部带 `--now-ms`、`--data-dir` 全部显式传 ⇒ `clock` 的兜底
/// 那一支与 `default_value = "./data"` 两个形状都零读者。把兜底整段删掉今天仍然全绿，
/// 而副作用是缺省调用退 3——没有一条用例知道这件事。这条是生产默认形状第一次上刻度。
#[test]
fn 不传时钟不传data_dir时brief走生产默认形状() {
    let dir = tempfile::tempdir().unwrap();
    // `current_dir` 是这条用例的一半判据：`default_value = "./data"` 会在 **cwd** 下真的建出
    // `./data/memory.db`，不设进临时目录就会往仓库里写。
    let out = Command::new(bin()).current_dir(dir.path()).arg("brief").output().unwrap();
    // ① 墙钟兜底必须成功——不许退 3（"不传时钟不许退 3"那一格，brief 一样证得到）。
    assert!(out.status.success(), "缺省调用实到 {:?}，stderr：{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    // ② 墙钟下**内容**不定，但**形状**定：一行、带围栏 ⇒ 所以不断 JSON 字段。
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(text.lines().count(), 1, "brief 必须一行，实到：{text}");
    assert!(text.starts_with("[recollect] "), "缺围栏：{text}");
    // ③ 兜底成功时不许交代失败原因。
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!err.contains("clock unavailable"), "兜底没接上，stderr：{err}");
    assert!(dir.path().join("data").join("memory.db").exists(), "`default_value = \"./data\"` 没在 cwd 下建库");
}

/// R76（评审 I-3）：C3 的"必报"半边零读者，而 `MAX_PROBE_QUERY_CHARS` 上方的注释逐字自称
/// "本阶段的判据只有『超限仍出声且必报』"——注释比代码强。这条把两半边都钉上：
/// 填充必须放在 `TOK` **之后**，否则截断把 `TOK` 削掉，"仍出声"那一格就变成偶然通过。
#[test]
fn 超长查询仍出声且必报截断() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let long = format!("{}{}", TOK, "x".repeat(2400));
    let out = hook(&[
        "probe", "--data-dir", data.to_str().unwrap(), &now_arg(), "--query", &long,
    ]);
    // ① 长提示是**合法输入**，拒掉它等于"用户话越长、记忆越沉默"（G7 要防的形状）。
    assert!(out.status.success(), "实到 {:?}，stderr：{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    // ② "仍出声"：一行带围栏。填充词是 ASCII 连续串，与中文夹具无公共词元（同
    //    `probe_沉默时stdout是零字节` 那条的理由），所以出的这一行只能来自 `TOK`。
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(text.lines().count(), 1, "超限后不出声/多出，实到：{text}");
    assert!(text.starts_with("[recollect] "), "缺围栏：{text}");
    // ③ "必报"：截断只影响尾部的词能不能命中，而这个损失必须说出去。
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("query truncated"), "静默截断（C3 失效形状），stderr：{err}");
}

/// R77（评审 I-4）：`read_stdin` 把"读不出来"折成"没有输入"——两向共用一串 stderr、一个退出码，
/// 而它们的 next_action 一个是查宿主配置、一个是改提示词（撞的是 `rc-agent/src/error.rs` 写进
/// 码里的那条纪律：库读不到与库里没东西是两种分型）。
// 名字自任务 5 起只说对一半，按控制器裁定**保留原名**（同上：改名孤儿化历史锚点）：
// 第一向（解不开的字节）**仍是 2 + `stdin unread`**，第二向（能解开的空提示）**现在走沉默
// 0 + 零 stdout + 零 stderr**，所以"只报 empty_query"从此刻起是历史描述——两向现在靠退出码分。
#[test]
fn stdin解不开时报stdin_unread_空提示只报empty_query() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let d = data.to_str().unwrap();
    // 第一向：UTF-8 解不开的字节 ⇒ 必须出声说"读不出来"。
    // 实测前提（控制器复跑）：std 在 UTF-8 失败时**不**把可解的前缀留在 buf，所以这一向的
    // blob 是空的 ⇒ 退出码仍是 2，但 stderr 必须多一条 `stdin unread`。
    let bad = hook_stdin_bytes(&["probe", "--data-dir", d, &now_arg()], &[0xff, 0xfe, 0x80, 0x80]);
    assert_eq!(bad.status.code(), Some(2), "实到 {:?}，stderr：{}", bad.status.code(), String::from_utf8_lossy(&bad.stderr));
    let err = String::from_utf8_lossy(&bad.stderr).to_string();
    assert!(err.contains("stdin unread"), "读不出来被折成了没有输入：{err}");
    // 第二向（任务 5 改期望值）：能解开的空提示 ⇒ **沉默**（0 + 零 stdout + 零 stderr），
    // 而上面那一向（解不开的字节）仍是 2 + `stdin unread`。"分开症状"这一格没被削弱，反而更硬：
    // 两向现在连退出码都可分（2 vs 0），旧的"两向共用 2、只靠 stderr 串区分"才是含糊形状。
    // 原来这里断 `err2.contains("empty_query")`——沉默出口一个字节都不写，那条断言随期望值一起撤；
    // `!err2.contains("stdin unread")` 逐字保留，它才是"没有输入没被报成读不出来"的那个读数。
    let blank = hook_stdin_bytes(&["probe", "--data-dir", d, &now_arg()], b"   ");
    assert_eq!(blank.status.code(), Some(0), "实到 {:?}，stderr：{}", blank.status.code(), String::from_utf8_lossy(&blank.stderr));
    assert!(blank.stdout.is_empty(), "沉默 = 零字节，实到：{:?}", blank.stdout);
    let err2 = String::from_utf8_lossy(&blank.stderr).to_string();
    assert_eq!(err2, "", "沉默时 stderr 也该零字节，实到：{err2}");
    assert!(!err2.contains("stdin unread"), "没有输入被报成了读不出来：{err2}");
}

/// R80（评审 m-5）：用法错误那一支是三个 CLI 里唯一没被断的
/// （hub 的 import/rebuild 与 doctor 的 capabilities 都断了自己的 `invalid invocation`）。
#[test]
fn 用法错误响亮退3_不回显操作员敲的token() {
    let out = hook(&["probe", "--nope"]);
    // ① 用法错误给 3 不给 2：clap 定的 2 在这里专指"输入不可用"，敲错参数不该让操作员以为
    //    提示词是空的（分型本身已裁合规，这条只补读者）。
    assert_eq!(out.status.code(), Some(3), "实到 {:?}，stderr：{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    // ② 失败面零字节。
    assert_eq!(out.stdout, b"", "用法错误却出声了，实到：{:?}", out.stdout);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    // ③ 稳定串。
    assert!(err.contains("invalid invocation"), "stderr 没落到 invalid invocation：{err}");
    // ④ 不回显操作员输入（与 hub 同族纪律：用法文本自己拼，因为 clap 原串会回显敲进来的 token，
    //    而它常常是个路径）。
    assert!(!err.contains("--nope"), "回显了操作员敲的 token：{err}");
}

#[test]
fn a_probe_with_a_session_records_a_shard_and_without_one_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let k = seed(&data);
    let out = hook(&[
        "probe", "--data-dir", data.to_str().unwrap(), &now_arg(), "--query", TOK,
        "--session", "sess-synthetic",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // 分片是旁路读数：stdout 的字节不变（仍是一行围栏）。
    assert_eq!(out.stdout.lines().count(), 1, "分片写入改变了 stdout：{:?}", out.stdout);
    let inject = data.join("inject");
    assert!(inject.is_dir(), "带 --session 且出声 ⇒ 必须建出 inject/");
    let mut files: Vec<String> = std::fs::read_dir(&inject)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("session-") && n.ends_with(".ids"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 1, "一次会话只该有一个分片：{files:?}");
    assert!(!files[0].contains("sess-synthetic"), "文件名泄露了原会话串：{}", files[0]);
    let body = std::fs::read_to_string(inject.join(&files[0])).unwrap();
    assert!(
        body.lines().any(|l| l == k.key_string()),
        "分片里没有这次注入的句柄：{body:?}"
    );

    // 没给 --session ⇒ 零文件 I/O：目录都不许建。
    let dir2 = tempfile::tempdir().unwrap();
    let data2 = data_of(&dir2);
    seed(&data2);
    let plain = hook(&[
        "probe", "--data-dir", data2.to_str().unwrap(), &now_arg(), "--query", TOK,
    ]);
    assert!(plain.status.success(), "{}", String::from_utf8_lossy(&plain.stderr));
    assert_eq!(plain.stdout.lines().count(), 1);
    assert!(!data2.join("inject").exists(), "没给 --session 却建了 inject/");
}

/// R81（评审 m-7）的历史锚点：`stamp_cooldown` 里"批里全是实体形态 ⇒ `keys` 为空 ⇒ `Ok(0)` ⇒
/// 走第一支、不打 note"这一支曾被这条用例当成唯一读数。
///
/// **S2 之后它守的不是那一支了（修复轮 1，评审 Important-2 的更正）**：`probe_view` 不再产
/// 锚候选 ⇒ `main.rs` 的 `if !block.is_empty()` 在全实体库上为假 ⇒ `stamp_cooldown` **一次都
/// 不会被调用**；被调用时 `keys` 必非空（`view.forms` 的每一条都不是锚），于是
/// `Ok(0) if 0 == keys.len()` 那一支从 CLI 上**结构不可达**（`n == keys.len()` 要求两边同为零，
/// 而这里 len ≥ 1）。`crates/hook/src/main.rs` 里那个防御性的 `MemTable::Anchor` 过滤保留不动——
/// 它是给"手拼 `ProbeView`"的调用方留的，不靠 CLI 走到；`stamp_cooldown` 的单测按控制器裁定
/// 推迟到终审（Task 6/7），本轮不补。
///
/// 所以本用例现在守的是**另一件同样要守的事**：全锚库在注入面上是**正当的沉默**
/// （rc 0 + 零字节 + 零 stderr），既不是一根空转的 note，也不是一行假出声。
/// 谁把锚重新放回候选（实到一行 `anchor:…`），或把沉默写成一个换行（实到 `"\n"`），这里当场红；
/// 第二个读数（零 stderr）挡住"把正当无事可做报成故障"那一向。用例名保留（历史锚点按名指认它），
/// "no_op" 的含义随契约一起挪到"这一轮无事可做"。
#[test]
fn 全实体批是正当no_op_两轮都零stderr() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    let ek = {
        let root = AgentRoot::open(&data).unwrap();
        let m = root.memory();
        // **只播种一条 entity，不播种 card** ⇒ 候选池剔除锚之后是空的。
        let ek = rc_memory::put_entity(
            m,
            &rc_memory::EntityContent {
                kind: "person".into(),
                title: TOK.into(),
                scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
                aliases: vec![],
            },
            &[Provenance { turn_uid: "f".repeat(64), anchor_role: AnchorRole::Human }],
            NOW,
        )
        .unwrap();
        rc_index::rebuild(m, NOW).unwrap();
        ek
    };
    let d = data.to_str().unwrap();
    // 阳性对照（不是判据本身）：这条实体真的在检索面命中——沉默只能来自"锚不进候选"，
    // 不能来自"库里根本没有可检索的东西"。少了这一格，空库也能让下面两轮全绿。
    {
        let root = AgentRoot::open(&data).unwrap();
        let ranked = rc_search::rank::rank(root.memory(), None, NOW, TOK).unwrap();
        assert!(
            ranked.iter().any(|s| s.key == ek),
            "夹具前提：实体没在 rank 里命中 ⇒ 下面的沉默是空库，不是过滤"
        );
    }
    for round in 0..2 {
        let out = hook(&["probe", "--data-dir", d, &now_arg(), "--query", TOK]);
        assert!(out.status.success(), "第{round}轮实到 {:?}，stderr：{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
        // ① 沉默 = 零字节（V3）。锚行若回到候选，这里实到一行 `[recollect] anchor:…`。
        assert!(
            out.stdout.is_empty(),
            "第{round}轮全实体库出声了（锚又回了候选）：{:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        // ② "正当"体现在**零 stderr**：沉默不是回执缺失，也不是"0/0 部分盖章"的告警。
        assert_eq!(out.stderr, b"", "第{round}轮沉默被当成了故障，实到 stderr：{}", String::from_utf8_lossy(&out.stderr));
    }
}

// ---- Task 5（本机即中心 · Phase 2）：空查询在宿主语境下是沉默，不是阻断 ----

/// 宿主语境下"没有查询"是沉默，不是运维错误：Qoder 对 `UserPromptSubmit` 的
/// exit 2 = 阻断该 prompt，而记忆系统绝不允许挡住用户说话（设计 §8.2 约束 3）。
/// 夹具照本文件既有形状：库里** seeded 且建好索引**（`seed`），所以这一格的 0 只能来自
/// "空查询走沉默出口"，不可能来自"库开不通"或"没建索引"那两支。
#[test]
fn 空查询在宿主语境下沉默且退出码为零() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    seed(&data);
    let d = data.to_str().unwrap();
    // 灌宿主形状的信封，但 prompt 为空（附件-only / slash 命令 / resume 都是这一形状）。
    let out = hook_stdin(
        &["probe", "--data-dir", d, &now_arg()],
        r#"{"hook_event_name":"UserPromptSubmit","prompt":""}"#,
    );
    assert_eq!(out.status.code(), Some(0), "空查询必须沉默，不许阻断宿主；stderr：{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "沉默 = 零字节，实到：{:?}", out.stdout);
}

/// 反面：库开不通仍必须是 3（运维失败），不能被上面那条一起放宽。
#[test]
fn 库不可达仍是运维失败三() {
    let out = hook(&[
        "probe", "--data-dir", "Z:/definitely/not/here", &now_arg(), "--query", "任意词",
    ]);
    assert_eq!(out.status.code(), Some(3), "实到 {:?}，stderr：{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "运维失败时不许同时出声");
}

// ---- W37-A1（G12）probe 的 CLI 接线钉：`--agent-id` 构造的 viewer 真的进了读闸 ----

/// 异 agent 的 private 卡在带 `--agent-id` 的 probe 下**不开口**。这一格钉的是
/// hook/main.rs 的接线（`--agent-id` → `Viewer::agent` → `probe_via_search`）：
/// 若接线退化回 `None`（声明的旁路形状），private 卡必开口，这里必红。
/// hook 面没有 `--principal-id`：agent viewer 无 principal ⇒ 4b 永不命中（fail-closed），
/// 这正是 CLI 的既定形状——同 principal 互读的那半边钉在
/// `rc-agent/tests/recall_principal_gate.rs` 的 `probe_via_search` 面上。
/// owner 自读照旧开口，证明沉默来自读闸而不是检索/索引缺席。
#[test]
fn probe带agent_id时异agent的私卡不开口_owner自读照旧() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_of(&dir);
    // agent-A 的 private 卡（带 principal p1），夹具形状照 rc-agent 的 recall 集成臂。
    {
        let root = AgentRoot::open(&data).unwrap();
        let items = vec![rc_memory::writers_x::WriteItem {
            table: rc_memory::keys::MemTable::Card,
            kind: "note".into(),
            title: format!("{TOK} 的私卡"),
            body: format!("{TOK} 的一句合成正文"),
            scope: Scope { kind: ScopeKind::Project, key: "proj-probe".into() },
            valid_to_ms: None,
            aliases: vec![],
            prov: vec![Provenance { turn_uid: "w".repeat(64), anchor_role: AnchorRole::Human }],
        }];
        rc_memory::writers_x::put_batch_with_owner(
            root.memory(),
            &items,
            &rc_memory::writers_x::BatchCtx {
                source_agent: "agent-A".into(),
                device_id: "dev-A".into(),
            },
            &rc_memory::owner::OwnerCtx {
                owner_principal_id: Some("p1".into()),
                owner_agent_id: "agent-A".into(),
                visibility: rc_memory::owner::Visibility::PrivateAgent,
                retention: "default".into(),
            },
            NOW,
            &[],
        )
        .unwrap()
        .remove(0)
        .expect("夹具行不被抑制");
        rc_index::rebuild(root.memory(), NOW).unwrap();
    }
    let d = data.to_str().unwrap();

    // 异 agent：viewer 在场（无 principal）⇒ 4b 永不命中 ⇒ private 卡被拒 ⇒ 沉默零字节。
    // None 旁路若回来，这一格必开口（None = 不过闸）。
    let stranger = hook(&["probe", "--data-dir", d, &now_arg(), "--agent-id", "agent-B", "--query", TOK]);
    assert!(stranger.status.success(), "{}", String::from_utf8_lossy(&stranger.stderr));
    assert_eq!(stranger.stdout, b"", "异 agent 的 private 卡经 --agent-id 泄进了 probe：{:?}", stranger.stdout);

    // owner 自读：第 4 条放行，照旧出声——沉默确系读闸所给，不是行不在场。
    let owner = hook(&["probe", "--data-dir", d, &now_arg(), "--agent-id", "agent-A", "--query", TOK]);
    assert!(owner.status.success(), "{}", String::from_utf8_lossy(&owner.stderr));
    assert_eq!(owner.stdout.lines().count(), 1, "owner 自读的卡没出声：{}", String::from_utf8_lossy(&owner.stdout));
}
