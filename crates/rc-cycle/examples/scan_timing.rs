//! D3 的计时入口：`rc_cycle::run_anomaly_scan` 的单轮耗时（1k / 10k 合成库，5 次取中位数）。
//!
//! 派发档 §二 逐条落在这里：`Root::open` 建库 → `rc_memory::writers_x::put_card` 以唯一标题/正文
//! 播 1k 与 10k 行（合成中文）→ `rc_index::rebuild` 建索引 → 播种好的库**复制 5 份**，每份一个新
//! 临时目录、只调一次 `run_anomaly_scan(&root, now_ms, 1)`（`rounds = 1` 与首轮全量扫描同）
//! → 打印每档 5 次、中位数、库文件字节数，以及按 `N = FULL_SCAN_EVERY` 摊到每轮的均值。
//!
//! **这是合成库读数，不是生产库标定**：行是 `put_card` 播出来的合成中文，没有真实姓名、设备名或
//! 生产文本；真实记忆库仍不存在（计划 §五）。
//!
//! 每份副本都是**干净副本**：播种函数返回时全部库句柄已经关掉（SQLite 在最后一条连接关闭时把
//! WAL 合并回主文件并删掉 `-wal`/`-shm`），再逐个文件复制；源库旁边只要还剩 `-wal`/`-shm`，
//! 本档就直接失败——那说明手上这一份不是干净副本，计时会量到别人的日志尾。
//!
//! 失败一律退非零并点名**是哪一档、第几次**；这里没有 `unwrap`：扫描失败的原串是封闭类别名
//! （`&'static str`），播种/复制/开库的失败带上各自的动作名。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, FactContent};
use rc_memory::Provenance;
use rc_store::{Root, DATA_FILES};
use std::path::Path;
use std::time::Instant;

/// 每档跑几次：5 次取中位数（派发档 §二）。
const RUNS: usize = 5;
/// 两档库：名字与行数。
const TIERS: [(&str, usize); 2] = [("1k", 1_000), ("10k", 10_000)];
/// 合成行的归属键。与 eval 的合成库不同名，免得两个夹具互相认成邻居。
const SYNTH_SCOPE: &str = "proj-scan-timing-synthetic";
/// 合成时钟：2026-09-22 前后的一个固定毫秒数。**这里刻意不读系统钟**——判据 12a 把取时钟那一支
/// 钉在五个 bin 的 `main.rs` 里（连名字都不许在这里出现），而本 example 量的是一份合成库上的
/// 一次扫描：行的时间、告警的戳、水位都只是库里的数，固定值比真时钟更适合"同一份库跑两遍可比"。
const SYNTH_NOW_MS: i64 = 1_790_000_000_000;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("scan_timing: {why}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    for (label, rows) in TIERS {
        measure(label, rows)?;
    }
    Ok(())
}

/// 例子的钟：固定合成时刻（理由见 `SYNTH_NOW_MS`）。库代码与判据都不自己取时间。
fn now_ms() -> i64 {
    SYNTH_NOW_MS
}

