use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, FreeLibrary, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::ipc::WM_APP_MPV_EVENT;
use crate::prefs::AspectMode;

// ── libmpv C ABI constants (mpv/client.h + mpv/render.h + mpv/render_d3d11.h) ──
const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_LOG_MESSAGE: c_int = 2;
const MPV_EVENT_START_FILE: c_int = 6;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_FILE_LOADED: c_int = 8;
const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;
const MPV_EVENT_SEEK: c_int = 20;
const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

const MPV_END_FILE_REASON_EOF: c_int = 0;
const MPV_END_FILE_REASON_ERROR: c_int = 4;

const MPV_RENDER_PARAM_INVALID: c_int = 0;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
const MPV_RENDER_PARAM_D3D11_INIT_PARAMS: c_int = 21;
const MPV_RENDER_PARAM_D3D11_FBO: c_int = 22;
const MPV_RENDER_UPDATE_FRAME: u64 = 1;

// Observed property userdata keys.
const PROP_TIME_POS: u64 = 1;
const PROP_DURATION: u64 = 2;
const PROP_CACHE_DURATION: u64 = 3;
const PROP_PAUSE: u64 = 4;
const PROP_PAUSED_FOR_CACHE: u64 = 5;
const PROP_EOF_REACHED: u64 = 6;
const PROP_CORE_IDLE: u64 = 7;
const PROP_CONTAINER_FPS: u64 = 8;
const PROP_ESTIMATED_VF_FPS: u64 = 9;
const PROP_VIDEO_W: u64 = 10;
const PROP_VIDEO_H: u64 = 11;
const PROP_AID: u64 = 12;
const PROP_SID: u64 = 13;
const PROP_TRACK_LIST: u64 = 14;
const PROP_HWDEC_CURRENT: u64 = 15;
const PROP_SPEED: u64 = 16;
const PROP_VOLUME: u64 = 17;
const PROP_MUTE: u64 = 18;
const PROP_SEEKING: u64 = 19;

// Pending UI notification bitflags (coalesced into UI thread wakeup).
pub const UI_EVT_PROGRESS: u32 = 1 << 0;
pub const UI_EVT_FILE_LOADED: u32 = 1 << 1;
pub const UI_EVT_EOF: u32 = 1 << 2;
pub const UI_EVT_ERROR: u32 = 1 << 3;
pub const UI_EVT_TRACKS: u32 = 1 << 4;
pub const UI_EVT_VIDEO_SIZE: u32 = 1 << 5;
pub const UI_EVT_SEEK: u32 = 1 << 6;

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventEndFile {
    reason: c_int,
    error: c_int,
    playlist_entry_id: i64,
    playlist_insert_id: i64,
    playlist_insert_num_entries: c_int,
}

#[repr(C)]
struct MpvEventLogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: c_int,
}

#[repr(C)]
struct MpvRenderParam {
    param_type: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvD3d11InitParams {
    device: *mut c_void,
}

#[repr(C)]
struct MpvD3d11Fbo {
    tex: *mut c_void,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub lang: String,
    pub selected: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MpvSnapshot {
    pub time_pos: f64,
    pub display_time_pos: f64,
    pub seeking: bool,
    pub duration: f64,
    pub cache_duration: f64,
    pub source_fps: f64,
    pub estimated_fps: f64,
    pub speed: f64,
    pub volume: f64,
    pub video_w: u32,
    pub video_h: u32,
    pub paused: bool,
    pub buffering: bool,
    pub eof_reached: bool,
    pub core_idle: bool,
    pub muted: bool,
    pub file_loaded: bool,
    pub aid: i64,
    pub sid: i64,
}

const SEEK_HOLD_TIMEOUT_MS: u64 = 3500;

fn monotonic_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let base = EPOCH.get_or_init(Instant::now);
    base.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Coordinates optimistic seek targets against asynchronous `mpv` seeks so
/// short clicks (`<= 1.25s`) and slow `hr-seek=yes` / network seeks never
/// bounce the HUD progress bar back to pre-seek `time-pos`.
#[derive(Debug, Clone, Copy, Default)]
struct PendingSeekState {
    active: bool,
    origin_secs: f64,
    target_secs: f64,
    keyframe_mode: bool,
    started_ms: u64,
    saw_mpv_seek: bool,
    mpv_seeking: bool,
}

impl PendingSeekState {
    #[cfg_attr(not(test), allow(dead_code))]
    fn begin(&mut self, target_secs: f64, now_ms: u64) {
        self.begin_with_mode(f64::NAN, target_secs, false, now_ms);
    }

    fn begin_with_mode(
        &mut self,
        origin_secs: f64,
        target_secs: f64,
        keyframe_mode: bool,
        now_ms: u64,
    ) {
        self.active = true;
        self.origin_secs = origin_secs;
        self.target_secs = if target_secs.is_finite() {
            target_secs.max(0.0)
        } else {
            0.0
        };
        self.keyframe_mode = keyframe_mode;
        self.started_ms = now_ms;
        self.saw_mpv_seek = false;
    }

    fn cancel(&mut self) {
        self.active = false;
        self.saw_mpv_seek = false;
        self.mpv_seeking = false;
    }

    fn on_mpv_seek_event(&mut self) {
        self.mpv_seeking = true;
        if self.active {
            self.saw_mpv_seek = true;
        }
    }

    fn on_mpv_seeking_prop(&mut self, seeking: bool, current_pos: f64, now_ms: u64) {
        self.mpv_seeking = seeking;
        if seeking {
            if self.active {
                self.saw_mpv_seek = true;
            }
        } else {
            self.try_settle(current_pos, false, now_ms);
        }
    }

    fn on_playback_restart(&mut self, fresh_pos: f64, now_ms: u64) {
        self.mpv_seeking = false;
        self.try_settle(fresh_pos, true, now_ms);
    }

    fn on_time_pos(&mut self, pos: f64, now_ms: u64) {
        if self.active && self.saw_mpv_seek && !self.mpv_seeking {
            self.try_settle(pos, false, now_ms);
        }
    }

    fn try_settle(&mut self, pos: f64, is_restart: bool, now_ms: u64) {
        if !self.active {
            return;
        }
        let elapsed = now_ms.saturating_sub(self.started_ms);
        let near_target = pos.is_finite() && (pos - self.target_secs).abs() <= 1.75;
        let keyframe_moved = self.keyframe_mode
            && pos.is_finite()
            && (!self.origin_secs.is_finite()
                || (self.target_secs - self.origin_secs).abs() <= 1.0
                || (pos - self.target_secs).abs() + 0.5
                    < (self.origin_secs - self.target_secs).abs());
        if is_restart {
            if elapsed >= 350 || (self.saw_mpv_seek && (near_target || keyframe_moved)) {
                self.active = false;
            }
        } else if self.saw_mpv_seek
            && !self.mpv_seeking
            && (near_target || keyframe_moved || elapsed >= 600)
        {
            self.active = false;
        }
    }

