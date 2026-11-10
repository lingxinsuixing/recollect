//! 采集锁：`mkdir` 原子建目录 + PID 文件，结束即删。
//!
//! 与 `.drain.lock` 无关：采集是短任务，抽干跑多久都不该挡采集。
//! 判断"持有者还活着"必须问操作系统（Unix `kill -0`、Windows `tasklist`），
//! 不能靠"目录存在"——崩溃留下的空壳会被永远当成占用。

use std::path::{Path, PathBuf};

/// 一把已持有的采集锁；`Drop` 时删除锁目录。
#[derive(Debug)]
pub struct CollectLock {
    dir: PathBuf,
}

impl CollectLock {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for CollectLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 在 `dir` 下尝试拿 `collect.lock`。
///
/// - 拿到了 ⇒ `Ok(Some(lock))`；
/// - 已有**活**进程持有 ⇒ `Ok(None)`；
/// - 目录建不了 ⇒ `Err`。
///
/// 锁目录里 PID 缺失或进程已死 ⇒ 清掉重试一次。
pub fn try_lock(dir: &Path) -> Result<Option<CollectLock>, String> {
    let lock_dir = dir.join("collect.lock");
    match std::fs::create_dir(&lock_dir) {
        Ok(()) => {
            write_pid(&lock_dir)?;
            return Ok(Some(CollectLock { dir: lock_dir }));
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("collect.lock is not creatable".to_string()),
    }

    let pid = std::fs::read_to_string(lock_dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if let Some(pid) = pid {
        if pid_alive(pid) {
            return Ok(None);
        }
    }
    // 死锁（或只建了目录还没来得及写 PID）：清掉重来一次。
    if std::fs::remove_dir_all(&lock_dir).is_err() {
        return Err("collect.lock is not removable".to_string());
    }
    match std::fs::create_dir(&lock_dir) {
        Ok(()) => {
            write_pid(&lock_dir)?;
            Ok(Some(CollectLock { dir: lock_dir }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(_) => Err("collect.lock is not creatable".to_string()),
    }
}

fn write_pid(lock_dir: &Path) -> Result<(), String> {
    std::fs::write(
        lock_dir.join("pid"),
        std::process::id().to_string().as_bytes(),
    )
    .map_err(|_| {
        let _ = std::fs::remove_dir_all(lock_dir);
        "collect.lock is not writable".to_string()
    })
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let filter = format!("PID eq {pid}");
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let want = pid.to_string();
    // 只有真正出现一行该 PID 的记录才算活；"No tasks are running" 不含 PID。
    text.split_whitespace().any(|tok| tok == want)
}

#[cfg(not(any(unix, windows)))]
fn pid_alive(_pid: u32) -> bool {
    false
}
