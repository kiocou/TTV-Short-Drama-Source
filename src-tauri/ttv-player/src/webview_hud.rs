use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2CompositionController,
    ICoreWebView2Controller, ICoreWebView2Controller2, ICoreWebView2Controller3,
    ICoreWebView2Environment, ICoreWebView2Environment3, ICoreWebView2Settings3,
    ICoreWebView2Settings6, ICoreWebView2_3, COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS,
    COREWEBVIEW2_COLOR, COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
    COREWEBVIEW2_MOUSE_EVENT_KIND, COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
    COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
};
use webview2_com::{
    take_pwstr, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, CursorChangedEventHandler,
    RasterizationScaleChangedEventHandler, WebMessageReceivedEventHandler,
};
use windows::core::{IUnknown, Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::DirectComposition::{IDCompositionDevice, IDCompositionVisual};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{LoadCursorW, SetCursor, HCURSOR, IDC_ARROW};

use crate::dcomp::client_size_px;
use crate::prefs::{data_dir, webview2_user_data_dir};

pub const EMBEDDED_HUD_HTML: &str = include_str!("../assets/hud.html");

const INHERITED_WV2_VARS: [&str; 6] = [
    "WEBVIEW2_USER_DATA_FOLDER",
    "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
    "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
    "WEBVIEW2_RELEASE_CHANNEL_PREFERENCE",
    "WEBVIEW2_WAIT_FOR_SCRIPT_DEBUGGER",
    "WEBVIEW2_WAIT_FOR_DEBUGGER",
];

pub fn clear_inherited_webview2_env() {
    for key in INHERITED_WV2_VARS {
        std::env::remove_var(key);
    }
}

pub fn materialize_hud_assets() -> Result<PathBuf, String> {
    let dir = data_dir().join("hud-assets");
    fs::create_dir_all(&dir).map_err(|e| format!("create hud-assets dir failed: {e}"))?;
    let target = dir.join("hud.html");
    let up_to_date = fs::read_to_string(&target)
        .map(|existing| existing == EMBEDDED_HUD_HTML)
        .unwrap_or(false);
    if !up_to_date {
        fs::write(&target, EMBEDDED_HUD_HTML)
            .map_err(|e| format!("write embedded hud.html failed: {e}"))?;
    }
    Ok(dir)
}

pub fn scale_from_dpi(dpi: u32) -> f64 {
    if dpi == 0 {
        1.0
    } else {
        (f64::from(dpi) / 96.0).clamp(0.5, 4.0)
    }
}

fn hwnd_rasterization_scale(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    scale_from_dpi(dpi)
}

type HudMessageCallback = Rc<RefCell<Box<dyn FnMut(Value)>>>;

pub struct WebViewHudInner {
    hwnd: HWND,
    dcomp_device: Option<IDCompositionDevice>,
    hud_visual: Option<IDCompositionVisual>,
    pub comp_ctrl: Option<ICoreWebView2CompositionController>,
    pub base_ctrl: Option<ICoreWebView2Controller>,
    pub webview: Option<ICoreWebView2>,
    pub hud_ready: bool,
    pending_msgs: Vec<String>,
    last_bounds: Cell<(u32, u32)>,
    last_scale: Cell<f64>,
    pub current_cursor: HCURSOR,
    on_message: HudMessageCallback,
}

pub type SharedWebViewHud = Rc<RefCell<WebViewHudInner>>;

impl WebViewHudInner {
    pub fn start<F>(
        hwnd: HWND,
        dcomp_device: Option<IDCompositionDevice>,
        hud_visual: Option<IDCompositionVisual>,
        on_message: F,
    ) -> Result<SharedWebViewHud, String>
    where
        F: FnMut(Value) + 'static,
    {
        clear_inherited_webview2_env();
        let assets_dir = materialize_hud_assets()?;
        let hud = Rc::new(RefCell::new(Self {
            hwnd,
            dcomp_device,
            hud_visual,
            comp_ctrl: None,
            base_ctrl: None,
            webview: None,
            hud_ready: false,
            pending_msgs: Vec::new(),
            last_bounds: Cell::new((0, 0)),
            last_scale: Cell::new(hwnd_rasterization_scale(hwnd)),
            current_cursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
            on_message: Rc::new(RefCell::new(Box::new(on_message))),
        }));

        let primary_udf = webview2_user_data_dir();
        if Self::try_create_env(&hud, &assets_dir, &primary_udf, true).is_err() {
            let fallback_udf = data_dir()
                .join("ttv-player-webview2-fallback")
                .join(std::process::id().to_string());
            Self::try_create_env(&hud, &assets_dir, &fallback_udf, false)?;
        }

        Ok(hud)
    }

    fn try_create_env(
        hud: &SharedWebViewHud,
        assets_dir: &Path,
        udf_dir: &Path,
        allow_fallback: bool,
    ) -> Result<(), String> {
        fs::create_dir_all(udf_dir).map_err(|e| format!("create webview2 udf dir failed: {e}"))?;
        let hud_weak = Rc::downgrade(hud);
        let assets_for_env = assets_dir.to_path_buf();
        let udf_wide: Vec<u16> = udf_dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |err, env_opt: Option<ICoreWebView2Environment>| {
                if err.is_err() || env_opt.is_none() {
                    if allow_fallback {
                        if let Some(hud_rc) = hud_weak.upgrade() {
                            let fallback_udf = data_dir()
                                .join("ttv-player-webview2-fallback")
                                .join(std::process::id().to_string());
                            let _ = Self::try_create_env(&hud_rc, &assets_for_env, &fallback_udf, false);
                        }
                    }
                    return err;
                }
                let Some(env) = env_opt else {
                    return Ok(());
                };
                let Ok(env3) = env.cast::<ICoreWebView2Environment3>() else {
                    return Ok(());
                };
                let Some(hud_rc) = hud_weak.upgrade() else {
                    return Ok(());
                };
                let hwnd_target = hud_rc.borrow().hwnd;
                let assets_dir_inner = assets_for_env.clone();
                let hud_weak2 = Rc::downgrade(&hud_rc);

                let comp_handler =
                    CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
                        move |cerr, comp_opt: Option<ICoreWebView2CompositionController>| {
                            cerr?;
                            let Some(comp_ctrl) = comp_opt else {
                                return Ok(());
                            };
                            let Some(hud_rc2) = hud_weak2.upgrade() else {
                                return Ok(());
                            };
                            let _ = attach_composition_controller(
                                &hud_rc2,
                                comp_ctrl,
                                &assets_dir_inner,
                            );
                            Ok(())
                        },
                    ));

                unsafe {
                    env3.CreateCoreWebView2CompositionController(hwnd_target, &comp_handler)
                }
            },
        ));

        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                None,
                PCWSTR(udf_wide.as_ptr()),
                None,
                &env_handler,
            )
        }
        .map_err(|e| format!("CreateCoreWebView2EnvironmentWithOptions failed: {e}"))
    }

    pub fn post_json(&mut self, msg: &Value) {
        let Ok(json_str) = serde_json::to_string(msg) else {
            return;
        };
        if !self.hud_ready || self.webview.is_none() {
            if self.pending_msgs.len() < 256 {
                self.pending_msgs.push(json_str);
            }
            return;
        }
        if let Some(wv) = &self.webview {
            post_raw_json(wv, &json_str);
        }
    }

    pub fn mark_hud_ready(&mut self) {
        self.hud_ready = true;
        let queued = std::mem::take(&mut self.pending_msgs);
        if let Some(wv) = &self.webview {
            for json_str in queued {
                post_raw_json(wv, &json_str);
            }
        }
        let (cw, ch) = client_size_px(self.hwnd);
        self.resize(cw, ch);
    }

    fn post_host_resize(&self, cw: u32, ch: u32, scale: f64) {
        if self.hud_ready {
            if let Some(wv) = &self.webview {
                let msg = json!({
                    "type": "hostResize",
                    "w": cw,
                    "h": ch,
                    "scale": scale,
                })
                .to_string();
                post_raw_json(wv, &msg);
            }
        }
    }

    fn sync_rasterization_scale(&self, ctrl: &ICoreWebView2Controller) -> f64 {
        let target = hwnd_rasterization_scale(self.hwnd);
        if let Ok(ctrl3) = ctrl.cast::<ICoreWebView2Controller3>() {
            let mut cur = target;
            let _ = unsafe { ctrl3.RasterizationScale(&mut cur) };
            if (cur - target).abs() > 0.005 {
                self.last_scale.set(target);
                unsafe {
                    let _ = ctrl3.SetRasterizationScale(target);
                }
            } else {
                self.last_scale.set(cur);
            }
        } else {
            self.last_scale.set(target);
        }
        self.last_scale.get()
    }

    pub fn resize(&self, w: u32, h: u32) {
        let cw = w.max(1);
        let ch = h.max(1);
        let mut scale = self.last_scale.get();
        if let Some(ctrl) = &self.base_ctrl {
            scale = self.sync_rasterization_scale(ctrl);
            let bounds = RECT {
                left: 0,
                top: 0,
                right: cw as i32,
                bottom: ch as i32,
            };
            let ok = unsafe { ctrl.SetBounds(bounds) }.is_ok();
            unsafe {
                let _ = ctrl.NotifyParentWindowPositionChanged();
            }
            if ok {
                self.last_bounds.set((cw, ch));
            }
        }
        if let Some(dcomp) = &self.dcomp_device {
            unsafe {
                let _ = dcomp.Commit();
            }
        }
        self.post_host_resize(cw, ch, scale);
    }

    pub fn notify_position_changed(&self) {
        if let Some(ctrl) = &self.base_ctrl {
            unsafe {
                let _ = ctrl.NotifyParentWindowPositionChanged();
            }
        }
        if let Some(dcomp) = &self.dcomp_device {
            unsafe {
                let _ = dcomp.Commit();
            }
        }
    }

    pub fn sync_to_hwnd_client_rect(&self) {
        let Some(ctrl) = &self.base_ctrl else {
            return;
        };
        let (cw, ch) = client_size_px(self.hwnd);
        let expected_rect = RECT {
            left: 0,
            top: 0,
            right: cw as i32,
            bottom: ch as i32,
        };
        let mut actual_bounds = expected_rect;
        let _ = unsafe { ctrl.Bounds(&mut actual_bounds) };
        let bounds_mismatch = actual_bounds.left != expected_rect.left
            || actual_bounds.top != expected_rect.top
            || actual_bounds.right != expected_rect.right
            || actual_bounds.bottom != expected_rect.bottom;
        let scale_mismatch = (hwnd_rasterization_scale(self.hwnd) - self.last_scale.get()).abs() > 0.005;
        if (cw, ch) != self.last_bounds.get() || bounds_mismatch || scale_mismatch {
            self.resize(cw, ch);
        }
    }

    pub fn send_mouse(
        &self,
        kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
        vkeys: i32,
        mouse_data: u32,
        pt: POINT,
    ) {
        if let Some(comp) = &self.comp_ctrl {
            unsafe {
                let _ = comp.SendMouseInput(
                    kind,
                    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(vkeys),
                    mouse_data,
                    pt,
                );
            }
        }
    }

    pub fn notify_host_leave(&mut self) {
        self.send_mouse(
            COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
            0,
            0,
            POINT { x: -1, y: -1 },
        );
        self.post_json(&json!({ "type": "hostLeave" }));
    }

    pub fn release_pointer_capture_for_drag(&self, client_pt: POINT) {
        if let Some(comp) = &self.comp_ctrl {
            unsafe {
                let _ = comp.SendMouseInput(
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
                    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                    0,
                    client_pt,
                );
                let _ = comp.SendMouseInput(
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
                    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                    0,
                    client_pt,
                );
            }
        }
        unsafe {
            let _ = ReleaseCapture();
            if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                let _ = SetCursor(Some(arrow));
            }
        }
    }

    pub fn apply_cursor(&self) {
        let cur = if self.current_cursor.is_invalid() {
            unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() }
        } else {
            self.current_cursor
        };
        unsafe {
            let _ = SetCursor(Some(cur));
        }
    }

    pub fn move_focus(&self) {
        if let Some(ctrl) = &self.base_ctrl {
            unsafe {
                let _ = ctrl.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
            }
        }
    }

    pub fn close(&mut self) {
        if let Some(ctrl) = self.base_ctrl.take() {
            unsafe {
                let _ = ctrl.Close();
            }
        }
        self.comp_ctrl = None;
        self.webview = None;
    }
}

