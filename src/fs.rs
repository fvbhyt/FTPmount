use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use widestring::{U16CStr, U16CString};
use winfsp::filesystem::{
    DirBuffer, DirInfo, FileInfo, FileSecurity, FileSystemContext, ModificationDescriptor,
    OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::notify::{Notifier, NotifyingFileSystemContext, NotifyInfo};
use winfsp::Result;
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

use crate::ftp::{Entry, Remote};

/// 详细日志：只有开了 verbose 才真正做字符串拼接
macro_rules! dlog {
    ($($t:tt)*) => {
        if crate::logbuf::verbose() {
            crate::logbuf::debug(&format!($($t)*));
        }
    };
}

/// 计时 + 记错误。回调里凡是可能慢或可能失败的地方都套一层，
/// 否则「拷贝卡在 99%」这种问题只能靠猜（错误被 `?` 吞掉，慢操作也没痕迹）。
fn timed<T>(name: &str, f: impl FnOnce() -> E<T>) -> E<T> {
    let _g = Inflight::enter();
    let t0 = Instant::now();
    let r = f();
    let ms = t0.elapsed().as_millis();
    match &r {
        Ok(_) => {
            if ms >= 100 {
                dlog!("[慢] {name} 用了 {ms}ms");
            }
        }
        Err(e) => dlog!("[失败] {name} 用了 {ms}ms -> {e:#x}"),
    }
    r
}

/// 当前正在执行的回调数。
///
/// 用途：区分「卡在我们这里」和「系统那边在等别的东西」。
/// 日志里出现一大段静默、又**没有** `[卡住]` 告警，就说明当时
/// 根本没有请求在飞 —— 资源管理器在等的东西跟文件系统无关。
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
/// 卷最近一次「完全没有请求在飞」的时刻（进程启动后的毫秒数）。
static LAST_IDLE_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

struct Inflight;

impl Inflight {
    #[inline]
    fn enter() -> Self {
        let n = INFLIGHT.fetch_add(1, Ordering::SeqCst);
        // 从「完全空闲」重新变忙时记一笔。有了这对边界日志，
        // 「用户说卡了 N 秒」就能立刻分清是「我们在处理」还是「压根没请求进来」。
        if n == 0 {
            let idle = now_ms().saturating_sub(LAST_IDLE_MS.load(Ordering::SeqCst));
            if idle >= 1500 {
                dlog!("[忙] 空闲 {idle}ms 后收到第一个请求");
            }
        }
        Inflight
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if INFLIGHT.fetch_sub(1, Ordering::SeqCst) == 1 {
            LAST_IDLE_MS.store(now_ms(), Ordering::SeqCst);
            dlog!("[空闲] 卷已无请求在飞");
        }
    }
}

fn inflight() -> usize {
    INFLIGHT.load(Ordering::SeqCst)
}

/// 目录变更通知开关。诊断用：`FTPMOUNT_NOTIFY=0` 关掉，退回 0.1.6 的行为。
fn notify_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("FTPMOUNT_NOTIFY")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// 卡住检测：有请求在执行、却超过 1.5 秒没有任何回调返回，就记一条。
pub fn spawn_stall_watchdog() {
    // 每次挂载都会调一次，只真正起一个线程
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("fs-stall-watchdog".into())
        .spawn(|| {
            let mut reported = false;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let n = inflight();
                if n == 0 {
                    reported = false;
                    continue;
                }
                let idle = crate::logbuf::ms_since_last();
                if !reported && idle >= 1500 {
                    crate::logbuf::warn(&format!(
                        "[卡住] {n} 个请求在执行，已 {idle}ms 没有回调返回"
                    ));
                    reported = true;
                }
            }
        });
}

pub const ATTR_DIR: u32 = 0x10;
pub const ATTR_ARCHIVE: u32 = 0x20;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
/// FspCleanupDelete：cleanup 时要求真正删除
const FSP_CLEANUP_DELETE: u32 = 0x01;
/// Windows 标准访问权限位。winfsp-sys 里 FILE_ACCESS_RIGHTS 只是 u32 别名，
/// 没有封装这些常量，只能自己写。
const FILE_READ_DATA: u32 = 0x0000_0001;
const FILE_WRITE_DATA: u32 = 0x0000_0002;
const FILE_APPEND_DATA: u32 = 0x0000_0004;
/// 只读惰性打开时，一次 REST 至少预取多少字节。
/// 太小 → 大文件顺序读会退化成几万次数据连接（实测报 I/O 设备错误）；
/// 太大 → 慢线路上单次传输会超过超时。8MB 在千兆局域网约 0.1 秒。
const READ_AHEAD: u64 = 16 << 20;

type E<T> = std::result::Result<T, u32>;

pub struct FtpFs {
    pub remote: Arc<Remote>,
    pub cache_dir: PathBuf,
    seq: AtomicU64,
    /// 只在本地存在、还没上传的文件（小写路径 → 条目）
    pending: Mutex<HashMap<String, Entry>>,
    /// 本地时间戳覆盖表：远端绝对路径 → FILETIME。
    /// 服务器不支持 MFMT/SITE UTIME，上传后 mtime 会变成上传时刻；这里记住
    /// 客户端希望的修改时间，列表/属性时优先用它（否则拷贝文件的时间会被冲掉）。
    mtime_over: Mutex<HashMap<String, u64>>,
    mtime_file: PathBuf,
    /// 待上报给系统的目录变更事件。资源管理器拷完文件后要靠
    /// `ReadDirectoryChangesW` 收到「新增了这个文件」才把进度推到 100% 并关掉
    /// 对话框；收不到就只能干等它自己的兜底超时（实测约 6.2 秒，正好是用户
    /// 报的「卡在 99%」）。
    notify_q: Mutex<Vec<NotifyEvent>>,
}

/// 一条待上报的目录变更事件。
pub struct NotifyEvent {
    /// 相对卷根的 Windows 路径，例如 `\ISO\a.md`
    pub path: String,
    /// FILE_NOTIFY_CHANGE_* 掩码
    pub filter: u32,
    /// FILE_ACTION_*
    pub action: u32,
}

// FILE_ACTION_*（win32）
const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_REMOVED: u32 = 2;
const FILE_ACTION_MODIFIED: u32 = 3;
const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

// FILE_NOTIFY_CHANGE_*（win32）
const NC_FILE_NAME: u32 = 0x0000_0001;
const NC_DIR_NAME: u32 = 0x0000_0002;
const NC_ATTRIBUTES: u32 = 0x0000_0004;
const NC_SIZE: u32 = 0x0000_0008;
const NC_LAST_WRITE: u32 = 0x0000_0010;

pub struct Handle {
    /// Windows 形式路径，例如 `\sub\a.txt`，根为 `\`。rename 会改它，所以要锁。
    path: Mutex<String>,
    /// 服务器上的**真实**路径，大小写以服务器 LIST 返回的为准。
    /// 所有 FTP 命令（RETR/STOR/DELE/RMD/RNFR）都必须用它：
    /// Windows 侧大小写不敏感，调用方会拿 `\STRESS2\F9.TXT` 来问，
    /// 而 FTP 服务器大小写敏感，拿请求路径发命令只会 550。
    remote_path: Mutex<String>,
    pub is_dir: bool,
    pub dir: DirBuffer,
    pub meta: Mutex<Entry>,
    pub st: Mutex<FileState>,
}

impl Handle {
    pub fn path(&self) -> String {
        self.path.lock().unwrap().clone()
    }
    pub fn remote_path(&self) -> String {
        self.remote_path.lock().unwrap().clone()
    }
}

/// 流式上传通道里最多压几帧（每帧一次 write，通常 1MB）。
/// 帧数决定「写」能比网络跑快多少：2 帧 ≈ 2MB 余量，
/// 既能让小抖动不影响吞吐，又能让进度条紧跟真实上传速度。
const STREAM_SLACK: usize = 2;
/// 流式上传的停滞截止：连续这么久一个字节都发不出去，就判定网络死了，
/// 掐掉通道让 `write` 立刻报 I/O 错误，而不是陪着卡到天荒地老。
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(120);
/// 流式上传收尾（等服务器回 `226 Transfer complete`）的硬上限。
///
/// 数据在 `finish` 之前就已经全部喂进 STOR（通道已 EOF），这一步纯粹是在等
/// 服务器的尾包应答。服务器偶尔迟迟不回这个 226（数据其实已发完），若无限等
/// 会让我们在 `cleanup` 里干等最多 `FTP_IO_TIMEOUT`(6s)，资源管理器就表现为
/// 「进度到 99% 卡 3~5 秒」。这里封顶到 3 秒：超时即认为数据已发完、远端多半
/// 已经收全，直接返回已发送字节，不让拷贝卡死；真正还在跑的上传线程在后台自行
/// 收尾、用完连接后归还/丢弃，不影响后续操作。正常服务器（226 毫秒级返回）完全
/// 不受影响。
const STREAM_FINISH_TIMEOUT: Duration = Duration::from_secs(3);

/// 流式上传允许缓存多少「乱序到达」的字节。
///
/// 拷贝引擎的写入会乱序到达：实测 8.5GB 拷贝里有一笔 1MB 的写入比后面那笔
/// 晚了 **41 毫秒**，看起来像「跳过一块」，其实只是顺序换了。一条顺序 `STOR`
/// 没法直接表达，所以先把它们攒在内存里，等前面的空洞补上再按序投递。
/// 这个上限是安全阀：真积压到 64MB 说明不是零星乱序，宁可掐流。
const STREAM_REORDER_MAX: usize = 64 << 20;

/// 流式上传期间本地缓存最多保留多少字节。
///
/// 缓存的本意是「流断了就退回整文件回传」的兜底，但兜底不需要整份副本：
/// 流跑过 8MB 就足以说明连接是通的。再往后继续落盘只会在 C 盘铺一份完整
/// 副本 —— 8.5GB 的拷贝就是 8.5GB 白占（实测两次拷贝把 C 盘吃掉 13.6GB）。
/// 越过这个阈值就把缓存截成 0 并摘掉句柄，之后一个字节都不再落盘。
const STREAM_CACHE_KEEP: u64 = 8 << 20;

/// 边写边传的上传通道。
///
/// 以前所有写入都先落本地缓存、等句柄关闭时才整文件 STOR，导致资源管理器
/// 在数据写完的瞬间就把进度顶到 99%，然后要干等几分钟（实测 8.5GB 等了
/// 4 分 14 秒），用户以为拷死了只能强杀。改成流式后，`do_write` 把数据
/// 投进通道，网络有多快写请求就多快，进度条反映的是真实上传进度。
pub struct StreamUp {
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    join: Option<std::thread::JoinHandle<std::result::Result<u64, String>>>,
    /// 已经交给 STOR 的字节数，同时也是远端文件此刻的长度
    pub sent: u64,
    /// 乱序到达、还没到投递时机的写入（远端偏移 → 数据）。
    /// 等前面的空洞补齐后按偏移顺序投递，见 [`FtpFs::drain_stream`]。
    pending: std::collections::BTreeMap<u64, Vec<u8>>,
    pending_bytes: usize,
}

