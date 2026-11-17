use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Resolves the root writable directory for `ttv-player`.
///
/// Honors `TTV_NATIVE_DATA_DIR` so isolated headless runs and `--selftest`
/// never touch the user's real profile directory.
pub fn data_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("TTV_NATIVE_DATA_DIR") {
        let s = custom.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let mut path = PathBuf::from(local);
        path.push("TTV");
        return path;
    }
    std::env::temp_dir().join("TTV")
}

pub fn prefs_path() -> PathBuf {
    data_dir().join("native-player-prefs.json")
}

pub fn shader_cache_dir() -> PathBuf {
    data_dir().join("shader-cache")
}

pub fn webview2_user_data_dir() -> PathBuf {
    data_dir().join("ttv-player-webview2")
}

pub fn screenshots_dir() -> PathBuf {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let mut p = PathBuf::from(profile);
        p.push("Pictures");
        p.push("TTV Screenshots");
        return p;
    }
    data_dir().join("screenshots")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AspectMode {
    #[default]
    Fit,
    Cover,
    Stretch,
}

impl AspectMode {
    pub fn parse_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cover" | "crop" | "fill" => Self::Cover,
            "stretch" | "full" => Self::Stretch,
            _ => Self::Fit,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fit => "fit",
            Self::Cover => "cover",
            Self::Stretch => "stretch",
        }
    }

    pub fn mpv_params(self) -> (f64, &'static str) {
        match self {
            Self::Fit => (0.0, "yes"),
            Self::Cover => (1.0, "yes"),
            Self::Stretch => (0.0, "no"),
        }
    }

    pub fn to_u32(self) -> u32 {
        match self {
            Self::Fit => 0,
            Self::Cover => 1,
            Self::Stretch => 2,
        }
    }

    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Cover,
            2 => Self::Stretch,
            _ => Self::Fit,
        }
    }
}

/// High-level orientation class used for per-orientation window/PiP geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoOrientation {
    Landscape,
    Portrait,
}

impl VideoOrientation {
    /// Classifies stream orientation from source pixel dimensions (`9:16` -> `Portrait`, else `Landscape`).
    pub fn from_dims(w: u32, h: u32) -> Self {
        if w > 0 && h > w {
            Self::Portrait
        } else {
            Self::Landscape
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SavedWindowRect {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: u32,
    pub h: u32,
}

impl Default for SavedWindowRect {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            w: 1120,
            h: 630,
        }
    }
}

impl SavedWindowRect {
    pub fn sanitized(&self, min_w: u32, min_h: u32) -> Self {
        Self {
            x: self.x,
            y: self.y,
            w: self.w.clamp(min_w, 7680),
            h: self.h.clamp(min_h, 4320),
        }
    }
}

/// Computes source display aspect ratio, defaulting to `16:9` when dimensions are unknown.
pub fn video_aspect_ratio(vid_w: u32, vid_h: u32) -> f64 {
    if vid_w > 0 && vid_h > 0 {
        (f64::from(vid_w) / f64::from(vid_h)).clamp(0.25, 4.0)
    } else {
        16.0 / 9.0
    }
}

/// Returns a crisp human-readable label for common aspect ratios.
pub fn describe_video_aspect(vid_w: u32, vid_h: u32) -> &'static str {
    if vid_w == 0 || vid_h == 0 {
        return "默认比例";
    }
    let r = f64::from(vid_w) / f64::from(vid_h);
    if (r - (16.0 / 9.0)).abs() < 0.08 {
        "16:9 横屏"
    } else if (r - (9.0 / 16.0)).abs() < 0.05 {
        "9:16 竖屏"
    } else if (r - (4.0 / 3.0)).abs() < 0.06 {
        "4:3 横屏"
    } else if (r - 1.0).abs() < 0.06 {
        "1:1 方屏"
    } else if r >= 1.0 {
        "横屏比例"
    } else {
        "竖屏比例"
    }
}

