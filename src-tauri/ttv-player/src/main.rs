#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(non_snake_case)]

#[no_mangle]
pub static NvOptimusEnablement: u32 = 1;
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: i32 = 1;

mod dcomp;
mod enhance;
mod interp;
mod ipc;
mod mpv;
mod prefs;
mod render;
mod selftest;
mod webview_hud;

use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{PtInRect, ScreenToClient};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_LBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, IsIconic, IsZoomed,
    KillTimer, LoadImageW, PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    GWLP_USERDATA, HICON, HTCLIENT, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, MINMAXINFO, MSG,
    NCCALCSIZE_PARAMS, SIZE_MINIMIZED, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_MAXIMIZE, SW_MINIMIZE,
    SW_RESTORE, SW_SHOW, SW_SHOWMAXIMIZED, WM_APP, WM_CAPTURECHANGED, WM_CLOSE, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDOWN,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SETICON, WM_SHOWWINDOW, WM_SIZE,
    WM_TIMER, WNDCLASSEXW, WS_CLIPCHILDREN, WS_EX_APPWINDOW, WS_EX_NOREDIRECTIONBITMAP,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
};

use dcomp::{
    clamp_window_to_monitor, client_size_px, compute_minmax_bounds, enter_borderless_fullscreen,
    exit_borderless_fullscreen, init_per_monitor_dpi, monitor_areas, monitor_refresh_hz,
    monitor_work_area, monitor_work_area_at_point, nchittest_resize_border, set_dwm_rounded_corners,
    set_window_topmost, DCompVideoHost, SavedFullscreenRect,
};
use interp::{CompiledShaderBlobs, InterpContentMode};
use ipc::{
    value_as_bool, value_as_f64, LaunchArgs, LoadRequest, PipeCommand, PipeServer, PipeWriter,
    QueueItemRaw, QueueSpec, WM_APP_MPV_EVENT, WM_APP_PIPE_CMD,
};
use mpv::{
    TrackInfo, UI_EVT_EOF, UI_EVT_ERROR, UI_EVT_FILE_LOADED, UI_EVT_PROGRESS, UI_EVT_TRACKS,
    UI_EVT_VIDEO_SIZE,
};
use prefs::{
    describe_video_aspect, ideal_pip_size_for_video, ideal_window_size_for_video,
    snap_window_size_to_aspect, AspectMode, AutoplayQueue, DanmakuPrefs, HoldBoostState,
    NativePlayerPrefs, QueueEpisode, SavedWindowRect, VideoOrientation,
};
use render::{RenderConfigShared, RenderController};
use webview_hud::{SharedWebViewHud, WebViewHudInner};

const TIMER_HEARTBEAT_ID: usize = 1;
const WM_MOUSELEAVE_MSG: u32 = 0x02A3; // Win32 WM_MOUSELEAVE (675u32)
const WM_APP_START_DRAG: u32 = WM_APP + 0x103;
const WM_APP_DRAIN_HUD_MSGS: u32 = WM_APP + 0x104;