    fn effective(&mut self, raw_pos: f64, now_ms: u64) -> (f64, bool) {
        if self.active && now_ms.saturating_sub(self.started_ms) >= SEEK_HOLD_TIMEOUT_MS {
            self.active = false;
        }
        if self.active {
            (self.target_secs, true)
        } else {
            (raw_pos, self.mpv_seeking)
        }
    }
}

/// Lock-free state shared cleanly across the mpv event thread, the D3D11
/// render thread, and the WebView2 UI thread.
///
/// The render thread NEVER acquires locks or calls `mpv_get_property`; it reads
/// only atomics updated by `ttv-mpv-events`.
pub struct MpvLiveState {
    time_pos_bits: AtomicU64,
    duration_bits: AtomicU64,
    cache_duration_bits: AtomicU64,
    source_fps_bits: AtomicU64,
    estimated_fps_bits: AtomicU64,
    speed_bits: AtomicU64,
    volume_bits: AtomicU64,
    volume_lock_ms: AtomicU64,
    video_w: AtomicU32,
    video_h: AtomicU32,
    paused: AtomicBool,
    buffering: AtomicBool,
    eof_reached: AtomicBool,
    core_idle: AtomicBool,
    muted: AtomicBool,
    file_loaded: AtomicBool,
    network_stream: AtomicBool,
    aid: AtomicI64,
    sid: AtomicI64,
    pub frame_dirty: AtomicBool,
    pub stopping: AtomicBool,
    pending_ui_events: AtomicU32,
    render_wakeup_event: isize,
    ui_hwnd: AtomicI64,
    pending_seek: Mutex<PendingSeekState>,
    deferred_seek_secs: Mutex<Option<f64>>,
    pub hwdec_current: Mutex<String>,
    pub last_error: Mutex<Option<String>>,
    pub last_failure_code: std::sync::atomic::AtomicU8,
    pub tracks: Mutex<Vec<TrackInfo>>,
}

pub const FAILURE_NONE: u8 = 0;
pub const FAILURE_NETWORK: u8 = 1;
pub const FAILURE_DECODE: u8 = 2;
pub const FAILURE_SOURCE: u8 = 3;

pub fn classify_mpv_error(msg: &str) -> u8 {
    let low = msg.to_ascii_lowercase();
    if low.contains("decode")
        || low.contains("codec")
        || low.contains("hwdec")
        || low.contains("d3d11")
        || low.contains("ffmpeg")
    {
        FAILURE_DECODE
    } else if low.contains("unrecognized")
        || low.contains("demux")
        || low.contains("no video")
        || low.contains("no audio")
        || low.contains("unsupported")
    {
        FAILURE_SOURCE
    } else {
        FAILURE_NETWORK
    }
}

impl MpvLiveState {
    pub fn new(render_wakeup_event: HANDLE, ui_hwnd: isize) -> Arc<Self> {
        Arc::new(Self {
            time_pos_bits: AtomicU64::new(0.0f64.to_bits()),
            duration_bits: AtomicU64::new(0.0f64.to_bits()),
            cache_duration_bits: AtomicU64::new(0.0f64.to_bits()),
            source_fps_bits: AtomicU64::new(0.0f64.to_bits()),
            estimated_fps_bits: AtomicU64::new(0.0f64.to_bits()),
            speed_bits: AtomicU64::new(1.0f64.to_bits()),
            volume_bits: AtomicU64::new(100.0f64.to_bits()),
            volume_lock_ms: AtomicU64::new(0),
            video_w: AtomicU32::new(0),
            video_h: AtomicU32::new(0),
            paused: AtomicBool::new(false),
            buffering: AtomicBool::new(false),
            eof_reached: AtomicBool::new(false),
            core_idle: AtomicBool::new(true),
            muted: AtomicBool::new(false),
            file_loaded: AtomicBool::new(false),
            network_stream: AtomicBool::new(false),
            aid: AtomicI64::new(0),
            sid: AtomicI64::new(0),
            frame_dirty: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            pending_ui_events: AtomicU32::new(0),
            render_wakeup_event: render_wakeup_event.0 as isize,
            ui_hwnd: AtomicI64::new(ui_hwnd as i64),
            pending_seek: Mutex::new(PendingSeekState::default()),
            deferred_seek_secs: Mutex::new(None),
            hwdec_current: Mutex::new(String::new()),
            last_error: Mutex::new(None),
            last_failure_code: std::sync::atomic::AtomicU8::new(FAILURE_NONE),
            tracks: Mutex::new(Vec::new()),
        })
    }

    #[cfg(test)]
    pub fn new_headless() -> Arc<Self> {
        Self::new(HANDLE(ptr::null_mut()), 0)
    }

    pub fn time_pos(&self) -> f64 {
        f64::from_bits(self.time_pos_bits.load(Ordering::Acquire))
    }

    pub fn duration(&self) -> f64 {
        f64::from_bits(self.duration_bits.load(Ordering::Acquire))
    }

    pub fn cache_duration(&self) -> f64 {
        f64::from_bits(self.cache_duration_bits.load(Ordering::Acquire))
    }

    pub fn source_fps(&self) -> f64 {
        let f = f64::from_bits(self.source_fps_bits.load(Ordering::Acquire));
        if f.is_finite() && f > 1.0 {
            f
        } else {
            let est = f64::from_bits(self.estimated_fps_bits.load(Ordering::Acquire));
            if est.is_finite() && est > 1.0 {
                est
            } else {
                25.0
            }
        }
    }

    pub fn speed(&self) -> f64 {
        f64::from_bits(self.speed_bits.load(Ordering::Acquire))
    }

    pub fn volume(&self) -> f64 {
        f64::from_bits(self.volume_bits.load(Ordering::Acquire))
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering.load(Ordering::Acquire)
    }

    pub fn is_file_loaded(&self) -> bool {
        self.file_loaded.load(Ordering::Acquire)
    }

    pub fn is_network_stream(&self) -> bool {
        self.network_stream.load(Ordering::Acquire)
    }

    /// Returns `true` when `target_secs` lies inside mpv's currently cached/demuxed window
    /// (or when playing a local file), where `exact` seek completes quickly without remote range hops.
    pub fn is_seek_in_buffered_range(&self, target_secs: f64) -> bool {
        if !self.is_network_stream() || !target_secs.is_finite() {
            return true;
        }
        let cur = self.time_pos();
        let cache_dur = self.cache_duration();
        if !cur.is_finite() || !cache_dur.is_finite() {
            return false;
        }
        if cache_dur <= 0.25 {
            return (target_secs - cur).abs() <= 3.0;
        }
        let low = (cur - 12.0).max(0.0);
        let high = cur + (cache_dur - 0.35).max(0.0);
        target_secs >= low && target_secs <= high
    }

    pub fn video_size(&self) -> (u32, u32) {
        (
            self.video_w.load(Ordering::Acquire),
            self.video_h.load(Ordering::Acquire),
        )
    }

    /// Resets or seeds expected source dimensions for a newly requested stream (`0,0` clears stale dims).
    pub fn prepare_for_load_dims(&self, w: u32, h: u32) {
        self.video_w.store(w, Ordering::Release);
        self.video_h.store(h, Ordering::Release);
    }

    pub fn set_expected_duration(&self, total_secs: f64) {
        if total_secs.is_finite() && total_secs > 0.0 && self.duration() <= 0.1 {
            self.duration_bits
                .store(total_secs.to_bits(), Ordering::Release);
        }
    }

    pub fn take_ui_events(&self) -> u32 {
        self.pending_ui_events.swap(0, Ordering::AcqRel)
    }

    fn push_ui_event(&self, mask: u32) {
        self.pending_ui_events.fetch_or(mask, Ordering::Release);
        let raw_hwnd = self.ui_hwnd.load(Ordering::Acquire);
        if raw_hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(windows::Win32::Foundation::HWND(raw_hwnd as *mut _)),
                    WM_APP_MPV_EVENT,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
    }

    pub fn begin_seek_with_mode(
        &self,
        origin_secs: f64,
        target_secs: f64,
        keyframe_mode: bool,
    ) {
        if let Ok(mut g) = self.pending_seek.lock() {
            g.begin_with_mode(origin_secs, target_secs, keyframe_mode, monotonic_ms());
        }
    }

    pub fn cancel_pending_seek(&self) {
        if let Ok(mut g) = self.pending_seek.lock() {
            g.cancel();
        }
    }

    pub fn set_deferred_seek(&self, secs: f64) {
        if let Ok(mut g) = self.deferred_seek_secs.lock() {
            *g = Some(secs);
        }
    }

    pub fn take_deferred_seek(&self) -> Option<f64> {
        self.deferred_seek_secs.lock().ok().and_then(|mut g| g.take())
    }

    pub fn clear_deferred_seek(&self) {
        if let Ok(mut g) = self.deferred_seek_secs.lock() {
            *g = None;
        }
    }

    pub fn effective_time_pos(&self) -> (f64, bool) {
        let raw = self.time_pos();
        match self.pending_seek.lock() {
            Ok(mut g) => g.effective(raw, monotonic_ms()),
            Err(_) => (raw, false),
        }
    }

    pub fn snapshot(&self) -> MpvSnapshot {
        let raw_pos = self.time_pos();
        let (display_time_pos, seeking) = self.effective_time_pos();
        MpvSnapshot {
            time_pos: raw_pos,
            display_time_pos,
            seeking,
            duration: self.duration(),
            cache_duration: self.cache_duration(),
            source_fps: self.source_fps(),
            estimated_fps: f64::from_bits(self.estimated_fps_bits.load(Ordering::Acquire)),
            speed: self.speed(),
            volume: self.volume(),
            video_w: self.video_w.load(Ordering::Acquire),
            video_h: self.video_h.load(Ordering::Acquire),
            paused: self.paused.load(Ordering::Acquire),
            buffering: self.buffering.load(Ordering::Acquire),
            eof_reached: self.eof_reached.load(Ordering::Acquire),
            core_idle: self.core_idle.load(Ordering::Acquire),
            muted: self.muted.load(Ordering::Acquire),
            file_loaded: self.file_loaded.load(Ordering::Acquire),
            aid: self.aid.load(Ordering::Acquire),
            sid: self.sid.load(Ordering::Acquire),
        }
    }

