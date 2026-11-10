pub mod capability;
pub mod ddl;
pub mod shards;
pub mod snapshot;

use ddl::MigrationReport;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DATA_FILES: [&str; 3] = ["transcript.db", "memory.db", "ledger.db"];

/// 连接级忙等上限（毫秒）。H1a：三条连接构造点（transcript/ledger/memory，含只读副本）
/// 统一设它——两个写者并发时，后到的一方在锁上等，而不是立刻拿 `SQLITE_BUSY` 回来。
/// 5s 是"短事务等一等就好"的量级；真正的长事务仍会响亮失败（不许无限等）。
pub const BUSY_TIMEOUT_MS: u64 = 5_000;

fn tune_busy_timeout(conn: &rusqlite::Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    Ok(())
}

/// 只读连接的唯一构造点：三个句柄（transcript/ledger/memory 的只读形态）共用同一组 flags。
/// flags 里的 `READ_ONLY` 是承重的：诊断路径不许因为"读了一眼"就把 WAL 撑大或建出空表。
/// 只读副本也要忙等：节点的读面与中心的写面可能落在同一份文件上（快照落地）。
fn connect_read_only(path: &Path) -> Result<rusqlite::Connection, StoreError> {
    use rusqlite::OpenFlags;
    let f = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(path, f)?;
    tune_busy_timeout(&conn)?;
    Ok(conn)
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("sqlite: {0}")]
    Sql(String),
    #[error("{db} 的 schema 版本 {found} 高于本产物支持的 {supported}，拒绝降级读写")]
    SchemaTooNew { db: &'static str, found: i32, supported: i32 },
    /// 旧 schema（canonical 基线之前）：记忆库没有迁移路径，操作员显式重建。
    #[error("{db} 是旧 schema（v{found} < v{supported}）：无迁移；删除/归档该库后重建")]
    SchemaRebuildRequired { db: &'static str, found: i32, supported: i32 },
    #[error("{0} 是只读副本，不许写")]
    ReadOnly(&'static str),
    #[error("the replica file is not there yet")]
    ReplicaMissing,
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e.to_string())
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e.to_string())
    }
}

/// 会话库句柄。构造它是私有的：只有 Root::open 能做到（守卫 4）。
pub struct TranscriptHandle {
    conn: rusqlite::Connection,
}

pub struct LedgerHandle {
    conn: rusqlite::Connection,
}

impl TranscriptHandle {
    fn connect(path: &Path) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        tune_busy_timeout(&conn)?;
        Ok(TranscriptHandle { conn })
    }
    /// 只读形态（诊断面的装配用）。与 [`TranscriptHandle::connect`] 的差别只有 flags：
    /// 这条连接上任何写都会由 SQLite 拒绝，迁移/建表因此不可能借道发生。
    fn connect_ro(path: &Path) -> Result<Self, StoreError> {
        Ok(TranscriptHandle { conn: connect_read_only(path)? })
    }
    /// SQL 逃生口，供 `rc-ledger`（唯一的会话/水位写入侧）与本仓库的测试使用。
    /// 它确实把"任意 SQL"暴露了出去——一期接受，理由见阶段 0 计划 Task 7 Step 4。
    ///
    /// 守卫仍在的另一半：句柄不可互换、不可在组合根之外构造（`tests\compile_fail\`）。
    /// "只有 `rc-ledger` 该拿连接"这句在 cargo 的依赖模型里没有对应物，一期用依赖声明收
    /// （`hub`/`doctor` 只调 `rc-ledger` 的函数，不自己写 SQL），用例收另一半
    /// （Task 8/11/12 的写入路径全经 `rc-ledger`，绕开它们的测试会先撞 `import_log` 行数断言）。
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

impl LedgerHandle {
    fn connect(path: &Path) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        tune_busy_timeout(&conn)?;
        Ok(LedgerHandle { conn })
    }
    /// 只读形态。同 [`TranscriptHandle::connect_ro`]：flags 是唯一差别。
    fn connect_ro(path: &Path) -> Result<Self, StoreError> {
        Ok(LedgerHandle { conn: connect_read_only(path)? })
    }
    /// SQL 逃生口，供 `rc-ledger`（唯一的会话/水位写入侧）与本仓库的测试使用。
    /// 它确实把"任意 SQL"暴露了出去——一期接受，理由见阶段 0 计划 Task 7 Step 4。
    ///
    /// 守卫仍在的另一半：句柄不可互换、不可在组合根之外构造（`tests\compile_fail\`）。
    /// "只有 `rc-ledger` 该拿连接"这句在 cargo 的依赖模型里没有对应物，一期用依赖声明收
    /// （`hub`/`doctor` 只调 `rc-ledger` 的函数，不自己写 SQL），用例收另一半
    /// （Task 8/11/12 的写入路径全经 `rc-ledger`，绕开它们的测试会先撞 `import_log` 行数断言）。
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