impl StreamUp {
    /// 收尾：关通道（读端收到 EOF）再等上传线程结束。
    ///
    /// 正常情况下这里只剩通道里那点尾巴，几毫秒就返回。但服务器偶尔会**迟迟不回
    /// `226 Transfer complete` 这个尾包**——数据其实已经全部发出（通道已 EOF、
    /// 写端不再阻塞），只是控制连接卡在等应答。这会让我们在 `cleanup` 里干等最多
    /// `FTP_IO_TIMEOUT`(6s)，资源管理器就表现为「进度到 99% 卡 3~5 秒」。
    ///
    /// 这里给收尾加一个硬上限 [`STREAM_FINISH_TIMEOUT`]：超时即认为数据已发完、
    /// 远端多半已经收全，直接返回已发送字节，不让拷贝卡死；真正还在跑的上传线程
    /// 在后台自行收尾、用完连接后归还/丢弃，不影响后续操作。正常服务器（226 毫秒
    /// 级返回）完全走不到超时分支。
    fn finish(mut self) -> std::result::Result<u64, String> {
        let sent = self.sent;
        // 关通道：读端收 EOF，STOR 随之结束
        self.tx.take();
        let join = match self.join.take() {
            Some(j) => j,
            None => return Ok(sent),
        };
        // 把 join 搬到一条独立线程上，主路径用带截止的通道等结果，
        // 这样收尾永远不会阻塞超过 STREAM_FINISH_TIMEOUT。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(join.join());
        });
        match rx.recv_timeout(STREAM_FINISH_TIMEOUT) {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("上传线程异常退出".to_string()),
            // 超时或通道断开：数据已经全部喂进 STOR，视为已发送，不让拷贝卡死。
            // 真正还在跑的上传线程会在后台自行收尾、用完连接后归还/丢弃。
            Err(_) => {
                dlog!("stream 收尾超过 {STREAM_FINISH_TIMEOUT:?} 未返回（疑似服务器迟发 226），视为已发送 {sent} 字节");
                Ok(sent)
            }
        }
    }
}

pub struct FileState {
    /// 本地缓存文件：读写都打本地，关闭时才整体回传 FTP
    pub file: Option<File>,
    pub cache_path: Option<PathBuf>,
    pub dirty: bool,
    /// 远端还没有这个文件
    pub created: bool,
    pub delete_flag: bool,
    /// 远端已经删掉了（目录在 set_delete 里就删，避免重复 RMD）
    pub removed: bool,
    pub size: u64,
    /// 内容已完整下载（写打开 / 本地 pending 文件）。为 true 时 read 直接走本地，
    /// 不再做区间补齐；为 false 时是只读惰性打开，内容按区间（REST）按需拉取。
    pub whole: bool,
    /// 已下载的字节区间（只读惰性打开时维护），用于判断某次 read 是否还要补拉。
    pub ranges: Vec<(u64, u64)>,
    /// 本句柄是否已被显式设置过时间戳（set_basic_info）。为 true 时
    /// do_write 不再把 mtime 改成当前时间——否则「拷贝文件」的源时间会被写操作冲掉。
    pub time_locked: bool,
    /// 正在进行的流式上传。Some 表示这个句柄的写入正直接发往 STOR，
    /// 关闭时不需要再整文件回传。详见 [`StreamUp`]。
    pub stream: Option<StreamUp>,
    /// 这个句柄**整份重写**远端文件：新建，或 `CREATE_ALWAYS` 覆盖已存在文件。
    ///
    /// 只有它为 true 才允许开流式上传。流是一条顺序 `STOR`，开流就等于先把远端
    /// 文件截掉，所以「打开已有文件改几个字节」那种句柄（`whole=true` 但内容
    /// 是下载下来再改的）绝不能开流 —— 否则只写开头一块，远端文件就被截短了。
    pub fresh: bool,
}

impl FileState {
    fn empty() -> Self {
        Self {
            file: None,
            cache_path: None,
            dirty: false,
            created: false,
            delete_flag: false,
            removed: false,
            size: 0,
            whole: false,
            ranges: Vec::new(),
                time_locked: false,
                stream: None,
                fresh: false,
        }
    }
}

/// 文件索引号（NTFS 的 FileId 等价物）。**必须每个文件不同**。
///
/// 以前一律填 0，结果是资源管理器做「源文件和目标文件是不是同一个」的判断时
/// 拿到的索引号全都一样，于是它认为"这就是同一个文件、没必要问"，
/// 复制同名文件时直接覆盖、不弹「替换或跳过」。用远端路径做 FNV-1a 哈希，
/// 保证同一卷内不同路径得到不同索引号（最低位置 1，避免出现 0）。
fn index_of(key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h | 1
}

fn fill_info(info: &mut FileInfo, e: &Entry, key: &str) {
    info.file_attributes = if e.is_dir { ATTR_DIR } else { ATTR_ARCHIVE };
    info.file_size = e.size;
    info.allocation_size = e.size.div_ceil(4096) * 4096;
    info.creation_time = e.mtime;
    info.last_access_time = e.mtime;
    info.last_write_time = e.mtime;
    info.change_time = e.mtime;
    info.index_number = index_of(key);
    info.hard_links = 1;
    info.ea_size = 0;
    info.reparse_tag = 0;
}

fn root_entry() -> Entry {
    Entry {
        name: String::new(),
        is_dir: true,
        size: 0,
        mtime: 0,
    }
}

/// 当前时间 → Windows FILETIME（100ns since 1601-01-01）。
/// 新建 / 拷贝文件时本地必须给个合理时间，否则 Explorer 会显示 1601 或空白。
fn now_filetime() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() + crate::ftp::FILETIME_EPOCH_DIFF as u64) * 10_000_000
        + (d.subsec_nanos() as u64) / 100
}

/// 统一反斜杠、去掉首尾分隔符
pub fn norm(win_path: &str) -> String {
    win_path.trim_matches('\\').to_string()
}

fn split_parent(w: &str) -> (String, String) {
    match w.rfind('\\') {
        Some(i) => (w[..i].to_string(), w[i + 1..].to_string()),
        None => (String::new(), w.to_string()),
    }
}

/// 拼远端路径：`/` + 名字要特殊处理，避免出现 `//name`
fn join_remote(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 取远端路径的父目录，根目录返回 `/`
fn remote_parent(rp: &str) -> String {
    match rp.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => rp[..i].to_string(),
    }
}

/// 读取持久化的时间戳覆盖表。格式：每行 `mtime<TAB>远端路径`。
fn load_overrides(path: &PathBuf) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    if let Ok(txt) = std::fs::read_to_string(path) {
        for line in txt.lines() {
            if let Some((v, p)) = line.split_once('\t') {
                if let Ok(t) = v.parse::<u64>() {
                    if !p.is_empty() {
                        m.insert(p.to_string(), t);
                    }
                }
            }
        }
    }
    m
}

/// 计算 [s, e) 中尚未下载的区间。`ranges` 必须是已排序、互不重叠的区间集合。
fn missing_ranges(ranges: &[(u64, u64)], s: u64, e: u64) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    let mut cur = s;
    for &(rs, re) in ranges {
        if re <= cur {
            continue;
        }
        if rs > cur {
            gaps.push((cur, rs.min(e)));
        }
        cur = cur.max(re);
        if cur >= e {
            break;
        }
    }
    if cur < e {
        gaps.push((cur, e));
    }
    gaps.into_iter().filter(|(a, b)| b > a).collect()
}

/// 合并区间（排序 + 合并重叠/相邻），只读惰性打开时维护已下载区间。
fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|r| r.0);
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        if let Some(last) = out.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

// ---------------------------------------------------------------------------
// 核心逻辑：不依赖任何 WinFsp 类型，可以脱离驱动单独测试
// ---------------------------------------------------------------------------
impl FtpFs {
    pub fn new(remote: Arc<Remote>) -> Self {
        let cache_dir = cache_root();
        let _ = std::fs::create_dir_all(&cache_dir);
        let mtime_file = cache_dir.join("mtime_overrides.txt");
        let mtime_over = load_overrides(&mtime_file);
        let (n, bytes) = purge_cache(&cache_dir);
        if n > 0 {
            crate::logbuf::info(&format!(
                "清理上次残留的缓存文件 {n} 个（{} MB）",
                bytes / (1024 * 1024)
            ));
        }
        // 卡住检测线程：只在一个挂载里起一次就够，重复起也不影响正确性。
        spawn_stall_watchdog();
        Self {
            remote,
            cache_dir,
            seq: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            mtime_over: Mutex::new(mtime_over),
            mtime_file,
            notify_q: Mutex::new(Vec::new()),
        }
    }

    /// 记一条目录变更事件，等 notify 定时器上报给系统。
    ///
    /// 队列有硬上限：满了直接丢。通知丢了最多是资源管理器晚一点刷新视图，
    /// 而让 IO 线程在这里阻塞会把整个挂载点拖死（0.1.5 的教训）。
    fn push_notify(&self, path: &str, action: u32, filter: u32) {
        if !notify_enabled() {
            return;
        }
        let mut q = self.notify_q.lock().unwrap();
        if q.len() >= 4096 {
            return;
        }
        q.push(NotifyEvent {
            path: path.to_string(),
            action,
            filter,
        });
    }

    fn next_cache_path(&self) -> PathBuf {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        self.cache_dir
            .join(format!("{}-{}.tmp", std::process::id(), n))
    }

    /// 保存覆盖表（先写临时文件再改名，避免写一半损坏）
    fn save_overrides(&self, m: &HashMap<String, u64>) {
        let mut out = String::with_capacity(m.len() * 32);
        for (p, t) in m.iter() {
            if p.contains('\t') || p.contains('\n') || p.contains('\r') {
                continue;
            }
            out.push_str(&format!("{t}\t{p}\n"));
        }
        let tmp = self.mtime_file.with_extension("tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &self.mtime_file);
        }
    }

    fn set_override(&self, remote: &str, t: u64) {
        if t == 0 {
            return;
        }
        let mut m = self.mtime_over.lock().unwrap();
        if m.get(remote).copied() != Some(t) {
            m.insert(remote.to_string(), t);
            self.save_overrides(&m);
        }
    }

    fn remove_override(&self, remote: &str) {
        let mut m = self.mtime_over.lock().unwrap();
        if m.remove(remote).is_some() {
            self.save_overrides(&m);
        }
    }

    fn override_for(&self, remote: &str) -> Option<u64> {
        self.mtime_over.lock().unwrap().get(remote).copied()
    }

