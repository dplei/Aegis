//! 每秒轮询一次：发现游戏 → 开会话；目标窗口变了 → 让同一个捕获源换窗口；进程全没了 → 停止并按默认名保存。
//! 画面、声音、编码、音画同步都交给 libobs：窗口用 WGC 抓（不注入游戏进程），
//! 声音用捕获源自带的「应用音频捕获」跟着当前窗口走，编码用 NVENC HEVC 纹理编码（显存零拷贝）。
use crate::config::{bitrate_kbps, render_name, Config, Game, Scope};
use crate::win;
use chrono::{DateTime, Local, TimeZone};
use libobs_simple::output::simple::{HardwareCodec, ObsContextSimpleExt, OutputFormat};
use libobs_simple::sources::windows::{
    MonitorCaptureSourceBuilder, ObsWindowCaptureMethod, WindowCaptureSourceBuilder,
};
use libobs_wrapper::context::ObsContext;
use libobs_wrapper::data::output::{ObsOutputRef, ObsOutputTrait};
use libobs_wrapper::data::video::ObsVideoInfoBuilder;
use libobs_wrapper::data::object::ObsObjectTrait;
use libobs_wrapper::data::{ObsData, ObsObjectUpdater};
use libobs_wrapper::scenes::{ObsSceneRef, SceneItemTrait};
use libobs_wrapper::sources::{ObsSourceAudioExt, ObsSourceBuilder, ObsSourceRef};
use libobs_wrapper::utils::{ObsPath, StartupInfo};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tauri::AppHandle;
use windows::Win32::Foundation::HWND;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const MIN_FREE_START: u64 = 5 << 30;
const MIN_FREE_RUN: u64 = 1 << 30;
const REC_FILE: &str = "rec.mp4";

pub enum Cmd {
    /// 提前结束，照常弹保存
    Stop,
    /// 丢弃这次录制
    Skip,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    Win(isize),
    Mon(isize),
}

/// 当前画面来源：录窗口时一个窗口捕获（自带应用音频）；录全屏时一个显示器捕获 + 一个应用音频捕获。
enum Capture {
    Win(libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::WindowCaptureSource>),
    Mon(
        libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::monitor_capture::MonitorCaptureSource>,
        libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>,
    ),
}

struct Session {
    game: Game,
    start: DateTime<Local>,
    dir: PathBuf,
    output: ObsOutputRef,
    cap: Option<(Target, Capture)>,
    mic: Option<libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>>,
    mic_muted: bool,
}

pub fn tmp_root(cfg: &Config) -> PathBuf {
    cfg.out_dir.join(".aegis-tmp")
}

fn game_pids(game: &Game, procs: &[(u32, String)]) -> Vec<(u32, String)> {
    procs
        .iter()
        .filter(|(_, exe)| game.exes.iter().any(|e| e.eq_ignore_ascii_case(exe)))
        .cloned()
        .collect()
}

/// 按 exes 的优先级找窗口：排前面的进程有窗口就录它，同一进程取面积最大的。返回 (目标, 窗口)。
fn pick(game: &Game, scope: Scope, pids: &[(u32, String)]) -> Option<(Target, isize)> {
    let wins = win::windows();
    for exe in &game.exes {
        let ids: Vec<u32> = pids.iter().filter(|(_, e)| e.eq_ignore_ascii_case(exe)).map(|p| p.0).collect();
        if let Some(w) = wins.iter().filter(|w| ids.contains(&w.pid)).max_by_key(|w| w.area) {
            let t = match scope {
                Scope::Window => Target::Win(w.hwnd),
                Scope::Monitor => Target::Mon(win::monitor_of(w.hwnd).0),
            };
            return Some((t, w.hwnd));
        }
    }
    None
}

struct Obs {
    ctx: ObsContext,
    scene: ObsSceneRef,
    canvas: (u32, u32),
}

