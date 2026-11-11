//! 前端能调用的命令。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

use ftpmount_rs::ftp::Remote;
use ftpmount_rs::logbuf::{self, Line};

use crate::mgr::{Mgr, Status};
use crate::store::{Settings, Site, Store};

pub struct AppState {
    pub mgr: Arc<Mgr>,
    pub store: Mutex<Store>,
}

// ---------------- 关于 ----------------

#[derive(Serialize)]
pub struct AppInfo {
    pub version: String,
    pub build_time: String,
    pub exe: String,
    pub winfsp: bool,
    pub os: String,
}

#[tauri::command]
pub fn app_info() -> AppInfo {
    let exe = std::env::current_exe().unwrap_or_default();
    let build_time = std::fs::metadata(&exe)
        .and_then(|m| m.modified())
        .map(|t| {
            let d: chrono::DateTime<chrono::Local> = t.into();
            d.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_else(|_| "—".into());
    // consts::OS 在 Windows 上返回小写 "windows"，直接用会出现 "Windows windows"
    let os_name = match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        other => other,
    };
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_time,
        exe: exe.display().to_string(),
        winfsp: ftpmount_rs::winfsp_installed(),
        os: format!("{os_name} ({})", std::env::consts::ARCH),
    }
}

// ---------------- 站点 / 设置 ----------------

#[tauri::command]
pub fn load_store(state: State<AppState>) -> Store {
    // 这行日志同时是个探针：日志里出现它，说明前端 JS 跑起来且 IPC 通了。
    logbuf::info("界面已连接");
    state.store.lock().unwrap().clone()
}

/// 前端上报视口尺寸，用于排查高 DPI 下 CSS 像素与窗口物理像素不一致导致的排版溢出。
#[tauri::command]
pub fn js_probe(app: AppHandle, w: u32, h: u32, dpr: f64) {
    let mut s = format!("界面视口 {w}×{h} CSS px，devicePixelRatio={dpr}");
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(sf) = win.scale_factor() {
            s.push_str(&format!("，窗口 scale={sf}"));
        }
        if let Ok(sz) = win.inner_size() {
            s.push_str(&format!("，内尺寸 {}×{} 物理 px", sz.width, sz.height));
        }
    }
    logbuf::info(&s);
}

#[tauri::command]
pub fn save_site(state: State<AppState>, mut site: Site) -> Result<Store, String> {
    site.ensure_id();
    let mut s = state.store.lock().unwrap();
    match s.sites.iter_mut().find(|x| x.id == site.id) {
        Some(old) => *old = site.clone(),
        None => s.sites.push(site.clone()),
    }
    s.active_id = site.id.clone();
    s.save().map_err(|e| format!("保存失败: {e}"))?;
    Ok(s.clone())
}

#[tauri::command]
pub fn delete_site(state: State<AppState>, id: String) -> Result<Store, String> {
    let mut s = state.store.lock().unwrap();
    s.sites.retain(|x| x.id != id);
    if s.active_id == id {
        s.active_id = s.sites.first().map(|x| x.id.clone()).unwrap_or_default();
    }
    if s.settings.auto_mount_id == id {
        s.settings.auto_mount_id.clear();
    }
    s.save().map_err(|e| format!("保存失败: {e}"))?;
    Ok(s.clone())
}

#[tauri::command]
pub fn set_active(state: State<AppState>, id: String) -> Result<Store, String> {
    let mut s = state.store.lock().unwrap();
    s.active_id = id;
    s.save().map_err(|e| format!("保存失败: {e}"))?;
    Ok(s.clone())
}

#[tauri::command]
pub fn set_settings(
    app: AppHandle,
    state: State<AppState>,
    settings: Settings,
) -> Result<Store, String> {
    logbuf::set_verbose(settings.verbose);

    // 开机自启：插件写的是 HKCU\...\Run，不需要管理员权限
    let al = app.autolaunch();
    let enabled = al.is_enabled().unwrap_or(false);
    if settings.autostart && !enabled {
        al.enable().map_err(|e| format!("设置开机自启失败: {e}"))?;
    } else if !settings.autostart && enabled {
        al.disable().map_err(|e| format!("取消开机自启失败: {e}"))?;
    }

    let mut s = state.store.lock().unwrap();
    s.settings = settings;
    s.save().map_err(|e| format!("保存失败: {e}"))?;
    Ok(s.clone())
}

