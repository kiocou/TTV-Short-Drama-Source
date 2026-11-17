use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchArgs {
    pub pipe_name: Option<String>,
    pub load_file: Option<String>,
    pub fg_override: Option<bool>,
    pub fg_mult_override: Option<u32>,
    pub fg_mode_override: Option<String>,
    pub kind_override: Option<String>,
    pub theme_override: Option<String>,
    pub autoplay_override: Option<bool>,
    pub autoplay_delay_override: Option<u32>,
    pub vsr_override: Option<bool>,
    pub selftest: bool,
    pub direct_url: Option<String>,
}

impl LaunchArgs {
    pub fn parse<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let items: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        let mut out = Self::default();
        let mut i = 1usize; // skip exe name
        while i < items.len() {
            let arg = items[i].trim();
            let next = items.get(i + 1).map(|s| s.as_str());
            match arg {
                "--selftest" => out.selftest = true,
                "--pipe" => {
                    if let Some(v) = next {
                        out.pipe_name = Some(v.to_string());
                        i += 1;
                    }
                }
                "--load-file" => {
                    if let Some(v) = next {
                        out.load_file = Some(v.to_string());
                        i += 1;
                    }
                }
                "--fg" => {
                    if let Some(v) = next {
                        out.fg_override = parse_bool_flag(v);
                        i += 1;
                    }
                }
                "--fgmult" => {
                    if let Some(v) = next {
                        if let Ok(n) = v.trim().parse::<u32>() {
                            out.fg_mult_override = Some(if n == 0 { 0 } else { n.clamp(2, 5) });
                        }
                        i += 1;
                    }
                }
                "--fgmode" => {
                    if let Some(v) = next {
                        out.fg_mode_override = Some(v.trim().to_ascii_lowercase());
                        i += 1;
                    }
                }
                "--kind" => {
                    if let Some(v) = next {
                        out.kind_override = Some(v.trim().to_ascii_lowercase());
                        i += 1;
                    }
                }
                "--theme" | "--style" => {
                    if let Some(v) = next {
                        let t = match v.trim().to_ascii_lowercase().as_str() {
                            "light" | "white" => "light",
                            _ => "dark",
                        };
                        out.theme_override = Some(t.to_string());
                        i += 1;
                    }
                }
                "--autoplay" => {
                    if let Some(v) = next {
                        out.autoplay_override = parse_bool_flag(v);
                        i += 1;
                    }
                }
                "--autoplay-delay" => {
                    if let Some(v) = next {
                        if let Ok(n) = v.trim().parse::<f64>() {
                            if n.is_finite() {
                                out.autoplay_delay_override =
                                    Some((n.round() as i64).clamp(0, 15) as u32);
                            }
                        }
                        i += 1;
                    }
                }
                "--vsr" => {
                    if let Some(v) = next {
                        out.vsr_override = parse_bool_flag(v);
                        i += 1;
                    }
                }
                other if !other.starts_with("--") && out.direct_url.is_none() => {
                    out.direct_url = Some(other.to_string());
                }
                _ => {}
            }
            i += 1;
        }
        out
    }
}

fn parse_bool_flag(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn de_f64_lossy<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(value_as_f64(&v).unwrap_or(0.0))
}

fn de_opt_f64_lossy<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|x| value_as_f64(&x)))
}

fn de_u32_lossy<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(value_as_f64(&v)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.round() as u32)
        .unwrap_or(0))
}

fn de_opt_u32_lossy<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|x| value_as_f64(&x))
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.round() as u32))
}

fn de_opt_usize_lossy<'de, D: Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|x| value_as_f64(&x))
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.round() as usize))
}

pub fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

