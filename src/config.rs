use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub no_check_certificate: bool,
    /// 远端起始目录，空表示根
    #[serde(default)]
    pub remote_path: String,
    #[serde(default = "d_drive")]
    pub drive: String,
    #[serde(default = "d_volname")]
    pub volname: String,
    /// LIST 时间修正（分钟）。服务器只有 LIST、没有 MLSD/MDTM 时，
    /// LIST 返回的是服务器本地时间，这里填与 UTC 的差值，例如北京填 480。
    #[serde(default)]
    pub list_tz_offset: i32,
    /// 并发连接数上限。
    ///
    /// 必须 > 1：只开一条连接时，大文件传输会把它占住，期间的目录枚举 / 新建 /
    /// 取属性全部排队直到超时，表现为「位置不可用，由于 I/O 设备错误」。
    /// 服务器若只允许单连接会回 421，客户端会自动收缩，填大了也不会坏。
    #[serde(default = "d_max_conn")]
    pub max_conn: usize,
}

fn d_port() -> u16 {
    21
}
fn d_max_conn() -> usize {
    4
}
fn d_drive() -> String {
    "R:".into()
}
fn d_volname() -> String {
    "FTP Drive".into()
}

impl Config {
    /// 兼容现有 ftpmount.conf 的 `[ftp]` 段落写法，也接受顶层直写。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取配置 {} 失败: {e}", path.display()))?;
        let root: toml::Value = toml::from_str(&text)?;
        let table = match root.get("ftp") {
            Some(v) => v.clone(),
            None => root,
        };
        let cfg: Config = table.try_into()?;
        Ok(cfg)
    }

    pub fn remote_root(&self) -> String {
        let p = self.remote_path.trim();
        if p.is_empty() || p == "/" {
            "/".to_string()
        } else if p.starts_with('/') {
            p.trim_end_matches('/').to_string()
        } else {
            format!("/{}", p.trim_end_matches('/'))
        }
    }
}
