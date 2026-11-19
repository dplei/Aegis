//! 录制状态机：
//! - 空闲：每秒轮询进程，发现已启用游戏且有合格窗口 → 开会话、建分片 MP4 输出，进入录游戏。
//! - 录游戏：按当前游戏进程优先级选窗口；每次轮询刷新最后已知显示器；其他游戏运行按原规则提醒。
//!   确认当前游戏匹配进程全部退出时：
//!   - 开关关闭：沿用原流程，停止输出并加入待命名队列。
//!   - 开关开启：先暂停输出音画，成功后进入等待选择并打开弹窗。
//! - 等待选择：保持暂停（无超时，新游戏打开或切前台也不自行恢复），等待保存/不保存/继续录制/关闭弹窗或托盘命令。
//! - 录桌面：仅录上一款游戏最后所在的显示器，移除游戏音源、保留麦克风；检测到切入合格前台游戏窗口时原子接力。
use crate::config::{bitrate_kbps, render_name, sanitize, Config, Game, Scope};
use crate::win::{self, MonitorRef, Win};
use chrono::{DateTime, Local, TimeZone};
use libobs_simple::output::simple::{HardwareCodec, ObsContextSimpleExt, OutputFormat};
use libobs_simple::sources::windows::{
    MonitorCaptureSourceBuilder, ObsDisplayCaptureMethod, ObsWindowCaptureMethod,
    WindowCaptureSourceBuilder,
};
use libobs_wrapper::context::ObsContext;
use libobs_wrapper::data::object::ObsObjectTrait;
use libobs_wrapper::data::output::{ObsOutputRef, ObsOutputTrait};
use libobs_wrapper::data::video::ObsVideoInfoBuilder;
use libobs_wrapper::data::{ObsData, ObsObjectUpdater};
use libobs_wrapper::enums::{ObsAlignment, ObsBoundsType, ObsOrderMovement};
use libobs_wrapper::graphics::Vec2;
use libobs_wrapper::scenes::{
    ObsSceneRef, ObsTransformInfoBuilder, SceneItemExtSceneTrait, SceneItemTrait,
};
use libobs_wrapper::sources::{ObsSourceAudioExt, ObsSourceBuilder, ObsSourceRef, ObsSourceTrait};
use libobs_wrapper::utils::{ObsPath, StartupInfo};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::AppHandle;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const MIN_FREE_START: u64 = 5 << 30;
const MIN_FREE_RUN: u64 = 1 << 30;
const REC_FILE: &str = "rec.mp4";
const NORMAL_POLL_INTERVAL: Duration = Duration::from_secs(1);
const FAST_POLL_INTERVAL: Duration = Duration::from_millis(50);
const UNHEALTHY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DESKTOP_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// 桌面段失帧/短暂重连宽限期：必须大于 OBS duplicator-monitor-capture 的 RESET_INTERVAL_SEC (3.0s)
const DESKTOP_UNHEALTHY_GRACE: Duration = Duration::from_secs(6);
const HANDOFF_PREWARM_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_RETRY_COOLDOWN: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordingStatus {
    Idle,
    Game { game: String },
    WaitingChoice { game: String },
    Desktop { from_game: String, monitor_name: String },
}