/// 记忆库句柄。构造私有：只有 Root::open / AgentRoot::open* 能做到（守卫 4）。
/// `read_only` 是下行整替落地文件（副本）的形状。
pub struct MemoryHandle {
    conn: rusqlite::Connection,
    read_only: bool,
}

impl MemoryHandle {
    fn connect_rw(path: &Path) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        tune_busy_timeout(&conn)?;
        Ok(MemoryHandle { conn, read_only: false })
    }
    fn connect_ro(path: &Path) -> Result<Self, StoreError> {
        Ok(MemoryHandle { conn: connect_read_only(path)?, read_only: true })
    }
    /// SQL 逃生口。同一条妥协的完整论证见阶段 0 计划 任务 7 Step 4：cargo 的依赖模型里
    /// 没有"只有某个 crate 该拿连接"这一项，本仓库收的是另一半——句柄不可互换、
    /// 不可在组合根外构造（tests\compile_fail\），加上 three_dbs.rs 的行为断言。
    pub fn conn(&self) -> &rusqlite::Connection { &self.conn }
    pub fn is_read_only(&self) -> bool { self.read_only }
}

/// 消费侧的装配结果：**只有记忆柄**。`hook`（以及 mcp 的读路径）用它。
/// 这条类型的存在是守卫 6 最强的形态——不是"hook 没调会话库"，是"hook 拿不到会话库"。
pub struct AgentRoot {
    data_dir: PathBuf,
    memory: MemoryHandle,
}

impl AgentRoot {
    pub fn open(data_dir: &Path) -> Result<AgentRoot, StoreError> {
        // 与 Root::open 同一条：宿主配置里 `--data-dir ./data` 是首次运行，目录还不存在。
        // 少这一行时症状不是崩溃而是"hook 每次会话开头报一次运维失败"，而宿主会把 hook 关掉。
        std::fs::create_dir_all(data_dir)?;
        // 只 connect memory.db：另两个文件不存在也得能起来（three_dbs.rs 第二条用例）
        let memory = MemoryHandle::connect_rw(&data_dir.join("memory.db"))?;
        ddl::migrate_memory(&memory)?;
        Ok(AgentRoot { data_dir: data_dir.to_path_buf(), memory })
    }
    /// 下行整替后的本地副本（阶段 4 的 snapshot 落地文件就是这个形状）。
    /// 记忆柄从副本文件拿、`data_dir` 只用于报形状：中心库与副本不可能被同一段代码写成同一个文件。
    pub fn open_replica(data_dir: &Path, snapshot_db: &Path) -> Result<AgentRoot, StoreError> {
        let memory = MemoryHandle::connect_ro(snapshot_db)?;
        ddl::require_memory_schema(&memory)?;
        Ok(AgentRoot { data_dir: data_dir.to_path_buf(), memory })
    }
    /// 节点为自己的副本建派生索引时用：同一份副本的可写句柄。
    /// 读面仍然只拿 [`AgentRoot::open_replica`] 的只读柄；这一条只给 `node` 的
    /// `index::ensure_current_replica` 用，构造点仍是 `AgentRoot::open*`（守卫 4）。
    pub fn open_replica_rw(data_dir: &Path, snapshot_db: &Path) -> Result<AgentRoot, StoreError> {
        let memory = MemoryHandle::connect_rw(snapshot_db)?;
        ddl::require_memory_schema(&memory)?;
        Ok(AgentRoot { data_dir: data_dir.to_path_buf(), memory })
    }
    pub fn data_dir(&self) -> &Path { &self.data_dir }
    pub fn memory(&self) -> &MemoryHandle { &self.memory }
}

pub struct Root {
    data_dir: PathBuf,
    transcript: TranscriptHandle,
    ledger: LedgerHandle,
    memory: MemoryHandle,
    migration: MigrationReport,
}

impl Root {
    /// 唯一的装配点：三库齐全（阶段 1 起 `memory.db` 也在内）。
    /// "谁能同时命名三个句柄"仍然只在 rc-store 内部——消费侧的装配结果是 `AgentRoot`。
    pub fn open(data_dir: &Path) -> Result<Root, StoreError> {
        std::fs::create_dir_all(data_dir)?;
        let transcript = TranscriptHandle::connect(&data_dir.join("transcript.db"))?;
        let ledger = LedgerHandle::connect(&data_dir.join("ledger.db"))?;
        let memory = MemoryHandle::connect_rw(&data_dir.join("memory.db"))?;
        let migration = ddl::migrate_all(&transcript, &ledger, &memory)?;
        Ok(Root { data_dir: data_dir.to_path_buf(), transcript, ledger, memory, migration })
    }

