//! 挂载生命周期管理。
//!
//! WinFsp 的 `start()` 只是启动分发线程就返回，但 `FileSystemHost` 必须一直活着。
//! 所以挂载跑在独立线程里，线程持有 host，主线程通过条件变量请求它停下。
//! 这样也避开了 `FileSystemHost<FtpFs>` 要不要实现 Send 的问题。

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use ftpmount_rs::config::Config;
use ftpmount_rs::fs::{FtpFs, NotifyEvent};
use ftpmount_rs::ftp::Remote;
use ftpmount_rs::logbuf;
use serde::Serialize;
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};

#[derive(Serialize, Clone, Default)]
pub struct Status {
    pub mounted: bool,
    /// 正在挂载中（还没出结果）。开机自动挂载时会短暂为 true，
    /// 前端据此把界面显示成「正在挂载…」，避免用户以为没挂上去手点「挂载」。
    pub starting: bool,
    pub drive: String,
    pub site: String,
    /// 已挂载秒数
    pub since: u64,
    /// 最近一次失败原因（成功挂载后清空）
    pub error: String,
}

struct Inner {
    running: bool,
    starting: bool,
    /// 每次 start 递增。挂载线程只认自己那一代，避免并发时旧线程把新状态清掉。
    gen: u64,
    stop_req: bool,
    drive: String,
    site: String,
    started: Option<Instant>,
    err: String,
    remote: Option<Arc<Remote>>,
}

pub struct Mgr {
    inner: Mutex<Inner>,
    cv: Condvar,
}

fn volume_params(drive: &str) -> VolumeParams {
    let mut params = VolumeParams::new();
    // 设备类型由 Prefix 是否为空决定：空 = WinFsp.Disk，非空 = WinFsp.Net。
    // 默认走磁盘模式（配文件夹挂载），因为只有「Disk + 文件夹」这一组合能同时
    // 拿到 FIXED 盘符身份和完整写权限，四种组合的对照见 lib.rs::winfsp_net_mode。
    if ftpmount_rs::winfsp_net_mode() {
        let prefix = ftpmount_rs::winfsp_prefix(drive);
        params.prefix(&prefix);
        logbuf::info(&format!("挂载模式 = WinFsp.Net (prefix={prefix})"));
    } else {
        logbuf::info("挂载模式 = WinFsp.Disk（磁盘模式，需配文件夹挂载）");
    }
    params
        .filesystem_name("FTP")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_creation_time(0)
        .volume_serial_number(0x4654_5031)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        // 打开：卷要像一块正常的 NTFS 盘那样支持并**执行** ACL。
        // 关掉（false）时 WinFsp 会用自己的一套默认权限做访问检查，
        // 结果是对已存在文件的写打开被拒（CreateFileW 带 GENERIC_WRITE 直接
        // ERROR_ACCESS_DENIED，连 FS 的 open 回调都进不来），资源管理器因此
        // 认为目标不可写，复制同名文件时不弹「替换或跳过」而是直接覆盖。
        // 我们所有文件都返回 Everyone 完全访问的描述符，打开是安全的。
        .persistent_acls(true)
        .file_info_timeout(1000)
        .post_cleanup_when_modified_only(true)
        .flush_and_purge_on_cleanup(true);
    params
}

