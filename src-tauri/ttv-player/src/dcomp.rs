use std::mem::size_of;
use std::ptr;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DONOTROUND, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIDevice1, IDXGIFactory2, IDXGISwapChain1, IDXGISwapChain2,
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS,
    MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowLongPtrW, IsZoomed, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_STYLE, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT,
    HTTOPRIGHT, HWND_NOTOPMOST, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SW_MAXIMIZE, SW_RESTORE, WS_CAPTION, WS_MAXIMIZE,
    WS_MAXIMIZEBOX, WS_THICKFRAME,
};

pub fn init_per_monitor_dpi() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn client_size_px(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let w = (rc.right - rc.left).max(1) as u32;
    let h = (rc.bottom - rc.top).max(1) as u32;
    (w, h)
}

pub fn monitor_refresh_hz(hwnd: HWND) -> f64 {
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi.monitorInfo).as_bool() {
            return 60.0;
        }
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
            .as_bool()
            && dm.dmDisplayFrequency >= 24
        {
            f64::from(dm.dmDisplayFrequency)
        } else {
            60.0
        }
    }
}

pub fn set_dwm_rounded_corners(hwnd: HWND, rounded: bool) {
    let pref = if rounded {
        DWMWCP_ROUND
    } else {
        DWMWCP_DONOTROUND
    };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&pref as *const _) as *const _,
            size_of::<u32>() as u32,
        );
        // Remove DWM thin border highlight in borderless fullscreen.
        let border_color: u32 = if rounded { 0x22FFFFFF } else { 0xFFFFFFFE }; // DWMWA_COLOR_NONE = 0xFFFFFFFE
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border_color as *const _) as *const _,
            size_of::<u32>() as u32,
        );
    }
}

pub fn set_window_topmost(hwnd: HWND, topmost: bool) {
    let insert_after = if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn monitor_work_area_at_point(x: i32, y: i32) -> RECT {
    use windows::Win32::Graphics::Gdi::MonitorFromPoint;
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        }
    }
}

pub fn monitor_work_area(hwnd: HWND) -> RECT {
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        }
    }
}

pub fn monitor_full_area(hwnd: HWND) -> RECT {
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcMonitor
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        }
    }
}

pub fn monitor_areas(hwnd: HWND) -> (RECT, RECT) {
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            (mi.rcMonitor, mi.rcWork)
        } else {
            let fallback = RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            };
            (fallback, fallback)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinMaxBounds {
    pub min_w: i32,
    pub min_h: i32,
    pub max_pos_x: i32,
    pub max_pos_y: i32,
    pub max_w: i32,
    pub max_h: i32,
    pub max_track_w: i32,
    pub max_track_h: i32,
}

/// Computes Win32 `WM_GETMINMAXINFO` geometry in monitor-relative coordinates.
/// During fullscreen, maximizes/track limits expand to the full monitor bounds
/// rather than clamping to `rcWork` (taskbar excluded).
pub fn compute_minmax_bounds(
    mon: RECT,
    work: RECT,
    is_fullscreen: bool,
    is_pip: bool,
) -> MinMaxBounds {
    let mon_w = (mon.right - mon.left).max(640);
    let mon_h = (mon.bottom - mon.top).max(480);
    let (max_pos_x, max_pos_y, max_w, max_h) = if is_fullscreen {
        (0, 0, mon_w, mon_h)
    } else {
        (
            work.left - mon.left,
            work.top - mon.top,
            (work.right - work.left).max(320),
            (work.bottom - work.top).max(240),
        )
    };
    MinMaxBounds {
        min_w: if is_pip { 220 } else { 360 },
        min_h: if is_pip { 150 } else { 260 },
        max_pos_x,
        max_pos_y,
        max_w,
        max_h,
        // Allow borderless fullscreen SetWindowPos up to full monitor (+32px safety)
        max_track_w: mon_w + 32,
        max_track_h: mon_h + 32,
    }
}

pub struct SavedFullscreenRect {
    pub style: isize,
    pub rect: RECT,
    pub was_maximized: bool,
}

