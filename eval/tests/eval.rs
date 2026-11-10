//! C1 的五条判据（`scripts/check-phase5.sh` 的 4①–4⑤ 逐条对应）。
//!
//! 头两条用**手算值**钉住指标：4① 的 aggregate recall 是 5/6 而不是 1，所以任何
//! "把 recall@k 写成恒 1"的改动都会在这里红——一条聚合值恒为 1 的用例挡不住这类变异。
//! 4② 把真值集里的一个标题写错，recall 必须掉到 0（桩有牙）。
//! 4③ 让"沉默"与"反噪"落在同一个查询形状上，分别判定：沉默案例 `forms` 为空，
//! 反噪案例 `forms` 非空但干扰行不在里面——两条判据不是同一条串。
//! 4④ 删掉一行 provenance，从库内度量到 CLI 退出码（3）都要红。
//! 4⑤ 扫 `crates/` 的清单与源码：生产码不依赖、不 import 这把尺子。

use eval::{
    manifest_depends_on_eval, measure_recall, measure_thresholds, run_extract, run_inject,
    source_imports_eval, EvalError, ExtractTruth, InjectCase, InjectKind, InjectTruth, RecallQuery,
    RecallTruth,
};
use rc_memory::{
    put_card, put_event, synth, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Stance,
};
use rc_store::Root;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const NOW: i64 = 1_700_000_000_000;

fn fact(title: &str, body: &str) -> FactContent {
    FactContent {
        kind: "note".into(),
        title: title.into(),
        body: body.into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-eval-test".into() },
        valid_to_ms: None,
    }
}

fn provenance(i: usize) -> Vec<Provenance> {
    vec![Provenance { turn_uid: format!("{:064x}", i + 1), anchor_role: AnchorRole::Human }]
}

/// 三行小库：查询 `红 绿 蓝` 的分数是 768 / 682 / 512（行长为 4 / 3 / 2），
/// 于是 k=2 时前两条是甲、乙；查询 `绿` 的前两条是丙、乙。手算值见各条断言。
fn hand_library(root: &Root) {
    let m = root.memory();
    put_card(m, &fact("甲", "红 绿 蓝"), &provenance(0), NOW).unwrap();
    put_card(m, &fact("乙", "红 绿"), &provenance(1), NOW).unwrap();
    put_card(m, &fact("丙", "绿"), &provenance(2), NOW).unwrap();
    put_card(m, &fact("丁", "青"), &provenance(3), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
}

#[test]
fn recall_metrics_match_a_hand_computed_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    hand_library(&root);

    let truth = RecallTruth {
        queries: vec![
            RecallQuery {
                query: "红 绿 蓝".into(),
                relevant_titles: vec!["甲".into(), "乙".into(), "丙".into()],
                hard_case: false,
            },
            RecallQuery {
                query: "绿".into(),
                relevant_titles: vec!["乙".into()],
                hard_case: false,
            },
        ],
    };
    let out = measure_recall(root.memory(), &truth, 2, NOW).unwrap();

    // q1：命中 2/3，precision 2/2=1，RR=1；q2：命中 1，precision 1/2=0.5，RR=1/2。
    // 聚合：recall (2/3+1)/2 = 5/6；precision (1+0.5)/2 = 0.75；MRR (1+0.5)/2 = 0.75。
    assert_eq!(out.queries, 2);
    assert!((out.recall_at_k - 5.0 / 6.0).abs() < 1e-12, "recall@2 手算应是 5/6，实得 {}", out.recall_at_k);
    assert_eq!(out.precision_at_k, 0.75);
    assert_eq!(out.mrr, 0.75);

    let q1 = &out.per_query[0];
    assert_eq!(q1.retrieved, vec!["甲".to_string(), "乙".to_string()]);
    assert!((q1.recall_at_k - 2.0 / 3.0).abs() < 1e-12);
    assert_eq!(q1.precision_at_k, 1.0);
    assert_eq!(q1.reciprocal_rank, 1.0);

    let q2 = &out.per_query[1];
    assert_eq!(q2.retrieved, vec!["丙".to_string(), "乙".to_string()], "乙必须排第二，MRR 才有 0.5 这一格");
    assert_eq!(q2.precision_at_k, 0.5);
    assert_eq!(q2.reciprocal_rank, 0.5);
}

