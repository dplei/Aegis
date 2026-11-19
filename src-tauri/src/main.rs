#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod config;
mod recorder;
mod win;

use config::Config;
use recorder::{ActiveChoice, ChoiceAction, ChoiceOutcome, RecordingStatus};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, sync_channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent, Wry};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;

struct State {
    cfg: Mutex<Config>,
    cfg_path: PathBuf,
    muted: Arc<AtomicBool>,
    tx: Mutex<Sender<recorder::Cmd>>,
    status: MenuItem<Wry>,
    /// 正在实际录制（录游戏或录桌面）：托盘图标闪红点；暂停等待选择时不闪红
    recording: Arc<AtomicBool>,
    /// 当前录制阶段，给设置页、托盘与保存弹窗同步展示
    rec_status: Mutex<RecordingStatus>,
    /// 当前暂停等待选择的会话上下文；首次有效操作或关窗后原子消费，避免重复触发
    active_choice: Mutex<Option<ActiveChoice>>,
    /// 控制保存弹窗 1.5s 兜底显示与隐藏之间的代际，避免隐藏后被旧定时任务重新弹出
    save_show_gen: Arc<AtomicU64>,
}

fn st(app: &AppHandle) -> tauri::State<'_, State> {
    app.state::<State>()
}

pub fn config_snapshot(app: &AppHandle) -> Config {
    st(app).cfg.lock().unwrap().clone()
}

fn save_config(app: &AppHandle, cfg: &Config) {
    let s = st(app);
    if let Err(e) = std::fs::write(&s.cfg_path, serde_json::to_string_pretty(cfg).unwrap()) {
        log::error!("写配置失败 {}：{e}", s.cfg_path.display());
    }
    *s.cfg.lock().unwrap() = cfg.clone();
}

pub fn notify(app: &AppHandle, msg: &str) {
    log::warn!("通知：{msg}"); // 通知可能被系统收起或屏蔽，日志里总留一份
    toast(app, msg);
}

/// 只弹系统通知、不记日志：给「开始录制」这类调用方自己记过 info 的提示
pub fn toast(app: &AppHandle, msg: &str) {
    let _ = app.notification().builder().title("Aegis").body(msg).show();
}

pub fn set_recording_dir(app: &AppHandle, dir: Option<PathBuf>) {
    let mut cfg = config_snapshot(app);
    if cfg.recording != dir {
        cfg.recording = dir;
        save_config(app, &cfg);
    }
}

fn format_tray_status(status: &RecordingStatus) -> String {
    match status {
        RecordingStatus::Idle => "空闲：等待游戏启动".into(),
        RecordingStatus::Game { game } => format!("● 正在录制游戏：{game}"),
        RecordingStatus::WaitingChoice { game } => format!("❚❚ 已暂停，等待选择（{game} 已退出）"),
        RecordingStatus::Desktop { from_game, .. } => format!("● 正在录制桌面（{from_game} 所在屏幕）"),
    }
}

pub fn set_status(app: &AppHandle, status: RecordingStatus) {
    let s = st(app);
    s.recording.store(status.is_actively_recording(), Relaxed);
    *s.rec_status.lock().unwrap() = status.clone();
    let _ = app.emit_to("settings", "recording_status", &status);
    let text = format_tray_status(&status);
    let _ = s.status.set_text(&text);
    if let Some(t) = app.tray_by_id("main") {
        let _ = t.set_tooltip(Some(format!("Aegis - {text}")));
    }
}

pub fn set_active_choice(app: &AppHandle, choice: Option<ActiveChoice>) {
    *st(app).active_choice.lock().unwrap() = choice;
}

pub fn clear_active_choice(app: &AppHandle, session_id: Option<u64>) {
    let s = st(app);
    let mut guard = s.active_choice.lock().unwrap();
    if session_id.is_none() || guard.as_ref().is_some_and(|c| Some(c.session_id) == session_id) {
        *guard = None;
    }
}

fn consume_matching_choice(slot: &mut Option<ActiveChoice>, session_id: u64, choice_id: u64) -> Option<ActiveChoice> {
    if slot
        .as_ref()
        .is_some_and(|c| c.session_id == session_id && c.choice_id == choice_id)
    {
        slot.take()
    } else {
        None
    }
}

