use crate::config::Config;
use chrono::Datelike;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use suppaftp::FtpStream;

pub const FILETIME_EPOCH_DIFF: i64 = 11_644_473_600;

/// 目录列表缓存时间。资源管理器每看到一个文件名都会 stat 一次，
/// 每次都真去 LIST 会把服务器和单条连接打死；1 秒足够覆盖一次枚举。
const LIST_CACHE_TTL: Duration = Duration::from_millis(10000);
/// 小文件内容缓存时间。Explorer/查毒/预览/preview 会在极短时间内反复
/// 打开同一个文件，每次打开都整文件 RETR；2 秒窗口足够覆盖一次完整
/// 操作链，又不至于让「服务器上被别人改过」的旧内容活太久。
const READ_CACHE_TTL: Duration = Duration::from_millis(2000);
/// 只有 ≤1MiB 的文件进内存缓存，再大永远流式落盘，内存峰值可控。
const READ_CACHE_MAX_FILE: usize = 1 << 20;
/// 缓存总量上限。超了直接整体清空——缓存而已，正确性由 TTL 和写失效兜底。
const READ_CACHE_MAX_TOTAL: usize = 64 << 20;
/// FTP 超时。没有超时的后果实测过：ftp-srv 崩过一次（Unhandled rejection），
/// 我们的回调从此永远收不到响应；而所有 FTP 操作共用一条带 mutex 的连接，
/// 于是整个盘僵死、挂载进程空转烧 CPU（用户报「机器基本卡死」的同类）。
/// 控制连接与数据连接（PASV）都要有上限。
const FTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const FTP_IO_TIMEOUT: Duration = Duration::from_secs(6);
/// 单个 FTP 操作的硬截止时间。套接字读写超时只在「无数据」时触发，
/// 对「连上了但服务器不响应 / 数据连接没继承超时」这种慢死连接拦不住，
/// 于是 LIST 能阻塞上百秒、把资源管理器的同步 IRP 卡死、整台机器假死。
/// 这里用独立线程跑操作 + 通道带截止兜底：超时就丢连接、立刻报错。
const FTP_OP_TIMEOUT: Duration = Duration::from_secs(8);
/// 服务器刚连接失败后的熔断冷却时间。这期间的新操作直接快速失败，不再每次
/// 去 connect 等 4 秒——否则服务器掉线时，资源管理器每 stat/删一个文件都要
/// 卡好几秒，表现为「未响应」。冷却结束后自动重试，恢复后无需重启挂载。
const FAIL_COOLDOWN: Duration = Duration::from_secs(3);
/// 空闲连接复用前先用 NOOP 探活的门槛。服务器 idle timeout 一般 1~5 分钟，
/// 这里取 10 秒：连续操作（资源管理器枚举、拷贝）大多在这个窗口内复用，
/// 不会多花往返；隔了一会儿再用时先探一下，避免在死 socket 上做无用功。
const IDLE_PROBE: Duration = Duration::from_secs(10);
/// 数据传输（RETR/STOR 一个区间）的截止时间。元数据 8 秒够了，但传几 MB
/// 在慢线路上要十几秒，套 8 秒会被误判超时 → 上层报「I/O 设备错误」。
const FTP_XFER_TIMEOUT: Duration = Duration::from_secs(120);
/// 上传（STOR 整个文件）的截止时间。2GB 在这台服务器上要传近 10 分钟，
/// 用读的超时会把「还在正常传」的 STOR 判死；一旦判死就会丢弃连接，
/// 而服务器只允许 1 条连接，旧连接还在传、新连接立刻 421。
const FTP_STOR_TIMEOUT: Duration = Duration::from_secs(900);
/// 流式 STOR 的截止时间。流式上传从第一笔写入一直开到句柄关闭，
/// 整个拷贝期间都占着这条命令，总时长没有上限（100GB 也能拷）。
/// 真正的「卡死」由写端检测：`FtpFs::do_write` 每次投递数据都有
/// 停滞截止（`STREAM_STALL_TIMEOUT`），一旦传不动就掐掉通道，
/// 这里读到 EOF 自然收尾，不会真等满 6 小时。
const FTP_STREAM_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// 流式上传的读端：从通道里一帧一帧取数据喂给 STOR。
///
/// 之所以不直接把 `TransferStream` 交给 `FtpFs`，是因为那会把 suppaftp
/// 的泛型参数泄漏到文件系统层；用通道做隔离后，写端只管 `try_send`，
/// 读端只管 `recv`，两边的类型互不相干。
struct ChanReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(v) => {
                    self.buf = v;
                    self.pos = 0;
                }
                // 写端 drop = 数据发完了
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// 这个错误是否意味着「连接本身已经不可信」，不能再还回池子。
///
/// - `ConnectionError`：socket 读写失败 / 超时 / 对端关闭
/// - `BadResponse`：响应语法都不对，通常是对端发来半截数据或连接已串包
///
/// `UnexpectedResponse`（如 550）属于**协议层**拒绝，连接是好的，要照常归还。
fn is_conn_err(e: &suppaftp::FtpError) -> bool {
    matches!(
        e,
        suppaftp::FtpError::ConnectionError(_) | suppaftp::FtpError::BadResponse
    )
}