#[test]
fn threshold_metrics_match_a_hand_computed_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    hand_library(&root);

    // 三行小库的分数（见 `hand_library` 注释；标题也算进 `row_len`）：
    // q1「红 绿 蓝」：甲 768 / 乙 682 / 丙 512；q2「绿」：丙 512 / 乙 341 / 甲 256；
    // q3「青」：丁 512 —— 它没有相关项，只用来读 `no_relevant_queries` 与 `silenced`。
    let truth = RecallTruth {
        queries: vec![
            RecallQuery {
                query: "红 绿 蓝".into(),
                relevant_titles: vec!["甲".into(), "乙".into(), "丙".into()],
                hard_case: false,
            },
            RecallQuery {
                query: "绿".into(),
                relevant_titles: vec!["乙".into()],
                hard_case: false,
            },
            RecallQuery {
                query: "青".into(),
                relevant_titles: Vec::new(),
                hard_case: false,
            },
        ],
    };
    let out = measure_thresholds(root.memory(), &truth, 2, NOW).unwrap();

    // 手算（k=2，均值只摊在有相关项的 2 条查询上）：
    // t ≤ 256：q1 放行甲乙（命中 2，p=1，r=2/3，f=4/5）；q2 放行丙乙（命中乙 1，p=1/2，r=1，f=2/3）
    //          ⇒ p=3/4，r=5/6，f=11/15，kept=2。
    // t=384/512：q1 不变；q2 只剩丙（非相关）⇒ p=0，r=0，f=0 ⇒ p=1/2，r=1/3，f=2/5，kept=3/2。
    // t=768：q1 只剩甲（命中 1，p=1，r=1/3，f=1/2）；q2 空集 ⇒ p=0 ⇒ p=1/2，r=1/6，f=1/4，kept=1/2。
    // t=1024：两条查询都空集 ⇒ p=r=f=0，kept=0。q3 在 768 起放行 0 行 ⇒ silenced=1。
    let expected: [(i64, f64, f64, f64, usize, f64, usize); 10] = [
        (0, 0.75, 5.0 / 6.0, 11.0 / 15.0, 2, 2.0, 0),
        (1, 0.75, 5.0 / 6.0, 11.0 / 15.0, 2, 2.0, 0),
        (32, 0.75, 5.0 / 6.0, 11.0 / 15.0, 2, 2.0, 0),
        (64, 0.75, 5.0 / 6.0, 11.0 / 15.0, 2, 2.0, 0),
        (128, 0.75, 5.0 / 6.0, 11.0 / 15.0, 2, 2.0, 0),
        (256, 0.75, 5.0 / 6.0, 11.0 / 15.0, 2, 2.0, 0),
        (384, 0.5, 1.0 / 3.0, 0.4, 2, 1.5, 0),
        (512, 0.5, 1.0 / 3.0, 0.4, 2, 1.5, 0),
        (768, 0.5, 1.0 / 6.0, 0.25, 2, 0.5, 1),
        (1024, 0.0, 0.0, 0.0, 2, 0.0, 1),
    ];
    assert_eq!(out.thresholds.len(), expected.len());
    for (got, (t, p, r, f1, rows, kept, silenced)) in out.thresholds.iter().zip(expected) {
        assert_eq!(got.t, t, "阈值档位顺序写错");
        assert!((got.precision - p).abs() < 1e-12, "t={t} precision 手算 {p}，实得 {}", got.precision);
        assert!((got.recall - r).abs() < 1e-12, "t={t} recall 手算 {r}，实得 {}", got.recall);
        assert!((got.f1 - f1).abs() < 1e-12, "t={t} f1 手算 {f1}，实得 {}", got.f1);
        assert_eq!(got.rows, rows, "t={t} 的宏观均值只许摊在有相关项的查询上");
        assert!((got.kept - kept).abs() < 1e-12, "t={t} 放行行数均值手算 {kept}，实得 {}", got.kept);
        assert_eq!(got.silenced, silenced, "t={t} 的空相关项沉默计数");
    }

    // 空相关项查询单列、不进均值：混合进去的话 t=384 的 recall 会从 1/3 变成 2/9。
    assert_eq!(out.queries, 3);
    assert_eq!(out.no_relevant_queries, 1);

    // F1 最高的是 t=0..256 这一段（手算 11/15）；同分取较小阈值 ⇒ 0；它已是最松档 ⇒ looser 为空。
    assert_eq!(out.selected, 0);
    assert_eq!(out.looser, None);
}