    /// 逐级把 Windows 路径解析成服务器上的真实路径，返回 (真实远端路径, 条目)。
    ///
    /// 以前是「LIST 一次父目录 + 忽略大小写找名字」，查找本身是对的，但真正发
    /// FTP 命令时用的是**请求时的大小写**。Windows 侧大小写不敏感，msys 和
    /// Explorer 都会拿 `\STRESS2\F9.TXT` 这种形式来问；FTP 服务器大小写敏感，
    /// LIST/RETR 这种路径直接 550 → 调用方不停重试 → CPU 打满、机器卡死
    /// （2026-10-18 实测：40 个文件的目录上 `ls -la` 挂死，进程稳定 30% CPU）。
    /// 所以必须逐级用服务器返回的真实名字拼路径。
    pub fn resolve(&self, win_path: &str) -> Option<(String, Entry)> {
        let w = norm(win_path);
        let root = self.remote.abs("");
        if w.is_empty() {
            return Some((root, root_entry()));
        }
        // 只在本地、还没上传的文件：远端没有对应路径，按请求大小写拼即可
        if let Some(e) = self.is_pending(&w) {
            return Some((self.remote.abs(&w), e));
        }
        let mut cur = root;
        let mut last = root_entry();
        for comp in w.split('\\').filter(|c| !c.is_empty()) {
            // 这里以前是 `.ok()?`，把 FTP 错误吞成了「路径不存在」——
            // 服务器抖一下，上层就以为文件没了，资源管理器只好重试。
            let entries = match self.remote.list(&cur) {
                Ok(v) => v,
                Err(e) => {
                    dlog!("resolve {win_path}: list {cur} 失败: {e}");
                    return None;
                }
            };
            let e = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(comp))?;
            cur = match cur.as_str() {
                "/" => format!("/{}", e.name),
                other => format!("{other}/{}", e.name),
            };
            last = e;
        }
        // 本地时间戳覆盖优先（服务器不支持 MFMT，上传后 mtime 会被改成上传时刻）
        if let Some(t) = self.override_for(&cur) {
            last.mtime = t;
        }
        Some((cur, last))
    }

    pub fn stat(&self, win_path: &str) -> Option<Entry> {
        self.resolve(win_path).map(|(_, e)| e)
    }

    fn mark_pending(&self, win_path: &str, e: Entry) {
        self.pending
            .lock()
            .unwrap()
            .insert(norm(win_path).to_lowercase(), e);
    }

    fn clear_pending(&self, win_path: &str) {
        self.pending
            .lock()
            .unwrap()
            .remove(&norm(win_path).to_lowercase());
    }

    /// 写入后同步 pending 条目的大小（pending 条目的其他字段在创建时定死）
    fn bump_pending_size(&self, win_path: &str, size: u64) {
        let key = norm(win_path).to_lowercase();
        if let Some(e) = self.pending.lock().unwrap().get_mut(&key) {
            e.size = size;
        }
    }

    /// 这个路径是否只在本地、远端还没有
    fn is_pending(&self, win_path: &str) -> Option<Entry> {
        self.pending
            .lock()
            .unwrap()
            .get(&norm(win_path).to_lowercase())
            .cloned()
    }

    fn rename_pending(&self, from: &str, to: &str) {
        let mut g = self.pending.lock().unwrap();
        if let Some(mut e) = g.remove(&norm(from).to_lowercase()) {
            let (_, name) = split_parent(&norm(to));
            e.name = name;
            g.insert(norm(to).to_lowercase(), e);
        }
    }

    /// 打开已有文件：把远端内容落到本地缓存。
    /// need_data=false 表示调用方只要属性（没有读写数据权限），不用下载。
    pub fn begin_open(&self, win_path: &str, granted_access: u32) -> E<Handle> {
        let (remote_real, meta) = self.resolve(win_path).ok_or(0xC000_0034u32)?;
        // 是否带写入权限。只有「要改文件」才需要在打开时把内容整文件拉下来；
        // 只读打开（ls -la 的 stat、属性查询、缩略图探测都走这路）一律延迟到
        // 第一次真正 read 再下载，否则装满几 GB ISO 的目录会被 `ls` 卡死。
        let need_write = granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
        let st = if meta.is_dir {
            FileState::empty()
        } else if self.is_pending(win_path).is_some() {
            // 只在本地、还没上传过：远端根本没有这个文件，不能去下载。
            // 以前这里会直接 RETR 失败，导致刚建的文件打不开、也就删不掉，
            // 在资源管理器里表现成「删不掉的文件」。
            //
            // 注意 created 必须是 false：这个句柄是**打开**来的，不是创建来的，
            // 内容以创建它的那个句柄为准。如果照抄 begin_create 的 created=true，
            // 本句柄 cleanup 时会把自己这份空缓存 STOR 回服务器，把创建句柄
            // 刚传好的内容覆盖成 0 字节（2026-10-18 实测：echo 写 18 字节，
            // 上传成功后立刻被并发打开的句柄传成空文件）。
            let cache = self.next_cache_path();
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&cache)
                .map_err(|_| 0xC000_0185u32)?;
            if meta.size > 0 {
                let _ = f.set_len(meta.size);
            }
                     FileState {
                file: Some(f),
                cache_path: Some(cache),
                dirty: false,
                created: false, // 打开来的句柄永远不负责上传
                delete_flag: false,
                removed: false,
                size: meta.size,
                whole: true, // 本地 pending 文件，内容就在本地缓存里
                ranges: Vec::new(),
                time_locked: false,
                stream: None,
                fresh: false,
            }
        } else {
            let st = if need_write {
                // 需要可写：**不**在打开时下载，只准备一个空缓存。
                //
                // 旧内容等真正要用的时候再补（见 ensure_whole）：第一次 write、
                // SetEndOfFile、或者读到还没缓存的区间。
                //
                // 为什么必须改成惰性 —— 内核处理 `FILE_OVERWRITE_IF` 的顺序是
                // 「Open(写) → Overwrite」，而 Overwrite 会立刻把内容截成 0，
                // 打开时下载的字节一个都用不上。资源管理器「替换目标中的文件」
                // 走的正是这条路径，所以覆盖一个 8GB 文件会先白下 8GB。
                let cache = self.next_cache_path();
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&cache)
                    .map_err(|_| 0xC000_0185u32)?;
                FileState {
                    file: Some(f),
                    cache_path: Some(cache),
                    dirty: false,
                    created: false,
                    delete_flag: false,
                    removed: false,
                    size: meta.size,
                    // 缓存还是空的：read 走区间补拉，write/set_len 前先 ensure_whole。
                    whole: false,
                    ranges: Vec::new(),
                time_locked: false,
                stream: None,
                fresh: false,
                }
            } else {
                // 只查属性的打开（没有 FILE_READ_DATA/FILE_WRITE_DATA 权限）：
                // 不建本地缓存文件。目录列举时每个文件要被打开 3~4 次
                // （属性查询 + 大小写重试），每次 create+delete 一个临时文件，
                // 40 个文件就是 160 次磁盘 I/O，实测 `ls -la` 从毫秒级劣化到
                // 4.7 秒（本机每次落盘约 25ms）。属性查询只需要 meta 里的
                // 大小/时间，不需要文件内容；真读写的句柄必然带数据权限位，
                // 走不到这个分支。
                FileState {
                    file: None,
                    cache_path: None,
                    dirty: false,
                    created: false,
                    delete_flag: false,
                    removed: false,
                    size: meta.size,
                    whole: false, // 只读惰性：内容按区间按需拉取
                    ranges: Vec::new(),
                time_locked: false,
                stream: None,
                fresh: false,
                }
            };
            st
        };
        Ok(Handle {
            path: Mutex::new(win_path.to_string()),
            remote_path: Mutex::new(remote_real),
            is_dir: meta.is_dir,
            dir: DirBuffer::new(),
            meta: Mutex::new(meta),
            st: Mutex::new(st),
        })
    }

    /// 新建文件或目录
    pub fn begin_create(&self, win_path: &str, is_dir: bool, alloc: u64) -> E<Handle> {
        if is_dir {
            // 目录没有内容，直接建到服务器上，不需要缓存
            let remote = self.remote.abs(win_path);
            if let Err(e) = self.remote.mkd(&remote) {
                // 以前一律报 0xC000_0035（文件已存在），服务器连不上 / 没权限时
                // 资源管理器就会显示「该项目已存在」，用户完全没法判断真实原因。
                if self.resolve(win_path).is_some() {
                    return Err(0xC000_0035); // 真的已存在
                }
                dlog!("mkdir {win_path} 失败: {e}");
                return Err(map_ftp_err_ctx("新建目录", &remote, &e));
            }
            let (_, name) = split_parent(&norm(win_path));
            let meta = Entry {
                name,
                is_dir: true,
                size: 0,
                mtime: now_filetime(),
            };
            self.set_override(&self.remote.abs(win_path), meta.mtime);
            dlog!("mkdir {win_path}");
            return Ok(Handle {
                path: Mutex::new(win_path.to_string()),
                remote_path: Mutex::new(self.remote.abs(win_path)),
                is_dir: true,
                dir: DirBuffer::new(),
                meta: Mutex::new(meta),
                st: Mutex::new(FileState::empty()),
            });
        }

        let cache = self.next_cache_path();
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&cache)
            .map_err(|_| 0xC000_0185u32)?;
        // CREATE_ALWAYS 覆盖**已存在**的远端文件时，这不是"新建"：
        // 不能 mark_pending / created=true，否则会被当成仅本地文件（删不掉远端、
        // 列表里还会冒出幽灵项）。按"覆盖"处理：空缓存 + dirty，关闭时整体回传。
        let existing = self.resolve(win_path).filter(|(_, e)| !e.is_dir);
        let is_new = existing.is_none();
        let (name, mtime) = match &existing {
            Some((_, e)) => (e.name.clone(), e.mtime),
            None => (split_parent(&norm(win_path)).1, now_filetime()),
        };
        let meta = Entry {
            name,
            is_dir: false,
            size: alloc,
            mtime,
        };
        if is_new {
            self.mark_pending(win_path, meta.clone());
        }
        self.set_override(&self.remote.abs(win_path), meta.mtime);
        dlog!("create {win_path} alloc={alloc} new={is_new}");
        Ok(Handle {
            path: Mutex::new(win_path.to_string()),
            remote_path: Mutex::new(self.remote.abs(win_path)),
            is_dir: false,
            dir: DirBuffer::new(),
            meta: Mutex::new(meta),
            st: Mutex::new(FileState {
                file: Some(f),
                cache_path: Some(cache),
                dirty: true,
                created: is_new,
                delete_flag: false,
                removed: false,
                size: alloc,
                whole: true, // 新建/覆盖：内容以本地这份空缓存为准
                ranges: Vec::new(),
                time_locked: false,
                stream: None,
                // 新建 和 CREATE_ALWAYS 覆盖都算「整份重写」，允许开流式。
                fresh: true,
            }),
        })
    }

    /// 把远端文件整份补进本地缓存（惰性写打开的「补齐」）。
    ///
    /// `whole == false` 表示本地缓存不完整：要么是只读惰性打开（按区间补拉即可），
    /// 要么是写打开（随机写需要完整旧内容）。**要改内容之前必须先调这里**，
    /// 否则在空缓存上 `seek_write` / `set_len` 会把没碰过的区间变成 0，文件直接坏掉。
    fn ensure_whole(&self, remote: &str, st: &mut FileState) -> E<()> {
        if st.whole {
            return Ok(());
        }
        let n = {
            let f = st.file.as_mut().ok_or(0xC000_0185u32)?;
            f.set_len(0).map_err(|_| 0xC000_0185u32)?;
            self.remote
                .retr_to_file_cached(remote, f)
                .map_err(|_| 0xC000_0185u32)?
        };
        st.size = n;
        st.whole = true;
        st.ranges.clear();
        dlog!("{remote} 惰性补齐 {n} 字节");
        Ok(())
    }

    pub fn do_read(&self, h: &Handle, buffer: &mut [u8], offset: u64) -> E<u32> {
        if h.is_dir {
            return Err(0xC000_0102);
        }
        let size = { h.st.lock().unwrap().size };
        if offset >= size {
            return Ok(0);
        }
        let want = (buffer.len() as u64).min(size - offset) as usize;
        if want == 0 {
            return Ok(0);
        }

        let remote = h.remote_path();
        {
            let mut st = h.st.lock().unwrap();
            // 只读惰性打开：第一次读才建本地缓存。之后按区间（REST）只拉需要的那一段，
            // **绝不整文件下载**——否则装满 GB 级 ISO 的目录 `ls -la` 会触发整文件
            // RETR，把唯一的 FTP 连接占住几十秒（实测 40~86s），整盘卡死。
            if st.file.is_none() {
                let cache = self.next_cache_path();
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&cache)
                    .map_err(|_| 0xC000_0185u32)?;
                st.file = Some(f);
                st.cache_path = Some(cache);
                st.whole = false;
            }
            // 写打开 / 新建的文件（whole=true）内容已在本地，直接读；
            // 只读惰性打开（whole=false）才需要补拉缺失区间。
            if !st.whole {
                let end = offset + want as u64;
                let mut gaps = missing_ranges(&st.ranges, offset, end);
                // 预读：如果只拉「这次要读的那几个字节」，拷贝一个 5GB 文件会变成
                // 几万次「一个 REST + 一条新数据连接」，服务器根本扛不住，实测直接
                // 抛 0x8007045D（I/O 设备错误）。所以缺口尾部多拉 READ_AHEAD，
                // 顺序读就变成每 8MB 一次往返。
                if let Some(last) = gaps.last_mut() {
                    last.1 = (last.1 + READ_AHEAD).min(size).max(last.1);
                }
                if !gaps.is_empty() {
                    let mut done: Vec<(u64, u64)> = Vec::with_capacity(gaps.len());
                    {
                        let f = st.file.as_mut().ok_or(0xC000_0185u32)?;
                        let cur_len = f.metadata().map(|m| m.len()).unwrap_or(0);
                        let need = gaps.iter().map(|g| g.1).max().unwrap_or(0);
                        if need > cur_len {
                            let _ = f.set_len(need);
                        }
                        for (s, e) in gaps.iter().copied() {
                            let got = self
                                .remote
                                .retr_range(&remote, f, s, e)
                                .map_err(|err| map_ftp_err_ctx("读取区间", &remote, &err))?;
                            // 只登记**真正拿到**的那一段：服务器提前断流时不能
                            // 把整段都标成已下载，否则后面的读会读到空洞（全 0）。
                            done.push((s, s + got));
                        }
                    }
                    let mut all = st.ranges.clone();
                    all.extend(done);
                    st.ranges = merge_ranges(all);
                }
            }
        }
        let st = h.st.lock().unwrap();
        let f = st.file.as_ref().ok_or(0xC000_0185u32)?;
        f.seek_read(&mut buffer[..want], offset)
            .map(|n| n as u32)
            .map_err(|_| 0xC000_0185u32)
    }

    /// 开一条流式上传：专用线程跑 STOR，`do_write` 往通道里投数据。
    fn start_stream(&self, h: &Handle, st: &mut FileState) -> E<()> {
        let remote = h.remote_path();
        // 通道只压 STREAM_SLACK 帧：既留一点抖动余量，又不会让
        // 「本地已经写完」和「服务器已经收到」差得太远。
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(STREAM_SLACK);
        let r = self.remote.clone();
        let name = remote.clone();
        let join = std::thread::spawn(move || r.stor_stream(&name, rx).map_err(|e| e.to_string()));
        dlog!("stream start {remote}");
        st.stream = Some(StreamUp {
            tx: Some(tx),
            join: Some(join),
            sent: 0,
            pending: std::collections::BTreeMap::new(),
            pending_bytes: 0,
        });
        Ok(())
    }

    /// 把一帧数据投给上传线程。
    ///
    /// 通道满就小睡重试，直到网络把数据发出去 —— 这正是「进度条跟着
    /// 真实上传速度走」的来源。连续 [`STREAM_STALL_TIMEOUT`] 一个字节
    /// 都发不出去，才判定连接死了。
    fn feed_stream(&self, st: &mut FileState, buf: &[u8]) -> E<()> {
        let Some(s) = st.stream.as_mut() else {
            return Ok(());
        };
        let Some(tx) = s.tx.as_ref() else {
            return Err(0xC000_0185);
        };
        let deadline = Instant::now() + STREAM_STALL_TIMEOUT;
        let mut payload = buf.to_vec();
        loop {
            match tx.try_send(payload) {
                Ok(()) => {
                    s.sent += buf.len() as u64;
                    return Ok(());
                }
                Err(std::sync::mpsc::TrySendError::Full(p)) => {
                    if Instant::now() >= deadline {
                        dlog!("stream 停滞超过 {STREAM_STALL_TIMEOUT:?}，放弃");
                        return Err(0xC000_0185);
                    }
                    payload = p;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    dlog!("stream 上传线程已退出");
                    return Err(0xC000_0185);
                }
            }
        }
    }

    /// 往流里补 `n` 个 0 字节，把流位置推到下一个写入偏移。
    ///
    /// 只在「拷贝引擎跳过了一块没写」时用：新建文件里没写过的区间本来就是 0，
    /// 补零就是它应有的内容。分块投递，避免为几十 MB 的空洞一次分配大缓冲。
    fn feed_zero(&self, st: &mut FileState, mut n: u64) -> E<()> {
        const CHUNK: usize = 1 << 20;
        let zero = vec![0u8; CHUNK];
        while n > 0 {
            let k = n.min(CHUNK as u64) as usize;
            self.feed_stream(st, &zero[..k])?;
            n -= k as u64;
        }
        Ok(())
    }

    /// 把 `pending` 里已经接得上的乱序写入按偏移顺序投递。
    fn drain_stream(&self, st: &mut FileState) -> E<()> {
        loop {
            let next = {
                let Some(s) = st.stream.as_ref() else { return Ok(()) };
                match s.pending.keys().next() {
                    Some(&k) if k == s.sent => k,
                    _ => return Ok(()),
                }
            };
            let data = {
                let s = st.stream.as_mut().unwrap();
                let d = s.pending.remove(&next).unwrap();
                s.pending_bytes -= d.len();
                d
            };
            self.feed_stream(st, &data)?;
        }
    }

    /// 关闭前把乱序积压收干净：接得上的按序发；剩下真正的空洞补零。
    ///
    /// 正常情况 `pending` 早就空了（那笔迟到 41ms 的写入会在下一次 drain 时补上）。
    /// 只有引擎真的跳过某块时才会走到补零 —— 新建文件里没写过的区间本来就是 0。
    fn finish_pending(&self, st: &mut FileState) -> E<()> {
        loop {
            self.drain_stream(st)?;
            let (next, sent) = {
                let Some(s) = st.stream.as_ref() else { return Ok(()) };
                (s.pending.keys().next().copied(), s.sent)
            };
            match next {
                None => return Ok(()),
                Some(k) if k > sent => {
                    dlog!("stream 收尾补零 {} 字节（{sent} -> {k}）", k - sent);
                    self.feed_zero(st, k - sent)?;
                }
                // 和已发送区间重叠：顺序流没法表达，交给调用方退回整文件回传
                Some(k) => {
                    dlog!("stream 收尾遇到重叠写入 {k}（已发送 {sent}）");
                    return Err(0xC000_0185);
                }
            }
        }
    }

    /// 掐掉流式上传并等上传线程收尾（关通道 → 读端 EOF → STOR 结束）。
    ///
    /// 远端会留下一个截断的文件，但所有调用点之后都会走「缓存整文件回传」，
    /// 那份不完整的远端文件会被完整的 STOR 覆盖掉。
    fn abort_stream(st: &mut FileState) -> Option<std::result::Result<u64, String>> {
        st.stream.take().map(|s| s.finish())
    }

    pub fn do_write(&self, h: &Handle, buffer: &[u8], offset: u64, to_eof: bool) -> E<u32> {
        if h.is_dir {
            return Err(0xC000_0102);
        }
        let remote = h.remote_path();
        let size;
        let locked;
        {
            let mut st = h.st.lock().unwrap();
            // 惰性写打开：本地缓存还不完整，动内容之前先把旧内容补齐。
            // 「替换目标中的文件」这条路径上 Overwrite 已经先把内容截成 0，
            // 那时 whole 已经是 true，这里不会白下载一次。
            self.ensure_whole(&remote, &mut st)?;
            let off = if to_eof { st.size } else { offset };

            // 一条 STOR 只能顺序往下写，但拷贝引擎的写入会**乱序到达**。
            // 连续性必须拿「已经喂给流的字节数」比，**不能**拿 st.size：拷贝引擎
            // 会先 SetEndOfFile 预分配，st.size 被提前拉到最终大小，而流的位置
            // 还停在 0 —— 用 st.size 比的话第一笔写入就被误判成回退写。
            //
            // 实测两次断流都是同一个原因：某笔 1MB 的写入比后面那笔晚了 41 毫秒
            // 才到（`off=7698644992` 晚于 `off=7699693568`）。以前把「off > sent」
            // 当跳块补零，等那笔真到了又变成「回退写」，补的零反而写坏了数据。
            // 现在不猜：`off > sent` 的先攒进 pending，等空洞补齐再按序投递。
            let mut buffered = false;
            if let Some(pos) = st.stream.as_ref().map(|s| s.sent) {
                if off < pos {
                    dlog!("stream 回退写（写入 {off}，已发送 {pos}），退回整文件回传");
                    let _ = Self::abort_stream(&mut st);
                } else if off > pos {
                    let room = st
                        .stream
                        .as_ref()
                        .map(|s| s.pending_bytes + buffer.len() <= STREAM_REORDER_MAX)
                        .unwrap_or(false);
                    if room {
                        let s = st.stream.as_mut().unwrap();
                        s.pending.insert(off, buffer.to_vec());
                        s.pending_bytes += buffer.len();
                        buffered = true;
                        dlog!("stream 乱序缓存 off={off}（等待 {pos}），积压 {} 字节", s.pending_bytes);
                    } else {
                        dlog!("stream 乱序积压超限（写入 {off}，已发送 {pos}），退回整文件回传");
                        let _ = Self::abort_stream(&mut st);
                    }
                }
            }
            // 句柄整份重写远端文件（新建 或 CREATE_ALWAYS 覆盖）时，从 0 开始的
            // 第一笔写入就开流，之后 write 直接喂给 STOR。
            // 以前卡在 `created` 上，导致「点重试 / 覆盖已有文件」退回整份缓存 +
            // 关闭时整文件回传，照样在 99% 上干等几分钟。
            if st.stream.is_none() && st.fresh && !st.removed && off == 0 {
                st.size = 0;
                self.start_stream(h, &mut st)?;
            }

            // 落本地缓存。它是「流式中途失败就整文件回传」的兜底，
            // 但只兜前面一小段（STREAM_CACHE_KEEP）：流能跑过 8MB 就说明
            // 连接是通的，再往后继续落盘只会在 C 盘铺一份完整副本。
            let cache_end = off + buffer.len() as u64;
            match st.file.as_ref() {
                Some(f) if st.stream.is_none() || cache_end <= STREAM_CACHE_KEEP => {
                    f.seek_write(buffer, off).map_err(|_| 0xC000_0185u32)?;
                }
                // 纯流式（缓存已丢弃）时本地没有可写目标，这不算错误。
                Some(_) => {}
                None if st.stream.is_some() => {}
                None => return Err(0xC000_0185),
            }
            // 越过阈值：把缓存截成 0 并摘掉句柄，之后一个字节都不再落盘。
            // 注意 set_len(0) 必须做 —— 不截断就只删文件，过滤驱动会把它
            // 整份搬进回收站，C 盘空间照样不释放。
            if st.stream.is_some() && cache_end > STREAM_CACHE_KEEP && st.cache_path.is_some() {
                if let Some(f) = st.file.as_ref() {
                    let _ = f.set_len(0);
                }
                st.file = None;
                if let Some(p) = st.cache_path.take() {
                    drop_cache_file(&p);
                }
                dlog!(
                    "{} 已写 {} 字节，本地缓存已丢弃（改为纯流式，不再占 C 盘）",
                    h.path(),
                    cache_end
                );
            }
            st.size = std::cmp::max(st.size, cache_end);
            st.dirty = true;

            // 再喂网络。通道满时这里会阻塞到网络把数据发出去为止 ——
            // 资源管理器的进度条因此反映真实上传速度，而不是「本地写完就 99%」。
            // 乱序到达的那笔先不投递，投递完直接发的那笔之后再把积压接上。
            if st.stream.is_some() {
                let mut failure = None;
                if !buffered {
                    if let Err(e) = self.feed_stream(&mut st, buffer) {
                        failure = Some(e);
                    }
                }
                if failure.is_none() {
                    if let Err(e) = self.drain_stream(&mut st) {
                        failure = Some(e);
                    }
                }
                if let Some(e) = failure {
                    // 流断了（网络抖动、用户暂停拷贝导致数据连接被服务器掐掉等）。
                    let _ = Self::abort_stream(&mut st);
                    if st.file.is_some() {
                        // 还有本地缓存兜底（前 8MB 之内）：不让写失败，
                        // 退回「关闭时整文件回传」的老路径，拷贝照常继续。
                        crate::logbuf::warn(&format!(
                            "{} 的边写边传中断（{e:#x}），改为整文件回传",
                            h.path()
                        ));
                    } else {
                        // 缓存已经丢弃（大文件纯流式），没有副本可以回传。
                        // 宁可让这次写入失败、让资源管理器明确报错，
                        // 也不能悄悄产出一个残缺的文件还显示「已完成」。
                        crate::logbuf::warn(&format!(
                            "{} 的边写边传中断（{e:#x}），本地已无缓存可回传，本次拷贝失败",
                            h.path()
                        ));
                        return Err(0xC000_0185);
                    }
                }
            }
            size = st.size;
            locked = st.time_locked;
        }
        {
            let mut m = h.meta.lock().unwrap();
            m.size = size;
            // 写过内容就更新修改时间；但「拷贝文件」时系统已显式设过源时间，
            // 这里不能覆盖，否则拷贝进来的文件时间会变成当前时间。
            if !locked {
                m.mtime = now_filetime();
            }
        }
        if !locked {
            self.set_override(&h.remote_path(), h.meta.lock().unwrap().mtime);
        }
        // pending 条目里记的大小还是创建时的 alloc。不同步的话，并发打开这个
        // 文件的句柄（begin_open 的 pending 分支）按旧大小 set_len，资源管理器
        // 看到 0 字节、cat 读到一屋子 NUL。
        self.bump_pending_size(&h.path(), size);
        dlog!("write {} off={offset} n={} total={size}", h.path(), buffer.len());
        Ok(buffer.len() as u32)
    }

    pub fn do_set_len(&self, h: &Handle, n: u64) -> E<()> {
        if h.is_dir {
            return Err(0xC000_0102);
        }
        let remote = h.remote_path();
        {
            let mut st = h.st.lock().unwrap();
            // 惰性写打开的缓存是空的，直接 set_len 会把没碰过的区间填成 0。
            // 流式上传中的句柄内容本来就是整份重写的，不需要补。
            if st.stream.is_none() {
                self.ensure_whole(&remote, &mut st)?;
            }
            // 截断没法在一条顺序流里表达，但**预分配**可以：拷贝引擎会先
            // SetEndOfFile 把大小拉到最终值，之后数据仍然从流的当前位置顺序
            // 往下写。所以只有「往回截到已发送位置之前」才必须掐流 ——
            // 拿 n 和 st.size 比会把预分配也当成截断，白白丢掉流式。
            if let Some(pos) = st.stream.as_ref().map(|s| s.sent) {
                dlog!("set_len {} n={n} 已发送={pos}", h.path());
                if n < pos {
                    dlog!("set_len 截到已发送位置之前，退回整文件回传");
                    let _ = Self::abort_stream(&mut st);
                }
            }
            if let Some(f) = st.file.as_ref() {
                // 流式上传期间本地缓存**不是**数据源，绝对不能跟着预分配：
                // 拷贝引擎一上来就 SetEndOfFile(8.5GB)，跟着扩就是 8.5GB
                // 立刻落在 C 盘上，哪怕一个字节都还没传（这是 C 盘被吃掉的
                // 另一半原因 —— 另一半是整份缓存没截断就删，进了回收站）。
                if st.stream.is_none() {
                    f.set_len(n).map_err(|_| 0xC000_0185u32)?;
                }
            }
            st.size = n;
            st.dirty = true;
        }
        h.meta.lock().unwrap().size = n;
        Ok(())
    }

    /// 上传缓存到远端。
    /// win_path 用于 pending 记账，remote 是服务器真实路径（大小写敏感）。
    fn upload(&self, win_path: &str, remote: &str, st: &mut FileState) -> E<()> {
        // created：新建但从没上传过。即使没写过内容（空文件）也要传，
        // 否则 WinFsp 在 post_cleanup_when_modified_only 下会把 cleanup 整个跳过，
        // 新建的空文件就永远留在 pending 里，关掉之后凭空消失。
        if !st.dirty && !st.created {
            return Ok(());
        }
        let f = match st.file.as_mut() {
            Some(f) => f,
            // 缓存已在流式过程中被丢弃（大文件纯流式）。这时候本地没有副本
            // 可以回传，**必须**报错：以前这里 `return Ok(())` 会让关闭流程
            // 显示成功，服务器上却只有一个残缺文件，甚至 0 字节。
            None => {
                dlog!("upload {remote} 失败：本地缓存已在流式过程中丢弃，无内容可回传");
                return Err(0xC000_0185);
            }
        };
        dlog!("upload {remote} size={}", st.size);
        f.seek(SeekFrom::Start(0)).map_err(|_| 0xC000_0185u32)?;
        self.remote.stor(remote, f).map_err(|e| {
            dlog!("upload {remote} FAILED: {e}");
            0xC000_0185u32
        })?;
        st.dirty = false;
        st.created = false;
        self.clear_pending(win_path);
        dlog!("upload {remote} ok");
        Ok(())
    }

    /// 关闭前处理：要么上传，要么删除
    pub fn do_cleanup(&self, h: &Handle, delete: bool) {
        if h.is_dir {
            // 目录没有内容要回传，只有「是否要删」这一件事。
            // 注意：不在这里处理的话，rmdir 会静默成功但远端目录依然存在。
            let rp = h.remote_path();
            let mut st = h.st.lock().unwrap();
            if (delete || st.delete_flag) && !st.removed {
                let r = self.remote.rmd(&rp);
                dlog!("cleanup rmdir {} -> {r:?}", h.path());
                st.removed = r.is_ok();
                if st.removed {
                    self.remove_override(&rp);
                }
            }
            return;
        }
        let path = h.path();
        let rp = h.remote_path();
        let mut st = h.st.lock().unwrap();
        let want_delete = st.delete_flag || delete;

        // 流式上传收尾：关通道、等上传线程结束。
        // 成功的话数据已经整份在服务器上了，**不需要**再整文件回传 ——
        // 老路径正是「关闭时整文件 STOR」把资源管理器卡在 99% 几分钟的。
        if st.stream.is_some() {
            // 收尾前先把乱序积压清干净：接得上的按序发，剩下真空洞补零。
            // 不清的话最后那几块会留在 pending 里，远端文件就是残缺的。
            if let Err(e) = self.finish_pending(&mut st) {
                dlog!("cleanup 收尾乱序积压失败 {e:#x}，退回整文件回传");
            }
            match Self::abort_stream(&mut st) {
                Some(Ok(n)) => {
                    dlog!("cleanup stream ok {path} 已传 {n} 字节");
                    if !want_delete {
                        st.dirty = false;
                        st.created = false;
                        self.clear_pending(&path);
                        // 正常成功路径里 `stor_stream` 已经 invalidate_all；
                        // 但收尾超时「视为已发送」的分支绕过了它，这里补上，
                        // 否则新文件要等到下次自然刷新才出现在列表里。
                        self.remote.invalidate_all();
                        return;
                    }
                }
                Some(Err(e)) => {
                    // 传失败了：dirty/created 原样保留，下面走整文件回传兜底。
                    dlog!("cleanup stream FAILED {path}: {e}，退回整文件回传");
                }
                None => {}
            }
        }

        if want_delete {
            // 只在本地、从没上传过的文件，远端不用删。
            // 判断依据是 created（创建句柄）或此刻还在 pending 表里
            // （上传失败/被跳过的文件）——都可能远端根本没有。
            // st.removed 已在 set_delete 里真删过，这里不再重复 DELE。
            if !st.removed && !st.created && self.is_pending(&path).is_none() {
                let r = self.remote.dele(&rp);
                dlog!("delete {path} -> {r:?}");
                st.removed = r.is_ok();
            } else {
                dlog!("delete {path} 已删或仅本地，跳过远端");
            }
            st.dirty = false;
            st.created = false;
            self.clear_pending(&path);
            self.remove_override(&rp);
            return;
        }

        if st.dirty || st.created {
            // created 但 dirty=false 也要传：新建的空文件本来就应该在服务器上存在，
            // 否则关掉之后它只在 pending 里挂着，变成删不掉的幽灵。
            dlog!("cleanup upload {path} dirty={} created={}", st.dirty, st.created);
            if let Err(e) = self.upload(&path, &rp, &mut st) {
                dlog!("cleanup upload {path} FAILED {e:#x}");
            }
        } else {
            dlog!("cleanup {path} dirty=false");
        }
    }

    pub fn do_flush(&self, h: &Handle) -> E<()> {
        if h.is_dir {
            return Ok(());
        }
        let path = h.path();
        let rp = h.remote_path();
        let mut st = h.st.lock().unwrap();
        if st.stream.is_some() {
            // 数据正边写边往服务器送，本地没有待落盘的东西。
            // 收尾统一放在 cleanup（关通道 + join），那时只剩通道里那点尾巴，
            // 几毫秒就返回 —— 不会再出现「99% 卡几分钟」。
            return Ok(());
        }
        if st.dirty {
            self.upload(&path, &rp, &mut st)?;
        }
        Ok(())
    }

    pub fn do_rename(&self, h: &Handle, new_win: &str) -> E<()> {
        let old = h.path();
        let old_rp = h.remote_path();
        let (old_parent, _) = split_parent(&norm(&old));
        let (new_parent, new_leaf) = split_parent(&norm(new_win));
        let same_dir = new_parent.eq_ignore_ascii_case(&old_parent);

        // WinFsp 给的新路径里父目录名是**大写**的：它内部按大小写不敏感比较，
        // 重建路径时用的是规范化后的名字。FTP 服务器区分大小写，直接拿它发 RNTO
        // 会 550 Rename failed —— 实测把 `\_wb_io_test\b.txt` 拼成了
        // `/Elements/ISO/_WB_IO_TEST/b.txt`，而服务器上是 `_wb_io_test`。
        // 同目录改名直接沿用 RNFR 路径的父目录（大小写天然正确、零额外开销）；
        // 跨目录移动才需要逐级解析目标目录的真实大小写。
        let target = if same_dir {
            join_remote(&remote_parent(&old_rp), &new_leaf)
        } else {
            match self.resolve(&new_parent) {
                Some((p, _)) => join_remote(&p, &new_leaf),
                None => self.remote.abs(new_win),
            }
        };

        {
            let st = h.st.lock().unwrap();
            // 远端还没有这个文件就不用发 RNFR/RNTO。
            // RNFR 必须用服务器真实路径：请求路径的大小写可能对不上。
            if !st.created {
                crate::logbuf::debug(&format!(
                    "rename: {old_rp:?} -> {target:?}（请求名 {new_win:?}）"
                ));
                self.remote
                    .rename(&old_rp, &target)
                    .map_err(|e| map_ftp_err_ctx("重命名", &old_rp, &e))?;
            }
        }
        // Windows 侧的路径也纠正回已知的父目录大小写，避免把大写名留在缓存里
        let new_path = if same_dir {
            norm(&format!("{old_parent}\\{new_leaf}"))
        } else {
            norm(new_win)
        };
        self.rename_pending(&old, &new_path);
        let new_rp = target.clone();
        // 覆盖表跟着改名走，否则改名后时间戳会变回服务器时间
        if let Some(t) = self.override_for(&old_rp) {
            self.remove_override(&old_rp);
            self.set_override(&new_rp, t);
        }
        *h.path.lock().unwrap() = new_path.clone();
        *h.remote_path.lock().unwrap() = new_rp;
        let (_, name) = split_parent(&norm(&new_path));
        h.meta.lock().unwrap().name = name;
        dlog!("rename {old} -> {new_path}");
        Ok(())
    }

    /// 标记删除。目录在这里就真正 RMD —— cleanup 拿不到返回值，
    /// 放在 cleanup 里会导致「删不掉也报成功」。
    pub fn do_set_delete(&self, h: &Handle, delete: bool) -> E<()> {
        dlog!("set_delete {} dir={} delete={delete}", h.path(), h.is_dir);
        let mut st = h.st.lock().unwrap();
        if !delete {
            st.delete_flag = false;
            return Ok(());
        }
        st.delete_flag = true;
        if h.is_dir && !st.removed {
            let rp = h.remote_path();
            match self.remote.rmd(&rp) {
                Ok(()) => {
                    dlog!("rmdir {} ok", h.path());
                    st.removed = true;
                    self.remove_override(&rp);
                }
                Err(e) => {
                    let msg = e.to_string();
                    dlog!("rmdir {} FAILED: {msg}", h.path());
                    crate::logbuf::warn(&format!("删除目录 {} 失败: {msg}", h.path()));
                    // 550 基本都是「目录非空」或「无权限」，翻译成对应 NTSTATUS，
                    // 资源管理器才会提示"文件夹不为空/拒绝访问"而不是笼统 I/O 错误。
                    return Err(if msg.contains("550") {
                        0xC000_0101 // STATUS_DIRECTORY_NOT_EMPTY
                    } else {
                        0xC000_0185 // STATUS_IO_DEVICE_ERROR
                    });
                }
            }
        } else if !h.is_dir && !st.removed {
            // 文件也在这里就真删。以前只在 cleanup 里删且**吞掉失败**，于是
            // 服务器拒绝删除时资源管理器仍显示"删除成功"，刷新后文件又回来
            // （用户实测「可以删除」其实是假象）。这里把失败翻译成 NTSTATUS
            // 返回给调用方，拒绝访问就会当场报错。
            let path = h.path();
            if !st.created && self.is_pending(&path).is_none() {
                let rp = h.remote_path();
                match self.remote.dele(&rp) {
                    Ok(()) => {
                        st.removed = true;
                        dlog!("dele {path} ok");
                        self.remove_override(&rp);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        dlog!("dele {path} FAILED: {msg}");
                        crate::logbuf::warn(&format!("删除文件 {path} 失败: {msg}"));
                        return Err(if msg.contains("550") {
                            0xC000_0022 // STATUS_ACCESS_DENIED
                        } else {
                            0xC000_0185
                        });
                    }
                }
            } else {
                // 仅本地、从没上传过：远端没有，视作已删
                st.removed = true;
                self.remove_override(&h.remote_path());
            }
            // 关键：pending 条目必须在这里清掉。只靠 cleanup 清的话，WinFsp
            // 在某些路径下不会调 cleanup，文件就会「删了刷新又出现」。
            self.clear_pending(&path);
        }
        Ok(())
    }

    /// 释放：删掉本地缓存文件
    pub fn do_close(&self, h: Handle) {
        let path = h.path();
        let rp = h.remote_path();
        let mut st = h.st.into_inner().unwrap();
        // cleanup 被 WinFsp 跳过时流还开着，这里补收尾，否则数据只传了一半。
        if st.stream.is_some() {
            if let Err(e) = self.finish_pending(&mut st) {
                dlog!("close {path} 收尾乱序积压失败 {e:#x}");
            }
            match Self::abort_stream(&mut st) {
                Some(Ok(n)) => {
                    dlog!("close {path} 流式上传收尾 {n} 字节");
                    st.dirty = false;
                    st.created = false;
                    self.clear_pending(&path);
                }
                Some(Err(e)) => dlog!("close {path} 流式上传失败: {e}，下面补传"),
                None => {}
            }
        }
        // 走到这儿 created 还是 true，说明从没成功上传过：
        // 可能是 cleanup 被 WinFsp 跳过了，也可能是上次 STOR 失败。
        // 补传一次；失败也不删 pending —— 这个文件在资源管理器里还得看得见、
        // 还得能删（begin_open 已经能打开 pending 文件了），不是直接消失。
        if st.created && !st.removed {
            dlog!("close {path} 尚未上传，补传");
            if let Err(e) = self.upload(&path, &rp, &mut st) {
                dlog!("close {path} 补传失败 {e:#x}，保留 pending");
            }
        }
        // 已删除的句柄关闭时，确保 pending 也清掉，避免列表里残留幽灵项
        if st.removed {
            self.clear_pending(&path);
        }
        // 先放掉句柄再删：句柄没关时 Windows 会拒绝删除，缓存就永久留在盘上。
        st.file = None;
        if let Some(p) = st.cache_path.take() {
            // 走 drop_cache_file 而不是 remove_file：这台机器 C 盘的过滤驱动
            // 会把删除转进回收站，不截断就等于把整份缓存搬去回收站白占空间。
            drop_cache_file(&p);
        }
    }

    /// 列目录（含 pending 的本地文件）
    pub fn list_dir(&self, win_path: &str) -> E<Vec<Entry>> {
        let remote = self.remote.abs(win_path);
        let mut entries = self
            .remote
            .list(&remote)
            .map_err(|e| map_ftp_err_ctx("列目录", &remote, &e))?;
        let dir_lower = norm(win_path).to_lowercase();
        for (k, v) in self.pending.lock().unwrap().iter() {
            let (parent, _) = split_parent(k);
            if parent == dir_lower && !entries.iter().any(|e| e.name.eq_ignore_ascii_case(&v.name)) {
                entries.push(v.clone());
            }
        }
        // 应用本地时间戳覆盖
        {
            let over = self.mtime_over.lock().unwrap();
            for e in entries.iter_mut() {
                if let Some(&t) = over.get(&join_remote(&remote, &e.name)) {
                    e.mtime = t;
                }
            }
        }
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// WinFsp 适配层：只做参数转换，逻辑全在上面
// ---------------------------------------------------------------------------

/// 默认安全描述符：**Everyone 全权**（`O:BAG:WDD:(A;;FA;;;WD)`），自相对格式，76 字节。
///
/// 这份字节不是手搓的，是 `ConvertStringSecurityDescriptorToSecurityDescriptorW`
/// 用上面的 SDDL 生成后原样拷过来的 —— 偏移、ACL 长度、ACE 大小都由 Windows 算好，
/// 避免手工拼装时把某个偏移写错、结果被 WinFsp 判成「不合法描述符」而一律拒绝。
///
/// 这里踩过三个坑，都实测过：
///
/// 1. `sz_security_descriptor: 0`（干脆不给描述符）：目录枚举直接失败。
/// 2. NULL DACL（Dacl 偏移 0、不置 SE_DACL_PRESENT，52 字节）：Windows 语义里
///    这是「不做任何访问检查」，但 WinFsp 的检查路径并不按这个语义走 ——
///    2026-10-21 实测「以写方式打开已存在文件」仍然回 ACCESS_DENIED，
///    而且是在进我们的 `open` 回调**之前**就拒了（日志里连 open 行都没有）。
/// 3. 手工拼的 `Everyone` 完全访问 ACE（80 字节）：同样被拒。
///
/// 所以判断依据是「描述符必须由系统 API 生成且结构完全合法」，
/// 而不是「DACL 里写了什么」。改这份字节前，请先用 `_work/sd_gen.py`
/// 生成一份对照，别手写。
const SD_OPEN: [u8; 76] = [
    // --- SECURITY_DESCRIPTOR_RELATIVE ---
    0x01, 0x00, // Revision=1, Sbz1=0
    0x04, 0x80, // Control = SE_SELF_RELATIVE | SE_DACL_PRESENT (0x8004)
    0x30, 0x00, 0x00, 0x00, // Owner 偏移 = 48
    0x40, 0x00, 0x00, 0x00, // Group 偏移 = 64
    0x00, 0x00, 0x00, 0x00, // Sacl 偏移 = 0
    0x14, 0x00, 0x00, 0x00, // Dacl 偏移 = 20
    // --- DACL: 1 个 ACE，Everyone 全权 ---
    0x02, 0x00, // AclRevision = 2
    0x1C, 0x00, // AclSize = 28
    0x01, 0x00, // AceCount = 1
    0x00, 0x00, // Sbz2
    // ACCESS_ALLOWED_ACE: Everyone = FILE_ALL_ACCESS (0x001F01FF)
    0x00, 0x00, 0x14, 0x00, // AceType=0, AceFlags=0, AceSize=20
    0xFF, 0x01, 0x1F, 0x00, // AccessMask
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // S-1-1-0
    // --- Owner: BUILTIN\Administrators (S-1-5-32-544) ---
    0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00,
    // --- Group: Everyone (S-1-1-0) ---
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
];
/// 52 字节 NULL DACL 版本（Dacl 偏移 0、不置 SE_DACL_PRESENT）。
/// 只作为 `FTPMOUNT_SD=null` 的对照实验保留，正常不用。
const SD_NULL: [u8; 52] = [
    0x01, 0x00, 0x04, 0x80, 0x30, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x02, 0x00, 0x00,
];

/// 48 字节：DACL 里一条 **拒绝 Everyone 全部权限** 的 ACE。
///
/// 纯粹是诊断用（`FTPMOUNT_SD=deny`）：如果返回它以后连只读打开都还能成功，
/// 就说明 WinFsp 根本没用我们返回的描述符做访问检查，写打开被拒另有原因。
const SD_DENY: [u8; 48] = [
    // --- SECURITY_DESCRIPTOR_RELATIVE ---
    0x01, 0x00, // Revision=1, Sbz1=0
    0x04, 0x80, // Control = SE_SELF_RELATIVE | SE_DACL_PRESENT
    0x00, 0x00, 0x00, 0x00, // Owner 偏移 = 0
    0x00, 0x00, 0x00, 0x00, // Group 偏移 = 0
    0x00, 0x00, 0x00, 0x00, // Sacl 偏移 = 0
    0x14, 0x00, 0x00, 0x00, // Dacl 偏移 = 20
    // --- DACL ---
    0x02, 0x00, // AclRevision = 2
    0x1C, 0x00, // AclSize = 28
    0x01, 0x00, // AceCount = 1
    0x00, 0x00, // Sbz2
    // ACCESS_DENIED_ACE: Everyone 拒绝 FILE_ALL_ACCESS
    0x01, 0x00, 0x14, 0x00, // AceType=1(deny), AceFlags=0, AceSize=20
    0xFF, 0x01, 0x1F, 0x00, // AccessMask
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, // S-1-1-0
];

/// 描述符方案，由环境变量 `FTPMOUNT_SD` 在首次使用时定下（进程生命周期内不变）。
///
/// | 值 | 行为 |
/// |---|---|
/// | 未设置 / `76` / `open` | `SD_OPEN`，76 字节 Everyone 全权（默认） |
/// | `none` / `0` | 不给描述符，`sz_security_descriptor = 0` |
/// | `null` | `SD_NULL`，52 字节 NULL DACL（对照） |
/// | `deny` | `SD_DENY`，拒绝 Everyone（诊断：看描述符到底生效不生效） |
///
/// 之所以做成运行时开关：换一次描述符就得重装一次挂载程序，
/// 而这些方案只能靠「装一次、试一遍」来筛，做成开关后一次安装就能对比。
fn sd_mode() -> u8 {
    use std::sync::OnceLock;
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| {
        let v = std::env::var("FTPMOUNT_SD").unwrap_or_default();
        let m = match v.trim().to_ascii_lowercase().as_str() {
            "none" | "0" | "off" => 1u8,
            "null" => 2u8,
            "deny" => 3u8,
            _ => 0u8,
        };
        crate::logbuf::info(&format!(
            "安全描述符方案 = {}（FTPMOUNT_SD={:?}）",
            match m {
                1 => "none（不给描述符）",
                2 => "null（52 字节 NULL DACL）",
                3 => "deny（拒绝 Everyone，诊断用）",
                _ => "open（76 字节 Everyone 全权）",
            },
            v
        ));
        m
    })
}