    pub fn hwdec_label(&self) -> String {
        self.hwdec_current
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn tracks_snapshot(&self) -> Vec<TrackInfo> {
        self.tracks.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn last_error_text(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    pub fn failure_label(&self) -> &'static str {
        match self.last_failure_code.load(Ordering::Acquire) {
            FAILURE_NETWORK => "network",
            FAILURE_DECODE => "decode",
            FAILURE_SOURCE => "source",
            _ => "none",
        }
    }

    pub fn notify_frame_presented(&self) {
        self.push_ui_event(UI_EVT_PROGRESS);
    }
}

type FnMpvCreate = unsafe extern "C" fn() -> *mut c_void;
type FnMpvInitialize = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnMpvDestroy = unsafe extern "C" fn(*mut c_void);
type FnMpvWakeup = unsafe extern "C" fn(*mut c_void);
type FnMpvErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type FnMpvFree = unsafe extern "C" fn(*mut c_void);
type FnMpvCommand = unsafe extern "C" fn(*mut c_void, *mut *const c_char) -> c_int;
type FnMpvCommandAsync = unsafe extern "C" fn(*mut c_void, u64, *mut *const c_char) -> c_int;
type FnMpvSetPropertyString =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnMpvSetProperty =
    unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
type FnMpvGetPropertyString = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type FnMpvObserveProperty =
    unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int) -> c_int;
type FnMpvWaitEvent = unsafe extern "C" fn(*mut c_void, c_double) -> *const MpvEvent;
type FnMpvRequestLogMessages = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type FnMpvRenderContextCreate =
    unsafe extern "C" fn(*mut *mut c_void, *mut c_void, *const MpvRenderParam) -> c_int;
type FnMpvRenderContextSetUpdateCallback =
    unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void), *mut c_void);
type FnMpvRenderContextUpdate = unsafe extern "C" fn(*mut c_void) -> u64;
type FnMpvRenderContextRender = unsafe extern "C" fn(*mut c_void, *const MpvRenderParam) -> c_int;
type FnMpvRenderContextReportSwap = unsafe extern "C" fn(*mut c_void);
type FnMpvRenderContextFree = unsafe extern "C" fn(*mut c_void);

struct MpvApi {
    module: HMODULE,
    mpv_create: FnMpvCreate,
    mpv_initialize: FnMpvInitialize,
    mpv_destroy: FnMpvDestroy,
    mpv_wakeup: FnMpvWakeup,
    mpv_error_string: FnMpvErrorString,
    mpv_free: FnMpvFree,
    #[allow(dead_code)]
    mpv_command: FnMpvCommand,
    mpv_command_async: FnMpvCommandAsync,
    mpv_set_property_string: FnMpvSetPropertyString,
    mpv_set_property: FnMpvSetProperty,
    mpv_get_property_string: FnMpvGetPropertyString,
    mpv_observe_property: FnMpvObserveProperty,
    mpv_wait_event: FnMpvWaitEvent,
    mpv_request_log_messages: FnMpvRequestLogMessages,
    mpv_render_context_create: FnMpvRenderContextCreate,
    mpv_render_context_set_update_callback: FnMpvRenderContextSetUpdateCallback,
    mpv_render_context_update: FnMpvRenderContextUpdate,
    mpv_render_context_render: FnMpvRenderContextRender,
    mpv_render_context_report_swap: FnMpvRenderContextReportSwap,
    mpv_render_context_free: FnMpvRenderContextFree,
}

unsafe impl Send for MpvApi {}
unsafe impl Sync for MpvApi {}

impl MpvApi {
    fn load(dll_path: &Path) -> Result<Self, String> {
        let wide: Vec<u16> = dll_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let module = unsafe {
            LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH)
        }
        .map_err(|e| format!("LoadLibraryExW({}) failed: {e}", dll_path.display()))?;

        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let cname = concat!($name, "\0");
                let proc = unsafe { GetProcAddress(module, PCSTR(cname.as_ptr())) };
                let Some(f) = proc else {
                    unsafe {
                        let _ = FreeLibrary(module);
                    }
                    return Err(format!("libmpv missing export: {}", $name));
                };
                unsafe { std::mem::transmute_copy::<_, $ty>(&f) }
            }};
        }

        Ok(Self {
            module,
            mpv_create: sym!("mpv_create", FnMpvCreate),
            mpv_initialize: sym!("mpv_initialize", FnMpvInitialize),
            mpv_destroy: sym!("mpv_destroy", FnMpvDestroy),
            mpv_wakeup: sym!("mpv_wakeup", FnMpvWakeup),
            mpv_error_string: sym!("mpv_error_string", FnMpvErrorString),
            mpv_free: sym!("mpv_free", FnMpvFree),
            mpv_command: sym!("mpv_command", FnMpvCommand),
            mpv_command_async: sym!("mpv_command_async", FnMpvCommandAsync),
            mpv_set_property_string: sym!("mpv_set_property_string", FnMpvSetPropertyString),
            mpv_set_property: sym!("mpv_set_property", FnMpvSetProperty),
            mpv_get_property_string: sym!("mpv_get_property_string", FnMpvGetPropertyString),
            mpv_observe_property: sym!("mpv_observe_property", FnMpvObserveProperty),
            mpv_wait_event: sym!("mpv_wait_event", FnMpvWaitEvent),
            mpv_request_log_messages: sym!("mpv_request_log_messages", FnMpvRequestLogMessages),
            mpv_render_context_create: sym!(
                "mpv_render_context_create",
                FnMpvRenderContextCreate
            ),
            mpv_render_context_set_update_callback: sym!(
                "mpv_render_context_set_update_callback",
                FnMpvRenderContextSetUpdateCallback
            ),
            mpv_render_context_update: sym!(
                "mpv_render_context_update",
                FnMpvRenderContextUpdate
            ),
            mpv_render_context_render: sym!(
                "mpv_render_context_render",
                FnMpvRenderContextRender
            ),
            mpv_render_context_report_swap: sym!(
                "mpv_render_context_report_swap",
                FnMpvRenderContextReportSwap
            ),
            mpv_render_context_free: sym!("mpv_render_context_free", FnMpvRenderContextFree),
        })
    }

    fn err_str(&self, code: c_int) -> String {
        unsafe {
            let p = (self.mpv_error_string)(code);
            if p.is_null() {
                format!("mpv error {code}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

impl Drop for MpvApi {
    fn drop(&mut self) {
        if !self.module.is_invalid() {
            unsafe {
                let _ = FreeLibrary(self.module);
            }
        }
    }
}

pub fn find_mpv_dll() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("TTV_MPV_DLL") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("libmpv-2.dll"));
            cands.push(dir.join("resources").join("ttv-player").join("libmpv-2.dll"));
            cands.push(dir.join("resources").join("mpv").join("libmpv-2.dll"));
            cands.push(dir.join("resources").join("native-player").join("libmpv-2.dll"));
            if let Some(parent) = dir.parent() {
                cands.push(parent.join("libmpv-2.dll"));
                cands.push(parent.join("ttv-player").join("libmpv-2.dll"));
                cands.push(parent.join("mpv").join("libmpv-2.dll"));
                cands.push(parent.join("resources").join("ttv-player").join("libmpv-2.dll"));
                cands.push(parent.join("resources").join("mpv").join("libmpv-2.dll"));
                cands.push(parent.join("resources").join("native-player").join("libmpv-2.dll"));
                if let Some(grandparent) = parent.parent() {
                    cands.push(grandparent.join("native-player").join("libmpv-2.dll"));
                    cands.push(
                        grandparent
                            .join("resources")
                            .join("ttv-player")
                            .join("libmpv-2.dll"),
                    );
                }
            }
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    cands.push(manifest_dir.join("libmpv-2.dll"));
    if let Some(src_tauri) = manifest_dir.parent() {
        cands.push(
            src_tauri
                .join("resources")
                .join("ttv-player")
                .join("libmpv-2.dll"),
        );
        cands.push(src_tauri.join("resources").join("mpv").join("libmpv-2.dll"));
        cands.push(src_tauri.join("native-player").join("libmpv-2.dll"));
        cands.push(
            src_tauri
                .join("target")
                .join("release")
                .join("resources")
                .join("ttv-player")
                .join("libmpv-2.dll"),
        );
        cands.push(
            src_tauri
                .join("target")
                .join("release")
                .join("resources")
                .join("native-player")
                .join("libmpv-2.dll"),
        );
        cands.push(
            src_tauri
                .join("target")
                .join("debug")
                .join("resources")
                .join("ttv-player")
                .join("libmpv-2.dll"),
        );
        cands.push(
            src_tauri
                .join("target")
                .join("debug")
                .join("resources")
                .join("native-player")
                .join("libmpv-2.dll"),
        );
    }
    cands.into_iter().find(|p| p.is_file())
}

#[derive(Clone, Copy)]
struct SendMpvHandle(*mut c_void);
unsafe impl Send for SendMpvHandle {}

pub struct MpvEngine {
    api: Arc<MpvApi>,
    ctx: *mut c_void,
    render_ctx: *mut c_void,
    pub state: Arc<MpvLiveState>,
    render_wakeup_event: HANDLE,
    event_thread: Option<JoinHandle<()>>,
    state_raw_for_cb: *mut MpvLiveState,
}

unsafe impl Send for MpvEngine {}
unsafe impl Sync for MpvEngine {}

impl MpvEngine {
    pub fn new(d3d_device: Option<&ID3D11Device>, ui_hwnd: isize) -> Result<Self, String> {
        let dll_path = find_mpv_dll().ok_or_else(|| {
            "找不到 libmpv-2.dll（请检查程序目录或 src-tauri/native-player/libmpv-2.dll）"
                .to_string()
        })?;
        let api = Arc::new(MpvApi::load(&dll_path)?);
        let ctx = unsafe { (api.mpv_create)() };
        if ctx.is_null() {
            return Err("mpv_create returned null".into());
        }

        // Core options: zero CPU vf=minterpolate (we interpolate on GPU), D3D11 zero-copy hwdec.
        set_opt(&api, ctx, "vo", "libmpv");
        set_opt(&api, ctx, "gpu-api", "d3d11");
        set_opt(&api, ctx, "hwdec", "d3d11va,auto-safe");
        set_opt(&api, ctx, "hwdec-codecs", "all");
        set_opt(&api, ctx, "keep-open", "yes");
        set_opt(&api, ctx, "idle", "yes");
        set_opt(&api, ctx, "hr-seek", "yes");
        set_opt(&api, ctx, "hr-seek-framedrop", "yes");
        set_opt(&api, ctx, "cache", "yes");
        set_opt(&api, ctx, "demuxer-max-bytes", "64MiB");
        set_opt(&api, ctx, "demuxer-max-back-bytes", "16MiB");
        set_opt(&api, ctx, "demuxer-readahead-secs", "30");
        set_opt(&api, ctx, "cache-secs", "30");
        set_opt(&api, ctx, "cache-pause", "yes");
        set_opt(&api, ctx, "cache-pause-wait", "0.15");
        set_opt(&api, ctx, "network-timeout", "10");
        set_opt(&api, ctx, "audio-display", "no");
        set_opt(&api, ctx, "sub-auto", "fuzzy");
        set_opt(&api, ctx, "osd-level", "0");
        set_opt(&api, ctx, "video-timing-offset", "0");
        set_opt(&api, ctx, "volume-max", "130");

        let init_rc = unsafe { (api.mpv_initialize)(ctx) };
        if init_rc < 0 {
            let msg = api.err_str(init_rc);
            unsafe { (api.mpv_destroy)(ctx) };
            return Err(format!("mpv_initialize failed: {msg}"));
        }

        let _ = unsafe {
            let lvl = c"warn".as_ptr();
            (api.mpv_request_log_messages)(ctx, lvl)
        };

        let render_wakeup_event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| format!("CreateEventW for mpv render wakeup failed: {e}"))?;
        let state = MpvLiveState::new(render_wakeup_event, ui_hwnd);

        // Observe all runtime properties so the render thread NEVER calls mpv_get_property.
        observe(&api, ctx, PROP_TIME_POS, "time-pos", MPV_FORMAT_DOUBLE);
        observe(&api, ctx, PROP_DURATION, "duration", MPV_FORMAT_DOUBLE);
        observe(
            &api,
            ctx,
            PROP_CACHE_DURATION,
            "demuxer-cache-duration",
            MPV_FORMAT_DOUBLE,
        );
        observe(&api, ctx, PROP_PAUSE, "pause", MPV_FORMAT_FLAG);
        observe(
            &api,
            ctx,
            PROP_PAUSED_FOR_CACHE,
            "paused-for-cache",
            MPV_FORMAT_FLAG,
        );
        observe(&api, ctx, PROP_EOF_REACHED, "eof-reached", MPV_FORMAT_FLAG);
        observe(&api, ctx, PROP_CORE_IDLE, "core-idle", MPV_FORMAT_FLAG);
        observe(
            &api,
            ctx,
            PROP_CONTAINER_FPS,
            "container-fps",
            MPV_FORMAT_DOUBLE,
        );
        observe(
            &api,
            ctx,
            PROP_ESTIMATED_VF_FPS,
            "estimated-vf-fps",
            MPV_FORMAT_DOUBLE,
        );
        observe(&api, ctx, PROP_VIDEO_W, "video-params/w", MPV_FORMAT_INT64);
        observe(&api, ctx, PROP_VIDEO_H, "video-params/h", MPV_FORMAT_INT64);
        observe(&api, ctx, PROP_AID, "aid", MPV_FORMAT_INT64);
        observe(&api, ctx, PROP_SID, "sid", MPV_FORMAT_INT64);
        observe(&api, ctx, PROP_TRACK_LIST, "track-list", MPV_FORMAT_STRING);
        observe(
            &api,
            ctx,
            PROP_HWDEC_CURRENT,
            "hwdec-current",
            MPV_FORMAT_STRING,
        );
        observe(&api, ctx, PROP_SPEED, "speed", MPV_FORMAT_DOUBLE);
        observe(&api, ctx, PROP_VOLUME, "volume", MPV_FORMAT_DOUBLE);
        observe(&api, ctx, PROP_MUTE, "mute", MPV_FORMAT_FLAG);
        observe(&api, ctx, PROP_SEEKING, "seeking", MPV_FORMAT_FLAG);

        let mut render_ctx: *mut c_void = ptr::null_mut();
        let state_raw_for_cb = Arc::into_raw(state.clone()) as *mut MpvLiveState;
        if let Some(dev) = d3d_device {
            let api_type = c"d3d11";
            let mut init_params = MpvD3d11InitParams {
                device: dev.as_raw(),
            };
            let params = [
                MpvRenderParam {
                    param_type: MPV_RENDER_PARAM_API_TYPE,
                    data: api_type.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    param_type: MPV_RENDER_PARAM_D3D11_INIT_PARAMS,
                    data: (&mut init_params as *mut MpvD3d11InitParams) as *mut c_void,
                },
                MpvRenderParam {
                    param_type: MPV_RENDER_PARAM_INVALID,
                    data: ptr::null_mut(),
                },
            ];
            let rc = unsafe {
                (api.mpv_render_context_create)(&mut render_ctx, ctx, params.as_ptr())
            };
            if rc < 0 {
                let msg = api.err_str(rc);
                unsafe {
                    drop(Arc::from_raw(state_raw_for_cb));
                    (api.mpv_destroy)(ctx);
                    let _ = CloseHandle(render_wakeup_event);
                }
                return Err(format!("mpv_render_context_create(d3d11) failed: {msg}"));
            }
            unsafe {
                (api.mpv_render_context_set_update_callback)(
                    render_ctx,
                    mpv_render_wakeup_cb,
                    state_raw_for_cb as *mut c_void,
                );
            }
        }

        let api_evt = api.clone();
        let state_evt = state.clone();
        let ctx_send = SendMpvHandle(ctx);
        let event_thread = thread::Builder::new()
            .name("ttv-mpv-events".into())
            .spawn(move || {
                let h = ctx_send;
                run_mpv_event_loop(&api_evt, h.0, &state_evt);
            })
            .map_err(|e| format!("spawn mpv event thread failed: {e}"))?;

        Ok(Self {
            api,
            ctx,
            render_ctx,
            state,
            render_wakeup_event,
            event_thread: Some(event_thread),
            state_raw_for_cb,
        })
    }

    pub fn render_wakeup_event(&self) -> HANDLE {
        self.render_wakeup_event
    }

    /// Called exclusively on the D3D11 render thread.
    ///
    /// Never acquires mutexes or calls `mpv_get_property`.
    /// Returns `true` when mpv produced a genuinely new decoded frame into `target`.
    pub fn render_if_updated(&self, target: &ID3D11Texture2D) -> bool {
        if self.render_ctx.is_null() {
            return false;
        }
        let dirty = self.state.frame_dirty.swap(false, Ordering::AcqRel);
        let flags = unsafe { (self.api.mpv_render_context_update)(self.render_ctx) };
        if !dirty && (flags & MPV_RENDER_UPDATE_FRAME) == 0 {
            return false;
        }
        if (flags & MPV_RENDER_UPDATE_FRAME) == 0 {
            return false;
        }
        self.render_fbo(target)
    }

    /// Redraws the current frame into `target` unconditionally (used after resize
    /// while paused so aspect ratio updates cleanly without waiting for playback).
    pub fn render_current_frame(&self, target: &ID3D11Texture2D) -> bool {
        if self.render_ctx.is_null() {
            return false;
        }
        unsafe {
            let _ = (self.api.mpv_render_context_update)(self.render_ctx);
        }
        self.render_fbo(target)
    }

    fn render_fbo(&self, target: &ID3D11Texture2D) -> bool {
        let mut fbo = MpvD3d11Fbo {
            tex: target.as_raw(),
        };
        let mut block: c_int = 0;
        let params = [
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_D3D11_FBO,
                data: (&mut fbo as *mut MpvD3d11Fbo) as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&mut block as *mut c_int) as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let rc = unsafe { (self.api.mpv_render_context_render)(self.render_ctx, params.as_ptr()) };
        if rc >= 0 {
            unsafe {
                (self.api.mpv_render_context_report_swap)(self.render_ctx);
            }
            true
        } else {
            false
        }
    }

    pub fn load_file(
        &self,
        url: &str,
        key: &str,
        start_secs: f64,
        headers: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        // Must set per-file lavf decryption option + headers + start BEFORE loadfile.
        // Passing options as 4th loadfile arg returns -4 on this libmpv build.
        let net_stream = is_http_source(url);
        self.state.network_stream.store(net_stream, Ordering::Release);
        let lavf_opt = build_demuxer_lavf_options(url, key);
        set_opt(&self.api, self.ctx, "demuxer-lavf-o", &lavf_opt);

        if let Some(ua) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.as_str())
        {
            set_opt(&self.api, self.ctx, "user-agent", ua);
        }
        if let Some(ref_val) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("referer") || k.eq_ignore_ascii_case("referrer"))
            .map(|(_, v)| v.as_str())
        {
            set_opt(&self.api, self.ctx, "referrer", ref_val);
        } else {
            set_opt(&self.api, self.ctx, "referrer", "");
        }
        let extra_headers: Vec<String> = headers
            .iter()
            .filter(|(k, _)| {
                !k.eq_ignore_ascii_case("user-agent")
                    && !k.eq_ignore_ascii_case("referer")
                    && !k.eq_ignore_ascii_case("referrer")
            })
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        if !extra_headers.is_empty() {
            set_opt(
                &self.api,
                self.ctx,
                "http-header-fields",
                &extra_headers.join(","),
            );
        } else {
            set_opt(&self.api, self.ctx, "http-header-fields", "");
        }

        let start_str = if start_secs.is_finite() && start_secs > 0.05 {
            format!("{start_secs:.3}")
        } else {
            "0".to_string()
        };
        set_opt(&self.api, self.ctx, "start", &start_str);

        self.state.eof_reached.store(false, Ordering::Release);
        self.state.file_loaded.store(false, Ordering::Release);
        self.state
            .last_failure_code
            .store(FAILURE_NONE, Ordering::Release);
        self.state.cancel_pending_seek();
        self.state.clear_deferred_seek();
        self.state
            .time_pos_bits
            .store(start_secs.max(0.0).to_bits(), Ordering::Release);
        if let Ok(mut err) = self.state.last_error.lock() {
            *err = None;
        }
        self.command_async(&["loadfile", url, "replace"])?;
        // Ensure playback resumes when loading a new episode after EOF/pause.
        self.set_paused(false);
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        self.state.paused.store(paused, Ordering::Release);
        self.set_flag("pause", paused);
    }

    pub fn toggle_pause(&self) -> bool {
        let next = !self.state.is_paused();
        self.set_paused(next);
        next
    }

    pub fn seek_absolute(&self, secs: f64) {
        let dur = self.state.duration();
        let clamped = clamp_seek_target(secs, dur);
        self.state.eof_reached.store(false, Ordering::Release);
        self.state
            .last_failure_code
            .store(FAILURE_NONE, Ordering::Release);
        if !self.state.is_file_loaded() {
            self.state.set_deferred_seek(clamped);
            self.state
                .time_pos_bits
                .store(clamped.to_bits(), Ordering::Release);
            return;
        }
        self.state.clear_deferred_seek();
        let (origin, _) = self.state.effective_time_pos();
        let in_buffer = self.state.is_seek_in_buffered_range(clamped);
        self.state
            .begin_seek_with_mode(origin, clamped, !in_buffer);
        let s = format!("{clamped:.3}");
        let mode = if in_buffer { "exact" } else { "keyframes" };
        let _ = self.command_async(&["seek", &s, "absolute", mode]);
    }

    pub fn seek_relative(&self, delta: f64) {
        if !delta.is_finite() {
            return;
        }
        let (origin, _) = self.state.effective_time_pos();
        let dur = self.state.duration();
        let target = clamp_seek_target(origin + delta, dur);
        self.state.eof_reached.store(false, Ordering::Release);
        self.state
            .last_failure_code
            .store(FAILURE_NONE, Ordering::Release);
        if !self.state.is_file_loaded() {
            self.state.set_deferred_seek(target);
            self.state
                .time_pos_bits
                .store(target.to_bits(), Ordering::Release);
            return;
        }
        self.state.clear_deferred_seek();
        let in_buffer = self.state.is_seek_in_buffered_range(target);
        self.state
            .begin_seek_with_mode(origin, target, !in_buffer);
        let mode = if in_buffer { "exact" } else { "keyframes" };
        if dur.is_finite() && dur > 0.0 {
            let s = format!("{target:.3}");
            let _ = self.command_async(&["seek", &s, "absolute", mode]);
        } else {
            let s = format!("{delta:.3}");
            let _ = self.command_async(&["seek", &s, "relative", mode]);
        }
    }

    pub fn frame_step(&self, dir: i32) {
        let cmd = if dir < 0 {
            "frame-back-step"
        } else {
            "frame-step"
        };
        let _ = self.command_async(&[cmd]);
    }

    pub fn set_volume(&self, vol: f64) {
        let clamped = if vol.is_finite() {
            vol.clamp(0.0, 130.0)
        } else {
            100.0
        };
        self.state
            .volume_bits
            .store(clamped.to_bits(), Ordering::Release);
        self.state
            .volume_lock_ms
            .store(monotonic_ms().max(1), Ordering::Release);
        self.set_double("volume", clamped);
    }

    pub fn set_muted(&self, muted: bool) {
        self.state.muted.store(muted, Ordering::Release);
        self.set_flag("mute", muted);
    }

    pub fn set_speed(&self, speed: f64) {
        let clamped = if speed.is_finite() {
            speed.clamp(0.25, 4.0)
        } else {
            1.0
        };
        self.state
            .speed_bits
            .store(clamped.to_bits(), Ordering::Release);
        self.set_double("speed", clamped);
    }

    pub fn set_aspect(&self, mode: AspectMode) {
        let (panscan, keepaspect) = mode.mpv_params();
        self.set_double("panscan", panscan);
        set_opt(&self.api, self.ctx, "keepaspect", keepaspect);
    }

    pub fn set_picture(&self, brightness: i32, contrast: i32, saturation: i32, gamma: i32) {
        self.set_double("brightness", f64::from(brightness.clamp(-100, 100)));
        self.set_double("contrast", f64::from(contrast.clamp(-100, 100)));
        self.set_double("saturation", f64::from(saturation.clamp(-100, 100)));
        self.set_double("gamma", f64::from(gamma.clamp(-100, 100)));
    }

    pub fn set_audio_track(&self, aid: i64) {
        let s = if aid <= 0 {
            "auto".to_string()
        } else {
            aid.to_string()
        };
        set_opt(&self.api, self.ctx, "aid", &s);
    }

    pub fn set_sub_track(&self, sid: i64) {
        let s = if sid <= 0 {
            "no".to_string()
        } else {
            sid.to_string()
        };
        set_opt(&self.api, self.ctx, "sid", &s);
    }

    pub fn set_flag(&self, name: &str, v: bool) {
        let Ok(cname) = CString::new(name) else {
            return;
        };
        let mut f: c_int = if v { 1 } else { 0 };
        unsafe {
            let _ = (self.api.mpv_set_property)(
                self.ctx,
                cname.as_ptr(),
                MPV_FORMAT_FLAG,
                (&mut f as *mut c_int) as *mut c_void,
            );
        }
    }

    pub fn set_double(&self, name: &str, v: f64) {
        let Ok(cname) = CString::new(name) else {
            return;
        };
        let mut val: c_double = v;
        unsafe {
            let _ = (self.api.mpv_set_property)(
                self.ctx,
                cname.as_ptr(),
                MPV_FORMAT_DOUBLE,
                (&mut val as *mut c_double) as *mut c_void,
            );
        }
    }

    pub fn command_async(&self, args: &[&str]) -> Result<(), String> {
        let cstrings: Vec<CString> = args
            .iter()
            .filter_map(|s| CString::new(*s).ok())
            .collect();
        let mut ptrs: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        let rc = unsafe { (self.api.mpv_command_async)(self.ctx, 0, ptrs.as_mut_ptr()) };
        if rc < 0 {
            Err(self.api.err_str(rc))
        } else {
            Ok(())
        }
    }

    #[allow(dead_code)]
    pub fn command_sync(&self, args: &[&str]) -> Result<(), String> {
        let cstrings: Vec<CString> = args
            .iter()
            .filter_map(|s| CString::new(*s).ok())
            .collect();
        let mut ptrs: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        let rc = unsafe { (self.api.mpv_command)(self.ctx, ptrs.as_mut_ptr()) };
        if rc < 0 {
            Err(self.api.err_str(rc))
        } else {
            Ok(())
        }
    }
}