pub fn to_filetime(dt: chrono::NaiveDateTime) -> u64 {
    let u = dt.and_utc();
    ((u.timestamp() + FILETIME_EPOCH_DIFF) as u64) * 10_000_000
        + (u.timestamp_subsec_nanos() as u64) / 100
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
}

/// 连接池：FTP 控制连接复用 + 并发上限。
///
/// 之前所有 FTP 操作共用**一条**带 mutex 的连接，于是任何一个慢 LIST 都会把
/// 整条连接占住，资源管理器枚举其他目录、甚至整个盘都卡死（用户报「双击就要
/// 等 / 机器卡死」）。改成连接池后，多个操作可以并行跑在不同连接上：一个目录
/// 的 LIST 再慢，也只阻塞它自己那一条连接，别的目录照样能列、能打开。
struct Pool {
    /// 空闲、可直接取用的连接（附最后归还时刻，用来判断是否已被服务器踢掉）
    idle: Mutex<Vec<IdleConn>>,
    /// 当前存活连接总数（空闲 + 借出），不超过 max
    count: Mutex<usize>,
    /// 取不到连接时在此等待（有人 checkin / 放弃连接时唤醒）
    sem: Condvar,
    /// 并发上限。服务器回 421（连接数已满）时会动态收缩，见 [`Pool::checkout`]。
    max: AtomicUsize,
}

struct IdleConn {
    s: FtpStream,
    since: Instant,
}

/// 服务器是不是在说「连接数满了」。
///
/// 只认 421：`530 Not logged in` 是认证问题，`421 Service not available`
/// 也可能是被临时封禁，都不该触发收缩（收缩后反而更难恢复）。
fn is_too_many_conn(e: &suppaftp::FtpError) -> bool {
    matches!(
        e,
        suppaftp::FtpError::UnexpectedResponse(r) if r.status == suppaftp::Status::NotAvailable
    )
}

impl Pool {
    fn new(max: usize) -> Arc<Pool> {
        Arc::new(Pool {
            idle: Mutex::new(Vec::new()),
            count: Mutex::new(0),
            sem: Condvar::new(),
            max: AtomicUsize::new(max.max(1)),
        })
    }

    /// 借一条连接：优先用空闲的；没有且未达上限就新建；达到上限就等别人还。
    ///
    /// 空闲连接**不能盲信**：服务器普遍有 idle timeout（这台实测几分钟），
    /// 空闲久了会单方面发 `421` 并关闭。以前直接把这种连接还回池子复用，
    /// 结果整条盘上每个操作都在死 socket 上失败，而 MKD 的失败又被上层映射成
    /// 「文件已存在」，表现就是「新建目录提示该项目已存在 / 该项目不在 R 中」，
    /// 而且永远不恢复。现在归还超过 [`IDLE_PROBE`] 的连接先用 NOOP 探活。
    fn checkout(
        &self,
        mut connect: impl FnMut() -> Result<FtpStream, suppaftp::FtpError>,
    ) -> Result<FtpStream, suppaftp::FtpError> {
        loop {
            let cand = { self.idle.lock().unwrap().pop() };
            if let Some(ic) = cand {
                let mut s = ic.s;
                if ic.since.elapsed() >= IDLE_PROBE {
                    match s.noop() {
                        Ok(()) => {}
                        Err(e) => {
                            crate::logbuf::debug(&format!(
                                "[ftp] 空闲连接已失效（{e}），重连"
                            ));
                            self.release();
                            continue;
                        }
                    }
                }
                return Ok(s);
            }

            {
                let mut cnt = self.count.lock().unwrap();
                let max = self.max.load(Ordering::Relaxed);
                if *cnt < max {
                    *cnt += 1;
                    drop(cnt);
                    match connect() {
                        Ok(c) => return Ok(c),
                        Err(e) => {
                            self.release();
                            if is_too_many_conn(&e) {
                                // 服务器不让开更多连接：把上限压到「当前已建立的条数」，
                                // 之后改走排队等空闲连接，而不是让这次操作直接失败。
                                let live = *self.count.lock().unwrap();
                                if live >= 1 {
                                    self.max.store(live, Ordering::Relaxed);
                                    crate::logbuf::debug(&format!(
                                        "[ftp] 服务器拒绝新连接（{e}），并发上限收缩为 {live}"
                                    ));
                                    continue;
                                }
                            }
                            return Err(e);
                        }
                    }
                }
            }

            // 达到上限：等有人还连接（带超时，避免永久阻塞）
            let idle = self.idle.lock().unwrap();
            if !idle.is_empty() {
                continue;
            }
            let (g, res) = self
                .sem
                .wait_timeout(idle, Duration::from_secs(20))
                .unwrap();
            drop(g);
            if res.timed_out() {
                return Err(suppaftp::FtpError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "等待 FTP 连接超时",
                )));
            }
        }
    }

    /// 还回一条连接（仅健康的才还）
    fn checkin(&self, c: FtpStream) {
        self.idle.lock().unwrap().push(IdleConn {
            s: c,
            since: Instant::now(),
        });
        self.sem.notify_one();
    }

    /// 放弃一条连接（超时/损坏）：存活计数减一，并唤醒等待者
    fn abandon(&self) {
        self.release();
    }

    fn release(&self) {
        let mut c = self.count.lock().unwrap();
        *c = c.saturating_sub(1);
        drop(c);
        self.sem.notify_all();
    }

    fn drain(&self) {
        let mut idle = self.idle.lock().unwrap();
        while let Some(mut ic) = idle.pop() {
            let _ = ic.s.quit();
        }
    }
}