// ---------------- 连接检测 ----------------

#[derive(Serialize, Default)]
pub struct ConnReport {
    pub ok: bool,
    pub ms: u64,
    pub root: String,
    pub items: usize,
    pub mlsd: bool,
    pub writable: bool,
    pub msg: String,
}

#[tauri::command]
pub async fn check_conn(site: Site) -> ConnReport {
    tauri::async_runtime::spawn_blocking(move || do_check(site))
        .await
        .unwrap_or_else(|e| ConnReport {
            msg: format!("检测线程异常: {e}"),
            ..Default::default()
        })
}

fn do_check(site: Site) -> ConnReport {
    let mut rep = ConnReport::default();
    if site.host.trim().is_empty() {
        rep.msg = "服务器地址为空".into();
        return rep;
    }
    let cfg = site.to_config();
    rep.root = cfg.remote_root();
    let t0 = Instant::now();
    let remote = Remote::new(cfg);
    match remote.list(&rep.root) {
        Ok(v) => {
            rep.ms = t0.elapsed().as_millis() as u64;
            rep.ok = true;
            rep.items = v.len();
            rep.mlsd = remote.list_mlsd(&rep.root).is_ok();

            // 写权限：建一个临时目录再删掉
            let probe = format!("{}/_ftpmount_probe", rep.root.trim_end_matches('/'));
            rep.writable = remote.mkd(&probe).is_ok() && remote.rmd(&probe).is_ok();
            rep.msg = format!("连接成功，{} 项", v.len());
            logbuf::info(&format!(
                "检测 {}:{} 成功，{} 项，用时 {}ms，可写={}",
                site.host, site.port, v.len(), rep.ms, rep.writable
            ));
        }
        Err(e) => {
            rep.ms = t0.elapsed().as_millis() as u64;
            rep.msg = format!("连接失败: {e}");
            logbuf::warn(&format!("检测 {}:{} 失败: {e}", site.host, site.port));
        }
    }
    remote.quit();
    rep
}

// ---------------- 挂载 / 卸载 ----------------

#[tauri::command]
pub fn mount(state: State<AppState>, site: Site, drive: String) -> Result<(), String> {
    if site.host.trim().is_empty() {
        return Err("请先填写服务器地址".into());
    }
    let drive = if drive.trim().is_empty() {
        site.drive.clone()
    } else {
        drive
    };
    let cfg = site.to_config();
    let name = if site.name.is_empty() {
        site.host.clone()
    } else {
        site.name.clone()
    };
    state.mgr.start(cfg, drive, name)?;

    // 挂载成功就把这个站点记下来：下次启动（尤其是开机自启）自动挂上。
    // 以前这个字段只有"清空"没有"赋值"，所以自动挂载的分支永远进不去。
    if !site.id.is_empty() {
        let mut s = state.store.lock().unwrap();
        if s.settings.auto_mount_id != site.id {
            s.settings.auto_mount_id = site.id.clone();
            let _ = s.save();
        }
    }
    Ok(())
}

#[tauri::command]
pub fn unmount(state: State<AppState>) -> Result<(), String> {
    state.mgr.stop()?;
    // 用户主动点卸载 = 不想再自动挂，把记忆清掉，免得重启后又自己挂上。
    // 注意：程序退出走的是 Mgr::stop 而不是这个命令，不会误清。
    let mut s = state.store.lock().unwrap();
    if !s.settings.auto_mount_id.is_empty() {
        s.settings.auto_mount_id.clear();
        let _ = s.save();
    }
    Ok(())
}

#[tauri::command]
pub fn status(state: State<AppState>) -> Status {
    state.mgr.status()
}

#[tauri::command]
pub fn open_drive(drive: String) -> Result<(), String> {
    let d = if drive.ends_with('\\') {
        drive.clone()
    } else {
        format!("{drive}\\")
    };
    std::process::Command::new("explorer.exe")
        .arg(&d)
        .spawn()
        .map_err(|e| format!("打开 {d} 失败: {e}"))?;
    Ok(())
}

// ---------------- 读写测试 ----------------