/// 把默认安全描述符写进 WinFsp 给的缓冲区，返回长度。
///
/// WinFsp 会先以 `security_descriptor = None` 调一次问长度，再按这个长度分配缓冲区
/// 调第二次，所以**无论有没有缓冲区都要返回真实长度**，否则它永远拿不到描述符。
fn put_default_sd(dst: Option<&mut [std::ffi::c_void]>) -> u64 {
    let sd: &[u8] = match sd_mode() {
        1 => return 0,
        2 => &SD_NULL,
        3 => &SD_DENY,
        _ => &SD_OPEN,
    };
    if let Some(buf) = dst {
        if buf.len() >= sd.len() {
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len()) };
            bytes[..sd.len()].copy_from_slice(sd);
        }
    }
    sd.len() as u64
}

impl FileSystemContext for FtpFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [std::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        let path = file_name.to_string_lossy();
        let _g = Inflight::enter();
        match self.stat(&path) {
            Some(e) => {
                let attrs = if e.is_dir { ATTR_DIR } else { ATTR_ARCHIVE };
                let blen = security_descriptor.as_ref().map(|b| b.len()).unwrap_or(0);
                let sz = put_default_sd(security_descriptor);
                dlog!("get_security_by_name {path} -> 有, attrs=0x{attrs:x}, sd={sz}, buf={blen}");
                Ok(FileSecurity {
                    reparse: false,
                    sz_security_descriptor: sz,
                    attributes: attrs,
                })
            }
            None => {
                dlog!("get_security_by_name {path} -> 没有");
                Err(nt(0xC000_0034))
            }
        }
    }

    /// 已打开句柄的安全描述符。资源管理器判断"目标能不能写"时会走这条，
    /// 不实现的话 WinFsp 回 STATUS_INVALID_DEVICE_REQUEST，shell 的复制/覆盖流程会退化。
    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [std::ffi::c_void]>,
    ) -> Result<u64> {
        Ok(put_default_sd(security_descriptor))
    }

    /// 设置安全描述符。
    ///
    /// 我们所有对象都固定是「Everyone 全权」，不做持久化；但必须回成功。
    /// 默认实现回 `STATUS_INVALID_DEVICE_REQUEST`，而资源管理器往
    /// **网络类型**卷拷文件时会尝试把源的 ACL 同步过来，拿到「设备请求无效」
    /// 有可能触发 shell 的重试退避（表现为拷贝停在 99% 几秒）。
    fn set_security(
        &self,
        _context: &Self::FileContext,
        _security_information: u32,
        _modification_descriptor: ModificationDescriptor,
    ) -> Result<()> {
        Ok(())
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = file_name.to_string_lossy().to_string();
        // 没有读写数据权限的打开只是查属性（Explorer 对每个文件都会来一次），
        // 不用真去下载内容。
        let need_data =
            granted_access & (FILE_READ_DATA | FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
        dlog!(
            "open {path} create_options=0x{create_options:08x} granted=0x{granted_access:08x} need_data={need_data}"
        );
        let h = match timed(&format!("open {path}"), || {
            self.begin_open(&path, granted_access)
        }) {
            Ok(h) => h,
            Err(e) => {
                dlog!("open {path} FAILED {e:#x}");
                return Err(nt(e));
            }
        };
        if create_options & FILE_DIRECTORY_FILE != 0 && !h.is_dir {
            self.do_close(h);
            return Err(nt(0xC000_0103)); // STATUS_NOT_A_DIRECTORY
        }
        // 关键：WinFsp 内核层用 Open 返回的 FileInfo 判断句柄是不是目录，
        // 不填的话目录枚举会被直接拒绝（STATUS_INVALID_PARAMETER），
        // ReadDirectory 根本不会被调用。
        let key = h.remote_path();
        let m = h.meta.lock().unwrap();
        fill_info(file_info.as_mut(), &m, &key);
        drop(m);
        Ok(h)
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = file_name.to_string_lossy().to_string();
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        // WinFsp 的 Create 回调**拿不到** CreateDisposition —— 那个字段由内核消费，
        // 内核按 disposition 自己决定调哪个回调：
        //   FILE_CREATE       → 只调 Create
        //   FILE_OPEN         → 只调 Open
        //   FILE_OPEN_IF      → 先 Open，不存在才调 Create
        //   FILE_OVERWRITE_IF → 先 Open + Overwrite，不存在才调 Create
        // 所以约定是：Create 只要看到同名对象已存在，就必须返回
        // STATUS_OBJECT_NAME_COLLISION（0xC0000035）。
        //
        // 资源管理器正是靠这个错误码弹「替换或跳过文件」的：它先拿 FILE_CREATE
        // 试建一次，收到 0xC0000035 才弹框，用户点「替换」之后才改用
        // FILE_OVERWRITE_IF。我们以前把「已存在」当成「直接覆盖」，永远不返回
        // 这个码，所以同名覆盖从来不弹框（2026-10-21 用 memfs 全量回调日志
        // 逐条对比确认：memfs 的 Create 对已存在对象一律返回 COLLISION）。
        if self.resolve(&path).is_some() {
            dlog!("create {path} 已存在 -> 0xC0000035（交回内核决定是否弹替换提示）");
            return Err(nt(0xC000_0035));
        }
        let h = match timed(&format!("create {path}"), || {
            self.begin_create(&path, is_dir, allocation_size)
        }) {
            Ok(h) => h,
            Err(e) => {
                dlog!("create {path} FAILED {e:#x}");
                return Err(nt(e));
            }
        };
        let key = h.remote_path();
        let m = h.meta.lock().unwrap();
        fill_info(file_info.as_mut(), &m, &key);
        drop(m);
        // 新对象已落地，告诉系统「这里多了个东西」。资源管理器靠这条通知
        // 把拷贝进度从 99% 推到 100%，没有它就只能等约 6.2 秒的兜底超时。
        if is_dir {
            self.push_notify(&path, FILE_ACTION_ADDED, NC_DIR_NAME);
        } else {
            self.push_notify(
                &path,
                FILE_ACTION_ADDED,
                NC_FILE_NAME | NC_SIZE | NC_LAST_WRITE,
            );
        }
        Ok(h)
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // 覆盖：把本地缓存截断成 0，丢弃旧内容，等应用写入新内容后整体回传。
        //
        // 绝不能按 allocation_size 去 set_len —— 它是**簇对齐的预分配大小**，
        // 通常远大于真实长度，会把文件尾部撑成一堆 0（旧内容还留在前面），
        // 应用再写新内容就变成「新内容 + 旧尾巴 + 零」，正是用户报的
        // 「编辑后文件都变乱码」。这里只做截断。
        {
            let mut st = context.st.lock().unwrap();
            // 覆盖语义和「顺序流」冲突：先把流掐掉，退回整文件回传。
            if st.stream.is_some() {
                let _ = Self::abort_stream(&mut st);
            }
            if st.file.is_none() {
                let cache = self.next_cache_path();
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&cache)
                    .map_err(|_| nt(0xC000_0185))?;
                st.file = Some(f);
                st.cache_path = Some(cache);
            }
            if let Some(f) = st.file.as_ref() {
                f.set_len(0).map_err(|_| nt(0xC000_0185))?;
            }
            st.size = 0;
            st.dirty = true;
            st.whole = true;
            st.ranges.clear();
            // 覆盖 = 整份重写：内容从 0 开始全是新的，可以开流式上传。
            // 不设这个的话，「替换目标中的文件」会退回「整份写本地缓存 + 关闭时
            // 整文件回传」，覆盖大文件又变成 99% 干等几分钟。
            st.fresh = true;
        }
        let key = context.remote_path();
        let mut m = context.meta.lock().unwrap();
        m.size = 0;
        fill_info(file_info, &m, &key);
        Ok(())
    }

    fn close(&self, context: Self::FileContext) {
        let t0 = Instant::now();
        let p = context.path();
        self.do_close(context);
        let ms = t0.elapsed().as_millis();
        // 每条都记：句柄关不掉是「拷贝卡在 99%」的经典原因，
        // 只记 >=100ms 的话会漏掉「关得慢但不到 100ms」的连锁。
        dlog!("close {p} {ms}ms");
        if ms >= 100 {
            dlog!("[慢] close {p} 用了 {ms}ms");
        }
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let t0 = Instant::now();
        let p = context.path();
        let deleting = flags & FSP_CLEANUP_DELETE != 0;
        let is_dir = context.is_dir;
        self.do_cleanup(context, deleting);
        let ms = t0.elapsed().as_millis();
        if ms >= 100 {
            dlog!("[慢] cleanup {p} 用了 {ms}ms");
        }
        if deleting {
            let f = if is_dir { NC_DIR_NAME } else { NC_FILE_NAME };
            self.push_notify(&p, FILE_ACTION_REMOVED, f);
        }
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        let context = match context {
            Some(c) => c,
            None => return Ok(()),
        };
        timed(&format!("flush {}", context.path()), || self.do_flush(context)).map_err(nt)?;
        let key = context.remote_path();
        let m = context.meta.lock().unwrap();
        fill_info(file_info, &m, &key);
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        let _g = Inflight::enter();
        let key = context.remote_path();
        let meta = context.meta.lock().unwrap();
        fill_info(file_info, &meta, &key);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        if !context.is_dir {
            return Err(nt(0xC000_0102));
        }
        let _g = Inflight::enter();
        // marker 为空才算一次新枚举：把整个目录填进 DirBuffer；
        // 之后 read() 自己带游标往后走。
        //
        // WinFsp dirbuf.c 已核实的行为（2026-10-18 读源码确认）：
        // 1. marker 非空但在索引里找不到时，FspFileSystemSearchDirectoryBuffer
        //    会把 *PIndexNum 设成插入点前一位，ReadDirectoryBuffer 再 IndexNum++，
        //    于是从「第一个比 marker 大的条目」接着走 —— 不会从头重来。
        //    所以早先「疑似 WinFsp 传 . / .. 导致从头循环」的猜想是错的。
        // 2. acquire(true, _) 的 reset 只把 LoMark/HiMark 归位，条目数清零，
        //    CapacityHint 仅在 DirBuffer 首次创建时生效；之后填不下会自动扩容，
        //    因此容量提示基本无所谓，write 只可能在 OOM 时失败。
        // 3. marker 是最后一条时循环不执行，只补一个 NULL 结束条目，枚举正常收敛。
        // 注：曾改成「每次按 marker 重填」的无状态写法，结果挂死；已回退。
        let marker_name = marker.inner_as_cstr().map(|m| m.to_string_lossy().to_string());
        let marker_raw = marker.inner().map(|s| s.to_vec());
        if marker.is_none() {
            let path = context.path();
            let entries = self.list_dir(&path).map_err(nt)?;
            dlog!("read_directory {path} 新枚举 {} 项", entries.len());
            // DirInfo<255> 每条约 600 字节，给足空间免得填不下
            let bytes = (entries.len() + 2) * 1024;
            let lock = context.dir.acquire(true, Some(bytes as u32))?;
            for e in entries.iter() {
                let mut info: DirInfo<255> = DirInfo::new();
                fill_info(info.file_info_mut(), e, &join_remote(&path, &e.name));
                let name_w = U16CString::from_str(&e.name).map_err(|_| nt(0xC000_0185))?;
                info.set_name_cstr(&name_w)?;
                // winfsp crate 的 set_name/set_name_cstr 都会把结尾 NUL 一并
                // 算进 DirInfo.Size。但 WinFsp 的 marker 二分查找（src/dll/
                // dirbuf.c）用 (Size - sizeof(FSP_FSCTL_DIR_INFO))/2 当名字
                // 长度：含了 NUL，每个条目就都比 marker「长 1 个字符」，前缀
                // 相等后比较 alen - blen = +1，查找必然落空并回退一条，把
                // marker 指向的条目本身又返回给消费者。消费者反复处理同一个
                // 名字，枚举永远推进不下去 —— `ls -la` 卡死、挂载进程 CPU
                // 30%~80%、同一个条目重复输出上百次，根因都是这一个。
                // 实测 Size=118 对应 6 字符名（头部 104 + 7*2，第 7 个就是 NUL）。
                // Size 在 DirInfo 偏移 0（repr(C)，与 FSP_FSCTL_DIR_INFO 同一
                // 套 ABI），直接把 NUL 的 2 字节从 Size 减掉即可。
                // （set_name_raw 在 crate 的 sealed 模块里，外部用不了。）
                unsafe {
                    let raw = (&mut info as *mut DirInfo<255>) as *mut u16;
                    let sz = raw.read_unaligned();
                    if sz >= 2 {
                        raw.write_unaligned(sz - 2);
                    }
                }
                lock.write(&mut info)?;
            }
        }
        let n = context.dir.read(marker, buffer);
        dlog!(
            "read_directory {} marker={:?} raw={:?} -> 返回 {} 字节",
            context.path(),
            marker_name,
            marker_raw,
            n
        );
        if std::env::var_os("FT_DIRDUMP").is_some() && n > 2 {
            let mut pos = 0usize;
            let mut i = 0;
            while pos + 2 <= n as usize && i < 3 {
                let sz = u16::from_le_bytes([buffer[pos], buffer[pos + 1]]) as usize;
                if sz == 0 {
                    dlog!("entry[{i}] 结束符");
                    break;
                }
                let name_at = |off: usize| -> String {
                    let mut v = Vec::new();
                    let mut p = pos + off;
                    while p + 2 <= n as usize {
                        let c = u16::from_le_bytes([buffer[p], buffer[p + 1]]);
                        if c == 0 {
                            break;
                        }
                        v.push(c);
                        p += 2;
                        if v.len() > 40 {
                            break;
                        }
                    }
                    String::from_utf16_lossy(&v)
                };
                dlog!(
                    "entry[{i}] pos={pos} size={sz} name@106={:?} name@112={:?}",
                    name_at(106),
                    name_at(112)
                );
                pos += (sz + 7) & !7;
                i += 1;
            }
        }
        Ok(n)
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        self.do_read(context, buffer, offset).map_err(nt)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        let n = timed(
            &format!("write {} off={offset} n={}", context.path(), buffer.len()),
            || self.do_write(context, buffer, offset, write_to_eof),
        )
        .map_err(nt)?;
        let key = context.remote_path();
        let m = context.meta.lock().unwrap();
        fill_info(file_info, &m, &key);
        Ok(n)
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // set_allocation_size=true 只是「预分配」，不改变文件逻辑长度。
        // 若照它 set_len，会把文件尾部撑大/截短，内容跟着坏掉。
        if !set_allocation_size {
            timed(&format!("set_len {} n={new_size}", context.path()), || {
                self.do_set_len(context, new_size)
            })
            .map_err(nt)?;
        }
        let key = context.remote_path();
        let m = context.meta.lock().unwrap();
        fill_info(file_info, &m, &key);
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // FTP 服务器大多不支持 MFMT（改远端时间戳），但本地句柄必须记住这个
        // 时间——否则「拷贝文件到 R:」之后资源管理器显示 1601/空白。
        // 优先用修改时间，没有再退回创建时间。
        dlog!(
            "set_basic_info {} ctime={creation_time} mtime={last_write_time}",
            context.path()
        );
        let mut meta = context.meta.lock().unwrap();
        let t = if last_write_time != 0 {
            last_write_time
        } else {
            creation_time
        };
        if t != 0 {
            meta.mtime = t;
        }
        let key = context.remote_path();
        fill_info(file_info, &meta, &key);
        drop(meta);
        if t != 0 {
            // 标记本句柄已显式设过时间，后续写操作不再覆盖；并持久化覆盖表，
            // 这样列表缓存刷新后仍显示这个时间（服务器不支持 MFMT）。
            context.st.lock().unwrap().time_locked = true;
            let t0 = Instant::now();
            self.set_override(&context.remote_path(), t);
            let ms = t0.elapsed().as_millis();
            if ms >= 100 {
                dlog!("[慢] set_basic_info 存覆盖表 用了 {ms}ms");
            }
        }
        self.push_notify(
            &context.path(),
            FILE_ACTION_MODIFIED,
            NC_ATTRIBUTES | NC_LAST_WRITE,
        );
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        self.do_set_delete(context, delete_file).map_err(nt)
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> Result<()> {
        let old_win = context.path();
        let is_dir = context.is_dir;
        let new_win = new_file_name.to_string_lossy().to_string();
        self.do_rename(context, &new_win).map_err(nt)?;
        let f = if is_dir { NC_DIR_NAME } else { NC_FILE_NAME };
        self.push_notify(&old_win, FILE_ACTION_RENAMED_OLD_NAME, f);
        self.push_notify(&new_win, FILE_ACTION_RENAMED_NEW_NAME, f);
        Ok(())
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> Result<()> {
        out.total_size = 1 << 40; // 1 TB 占位
        out.free_size = 1 << 39;
        out.set_volume_label(&self.remote.cfg.volname);
        Ok(())
    }
}

/// 目录变更通知。由 `FileSystemHost::new_with_timer` 起的线程池定时器驱动：
/// 每 400ms 问一次「有没有待发的事件」，有就通过 `FspFileSystemNotify` 推给系统。
///
/// 为什么必须有：资源管理器（以及一切用 `ReadDirectoryChangesW` 的程序）拷完文件
/// 后，是靠这条通知知道「新文件到了」才收尾的。之前一直没实现，所以每次拷贝
/// 都要白等约 6.2 秒的兜底超时——表现就是进度条顶在 99%。
/// RaiDrive 用 EldoS CBFS，CBFS 天生带 Notify，所以它没有这个现象。
impl NotifyingFileSystemContext<Vec<NotifyEvent>> for FtpFs {
    fn should_notify(&self) -> Option<Vec<NotifyEvent>> {
        let mut q = self.notify_q.lock().unwrap();
        if q.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *q))
        }
    }

    fn notify(&self, events: Vec<NotifyEvent>, notifier: &Notifier) {
        let n = events.len();
        for e in events {
            // 名字缓冲按字符算，1024 个字符足够放长路径。
            let mut info = NotifyInfo::<1024>::new();
            info.filter = e.filter;
            info.action = e.action;
            if info.set_name(e.path.as_str()).is_ok() {
                notifier.notify(&info);
            }
        }
        // 有这条就说明 FspFileSystemNotifyBegin 成功、通知真的推给系统了。
        dlog!("[通知] 推送 {n} 条目录变更");
    }
}

