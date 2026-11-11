//! ftpmount 核心库。
//!
//! - [`ftp`]：FTP 连接池、命令封装、列表解析
//! - [`fs`]：WinFsp 文件系统实现
//! - [`config`]：TOML 配置
//! - [`logbuf`]：全局日志（内存环形缓冲 + 按天轮转文件 + 可选实时回调）
//!
//! CLI（`src/main.rs`）与桌面版（`desktop/`）都复用这里的实现。

pub mod config;
pub mod ftp;
pub mod fs;
pub mod logbuf;

/// 加载 winfsp-x64.dll。
///
/// 任何用到 `winfsp::filesystem::DirBuffer` 的地方都要先调用它：DirBuffer 析构会
/// 调 `FspFileSystemDeleteDirectoryBuffer`，延迟加载失败会让进程被直接终止
/// （没有 panic、没有错误码）。
pub fn preload_winfsp() {
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> isize;
    }

    // 正常情况下 winfsp-x64.dll 会在 exe 目录或 PATH 里；
    // 找不到时按官方安装路径兜底加载（加载后按名字再查就能命中）。
    let candidates = [
        "C:\\Program Files (x86)\\WinFsp\\bin\\winfsp-x64.dll",
        "C:\\Program Files\\WinFsp\\bin\\winfsp-x64.dll",
    ];
    for c in candidates {
        let wide: Vec<u16> = c.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if LoadLibraryW(wide.as_ptr()) != 0 {
                return;
            }
        }
    }
}

/// 是否用 `WinFsp.Net`（网络提供程序）模式挂载，默认 true。
///
/// WinFsp 有两种设备，由 `VolumeParams.Prefix` 是否为空决定：
/// - Prefix 为空 → `WinFsp.Disk`，盘符卷模式
/// - Prefix 非空（形如 `\server\share`）→ `WinFsp.Net`，网络提供程序模式
///
/// **为什么必须用网络模式**：这台机器上磁盘模式是坏的 —— 直接挂成盘符后，对
/// **已存在**文件做任何带 `FILE_WRITE_DATA` 的打开都返回 ERROR_ACCESS_DENIED，
/// 新建文件却正常（新建检查的是父目录）。不是我们代码的问题：WinFsp 自带的
/// `memfs-x64.exe -m X:` 复现完全一致（READ_DATA / WRITE_ATTRIBUTES / DELETE 放行，
/// WRITE_DATA / WRITE_EA / GENERIC_WRITE 全拒）。换网络模式后全部通过。
///
/// ## 代价：资源管理器对网络盘符会等 6 秒
///
/// 网络模式下 `GetDriveType("Z:")` 是 `DRIVE_REMOTE`，资源管理器把它当网络驱动器，
/// 每次文件操作后多发两次 `CreateOptions=0x01210040` 的属性打开，然后干等约 6 秒。
/// 磁盘模式下是 `FIXED`、一次都不发 —— 但磁盘模式吃 err=5，不可用。
///
/// ## 2026-10-21 试过、已确认走不通的路
///
/// | 组合 | 盘符类型 | 写打开 |
/// |---|---|---|
/// | Disk + 直接挂盘符（0.1.2） | FIXED | **err=5** |
/// | Disk + 挂文件夹 | —（无盘符） | 全通 |
/// | Net + 直接挂盘符（现行） | REMOTE | 正常，但 6 秒 |
/// | Net + 挂文件夹 | — | **拒绝网络访问**（走 MUP，没人认领 `\ftpmount\Z`） |
///
/// 于是试过「Disk 模式挂文件夹 + DefineDosDevice 把盘符映射到文件夹」，
/// 确实能同时拿到 FIXED 和完整权限（memfs 实测 11 项全通）。**已放弃**，原因：
/// 1. 本质是骗 `GetDriveType`，属于绕路；
/// 2. **C 盘（系统盘）根本挂不上** —— WinFsp 建 reparse point 那步返回
///    `STATUS_OBJECT_NAME_COLLISION`，`memfs -m C:\...\Temp\xxx` 同样报错，
///    只有 D 盘这类非系统盘能挂，单盘机器直接不可用；
/// 3. 还得在数据盘根建 `D:\FtpMount\mnt`，打扰用户目录。
///
/// 结论：6 秒这个回归在 WinFsp 框架内无解，除非改驱动里 disk 设备的默认安全
/// 描述符，或者让资源管理器别对网络驱动器做那两次探测 —— 都不在我们手里。
/// 实际做法是「能用就用 RaiDrive 挂」（CBFS 驱动，无此问题）。
///
/// 要退回磁盘模式：设环境变量 `FTPMOUNT_NET=0`（会吃 err=5，仅诊断用）。
pub fn winfsp_net_mode() -> bool {
    std::env::var("FTPMOUNT_NET")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// 按盘符生成 WinFsp.Net 需要的 UNC 前缀，如 `"Z:"` → `\ftpmount\Z`。
/// 每个挂载点用不同前缀，避免多个站点互相抢同一个 UNC 名。
pub fn winfsp_prefix(drive: &str) -> String {
    let letter: String = drive
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(1)
        .collect();
    if letter.is_empty() {
        String::from("\\ftpmount\\ftp")
    } else {
        format!("\\ftpmount\\{}", letter.to_ascii_uppercase())
    }
}

/// WinFsp 是否已安装（看 dll 是否存在）。
pub fn winfsp_installed() -> bool {
    const PATHS: [&str; 2] = [
        "C:\\Program Files (x86)\\WinFsp\\bin\\winfsp-x64.dll",
        "C:\\Program Files\\WinFsp\\bin\\winfsp-x64.dll",
    ];
    PATHS.iter().any(|p| std::path::Path::new(p).exists())
}
