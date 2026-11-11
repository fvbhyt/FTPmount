#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! FtpMount 桌面版入口：托盘常驻 + 单实例 + 开机自启 + 挂载控制。

mod cmds;
mod dpapi;
mod keepawake;
mod mgr;
mod store;

use std::sync::Mutex;

use tauri::menu::{IconMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};

use cmds::AppState;
use ftpmount_rs::logbuf;
use mgr::Mgr;
use store::Store;

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 画一个 16×16 的实心圆点，用作菜单里的状态指示。
///
/// 原生菜单项**改不了文字颜色**（Windows 没这个接口，Tauri 也没暴露），
/// 想让「常亮中」在右键菜单里一眼看出来，只能用图标带颜色。
fn dot(rgb: (u8, u8, u8)) -> tauri::image::Image<'static> {
    const N: u32 = 16;
    let mut buf = vec![0u8; (N * N * 4) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    let r = 6.0f32;
    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt();
            // 边缘 1px 做个简单的抗锯齿，否则小圆看着有锯齿
            let a = if d <= r - 0.5 {
                1.0
            } else if d >= r + 0.5 {
                0.0
            } else {
                r + 0.5 - d
            };
            let i = ((y * N + x) * 4) as usize;
            buf[i] = rgb.0;
            buf[i + 1] = rgb.1;
            buf[i + 2] = rgb.2;
            buf[i + 3] = (a * 255.0) as u8;
        }
    }
    tauri::image::Image::new_owned(buf, N, N)
}