fn attach_composition_controller(
    hud_rc: &SharedWebViewHud,
    comp_ctrl: ICoreWebView2CompositionController,
    assets_dir: &Path,
) -> Result<(), String> {
    let base_ctrl: ICoreWebView2Controller = comp_ctrl
        .cast()
        .map_err(|e| format!("cast ICoreWebView2Controller: {e}"))?;
    if let Ok(ctrl2) = comp_ctrl.cast::<ICoreWebView2Controller2>() {
        unsafe {
            let _ = ctrl2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                A: 0,
                R: 0,
                G: 0,
                B: 0,
            });
        }
    }

    let webview = unsafe { base_ctrl.CoreWebView2() }
        .map_err(|e| format!("CoreWebView2() failed: {e}"))?;

    // Harden HUD webview UX: no browser right-click menu, zoom, status bar, or swipe navigation.
    if let Ok(settings) = unsafe { webview.Settings() } {
        unsafe {
            let _ = settings.SetAreDefaultContextMenusEnabled(false);
            let _ = settings.SetIsStatusBarEnabled(false);
            let _ = settings.SetIsZoomControlEnabled(false);
        }
        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
            unsafe {
                let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
            }
        }
        if let Ok(s6) = settings.cast::<ICoreWebView2Settings6>() {
            unsafe {
                let _ = s6.SetIsSwipeNavigationEnabled(false);
            }
        }
    }

    let hwnd = {
        let b = hud_rc.borrow();
        if let Some(hud_vis) = &b.hud_visual {
            if let Ok(unk) = hud_vis.cast::<IUnknown>() {
                unsafe {
                    let _ = comp_ctrl.SetRootVisualTarget(&unk);
                }
            }
        }
        if let Some(dcomp) = &b.dcomp_device {
            unsafe {
                let _ = dcomp.Commit();
            }
        }
        b.hwnd
    };

    let init_scale = hwnd_rasterization_scale(hwnd);
    if let Ok(ctrl3) = base_ctrl.cast::<ICoreWebView2Controller3>() {
        unsafe {
            let _ = ctrl3.SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS);
            let _ = ctrl3.SetShouldDetectMonitorScaleChanges(true);
            let _ = ctrl3.SetRasterizationScale(init_scale);
        }
        let mut scale_token = 0i64;
        let hud_weak_scale = Rc::downgrade(hud_rc);
        let ctrl3_for_scale = ctrl3.clone();
        let scale_handler = RasterizationScaleChangedEventHandler::create(Box::new(move |_sender, _args| {
            if let Some(rc) = hud_weak_scale.upgrade() {
                if let Ok(b) = rc.try_borrow() {
                    let mut actual_scale = hwnd_rasterization_scale(b.hwnd);
                    let _ = unsafe { ctrl3_for_scale.RasterizationScale(&mut actual_scale) };
                    b.last_scale.set(actual_scale);
                    let (cw, ch) = client_size_px(b.hwnd);
                    if let Some(ctrl) = &b.base_ctrl {
                        let bounds = RECT {
                            left: 0,
                            top: 0,
                            right: cw as i32,
                            bottom: ch as i32,
                        };
                        if unsafe { ctrl.SetBounds(bounds) }.is_ok() {
                            b.last_bounds.set((cw, ch));
                        }
                        unsafe {
                            let _ = ctrl.NotifyParentWindowPositionChanged();
                        }
                    }
                    if let Some(dcomp) = &b.dcomp_device {
                        unsafe {
                            let _ = dcomp.Commit();
                        }
                    }
                    b.post_host_resize(cw, ch, actual_scale);
                }
            }
            Ok(())
        }));
        unsafe {
            let _ = ctrl3.add_RasterizationScaleChanged(&scale_handler, &mut scale_token);
        }
    }

    let (w, h) = client_size_px(hwnd);
    unsafe {
        let _ = base_ctrl.SetBounds(RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        });
        let _ = base_ctrl.NotifyParentWindowPositionChanged();
        let _ = base_ctrl.SetIsVisible(true);
    }

    let mut cur_token = 0i64;
    let hud_weak_cur = Rc::downgrade(hud_rc);
    let comp_for_cur = comp_ctrl.clone();
    let cursor_handler = CursorChangedEventHandler::create(Box::new(move |_sender, _args| {
        if let Some(rc) = hud_weak_cur.upgrade() {
            let mut hcur = HCURSOR::default();
            if unsafe { comp_for_cur.Cursor(&mut hcur) }.is_ok() {
                if let Ok(mut b) = rc.try_borrow_mut() {
                    b.current_cursor = hcur;
                    b.apply_cursor();
                }
            }
        }
        Ok(())
    }));
    unsafe {
        let _ = comp_ctrl.add_CursorChanged(&cursor_handler, &mut cur_token);
    }

    let mut msg_token = 0i64;
    let on_msg_rc = hud_rc.borrow().on_message.clone();
    let msg_handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut pw = windows::core::PWSTR::null();
        if unsafe { args.WebMessageAsJson(&mut pw) }.is_ok() {
            let s = take_pwstr(pw);
            if let Ok(val) = serde_json::from_str::<Value>(&s) {
                if let Ok(mut cb) = on_msg_rc.try_borrow_mut() {
                    cb(val);
                }
            }
        }
        Ok(())
    }));
    unsafe {
        let _ = webview.add_WebMessageReceived(&msg_handler, &mut msg_token);
    }

    if let Ok(wv3) = webview.cast::<ICoreWebView2_3>() {
        let host_wide: Vec<u16> = "hud.ttv.local\0".encode_utf16().collect();
        let dir_wide: Vec<u16> = assets_dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = wv3.SetVirtualHostNameToFolderMapping(
                PCWSTR(host_wide.as_ptr()),
                PCWSTR(dir_wide.as_ptr()),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
            );
        }
    }

    let nav_wide: Vec<u16> = "https://hud.ttv.local/hud.html\0".encode_utf16().collect();
    unsafe {
        let _ = webview.Navigate(PCWSTR(nav_wide.as_ptr()));
    }

    let dcomp_for_commit = {
        let mut b = hud_rc.borrow_mut();
        b.comp_ctrl = Some(comp_ctrl);
        b.base_ctrl = Some(base_ctrl);
        b.webview = Some(webview);
        b.last_bounds.set((w, h));
        b.last_scale.set(init_scale);
        b.dcomp_device.clone()
    };
    if let Some(dcomp) = dcomp_for_commit {
        unsafe {
            let _ = dcomp.Commit();
        }
    }
    Ok(())
}

