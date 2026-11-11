//! 全局日志。
//!
//! 三个出口：
//! 1. 内存环形缓冲（GUI 打开就能看到历史，最多 [`RING_CAP`] 行）
//! 2. 按天轮转的文件 `<dir>/ftpmount-YYYY-MM-DD.log`
//! 3. 可选回调（GUI 用它实时推送到前端；命令行版不设回调）
//!
//! `debug` 级别受 [`set_verbose`] 控制，关闭时**不格式化**（调用方用 `verbose()`
//! 先判断），避免热路径上白白做字符串拼接。

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const RING_CAP: usize = 3000;
/// 日志保留天数
const KEEP_DAYS: i64 = 14;
/// 推送队列容量。满了就丢（日志绝不能拖慢文件系统）。
const SINK_QUEUE: usize = 16384;
/// 等锁超过这个时间就记一笔：说明有线程长时间持有日志锁。
const LOCK_WARN: u128 = 200;
/// 前端单条推送超过这个时间就记一笔：UI 忙，但已与 IO 解耦。
const SINK_WARN: u128 = 500;

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    /// `HH:MM:SS.mmm`
    pub ts: String,
    /// DEBUG / INFO / WARN / ERROR
    pub level: &'static str,
    pub text: String,
}

struct Inner {
    ring: VecDeque<Line>,
    file: Option<(String, File)>,
    dir: Option<PathBuf>,
    /// 推给前端的队列。**只入队、绝不等待**：以前是在持锁状态下直接
    /// 同步调用回调（GUI 那边是 `emit` 到 WebView），UI 一忙就把
    /// 所有正在打日志的文件系统线程一起堵住。
    sink_tx: Option<SyncSender<Line>>,
    /// 上次检查「日志文件还在不在」的时刻。用户把日志文件拷走/删掉之后，
    /// 进程手里的句柄还指着那个（已被移走的）文件，会一直往那边追加，
    /// 而新路径下永远不出现新文件。所以每 2 秒确认一次路径是否还存在。
    checked_at: Instant,
}

fn inner() -> &'static Mutex<Inner> {
    static I: OnceLock<Mutex<Inner>> = OnceLock::new();
    I.get_or_init(|| {
        Mutex::new(Inner {
            ring: VecDeque::with_capacity(RING_CAP),
            file: None,
            dir: None,
            sink_tx: None,
            checked_at: Instant::now(),
        })
    })
}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static ECHO: AtomicBool = AtomicBool::new(false);
/// 最后一次写日志的时刻（毫秒时间戳），给「卡住检测」用。
static LAST_LOG: AtomicU64 = AtomicU64::new(0);

