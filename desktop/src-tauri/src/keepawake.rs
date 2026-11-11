//! 防锁屏：列表里任意一个「已启用」的程序在运行，就让屏幕保持常亮。
//!
//! 实现只有一个 API：`SetThreadExecutionState`。
//! 它是**线程级**状态 —— 必须由一个活着的线程周期性地重设，
//! 线程退出 / 进程结束就自动失效，不会留下「永久常亮」把屏幕烧着。
//!
//! 挡得住：关闭显示器、进入睡眠（本机 5 分钟关屏就是被这个挡住的）。
//! 挡不住：手动 `Win+L`、组策略「计算机不活动限制」、动态锁 —— 这三样任何工具都做不到。
//!
//! 配置写在 `keepawake.json`：优先放 exe 同级目录（便携 / 免安装场景，方便手改），
//! 目录不可写（例如装在 `Program Files`）时回落到 `%APPDATA%\FtpMount\keepawake.json`。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::Emitter;
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

/// 轮询间隔。2 秒足够：股票软件不会秒开秒关。
const POLL: Duration = Duration::from_secs(2);

// ---------------- 配置 ----------------

fn d_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 进程名，含 `.exe`；比较时忽略大小写
    pub name: String,
    #[serde(default = "d_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// 全局强制常亮：不看列表，直接常亮
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// 运行时状态（不落盘）
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Status {
    pub active: bool,
    pub forced: bool,
    /// 正在触发常亮的进程名（空 = 没有触发）
    pub matched: Vec<String>,
    /// 列表里所有「已启用」的进程名。托盘菜单里固定显示这些名字，
    /// 靠颜色（红/灰）区分是否正在常亮 —— 文字不跟着状态变，一眼就能对比。
    pub watching: Vec<String>,
}

/// 给前端的一次性快照
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub config: Config,
    pub status: Status,
    /// keepawake.json 的实际位置
    pub path: String,
    /// true = 落在 exe 同级目录（方便手改）；false = 回落到 %APPDATA%
    pub beside_exe: bool,
}

// ---------------- 全局实例 ----------------

pub struct KeepAwake {
    path: PathBuf,
    beside_exe: bool,
    cfg: Mutex<Config>,
    st: Mutex<Status>,
}

static KA: OnceLock<KeepAwake> = OnceLock::new();

pub fn get() -> &'static KeepAwake {
    KA.get_or_init(|| {
        let (path, beside_exe) = resolve_path();
        let cfg = read_cfg(&path).unwrap_or_default();
        KeepAwake {
            path,
            beside_exe,
            cfg: Mutex::new(cfg),
            st: Mutex::new(Status::default()),
        }
    })
}

impl KeepAwake {
    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    pub fn status(&self) -> Status {
        self.st.lock().unwrap().clone()
    }

    pub fn view(&self) -> View {
        View {
            config: self.config(),
            status: self.status(),
            path: self.path.display().to_string(),
            beside_exe: self.beside_exe,
        }
    }

    pub fn save(&self, cfg: Config) -> Result<View, String> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(&cfg).map_err(|e| format!("序列化失败: {e}"))?;
        std::fs::write(&self.path, text)
            .map_err(|e| format!("写入 {} 失败: {e}", self.path.display()))?;
        *self.cfg.lock().unwrap() = cfg;
        Ok(self.view())
    }
}

/// 优先 exe 同级目录；写不进去（Program Files）就回落 %APPDATA%。
fn resolve_path() -> (PathBuf, bool) {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("keepawake.json");
            if p.exists() {
                return (p, true);
            }
            // 探一下能不能写：装在 Program Files 时普通权限是写不进去的
            let probe = dir.join(".keepawake_write_probe");
            if std::fs::write(&probe, b"1").is_ok() {
                let _ = std::fs::remove_file(&probe);
                return (p, true);
            }
        }
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    (base.join("FtpMount").join("keepawake.json"), false)
}