#[test]
fn a_wrong_relevant_title_makes_recall_fail() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    hand_library(&root);

    let correct = RecallTruth {
        queries: vec![RecallQuery {
            query: "红 绿 蓝".into(),
            relevant_titles: vec!["甲".into()],
            hard_case: false,
        }],
    };
    assert_eq!(measure_recall(root.memory(), &correct, 2, NOW).unwrap().recall_at_k, 1.0);

    // 真值集写错一个标题 ⇒ 它谁也不交集，recall 必须掉到 0；桩不许"差不多也算对"。
    let wrong = RecallTruth {
        queries: vec![RecallQuery {
            query: "红 绿 蓝".into(),
            relevant_titles: vec!["合成-不存在".into()],
            hard_case: false,
        }],
    };
    let out = measure_recall(root.memory(), &wrong, 2, NOW).unwrap();
    assert!(out.recall_at_k < 1.0, "写错标题仍报满分 ⇒ 这把尺子没有牙");
    assert_eq!(out.recall_at_k, 0.0);
    assert_eq!(out.precision_at_k, 0.0);
    assert_eq!(out.mrr, 0.0);
}

#[test]
fn silence_and_anti_noise_are_distinguishable() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    put_event(m, &fact("会议记录归档", "会上记录了一条决定"), &provenance(0), NOW).unwrap();
    let rejected = put_card(m, &fact("被否方案", "一条被判决否掉的短正文"), &provenance(1), NOW).unwrap();
    rc_memory::attest(
        m,
        &rejected,
        Stance::Rejected,
        None,
        Some("合成：与最新结论冲突"),
        NOW,
    )
    .unwrap();
    // 反噪的干扰行：1500 个 `记录` token ⇒ 匹配 1 个词时 score 为 0，必然低分沉默。
    synth::seed_padded(m, NOW, 1500).unwrap();
    rc_index::rebuild(m, NOW).unwrap();

    let truth = InjectTruth {
        cases: vec![
            InjectCase {
                kind: InjectKind::Hit,
                query: "会议 记录 归档".into(),
                expected_title: Some("会议记录归档".into()),
            },
            InjectCase {
                kind: InjectKind::Silent,
                query: "被否方案".into(),
                expected_title: None,
            },
            InjectCase {
                kind: InjectKind::AntiNoise,
                query: "会议 记录 归档".into(),
                expected_title: Some("长行低分卡".into()),
            },
        ],
    };
    let out = run_inject(m, &truth, NOW).unwrap();
    assert!(out.ok, "三类都应命中：{out:?}");
    assert_eq!((out.hit.passed, out.hit.total), (1, 1));
    assert_eq!((out.silent.passed, out.silent.total), (1, 1));
    assert_eq!((out.anti_noise.passed, out.anti_noise.total), (1, 1));

    let hit = &out.per_case[0];
    assert!(hit.ok);
    assert!(hit.forms.iter().any(|t| t == "会议记录归档"));

    let silent = &out.per_case[1];
    assert!(silent.ok);
    assert!(silent.forms.is_empty(), "被否决的行不许开口");
    assert!(silent.candidates > 0, "沉默必须落在有候选的查询上，否则这条尺子是空转");

    let anti = &out.per_case[2];
    assert!(anti.ok);
    assert!(!anti.forms.iter().any(|t| t == "长行低分卡"));
    // 关键那一句：反噪案例的 `forms` **非空**。若把它按"沉默 = forms 为空"去判，
    // 同一个读数会给出相反结论——两条判据因此不是共用一条串。
    assert!(!anti.forms.is_empty(), "反噪得有别的行开口，才证明判的是'这一条不在'而不是'整张表空'");
    assert!(anti.candidates > 0, "干扰行必须真的进过候选池");
}