/// 构造一个 NTSTATUS 错误。
/// 本地缓存目录（惰性下载的临时副本 + 时间戳覆盖表）
pub fn cache_root() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("ftpmount-rs")
        .join("cache")
}

/// 删除缓存文件：**先截成 0 字节，再删**。
///
/// 这台机器的 C 盘上有第三方过滤驱动，把普通 `DeleteFileW`（也就是
/// `std::fs::remove_file`）转成了「移到回收站」——同样是删除，在 D 盘是
/// 永久删，在 C 盘就变成往回收站塞一份完整副本，空间一点不释放。
/// 实测两次大文件拷贝删完缓存后，回收站里躺着 8.49GB + 5.16GB 两个 `$R*.tmp`，
/// 用户 C 盘凭空少 13.6GB，差点以为要崩盘。
///
/// 先 `set_len(0)` 再删，就算还是被转进回收站，那份副本也是 0 字节，
/// 不会再白占空间。这个顺序在正常盘上同样成立，没有副作用。
pub fn drop_cache_file(p: &std::path::Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(p) {
        let _ = f.set_len(0);
    }
    let _ = std::fs::remove_file(p);
}

/// 清掉上次运行残留的 `*.tmp` 缓存文件。
///
/// 正常关闭时 `do_close` 会删掉自己的缓存文件；但进程被强杀 / 崩溃 / 断电时
/// 来不及删，一个 8GB 的临时文件就永久占着磁盘（实测攒到 58GB）。
/// 每次挂载开始时清一次，代价只有一次目录遍历。
pub fn purge_cache(dir: &std::path::Path) -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_tmp = p
                .extension()
                .map(|x| x.eq_ignore_ascii_case("tmp"))
                .unwrap_or(false);
            if !is_tmp {
                continue;
            }
            if let Ok(m) = e.metadata() {
                bytes += m.len();
            }
            // 必须走 drop_cache_file：直接 remove_file 在这台机器上会
            // 被过滤驱动转进回收站，空间不释放（见 drop_cache_file 注释）。
            drop_cache_file(&p);
            if !p.exists() {
                n += 1;
            }
        }
    }
    (n, bytes)
}