impl Drop for MpvEngine {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        unsafe {
            if !self.render_ctx.is_null() {
                (self.api.mpv_render_context_set_update_callback)(
                    self.render_ctx,
                    noop_update_cb,
                    ptr::null_mut(),
                );
            }
            (self.api.mpv_wakeup)(self.ctx);
        }
        if let Some(jh) = self.event_thread.take() {
            let _ = jh.join();
        }
        unsafe {
            if !self.render_ctx.is_null() {
                (self.api.mpv_render_context_free)(self.render_ctx);
                self.render_ctx = ptr::null_mut();
            }
            if !self.ctx.is_null() {
                (self.api.mpv_destroy)(self.ctx);
                self.ctx = ptr::null_mut();
            }
            if !self.state_raw_for_cb.is_null() {
                drop(Arc::from_raw(self.state_raw_for_cb));
                self.state_raw_for_cb = ptr::null_mut();
            }
            if !self.render_wakeup_event.is_invalid() {
                let _ = CloseHandle(self.render_wakeup_event);
            }
        }
    }
}

unsafe extern "C" fn noop_update_cb(_cb_ctx: *mut c_void) {}

unsafe extern "C" fn mpv_render_wakeup_cb(cb_ctx: *mut c_void) {
    if cb_ctx.is_null() {
        return;
    }
    let state = &*(cb_ctx as *const MpvLiveState);
    if !state.stopping.load(Ordering::Acquire) {
        state.frame_dirty.store(true, Ordering::Release);
        if state.render_wakeup_event != 0 {
            let _ = SetEvent(HANDLE(state.render_wakeup_event as *mut _));
        }
    }
}