fn main() {
    init_per_monitor_dpi();
    let args = LaunchArgs::parse(std::env::args());
    if args.selftest {
        if let Err(err) = selftest::run_selftest() {
            eprintln!("[SELFTEST FAILED] {err}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(err) = run_player(args) {
        eprintln!("[ttv-player FATAL] {err}");
        std::process::exit(1);
    }
}

/// Lock-free / non-`RefCell` window-proc state so synchronous Win32 sizing,
/// min/max queries, and mouse routing never fail with `BorrowError` when
/// `AppController` is mutably borrowed during `SetWindowPos` / `ShowWindow`.
struct WndShared {
    fullscreen: Cell<bool>,
    pip: Cell<bool>,
    cursor_hidden: Cell<bool>,
    mouse_tracked: Cell<bool>,
    releasing_capture: Cell<bool>,
    render_shared: Arc<RenderConfigShared>,
    hud: RefCell<Option<SharedWebViewHud>>,
}

struct WindowUserData {
    wnd: Rc<WndShared>,
    app: Rc<RefCell<AppController>>,
    deferred_hud_msgs: Rc<RefCell<VecDeque<Value>>>,
}

/// Unsticks `:hover` / hovered menus when a window shrink or monitor move places
/// a stationary cursor outside the new client rect.
fn sync_cursor_after_window_change(hwnd: HWND, wnd: &WndShared) {
    let mut pt = POINT::default();
    let mut rc = RECT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_err()
            || !ScreenToClient(hwnd, &mut pt).as_bool()
            || GetClientRect(hwnd, &mut rc).is_err()
        {
            return;
        }
        if !PtInRect(&rc, pt).as_bool() && wnd.mouse_tracked.get() {
            wnd.mouse_tracked.set(false);
            if let Ok(hud_opt) = wnd.hud.try_borrow() {
                if let Some(hud) = hud_opt.as_ref() {
                    if let Ok(mut b) = hud.try_borrow_mut() {
                        b.notify_host_leave();
                    }
                }
            }
        }
    }
}

fn sync_surfaces_to_hwnd(hwnd: HWND, wnd: &WndShared) {
    if unsafe { IsIconic(hwnd).as_bool() } {
        return;
    }
    let (w, h) = client_size_px(hwnd);
    wnd.render_shared.request_resize(w, h);
    if let Ok(hud_opt) = wnd.hud.try_borrow() {
        if let Some(hud) = hud_opt.as_ref() {
            if let Ok(b) = hud.try_borrow() {
                b.resize(w, h);
            }
        }
    }
    set_dwm_rounded_corners(hwnd, !wnd.fullscreen.get() && !unsafe { IsZoomed(hwnd).as_bool() });
    sync_cursor_after_window_change(hwnd, wnd);
}

#[derive(Clone)]
struct ActiveSourceState {
    url: String,
    key: String,
    headers: std::collections::BTreeMap<String, String>,
    retry_count: u8,
}

fn should_auto_recover_error(url: &str, err_msg: &str, failure_code: u8, retry_count: u8) -> bool {
    let low_url = url.trim().to_ascii_lowercase();
    if !(low_url.starts_with("http://") || low_url.starts_with("https://")) {
        return false;
    }
    if retry_count >= 2 || failure_code == mpv::FAILURE_DECODE {
        return false;
    }
    let low = err_msg.to_ascii_lowercase();
    !(low.contains("403")
        || low.contains("401")
        || low.contains("404")
        || low.contains("410")
        || low.contains("forbidden")
        || low.contains("unauthorized")
        || low.contains("unrecognized")
        || low.contains("unsupported"))
}

struct AppController {
    hwnd: HWND,
    prefs: NativePlayerPrefs,
    queue: AutoplayQueue,
    hold_boost: HoldBoostState,
    render: RenderController,
    wnd: Rc<WndShared>,
    pipe_server: Option<PipeServer>,
    pipe_writer: PipeWriter,

    title: String,
    kind: String,
    series_id: String,
    episode_id: String,
    vid: String,
    ep: u32,
    total_eps: u32,
    current_index: usize,

    saved_fullscreen: Option<SavedFullscreenRect>,
    saved_pip_rect: Option<RECT>,
    adapted_orientation: Option<VideoOrientation>,

    ab_a: Option<f64>,
    ab_b: Option<f64>,

    countdown_active: bool,
    countdown_deadline: Option<Instant>,
    countdown_target_index: usize,

    danmaku_items: Vec<Value>,
    danmaku_requested_windows: BTreeSet<u64>,
    danmaku_seen_keys: BTreeSet<String>,
    last_pipe_heartbeat: Instant,
    last_eof_emitted_ep: Option<usize>,
    prefetched_next_index: Option<usize>,
    last_shot_notified: Option<String>,
    last_hud_seek_seq: u64,
    last_out_ratio: f64,
    has_requested_source: bool,
    active_source: Option<ActiveSourceState>,
    last_seek_target: Option<f64>,
}

impl AppController {
    fn with_hud(&self, f: impl FnOnce(&mut WebViewHudInner)) {
        if let Ok(hud_opt) = self.wnd.hud.try_borrow() {
            if let Some(hud) = hud_opt.as_ref() {
                if let Ok(mut b) = hud.try_borrow_mut() {
                    f(&mut b);
                }
            }
        }
    }

    fn current_mode(&self) -> InterpContentMode {
        InterpContentMode::from_modes(&self.prefs.fg_mode, &self.kind)
    }

    fn current_mode_str(&self) -> &'static str {
        mode_as_str(self.current_mode())
    }

    fn compute_fg_engine(&self, src_fps: f64) -> &'static str {
        compute_fg_engine_name(
            self.prefs.fg_enabled,
            self.render.shared.is_pipeline_ready(),
            self.render.shared.monitor_hz(),
            src_fps,
        )
    }

    fn emit_fg_changed_event(&self) {
        let src_fps = self.render.mpv.state.source_fps();
        let fg_engine = self.compute_fg_engine(src_fps);
        let mut ev = serde_json::Map::new();
        ev.insert("on".into(), json!(self.prefs.fg_enabled));
        ev.insert("fg".into(), json!(self.prefs.fg_enabled));
        ev.insert(
            "mult".into(),
            json!(if self.prefs.fg_enabled { self.prefs.fg_mult } else { 0 }),
        );
        ev.insert("fgEngine".into(), json!(fg_engine));
        ev.insert("fgMode".into(), json!(self.prefs.fg_mode));
        ev.insert("fgModeEffective".into(), json!(self.current_mode_str()));
        self.pipe_writer.send_event("fgChanged", ev);
    }

    fn push_pipe_progress_now(&mut self) {
        self.last_pipe_heartbeat = Instant::now();
        let snap = self.render.mpv.state.snapshot();
        let has_presented = self.render.shared.has_presented_frame();
        let failure = self.render.mpv.state.failure_label();
        let phase = compute_playback_phase(
            self.has_requested_source,
            failure,
            snap.file_loaded,
            has_presented,
            snap.buffering,
        );
        let cached_until = if snap.file_loaded {
            (snap.time_pos + snap.cache_duration).max(snap.time_pos)
        } else {
            -1.0
        };
        let src_fps = snap.source_fps;
        let out_fps = self.render.shared.out_fps();
        let fg_engine = self.compute_fg_engine(src_fps);
        if !self.prefs.fg_enabled || fg_engine != "gpu-optical-flow" {
            self.last_out_ratio = 1.0;
        } else if !snap.paused && snap.file_loaded && has_presented && src_fps > 1.0 && out_fps > 1.0 {
            let ratio = ((out_fps / src_fps) * 100.0).round() / 100.0;
            self.last_out_ratio = ratio.clamp(0.5, 8.0);
        }
        let out_ratio = self.last_out_ratio;
        let out_mult = if !self.prefs.fg_enabled {
            0
        } else if fg_engine == "gpu-optical-flow" {
            (out_ratio.round() as u32).clamp(1, 5)
        } else {
            1
        };
        let display_hz = (self.render.shared.monitor_hz().round() as i64).max(0);

        let mut m = serde_json::Map::new();
        m.insert("pos".into(), json!(snap.display_time_pos));
        m.insert("dur".into(), json!(snap.duration));
        m.insert("buf".into(), json!(snap.cache_duration));
        m.insert("cachedUntil".into(), json!(cached_until));
        m.insert("phase".into(), json!(phase));
        m.insert("failure".into(), json!(failure));
        m.insert("paused".into(), json!(snap.paused));
        m.insert("idle".into(), json!(snap.core_idle));
        m.insert("vw".into(), json!(snap.video_w));
        m.insert("vh".into(), json!(snap.video_h));
        m.insert("quality".into(), json!("auto"));
        m.insert("ep".into(), json!(self.ep));
        m.insert("index".into(), json!(self.current_index));
        m.insert("autoplay".into(), json!(self.prefs.autoplay));
        m.insert("queueLen".into(), json!(self.queue.len()));
        m.insert("fg".into(), json!(self.prefs.fg_enabled));
        m.insert(
            "mult".into(),
            json!(if self.prefs.fg_enabled { self.prefs.fg_mult } else { 0 }),
        );
        m.insert("outMult".into(), json!(out_mult));
        m.insert("outRatio".into(), json!(out_ratio));
        m.insert("fgEngine".into(), json!(fg_engine));
        m.insert("fgMode".into(), json!(self.prefs.fg_mode));
        m.insert("fgModeEffective".into(), json!(self.current_mode_str()));
        m.insert("displaySync".into(), json!(true));
        m.insert("displayHz".into(), json!(display_hz));
        m.insert("fgEnabled".into(), json!(self.prefs.fg_enabled));
        m.insert("fgMult".into(), json!(self.prefs.fg_mult));
        m.insert("outFps".into(), json!(out_fps));
        m.insert("srcFps".into(), json!(src_fps));
        self.pipe_writer.send_event("progress", m);
    }

    fn apply_initial_prefs_to_mpv(&self) {
        let m = &self.render.mpv;
        m.set_volume(self.prefs.volume);
        m.set_muted(self.prefs.muted);
        m.set_speed(self.prefs.speed);
        m.set_aspect(self.prefs.aspect);
        self.render.shared.set_aspect_mode(self.prefs.aspect);
        m.set_picture(
            self.prefs.picture.brightness,
            self.prefs.picture.contrast,
            self.prefs.picture.saturation,
            self.prefs.picture.gamma,
        );
    }

    fn push_full_hud_state(&self) {
        let snap = self.render.mpv.state.snapshot();
        let countdown_left = self
            .countdown_deadline
            .map(|d| d.saturating_duration_since(Instant::now()).as_secs_f64())
            .unwrap_or(0.0);
        let next_ep = self.current_index + 2;
        let payload = json!({
            "type": "state",
            "theme": self.prefs.theme,
            "title": self.title,
            "ep": self.ep,
            "totalEps": self.total_eps,
            "currentIndex": self.current_index,
            "paused": snap.paused,
            "buffering": snap.buffering,
            "hasFrame": snap.file_loaded,
            "pos": snap.display_time_pos,
            "seeking": snap.seeking,
            "seekSeq": self.last_hud_seek_seq,
            "duration": snap.duration,
            "videoW": snap.video_w,
            "videoH": snap.video_h,
            "volume": self.prefs.volume,
            "muted": self.prefs.muted,
            "speed": self.prefs.speed,
            "fgEnabled": self.prefs.fg_enabled,
            "fgMult": self.prefs.fg_mult,
            "fgMode": self.prefs.fg_mode,
            "autoplay": self.prefs.autoplay,
            "topmost": self.prefs.topmost,
            "fullscreen": self.wnd.fullscreen.get(),
            "pip": self.wnd.pip.get(),
            "vsr": self.prefs.vsr_enabled,
            "vsrHwActive": self.render.shared.is_vsr_hw_active(),
            "vsrHwSupported": self.render.shared.vsr_hw_supported.load(std::sync::atomic::Ordering::Acquire),
            "aspect": self.prefs.aspect.as_str(),
            "picture": self.prefs.picture,
            "danmaku": self.prefs.danmaku,
            "countdownActive": self.countdown_active,
            "countdownLeft": countdown_left,
            "countdownTitle": format!("即将播放第 {next_ep} 集"),
        });
        self.with_hud(|b| b.post_json(&payload));
    }

    fn push_hud_toast(&self, text: &str) {
        self.with_hud(|b| b.post_json(&json!({ "type": "toast", "text": text })));
    }

    fn apply_load_request(&mut self, req: LoadRequest) {
        self.countdown_active = false;
        self.countdown_deadline = None;
        self.ab_a = None;
        self.ab_b = None;
        self.hold_boost.end();
        self.danmaku_items.clear();
        self.danmaku_requested_windows.clear();
        self.danmaku_seen_keys.clear();
        self.last_eof_emitted_ep = None;
        self.prefetched_next_index = None;
        self.last_out_ratio = 1.0;

        self.with_hud(|b| b.post_json(&json!({ "type": "danmakuClear" })));

        if !req.title.trim().is_empty() {
            self.title = req.title.clone();
        }
        if !req.kind.trim().is_empty() {
            self.kind = req.kind.trim().to_ascii_lowercase();
        }
        if !req.series_id.trim().is_empty() {
            self.series_id = req.series_id.clone();
        }
        if !req.episode_id.trim().is_empty() {
            self.episode_id = req.episode_id.clone();
        }
        if !req.vid.trim().is_empty() {
            self.vid = req.vid.clone();
        }
        if req.ep > 0 {
            self.ep = req.ep;
        } else if let Some(idx) = req.index {
            self.ep = idx as u32 + 1;
        }
        if let Some(idx) = req.index {
            self.current_index = idx;
        } else if self.ep > 0 {
            self.current_index = (self.ep - 1) as usize;
        }
        if let Some(te) = req.total_eps {
            self.total_eps = te.max(self.ep);
        }
        if let Some(vol) = req.volume {
            self.prefs.volume = vol.clamp(0.0, 130.0);
            self.render.mpv.set_volume(self.prefs.volume);
        }
        if !req.fg_mode.trim().is_empty() {
            self.prefs.fg_mode = NativePlayerPrefs::normalize_fg_mode(&req.fg_mode);
            let mode = self.current_mode();
            self.render.shared.mode_raw.store(mode as u32, std::sync::atomic::Ordering::Release);
        }
        if let Some(ap) = req.autoplay {
            self.prefs.autoplay = ap;
        }
        if let Some(ad) = req.autoplay_delay {
            self.prefs.autoplay_delay = (ad.round() as i64).clamp(0, 15) as u32;
        }

        // Sync optional danmaku & VSR settings bundled in open() load JSON (host uses % & 0..2).
        if let Some(b) = req.resolved_danmaku_enabled() {
            self.prefs.danmaku.enabled = b;
        }
        if let Some(v) = req.danmaku_area {
            self.prefs.danmaku.region = DanmakuPrefs::area_from_host(v);
        }
        if let Some(v) = req.danmaku_opacity {
            self.prefs.danmaku.opacity = DanmakuPrefs::opacity_from_host(v);
        }
        if let Some(v) = req.danmaku_font_scale {
            self.prefs.danmaku.font_scale = DanmakuPrefs::font_scale_from_host(v);
        }
        if let Some(v) = req.danmaku_speed {
            self.prefs.danmaku.speed_sec = DanmakuPrefs::speed_sec_from_host(v);
        }
        if let Some(ref f) = req.danmaku_font {
            if !f.trim().is_empty() {
                self.prefs.danmaku.font_family = f.clone();
            }
        }
        if let Some(ref c) = req.danmaku_color {
            if !c.trim().is_empty() {
                self.prefs.danmaku.color = c.clone();
            }
        }
        if let Some(b) = req.danmaku_bold {
            self.prefs.danmaku.font_weight = if b { "700".into() } else { "400".into() };
        }
        if let Some(ref s) = req.danmaku_stroke {
            if !s.trim().is_empty() {
                self.prefs.danmaku.stroke = s.clone();
            }
        }
        if let Some(vsr) = req.vsr_enabled {
            self.prefs.vsr_enabled = vsr;
        }
        self.render.shared.set_vsr_enabled(self.prefs.vsr_enabled);

        // Replace queue ONLY when load JSON explicitly carried `queue` (never wipe on bare reply load).
        if let Some(qspec) = req.queue {
            let (items, opt_idx, opt_ap, opt_tot) = match qspec {
                QueueSpec::Object(o) => (o.items, o.index, o.autoplay, o.total_eps),
                QueueSpec::List(l) => (l, None, None, None),
            };
            if let Some(idx) = opt_idx {
                self.current_index = idx;
            }
            if let Some(ap) = opt_ap {
                self.prefs.autoplay = ap;
            }
            if let Some(tot) = opt_tot {
                self.total_eps = self.total_eps.max(tot);
            }
            let eps = convert_queue_items(&items);
            self.queue.replace(self.total_eps as usize, eps);
            self.total_eps = self.total_eps.max(self.queue.total_eps as u32);
        }

        if self.total_eps == 0 {
            self.total_eps = self.ep.max(1);
        }

        self.has_requested_source = !req.url.trim().is_empty();
        self.render.mpv.state.set_expected_duration(req.total);
        self.render.mpv.state.prepare_for_load_dims(req.vw, req.vh);
        self.adapt_window_for_video_dims(req.vw, req.vh);
        self.render.shared.reset_frames();

        if self.has_requested_source {
            self.active_source = Some(ActiveSourceState {
                url: req.url.clone(),
                key: req.key.clone(),
                headers: req.headers.clone(),
                retry_count: 0,
            });
            self.last_seek_target = if req.start > 0.05 { Some(req.start) } else { None };
            if let Err(err) = self
                .render
                .mpv
                .load_file(&req.url, &req.key, req.start, &req.headers)
            {
                self.push_hud_toast(&format!("加载失败: {err}"));
            } else {
                let mut m = serde_json::Map::new();
                m.insert("ep".into(), json!(self.ep));
                m.insert("index".into(), json!(self.current_index));
                self.pipe_writer.send_event("ready", m);
                self.request_danmaku_window_for(req.start);
            }
        } else {
            self.active_source = None;
            self.last_seek_target = None;
        }
        self.push_full_hud_state();
        self.push_pipe_progress_now();
    }

    fn request_danmaku_window_for(&mut self, pos_sec: f64) {
        if !self.prefs.danmaku.enabled {
            return;
        }
        if self.series_id.trim().is_empty()
            || (self.episode_id.trim().is_empty() && self.vid.trim().is_empty())
        {
            return;
        }
        let win_ms = ((pos_sec.max(0.0) / 30.0).floor() as u64) * 30_000;
        if !self.danmaku_requested_windows.insert(win_ms) {
            return;
        }
        let mut m = serde_json::Map::new();
        m.insert("seriesId".into(), json!(self.series_id));
        m.insert("episodeId".into(), json!(self.episode_id));
        m.insert("vid".into(), json!(self.vid));
        m.insert("startMs".into(), json!(win_ms));
        m.insert("durationMs".into(), json!(30_000u64));
        self.pipe_writer.send_event("requestDanmaku", m);
    }

    fn next_episode_target(&self) -> Option<usize> {
        let sequential = self.current_index + 1;
        if sequential < self.total_eps as usize {
            return Some(sequential);
        }
        self.queue.next_after(self.current_index).map(|e| e.index)
    }

    fn maybe_request_prefetch_next(&mut self, file_loaded: bool, time_pos: f64) {
        if !self.prefs.autoplay || !file_loaded || time_pos < 1.5 {
            return;
        }
        let Some(next_idx) = self.next_episode_target() else {
            return;
        };
        if self
            .queue
            .get(next_idx)
            .is_some_and(|e| !e.url.trim().is_empty())
        {
            return;
        }
        if self.prefetched_next_index == Some(next_idx) {
            return;
        }
        self.prefetched_next_index = Some(next_idx);
        let mut ev = serde_json::Map::new();
        ev.insert("index".into(), json!(next_idx));
        ev.insert("ep".into(), json!(next_idx as u32 + 1));
        ev.insert("totalEps".into(), json!(self.total_eps));
        if !self.series_id.is_empty() {
            ev.insert("seriesId".into(), json!(self.series_id));
        }
        self.pipe_writer.send_event("prefetchNext", ev);
    }

    fn switch_episode_index(&mut self, target_index: usize) {
        let from_idx = self.current_index;
        self.countdown_active = false;
        self.countdown_deadline = None;
        self.prefetched_next_index = None;

        if let Some(item) = self
            .queue
            .get(target_index)
            .filter(|e| !e.url.trim().is_empty())
            .cloned()
        {
            self.current_index = item.index;
            self.ep = item.ep;
            if !item.title.trim().is_empty() {
                self.title = item.title.clone();
            }
            if !item.series_id.trim().is_empty() {
                self.series_id = item.series_id.clone();
            }
            if !item.episode_id.trim().is_empty() {
                self.episode_id = item.episode_id.clone();
            }
            if !item.vid.trim().is_empty() {
                self.vid = item.vid.clone();
            }
            self.ab_a = None;
            self.ab_b = None;
            self.danmaku_items.clear();
            self.danmaku_requested_windows.clear();
            self.danmaku_seen_keys.clear();
            self.last_eof_emitted_ep = None;
            self.last_out_ratio = 1.0;
            self.has_requested_source = true;
            self.active_source = Some(ActiveSourceState {
                url: item.url.clone(),
                key: item.key.clone(),
                headers: item.headers.clone(),
                retry_count: 0,
            });
            self.last_seek_target = if item.start > 0.05 { Some(item.start) } else { None };
            self.with_hud(|b| b.post_json(&json!({ "type": "danmakuClear" })));
            self.render.mpv.state.set_expected_duration(item.duration);
            self.render.shared.reset_frames();
            self.render.shared.mark_seek();
            let _ = self
                .render
                .mpv
                .load_file(&item.url, &item.key, item.start, &item.headers);

            let mut ev = serde_json::Map::new();
            ev.insert("index".into(), json!(item.index));
            ev.insert("ep".into(), json!(item.ep));
            ev.insert("fromIndex".into(), json!(from_idx));
            self.pipe_writer.send_event("advance", ev);
            self.request_danmaku_window_for(item.start);
            self.push_full_hud_state();
            self.push_pipe_progress_now();
        } else {
            let mut ev = serde_json::Map::new();
            ev.insert("index".into(), json!(target_index));
            ev.insert("ep".into(), json!(target_index as u32 + 1));
            ev.insert("totalEps".into(), json!(self.total_eps));
            if !self.series_id.is_empty() {
                ev.insert("seriesId".into(), json!(self.series_id));
            }
            self.pipe_writer.send_event("requestNext", ev);
            self.push_hud_toast(&format!("正在加载第 {} 集…", target_index + 1));
        }
    }

    fn toggle_fullscreen(&mut self, desired: Option<bool>) {
        let next = desired.unwrap_or(!self.wnd.fullscreen.get());
        if next == self.wnd.fullscreen.get() {
            return;
        }
        if next {
            if self.wnd.pip.get() {
                self.toggle_pip(Some(false));
            }
            self.wnd.fullscreen.set(true);
            self.saved_fullscreen = Some(enter_borderless_fullscreen(self.hwnd));
            unsafe {
                let _ = SetForegroundWindow(self.hwnd);
            }
        } else {
            self.wnd.fullscreen.set(false);
            if let Some(saved) = self.saved_fullscreen.take() {
                exit_borderless_fullscreen(self.hwnd, saved, self.prefs.topmost);
            }
        }
        sync_surfaces_to_hwnd(self.hwnd, &self.wnd);
        self.push_full_hud_state();
    }

    fn save_current_rect_to_orientation(&mut self, ori: VideoOrientation) {
        if self.wnd.fullscreen.get() {
            return;
        }
        let mut rc = RECT::default();
        if unsafe { GetWindowRect(self.hwnd, &mut rc) }.is_err() {
            return;
        }
        if self.wnd.pip.get() {
            self.prefs.placement.set_pip_rect_for(
                ori,
                SavedWindowRect {
                    x: Some(rc.left),
                    y: Some(rc.top),
                    w: (rc.right - rc.left).max(220) as u32,
                    h: (rc.bottom - rc.top).max(150) as u32,
                },
            );
            return;
        }
        if unsafe { IsZoomed(self.hwnd).as_bool() } {
            return;
        }
        self.prefs.placement.set_rect_for(
            ori,
            SavedWindowRect {
                x: Some(rc.left),
                y: Some(rc.top),
                w: (rc.right - rc.left).max(360) as u32,
                h: (rc.bottom - rc.top).max(240) as u32,
            },
        );
    }

    fn resize_hwnd_keeping_center(
        &self,
        target_w: i32,
        target_h: i32,
        prefer_pos: Option<(i32, i32)>,
    ) {
        let mut rc = RECT::default();
        let (x, y) = if let Some((px, py)) = prefer_pos {
            (px, py)
        } else if unsafe { GetWindowRect(self.hwnd, &mut rc) }.is_ok() {
            let cx = (rc.left + rc.right) / 2;
            let cy = (rc.top + rc.bottom) / 2;
            (cx - target_w / 2, cy - target_h / 2)
        } else {
            (180, 90)
        };
        let (fx, fy, fw, fh) = clamp_window_to_monitor(x, y, target_w, target_h);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                fx,
                fy,
                fw,
                fh,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        sync_surfaces_to_hwnd(self.hwnd, &self.wnd);
    }

    /// Adapts windowed / PiP geometry smoothly when content switches between `16:9` and `9:16`.
    fn adapt_window_for_video_dims(&mut self, vid_w: u32, vid_h: u32) {
        if vid_w == 0 || vid_h == 0 {
            return;
        }
        let new_ori = VideoOrientation::from_dims(vid_w, vid_h);
        if self.adapted_orientation == Some(new_ori) {
            return;
        }
        if let Some(prev_ori) = self.adapted_orientation {
            self.save_current_rect_to_orientation(prev_ori);
        }
        self.adapted_orientation = Some(new_ori);

        if self.wnd.fullscreen.get() {
            self.render.shared.wake();
            return;
        }
        let work = monitor_work_area(self.hwnd);
        let ww = (work.right - work.left).max(480);
        let wh = (work.bottom - work.top).max(360);
        if self.wnd.pip.get() {
            let (pw, ph, pos) = if let Some(saved) = self.prefs.placement.pip_rect_for(new_ori) {
                (saved.w as i32, saved.h as i32, saved.x.zip(saved.y))
            } else {
                let (iw, ih) = ideal_pip_size_for_video(vid_w, vid_h, ww, wh);
                (iw, ih, None)
            };
            self.resize_hwnd_keeping_center(pw, ph, pos);
        } else if !unsafe { IsZoomed(self.hwnd).as_bool() } {
            let (tw, th, pos) = if let Some(saved) = self.prefs.placement.rect_for(new_ori) {
                (saved.w as i32, saved.h as i32, saved.x.zip(saved.y))
            } else {
                let (iw, ih) = ideal_window_size_for_video(vid_w, vid_h, ww, wh);
                (iw, ih, None)
            };
            self.resize_hwnd_keeping_center(tw, th, pos);
        }
        self.render.shared.wake();
    }

    /// Snaps the current window / PiP frame cleanly to match exact encoded video aspect ratio.
    fn fit_window_to_video_aspect(&mut self) {
        if self.wnd.fullscreen.get() || unsafe { IsZoomed(self.hwnd).as_bool() } {
            self.push_hud_toast("全屏或最大化模式下无需贴合窗口比例");
            return;
        }
        let (vid_w, vid_h) = self.render.mpv.state.video_size();
        let work = monitor_work_area(self.hwnd);
        let ww = (work.right - work.left).max(480);
        let wh = (work.bottom - work.top).max(360);
        let mut rc = RECT::default();
        let (cur_w, cur_h) = if unsafe { GetWindowRect(self.hwnd, &mut rc) }.is_ok() {
            (rc.right - rc.left, rc.bottom - rc.top)
        } else {
            (1120, 630)
        };
        let (tw, th) = if self.wnd.pip.get() {
            ideal_pip_size_for_video(vid_w, vid_h, ww, wh)
        } else {
            snap_window_size_to_aspect(vid_w, vid_h, cur_w, cur_h, ww, wh)
        };
        self.resize_hwnd_keeping_center(tw, th, None);
        let ori = VideoOrientation::from_dims(vid_w, vid_h);
        self.adapted_orientation = Some(ori);
        self.save_current_rect_to_orientation(ori);
        self.push_full_hud_state();
        self.push_hud_toast(&format!("窗口已贴合 {}", describe_video_aspect(vid_w, vid_h)));
    }

    fn toggle_pip(&mut self, desired: Option<bool>) {
        let next = desired.unwrap_or(!self.wnd.pip.get());
        if next == self.wnd.pip.get() {
            return;
        }
        let (vid_w, vid_h) = self.render.mpv.state.video_size();
        let ori = VideoOrientation::from_dims(vid_w, vid_h);
        if next {
            if self.wnd.fullscreen.get() {
                self.toggle_fullscreen(Some(false));
            }
            let mut rc = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.hwnd, &mut rc);
            }
            self.saved_pip_rect = Some(rc);
            let work = monitor_work_area(self.hwnd);
            let ww = (work.right - work.left).max(480);
            let wh = (work.bottom - work.top).max(360);
            let (pw, ph, saved_pos) = if let Some(saved) = self.prefs.placement.pip_rect_for(ori) {
                (saved.w as i32, saved.h as i32, saved.x.zip(saved.y))
            } else {
                let (iw, ih) = ideal_pip_size_for_video(vid_w, vid_h, ww, wh);
                (iw, ih, None)
            };
            let px = saved_pos
                .map(|p| p.0)
                .or(self.prefs.placement.pip_x)
                .unwrap_or_else(|| work.right - pw - 24);
            let py = saved_pos
                .map(|p| p.1)
                .or(self.prefs.placement.pip_y)
                .unwrap_or_else(|| work.bottom - ph - 24);
            let (cx, cy, cw, ch) = clamp_window_to_monitor(px, py, pw, ph);
            self.wnd.pip.set(true);
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
                    cx,
                    cy,
                    cw,
                    ch,
                    SWP_SHOWWINDOW | SWP_FRAMECHANGED,
                );
            }
            set_dwm_rounded_corners(self.hwnd, true);
        } else {
            self.save_current_rect_to_orientation(ori);
            self.wnd.pip.set(false);
            set_window_topmost(self.hwnd, self.prefs.topmost);
            if let Some(prev) = self.saved_pip_rect.take() {
                let (cx, cy, cw, ch) = clamp_window_to_monitor(
                    prev.left,
                    prev.top,
                    prev.right - prev.left,
                    prev.bottom - prev.top,
                );
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        cx,
                        cy,
                        cw,
                        ch,
                        SWP_NOZORDER | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
                    );
                }
            }
        }
        sync_surfaces_to_hwnd(self.hwnd, &self.wnd);
        self.push_full_hud_state();
    }

    fn restore_from_mini_or_pip(&mut self) {
        if self.wnd.pip.get() {
            self.toggle_pip(Some(false));
            return;
        }
        let (vid_w, vid_h) = self.render.mpv.state.video_size();
        let ori = VideoOrientation::from_dims(vid_w, vid_h);
        let work = monitor_work_area(self.hwnd);
        let ww = (work.right - work.left).max(480);
        let wh = (work.bottom - work.top).max(360);
        let (tw, th, pos) = if let Some(saved) = self
            .prefs
            .placement
            .rect_for(ori)
            .filter(|r| r.w >= 460 && r.h >= 300)
        {
            (saved.w as i32, saved.h as i32, saved.x.zip(saved.y))
        } else {
            let (iw, ih) = ideal_window_size_for_video(vid_w, vid_h, ww, wh);
            (iw, ih, None)
        };
        self.resize_hwnd_keeping_center(tw, th, pos);
        self.push_full_hud_state();
    }

    fn set_theme(&mut self, theme: &str) {
        self.prefs.theme = NativePlayerPrefs::normalize_theme(theme);
        self.render.shared.light_theme.store(
            self.prefs.theme == "light",
            std::sync::atomic::Ordering::Release,
        );
        self.render.shared.wake();
        self.push_full_hud_state();
    }

    fn handle_hud_message(&mut self, msg: Value) {
        let Some(cmd) = msg.get("cmd").and_then(Value::as_str) else {
            return;
        };
        match cmd {
            "hudReady" => {
                self.with_hud(|b| {
                    b.mark_hud_ready();
                    b.move_focus();
                });
                self.push_full_hud_state();
                let tracks = self.render.mpv.state.tracks_snapshot();
                if !tracks.is_empty() {
                    self.with_hud(|b| b.post_json(&json!({ "type": "tracks", "tracks": tracks })));
                }
            }
            "videoRect" => {
                let x = msg.get("x").and_then(value_as_f64).unwrap_or(0.0).round() as i32;
                let y = msg.get("y").and_then(value_as_f64).unwrap_or(0.0).round() as i32;
                let w = msg.get("w").and_then(value_as_f64).unwrap_or(64.0).round() as u32;
                let h = msg.get("h").and_then(value_as_f64).unwrap_or(64.0).round() as u32;
                self.render.shared.set_video_rect(x, y, w.max(64), h.max(64));
            }
            "startDrag" => {
                // Defer modal HTCAPTION move loop out of AppController's RefCell borrow
                // so Snap resizes, timer heartbeats, and WebView messages are never dropped.
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_APP_START_DRAG, WPARAM(0), LPARAM(0));
                }
            }
            "cursorHidden" => {
                let hidden = msg.get("hidden").and_then(value_as_bool).unwrap_or(false);
                self.wnd.cursor_hidden.set(hidden);
            }
            "home" => {
                self.pipe_writer.send_event("home", Default::default());
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            "minimize" => unsafe {
                let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
            },
            "maximize" => {
                if self.wnd.fullscreen.get() {
                    self.toggle_fullscreen(Some(false));
                } else {
                    unsafe {
                        let maximized = IsZoomed(self.hwnd).as_bool();
                        let _ = ShowWindow(self.hwnd, if maximized { SW_RESTORE } else { SW_MAXIMIZE });
                    }
                    sync_surfaces_to_hwnd(self.hwnd, &self.wnd);
                    self.push_full_hud_state();
                }
            }
            "close" => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            "togglePause" => {
                let paused = self.render.mpv.toggle_pause();
                self.render.shared.wake();
                update_execution_wake_lock(!paused);
            }
            "prev" => {
                if self.current_index > 0 {
                    self.switch_episode_index(self.current_index - 1);
                } else {
                    self.pipe_writer.send_event("prev", Default::default());
                }
            }
            "next" => {
                if self.current_index + 1 < self.total_eps as usize {
                    self.switch_episode_index(self.current_index + 1);
                } else {
                    self.pipe_writer.send_event("next", Default::default());
                }
            }
            "pickEpisode" => {
                if let Some(idx) = msg.get("index").and_then(value_as_f64).map(|n| n.round() as usize)
                {
                    if self.queue.get(idx).is_some_and(|e| !e.url.trim().is_empty()) {
                        self.switch_episode_index(idx);
                    } else {
                        let mut ev = serde_json::Map::new();
                        ev.insert("index".into(), json!(idx));
                        if !self.series_id.is_empty() {
                            ev.insert("seriesId".into(), json!(self.series_id));
                        }
                        self.pipe_writer.send_event("pickEpisode", ev);
                        self.current_index = idx;
                        self.ep = idx as u32 + 1;
                        self.push_hud_toast(&format!("正在加载第 {} 集…", self.ep));
                    }
                }
            }
            "seek" => {
                if let Some(seq) = msg.get("seekSeq").and_then(value_as_f64) {
                    self.last_hud_seek_seq =
                        self.last_hud_seek_seq.max(seq.max(0.0).round() as u64);
                }
                if let Some(pos) = msg.get("pos").and_then(value_as_f64) {
                    let dur = self.render.mpv.state.duration();
                    let target = mpv::clamp_seek_target(pos, dur);
                    self.last_seek_target = Some(target);
                    self.render.shared.mark_seek();
                    self.render.mpv.seek_absolute(target);
                    self.request_danmaku_window_for(target);
                }
            }
            "seekRel" => {
                if let Some(seq) = msg.get("seekSeq").and_then(value_as_f64) {
                    self.last_hud_seek_seq =
                        self.last_hud_seek_seq.max(seq.max(0.0).round() as u64);
                }
                if let Some(delta) = msg.get("delta").and_then(value_as_f64) {
                    let (cur, _) = self.render.mpv.state.effective_time_pos();
                    let dur = self.render.mpv.state.duration();
                    let target = mpv::clamp_seek_target(cur + delta, dur);
                    self.last_seek_target = Some(target);
                    self.render.shared.mark_seek();
                    self.render.mpv.seek_relative(delta);
                }
            }
            "volume" => {
                if let Some(v) = msg.get("value").and_then(value_as_f64) {
                    self.prefs.volume = v.clamp(0.0, 130.0);
                    self.prefs.muted = self.prefs.volume <= 0.0;
                    self.render.mpv.set_volume(self.prefs.volume);
                    self.render.mpv.set_muted(self.prefs.muted);
                }
            }
            "mute" => {
                let m = msg
                    .get("muted")
                    .and_then(value_as_bool)
                    .unwrap_or(!self.prefs.muted);
                self.prefs.muted = m;
                self.render.mpv.set_muted(m);
            }
            "speed" => {
                if let Some(v) = msg.get("value").and_then(value_as_f64) {
                    self.prefs.speed = v.clamp(0.25, 4.0);
                    if !self.hold_boost.active {
                        self.render.mpv.set_speed(self.prefs.speed);
                    }
                }
            }
            "boostStart" => {
                let spd = self.hold_boost.begin(self.prefs.speed);
                self.render.mpv.set_speed(spd);
            }
            "boostEnd" => {
                if let Some(restored) = self.hold_boost.end() {
                    self.prefs.speed = restored;
                    self.render.mpv.set_speed(restored);
                }
            }
            "fg" => {
                let on = msg
                    .get("on")
                    .and_then(value_as_bool)
                    .unwrap_or(self.prefs.fg_enabled);
                let mult = msg
                    .get("mult")
                    .and_then(value_as_f64)
                    .map(|n| {
                        let r = n.round() as i64;
                        if r <= 0 { 0 } else { r.clamp(2, 5) as u32 }
                    })
                    .unwrap_or(self.prefs.fg_mult);
                self.prefs.fg_enabled = on;
                self.prefs.fg_mult = mult;
                self.render
                    .shared
                    .fg_enabled
                    .store(on, std::sync::atomic::Ordering::Release);
                self.render
                    .shared
                    .fg_mult
                    .store(mult, std::sync::atomic::Ordering::Release);
                self.render.shared.wake();
                self.emit_fg_changed_event();
            }
            "fgMode" => {
                let raw = msg.get("mode").and_then(Value::as_str).unwrap_or("auto");
                self.prefs.fg_mode = NativePlayerPrefs::normalize_fg_mode(raw);
                let eff = self.current_mode();
                self.render
                    .shared
                    .mode_raw
                    .store(eff as u32, std::sync::atomic::Ordering::Release);
                self.render.shared.wake();
                self.emit_fg_changed_event();
            }
            "autoplay" => {
                let on = msg
                    .get("on")
                    .and_then(value_as_bool)
                    .unwrap_or(!self.prefs.autoplay);
                self.prefs.autoplay = on;
                if !on {
                    self.countdown_active = false;
                    self.countdown_deadline = None;
                }
                let mut ev = serde_json::Map::new();
                ev.insert("on".into(), json!(on));
                self.pipe_writer.send_event("autoplayChanged", ev);
            }
            "pin" => {
                let on = msg
                    .get("on")
                    .and_then(value_as_bool)
                    .unwrap_or(!self.prefs.topmost);
                self.prefs.topmost = on;
                if !self.wnd.fullscreen.get() && !self.wnd.pip.get() {
                    set_window_topmost(self.hwnd, on);
                }
            }
            "theme" => {
                let t = msg.get("theme").and_then(Value::as_str).unwrap_or("dark");
                self.set_theme(t);
            }
            "fullscreen" => {
                let on = msg.get("on").and_then(value_as_bool);
                self.toggle_fullscreen(on);
            }
            "pip" => {
                let on = msg.get("on").and_then(value_as_bool);
                self.toggle_pip(on);
            }
            "restoreWindow" => {
                self.restore_from_mini_or_pip();
            }
            "aspect" => {
                let raw = msg.get("mode").and_then(Value::as_str).unwrap_or("fit");
                self.prefs.aspect = AspectMode::parse_lossy(raw);
                self.render.mpv.set_aspect(self.prefs.aspect);
                self.render.shared.set_aspect_mode(self.prefs.aspect);
            }
            "fitWindow" => {
                self.fit_window_to_video_aspect();
            }
            "frameStep" => {
                let dir = msg
                    .get("dir")
                    .and_then(value_as_f64)
                    .map(|n| if n < 0.0 { -1 } else { 1 })
                    .unwrap_or(1);
                self.render.shared.mark_seek();
                self.render.mpv.set_paused(true);
                self.render.mpv.frame_step(dir);
            }
            "shot" => {
                self.render
                    .shared
                    .shot_requested
                    .store(true, std::sync::atomic::Ordering::Release);
                self.render.shared.wake();
            }
            "loop" => {
                if msg.get("clear").and_then(value_as_bool).unwrap_or(false) {
                    self.ab_a = None;
                    self.ab_b = None;
                } else {
                    self.ab_a = msg.get("a").and_then(value_as_f64);
                    self.ab_b = msg.get("b").and_then(value_as_f64);
                }
            }
            "audioTrack" => {
                if let Some(id) = msg.get("id").and_then(value_as_f64).map(|n| n.round() as i64) {
                    self.render.mpv.set_audio_track(id);
                }
            }
            "picture" => {
                if let Some(p) = msg.get("preset").and_then(Value::as_str) {
                    self.prefs.picture.preset = p.to_string();
                }
                if let Some(v) = msg.get("brightness").and_then(value_as_f64) {
                    self.prefs.picture.brightness = v.round() as i32;
                }
                if let Some(v) = msg.get("contrast").and_then(value_as_f64) {
                    self.prefs.picture.contrast = v.round() as i32;
                }
                if let Some(v) = msg.get("saturation").and_then(value_as_f64) {
                    self.prefs.picture.saturation = v.round() as i32;
                }
                if let Some(v) = msg.get("gamma").and_then(value_as_f64) {
                    self.prefs.picture.gamma = v.round() as i32;
                }
                self.prefs.picture = self.prefs.picture.clamped();
                self.render.mpv.set_picture(
                    self.prefs.picture.brightness,
                    self.prefs.picture.contrast,
                    self.prefs.picture.saturation,
                    self.prefs.picture.gamma,
                );
            }
            "danmakuPrefs" => {
                if let Some(b) = msg.get("enabled").and_then(value_as_bool) {
                    self.prefs.danmaku.enabled = b;
                }
                if let Some(v) = msg.get("region").and_then(value_as_f64) {
                    self.prefs.danmaku.region = v;
                }
                if let Some(v) = msg.get("opacity").and_then(value_as_f64) {
                    self.prefs.danmaku.opacity = v;
                }
                if let Some(v) = msg.get("fontScale").and_then(value_as_f64) {
                    self.prefs.danmaku.font_scale = v;
                }
                if let Some(v) = msg.get("speedSec").and_then(value_as_f64) {
                    self.prefs.danmaku.speed_sec = v;
                }
                if let Some(s) = msg.get("font").and_then(Value::as_str) {
                    self.prefs.danmaku.font_family = s.to_string();
                }
                if let Some(s) = msg.get("color").and_then(Value::as_str) {
                    self.prefs.danmaku.color = s.to_string();
                }
                if let Some(b) = msg.get("bold").and_then(value_as_bool) {
                    self.prefs.danmaku.font_weight = if b { "700".into() } else { "400".into() };
                }
                if let Some(s) = msg.get("stroke").and_then(Value::as_str) {
                    self.prefs.danmaku.stroke = s.to_string();
                }
                self.prefs.danmaku = self.prefs.danmaku.sanitized();

                let mut ev = serde_json::Map::new();
                ev.insert("danmakuEnabled".into(), json!(self.prefs.danmaku.enabled));
                ev.insert(
                    "danmakuArea".into(),
                    json!(DanmakuPrefs::area_to_host(self.prefs.danmaku.region)),
                );
                ev.insert(
                    "danmakuOpacity".into(),
                    json!(DanmakuPrefs::opacity_to_host(self.prefs.danmaku.opacity)),
                );
                ev.insert(
                    "danmakuFontScale".into(),
                    json!(DanmakuPrefs::font_scale_to_host(self.prefs.danmaku.font_scale)),
                );
                ev.insert(
                    "danmakuSpeed".into(),
                    json!(DanmakuPrefs::speed_to_host(self.prefs.danmaku.speed_sec)),
                );
                ev.insert("danmakuFont".into(), json!(self.prefs.danmaku.font_family));
                ev.insert("danmakuColor".into(), json!(self.prefs.danmaku.color));
                ev.insert(
                    "danmakuBold".into(),
                    json!(self.prefs.danmaku.font_weight == "700"),
                );
                ev.insert("danmakuStroke".into(), json!(self.prefs.danmaku.stroke));
                self.pipe_writer.send_event("danmakuSettingsChanged", ev);

                if self.prefs.danmaku.enabled {
                    let pos = self.render.mpv.state.time_pos();
                    self.request_danmaku_window_for(pos);
                }
            }
            "vsr" => {
                let on = msg
                    .get("on")
                    .and_then(value_as_bool)
                    .unwrap_or(!self.prefs.vsr_enabled);
                self.prefs.vsr_enabled = on;
                self.render.shared.set_vsr_enabled(on);
                let mut ev = serde_json::Map::new();
                ev.insert("on".into(), json!(on));
                self.pipe_writer.send_event("vsrChanged", ev);
                self.push_full_hud_state();
            }
            "countdownGo" => {
                self.switch_episode_index(self.countdown_target_index);
            }
            "countdownCancel" => {
                self.countdown_active = false;
                self.countdown_deadline = None;
            }
            _ => {}
        }
    }

    fn drain_pipe_commands(&mut self) {
        let mut cmds = Vec::new();
        if let Some(ps) = &self.pipe_server {
            while let Ok(c) = ps.rx.try_recv() {
                cmds.push(c);
            }
        }
        for cmd in cmds {
            match cmd {
                PipeCommand::Load(req) => self.apply_load_request(*req),
                PipeCommand::Queue(v) => {
                    if let Some(items_val) = v.get("items") {
                        if let Ok(raw) = serde_json::from_value::<Vec<QueueItemRaw>>(items_val.clone())
                        {
                            let tot = v
                                .get("totalEps")
                                .or_else(|| v.get("total_eps"))
                                .and_then(value_as_f64)
                                .map(|n| n.round() as usize)
                                .unwrap_or(raw.len());
                            self.queue.replace(tot, convert_queue_items(&raw));
                            self.total_eps = self.total_eps.max(self.queue.total_eps as u32);
                        }
                    }
                    if let Some(idx) = v.get("index").and_then(value_as_f64) {
                        self.current_index = idx.round() as usize;
                    }
                    if let Some(ap) = v.get("autoplay").and_then(value_as_bool) {
                        self.prefs.autoplay = ap;
                    }
                    self.prefetched_next_index = None;
                    self.push_full_hud_state();
                }
                PipeCommand::QueuePush(v) => {
                    let target_obj = v.get("item").unwrap_or(&v);
                    if let Some(arr) = v.get("items") {
                        if let Ok(raw) = serde_json::from_value::<Vec<QueueItemRaw>>(arr.clone()) {
                            for ep in convert_queue_items(&raw) {
                                self.queue.push(ep);
                            }
                            self.total_eps = self.total_eps.max(self.queue.total_eps as u32);
                        }
                    } else if let Ok(item) = serde_json::from_value::<QueueItemRaw>(target_obj.clone()) {
                        let converted = convert_queue_items(&[item]);
                        for ep in converted {
                            self.queue.push(ep);
                        }
                        self.total_eps = self.total_eps.max(self.queue.total_eps as u32);
                    }
                }
                PipeCommand::QueueClear => {
                    self.queue.clear();
                    self.prefetched_next_index = None;
                }
                PipeCommand::Autoplay(v) => {
                    if let Some(on) = v
                        .get("on")
                        .or_else(|| v.get("enabled"))
                        .and_then(value_as_bool)
                    {
                        self.prefs.autoplay = on;
                    }
                    if let Some(d) = v.get("delay").and_then(value_as_f64) {
                        self.prefs.autoplay_delay = (d.round() as i64).clamp(0, 15) as u32;
                    }
                    self.push_full_hud_state();
                }
                PipeCommand::DanmakuData(v) => {
                    if !danmaku_reply_matches_episode(&self.series_id, &self.episode_id, &v) {
                        continue;
                    }
                    if let Some(arr) = v.get("items").and_then(Value::as_array) {
                        let mut added = false;
                        for item in arr {
                            let key = danmaku_item_key(item);
                            if self.danmaku_seen_keys.insert(key) {
                                self.danmaku_items.push(item.clone());
                                added = true;
                            }
                        }
                        if added {
                            if self.danmaku_items.len() > 2000 {
                                let drop_n = self.danmaku_items.len() - 1500;
                                self.danmaku_items.drain(..drop_n);
                                self.danmaku_seen_keys =
                                    self.danmaku_items.iter().map(danmaku_item_key).collect();
                            }
                            self.with_hud(|b| {
                                b.post_json(&json!({
                                    "type": "danmakuData",
                                    "items": self.danmaku_items,
                                }));
                            });
                        }
                    }
                }
                PipeCommand::SettingsSync(v) => {
                    if let Some(b) = v.get("danmakuEnabled").and_then(value_as_bool) {
                        self.prefs.danmaku.enabled = b;
                    }
                    if let Some(n) = v.get("danmakuArea").and_then(value_as_f64) {
                        self.prefs.danmaku.region = DanmakuPrefs::area_from_host(n);
                    }
                    if let Some(n) = v.get("danmakuOpacity").and_then(value_as_f64) {
                        self.prefs.danmaku.opacity = DanmakuPrefs::opacity_from_host(n);
                    }
                    if let Some(n) = v.get("danmakuFontScale").and_then(value_as_f64) {
                        self.prefs.danmaku.font_scale = DanmakuPrefs::font_scale_from_host(n);
                    }
                    if let Some(n) = v.get("danmakuSpeed").and_then(value_as_f64) {
                        self.prefs.danmaku.speed_sec = DanmakuPrefs::speed_sec_from_host(n);
                    }
                    if let Some(s) = v.get("danmakuFont").and_then(Value::as_str) {
                        self.prefs.danmaku.font_family = s.to_string();
                    }
                    if let Some(s) = v.get("danmakuColor").and_then(Value::as_str) {
                        self.prefs.danmaku.color = s.to_string();
                    }
                    if let Some(b) = v.get("danmakuBold").and_then(value_as_bool) {
                        self.prefs.danmaku.font_weight = if b { "700".into() } else { "400".into() };
                    }
                    if let Some(s) = v.get("danmakuStroke").and_then(Value::as_str) {
                        self.prefs.danmaku.stroke = s.to_string();
                    }
                    if let Some(vsr) = v.get("vsrEnabled").and_then(value_as_bool) {
                        self.prefs.vsr_enabled = vsr;
                        self.render.shared.set_vsr_enabled(vsr);
                    }
                    self.prefs.danmaku = self.prefs.danmaku.sanitized();
                    self.push_full_hud_state();
                }
                PipeCommand::DanmakuToggle(v) => {
                    if let Some(on) = v.get("on").or_else(|| v.get("enabled")).and_then(value_as_bool)
                    {
                        self.prefs.danmaku.enabled = on;
                        self.push_full_hud_state();
                    }
                }
                PipeCommand::Vsr(v) => {
                    if let Some(on) = v.get("on").or_else(|| v.get("enabled")).and_then(value_as_bool)
                    {
                        self.prefs.vsr_enabled = on;
                        self.render.shared.set_vsr_enabled(on);
                        self.push_full_hud_state();
                    }
                }
                PipeCommand::Pip(v) => {
                    let on = v.get("on").or_else(|| v.get("enabled")).and_then(value_as_bool);
                    self.toggle_pip(on);
                }
                PipeCommand::Pause(v) => {
                    let p = v
                        .get("paused")
                        .or_else(|| v.get("on"))
                        .and_then(value_as_bool)
                        .unwrap_or_else(|| !self.render.mpv.state.is_paused());
                    self.render.mpv.set_paused(p);
                    update_execution_wake_lock(!p);
                    self.push_full_hud_state();
                }
                PipeCommand::Play => {
                    self.render.mpv.set_paused(false);
                    update_execution_wake_lock(true);
                    self.push_full_hud_state();
                }
                PipeCommand::Seek(pos) => {
                    let dur = self.render.mpv.state.duration();
                    let target = mpv::clamp_seek_target(pos, dur);
                    self.last_seek_target = Some(target);
                    self.render.shared.mark_seek();
                    self.render.mpv.seek_absolute(target);
                }
                PipeCommand::Volume(v) => {
                    self.prefs.volume = v.clamp(0.0, 130.0);
                    self.render.mpv.set_volume(self.prefs.volume);
                    self.push_full_hud_state();
                }
                PipeCommand::Mute(opt) => {
                    let m = opt.unwrap_or(!self.prefs.muted);
                    self.prefs.muted = m;
                    self.render.mpv.set_muted(m);
                    self.push_full_hud_state();
                }
                PipeCommand::Speed(v) => {
                    self.prefs.speed = v.clamp(0.25, 4.0);
                    self.render.mpv.set_speed(self.prefs.speed);
                    self.push_full_hud_state();
                }
                PipeCommand::Fg(v) => {
                    let on = v
                        .get("on")
                        .or_else(|| v.get("enabled"))
                        .and_then(value_as_bool)
                        .unwrap_or(self.prefs.fg_enabled);
                    let mult = v
                        .get("mult")
                        .and_then(value_as_f64)
                        .map(|n| {
                            let r = n.round() as i64;
                            if r <= 0 { 0 } else { r.clamp(2, 5) as u32 }
                        })
                        .unwrap_or(self.prefs.fg_mult);
                    self.prefs.fg_enabled = on;
                    self.prefs.fg_mult = mult;
                    if let Some(raw_mode) = v
                        .get("mode")
                        .or_else(|| v.get("fgMode"))
                        .and_then(Value::as_str)
                    {
                        self.prefs.fg_mode = NativePlayerPrefs::normalize_fg_mode(raw_mode);
                        let eff = self.current_mode();
                        self.render
                            .shared
                            .mode_raw
                            .store(eff as u32, std::sync::atomic::Ordering::Release);
                    }
                    self.render
                        .shared
                        .fg_enabled
                        .store(on, std::sync::atomic::Ordering::Release);
                    self.render
                        .shared
                        .fg_mult
                        .store(mult, std::sync::atomic::Ordering::Release);
                    self.render.shared.wake();
                    self.emit_fg_changed_event();
                    self.push_full_hud_state();
                }
                PipeCommand::FgMode(mode) => {
                    self.prefs.fg_mode = NativePlayerPrefs::normalize_fg_mode(&mode);
                    let eff = self.current_mode();
                    self.render
                        .shared
                        .mode_raw
                        .store(eff as u32, std::sync::atomic::Ordering::Release);
                    self.render.shared.wake();
                    self.emit_fg_changed_event();
                    self.push_full_hud_state();
                }
                PipeCommand::Track(v) => {
                    if let Some(aid) = v.get("aid").and_then(value_as_f64) {
                        self.render.mpv.set_audio_track(aid.round() as i64);
                    }
                    if let Some(sid) = v.get("sid").and_then(value_as_f64) {
                        self.render.mpv.set_sub_track(sid.round() as i64);
                    }
                    if let Some(id) = v.get("id").and_then(value_as_f64).map(|n| n.round() as i64) {
                        match v.get("kind").and_then(Value::as_str).unwrap_or("audio") {
                            "sub" => self.render.mpv.set_sub_track(id),
                            _ => self.render.mpv.set_audio_track(id),
                        }
                    }
                }
                PipeCommand::Shot => {
                    self.render
                        .shared
                        .shot_requested
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.render.shared.wake();
                }
                PipeCommand::Theme(t) => self.set_theme(&t),
                PipeCommand::FrameStep(dir) => {
                    self.render.shared.mark_seek();
                    self.render.mpv.set_paused(true);
                    self.render.mpv.frame_step(dir);
                }
                PipeCommand::Loop(v) => {
                    if v.get("clear").and_then(value_as_bool).unwrap_or(false) {
                        self.ab_a = None;
                        self.ab_b = None;
                    } else {
                        self.ab_a = v.get("a").and_then(value_as_f64);
                        self.ab_b = v.get("b").and_then(value_as_f64);
                    }
                    self.push_full_hud_state();
                }
                PipeCommand::Aspect(raw) => {
                    self.prefs.aspect = AspectMode::parse_lossy(&raw);
                    self.render.mpv.set_aspect(self.prefs.aspect);
                    self.render.shared.set_aspect_mode(self.prefs.aspect);
                    self.push_full_hud_state();
                }
                PipeCommand::Stats(on) => {
                    if on.unwrap_or(true) {
                        let src = self.render.mpv.state.source_fps();
                        let out = self.render.shared.out_fps();
                        let fg = if !self.prefs.fg_enabled {
                            "关".to_string()
                        } else if self.prefs.fg_mult == 0 {
                            "自动".to_string()
                        } else {
                            format!("{}×", self.prefs.fg_mult)
                        };
                        self.push_hud_toast(&format!(
                            "源 {:.1}fps · 输出 {:.1}fps · 补帧 {}",
                            src, out, fg
                        ));
                    }
                }
                PipeCommand::Fullscreen(on) => self.toggle_fullscreen(on),
                PipeCommand::Quit => unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                },
            }
        }
    }

    fn handle_mpv_events(&mut self) {
        let mask = self.render.mpv.state.take_ui_events();
        if mask == 0 {
            return;
        }
        if (mask & UI_EVT_FILE_LOADED) != 0 {
            if let Some(deferred) = self.render.mpv.state.take_deferred_seek() {
                self.last_seek_target = Some(deferred);
                self.render.shared.mark_seek();
                self.render.mpv.seek_absolute(deferred);
            }
            self.render.shared.wake();
            self.push_full_hud_state();
            let mut m = serde_json::Map::new();
            m.insert("ep".into(), json!(self.ep));
            m.insert("index".into(), json!(self.current_index));
            self.pipe_writer.send_event("loaded", m);
            self.push_pipe_progress_now();
        }
        if (mask & UI_EVT_VIDEO_SIZE) != 0 {
            let (vw, vh) = self.render.mpv.state.video_size();
            self.adapt_window_for_video_dims(vw, vh);
            self.push_full_hud_state();
        }
        if (mask & UI_EVT_TRACKS) != 0 {
            let tracks = self.render.mpv.state.tracks_snapshot();
            self.with_hud(|b| b.post_json(&json!({ "type": "tracks", "tracks": tracks })));
            let (audio, subs, audio_id, sub_id) = format_tracks_payload(&tracks);
            let mut m = serde_json::Map::new();
            m.insert("tracks".into(), json!(tracks));
            m.insert("audio".into(), Value::Array(audio));
            m.insert("subs".into(), Value::Array(subs));
            m.insert("audioId".into(), json!(audio_id));
            m.insert("subId".into(), json!(sub_id));
            self.pipe_writer.send_event("tracks", m);
        }
        if (mask & UI_EVT_ERROR) != 0 {
            if let Some(err) = self.render.mpv.state.last_error_text() {
                let failure_code = self
                    .render
                    .mpv
                    .state
                    .last_failure_code
                    .load(std::sync::atomic::Ordering::Acquire);
                let can_recover = self.active_source.as_ref().is_some_and(|src| {
                    should_auto_recover_error(&src.url, &err, failure_code, src.retry_count)
                });
                if can_recover {
                    if let Some(src) = self.active_source.as_mut() {
                        src.retry_count += 1;
                        let snap = self.render.mpv.state.snapshot();
                        let resume_raw = self
                            .last_seek_target
                            .unwrap_or(snap.time_pos)
                            .max(0.0);
                        let resume_secs = mpv::clamp_seek_target(resume_raw, snap.duration);
                        let url = src.url.clone();
                        let key = src.key.clone();
                        let headers = src.headers.clone();
                        self.render.shared.reset_frames();
                        self.render.shared.mark_seek();
                        if self
                            .render
                            .mpv
                            .load_file(&url, &key, resume_secs, &headers)
                            .is_ok()
                        {
                            self.push_hud_toast("网络波动，正在恢复至目标位置…");
                            self.push_pipe_progress_now();
                            return;
                        }
                    }
                }
                self.push_hud_toast(&format!("播放出错: {err}"));
                let mut m = serde_json::Map::new();
                m.insert("msg".into(), json!(err));
                self.pipe_writer.send_event("error", m);
            }
            self.push_pipe_progress_now();
        }
        if (mask & UI_EVT_EOF) != 0 && self.last_eof_emitted_ep != Some(self.current_index) {
            self.last_eof_emitted_ep = Some(self.current_index);
            let snap = self.render.mpv.state.snapshot();
            let mut pend = serde_json::Map::new();
            pend.insert("pos".into(), json!(snap.duration.max(snap.time_pos)));
            pend.insert("ep".into(), json!(self.ep));
            self.pipe_writer.send_event("progressEnd", pend);

            let next_target = self.next_episode_target();
            let has_next = next_target.is_some();
            match decide_eof_autoplay_action(
                self.prefs.autoplay,
                self.prefs.autoplay_delay,
                next_target,
            ) {
                EofAutoplayAction::AdvanceImmediate(next_idx) => {
                    self.switch_episode_index(next_idx);
                }
                EofAutoplayAction::StartCountdown(next_idx) => {
                    self.countdown_target_index = next_idx;
                    self.countdown_active = true;
                    self.countdown_deadline = Some(
                        Instant::now() + Duration::from_secs(u64::from(self.prefs.autoplay_delay)),
                    );
                    self.push_full_hud_state();
                }
                EofAutoplayAction::StayStopped => {
                    let final_pos = snap.duration.max(snap.time_pos);
                    let mut eev = serde_json::Map::new();
                    eev.insert("pos".into(), json!(final_pos));
                    eev.insert("dur".into(), json!(snap.duration));
                    eev.insert("ep".into(), json!(self.ep));
                    eev.insert("final".into(), json!(!has_next));
                    self.pipe_writer.send_event("ended", eev);
                }
            }
        }
        if (mask & UI_EVT_PROGRESS) != 0 {
            let snap = self.render.mpv.state.snapshot();
            if snap.file_loaded
                && self.render.shared.has_presented_frame()
                && !snap.seeking
                && !snap.buffering
            {
                if let Some(src) = self.active_source.as_mut() {
                    src.retry_count = 0;
                }
                self.last_seek_target = None;
            }
            self.push_progress_to_hud();
            // Flush first-frame / buffering transitions immediately so host launch-gate never waits 400ms.
            if self.render.shared.has_presented_frame()
                && self.last_pipe_heartbeat.elapsed() >= Duration::from_millis(80)
            {
                self.push_pipe_progress_now();
            }
        }
    }

    fn on_heartbeat_timer(&mut self) {
        if let Ok(hud_opt) = self.wnd.hud.try_borrow() {
            if let Some(hud) = hud_opt.as_ref() {
                if let Ok(b) = hud.try_borrow() {
                    b.sync_to_hwnd_client_rect();
                }
            }
        }
        let snap = self.render.mpv.state.snapshot();

        // A-B loop enforcement
        if let (Some(a), Some(b)) = (self.ab_a, self.ab_b) {
            if b > a + 0.3 && !snap.seeking && snap.time_pos >= b && !snap.paused {
                self.render.shared.mark_seek();
                self.render.mpv.seek_absolute(a);
            }
        }

        // Autoplay countdown check
        if self.countdown_active {
            if let Some(deadline) = self.countdown_deadline {
                if Instant::now() >= deadline {
                    self.switch_episode_index(self.countdown_target_index);
                } else {
                    self.push_full_hud_state();
                }
            }
        }

        // Screenshot toast feedback
        if let Ok(mut g) = self.render.shared.last_shot_path.lock() {
            if let Some(p) = g.take() {
                if self.last_shot_notified.as_deref() != Some(&p) {
                    self.last_shot_notified = Some(p.clone());
                    self.push_hud_toast("截图已保存到图片文件夹");
                }
            }
        }

        // Sliding danmaku chunk prefetch & proactive next-episode URL prefetch
        if !snap.paused && snap.file_loaded {
            self.request_danmaku_window_for(snap.display_time_pos);
            self.request_danmaku_window_for(snap.display_time_pos + 15.0);
        }
        self.maybe_request_prefetch_next(snap.file_loaded, snap.display_time_pos);

        update_execution_wake_lock(!snap.paused && snap.file_loaded);
        self.push_progress_to_hud();

        // Emit throttled progress heartbeat to main TTV host (~400ms)
        if self.last_pipe_heartbeat.elapsed() >= Duration::from_millis(400) {
            self.push_pipe_progress_now();
        }
    }

    fn push_progress_to_hud(&self) {
        let snap = self.render.mpv.state.snapshot();
        let hw = self.render.mpv.state.hwdec_label();
        let payload = json!({
            "type": "progress",
            "pos": snap.display_time_pos,
            "seeking": snap.seeking,
            "seekSeq": self.last_hud_seek_seq,
            "duration": snap.duration,
            "buffer": snap.time_pos + snap.cache_duration,
            "paused": snap.paused,
            "buffering": snap.buffering,
            "hasFrame": snap.file_loaded,
            "outFps": self.render.shared.out_fps(),
            "srcFps": snap.source_fps,
            "videoW": snap.video_w,
            "videoH": snap.video_h,
            "hwdec": hw,
        });
        self.with_hud(|b| b.post_json(&payload));
    }

    fn save_and_emit_close(&mut self) {
        let (vw, vh) = self.render.mpv.state.video_size();
        let ori = self
            .adapted_orientation
            .unwrap_or_else(|| VideoOrientation::from_dims(vw, vh));
        if !self.wnd.fullscreen.get() && !self.wnd.pip.get() {
            let maximized = unsafe { IsZoomed(self.hwnd).as_bool() };
            self.prefs.placement.maximized = maximized;
            if !maximized {
                self.save_current_rect_to_orientation(ori);
            }
        } else if self.wnd.pip.get() {
            self.save_current_rect_to_orientation(ori);
        }
        let _ = self.prefs.save();

        let snap = self.render.mpv.state.snapshot();
        let mut m = serde_json::Map::new();
        m.insert("pos".into(), json!(snap.display_time_pos));
        m.insert("dur".into(), json!(snap.duration));
        m.insert("ep".into(), json!(self.ep));
        self.pipe_writer.send_event("closed", m);
        self.pipe_writer.flush();
    }
}