fn should_show_save(app: &AppHandle) -> bool {
    let s = st(app);
    if s.active_choice.lock().unwrap().is_some() {
        return true;
    }
    if matches!(*s.rec_status.lock().unwrap(), RecordingStatus::Desktop { .. }) {
        return false;
    }
    config_snapshot(app).pending.iter().any(|p| p.exists())
}

pub fn show_save_window(app: &AppHandle) {
    show_window(app, "save");
}

pub fn add_pending(app: &AppHandle, path: PathBuf) {
    let mut cfg = config_snapshot(app);
    cfg.pending.push(path);
    save_config(app, &cfg);
    show_window(app, "save");
}

/// 隐藏保存弹窗并同步确认其不可见（含 DWM 合成等待），用于恢复桌面录制前避免把弹窗录进去。
pub fn hide_save_window_sync(app: &AppHandle) -> bool {
    st(app).save_show_gen.fetch_add(1, Relaxed);
    let query_and_hide = || -> Option<bool> {
        let (tx, rx) = sync_channel(1);
        let app2 = app.clone();
        if app
            .run_on_main_thread(move || {
                let hidden = if let Some(w) = app2.get_webview_window("save") {
                    let _ = w.hide();
                    !w.is_visible().unwrap_or(true)
                } else {
                    true
                };
                let _ = tx.send(hidden);
            })
            .is_err()
        {
            return None;
        }
        rx.recv_timeout(Duration::from_secs(3)).ok()
    };

    for _ in 0..20 {
        match query_and_hide() {
            Some(true) => {
                win::dwm_flush();
                win::dwm_flush();
                std::thread::sleep(Duration::from_millis(40));
                return true;
            }
            Some(false) => std::thread::sleep(Duration::from_millis(25)),
            None => return false,
        }
    }
    false
}

/// 暂停等待选择被外部终止（如托盘跳过）时，同步刷新旧待命名列表或隐藏已空弹窗。
pub fn refresh_or_hide_save_window(app: &AppHandle) {
    let has_pending = config_snapshot(app).pending.iter().any(|p| p.exists());
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(w) = app2.get_webview_window("save") else { return };
        if !w.is_visible().unwrap_or(false) {
            return;
        }
        if has_pending {
            let _ = w.emit_to("save", "refresh", ());
        } else {
            st(&app2).save_show_gen.fetch_add(1, Relaxed);
            let _ = w.hide();
        }
    });
}

fn handle_save_window_closed(app: &AppHandle) {
    let s = st(app);
    s.save_show_gen.fetch_add(1, Relaxed);
    let choice = s.active_choice.lock().unwrap().take();
    if let Some(c) = choice {
        let _ = s.tx.lock().unwrap().send(recorder::Cmd::Choice {
            session_id: c.session_id,
            choice_id: c.choice_id,
            action: ChoiceAction::CloseDefault,
            reply: None,
        });
    }
}

fn spawn_reveal_fallback(app: &AppHandle, label: &str, save_gen: Option<u64>) {
    let app2 = app.clone();
    let label2 = label.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        let app3 = app2.clone();
        let _ = app2.run_on_main_thread(move || {
            if let Some(gen) = save_gen {
                if st(&app3).save_show_gen.load(Relaxed) != gen || !should_show_save(&app3) {
                    return;
                }
            }
            if let Some(w) = app3.get_webview_window(&label2) {
                if !w.is_visible().unwrap_or(true) {
                    log::warn!("{label2} 页面 1.5 秒内没画完，兜底显示");
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        });
    });
}