fn measure(label: &str, rows: usize) -> Result<(), String> {
    let base = tempfile::tempdir()
        .map_err(|e| format!("{label} 档：建临时目录失败（{e}）"))?;
    seed(base.path(), rows).map_err(|e| format!("{label} 档：{e}"))?;
    let (memory_bytes, total_bytes) =
        library_bytes(base.path()).map_err(|e| format!("{label} 档：{e}"))?;

    println!("== {label} 合成库 ==");
    println!("行数 {rows}");
    println!("memory.db {memory_bytes} 字节；三库合计 {total_bytes} 字节");

    let mut samples: Vec<f64> = Vec::with_capacity(RUNS);
    for i in 1..=RUNS {
        let copy = tempfile::tempdir()
            .map_err(|e| format!("{label} 档第 {i} 次：建临时目录失败（{e}）"))?;
        copy_clean(base.path(), copy.path())
            .map_err(|e| format!("{label} 档第 {i} 次：{e}"))?;
        let root = Root::open(copy.path())
            .map_err(|e| format!("{label} 档第 {i} 次：开库失败（{e}）"))?;
        let now = now_ms();
        let started = Instant::now();
        rc_cycle::run_anomaly_scan(&root, now, 1)
            .map_err(|cat| format!("{label} 档第 {i} 次：扫描失败（{cat}）"))?;
        let ms = started.elapsed().as_secs_f64() * 1_000.0;
        println!("第 {i} 次 {ms:.3} ms");
        samples.push(ms);
    }

    let median = median(&mut samples).ok_or_else(|| format!("{label} 档：没有样本"))?;
    let list: Vec<String> = samples.iter().map(|ms| format!("{ms:.3}")).collect();
    let amortized = median / rc_cycle::FULL_SCAN_EVERY as f64;
    println!("{RUNS} 次：{} ms", list.join(" / "));
    println!("中位数 {median:.3} ms");
    println!(
        "N={} 摊还 {amortized:.4} ms/轮",
        rc_cycle::FULL_SCAN_EVERY
    );
    Ok(())
}

/// 播种：`Root::open` 建三库 → 逐行 `put_card`（标题/正文唯一）→ 建索引。
/// 返回时 `root` 已经析构，三库的句柄全部关闭 ⇒ WAL 已合并（`copy_clean` 会再核一次）。
fn seed(dir: &Path, rows: usize) -> Result<(), String> {
    let root = Root::open(dir).map_err(|e| format!("播种时开库失败（{e}）"))?;
    let now = now_ms();
    for i in 0..rows {
        let card = FactContent {
            kind: "note".into(),
            title: format!("合成-扫描-{i:05}"),
            body: format!("合成 扫描 计时 第{i}条 样本 备注"),
            scope: Scope { kind: ScopeKind::Project, key: SYNTH_SCOPE.into() },
            valid_to_ms: None,
        };
        // 出处形状与内容寻址 id 同形（64 位小写十六进制），角色一律 human：合成夹具不模拟弱锚。
        let prov = [Provenance {
            turn_uid: format!("{:064x}", i + 1),
            anchor_role: AnchorRole::Human,
        }];
        put_card(root.memory(), &card, &prov, now)
            .map_err(|e| format!("播种时第 {i} 行写入失败（{e}）"))?;
    }
    rc_index::rebuild(root.memory(), now).map_err(|e| format!("播种时建索引失败（{e}）"))?;
    Ok(())
}

/// 三个库文件的字节数：`(memory.db, 三库合计)`。
fn library_bytes(dir: &Path) -> Result<(u64, u64), String> {
    let mut total = 0u64;
    let mut memory = 0u64;
    for name in DATA_FILES {
        let len = std::fs::metadata(dir.join(name))
            .map_err(|e| format!("读 {name} 的字节数失败（{e}）"))?
            .len();
        if name == "memory.db" {
            memory = len;
        }
        total += len;
    }
    Ok((memory, total))
}

/// 干净副本：源库旁边不许有 `-wal`/`-shm`（有 ⇒ 不是干净副本，直接红），再逐个文件复制。
/// 复制而不是 `VACUUM INTO` 的等价理由写在文件头：句柄全关之后 SQLite 已经把 WAL 合并回主文件。
fn copy_clean(from: &Path, to: &Path) -> Result<(), String> {
    for name in DATA_FILES {
        for suffix in ["-wal", "-shm"] {
            if from.join(format!("{name}{suffix}")).exists() {
                return Err(format!("{name}{suffix} 还在源库旁边 ⇒ 这不是一份干净副本"));
            }
        }
        std::fs::copy(from.join(name), to.join(name))
            .map_err(|e| format!("复制 {name} 失败（{e}）"))?;
    }
    Ok(())
}

/// 中位数：5 个数排序后取中间那一个（偶数个取上中位——本档固定 5 次，不会走到）。
fn median(samples: &mut [f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(samples[samples.len() / 2])
}
