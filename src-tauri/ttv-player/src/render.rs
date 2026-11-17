use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::DXGI_PRESENT;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CreateEventW, CreateWaitableTimerExW, SetEvent, SetWaitableTimer, WaitForMultipleObjects,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

use crate::dcomp::{DCompVideoHost, monitor_refresh_hz};
use crate::enhance::{compute_native_mpv_canvas, VideoEnhanceEngine};
use crate::interp::{
    compute_active_uv_spec, read_bgra_texture, CompiledShaderBlobs, GpuInterpPipeline,
    InterpContentMode, InterpShaderSet, LinearBlitPass,
};
use crate::mpv::{MpvEngine, MpvLiveState};
use crate::prefs::{screenshots_dir, AspectMode};

pub struct RenderConfigShared {
    pub stopping: AtomicBool,
    pub fg_enabled: AtomicBool,
    pub fg_mult: AtomicU32,
    pub vsr_enabled: AtomicBool,
    pub vsr_dirty: AtomicBool,
    pub vsr_hw_active: AtomicBool,
    pub vsr_hw_supported: AtomicBool,
    pub mode_raw: AtomicU32, // 0=Drama, 1=Comic, 2=Anime
    pub aspect_raw: AtomicU32, // 0=Fit, 1=Cover, 2=Stretch
    pub light_theme: AtomicBool,
    pub pending_resize_w: AtomicU32,
    pub pending_resize_h: AtomicU32,
    pub resize_dirty: AtomicBool,
    pub reset_frames_dirty: AtomicBool,
    pub seek_reset_dirty: AtomicBool,
    pub video_rect_x: AtomicI32,
    pub video_rect_y: AtomicI32,
    pub video_rect_w: AtomicU32,
    pub video_rect_h: AtomicU32,
    pub monitor_hz_bits: AtomicU64,
    pub shot_requested: AtomicBool,
    pub last_shot_path: Mutex<Option<String>>,
    pub out_fps_bits: AtomicU64,
    pub active_interp: AtomicBool,
    pub pipeline_ready: AtomicBool,
    pub has_presented_frame: AtomicBool,
    notify_event: isize,
}

impl RenderConfigShared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        init_w: u32,
        init_h: u32,
        init_hz: f64,
        fg_enabled: bool,
        fg_mult: u32,
        vsr_enabled: bool,
        mode: InterpContentMode,
        light_theme: bool,
    ) -> Result<Arc<Self>, String> {
        let evt = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| format!("CreateEventW render notify failed: {e}"))?;
        Ok(Arc::new(Self {
            stopping: AtomicBool::new(false),
            fg_enabled: AtomicBool::new(fg_enabled),
            fg_mult: AtomicU32::new(if fg_mult == 0 { 0 } else { fg_mult.clamp(2, 5) }),
            vsr_enabled: AtomicBool::new(vsr_enabled),
            vsr_dirty: AtomicBool::new(false),
            vsr_hw_active: AtomicBool::new(false),
            vsr_hw_supported: AtomicBool::new(false),
            mode_raw: AtomicU32::new(mode as u32),
            aspect_raw: AtomicU32::new(AspectMode::Fit.to_u32()),
            light_theme: AtomicBool::new(light_theme),
            pending_resize_w: AtomicU32::new(init_w.max(64)),
            pending_resize_h: AtomicU32::new(init_h.max(64)),
            resize_dirty: AtomicBool::new(false),
            reset_frames_dirty: AtomicBool::new(false),
            seek_reset_dirty: AtomicBool::new(false),
            video_rect_x: AtomicI32::new(0),
            video_rect_y: AtomicI32::new(0),
            video_rect_w: AtomicU32::new(init_w.max(64)),
            video_rect_h: AtomicU32::new(init_h.max(64)),
            monitor_hz_bits: AtomicU64::new(init_hz.max(24.0).to_bits()),
            shot_requested: AtomicBool::new(false),
            last_shot_path: Mutex::new(None),
            out_fps_bits: AtomicU64::new(0.0f64.to_bits()),
            active_interp: AtomicBool::new(false),
            pipeline_ready: AtomicBool::new(false),
            has_presented_frame: AtomicBool::new(false),
            notify_event: evt.0 as isize,
        }))
    }

    pub fn wake(&self) {
        if self.notify_event != 0 {
            unsafe {
                let _ = SetEvent(HANDLE(self.notify_event as *mut _));
            }
        }
    }

    pub fn request_resize(&self, w: u32, h: u32) {
        let cw = w.max(32);
        let ch = h.max(32);
        self.pending_resize_w.store(cw, Ordering::Release);
        self.pending_resize_h.store(ch, Ordering::Release);
        // Keep video_rect locked to the full client swapchain bounds during resizes
        // so stretching the window never stalls behind an async DOM ResizeObserver round-trip.
        self.video_rect_x.store(0, Ordering::Release);
        self.video_rect_y.store(0, Ordering::Release);
        self.video_rect_w.store(cw, Ordering::Release);
        self.video_rect_h.store(ch, Ordering::Release);
        self.resize_dirty.store(true, Ordering::Release);
        self.wake();
    }

    pub fn set_video_rect(&self, x: i32, y: i32, w: u32, h: u32) {
        self.video_rect_x.store(x, Ordering::Release);
        self.video_rect_y.store(y, Ordering::Release);
        self.video_rect_w.store(w.max(1), Ordering::Release);
        self.video_rect_h.store(h.max(1), Ordering::Release);
        self.wake();
    }

    pub fn set_monitor_hz(&self, hz: f64) {
        if hz.is_finite() && hz >= 24.0 {
            self.monitor_hz_bits.store(hz.to_bits(), Ordering::Release);
        }
    }

    pub fn monitor_hz(&self) -> f64 {
        f64::from_bits(self.monitor_hz_bits.load(Ordering::Acquire))
    }

    pub fn has_presented_frame(&self) -> bool {
        self.has_presented_frame.load(Ordering::Acquire)
    }

    pub fn is_pipeline_ready(&self) -> bool {
        self.pipeline_ready.load(Ordering::Acquire)
    }

    pub fn reset_frames(&self) {
        self.has_presented_frame.store(false, Ordering::Release);
        self.reset_frames_dirty.store(true, Ordering::Release);
        self.wake();
    }

    pub fn mark_seek(&self) {
        self.seek_reset_dirty.store(true, Ordering::Release);
        self.wake();
    }

    pub fn out_fps(&self) -> f64 {
        f64::from_bits(self.out_fps_bits.load(Ordering::Acquire))
    }

    pub fn content_mode(&self) -> InterpContentMode {
        match self.mode_raw.load(Ordering::Acquire) {
            1 => InterpContentMode::Comic,
            2 => InterpContentMode::Anime,
            _ => InterpContentMode::Drama,
        }
    }

    pub fn aspect_mode(&self) -> AspectMode {
        AspectMode::from_u32(self.aspect_raw.load(Ordering::Acquire))
    }

    pub fn set_aspect_mode(&self, aspect: AspectMode) {
        self.aspect_raw.store(aspect.to_u32(), Ordering::Release);
        self.vsr_dirty.store(true, Ordering::Release);
        self.wake();
    }

    pub fn set_vsr_enabled(&self, on: bool) {
        let prev = self.vsr_enabled.swap(on, Ordering::AcqRel);
        if prev != on {
            self.vsr_dirty.store(true, Ordering::Release);
            self.wake();
        }
    }

    pub fn is_vsr_hw_active(&self) -> bool {
        self.vsr_hw_active.load(Ordering::Acquire)
    }
}