fn clamp_aspect_dims_to_work(
    mut w: f64,
    mut h: f64,
    aspect: f64,
    min_w: f64,
    min_h: f64,
    work_w: i32,
    work_h: i32,
) -> (i32, i32) {
    let max_w = f64::from(work_w.max(320));
    let max_h = f64::from(work_h.max(240));
    if w > max_w {
        w = max_w;
        h = w / aspect;
    }
    if h > max_h {
        h = max_h;
        w = h * aspect;
    }
    if w < min_w {
        w = min_w.min(max_w);
        h = (w / aspect).min(max_h);
    }
    if h < min_h {
        h = min_h.min(max_h);
        w = (h * aspect).min(max_w);
    }
    (
        (w.round() as i32).clamp(220, work_w.max(220)),
        (h.round() as i32).clamp(150, work_h.max(150)),
    )
}

/// Computes ergonomic default windowed dimensions tuned for `16:9` vs `9:16` within `work_w x work_h`.
pub fn ideal_window_size_for_video(
    vid_w: u32,
    vid_h: u32,
    work_w: i32,
    work_h: i32,
) -> (i32, i32) {
    let aspect = video_aspect_ratio(vid_w, vid_h);
    let ww = f64::from(work_w.max(480));
    let wh = f64::from(work_h.max(360));
    let (w, h) = if aspect < 1.0 {
        // Vertical 9:16 short drama: keep generous height (~78% work area) so HUD buttons never crowd.
        let mut th = (wh * 0.78).clamp(620.0, 1280.0);
        let mut tw = th * aspect;
        if tw > ww * 0.84 {
            tw = ww * 0.84;
            th = tw / aspect;
        }
        (tw, th)
    } else {
        // Horizontal 16:9: ~58% work width (1120x630 on 1080p, ~1490x838 on 1600p 150% DPI).
        let mut tw = (ww * 0.5833334).clamp(960.0, 1600.0);
        let mut th = tw / aspect;
        if th > wh * 0.84 {
            th = wh * 0.84;
            tw = th * aspect;
        }
        (tw, th)
    };
    clamp_aspect_dims_to_work(w, h, aspect, 360.0, 260.0, work_w - 16, work_h - 16)
}

/// Computes aspect-matched compact Picture-in-Picture dimensions for `16:9` or `9:16`.
pub fn ideal_pip_size_for_video(vid_w: u32, vid_h: u32, work_w: i32, work_h: i32) -> (i32, i32) {
    let aspect = video_aspect_ratio(vid_w, vid_h);
    let ww = f64::from(work_w.max(480));
    let wh = f64::from(work_h.max(360));
    let (w, h) = if aspect < 1.0 {
        let th = (wh * 0.44).clamp(440.0, 740.0);
        (th * aspect, th)
    } else {
        let tw = (ww * 0.28).clamp(480.0, 720.0);
        (tw, tw / aspect)
    };
    clamp_aspect_dims_to_work(w, h, aspect, 240.0, 160.0, work_w - 24, work_h - 24)
}

/// Snaps an existing windowed frame to exact `(vid_w, vid_h)` aspect ratio while roughly preserving area.
pub fn snap_window_size_to_aspect(
    vid_w: u32,
    vid_h: u32,
    cur_w: i32,
    cur_h: i32,
    work_w: i32,
    work_h: i32,
) -> (i32, i32) {
    let aspect = video_aspect_ratio(vid_w, vid_h);
    let area = (f64::from(cur_w.max(360)) * f64::from(cur_h.max(260))).max(1.0);
    let w = (area * aspect).sqrt();
    let h = w / aspect;
    clamp_aspect_dims_to_work(w, h, aspect, 360.0, 260.0, work_w, work_h)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowPlacement {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: u32,
    pub h: u32,
    pub maximized: bool,
    pub pip_x: Option<i32>,
    pub pip_y: Option<i32>,
    pub pip_w: Option<u32>,
    pub pip_h: Option<u32>,
    pub landscape: Option<SavedWindowRect>,
    pub portrait: Option<SavedWindowRect>,
    pub pip_landscape: Option<SavedWindowRect>,
    pub pip_portrait: Option<SavedWindowRect>,
}

impl Default for WindowPlacement {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            w: 1120,
            h: 630,
            maximized: false,
            pip_x: None,
            pip_y: None,
            pip_w: Some(528),
            pip_h: Some(297),
            landscape: None,
            portrait: None,
            pip_landscape: None,
            pip_portrait: None,
        }
    }
}

