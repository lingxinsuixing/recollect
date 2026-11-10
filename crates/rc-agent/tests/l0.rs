//! C3：`skills/recollect-L0.md` 与 rc-agent 内嵌常量的同源对拍（2 条）。
//!
//! 第一条钉两件事：产物里的 `L0_TEXT` 就是仓库文件的内容（`include_str!` 的编译期同源），
//! 且文件的 sha256 等于 `L0_SHA256`。改文本不改戳 ⇒ 这一条红，阶段 5 判据 4⑧ 跟着红。
//! 第二条钉工具面：六个 MCP 工具名必须逐一出现在文本里，skill 与工具面漂移即红。

use sha2::{Digest, Sha256};

/// 仓库里的那份文本。路径从 crate 根往上一级到 `crates/`，再上一级到仓根。
const L0_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/recollect-L0.md");

const MCP_TOOLS: [&str; 6] = [
    "recollect_recall",
    "recollect_remember",
    "recollect_vote",
    "recollect_status",
    "recollect_export",
    "recollect_report",
];

/// W38a 刀 B / W39 刀 B：转录面四件（细案 2026-11-08-w38a-transcript-search §三、
/// 2026-11-09-w39-remote-transcript §3.4）。它们同为 `recollect_*` 前缀，但不进
/// L0 skill 文本——那份文本只教记忆面的查询路由（skills/ 文件不在那两刀的允许文件集里，
/// `core_*` 同样不在文本里）。清单顺序 = `capabilities().tools` 的出场顺序。
const TRANSCRIPT_TOOLS: [&str; 4] =
    ["recollect_search", "recollect_session", "recollect_turn", "recollect_sessions"];

/// W38b 刀 D（G19）：自查两件。与转录面四件同一条纪律：同为 `recollect_*` 前缀、
/// 不进 L0 skill 文本，由各自的工具描述教（whoami / ingest_status 的读数本身
/// 就是接入文档 §3 的两个自查入口，见 rust/docs/2026-11-08-onboarding.md）。
const SELFCHECK_TOOLS: [&str; 2] = ["recollect_whoami", "recollect_ingest_status"];

/// 波二刀 A（G1 复用反馈）：`recollect_reuse`。与转录面四件同一条纪律——同为
/// `recollect_*` 前缀、不进 L0 skill 文本（skills/ 不在该刀的允许文件集里），
/// 由工具描述与 onboarding 文档教"用了就报"（刀 B 落文档）。
const REUSE_TOOLS: [&str; 1] = ["recollect_reuse"];

/// 小写十六进制。`sha2` 只给字节，这里的 64 个字符是版本戳的 wire 形状。
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[test]
fn the_text_is_embedded_and_stamped() {
    let on_disk = std::fs::read_to_string(L0_PATH)
        .unwrap_or_else(|e| panic!("cannot read {L0_PATH}: {e}"));
    assert_eq!(
        rc_agent::l0::L0_TEXT,
        on_disk,
        "L0_TEXT must be the exact contents of skills/recollect-L0.md"
    );
    let digest = Sha256::digest(on_disk.as_bytes());
    assert_eq!(
        hex_lower(&digest),
        rc_agent::l0::L0_SHA256,
        "skills/recollect-L0.md changed without updating L0_SHA256 in crates/rc-agent/src/l0.rs"
    );
}

#[test]
fn the_text_names_every_mcp_tool() {
    // 工具清单的唯一家是 `capabilities().tools`，而 L0 skill 只讲**记忆面那六个**名字：
    // 核心层工具由 `core_*` 自己的工具描述教，转录面四件由各自的工具描述教
    // （见 TRANSCRIPT_TOOLS 的档注），都不在这份文本里。所以这里先按前缀把
    // `recollect_*` 子集从同一张清单里取出来、与两份逐字清单对撞（清单多一个或少一个
    // 都红），再查文本。写成"清单的 `recollect_*` 子集"而不是"全部工具"：后者会把
    // skill 文本逼成第二份工具清单，而 `tools/list` 的清单已经由 `mcp/tests/tools.rs` 对撞。
    let mut named = rc_agent::capabilities()
        .tools
        .into_iter()
        .filter(|t| t.starts_with("recollect_"));
    for tool in MCP_TOOLS.into_iter().chain(REUSE_TOOLS).chain(TRANSCRIPT_TOOLS)
        .chain(SELFCHECK_TOOLS) {
        assert_eq!(named.next(), Some(tool), "recollect_* 工具清单漂了");
    }
    assert_eq!(named.next(), None, "recollect_* 多了一个工具，这条对撞没跟上");
    for tool in MCP_TOOLS {
        assert!(
            rc_agent::l0::L0_TEXT.contains(tool),
            "skills/recollect-L0.md no longer names {tool}; the skill and the tool surface drifted"
        );
    }
}
