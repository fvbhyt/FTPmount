//! 站点与设置的持久化。
//!
//! 位置：`%APPDATA%\FtpMount\config.json`
//! 密码落盘前用 DPAPI 加密（见 [`crate::dpapi`]），前端拿到的是明文。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use ftpmount_rs::config::Config;

fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("FtpMount")
}

pub fn log_dir() -> PathBuf {
    app_dir().join("logs")
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

/// 给前端用的站点（密码是明文）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    #[serde(default)]
    pub remote_path: String,
    #[serde(default = "d_drive")]
    pub drive: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub list_tz_offset: i32,
    /// 并发连接数（见核心库 `Config::max_conn`）。不放在界面上，默认 4 即可；
    /// 服务器只允许单连接时会自动收缩。
    #[serde(default = "d_max_conn")]
    pub max_conn: usize,
}

fn d_max_conn() -> usize {
    4
}
fn d_port() -> u16 {
    21
}
fn d_drive() -> String {
    "R:".into()
}

impl Default for Site {
    fn default() -> Self {
        Site {
            id: String::new(),
            name: "新站点".into(),
            host: String::new(),
            port: 21,
            user: String::new(),
            pass: String::new(),
            remote_path: "/".into(),
            drive: "R:".into(),
            tls: false,
            list_tz_offset: 480,
            max_conn: 4,
        }
    }
}

/// 落盘用的站点（密码是 DPAPI 密文）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SiteDisk {
    id: String,
    name: String,
    host: String,
    port: u16,
    user: String,
    #[serde(default)]
    pass_enc: String,
    remote_path: String,
    drive: String,
    tls: bool,
    list_tz_offset: i32,
    #[serde(default = "d_max_conn")]
    max_conn: usize,
}

impl From<&Site> for SiteDisk {
    fn from(s: &Site) -> Self {
        SiteDisk {
            id: s.id.clone(),
            name: s.name.clone(),
            host: s.host.clone(),
            port: s.port,
            user: s.user.clone(),
            pass_enc: crate::dpapi::encrypt(&s.pass),
            remote_path: s.remote_path.clone(),
            drive: s.drive.clone(),
            tls: s.tls,
            list_tz_offset: s.list_tz_offset,
            max_conn: s.max_conn,
        }
    }
}

impl From<SiteDisk> for Site {
    fn from(d: SiteDisk) -> Self {
        Site {
            id: d.id,
            name: d.name,
            host: d.host,
            port: d.port,
            user: d.user,
            pass: crate::dpapi::decrypt(&d.pass_enc),
            remote_path: d.remote_path,
            drive: d.drive,
            tls: d.tls,
            list_tz_offset: d.list_tz_offset,
            max_conn: d.max_conn,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub autostart: bool,
    #[serde(default = "d_true")]
    pub minimize_on_close: bool,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default = "d_theme")]
    pub theme: String,
    /// 启动时自动挂载的站点 id（空 = 不自动挂载）
    #[serde(default)]
    pub auto_mount_id: String,
}

fn d_true() -> bool {
    true
}
fn d_theme() -> String {
    "dark".into()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            autostart: false,
            minimize_on_close: true,
            verbose: false,
            theme: d_theme(),
            auto_mount_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Store {
    #[serde(default)]
    pub sites: Vec<Site>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub active_id: String,
}

/// 磁盘格式
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoreDisk {
    #[serde(default)]
    sites: Vec<SiteDisk>,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    active_id: String,
}

impl Store {
    pub fn load() -> Self {
        let p = config_path();
        let Ok(text) = std::fs::read_to_string(&p) else {
            return Store::default();
        };
        match serde_json::from_str::<StoreDisk>(&text) {
            Ok(d) => Store {
                sites: d.sites.into_iter().map(Site::from).collect(),
                settings: d.settings,
                active_id: d.active_id,
            },
            Err(e) => {
                ftpmount_rs::logbuf::warn(&format!("配置文件解析失败（{}）: {e}", p.display()));
                Store::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = app_dir();
        std::fs::create_dir_all(&dir)?;
        let disk = StoreDisk {
            sites: self.sites.iter().map(SiteDisk::from).collect(),
            settings: self.settings.clone(),
            active_id: self.active_id.clone(),
        };
        let text = serde_json::to_string_pretty(&disk).unwrap_or_else(|_| "{}".into());
        std::fs::write(config_path(), text)
    }

    pub fn find(&self, id: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.id == id)
    }
}

impl Site {
    /// 转成核心库的 Config
    pub fn to_config(&self) -> Config {
        Config {
            host: self.host.trim().to_string(),
            port: self.port,
            user: self.user.clone(),
            pass: self.pass.clone(),
            tls: self.tls,
            no_check_certificate: false,
            remote_path: self.remote_path.clone(),
            drive: self.drive.clone(),
            volname: if self.name.is_empty() {
                "FTP Drive".into()
            } else {
                self.name.clone()
            },
            list_tz_offset: self.list_tz_offset,
            max_conn: self.max_conn,
        }
    }

    /// 给新站点生成 id
    pub fn ensure_id(&mut self) {
        if self.id.is_empty() {
            self.id = format!("s{}", chrono::Local::now().timestamp_millis());
        }
    }
}