impl WindowPlacement {
    pub fn sanitized(&self) -> Self {
        let w = self.w.clamp(320, 7680);
        let h = self.h.clamp(240, 4320);
        let pip_w = self.pip_w.map(|v| v.clamp(220, 1280));
        let pip_h = self.pip_h.map(|v| v.clamp(150, 1280));
        Self {
            x: self.x,
            y: self.y,
            w,
            h,
            maximized: self.maximized,
            pip_x: self.pip_x,
            pip_y: self.pip_y,
            pip_w,
            pip_h,
            landscape: self.landscape.as_ref().map(|r| r.sanitized(360, 240)),
            portrait: self.portrait.as_ref().map(|r| r.sanitized(360, 260)),
            pip_landscape: self.pip_landscape.as_ref().map(|r| r.sanitized(220, 150)),
            pip_portrait: self.pip_portrait.as_ref().map(|r| r.sanitized(220, 150)),
        }
    }

    pub fn rect_for(&self, ori: VideoOrientation) -> Option<&SavedWindowRect> {
        match ori {
            VideoOrientation::Landscape => self.landscape.as_ref(),
            VideoOrientation::Portrait => self.portrait.as_ref(),
        }
    }

    pub fn set_rect_for(&mut self, ori: VideoOrientation, rect: SavedWindowRect) {
        let clean = rect.sanitized(360, 240);
        self.x = clean.x;
        self.y = clean.y;
        self.w = clean.w;
        self.h = clean.h;
        match ori {
            VideoOrientation::Landscape => self.landscape = Some(clean),
            VideoOrientation::Portrait => self.portrait = Some(clean),
        }
    }

    pub fn pip_rect_for(&self, ori: VideoOrientation) -> Option<&SavedWindowRect> {
        match ori {
            VideoOrientation::Landscape => self.pip_landscape.as_ref(),
            VideoOrientation::Portrait => self.pip_portrait.as_ref(),
        }
    }