pub struct Remote {
    pub cfg: Config,
    pool: Arc<Pool>,
    /// 目录路径 → (缓存时间, 条目)。只缓存 list() 的结果。
    list_cache: Mutex<HashMap<String, (Instant, Vec<Entry>)>>,
    /// 远端路径 → (缓存时间, 文件内容)。只缓存 ≤1MiB 的小文件。
    read_cache: Mutex<HashMap<String, (Instant, Vec<u8>)>>,
    /// 最近一次连接失败时间（熔断用）
    last_fail: Mutex<Option<Instant>>,
}

impl Remote {
    pub fn new(cfg: Config) -> Self {
        // 并发连接数：**必须大于 1**。
        //
        // 只开 1 条连接时，一个大文件的 RETR 会把连接占住几分钟，期间资源管理器的
        // 刷新 / 新建目录 / 取属性全部排在后面，8 秒超时一到就失败，用户看到的是
        // 「位置不可用，由于 I/O 设备错误」。实测：8GB 拷贝进行中，列目录要等 21 秒，
        // 取文件属性 20 秒后直接报「路径不存在」。
        //
        // 所以按配置开多条（默认 4）。遇到只允许单连接的服务器，服务器会回 421，
        // `Pool::checkout` 会动态把上限收缩到已建立的条数，退化成原来的行为，
        // 不会把老服务器打死。
        let max_conn = cfg.max_conn.max(1);
        Self {
            cfg,
            pool: Pool::new(max_conn),
            list_cache: Mutex::new(HashMap::new()),
            read_cache: Mutex::new(HashMap::new()),
            last_fail: Mutex::new(None),
        }
    }