/// 距最后一行日志过去了多少毫秒。0 表示还没写过。
pub fn ms_since_last() -> u64 {
    let last = LAST_LOG.load(Ordering::Relaxed);
    if last == 0 {
        return 0;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_sub(last)
}

/// 详细日志是否开启（`dlog!` 会先问它，再决定要不要 `format!`）。
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

/// 是否同时往 stderr 打（命令行版用；GUI 版关掉）。
pub fn set_echo(v: bool) {
    ECHO.store(v, Ordering::Relaxed);
}

/// 设定日志目录并清理过期日志。可重复调用（目录变了会重新打开文件）。
pub fn set_dir(dir: impl AsRef<Path>) {
    let dir = dir.as_ref().to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    let mut g = inner().lock().unwrap();
    if g.dir.as_ref() == Some(&dir) {
        return;
    }
    g.dir = Some(dir.clone());
    g.file = None;
    drop(g);
    prune(&dir);
}

/// 安装实时回调（覆盖旧的）。
///
/// 回调跑在**独立的 log-sink 线程**上，和打日志的线程完全解耦：
/// `push` 只做一次 `try_send`（永不阻塞），前端再慢也不会反压到
/// 文件系统回调里。
pub fn set_sink(f: Option<Box<dyn Fn(&Line) + Send + Sync>>) {
    let mut g = inner().lock().unwrap();
    match f {
        None => g.sink_tx = None,
        Some(cb) => {
            let cb: Arc<dyn Fn(&Line) + Send + Sync> = Arc::from(cb);
            let (tx, rx) = sync_channel::<Line>(SINK_QUEUE);
            let _ = std::thread::Builder::new()
                .name("log-sink".into())
                .spawn(move || {
                    while let Ok(line) = rx.recv() {
                        let t0 = Instant::now();
                        cb(&line);
                        let ms = t0.elapsed().as_millis();
                        if ms >= SINK_WARN {
                            // 再 push 一次只是入队，不会死锁。
                            warn(&format!(
                                "[日志] 推送前端耗时 {ms}ms（UI 繁忙，已与文件系统解耦，不影响读写）"
                            ));
                        }
                    }
                });
            g.sink_tx = Some(tx);
        }
    }
}

/// 取一份日志快照（最早 → 最新）。
pub fn snapshot() -> Vec<Line> {
    inner().lock().unwrap().ring.iter().cloned().collect()
}

pub fn clear() {
    inner().lock().unwrap().ring.clear();
}

pub fn debug(text: &str) {
    if verbose() {
        push("DEBUG", text);
    }
}

pub fn info(text: &str) {
    push("INFO", text);
}

pub fn warn(text: &str) {
    push("WARN", text);
}

pub fn error(text: &str) {
    push("ERROR", text);
}

fn now_hm() -> (String, String) {
    let n = chrono::Local::now();
    (
        n.format("%Y-%m-%d").to_string(),
        n.format("%H:%M:%S%.3f").to_string(),
    )
}

/// 往环 + 文件里写一行（调用方必须已持有锁）。
fn write_one(g: &mut Inner, date: &str, ts: &str, level: &'static str, text: &str) {
    if g.ring.len() == RING_CAP {
        g.ring.pop_front();
    }
    g.ring.push_back(Line {
        ts: ts.to_string(),
        level,
        text: text.to_string(),
    });

    // 文件：按天轮转；另外每 2 秒确认一次文件还在原处
    if let Some(dir) = g.dir.clone() {
        let p = dir.join(format!("ftpmount-{date}.log"));
        let stale = g.checked_at.elapsed() >= std::time::Duration::from_secs(2);
        if stale {
            g.checked_at = Instant::now();
        }
        let need_open = match &g.file {
            // 日期变了要换文件；文件被移走/删掉了也要重建（否则会一直往
            // 那个已经被挪走的文件句柄里追加，新路径下永远不出现新文件）
            Some((d, _)) => d != date || (stale && !p.exists()),
            None => true,
        };
        if need_open {
            g.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok()
                .map(|f| (date.to_string(), f));
        }
        if let Some((_, f)) = g.file.as_mut() {
            let _ = writeln!(f, "{ts} {level:<5} {text}");
        }
    }
}

fn push(level: &'static str, text: &str) {
    let (date, ts) = now_hm();

    let t_wait = Instant::now();
    let mut g = inner().lock().unwrap();
    let wait_ms = t_wait.elapsed().as_millis();

    write_one(&mut g, &date, &ts, level, text);
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        LAST_LOG.store(d.as_millis() as u64, Ordering::Relaxed);
    }
    if wait_ms >= LOCK_WARN {
        // 持锁时间异常：说明别的线程卡在锁里（历史上前端推送就在锁内）。
        let (_, ts2) = now_hm();
        write_one(
            &mut g,
            &date,
            &ts2,
            "WARN",
            &format!("[日志] 等待日志锁 {wait_ms}ms（有线程长时间持锁）"),
        );
    }
    let tx = g.sink_tx.clone();
    drop(g); // 关键：出锁之后才做任何可能阻塞的事

    if let Some(tx) = tx {
        // try_send：队列满就丢这一行，绝不等待。
        let _ = tx.try_send(Line {
            ts,
            level,
            text: text.to_string(),
        });
    }

    if ECHO.load(Ordering::Relaxed) {
        let (_, ts3) = now_hm();
        eprintln!("{ts3} {level:<5} {text}");
    }
}

fn prune(dir: &Path) {
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(KEEP_DAYS);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("ftpmount-") else {
            continue;
        };
        let Some(dstr) = rest.strip_suffix(".log") else {
            continue;
        };
        if let Ok(d) = chrono::NaiveDate::parse_from_str(dstr, "%Y-%m-%d") {
            if d < cutoff {
                // 先截成 0 再删：本机 C 盘有过滤驱动把删除转进回收站，
                // 不截断的话这些日志会整份躺在回收站里继续占空间。
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(e.path()) {
                    let _ = f.set_len(0);
                }
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}
