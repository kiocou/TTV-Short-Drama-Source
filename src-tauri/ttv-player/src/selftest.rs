use std::collections::BTreeMap;
use std::fs;
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_RASTERIZER_DESC,
    D3D11_TEXTURE2D_DESC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WNDCLASSEXW,
    WS_EX_NOREDIRECTIONBITMAP, WS_POPUP,
};

use crate::dcomp::{create_d3d11_device, DCompVideoHost};
use crate::enhance::{compute_native_mpv_canvas, inspect_device_adapter, VideoEnhanceEngine};
use crate::interp::{
    compute_active_uv_spec, create_bgra_texture, read_bgra_texture, CompiledShaderBlobs,
    GpuInterpPipeline, InterpContentMode, InterpShaderSet, LinearBlitPass, VideoActiveUvSpec,
    FD_DUPLICATE, FD_SCENE_CUT, FD_STATIC_PANEL,
};
use crate::ipc::{LaunchArgs, LoadRequest, QueueSpec};
use crate::mpv::{find_mpv_dll, MpvEngine, MpvLiveState};
use crate::prefs::{
    AspectMode, AutoplayQueue, HoldBoostState, NativePlayerPrefs, PictureTuning, QueueEpisode,
};
use crate::render::write_bgra_png;
use crate::webview_hud::{clear_inherited_webview2_env, EMBEDDED_HUD_HTML};

/// PiP 小窗 chrome 高度预算：任一边长 200 CSS px 时，上下 chrome 合计不得超过 35%。
/// 参照实测：用户 2560x1600@200% 下 717x403 物理 = 358x201 CSS，旧布局用 104px
/// 近黑玻璃条吃掉 53% 的画面。
const PIP_CHROME_BUDGET_RATIO: f64 = 0.35;
const PIP_CHROME_REF_EDGE_PX: f64 = 200.0;
/// `--pip-chrome-h` 之外的额外垂直占用（.pip-hud 的 4px 底部内边距）。
const PIP_TINY_ROW_BOTTOM_PADDING_PX: f64 = 4.0;

pub fn run_selftest() -> Result<(), String> {
    let total_t0 = Instant::now();
    let test_dir = std::env::temp_dir().join(format!("ttv-player-selftest-{}", std::process::id()));
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&test_dir).map_err(|e| format!("create selftest temp dir: {e}"))?;
    std::env::set_var("TTV_NATIVE_DATA_DIR", &test_dir);

    let result = run_all_checks(&test_dir);

    let _ = fs::remove_dir_all(&test_dir);
    if result.is_ok() {
        println!(
            "[SELFTEST] ALL CHECKS PASSED in {} ms",
            total_t0.elapsed().as_millis()
        );
    }
    result
}

