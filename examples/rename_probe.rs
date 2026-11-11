//! 最小复现：用与 ftpmount 完全相同的 suppaftp 调用验证 RNFR/RNTO。
//!
//! 从标准输入读取 host / port / user / password / from / to（每行一个），
//! 避免密码出现在命令行参数里。

use std::io::{self, BufRead};

use suppaftp::FtpStream;

fn main() -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut next = || lines.next().unwrap_or_else(|| Ok(String::new())).unwrap_or_default();

    let host = next();
    let port: u16 = next().parse()?;
    let user = next();
    let pass = next();
    let from = next();
    let to = next();

    println!("连接 {host}:{port} 用户 {user}");
    let mut ftp = FtpStream::connect((host.as_str(), port))?;
    ftp.login(&user, &pass)?;
    println!("登录成功");
    let cwd = ftp.pwd()?;
    println!("登录后 CWD = {cwd}");

    println!("RNFR {from} -> RNTO {to}");
    match ftp.rename(&from, &to) {
        Ok(()) => println!("  成功"),
        Err(e) => println!("  失败: {e}"),
    }

    // 再试一次：显式 CWD 到目标目录，用相对路径改名
    println!("改成相对路径再试（先 CWD 到父目录）");
    if let Some((parent, leaf)) = from.rsplit_once('/') {
        let dst_leaf = to.rsplit_once('/').map(|(_, l)| l).unwrap_or(&to);
        println!("  CWD {parent}");
        match ftp.cwd(parent) {
            Ok(()) => println!("  CWD ok, pwd={}", ftp.pwd()?),
            Err(e) => println!("  CWD 失败: {e}"),
        }
        match ftp.rename(leaf, dst_leaf) {
            Ok(()) => println!("  相对路径成功"),
            Err(e) => println!("  相对路径失败: {e}"),
        }
        // 改回去
        let _ = ftp.rename(dst_leaf, leaf);
    }

    let _ = ftp.quit();
    Ok(())
}