fn pack_screen_point_lparam(pt: POINT) -> LPARAM {
    let x = (pt.x as i16 as u16) as u32;
    let y = (pt.y as i16 as u16) as u32;
    LPARAM(((y << 16) | x) as isize)
}

fn primary_drag_vk(swap_buttons: bool) -> i32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_RBUTTON;
    if swap_buttons {
        VK_RBUTTON.0 as i32
    } else {
        VK_LBUTTON.0 as i32
    }
}

fn should_start_drag(async_state: i16, sync_state: i16, lbutton_up_queued: bool) -> bool {
    if (async_state as u16 & 0x8000) != 0 {
        return true;
    }
    (sync_state as u16 & 0x8000) != 0 && !lbutton_up_queued
}

unsafe fn is_primary_drag_button_down(hwnd: HWND) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, PeekMessageW, MSG, PM_NOREMOVE, SM_SWAPBUTTON,
    };
    let vk = primary_drag_vk(GetSystemMetrics(SM_SWAPBUTTON) != 0);
    let async_state = GetAsyncKeyState(vk);
    let sync_state = GetKeyState(VK_LBUTTON.0 as i32);
    let mut queued = MSG::default();
    let up_queued = PeekMessageW(
        &mut queued,
        Some(hwnd),
        WM_LBUTTONUP,
        WM_LBUTTONUP,
        PM_NOREMOVE,
    )
    .as_bool();
    should_start_drag(async_state, sync_state, up_queued)
}