fn read_cfg(p: &Path) -> Option<Config> {
    let text = std::fs::read_to_string(p).ok()?;
    match serde_json::from_str::<Config>(&text) {
        Ok(c) => Some(c),
        Err(e) => {
            // 解析失败**不要**退回默认值 —— 那等于用户手改时写错一个逗号，
            // 整个列表就被清空了。保留当前内存里的配置，只报一条警告。
            ftpmount_rs::logbuf::warn(&format!("keepawake.json 解析失败（{}）: {e}", p.display()));
            None
        }
    }
}

fn file_mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

// ---------------- 常亮开关 ----------------

/// 返回 `SetThreadExecutionState` 的返回值。
///
/// 文档：成功返回**上一次**的状态，失败返回 0。所以 0 有两种含义
/// （失败，或上次本来就没设），单看它判不了失败 —— 但它至少能把
/// 「调用压根没生效」这类情况暴露出来，比自己吞掉强。
fn apply(on: bool) -> u32 {
    let flags = if on {
        ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    unsafe { SetThreadExecutionState(flags).0 }
}

// ---------------- 进程快照 ----------------

#[derive(Debug, Clone, Serialize)]
pub struct ProcItem {
    pub name: String,
    /// 该进程的第一个可见窗口标题（可能为空）
    pub title: String,
    /// 同名进程数量（同花顺这类会开好几个）
    pub count: u32,
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

/// pid -> (exe 名, 窗口标题)
fn snapshot() -> Vec<(u32, String, String)> {
    let mut list: Vec<(u32, String)> = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        if snap.is_invalid() {
            return Vec::new();
        }
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let name = wide_to_string(&e.szExeFile);
                if !name.is_empty() {
                    list.push((e.th32ProcessID, name));
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }

    let titles = window_titles();
    list.into_iter()
        .map(|(pid, name)| {
            let t = titles.get(&pid).cloned().unwrap_or_default();
            (pid, name, t)
        })
        .collect()
}

fn window_titles() -> BTreeMap<u32, String> {
    let mut map: BTreeMap<u32, String> = BTreeMap::new();
    unsafe {
        let _ = EnumWindows(Some(enum_win), LPARAM(&mut map as *mut _ as isize));
    }
    map
}

unsafe extern "system" fn enum_win(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let map = &mut *(lparam.0 as *mut BTreeMap<u32, String>);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return BOOL(1);
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n <= 0 {
        return BOOL(1);
    }
    let mut pid = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != 0 {
        let t = String::from_utf16_lossy(&buf[..n as usize]);
        if !t.trim().is_empty() {
            map.entry(pid).or_insert(t);
        }
    }
    BOOL(1)
}

fn running_set() -> HashSet<String> {
    snapshot()
        .into_iter()
        .map(|(_, n, _)| n.to_lowercase())
        .collect()
}

/// 给「从运行中挑」用的列表：按 exe 名去重，带窗口标题。
///
/// 只按进程名挑是不够的 —— 同花顺这类软件主程序和 helper 是分开的，
/// 挑到 `hexinhelper.exe` 会出现「主程序关了盘还亮着」，所以必须把窗口标题带出来。
pub fn running_for_picker() -> Vec<ProcItem> {
    let mut by_name: BTreeMap<String, ProcItem> = BTreeMap::new();
    for (_, name, title) in snapshot() {
        let key = name.to_lowercase();
        let item = by_name.entry(key).or_insert_with(|| ProcItem {
            name: name.clone(),
            title: String::new(),
            count: 0,
        });
        item.count += 1;
        if item.title.is_empty() && !title.trim().is_empty() {
            item.title = title;
        }
    }
    let mut v: Vec<ProcItem> = by_name.into_values().collect();
    // 有窗口标题的排前面（那才是用户认得的「主界面」），同组内按名字排
    v.sort_by(|a, b| {
        b.title
            .is_empty()
            .cmp(&a.title.is_empty())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    v
}

// ---------------- 守护线程 ----------------

pub fn spawn_monitor(app: tauri::AppHandle) {
    let _ = std::thread::Builder::new()
        .name("keepawake".into())
        .spawn(move || {
            let ka = get();
            let mut last: Option<Status> = None;
            let mut last_mtime = file_mtime(&ka.path);

            loop {
                std::thread::sleep(POLL);

                // 用户手改 keepawake.json 时不用重启：mtime 变了就重读一次。
                // 这样「普通可读，方便你自己改」才真的成立。
                let m = file_mtime(&ka.path);
                if m != last_mtime {
                    last_mtime = m;
                    if let Some(c) = read_cfg(&ka.path) {
                        *ka.cfg.lock().unwrap() = c;
                        ftpmount_rs::logbuf::info("keepawake.json 有改动，已重新载入");
                    }
                }

                let cfg = ka.config();
                // 列表全停用且没强制时，连进程都懒得枚举
                let need = cfg.force || cfg.entries.iter().any(|e| e.enabled);
                let running = if need { running_set() } else { HashSet::new() };

                let matched: Vec<String> = if cfg.force {
                    Vec::new()
                } else {
                    cfg.entries
                        .iter()
                        .filter(|e| e.enabled && running.contains(&e.name.trim().to_lowercase()))
                        .map(|e| e.name.clone())
                        .collect()
                };
                let want = cfg.force || !matched.is_empty();

                // 每一轮都重设一次：ES_CONTINUOUS 是线程级状态，靠重复调用来续命
                let prev = apply(want);

                let watching: Vec<String> = cfg
                    .entries
                    .iter()
                    .filter(|e| e.enabled)
                    .map(|e| e.name.clone())
                    .collect();

                let st = Status {
                    active: want,
                    forced: cfg.force,
                    matched,
                    watching,
                };
                if last.as_ref() != Some(&st) {
                    *ka.st.lock().unwrap() = st.clone();
                    update_tray(&app, &st);
                    let _ = app.emit("keepawake", st.clone());
                    ftpmount_rs::logbuf::info(&format!(
                        "防锁屏 {}：{}（SetThreadExecutionState 返回 {prev:#x}）",
                        if st.active { "开启" } else { "关闭" },
                        if st.forced {
                            "强制常亮".to_string()
                        } else if st.matched.is_empty() {
                            "列表内程序均未运行".to_string()
                        } else {
                            st.matched.join(", ")
                        }
                    ));
                    last = Some(st);
                }
            }
        });
}

/// 托盘右键菜单里的状态行，实时显示「现在是谁在触发」。
/// 鼠标悬停的提示保持原样（只写 FtpMount），不往里面塞状态文字。
fn update_tray(app: &tauri::AppHandle, st: &Status) {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };

    // 右键菜单里的状态行（红点 = 常亮中，灰点 = 未触发）
    match crate::build_tray_menu(app, st) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                ftpmount_rs::logbuf::warn(&format!("重建托盘菜单失败: {e}"));
            }
        }
        Err(e) => ftpmount_rs::logbuf::warn(&format!("重建托盘菜单失败: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程枚举必须能看到自己，否则「监控程序在不在」这件事从根上就不成立
    #[test]
    fn snapshot_sees_self() {
        let me = std::env::current_exe().unwrap();
        let name = me
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        let set = running_set();
        assert!(set.contains(&name), "进程快照里没看到自己: {name}");
    }

    /// 下拉列表要按 exe 名去重，并且能带出窗口标题
    #[test]
    fn picker_dedupes_and_has_titles() {
        let v = running_for_picker();
        assert!(!v.is_empty(), "一个进程都没枚举到");

        let mut names: Vec<String> = v.iter().map(|p| p.name.to_lowercase()).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "同名进程没有合并成一条");

        let with_title = v.iter().filter(|p| !p.title.is_empty()).count();
        println!("枚举 {} 个 exe，其中 {} 个带窗口标题", v.len(), with_title);
        // 有界面的机器上至少 explorer 之类会带标题；无界面环境允许为 0
        assert!(with_title > 0, "一个窗口标题都没读到，下拉框会失去意义");
    }

    /// 常亮开关开/关都不能 panic（真正生效与否要看电源设置）
    #[test]
    fn exec_state_toggles() {
        apply(true);
        apply(false);
    }
}
