use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};

use ftpmount_rs::config::Config;
use ftpmount_rs::fs::{FtpFs, NotifyEvent};
use ftpmount_rs::ftp::Remote;
use ftpmount_rs::{logbuf, preload_winfsp};

fn usage() -> ! {
    eprintln!(
        "ftpmount-rs — 把 FTP 挂成 Windows 盘符\n\
         \n\
         用法:\n\
         \x20 ftpmount-rs mount <config.toml> [盘符]\n\
         \x20 ftpmount-rs check <config.toml>\n\
         \x20 ftpmount-rs selftest <config.toml>\n\
         \x20 ftpmount-rs fstest <config.toml>      # 不挂载，直接验证文件系统逻辑层\n"
    );
    std::process::exit(2);
}

/// 不依赖 WinFsp，单独验证 FTP 写路径（上传/读取/建目录/改名/删除）
fn selftest(cfg: Config) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let remote = Remote::new(cfg);
    let root = remote.cfg.remote_root();
    let base = if root == "/" { String::new() } else { root.clone() };

    let f1 = format!("{base}/_rs_selftest.txt");
    let f2 = format!("{base}/_rs_selftest_renamed.txt");
    let d1 = format!("{base}/_rs_selftest_dir");
    let payload = b"line1\nline2\n\x00\x01\x02 binary tail\n";

    // 先做一次 LIST，模拟挂载时的连接状态（LIST 之后再 STOR）
    let _ = remote.list(&root);
    println!("[0] 先 LIST 一次，连接已用过数据通道");

    // 1) 上传
    let mut tmp = tempfile_like()?;
    tmp.write_all(payload)?;
    tmp.seek(SeekFrom::Start(0))?;
    let n = remote
        .stor(&f1, &mut tmp)
        .map_err(|e| anyhow::anyhow!("STOR 失败: {e}"))?;
    println!("[1] STOR {f1} -> {n} bytes");

    // 2) 读回比对
    let back = remote
        .read_file(&f1)
        .map_err(|e| anyhow::anyhow!("RETR 失败: {e}"))?;
    println!(
        "[2] RETR 回读 {} bytes, 内容一致={}",
        back.len(),
        back == payload
    );

    // 3) 建目录
    match remote.mkd(&d1) {
        Ok(_) => println!("[3] MKD {d1} ok"),
        Err(e) => println!("[3] MKD {d1} 失败: {e}"),
    }
    let listed = remote.list(&root).map_err(|e| anyhow::anyhow!("LIST 失败: {e}"))?;
    println!(
        "[3] 列表里能看到目录={}",
        listed.iter().any(|e| e.is_dir && e.name == "_rs_selftest_dir")
    );

    // 4) 改名
    match remote.rename(&f1, &f2) {
        Ok(_) => {
            let listed = remote.list(&root).map_err(|e| anyhow::anyhow!("LIST 失败: {e}"))?;
            println!(
                "[4] RNFR/RNTO ok，旧名还在={} 新名存在={}",
                listed.iter().any(|e| e.name == "_rs_selftest.txt"),
                listed.iter().any(|e| e.name == "_rs_selftest_renamed.txt")
            );
        }
        Err(e) => println!("[4] RNFR/RNTO 失败: {e}"),
    }

    // 5) 删除
    let _ = remote.dele(&f2);
    let _ = remote.rmd(&d1);
    let listed = remote.list(&root).map_err(|e| anyhow::anyhow!("LIST 失败: {e}"))?;
    println!(
        "[5] 清理后残留文件={} 残留目录={}",
        listed.iter().any(|e| !e.is_dir && e.name.starts_with("_rs_selftest")),
        listed.iter().any(|e| e.is_dir && e.name == "_rs_selftest_dir")
    );

    remote.quit();
    let _ = std::fs::remove_file(tmp_path_of(&tmp));
    Ok(())
}