pub fn enter_borderless_fullscreen(hwnd: HWND) -> SavedFullscreenRect {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let was_maximized = unsafe { IsZoomed(hwnd).as_bool() };
    if was_maximized {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } & !(WS_MAXIMIZE.0 as isize);
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    let mon = monitor_full_area(hwnd);
    let fs_style = style
        & !(WS_THICKFRAME.0 as isize
            | WS_CAPTION.0 as isize
            | WS_MAXIMIZEBOX.0 as isize
            | WS_MAXIMIZE.0 as isize);
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, fs_style);
    }
    set_dwm_rounded_corners(hwnd, false);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            mon.left,
            mon.top,
            mon.right - mon.left,
            mon.bottom - mon.top,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
    }
    SavedFullscreenRect {
        style,
        rect,
        was_maximized,
    }
}

pub fn exit_borderless_fullscreen(hwnd: HWND, saved: SavedFullscreenRect, keep_topmost: bool) {
    let insert_after = if keep_topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    let restored_style =
        (saved.style | (WS_THICKFRAME.0 as isize) | (WS_MAXIMIZEBOX.0 as isize))
            & !(WS_MAXIMIZE.0 as isize);
    let (cx, cy, cw, ch) = clamp_window_to_monitor(
        saved.rect.left,
        saved.rect.top,
        (saved.rect.right - saved.rect.left).max(320),
        (saved.rect.bottom - saved.rect.top).max(240),
    );
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, restored_style);
        let _ = SetWindowPos(
            hwnd,
            Some(insert_after),
            cx,
            cy,
            cw,
            ch,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
        if saved.was_maximized {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }
    set_dwm_rounded_corners(hwnd, !saved.was_maximized);
}

pub fn clamp_window_to_monitor(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    let work = monitor_work_area_at_point(x + w / 2, y + h / 2);
    let ww = (work.right - work.left).max(320);
    let wh = (work.bottom - work.top).max(240);
    let cw = w.clamp(220, ww);
    let ch = h.clamp(150, wh);
    let cx = x.clamp(work.left, (work.right - cw).max(work.left));
    let cy = y.clamp(work.top, (work.bottom - ch).max(work.top));
    (cx, cy, cw, ch)
}

pub fn nchittest_resize_border(
    hwnd: HWND,
    screen_x: i32,
    screen_y: i32,
    border_px: i32,
) -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
        return None;
    }
    let left = screen_x < rc.left + border_px;
    let right = screen_x >= rc.right - border_px;
    let top = screen_y < rc.top + border_px;
    let bottom = screen_y >= rc.bottom - border_px;
    let code = match (left, top, right, bottom) {
        (true, true, _, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, _, true, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, _, true, _) => HTRIGHT,
        (_, true, _, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => return None,
    };
    Some(code as isize)
}

fn create_d3d11_on_adapter(
    adapter: Option<&IDXGIAdapter>,
) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let driver_type = if adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    };
    let mut device = None;
    let mut ctx = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver_type,
            HMODULE(ptr::null_mut()),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
    }
    .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;

    let device = device.ok_or_else(|| "D3D11CreateDevice returned None device".to_string())?;
    let ctx = ctx.ok_or_else(|| "D3D11CreateDevice returned None context".to_string())?;

    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    if let Ok(dxgi1) = device.cast::<IDXGIDevice1>() {
        unsafe {
            let _ = dxgi1.SetMaximumFrameLatency(1);
        }
    }
    Ok((device, ctx))
}

pub fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    if let Some(pref) = crate::enhance::pick_preferred_hardware_adapter() {
        if let Ok(base) = pref.cast::<IDXGIAdapter>() {
            if let Ok(pair) = create_d3d11_on_adapter(Some(&base)) {
                return Ok(pair);
            }
        }
    }
    create_d3d11_on_adapter(None)
}

pub struct DCompVideoHost {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub swap_chain: IDXGISwapChain1,
    pub backbuffer_rtv: Option<ID3D11RenderTargetView>,
    pub swap_w: u32,
    pub swap_h: u32,
    pub waitable_latency: Option<windows::Win32::Foundation::HANDLE>,
    pub dcomp_device: Option<IDCompositionDevice>,
    // Kept alive on DCompVideoHost to retain COM visual tree ownership.
    #[allow(dead_code)]
    pub dcomp_target: Option<IDCompositionTarget>,
    #[allow(dead_code)]
    pub root_visual: Option<IDCompositionVisual>,
    #[allow(dead_code)]
    pub video_visual: Option<IDCompositionVisual>,
    pub hud_visual: Option<IDCompositionVisual>,
    pub uses_dcomp: bool,
}