fn show_window(app: &AppHandle, label: &str) {
    let app = app.clone();
    let label = label.to_string();
    let save_gen = (label == "save").then(|| st(&app).save_show_gen.fetch_add(1, Relaxed) + 1);
    // 可能在后台线程调用；窗口要在主线程建
    let _ = app.clone().run_on_main_thread(move || {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.emit_to(&label, "refresh", ());
            if label == "save" && !w.is_visible().unwrap_or(false) {
                // 已隐藏复用时等页面刷新完成后由 reveal_save 显示，避免先闪出一帧旧内容
                spawn_reveal_fallback(&app, &label, save_gen);
                return;
            }
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
            return;
        }
        let (title, w, h, top) = if label == "save" { ("保存录像", 480.0, 410.0, true) } else { ("Aegis 设置", 760.0, 720.0, false) };
        // 先隐藏着建：新建 WebView 要约 0.4 秒才画出页面，直接显示会先黑一下。页面渲染完自己调 reveal() 显示；
        // 页面脚本出错没调到时，1.5 秒后兜底显示
        spawn_reveal_fallback(&app, &label, save_gen);
        let built = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(format!("{label}.html").into()))
            .visible(false)
            .title(title)
            .inner_size(w, h)
            .always_on_top(top)
            // 不要系统标题栏，顶栏在页面里自己画；底色和页面一致，避免打开时白闪
            .decorations(false)
            .shadow(true)
            .background_color(tauri::window::Color(10, 10, 10, 255))
            .min_inner_size(if top { w } else { 640.0 }, if top { h } else { 480.0 })
            .resizable(!top)
            // 系统的文件拖放处理会吞掉页面里的 HTML5 拖放（游戏列表拖动排序要用）
            .disable_drag_drop_handler()
            .center()
            .build();
        if let Ok(win) = built {
            if label == "save" {
                let app_close = app.clone();
                win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { .. } = event {
                        handle_save_window_closed(&app_close);
                    }
                });
            }
        }
    });
}

use tauri::Emitter;

// ---------- 前端命令 ----------

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    config_snapshot(&app)
}

#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Autostart {
    Off,
    Registry,
    StartupFolder,
}

/// 开机自启：先写注册表 Run；被拦（多半是火绒、360 这类安全软件）就退到「启动」文件夹放快捷方式。
/// reg / lnk 分别开关两种方式，返回实际生效的那种。
fn apply_autostart_with(
    on: bool,
    reg: impl FnOnce(bool) -> Result<(), String>,
    lnk: impl Fn(bool) -> Result<(), String>,
) -> Result<Autostart, String> {
    if !on {
        let r = reg(false);
        lnk(false)?;
        return r.map(|_| Autostart::Off);
    }
    match reg(true) {
        Ok(()) => {
            let _ = lnk(false);
            Ok(Autostart::Registry)
        }
        Err(e) => {
            log::warn!("写注册表自启失败：{e}，改用启动文件夹");
            lnk(true).map(|_| Autostart::StartupFolder).map_err(|e2| format!("注册表：{e}；启动文件夹：{e2}"))
        }
    }
}

fn apply_autostart(app: &AppHandle, on: bool) -> Result<Autostart, String> {
    let al = app.autolaunch();
    let r = apply_autostart_with(
        on,
        |on| if on { al.enable() } else { al.disable() }.map_err(|e| e.to_string()),
        |on| win::set_startup_lnk(on, "--autostart"),
    );
    match &r {
        Ok(m) => log::info!("开机自启：{m:?}"),
        Err(e) => log::error!("开机自启设置失败（on={on}）：{e}"),
    }
    r
}

/// 自启设置失败不挡其他设置：先照常保存，自启没开成就把开关记回关闭，再把原因报给前端。
#[tauri::command]
fn set_config(app: AppHandle, cfg: Config) -> Result<(), String> {
    let mut cfg = cfg;
    let old = config_snapshot(&app);
    cfg.pending = old.pending; // 待命名列表、录制状态只由后端维护
    cfg.recording = old.recording;
    let mut err = None;
    if cfg.autostart != old.autostart {
        if let Err(e) = apply_autostart(&app, cfg.autostart) {
            cfg.autostart = autostart_status(app.clone()) != Autostart::Off;
            err = Some(format!("开机自启设置失败，其他设置已保存。请在火绒、360 等安全软件里放行 Aegis 后重试。（{e}）"));
        }
    }
    save_config(&app, &cfg);
    err.map_or(Ok(()), Err)
}

/// 当前实际生效的自启方式（注册表值可能被安全软件事后删掉，所以每次现查）
#[tauri::command]
fn autostart_status(app: AppHandle) -> Autostart {
    if app.autolaunch().is_enabled().unwrap_or(false) {
        Autostart::Registry
    } else if win::startup_lnk().exists() {
        Autostart::StartupFolder
    } else {
        Autostart::Off
    }
}