unsafe fn SendMessageLikeDrag(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, HTCAPTION};
    let mut screen_pt = POINT::default();
    let lparam = if GetCursorPos(&mut screen_pt).is_ok() {
        pack_screen_point_lparam(screen_pt)
    } else {
        LPARAM(0)
    };
    let _ = SendMessageW(
        hwnd,
        WM_NCLBUTTONDOWN,
        Some(WPARAM(HTCAPTION as usize)),
        Some(lparam),
    );
}

fn mode_as_str(mode: InterpContentMode) -> &'static str {
    match mode {
        InterpContentMode::Drama => "drama",
        InterpContentMode::Comic => "comic",
        InterpContentMode::Anime => "anime",
    }
}

fn compute_playback_phase(
    has_source: bool,
    failure: &str,
    file_loaded: bool,
    has_presented_frame: bool,
    buffering: bool,
) -> &'static str {
    if failure != "none" {
        "failed"
    } else if !has_source {
        "idle"
    } else if !file_loaded {
        "loading"
    } else if buffering {
        "buffering"
    } else if has_presented_frame {
        "ready"
    } else {
        "loading"
    }
}

fn compute_fg_engine_name(
    fg_enabled: bool,
    pipeline_ready: bool,
    monitor_hz: f64,
    src_fps: f64,
) -> &'static str {
    if !fg_enabled {
        "off"
    } else if !pipeline_ready {
        "gpu-interp-inactive"
    } else if monitor_hz.is_finite() && monitor_hz >= 24.0 && src_fps >= monitor_hz - 0.5 {
        "gpu-panel-bypass"
    } else {
        "gpu-optical-flow"
    }
}