unsafe impl Send for DCompVideoHost {}

impl DCompVideoHost {
    pub fn new(hwnd: HWND, init_w: u32, init_h: u32) -> Result<Self, String> {
        let (mut device, mut ctx) = create_d3d11_device()?;
        let w = init_w.max(64);
        let h = init_h.max(64);

        let mut dxgi_dev: IDXGIDevice = device.cast().map_err(|e| format!("cast IDXGIDevice: {e}"))?;
        let mut dcomp_attempt: Result<IDCompositionDevice, _> =
            unsafe { DCompositionCreateDevice(&dxgi_dev) };
        if dcomp_attempt.is_err() {
            if let Ok((fb_dev, fb_ctx)) = create_d3d11_on_adapter(None) {
                if let Ok(fb_dxgi) = fb_dev.cast::<IDXGIDevice>() {
                    let fb_dcomp = unsafe { DCompositionCreateDevice(&fb_dxgi) };
                    if fb_dcomp.is_ok() {
                        device = fb_dev;
                        ctx = fb_ctx;
                        dxgi_dev = fb_dxgi;
                        dcomp_attempt = fb_dcomp;
                    }
                }
            }
        }
        let adapter: IDXGIAdapter =
            unsafe { dxgi_dev.GetAdapter() }.map_err(|e| format!("GetAdapter: {e}"))?;
        let factory: IDXGIFactory2 =
            unsafe { adapter.GetParent() }.map_err(|e| format!("GetParent(Factory2): {e}"))?;

        let (swap_chain, waitable_latency, dcomp_device, dcomp_target, root_visual, video_visual, hud_visual, uses_dcomp) =
            if let Ok(dcomp) = dcomp_attempt {
                let sc_desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: w,
                    Height: h,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    Stereo: false.into(),
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
                };
                let sc = unsafe { factory.CreateSwapChainForComposition(&device, &sc_desc, None) }
                    .map_err(|e| format!("CreateSwapChainForComposition failed: {e}"))?;
                let waitable = configure_waitable_latency(&sc);

                let target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true) }
                    .map_err(|e| format!("CreateTargetForHwnd failed: {e}"))?;
                let root = unsafe { dcomp.CreateVisual() }
                    .map_err(|e| format!("CreateVisual root failed: {e}"))?;
                let video = unsafe { dcomp.CreateVisual() }
                    .map_err(|e| format!("CreateVisual video failed: {e}"))?;
                let hud = unsafe { dcomp.CreateVisual() }
                    .map_err(|e| format!("CreateVisual hud failed: {e}"))?;

                unsafe {
                    video.SetContent(&sc).map_err(|e| format!("video.SetContent: {e}"))?;
                    root.AddVisual(&video, false, None)
                        .map_err(|e| format!("AddVisual video: {e}"))?;
                    root.AddVisual(&hud, true, Some(&video))
                        .map_err(|e| format!("AddVisual hud: {e}"))?;
                    target.SetRoot(&root).map_err(|e| format!("SetRoot: {e}"))?;
                    dcomp.Commit().map_err(|e| format!("dcomp.Commit: {e}"))?;
                }

                (
                    sc,
                    waitable,
                    Some(dcomp),
                    Some(target),
                    Some(root),
                    Some(video),
                    Some(hud),
                    true,
                )
            } else {
                let sc_desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: w,
                    Height: h,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    Stereo: false.into(),
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
                };
                let sc = unsafe {
                    factory.CreateSwapChainForHwnd(&device, hwnd, &sc_desc, None, None)
                }
                .map_err(|e| format!("CreateSwapChainForHwnd fallback failed: {e}"))?;
                let waitable = configure_waitable_latency(&sc);
                (sc, waitable, None, None, None, None, None, false)
            };

        let backbuffer_rtv = Some(create_backbuffer_rtv(&device, &swap_chain)?);
        Ok(Self {
            device,
            ctx,
            swap_chain,
            backbuffer_rtv,
            swap_w: w,
            swap_h: h,
            waitable_latency,
            dcomp_device,
            dcomp_target,
            root_visual,
            video_visual,
            hud_visual,
            uses_dcomp,
        })
    }

    pub fn resize_swapchain(&mut self, new_w: u32, new_h: u32) -> Result<(), String> {
        let w = new_w.max(32);
        let h = new_h.max(32);
        if w == self.swap_w && h == self.swap_h && self.backbuffer_rtv.is_some() {
            return Ok(());
        }
        unsafe {
            self.ctx.OMSetRenderTargets(None, None);
        }
        self.backbuffer_rtv = None;
        let resize_res = unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
            self.swap_chain.ResizeBuffers(
                0,
                w,
                h,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )
        };
        if let Err(e) = resize_res {
            // Recover existing RTV if ResizeBuffers failed so rendering does not freeze permanently.
            self.backbuffer_rtv = create_backbuffer_rtv(&self.device, &self.swap_chain).ok();
            return Err(format!("ResizeBuffers({w}x{h}) failed: {e}"));
        }
        self.backbuffer_rtv = Some(create_backbuffer_rtv(&self.device, &self.swap_chain)?);
        self.swap_w = w;
        self.swap_h = h;
        Ok(())
    }

    pub fn clear_and_present(&self, rgba: [f32; 4], sync_interval: u32) {
        if let Some(rtv) = &self.backbuffer_rtv {
            unsafe {
                self.ctx.ClearRenderTargetView(rtv, &rgba);
                let _ = self.swap_chain.Present(sync_interval, DXGI_PRESENT(0));
            }
        }
    }

    pub fn wait_frame_latency(&self, timeout_ms: u32) {
        if let Some(h) = self.waitable_latency {
            unsafe {
                let _ = WaitForSingleObject(h, timeout_ms);
            }
        }
    }

    pub fn commit_dcomp(&self) {
        if let Some(dcomp) = &self.dcomp_device {
            unsafe {
                let _ = dcomp.Commit();
            }
        }
    }
}