impl Drop for RenderConfigShared {
    fn drop(&mut self) {
        if self.notify_event != 0 {
            unsafe {
                let _ = CloseHandle(HANDLE(self.notify_event as *mut _));
            }
        }
    }
}

pub struct RenderController {
    pub shared: Arc<RenderConfigShared>,
    pub mpv: Arc<MpvEngine>,
    join: Option<JoinHandle<()>>,
}

impl RenderController {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        hwnd: windows::Win32::Foundation::HWND,
        dcomp: DCompVideoHost,
        fg_enabled: bool,
        fg_mult: u32,
        vsr_enabled: bool,
        mode: InterpContentMode,
        light_theme: bool,
        shader_blobs_rx: std::sync::mpsc::Receiver<Result<CompiledShaderBlobs, String>>,
    ) -> Result<Self, String> {
        let hz = monitor_refresh_hz(hwnd);
        let shared = RenderConfigShared::new(
            dcomp.swap_w,
            dcomp.swap_h,
            hz,
            fg_enabled,
            fg_mult,
            vsr_enabled,
            mode,
            light_theme,
        )?;
        let mpv = Arc::new(MpvEngine::new(Some(&dcomp.device), hwnd.0 as isize)?);
        let mpv_for_thread = mpv.clone();
        let shared_for_thread = shared.clone();

        let join = thread::Builder::new()
            .name("ttv-render".into())
            .spawn(move || {
                run_render_loop(dcomp, mpv_for_thread, shared_for_thread, shader_blobs_rx);
            })
            .map_err(|e| format!("spawn render thread failed: {e}"))?;

        Ok(Self {
            shared,
            mpv,
            join: Some(join),
        })
    }

    pub fn shutdown(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.wake();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for RenderController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn create_staging_render_target(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w.max(64),
        Height: h.max(64),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .map_err(|e| format!("CreateTexture2D mpv staging({w}x{h}) failed: {e}"))?;
    tex.ok_or_else(|| "CreateTexture2D mpv staging returned None".into())
}

/// Keeps optical-flow compute resolution bounded <= 1920px on the longest edge so
/// even 4K/5K monitors run 4-level optical flow at full 120Hz+ with zero GPU hitch.
fn capped_pipeline_dims(vw: u32, vh: u32) -> (u32, u32) {
    let w = vw.max(64);
    let h = vh.max(64);
    let max_edge = w.max(h);
    if max_edge <= 1920 {
        (w, h)
    } else {
        let scale = 1920.0 / f64::from(max_edge);
        (
            ((f64::from(w) * scale).round() as u32).max(64),
            ((f64::from(h) * scale).round() as u32).max(64),
        )
    }
}

fn compute_target_interval_sec(
    state: &MpvLiveState,
    shared: &RenderConfigShared,
    measured_src_interval: f64,
) -> (bool, f64, f64) {
    let src_fps = state.source_fps().clamp(10.0, 240.0);
    let speed = state.speed().clamp(0.25, 4.0);
    let nominal_src_interval = 1.0 / (src_fps * speed);
    let effective_src_interval = if measured_src_interval.is_finite()
        && measured_src_interval > nominal_src_interval * 0.45
        && measured_src_interval < nominal_src_interval * 1.85
    {
        measured_src_interval
    } else {
        nominal_src_interval
    };

    let fg_on = shared.fg_enabled.load(Ordering::Acquire);
    let raw_mult = shared.fg_mult.load(Ordering::Acquire);
    let monitor_hz = f64::from_bits(shared.monitor_hz_bits.load(Ordering::Acquire)).clamp(24.0, 360.0);
    if fg_on && !state.is_paused() && !state.is_buffering() {
        let desired_fps = if raw_mult == 0 {
            monitor_hz
        } else {
            let mult = f64::from(raw_mult.clamp(2, 5));
            (src_fps * speed * mult).min(monitor_hz)
        };
        let interval = (1.0 / desired_fps.max(24.0)).max(1.0 / 360.0);
        (true, interval, effective_src_interval)
    } else {
        (false, nominal_src_interval, effective_src_interval)
    }
}

fn qpc_freq() -> f64 {
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1) as f64
}