/// 托盘右键菜单。状态变了就整份重建 —— 菜单项的文字/图标没有「就地更新」那么省事，
/// 而状态切换本来就不频繁，重建最省心，也不用到处存句柄。
pub(crate) fn build_tray_menu(
    app: &tauri::AppHandle,
    st: &keepawake::Status,
) -> tauri::Result<Menu<tauri::Wry>> {
    // 菜单里只写进程名，不写「FtpMount · 常亮：」这种前缀。
    // 文字**不随状态变**，靠圆点颜色区分是否正在常亮（红 = 亮着，灰 = 没亮）。
    let text = if st.watching.is_empty() {
        if st.forced {
            "强制常亮".to_string()
        } else {
            "未添加程序".to_string()
        }
    } else {
        st.watching.join("  ")
    };
    let color = if st.active {
        (239, 68, 68)
    } else {
        (139, 151, 174)
    };
    // 状态行做成**可点**的：不可点会被 Windows 画成灰色，看着像故障。
    // 点它 = 打开主窗口并切到「防锁屏」页签，比一个点了没反应的灰条有用。
    let status = IconMenuItem::with_id(
        app,
        "ka-status",
        text,
        true,
        Some(dot(color)),
        None::<&str>,
    )?;
    let sep0 = PredefinedMenuItem::separator(app)?;
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let mount = MenuItem::with_id(app, "mount", "挂载", true, None::<&str>)?;
    let unmount = MenuItem::with_id(app, "unmount", "卸载", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    Menu::with_items(
        app,
        &[&status, &sep0, &show, &mount, &unmount, &sep, &quit],
    )
}

fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let menu = build_tray_menu(app, &keepawake::Status::default())?;

    TrayIconBuilder::with_id("main-tray")
        .icon(
            app.default_window_icon()
                .expect("缺少应用图标")
                .clone(),
        )
        .tooltip("FtpMount")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "ka-status" => {
                show_main(app);
                let _ = app.emit("goto-tab", "awake");
            }
            "show" => show_main(app),
            "mount" => {
                show_main(app);
                let _ = app.emit("tray-mount", ());
            }
            "unmount" => {
                if let Some(st) = app.try_state::<AppState>() {
                    let _ = st.mgr.stop();
                    // 托盘卸载也算"用户主动卸载"，清掉自动挂载记忆
                    let mut s = st.store.lock().unwrap();
                    if !s.settings.auto_mount_id.is_empty() {
                        s.settings.auto_mount_id.clear();
                        let _ = s.save();
                    }
                }
            }
            "quit" => {
                if let Some(st) = app.try_state::<AppState>() {
                    let _ = st.mgr.stop();
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 启动阶段打点，写到 %TEMP%\ftpmount_boot.log。
/// 窗口没出来时，看这个文件就知道卡在哪一步。
fn mark(msg: &str) {
    use std::io::Write;
    let p = std::env::temp_dir().join("ftpmount_boot.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(
            f,
            "{} [{}] {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            std::process::id(),
            msg
        );
    }
}

fn main() {
    mark("main: enter");
    // DirBuffer 析构要调 winfsp-x64.dll，先确保它被加载
    ftpmount_rs::preload_winfsp();
    mark("main: winfsp dll loaded");

    let builder = tauri::Builder::default().plugin(tauri_plugin_single_instance::init(
        |app, _args, _cwd| {
            show_main(app);
        },
    ));
    mark("main: single-instance registered");

    let mut builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        Some(vec!["--minimized"]),
    ));
    mark("main: autostart registered");

    builder = builder
        .setup(|app| {
            mark("setup: enter");
            let s = Store::load();
            mark("setup: store loaded");

            logbuf::set_dir(store::log_dir());
            logbuf::set_verbose(s.settings.verbose);
            logbuf::info(&format!(
                "FtpMount v{} 启动（{}）",
                env!("CARGO_PKG_VERSION"),
                if ftpmount_rs::winfsp_installed() {
                    "WinFsp 已安装"
                } else {
                    "未检测到 WinFsp"
                }
            ));
            mark("setup: log ready");

            // 日志实时推给前端
            let handle = app.handle().clone();
            logbuf::set_sink(Some(Box::new(move |line| {
                let _ = handle.emit("log", line);
            })));

            let mgr = Mgr::new();
            app.manage(AppState {
                mgr: mgr.clone(),
                store: Mutex::new(s.clone()),
            });
            mark("setup: state managed");

            build_tray(app.handle())?;
            mark("setup: tray built");

            // 防锁屏守护线程：每 2 秒看一次列表里的程序在不在，在就保持屏幕常亮
            keepawake::spawn_monitor(app.handle().clone());
            mark("setup: keepawake monitor started");

            let minimized = std::env::args().any(|a| a == "--minimized");
            if !minimized {
                show_main(app.handle());
            }
            mark("setup: done");

            // 开机自启 + 上次挂着没卸：后台静默挂上
            if !s.settings.auto_mount_id.is_empty() {
                if let Some(site) = s.find(&s.settings.auto_mount_id).cloned() {
                    logbuf::info(&format!("自动挂载站点「{}」", site.name));
                    let drive = site.drive.clone();
                    let name = if site.name.is_empty() {
                        site.host.clone()
                    } else {
                        site.name.clone()
                    };
                    std::thread::spawn(move || {
                        // 开机瞬间网络 / DNS 往往还没就绪，只等 3 秒很容易抢跑失败，
                        // 而且以前 `let _ =` 把错误全吞了，界面上就是"没自动挂"。
                        // 现在分档重试，任一次成功就停，最后一次失败才报警。
                        const WAITS: [u64; 5] = [3, 6, 12, 24, 36];
                        for (i, secs) in WAITS.iter().enumerate() {
                            std::thread::sleep(std::time::Duration::from_secs(*secs));
                            match mgr.start(site.to_config(), drive.clone(), name.clone()) {
                                Ok(()) => {
                                    logbuf::info(&format!("自动挂载成功：{name} -> {drive}"));
                                    return;
                                }
                                Err(e) => {
                                    if i + 1 == WAITS.len() {
                                        logbuf::warn(&format!(
                                            "自动挂载失败（已重试 {} 次）：{e}",
                                            WAITS.len()
                                        ));
                                    } else {
                                        logbuf::debug(&format!(
                                            "自动挂载第 {} 次失败，稍后重试：{e}",
                                            i + 1
                                        ));
                                    }
                                }
                            }
                        }
                    });
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle().clone();
                let minimize = app
                    .try_state::<AppState>()
                    .map(|st| st.store.lock().unwrap().settings.minimize_on_close)
                    .unwrap_or(false);

                api.prevent_close();
                if minimize {
                    let _ = window.hide();
                    logbuf::info("窗口已最小化到托盘（托盘右键可退出）");
                } else {
                    // 真正退出：先卸载再退出，避免盘符残留
                    std::thread::spawn(move || {
                        if let Some(st) = app.try_state::<AppState>() {
                            let _ = st.mgr.stop();
                        }
                        app.exit(0);
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            cmds::app_info,
            cmds::load_store,
            cmds::js_probe,
            cmds::save_site,
            cmds::delete_site,
            cmds::set_active,
            cmds::set_settings,
            cmds::check_conn,
            cmds::mount,
            cmds::unmount,
            cmds::status,
            cmds::open_drive,
            cmds::test_rw,
            cmds::logs,
            cmds::clear_logs,
            cmds::open_log_dir,
            cmds::win_minimize,
            cmds::win_toggle_max,
            cmds::win_close,
            cmds::keepawake_view,
            cmds::keepawake_save,
            cmds::keepawake_processes,
        ]);

    mark("main: calling run()");
    builder
        .run(tauri::generate_context!())
        .expect("FtpMount 启动失败");
}