    /// 诊断面的装配：三库齐全，但**全部是只读句柄**。
    ///
    /// 与 [`Root::open`] 的差别是承重的：这里**不建目录、不建表、不跑迁移**。`Root::open`
    /// 的诊断副作用实测存在（跑一次 doctor 就在现网新建空 `card_verdict_event`、把 WAL 撑大），
    /// 而"读一眼"不该改库。schema 只核不修：库缺失或版本不是当前值 ⇒ `Err`，由调用方
    /// 走运维失败（版本落后时错误文本会说清"先用可写入口打开一次"）。
    pub fn open_readonly(data_dir: &Path) -> Result<Root, StoreError> {
        let transcript = TranscriptHandle::connect_ro(&data_dir.join("transcript.db"))?;
        let ledger = LedgerHandle::connect_ro(&data_dir.join("ledger.db"))?;
        let memory = MemoryHandle::connect_ro(&data_dir.join("memory.db"))?;
        ddl::require_all_schemas(&transcript, &ledger, &memory)?;
        Ok(Root {
            data_dir: data_dir.to_path_buf(),
            transcript,
            ledger,
            memory,
            // 只读装配没有迁移可言：这一格恒为默认值，`ledger_warning()` 因此恒 `None`。
            migration: MigrationReport::default(),
        })
    }

    /// 节点离线自蒸：转录 + 水位照常开，记忆库指向调用方给的**可写**库（暂存层）。
    /// 构造点仍是 `Root::open*`（守卫 4）；hub 的 `Root::open` 一字不动。
    ///
    /// 与 [`Root::open`] 只差 memory 的路径：另两个库仍然住在 `data_dir` 下，
    /// 因为离线自蒸的**账**属于节点自己（`ledger.db`），只有被蒸的那一册记忆是副本的拷贝。
    /// 迁移照跑：暂存层是底座副本的拷贝，`memory.db` 的版本可能比本产物旧
    /// （B1b 的 `prepare` 只拷文件，不迁 schema）。
    pub fn open_with_memory(data_dir: &Path, memory_db: &Path) -> Result<Root, StoreError> {
        std::fs::create_dir_all(data_dir)?;
        let transcript = TranscriptHandle::connect(&data_dir.join("transcript.db"))?;
        let ledger = LedgerHandle::connect(&data_dir.join("ledger.db"))?;
        let memory = MemoryHandle::connect_rw(memory_db)?;
        let migration = ddl::migrate_all(&transcript, &ledger, &memory)?;
        Ok(Root { data_dir: data_dir.to_path_buf(), transcript, ledger, memory, migration })
    }
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    /// 上一次 `open` 的迁移读数（E2：distill 游标归零的 scope 数）。
    /// 启动面据此响亮打印；读它是纯读，不触发第二次迁移。
    pub fn migration_report(&self) -> &MigrationReport {
        &self.migration
    }
    pub fn transcript(&self) -> &TranscriptHandle {
        &self.transcript
    }
    pub fn transcript_mut(&mut self) -> &mut TranscriptHandle {
        &mut self.transcript
    }
    pub fn ledger(&self) -> &LedgerHandle {
        &self.ledger
    }
    pub fn ledger_mut(&mut self) -> &mut LedgerHandle {
        &mut self.ledger
    }
    pub fn memory(&self) -> &MemoryHandle {
        &self.memory
    }
    pub fn memory_mut(&mut self) -> &mut MemoryHandle {
        &mut self.memory
    }
    /// 只为测试的所有权形状存在（计划 任务 1 Produces 那条）：句柄是从 `Root` 借出来的，
    /// 一个函数不能同时返回 `TempDir` 和它的借用。消费掉 `Root` 会关掉另两个句柄。
    pub fn into_memory(self) -> MemoryHandle {
        self.memory
    }
}

/// 节点侧的装配结果：**转录 RW + 水位 RW + 记忆 RO**。
///
/// 打开一份只读副本。**先问"文件在不在"，再连**：`connect_ro` 对着一个不存在的文件
/// 会报 `StoreError::Sql`（SQLite 的 "unable to open database file"），那与"应该有却
/// 没有"是两种后果 ⇒ 用 `is_file()` 把它们分开。schema 也在这里核，调用方不必自己拼。
pub fn open_replica(path: &Path) -> Result<MemoryHandle, StoreError> {
    if !path.is_file() {
        return Err(StoreError::ReplicaMissing);
    }
    let handle = MemoryHandle::connect_ro(path)?;
    ddl::require_memory_schema(&handle)?;
    Ok(handle)
}

