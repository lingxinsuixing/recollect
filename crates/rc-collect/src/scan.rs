//! 源扫描：files 源（单层 `*` glob、静默准入、mtime/size/头 4KB 判变更）与
//! sqlite 源（只读 URI 取 `cursor_sql` 的 `max_ms`，套静默闸与水位闸）。
//!
//! 这里只**发现**该转什么，不改状态；水位由调用方在收到回执后推进。
//! 路径键统一为绝对路径 + 正斜杠，Windows 与 Unix 的状态文件可互相读懂。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::config::{Source, SourceKind};
use crate::state::StateDoc;

/// 一个到期待处理的文件（未排序前的中间结构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueFile {
    pub path: PathBuf,
    pub mtime_ms: i64,
    pub size: u64,
    pub offset: u64,
    pub last_turns: i64,
}

/// 整文件源的一个到期待转文件（变更判定已做完）。
///
/// 它**没有 offset**：整文件源不存在字节偏移，文件变了就整份重转。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeDue {
    pub path: PathBuf,
    pub mtime_ms: i64,
    pub size: u64,
    /// 扫描时算出的头 4KB 哈希：水位要记的**就是这一份**（扫描与转换之间文件又变了 ⇒
    /// 下轮再转一次，宁可多转一次也不漏）。
    pub head_sha256: String,
}

/// 整文件源的到期判定：命中 glob 的文件里，**水位三字段变了**的那些才算到期。
///
/// 判定口径 = files 源同一套：`mtime_ms` + `size` 都一样时再比**头 4KB 的 sha256**
/// （同长重写只有头哈希能看出来）。三字段全同 ⇒ 没变 ⇒ 跳过。
///
/// 静默窗口照旧生效：最近 `quiet_ms` 内被写过的文件不碰（还在写，转了也是半份）。
/// 排序与 files 源一致：**新先处理**（新鲜的会话别被旧文件挤掉日帽）。
pub fn scan_whole(src: &Source, st: &StateDoc, quiet_ms: i64, now_ms: i64) -> Vec<WholeDue> {
    if src.kind != SourceKind::Whole {
        return Vec::new();
    }
    let (root, glob) = match (&src.root, &src.glob) {
        (Some(r), Some(g)) => (r, g),
        _ => return Vec::new(),
    };
    // `**` 不递归通配：宁可漏一晚，不猜语义（与 files 源同口径）。
    if glob.contains("**") {
        return Vec::new();
    }

    let (base, segs) = split_glob(root, glob);
    let mut paths = Vec::new();
    walk(&base, &segs, &mut paths);

    let mut due = Vec::new();
    for path in paths {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // 静默准入：还在写的文件不碰。
        if now_ms - mtime_ms < quiet_ms {
            continue;
        }
        let key = path_key(&path);
        let head = head_sha256(&path).unwrap_or_default();
        if let Some(prev) = st.whole.get(&key) {
            if prev.mtime_ms == mtime_ms
                && prev.size == size as i64
                && prev.head_sha256 == head
            {
                // 没变：跳过（不产生空转）。
                continue;
            }
        }
        due.push(WholeDue {
            path,
            mtime_ms,
            size,
            head_sha256: head,
        });
    }

    due.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    due
}

/// 源扫描：files 源（逐行增量）与整文件源的入口。
/// 列出 `src` 下所有该收的文件，按 mtime 升序（再按路径）。
///
/// - 只支持单层 `*`；出现 `**` 视为不支持，返回空（宁可漏一晚，不猜语义）。
/// - `quiet_ms` 内仍在写的文件不进来。
/// - offset 超过当前 size（文件被截断）时归零。
/// - mtime/size 都没变时才读头 4KB 比对；哈希变了视为同长重写，offset 归零。
pub fn scan_files(src: &Source, st: &StateDoc, quiet_ms: i64, now_ms: i64) -> Vec<DueFile> {
    if src.kind != SourceKind::Files {
        return Vec::new();
    }
    let (root, glob) = match (&src.root, &src.glob) {
        (Some(r), Some(g)) => (r, g),
        _ => return Vec::new(),
    };
    if glob.contains("**") {
        return Vec::new();
    }

    let (base, segs) = split_glob(root, glob);
    let mut paths = Vec::new();
    walk(&base, &segs, &mut paths);

    let mut due = Vec::new();
    for path in paths {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // 静默准入：差 < quiet_ms 一律跳过（不影响状态）。
        if now_ms - mtime_ms < quiet_ms {
            continue;
        }

        let key = path_key(&path);
        let mut offset: u64 = 0;
        let mut last_turns: i64 = 0;
        if let Some(prev) = st.files.get(&key) {
            last_turns = prev.last_turns;
            offset = u64::try_from(prev.offset).unwrap_or(0);
            if offset > size {
                offset = 0;
            }
            let same_mtime = prev.mtime_ms == mtime_ms;
            let same_size = prev.size >= 0 && prev.size as u64 == size;
            if same_mtime && same_size {
                let head = head_sha256(&path).unwrap_or_default();
                if head == prev.head_sha256 {
                    continue;
                }
                // 头哈希变了：同长重写，整文件重转。
                offset = 0;
            } else if same_size {
                // 长度没变但 mtime 变了：写者可能原地改了中段，按重写处理。
                // （中段同长改写本就探测不到，这里取保守的一侧。）
                offset = 0;
            }
        }
        if offset >= size {
            // 没有新字节：不产生空段，也不写状态。
            continue;
        }
        due.push(DueFile {
            path,
            mtime_ms,
            size,
            offset,
            last_turns,
        });
    }

    // 新的先处理：日帽被一次历史回填吃满时，先保住“新鲜会话”的入库，
    // 旧历史顺延到下一个窗口（2026-11-10 实测：旧→新会把当天额度烧在 9 月的会话上）。
    due.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    due
}