fn qpc_now() -> i64 {
    let mut t = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
    }
    t
}

fn theme_clear_color(light: bool) -> [f32; 4] {
    if light {
        [0.953, 0.957, 0.969, 1.0]
    } else {
        [0.039, 0.043, 0.055, 1.0]
    }
}

fn run_render_loop(
    mut dcomp: DCompVideoHost,
    mpv: Arc<MpvEngine>,
    shared: Arc<RenderConfigShared>,
    shader_blobs_rx: std::sync::mpsc::Receiver<Result<CompiledShaderBlobs, String>>,
) {
    let freq = qpc_freq();
    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
    }
    .ok();

    let mut pipeline: Option<GpuInterpPipeline> = None;
    let mut fallback_blit: Option<LinearBlitPass> = None;
    let mut enhance = VideoEnhanceEngine::new(dcomp.device.clone(), dcomp.ctx.clone()).ok();
    let mut blobs_pending = true;
    let mut mpv_tex: Option<(u32, u32, ID3D11Texture2D)> = None;
    let mut flow_src_tex: Option<ID3D11Texture2D> = None;
    let mut has_drawn_mpv_tex = false;
    let mut last_fg_on = shared.fg_enabled.load(Ordering::Acquire);
    let mut last_buffering = mpv.state.is_buffering();

    let mut last_mpv_frame_qpc = qpc_now();
    let mut prev_mpv_frame_qpc = 0i64;
    let mut measured_src_interval = 0.0f64;
    let mut next_slot_qpc = 0i64;

    let mut fps_window_start = qpc_now();
    let mut fps_present_count = 0u32;

    // Initial clear so window never shows black garbage while stream connects.
    dcomp.clear_and_present(
        theme_clear_color(shared.light_theme.load(Ordering::Acquire)),
        0,
    );

    let wait_handles = [
        mpv.render_wakeup_event(),
        HANDLE(shared.notify_event as *mut _),
    ];

    while !shared.stopping.load(Ordering::Acquire) {
        // Lazy-finish background HLSL DXBC compilation once ready.
        if blobs_pending {
            if let Ok(res) = shader_blobs_rx.try_recv() {
                blobs_pending = false;
                if let Ok(blobs) = res {
                    if let Ok(shader_set) = InterpShaderSet::from_blobs(&dcomp.device, blobs) {
                        if let Ok(mut p) = GpuInterpPipeline::new(
                            dcomp.device.clone(),
                            dcomp.ctx.clone(),
                            shader_set,
                        ) {
                            if has_drawn_mpv_tex {
                                if let Some(tex) = flow_src_tex.as_ref().or_else(|| mpv_tex.as_ref().map(|(_, _, t)| t)) {
                                    let (sw, sh) = mpv.state.video_size();
                                    let mut td = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                                    unsafe { tex.GetDesc(&mut td) };
                                    p.set_active_uv_spec(compute_active_uv_spec(
                                        sw,
                                        sh,
                                        td.Width,
                                        td.Height,
                                        shared.aspect_mode(),
                                    ));
                                    let _ = p.push_frame_and_prepare(
                                        tex,
                                        shared.content_mode(),
                                        false,
                                    );
                                }
                            }
                            pipeline = Some(p);
                            shared.pipeline_ready.store(true, Ordering::Release);
                        }
                    }
                }
            }
        }

        let mut layout_or_enhance_dirty = false;
        if shared.resize_dirty.swap(false, Ordering::AcqRel) {
            let rw = shared.pending_resize_w.load(Ordering::Acquire);
            let rh = shared.pending_resize_h.load(Ordering::Acquire);
            if dcomp.resize_swapchain(rw, rh).is_ok() {
                layout_or_enhance_dirty = true;
            }
        }

        if shared.reset_frames_dirty.swap(false, Ordering::AcqRel) {
            shared.has_presented_frame.store(false, Ordering::Release);
            if let Some(p) = &mut pipeline {
                p.reset_frames();
            }
            flow_src_tex = None;
            prev_mpv_frame_qpc = 0;
            measured_src_interval = 0.0;
            next_slot_qpc = 0;
            layout_or_enhance_dirty = true;
        }

        if shared.seek_reset_dirty.swap(false, Ordering::AcqRel) {
            if let Some(p) = &mut pipeline {
                p.mark_seek_discontinuity();
            }
            prev_mpv_frame_qpc = 0;
            measured_src_interval = 0.0;
            next_slot_qpc = 0;
        }

        if shared.vsr_dirty.swap(false, Ordering::AcqRel) {
            layout_or_enhance_dirty = true;
        }

        let fg_on = shared.fg_enabled.load(Ordering::Acquire);
        if fg_on != last_fg_on {
            last_fg_on = fg_on;
            next_slot_qpc = 0;
            layout_or_enhance_dirty = true;
            if let Some(p) = &mut pipeline {
                p.mark_seek_discontinuity();
            }
        }

        let now_buffering = mpv.state.is_buffering();
        if now_buffering != last_buffering {
            last_buffering = now_buffering;
            if !now_buffering {
                prev_mpv_frame_qpc = 0;
                measured_src_interval = 0.0;
                next_slot_qpc = 0;
                if let Some(p) = &mut pipeline {
                    p.mark_seek_discontinuity();
                }
            }
        }

        // Compute target video rectangle inside the swapchain backbuffer.
        let vx = shared.video_rect_x.load(Ordering::Acquire).max(0) as u32;
        let vy = shared.video_rect_y.load(Ordering::Acquire).max(0) as u32;
        let max_vw = dcomp.swap_w.saturating_sub(vx).max(1);
        let max_vh = dcomp.swap_h.saturating_sub(vy).max(1);
        let vw = shared.video_rect_w.load(Ordering::Acquire).min(max_vw).max(64);
        let vh = shared.video_rect_h.load(Ordering::Acquire).min(max_vh).max(64);

        let mode = shared.content_mode();
        let vsr_on = shared.vsr_enabled.load(Ordering::Acquire);
        let paused = mpv.state.is_paused();
        let interp_candidate = fg_on && !paused && !mpv.state.is_buffering();

        // Only cap to <= 1920px when 4-level optical flow is actively running;
        // when FG is off or paused, rasterize at full native swapchain resolution.
        let (tgt_w, tgt_h) = if interp_candidate {
            capped_pipeline_dims(vw, vh)
        } else {
            (vw, vh)
        };
        let (src_w, src_h) = mpv.state.video_size();
        let aspect = shared.aspect_mode();
        let canvas_plan = compute_native_mpv_canvas(src_w, src_h, tgt_w, tgt_h, aspect, vsr_on);

        let need_new_mpv_tex = match &mpv_tex {
            Some((tw, th, _)) => *tw != canvas_plan.mpv_w || *th != canvas_plan.mpv_h,
            None => true,
        };
        if need_new_mpv_tex {
            if let Ok(tex) = create_staging_render_target(&dcomp.device, canvas_plan.mpv_w, canvas_plan.mpv_h) {
                mpv_tex = Some((canvas_plan.mpv_w, canvas_plan.mpv_h, tex));
                has_drawn_mpv_tex = false;
                flow_src_tex = None;
                layout_or_enhance_dirty = true;
            }
        }

        let target_uv = compute_active_uv_spec(src_w, src_h, tgt_w, tgt_h, aspect);
        if let Some(p) = &mut pipeline {
            p.set_active_uv_spec(target_uv);
        }

        // Pull at most one decoded frame from mpv when mpv signaled update, or
        // redraw current frame immediately when canvas size / VSR toggle / aspect changed.
        let mut got_new_mpv_frame = false;
        let mut forced_refresh_frame = false;
        let mut decoded_frame_now = false;
        if let Some((_, _, tex)) = &mpv_tex {
            if mpv.render_if_updated(tex) {
                got_new_mpv_frame = true;
                decoded_frame_now = true;
                has_drawn_mpv_tex = true;
                let now = qpc_now();
                let nominal_dt = 1.0 / (mpv.state.source_fps().clamp(10.0, 240.0) * mpv.state.speed().clamp(0.25, 4.0));
                if prev_mpv_frame_qpc != 0 {
                    let dt = (now - prev_mpv_frame_qpc) as f64 / freq;
                    if dt > nominal_dt * 0.70 && dt < nominal_dt * 1.40 {
                        measured_src_interval = if measured_src_interval <= 0.0 {
                            dt
                        } else {
                            measured_src_interval * 0.90 + dt * 0.10
                        };
                    }
                }
                prev_mpv_frame_qpc = now;
                last_mpv_frame_qpc = now;
            } else if (need_new_mpv_tex || layout_or_enhance_dirty)
                && mpv.state.is_file_loaded()
                && mpv.render_current_frame(tex)
            {
                got_new_mpv_frame = true;
                forced_refresh_frame = true;
                has_drawn_mpv_tex = true;
            }

            if got_new_mpv_frame {
                let mut frame_for_flow = tex.clone();
                if interp_candidate {
                    if vsr_on || canvas_plan.mpv_w != tgt_w || canvas_plan.mpv_h != tgt_h {
                        if let Some(eng) = enhance.as_mut() {
                            if let Ok(enhanced) = eng.process_to_texture_with_vsr(
                                tex,
                                canvas_plan,
                                tgt_w,
                                tgt_h,
                                target_uv,
                                vsr_on,
                            ) {
                                frame_for_flow = enhanced;
                            }
                            shared
                                .vsr_hw_supported
                                .store(eng.status.hw_ext_supported, Ordering::Release);
                            shared
                                .vsr_hw_active
                                .store(eng.status.hw_active && vsr_on, Ordering::Release);
                        }
                    } else {
                        shared.vsr_hw_active.store(false, Ordering::Release);
                    }
                    flow_src_tex = Some(frame_for_flow.clone());
                    if let Some(p) = &mut pipeline {
                        if forced_refresh_frame {
                            p.mark_seek_discontinuity();
                        }
                        let _ = p.push_frame_and_prepare(&frame_for_flow, mode, decoded_frame_now);
                    }
                } else {
                    flow_src_tex = Some(tex.clone());
                    if let Some(p) = &mut pipeline {
                        p.mark_seek_discontinuity();
                        let _ = p.push_frame_and_prepare(tex, mode, false);
                    }
                }
            }
        }

        if shared.shot_requested.swap(false, Ordering::AcqRel) {
            if let Some(tex) = flow_src_tex.as_ref().or_else(|| mpv_tex.as_ref().map(|(_, _, t)| t)) {
                if let Ok(path) = save_texture_screenshot(&dcomp.device, &dcomp.ctx, tex) {
                    if let Ok(mut g) = shared.last_shot_path.lock() {
                        *g = Some(path);
                    }
                }
            }
        }

        let (interp_running, target_interval, src_interval) =
            compute_target_interval_sec(&mpv.state, &shared, measured_src_interval);

        let can_present_video = has_drawn_mpv_tex
            && (pipeline.as_ref().is_some_and(|p| p.has_prev_frame) || got_new_mpv_frame);
        let should_present_now = if !can_present_video {
            !shared.has_presented_frame.load(Ordering::Acquire) || layout_or_enhance_dirty
        } else if interp_running {
            true
        } else {
            got_new_mpv_frame || layout_or_enhance_dirty
        };

        if can_present_video && should_present_now {
            let now = qpc_now();
            let pred_elapsed = ((now - last_mpv_frame_qpc) as f64 / freq).max(0.0)
                + if interp_running { target_interval * 0.35 } else { 0.0 };
            let alpha = if interp_running
                && pipeline.as_ref().is_some_and(|p| p.prepared_pair)
            {
                (pred_elapsed / src_interval.max(0.004)).clamp(0.0, 1.0) as f32
            } else {
                1.0
            };

            dcomp.wait_frame_latency(16);
            if let Some(rtv) = &dcomp.backbuffer_rtv {
                let clear = [0.02f32, 0.02, 0.025, 1.0];
                unsafe {
                    dcomp.ctx.ClearRenderTargetView(rtv, &clear);
                }

                let mut synthesized = false;
                if interp_running || (pipeline.as_ref().is_some_and(|p| p.has_prev_frame) && !vsr_on && canvas_plan.mpv_w == vw && canvas_plan.mpv_h == vh) {
                    if let Some(p) = &pipeline {
                        if p.has_prev_frame && interp_running {
                            synthesized = p
                                .synthesize_to_rtv(
                                    alpha,
                                    rtv,
                                    vx as f32,
                                    vy as f32,
                                    vw as f32,
                                    vh as f32,
                                    mode,
                                )
                                .is_ok();
                        }
                    }
                }

                if !synthesized {
                    if let Some((_, _, tex)) = &mpv_tex {
                        let mut enhanced_ok = false;
                        if vsr_on || canvas_plan.mpv_w != vw || canvas_plan.mpv_h != vh {
                            if let Some(eng) = enhance.as_mut() {
                                let back_uv = compute_active_uv_spec(src_w, src_h, vw, vh, aspect);
                                enhanced_ok = eng
                                    .render_to_target_rtv_at(
                                        tex,
                                        canvas_plan,
                                        (vx as f32, vy as f32),
                                        (vw, vh),
                                        rtv,
                                        back_uv,
                                        vsr_on,
                                    )
                                    .is_ok();
                                shared
                                    .vsr_hw_supported
                                    .store(eng.status.hw_ext_supported, Ordering::Release);
                                shared
                                    .vsr_hw_active
                                    .store(eng.status.hw_active && vsr_on, Ordering::Release);
                            }
                        } else if !vsr_on {
                            shared.vsr_hw_active.store(false, Ordering::Release);
                        }

                        if !enhanced_ok {
                            blit_copy_fallback(
                                &dcomp,
                                rtv,
                                tex,
                                vx,
                                vy,
                                vw,
                                vh,
                                &mut fallback_blit,
                            );
                        }
                    }
                }

                unsafe {
                    let _ = dcomp.swap_chain.Present(0, DXGI_PRESENT(0));
                }
                if !shared.has_presented_frame.swap(true, Ordering::AcqRel) {
                    mpv.state.notify_frame_presented();
                }

                fps_present_count += 1;
                let fps_dt = (qpc_now() - fps_window_start) as f64 / freq;
                if fps_dt >= 0.5 {
                    let out_fps = f64::from(fps_present_count) / fps_dt;
                    shared
                        .out_fps_bits
                        .store(out_fps.to_bits(), Ordering::Release);
                    shared
                        .active_interp
                        .store(interp_running && fg_on && !paused, Ordering::Release);
                    fps_window_start = qpc_now();
                    fps_present_count = 0;
                }
            }
        } else if !can_present_video && should_present_now {
            // Still loading / idle before first decoded frame: only repaint on events/resize/theme.
            dcomp.clear_and_present(
                theme_clear_color(shared.light_theme.load(Ordering::Acquire)),
                0,
            );
            shared.active_interp.store(false, Ordering::Release);
            shared.vsr_hw_active.store(false, Ordering::Release);
        }

        // Sleep cleanly until absolute phase-locked subframe deadline or immediate wakeup.
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        if interp_running && can_present_video {
            let slot_ticks = ((target_interval * freq).round() as i64).max(1);
            let now_after = qpc_now();
            if next_slot_qpc == 0 || now_after - next_slot_qpc > slot_ticks * 2 {
                next_slot_qpc = now_after + slot_ticks;
            } else {
                next_slot_qpc += slot_ticks;
                if next_slot_qpc <= now_after {
                    next_slot_qpc = now_after + slot_ticks / 2;
                }
            }
            let wait_ticks = (next_slot_qpc - now_after).max(1);
            let wait_100ns = -(((wait_ticks as f64 / freq) * 10_000_000.0).round() as i64).clamp(5_000, 350_000);
            if let Some(t) = timer {
                unsafe {
                    let _ = SetWaitableTimer(t, &wait_100ns, 0, None, None, false);
                    let handles = [wait_handles[0], wait_handles[1], t];
                    let _ = WaitForMultipleObjects(&handles, false, 30);
                }
            } else {
                unsafe {
                    let _ = WaitForMultipleObjects(&wait_handles, false, 8);
                }
            }
        } else {
            next_slot_qpc = 0;
            // When paused / no-interp / idle: 0% spin, wake on new mpv frame or command!
            let timeout = if can_present_video && !paused {
                32
            } else {
                250
            };
            unsafe {
                let _ = WaitForMultipleObjects(&wait_handles, false, timeout);
            }
        }
    }

    if let Some(t) = timer {
        unsafe {
            let _ = CloseHandle(t);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_copy_fallback(
    dcomp: &DCompVideoHost,
    rtv: &ID3D11RenderTargetView,
    tex: &ID3D11Texture2D,
    vx: u32,
    vy: u32,
    vw: u32,
    vh: u32,
    fallback_blit: &mut Option<LinearBlitPass>,
) {
    if let Ok(bb) = unsafe { dcomp.swap_chain.GetBuffer::<ID3D11Texture2D>(0) } {
        let mut tdesc = D3D11_TEXTURE2D_DESC::default();
        let mut bdesc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            tex.GetDesc(&mut tdesc);
            bb.GetDesc(&mut bdesc);
        }
        if vx == 0
            && vy == 0
            && tdesc.Width == vw
            && tdesc.Height == vh
            && tdesc.Width == bdesc.Width
            && tdesc.Height == bdesc.Height
        {
            unsafe {
                dcomp.ctx.OMSetRenderTargets(None, None);
                dcomp.ctx.CopyResource(&bb, tex);
            }
            return;
        }
        if fallback_blit.is_none() {
            if let Ok(pass) = LinearBlitPass::new(dcomp.device.clone(), dcomp.ctx.clone()) {
                *fallback_blit = Some(pass);
            }
        }
        if let Some(pass) = fallback_blit.as_ref() {
            if pass
                .blit_to_rtv(tex, rtv, vx as f32, vy as f32, vw as f32, vh as f32)
                .is_ok()
            {
                return;
            }
        }
        let copy_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: tdesc.Width.min(bdesc.Width),
            bottom: tdesc.Height.min(bdesc.Height),
            back: 1,
        };
        unsafe {
            dcomp.ctx.OMSetRenderTargets(None, None);
            dcomp
                .ctx
                .CopySubresourceRegion(&bb, 0, 0, 0, 0, tex, 0, Some(&copy_box));
        }
    }
}