    pub fn set_pip_rect_for(&mut self, ori: VideoOrientation, rect: SavedWindowRect) {
        let clean = rect.sanitized(220, 150);
        self.pip_x = clean.x;
        self.pip_y = clean.y;
        self.pip_w = Some(clean.w);
        self.pip_h = Some(clean.h);
        match ori {
            VideoOrientation::Landscape => self.pip_landscape = Some(clean),
            VideoOrientation::Portrait => self.pip_portrait = Some(clean),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PictureTuning {
    pub preset: String,
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    pub gamma: i32,
}

impl Default for PictureTuning {
    fn default() -> Self {
        Self {
            preset: "orig".to_string(),
            brightness: 0,
            contrast: 0,
            saturation: 0,
            gamma: 0,
        }
    }
}

impl PictureTuning {
    pub fn preset_values(preset: &str) -> (i32, i32, i32, i32) {
        match preset.trim().to_ascii_lowercase().as_str() {
            "vivid" => (2, 8, 16, 4),
            "eye" => (-4, -6, -8, -4),
            "night" => (6, 4, 2, 10),
            _ => (0, 0, 0, 0),
        }
    }

    pub fn apply_preset(&mut self, preset: &str) {
        let key = match preset.trim().to_ascii_lowercase().as_str() {
            "vivid" => "vivid",
            "eye" => "eye",
            "night" => "night",
            _ => "orig",
        };
        let (b, c, s, g) = Self::preset_values(key);
        self.preset = key.to_string();
        self.brightness = b;
        self.contrast = c;
        self.saturation = s;
        self.gamma = g;
    }

    pub fn clamped(&self) -> Self {
        let preset = match self.preset.trim().to_ascii_lowercase().as_str() {
            "vivid" | "eye" | "night" | "custom" => self.preset.trim().to_ascii_lowercase(),
            _ => "orig".to_string(),
        };
        Self {
            preset,
            brightness: self.brightness.clamp(-100, 100),
            contrast: self.contrast.clamp(-100, 100),
            saturation: self.saturation.clamp(-100, 100),
            gamma: self.gamma.clamp(-100, 100),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct DanmakuPrefs {
    pub enabled: bool,
    pub region: f64,
    pub opacity: f64,
    pub font_scale: f64,
    pub speed_sec: f64,
    pub font_family: String,
    pub font_weight: String,
    pub color: String,
    pub stroke: String,
}

impl Default for DanmakuPrefs {
    fn default() -> Self {
        Self {
            enabled: true,
            region: 0.25,
            opacity: 1.0,
            font_scale: 1.0,
            speed_sec: 8.0,
            font_family: "Microsoft YaHei".to_string(),
            font_weight: "700".to_string(),
            color: "#ffffff".to_string(),
            stroke: "heavy".to_string(),
        }
    }
}

impl DanmakuPrefs {
    pub fn sanitized(&self) -> Self {
        let stroke = match self.stroke.trim().to_ascii_lowercase().as_str() {
            "outline" | "shadow" | "none" => self.stroke.trim().to_ascii_lowercase(),
            _ => "heavy".to_string(),
        };
        let font_family = if self.font_family.trim().is_empty() {
            "Microsoft YaHei".to_string()
        } else {
            self.font_family.trim().to_string()
        };
        let font_weight = if matches!(self.font_weight.trim(), "400" | "normal") {
            "400".to_string()
        } else {
            "700".to_string()
        };
        let color = if self.color.starts_with('#') && (self.color.len() == 4 || self.color.len() == 7)
        {
            self.color.clone()
        } else {
            "#ffffff".to_string()
        };
        Self {
            enabled: self.enabled,
            region: clamp_finite(self.region, 0.15, 1.0, 0.25),
            opacity: clamp_finite(self.opacity, 0.15, 1.0, 1.0),
            font_scale: clamp_finite(self.font_scale, 0.6, 2.0, 1.0),
            speed_sec: clamp_finite(self.speed_sec, 4.0, 14.0, 8.0),
            font_family,
            font_weight,
            color,
            stroke,
        }
    }

    pub fn area_from_host(raw: f64) -> f64 {
        let v = if raw > 1.5 { raw / 100.0 } else { raw };
        clamp_finite(v, 0.15, 1.0, 0.25)
    }

    pub fn opacity_from_host(raw: f64) -> f64 {
        let v = if raw > 1.5 { raw / 100.0 } else { raw };
        clamp_finite(v, 0.15, 1.0, 1.0)
    }

    pub fn font_scale_from_host(raw: f64) -> f64 {
        let v = if raw > 5.0 { raw / 100.0 } else { raw };
        clamp_finite(v, 0.6, 2.0, 1.0)
    }

    pub fn speed_sec_from_host(raw: f64) -> f64 {
        if !raw.is_finite() {
            return 8.0;
        }
        let r = raw.round() as i64;
        if (0..=2).contains(&r) && (raw - r as f64).abs() < 0.05 {
            match r {
                0 => 12.0,
                2 => 5.0,
                _ => 8.0,
            }
        } else {
            raw.clamp(4.0, 14.0)
        }
    }

    pub fn area_to_host(region: f64) -> f64 {
        (clamp_finite(region, 0.15, 1.0, 0.25) * 100.0)
            .round()
            .clamp(25.0, 100.0)
    }

    pub fn opacity_to_host(opacity: f64) -> f64 {
        (clamp_finite(opacity, 0.15, 1.0, 1.0) * 100.0)
            .round()
            .clamp(10.0, 100.0)
    }

    pub fn font_scale_to_host(font_scale: f64) -> f64 {
        (clamp_finite(font_scale, 0.6, 2.0, 1.0) * 100.0)
            .round()
            .clamp(50.0, 200.0)
    }

    pub fn speed_to_host(speed_sec: f64) -> i64 {
        if !speed_sec.is_finite() {
            1
        } else if speed_sec >= 10.0 {
            0
        } else if speed_sec <= 6.5 {
            2
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct NativePlayerPrefs {
    pub theme: String,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub fg_enabled: bool,
    pub fg_mult: u32,
    pub fg_mode: String,
    pub autoplay: bool,
    pub autoplay_delay: u32,
    pub aspect: AspectMode,
    pub vsr_enabled: bool,
    pub topmost: bool,
    pub picture: PictureTuning,
    pub danmaku: DanmakuPrefs,
    pub placement: WindowPlacement,
}

impl Default for NativePlayerPrefs {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            volume: 100.0,
            muted: false,
            speed: 1.0,
            fg_enabled: true,
            fg_mult: 2,
            fg_mode: "auto".to_string(),
            autoplay: true,
            autoplay_delay: 0,
            aspect: AspectMode::Fit,
            vsr_enabled: true,
            topmost: false,
            picture: PictureTuning::default(),
            danmaku: DanmakuPrefs::default(),
            placement: WindowPlacement::default(),
        }
    }
}

impl NativePlayerPrefs {
    pub fn normalize_theme(raw: &str) -> String {
        match raw.trim().to_ascii_lowercase().as_str() {
            "light" | "white" => "light".to_string(),
            _ => "dark".to_string(),
        }
    }

    pub fn normalize_fg_mode(raw: &str) -> String {
        match raw.trim().to_ascii_lowercase().as_str() {
            "drama" | "comic" | "anime" => raw.trim().to_ascii_lowercase(),
            _ => "auto".to_string(),
        }
    }

    pub fn sanitized(&self) -> Self {
        Self {
            theme: Self::normalize_theme(&self.theme),
            volume: clamp_finite(self.volume, 0.0, 100.0, 100.0),
            muted: self.muted,
            speed: clamp_finite(self.speed, 0.25, 4.0, 1.0),
            fg_enabled: self.fg_enabled,
            fg_mult: if self.fg_mult == 0 {
                0
            } else {
                self.fg_mult.clamp(2, 5)
            },
            fg_mode: Self::normalize_fg_mode(&self.fg_mode),
            autoplay: self.autoplay,
            autoplay_delay: self.autoplay_delay.clamp(0, 15),
            aspect: self.aspect,
            vsr_enabled: self.vsr_enabled,
            topmost: self.topmost,
            picture: self.picture.clamped(),
            danmaku: self.danmaku.sanitized(),
            placement: self.placement.sanitized(),
        }
    }

    pub fn load() -> Self {
        let path = prefs_path();
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&raw)
            .map(|p| p.sanitized())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let clean = self.sanitized();
        let path = prefs_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create prefs dir failed: {e}"))?;
        }
        let body = serde_json::to_string_pretty(&clean)
            .map_err(|e| format!("serialize prefs failed: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, body).map_err(|e| format!("write prefs tmp failed: {e}"))?;
        fs::rename(&tmp, &path).map_err(|e| format!("rename prefs failed: {e}"))?;
        Ok(())
    }
}

fn clamp_finite(v: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        fallback
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HoldBoostState {
    pub active: bool,
    pub saved_speed: f64,
    pub boost_speed: f64,
}

impl Default for HoldBoostState {
    fn default() -> Self {
        Self {
            active: false,
            saved_speed: 1.0,
            boost_speed: 3.0,
        }
    }
}

impl HoldBoostState {
    pub fn begin(&mut self, current_speed: f64) -> f64 {
        if !self.active {
            self.saved_speed = clamp_finite(current_speed, 0.25, 4.0, 1.0);
            self.active = true;
        }
        self.boost_speed
    }

    pub fn end(&mut self) -> Option<f64> {
        if !self.active {
            return None;
        }
        self.active = false;
        Some(self.saved_speed)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueEpisode {
    pub index: usize,
    pub url: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub ep: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub vid: String,
    #[serde(default)]
    pub series_id: String,
    #[serde(default)]
    pub episode_id: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub start: f64,
}

#[derive(Debug, Clone, Default)]
pub struct AutoplayQueue {
    pub total_eps: usize,
    items: BTreeMap<usize, QueueEpisode>,
}

impl AutoplayQueue {
    pub fn replace(&mut self, total_eps: usize, entries: Vec<QueueEpisode>) {
        self.items.clear();
        self.total_eps = total_eps;
        for entry in entries {
            self.push(entry);
        }
        if self.total_eps == 0 {
            if let Some(&last_idx) = self.items.keys().next_back() {
                self.total_eps = last_idx + 1;
            }
        }
    }

    pub fn push(&mut self, mut entry: QueueEpisode) {
        if entry.url.trim().is_empty()
            && entry.vid.trim().is_empty()
            && entry.episode_id.trim().is_empty()
            && entry.title.trim().is_empty()
        {
            return;
        }
        if entry.ep == 0 {
            entry.ep = (entry.index + 1) as u32;
        }
        if entry.index + 1 > self.total_eps {
            self.total_eps = entry.index + 1;
        }
        self.items.insert(entry.index, entry);
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.total_eps = 0;
    }

    pub fn get(&self, index: usize) -> Option<&QueueEpisode> {
        self.items.get(&index)
    }

    pub fn next_after(&self, current_index: usize) -> Option<&QueueEpisode> {
        self.items
            .range((current_index + 1)..)
            .next()
            .map(|(_, v)| v)
    }

    pub fn prev_before(&self, current_index: usize) -> Option<&QueueEpisode> {
        self.items
            .range(..current_index)
            .next_back()
            .map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_presets_and_clamping() {
        let mut p = PictureTuning::default();
        p.apply_preset("VIVID");
        assert_eq!(p.preset, "vivid");
        assert_eq!(p.brightness, 2);
        assert_eq!(p.contrast, 8);
        assert_eq!(p.saturation, 16);
        assert_eq!(p.gamma, 4);

        let dirty = PictureTuning {
            preset: "weird".to_string(),
            brightness: 500,
            contrast: -500,
            saturation: 42,
            gamma: -120,
        };
        let clean = dirty.clamped();
        assert_eq!(clean.preset, "orig");
        assert_eq!(clean.brightness, 100);
        assert_eq!(clean.contrast, -100);
        assert_eq!(clean.saturation, 42);
        assert_eq!(clean.gamma, -100);
    }

    #[test]
    fn hold_boost_restores_exact_prior_speed() {
        let mut hb = HoldBoostState::default();
        assert_eq!(hb.end(), None);
        assert_eq!(hb.begin(1.25), 3.0);
        // Repeated begin while already boosting must preserve original 1.25.
        assert_eq!(hb.begin(3.0), 3.0);
        assert_eq!(hb.end(), Some(1.25));
        assert_eq!(hb.end(), None);
    }

    #[test]
    fn autoplay_queue_handles_sparse_out_of_order_and_dedupes() {
        let mut q = AutoplayQueue::default();
        q.push(QueueEpisode {
            index: 4,
            url: "https://example.com/5.mp4".into(),
            ep: 5,
            title: "Ep5".into(),
            vid: "v5".into(),
            headers: BTreeMap::new(),
            duration: 90.0,
            start: 0.0,
            ..Default::default()
        });
        q.push(QueueEpisode {
            index: 1,
            url: "https://example.com/2.mp4".into(),
            ep: 2,
            title: "Ep2".into(),
            vid: "v2".into(),
            headers: BTreeMap::new(),
            duration: 90.0,
            start: 0.0,
            ..Default::default()
        });
        // Completely blank entries must be rejected, while metadata-only entries are kept.
        q.push(QueueEpisode {
            index: 2,
            url: "   ".into(),
            ep: 3,
            title: String::new(),
            vid: String::new(),
            headers: BTreeMap::new(),
            duration: 0.0,
            start: 0.0,
            ..Default::default()
        });
        q.push(QueueEpisode {
            index: 3,
            url: String::new(),
            ep: 4,
            title: "Ep4".into(),
            vid: "v4".into(),
            headers: BTreeMap::new(),
            duration: 90.0,
            start: 0.0,
            ..Default::default()
        });
        // Update index 4.
        q.push(QueueEpisode {
            index: 4,
            url: "https://example.com/5b.mp4".into(),
            ep: 5,
            title: "Ep5b".into(),
            vid: "v5".into(),
            headers: BTreeMap::new(),
            duration: 95.0,
            start: 10.0,
            ..Default::default()
        });
        assert_eq!(q.len(), 3);
        assert_eq!(q.total_eps, 5);
        assert_eq!(q.next_after(1).map(|e| e.index), Some(3));
        assert_eq!(q.prev_before(4).map(|e| e.index), Some(3));
        assert_eq!(q.get(4).unwrap().url, "https://example.com/5b.mp4");
    }

    #[test]
    fn prefs_round_trip_and_sanitization() {
        let unique = std::env::temp_dir().join(format!(
            "ttv-player-test-prefs-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&unique);
        std::env::set_var("TTV_NATIVE_DATA_DIR", &unique);

        let prefs = NativePlayerPrefs {
            theme: "light".into(),
            volume: 140.0,
            fg_mult: 9,
            autoplay_delay: 0,
            speed: f64::NAN,
            ..Default::default()
        };
        prefs.save().expect("save prefs");

        let loaded = NativePlayerPrefs::load();
        assert_eq!(loaded.theme, "light");
        assert_eq!(loaded.volume, 100.0);
        assert_eq!(loaded.fg_mult, 5);
        assert_eq!(loaded.autoplay_delay, 0);
        assert_eq!(loaded.speed, 1.0);

        let clamped_hi = NativePlayerPrefs {
            autoplay_delay: 99,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(clamped_hi.autoplay_delay, 15);

        let _ = fs::remove_dir_all(&unique);
        std::env::remove_var("TTV_NATIVE_DATA_DIR");
    }

    #[test]
    fn orientation_window_and_pip_sizing_fit_work_area_and_aspect() {
        // 16:9 on 1920x1040 work area -> ~1120x630
        let (w16, h16) = ideal_window_size_for_video(1920, 1080, 1920, 1040);
        assert!((f64::from(w16) / f64::from(h16) - (16.0 / 9.0)).abs() < 0.01);
        assert!((1000..=1200).contains(&w16));

        // 9:16 on 2560x1520 work area (1600p 150% DPI) -> ~666x1185 (~444x790 CSS)
        let (w9, h9) = ideal_window_size_for_video(1080, 1920, 2560, 1520);
        assert!((f64::from(w9) / f64::from(h9) - (9.0 / 16.0)).abs() < 0.01);
        assert!((1000..=1280).contains(&h9));

        // PiP sizing matches landscape vs portrait aspect
        let (pw16, ph16) = ideal_pip_size_for_video(1920, 1080, 1920, 1040);
        assert!(pw16 > ph16);
        assert!((f64::from(pw16) / f64::from(ph16) - (16.0 / 9.0)).abs() < 0.02);

        let (pw9, ph9) = ideal_pip_size_for_video(1080, 1920, 2560, 1520);
        assert!(ph9 > pw9);
        assert!((f64::from(pw9) / f64::from(ph9) - (9.0 / 16.0)).abs() < 0.02);

        // Snap existing weird shape cleanly back to exact aspect
        let (sw, sh) = snap_window_size_to_aspect(1920, 1080, 900, 900, 1920, 1040);
        assert!((f64::from(sw) / f64::from(sh) - (16.0 / 9.0)).abs() < 0.01);
    }

    #[test]
    fn placement_separates_landscape_and_portrait_with_legacy_compat() {
        let legacy_json = r#"{"x":100,"y":80,"w":1120,"h":676,"maximized":false,"pip_w":264,"pip_h":469}"#;
        let mut pl: WindowPlacement = serde_json::from_str(legacy_json).expect("legacy parse");
        assert!(pl.rect_for(VideoOrientation::Landscape).is_none());
        assert!(pl.rect_for(VideoOrientation::Portrait).is_none());

        pl.set_rect_for(
            VideoOrientation::Landscape,
            SavedWindowRect {
                x: Some(120),
                y: Some(60),
                w: 1440,
                h: 810,
            },
        );
        pl.set_rect_for(
            VideoOrientation::Portrait,
            SavedWindowRect {
                x: Some(400),
                y: Some(40),
                w: 666,
                h: 1184,
            },
        );
        assert_eq!(pl.rect_for(VideoOrientation::Landscape).unwrap().w, 1440);
        assert_eq!(pl.rect_for(VideoOrientation::Portrait).unwrap().h, 1184);
    }

    #[test]
    fn danmaku_host_unit_roundtrips() {
        // Host percent (25..100) <-> internal fraction (0.15..1.0)
        assert!((DanmakuPrefs::area_from_host(25.0) - 0.25).abs() < 1e-6);
        assert!((DanmakuPrefs::area_from_host(100.0) - 1.0).abs() < 1e-6);
        assert!((DanmakuPrefs::area_from_host(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(DanmakuPrefs::area_to_host(0.25), 25.0);
        assert_eq!(DanmakuPrefs::area_to_host(0.15), 25.0);
        assert_eq!(DanmakuPrefs::area_to_host(1.0), 100.0);

        // Opacity percent (10..100) <-> fraction (0.15..1.0)
        assert!((DanmakuPrefs::opacity_from_host(80.0) - 0.80).abs() < 1e-6);
        assert!((DanmakuPrefs::opacity_from_host(0.4) - 0.40).abs() < 1e-6);
        assert_eq!(DanmakuPrefs::opacity_to_host(0.85), 85.0);

        // Font scale percent (50..200) <-> factor (0.6..2.0)
        assert!((DanmakuPrefs::font_scale_from_host(125.0) - 1.25).abs() < 1e-6);
        assert!((DanmakuPrefs::font_scale_from_host(1.25) - 1.25).abs() < 1e-6);
        assert_eq!(DanmakuPrefs::font_scale_to_host(1.25), 125.0);

        // Speed enum (0=slow, 1=med, 2=fast) <-> seconds (12 / 8 / 5)
        assert_eq!(DanmakuPrefs::speed_sec_from_host(0.0), 12.0);
        assert_eq!(DanmakuPrefs::speed_sec_from_host(1.0), 8.0);
        assert_eq!(DanmakuPrefs::speed_sec_from_host(2.0), 5.0);
        assert_eq!(DanmakuPrefs::speed_sec_from_host(9.5), 9.5);
        assert_eq!(DanmakuPrefs::speed_to_host(12.0), 0);
        assert_eq!(DanmakuPrefs::speed_to_host(8.0), 1);
        assert_eq!(DanmakuPrefs::speed_to_host(5.0), 2);
    }
}