fn video_info((w, h): (u32, u32)) -> libobs_wrapper::data::video::ObsVideoInfo {
    ObsVideoInfoBuilder::new()
        .base_width(w)
        .base_height(h)
        .output_width(w)
        .output_height(h)
        .fps_num(60)
        .fps_den(1)
        .build()
}

impl Obs {
    /// conf_dir：OBS 各插件写配置的地方；不设的话会写进当前工作目录
    fn start(conf_dir: &Path) -> Res<Obs> {
        let canvas = (1920, 1080);
        let conf = ObsPath::new(&format!("{}/", conf_dir.join("obs").to_string_lossy()));
        let mut ctx = StartupInfo::new().set_video_info(video_info(canvas)).set_module_config_path(conf).start()?;
        let scene = ctx.scene("main", Some(0))?;
        Ok(Obs { ctx, scene, canvas })
    }

    fn window_info(hwnd: isize) -> Res<libobs_window_helper::WindowInfo> {
        Ok(libobs_window_helper::get_window_info(HWND(hwnd as _))?)
    }

    /// 让捕获对准 target；同类目标只改设置，不重建源。
    fn aim(&mut self, cur: &mut Option<(Target, Capture)>, target: Target, hwnd: isize) -> Res<()> {
        let info = Self::window_info(hwnd)?;
        match (cur.as_mut(), target) {
            (Some((t, Capture::Win(item))), Target::Win(_)) => {
                item.inner_source_mut().create_updater()?.set_window(&info).update()?;
                item.fit_source_to_screen()?;
                *t = target;
            }
            (Some((t, Capture::Mon(item, audio))), Target::Mon(m)) => {
                let mon = monitor(m)?;
                item.inner_source_mut().create_updater()?.set_monitor(&mon).update()?;
                item.fit_source_to_screen()?;
                let mut d = ObsData::new(self.ctx.runtime().clone())?;
                d.bulk_update().set_string("window", info.obs_id.as_str()).apply()?;
                audio.inner_source().update_settings(d)?;
                *t = target;
            }
            _ => {
                self.drop_capture(cur);
                let cap = match target {
                    Target::Win(_) => {
                        let item = self
                            .ctx
                            .source_builder::<WindowCaptureSourceBuilder, _>("game")?
                            .set_capture_method(ObsWindowCaptureMethod::MethodWgc)
                            .set_window(&info)
                            .set_client_area(true)
                            .set_capture_audio(true)?
                            .add_to_scene(&mut self.scene)?;
                        item.fit_source_to_screen()?;
                        Capture::Win(item)
                    }
                    Target::Mon(m) => {
                        let item = self
                            .ctx
                            .source_builder::<MonitorCaptureSourceBuilder, _>("screen")?
                            .set_monitor(&monitor(m)?)
                            .add_to_scene(&mut self.scene)?;
                        item.fit_source_to_screen()?;
                        let audio = self.add_raw("wasapi_process_output_capture", "game-audio", &[("window", &info.obs_id)])?;
                        Capture::Mon(item, audio)
                    }
                };
                *cur = Some((target, cap));
            }
        }
        Ok(())
    }

    fn drop_capture(&mut self, cur: &mut Option<(Target, Capture)>) {
        match cur.take() {
            Some((_, Capture::Win(i))) => drop(i.remove_from_scene()),
            Some((_, Capture::Mon(i, a))) => {
                let _ = i.remove_from_scene();
                let _ = a.remove_from_scene();
            }
            None => {}
        }
    }

    /// libobs-simple 没有封装的源（麦克风、应用音频），按类型 id + 字符串设置创建并加进场景。
    fn add_raw(&mut self, kind: &str, name: &str, settings: &[(&str, &str)]) -> Res<libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>> {
        let st = self.ctx.source_type(kind)?.ok_or_else(|| format!("OBS 里没有 {kind} 这种源"))?;
        let mut d = ObsData::new(self.ctx.runtime().clone())?;
        let mut u = d.bulk_update();
        for (k, v) in settings {
            u = u.set_string(*k, *v);
        }
        u.apply()?;
        let src = self.ctx.create_source(&st, name, Some(d))?;
        Ok(self.scene.add(src)?)
    }

