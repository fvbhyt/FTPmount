//! 大文件读取验证：`cargo run --release --example bigread -- myftp_iso.toml`
//!
//! 复现「拷贝 5GB ISO 报 0x8007045D」的路径：只读惰性打开 + 顺序读 + 跨 4GB 偏移。
//! 只读前 64MB 和 4.5GB 处各 8MB，避免真的下完整个文件。

use std::sync::Arc;
use std::time::Instant;

use ftpmount_rs::config::Config;
use ftpmount_rs::fs::FtpFs;
use ftpmount_rs::ftp::Remote;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg_path = args.first().cloned().unwrap_or_else(|| "myftp_iso.toml".into());
    let name = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "zh-cn_windows_server_2022_updated_sep_2023_x64_dvd_892eeda9.iso".into());

    ftpmount_rs::preload_winfsp();
    let cfg = Config::load(std::path::Path::new(&cfg_path))?;
    let remote = Arc::new(Remote::new(cfg));
    let fs = FtpFs::new(remote);

    let path = format!("\\{name}");
    let st = fs.stat(&path).expect("找不到文件");
    println!("文件 {name} 大小 {} 字节 ({:.2} GB)", st.size, st.size as f64 / 1e9);

    // 0x0000_0001 = FILE_READ_DATA：只读打开，走惰性区间读
    let h = fs.begin_open(&path, 0x0000_0001).expect("打开失败");
    let mut buf = vec![0u8; 1 << 20];

    // 1) 顺序读前 64MB
    let t0 = Instant::now();
    let mut off = 0u64;
    let limit = 64u64 << 20;
    while off < limit {
        let n = fs.do_read(&h, &mut buf, off).expect("顺序读失败") as u64;
        if n == 0 {
            break;
        }
        off += n;
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "顺序读 {:.1} MB 用时 {:.2}s（{:.1} MB/s）",
        off as f64 / 1048576.0,
        dt,
        off as f64 / 1048576.0 / dt
    );

    // 2) 跨 4GB 偏移读 8MB
    let big = 4500u64 << 20;
    if st.size > big + (8 << 20) {
        let t1 = Instant::now();
        let mut got = 0u64;
        while got < (8u64 << 20) {
            let n = fs
                .do_read(&h, &mut buf, big + got)
                .expect("4GB 之后读失败") as u64;
            if n == 0 {
                break;
            }
            got += n;
        }
        println!(
            "4.5GB 处读 {:.1} MB 用时 {:.2}s",
            got as f64 / 1048576.0,
            t1.elapsed().as_secs_f64()
        );
    }

    fs.do_close(h);
    println!("完成");
    Ok(())
}