impl RecordingStatus {
    pub fn is_actively_recording(&self) -> bool {
        matches!(self, Self::Game { .. } | Self::Desktop { .. })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ActiveChoice {
    pub session_id: u64,
    pub choice_id: u64,
    pub first_game: String,
    pub last_game: String,
    pub default_name: String,
    pub out_dir: PathBuf,
    pub monitor_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceOutcome {
    Continued,
    Ended,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChoiceAction {
    Continue,
    Save(String),
    Discard,
    CloseDefault,
}

pub enum Cmd {
    /// 托盘：提前结束，照常弹待命名保存
    Stop,
    /// 托盘：丢弃这次录制
    Skip,
    /// 保存弹窗：等待选择阶段的用户动作
    Choice {
        session_id: u64,
        choice_id: u64,
        action: ChoiceAction,
        reply: Option<SyncSender<Result<ChoiceOutcome, String>>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Win { hwnd: isize },
    Mon { hmon: isize, audio_hwnd: isize },
    Desktop { monitor: MonitorRef },
}

/// 当前画面来源：
/// - Win：仅视频窗口捕获 + 独立应用音频捕获（独立控制，预热候选时只挂视频不挂音频，杜绝提前串音）
/// - Mon：显示器捕获 + 独立应用音频捕获（按 audio_hwnd 跟踪，即使同屏切换游戏/窗口也更新音频）
/// - Desktop：仅显示器捕获，不含游戏音源（只留麦克风）
enum Capture {
    Win(
        libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::WindowCaptureSource>,
        libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>,
    ),
    Mon(
        libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::monitor_capture::MonitorCaptureSource>,
        libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>,
    ),
    Desktop(
        libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::monitor_capture::MonitorCaptureSource>,
    ),
}

/// 接力候选源预热项：可见但置于画布之外且无应用音频，等待首帧到达后再切入主画布并挂接音源。
enum PrewarmVideo {
    Win(libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::WindowCaptureSource>),
    Mon(
        libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::monitor_capture::MonitorCaptureSource>,
    ),
}

#[derive(Clone, Debug)]
struct HandoffSpec {
    game: Game,
    scope: Scope,
    target: Target,
    hwnd: isize,
}

impl PartialEq for HandoffSpec {
    fn eq(&self, other: &Self) -> bool {
        self.game.name == other.game.name
            && self.scope == other.scope
            && self.target == other.target
            && self.hwnd == other.hwnd
    }
}

impl Eq for HandoffSpec {}

impl HandoffSpec {
    fn audio_hwnd(&self) -> isize {
        match self.target {
            Target::Win { hwnd } => hwnd,
            Target::Mon { audio_hwnd, .. } => audio_hwnd,
            Target::Desktop { .. } => self.hwnd,
        }
    }
}

struct HandoffCandidate {
    spec: HandoffSpec,
    video: PrewarmVideo,
    deadline: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HandoffBackoff {
    target: Target,
    until: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesktopUnhealthyReason {
    MonitorMissing,
    NoFrame,
}

#[derive(Clone, Debug)]
struct DesktopHealth {
    unhealthy_since: Option<Instant>,
    reason: Option<DesktopUnhealthyReason>,
}

impl DesktopHealth {
    fn new() -> Self {
        Self { unhealthy_since: None, reason: None }
    }

    /// 每次健康检查更新状态：同时确认授权显示器仍在且捕获源有可用画面才算健康；
    /// 否则进入宽限期计时，超过 `grace` 返回持续失帧/断屏原因以触发安全收尾。
    fn observe(
        &mut self,
        now: Instant,
        monitor_ok: bool,
        frame_ok: bool,
        grace: Duration,
    ) -> Option<DesktopUnhealthyReason> {
        if monitor_ok && frame_ok {
            self.unhealthy_since = None;
            self.reason = None;
            return None;
        }
        let reason = if !monitor_ok {
            DesktopUnhealthyReason::MonitorMissing
        } else {
            DesktopUnhealthyReason::NoFrame
        };
        let since = *self.unhealthy_since.get_or_insert(now);
        self.reason = Some(reason);
        if now.saturating_duration_since(since) >= grace {
            Some(reason)
        } else {
            None
        }
    }

    fn is_unhealthy(&self) -> bool {
        self.unhealthy_since.is_some()
    }
}

#[derive(Clone, Debug)]
enum Phase {
    Game,
    WaitingChoice {
        choice_id: u64,
        monitor: Option<MonitorRef>,
        last_game: String,
    },
    /// 用户点了「继续录制」，输出仍保持暂停，正在等待授权桌面源产出首帧；
    /// 等待期间不阻塞命令循环，Stop/Skip/低磁盘仍优先终止。
    PreparingDesktop {
        monitor: MonitorRef,
        last_game: String,
        deadline: Instant,
    },
    Desktop {
        monitor: MonitorRef,
        last_game: String,
        health: DesktopHealth,
    },
}

enum FinalizeMode {
    /// 按默认名保存并加入待命名队列、打开保存弹窗（原流程 / 托盘停止 / 低磁盘 / 暂停失败兜底）
    DefaultPending,
    /// 按默认名保存，不加入待命名队列、不再次弹窗（弹窗关闭 / 显示器不可用）
    DefaultSilent,
    /// 按用户输入名保存，不加入待命名队列（弹窗点保存）
    CustomName(String),
    /// 按默认名移出后送入回收站，不加入待命名队列（弹窗点不保存）
    Recycle,
    /// 直接删掉临时目录（托盘跳过）
    DiscardTemp,
}

/// 一次录制会话。开关、全局范围、游戏列表、模板和输出目录均在开录时定下，录中修改从下次开录生效。
struct Session {
    id: u64,
    first_game: String,
    current_game: Game,
    current_scope: Scope,
    default_scope: Scope,
    continue_after_exit: bool,
    games: Vec<Game>,
    start: DateTime<Local>,
    dir: PathBuf,
    name_template: String,
    output: ObsOutputRef,
    cap: Option<(Target, Capture)>,
    prewarm: Option<HandoffCandidate>,
    handoff_backoff: Option<HandoffBackoff>,
    pending_continue_reply: Option<SyncSender<Result<ChoiceOutcome, String>>>,
    mic: Option<libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>>,
    mic_muted: bool,
    last_monitor: Option<MonitorRef>,
    phase: Phase,
}

impl Session {
    fn waiting_choice_id(&self) -> Option<u64> {
        match self.phase {
            Phase::WaitingChoice { choice_id, .. } => Some(choice_id),
            _ => None,
        }
    }

    fn tick_interval(&self) -> Duration {
        match &self.phase {
            Phase::PreparingDesktop { .. } => FAST_POLL_INTERVAL,
            Phase::Desktop { health, .. } => {
                if self.prewarm.is_some() {
                    FAST_POLL_INTERVAL
                } else if health.is_unhealthy() {
                    UNHEALTHY_POLL_INTERVAL
                } else {
                    NORMAL_POLL_INTERVAL
                }
            }
            Phase::Game | Phase::WaitingChoice { .. } => NORMAL_POLL_INTERVAL,
        }
    }

    fn sync_mic_mute(&mut self, muted: bool) {
        if let Some(mic) = &self.mic {
            if self.mic_muted != muted && mic.inner_source().set_muted(muted).is_ok() {
                self.mic_muted = muted;
            }
        }
    }

    fn out_dir(&self, fallback: &Path) -> PathBuf {
        self.dir
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| fallback.to_path_buf(), Path::to_path_buf)
    }
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

/// 按 exes 优先级找窗口：排前面的进程有窗口就录它，同一进程取面积最大的。返回 (目标, 窗口)。
fn pick_from_wins(
    game: &Game,
    scope: Scope,
    pids: &[(u32, String)],
    wins: &[Win],
    monitor_lookup: impl Fn(isize) -> Option<isize>,
) -> Option<(Target, isize)> {
    for exe in &game.exes {
        let ids: Vec<u32> = pids
            .iter()
            .filter(|(_, e)| e.eq_ignore_ascii_case(exe))
            .map(|p| p.0)
            .collect();
        if let Some(w) = wins.iter().filter(|w| ids.contains(&w.pid)).max_by_key(|w| w.area) {
            let t = match scope {
                Scope::Window => Target::Win { hwnd: w.hwnd },
                Scope::Monitor => Target::Mon {
                    hmon: monitor_lookup(w.hwnd)?,
                    audio_hwnd: w.hwnd,
                },
            };
            return Some((t, w.hwnd));
        }
    }
    None
}

/// 桌面段接力识别：用前台合格窗口及其进程判断用户切到了哪款已启用游戏，
/// 同一窗口匹配多条配置时按列表顺序取首条，再用该游戏既有进程优先级挑实际录制窗口。
fn pick_foreground_handoff(
    games: &[Game],
    default_scope: Scope,
    procs: &[(u32, String)],
    wins: &[Win],
    fg_hwnd: Option<isize>,
    monitor_lookup: impl Fn(isize) -> Option<isize>,
) -> Option<(Game, Scope, Target, isize)> {
    let fg_hwnd = fg_hwnd?;
    let fg_win = wins.iter().find(|w| w.hwnd == fg_hwnd)?;
    let exe = procs.iter().find(|(pid, _)| *pid == fg_win.pid).map(|(_, e)| e.as_str())?;
    let game = games
        .iter()
        .find(|g| g.enabled && g.exes.iter().any(|e| e.eq_ignore_ascii_case(exe)))?;
    let pids = game_pids(game, procs);
    let scope = game.scope.unwrap_or(default_scope);
    let (target, hwnd) = pick_from_wins(game, scope, &pids, wins, monitor_lookup)?;
    Some((game.clone(), scope, target, hwnd))
}

/// 会话结束时记录抑制集合：
/// - 开启跨游戏续录时，把结束瞬间仍在运行的所有已启用游戏加入抑制集合，避免立刻自动开录后台游戏。
/// - 开关关闭时沿用原有单游戏抑制语义（仅在需要抑制的终止原因下由调用方传入）。
fn record_suppressed_on_end(
    suppressed: &mut HashSet<String>,
    continue_after_exit: bool,
    current_game: &str,
    live_games: &[Game],
    procs: Option<&[(u32, String)]>,
    fallback_procs: &[(u32, String)],
) {
    if continue_after_exit {
        let snapshot = procs.unwrap_or(fallback_procs);
        if snapshot.is_empty() && procs.is_none() {
            suppressed.insert(current_game.to_string());
            return;
        }
        for g in live_games.iter().filter(|g| g.enabled && !game_pids(g, snapshot).is_empty()) {
            suppressed.insert(g.name.clone());
        }
    } else {
        suppressed.insert(current_game.to_string());
    }
}

/// 只有在进程枚举成功且观察到某款游戏的匹配进程全部退出后，才解除该游戏的抑制。
fn prune_suppressed(suppressed: &mut HashSet<String>, games: &[Game], procs: &[(u32, String)]) {
    suppressed.retain(|name| games.iter().any(|g| &g.name == name && !game_pids(g, procs).is_empty()));
}

enum BatchDecision {
    None,
    Stop,
    Skip,
    LowDisk,
    Choice {
        action: ChoiceAction,
        reply: Option<SyncSender<Result<ChoiceOutcome, String>>>,
    },
}

fn reply_stale(reply: Option<SyncSender<Result<ChoiceOutcome, String>>>) {
    if let Some(tx) = reply {
        let _ = tx.send(Ok(ChoiceOutcome::Stale));
    }
}

/// 同一轮里同时到达多条命令或低磁盘时：终止（Stop/Skip/低磁盘）优先于继续/弹窗操作；
/// 仅接受首个匹配当前 (session_id, waiting_choice_id) 的弹窗操作，重复或过期命令直接作废。
fn resolve_batch(
    cmds: Vec<Cmd>,
    disk_low: bool,
    session_id: u64,
    waiting_choice_id: Option<u64>,
) -> BatchDecision {
    let mut termination: Option<BatchDecision> = None;
    let mut accepted_choice: Option<(ChoiceAction, Option<SyncSender<Result<ChoiceOutcome, String>>>)> = None;

    for cmd in cmds {
        match cmd {
            Cmd::Stop => {
                if termination.is_none() {
                    termination = Some(BatchDecision::Stop);
                }
            }
            Cmd::Skip => {
                if termination.is_none() {
                    termination = Some(BatchDecision::Skip);
                }
            }
            Cmd::Choice { session_id: sid, choice_id: cid, action, reply } => {
                if termination.is_some() || disk_low {
                    reply_stale(reply);
                } else if accepted_choice.is_none()
                    && sid == session_id
                    && waiting_choice_id == Some(cid)
                {
                    accepted_choice = Some((action, reply));
                } else {
                    reply_stale(reply);
                }
            }
        }
    }

    if let Some(term) = termination {
        if let Some((_, reply)) = accepted_choice {
            reply_stale(reply);
        }
        return term;
    }
    if disk_low {
        if let Some((_, reply)) = accepted_choice {
            reply_stale(reply);
        }
        return BatchDecision::LowDisk;
    }
    if let Some((action, reply)) = accepted_choice {
        return BatchDecision::Choice { action, reply };
    }
    BatchDecision::None
}

fn is_same_monitor(a: &MonitorRef, b: &MonitorRef) -> bool {
    match (
        a.device_id.as_deref().filter(|s| !s.is_empty()),
        b.device_id.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(da), Some(db)) => da == db,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => a.hmon == b.hmon && (a.name.is_empty() || b.name.is_empty() || a.name == b.name),
    }
}

fn match_monitor_index(monitors: &[MonitorRef], target: &MonitorRef) -> Option<usize> {
    let target_dev = target.device_id.as_deref().filter(|s| !s.is_empty())?;
    monitors.iter().position(|m| m.device_id.as_deref() == Some(target_dev))
}

fn list_monitors() -> Res<Vec<(MonitorRef, display_info::DisplayInfo)>> {
    let all = MonitorCaptureSourceBuilder::get_monitors()?;
    Ok(all
        .into_iter()
        .map(|m| {
            let device_id = win::monitor_device_id(&m.name);
            let r = MonitorRef { hmon: m.raw_handle.0 as isize, name: m.name.clone(), device_id };
            (r, m)
        })
        .collect())
}

fn find_monitor(target: &MonitorRef) -> Res<(MonitorRef, display_info::DisplayInfo)> {
    let all = list_monitors()?;
    let refs: Vec<MonitorRef> = all.iter().map(|(r, _)| r.clone()).collect();
    let idx = match_monitor_index(&refs, target).ok_or("找不到原游戏所在的显示器")?;
    Ok(all.into_iter().nth(idx).unwrap())
}

fn find_monitor_by_hmon(hmon: isize) -> Res<(MonitorRef, display_info::DisplayInfo)> {
    list_monitors()?
        .into_iter()
        .find(|(r, _)| r.hmon == hmon)
        .ok_or_else(|| "找不到游戏所在的显示器".into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreparingDesktopDecision {
    Ready,
    Wait,
    MonitorLost,
    TimedOut,
}

fn evaluate_preparing_desktop(
    now: Instant,
    deadline: Instant,
    monitor_ok: bool,
    frame_ok: bool,
) -> PreparingDesktopDecision {
    if !monitor_ok {
        PreparingDesktopDecision::MonitorLost
    } else if frame_ok {
        PreparingDesktopDecision::Ready
    } else if now >= deadline {
        PreparingDesktopDecision::TimedOut
    } else {
        PreparingDesktopDecision::Wait
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrewarmStep {
    NoPrewarm,
    KeepWaiting,
    Commit,
    Timeout,
    Cancel,
}

fn reconcile_prewarm(
    now: Instant,
    desired: Option<&HandoffSpec>,
    active_spec: Option<&HandoffSpec>,
    deadline: Option<Instant>,
    prewarm_ready: bool,
) -> PrewarmStep {
    let (Some(cur), Some(dl)) = (active_spec, deadline) else {
        return PrewarmStep::NoPrewarm;
    };
    if desired != Some(cur) {
        return PrewarmStep::Cancel;
    }
    if prewarm_ready {
        PrewarmStep::Commit
    } else if now >= dl {
        PrewarmStep::Timeout
    } else {
        PrewarmStep::KeepWaiting
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NewHandoffAction {
    KeepDesktop,
    CommitReuseDesktop(HandoffSpec),
    StartPrewarm(HandoffSpec),
}

fn plan_new_handoff(
    now: Instant,
    desktop_mon: &MonitorRef,
    desktop_healthy_and_ready: bool,
    desired: Option<&HandoffSpec>,
    desired_target_mon: Option<&MonitorRef>,
    backoff: Option<&HandoffBackoff>,
) -> NewHandoffAction {
    let Some(spec) = desired else {
        return NewHandoffAction::KeepDesktop;
    };
    if backoff.is_some_and(|b| b.target == spec.target && now < b.until) {
        return NewHandoffAction::KeepDesktop;
    }
    if matches!(spec.target, Target::Mon { .. })
        && desired_target_mon.is_some_and(|m| is_same_monitor(desktop_mon, m))
    {
        if desktop_healthy_and_ready {
            return NewHandoffAction::CommitReuseDesktop(spec.clone());
        }
        return NewHandoffAction::KeepDesktop;
    }
    NewHandoffAction::StartPrewarm(spec.clone())
}

/// 查询 OBS 源当前可见状态及实际画面纹理尺寸（基于 OBS 32.1.2 源码确认）：
/// - `duplicator-monitor-capture.c` (WGC/DXGI) 与 `window-capture.c` (WGC) 创建及挂入场景时宽高为 0；
/// - WGC 仅在 `winrt-capture.cpp::on_frame_arrived` 写入首帧纹理后 `texture_width/height > 0`；
///   DXGI 仅在 `gs_duplicator_update_frame` 成功且纹理非空后 `width/height > 0`；
/// - 屏幕断开、WGC `on_closed`/格式变化/设备丢失、窗口关闭或最小化、源隐藏时，宽高均复位为 0。
/// 在 OBS actor 线程进入 `obs_enter_graphics` 后读取，与 `video_tick`/`video_render`/`on_frame_arrived`
/// 严格串行化，防止并发释放 `capture_winrt` 竞态，并保证返回 >0 时首帧纹理复制已完成。
fn source_frame_size<T: ObsSourceTrait + ?Sized>(
    source: &T,
) -> Result<(bool, u32, u32), libobs_wrapper::utils::ObsError> {
    let ptr = source.__native_handle();
    let runtime = source.runtime().clone();
    libobs_wrapper::run_with_obs!(runtime, (ptr), move || unsafe {
        let raw = ptr.raw_ptr_unchecked();
        libobs_wrapper::sys::obs_enter_graphics();
        let showing = libobs_wrapper::sys::obs_source_showing(raw);
        let w = libobs_wrapper::sys::obs_source_get_width(raw);
        let h = libobs_wrapper::sys::obs_source_get_height(raw);
        libobs_wrapper::sys::obs_leave_graphics();
        (showing, w, h)
    })
}

fn is_source_frame_ready<T: ObsSourceTrait + ?Sized>(source: &T) -> bool {
    matches!(source_frame_size(source), Ok((true, w, h)) if w > 0 && h > 0)
}

struct Obs {
    ctx: ObsContext,
    scene: ObsSceneRef,
    canvas: (u32, u32),
    src_seq: u64,
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
        let mut ctx = StartupInfo::new()
            .set_video_info(video_info(canvas))
            .set_module_config_path(conf)
            .start()?;
        let scene = ctx.scene("main", Some(0))?;
        Ok(Obs { ctx, scene, canvas, src_seq: 0 })
    }

    fn next_name(&mut self, prefix: &str) -> String {
        self.src_seq += 1;
        format!("{prefix}-{}", self.src_seq)
    }

    /// OBS 认窗口用的 "标题:类名:exe" 字符串。自己拼，不用 libobs_window_helper::get_window_info：
    /// 它要读目标进程内存拿路径和命令行，LOL 这类受反作弊保护的进程会拒绝访问。
    fn window_id(hwnd: isize) -> Res<String> {
        Ok(win::obs_window_id(hwnd).ok_or("读不到游戏窗口的标题或进程名")?)
    }

    /// 把预热候选项放在输出画布之外并沉到底层：保持 `visible=true`（即 `obs_source_showing=true`），
    /// 令 OBS 正常执行 `video_tick` 与 `video_render` 推进 WGC/DXGI 采帧，同时不向主画布写入任何像素。
    fn place_off_canvas<T: SceneItemTrait>(&self, item: &T) -> Res<()> {
        let (cw, ch) = self.canvas;
        let info = ObsTransformInfoBuilder::new()
            .set_pos(Vec2::new(cw as f32 + 4096.0, ch as f32 + 4096.0))
            .set_alignment(ObsAlignment::LEFT | ObsAlignment::TOP)
            .set_bounds(Vec2::new(16.0, 16.0))
            .set_bounds_type(ObsBoundsType::Stretch)
            .set_bounds_alignment(ObsAlignment::LEFT | ObsAlignment::TOP)
            .set_crop_to_bounds(false)
            .build(cw, ch);
        item.set_transform_info(&info)?;
        item.move_order(ObsOrderMovement::Bottom)?;
        Ok(())
    }

    fn add_window_source(
        &mut self,
        name: &str,
        window_id: &str,
        off_canvas: bool,
    ) -> Res<libobs_wrapper::scenes::ObsSceneItemRef<libobs_simple::sources::windows::WindowCaptureSource>> {
        let mut source = self
            .ctx
            .source_builder::<WindowCaptureSourceBuilder, _>(name)?
            .set_window_raw(window_id)
            .set_client_area(true)
            .set_capture_audio(false)?
            .build()?;
        let item = self.scene.add_source(source.clone())?;
        let transform_res = if off_canvas {
            self.place_off_canvas(&item)
        } else {
            item.fit_source_to_screen().map(|_| ()).map_err(Into::into)
        };
        if let Err(e) = transform_res {
            let _ = self.scene.remove_item(&item);
            return Err(e);
        }
        // 在已设好画布外/全屏变换后再切到 WGC，触发 wc_update -> force_reset 立即开启首帧搜索
        if let Err(e) = source
            .create_updater()?
            .set_capture_method(ObsWindowCaptureMethod::MethodWgc)
            .update()
        {
            let _ = self.scene.remove_item(&item);
            return Err(e.into());
        }
        Ok(item)
    }

    fn add_monitor_source(
        &mut self,
        name: &str,
        obs_monitor_id: &str,
        off_canvas: bool,
    ) -> Res<
        libobs_wrapper::scenes::ObsSceneItemRef<
            libobs_simple::sources::windows::monitor_capture::MonitorCaptureSource,
        >,
    > {
        let mut source = self
            .ctx
            .source_builder::<MonitorCaptureSourceBuilder, _>(name)?
            .set_monitor_id_raw(obs_monitor_id)
            .build()?;
        let item = self.scene.add_source(source.clone())?;
        let transform_res = if off_canvas {
            self.place_off_canvas(&item)
        } else {
            item.fit_source_to_screen().map(|_| ()).map_err(Into::into)
        };
        if let Err(e) = transform_res {
            let _ = self.scene.remove_item(&item);
            return Err(e);
        }
        if let Err(e) = source
            .create_updater()?
            .set_capture_method(ObsDisplayCaptureMethod::MethodWgc)?
            .update()
        {
            let _ = self.scene.remove_item(&item);
            return Err(e.into());
        }
        Ok(item)
    }

    fn capture_video_ready(&self, cur: Option<&(Target, Capture)>) -> bool {
        match cur {
            Some((_, Capture::Win(item, _))) => is_source_frame_ready(item.inner_source()),
            Some((_, Capture::Mon(item, _))) => is_source_frame_ready(item.inner_source()),
            Some((_, Capture::Desktop(item))) => is_source_frame_ready(item.inner_source()),
            None => false,
        }
    }

    fn prewarm_video_ready(&self, video: &PrewarmVideo) -> bool {
        match video {
            PrewarmVideo::Win(item) => is_source_frame_ready(item.inner_source()),
            PrewarmVideo::Mon(item) => is_source_frame_ready(item.inner_source()),
        }
    }

    /// 继续录制（输出已暂停）时准备授权显示器的仅视频桌面源，并确保旧游戏音源已移除；
    /// 桌面源按授权屏幕的设备路径创建，并在 `PreparingDesktop` 阶段确认实际画面就绪。
    fn prepare_desktop_capture(
        &mut self,
        cur: &mut Option<(Target, Capture)>,
        monitor: &MonitorRef,
    ) -> Res<MonitorRef> {
        let (live_mon, _) = find_monitor(monitor)?;
        if matches!(
            cur.as_ref(),
            Some((Target::Desktop { monitor: cur_m }, Capture::Desktop(_))) if is_same_monitor(cur_m, &live_mon)
        ) {
            return Ok(live_mon);
        }
        // 游戏源可能绑定了已被复用的显示器句柄。暂停期间按设备路径重新建源。
        self.drop_capture(cur);
        let screen_name = self.next_name("screen");
        let item = self.add_monitor_source(&screen_name, live_mon.obs_monitor_id(), false)?;
        *cur = Some((Target::Desktop { monitor: live_mon.clone() }, Capture::Desktop(item)));
        Ok(live_mon)
    }

    /// 启动接力候选视频源预热（画布外、可见、无应用音频）。旧桌面源与麦克风保持原样。
    fn start_prewarm(&mut self, target: &Target) -> Res<PrewarmVideo> {
        match target {
            Target::Win { hwnd } => {
                let id = Self::window_id(*hwnd)?;
                let name = self.next_name("prewarm-win");
                let item = self.add_window_source(&name, &id, true)?;
                Ok(PrewarmVideo::Win(item))
            }
            Target::Mon { hmon, .. } => {
                let (mon_ref, _) = find_monitor_by_hmon(*hmon)?;
                let name = self.next_name("prewarm-screen");
                let item = self.add_monitor_source(&name, mon_ref.obs_monitor_id(), true)?;
                Ok(PrewarmVideo::Mon(item))
            }
            Target::Desktop { .. } => Err("桌面目标无需候选预热".into()),
        }
    }

    fn remove_prewarm_video(&mut self, video: PrewarmVideo) {
        match video {
            PrewarmVideo::Win(item) => {
                let _ = self.scene.remove_item(&item);
            }
            PrewarmVideo::Mon(item) => {
                let _ = self.scene.remove_item(&item);
            }
        }
    }

    fn drop_prewarm(&mut self, prewarm: &mut Option<HandoffCandidate>) {
        if let Some(cand) = prewarm.take() {
            self.remove_prewarm_video(cand.video);
        }
    }

    /// 将已就绪的预热候选切为主采集：先挂载应用音频并将候选视频提升到主画布顶层，
    /// 全部成功后再移除旧桌面源；任意步骤失败时回滚候选与新音源，旧桌面源保持完好。
    fn commit_prewarm(
        &mut self,
        cur: &mut Option<(Target, Capture)>,
        cand: HandoffCandidate,
    ) -> Res<()> {
        let audio_id = match Self::window_id(cand.spec.audio_hwnd()) {
            Ok(id) => id,
            Err(e) => {
                self.remove_prewarm_video(cand.video);
                return Err(e);
            }
        };
        let audio_name = self.next_name("game-audio");
        let audio = match self.add_raw("wasapi_process_output_capture", &audio_name, &[("window", &audio_id)]) {
            Ok(a) => a,
            Err(e) => {
                self.remove_prewarm_video(cand.video);
                return Err(e);
            }
        };
        let promote_res = match &cand.video {
            PrewarmVideo::Win(item) => item
                .fit_source_to_screen()
                .and_then(|_| item.move_order(ObsOrderMovement::Top)),
            PrewarmVideo::Mon(item) => item
                .fit_source_to_screen()
                .and_then(|_| item.move_order(ObsOrderMovement::Top)),
        };
        if let Err(e) = promote_res {
            let _ = self.scene.remove_item(&audio);
            self.remove_prewarm_video(cand.video);
            return Err(e.into());
        }
        self.drop_capture(cur);
        let cap = match cand.video {
            PrewarmVideo::Win(item) => Capture::Win(item, audio),
            PrewarmVideo::Mon(item) => Capture::Mon(item, audio),
        };
        *cur = Some((cand.spec.target, cap));
        Ok(())
    }

    /// 同屏全屏接力：重用已就绪的原桌面显示器源，仅挂接新游戏的应用音频。
    fn reuse_desktop_for_mon(
        &mut self,
        cur: &mut Option<(Target, Capture)>,
        target: Target,
        audio_hwnd: isize,
    ) -> Res<()> {
        if !matches!(cur.as_ref(), Some((_, Capture::Desktop(_)))) {
            return Err("当前不是可复用的桌面源".into());
        }
        let audio_id = Self::window_id(audio_hwnd)?;
        let audio_name = self.next_name("game-audio");
        let audio = self.add_raw("wasapi_process_output_capture", &audio_name, &[("window", &audio_id)])?;
        let Some((_, Capture::Desktop(item))) = cur.take() else {
            let _ = self.scene.remove_item(&audio);
            return Err("当前不是可复用的桌面源".into());
        };
        *cur = Some((target, Capture::Mon(item, audio)));
        Ok(())
    }

    /// 让捕获对准 target（用于空闲开录及同一游戏内的窗口/屏幕切换）：
    /// - 同类目标就地更新设置；
    /// - 跨类型切换时先建新源，全部成功后再通过 `scene.remove_item` 彻底释放旧源。
    fn aim(&mut self, cur: &mut Option<(Target, Capture)>, target: Target) -> Res<()> {
        match target.clone() {
            Target::Win { hwnd } => {
                let id = Self::window_id(hwnd)?;
                if let Some((t, Capture::Win(item, audio))) = cur.as_mut() {
                    item.inner_source_mut().create_updater()?.set_window_raw(id.as_str()).update()?;
                    item.fit_source_to_screen()?;
                    let mut d = ObsData::new(self.ctx.runtime().clone())?;
                    d.bulk_update().set_string("window", id.as_str()).apply()?;
                    audio.inner_source().update_settings(d)?;
                    *t = target;
                    return Ok(());
                }
                let name = self.next_name("game");
                let item = self.add_window_source(&name, &id, false)?;
                let audio_name = self.next_name("game-audio");
                let audio = match self.add_raw("wasapi_process_output_capture", &audio_name, &[("window", &id)]) {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = self.scene.remove_item(&item);
                        return Err(e);
                    }
                };
                self.drop_capture(cur);
                *cur = Some((target, Capture::Win(item, audio)));
                Ok(())
            }
            Target::Mon { hmon, audio_hwnd } => {
                let id = Self::window_id(audio_hwnd)?;
                let (mon_ref, _) = find_monitor_by_hmon(hmon)?;
                if let Some((Target::Mon { hmon: cur_m, audio_hwnd: cur_h }, Capture::Mon(item, audio))) =
                    cur.as_mut()
                {
                    if *cur_m != hmon {
                        item.inner_source_mut()
                            .create_updater()?
                            .set_monitor_id_raw(mon_ref.obs_monitor_id())
                            .update()?;
                        item.fit_source_to_screen()?;
                        *cur_m = hmon;
                    }
                    if *cur_h != audio_hwnd {
                        let mut d = ObsData::new(self.ctx.runtime().clone())?;
                        d.bulk_update().set_string("window", id.as_str()).apply()?;
                        audio.inner_source().update_settings(d)?;
                        *cur_h = audio_hwnd;
                    }
                    return Ok(());
                }
                let screen_name = self.next_name("screen");
                let item = self.add_monitor_source(&screen_name, mon_ref.obs_monitor_id(), false)?;
                let audio_name = self.next_name("game-audio");
                let audio = match self.add_raw("wasapi_process_output_capture", &audio_name, &[("window", &id)]) {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = self.scene.remove_item(&item);
                        return Err(e);
                    }
                };
                self.drop_capture(cur);
                *cur = Some((target, Capture::Mon(item, audio)));
                Ok(())
            }
            Target::Desktop { monitor: ref mon_ref } => {
                self.prepare_desktop_capture(cur, mon_ref).map(|_| ())
            }
        }
    }

    fn drop_capture(&mut self, cur: &mut Option<(Target, Capture)>) {
        match cur.take() {
            Some((_, Capture::Win(i, a))) => {
                let _ = self.scene.remove_item(&i);
                let _ = self.scene.remove_item(&a);
            }
            Some((_, Capture::Mon(i, a))) => {
                let _ = self.scene.remove_item(&i);
                let _ = self.scene.remove_item(&a);
            }
            Some((_, Capture::Desktop(i))) => {
                let _ = self.scene.remove_item(&i);
            }
            None => {}
        }
    }

    /// libobs-simple 没有封装的源（麦克风、应用音频），按类型 id + 字符串设置创建并加进场景。
    fn add_raw(
        &mut self,
        kind: &str,
        name: &str,
        settings: &[(&str, &str)],
    ) -> Res<libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>> {
        let st = self
            .ctx
            .source_type(kind)?
            .ok_or_else(|| format!("OBS 里没有 {kind} 这种源"))?;
        let mut d = ObsData::new(self.ctx.runtime().clone())?;
        let mut u = d.bulk_update();
        for (k, v) in settings {
            u = u.set_string(*k, *v);
        }
        u.apply()?;
        let src = self.ctx.create_source(&st, name, Some(d))?;
        Ok(self.scene.add(src)?)
    }

    fn begin(
        &mut self,
        cfg: &Config,
        session_id: u64,
        game: &Game,
        mon_size: (u32, u32),
        dir: &Path,
        muted: bool,
    ) -> Res<(ObsOutputRef, Option<libobs_wrapper::scenes::ObsSceneItemRef<ObsSourceRef>>, bool)> {
        let canvas = cfg.canvas(mon_size);
        if canvas != self.canvas {
            self.ctx.reset_video(video_info(canvas))?;
            self.canvas = canvas;
        }
        let mut mic_muted = false;
        let mic = if cfg.mic_enabled {
            // OBS 的 "default" 和系统设置里的默认录音设备不一定是同一个，自己解析成具体 id
            let id = cfg
                .mic_device
                .clone()
                .or_else(win::default_mic)
                .unwrap_or_else(|| "default".into());
            let name = self.next_name("mic");
            let m = self.add_raw("wasapi_input_capture", &name, &[("device_id", &id)])?;
            if muted && m.inner_source().set_muted(true).is_ok() {
                mic_muted = true;
            }
            Some(m)
        } else {
            None
        };
        let output = self
            .ctx
            .simple_output_builder(
                format!("rec-{session_id}-{}", game.name),
                ObsPath::new(&dir.join(REC_FILE).to_string_lossy()),
            )
            .video_bitrate(bitrate_kbps(canvas.0, canvas.1))
            .audio_bitrate(160)
            .prefer_hardware_encoder(HardwareCodec::HEVC)
            // 分片 MP4：程序崩溃或断电时，已录的部分照样能播
            .format(OutputFormat::FragmentedMP4)
            .build()?;
        Ok((output, mic, mic_muted))
    }
}

fn pause_output(output: &ObsOutputRef) -> Res<()> {
    output.pause()?;
    if !output.is_paused()? {
        return Err("输出未进入暂停状态".into());
    }
    Ok(())
}

fn unpause_output(output: &ObsOutputRef) -> Res<()> {
    output.unpause()?;
    if output.is_paused()? {
        return Err("输出未恢复录制状态".into());
    }
    Ok(())
}

/// 停止输出并确认已彻底停止后才返回 Ok；未确认停止前不移动或删除文件。
fn stop_output_safely(output: &ObsOutputRef) -> Res<()> {
    if let Err(e) = output.stop() {
        log::warn!("常规停止录制输出失败：{e}");
        let still_running = output.is_active().unwrap_or(true) || output.is_reconnecting().unwrap_or(true);
        if still_running {
            if let Err(e2) = output.force_stop() {
                log::error!("强制停止录制输出失败：{e2}");
            }
        }
    }
    let active = output.is_active().unwrap_or(true);
    let reconnecting = output.is_reconnecting().unwrap_or(true);
    if active || reconnecting {
        return Err("无法确认录制输出已停止".into());
    }
    Ok(())
}

fn finish_session(
    mut s: Session,
    obs: &mut Obs,
    app: &AppHandle,
    mode: FinalizeMode,
    reason: &str,
) -> Result<(), String> {
    crate::clear_active_choice(app, Some(s.id));
    let stop_res = stop_output_safely(&s.output);
    obs.drop_prewarm(&mut s.prewarm);
    obs.drop_capture(&mut s.cap);
    if let Some(m) = s.mic.take() {
        let _ = obs.scene.remove_item(&m);
    }
    crate::set_status(app, RecordingStatus::Idle);
    if let Err(e) = stop_res {
        let rec = s.dir.join(REC_FILE);
        log::error!("结束录制 {}（{}）失败，输出未确认停止：{e}", s.first_game, reason);
        crate::notify(
            app,
            &format!("停止录制失败，原始文件保留在 {}：{e}", rec.display()),
        );
        let err = format!("停止录制失败：{e}");
        if let Some(tx) = s.pending_continue_reply.take() {
            let _ = tx.send(Err(err.clone()));
        }
        return Err(err);
    }

    let was_waiting = matches!(s.phase, Phase::WaitingChoice { .. } | Phase::PreparingDesktop { .. });
    log::info!("结束录制 {}（{}）", s.first_game, reason);
    let res = match mode {
        FinalizeMode::DefaultPending => {
            let added = finalize(app, &s.dir, &s.first_game, s.start);
            if !added && was_waiting {
                crate::refresh_or_hide_save_window(app);
            }
            Ok(())
        }
        FinalizeMode::DefaultSilent => finalize_direct(app, &s.dir, &s.first_game, s.start, None, false),
        FinalizeMode::CustomName(name) => {
            let custom = name.trim();
            let custom = (!custom.is_empty()).then_some(custom);
            finalize_direct(app, &s.dir, &s.first_game, s.start, custom, false)
        }
        FinalizeMode::Recycle => finalize_direct(app, &s.dir, &s.first_game, s.start, None, true),
        FinalizeMode::DiscardTemp => {
            let _ = fs::remove_dir_all(&s.dir);
            Ok(())
        }
    };
    crate::set_recording_dir(app, None);
    if let Some(tx) = s.pending_continue_reply.take() {
        let _ = tx.send(Ok(ChoiceOutcome::Ended));
    }
    res
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
    log::info!("录制内核已启动");

    let mut sess: Option<Session> = None;
    let mut suppressed: HashSet<String> = HashSet::new();
    let mut noticed: HashSet<String> = HashSet::new();
    let mut last_procs: Vec<(u32, String)> = Vec::new();
    let mut next_session_id: u64 = 1;
    let mut next_choice_id: u64 = 1;
    let mut next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
    let mut next_proc_poll = next_poll;

    loop {
        let mut cmds = Vec::new();
        let wait = next_poll.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(c) => cmds.push(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        let now = Instant::now();
        let polled = now >= next_poll;
        if polled {
            let interval = sess.as_ref().map_or(NORMAL_POLL_INTERVAL, Session::tick_interval);
            next_poll = now + interval;
        }

        let cfg = crate::config_snapshot(&app);

        if let Some(s) = &mut sess {
            s.sync_mic_mute(muted.load(Relaxed));

            let disk_low = win::disk_free(&s.dir) < MIN_FREE_RUN;
            let decision = resolve_batch(cmds, disk_low, s.id, s.waiting_choice_id());

            match decision {
                BatchDecision::Stop => {
                    let s = sess.take().unwrap();
                    let live_procs = win::processes().ok();
                    record_suppressed_on_end(
                        &mut suppressed,
                        s.continue_after_exit,
                        &s.current_game.name,
                        &cfg.games,
                        live_procs.as_deref(),
                        &last_procs,
                    );
                    let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultPending, "托盘保存");
                    noticed.clear();
                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                    continue;
                }
                BatchDecision::Skip => {
                    let s = sess.take().unwrap();
                    let was_waiting = matches!(s.phase, Phase::WaitingChoice { .. });
                    let live_procs = win::processes().ok();
                    record_suppressed_on_end(
                        &mut suppressed,
                        s.continue_after_exit,
                        &s.current_game.name,
                        &cfg.games,
                        live_procs.as_deref(),
                        &last_procs,
                    );
                    let _ = finish_session(s, &mut obs, &app, FinalizeMode::DiscardTemp, "托盘跳过");
                    if was_waiting {
                        crate::refresh_or_hide_save_window(&app);
                    }
                    noticed.clear();
                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                    continue;
                }
                BatchDecision::LowDisk => {
                    let s = sess.take().unwrap();
                    let live_procs = win::processes().ok();
                    record_suppressed_on_end(
                        &mut suppressed,
                        s.continue_after_exit,
                        &s.current_game.name,
                        &cfg.games,
                        live_procs.as_deref(),
                        &last_procs,
                    );
                    crate::notify(&app, "硬盘空间不足 1GB，已停止录制并保存已录部分");
                    let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultPending, "低磁盘");
                    noticed.clear();
                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                    continue;
                }
                BatchDecision::Choice { action, reply } => {
                    match action {
                        ChoiceAction::Save(name) => {
                            let s = sess.take().unwrap();
                            let live_procs = win::processes().ok();
                            record_suppressed_on_end(
                                &mut suppressed,
                                s.continue_after_exit,
                                &s.current_game.name,
                                &cfg.games,
                                live_procs.as_deref(),
                                &last_procs,
                            );
                            let res = finish_session(s, &mut obs, &app, FinalizeMode::CustomName(name), "弹窗保存")
                                .map(|()| ChoiceOutcome::Ended);
                            if let Some(tx) = reply {
                                let _ = tx.send(res);
                            }
                            noticed.clear();
                            next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                            continue;
                        }
                        ChoiceAction::Discard => {
                            let s = sess.take().unwrap();
                            let live_procs = win::processes().ok();
                            record_suppressed_on_end(
                                &mut suppressed,
                                s.continue_after_exit,
                                &s.current_game.name,
                                &cfg.games,
                                live_procs.as_deref(),
                                &last_procs,
                            );
                            let res = finish_session(s, &mut obs, &app, FinalizeMode::Recycle, "弹窗不保存")
                                .map(|()| ChoiceOutcome::Ended);
                            if let Some(tx) = reply {
                                let _ = tx.send(res);
                            }
                            noticed.clear();
                            next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                            continue;
                        }
                        ChoiceAction::CloseDefault => {
                            let s = sess.take().unwrap();
                            let live_procs = win::processes().ok();
                            record_suppressed_on_end(
                                &mut suppressed,
                                s.continue_after_exit,
                                &s.current_game.name,
                                &cfg.games,
                                live_procs.as_deref(),
                                &last_procs,
                            );
                            let res = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "关闭弹窗默认保存")
                                .map(|()| ChoiceOutcome::Ended);
                            if let Some(tx) = reply {
                                let _ = tx.send(res);
                            }
                            noticed.clear();
                            next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                            continue;
                        }
                        ChoiceAction::Continue => {
                            let (monitor, last_game) = match &s.phase {
                                Phase::WaitingChoice { monitor, last_game, .. } => (monitor.clone(), last_game.clone()),
                                _ => {
                                    reply_stale(reply);
                                    continue;
                                }
                            };
                            let Some(monitor) = monitor else {
                                let mut s = sess.take().unwrap();
                                s.pending_continue_reply = reply;
                                let live_procs = win::processes().ok();
                                record_suppressed_on_end(
                                    &mut suppressed,
                                    s.continue_after_exit,
                                    &s.current_game.name,
                                    &cfg.games,
                                    live_procs.as_deref(),
                                    &last_procs,
                                );
                                crate::notify(&app, "无法识别上一款游戏所在的显示器，已按默认名保存已录内容");
                                let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "无授权显示器");
                                noticed.clear();
                                next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                continue;
                            };

                            // 保持输出暂停：核验授权显示器身份、准备仅视频桌面源、移除旧游戏音源，
                            // 随后进入非阻塞 PreparingDesktop 等待实际首帧就绪（期间仍保持命令即时响应与终止优先）。
                            match obs.prepare_desktop_capture(&mut s.cap, &monitor) {
                                Ok(live_mon) => {
                                    s.sync_mic_mute(muted.load(Relaxed));
                                    s.pending_continue_reply = reply;
                                    s.phase = Phase::PreparingDesktop {
                                        monitor: live_mon,
                                        last_game,
                                        deadline: Instant::now() + DESKTOP_READY_TIMEOUT,
                                    };
                                    // 立刻触发一次就绪检查（同屏复用场景可零等待通过）
                                    next_poll = Instant::now();
                                    continue;
                                }
                                Err(e) => {
                                    let mut s = sess.take().unwrap();
                                    s.pending_continue_reply = reply;
                                    let live_procs = win::processes().ok();
                                    record_suppressed_on_end(
                                        &mut suppressed,
                                        s.continue_after_exit,
                                        &s.current_game.name,
                                        &cfg.games,
                                        live_procs.as_deref(),
                                        &last_procs,
                                    );
                                    crate::notify(
                                        &app,
                                        &format!("原游戏所在显示器不可用，已按默认名保存已录内容：{e}"),
                                    );
                                    let _ = finish_session(
                                        s,
                                        &mut obs,
                                        &app,
                                        FinalizeMode::DefaultSilent,
                                        "桌面显示器不可用",
                                    );
                                    noticed.clear();
                                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                    continue;
                                }
                            }
                        }
                    }
                }
                BatchDecision::None => {}
            }

            if !polled {
                continue;
            }

            // 处于 PreparingDesktop 时无需每秒枚举进程；桌面段有预热候选时需实时校验前台目标
            let need_procs = match &s.phase {
                Phase::PreparingDesktop { .. } => false,
                Phase::Desktop { .. } => s.prewarm.is_some() || now >= next_proc_poll,
                Phase::Game | Phase::WaitingChoice { .. } => now >= next_proc_poll,
            };
            let procs_res = if need_procs {
                if now >= next_proc_poll {
                    next_proc_poll = now + NORMAL_POLL_INTERVAL;
                }
                let res = win::processes();
                if let Ok(ref p) = res {
                    last_procs = p.clone();
                    prune_suppressed(&mut suppressed, &cfg.games, p);
                } else if let Err(ref e) = res {
                    log::warn!("枚举进程失败，暂保持当前录制状态：{e}");
                }
                Some(res)
            } else {
                None
            };

            match s.phase.clone() {
                Phase::PreparingDesktop { monitor, last_game, deadline } => {
                    let monitor_res = find_monitor(&monitor);
                    let frame_ok = obs.capture_video_ready(s.cap.as_ref());
                    match evaluate_preparing_desktop(now, deadline, monitor_res.is_ok(), frame_ok) {
                        PreparingDesktopDecision::Wait => {
                            // 保持暂停等待桌面首帧，下个 50ms tick 再查
                            continue;
                        }
                        PreparingDesktopDecision::MonitorLost => {
                            let s = sess.take().unwrap();
                            let live_procs = win::processes().ok();
                            record_suppressed_on_end(
                                &mut suppressed,
                                s.continue_after_exit,
                                &s.current_game.name,
                                &cfg.games,
                                live_procs.as_deref(),
                                &last_procs,
                            );
                            crate::notify(&app, "原游戏所在显示器在准备桌面画面期间已断开，已按默认名保存已录内容");
                            let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "桌面显示器失效");
                            noticed.clear();
                            next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                            continue;
                        }
                        PreparingDesktopDecision::TimedOut => {
                            let s = sess.take().unwrap();
                            let live_procs = win::processes().ok();
                            record_suppressed_on_end(
                                &mut suppressed,
                                s.continue_after_exit,
                                &s.current_game.name,
                                &cfg.games,
                                live_procs.as_deref(),
                                &last_procs,
                            );
                            crate::notify(
                                &app,
                                "原游戏所在显示器在限定时间内未取得可用画面，已按默认名保存已录内容",
                            );
                            let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "桌面画面就绪超时");
                            noticed.clear();
                            next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                            continue;
                        }
                        PreparingDesktopDecision::Ready => {
                            // 1. 桌面源已确认有实际画面且旧游戏音源已移除，先同步麦克风静音
                            s.sync_mic_mute(muted.load(Relaxed));
                            // 2. 隐藏选择弹窗并等待 DWM 合成确认，避免把弹窗录进桌面段
                            if !crate::hide_save_window_sync(&app) {
                                let s = sess.take().unwrap();
                                let live_procs = win::processes().ok();
                                record_suppressed_on_end(
                                    &mut suppressed,
                                    s.continue_after_exit,
                                    &s.current_game.name,
                                    &cfg.games,
                                    live_procs.as_deref(),
                                    &last_procs,
                                );
                                crate::notify(&app, "无法确认选择弹窗已隐藏，已按默认名保存已录内容");
                                let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "隐藏弹窗失败");
                                noticed.clear();
                                next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                continue;
                            }
                            // 3. 隐藏弹窗期间若收到终止命令或低磁盘，保留终止优先语义，不恢复输出
                            let mut late_cmds = Vec::new();
                            while let Ok(c) = rx.try_recv() {
                                late_cmds.push(c);
                            }
                            let late_disk_low = win::disk_free(&s.dir) < MIN_FREE_RUN;
                            match resolve_batch(late_cmds, late_disk_low, s.id, None) {
                                BatchDecision::Stop => {
                                    let s = sess.take().unwrap();
                                    let live_procs = win::processes().ok();
                                    record_suppressed_on_end(
                                        &mut suppressed,
                                        s.continue_after_exit,
                                        &s.current_game.name,
                                        &cfg.games,
                                        live_procs.as_deref(),
                                        &last_procs,
                                    );
                                    let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultPending, "托盘保存");
                                    noticed.clear();
                                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                    continue;
                                }
                                BatchDecision::Skip => {
                                    let s = sess.take().unwrap();
                                    let live_procs = win::processes().ok();
                                    record_suppressed_on_end(
                                        &mut suppressed,
                                        s.continue_after_exit,
                                        &s.current_game.name,
                                        &cfg.games,
                                        live_procs.as_deref(),
                                        &last_procs,
                                    );
                                    let _ = finish_session(s, &mut obs, &app, FinalizeMode::DiscardTemp, "托盘跳过");
                                    noticed.clear();
                                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                    continue;
                                }
                                BatchDecision::LowDisk => {
                                    let s = sess.take().unwrap();
                                    let live_procs = win::processes().ok();
                                    record_suppressed_on_end(
                                        &mut suppressed,
                                        s.continue_after_exit,
                                        &s.current_game.name,
                                        &cfg.games,
                                        live_procs.as_deref(),
                                        &last_procs,
                                    );
                                    crate::notify(&app, "硬盘空间不足 1GB，已停止录制并保存已录部分");
                                    let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultPending, "低磁盘");
                                    noticed.clear();
                                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                    continue;
                                }
                                BatchDecision::None | BatchDecision::Choice { .. } => {}
                            }
                            // 4. 恢复前再次核验授权屏幕与桌面源画面仍可用
                            let Ok((live_mon, _)) = find_monitor(&monitor) else {
                                let s = sess.take().unwrap();
                                let live_procs = win::processes().ok();
                                record_suppressed_on_end(
                                    &mut suppressed,
                                    s.continue_after_exit,
                                    &s.current_game.name,
                                    &cfg.games,
                                    live_procs.as_deref(),
                                    &last_procs,
                                );
                                crate::notify(&app, "原游戏所在显示器在恢复前已断开，已按默认名保存已录内容");
                                let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "桌面显示器失效");
                                noticed.clear();
                                next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                continue;
                            };
                            if !obs.capture_video_ready(s.cap.as_ref()) {
                                let s = sess.take().unwrap();
                                let live_procs = win::processes().ok();
                                record_suppressed_on_end(
                                    &mut suppressed,
                                    s.continue_after_exit,
                                    &s.current_game.name,
                                    &cfg.games,
                                    live_procs.as_deref(),
                                    &last_procs,
                                );
                                crate::notify(&app, "桌面画面在恢复前失效，已按默认名保存已录内容");
                                let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "桌面画面失效");
                                noticed.clear();
                                next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                continue;
                            }
                            // 5. 恢复同一输出
                            if let Err(e) = unpause_output(&s.output) {
                                let s = sess.take().unwrap();
                                let live_procs = win::processes().ok();
                                record_suppressed_on_end(
                                    &mut suppressed,
                                    s.continue_after_exit,
                                    &s.current_game.name,
                                    &cfg.games,
                                    live_procs.as_deref(),
                                    &last_procs,
                                );
                                crate::notify(&app, &format!("恢复录制失败，已按默认名保存已录内容：{e}"));
                                let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, "恢复输出失败");
                                noticed.clear();
                                next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                continue;
                            }
                            s.handoff_backoff = None;
                            s.phase = Phase::Desktop {
                                monitor: live_mon.clone(),
                                last_game: last_game.clone(),
                                health: DesktopHealth::new(),
                            };
                            crate::set_status(
                                &app,
                                RecordingStatus::Desktop {
                                    from_game: last_game.clone(),
                                    monitor_name: live_mon.name.clone(),
                                },
                            );
                            log::info!("已确认桌面首帧并恢复录制桌面（{} 所在屏幕 {}）", last_game, live_mon.name);
                            if let Some(tx) = s.pending_continue_reply.take() {
                                let _ = tx.send(Ok(ChoiceOutcome::Continued));
                            }
                            next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                            continue;
                        }
                    }
                }
                Phase::Game => {
                    let Some(Ok(ref procs)) = procs_res else {
                        // 进程枚举失败不得误判为游戏退出
                        continue;
                    };
                    let pids = game_pids(&s.current_game, procs);
                    if pids.is_empty() {
                        if !s.continue_after_exit {
                            let s = sess.take().unwrap();
                            let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultPending, "游戏退出");
                            noticed.clear();
                            continue;
                        }
                        // 先暂停音画成功，再进入等待选择并显示弹窗
                        if let Err(e) = pause_output(&s.output) {
                            let s = sess.take().unwrap();
                            record_suppressed_on_end(
                                &mut suppressed,
                                s.continue_after_exit,
                                &s.current_game.name,
                                &cfg.games,
                                Some(procs),
                                &last_procs,
                            );
                            crate::notify(&app, &format!("暂停录制失败，已按默认名保存已录内容：{e}"));
                            let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultPending, "暂停失败");
                            noticed.clear();
                            continue;
                        }
                        let choice_id = next_choice_id;
                        next_choice_id += 1;
                        let last_game = s.current_game.name.clone();
                        let monitor = s.last_monitor.clone();
                        let default_name = render_name(&s.name_template, &s.first_game, s.start);
                        let out_dir = s.out_dir(&cfg.out_dir);
                        let active_choice = ActiveChoice {
                            session_id: s.id,
                            choice_id,
                            first_game: s.first_game.clone(),
                            last_game: last_game.clone(),
                            default_name,
                            out_dir,
                            monitor_name: monitor.as_ref().map(|m| m.name.clone()),
                        };
                        s.phase = Phase::WaitingChoice {
                            choice_id,
                            monitor,
                            last_game: last_game.clone(),
                        };
                        crate::set_active_choice(&app, Some(active_choice));
                        crate::set_status(&app, RecordingStatus::WaitingChoice { game: last_game.clone() });
                        crate::show_save_window(&app);
                        log::info!("{} 已退出，已暂停音画并等待选择（session={}, choice={}）", last_game, s.id, choice_id);
                        continue;
                    }