    fn begin(&mut self, cfg: &Config, game: &Game, hwnd: isize, dir: &Path) -> Res<(ObsOutputRef, Option<libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>>)> {
        let (_, mw, mh) = win::monitor_of(hwnd);
        let canvas = cfg.canvas((mw, mh));
        if canvas != self.canvas {
            self.ctx.reset_video(video_info(canvas))?;
            self.canvas = canvas;
        }
        let mic = if cfg.mic_enabled {
            // OBS 的 "default" 和系统设置里的默认录音设备不一定是同一个，自己解析成具体 id
            let id = cfg.mic_device.clone().or_else(win::default_mic).unwrap_or_else(|| "default".into());
            Some(self.add_raw("wasapi_input_capture", "mic", &[("device_id", &id)])?)
        } else {
            None
        };
        let output = self
            .ctx
            .simple_output_builder(format!("rec-{}", game.name), ObsPath::new(&dir.join(REC_FILE).to_string_lossy()))
            .video_bitrate(bitrate_kbps(canvas.0, canvas.1))
            .audio_bitrate(160)
            .prefer_hardware_encoder(HardwareCodec::HEVC)
            // 分片 MP4：程序崩溃或断电时，已录的部分照样能播
            .format(OutputFormat::FragmentedMP4)
            .build()?;
        Ok((output.into(), mic))
    }
}

fn monitor(hmon: isize) -> Res<display_info::DisplayInfo> {
    MonitorCaptureSourceBuilder::get_monitors()?
        .into_iter()
        .find(|m| m.raw_handle.0 as isize == hmon)
        .ok_or_else(|| "找不到游戏所在的显示器".into())
}

impl Session {
    fn tick(&mut self, obs: &mut Obs, cfg: &Config, pids: &[(u32, String)], muted: bool) {
        if let Some(mic) = &self.mic {
            if self.mic_muted != muted && mic.inner_source().set_muted(muted).is_ok() {
                self.mic_muted = muted;
            }
        }
        let scope = self.game.scope.unwrap_or(cfg.scope);
        let Some((target, hwnd)) = pick(&self.game, scope, pids) else { return };
        if self.cap.as_ref().is_some_and(|(t, _)| *t == target) {
            return;
        }
        if let Err(e) = obs.aim(&mut self.cap, target, hwnd) {
            eprintln!("切换捕获目标失败：{e}");
        }
    }

    fn end(mut self, obs: &mut Obs) -> PathBuf {
        let _ = self.output.stop();
        obs.drop_capture(&mut self.cap);
        if let Some(m) = self.mic.take() {
            let _ = m.remove_from_scene();
        }
        self.dir
    }
}