pub fn value_as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|x| x != 0.0),
        Value::String(s) => parse_bool_flag(s),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemRaw {
    #[serde(default, deserialize_with = "de_opt_usize_lossy")]
    pub index: Option<usize>,
    #[serde(default)]
    pub url: String,
    #[serde(alias = "content_key", default)]
    pub key: String,
    #[serde(default, deserialize_with = "de_u32_lossy")]
    pub ep: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub vid: String,
    #[serde(alias = "series_id", default)]
    pub series_id: String,
    #[serde(alias = "episode_id", default)]
    pub episode_id: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "de_f64_lossy")]
    pub duration: f64,
    #[serde(default, deserialize_with = "de_f64_lossy")]
    pub start: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct QueueObjectRaw {
    #[serde(default)]
    pub items: Vec<QueueItemRaw>,
    #[serde(default, deserialize_with = "de_opt_usize_lossy")]
    pub index: Option<usize>,
    #[serde(default)]
    pub autoplay: Option<bool>,
    #[serde(alias = "total_eps", default, deserialize_with = "de_opt_u32_lossy")]
    pub total_eps: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum QueueSpec {
    Object(QueueObjectRaw),
    List(Vec<QueueItemRaw>),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct LoadRequest {
    pub url: String,
    pub key: String,
    #[serde(deserialize_with = "de_f64_lossy")]
    pub start: f64,
    pub title: String,
    #[serde(deserialize_with = "de_u32_lossy")]
    pub ep: u32,
    #[serde(deserialize_with = "de_f64_lossy")]
    pub total: f64,
    #[serde(deserialize_with = "de_u32_lossy")]
    pub vw: u32,
    #[serde(deserialize_with = "de_u32_lossy")]
    pub vh: u32,
    #[serde(default, deserialize_with = "de_opt_f64_lossy")]
    pub volume: Option<f64>,
    pub kind: String,
    #[serde(alias = "series_id")]
    pub series_id: String,
    #[serde(alias = "episode_id")]
    pub episode_id: String,
    pub vid: String,
    #[serde(alias = "fg_mode")]
    pub fg_mode: String,
    #[serde(alias = "total_eps", default, deserialize_with = "de_opt_u32_lossy")]
    pub total_eps: Option<u32>,
    #[serde(default, deserialize_with = "de_opt_usize_lossy")]
    pub index: Option<usize>,
    pub autoplay: Option<bool>,
    #[serde(alias = "autoplay_delay", default, deserialize_with = "de_opt_f64_lossy")]
    pub autoplay_delay: Option<f64>,
    pub headers: BTreeMap<String, String>,
    pub queue: Option<QueueSpec>,
    // Synced settings optionally bundled inside open() load JSON:
    #[serde(alias = "danmaku_enabled")]
    pub danmaku_enabled: Option<bool>,
    #[serde(alias = "danmakuArea", default, deserialize_with = "de_opt_f64_lossy")]
    pub danmaku_area: Option<f64>,
    #[serde(alias = "danmakuOpacity", default, deserialize_with = "de_opt_f64_lossy")]
    pub danmaku_opacity: Option<f64>,
    #[serde(alias = "danmakuFontScale", default, deserialize_with = "de_opt_f64_lossy")]
    pub danmaku_font_scale: Option<f64>,
    #[serde(alias = "danmakuSpeed", default, deserialize_with = "de_opt_f64_lossy")]
    pub danmaku_speed: Option<f64>,
    #[serde(alias = "danmakuFont")]
    pub danmaku_font: Option<String>,
    #[serde(alias = "danmakuColor")]
    pub danmaku_color: Option<String>,
    #[serde(alias = "danmakuBold")]
    pub danmaku_bold: Option<bool>,
    #[serde(alias = "danmakuStroke")]
    pub danmaku_stroke: Option<String>,
    #[serde(alias = "vsrEnabled")]
    pub vsr_enabled: Option<bool>,
}

impl LoadRequest {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim().trim_start_matches('\u{feff}');
        serde_json::from_str::<Self>(trimmed).map_err(|e| format!("parse LoadRequest failed: {e}"))
    }

    pub fn resolved_danmaku_enabled(&self) -> Option<bool> {
        self.danmaku_enabled
    }
}

#[derive(Debug, Clone)]
pub enum PipeCommand {
    Load(Box<LoadRequest>),
    Queue(Value),
    QueuePush(Value),
    QueueClear,
    Autoplay(Value),
    DanmakuData(Value),
    SettingsSync(Value),
    DanmakuToggle(Value),
    Vsr(Value),
    Pip(Value),
    Pause(Value),
    Play,
    Seek(f64),
    Volume(f64),
    Mute(Option<bool>),
    Speed(f64),
    Fg(Value),
    FgMode(String),
    Track(Value),
    Shot,
    Theme(String),
    FrameStep(i32),
    Loop(Value),
    Aspect(String),
    Stats(Option<bool>),
    Fullscreen(Option<bool>),
    Quit,
}

impl PipeCommand {
    pub fn parse_line(line: &str) -> Option<Self> {
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(trimmed).ok()?;
        let cmd = v.get("cmd").and_then(Value::as_str)?.trim();
        match cmd {
            "load" => serde_json::from_value::<LoadRequest>(v)
                .ok()
                .map(|r| Self::Load(Box::new(r))),
            "queue" => Some(Self::Queue(v)),
            "queuePush" => Some(Self::QueuePush(v)),
            "queueClear" => Some(Self::QueueClear),
            "autoplay" => Some(Self::Autoplay(v)),
            "danmakuData" => Some(Self::DanmakuData(v)),
            "settingsSync" => Some(Self::SettingsSync(v)),
            "danmaku" => Some(Self::DanmakuToggle(v)),
            "vsr" => Some(Self::Vsr(v)),
            "pip" => Some(Self::Pip(v)),
            "pause" => Some(Self::Pause(v)),
            "play" => Some(Self::Play),
            "seek" => v
                .get("pos")
                .or_else(|| v.get("position"))
                .or_else(|| v.get("time"))
                .or_else(|| v.get("seconds"))
                .and_then(value_as_f64)
                .map(Self::Seek),
            "volume" => v
                .get("v")
                .or_else(|| v.get("value"))
                .or_else(|| v.get("volume"))
                .and_then(value_as_f64)
                .map(Self::Volume),
            "mute" => Some(Self::Mute(
                v.get("muted")
                    .or_else(|| v.get("on"))
                    .or_else(|| v.get("value"))
                    .and_then(value_as_bool),
            )),
            "speed" => v
                .get("value")
                .or_else(|| v.get("speed"))
                .or_else(|| v.get("rate"))
                .and_then(value_as_f64)
                .map(Self::Speed),
            "fg" => Some(Self::Fg(v)),
            "fgmode" => Some(Self::FgMode(
                v.get("mode")
                    .or_else(|| v.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or("auto")
                    .to_string(),
            )),
            "track" => Some(Self::Track(v)),
            "shot" => Some(Self::Shot),
            "style" | "theme" => Some(Self::Theme(
                v.get("theme")
                    .or_else(|| v.get("mode"))
                    .or_else(|| v.get("style"))
                    .or_else(|| v.get("id"))
                    .or_else(|| v.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or("dark")
                    .to_string(),
            )),
            "frame" => {
                let dir = match v.get("dir").or_else(|| v.get("step")) {
                    Some(Value::String(s)) => {
                        let low = s.trim().to_ascii_lowercase();
                        if matches!(low.as_str(), "back" | "prev" | "backward" | "-1") {
                            -1
                        } else {
                            1
                        }
                    }
                    Some(other) => value_as_f64(other)
                        .map(|n| if n < 0.0 { -1 } else { 1 })
                        .unwrap_or(1),
                    None => 1,
                };
                Some(Self::FrameStep(dir))
            }
            "loop" => Some(Self::Loop(v)),
            "aspect" => Some(Self::Aspect(
                v.get("mode")
                    .or_else(|| v.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or("fit")
                    .to_string(),
            )),
            "stats" => Some(Self::Stats(
                v.get("on")
                    .or_else(|| v.get("enabled"))
                    .and_then(value_as_bool),
            )),
            "fullscreen" => Some(Self::Fullscreen(
                v.get("on")
                    .or_else(|| v.get("enabled"))
                    .and_then(value_as_bool),
            )),
            "quit" => Some(Self::Quit),
            _ => None,
        }
    }
}

#[derive(Clone, Default)]
pub struct PipeWriter {
    inner: Arc<(Mutex<PipeWriterState>, Condvar)>,
}

#[derive(Default)]
struct PipeWriterState {
    handle: Option<isize>,
    connected: bool,
    closed: bool,
    writing: bool,
    pending: VecDeque<String>,
}

impl PipeWriter {
    pub fn send(&self, payload: &Value) {
        let Ok(mut line) = serde_json::to_string(payload) else {
            return;
        };
        line.push('\n');
        let (lock, cvar) = &*self.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return;
        }
        if guard.pending.len() >= 256 {
            guard.pending.pop_front();
        }
        guard.pending.push_back(line);
        cvar.notify_all();
    }

    pub fn send_event(&self, event_name: &str, mut body: serde_json::Map<String, Value>) {
        body.insert("event".into(), json!(event_name));
        self.send(&Value::Object(body));
    }

    fn set_connected(&self, raw_handle: isize) {
        let (lock, cvar) = &*self.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.handle = Some(raw_handle);
        guard.connected = true;
        guard.closed = false;
        cvar.notify_all();
    }

    fn clear_handle(&self) {
        let (lock, cvar) = &*self.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.handle = None;
        guard.connected = false;
        guard.closed = true;
        cvar.notify_all();
    }

    pub fn flush(&self) {
        let (lock, cvar) = &*self.inner;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        while guard.connected && (!guard.pending.is_empty() || guard.writing) {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            let (next_guard, _) = cvar
                .wait_timeout(guard, deadline.saturating_duration_since(now))
                .unwrap_or_else(|e| e.into_inner());
            guard = next_guard;
        }
        let handle = if guard.connected { guard.handle } else { None };
        drop(guard);
        if let Some(raw) = handle {
            unsafe {
                let _ = FlushFileBuffers(HANDLE(raw as *mut _));
            }
        }
    }

    fn run_writer_loop(&self) {
        let (lock, cvar) = &*self.inner;
        loop {
            let (raw_handle, line) = {
                let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !guard.closed && (!guard.connected || guard.pending.is_empty()) {
                    guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
                }
                if guard.closed {
                    return;
                }
                let Some(raw) = guard.handle else {
                    guard.connected = false;
                    continue;
                };
                let Some(line) = guard.pending.pop_front() else {
                    continue;
                };
                guard.writing = true;
                (raw, line)
            };

            let ok = write_overlapped_bytes(HANDLE(raw_handle as *mut _), line.as_bytes());
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.writing = false;
            if !ok {
                guard.connected = false;
            }
            cvar.notify_all();
        }
    }
}

pub struct PipeServer {
    pub writer: PipeWriter,
    pub rx: Receiver<PipeCommand>,
}

impl PipeServer {
    pub fn start(pipe_name: &str, wakeup_hwnd: Option<isize>) -> Result<Self, String> {
        let full_name = if pipe_name.starts_with(r"\\.\pipe\") {
            pipe_name.to_string()
        } else {
            format!(r"\\.\pipe\{pipe_name}")
        };
        let wide: Vec<u16> = OsStr::new(&full_name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let h_pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                1024 * 64,
                1024 * 64,
                0,
                None,
            )
        };
        if h_pipe == INVALID_HANDLE_VALUE || h_pipe.is_invalid() {
            let err = unsafe { GetLastError() };
            return Err(format!("CreateNamedPipeW({full_name}) failed: {:?}", err));
        }

        let writer = PipeWriter::default();
        let (tx, rx) = mpsc::channel();
        let raw_handle = h_pipe.0 as isize;
        let writer_for_io = writer.clone();
        let writer_for_tx = writer.clone();

        thread::Builder::new()
            .name("ttv-pipe-writer".into())
            .spawn(move || {
                writer_for_tx.run_writer_loop();
            })
            .map_err(|e| format!("spawn pipe writer thread failed: {e}"))?;

        thread::Builder::new()
            .name("ttv-pipe-io".into())
            .spawn(move || {
                run_pipe_loop(raw_handle, writer_for_io, tx, wakeup_hwnd);
            })
            .map_err(|e| format!("spawn pipe thread failed: {e}"))?;

        Ok(Self { writer, rx })
    }
}

fn run_pipe_loop(
    raw_handle: isize,
    writer: PipeWriter,
    tx: Sender<PipeCommand>,
    wakeup_hwnd: Option<isize>,
) {
    let h_pipe = HANDLE(raw_handle as *mut _);
    let Ok(connect_evt) = (unsafe { CreateEventW(None, true, false, None) }) else {
        writer.clear_handle();
        unsafe {
            let _ = CloseHandle(h_pipe);
        }
        return;
    };

    let mut ov = OVERLAPPED {
        hEvent: connect_evt,
        ..Default::default()
    };
    let connected = unsafe { ConnectNamedPipe(h_pipe, Some(&mut ov)) };
    let is_ok = if connected.is_ok() {
        true
    } else {
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_CONNECTED {
            true
        } else if err == ERROR_IO_PENDING {
            let mut transferred = 0u32;
            unsafe {
                if WaitForSingleObject(connect_evt, 30_000) == WAIT_OBJECT_0 {
                    GetOverlappedResult(h_pipe, &ov, &mut transferred, true).is_ok()
                } else {
                    let _ = CancelIoEx(h_pipe, Some(&ov));
                    let _ = GetOverlappedResult(h_pipe, &ov, &mut transferred, true);
                    false
                }
            }
        } else {
            false
        }
    };
    unsafe {
        let _ = CloseHandle(connect_evt);
    }

    if !is_ok {
        writer.clear_handle();
        unsafe {
            let _ = CloseHandle(h_pipe);
        }
        if let Some(hwnd) = wakeup_hwnd {
            notify_ui_pipe_closed(hwnd);
        }
        return;
    }

    writer.set_connected(raw_handle);
    if let Some(hwnd) = wakeup_hwnd {
        notify_ui_pipe_wakeup(hwnd);
    }

    let Ok(read_evt) = (unsafe { CreateEventW(None, true, false, None) }) else {
        writer.clear_handle();
        unsafe {
            let _ = CloseHandle(h_pipe);
        }
        return;
    };

    const MAX_PIPE_LINE_BYTES: usize = 256 * 1024;
    let mut line_buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];

    loop {
        let mut r_ov = OVERLAPPED {
            hEvent: read_evt,
            ..Default::default()
        };
        let mut read_bytes = 0u32;
        let res = unsafe {
            ReadFile(
                h_pipe,
                Some(&mut chunk),
                Some(&mut read_bytes),
                Some(&mut r_ov),
            )
        };
        if res.is_err() {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                unsafe {
                    if WaitForSingleObject(read_evt, u32::MAX) != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(h_pipe, Some(&r_ov));
                        let _ = GetOverlappedResult(h_pipe, &r_ov, &mut read_bytes, true);
                        break;
                    }
                    if GetOverlappedResult(h_pipe, &r_ov, &mut read_bytes, true).is_err() {
                        break;
                    }
                }
            } else if err == ERROR_MORE_DATA {
                let mut extra = 0u32;
                let _ = unsafe { GetOverlappedResult(h_pipe, &r_ov, &mut extra, true) };
                if extra > 0 {
                    read_bytes = extra;
                }
            } else {
                break;
            }
        }

        if read_bytes == 0 {
            break;
        }
        if line_buf.len() + (read_bytes as usize) > MAX_PIPE_LINE_BYTES {
            line_buf.clear();
            continue;
        }
        line_buf.extend_from_slice(&chunk[..read_bytes as usize]);

        while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
            let raw_line: Vec<u8> = line_buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw_line);
            if let Some(cmd) = PipeCommand::parse_line(&line) {
                let _ = tx.send(cmd);
                if let Some(hwnd) = wakeup_hwnd {
                    notify_ui_pipe_wakeup(hwnd);
                }
            }
        }
    }

    writer.clear_handle();
    unsafe {
        let _ = CloseHandle(read_evt);
        let _ = FlushFileBuffers(h_pipe);
        let _ = DisconnectNamedPipe(h_pipe);
        let _ = CloseHandle(h_pipe);
    }
    if let Some(hwnd) = wakeup_hwnd {
        notify_ui_pipe_closed(hwnd);
    }
}