fn is_http_source(url: &str) -> bool {
    let trimmed = url.trim_start();
    trimmed.starts_with("http://") || trimmed.starts_with("https://")
}

pub(crate) fn clamp_seek_target(secs: f64, dur: f64) -> f64 {
    let mut clamped = if secs.is_finite() { secs.max(0.0) } else { 0.0 };
    if dur.is_finite() && dur > 0.0 {
        let upper = if dur > 0.6 { (dur - 0.3).max(0.0) } else { dur };
        clamped = clamped.min(upper);
    }
    clamped
}

fn build_demuxer_lavf_options(url: &str, key: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if is_http_source(url) {
        parts.extend([
            "http_persistent=1".to_string(),
            "multiple_requests=1".to_string(),
            "reconnect=1".to_string(),
            "reconnect_streamed=1".to_string(),
            "reconnect_on_network_error=1".to_string(),
            "reconnect_delay_max=2".to_string(),
        ]);
    }
    let key = key.trim();
    if !key.is_empty() {
        parts.push(format!("decryption_key={key}"));
    }
    parts.join(",")
}

fn set_opt(api: &MpvApi, ctx: *mut c_void, name: &str, val: &str) {
    let (Ok(cn), Ok(cv)) = (CString::new(name), CString::new(val)) else {
        return;
    };
    unsafe {
        let _ = (api.mpv_set_property_string)(ctx, cn.as_ptr(), cv.as_ptr());
    }
}