/// 没建索引的库必须回**独立的一支码**，不许读成 `recall_failed（unexpected error）`。
/// 现场依据（§10.148）：中心与新建臂的 `index status` 都是 `rebuilt_at_ms:null`、
/// shadow 表压根不存在，而尺子回的是 `recall_failed: the recall path returned an unexpected error`
/// ⇒ 操作员分不清"你还没建索引"与"真坏了"，也看不出下一步是 `hub index rebuild`。
/// 而这条路径在生产里是活的：建索引的只有 `cycle once`／复制接收／恢复／手动 rebuild 四条，
/// 每日抽干走的是 `distill once`+`review run`，一条都不碰。
#[test]
fn a_library_without_the_index_layer_says_so_instead_of_recall_failed() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    put_card(m, &fact("甲", "红 绿 蓝"), &provenance(0), NOW).unwrap();
    let truth = RecallTruth {
        queries: vec![RecallQuery {
            query: "红".into(),
            relevant_titles: vec!["甲".into()],
            hard_case: false,
        }],
    };
    // 刻意不 rebuild：这就是"只被抽干喂过"的库的形状。
    let err = measure_recall(m, &truth, 2, NOW).unwrap_err();
    // 载荷必须是 rc-agent 分型表里那一条 hint（不是尺子自己写的句子）——两处各自措辞迟早漂。
    let want = rc_agent::ToolError::from(rc_index::IndexError::NotBuilt);
    assert_eq!(err, EvalError::IndexNotBuilt(want.hint), "没索引必须复用同一句下一步：{err}");
    assert!(err.to_string().contains("index rebuild"), "错误面要给出下一步动作：{err}");

    // 阳性对照：建完索引，同一支尺子必须量出数——否则上面那条红是"读者坏了"，不是"没索引"。
    rc_index::rebuild(m, NOW).unwrap();
    let out = measure_recall(m, &truth, 2, NOW).unwrap();
    assert_eq!(out.recall_at_k, 1.0, "重建后同一查询应命中，实得 {}", out.recall_at_k);
}

fn collect_rs(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs(&path, into);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            into.push(path);
        }
    }
}