fn track_display_label(track: &TrackInfo, fallback_prefix: &str) -> String {
    let title = track.title.trim();
    let lang = track.lang.trim();
    if !title.is_empty() && !lang.is_empty() {
        format!("{title} ({lang})")
    } else if !title.is_empty() {
        title.to_string()
    } else if !lang.is_empty() {
        lang.to_string()
    } else {
        format!("{fallback_prefix} {}", track.id)
    }
}

fn format_tracks_payload(tracks: &[TrackInfo]) -> (Vec<Value>, Vec<Value>, i64, i64) {
    let mut audio = Vec::new();
    let mut subs = Vec::new();
    let mut audio_id = -1i64;
    let mut sub_id = -1i64;
    for t in tracks {
        match t.kind.as_str() {
            "audio" => {
                if t.selected {
                    audio_id = t.id;
                }
                audio.push(json!({
                    "id": t.id,
                    "label": track_display_label(t, "音轨"),
                }));
            }
            "sub" => {
                if t.selected {
                    sub_id = t.id;
                }
                subs.push(json!({
                    "id": t.id,
                    "label": track_display_label(t, "字幕"),
                }));
            }
            _ => {}
        }
    }
    (audio, subs, audio_id, sub_id)
}

fn danmaku_reply_matches_episode(
    current_series_id: &str,
    current_episode_id: &str,
    reply: &Value,
) -> bool {
    let reply_series = reply
        .get("seriesId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !reply_series.is_empty()
        && !current_series_id.trim().is_empty()
        && reply_series != current_series_id.trim()
    {
        return false;
    }
    let reply_episode = reply
        .get("episodeId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !reply_episode.is_empty()
        && !current_episode_id.trim().is_empty()
        && reply_episode != current_episode_id.trim()
    {
        return false;
    }
    true
}

fn danmaku_item_key(item: &Value) -> String {
    if let Some(id_str) = item.get("id").and_then(Value::as_str) {
        let s = id_str.trim();
        if !s.is_empty() {
            return format!("id:{s}");
        }
    } else if let Some(id_num) = item.get("id").and_then(value_as_f64) {
        return format!("id:{}", id_num.round() as i64);
    }
    let t = item
        .get("timeMs")
        .and_then(value_as_f64)
        .map(|n| n.round() as i64)
        .unwrap_or(0);
    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
    format!("txt:{t}:{text}")
}

fn convert_queue_items(items: &[QueueItemRaw]) -> Vec<QueueEpisode> {
    items
        .iter()
        .enumerate()
        .filter_map(|(pos_idx, raw)| {
            let url = raw.url.trim().to_string();
            if url.is_empty()
                && raw.vid.trim().is_empty()
                && raw.episode_id.trim().is_empty()
                && raw.title.trim().is_empty()
            {
                return None;
            }
            let idx = raw.index.unwrap_or(pos_idx);
            let ep = if raw.ep > 0 { raw.ep } else { idx as u32 + 1 };
            Some(QueueEpisode {
                index: idx,
                url,
                key: raw.key.clone(),
                ep,
                title: raw.title.clone(),
                vid: raw.vid.clone(),
                series_id: raw.series_id.clone(),
                episode_id: raw.episode_id.clone(),
                headers: raw.headers.clone(),
                duration: raw.duration,
                start: raw.start,
            })
        })
        .collect()
}

fn update_execution_wake_lock(playing: bool) {
    unsafe {
        if playing {
            let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
        } else {
            let _ = SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EofAutoplayAction {
    AdvanceImmediate(usize),
    StartCountdown(usize),
    StayStopped,
}

fn decide_eof_autoplay_action(
    autoplay: bool,
    autoplay_delay: u32,
    next_target: Option<usize>,
) -> EofAutoplayAction {
    if !autoplay {
        return EofAutoplayAction::StayStopped;
    }
    match next_target {
        Some(idx) if autoplay_delay == 0 => EofAutoplayAction::AdvanceImmediate(idx),
        Some(idx) => EofAutoplayAction::StartCountdown(idx),
        None => EofAutoplayAction::StayStopped,
    }
}

fn load_embedded_player_icons(hinst: HINSTANCE) -> (Option<HICON>, Option<HICON>) {
    // MAKEINTRESOURCEW(1): Win32 resource ID 1 encoded cleanly in low pointer bits without provenance
    let icon_res = PCWSTR(std::ptr::without_provenance(1));
    let big_w = unsafe { GetSystemMetrics(SM_CXICON) }.clamp(32, 256);
    let big_h = unsafe { GetSystemMetrics(SM_CYICON) }.clamp(32, 256);
    let big = unsafe { LoadImageW(Some(hinst), icon_res, IMAGE_ICON, big_w, big_h, LR_SHARED) }
        .ok()
        .map(|h| HICON(h.0));
    let sm_w = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    let sm_h = unsafe { GetSystemMetrics(SM_CYSMICON) }.clamp(16, 64);
    let small = unsafe { LoadImageW(Some(hinst), icon_res, IMAGE_ICON, sm_w, sm_h, LR_SHARED) }
        .ok()
        .map(|h| HICON(h.0));
    (big, small)
}

fn run_player(args: LaunchArgs) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    }

    // Warm up / compile HLSL optical-flow DXBC cache concurrently off the UI thread!
    let (blob_tx, blob_rx) = mpsc::channel();
    thread::Builder::new()
        .name("ttv-shader-warmup".into())
        .spawn(move || {
            let _ = blob_tx.send(CompiledShaderBlobs::load_or_compile());
        })
        .map_err(|e| format!("spawn shader warmup thread failed: {e}"))?;

    let mut prefs = NativePlayerPrefs::load();
    if let Some(fg) = args.fg_override {
        prefs.fg_enabled = fg;
    }
    if let Some(m) = args.fg_mult_override {
        prefs.fg_mult = m;
    }
    if let Some(ref fm) = args.fg_mode_override {
        prefs.fg_mode = NativePlayerPrefs::normalize_fg_mode(fm);
    }
    if let Some(ref t) = args.theme_override {
        prefs.theme = NativePlayerPrefs::normalize_theme(t);
    }
    if let Some(ap) = args.autoplay_override {
        prefs.autoplay = ap;
    }
    if let Some(ad) = args.autoplay_delay_override {
        prefs.autoplay_delay = ad;
    }
    if let Some(vsr) = args.vsr_override {
        prefs.vsr_enabled = vsr;
    }
    let kind = args
        .kind_override
        .clone()
        .unwrap_or_else(|| "drama".to_string());

    let mut initial_load: Option<LoadRequest> = None;
    if let Some(ref path) = args.load_file {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(req) = LoadRequest::from_json_str(&raw) {
                initial_load = Some(req);
            }
        }
    } else if let Some(ref url) = args.direct_url {
        initial_load = Some(LoadRequest {
            url: url.clone(),
            title: url.clone(),
            ep: 1,
            ..Default::default()
        });
    }

    // Choose crisp initial window size from per-orientation saved bounds or ideal aspect sizing.
    let (hint_vw, hint_vh) = initial_load.as_ref().map(|r| (r.vw, r.vh)).unwrap_or((0, 0));
    let init_ori = VideoOrientation::from_dims(hint_vw, hint_vh);
    let probe_x = prefs.placement.x.unwrap_or(180);
    let probe_y = prefs.placement.y.unwrap_or(90);
    let init_work = monitor_work_area_at_point(probe_x + 400, probe_y + 300);
    let init_ww = (init_work.right - init_work.left).max(480);
    let init_wh = (init_work.bottom - init_work.top).max(360);
    let (raw_x, raw_y, win_w, win_h) = if let Some(saved) = prefs.placement.rect_for(init_ori).cloned() {
        (
            saved.x.or(prefs.placement.x).unwrap_or_else(|| init_work.left + (init_ww - saved.w as i32) / 2),
            saved.y.or(prefs.placement.y).unwrap_or_else(|| init_work.top + (init_wh - saved.h as i32) / 2),
            saved.w as i32,
            saved.h as i32,
        )
    } else if hint_vw == 0 && hint_vh == 0 && prefs.placement.x.is_some() {
        (
            prefs.placement.x.unwrap_or(180),
            prefs.placement.y.unwrap_or(90),
            prefs.placement.w as i32,
            prefs.placement.h as i32,
        )
    } else {
        let (iw, ih) = ideal_window_size_for_video(hint_vw, hint_vh, init_ww, init_wh);
        (
            init_work.left + (init_ww - iw) / 2,
            init_work.top + (init_wh - ih) / 2,
            iw,
            ih,
        )
    };
    let (init_x, init_y, init_w, init_h) = clamp_window_to_monitor(raw_x, raw_y, win_w, win_h);

    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map_err(|e| format!("GetModuleHandleW: {e}"))?
        .into();
    let (icon_big, icon_small) = load_embedded_player_icons(hinst);
    let cls_name = w!("TTVNativePlayerWindowClass");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(player_wnd_proc),
        hInstance: hinst,
        hIcon: icon_big.unwrap_or_default(),
        hIconSm: icon_small.unwrap_or_default(),
        lpszClassName: cls_name,
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }

    let style = WS_POPUP | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU | WS_CLIPCHILDREN;
    let ex_style = WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW;
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            cls_name,
            w!("TTV 独立播放器"),
            style,
            init_x,
            init_y,
            init_w,
            init_h,
            None,
            None,
            Some(hinst),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

    unsafe {
        if let Some(ico) = icon_big {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(ico.0 as isize)),
            );
        }
        if let Some(ico) = icon_small {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(ico.0 as isize)),
            );
        }
    }

    set_dwm_rounded_corners(hwnd, !prefs.placement.maximized);
    if prefs.topmost {
        set_window_topmost(hwnd, true);
    }

    let (cw, ch) = client_size_px(hwnd);
    let dcomp = DCompVideoHost::new(hwnd, cw, ch)?;
    let dcomp_dev_for_hud = dcomp.dcomp_device.clone();
    let hud_visual_for_hud = dcomp.hud_visual.clone();

    let mode = InterpContentMode::from_modes(&prefs.fg_mode, &kind);
    let light_theme = prefs.theme == "light";
    let render = RenderController::start(
        hwnd,
        dcomp,
        prefs.fg_enabled,
        prefs.fg_mult,
        prefs.vsr_enabled,
        mode,
        light_theme,
        blob_rx,
    )?;

    let pipe_server = if let Some(ref pn) = args.pipe_name {
        Some(PipeServer::start(pn, Some(hwnd.0 as isize))?)
    } else {
        None
    };
    let pipe_writer = pipe_server
        .as_ref()
        .map(|s| s.writer.clone())
        .unwrap_or_default();

    let wnd = Rc::new(WndShared {
        fullscreen: Cell::new(false),
        pip: Cell::new(false),
        cursor_hidden: Cell::new(false),
        mouse_tracked: Cell::new(false),
        releasing_capture: Cell::new(false),
        render_shared: render.shared.clone(),
        hud: RefCell::new(None),
    });

    let app = Rc::new(RefCell::new(AppController {
        hwnd,
        prefs,
        queue: AutoplayQueue::default(),
        hold_boost: HoldBoostState::default(),
        render,
        wnd: wnd.clone(),
        pipe_server,
        pipe_writer,
        title: "TTV 独立播放器".into(),
        kind,
        series_id: String::new(),
        episode_id: String::new(),
        vid: String::new(),
        ep: 1,
        total_eps: 1,
        current_index: 0,
        saved_fullscreen: None,
        saved_pip_rect: None,
        adapted_orientation: Some(init_ori),
        ab_a: None,
        ab_b: None,
        countdown_active: false,
        countdown_deadline: None,
        countdown_target_index: 0,
        danmaku_items: Vec::new(),
        danmaku_requested_windows: BTreeSet::new(),
        danmaku_seen_keys: BTreeSet::new(),
        last_pipe_heartbeat: Instant::now(),
        last_eof_emitted_ep: None,
        prefetched_next_index: None,
        last_shot_notified: None,
        last_hud_seek_seq: 0,
        last_out_ratio: 1.0,
        has_requested_source: false,
        active_source: None,
        last_seek_target: None,
    }));

    let app_for_hud = Rc::downgrade(&app);
    let wnd_for_hud = wnd.clone();
    let deferred_hud_msgs = Rc::new(RefCell::new(VecDeque::<Value>::new()));
    let deferred_for_hud = deferred_hud_msgs.clone();
    let hud = WebViewHudInner::start(hwnd, dcomp_dev_for_hud, hud_visual_for_hud, move |val| {
        if val.get("cmd").and_then(Value::as_str) == Some("videoRect") {
            let x = val.get("x").and_then(value_as_f64).unwrap_or(0.0).round() as i32;
            let y = val.get("y").and_then(value_as_f64).unwrap_or(0.0).round() as i32;
            let w = val.get("w").and_then(value_as_f64).unwrap_or(64.0).round() as u32;
            let h = val.get("h").and_then(value_as_f64).unwrap_or(64.0).round() as u32;
            wnd_for_hud
                .render_shared
                .set_video_rect(x, y, w.max(64), h.max(64));
            return;
        }
        if let Some(rc) = app_for_hud.upgrade() {
            if let Ok(mut a) = rc.try_borrow_mut() {
                a.handle_hud_message(val);
            } else if let Ok(mut q) = deferred_for_hud.try_borrow_mut() {
                if q.len() < 64 {
                    q.push_back(val);
                }
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_APP_DRAIN_HUD_MSGS, WPARAM(0), LPARAM(0));
                }
            }
        }
    })?;
    *wnd.hud.borrow_mut() = Some(hud);
    {
        let mut a = app.borrow_mut();
        a.apply_initial_prefs_to_mpv();
        if let Some(req) = initial_load {
            a.apply_load_request(req);
        }
    }

    let user_data = Rc::new(WindowUserData {
        wnd: wnd.clone(),
        app: app.clone(),
        deferred_hud_msgs,
    });
    let raw_ud_ptr = Rc::into_raw(user_data) as isize;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_ud_ptr);
        SetTimer(Some(hwnd), TIMER_HEARTBEAT_ID, 200, None);
        let maximized = app.borrow().prefs.placement.maximized;
        let _ = ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOW });
    }
    sync_surfaces_to_hwnd(hwnd, &wnd);

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Rc::from_raw(raw_ud_ptr as *const WindowUserData));
        update_execution_wake_lock(false);
        CoUninitialize();
    }
    Ok(())
}