/// sqlite 源的一次到期判断：从水位 `since_ms` 收到库内当前最大 `latest_ms`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteDue {
    pub since_ms: i64,
    pub latest_ms: i64,
}

/// 判一个 sqlite 源这轮该不该跑。
///
/// 只读 URI 打开 `db` 并执行 `cursor_sql`（单条 SELECT）取 `max_ms`：
/// - 取不到值（空表 / 全 NULL）⇒ `Ok(None)`；
/// - `now_ms - max_ms < quiet_ms`（还在写）⇒ `Ok(None)`；
/// - `max_ms <= 水位`（没有新行）⇒ `Ok(None)`；
/// - 否则 `Some{SqliteDue{since_ms: 水位, latest_ms: max_ms}}`。
///
/// 首跑没有水位时 `since_ms = 0`；"只收未来"的播种由 `run_once` 做（它才写状态）。
/// `Err` 只表示库或 `cursor_sql` 坏了，调用方记 `reason` 后继续下一个源。
pub fn scan_sqlite(
    src: &Source,
    st: &StateDoc,
    quiet_ms: i64,
    now_ms: i64,
) -> Result<Option<SqliteDue>, String> {
    if src.kind != SourceKind::Sqlite {
        return Ok(None);
    }
    let (db, sql) = match (&src.db, &src.cursor_sql) {
        (Some(d), Some(s)) => (d, s),
        _ => return Ok(None),
    };
    let latest_ms = match cursor_max_ms(db, sql)? {
        Some(m) => m,
        None => return Ok(None),
    };
    if now_ms - latest_ms < quiet_ms {
        return Ok(None);
    }
    let since_ms = st
        .sqlite
        .get(&src.name)
        .map(|c| c.last_created_at_ms)
        .unwrap_or(0);
    if latest_ms <= since_ms {
        return Ok(None);
    }
    Ok(Some(SqliteDue {
        since_ms,
        latest_ms,
    }))
}

/// 执行 `cursor_sql` 取第一行第一列（`NULL` ⇒ `None`）。
fn cursor_max_ms(db: &Path, sql: &str) -> Result<Option<i64>, String> {
    let conn = open_read_only(db)?;
    conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))
        .map_err(|_| "cursor_sql failed".to_string())
}

/// 只读打开：先按 `file:…?mode=ro` URI（`SQLITE_OPEN_URI`），失败再退回只读旗标。
/// 两条路都不会建库、不会拿写锁；库不存在在这里就是 `Err`。
fn open_read_only(path: &Path) -> Result<Connection, String> {
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    if let Ok(conn) = Connection::open_with_flags(&uri, ro | OpenFlags::SQLITE_OPEN_URI) {
        return Ok(conn);
    }
    Connection::open_with_flags(path, ro).map_err(|_| "sqlite db is unreadable".to_string())
}

/// 文件头 4096 字节的 sha256（hex）。用于"mtime/size 都相同"的同长重写判定。
pub fn head_sha256(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|_| "file is unreadable".to_string())?;
    let mut buf = vec![0u8; 4096];
    let mut n = 0usize;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return Err("file is unreadable".to_string()),
        }
    }
    buf.truncate(n);
    let mut h = Sha256::new();
    h.update(&buf);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// 状态文件里的路径键：绝对路径、正斜杠。
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 把 `root` + `glob` 拆成起点目录与待匹配的分段。
/// 绝对 glob 以它自己的父目录为起点（配置文件允许写绝对 glob）。
fn split_glob(root: &Path, glob: &str) -> (PathBuf, Vec<String>) {
    let g = Path::new(glob);
    if g.is_absolute() {
        let base = g
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        let name = g
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        return (base, vec![name]);
    }
    let segs: Vec<String> = glob
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (root.to_path_buf(), segs)
}

fn walk(dir: &Path, segs: &[String], out: &mut Vec<PathBuf>) {
    let (head, rest) = match segs.split_first() {
        Some(x) => x,
        None => return,
    };
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !matches_segment(head, &name) {
            continue;
        }
        let p = entry.path();
        if rest.is_empty() {
            if p.is_file() {
                out.push(p);
            }
        } else if p.is_dir() {
            walk(&p, rest, out);
        }
    }
}

/// 单个路径段的匹配：`*` 匹配任意字符（含空），其余按字面。
fn matches_segment(pattern: &str, name: &str) -> bool {
    let p = pattern.as_bytes();
    let s = name.as_bytes();
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while si < s.len() {
        if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            mark = si;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::matches_segment;

    #[test]
    fn segment_glob_is_single_layer_star() {
        assert!(matches_segment("*.jsonl", "a.jsonl"));
        assert!(!matches_segment("*.jsonl", "a.txt"));
        assert!(matches_segment("*", "anything"));
        assert!(matches_segment("a*b*c", "aXbYc"));
        assert!(!matches_segment("a*b", "ac"));
    }
}