/// 日志、配置和系统信息打成 zip 放到桌面，并在资源管理器里选中它。返回 zip 路径。
#[tauri::command]
async fn export_logs(app: AppHandle) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let p = app.path();
    let logs = p.app_log_dir().map_err(|e| e.to_string())?;
    let zip = p.desktop_dir().map_err(|e| e.to_string())?.join(format!("Aegis-logs-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let info = format!(
        "Aegis {}\nexe: {}\n配置里的自启开关: {}\n实际自启方式: {:?}\n",
        app.package_info().version,
        std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default(),
        config_snapshot(&app).autostart,
        autostart_status(app.clone()),
    );
    let script = std::env::temp_dir().join("aegis-export-logs.ps1");
    std::fs::write(&script, include_str!("export-logs.ps1")).map_err(|e| e.to_string())?;
    log::info!("导出日志到 {}", zip.display());
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .env("AEGIS_LOGS", &logs)
        .env("AEGIS_CONF", &st(&app).cfg_path)
        .env("AEGIS_ZIP", &zip)
        .env("AEGIS_INFO", info)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("导出失败：{e}"))?;
    let _ = std::fs::remove_file(&script);
    if !out.status.success() || !zip.exists() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        log::error!("导出日志失败：{e}");
        return Err(format!("导出失败：{e}"));
    }
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", zip.display())).spawn();
    Ok(zip.display().to_string())
}

#[tauri::command]
fn recording_status(app: AppHandle) -> RecordingStatus {
    st(&app).rec_status.lock().unwrap().clone()
}

#[derive(serde::Serialize)]
struct SaveDialogState {
    active: Option<ActiveChoice>,
    pending: Vec<PathBuf>,
}

#[tauri::command]
fn get_save_state(app: AppHandle) -> SaveDialogState {
    let active = st(&app).active_choice.lock().unwrap().clone();
    let pending = get_pending(app);
    SaveDialogState { active, pending }
}

/// 仅在确实处于等待选择或（非桌面录制且有待命名条目）时显示保存弹窗，防止过期渲染把弹窗重新带出。
#[tauri::command]
fn reveal_save(app: AppHandle) {
    if !should_show_save(&app) {
        return;
    }
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window("save") {
            if !w.is_visible().unwrap_or(false) {
                let _ = w.show();
            }
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    });
}

#[tauri::command]
async fn resolve_choice(
    app: AppHandle,
    session_id: u64,
    choice_id: u64,
    action: String,
    name: Option<String>,
) -> Result<ChoiceOutcome, String> {
    let parsed = match action.as_str() {
        "continue" => ChoiceAction::Continue,
        "save" => ChoiceAction::Save(name.unwrap_or_default()),
        "discard" => ChoiceAction::Discard,
        _ => return Err("不支持的操作".into()),
    };
    let consumed = {
        let s = st(&app);
        let mut guard = s.active_choice.lock().unwrap();
        consume_matching_choice(&mut guard, session_id, choice_id)
    };
    if consumed.is_none() {
        return Ok(ChoiceOutcome::Stale);
    }
    let (reply_tx, reply_rx) = sync_channel(1);
    st(&app)
        .tx
        .lock()
        .unwrap()
        .send(recorder::Cmd::Choice {
            session_id,
            choice_id,
            action: parsed,
            reply: Some(reply_tx),
        })
        .map_err(|_| "录制线程不可用".to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        reply_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "等待录制线程处理超时".to_string())?
    })
    .await
    .map_err(|e| format!("处理操作失败：{e}"))?
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
    let procs = win::processes().unwrap_or_default();
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
                std::fs::rename(&path, &dest).map_err(|e| {
                    log::error!("改名失败 {} -> {}：{e}", path.display(), dest.display());
                    format!("改名失败：{e}")
                })?;
            }
            result = Some(dest);
        }
        None => {
            if !win::recycle(&path) {
                log::error!("移到回收站失败：{}", path.display());
                return Err("移到回收站失败".into());
            }
        }
    }
    let mut cfg = config_snapshot(&app);
    cfg.pending.retain(|p| p != &path);
    save_config(&app, &cfg);
    Ok(result)
}