#[test]
fn a_row_without_provenance_fails_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let first = put_card(m, &fact("合成-甲", "离线 回并 批次"), &provenance(0), NOW).unwrap();
    put_card(m, &fact("合成-乙", "索引 重建 影子 表"), &provenance(1), NOW).unwrap();

    let truth = ExtractTruth {
        turns: Vec::new(),
        expected_titles: vec!["合成-甲".into(), "合成-乙".into()],
        must_keep_titles: vec!["合成-甲".into(), "合成-乙".into()],
    };
    let before = run_extract(m, &truth).unwrap();
    assert!(before.ok, "初始库应全绿：{before:?}");
    assert_eq!(before.provenance_coverage, 1.0);

    // 只删这一行的出处：行还在、标题还在、判定族也没动——红点只能来自覆盖率。
    let deleted = m
        .conn()
        .execute(
            "DELETE FROM provenance WHERE mem_table=?1 AND mem_id=?2",
            ("card", first.id.as_str()),
        )
        .unwrap();
    assert_eq!(deleted, 1, "夹具至少要有一条出处可删");

    let after = run_extract(m, &truth).unwrap();
    assert!(!after.ok);
    assert_eq!((after.provenance_with_origin, after.provenance_rows), (1, 2));
    assert!(after.provenance_coverage < 1.0);
    assert!(after.missing_titles.is_empty(), "行还在；红的是出处，不是期望标题");

    // 退出码也要红：CLI 走的是只读句柄 + 同一套度量，缺一条 provenance ⇒ 3。
    let truth_path = dir.path().join("extract-truth.json");
    let truth_json = serde_json::json!({
        "turns": [],
        "expected_titles": ["合成-甲", "合成-乙"],
        "must_keep_titles": ["合成-甲", "合成-乙"]
    });
    fs::write(&truth_path, truth_json.to_string()).unwrap();
    drop(root);

    let output = Command::new(env!("CARGO_BIN_EXE_recollect-eval"))
        .args([
            "extract",
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--truth",
            truth_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "缺一条 provenance 必须退出 3；stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["ok"], serde_json::json!(false));
    // 错误串封闭、零路径：量出来的失败是读数，不是把数据目录打给运维面。
    let leak = dir.path().to_str().unwrap();
    assert!(!stdout.contains(leak), "stdout 带出了路径");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(leak), "stderr 带出了路径");
}

#[test]
fn eval_is_not_a_dependency_of_any_production_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let crates = root.join("crates");
    assert!(crates.is_dir(), "找不到生产 crate 目录");

    let mut manifests = 0_usize;
    for entry in fs::read_dir(&crates).unwrap() {
        let manifest = entry.unwrap().path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        manifests += 1;
        let text = fs::read_to_string(&manifest).unwrap();
        for (n, line) in text.lines().enumerate() {
            assert!(
                !manifest_depends_on_eval(line),
                "{}:{} 依赖了 eval：{line}",
                manifest.display(),
                n + 1
            );
        }
    }
    assert!(manifests >= 20, "只扫到 {manifests} 份 crate 清单，空转的守卫比漏网更坏");

    let mut sources = Vec::new();
    collect_rs(&crates, &mut sources);
    assert!(sources.len() >= 200, "只扫到 {} 份源码", sources.len());
    for path in &sources {
        let text = fs::read_to_string(path).unwrap();
        for (n, line) in text.lines().enumerate() {
            assert!(
                !source_imports_eval(line),
                "{}:{} 引用了 eval：{line}",
                path.display(),
                n + 1
            );
        }
    }

    // 匹配器自己有牙：四种依赖写法、三种 import 形状都认；`evaluation` 与注释不误伤。
    assert!(manifest_depends_on_eval("eval = { path = \"../../eval\" }"));
    assert!(manifest_depends_on_eval("recollect-eval = \"0.1\""));
    assert!(manifest_depends_on_eval("[dependencies.eval]"));
    assert!(manifest_depends_on_eval("foo = { package = \"eval\", version = \"1\" }"));
    assert!(!manifest_depends_on_eval("# eval = { path = \"../../eval\" }"));
    assert!(!manifest_depends_on_eval("evaluation = \"1\""));
    assert!(source_imports_eval("use eval::measure_recall;"));
    assert!(source_imports_eval("    eval::run_inject(m, truth, now)?;"));
    assert!(source_imports_eval("use recollect_eval as ruler;"));
    assert!(!source_imports_eval("let evaluation = 1;"));
    assert!(!source_imports_eval("// eval:: is forbidden"));
    // 生产 crate 自己的 a1_eval 模块不是尺子 crate；判据只认独立标识符路径。
    assert!(!source_imports_eval("use rc_distill::a1_eval::{evaluate, PlanView};"));
    assert!(!source_imports_eval("a1_eval::evaluate(&plan, &gold, source);"));
    assert!(!source_imports_eval("use my_eval::thing;"));
    assert!(source_imports_eval("eval::run_inject(m, truth, now)?;"));
    assert!(source_imports_eval("use eval;"));
    assert!(!source_imports_eval("use evaluation::thing;"));
}