fn save_texture_screenshot(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
) -> Result<String, String> {
    let (w, h, bgra) = read_bgra_texture(device, ctx, tex)?;
    let dir = screenshots_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create screenshots dir failed: {e}"))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path: PathBuf = dir.join(format!("ttv-shot-{ts}.png"));
    write_bgra_png(&path, w, h, &bgra)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Minimal dependency-free uncompressed-DEFLATE RGBA PNG encoder for screenshots.
pub fn write_bgra_png(path: &PathBuf, w: u32, h: u32, bgra: &[u8]) -> Result<(), String> {
    let row_len = (w as usize) * 4 + 1;
    let mut raw = Vec::with_capacity(row_len * h as usize);
    for y in 0..h as usize {
        raw.push(0); // filter type 0 (None)
        let row = &bgra[y * (w as usize) * 4..(y + 1) * (w as usize) * 4];
        for px in row.chunks_exact(4) {
            raw.push(px[2]); // R
            raw.push(px[1]); // G
            raw.push(px[0]); // B
            raw.push(255);   // opaque A
        }
    }

    let mut zlib = Vec::with_capacity(raw.len() + (raw.len() / 65535 + 2) * 5 + 8);
    zlib.push(0x78);
    zlib.push(0x01);
    let mut offset = 0usize;
    while offset < raw.len() {
        let chunk_len = (raw.len() - offset).min(65535);
        let is_last = offset + chunk_len >= raw.len();
        zlib.push(if is_last { 1 } else { 0 });
        let len_u16 = chunk_len as u16;
        let nlen_u16 = !len_u16;
        zlib.extend_from_slice(&len_u16.to_le_bytes());
        zlib.extend_from_slice(&nlen_u16.to_le_bytes());
        zlib.extend_from_slice(&raw[offset..offset + chunk_len]);
        offset += chunk_len;
    }
    let adler = adler32(&raw);
    zlib.extend_from_slice(&adler.to_be_bytes());

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    push_png_chunk(&mut png, b"IHDR", &ihdr);
    push_png_chunk(&mut png, b"IDAT", &zlib);
    push_png_chunk(&mut png, b"IEND", &[]);

    fs::write(path, png).map_err(|e| format!("write png failed: {e}"))
}

fn push_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320u32 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &b in bytes {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    (s2 << 16) | s1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WNDCLASSEXW,
        WS_EX_NOREDIRECTIONBITMAP, WS_POPUP,
    };

    unsafe extern "system" fn test_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn sample_rgb(w: u32, h: u32, bgra: &[u8], xf: f64, yf: f64) -> (u8, u8, u8) {
        let x = ((w as f64) * xf).round().clamp(0.0, (w - 1) as f64) as u32;
        let y = ((h as f64) * yf).round().clamp(0.0, (h - 1) as f64) as u32;
        let i = ((y * w + x) * 4) as usize;
        (bgra[i + 2], bgra[i + 1], bgra[i])
    }

    fn assert_full_colorbar_coverage(label: &str, w: u32, h: u32, bgra: &[u8]) {
        assert_eq!((w, h), (2560, 1600), "{label}: unexpected backbuffer dims");
        // Verify right-most Cyan bar (x=0.91 > 0.75) in both upper and lower quadrants,
        // plus bottom half of Magenta/Yellow/Blue/Red bars (y=0.90 > 0.75).
        let cyan_top = sample_rgb(w, h, bgra, 0.91, 0.25);
        assert!(
            cyan_top.0 < 40 && cyan_top.1 > 200 && cyan_top.2 > 200,
            "{label}: right-top cyan missing/cropped at 2560x1600: {cyan_top:?}"
        );
        let cyan_bot = sample_rgb(w, h, bgra, 0.91, 0.90);
        assert!(
            cyan_bot.0 < 40 && cyan_bot.1 > 200 && cyan_bot.2 > 200,
            "{label}: right-bottom cyan missing/cropped at 2560x1600: {cyan_bot:?}"
        );
        let red_bot = sample_rgb(w, h, bgra, 0.08, 0.90);
        assert!(
            red_bot.0 > 200 && red_bot.1 < 40 && red_bot.2 < 40,
            "{label}: bottom-left red missing/cropped at 2560x1600: {red_bot:?}"
        );
        let yellow_bot = sample_rgb(w, h, bgra, 0.41, 0.90);
        assert!(
            yellow_bot.0 > 200 && yellow_bot.1 > 200 && yellow_bot.2 < 40,
            "{label}: bottom-center yellow missing/cropped at 2560x1600: {yellow_bot:?}"
        );
        let magenta_bot = sample_rgb(w, h, bgra, 0.78, 0.90);
        assert!(
            magenta_bot.0 > 200 && magenta_bot.1 < 40 && magenta_bot.2 > 200,
            "{label}: bottom-right magenta missing/cropped at 2560x1600: {magenta_bot:?}"
        );
    }

    #[test]
    fn fullscreen_2560x1600_resize_never_crops_to_pipeline_scissor() {
        if crate::mpv::find_mpv_dll().is_none() {
            return;
        }
        let clip = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join("playback-parallel-20261112")
            .join("native")
            .join("media")
            .join("motion-24.mp4");
        if !clip.is_file() {
            return;
        }

        let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }.unwrap().into();
        let cls = w!("TTVFullscreenResizeTest");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(test_wnd_proc),
            hInstance: hinst,
            lpszClassName: cls,
            ..Default::default()
        };
        unsafe { RegisterClassExW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                cls,
                w!("resize-test"),
                WS_POPUP,
                0,
                0,
                1120,
                676,
                None,
                None,
                Some(hinst),
                None,
            )
        }
        .unwrap();

        let mut dcomp = DCompVideoHost::new(hwnd, 1120, 676).unwrap();
        let mpv = MpvEngine::new(Some(&dcomp.device), hwnd.0 as isize).unwrap();
        let blobs = CompiledShaderBlobs::load_or_compile().unwrap();
        let shaders = InterpShaderSet::from_blobs(&dcomp.device, blobs).unwrap();
        let mut pipeline =
            GpuInterpPipeline::new(dcomp.device.clone(), dcomp.ctx.clone(), shaders).unwrap();
        let mut fallback_blit = None;

        mpv.load_file(&clip.to_string_lossy(), "", 0.0, &BTreeMap::new())
            .unwrap();

        let tex_1120 = create_staging_render_target(&dcomp.device, 1120, 676).unwrap();
        let start = Instant::now();
        let mut rendered_1120 = 0;
        while rendered_1120 < 3 && start.elapsed() < Duration::from_secs(5) {
            if mpv.render_if_updated(&tex_1120) {
                rendered_1120 += 1;
                pipeline
                    .push_frame_and_prepare(&tex_1120, InterpContentMode::Drama, true)
                    .unwrap();
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(rendered_1120 >= 2, "failed initial frames at 1120x676");

        dcomp.resize_swapchain(2560, 1600).unwrap();
        let (pw, ph) = capped_pipeline_dims(2560, 1600);
        assert_eq!((pw, ph), (1920, 1200));
        let tex_1920 = create_staging_render_target(&dcomp.device, pw, ph).unwrap();

        let mut got_new = 0;
        let t2 = Instant::now();
        while got_new < 3 && t2.elapsed() < Duration::from_secs(4) {
            if mpv.render_if_updated(&tex_1920) {
                got_new += 1;
                pipeline
                    .push_frame_and_prepare(&tex_1920, InterpContentMode::Drama, true)
                    .unwrap();
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(got_new >= 2, "failed frames at 1920x1200");

        // Trigger a fresh mpv render_current_frame right before drawing so libmpv's
        // D3D11 ScissorEnable=TRUE + ScissorRect(0,0,1920,1200) is actively bound.
        assert!(mpv.render_current_frame(&tex_1920));
        pipeline
            .push_frame_and_prepare(&tex_1920, InterpContentMode::Drama, false)
            .unwrap();
        let _ = mpv.render_current_frame(&tex_1920);

        let rtv = dcomp.backbuffer_rtv.as_ref().unwrap();
        unsafe {
            dcomp
                .ctx
                .ClearRenderTargetView(rtv, &[0.02, 0.02, 0.025, 1.0]);
        }
        pipeline
            .synthesize_to_rtv(
                0.33,
                rtv,
                0.0,
                0.0,
                2560.0,
                1600.0,
                InterpContentMode::Drama,
            )
            .unwrap();
        let bb: ID3D11Texture2D = unsafe { dcomp.swap_chain.GetBuffer(0) }.unwrap();
        let (bw, bh, bbgra) = read_bgra_texture(&dcomp.device, &dcomp.ctx, &bb).unwrap();
        assert_full_colorbar_coverage("synthesize_to_rtv", bw, bh, &bbgra);

        // Also verify fallback linear stretch blit scales 1920x1200 -> 2560x1600 uncropped.
        let _ = mpv.render_current_frame(&tex_1920);
        unsafe {
            dcomp
                .ctx
                .ClearRenderTargetView(rtv, &[0.02, 0.02, 0.025, 1.0]);
        }
        blit_copy_fallback(&dcomp, rtv, &tex_1920, 0, 0, 2560, 1600, &mut fallback_blit);
        let (fw, fh, fbgra) = read_bgra_texture(&dcomp.device, &dcomp.ctx, &bb).unwrap();
        assert_full_colorbar_coverage("blit_copy_fallback", fw, fh, &fbgra);

        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