#[derive(serde::Serialize)]
struct DirCheck {
    exists: bool,
    /// 所在磁盘剩余空间（GB）
    free_gb: f64,
    /// Some = 不能用，原因
    error: Option<String>,
}

/// 检查保存目录能不能用：绝对路径、不是文件、可写；不存在时只报告，交给前端问要不要新建。
#[tauri::command]
fn check_dir(path: String) -> DirCheck {
    let p = PathBuf::from(path.trim());
    let disk = p.ancestors().find(|a| a.exists()).unwrap_or(&p);
    let free_gb = win::disk_free(disk) as f64 / (1u64 << 30) as f64;
    let fail = |e: &str| DirCheck { exists: p.exists(), free_gb, error: Some(e.into()) };
    if !p.is_absolute() {
        return fail("请填写完整路径，比如 D:\\Videos\\Aegis");
    }
    if p.exists() && !p.is_dir() {
        return fail("这是一个文件，不是文件夹");
    }
    if !p.exists() {
        return DirCheck { exists: false, free_gb, error: None };
    }
    let probe = p.join(".aegis-write-test");
    if let Err(e) = std::fs::write(&probe, b"") {
        log::warn!("保存目录不可写 {}：{e}", p.display());
        return fail("这个文件夹没有写入权限");
    }
    let _ = std::fs::remove_file(probe);
    DirCheck { exists: true, free_gb, error: None }
}

#[tauri::command]
fn make_dir(path: String) -> Result<(), String> {
    std::fs::create_dir_all(path.trim()).map_err(|e| {
        log::warn!("新建目录失败 {path}：{e}");
        format!("新建失败：{e}")
    })
}