pub fn run(app: AppHandle, rx: Receiver<Cmd>, muted: Arc<AtomicBool>) {
    let conf_dir = tauri::Manager::path(&app).app_config_dir().unwrap_or_default();
    let mut obs = match Obs::start(&conf_dir) {
        Ok(o) => o,
        Err(e) => {
            crate::notify(&app, &format!("录制内核启动失败，无法录制：{e}"));
            return;
        }
    };
    let mut sess: Option<Session> = None;
    let mut suppressed: Option<String> = None; // 手动结束/跳过/空间不足后，等游戏退出前不再自动录
    loop {
        thread::sleep(Duration::from_secs(1));
        let cfg = crate::config_snapshot(&app);
        let procs = win::processes();

        if let Some(s) = &mut sess {
            let pids = game_pids(&s.game, &procs);
            let mut end = None;
            if let Ok(cmd) = rx.try_recv() {
                end = Some(matches!(cmd, Cmd::Stop));
                suppressed = Some(s.game.name.clone());
            } else if pids.is_empty() {
                end = Some(true);
            } else if win::disk_free(&cfg.out_dir) < MIN_FREE_RUN {
                crate::notify(&app, "硬盘空间不足 1GB，已停止录制并保存已录部分");
                suppressed = Some(s.game.name.clone());
                end = Some(true);
            }
            match end {
                None => s.tick(&mut obs, &cfg, &pids, muted.load(Relaxed)),
                Some(keep) => {
                    let s = sess.take().unwrap();
                    let (game, start) = (s.game.name.clone(), s.start);
                    let dir = s.end(&mut obs);
                    crate::set_status(&app, None);
                    if keep {
                        finalize(&app, &dir, &game, start);
                    } else {
                        let _ = fs::remove_dir_all(&dir);
                    }
                }
            }
            continue;
        }

        while rx.try_recv().is_ok() {} // 没在录时的托盘命令忽略
        if let Some(name) = &suppressed {
            let still = cfg.games.iter().any(|g| &g.name == name && !game_pids(g, &procs).is_empty());
            if !still {
                suppressed = None;
            }
        }
        let Some(game) = cfg
            .games
            .iter()
            .find(|g| g.enabled && Some(&g.name) != suppressed.as_ref() && !game_pids(g, &procs).is_empty())
        else {
            continue;
        };
        // 等游戏有了窗口再开始（进程刚起来时往往还没窗口）
        let scope = game.scope.unwrap_or(cfg.scope);
        let Some((target, hwnd)) = pick(game, scope, &game_pids(game, &procs)) else { continue };
        let _ = fs::create_dir_all(&cfg.out_dir);
        if win::disk_free(&cfg.out_dir) < MIN_FREE_START {
            crate::notify(&app, &format!("硬盘剩余空间不足 5GB，本次不录制 {}", game.name));
            suppressed = Some(game.name.clone());
            continue;
        }
        let start = Local::now();
        let dir = tmp_root(&cfg).join(start.timestamp().to_string());
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("game.txt"), &game.name);
        let mut cap = None;
        let began = obs.aim(&mut cap, target, hwnd).and_then(|_| obs.begin(&cfg, game, hwnd, &dir));
        let (output, mic) = match began {
            Ok(v) => v,
            Err(e) => {
                obs.drop_capture(&mut cap);
                crate::notify(&app, &format!("开始录制 {} 失败：{e}", game.name));
                suppressed = Some(game.name.clone());
                let _ = fs::remove_dir_all(&dir);
                continue;
            }
        };
        if let Err(e) = output.start() {
            obs.drop_capture(&mut cap);
            crate::notify(&app, &format!("开始录制 {} 失败：{e}", game.name));
            suppressed = Some(game.name.clone());
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        crate::set_status(&app, Some(&game.name));
        sess = Some(Session { game: game.clone(), start, dir, output, cap, mic, mic_muted: false });
    }
}

/// 把会话目录里的录像按默认名移到输出目录，记为待命名。
pub fn finalize(app: &AppHandle, dir: &Path, game: &str, start: DateTime<Local>) {
    let rec = dir.join(REC_FILE);
    if fs::metadata(&rec).map(|m| m.len()).unwrap_or(0) < 1024 {
        let _ = fs::remove_dir_all(dir);
        return;
    }
    let cfg = crate::config_snapshot(app);
    let out = unique_path(&cfg.out_dir, &render_name(&cfg.name_template, game, start));
    if fs::rename(&rec, &out).is_ok() {
        let _ = fs::remove_dir_all(dir);
        crate::add_pending(app, out);
    } else {
        crate::notify(app, &format!("保存录像失败，原始文件在 {}", rec.display()));
    }
}

/// 启动时把上次异常退出留下的会话目录收尾。
pub fn recover(app: &AppHandle) {
    let root = tmp_root(&crate::config_snapshot(app));
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let dir = e.path();
        let ts = dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<i64>().ok());
        if let Some(start) = ts.and_then(|t| Local.timestamp_opt(t, 0).single()) {
            let game = fs::read_to_string(dir.join("game.txt")).unwrap_or_else(|_| "recording".into());
            finalize(app, &dir, &game, start);
        }
    }
}

pub fn unique_path(dir: &Path, stem: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}.mp4"));
    let mut i = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({i}).mp4"));
        i += 1;
    }
    p
}