impl Mgr {
    pub fn new() -> Arc<Self> {
        Arc::new(Mgr {
            inner: Mutex::new(Inner {
                running: false,
                starting: false,
                gen: 0,
                stop_req: false,
                drive: String::new(),
                site: String::new(),
                started: None,
                err: String::new(),
                remote: None,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn status(&self) -> Status {
        let g = self.inner.lock().unwrap();
        Status {
            mounted: g.running,
            starting: g.starting,
            drive: g.drive.clone(),
            site: g.site.clone(),
            since: g.started.map(|t| t.elapsed().as_secs()).unwrap_or(0),
            error: g.err.clone(),
        }
    }

    /// 已挂载时拿得到底层连接（读写测试要用）
    pub fn remote(&self) -> Option<Arc<Remote>> {
        self.inner.lock().unwrap().remote.clone()
    }

    /// 启动挂载，等到「挂载成功」或「明确失败」再返回。
    ///
    /// **幂等 + 串行**。开机自启时「自动挂载」和用户手点「挂载」很容易撞在一起 ——
    /// 自动挂载是静默的，界面要两三秒后才变成「已挂载」，用户看着像没反应就去点按钮。
    /// 以前两个线程同时 `host.mount("Z:")`，后一个拿到
    /// `STATUS_OBJECT_NAME_COLLISION`（0xD0000035「对象名已存在」），界面上显示
    /// 「挂载失败」，可盘其实已经挂上了；更糟的是失败那条线程还会把 `running`
    /// 清成 false，于是界面一直以为没挂、点「卸载」也卸载不掉。
    ///
    /// 现在用 `starting` + `gen` 串行化：后来的调用先等前一个出结果，
    /// 再按「同盘符算成功、不同盘符报已挂载」处理，绝不会有两条线程同时抢盘符。
    pub fn start(self: &Arc<Self>, cfg: Config, drive: String, site: String) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let gen: u64;
        {
            let mut g = self.inner.lock().unwrap();
            // 另一次挂载正在进行：等它出结果再决定，不要并发去抢同一个盘符
            while g.starting {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("另一次挂载还在进行，请稍后再试".into());
                }
                let (ng, _) = self.cv.wait_timeout(g, left).unwrap();
                g = ng;
            }
            if g.running {
                if g.drive.eq_ignore_ascii_case(drive.trim()) {
                    logbuf::debug(&format!("{drive} 已经在挂载中，忽略重复的挂载请求"));
                    return Ok(());
                }
                return Err(format!("已经挂载到 {}，请先卸载再换盘符", g.drive));
            }
            g.starting = true;
            g.gen = g.gen.wrapping_add(1);
            gen = g.gen;
            g.stop_req = false;
            g.err.clear();
            g.drive = drive.clone();
            g.site = site.clone();
        }

        // 盘符被别的程序占着时早点报错，不要等 WinFsp 抛一句看不懂的 NTSTATUS
        if std::fs::metadata(format!("{drive}\\")).is_ok() {
            let msg = format!("{drive} 已被占用，请换一个盘符");
            {
                let mut g = self.inner.lock().unwrap();
                if g.gen == gen {
                    g.err = msg.clone();
                    g.starting = false;
                }
            }
            self.cv.notify_all();
            return Err(msg);
        }

        let me = self.clone();
        std::thread::spawn(move || {
            let r = do_mount(&cfg, &drive, &me, &site, gen);
            let mut g = me.inner.lock().unwrap();
            // 只有「当前这一代」才能改状态：万一有并发的旧线程还在收尾，
            // 它不能把新一轮挂载的状态清掉。
            if g.gen == gen {
                g.running = false;
                g.starting = false;
                g.remote = None;
                g.started = None;
                if let Err(e) = r {
                    let msg = format!("{e:#}");
                    logbuf::error(&format!("挂载失败: {msg}"));
                    g.err = msg;
                }
            }
            drop(g);
            me.cv.notify_all();
        });

        // 等它挂上（或失败）
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.gen == gen {
                if g.running {
                    return Ok(());
                }
                if !g.err.is_empty() {
                    return Err(g.err.clone());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("挂载超时（15 秒内没有完成，请查看日志）".into());
            }
            let (ng, _) = self.cv.wait_timeout(g, left).unwrap();
            g = ng;
        }
    }

    /// 请求卸载并等它真的停掉
    pub fn stop(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        {
            let mut g = self.inner.lock().unwrap();
            // 正在挂载时不能直接喊停：等它挂上或失败，否则会留下半启动的 host
            while g.starting {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("正在挂载中，请稍后再卸载".into());
                }
                let (ng, _) = self.cv.wait_timeout(g, left).unwrap();
                g = ng;
            }
            if !g.running {
                return Ok(());
            }
            g.stop_req = true;
        }
        self.cv.notify_all();

        let mut g = self.inner.lock().unwrap();
        loop {
            if !g.running {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("卸载超时（有文件正在被占用？关掉资源管理器窗口再试）".into());
            }
            let (ng, _) = self.cv.wait_timeout(g, left).unwrap();
            g = ng;
        }
    }

    /// 挂载线程等这个
    fn wait_stop(&self) {
        let mut g = self.inner.lock().unwrap();
        while !g.stop_req {
            let (ng, _) = self.cv.wait_timeout(g, Duration::from_millis(500)).unwrap();
            g = ng;
        }
    }

    fn set_running(&self, remote: Arc<Remote>, gen: u64) {
        let mut g = self.inner.lock().unwrap();
        if g.gen != gen {
            // 这一代已经被后一次 start 取代，别把状态写成"已挂载"
            return;
        }
        g.running = true;
        g.starting = false;
        g.started = Some(Instant::now());
        g.remote = Some(remote);
        g.err.clear();
        drop(g);
        self.cv.notify_all();
    }
}

fn do_mount(
    cfg: &Config,
    drive: &str,
    mgr: &Arc<Mgr>,
    site: &str,
    gen: u64,
) -> anyhow::Result<()> {
    ftpmount_rs::preload_winfsp();

    if !ftpmount_rs::winfsp_installed() {
        return Err(anyhow!(
            "没有找到 WinFsp。请先安装 WinFsp 2.x（https://winfsp.dev/rel/）"
        ));
    }

    let _init = winfsp::winfsp_init()
        .map_err(|e| anyhow!("WinFsp 初始化失败（是否已安装 WinFsp 2.x？）: {e:?}"))?;

    logbuf::info(&format!(
        "连接 ftp://{}:{}{}",
        cfg.host,
        cfg.port,
        cfg.remote_root()
    ));
    let remote = Arc::new(Remote::new(cfg.clone()));
    let root = cfg.remote_root();
    let entries = remote
        .list(&root)
        .map_err(|e| anyhow!("连接服务器失败: {e}"))?;
    logbuf::info(&format!("远端 {root} 共 {} 项", entries.len()));

    let fs = FtpFs::new(remote.clone());
    // 带定时器的构造：每 400ms 把攒下的目录变更事件推给系统。
    // 不用它的话资源管理器拷完文件收不到通知，要白等约 6.2 秒才把进度推到 100%。
    let mut host: FileSystemHost<FtpFs> =
        FileSystemHost::new_with_timer::<Vec<NotifyEvent>, 400>(
            FileSystemParams::default_params(volume_params(drive)),
            fs,
        )
        .map_err(|e| anyhow!("创建文件系统失败: {e:?}"))?;
    // 盘符直接交给 WinFsp。网络模式下由 WinFsp.Launcher 服务分配盘符，
    // 不需要 net use。注意这样盘符是 DRIVE_REMOTE，资源管理器会按网络驱动器
    // 处理、每次操作后等约 6 秒 —— 原因和已排除的绕法见 lib.rs::winfsp_net_mode。
    host.mount(drive)
        .map_err(|e| anyhow!("挂载到 {drive} 失败: {e:?}"))?;
    // 必须用 UFCS：FineGuard / CoarseGuard 两个 impl 都有 `start`，
    // 方法调用语法会被判成歧义（E0034）。
    FileSystemHost::<FtpFs>::start(&mut host)
        .map_err(|e| anyhow!("启动分发线程失败: {e:?}"))?;

    mgr.set_running(remote, gen);
    logbuf::info(&format!(
        "已挂载 {drive}  [{site}] -> ftp://{}:{}{}",
        cfg.host,
        cfg.port,
        root
    ));

    mgr.wait_stop();

    logbuf::info("正在卸载…");
    host.stop();
    host.unmount();
    logbuf::info(&format!("{drive} 已卸载"));
    Ok(())
}