fn observe(api: &MpvApi, ctx: *mut c_void, id: u64, name: &str, format: c_int) {
    let Ok(cn) = CString::new(name) else {
        return;
    };
    unsafe {
        let _ = (api.mpv_observe_property)(ctx, id, cn.as_ptr(), format);
    }
}

fn run_mpv_event_loop(api: &MpvApi, ctx: *mut c_void, state: &MpvLiveState) {
    while !state.stopping.load(Ordering::Acquire) {
        let ev_ptr = unsafe { (api.mpv_wait_event)(ctx, 0.25) };
        if ev_ptr.is_null() {
            continue;
        }
        let ev = unsafe { &*ev_ptr };
        match ev.event_id {
            MPV_EVENT_NONE => continue,
            MPV_EVENT_SHUTDOWN => break,
            MPV_EVENT_LOG_MESSAGE => {
                if !ev.data.is_null() {
                    let msg = unsafe { &*(ev.data as *const MpvEventLogMessage) };
                    if msg.log_level <= 20 && !msg.text.is_null() {
                        let text = unsafe { CStr::from_ptr(msg.text) }.to_string_lossy();
                        let trimmed = text.trim();
                        if !trimmed.is_empty() && trimmed.contains("error") {
                            if let Ok(mut g) = state.last_error.lock() {
                                *g = Some(trimmed.to_string());
                            }
                        }
                    }
                }
            }
            MPV_EVENT_START_FILE => {
                state.eof_reached.store(false, Ordering::Release);
                state.file_loaded.store(false, Ordering::Release);
                state
                    .last_failure_code
                    .store(FAILURE_NONE, Ordering::Release);
                state.cancel_pending_seek();
            }
            MPV_EVENT_FILE_LOADED => {
                state.file_loaded.store(true, Ordering::Release);
                state
                    .last_failure_code
                    .store(FAILURE_NONE, Ordering::Release);
                refresh_track_list(api, ctx, state);
                state.push_ui_event(UI_EVT_FILE_LOADED | UI_EVT_TRACKS | UI_EVT_PROGRESS);
            }
            MPV_EVENT_VIDEO_RECONFIG => {
                state.push_ui_event(UI_EVT_VIDEO_SIZE);
            }
            MPV_EVENT_SEEK => {
                if let Ok(mut g) = state.pending_seek.lock() {
                    g.on_mpv_seek_event();
                }
                state.push_ui_event(UI_EVT_SEEK | UI_EVT_PROGRESS);
            }
            MPV_EVENT_PLAYBACK_RESTART => {
                if let Some(v) = read_mpv_double_prop(api, ctx, c"time-pos") {
                    if v.is_finite() && v >= 0.0 {
                        state.time_pos_bits.store(v.to_bits(), Ordering::Release);
                    }
                }
                let fresh = state.time_pos();
                if let Ok(mut g) = state.pending_seek.lock() {
                    g.on_playback_restart(fresh, monotonic_ms());
                }
                state.push_ui_event(UI_EVT_PROGRESS);
            }
            MPV_EVENT_END_FILE => {
                state.cancel_pending_seek();
                if !ev.data.is_null() {
                    let ef = unsafe { &*(ev.data as *const MpvEventEndFile) };
                    if ef.reason == MPV_END_FILE_REASON_EOF {
                        state.eof_reached.store(true, Ordering::Release);
                        state.push_ui_event(UI_EVT_EOF | UI_EVT_PROGRESS);
                    } else if ef.reason == MPV_END_FILE_REASON_ERROR {
                        let err_msg = api.err_str(ef.error);
                        let code = classify_mpv_error(&err_msg);
                        state.last_failure_code.store(code, Ordering::Release);
                        if let Ok(mut g) = state.last_error.lock() {
                            *g = Some(err_msg);
                        }
                        state.push_ui_event(UI_EVT_ERROR | UI_EVT_PROGRESS);
                    }
                }
            }
            MPV_EVENT_PROPERTY_CHANGE => {
                if ev.data.is_null() {
                    continue;
                }
                let prop = unsafe { &*(ev.data as *const MpvEventProperty) };
                handle_property_change(api, ctx, state, ev.reply_userdata, prop);
            }
            _ => {}
        }
    }
}