unsafe extern "system" fn player_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let raw_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let ud_opt: Option<&WindowUserData> = if raw_ptr != 0 {
        Some(&*(raw_ptr as *const WindowUserData))
    } else {
        None
    };

    match msg {
        WM_NCCALCSIZE => {
            if wparam.0 != 0 {
                let ncc = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let is_fullscreen = ud_opt.is_some_and(|u| u.wnd.fullscreen.get());
                if !is_fullscreen && IsZoomed(hwnd).as_bool() {
                    ncc.rgrc[0] = monitor_work_area(hwnd);
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            let is_fs = ud_opt.is_some_and(|u| u.wnd.fullscreen.get());
            let is_pip = ud_opt.is_some_and(|u| u.wnd.pip.get());
            let (mon, work) = monitor_areas(hwnd);
            let bounds = compute_minmax_bounds(mon, work, is_fs, is_pip);
            mmi.ptMinTrackSize.x = bounds.min_w;
            mmi.ptMinTrackSize.y = bounds.min_h;
            mmi.ptMaxPosition.x = bounds.max_pos_x;
            mmi.ptMaxPosition.y = bounds.max_pos_y;
            mmi.ptMaxSize.x = bounds.max_w;
            mmi.ptMaxSize.y = bounds.max_h;
            mmi.ptMaxTrackSize.x = bounds.max_track_w;
            mmi.ptMaxTrackSize.y = bounds.max_track_h;
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let is_fs = ud_opt.is_some_and(|u| u.wnd.fullscreen.get());
            if !is_fs && !IsZoomed(hwnd).as_bool() {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some(hit) = nchittest_resize_border(hwnd, x, y, 6) {
                    return LRESULT(hit);
                }
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_SIZE => {
            if wparam.0 != SIZE_MINIMIZED as usize {
                if let Some(u) = ud_opt {
                    sync_surfaces_to_hwnd(hwnd, &u.wnd);
                }
            }
            LRESULT(0)
        }
        WM_MOVE => {
            if let Some(u) = ud_opt {
                if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                    if let Some(hud) = hud_opt.as_ref() {
                        if let Ok(b) = hud.try_borrow() {
                            b.notify_position_changed();
                            b.sync_to_hwnd_client_rect();
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            // Keep Windows' move/size capture intact: DefWindowProcW acquires
            // SetCapture(hwnd) before sending WM_ENTERSIZEMOVE.
            if let Some(u) = ud_opt {
                if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                    if let Some(hud) = hud_opt.as_ref() {
                        if let Ok(mut b) = hud.try_borrow_mut() {
                            b.post_json(&json!({ "type": "hostCaptureLost" }));
                        }
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CANCELMODE => {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE | WM_SHOWWINDOW => {
            if let Some(u) = ud_opt {
                sync_surfaces_to_hwnd(hwnd, &u.wnd);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DISPLAYCHANGE => {
            if let Some(u) = ud_opt {
                u.wnd.render_shared.set_monitor_hz(monitor_refresh_hz(hwnd));
                sync_surfaces_to_hwnd(hwnd, &u.wnd);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let rect_ptr = lparam.0 as *const RECT;
            if !rect_ptr.is_null() {
                let r = *rect_ptr;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if let Some(u) = ud_opt {
                u.wnd.render_shared.set_monitor_hz(monitor_refresh_hz(hwnd));
                sync_surfaces_to_hwnd(hwnd, &u.wnd);
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Some(u) = ud_opt {
                if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                    if let Some(hud) = hud_opt.as_ref() {
                        if let Ok(b) = hud.try_borrow() {
                            b.move_focus();
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let hit = (lparam.0 & 0xFFFF) as u16 as u32;
            if hit == HTCLIENT {
                if let Some(u) = ud_opt {
                    if u.wnd.cursor_hidden.get() {
                        windows::Win32::UI::WindowsAndMessaging::SetCursor(None);
                        return LRESULT(1);
                    }
                    if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                        if let Some(hud) = hud_opt.as_ref() {
                            if let Ok(b) = hud.try_borrow() {
                                b.apply_cursor();
                                return LRESULT(1);
                            }
                        }
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN
        | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if let Some(u) = ud_opt {
                if msg == WM_MOUSEMOVE && !u.wnd.mouse_tracked.get() {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    if TrackMouseEvent(&mut tme).is_ok() {
                        u.wnd.mouse_tracked.set(true);
                    }
                }
                let mut pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                if (msg == WM_LBUTTONDOWN || msg == WM_LBUTTONDBLCLK) && GetCapture() != hwnd {
                    SetCapture(hwnd);
                }
                let vkeys = (wparam.0 & 0xFFFF) as i32;
                let mdata = ((wparam.0 >> 16) & 0xFFFF) as u32;
                let kind = match msg {
                    WM_LBUTTONDOWN => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
                    WM_LBUTTONUP => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
                    WM_LBUTTONDBLCLK => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK,
                    WM_RBUTTONDOWN => COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
                    WM_RBUTTONUP => COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP,
                    WM_MBUTTONDOWN => COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
                    WM_MBUTTONUP => COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
                    WM_MOUSEWHEEL => COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
                    WM_MOUSEHWHEEL => COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
                    _ => COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                };
                if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                    if let Some(hud) = hud_opt.as_ref() {
                        if let Ok(b) = hud.try_borrow() {
                            b.send_mouse(kind, vkeys, mdata, pt);
                        }
                    }
                }
                if msg == WM_LBUTTONUP && GetCapture() == hwnd {
                    // Do not emit `hostCaptureLost` for our own normal mouse-up release;
                    // otherwise WebView2 IPC can race ahead of DOM `pointerup` and abort clicks.
                    u.wnd.releasing_capture.set(true);
                    let _ = ReleaseCapture();
                    u.wnd.releasing_capture.set(false);
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            if lparam.0 != hwnd.0 as isize {
                if let Some(u) = ud_opt {
                    if !u.wnd.releasing_capture.get() {
                        if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                            if let Some(hud) = hud_opt.as_ref() {
                                if let Ok(mut b) = hud.try_borrow_mut() {
                                    b.post_json(&json!({ "type": "hostCaptureLost" }));
                                }
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            if let Some(u) = ud_opt {
                u.wnd.mouse_tracked.set(false);
                if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                    if let Some(hud) = hud_opt.as_ref() {
                        if let Ok(mut b) = hud.try_borrow_mut() {
                            b.notify_host_leave();
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_START_DRAG => {
            if let Some(u) = ud_opt {
                if !u.wnd.fullscreen.get() && is_primary_drag_button_down(hwnd) {
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        let _ = ScreenToClient(hwnd, &mut pt);
                    }
                    u.wnd.mouse_tracked.set(false);
                    if let Ok(hud_opt) = u.wnd.hud.try_borrow() {
                        if let Some(hud) = hud_opt.as_ref() {
                            if let Ok(b) = hud.try_borrow() {
                                b.release_pointer_capture_for_drag(pt);
                            }
                        }
                    }
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    SendMessageLikeDrag(hwnd);
                    sync_surfaces_to_hwnd(hwnd, &u.wnd);
                }
            }
            LRESULT(0)
        }
        WM_APP_DRAIN_HUD_MSGS => {
            if let Some(u) = ud_opt {
                let pending: Vec<Value> = if let Ok(mut q) = u.deferred_hud_msgs.try_borrow_mut() {
                    q.drain(..).collect()
                } else {
                    Vec::new()
                };
                if !pending.is_empty() {
                    if let Ok(mut a) = u.app.try_borrow_mut() {
                        for val in pending {
                            a.handle_hud_message(val);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_PIPE_CMD => {
            if let Some(u) = ud_opt {
                if let Ok(mut a) = u.app.try_borrow_mut() {
                    a.drain_pipe_commands();
                }
            }
            LRESULT(0)
        }
        WM_APP_MPV_EVENT => {
            if let Some(u) = ud_opt {
                if let Ok(mut a) = u.app.try_borrow_mut() {
                    a.handle_mpv_events();
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_HEARTBEAT_ID {
                if let Some(u) = ud_opt {
                    if let Ok(mut a) = u.app.try_borrow_mut() {
                        a.on_heartbeat_timer();
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = KillTimer(Some(hwnd), TIMER_HEARTBEAT_ID);
            if let Some(u) = ud_opt {
                if let Ok(mut a) = u.app.try_borrow_mut() {
                    a.save_and_emit_close();
                    a.render.shutdown();
                }
                if let Some(hud) = u.wnd.hud.borrow_mut().take() {
                    if let Ok(mut b) = hud.try_borrow_mut() {
                        b.close();
                    }
                }
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_RBUTTON;

    #[test]
    fn pack_screen_point_lparam_preserves_signed_multimonitor_coords() {
        let zero = pack_screen_point_lparam(POINT { x: 0, y: 0 });
        assert_eq!(zero.0, 0);

        let pos = pack_screen_point_lparam(POINT { x: 1920, y: 540 });
        assert_eq!((pos.0 & 0xFFFF) as i16, 1920);
        assert_eq!(((pos.0 >> 16) & 0xFFFF) as i16, 540);

        // Secondary monitor positioned left/above the primary display.
        let neg = pack_screen_point_lparam(POINT { x: -320, y: -48 });
        assert_eq!((neg.0 & 0xFFFF) as i16, -320);
        assert_eq!(((neg.0 >> 16) & 0xFFFF) as i16, -48);
    }

    #[test]
    fn primary_drag_vk_honors_swapped_mouse_buttons() {
        assert_eq!(primary_drag_vk(false), VK_LBUTTON.0 as i32);
        assert_eq!(primary_drag_vk(true), VK_RBUTTON.0 as i32);
    }

    #[test]
    fn should_start_drag_rejects_released_or_queued_up_clicks() {
        let down = -32768i16;
        assert!(should_start_drag(down, 0, false));
        assert!(should_start_drag(down, down, true));
        assert!(should_start_drag(0, down, false));
        assert!(!should_start_drag(0, down, true));
        assert!(!should_start_drag(0, 0, false));
    }

    #[test]
    fn playback_phase_and_fg_engine_follow_host_contract() {
        assert_eq!(compute_playback_phase(false, "none", false, false, false), "idle");
        assert_eq!(compute_playback_phase(true, "network", true, true, false), "failed");
        assert_eq!(compute_playback_phase(true, "none", false, false, false), "loading");
        assert_eq!(compute_playback_phase(true, "none", true, false, false), "loading");
        assert_eq!(compute_playback_phase(true, "none", true, false, true), "buffering");
        assert_eq!(compute_playback_phase(true, "none", true, true, false), "ready");

        assert_eq!(compute_fg_engine_name(false, true, 165.0, 25.0), "off");
        assert_eq!(compute_fg_engine_name(true, false, 165.0, 25.0), "gpu-interp-inactive");
        assert_eq!(compute_fg_engine_name(true, true, 60.0, 60.0), "gpu-panel-bypass");
        assert_eq!(compute_fg_engine_name(true, true, 165.0, 25.0), "gpu-optical-flow");
    }

    #[test]
    fn tracks_payload_matches_host_schema() {
        let tracks = vec![
            TrackInfo {
                id: 1,
                kind: "audio".into(),
                title: "国语".into(),
                lang: "zh".into(),
                selected: true,
            },
            TrackInfo {
                id: 2,
                kind: "audio".into(),
                title: String::new(),
                lang: "en".into(),
                selected: false,
            },
            TrackInfo {
                id: 3,
                kind: "sub".into(),
                title: "简体中文字幕".into(),
                lang: String::new(),
                selected: false,
            },
        ];
        let (audio, subs, aid, sid) = format_tracks_payload(&tracks);
        assert_eq!(aid, 1);
        assert_eq!(sid, -1);
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0]["label"], json!("国语 (zh)"));
        assert_eq!(audio[1]["label"], json!("en"));
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0]["id"], json!(3));
        assert_eq!(subs[0]["label"], json!("简体中文字幕"));
    }

    #[test]
    fn danmaku_reply_guard_rejects_stale_episode_and_dedupes() {
        let stale = json!({
            "seriesId": "s1",
            "episodeId": "ep1",
            "items": [{"id": "d1", "timeMs": 1200, "text": "hi"}]
        });
        assert!(danmaku_reply_matches_episode("s1", "ep1", &stale));
        assert!(!danmaku_reply_matches_episode("s1", "ep2", &stale));
        assert!(!danmaku_reply_matches_episode("s2", "ep1", &stale));

        let k1 = danmaku_item_key(&json!({"id": "abc", "timeMs": 1000, "text": "a"}));
        let k2 = danmaku_item_key(&json!({"id": "abc", "timeMs": 1000, "text": "a"}));
        assert_eq!(k1, k2);
    }

    #[test]
    fn eof_autoplay_advances_immediately_when_delay_zero() {
        assert_eq!(
            decide_eof_autoplay_action(true, 0, Some(2)),
            EofAutoplayAction::AdvanceImmediate(2)
        );
        assert_eq!(
            decide_eof_autoplay_action(true, 3, Some(2)),
            EofAutoplayAction::StartCountdown(2)
        );
        assert_eq!(
            decide_eof_autoplay_action(true, 0, None),
            EofAutoplayAction::StayStopped
        );
        assert_eq!(
            decide_eof_autoplay_action(false, 0, Some(2)),
            EofAutoplayAction::StayStopped
        );
    }

    #[test]
    fn queue_items_preserve_cenc_decryption_key() {
        let raw: QueueItemRaw = serde_json::from_value(json!({
            "index": 1,
            "url": "https://cdn.example/ep2.mp4",
            "key": "00112233445566778899aabbccddeeff",
            "ep": 2,
        }))
        .expect("parse raw item");
        let converted = convert_queue_items(&[raw]);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].key, "00112233445566778899aabbccddeeff");

        let meta_raw: QueueItemRaw = serde_json::from_value(json!({
            "index": 2,
            "url": "",
            "vid": "7700112233",
            "episodeId": "ep-3",
            "ep": 3,
            "title": "第3集",
        }))
        .expect("parse metadata item");
        let meta_converted = convert_queue_items(&[meta_raw]);
        assert_eq!(meta_converted.len(), 1);
        assert_eq!(meta_converted[0].vid, "7700112233");
        assert_eq!(meta_converted[0].url, "");
    }

    #[test]
    fn bundled_player_ico_is_valid_multi_resolution_icon() {
        let ico = include_bytes!("../assets/ttv-player.ico");
        assert!(ico.len() > 22);
        assert_eq!(&ico[0..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes([ico[4], ico[5]]);
        assert!(count >= 5, "expected multi-resolution icon, got {count}");
    }

    #[test]
    fn seek_error_auto_recovery_guards_permanent_and_local_errors() {
        assert!(should_auto_recover_error(
            "https://cdn.example/video.mp4",
            "End of file",
            mpv::FAILURE_NETWORK,
            0
        ));
        assert!(should_auto_recover_error(
            "http://127.0.0.1:9000/stream.m3u8",
            "Connection reset by peer",
            mpv::FAILURE_NETWORK,
            1
        ));
        assert!(!should_auto_recover_error(
            "https://cdn.example/video.mp4",
            "Connection reset by peer",
            mpv::FAILURE_NETWORK,
            2
        ));
        assert!(!should_auto_recover_error(
            "https://cdn.example/video.mp4",
            "HTTP error 403 Forbidden",
            mpv::FAILURE_NETWORK,
            0
        ));
        assert!(!should_auto_recover_error(
            "https://cdn.example/video.mp4",
            "d3d11va video decoder failed",
            mpv::FAILURE_DECODE,
            0
        ));
        assert!(!should_auto_recover_error(
            "C:\\videos\\local.mp4",
            "End of file",
            mpv::FAILURE_NETWORK,
            0
        ));
    }
}