                    // 当前游戏仍在运行：其他已启用游戏打开时仅按原规则提醒
                    for g in cfg
                        .games
                        .iter()
                        .filter(|g| g.enabled && g.name != s.current_game.name && !suppressed.contains(&g.name))
                    {
                        if game_pids(g, procs).is_empty() {
                            noticed.remove(&g.name);
                        } else if noticed.insert(g.name.clone()) {
                            log::info!("{} 也开着，继续录 {}", g.name, s.current_game.name);
                            crate::toast(
                                &app,
                                &format!(
                                    "{} 也开着，正在录 {}。想改录 {}：托盘 → 结束录制并保存",
                                    g.name, s.current_game.name, g.name
                                ),
                            );
                        }
                    }

                    let wins = win::windows();
                    if let Some((target, hwnd)) = pick_from_wins(
                        &s.current_game,
                        s.current_scope,
                        &pids,
                        &wins,
                        |h| win::monitor_of_valid(h).map(|(m, _, _)| m.hmon),
                    ) {
                        // 即使窗口捕获模式下 HWND 没变，也刷新最后已知显示器（处理把窗口拖到副屏的情况）
                        if let Some((mon, _, _)) = win::monitor_of_valid(hwnd) {
                            s.last_monitor = Some(mon);
                        }
                        if s.cap.as_ref().is_none_or(|(t, _)| *t != target) {
                            if let Err(e) = obs.aim(&mut s.cap, target) {
                                log::warn!("切换捕获目标失败：{e}");
                            }
                        }
                    }
                }
                Phase::WaitingChoice { .. } => {
                    // 保持暂停，无超时；下一款游戏打开或切前台也不自行恢复
                }
                Phase::Desktop { mut monitor, last_game, mut health } => {
                    // 1. 周期验证授权屏幕身份与桌面捕获源实际健康，给短暂重连留宽限，持续失帧则按默认名保存
                    let monitor_res = find_monitor(&monitor);
                    let frame_ok = obs.capture_video_ready(s.cap.as_ref());
                    if let Ok((ref live_mon, _)) = monitor_res {
                        monitor.hmon = live_mon.hmon;
                        monitor.name = live_mon.name.clone();
                    }
                    let was_unhealthy = health.is_unhealthy();
                    if let Some(expired) = health.observe(now, monitor_res.is_ok(), frame_ok, DESKTOP_UNHEALTHY_GRACE) {
                        let s = sess.take().unwrap();
                        record_suppressed_on_end(
                            &mut suppressed,
                            s.continue_after_exit,
                            &last_game,
                            &cfg.games,
                            procs_res.as_ref().and_then(|r| r.as_deref().ok()),
                            &last_procs,
                        );
                        let (msg, log_reason) = match expired {
                            DesktopUnhealthyReason::MonitorMissing => (
                                "原游戏所在显示器已断开，已停止录制并按默认名保存已录内容",
                                "桌面显示器断开",
                            ),
                            DesktopUnhealthyReason::NoFrame => (
                                "原游戏所在屏幕持续无法采集画面，已停止录制并按默认名保存已录内容",
                                "桌面持续失帧",
                            ),
                        };
                        crate::notify(&app, msg);
                        let _ = finish_session(s, &mut obs, &app, FinalizeMode::DefaultSilent, log_reason);
                        noticed.clear();
                        next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                        continue;
                    }
                    if !was_unhealthy && health.is_unhealthy() {
                        log::warn!(
                            "桌面捕获健康异常（{:?}，屏幕 {}），宽限 {}s 等待重连",
                            health.reason,
                            monitor.name,
                            DESKTOP_UNHEALTHY_GRACE.as_secs()
                        );
                    } else if was_unhealthy && !health.is_unhealthy() {
                        log::info!("桌面捕获已恢复正常（屏幕 {}）", monitor.name);
                    }
                    let desktop_healthy_and_ready = monitor_res.is_ok() && frame_ok;
                    // 将更新后的 monitor / health 写回当前阶段
                    if let Phase::Desktop { monitor: ref mut m, health: ref mut h, .. } = s.phase {
                        *m = monitor.clone();
                        *h = health;
                    }
                    if !desktop_healthy_and_ready && s.prewarm.is_none() {
                        next_poll = now + UNHEALTHY_POLL_INTERVAL;
                    }

                    // 2. 游戏接力预热与切换
                    let Some(Ok(ref procs)) = procs_res else {
                        // 进程枚举失败或未到枚举周期：若已有候选超时仍按时清理，避免泄漏或卡死
                        if s.prewarm.as_ref().is_some_and(|c| now >= c.deadline) {
                            if let Some(cand) = s.prewarm.take() {
                                let target = cand.spec.target.clone();
                                let gname = cand.spec.game.name.clone();
                                obs.remove_prewarm_video(cand.video);
                                s.handoff_backoff =
                                    Some(HandoffBackoff { target, until: now + HANDOFF_RETRY_COOLDOWN });
                                log::warn!("接力候选 {} 预热超时，清理候选并保留桌面录制", gname);
                            }
                        }
                        continue;
                    };

                    let wins = win::windows();
                    let fg = win::foreground_window();
                    let desired = pick_foreground_handoff(
                        &s.games,
                        s.default_scope,
                        procs,
                        &wins,
                        fg,
                        |h| win::monitor_of_valid(h).map(|(m, _, _)| m.hmon),
                    )
                    .map(|(game, scope, target, hwnd)| HandoffSpec { game, scope, target, hwnd });

                    let prewarm_ready = s.prewarm.as_ref().is_some_and(|c| obs.prewarm_video_ready(&c.video));
                    match reconcile_prewarm(
                        now,
                        desired.as_ref(),
                        s.prewarm.as_ref().map(|c| &c.spec),
                        s.prewarm.as_ref().map(|c| c.deadline),
                        prewarm_ready,
                    ) {
                        PrewarmStep::KeepWaiting => {
                            next_poll = now + FAST_POLL_INTERVAL;
                            continue;
                        }
                        PrewarmStep::Timeout => {
                            if let Some(cand) = s.prewarm.take() {
                                let target = cand.spec.target.clone();
                                let gname = cand.spec.game.name.clone();
                                obs.remove_prewarm_video(cand.video);
                                s.handoff_backoff =
                                    Some(HandoffBackoff { target, until: now + HANDOFF_RETRY_COOLDOWN });
                                log::warn!(
                                    "接力候选 {} 在 {}s 内未取得可用画面，已清理候选并保留桌面录制，{}s 后重试",
                                    gname,
                                    HANDOFF_PREWARM_TIMEOUT.as_secs(),
                                    HANDOFF_RETRY_COOLDOWN.as_secs()
                                );
                            }
                            continue;
                        }
                        PrewarmStep::Commit => {
                            let cand = s.prewarm.take().unwrap();
                            let spec = cand.spec.clone();
                            let target_valid = match spec.target {
                                Target::Mon { hmon, .. } => find_monitor_by_hmon(hmon).is_ok(),
                                Target::Win { .. } => true,
                                Target::Desktop { .. } => false,
                            };
                            let commit_res = if target_valid {
                                obs.commit_prewarm(&mut s.cap, cand)
                            } else {
                                obs.remove_prewarm_video(cand.video);
                                Err("接力目标显示器已失效".into())
                            };
                            match commit_res {
                                Ok(()) => {
                                    if let Some((mon, _, _)) = win::monitor_of_valid(spec.hwnd) {
                                        s.last_monitor = Some(mon);
                                    }
                                    s.current_game = spec.game.clone();
                                    s.current_scope = spec.scope;
                                    s.handoff_backoff = None;
                                    s.phase = Phase::Game;
                                    noticed.clear();
                                    crate::set_status(&app, RecordingStatus::Game { game: spec.game.name.clone() });
                                    log::info!("候选首帧就绪，桌面接力切换至 {}（{:?}）", spec.game.name, spec.scope);
                                    crate::toast(&app, &format!("已接力录制 {}", spec.game.name));
                                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                }
                                Err(e) => {
                                    s.handoff_backoff = Some(HandoffBackoff {
                                        target: spec.target,
                                        until: now + HANDOFF_RETRY_COOLDOWN,
                                    });
                                    log::warn!("桌面接力提交 {} 失败，已回滚并维持桌面录制：{e}", spec.game.name);
                                }
                            }
                            continue;
                        }
                        PrewarmStep::Cancel => {
                            if let Some(cand) = s.prewarm.take() {
                                log::info!("接力候选 {} 已撤销或变化，清理预热源并保留桌面录制", cand.spec.game.name);
                                obs.remove_prewarm_video(cand.video);
                            }
                            // 继续往下检查新的 desired（如用户直接切到了另一款合格游戏）
                        }
                        PrewarmStep::NoPrewarm => {}
                    }

                    let desired_target_mon = desired.as_ref().and_then(|d| match d.target {
                        Target::Mon { hmon, .. } => win::monitor_of_valid(d.hwnd)
                            .map(|(m, _, _)| m)
                            .or_else(|| find_monitor_by_hmon(hmon).ok().map(|(m, _)| m)),
                        _ => None,
                    });

                    match plan_new_handoff(
                        now,
                        &monitor,
                        desktop_healthy_and_ready,
                        desired.as_ref(),
                        desired_target_mon.as_ref(),
                        s.handoff_backoff.as_ref(),
                    ) {
                        NewHandoffAction::KeepDesktop => {}
                        NewHandoffAction::CommitReuseDesktop(spec) => {
                            match obs.reuse_desktop_for_mon(&mut s.cap, spec.target.clone(), spec.audio_hwnd()) {
                                Ok(()) => {
                                    if let Some((mon, _, _)) = win::monitor_of_valid(spec.hwnd) {
                                        s.last_monitor = Some(mon);
                                    }
                                    s.current_game = spec.game.clone();
                                    s.current_scope = spec.scope;
                                    s.handoff_backoff = None;
                                    s.phase = Phase::Game;
                                    noticed.clear();
                                    crate::set_status(&app, RecordingStatus::Game { game: spec.game.name.clone() });
                                    log::info!("同屏全屏复用已就绪桌面画面，接力切换至 {}", spec.game.name);
                                    crate::toast(&app, &format!("已接力录制 {}", spec.game.name));
                                    next_poll = Instant::now() + NORMAL_POLL_INTERVAL;
                                }
                                Err(e) => {
                                    s.handoff_backoff = Some(HandoffBackoff {
                                        target: spec.target,
                                        until: now + HANDOFF_RETRY_COOLDOWN,
                                    });
                                    log::warn!("同屏全屏接力挂载 {} 音源失败，维持桌面录制：{e}", spec.game.name);
                                }
                            }
                        }
                        NewHandoffAction::StartPrewarm(spec) => {
                            match obs.start_prewarm(&spec.target) {
                                Ok(video) => {
                                    log::info!("开始预热接力候选 {}（{:?}，画布外仅视频）", spec.game.name, spec.scope);
                                    s.prewarm = Some(HandoffCandidate {
                                        spec,
                                        video,
                                        deadline: now + HANDOFF_PREWARM_TIMEOUT,
                                    });
                                    next_poll = now + FAST_POLL_INTERVAL;
                                }
                                Err(e) => {
                                    s.handoff_backoff = Some(HandoffBackoff {
                                        target: spec.target,
                                        until: now + HANDOFF_RETRY_COOLDOWN,
                                    });
                                    log::warn!("创建接力候选 {} 预热源失败，维持桌面录制：{e}", spec.game.name);
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }

        // 空闲状态：忽略托盘停止/跳过，把过期弹窗选择直接回复 Stale
        for cmd in cmds {
            if let Cmd::Choice { reply, .. } = cmd {
                reply_stale(reply);
            }
        }
        if !polled {
            continue;
        }

        let procs = match win::processes() {
            Ok(p) => {
                last_procs = p.clone();
                p
            }
            Err(e) => {
                log::warn!("空闲轮询枚举进程失败：{e}");
                continue;
            }
        };
        prune_suppressed(&mut suppressed, &cfg.games, &procs);

        let Some(game) = cfg
            .games
            .iter()
            .find(|g| g.enabled && !suppressed.contains(&g.name) && !game_pids(g, &procs).is_empty())
        else {
            continue;
        };
        // 等游戏有了窗口再开始（进程刚起来时往往还没窗口）
        let scope = game.scope.unwrap_or(cfg.scope);
        let wins = win::windows();
        let pids = game_pids(game, &procs);
        let Some((target, hwnd)) = pick_from_wins(game, scope, &pids, &wins, |h| {
            win::monitor_of_valid(h).map(|(m, _, _)| m.hmon)
        }) else {
            continue;
        };
        let Some((init_mon, mw, mh)) = win::monitor_of_valid(hwnd) else {
            continue;
        };

        let _ = fs::create_dir_all(&cfg.out_dir);
        if win::disk_free(&cfg.out_dir) < MIN_FREE_START {
            crate::notify(&app, &format!("硬盘剩余空间不足 5GB，本次不录制 {}", game.name));
            suppressed.insert(game.name.clone());
            continue;
        }
        let start = Local::now();
        let dir = tmp_root(&cfg).join(start.timestamp().to_string());
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("game.txt"), &game.name);
        let _ = fs::write(dir.join("tpl.txt"), &cfg.name_template);
        let session_id = next_session_id;
        next_session_id += 1;

        let mut cap = None;
        let began = obs
            .aim(&mut cap, target)
            .and_then(|_| obs.begin(&cfg, session_id, game, (mw, mh), &dir, muted.load(Relaxed)));
        let (output, mic, mic_muted) = match began {
            Ok(v) => v,
            Err(e) => {
                obs.drop_capture(&mut cap);
                crate::notify(&app, &format!("开始录制 {} 失败：{e}", game.name));
                suppressed.insert(game.name.clone());
                let _ = fs::remove_dir_all(&dir);
                continue;
            }
        };
        if let Err(e) = output.start() {
            obs.drop_capture(&mut cap);
            if let Some(m) = mic {
                let _ = obs.scene.remove_item(&m);
            }
            crate::notify(&app, &format!("开始录制 {} 失败：{e}", game.name));
            suppressed.insert(game.name.clone());
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        crate::set_status(&app, RecordingStatus::Game { game: game.name.clone() });
        crate::set_recording_dir(&app, Some(dir.clone()));
        log::info!("开始录制 {}（{:?}，进程 {:?}，跨游戏续录={}）", game.name, scope, pids, cfg.continue_after_exit);
        crate::toast(&app, &format!("开始录制 {}", game.name));
        sess = Some(Session {
            id: session_id,
            first_game: game.name.clone(),
            current_game: game.clone(),
            current_scope: scope,
            default_scope: cfg.scope,
            continue_after_exit: cfg.continue_after_exit,
            games: cfg.games.clone(),
            start,
            dir,
            name_template: cfg.name_template.clone(),
            output,
            cap,
            prewarm: None,
            handoff_backoff: None,
            pending_continue_reply: None,
            mic,
            mic_muted,
            last_monitor: Some(init_mon),
            phase: Phase::Game,
        });
    }
}

/// 把会话目录里的录像按默认名移到输出目录，记为待命名。返回是否加入了待命名队列。
/// 输出目录和模板都按开始录制时的：dir 是 <输出目录>/.aegis-tmp/<时间戳>，同盘改名，不会跨盘复制。
pub fn finalize(app: &AppHandle, dir: &Path, game: &str, start: DateTime<Local>) -> bool {
    let rec = dir.join(REC_FILE);
    if fs::metadata(&rec).map(|m| m.len()).unwrap_or(0) < 1024 {
        let _ = fs::remove_dir_all(dir);
        return false;
    }
    let cfg = crate::config_snapshot(app);
    let out_dir = dir
        .parent()
        .and_then(Path::parent)
        .map_or(cfg.out_dir.clone(), Path::to_path_buf);
    let tpl = fs::read_to_string(dir.join("tpl.txt")).unwrap_or(cfg.name_template);
    let out = unique_path(&out_dir, &render_name(&tpl, game, start));
    if let Err(e) = fs::rename(&rec, &out) {
        log::error!("移动录像失败 {} -> {}：{e}", rec.display(), out.display());
        crate::notify(app, &format!("保存录像失败，原始文件在 {}", rec.display()));
        false
    } else {
        let _ = fs::remove_dir_all(dir);
        log::info!("录像已保存：{}", out.display());
        crate::add_pending(app, out);
        true
    }
}

/// 直接落盘（可自定义文件名，或移入回收站），不写入待命名队列，不干扰已有待命名条目。
fn finalize_direct(
    app: &AppHandle,
    dir: &Path,
    first_game: &str,
    start: DateTime<Local>,
    custom_name: Option<&str>,
    recycle: bool,
) -> Result<(), String> {
    let rec = dir.join(REC_FILE);
    if fs::metadata(&rec).map(|m| m.len()).unwrap_or(0) < 1024 {
        let _ = fs::remove_dir_all(dir);
        return Ok(());
    }
    let cfg = crate::config_snapshot(app);
    let out_dir = dir
        .parent()
        .and_then(Path::parent)
        .map_or(cfg.out_dir.clone(), Path::to_path_buf);
    let tpl = fs::read_to_string(dir.join("tpl.txt")).unwrap_or(cfg.name_template);
    let stem = match custom_name {
        Some(n) => sanitize(n),
        None => render_name(&tpl, first_game, start),
    };
    let out = unique_path(&out_dir, &stem);
    if let Err(e) = fs::rename(&rec, &out) {
        log::error!("移动录像失败 {} -> {}：{e}", rec.display(), out.display());
        crate::notify(app, &format!("保存录像失败，原始文件在 {}", rec.display()));
        return Err(format!("保存录像失败：{e}"));
    }
    let _ = fs::remove_dir_all(dir);
    if recycle {
        if !win::recycle(&out) {
            log::error!("移到回收站失败：{}", out.display());
            crate::add_pending(app, out);
            return Err("移到回收站失败".into());
        }
        log::info!("录像已移入回收站：{}", out.display());
    } else {
        log::info!("录像已保存：{}", out.display());
    }
    Ok(())
}

/// 启动时把上次异常退出留下的会话目录收尾：当前输出目录，加上录制中途被改掉的那个旧目录。
pub fn recover(app: &AppHandle) {
    let cfg = crate::config_snapshot(app);
    let mut roots = vec![tmp_root(&cfg)];
    roots.extend(
        cfg.recording
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .filter(|r| r != &roots[0]),
    );
    crate::set_recording_dir(app, None);
    for e in roots.iter().flat_map(fs::read_dir).flatten().flatten() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn mk_game(name: &str, exes: &[&str], enabled: bool, scope: Option<Scope>) -> Game {
        Game {
            name: name.into(),
            exes: exes.iter().map(|s| s.to_string()).collect(),
            enabled,
            scope,
        }
    }

    fn mk_win(hwnd: isize, pid: u32, area: i64) -> Win {
        Win { hwnd, pid, title: format!("w-{hwnd}"), area }
    }

    fn mk_mon(hmon: isize, name: &str, device_id: Option<&str>) -> MonitorRef {
        MonitorRef {
            hmon,
            name: name.into(),
            device_id: device_id.map(str::to_string),
        }
    }

    #[test]
    fn target_mon_tracks_audio_hwnd_even_on_same_monitor() {
        let t1 = Target::Mon { hmon: 100, audio_hwnd: 1 };
        let t2 = Target::Mon { hmon: 100, audio_hwnd: 2 };
        assert_ne!(t1, t2);
        let td = Target::Desktop { monitor: mk_mon(100, r"\\.\DISPLAY1", None) };
        assert_ne!(t1, td);
    }

    #[test]
    fn lol_multi_process_priority_and_client_fallback() {
        let lol = mk_game("LOL", &["League of Legends.exe", "LeagueClientUx.exe"], true, Some(Scope::Window));
        let procs = vec![(10, "LeagueClientUx.exe".into()), (20, "League of Legends.exe".into())];
        let wins = vec![mk_win(1, 10, 1000), mk_win(2, 20, 500)];
        let pids = game_pids(&lol, &procs);
        // 对局窗口优先于客户端，即使客户端面积更大
        assert_eq!(
            pick_from_wins(&lol, Scope::Window, &pids, &wins, |_| Some(100)),
            Some((Target::Win { hwnd: 2 }, 2))
        );
        // 对局窗口退出，客户端仍在 -> 切回客户端，进程列表非空
        let procs2 = vec![(10, "LeagueClientUx.exe".into())];
        let pids2 = game_pids(&lol, &procs2);
        assert!(!pids2.is_empty());
        assert_eq!(
            pick_from_wins(&lol, Scope::Window, &pids2, &wins, |_| Some(100)),
            Some((Target::Win { hwnd: 1 }, 1))
        );
    }

    #[test]
    fn foreground_handoff_respects_enabled_eligible_priority_and_scope() {
        let games = vec![
            mk_game("LOL", &["League of Legends.exe", "LeagueClientUx.exe"], true, Some(Scope::Monitor)),
            mk_game("B", &["b.exe"], true, None),
            mk_game("C", &["c.exe"], false, Some(Scope::Window)),
            mk_game("B_DUP", &["b.exe"], true, Some(Scope::Monitor)),
        ];
        let procs = vec![
            (1, "LeagueClientUx.exe".into()),
            (2, "League of Legends.exe".into()),
            (3, "b.exe".into()),
            (4, "c.exe".into()),
            (5, "explorer.exe".into()),
        ];
        let wins = vec![mk_win(11, 1, 1000), mk_win(12, 2, 800), mk_win(13, 3, 900), mk_win(14, 4, 900)];

        // 停在桌面 / explorer -> 不接力
        assert!(pick_foreground_handoff(&games, Scope::Window, &procs, &wins, Some(999), |_| Some(1)).is_none());
        // 切到未启用的 C -> 不接力
        assert!(pick_foreground_handoff(&games, Scope::Window, &procs, &wins, Some(14), |_| Some(1)).is_none());
        // B 已在后台运行但前台不是 B -> 不接力
        assert!(pick_foreground_handoff(&games, Scope::Window, &procs, &wins, None, |_| Some(1)).is_none());
        // 切到 B：同窗口匹配 B 和 B_DUP，按列表顺序选 B，并继承全局 scope
        let (g, scope, target, hwnd) =
            pick_foreground_handoff(&games, Scope::Window, &procs, &wins, Some(13), |_| Some(77)).unwrap();
        assert_eq!(g.name, "B");
        assert_eq!(scope, Scope::Window);
        assert_eq!(target, Target::Win { hwnd: 13 });
        assert_eq!(hwnd, 13);
        // 切到 LOL 客户端前台，但对局窗口也开着：按 LOL 进程优先级选对局窗口，并用单游戏 scope 覆盖
        let (g, scope, target, hwnd) =
            pick_foreground_handoff(&games, Scope::Window, &procs, &wins, Some(11), |h| Some(if h == 12 { 200 } else { 100 })).unwrap();
        assert_eq!(g.name, "LOL");
        assert_eq!(scope, Scope::Monitor);
        assert_eq!(target, Target::Mon { hmon: 200, audio_hwnd: 12 });
        assert_eq!(hwnd, 12);
    }

    #[test]
    fn suppression_multi_game_and_legacy_modes() {
        let games = vec![
            mk_game("A", &["a.exe"], true, None),
            mk_game("B", &["b.exe"], true, None),
            mk_game("C", &["c.exe"], false, None),
            mk_game("D", &["d.exe"], true, None),
        ];
        // 开启新模式：终止时 A、B、C 在运行（C 未启用），D 未运行
        let mut sup = HashSet::new();
        let procs = vec![(1, "a.exe".into()), (2, "b.exe".into()), (3, "c.exe".into())];
        record_suppressed_on_end(&mut sup, true, "A", &games, Some(&procs), &[]);
        assert!(sup.contains("A"));
        assert!(sup.contains("B"));
        assert!(!sup.contains("C"));
        assert!(!sup.contains("D"));

        // A 退出后仅解除 A，B 仍受抑制
        let procs_after_a_exit = vec![(2, "b.exe".into())];
        prune_suppressed(&mut sup, &games, &procs_after_a_exit);
        assert!(!sup.contains("A"));
        assert!(sup.contains("B"));

        // B 退出后解除 B
        prune_suppressed(&mut sup, &games, &[]);
        assert!(sup.is_empty());

        // 关闭开关：沿用原有单游戏抑制
        let mut legacy = HashSet::new();
        record_suppressed_on_end(&mut legacy, false, "A", &games, Some(&procs), &[]);
        assert_eq!(legacy, HashSet::from(["A".to_string()]));
    }

    #[test]
    fn batch_priority_termination_wins_and_duplicate_or_stale_choices_rejected() {
        // 终止命令优先于同轮到达的继续命令
        let (tx1, rx1) = sync_channel(1);
        let d = resolve_batch(
            vec![
                Cmd::Choice { session_id: 1, choice_id: 10, action: ChoiceAction::Continue, reply: Some(tx1) },
                Cmd::Stop,
            ],
            false,
            1,
            Some(10),
        );
        assert!(matches!(d, BatchDecision::Stop));
        assert_eq!(rx1.recv().unwrap().unwrap(), ChoiceOutcome::Stale);

        // 低磁盘优先于同轮到达的继续命令
        let (tx2, rx2) = sync_channel(1);
        let d = resolve_batch(
            vec![Cmd::Choice { session_id: 1, choice_id: 10, action: ChoiceAction::Continue, reply: Some(tx2) }],
            true,
            1,
            Some(10),
        );
        assert!(matches!(d, BatchDecision::LowDisk));
        assert_eq!(rx2.recv().unwrap().unwrap(), ChoiceOutcome::Stale);

        // 重复点击与过期 choice_id：只接受首个匹配项
        let (tx_old, rx_old) = sync_channel(1);
        let (tx_ok, _rx_ok) = sync_channel(1);
        let (tx_dup, rx_dup) = sync_channel(1);
        let d = resolve_batch(
            vec![
                Cmd::Choice { session_id: 1, choice_id: 9, action: ChoiceAction::Continue, reply: Some(tx_old) },
                Cmd::Choice { session_id: 1, choice_id: 10, action: ChoiceAction::Continue, reply: Some(tx_ok) },
                Cmd::Choice { session_id: 1, choice_id: 10, action: ChoiceAction::Save("x".into()), reply: Some(tx_dup) },
            ],
            false,
            1,
            Some(10),
        );
        assert!(matches!(d, BatchDecision::Choice { action: ChoiceAction::Continue, .. }));
        assert_eq!(rx_old.recv().unwrap().unwrap(), ChoiceOutcome::Stale);
        assert_eq!(rx_dup.recv().unwrap().unwrap(), ChoiceOutcome::Stale);
    }

    #[test]
    fn monitor_match_never_falls_back_to_other_screen() {
        let m1 = mk_mon(1, r"\\.\DISPLAY1", None);
        let m2 = mk_mon(2, r"\\.\DISPLAY2", None);
        // 句柄和设备名都可能被复用，无法核验设备身份时不能录桌面。
        assert_eq!(match_monitor_index(&[m1.clone(), m2.clone()], &m2), None);
        // 副屏断开后只剩主屏 -> 返回 None，绝不回退到主屏
        assert_eq!(match_monitor_index(&[m1.clone()], &m2), None);
        // 句柄恰好复用但设备名变了 -> 仍返回 None
        let reused = mk_mon(2, r"\\.\DISPLAY3", None);
        assert_eq!(match_monitor_index(&[m1, reused], &m2), None);

        // 有 PnP device_id 时按硬件身份严格核验：即使新显示器复用了 hmon 和 \\.\DISPLAY2 也绝不误匹配
        let p1 = mk_mon(1, r"\\.\DISPLAY1", Some(r"\\?\DISPLAY#AAA#1"));
        let p2 = mk_mon(2, r"\\.\DISPLAY2", Some(r"\\?\DISPLAY#BBB#2"));
        let imposter = mk_mon(2, r"\\.\DISPLAY2", Some(r"\\?\DISPLAY#CCC#3"));
        assert_eq!(match_monitor_index(&[p1.clone(), imposter], &p2), None);
        // 同一物理显示器短暂重连后 hmon/名变化但 device_id 不变 -> 正确识别回原屏
        let reconnected = mk_mon(99, r"\\.\DISPLAY4", Some(r"\\?\DISPLAY#BBB#2"));
        assert_eq!(match_monitor_index(&[p1, reconnected], &p2), Some(1));
    }

    #[test]
    fn preparing_desktop_waits_for_frame_and_aborts_on_timeout_or_monitor_loss() {
        let t0 = Instant::now();
        let dl = t0 + DESKTOP_READY_TIMEOUT;
        // 源已创建但尚无首帧（0x0） -> 保持等待（输出维持暂停）
        assert_eq!(evaluate_preparing_desktop(t0, dl, true, false), PreparingDesktopDecision::Wait);
        assert_eq!(
            evaluate_preparing_desktop(t0 + Duration::from_secs(4), dl, true, false),
            PreparingDesktopDecision::Wait
        );
        // 限时内取得首帧 -> Ready，允许隐藏弹窗并恢复输出
        assert_eq!(
            evaluate_preparing_desktop(t0 + Duration::from_millis(200), dl, true, true),
            PreparingDesktopDecision::Ready
        );
        // 超过限时仍无画面 -> TimedOut，安全按默认名收尾
        assert_eq!(evaluate_preparing_desktop(dl, dl, true, false), PreparingDesktopDecision::TimedOut);
        // 原屏失效 -> 立刻 MonitorLost，绝不切到其他屏
        assert_eq!(
            evaluate_preparing_desktop(t0 + Duration::from_millis(100), dl, false, false),
            PreparingDesktopDecision::MonitorLost
        );
        // 等待桌面首帧期间（waiting_choice_id == None），Stop/Skip/低磁盘仍优先终止，重复选择被拒绝
        let (tx, rx) = sync_channel(1);
        let d = resolve_batch(
            vec![
                Cmd::Choice { session_id: 1, choice_id: 10, action: ChoiceAction::Continue, reply: Some(tx) },
                Cmd::Stop,
            ],
            false,
            1,
            None,
        );
        assert!(matches!(d, BatchDecision::Stop));
        assert_eq!(rx.recv().unwrap().unwrap(), ChoiceOutcome::Stale);
    }

    #[test]
    fn desktop_health_grace_recovers_brief_reconnect_and_terminates_persistent_loss() {
        let t0 = Instant::now();
        let mut h = DesktopHealth::new();
        assert!(!h.is_unhealthy());

        // 正常健康
        assert_eq!(h.observe(t0, true, true, DESKTOP_UNHEALTHY_GRACE), None);
        assert!(!h.is_unhealthy());

        // 短暂失帧（例如模式切换 / WGC 3s 重连周期内）：宽限期内不终止，恢复后清零计时
        assert_eq!(h.observe(t0 + Duration::from_secs(1), true, false, DESKTOP_UNHEALTHY_GRACE), None);
        assert!(h.is_unhealthy());
        assert_eq!(h.observe(t0 + Duration::from_secs(5), true, false, DESKTOP_UNHEALTHY_GRACE), None);
        assert_eq!(h.observe(t0 + Duration::from_secs(6), true, true, DESKTOP_UNHEALTHY_GRACE), None);
        assert!(!h.is_unhealthy());

        // 持续失帧超过宽限期 -> 返回 NoFrame 触发安全默认名收尾
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(h.observe(t1, true, false, DESKTOP_UNHEALTHY_GRACE), None);
        assert_eq!(
            h.observe(t1 + DESKTOP_UNHEALTHY_GRACE, true, false, DESKTOP_UNHEALTHY_GRACE),
            Some(DesktopUnhealthyReason::NoFrame)
        );

        // 原屏持续断开超过宽限期 -> 返回 MonitorMissing
        let mut h2 = DesktopHealth::new();
        assert_eq!(h2.observe(t1, false, false, DESKTOP_UNHEALTHY_GRACE), None);
        assert_eq!(
            h2.observe(t1 + Duration::from_secs(3), false, false, DESKTOP_UNHEALTHY_GRACE),
            None
        );
        assert_eq!(
            h2.observe(t1 + DESKTOP_UNHEALTHY_GRACE, false, false, DESKTOP_UNHEALTHY_GRACE),
            Some(DesktopUnhealthyReason::MonitorMissing)
        );
    }

    #[test]
    fn handoff_prewarm_not_ready_timeout_revoke_commit_and_same_screen_reuse() {
        let t0 = Instant::now();
        let desk_mon = mk_mon(100, r"\\.\DISPLAY1", Some(r"\\?\DISPLAY#M1"));
        let other_mon = mk_mon(200, r"\\.\DISPLAY2", Some(r"\\?\DISPLAY#M2"));
        let game_b = mk_game("B", &["b.exe"], true, Some(Scope::Window));
        let spec_win = HandoffSpec {
            game: game_b.clone(),
            scope: Scope::Window,
            target: Target::Win { hwnd: 55 },
            hwnd: 55,
        };

        // 1. 窗口捕获接力：先启动预热（不直接切）
        assert_eq!(
            plan_new_handoff(t0, &desk_mon, true, Some(&spec_win), None, None),
            NewHandoffAction::StartPrewarm(spec_win.clone())
        );

        // 2. 候选预热未就绪 -> 维持等待，旧桌面保持可用
        let dl = t0 + HANDOFF_PREWARM_TIMEOUT;
        assert_eq!(
            reconcile_prewarm(t0 + Duration::from_secs(2), Some(&spec_win), Some(&spec_win), Some(dl), false),
            PrewarmStep::KeepWaiting
        );

        // 3. 候选预热超时 -> Timeout（清理候选、保留旧桌面并进入冷却），冷却期内不频繁重建
        assert_eq!(
            reconcile_prewarm(dl, Some(&spec_win), Some(&spec_win), Some(dl), false),
            PrewarmStep::Timeout
        );
        let backoff = HandoffBackoff {
            target: spec_win.target.clone(),
            until: dl + HANDOFF_RETRY_COOLDOWN,
        };
        assert_eq!(
            plan_new_handoff(dl + Duration::from_secs(1), &desk_mon, true, Some(&spec_win), None, Some(&backoff)),
            NewHandoffAction::KeepDesktop
        );
        assert_eq!(
            plan_new_handoff(
                dl + HANDOFF_RETRY_COOLDOWN,
                &desk_mon,
                true,
                Some(&spec_win),
                None,
                Some(&backoff)
            ),
            NewHandoffAction::StartPrewarm(spec_win.clone())
        );

        // 4. 预热期间候选被撤销（用户切回桌面或切到别的游戏） -> Cancel（立即清理候选）
        assert_eq!(
            reconcile_prewarm(t0 + Duration::from_secs(1), None, Some(&spec_win), Some(dl), true),
            PrewarmStep::Cancel
        );

        // 5. 候选首次就绪且仍是有效目标 -> Commit
        assert_eq!(
            reconcile_prewarm(t0 + Duration::from_millis(300), Some(&spec_win), Some(&spec_win), Some(dl), true),
            PrewarmStep::Commit
        );

        // 6. 同屏全屏接力：桌面已就绪可直接复用原桌面画面；若桌面当前失帧则暂不接力
        let spec_same_mon = HandoffSpec {
            game: mk_game("C", &["c.exe"], true, Some(Scope::Monitor)),
            scope: Scope::Monitor,
            target: Target::Mon { hmon: 100, audio_hwnd: 88 },
            hwnd: 88,
        };
        assert_eq!(
            plan_new_handoff(t0, &desk_mon, true, Some(&spec_same_mon), Some(&desk_mon), None),
            NewHandoffAction::CommitReuseDesktop(spec_same_mon.clone())
        );
        assert_eq!(
            plan_new_handoff(t0, &desk_mon, false, Some(&spec_same_mon), Some(&desk_mon), None),
            NewHandoffAction::KeepDesktop
        );

        // 7. 异屏全屏接力：不能复用原桌面，走候选预热
        let spec_other_mon = HandoffSpec {
            game: mk_game("D", &["d.exe"], true, Some(Scope::Monitor)),
            scope: Scope::Monitor,
            target: Target::Mon { hmon: 200, audio_hwnd: 99 },
            hwnd: 99,
        };
        assert_eq!(
            plan_new_handoff(t0, &desk_mon, true, Some(&spec_other_mon), Some(&other_mon), None),
            NewHandoffAction::StartPrewarm(spec_other_mon)
        );
    }

    /// 实机 OBS 验证（需要 obs-runtime 目录存在）：
    /// 验证 FragmentedMP4 (ffmpeg_muxer) 连续暂停、恢复、暂停中停止，并检查产出 MP4 的封装结构与音画时长。
    #[test]
    #[ignore]
    fn real_obs_fragmented_mp4_pause_resume_and_stop_while_paused() {
        use libobs_wrapper::utils::StartupPaths;
        use std::thread;

        let runtime_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../obs-runtime");
        assert!(runtime_dir.join("data/libobs").exists(), "obs-runtime not found at {}", runtime_dir.display());
        let paths = StartupPaths::new(
            ObsPath::new(&runtime_dir.join("data/libobs").to_string_lossy()),
            ObsPath::new(&runtime_dir.join("obs-plugins/64bit").to_string_lossy()),
            ObsPath::new(&runtime_dir.join("data/obs-plugins/%module%").to_string_lossy()),
        );

        let tmp = std::env::temp_dir().join(format!("aegis-obs-pause-test-{}", Local::now().timestamp_millis()));
        let conf_dir = tmp.join("conf");
        let rec_dir = tmp.join("rec");
        fs::create_dir_all(&conf_dir).unwrap();
        fs::create_dir_all(&rec_dir).unwrap();

        let canvas = (1920, 1080);
        let conf = ObsPath::new(&format!("{}/", conf_dir.join("obs").to_string_lossy()));
        let mut ctx = StartupInfo::new()
            .set_startup_paths(paths)
            .set_video_info(video_info(canvas))
            .set_module_config_path(conf)
            .start()
            .expect("OBS start failed");
        let scene = ctx.scene("main", Some(0)).expect("scene failed");
        let mut obs = Obs { ctx, scene, canvas, src_seq: 0 };

        let (mon_ref, mon) = list_monitors().expect("list_monitors failed").into_iter().next().expect("no monitor");
        let mut cap = None;
        obs.aim(&mut cap, Target::Desktop { monitor: mon_ref }).expect("aim desktop failed");

        let mut cfg = Config::new(tmp.clone());
        cfg.resolution = 720;
        cfg.mic_enabled = false;
        let g = mk_game("PauseTest", &[], true, Some(Scope::Monitor));
        let (output, mic, _) = obs.begin(&cfg, 999, &g, (mon.width, mon.height), &rec_dir, false).expect("begin failed");

        output.start().expect("output start failed");
        thread::sleep(Duration::from_millis(1200));

        // 第一次暂停（等待 1.5s，期间不应继续写入音画）
        pause_output(&output).expect("first pause failed");
        assert!(output.is_paused().unwrap());
        let size_at_pause_1 = fs::metadata(rec_dir.join(REC_FILE)).map(|m| m.len()).unwrap_or(0);
        thread::sleep(Duration::from_millis(1500));
        let size_after_wait_1 = fs::metadata(rec_dir.join(REC_FILE)).map(|m| m.len()).unwrap_or(0);

        // 恢复录制（再录 1.2s）
        unpause_output(&output).expect("first unpause failed");
        assert!(!output.is_paused().unwrap());
        thread::sleep(Duration::from_millis(1200));

        // 第二次暂停（等待 1.0s），并在暂停状态下直接停止
        pause_output(&output).expect("second pause failed");
        assert!(output.is_paused().unwrap());
        let size_at_pause_2 = fs::metadata(rec_dir.join(REC_FILE)).map(|m| m.len()).unwrap_or(0);
        thread::sleep(Duration::from_millis(1000));
        let size_after_wait_2 = fs::metadata(rec_dir.join(REC_FILE)).map(|m| m.len()).unwrap_or(0);

        stop_output_safely(&output).expect("stop while paused failed");
        assert!(!output.is_active().unwrap());

        obs.drop_capture(&mut cap);
        if let Some(m) = mic {
            let _ = obs.scene.remove_item(&m);
        }

        let mp4 = fs::read(rec_dir.join(REC_FILE)).expect("read mp4 failed");
        assert!(mp4.len() > 4096, "mp4 too small: {}", mp4.len());
        let has_box = |tag: &[u8; 4]| mp4.windows(4).any(|w| w == tag);
        assert!(has_box(b"ftyp"), "missing ftyp box");
        assert!(has_box(b"moov"), "missing moov box");
        assert!(has_box(b"mdat"), "missing mdat box");

        // 解析 mdhd（每条音/视频轨道的 timescale 与 duration）以验证暂停期间未写入且音画时长对齐
        let mut track_durations_sec = Vec::new();
        let mut i = 0usize;
        while i + 8 <= mp4.len() {
            if &mp4[i + 4..i + 8] == b"mdhd" {
                let sz = u32::from_be_bytes(mp4[i..i + 4].try_into().unwrap()) as usize;
                if sz >= 32 && i + sz <= mp4.len() {
                    let version = mp4[i + 8];
                    let (ts, dur) = if version == 0 {
                        let ts = u32::from_be_bytes(mp4[i + 20..i + 24].try_into().unwrap()) as u64;
                        let dur = u32::from_be_bytes(mp4[i + 24..i + 28].try_into().unwrap()) as u64;
                        (ts, dur)
                    } else if version == 1 && sz >= 44 {
                        let ts = u32::from_be_bytes(mp4[i + 28..i + 32].try_into().unwrap()) as u64;
                        let dur = u64::from_be_bytes(mp4[i + 32..i + 40].try_into().unwrap());
                        (ts, dur)
                    } else {
                        (0, 0)
                    };
                    if ts > 0 {
                        track_durations_sec.push(dur as f64 / ts as f64);
                    }
                }
            }
            i += 1;
        }
        println!(
            "OBS pause/resume/stop-while-paused OK: total={} bytes, tracks_sec={:?}, pause1={}->{}, pause2={}->{}",
            mp4.len(),
            track_durations_sec,
            size_at_pause_1,
            size_after_wait_1,
            size_at_pause_2,
            size_after_wait_2,
        );
        assert!(track_durations_sec.len() >= 2, "expected video + audio tracks, got {track_durations_sec:?}");
        for &d in &track_durations_sec {
            // 实际录制 1.2s + 1.2s = 2.4s，总挂钟 ~4.9s（含 2.5s 暂停）：轨道时长应接近 2.4s，绝不应包含暂停时长
            assert!((1.6..=3.2).contains(&d), "unexpected track duration {d:.3}s (wall ~4.9s, rec ~2.4s)");
        }
        let av_diff = (track_durations_sec[0] - track_durations_sec[1]).abs();
        assert!(av_diff < 0.15, "A/V duration drift too large: {av_diff:.3}s ({track_durations_sec:?})");
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 实机 OBS 32.1.2 验证（单屏本地环境，需要 obs-runtime 目录存在）：
    /// 1) 输出暂停期间准备桌面源（使用 PnP device_id）：刚创建时 0x0，随后在有限期限内于暂停态收到首帧 (>0x>0)；
    /// 2) 隐藏 scene item 会令 monitor_capture 停止采集并复位为 0x0；
    /// 3) 无效显示器 / 不可达窗口源虽创建成功但持续 0x0（不误判为就绪）；
    /// 4) 画布外可见预热候选在画布外即可就绪 (>0x>0)，期间旧桌面保持健康；
    /// 5) 撤销/超时/成功接力及最终清理后 scene.managed_sources() 计数精确归零（无源泄漏）。
    #[test]
    #[ignore]
    fn real_obs_p1_readiness_offcanvas_prewarm_isolation_and_zero_leak() {
        use libobs_wrapper::utils::StartupPaths;
        use std::thread;

        let runtime_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../obs-runtime");
        assert!(runtime_dir.join("data/libobs").exists(), "obs-runtime not found at {}", runtime_dir.display());
        let paths = StartupPaths::new(
            ObsPath::new(&runtime_dir.join("data/libobs").to_string_lossy()),
            ObsPath::new(&runtime_dir.join("obs-plugins/64bit").to_string_lossy()),
            ObsPath::new(&runtime_dir.join("data/obs-plugins/%module%").to_string_lossy()),
        );

        let tmp = std::env::temp_dir().join(format!("aegis-obs-p1-green-{}", Local::now().timestamp_millis()));
        let conf_dir = tmp.join("conf");
        let rec_dir = tmp.join("rec");
        fs::create_dir_all(&conf_dir).unwrap();
        fs::create_dir_all(&rec_dir).unwrap();

        let canvas = (1920, 1080);
        let conf = ObsPath::new(&format!("{}/", conf_dir.join("obs").to_string_lossy()));
        let mut ctx = StartupInfo::new()
            .set_startup_paths(paths)
            .set_video_info(video_info(canvas))
            .set_module_config_path(conf)
            .start()
            .expect("OBS start failed");
        let scene = ctx.scene("main", Some(0)).expect("scene failed");
        let mut obs = Obs { ctx, scene, canvas, src_seq: 0 };

        let (mon_ref, mon) = list_monitors().expect("list_monitors failed").into_iter().next().expect("no monitor");
        println!(
            "Using monitor: name={}, hmon={}, device_id={:?}, obs_id={}",
            mon_ref.name,
            mon_ref.hmon,
            mon_ref.device_id,
            mon_ref.obs_monitor_id()
        );

        // 先启动输出并暂停，验证「暂停期间」准备桌面源也能拿到首帧
        let mut cfg = Config::new(tmp.clone());
        cfg.resolution = 720;
        cfg.mic_enabled = false;
        let g = mk_game("P1Test", &[], true, Some(Scope::Monitor));
        let (output, mic, _) = obs.begin(&cfg, 1001, &g, (mon.width, mon.height), &rec_dir, false).expect("begin failed");
        output.start().expect("output start failed");
        thread::sleep(Duration::from_millis(300));
        pause_output(&output).expect("pause failed");
        assert!(output.is_paused().unwrap());

        let mut cap = None;
        obs.prepare_desktop_capture(&mut cap, &mon_ref).expect("prepare_desktop_capture failed");
        let wait_start = Instant::now();
        let mut desktop_ready = false;
        while wait_start.elapsed() < DESKTOP_READY_TIMEOUT {
            if obs.capture_video_ready(cap.as_ref()) {
                desktop_ready = true;
                break;
            }
            thread::sleep(FAST_POLL_INTERVAL);
        }
        assert!(desktop_ready, "desktop capture failed to become ready while output was paused");
        assert!(output.is_paused().unwrap(), "output must remain paused while waiting for desktop readiness");
        let ready_ms = wait_start.elapsed().as_millis();
        println!("1) Paused desktop readiness confirmed in {ready_ms}ms");

        // 恢复输出进入桌面段
        unpause_output(&output).expect("unpause failed");

        // 2) 验证无效显示器源创建成功但持续 0x0（证明创建成功不等于就绪），移除后无泄漏
        let bogus_mon = obs.add_monitor_source("bogus-mon", "DUMMY_MISSING_MONITOR_ID", true).expect("bogus create");
        thread::sleep(Duration::from_millis(250));
        assert!(
            !is_source_frame_ready(bogus_mon.inner_source()),
            "missing monitor source must stay not-ready (0x0)"
        );
        obs.scene.remove_item(&bogus_mon).expect("remove bogus mon");
        assert_eq!(obs.scene.managed_sources().unwrap().len(), 1, "expected only desktop source after bogus removal");
        println!("2) Bogus monitor stayed 0x0 and cleaned up cleanly");

        // 3) 验证不可达候选窗口预热（画布外、无音频）持续 0x0 且旧桌面保持就绪；撤销后无泄漏
        let bad_win = obs
            .add_window_source("prewarm-bad-win", "NoSuchWindow:NoSuchClass:nosuch.exe", true)
            .expect("bad win create");
        let bad_pos = bad_win.get_source_position().unwrap();
        assert!(*bad_pos.x() > obs.canvas.0 as f32, "prewarm must be off-canvas, got {bad_pos:?}");
        thread::sleep(Duration::from_millis(250));
        assert!(!is_source_frame_ready(bad_win.inner_source()), "unreachable window must stay not-ready");
        assert!(obs.capture_video_ready(cap.as_ref()), "old desktop must stay ready while bad candidate prewarms");
        obs.scene.remove_item(&bad_win).expect("remove bad win");
        assert_eq!(obs.scene.managed_sources().unwrap().len(), 1, "bad candidate must not leak");
        println!("3) Unreachable window prewarm stayed off-canvas & 0x0 while old desktop remained ready");

        // 4) 验证简单隐藏 (set_visible(false)) 会使 monitor_capture 停止采集并复位为 0x0，
        //    而画布外可见 (visible=true + place_off_canvas) 能在不进入主画布的情况下真正取到首帧！
        let prewarm_mon = obs
            .add_monitor_source("prewarm-ok-mon", mon_ref.obs_monitor_id(), true)
            .expect("prewarm monitor create");
        let pw_pos = prewarm_mon.get_source_position().unwrap();
        assert!(*pw_pos.x() > obs.canvas.0 as f32, "prewarm monitor must be off-canvas, got {pw_pos:?}");
        let pw_start = Instant::now();
        let mut pw_ready = false;
        while pw_start.elapsed() < HANDOFF_PREWARM_TIMEOUT {
            if is_source_frame_ready(prewarm_mon.inner_source()) {
                pw_ready = true;
                break;
            }
            thread::sleep(FAST_POLL_INTERVAL);
        }
        assert!(pw_ready, "off-canvas visible prewarm monitor should deliver frames");
        let (showing_on, w_on, h_on) = source_frame_size(prewarm_mon.inner_source()).unwrap();
        println!("4a) Off-canvas visible prewarm ready in {}ms: showing={showing_on}, {w_on}x{h_on}", pw_start.elapsed().as_millis());

        // 隐藏后应复位为 showing=false, 0x0
        prewarm_mon.set_visible(false).unwrap();
        let hide_start = Instant::now();
        let mut reset_to_zero = false;
        while hide_start.elapsed() < Duration::from_secs(2) {
            if let Ok((false, 0, 0)) = source_frame_size(prewarm_mon.inner_source()) {
                reset_to_zero = true;
                break;
            }
            thread::sleep(FAST_POLL_INTERVAL);
        }
        assert!(reset_to_zero, "hiding scene item should stop monitor_capture and reset size to 0x0");
        println!("4b) Confirmed hiding scene item resets monitor_capture to (showing=false, 0x0)");

        // 重新显示（仍在画布外）并等其再次就绪，然后提升到画布并移除旧桌面，验证零泄漏
        prewarm_mon.set_visible(true).unwrap();
        let reshow_start = Instant::now();
        let mut reshow_ready = false;
        while reshow_start.elapsed() < HANDOFF_PREWARM_TIMEOUT {
            if is_source_frame_ready(prewarm_mon.inner_source()) {
                reshow_ready = true;
                break;
            }
            thread::sleep(FAST_POLL_INTERVAL);
        }
        assert!(reshow_ready, "off-canvas prewarm should recover after set_visible(true)");
        // 5) 验证 commit_prewarm / reuse_desktop_for_mon 在音源挂载失败时安全回滚、保留旧桌面且不泄漏源，
        //    再验证将已就绪画布外候选提升到主画布并移除旧桌面后零泄漏
        let failed_commit = obs.commit_prewarm(
            &mut cap,
            HandoffCandidate {
                spec: HandoffSpec {
                    game: mk_game("BadAudio", &["none.exe"], true, Some(Scope::Monitor)),
                    scope: Scope::Monitor,
                    target: Target::Mon { hmon: mon_ref.hmon, audio_hwnd: 0 },
                    hwnd: 0,
                },
                video: PrewarmVideo::Mon(prewarm_mon),
                deadline: Instant::now() + HANDOFF_PREWARM_TIMEOUT,
            },
        );
        assert!(failed_commit.is_err(), "commit_prewarm with invalid audio_hwnd must fail");
        assert!(obs.capture_video_ready(cap.as_ref()), "old desktop must remain intact after failed commit");
        assert_eq!(obs.scene.managed_sources().unwrap().len(), 1, "failed commit must clean up candidate video");
        assert!(
            obs.reuse_desktop_for_mon(&mut cap, Target::Mon { hmon: mon_ref.hmon, audio_hwnd: 0 }, 0).is_err(),
            "reuse_desktop_for_mon with invalid audio_hwnd must fail"
        );
        assert!(obs.capture_video_ready(cap.as_ref()), "old desktop must remain intact after failed reuse");

        let prewarm_ok2 = obs
            .add_monitor_source("prewarm-ok-mon-2", mon_ref.obs_monitor_id(), true)
            .expect("prewarm monitor 2 create");
        let pw2_start = Instant::now();
        let mut pw2_ready = false;
        while pw2_start.elapsed() < HANDOFF_PREWARM_TIMEOUT {
            if is_source_frame_ready(prewarm_ok2.inner_source()) {
                pw2_ready = true;
                break;
            }
            thread::sleep(FAST_POLL_INTERVAL);
        }
        assert!(pw2_ready, "second off-canvas prewarm should become ready");
        prewarm_ok2.fit_source_to_screen().unwrap();
        prewarm_ok2.move_order(ObsOrderMovement::Top).unwrap();
        let promoted_pos = prewarm_ok2.get_source_position().unwrap();
        assert_eq!(*promoted_pos.x(), 0.0);
        assert_eq!(*promoted_pos.y(), 0.0);
        obs.drop_capture(&mut cap);
        assert_eq!(obs.scene.managed_sources().unwrap().len(), 1, "old desktop must be removed without leak");
        obs.scene.remove_item(&prewarm_ok2).unwrap();
        assert_eq!(obs.scene.managed_sources().unwrap().len(), 0, "all sources must be released without leak");
        println!("5) Verified failed commit/reuse rollback, off-canvas promotion to (0,0), and 0 leaked sources");

        stop_output_safely(&output).expect("stop output failed");
        if let Some(m) = mic {
            let _ = obs.scene.remove_item(&m);
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