fn run_all_checks(test_dir: &std::path::Path) -> Result<(), String> {
    // 1. CLI + LoadRequest float parsing + Queue + Prefs
    let args = LaunchArgs::parse([
        "ttv-player.exe",
        "--pipe",
        "p1",
        "--fg",
        "1",
        "--fgmult",
        "4",
        "--fgmode",
        "comic",
        "--theme",
        "light",
        "--vsr",
        "1",
    ]);
    assert_eq!(args.fg_override, Some(true));
    assert_eq!(args.fg_mult_override, Some(4));
    assert_eq!(args.fg_mode_override.as_deref(), Some("comic"));
    assert_eq!(args.theme_override.as_deref(), Some("light"));
    assert_eq!(args.vsr_override, Some(true));

    let load_json = r#"{
        "url":"https://example.com/1.mp4",
        "ep": 14.0,
        "total": 180.0,
        "vw": 1080.0,
        "vh": 1920.0,
        "volume": 85.0,
        "index": 13.0,
        "totalEps": 80.0,
        "autoplay": true,
        "queue": { "index": 13.0, "autoplay": true, "items": [{"index":13.0,"ep":14.0,"url":"https://example.com/14.mp4"}] }
    }"#;
    let lr = LoadRequest::from_json_str(load_json)?;
    if lr.ep != 14 || lr.vw != 1080 || lr.vh != 1920 || lr.total_eps != Some(80) {
        return Err("LoadRequest float parsing mismatch".into());
    }
    if !matches!(lr.queue, Some(QueueSpec::Object(_))) {
        return Err("LoadRequest queue object mismatch".into());
    }

    let mut q = AutoplayQueue::default();
    q.push(QueueEpisode {
        index: 5,
        url: "https://example.com/6.mp4".into(),
        ep: 6,
        title: "Ep6".into(),
        vid: "v6".into(),
        headers: BTreeMap::new(),
        duration: 100.0,
        start: 0.0,
        ..Default::default()
    });
    q.push(QueueEpisode {
        index: 2,
        url: "https://example.com/3.mp4".into(),
        ep: 3,
        title: "Ep3".into(),
        vid: "v3".into(),
        headers: BTreeMap::new(),
        duration: 100.0,
        start: 0.0,
        ..Default::default()
    });
    if q.next_after(2).map(|e| e.index) != Some(5) || q.prev_before(5).map(|e| e.index) != Some(2)
    {
        return Err("AutoplayQueue sparse ordering failed".into());
    }

    let mut pic = PictureTuning::default();
    pic.apply_preset("vivid");
    if pic.contrast != 8 || pic.saturation != 16 {
        return Err("PictureTuning preset mismatch".into());
    }
    let mut hb = HoldBoostState::default();
    if hb.begin(1.5) != 3.0 || hb.end() != Some(1.5) {
        return Err("HoldBoostState restore failed".into());
    }

    let prefs = NativePlayerPrefs {
        volume: 150.0,
        fg_mult: 9,
        theme: "light".into(),
        ..Default::default()
    };
    prefs.save()?;
    let reloaded = NativePlayerPrefs::load();
    if reloaded.volume != 100.0 || reloaded.fg_mult != 5 || reloaded.theme != "light" {
        return Err("NativePlayerPrefs sanitization/reload failed".into());
    }
    println!("[SELFTEST] 1/7 Prefs, AutoplayQueue, HoldBoost, LoadRequest OK");

    // 2. WebView2 inherited env clearing
    std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", "C:\\bad_inherited");
    clear_inherited_webview2_env();
    if std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_some() {
        return Err("clear_inherited_webview2_env failed".into());
    }
    println!("[SELFTEST] 2/7 WebView2 inherited env clear OK");

    // 3. Embedded 07 Crystal HUD monochrome & regression locks
    for required in [
        r#"data-theme="dark""#,
        r#"data-theme="light""#,
        "background: transparent !important",
        "scheduleHoverMenuClose",
        "hostLeave",
        "activeRangeDrag",
        "activePickerLock",
        "hudReady",
        "videoRect",
        "startDrag",
        "fitWindow",
        "is-portrait",
        "is-tall-window",
        "is-portrait-fullscreen",
        "is-idle",
        "is-pip-tiny",
        ".player-frame.is-pip .chrome-scrim",
        ".player-frame.is-pip .pip-hud",
        "0 4px 14px rgba(0, 0, 0, 0.42)",
        ".player-frame.is-pip-tiny .pip-time",
        "display: inline-flex",
        "pipPlayInline",
        "pipCloseInline",
        "calc(100% - 14px)",
        "--pip-chrome-h: 30px",
    ] {
        if !EMBEDDED_HUD_HTML.contains(required) {
            return Err(format!("hud.html missing required contract: {required}"));
        }
    }
    for forbidden in [
        ".lab-header",
        ".spec-strip",
        "background: var(--glass);",
        "transparent 20%, transparent 74%",
    ] {
        if EMBEDDED_HUD_HTML.contains(forbidden) {
            return Err(format!("hud.html contains forbidden mockup chrome: {forbidden}"));
        }
    }
    // PiP 小窗 chrome 高度预算：单行 --pip-chrome-h + pip-hud 底部内边距合计，
    // 在 200 CSS px 边长下不得超过 PIP_CHROME_BUDGET_RATIO。旧布局 104px/196px = 53%。
    let chrome_h = EMBEDDED_HUD_HTML
        .split("--pip-chrome-h:")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .map(str::trim)
        .and_then(|s| s.trim_end_matches("px").trim().parse::<f64>().ok())
        .ok_or_else(|| "hud.html missing --pip-chrome-h for PiP single-row contract".to_string())?;
    let chrome_total = chrome_h + PIP_TINY_ROW_BOTTOM_PADDING_PX;
    let chrome_ratio = chrome_total / PIP_CHROME_REF_EDGE_PX;
    if chrome_ratio > PIP_CHROME_BUDGET_RATIO {
        return Err(format!(
            "PiP chrome height budget blown: {chrome_total}px / {PIP_CHROME_REF_EDGE_PX}px = {:.0}% > {:.0}%",
            chrome_ratio * 100.0,
            PIP_CHROME_BUDGET_RATIO * 100.0
        ));
    }
    println!(
        "[SELFTEST] 3/7 07 Crystal HUD monochrome + auto-hide + auto-collapse locks OK (PiP chrome {chrome_total}px = {:.0}% of {PIP_CHROME_REF_EDGE_PX}px)",
        chrome_ratio * 100.0
    );

    // 4. HLSL cold compile + warm DXBC cache benchmark
    let cold_blobs = CompiledShaderBlobs::load_or_compile()?;
    if cold_blobs.blobs.len() != 8 || cold_blobs.compiled_count != 8 {
        return Err(format!(
            "unexpected cold compile count: {} (blobs={})",
            cold_blobs.compiled_count,
            cold_blobs.blobs.len()
        ));
    }
    let warm_blobs = CompiledShaderBlobs::load_or_compile()?;
    if warm_blobs.cache_hits != 8 || warm_blobs.elapsed_ms > 250 {
        return Err(format!(
            "warm shader cache check failed: hits={}, elapsed_ms={}",
            warm_blobs.cache_hits, warm_blobs.elapsed_ms
        ));
    }
    println!(
        "[SELFTEST] 4/7 HLSL DXBC parallel compile ({} ms) + warm cache ({} ms, 8/8 hits) OK",
        cold_blobs.elapsed_ms, warm_blobs.elapsed_ms
    );

    // 5. D3D11 GPU 4-level optical flow accuracy + static panel + anime dedupe + 1080x1920 perf
    let (device, ctx) = create_d3d11_device()?;
    let shaders = InterpShaderSet::from_blobs(&device, warm_blobs)?;
    let mut pipe = GpuInterpPipeline::new(device.clone(), ctx.clone(), shaders)?;

    // 5a. Optical flow MAE test on moving textured patch
    let (w, h) = (256u32, 256u32);
    let f0_bytes = synth_moving_scene(w, h, 80, 100);
    let f1_gt_bytes = synth_moving_scene(w, h, 84, 102); // exact midpoint dx=4, dy=2
    let f2_bytes = synth_moving_scene(w, h, 88, 104);    // dx=8, dy=4
    let (t0_tex, _) = create_bgra_texture(&device, w, h, Some(&f0_bytes), false)?;
    let (t2_tex, _) = create_bgra_texture(&device, w, h, Some(&f2_bytes), false)?;
    let (out_tex, out_rtv) = create_bgra_texture(&device, w, h, None, true)?;
    let out_rtv = out_rtv.unwrap();

    pipe.prepare_pair(&t0_tex, &t2_tex, InterpContentMode::Drama)?;
    pipe.synthesize_to_rtv(
        0.5,
        &out_rtv,
        0.0,
        0.0,
        w as f32,
        h as f32,
        InterpContentMode::Drama,
    )?;
    let (_, _, interp_bytes) = read_bgra_texture(&device, &ctx, &out_tex)?;
    let flow_mae = compute_bgra_mae(&interp_bytes, &f1_gt_bytes);
    let blend_mae = compute_naive_blend_mae(&f0_bytes, &f2_bytes, &f1_gt_bytes);
    if flow_mae >= blend_mae * 0.7 {
        return Err(format!(
            "optical flow MAE too high vs naive blend: flow_mae={flow_mae:.2}, blend_mae={blend_mae:.2}"
        ));
    }

    // 5b. Comic static panel bypass & Anime duplicate detection
    let (same_a, _) = create_bgra_texture(&device, w, h, Some(&f0_bytes), false)?;
    let (same_b, _) = create_bgra_texture(&device, w, h, Some(&f0_bytes), false)?;
    pipe.prepare_pair(&same_a, &same_b, InterpContentMode::Comic)?;
    let dec_comic = pipe.read_decision()?;
    if (dec_comic.flags & FD_STATIC_PANEL) == 0 {
        return Err(format!(
            "Comic static panel flag not set: flags={}",
            dec_comic.flags
        ));
    }
    pipe.prepare_pair(&same_a, &same_b, InterpContentMode::Anime)?;
    let dec_anime = pipe.read_decision()?;
    if (dec_anime.flags & (FD_DUPLICATE | FD_STATIC_PANEL)) == 0 {
        return Err(format!(
            "Anime dedupe flag not set: flags={}",
            dec_anime.flags
        ));
    }

    // 5c. 1080x1920 vertical portrait throughput test
    let (pw, ph) = (1080u32, 1920u32);
    let p_a_bytes = synth_moving_scene(pw, ph, 200, 400);
    let p_b_bytes = synth_moving_scene(pw, ph, 208, 404);
    let (p_a, _) = create_bgra_texture(&device, pw, ph, Some(&p_a_bytes), false)?;
    let (p_b, _) = create_bgra_texture(&device, pw, ph, Some(&p_b_bytes), false)?;
    let (_p_out, p_out_rtv) = create_bgra_texture(&device, pw, ph, None, true)?;
    let p_out_rtv = p_out_rtv.unwrap();
    let perf_t0 = Instant::now();
    pipe.prepare_pair(&p_a, &p_b, InterpContentMode::Drama)?;
    pipe.synthesize_to_rtv(
        0.5,
        &p_out_rtv,
        0.0,
        0.0,
        pw as f32,
        ph as f32,
        InterpContentMode::Drama,
    )?;
    unsafe { ctx.Flush() };
    let _ = pipe.read_decision()?;
    let gpu_ms = perf_t0.elapsed().as_secs_f64() * 1000.0;

    // 5d. Seek discontinuity frame retention check (prevents blank flash on seek)
    pipe.push_frame_and_prepare(&t0_tex, InterpContentMode::Drama, true)?;
    pipe.mark_seek_discontinuity();
    if !pipe.has_prev_frame || pipe.prepared_pair || !pipe.seek_discontinuity {
        return Err(format!(
            "seek discontinuity state invalid: has_prev={}, prepared={}, disc={}",
            pipe.has_prev_frame, pipe.prepared_pair, pipe.seek_discontinuity
        ));
    }
    pipe.push_frame_and_prepare(&t2_tex, InterpContentMode::Drama, true)?;
    if pipe.seek_discontinuity || !pipe.has_prev_frame || pipe.prepared_pair {
        return Err(format!(
            "post-seek reset state invalid: has_prev={}, prepared={}, disc={}",
            pipe.has_prev_frame, pipe.prepared_pair, pipe.seek_discontinuity
        ));
    }
    pipe.synthesize_to_rtv(
        0.5,
        &out_rtv,
        0.0,
        0.0,
        w as f32,
        h as f32,
        InterpContentMode::Drama,
    )?;

    // 5e. Hostile scissor-clip invariance check (libmpv leaves ScissorEnable=TRUE
    // clipped to staging 1920x1200 when presentation swapchain is 2560x1600)
    let (sw_src, sh_src) = (1920u32, 1200u32);
    let (sw_dst, sh_dst) = (2560u32, 1600u32);
    let mut cyan_bytes = vec![0u8; (sw_src * sh_src * 4) as usize];
    for px in cyan_bytes.chunks_exact_mut(4) {
        px[0] = 240; // B
        px[1] = 230; // G
        px[2] = 20;  // R
        px[3] = 255;
    }
    let (src_1920, _) = create_bgra_texture(&device, sw_src, sh_src, Some(&cyan_bytes), false)?;
    let (dst_2560, dst_2560_rtv) = create_bgra_texture(&device, sw_dst, sh_dst, None, true)?;
    let dst_2560_rtv = dst_2560_rtv.unwrap();
    pipe.push_frame_and_prepare(&src_1920, InterpContentMode::Drama, false)?;

    let rs_hostile_desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        ScissorEnable: true.into(),
        ..Default::default()
    };
    let mut rs_hostile = None;
    unsafe { device.CreateRasterizerState(&rs_hostile_desc, Some(&mut rs_hostile)) }
        .map_err(|e| format!("CreateRasterizerState(hostile): {e}"))?;
    let rs_hostile = rs_hostile.unwrap();
    let hostile_rect = RECT {
        left: 0,
        top: 0,
        right: sw_src as i32,
        bottom: sh_src as i32,
    };
    let bind_hostile_scissor = || unsafe {
        ctx.ClearRenderTargetView(&dst_2560_rtv, &[0.0, 0.0, 0.0, 1.0]);
        ctx.RSSetState(Some(&rs_hostile));
        ctx.RSSetScissorRects(Some(&[hostile_rect]));
    };

    bind_hostile_scissor();
    pipe.synthesize_to_rtv(
        1.0,
        &dst_2560_rtv,
        0.0,
        0.0,
        sw_dst as f32,
        sh_dst as f32,
        InterpContentMode::Drama,
    )?;
    let (_, _, synth_2560) = read_bgra_texture(&device, &ctx, &dst_2560)?;

    let linear_blit = LinearBlitPass::new(device.clone(), ctx.clone())?;
    bind_hostile_scissor();
    linear_blit.blit_to_rtv(
        &src_1920,
        &dst_2560_rtv,
        0.0,
        0.0,
        sw_dst as f32,
        sh_dst as f32,
    )?;
    let (_, _, blit_2560) = read_bgra_texture(&device, &ctx, &dst_2560)?;

    for (tag, buf) in [("synthesize_to_rtv", &synth_2560), ("linear_blit", &blit_2560)] {
        for (px, py) in [(2350u32, 400u32), (400, 1450), (2350, 1450)] {
            let idx = ((py * sw_dst + px) * 4) as usize;
            if buf[idx] < 200 || buf[idx + 1] < 200 || buf[idx + 2] > 60 {
                return Err(format!(
                    "{tag} clipped/missing outside 1920x1200 at ({px},{py}): BGRA=({},{},{})",
                    buf[idx],
                    buf[idx + 1],
                    buf[idx + 2]
                ));
            }
        }
    }

    // 5f. 9:16-in-16:10 pillarboxed scene cut & edge-isolation check
    let mut pb_a = vec![0u8; (sw_src * sh_src * 4) as usize];
    let mut pb_b = vec![0u8; (sw_src * sh_src * 4) as usize];
    for y in 0..sh_src {
        for x in 623..1297u32 {
            let idx = ((y * sw_src + x) * 4) as usize;
            // Frame A: bright sky-blue/white grid inside 9:16 active strip
            let ca = if ((x / 16) + (y / 16)) & 1 == 0 { 220u8 } else { 70u8 };
            pb_a[idx] = 235;
            pb_a[idx + 1] = ca;
            pb_a[idx + 2] = ca / 2;
            pb_a[idx + 3] = 255;

            // Frame B: completely different scene cut (inverted warm stripes)
            let cb = if ((x / 20) ^ (y / 20)) & 1 == 0 { 245u8 } else { 30u8 };
            pb_b[idx] = 20;
            pb_b[idx + 1] = cb / 2;
            pb_b[idx + 2] = cb;
            pb_b[idx + 3] = 255;
        }
    }
    let (pb_tex_a, _) = create_bgra_texture(&device, sw_src, sh_src, Some(&pb_a), false)?;
    let (pb_tex_b, _) = create_bgra_texture(&device, sw_src, sh_src, Some(&pb_b), false)?;
    let (pb_out, pb_out_rtv) = create_bgra_texture(&device, sw_src, sh_src, None, true)?;
    let pb_out_rtv = pb_out_rtv.unwrap();
    pipe.set_active_uv_spec(compute_active_uv_spec(
        1080,
        1920,
        sw_src,
        sh_src,
        AspectMode::Fit,
    ));
    pipe.prepare_pair(&pb_tex_a, &pb_tex_b, InterpContentMode::Drama)?;
    let pb_dec = pipe.read_decision()?;
    if (pb_dec.flags & FD_SCENE_CUT) == 0 {
        return Err(format!(
            "9:16-in-16:10 pillarboxed scene cut missed: flags={}, changed_frac={:.3}, mad={:.3}",
            pb_dec.flags, pb_dec.changed_frac, pb_dec.mad_mean
        ));
    }
    pipe.synthesize_to_rtv(
        0.5,
        &pb_out_rtv,
        0.0,
        0.0,
        sw_src as f32,
        sh_src as f32,
        InterpContentMode::Drama,
    )?;
    let (_, _, pb_synth) = read_bgra_texture(&device, &ctx, &pb_out)?;
    for px in [120u32, 1800u32] {
        let idx = ((600 * sw_src + px) * 4) as usize;
        if pb_synth[idx] > 4 || pb_synth[idx + 1] > 4 || pb_synth[idx + 2] > 4 {
            return Err(format!(
                "pillarbox margin bleed at x={px}: BGRA=({},{},{})",
                pb_synth[idx], pb_synth[idx + 1], pb_synth[idx + 2]
            ));
        }
    }
    pipe.set_active_uv_spec(VideoActiveUvSpec::default());

    let adapter_desc = inspect_device_adapter(&device)
        .map(|a| format!("{} [{}MB]", a.description, a.dedicated_vram_mb))
        .unwrap_or_else(|| "unknown".to_string());
    let mut enhance = VideoEnhanceEngine::new(device.clone(), ctx.clone())?;
    let vsr_plan = compute_native_mpv_canvas(720, 1280, 2560, 1600, AspectMode::Fit, true);
    if !vsr_plan.needs_upscale {
        return Err("expected 720x1280 -> 2560x1600 to request VSR upscale".into());
    }
    let mut vsr_raw = vec![0u8; (vsr_plan.mpv_w * vsr_plan.mpv_h * 4) as usize];
    for y in vsr_plan.src_active.top..vsr_plan.src_active.bottom {
        for x in vsr_plan.src_active.left..vsr_plan.src_active.right {
            let idx = (((y as u32) * vsr_plan.mpv_w + (x as u32)) * 4) as usize;
            vsr_raw[idx] = 220;
            vsr_raw[idx + 1] = 190;
            vsr_raw[idx + 2] = 40;
            vsr_raw[idx + 3] = 255;
        }
    }
    let (vsr_src, _) =
        create_bgra_texture(&device, vsr_plan.mpv_w, vsr_plan.mpv_h, Some(&vsr_raw), true)?;
    let vsr_uv = compute_active_uv_spec(720, 1280, 2560, 1600, AspectMode::Fit);
    let vsr_out = enhance.process_to_texture(&vsr_src, vsr_plan, 2560, 1600, vsr_uv)?;
    let (vw_out, vh_out, vsr_bytes) = read_bgra_texture(&device, &ctx, &vsr_out)?;
    if (vw_out, vh_out) != (2560, 1600) || !enhance.status.clarity_active {
        return Err(format!(
            "VSR enhance output unexpected: {}x{}, clarity={}",
            vw_out, vh_out, enhance.status.clarity_active
        ));
    }
    let vc_idx = ((800 * vw_out + 1280) * 4) as usize;
    let vm_idx = ((800 * vw_out + 100) * 4) as usize;
    if vsr_bytes[vc_idx] <= 180
        || vsr_bytes[vc_idx + 1] <= 150
        || (vsr_bytes[vm_idx], vsr_bytes[vm_idx + 1], vsr_bytes[vm_idx + 2]) != (0, 0, 0)
    {
        return Err("VSR active/margin pixel check failed".into());
    }

    println!(
        "[SELFTEST] 5/7 GPU 4-level flow + 9:16 pillarbox cut + VSR OK (MAE={flow_mae:.2} vs blend={blend_mae:.2}, 1080x1920={gpu_ms:.2}ms, gpu='{adapter_desc}', hw_vsr={}, clarity={})",
        enhance.status.hw_active, enhance.status.clarity_active
    );

    // 6. Screenshot PNG writer check
    let shot_path = test_dir.join("shot-test.png");
    write_bgra_png(&shot_path, w, h, &interp_bytes)?;
    let png_bytes = fs::read(&shot_path).map_err(|e| format!("read test png: {e}"))?;
    if !png_bytes.starts_with(b"\x89PNG\r\n\x1a\n") || png_bytes.len() < 1000 {
        return Err("invalid PNG header/size in write_bgra_png".into());
    }

    // 7. Hidden offscreen HWND DirectComposition + libmpv D3D11 init test (NEVER calls ShowWindow!)
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map_err(|e| format!("GetModuleHandleW: {e}"))?
        .into();
    let cls_name = w!("TTVSelfTestHiddenClass");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(hidden_wnd_proc),
        hInstance: hinst,
        lpszClassName: cls_name,
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            cls_name,
            w!("ttv-selftest"),
            WS_POPUP,
            0,
            0,
            320,
            240,
            None,
            None,
            Some(hinst),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW hidden: {e}"))?;

    let mut dcomp = DCompVideoHost::new(hwnd, 320, 240)?;
    if !dcomp.uses_dcomp {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err("DirectComposition not active on DCompVideoHost".into());
    }
    dcomp.clear_and_present([0.04, 0.04, 0.05, 1.0], 0);
    dcomp.commit_dcomp();

    // 6b. PiP 264x469 纹理契约：输出纹理必须恒等于 backbuffer。
    // 尺寸不等时 CopyResource 会静默放弃绘制（黑帧），所以两条路都要锁死：
    // 同尺寸走 CopyResource 快路径、不同尺寸必须落到 fallback blit。
    let (pip_w, pip_h) = (264u32, 469u32);
    dcomp.resize_swapchain(pip_w, pip_h)?;
    if (dcomp.swap_w, dcomp.swap_h) != (pip_w, pip_h) {
        unsafe { let _ = DestroyWindow(hwnd); }
        return Err(format!(
            "PiP swapchain resize mismatch: {}x{}",
            dcomp.swap_w, dcomp.swap_h
        ));
    }
    let pip_bb: ID3D11Texture2D = unsafe { dcomp.swap_chain.GetBuffer(0) }
        .map_err(|e| format!("GetBuffer(swapchain, PiP) failed: {e}"))?;
    let mut pip_bb_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { pip_bb.GetDesc(&mut pip_bb_desc) };
    if (pip_bb_desc.Width, pip_bb_desc.Height) != (pip_w, pip_h) {
        unsafe { let _ = DestroyWindow(hwnd); }
        return Err(format!(
            "PiP backbuffer mismatch: {}x{}",
            pip_bb_desc.Width, pip_bb_desc.Height
        ));
    }

    // 同尺寸：mpv 输出纹理 == backbuffer，一一对应 CopyResource 必须真的画出内容。
    // 这里用等尺寸代理目标复刻快路径（不读 swapchain，避免隐藏窗口下 Map 触发设备挂起）。
    let mut pip_src = vec![0u8; (pip_w * pip_h * 4) as usize];
    for px in pip_src.chunks_exact_mut(4) {
        px[0] = 240; // B
        px[1] = 220; // G
        px[2] = 30; // R
        px[3] = 255;
    }
    let (pip_tex, _) = create_bgra_texture(&dcomp.device, pip_w, pip_h, Some(&pip_src), true)?;
    let (pip_dst, pip_dst_rtv) = create_bgra_texture(&dcomp.device, pip_w, pip_h, None, true)?;
    let pip_dst_rtv = pip_dst_rtv.unwrap();
    unsafe {
        dcomp
            .ctx
            .ClearRenderTargetView(&pip_dst_rtv, &[0.02, 0.02, 0.025, 1.0]);
        dcomp.ctx.CopyResource(&pip_dst, &pip_tex);
    }
    let (pip_bw, pip_bh, pip_out) = read_bgra_texture(&dcomp.device, &dcomp.ctx, &pip_dst)?;
    let pip_mid = (((pip_bh / 2) * pip_bw + pip_bw / 2) * 4) as usize;
    if pip_out[pip_mid] < 200 || pip_out[pip_mid + 1] < 180 || pip_out[pip_mid + 2] > 60 {
        unsafe { let _ = DestroyWindow(hwnd); }
        return Err(format!(
            "PiP equal-size copy presented clear instead of video: BGRA=({},{},{})",
            pip_out[pip_mid], pip_out[pip_mid + 1], pip_out[pip_mid + 2]
        ));
    }

    // 6c. 尺寸不一致（mpv canvas 528x938 → backbuffer 264x469）必须落到缩放 blit，
    // 不允许整帧黑；四角采样确认缩放后画面没有被裁掉。
    let (mw, mh) = (pip_w * 2, pip_h * 2);
    let mut m_src = vec![0u8; (mw * mh * 4) as usize];
    for px in m_src.chunks_exact_mut(4) {
        px[0] = 30; // B
        px[1] = 220; // G
        px[2] = 235; // R
        px[3] = 255;
    }
    let (mismatch_tex, _) = create_bgra_texture(&dcomp.device, mw, mh, Some(&m_src), true)?;
    let pip_blit = LinearBlitPass::new(dcomp.device.clone(), dcomp.ctx.clone())?;
    unsafe {
        dcomp
            .ctx
            .ClearRenderTargetView(&pip_dst_rtv, &[0.0, 0.0, 0.0, 1.0]);
    }
    pip_blit.blit_to_rtv(
        &mismatch_tex,
        &pip_dst_rtv,
        0.0,
        0.0,
        pip_w as f32,
        pip_h as f32,
    )?;
    let (_, _, m_out) = read_bgra_texture(&dcomp.device, &dcomp.ctx, &pip_dst)?;
    for (sx, sy) in [
        (4u32, 4u32),
        (pip_w - 5, 4),
        (4, pip_h - 5),
        (pip_w - 5, pip_h - 5),
    ] {
        let idx = ((sy * pip_w + sx) * 4) as usize;
        if m_out[idx] > 80 || m_out[idx + 1] < 150 || m_out[idx + 2] < 150 {
            unsafe { let _ = DestroyWindow(hwnd); }
            return Err(format!(
                "PiP mismatch blit dropped the frame at ({sx},{sy}): BGRA=({},{},{})",
                m_out[idx], m_out[idx + 1], m_out[idx + 2]
            ));
        }
    }
    println!(
        "[SELFTEST] 6b/7 PiP 264x469 swapchain=backbuffer copy + mismatch scale blit OK"
    );

    if find_mpv_dll().is_some() {
        let engine = MpvEngine::new(Some(&dcomp.device), hwnd.0 as isize)?;
        let _live: &MpvLiveState = &engine.state;
        engine.set_paused(true);
        println!("[SELFTEST] 6/7 DComp + libmpv D3D11 render_context OK");
    } else {
        println!("[SELFTEST] 6/7 DComp OK (libmpv-2.dll absent, skipped mpv init)");
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    println!("[SELFTEST] 7/7 PNG writer & clean teardown OK");
    Ok(())
}