/// 与 `Root` 的区别只有一处，但那一处是承重的：节点的记忆库是**下行整替落地的副本**，
/// 它是只读的。节点的任何写入都只许落在转录层（水位层记"这一条已经上行了"）。
/// 因此"可写记忆"的那个入口在这个类型上**不存在**——不是忘了给，是"给"了就等于给了节点
/// 写副本的能力，而副本是中心的内容，节点写它就是两台机器各写各的。
///
/// `replica_db = None` 的含义是"**还没同步过**"：节点第一次起来时副本文件不存在，
/// 这时它仍然要有转录层（`remember` 要写本机），所以三样里少一样而不是整个起不来。
/// `Some(path)` 而文件不在 ⇒ `StoreError::ReplicaMissing`（"应该有却没有"与"还没有"是两种后果）。
pub struct NodeRoot {
    data_dir: PathBuf,
    transcript: TranscriptHandle,
    ledger: LedgerHandle,
    memory: Option<MemoryHandle>,
    migration: MigrationReport,
}

impl NodeRoot {
    pub fn open_node(data_dir: &Path, replica_db: Option<&Path>) -> Result<NodeRoot, StoreError> {
        std::fs::create_dir_all(data_dir)?;
        let transcript = TranscriptHandle::connect(&data_dir.join("transcript.db"))?;
        let ledger = LedgerHandle::connect(&data_dir.join("ledger.db"))?;
        ddl::migrate_transcript(&transcript)?;
        let ledger_cursor_reset = ddl::migrate_ledger(&ledger)?;
        let memory = match replica_db {
            None => None,
            Some(path) => Some(open_replica(path)?),
        };
        Ok(NodeRoot {
            data_dir: data_dir.to_path_buf(),
            transcript,
            ledger,
            memory,
            migration: MigrationReport { ledger_cursor_reset },
        })
    }
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    pub fn transcript(&self) -> &TranscriptHandle {
        &self.transcript
    }
    pub fn transcript_mut(&mut self) -> &mut TranscriptHandle {
        &mut self.transcript
    }
    pub fn ledger(&self) -> &LedgerHandle {
        &self.ledger
    }
    pub fn ledger_mut(&mut self) -> &mut LedgerHandle {
        &mut self.ledger
    }
    /// 副本句柄。`None` = 这台机器还没同步过，调用方要**显式说"无可用记忆"**，
    /// 不许把它折成"库里没有"（那是最像成功的失败）。
    pub fn memory(&self) -> Option<&MemoryHandle> {
        self.memory.as_ref()
    }
    pub fn migration_report(&self) -> &MigrationReport {
        &self.migration
    }
}

/// H1a：`busy_timeout` 的结构面 + 行为面。行为面不靠墙钟阈值取胜：持锁 300ms 的写者
/// 让第二个 `execute` 必须等，等不到才报错——`busy_timeout=0` 时它会立刻返回 Err，
/// 所以"Ok 且耗时 > 200ms"同时钉住"设了"与"没退化成忙等无限"。
#[cfg(test)]
mod busy_timeout_tests {
    use super::*;

    fn timeout(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn all_handles_carry_the_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        assert_eq!(timeout(root.transcript().conn()), BUSY_TIMEOUT_MS as i64);
        assert_eq!(timeout(root.ledger().conn()), BUSY_TIMEOUT_MS as i64);
        assert_eq!(timeout(root.memory().conn()), BUSY_TIMEOUT_MS as i64);
        let replica = open_replica(&dir.path().join("memory.db")).unwrap();
        assert_eq!(timeout(replica.conn()), BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn a_second_writer_waits_instead_of_failing_busy() {
        let dir = tempfile::tempdir().unwrap();
        let _root = Root::open(dir.path()).unwrap();
        let path = dir.path().join("memory.db");
        let holder = MemoryHandle::connect_rw(&path).unwrap();
        holder.conn().execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let conn = MemoryHandle::connect_rw(&writer_path).unwrap();
            conn.conn().execute(
                "INSERT INTO attention(kind, mem_table, mem_id, created_at_ms)
                 VALUES('quote_conflict','fact','busy-probe',1)",
                [],
            )
        });
        // 让写者先撞上写锁，再把锁放掉：它必须等到这里，而不是立刻 SQLITE_BUSY。
        std::thread::sleep(Duration::from_millis(300));
        holder.conn().execute_batch("COMMIT").unwrap();
        let result = writer.join().unwrap();
        let elapsed = started.elapsed();
        assert!(result.is_ok(), "第二个写者必须等锁成功，实际：{result:?}");
        assert!(
            elapsed >= Duration::from_millis(200),
            "第二个写者没有等锁（耗时 {elapsed:?}）"
        );
    }
}