fn tempfile_like() -> Result<std::fs::File> {
    let p = std::env::temp_dir().join(format!("ftpmount-selftest-{}.tmp", std::process::id()));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&p)?;
    Ok(f)
}

fn tmp_path_of(f: &std::fs::File) -> PathBuf {
    let _ = f;
    std::env::temp_dir().join(format!("ftpmount-selftest-{}.tmp", std::process::id()))
}

/// 不依赖 WinFsp，直接验证文件系统逻辑层。
///
/// 重点覆盖两条在沙箱里挂载后无法触发的路径：
///   1. 覆盖已有文件（create 覆盖 / open + 截断）
///   2. 删除文件（set_delete + cleanup 发 DELE）
/// 这两条走的就是 WinFsp 回调会调到的同一份代码，只是少了参数转换那一层。
fn fstest(cfg: Config) -> Result<()> {
    let remote = Arc::new(Remote::new(cfg));
    let fs = FtpFs::new(remote.clone());

    let mut pass = 0u32;
    let mut fail = 0u32;

    let ow = "\\_rs_fs_ow.txt";
    let del = "\\_rs_fs_del.txt";
    let dir = "\\_rs_fs_dir";
    let ne_dir = "\\_rs_fs_ne";
    let ghost = "\\_rs_fs_ghost.txt";

    // ---------- 1. 覆盖已有文件：create 路径 ----------
    println!("[1] 覆盖已有文件（create 路径）");
    {
        let h = fs
            .begin_create(ow, false, 0)
            .map_err(|e| anyhow::anyhow!("create 失败: {e:#x}"))?;
        fs.do_write(&h, b"AAAAAAAAAA", 0, false)
            .map_err(|e| anyhow::anyhow!("write 失败: {e:#x}"))?;
        fs.do_cleanup(&h, false);
        fs.do_close(h);
        let got = remote.read_file(&remote.abs(ow)).ok();
        if got.as_deref() == Some(b"AAAAAAAAAA".as_ref()) {
            pass += 1;
            println!("  [OK]   首次上传 10 字节");
        } else {
            fail += 1;
            println!("  [FAIL] 首次上传内容不对: {:?}", got);
        }
    }
    {
        // 同一个路径再来一次，写更短的内容：旧的长内容必须被完整替换
        let h = fs
            .begin_create(ow, false, 0)
            .map_err(|e| anyhow::anyhow!("覆盖 create 失败: {e:#x}"))?;
        fs.do_write(&h, b"B", 0, false)
            .map_err(|e| anyhow::anyhow!("覆盖 write 失败: {e:#x}"))?;
        fs.do_cleanup(&h, false);
        fs.do_close(h);
        let got = remote.read_file(&remote.abs(ow)).ok();
        if got.as_deref() == Some(b"B".as_ref()) {
            pass += 1;
            println!("  [OK]   覆盖后内容为 'B'，旧内容无残留");
        } else {
            fail += 1;
            println!("  [FAIL] 覆盖后内容: {:?}（期望 b\"B\"）", got);
        }
        if let Some(e) = fs.stat(ow) {
            if e.size == 1 {
                pass += 1;
                println!("  [OK]   覆盖后 stat 大小 = 1");
            } else {
                fail += 1;
                println!("  [FAIL] 覆盖后 stat 大小 = {}", e.size);
            }
        } else {
            fail += 1;
            println!("  [FAIL] 覆盖后 stat 找不到文件");
        }
    }

    // ---------- 2. 覆盖已有文件：open + 截断路径 ----------
    println!("[2] 覆盖已有文件（open + set_len(0) 路径）");
    {
        let h = fs
            .begin_open(ow, 0x0000_0007)
            .map_err(|e| anyhow::anyhow!("open 失败: {e:#x}"))?;
        fs.do_set_len(&h, 0)
            .map_err(|e| anyhow::anyhow!("set_len 失败: {e:#x}"))?;
        fs.do_write(&h, b"CCC", 0, false)
            .map_err(|e| anyhow::anyhow!("write 失败: {e:#x}"))?;
        fs.do_cleanup(&h, false);
        fs.do_close(h);
        let got = remote.read_file(&remote.abs(ow)).ok();
        if got.as_deref() == Some(b"CCC".as_ref()) {
            pass += 1;
            println!("  [OK]   截断后写入 'CCC'，服务器一致");
        } else {
            fail += 1;
            println!("  [FAIL] 截断后内容: {:?}", got);
        }
    }

    // ---------- 3. 删除文件 ----------
    println!("[3] 删除文件（set_delete + cleanup 发 DELE）");
    {
        let h = fs
            .begin_create(del, false, 0)
            .map_err(|e| anyhow::anyhow!("create 失败: {e:#x}"))?;
        fs.do_write(&h, b"to-be-deleted\n", 0, false)
            .map_err(|e| anyhow::anyhow!("write 失败: {e:#x}"))?;
        fs.do_cleanup(&h, false);
        fs.do_close(h);
        let exists_before = fs.stat(del).is_some();
        println!("  · 准备：文件已上传，服务器上存在={exists_before}");

        let h = fs
            .begin_open(del, 0x0000_0007)
            .map_err(|e| anyhow::anyhow!("open 失败: {e:#x}"))?;
        match fs.do_set_delete(&h, true) {
            Ok(()) => {
                pass += 1;
                println!("  [OK]   set_delete(true) 返回成功");
            }
            Err(e) => {
                fail += 1;
                println!("  [FAIL] set_delete(true) 返回 {e:#x}");
            }
        }
        fs.do_cleanup(&h, true);
        fs.do_close(h);

        let gone = fs.stat(del).is_none();
        if gone {
            pass += 1;
            println!("  [OK]   服务器上文件已消失");
        } else {
            fail += 1;
            println!("  [FAIL] 服务器上文件仍然存在");
        }
        if remote.read_file(&remote.abs(del)).is_err() {
            pass += 1;
            println!("  [OK]   RETR 该路径失败，确认真的没了");
        } else {
            fail += 1;
            println!("  [FAIL] RETR 仍能取到内容");
        }
    }

    // ---------- 4. 删除空目录 ----------
    println!("[4] 删除空目录");
    {
        let h = fs
            .begin_create(dir, true, 0)
            .map_err(|e| anyhow::anyhow!("mkdir 失败: {e:#x}"))?;
        fs.do_close(h);
        let made = fs.stat(dir).map(|e| e.is_dir).unwrap_or(false);
        println!("  · 准备：目录已创建={made}");

        let h = fs
            .begin_open(dir, 0x0000_0007)
            .map_err(|e| anyhow::anyhow!("open dir 失败: {e:#x}"))?;
        match fs.do_set_delete(&h, true) {
            Ok(()) => {
                pass += 1;
                println!("  [OK]   目录 set_delete 成功（内部已发 RMD）");
            }
            Err(e) => {
                fail += 1;
                println!("  [FAIL] 目录 set_delete 返回 {e:#x}");
            }
        }
        fs.do_cleanup(&h, true);
        fs.do_close(h);
        if fs.stat(dir).is_none() {
            pass += 1;
            println!("  [OK]   服务器上目录已消失");
        } else {
            fail += 1;
            println!("  [FAIL] 服务器上目录仍存在");
        }
    }

    // ---------- 5. 删除非空目录必须报错 ----------
    println!("[5] 删除非空目录应报错");
    {
        let h = fs
            .begin_create(ne_dir, true, 0)
            .map_err(|e| anyhow::anyhow!("mkdir 失败: {e:#x}"))?;
        fs.do_close(h);
        let inner = format!("{ne_dir}\\inner.txt");
        let h = fs
            .begin_create(&inner, false, 0)
            .map_err(|e| anyhow::anyhow!("create inner 失败: {e:#x}"))?;
        fs.do_write(&h, b"x\n", 0, false)
            .map_err(|e| anyhow::anyhow!("write inner 失败: {e:#x}"))?;
        fs.do_cleanup(&h, false);
        fs.do_close(h);
        println!("  · 准备：目录内有 1 个文件");

        let h = fs
            .begin_open(ne_dir, 0x0000_0007)
            .map_err(|e| anyhow::anyhow!("open dir 失败: {e:#x}"))?;
        match fs.do_set_delete(&h, true) {
            Ok(()) => {
                fail += 1;
                println!("  [FAIL] 非空目录删除竟然成功了");
            }
            Err(0xC000_0101) => {
                pass += 1;
                println!("  [OK]   返回 STATUS_DIRECTORY_NOT_EMPTY（资源管理器会提示「文件夹不为空」）");
            }
            Err(e) => {
                fail += 1;
                println!("  [FAIL] 返回了意外的错误码 {e:#x}");
            }
        }
        fs.do_close(h);

        // 清理
        let h = fs.begin_open(&inner, 0x0000_0007).map_err(|e| anyhow::anyhow!("{e:#x}"))?;
        let _ = fs.do_set_delete(&h, true);
        fs.do_cleanup(&h, true);
        fs.do_close(h);
        let h = fs.begin_open(ne_dir, 0x0000_0007).map_err(|e| anyhow::anyhow!("{e:#x}"))?;
        let _ = fs.do_set_delete(&h, true);
        fs.do_cleanup(&h, true);
        fs.do_close(h);
        if fs.stat(ne_dir).is_none() {
            println!("  · 清理完成");
        }
    }

    // ---------- 6. 从没上传过的文件被删除，不该向服务器发 DELE ----------
    println!("[6] 只在本地、从未上传的文件被删除");
    {
        let h = fs
            .begin_create(ghost, false, 0)
            .map_err(|e| anyhow::anyhow!("create 失败: {e:#x}"))?;
        fs.do_set_delete(&h, true)
            .map_err(|e| anyhow::anyhow!("set_delete 失败: {e:#x}"))?;
        fs.do_cleanup(&h, true);
        fs.do_close(h);
        if fs.stat(ghost).is_none() {
            pass += 1;
            println!("  [OK]   本地取消成功，远端本就没有，没有产生 550");
        } else {
            fail += 1;
            println!("  [FAIL] 竟然还能看到这个文件");
        }
    }

    // ---------- 7. 只读打开按区间读（不整文件下载） ----------
    println!("[7] 只读打开按区间读（REST 续传）");
    {
        let range_file = "\\_rs_fs_range.bin";
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let h = fs
            .begin_create(range_file, false, 0)
            .map_err(|e| anyhow::anyhow!("create 失败: {e:#x}"))?;
        fs.do_write(&h, &payload, 0, false)
            .map_err(|e| anyhow::anyhow!("write 失败: {e:#x}"))?;
        fs.do_cleanup(&h, false);
        fs.do_close(h);

        // 只读打开：只给 FILE_READ_DATA，必须走区间读、不能整文件下载
        let h = fs
            .begin_open(range_file, 0x0000_0001)
            .map_err(|e| anyhow::anyhow!("open 只读失败: {e:#x}"))?;
        let mut buf = vec![0u8; 100];
        match fs.do_read(&h, &mut buf, 1000) {
            Ok(n) if n == 100 && buf[..] == payload[1000..1100] => {
                pass += 1;
                println!("  [OK]   区间 [1000,1100) 内容正确");
            }
            Ok(n) => {
                fail += 1;
                println!("  [FAIL] 区间读返回 {n} 字节，内容不匹配");
            }
            Err(e) => {
                fail += 1;
                println!("  [FAIL] 区间读失败 {e:#x}");
            }
        }
        let mut tail = vec![0u8; 200];
        match fs.do_read(&h, &mut tail, 4000) {
            Ok(n) if n == 96 && tail[..96] == payload[4000..4096] => {
                pass += 1;
                println!("  [OK]   尾部区间 [4000,EOF) 正确（{n} 字节，跨 EOF 截断）");
            }
            Ok(n) => {
                fail += 1;
                println!("  [FAIL] 尾部读返回 {n} 字节（期望 96）");
            }
            Err(e) => {
                fail += 1;
                println!("  [FAIL] 尾部读失败 {e:#x}");
            }
        }
        fs.do_close(h);

        let h = fs
            .begin_open(range_file, 0x0000_0007)
            .map_err(|e| anyhow::anyhow!("{e:#x}"))?;
        let _ = fs.do_set_delete(&h, true);
        fs.do_cleanup(&h, true);
        fs.do_close(h);
    }

    // ---------- 8. 仅本地未上传的文件：删除后列表不能残留（pending 幽灵） ----------
    println!("[8] 仅本地未上传文件删除后不残留");
    {
        let ghost2 = "\\_rs_fs_ghost2.txt";
        let h = fs
            .begin_create(ghost2, false, 0)
            .map_err(|e| anyhow::anyhow!("create 失败: {e:#x}"))?;
        fs.do_write(&h, b"pending-only", 0, false)
            .map_err(|e| anyhow::anyhow!("write 失败: {e:#x}"))?;
        // 关键：故意不调 do_cleanup，模拟 WinFsp 跳过 cleanup 的情况
        match fs.do_set_delete(&h, true) {
            Ok(()) => {
                pass += 1;
                println!("  [OK]   set_delete 成功");
            }
            Err(e) => {
                fail += 1;
                println!("  [FAIL] set_delete 失败 {e:#x}");
            }
        }
        fs.do_close(h);
        let listed = fs
            .list_dir("")
            .map_err(|e| anyhow::anyhow!("list 失败: {e:#x}"))?;
        if !listed
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case("_rs_fs_ghost2.txt"))
        {
            pass += 1;
            println!("  [OK]   列表里没有残留（刷新也不会出现）");
        } else {
            fail += 1;
            println!("  [FAIL] 列表里仍有残留 —— 这就是「删了刷新又出现」");
        }
        if fs.stat(ghost2).is_none() {
            pass += 1;
            println!("  [OK]   stat 找不到该文件");
        } else {
            fail += 1;
            println!("  [FAIL] stat 仍能找到该文件");
        }
    }

    // ---------- 收尾 ----------
    let _ = remote.dele(&remote.abs(ow));
    remote.quit();

    println!();
    println!("通过 {pass} 项，失败 {fail} 项");
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn main() -> Result<()> {
    // 任何子命令都要先加载 winfsp-x64.dll：Handle 里带 DirBuffer，
    // 它的析构会调 FspFileSystemDeleteDirectoryBuffer。延迟加载失败会让进程
    // 直接被终止（没有 panic、没有错误码），fstest 就是这么被坑掉的。
    preload_winfsp();

    // 命令行版：日志同时打 stderr；FT_DEBUG=1 打开详细日志。
    logbuf::set_echo(true);
    logbuf::set_verbose(std::env::var_os("FT_DEBUG").is_some());

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }

    match args[0].as_str() {
        "check" => {
            let path = args.get(1).context("缺少配置文件路径")?;
            let cfg = Config::load(&PathBuf::from(path))?;
            println!("配置: {cfg:#?}");
            let remote = Remote::new(cfg);
            let root = remote.cfg.remote_root();
            match remote.list_mlsd(&root) {
                Ok(v) => println!("MLSD: 支持（{} 项）", v.len()),
                Err(e) => println!("MLSD: 不支持 -> {e}"),
            }
            let entries = remote
                .list(&root)
                .map_err(|e| anyhow::anyhow!("列目录失败: {e}"))?;
            println!("远端 {root} 共 {} 项:", entries.len());
            for e in entries.iter().take(20) {
                println!(
                    "  {:<30} {:>10}  {}{}",
                    e.name,
                    e.size,
                    if e.is_dir { "<DIR> " } else { "" },
                    e.mtime
                );
            }
            remote.quit();
            Ok(())
        }
        "selftest" => {
            let path = args.get(1).context("缺少配置文件路径")?;
            let cfg = Config::load(&PathBuf::from(path))?;
            selftest(cfg)
        }
        "fstest" => {
            let path = args.get(1).context("缺少配置文件路径")?;
            let cfg = Config::load(&PathBuf::from(path))?;
            fstest(cfg)
        }
        "put" => {
            // put <config> <本地文件> <远端绝对路径>
            let path = args.get(1).context("缺少配置文件路径")?;
            let local = args.get(2).context("缺少本地文件路径")?;
            let rp = args.get(3).context("缺少远端路径")?;
            let cfg = Config::load(&PathBuf::from(path))?;
            let remote = Remote::new(cfg);
            let mut f = std::fs::File::open(local)?;
            let n = remote
                .stor(rp, &mut f)
                .map_err(|e| anyhow::anyhow!("STOR 失败: {e}"))?;
            println!("已上传 {rp} -> {n} 字节");
            remote.quit();
            Ok(())
        }
        "mount" => {
            let path = args.get(1).context("缺少配置文件路径")?;
            let cfg = Config::load(&PathBuf::from(path))?;
            let drive = args.get(2).cloned().unwrap_or_else(|| cfg.drive.clone());
            mount(cfg, &drive)
        }
        _ => usage(),
    }
}