unsafe extern "system" fn hidden_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn synth_moving_scene(w: u32, h: u32, cx: i32, cy: i32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let dx = x - cx;
            let dy = y - cy;
            let in_box = dx.abs() <= 28 && dy.abs() <= 28;
            let (b, g, r) = if in_box {
                let checker = (((dx + 28) / 7) + ((dy + 28) / 7)) & 1;
                if checker == 0 {
                    (230u8, 220u8, 245u8)
                } else {
                    (70u8, 130u8, 210u8)
                }
            } else {
                let bg = (20 + (x * 18 / (w as i32).max(1)) + (y * 12 / (h as i32).max(1)))
                    .clamp(16, 60) as u8;
                (bg, bg.saturating_add(4), bg.saturating_add(8))
            };
            let idx = ((y as u32 * w + x as u32) * 4) as usize;
            out[idx] = b;
            out[idx + 1] = g;
            out[idx + 2] = r;
            out[idx + 3] = 255;
        }
    }
    out
}

fn compute_bgra_mae(a: &[u8], b: &[u8]) -> f64 {
    let mut sum = 0u64;
    let mut count = 0u64;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for c in 0..3 {
            sum += u64::from(pa[c].abs_diff(pb[c]));
            count += 1;
        }
    }
    sum as f64 / count.max(1) as f64
}

fn compute_naive_blend_mae(f0: &[u8], f2: &[u8], gt: &[u8]) -> f64 {
    let mut sum = 0u64;
    let mut count = 0u64;
    for ((p0, p2), pgt) in f0
        .chunks_exact(4)
        .zip(f2.chunks_exact(4))
        .zip(gt.chunks_exact(4))
    {
        for c in 0..3 {
            let avg = ((u16::from(p0[c]) + u16::from(p2[c])) / 2) as u8;
            sum += u64::from(avg.abs_diff(pgt[c]));
            count += 1;
        }
    }
    sum as f64 / count.max(1) as f64
}