#[derive(Serialize, Default)]
pub struct RwReport {
    pub ok: bool,
    pub write_ms: u64,
    pub read_ms: u64,
    pub bytes: usize,
    pub msg: String,
}

#[tauri::command]
pub async fn test_rw(state: State<'_, AppState>) -> Result<RwReport, String> {
    let remote = state
        .mgr
        .remote()
        .ok_or_else(|| "还没有挂载，先挂载再测".to_string())?;
    tauri::async_runtime::spawn_blocking(move || do_rw(remote))
        .await
        .map_err(|e| format!("测试线程异常: {e}"))
}

fn do_rw(remote: Arc<Remote>) -> RwReport {
    let mut rep = RwReport::default();
    let root = remote.cfg.remote_root();
    let path = format!("{}/_ftpmount_rwtest.txt", root.trim_end_matches('/'));
    let payload = format!(
        "ftpmount 读写测试 {}\n这是一段用来验证上传/下载往返的中文内容。\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    let payload_bytes = payload.as_bytes();

    // 写
    let tmp = std::env::temp_dir().join("ftpmount_rwtest.txt");
    if let Err(e) = std::fs::write(&tmp, payload_bytes) {
        rep.msg = format!("写临时文件失败: {e}");
        return rep;
    }
    let Ok(mut f) = std::fs::File::open(&tmp) else {
        rep.msg = "打开临时文件失败".into();
        return rep;
    };
    let t0 = Instant::now();
    match remote.stor(&path, &mut f) {
        Ok(n) => {
            rep.write_ms = t0.elapsed().as_millis() as u64;
            rep.bytes = n as usize;
        }
        Err(e) => {
            rep.msg = format!("上传失败: {e}");
            logbuf::warn(&format!("读写测试上传失败: {e}"));
            return rep;
        }
    }
    drop(f);

    // 读回
    let back = std::env::temp_dir().join("ftpmount_rwtest_back.txt");
    let t1 = Instant::now();
    let read_res = std::fs::File::create(&back)
        .map_err(|e| e.to_string())
        .and_then(|mut out| remote.retr_to_file(&path, &mut out).map_err(|e| e.to_string()));
    rep.read_ms = t1.elapsed().as_millis() as u64;
    match read_res {
        Ok(_) => {
            let got = std::fs::read(&back).unwrap_or_default();
            if got == payload_bytes {
                rep.ok = true;
                rep.msg = format!(
                    "读写正常：上传 {} 字节 / {} ms，下载 {} ms",
                    rep.bytes, rep.write_ms, rep.read_ms
                );
                logbuf::info(&rep.msg.clone());
            } else {
                rep.msg = format!(
                    "内容不一致：写 {} 字节，读回 {} 字节",
                    payload_bytes.len(),
                    got.len()
                );
                logbuf::warn(&rep.msg.clone());
            }
        }
        Err(e) => {
            rep.msg = format!("下载失败: {e}");
            logbuf::warn(&format!("读写测试下载失败: {e}"));
        }
    }

    // 清理
    let _ = remote.dele(&path);
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&back);
    rep
}

// ---------------- 日志 ----------------

#[tauri::command]
pub fn logs() -> Vec<Line> {
    logbuf::snapshot()
}

#[tauri::command]
pub fn clear_logs() {
    logbuf::clear();
}

#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    let dir = crate::store::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .map_err(|e| format!("打开日志目录失败: {e}"))?;
    Ok(())
}

// ---------------- 防锁屏 ----------------

#[tauri::command]
pub fn keepawake_view() -> crate::keepawake::View {
    crate::keepawake::get().view()
}

#[tauri::command]
pub fn keepawake_save(cfg: crate::keepawake::Config) -> Result<crate::keepawake::View, String> {
    crate::keepawake::get().save(cfg)
}

/// 「从正在运行的程序里挑」用的列表（带窗口标题，避免挑到 helper 进程）
#[tauri::command]
pub fn keepawake_processes() -> Vec<crate::keepawake::ProcItem> {
    crate::keepawake::running_for_picker()
}

// ---------------- 窗口控制 ----------------
#[tauri::command]
pub fn win_minimize(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.minimize();
    }
}

#[tauri::command]
pub fn win_toggle_max(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
    }
}

#[tauri::command]
pub fn win_close(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}