fn mount(cfg: Config, drive: &str) -> Result<()> {
    preload_winfsp();
    let _init = winfsp::winfsp_init()
        .map_err(|e| anyhow::anyhow!("WinFsp 初始化失败（是否已安装 WinFsp 2.x？）: {e:?}"))?;

    let remote = Arc::new(Remote::new(cfg.clone()));

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
    // PersistentAcls：WinFsp 用这个标志决定「访问检查时用文件系统返回的描述符」
    // 还是「用 WinFsp 自己的默认描述符」。之前填 false，结果任何带写权限的
    // 「打开已存在文件」都在进回调之前被拒（err=5），而新建文件却正常 ——
    // 因为新建检查的是父目录，打开已有文件检查的是文件本身。
    // 用 FTPMOUNT_ACLS=0/1 可在一次安装里对比两种取值。
    let acls = std::env::var("FTPMOUNT_ACLS")
        .map(|v| v.trim() != "0")
        .unwrap_or(true);
    crate::logbuf::info(&format!("PersistentAcls = {acls}"));

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
        .persistent_acls(acls)
        .file_info_timeout(1000)
        .post_cleanup_when_modified_only(true)
        .flush_and_purge_on_cleanup(true);

    let fs = FtpFs::new(remote);
    // 带定时器的构造：每 400ms 把攒下的目录变更事件推给系统。
    // 不用它的话资源管理器拷完文件收不到通知，要白等约 6.2 秒才把进度推到 100%。
    let mut host =
        FileSystemHost::new_with_timer::<Vec<NotifyEvent>, 400>(
            FileSystemParams::default_params(params),
            fs,
        )
        .map_err(|e| anyhow::anyhow!("创建文件系统失败: {e:?}"))?;

    // 盘符直接交给 WinFsp。网络模式下由 WinFsp.Launcher 服务分配盘符，
    // 不需要 net use。注意这样盘符是 DRIVE_REMOTE，资源管理器会按网络驱动器
    // 处理、每次操作后等约 6 秒 —— 原因和已排除的绕法见 lib.rs::winfsp_net_mode。
    host.mount(drive)
        .map_err(|e| anyhow::anyhow!("挂载到 {drive} 失败: {e:?}"))?;

    FileSystemHost::<FtpFs>::start(&mut host)
        .map_err(|e| anyhow::anyhow!("启动失败: {e:?}"))?;

    println!(
        "已挂载 {drive}  ->  ftp://{}:{}{}",
        cfg.host,
        cfg.port,
        cfg.remote_root()
    );
    println!("结束本进程即卸载。");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