fn post_raw_json(wv: &ICoreWebView2, json_str: &str) {
    let wide: Vec<u16> = json_str.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = wv.PostWebMessageAsJson(PCWSTR(wide.as_ptr()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_inherited_webview2_env_removes_all_parent_overrides() {
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", "C:\\bad");
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "--bad");
        clear_inherited_webview2_env();
        assert!(std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none());
        assert!(std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_none());
    }

    #[test]
    fn scale_from_dpi_handles_fractional_and_zero_dpi() {
        assert!((scale_from_dpi(0) - 1.0).abs() < 1e-6);
        assert!((scale_from_dpi(96) - 1.0).abs() < 1e-6);
        assert!((scale_from_dpi(120) - 1.25).abs() < 1e-6);
        assert!((scale_from_dpi(144) - 1.5).abs() < 1e-6);
        assert!((scale_from_dpi(168) - 1.75).abs() < 1e-6);
        assert!((scale_from_dpi(960) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn embedded_hud_contains_crystal_scrollbars_resize_and_f11_guards() {
        for needle in [
            "::-webkit-scrollbar-button",
            "overflow-x: hidden",
            "F11",
            "hostResize",
            "syncMenuViewportConstraints",
            "is-resizing",
            "getLayoutBoxInFrame",
            "--frame-w",
            "drag-handle",
            "min-width: 28px",
            "--inner-highlight",
            "chrome-scrim",
            "syncCrystalRangeVisual",
            "formatDisplayTitle",
            "vol-unity-mark",
            "--fill-start",
            "latestSeekSeq",
            "finishScrub",
            "btnDanmaku",
            "pipHud",
            "is-pip",
            "EMOTE_MAP",
            "'送花':",
            "restoreWindow",
            ".player-frame.is-locked.is-idle .lock-fab { opacity: 0;",
        ] {
            assert!(
                EMBEDDED_HUD_HTML.contains(needle),
                "EMBEDDED_HUD_HTML missing regression guard `{needle}`"
            );
        }
        assert!(
            !EMBEDDED_HUD_HTML.contains("Math.max(frameEl.clientWidth"),
            "EMBEDDED_HUD_HTML must not use stale Math.max width latching on shrink"
        );
    }
}
