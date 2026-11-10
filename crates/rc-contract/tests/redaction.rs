//! 入库时值级脱敏（设计 `2026-10-25-extraction-system-blueprint.md` 第 31–32 行 / 硬规则 1）。
//!
//! 三条判据钉在这一个文件里：
//! - 真凭据被换成 `<redacted:credential>`；
//! - commit sha（7–8 位 hex）、路径、≥32 位长数字**原样保留**（阴性对照，三样本同一用例）；
//! - `turn_uid`/`body_fp` 在脱敏前后不变（身份用原文算），重复导入仍是同一条。

use rc_contract::{
    compute_body_fp, compute_turn_uid, normalize, normalize_with, parse, redact_text,
    RedactionEntry, REDACTED_CREDENTIAL,
};

fn doc(body: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-redact",
            "device_id": "dev-redact",
            "source_agent": "qoder",
            "turns": [{"role": "human", "body": body, "created_at_ms": 1_800_000_000_000i64}]
        }]
    }))
    .unwrap()
}

fn redacted(body: &str) -> String {
    redact_text(body).text
}

/// 验收 2：同一条正文里三条样本，任一条判错即红。
///
/// (a) 合成假凭据 ⇒ 必须被替换；(b) commit sha 与路径 ⇒ 必须原样；(c) ≥32 位数字 ⇒ 必须原样。
#[test]
fn 阴性对照_真凭据被替换_sha路径长数字原样保留() {
    let commit = "439ef5c";
    let path = "/a/b/c.py";
    let long_number = "12345678901234567890123456789012";
    let body = format!(
        "服务器口令：password = Abc12345xyz\n\
         修复在 commit {commit}，文件 {path}\n\
         构建号 {long_number}\n"
    );

    let out = redacted(&body);

    assert!(
        out.contains(&format!("password = {REDACTED_CREDENTIAL}")),
        "(a) 真凭据没有被替换：{out}"
    );
    assert!(!out.contains("Abc12345xyz"), "(a) 明文仍在正文里：{out}");
    assert!(out.contains(commit), "(b) commit sha 被误伤：{out}");
    assert!(out.contains(path), "(b) 路径被误伤：{out}");
    assert!(out.contains(long_number), "(c) 长数字被误伤：{out}");

    let spans = redact_text(&body).spans;
    assert_eq!(spans.len(), 1, "只该命中那一段口令：{spans:?}");
}

/// 主支的形状：中文关键词、`PASS=`、引号包着的值都算；散文里的关键词不算。
#[test]
fn 口令关键词支_命中与放过() {
    for body in [
        "数据库口令：Abc12345xyz 记一下",
        "服务器密码为 Abc12345xyz",
        "password: Abc12345xyz",
        "password=\"Abc12345xyz\"",
        "PASS=Abc12345xyz 在 .env 里",
        "passphrase 是 Abc12345xyz",
        "passwd = Abc12345xyz",
    ] {
        let out = redacted(body);
        assert!(out.contains(REDACTED_CREDENTIAL), "该命中却被放过：{body} ⇒ {out}");
        assert!(!out.contains("Abc12345xyz"), "明文残留：{body} ⇒ {out}");
    }

    // 值不成立（缺数字/太短/纯散文）⇒ 一个字都不许动。
    for body in [
        "password must be at least 12 characters",
        "password = short1",
        "password = onlylettershere",
        "passwordless login is enabled",
        "bypass the queue",
        "PASS the ball",
        // 值必须同行：换行是句子边界，不是分隔符。
        "password\nAbc12345xyz",
        "PASS\n=Abc12345xyz",
    ] {
        assert_eq!(redacted(body), body, "误伤：{body}");
    }
}