    fn connect(&self) -> Result<FtpStream, suppaftp::FtpError> {
        use std::net::ToSocketAddrs;
        let addr = (self.cfg.host.as_str(), self.cfg.port)
            .to_socket_addrs()
            .map_err(suppaftp::FtpError::ConnectionError)?
            .next()
            .ok_or_else(|| {
                suppaftp::FtpError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("无法解析 {}:{}", self.cfg.host, self.cfg.port),
                ))
            })?;
        // 建连超时 + 控制连接读写超时：服务器挂掉时快点失败、快点重连，
        // 而不是把整个文件系统拖死。
        let tcp = std::net::TcpStream::connect_timeout(&addr, FTP_CONNECT_TIMEOUT)
            .map_err(suppaftp::FtpError::ConnectionError)?;
        tcp.set_read_timeout(Some(FTP_IO_TIMEOUT))
            .map_err(suppaftp::FtpError::ConnectionError)?;
        tcp.set_write_timeout(Some(FTP_IO_TIMEOUT))
            .map_err(suppaftp::FtpError::ConnectionError)?;
        let mut s = FtpStream::connect_with_stream(tcp)?;
        // 数据连接（PASV）同样要超时，否则传输中途卡住会永远阻塞在 io::copy
        s = s.passive_stream_builder(|addr| {
            let stream = std::net::TcpStream::connect(addr)
                .map_err(suppaftp::FtpError::ConnectionError)?;
            stream
                .set_read_timeout(Some(FTP_IO_TIMEOUT))
                .map_err(suppaftp::FtpError::ConnectionError)?;
            stream
                .set_write_timeout(Some(FTP_IO_TIMEOUT))
                .map_err(suppaftp::FtpError::ConnectionError)?;
            Ok(stream)
        });
        s.login(&self.cfg.user, &self.cfg.pass)?;
        // 必须显式切二进制，否则 ASCII 模式会改 CRLF，写二进制文件必坏
        s.transfer_type(suppaftp::types::FileType::Binary)?;
        Ok(s)
    }

    /// 借一条连接执行操作；出错就丢弃连接，下次自动重连。
    ///
    /// 所有 FTP 操作都走这里，统一套上 **应用层硬超时**（`FTP_OP_TIMEOUT`）和
    /// **连接池**：操作在独立线程里跑，主线程用带截止的会合通道等结果。超时
    /// 就丢连接、立刻报错——绝不让一个慢 FTP 命令把 WinFsp 的同步 IRP 卡死，
    /// 进而把整个资源管理器乃至机器拖假死（2026-10-18 实测：单个 `list /`
    /// 阻塞 142 秒）。连接池让多个操作并行跑在不同连接上，一个慢 LIST 只阻塞
    /// 它自己那条连接，别的目录照样能列、能打开。
    fn with<T>(
        &self,
        f: impl FnOnce(&mut FtpStream) -> Result<T, suppaftp::FtpError> + Send + 'static,
    ) -> Result<T, suppaftp::FtpError>
    where
        T: Send + 'static,
    {
        self.with_timeout(f, FTP_OP_TIMEOUT)
    }

    /// 同 [`Self::with`]，但可以指定截止时间。
    ///
    /// 元数据操作（LIST/MKD/DELE）8 秒足够；**数据传输**（RETR/STOR 一个区间）
    /// 不能套 8 秒——几 MB 的区间在慢线路上就要超过 8 秒，会被误判超时，
    /// 上层再翻译成「I/O 设备错误」，用户看到的就是拷贝大文件失败。
    fn with_timeout<T>(
        &self,
        f: impl FnOnce(&mut FtpStream) -> Result<T, suppaftp::FtpError> + Send + 'static,
        timeout: Duration,
    ) -> Result<T, suppaftp::FtpError>
    where
        T: Send + 'static,
    {
        // 熔断：刚连接失败过就快速失败，别让每个 IRP 都等一次连接超时。
        if let Some(t) = *self.last_fail.lock().unwrap() {
            if t.elapsed() < FAIL_COOLDOWN {
                return Err(suppaftp::FtpError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "服务器暂不可达（熔断中，稍后自动重试）",
                )));
            }
        }
        let mut conn = match self.pool.checkout(|| self.connect()) {
            Ok(c) => {
                *self.last_fail.lock().unwrap() = None;
                c
            }
            Err(e) => {
                *self.last_fail.lock().unwrap() = Some(Instant::now());
                return Err(e);
            }
        };
        let pool = self.pool.clone();
        // 操作在独立线程里跑，主线程用带截止的会合通道（容量 0）等结果。
        // 会合通道保证：主线程没在等（已超时丢弃 rx）时 send 立刻失败，后台
        // 线程随即退出并把连接一起丢弃，不会泄漏。
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<(Result<T, suppaftp::FtpError>, FtpStream)>(0);
        let _ = std::thread::spawn(move || {
            let r = f(&mut conn);
            let _ = tx.send((r, conn));
        });
        match rx.recv_timeout(timeout) {
            Ok((r, conn)) => {
                // 连接层出错说明这条 socket 已经不可信，**绝不能**还回池子：
                // 还回去的话后面每个操作都会在同一个死连接上失败，而且永远
                // 不恢复（用户报的「等一会儿再新建目录，提示该项目已存在 /
                // 不在 R 中」就是这么来的）。协议层错误（550 之类）连接本身
                // 是好的，照常归还。
                if let Err(e) = &r {
                    if is_conn_err(e) {
                        crate::logbuf::debug(&format!("[ftp] 连接损坏，丢弃并重连: {e}"));
                        pool.abandon();
                        return r;
                    }
                }
                pool.checkin(conn); // 连接完好，放回池子复用
                r
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 超时：丢弃这条连接（后台线程的 send 会失败、自行退出），
                // 存活计数减一，下次自动重连。
                pool.abandon();
                Err(suppaftp::FtpError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "FTP 操作超时（服务器无响应，已放弃该连接）",
                )))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                pool.abandon();
                Err(suppaftp::FtpError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "FTP 操作线程异常退出",
                )))
            }
        }
    }

    /// Windows 路径（\a\b）→ 远端绝对路径（/root/a/b）
    pub fn abs(&self, win_path: &str) -> String {
        let p = win_path.replace('\\', "/");
        let p = p.trim_start_matches('/');
        let root = self.cfg.remote_root();
        if p.is_empty() {
            root
        } else if root == "/" {
            format!("/{p}")
        } else {
            format!("{root}/{p}")
        }
    }

    pub fn list_mlsd(&self, remote: &str) -> Result<Vec<Entry>, suppaftp::FtpError> {
        let r = remote.to_string();
        let raw = self.with(move |s| s.mlsd(Some(&r)))?;
        Ok(raw.iter().filter_map(|l| parse_mlsd(l)).collect())
    }

    pub fn list_raw(&self, remote: &str) -> Result<Vec<Entry>, suppaftp::FtpError> {
        let r = remote.to_string();
        let raw = self.with(move |s| s.list(Some(&r)))?;
        let off = self.cfg.list_tz_offset;
        Ok(raw.iter().filter_map(|l| parse_list(l, off)).collect())
    }

    /// 目录列表：先查 10 秒缓存；没有或过期才真去问服务器。
    ///
    /// 以前这里没有缓存，Explorer 枚举一个目录时对每个文件都会走过来一次
    /// （get_security_by_name → stat → list 父目录），N 个文件就是 N 次
    /// MLSD+LIST，LIST 不通时还要再补 N 次 MDTM，全部串行挤在同一条连接上。
    /// 目录一大就把整个文件系统卡死。
    ///
    /// 现在失败时有「过期缓存兜底」：服务器这次慢/超时而 list_uncached 失败，
    /// 只要手里还有一份（哪怕过期）缓存，就先端出去——目录照样能打开，总比
    /// 让 Explorer 弹「位置不可用」强。下次枚举若服务器恢复，自然就刷新了。
    pub fn list(&self, remote: &str) -> Result<Vec<Entry>, suppaftp::FtpError> {
        let key = remote.trim_end_matches('/').to_string();
        {
            let c = self.list_cache.lock().unwrap();
            if let Some((t, v)) = c.get(&key) {
                if t.elapsed() < LIST_CACHE_TTL {
                    return Ok(v.clone());
                }
            }
        }
        match self.list_uncached(remote) {
            Ok(out) => {
                self.list_cache
                    .lock()
                    .unwrap()
                    .insert(key, (Instant::now(), out.clone()));
                Ok(out)
            }
            Err(e) => {
                let c = self.list_cache.lock().unwrap();
                if let Some((_, v)) = c.get(&key) {
                    crate::logbuf::debug(&format!(
                        "[ftp] list {remote} 失败，退回过期缓存: {e}"
                    ));
                    Ok(v.clone())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// 任何改动都让全部列表缓存失效。改动本来就少，简单粗暴最安全。
    pub fn invalidate_lists(&self) {
        self.list_cache.lock().unwrap().clear();
    }

    fn list_uncached(&self, remote: &str) -> Result<Vec<Entry>, suppaftp::FtpError> {
        let t0 = Instant::now();
        let out = if let Ok(v) = self.list_mlsd(remote) {
            Ok(v)
        } else {
            // MLSD 不支持（如用户服务器返回 500 Unknown command）：退回 LIST。
            // LIST 行本身已带时间，直接解析即可，**绝不**再逐个文件补 MDTM——
            // 那是 O(N) 次往返，22 个文件的目录会被拖到 6~7 秒，且每次都占一条
            // 连接，正是「双击文件夹要等很久」的元凶之一。
            let v = self.list_raw(remote)?;
            Ok(v)
        };
        crate::logbuf::debug(&format!(
            "[ftp] list {} 用时 {}ms",
            remote,
            t0.elapsed().as_millis()
        ));
        out
    }

    pub fn read_file(&self, remote: &str) -> Result<Vec<u8>, suppaftp::FtpError> {
        let r = remote.to_string();
        let cur = self.with(move |s| s.retr_as_buffer(&r))?;
        Ok(cur.into_inner())
    }

    /// 下载到本地文件：边收边写，内存占用与文件大小无关。
    /// 大文件（几百 MB）用 retr_as_buffer 会把整个文件读进 Vec，峰值能到文件的两倍。
    pub fn retr_to_file(
        &self,
        remote: &str,
        out: &mut std::fs::File,
    ) -> Result<u64, suppaftp::FtpError> {
        // 闭包要移到独立线程跑（带硬超时），所以需持有数据所有权：
        // remote 转 owned，out 用 try_clone() 复制 fd（dup，写到同一文件）。
        let r = remote.to_string();
        let mut out_clone = out
            .try_clone()
            .map_err(|e| suppaftp::FtpError::ConnectionError(e))?;
        self.with_timeout(move |s| {
            // 复位 REST：这条连接可能刚被区间读用过、残留了续传偏移。
            // 不清掉的话，这里的全量 RETR 会从那个偏移开始，下载内容整体错位。
            let _ = s.resume_transfer(0);
            s.retr(&r, |stream| {
                std::io::copy(stream, &mut out_clone).map_err(suppaftp::FtpError::ConnectionError)
            })
        }, FTP_XFER_TIMEOUT)
    }

    /// 带内容缓存的下载。小文件在 TTL 内被重复打开时直接走内存，
    /// 不再打 FTP；大文件和缓存未命中都退回 `retr_to_file` 流式下载。
    ///
    /// 返回写入 `out` 的字节数。`out` 必须可读可写（调用方建缓存文件时
    /// 就是 read+write），未命中时要回读内容进缓存。
    pub fn retr_to_file_cached(
        &self,
        remote: &str,
        out: &mut std::fs::File,
    ) -> Result<u64, suppaftp::FtpError> {
        // 1) 命中：内存里已有 2 秒内的内容，写进缓存文件即可
        let hit = {
            let c = self.read_cache.lock().unwrap();
            match c.get(remote) {
                Some((t, d)) if t.elapsed() < READ_CACHE_TTL => Some(d.clone()),
                _ => None,
            }
        };
        if let Some(d) = hit {
            out.seek(SeekFrom::Start(0))
                .map_err(suppaftp::FtpError::ConnectionError)?;
            out.set_len(0)
                .map_err(suppaftp::FtpError::ConnectionError)?;
            out.write_all(&d)
                .map_err(suppaftp::FtpError::ConnectionError)?;
            crate::logbuf::debug(&format!("[ftp] 内存缓存命中 {remote} ({} 字节)", d.len()));
            return Ok(d.len() as u64);
        }

        // 2) 未命中：流式下载（与 want 大小无关，大文件也不会进内存）
        let n = self.retr_to_file(remote, out)?;

        // 3) 小文件回读进缓存。回读失败不碍事，当本次没缓存。
        if (n as usize) <= READ_CACHE_MAX_FILE {
            let mut buf = vec![0u8; n as usize];
            if out.seek(SeekFrom::Start(0)).is_ok() && out.read_exact(&mut buf).is_ok() {
                self.read_cache_put(remote.to_string(), buf);
                crate::logbuf::debug(&format!("[ftp] 下载并缓存 {remote} ({n} 字节)"));
            }
        } else {
            crate::logbuf::debug(&format!("[ftp] 流式下载 {remote} ({n} 字节，大文件不进缓存)"));
        }
        Ok(n)
    }

    /// 按需拉取文件的 [start, end) 区间（REST 续传），写入本地缓存文件的对应偏移。
    ///
    /// 只读打开的大文件**不再整文件下载**——资源管理器读 ISO 前几 KB 做缩略图、
    /// 视频播放器按块读、文本编辑器读头部，都只拉需要的那一段。几百 GB 的 ISO
    /// 也能秒开，且不会把唯一的 FTP 连接占满导致整个盘卡死（2026-10-18 实测：
    /// `ls -la` 一个装满大 ISO 的目录会因为整文件下载挂死 40~86 秒）。
    pub fn retr_range(
        &self,
        remote: &str,
        out: &mut std::fs::File,
        start: u64,
        end: u64,
    ) -> Result<u64, suppaftp::FtpError> {
        let r = remote.to_string();
        let mut out_clone = out
            .try_clone()
            .map_err(suppaftp::FtpError::ConnectionError)?;
        let want = end - start;
        let got = Arc::new(AtomicU64::new(0));
        let got_in = got.clone();
        self.with_timeout(move |s| {
            // 先复位再设目标偏移：避免上一条命令残留的 REST 影响本次续传。
            let _ = s.resume_transfer(0);
            // REST 告诉服务器从 start 字节续传，随后 RETR 只传 [start, end) 这一段。
            // 标准 FTP 服务器（RFC 3659）都支持 REST，用户这台也在其列。
            s.resume_transfer(start as usize)?;
            let res = s.retr(&r, |stream| {
                out_clone
                    .seek(SeekFrom::Start(start))
                    .map_err(suppaftp::FtpError::ConnectionError)?;
                let mut remaining = want;
                let mut buf = [0u8; 1 << 16];
                while remaining > 0 {
                    let take = buf.len().min(remaining as usize);
                    let n = stream
                        .read(&mut buf[..take])
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    if n == 0 {
                        break;
                    }
                    out_clone
                        .write_all(&buf[..n])
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    remaining -= n as u64;
                }
                got_in.store(want - remaining, Ordering::Relaxed);
                Ok(())
            });
            // 关键：区间读是「收够就主动断开数据连接」，服务器会回一个
            // 426/451（Transfer aborted）表示抗议。数据其实已经收全了，
            // 不能把它当失败——否则拷贝大文件时每次预读都会报 I/O 错误
            // （用户实测 5GB ISO → 0x8007045D）。
            if let Err(e) = res {
                if got_in.load(Ordering::Relaxed) >= want {
                    crate::logbuf::debug(&format!(
                        "[ftp] 区间读 [{}..{}) 收够 {} 字节后断开，忽略服务器尾包: {e}",
                        start, end, want
                    ));
                } else {
                    return Err(e);
                }
            }
            // 传输完把 REST 复位，别把续传偏移留给连接池里的下一条命令。
            let _ = s.resume_transfer(0);
            Ok(())
        }, FTP_XFER_TIMEOUT)?;
        Ok(got.load(Ordering::Relaxed))
    }

    fn read_cache_put(&self, key: String, data: Vec<u8>) {
        let mut c = self.read_cache.lock().unwrap();
        // 顺手清过期的，防止 map 只涨不消
        let now = Instant::now();
        let expired: Vec<String> = c
            .iter()
            .filter(|(_, (t, _))| t.elapsed() >= READ_CACHE_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            c.remove(&k);
        }
        let used: usize = c.values().map(|(_, d)| d.len()).sum();
        if used + data.len() > READ_CACHE_MAX_TOTAL {
            c.clear();
        }
        c.insert(key, (now, data));
    }

    /// 清读内容缓存。上传/删除/改名成功后调用：内容变了，旧缓存就是脏数据。
    pub fn invalidate_reads(&self) {
        self.read_cache.lock().unwrap().clear();
    }

    /// 任何改动都让列表缓存和读缓存全部失效。改动本来就少，简单粗暴最安全。
    pub fn invalidate_all(&self) {
        self.invalidate_lists();
        self.invalidate_reads();
    }

    pub fn mtime(&self, remote: &str) -> Option<u64> {
        let rp = remote.to_string();
        let m = self.with(move |s| s.mdtm(&rp)).ok()?;
        let ft = to_filetime(m);
        crate::logbuf::debug(&format!("[ftp] MDTM {remote} -> {ft}"));
        Some(ft)
    }

    /// 上传：从本地缓存文件整体 STOR 上去。
    ///
    /// 必须走连接池，**不能**再单独 `connect()` 开第二条连接——用户这台服务器
    /// 只允许 1 个并发连接，池里已经占着一条，再开一条立刻被 `421 There are too
    /// many connected users` 拒绝，表现就是"新建/覆盖/编辑全部失败"。
    pub fn stor(&self, remote: &str, data: &mut std::fs::File) -> Result<u64, suppaftp::FtpError> {
        let r = remote.to_string();
        let mut data = data
            .try_clone()
            .map_err(suppaftp::FtpError::ConnectionError)?;
        data.seek(SeekFrom::Start(0))
            .map_err(suppaftp::FtpError::ConnectionError)?;
        let res = self.with_timeout(
            move |s| {
                // 防御性复位 REST，确保 STOR 从 0 覆盖写。
                let _ = s.resume_transfer(0);
                s.put_file(&r, &mut data)
            },
            FTP_STOR_TIMEOUT,
        );
        if res.is_ok() {
            self.invalidate_all();
        }
        res
    }

    /// 流式 STOR：边收边传，直到通道被写端关闭（= 数据发完）才返回。
    ///
    /// 这是大文件拷贝不再「卡在 99%」的关键。以前的路径是：所有写入先落
    /// 本地缓存，句柄关闭时才整文件 STOR —— 资源管理器在数据写完的那一刻
    /// 就把进度条顶到 99%，之后要干等整文件上传完（实测 8.5GB 卡了
    /// 4 分 14 秒），用户只能强杀。改成流式后，`write` 会被网络节奏带着走，
    /// 进度条反映的是真实上传进度。
    pub fn stor_stream(
        &self,
        remote: &str,
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
    ) -> Result<u64, suppaftp::FtpError> {
        let r = remote.to_string();
        // 1MB 缓冲：suppaftp 的 put_file 内部默认只给 8KB，
        // 大文件下系统调用次数太多，这里自己套一层大缓冲。
        let mut reader = std::io::BufReader::with_capacity(
            1 << 20,
            ChanReader {
                rx,
                buf: Vec::new(),
                pos: 0,
            },
        );
        let res = self.with_timeout(
            move |s| {
                // 防御性复位 REST，确保 STOR 从 0 覆盖写。
                let _ = s.resume_transfer(0);
                s.put_file(&r, &mut reader)
            },
            FTP_STREAM_TIMEOUT,
        );
        if res.is_ok() {
            self.invalidate_all();
        }
        res
    }

    pub fn dele(&self, remote: &str) -> Result<(), suppaftp::FtpError> {
        let rp = remote.to_string();
        let r = self.with(move |s| s.rm(&rp));
        if r.is_ok() {
            self.invalidate_all();
        }
        r
    }

    pub fn mkd(&self, remote: &str) -> Result<(), suppaftp::FtpError> {
        let rp = remote.to_string();
        let r = self.with(move |s| s.mkdir(&rp));
        if r.is_ok() {
            self.invalidate_all();
        }
        r
    }

    pub fn rmd(&self, remote: &str) -> Result<(), suppaftp::FtpError> {
        let rp = remote.to_string();
        let r = self.with(move |s| s.rmdir(&rp));
        if r.is_ok() {
            self.invalidate_all();
        }
        r
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), suppaftp::FtpError> {
        let fp = from.to_string();
        let tp = to.to_string();
        let r = self.with(move |s| s.rename(&fp, &tp));
        if r.is_ok() {
            self.invalidate_all();
        }
        r
    }

    pub fn quit(&self) {
        self.pool.drain();
    }
}

/// MLSD 行示例：`type=file;size=123;modify=20240315103000.000; name.txt`
pub fn parse_mlsd(line: &str) -> Option<Entry> {
    let (facts, name) = line.split_once(' ')?;
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let mut is_dir = false;
    let mut size = 0u64;
    let mut mtime = 0u64;
    for kv in facts.split(';') {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "type" => is_dir = v.eq_ignore_ascii_case("dir"),
                "size" => size = v.parse().unwrap_or(0),
                "modify" | "create" => {
                    let core = v.split('.').next().unwrap_or(v);
                    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(core, "%Y%m%d%H%M%S") {
                        mtime = to_filetime(dt);
                    }
                }
                _ => {}
            }
        }
    }
    Some(Entry {
        name: name.to_string(),
        is_dir,
        size,
        mtime,
    })
}

/// LIST 行示例：
/// `-rw-r--r--   1 ftp ftp  1234 Mar 15 10:30 hello.txt`
/// `drwxr-xr-x   2 ftp ftp     0 Mar 15  2024 subdir`
pub fn parse_list(line: &str, tz_offset_min: i32) -> Option<Entry> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with("total") {
        return None;
    }
    let perms = line.split_whitespace().next()?;
    if perms.len() < 10 {
        return None;
    }
    let is_dir = perms.starts_with('d');

    // 前 8 个字段用空白切，剩下整段是文件名（文件名里可以有空格）
    let mut rest = line;
    let mut fields: Vec<&str> = Vec::with_capacity(8);
    for _ in 0..8 {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    let name = rest.trim_start().to_string();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    let size: u64 = fields[4].parse().unwrap_or(0);

    let dt = if fields[7].contains(':') {
        // 无年份：默认今年
        let this_year = chrono::Utc::now().year();
        let s = format!("{} {} {} {}", fields[5], fields[6], fields[7], this_year);
        chrono::NaiveDateTime::parse_from_str(&s, "%b %e %H:%M %Y").ok()?
    } else {
        let s = format!("{} {} {}", fields[5], fields[6], fields[7]);
        chrono::NaiveDate::parse_from_str(&s, "%b %e %Y")
            .ok()?
            .and_hms_opt(0, 0, 0)?
    };

    let mut mtime = to_filetime(dt);
    if tz_offset_min != 0 {
        // LIST 给的是服务器本地时间，减去偏移换算成 UTC
        mtime = (mtime as i64 - tz_offset_min as i64 * 60 * 10_000_000) as u64;
    }

    Some(Entry {
        name,
        is_dir,
        size,
        mtime,
    })
}
