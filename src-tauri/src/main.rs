#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod config;
mod recorder;
mod win;

use config::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;

struct State {
    cfg: Mutex<Config>,
    cfg_path: PathBuf,
    muted: Arc<AtomicBool>,
    tx: Mutex<Sender<recorder::Cmd>>,
    status: MenuItem<Wry>,
}

fn st(app: &AppHandle) -> tauri::State<'_, State> {
    app.state::<State>()
}

pub fn config_snapshot(app: &AppHandle) -> Config {
    st(app).cfg.lock().unwrap().clone()
}

fn save_config(app: &AppHandle, cfg: &Config) {
    let s = st(app);
    let _ = std::fs::write(&s.cfg_path, serde_json::to_string_pretty(cfg).unwrap());
    *s.cfg.lock().unwrap() = cfg.clone();
}

pub fn notify(app: &AppHandle, msg: &str) {
    let _ = app.notification().builder().title("Aegis").body(msg).show();
}

pub fn set_status(app: &AppHandle, game: Option<&str>) {
    let text = match game {
        Some(g) => format!("● 正在录制：{g}"),
        None => "空闲：等待游戏启动".into(),
    };
    let _ = st(app).status.set_text(&text);
    if let Some(t) = app.tray_by_id("main") {
        let _ = t.set_tooltip(Some(format!("Aegis - {text}")));
    }
}

pub fn add_pending(app: &AppHandle, path: PathBuf) {
    let mut cfg = config_snapshot(app);
    cfg.pending.push(path);
    save_config(app, &cfg);
    show_window(app, "save");
}

fn show_window(app: &AppHandle, label: &str) {
    let app = app.clone();
    let label = label.to_string();
    // 可能在后台线程调用；窗口要在主线程建
    let _ = app.clone().run_on_main_thread(move || {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.emit_to(&label, "refresh", ());
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
            return;
        }
        let (title, w, h, top) = if label == "save" { ("保存录像", 480.0, 360.0, true) } else { ("Aegis 设置", 760.0, 720.0, false) };
        let _ = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(format!("{label}.html").into()))
            .title(title)
            .inner_size(w, h)
            .always_on_top(top)
            .center()
            .build();
    });
}

use tauri::Emitter;

// ---------- 前端命令 ----------

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    config_snapshot(&app)
}

#[tauri::command]
fn set_config(app: AppHandle, cfg: Config) -> Result<(), String> {
    let al = app.autolaunch();
    if cfg.autostart { al.enable() } else { al.disable() }.map_err(|e| e.to_string())?;
    let mut cfg = cfg;
    cfg.pending = config_snapshot(&app).pending; // 待命名列表只由后端维护
    save_config(&app, &cfg);
    Ok(())
}

#[tauri::command]
fn preview_name(template: String, game: String) -> String {
    config::render_name(&template, &game, chrono::Local::now()) + ".mp4"
}

#[derive(serde::Serialize)]
struct RunningWindow {
    exe: String,
    title: String,
}

#[tauri::command]
fn list_windows() -> Vec<RunningWindow> {
    let procs = win::processes();
    let mut out: Vec<RunningWindow> = win::windows()
        .into_iter()
        .filter(|w| w.pid != std::process::id())
        .filter_map(|w| {
            let exe = procs.iter().find(|p| p.0 == w.pid)?.1.clone();
            Some(RunningWindow { exe, title: w.title })
        })
        .collect();
    out.sort_by(|a, b| a.exe.to_lowercase().cmp(&b.exe.to_lowercase()));
    out.dedup_by(|a, b| a.exe.eq_ignore_ascii_case(&b.exe));
    out
}

#[tauri::command]
fn list_mics() -> Vec<(String, String)> {
    win::list_mics()
}

#[tauri::command]
fn get_pending(app: AppHandle) -> Vec<PathBuf> {
    let mut cfg = config_snapshot(&app);
    let n = cfg.pending.len();
    cfg.pending.retain(|p| p.exists()); // 用户在外面删掉的就不再问
    if cfg.pending.len() != n {
        save_config(&app, &cfg);
    }
    cfg.pending
}

/// name = Some(新名字) 保存；None 移到回收站。返回最终路径。
#[tauri::command]
fn resolve_pending(app: AppHandle, path: PathBuf, name: Option<String>) -> Result<Option<PathBuf>, String> {
    let mut result = None;
    match name {
        Some(n) => {
            let stem = config::sanitize(&n);
            let cur = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            let dest = if cur.as_deref() == Some(stem.as_str()) {
                path.clone()
            } else {
                recorder::unique_path(path.parent().unwrap(), &stem)
            };
            if dest != path {
                std::fs::rename(&path, &dest).map_err(|e| format!("改名失败：{e}"))?;
            }
            result = Some(dest);
        }
        None => {
            if !win::recycle(&path) {
                return Err("移到回收站失败".into());
            }
        }
    }
    let mut cfg = config_snapshot(&app);
    cfg.pending.retain(|p| p != &path);
    save_config(&app, &cfg);
    Ok(result)
}

#[tauri::command]
fn open_folder(app: AppHandle) {
    let dir = config_snapshot(&app).out_dir;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

// ---------- 启动 ----------

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_window(app, "settings")))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_config, set_config, preview_name, list_windows, list_mics, get_pending, resolve_pending, open_folder
        ])
        .setup(|app| {
            let h = app.handle().clone();
            let dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&dir)?;
            let cfg_path = dir.join("config.json");
            let first_run = !cfg_path.exists();
            let cfg = std::fs::read_to_string(&cfg_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| Config::new(app.path().video_dir().unwrap_or_default().join("Aegis")));
            if first_run && cfg.autostart {
                let _ = app.autolaunch().enable();
            }

            let status = MenuItem::with_id(app, "status", "空闲：等待游戏启动", false, None::<&str>)?;
            let mute = CheckMenuItem::with_id(app, "mute", "麦克风静音", true, false, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &status,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "stop", "结束录制并保存", true, None::<&str>)?,
                    &MenuItem::with_id(app, "skip", "跳过本次（不保存）", true, None::<&str>)?,
                    &mute,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "folder", "打开视频目录", true, None::<&str>)?,
                    &MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?,
                    &MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
                ],
            )?;
            let (tx, rx) = channel();
            let muted = Arc::new(AtomicBool::new(false));
            app.manage(State { cfg: Mutex::new(cfg), cfg_path, muted: muted.clone(), tx: Mutex::new(tx), status });

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Aegis - 空闲：等待游戏启动")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|t, e| {
                    if let tauri::tray::TrayIconEvent::DoubleClick { .. } = e {
                        show_window(t.app_handle(), "settings");
                    }
                })
                .on_menu_event(move |app, e| {
                    let s = st(app);
                    match e.id.as_ref() {
                        "stop" => drop(s.tx.lock().unwrap().send(recorder::Cmd::Stop)),
                        "skip" => drop(s.tx.lock().unwrap().send(recorder::Cmd::Skip)),
                        "mute" => s.muted.store(mute.is_checked().unwrap_or(false), Relaxed),
                        "folder" => open_folder(app.clone()),
                        "settings" => show_window(app, "settings"),
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .build(app)?;

            let h2 = h.clone();
            std::thread::spawn(move || {
                recorder::recover(&h2);
                recorder::run(h2, rx, muted);
            });

            if !std::env::args().any(|a| a == "--autostart") {
                show_window(&h, "settings");
            }
            if !config_snapshot(&h).pending.is_empty() {
                show_window(&h, "save");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("tauri build")
        .run(|_, e| {
            // 关掉所有窗口时留在托盘
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = e {
                api.prevent_exit();
            }
        });
}