/// 低误杀支：固定前缀的密钥与 `scheme://user:pass@host`。
#[test]
fn 前缀密钥支与url支() {
    let cases = [
        ("AWS 的 key 是 AKIAIOSFODNN7EXAMPLE 呢", "AKIAIOSFODNN7EXAMPLE"),
        ("token ghp_0123456789abcdefghij 过期了", "ghp_0123456789abcdefghij"),
        ("slack xoxb-1234567890 换掉", "xoxb-1234567890"),
        ("export OPENAI=sk-proj012345678", "sk-proj012345678"),
    ];
    for (body, secret) in cases {
        let out = redacted(body);
        assert!(out.contains(REDACTED_CREDENTIAL), "该命中却被放过：{body} ⇒ {out}");
        assert!(!out.contains(secret), "明文残留：{body} ⇒ {out}");
    }

    // URL 只换口令段：scheme/user/host/端口都留着。
    let out = redacted("连 postgres://app:Abc12345xyz@db.internal:5432/x 就行");
    assert!(out.contains(&format!("postgres://app:{REDACTED_CREDENTIAL}@db.internal:5432/x")), "{out}");

    // 没有口令段（用户名、端口、路径里的 @）都不是凭据。
    for body in [
        "ssh://git@github.com/org/repo",
        "https://example.com/@handle",
        "https://example.com/a/b@2x.png",
    ] {
        assert_eq!(redacted(body), body, "URL 误伤：{body}");
    }
}

/// 已打码的正文不能被二次命中（幂等读取者把同一段再导一遍也一样）。
#[test]
fn 已打码的正文保持原样() {
    let body = format!("password = {REDACTED_CREDENTIAL}");
    assert_eq!(redacted(&body), body);
    assert!(redact_text(&body).spans.is_empty());
}

/// 验收 3：身份用**原文**算——脱敏只改入库正文。
#[test]
fn 脱敏前后turn_uid与body_fp不变() {
    let original = "注意：密码：Abc12345xyz 只在本机用";
    let file = parse(&doc(original)).unwrap();
    let rep = normalize(file, 1_800_000_000_100);
    assert_eq!(rep.accepted.len(), 1);
    let turn = &rep.accepted[0].turn;

    // 原文归一后的串（`normalize_body` 只做换行/NFC/trim，不改内容）
    let normalized = original.to_string();
    assert_eq!(
        turn.turn_uid,
        compute_turn_uid("dev-redact", "sess-redact", "human", &normalized, 1_800_000_000_000),
        "turn_uid 必须仍由原文算出"
    );
    assert_eq!(
        turn.body_fp,
        compute_body_fp(&normalized),
        "body_fp 必须仍由原文算出"
    );
    assert!(turn.body.contains(REDACTED_CREDENTIAL), "入库正文没脱敏：{}", turn.body);
    assert!(!turn.body.contains("Abc12345xyz"), "入库正文残留明文：{}", turn.body);
    assert_eq!(rep.redacted_spans, 1, "计数可观测，且零正文");
    // 计数不许把明文带进 Debug 面（BatchReport 会被调试打印）。
    let rendered = format!("{rep:?}");
    assert!(!rendered.contains("Abc12345xyz"), "报告 Debug 泄露明文：{rendered}");
}

/// 映射只在显式收集时才留下；默认路径一个明文副本都不留。
#[test]
fn 映射只在显式给收集器时才留() {
    let body = "密码：Abc12345xyz";
    let rep = normalize(parse(&doc(body)).unwrap(), 1_800_000_000_100);
    assert_eq!(rep.redacted_spans, 1, "默认路径也要计数");

    let mut map: Vec<RedactionEntry> = Vec::new();
    let rep = normalize_with(parse(&doc(body)).unwrap(), 1_800_000_000_100, Some(&mut map));
    assert_eq!(rep.redacted_spans, 1);
    assert_eq!(map.len(), 1);
    assert_eq!(map[0].original(), "Abc12345xyz");
    assert_eq!(map[0].kind, "keyword_value");
    assert_eq!(map[0].turn_uid, rep.accepted[0].turn.turn_uid);
    // 这份记录本身也不许在 Debug 面露出明文（它可能被顺手打进日志）。
    let rendered = format!("{:?}", map[0]);
    assert!(!rendered.contains("Abc12345xyz"), "映射记录 Debug 泄露明文：{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
}

/// 同一份输入两次归一 ⇒ 同一个 `turn_uid`（重复导入靠它走 `skipped_duplicate`）。
#[test]
fn 同一份输入两次归一的身份相同() {
    let bytes = doc("密码：Abc12345xyz");
    let a = normalize(parse(&bytes).unwrap(), 1);
    let b = normalize(parse(&bytes).unwrap(), 999_999);
    assert_eq!(a.accepted[0].turn.turn_uid, b.accepted[0].turn.turn_uid);
    assert_eq!(a.accepted[0].turn.body_fp, b.accepted[0].turn.body_fp);
    assert_eq!(a.accepted[0].turn.body, b.accepted[0].turn.body);
}