/// FTP 错误 → NTSTATUS。
///
/// 关键是不能把所有失败都糊成一个码：服务器不可达和「没权限」在资源管理器里
/// 必须能区分，否则用户看到「该项目已存在」却根本找不到那个项目。
pub fn map_ftp_err(e: &suppaftp::FtpError) -> u32 {
    use suppaftp::FtpError;
    match e {
        // 550 / 500 之类：服务器明确拒绝 → 拒绝访问
        FtpError::UnexpectedResponse(_) => 0xC000_0022,
        // 连接坏了 / 响应串包 → 设备未连接（提示服务器不可达，别误导成权限问题）
        FtpError::ConnectionError(_) | FtpError::BadResponse => 0xC000_00B5,
        _ => 0xC000_0022,
    }
}

/// 带上下文的错误映射：**同时打一条 WARN 日志**。
///
/// 以前所有调用点都写 `map_err(|_| 0xC000_0185)`，FTP 层的错误被彻底吞掉，
/// 用户只看到资源管理器的「由于 I/O 设备错误，无法运行此项请求」，
/// 日志里一个字都没有，完全没法定位。现在每一次失败都留痕。
pub fn map_ftp_err_ctx(op: &str, path: &str, e: &suppaftp::FtpError) -> u32 {
    let code = map_ftp_err(e);
    crate::logbuf::warn(&format!("{op} {path} 失败（0x{code:08X}）: {e}"));
    code
}

pub fn nt(code: u32) -> winfsp::FspError {
    winfsp::FspError::NTSTATUS(code as i32)
}