pub const WM_APP_PIPE_CMD: u32 = 0x8000 + 0x101;
pub const WM_APP_MPV_EVENT: u32 = 0x8000 + 0x102;

pub fn notify_ui_pipe_wakeup(raw_hwnd: isize) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    if raw_hwnd != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(raw_hwnd as *mut _)),
                WM_APP_PIPE_CMD,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn notify_ui_pipe_closed(raw_hwnd: isize) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
    if raw_hwnd != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(raw_hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

fn write_overlapped_bytes(h_pipe: HANDLE, bytes: &[u8]) -> bool {
    let Ok(write_evt) = (unsafe { CreateEventW(None, true, false, None) }) else {
        return false;
    };
    let mut ov = OVERLAPPED {
        hEvent: write_evt,
        ..Default::default()
    };
    let mut written = 0u32;
    let res = unsafe { WriteFile(h_pipe, Some(bytes), Some(&mut written), Some(&mut ov)) };
    let ok = if res.is_ok() {
        true
    } else {
        let err = unsafe { GetLastError() };
        if err == ERROR_IO_PENDING {
            unsafe {
                if WaitForSingleObject(write_evt, 3000) != WAIT_OBJECT_0 {
                    // Cancel before returning so the kernel never writes into the popped stack OVERLAPPED.
                    let _ = CancelIoEx(h_pipe, Some(&ov));
                    let _ = GetOverlappedResult(h_pipe, &ov, &mut written, true);
                    false
                } else {
                    GetOverlappedResult(h_pipe, &ov, &mut written, true).is_ok()
                }
            }
        } else {
            false
        }
    };
    unsafe {
        let _ = CloseHandle(write_evt);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::time::Duration;

    #[test]
    fn cli_args_parse_correctly() {
        let parsed = LaunchArgs::parse([
            "ttv-player.exe",
            "--pipe",
            "ttv_pipe_1",
            "--load-file",
            "load.json",
            "--fg",
            "1",
            "--fgmult",
            "3",
            "--fgmode",
            "anime",
            "--kind",
            "comic",
            "--theme",
            "light",
            "--autoplay",
            "0",
            "--autoplay-delay",
            "7",
            "--vsr",
            "1",
        ]);
        assert_eq!(parsed.pipe_name.as_deref(), Some("ttv_pipe_1"));
        assert_eq!(parsed.load_file.as_deref(), Some("load.json"));
        assert_eq!(parsed.fg_override, Some(true));
        assert_eq!(parsed.fg_mult_override, Some(3));
        assert_eq!(parsed.fg_mode_override.as_deref(), Some("anime"));
        assert_eq!(parsed.kind_override.as_deref(), Some("comic"));
        assert_eq!(parsed.theme_override.as_deref(), Some("light"));
        assert_eq!(parsed.autoplay_override, Some(false));
        assert_eq!(parsed.autoplay_delay_override, Some(7));
        assert_eq!(parsed.vsr_override, Some(true));

        let parsed_vsr_off = LaunchArgs::parse(["ttv-player.exe", "--vsr", "0"]);
        assert_eq!(parsed_vsr_off.vsr_override, Some(false));

        let parsed_auto_fg = LaunchArgs::parse(["ttv-player.exe", "--fg", "1", "--fgmult", "0"]);
        assert_eq!(parsed_auto_fg.fg_override, Some(true));
        assert_eq!(parsed_auto_fg.fg_mult_override, Some(0));
    }

    #[test]
    fn load_request_tolerates_rust_floats_and_nested_queue() {
        let raw = r#"{
            "url": "https://cdn.example.com/v1.mp4",
            "key": "abc",
            "start": 12.5,
            "title": "第14集",
            "ep": 14.0,
            "total": 120.0,
            "vw": 1080.0,
            "vh": 1920.0,
            "volume": 65.0,
            "kind": "drama",
            "totalEps": 80.0,
            "index": 13.0,
            "autoplay": true,
            "danmakuEnabled": true,
            "danmakuArea": 0.25,
            "queue": {
                "index": 13.0,
                "autoplay": true,
                "items": [
                    {"index": 13.0, "ep": 14.0, "url": "https://cdn.example.com/v14.mp4", "title": "第14集"},
                    {"index": 14.0, "ep": 15.0, "url": "https://cdn.example.com/v15.mp4", "title": "第15集"}
                ]
            }
        }"#;
        let req = LoadRequest::from_json_str(raw).expect("parse LoadRequest");
        assert_eq!(req.ep, 14);
        assert_eq!(req.vw, 1080);
        assert_eq!(req.vh, 1920);
        assert_eq!(req.volume, Some(65.0));
        assert_eq!(req.total_eps, Some(80));
        assert_eq!(req.index, Some(13));
        assert_eq!(req.resolved_danmaku_enabled(), Some(true));
        match req.queue.expect("queue") {
            QueueSpec::Object(q) => {
                assert_eq!(q.index, Some(13));
                assert_eq!(q.items.len(), 2);
                assert_eq!(q.items[1].ep, 15);
            }
            _ => panic!("expected QueueSpec::Object"),
        }
    }

    #[test]
    fn overlapped_named_pipe_is_full_duplex() {
        let pipe_name = format!("ttv_test_duplex_{}", std::process::id());
        let server = PipeServer::start(&pipe_name, None).expect("start pipe server");

        let client_path = format!(r"\\.\pipe\{pipe_name}");
        let mut client_file: Option<File> = None;
        for _ in 0..40 {
            if let Ok(f) = OpenOptions::new().read(true).write(true).open(&client_path) {
                client_file = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        let mut client = client_file.expect("open client pipe");

        // Server writes event while client hasn't written anything yet (tests non-blocking duplex!).
        let mut map = serde_json::Map::new();
        map.insert("ep".into(), json!(1));
        server.writer.send_event("ready", map);

        let mut reader = BufReader::new(client.try_clone().expect("clone client"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read event from server");
        assert!(line.contains(r#""event":"ready""#));

        // Client sends command to server.
        writeln!(client, r#"{{"cmd":"seek","pos":42.5}}"#).expect("write cmd");
        client.flush().unwrap();

        let cmd = server
            .rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recv Seek");
        match cmd {
            PipeCommand::Seek(t) => assert!((t - 42.5).abs() < 1e-6),
            other => panic!("unexpected cmd: {other:?}"),
        }

        writeln!(client, r#"{{"cmd":"quit"}}"#).unwrap();
        let quit = server
            .rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recv Quit");
        assert!(matches!(quit, PipeCommand::Quit));
    }

    #[test]
    fn parses_host_compat_command_variants() {
        match PipeCommand::parse_line(r#"{"cmd":"volume","v":42}"#).expect("vol v") {
            PipeCommand::Volume(v) => assert!((v - 42.0).abs() < 1e-6),
            other => panic!("unexpected: {other:?}"),
        }
        match PipeCommand::parse_line(r#"{"cmd":"theme","mode":"light"}"#).expect("theme mode") {
            PipeCommand::Theme(t) => assert_eq!(t, "light"),
            other => panic!("unexpected: {other:?}"),
        }
        match PipeCommand::parse_line(r#"{"cmd":"style","id":"dark"}"#).expect("style id") {
            PipeCommand::Theme(t) => assert_eq!(t, "dark"),
            other => panic!("unexpected: {other:?}"),
        }
        match PipeCommand::parse_line(r#"{"cmd":"frame","dir":"back"}"#).expect("frame back") {
            PipeCommand::FrameStep(d) => assert_eq!(d, -1),
            other => panic!("unexpected: {other:?}"),
        }
        match PipeCommand::parse_line(r#"{"cmd":"frame","dir":"fwd"}"#).expect("frame fwd") {
            PipeCommand::FrameStep(d) => assert_eq!(d, 1),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