fn handle_property_change(
    api: &MpvApi,
    ctx: *mut c_void,
    state: &MpvLiveState,
    id: u64,
    prop: &MpvEventProperty,
) {
    match (id, prop.format) {
        (PROP_TIME_POS, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v >= 0.0 {
                state.time_pos_bits.store(v.to_bits(), Ordering::Release);
                if let Ok(mut g) = state.pending_seek.lock() {
                    g.on_time_pos(v, monotonic_ms());
                }
                state.push_ui_event(UI_EVT_PROGRESS);
            }
        }
        (PROP_DURATION, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v > 0.0 {
                state.duration_bits.store(v.to_bits(), Ordering::Release);
                state.push_ui_event(UI_EVT_PROGRESS);
            }
        }
        (PROP_CACHE_DURATION, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v >= 0.0 {
                state
                    .cache_duration_bits
                    .store(v.to_bits(), Ordering::Release);
                state.push_ui_event(UI_EVT_PROGRESS);
            }
        }
        (PROP_PAUSE, MPV_FORMAT_FLAG) if !prop.data.is_null() => {
            let flag = unsafe { *(prop.data as *const c_int) } != 0;
            state.paused.store(flag, Ordering::Release);
            state.push_ui_event(UI_EVT_PROGRESS);
        }
        (PROP_PAUSED_FOR_CACHE, MPV_FORMAT_FLAG) if !prop.data.is_null() => {
            let flag = unsafe { *(prop.data as *const c_int) } != 0;
            state.buffering.store(flag, Ordering::Release);
            state.push_ui_event(UI_EVT_PROGRESS);
        }
        (PROP_EOF_REACHED, MPV_FORMAT_FLAG) if !prop.data.is_null() => {
            let flag = unsafe { *(prop.data as *const c_int) } != 0;
            let prev = state.eof_reached.swap(flag, Ordering::AcqRel);
            if flag && !prev {
                state.push_ui_event(UI_EVT_EOF | UI_EVT_PROGRESS);
            } else {
                state.push_ui_event(UI_EVT_PROGRESS);
            }
        }
        (PROP_CORE_IDLE, MPV_FORMAT_FLAG) if !prop.data.is_null() => {
            let flag = unsafe { *(prop.data as *const c_int) } != 0;
            state.core_idle.store(flag, Ordering::Release);
        }
        (PROP_CONTAINER_FPS, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v > 0.0 {
                state.source_fps_bits.store(v.to_bits(), Ordering::Release);
            }
        }
        (PROP_ESTIMATED_VF_FPS, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v > 0.0 {
                state
                    .estimated_fps_bits
                    .store(v.to_bits(), Ordering::Release);
            }
        }
        (PROP_VIDEO_W, MPV_FORMAT_INT64) if !prop.data.is_null() => {
            let w = unsafe { *(prop.data as *const i64) }.max(0) as u32;
            if w > 0 {
                state.video_w.store(w, Ordering::Release);
                state.push_ui_event(UI_EVT_VIDEO_SIZE);
            }
        }
        (PROP_VIDEO_H, MPV_FORMAT_INT64) if !prop.data.is_null() => {
            let h = unsafe { *(prop.data as *const i64) }.max(0) as u32;
            if h > 0 {
                state.video_h.store(h, Ordering::Release);
                state.push_ui_event(UI_EVT_VIDEO_SIZE);
            }
        }
        (PROP_AID, MPV_FORMAT_INT64) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const i64) };
            state.aid.store(v, Ordering::Release);
            refresh_track_list(api, ctx, state);
            state.push_ui_event(UI_EVT_TRACKS);
        }
        (PROP_SID, MPV_FORMAT_INT64) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const i64) };
            state.sid.store(v, Ordering::Release);
            refresh_track_list(api, ctx, state);
            state.push_ui_event(UI_EVT_TRACKS);
        }
        (PROP_TRACK_LIST, _) => {
            refresh_track_list(api, ctx, state);
            state.push_ui_event(UI_EVT_TRACKS);
        }
        (PROP_HWDEC_CURRENT, MPV_FORMAT_STRING) if !prop.data.is_null() => {
            let p = unsafe { *(prop.data as *const *const c_char) };
            if !p.is_null() {
                let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
                if let Ok(mut g) = state.hwdec_current.lock() {
                    *g = s;
                }
            }
        }
        (PROP_SPEED, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v > 0.0 {
                state.speed_bits.store(v.to_bits(), Ordering::Release);
            }
        }
        (PROP_VOLUME, MPV_FORMAT_DOUBLE) if !prop.data.is_null() => {
            let v = unsafe { *(prop.data as *const c_double) };
            if v.is_finite() && v >= 0.0 {
                let lock_ms = state.volume_lock_ms.load(Ordering::Acquire);
                let recent_lock =
                    lock_ms > 0 && monotonic_ms().saturating_sub(lock_ms) <= 300;
                if !recent_lock || (v - state.volume()).abs() < 0.01 {
                    state.volume_bits.store(v.to_bits(), Ordering::Release);
                }
            }
        }
        (PROP_MUTE, MPV_FORMAT_FLAG) if !prop.data.is_null() => {
            let flag = unsafe { *(prop.data as *const c_int) } != 0;
            state.muted.store(flag, Ordering::Release);
        }
        (PROP_SEEKING, MPV_FORMAT_FLAG) if !prop.data.is_null() => {
            let flag = unsafe { *(prop.data as *const c_int) } != 0;
            let cur_pos = state.time_pos();
            if let Ok(mut g) = state.pending_seek.lock() {
                g.on_mpv_seeking_prop(flag, cur_pos, monotonic_ms());
            }
            state.push_ui_event(UI_EVT_PROGRESS);
        }
        _ => {}
    }
}

fn read_mpv_double_prop(api: &MpvApi, ctx: *mut c_void, name: &CStr) -> Option<f64> {
    let raw_ptr = unsafe { (api.mpv_get_property_string)(ctx, name.as_ptr()) };
    if raw_ptr.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(raw_ptr) }.to_string_lossy().into_owned();
    unsafe {
        (api.mpv_free)(raw_ptr as *mut c_void);
    }
    text.trim().parse::<f64>().ok()
}

fn refresh_track_list(api: &MpvApi, ctx: *mut c_void, state: &MpvLiveState) {
    let raw_ptr = unsafe { (api.mpv_get_property_string)(ctx, c"track-list".as_ptr()) };
    if raw_ptr.is_null() {
        return;
    }
    let json_text = unsafe { CStr::from_ptr(raw_ptr) }.to_string_lossy().into_owned();
    unsafe {
        (api.mpv_free)(raw_ptr as *mut c_void);
    }
    let parsed = parse_mpv_track_list_json(&json_text);
    if !parsed.is_empty() {
        if let Ok(mut g) = state.tracks.lock() {
            *g = parsed;
        }
    }
}