impl Drop for DCompVideoHost {
    fn drop(&mut self) {
        if let Some(h) = self.waitable_latency.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

fn configure_waitable_latency(sc: &IDXGISwapChain1) -> Option<windows::Win32::Foundation::HANDLE> {
    if let Ok(sc2) = sc.cast::<IDXGISwapChain2>() {
        unsafe {
            let _ = sc2.SetMaximumFrameLatency(1);
            let h = sc2.GetFrameLatencyWaitableObject();
            if !h.is_invalid() {
                return Some(h);
            }
        }
    }
    None
}

pub fn create_backbuffer_rtv(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView, String> {
    let bb: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }
        .map_err(|e| format!("GetBuffer(0) failed: {e}"))?;
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&bb, None, Some(&mut rtv)) }
        .map_err(|e| format!("CreateRenderTargetView(backbuffer) failed: {e}"))?;
    rtv.ok_or_else(|| "CreateRenderTargetView(backbuffer) returned None".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minmax_bounds_use_monitor_relative_coords_and_full_monitor_in_fullscreen() {
        let mon = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let work = RECT {
            left: -1920,
            top: 40,
            right: 0,
            bottom: 1032,
        };
        let maximized = compute_minmax_bounds(mon, work, false, false);
        assert_eq!(maximized.max_pos_x, 0);
        assert_eq!(maximized.max_pos_y, 40);
        assert_eq!(maximized.max_w, 1920);
        assert_eq!(maximized.max_h, 992);
        assert_eq!(maximized.min_w, 360);

        let fs = compute_minmax_bounds(mon, work, true, false);
        assert_eq!(fs.max_pos_x, 0);
        assert_eq!(fs.max_pos_y, 0);
        assert_eq!(fs.max_w, 1920);
        assert_eq!(fs.max_h, 1080);
        assert!(fs.max_track_w >= 1920 && fs.max_track_h >= 1080);

        let pip = compute_minmax_bounds(mon, work, false, true);
        assert_eq!(pip.min_w, 220);
        assert_eq!(pip.min_h, 150);
    }
}