/// Windows 原生的文件夹选择窗口，从当前目录（或它最近的已存在上级）开始。
#[tauri::command]
async fn pick_dir(app: AppHandle, current: String) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let start = PathBuf::from(current.trim());
    let mut d = app.dialog().file().set_title("选择保存目录");
    if let Some(dir) = start.ancestors().find(|a| a.is_dir()) {
        d = d.set_directory(dir);
    }
    if let Some(w) = app.get_webview_window("settings") {
        d = d.set_parent(&w);
    }
    d.blocking_pick_folder()?.into_path().ok().map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn open_folder(app: AppHandle) {
    let dir = config_snapshot(&app).out_dir;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

// ---------- 启动 ----------

fn main() {
    use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
    // release 版没有控制台，panic 默认输出会丢，记进日志
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        log::error!("panic: {p}");
        default_hook(p);
    }));
    tauri::Builder::default()
        // 写到 %LOCALAPPDATA%\app.aegis.recorder\logs\Aegis.log，满 2MB 轮转、留 3 份
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .target(Target::new(TargetKind::LogDir { file_name: None }))
                .level(log::LevelFilter::Info)
                .max_file_size(2_000_000)
                .rotation_strategy(RotationStrategy::KeepSome(3))
                .timezone_strategy(TimezoneStrategy::UseLocal)
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_window(app, "settings")))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_config, set_config, preview_name, list_windows, list_mics, get_pending, resolve_pending,
            get_save_state, reveal_save, resolve_choice, open_folder, check_dir, make_dir, pick_dir,
            autostart_status, export_logs, recording_status
        ])
        .setup(|app| {
            let h = app.handle().clone();
            log::info!("Aegis {} 启动，参数 {:?}", app.package_info().version, std::env::args().skip(1).collect::<Vec<_>>());
            let dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&dir)?;
            let cfg_path = dir.join("config.json");
            let cfg = std::fs::read_to_string(&cfg_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| Config::new(app.path().video_dir().unwrap_or_default().join("Aegis")));
            // 卸载会删掉注册表 Run 值但配置还留着，安全软件也可能事后删：两种方式都没了就补上。
            // 只看「有没有」不看「是否启用」：用户在任务管理器里禁用的不去覆盖
            if cfg.autostart && !win::run_value_exists("Aegis") && !win::startup_lnk().exists() {
                let _ = apply_autostart(&h, true);
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
            let recording = Arc::new(AtomicBool::new(false));
            app.manage(State {
                cfg: Mutex::new(cfg),
                cfg_path,
                muted: muted.clone(),
                tx: Mutex::new(tx),
                status,
                recording: recording.clone(),
                rec_status: Mutex::new(RecordingStatus::Idle),
                active_choice: Mutex::new(None),
                save_show_gen: Arc::new(AtomicU64::new(0)),
            });
            blink_tray(app.handle().clone(), recording);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn autostart_fallback() {
        let denied = |_: bool| Err::<(), _>("拒绝访问。 (os error -2147024891)".to_string());
        let calls = RefCell::new(vec![]);
        let lnk = |on: bool| {
            calls.borrow_mut().push(on);
            Ok(())
        };
        // 注册表被拦：退到启动文件夹
        assert_eq!(apply_autostart_with(true, denied, lnk), Ok(Autostart::StartupFolder));
        // 注册表能写：用注册表，并清掉可能残留的快捷方式，免得启动两次
        assert_eq!(apply_autostart_with(true, |_| Ok(()), lnk), Ok(Autostart::Registry));
        assert_eq!(*calls.borrow(), [true, false]);
        // 两条路都不通：报错
        assert!(apply_autostart_with(true, denied, |_| Err("拒绝访问".into())).is_err());
        // 关闭：两边都清
        assert_eq!(apply_autostart_with(false, |_| Ok(()), lnk), Ok(Autostart::Off));
    }

    #[test]
    fn tray_status_and_single_consume_choice() {
        assert_eq!(format_tray_status(&RecordingStatus::Idle), "空闲：等待游戏启动");
        assert!(format_tray_status(&RecordingStatus::Game { game: "LOL".into() }).contains("正在录制游戏"));
        assert!(format_tray_status(&RecordingStatus::WaitingChoice { game: "LOL".into() }).contains("已暂停，等待选择"));
        let desk = format_tray_status(&RecordingStatus::Desktop {
            from_game: "LOL".into(),
            monitor_name: r"\\.\DISPLAY2".into(),
        });
        assert!(desk.contains("正在录制桌面"));
        assert!(desk.contains("LOL"));
        assert!(!RecordingStatus::WaitingChoice { game: "LOL".into() }.is_actively_recording());
        assert!(RecordingStatus::Desktop { from_game: "LOL".into(), monitor_name: "".into() }.is_actively_recording());

        let mut slot = Some(ActiveChoice {
            session_id: 3,
            choice_id: 7,
            first_game: "LOL".into(),
            last_game: "Apex".into(),
            default_name: "LOL_2026-11-19 15-30-00".into(),
            out_dir: PathBuf::from(r"D:\Videos"),
            monitor_name: Some(r"\\.\DISPLAY1".into()),
        });
        // 不匹配 session_id / choice_id 时不消费
        assert!(consume_matching_choice(&mut slot, 2, 7).is_none());
        assert!(consume_matching_choice(&mut slot, 3, 6).is_none());
        assert!(slot.is_some());
        // 首次匹配消费成功，重复提交被拒绝
        assert!(consume_matching_choice(&mut slot, 3, 7).is_some());
        assert!(consume_matching_choice(&mut slot, 3, 7).is_none());
    }
}

/// 录制时托盘图标的红点每 0.6 秒亮灭一次；停止后换回原来的图标。
fn blink_tray(app: AppHandle, recording: Arc<AtomicBool>) {
    use tauri::image::Image;
    let on = Image::from_bytes(include_bytes!("../icons/tray-rec-on.png")).expect("tray icon");
    let off = Image::from_bytes(include_bytes!("../icons/tray-rec-off.png")).expect("tray icon");
    let idle = app.default_window_icon().map(|i| i.clone().to_owned());
    std::thread::spawn(move || {
        let (mut was, mut lit) = (false, false);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(600));
            let now = recording.load(Relaxed);
            if !now && !was {
                continue;
            }
            let Some(tray) = app.tray_by_id("main") else { continue };
            lit = now && !lit;
            let icon = if !now { idle.clone() } else if lit { Some(on.clone()) } else { Some(off.clone()) };
            let _ = tray.set_icon(icon);
            was = now;
        }
    });
}