pub fn parse_mpv_track_list_json(raw: &str) -> Vec<TrackInfo> {
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    arr.into_iter()
        .filter_map(|item| {
            let kind = item.get("type")?.as_str()?.to_string();
            if kind != "audio" && kind != "sub" {
                return None;
            }
            let id = item.get("id")?.as_i64()?;
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let lang = item
                .get("lang")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let selected = item
                .get("selected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(TrackInfo {
                id,
                kind,
                title,
                lang,
                selected,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_state_atomics_and_fallbacks() {
        let s = MpvLiveState::new_headless();
        assert_eq!(s.source_fps(), 25.0);
        s.estimated_fps_bits.store(29.97f64.to_bits(), Ordering::Release);
        assert!((s.source_fps() - 29.97).abs() < 1e-6);
        s.source_fps_bits.store(24.0f64.to_bits(), Ordering::Release);
        assert!((s.source_fps() - 24.0).abs() < 1e-6);

        s.set_expected_duration(125.0);
        assert_eq!(s.duration(), 125.0);

        s.prepare_for_load_dims(1080, 1920);
        assert_eq!(s.video_size(), (1080, 1920));
        s.prepare_for_load_dims(0, 0);
        assert_eq!(s.video_size(), (0, 0));
    }

    #[test]
    fn track_list_parser_extracts_audio_and_subtitles() {
        let sample = r#"[
            {"id":1,"type":"video","selected":true},
            {"id":1,"type":"audio","title":"国语","lang":"zh","selected":true},
            {"id":2,"type":"audio","title":"原声","lang":"en","selected":false},
            {"id":1,"type":"sub","title":"简体中文字幕","lang":"chs","selected":false}
        ]"#;
        let tracks = parse_mpv_track_list_json(sample);
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].kind, "audio");
        assert!(tracks[0].selected);
        assert_eq!(tracks[2].kind, "sub");
    }

    #[test]
    fn can_initialize_headless_libmpv_if_present() {
        if find_mpv_dll().is_none() {
            return;
        }
        let engine = MpvEngine::new(None, 0).expect("init headless MpvEngine");
        engine.set_aspect(AspectMode::Cover);
        engine.set_picture(5, 10, 12, 0);
        engine.set_volume(75.0);
        assert_eq!(engine.state.volume(), 75.0);
        let opt = build_demuxer_lavf_options("https://cdn.example/video.mp4", "0123456789abcdef0123456789abcdef");
        let name = std::ffi::CString::new("demuxer-lavf-o").unwrap();
        let val = std::ffi::CString::new(opt).unwrap();
        let rc = unsafe { (engine.api.mpv_set_property_string)(engine.ctx, name.as_ptr(), val.as_ptr()) };
        assert!(rc >= 0, "libmpv rejected demuxer-lavf-o: {rc}");
    }

    #[test]
    fn pending_seek_prevents_short_and_slow_snapback() {
        let mut p = PendingSeekState::default();

        // Short click (< 1.25s ahead of current playback): pre-seek ticks must NOT bounce back.
        p.begin(5.8, 100);
        p.on_time_pos(5.05, 110);
        assert_eq!(p.effective(5.05, 110), (5.8, true));

        // MPV_EVENT_SEEK arrives while mpv still reports 5.05: hold stays at 5.8.
        p.on_mpv_seek_event();
        p.on_time_pos(5.05, 250);
        assert_eq!(p.effective(5.05, 250), (5.8, true));

        // Slow network/exact seek (> 450ms old cutoff) still holds target cleanly.
        assert_eq!(p.effective(5.05, 950), (5.8, true));

        // Playback restarts at settled PTS: clears hold and exposes real position.
        p.on_playback_restart(5.82, 980);
        assert_eq!(p.effective(5.82, 980), (5.82, false));
    }

    #[test]
    fn pending_seek_handles_rapid_clicks_and_timeout() {
        let mut p = PendingSeekState::default();

        // User clicks 10s, then rapidly clicks 16s 40ms later before first restart finishes.
        p.begin(10.0, 1000);
        p.on_mpv_seek_event();
        p.begin(16.0, 1040);

        // Old seek's restart for 10s arrives before new seek starts: must NOT jump back to 10s.
        p.on_playback_restart(10.05, 1060);
        assert_eq!(p.effective(10.05, 1060), (16.0, true));

        // Second seek starts and restarts at 16.02s: settles cleanly.
        p.on_mpv_seek_event();
        p.on_playback_restart(16.02, 1180);
        assert_eq!(p.effective(16.02, 1180), (16.02, false));

        // Safety timeout recovers automatically even without restart.
        p.begin(19.5, 2000);
        assert_eq!(p.effective(3.0, 2000 + SEEK_HOLD_TIMEOUT_MS), (3.0, false));
    }

    #[test]
    fn classify_mpv_error_buckets() {
        assert_eq!(classify_mpv_error("HTTP error 403 Forbidden"), FAILURE_NETWORK);
        assert_eq!(classify_mpv_error("Failed to open segment: timeout"), FAILURE_NETWORK);
        assert_eq!(classify_mpv_error("d3d11va video decoder failed"), FAILURE_DECODE);
        assert_eq!(classify_mpv_error("Failed to recognize file format / demux"), FAILURE_SOURCE);
    }

    #[test]
    fn buffered_range_selects_keyframes_only_for_unbuffered_remote_seeks() {
        let s = MpvLiveState::new_headless();
        s.time_pos_bits.store(20.0f64.to_bits(), Ordering::Release);
        s.cache_duration_bits.store(15.0f64.to_bits(), Ordering::Release);

        // Local file always keeps exact seeking regardless of cache_duration.
        s.network_stream.store(false, Ordering::Release);
        assert!(s.is_seek_in_buffered_range(120.0));

        // Network stream uses exact inside [cur - 12, cur + cache_dur - 0.35], keyframes outside.
        s.network_stream.store(true, Ordering::Release);
        assert!(s.is_seek_in_buffered_range(12.0));
        assert!(s.is_seek_in_buffered_range(34.0));
        assert!(!s.is_seek_in_buffered_range(36.0));
        assert!(!s.is_seek_in_buffered_range(2.0));

        // Short click with tiny cache still uses exact; far jump with empty cache uses keyframes.
        s.cache_duration_bits.store(0.0f64.to_bits(), Ordering::Release);
        assert!(s.is_seek_in_buffered_range(21.2));
        assert!(!s.is_seek_in_buffered_range(60.0));
    }

    #[test]
    fn pending_seek_keyframe_mode_settles_fast_without_timeout() {
        let mut p = PendingSeekState::default();
        p.begin_with_mode(10.0, 85.0, true, 100);
        // Pre-seek tick still at origin must not bounce back.
        p.on_time_pos(10.0, 120);
        assert_eq!(p.effective(10.0, 120), (85.0, true));

        // Fast restart lands on nearest keyframe at 81.8s (> 1.75s away from 85.0, elapsed < 350ms).
        p.on_mpv_seek_event();
        p.on_playback_restart(81.8, 210);
        assert_eq!(p.effective(81.8, 210), (81.8, false));
    }

    #[test]
    fn build_demuxer_lavf_options_combines_http_keepalive_and_key() {
        let net = build_demuxer_lavf_options("https://cdn.example/a.mp4", "abcd");
        assert!(net.contains("http_persistent=1"));
        assert!(net.contains("reconnect_streamed=1"));
        assert!(net.contains("reconnect_on_network_error=1"));
        assert!(net.contains("decryption_key=abcd"));

        let local = build_demuxer_lavf_options("C:\\video\\a.mp4", "abcd");
        assert_eq!(local, "decryption_key=abcd");
    }

    #[test]
    fn clamp_seek_target_stays_safely_below_eof_and_validates_inputs() {
        assert_eq!(clamp_seek_target(f64::NAN, 120.0), 0.0);
        assert_eq!(clamp_seek_target(-5.0, 120.0), 0.0);
        assert!((clamp_seek_target(120.0, 120.0) - 119.7).abs() < 1e-6);
        assert!((clamp_seek_target(150.0, 120.0) - 119.7).abs() < 1e-6);
        assert!((clamp_seek_target(42.5, 120.0) - 42.5).abs() < 1e-6);
        assert!((clamp_seek_target(0.4, 0.5) - 0.4).abs() < 1e-6);

        let s = MpvLiveState::new_headless();
        s.set_deferred_seek(48.25);
        assert_eq!(s.take_deferred_seek(), Some(48.25));
        assert_eq!(s.take_deferred_seek(), None);
    }
}
