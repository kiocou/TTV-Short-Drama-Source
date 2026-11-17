use std::mem::ManuallyDrop;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_CPU_ACCESS_WRITE, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIDEO_COLOR,
    D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VIEWPORT, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use crate::interp::{
    compute_active_uv_spec, create_sampler_state, create_solid_no_scissor_rs,
    prepare_fullscreen_draw_state, D3DCompilerDll, VideoActiveUvSpec,
};
use crate::prefs::AspectMode;

pub const NVIDIA_VENDOR_ID: u32 = 0x10DE;

/// NVIDIA RTX Video Super Resolution stream extension GUID used with
/// `ID3D11VideoContext::VideoProcessorSetStreamExtension`.
pub const NVIDIA_VSR_STREAM_GUID: GUID = GUID::from_values(
    0xD43CE1B3,
    0x1F4B,
    0x48AC,
    [0xBA, 0xEE, 0xC3, 0xC2, 0x53, 0x75, 0xE6, 0xF7],
);

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvVsrStreamExt {
    pub version: u32,
    pub method: u32,
    pub enable: u32,
}

impl NvVsrStreamExt {
    pub const fn enabled() -> Self {
        Self {
            version: 1,
            method: 2,
            enable: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DxgiAdapterInfo {
    #[allow(dead_code)]
    pub vendor_id: u32,
    #[allow(dead_code)]
    pub device_id: u32,
    #[allow(dead_code)]
    pub dedicated_vram_mb: u64,
    #[allow(dead_code)]
    pub description: String,
    pub is_nvidia: bool,
}

pub fn inspect_device_adapter(device: &ID3D11Device) -> Option<DxgiAdapterInfo> {
    let dxgi_dev: IDXGIDevice = device.cast().ok()?;
    let adapter = unsafe { dxgi_dev.GetAdapter().ok()? };
    let adapter1: IDXGIAdapter1 = adapter.cast().ok()?;
    let desc = unsafe { adapter1.GetDesc1().ok()? };
    let end = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    let description = String::from_utf16_lossy(&desc.Description[..end]);
    Some(DxgiAdapterInfo {
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        dedicated_vram_mb: (desc.DedicatedVideoMemory as u64) / (1024 * 1024),
        description,
        is_nvidia: desc.VendorId == NVIDIA_VENDOR_ID,
    })
}

/// Enumerates DXGI adapters and selects the highest-priority discrete GPU,
/// strongly preferring NVIDIA (`0x10DE`) on Optimus laptops so D3D11 optical-flow
/// compute shaders and RTX VSR run on the dedicated GPU rather than an iGPU.
pub fn pick_preferred_hardware_adapter() -> Option<IDXGIAdapter1> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    let mut best_nvidia: Option<(usize, IDXGIAdapter1)> = None;
    let mut best_any: Option<(usize, IDXGIAdapter1)> = None;
    for idx in 0..16u32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(idx) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
            continue;
        }
        let vram = desc.DedicatedVideoMemory;
        if desc.VendorId == NVIDIA_VENDOR_ID {
            let replace = best_nvidia
                .as_ref()
                .is_none_or(|(best_vram, _)| vram > *best_vram);
            if replace {
                best_nvidia = Some((vram, adapter));
            }
        } else {
            let replace = best_any
                .as_ref()
                .is_none_or(|(best_vram, _)| vram > *best_vram);
            if replace {
                best_any = Some((vram, adapter));
            }
        }
    }
    best_nvidia.or(best_any).map(|(_, a)| a)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeCanvasPlan {
    pub mpv_w: u32,
    pub mpv_h: u32,
    pub src_active: RECT,
    pub dst_active: RECT,
    pub needs_upscale: bool,
}

fn rect_from_uv(uv: VideoActiveUvSpec, w: u32, h: u32) -> RECT {
    let left = (uv.uv_min[0] * w as f32).round().clamp(0.0, (w.saturating_sub(2)) as f32) as i32;
    let top = (uv.uv_min[1] * h as f32).round().clamp(0.0, (h.saturating_sub(2)) as f32) as i32;
    let right = (uv.uv_max[0] * w as f32)
        .round()
        .clamp((left + 2) as f32, w as f32) as i32;
    let bottom = (uv.uv_max[1] * h as f32)
        .round()
        .clamp((top + 2) as f32, h as f32) as i32;
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

/// Plans native `mpv` rasterization canvas dimensions so RTX VSR receives an
/// un-upscaled video image whenever source resolution is smaller than target
/// display active region.
pub fn compute_native_mpv_canvas(
    src_w: u32,
    src_h: u32,
    target_w: u32,
    target_h: u32,
    aspect: AspectMode,
    vsr_enabled: bool,
) -> NativeCanvasPlan {
    let tw = target_w.max(64);
    let th = target_h.max(64);
    let dst_uv = compute_active_uv_spec(src_w, src_h, tw, th, aspect);
    let dst_active = rect_from_uv(dst_uv, tw, th);
    let active_w = (dst_active.right - dst_active.left).max(16) as u32;
    let active_h = (dst_active.bottom - dst_active.top).max(16) as u32;

    if vsr_enabled && src_w >= 64 && src_h >= 64 && (src_w + 16 < active_w || src_h + 16 < active_h)
    {
        let scale_x = f64::from(active_w) / f64::from(src_w);
        let scale_y = f64::from(active_h) / f64::from(src_h);
        let up = scale_x.min(scale_y);
        if up > 1.04 {
            let mut mpv_w = ((f64::from(tw) / up).round() as u32).max(128) & !1;
            let mut mpv_h = ((f64::from(th) / up).round() as u32).max(128) & !1;
            mpv_w = mpv_w.min(tw.saturating_sub(2).max(64));
            mpv_h = mpv_h.min(th.saturating_sub(2).max(64));
            if mpv_w < tw && mpv_h < th {
                let src_uv = compute_active_uv_spec(src_w, src_h, mpv_w, mpv_h, aspect);
                let src_active = rect_from_uv(src_uv, mpv_w, mpv_h);
                return NativeCanvasPlan {
                    mpv_w,
                    mpv_h,
                    src_active,
                    dst_active,
                    needs_upscale: true,
                };
            }
        }
    }

    NativeCanvasPlan {
        mpv_w: tw,
        mpv_h: th,
        src_active: dst_active,
        dst_active,
        needs_upscale: false,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VsrRuntimeStatus {
    pub hw_ext_supported: bool,
    pub hw_active: bool,
    pub clarity_active: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EnhanceCBData {
    src_texel: [f32; 2],
    dst_size: [f32; 2],
    active_uv_min: [f32; 2],
    active_uv_max: [f32; 2],
    sharpen_strength: f32,
    deblock_strength: f32,
    _pad0: f32,
    _pad1: f32,
}

const SHADER_ENHANCE_CLARITY: &str = r#"
cbuffer EnhanceCB : register(b0)
{
    float2 srcTexel;
    float2 dstSize;
    float2 activeUvMin;
    float2 activeUvMax;
    float  sharpenStrength;
    float  deblockStrength;
    float  _pad0;
    float  _pad1;
};

Texture2D<float4> srcTex : register(t0);
SamplerState sampLinear : register(s0);
SamplerState sampPoint  : register(s1);

struct VSOut
{
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

VSOut VSEnhance(uint id : SV_VertexID)
{
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float2 ClampActiveUv(float2 uv)
{
    float2 margin = srcTexel * 0.5;
    float2 lo = min(activeUvMin + margin, activeUvMax);
    float2 hi = max(activeUvMax - margin, lo);
    return clamp(uv, lo, hi);
}

bool OutsideActive(float2 uv)
{
    return uv.x < activeUvMin.x || uv.x > activeUvMax.x ||
           uv.y < activeUvMin.y || uv.y > activeUvMax.y;
}

float Luma709(float3 c)
{
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

float3 SampleCatmullRomActive(float2 uv)
{
    float2 texSize = 1.0 / max(srcTexel, 1e-6);
    float2 samplePos = uv * texSize;
    float2 tc = floor(samplePos - 0.5) + 0.5;
    float2 f = samplePos - tc;
    float2 f2 = f * f;
    float2 f3 = f2 * f;

    float2 w0 = f2 - 0.5 * (f3 + f);
    float2 w1 = 1.5 * f3 - 2.5 * f2 + 1.0;
    float2 w3 = 0.5 * (f3 - f2);
    float2 w2 = 1.0 - w0 - w1 - w3;

    float2 w12 = w1 + w2;
    float2 tc0  = (tc - 1.0) * srcTexel;
    float2 tc3  = (tc + 2.0) * srcTexel;
    float2 tc12 = (tc + w2 / max(w12, 1e-5)) * srcTexel;

    float3 sum =
        srcTex.SampleLevel(sampLinear, ClampActiveUv(float2(tc12.x, tc0.y)),  0).rgb * (w12.x * w0.y) +
        srcTex.SampleLevel(sampLinear, ClampActiveUv(float2(tc0.x,  tc12.y)), 0).rgb * (w0.x  * w12.y) +
        srcTex.SampleLevel(sampLinear, ClampActiveUv(float2(tc12.x, tc12.y)), 0).rgb * (w12.x * w12.y) +
        srcTex.SampleLevel(sampLinear, ClampActiveUv(float2(tc3.x,  tc12.y)), 0).rgb * (w3.x  * w12.y) +
        srcTex.SampleLevel(sampLinear, ClampActiveUv(float2(tc12.x, tc3.y)),  0).rgb * (w12.x * w3.y);

    float norm = (w12.x * w0.y) + (w0.x * w12.y) + (w12.x * w12.y) + (w3.x * w12.y) + (w12.x * w3.y);
    float3 res = sum / max(norm, 1e-5);
    float3 cLin = srcTex.SampleLevel(sampLinear, ClampActiveUv(uv), 0).rgb;
    float3 cMin = min(
        cLin,
        min(
            min(srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2(-srcTexel.x, 0.0)), 0).rgb,
                srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2( srcTexel.x, 0.0)), 0).rgb),
            min(srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2(0.0, -srcTexel.y)), 0).rgb,
                srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2(0.0,  srcTexel.y)), 0).rgb)
        )
    );
    float3 cMax = max(
        cLin,
        max(
            max(srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2(-srcTexel.x, 0.0)), 0).rgb,
                srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2( srcTexel.x, 0.0)), 0).rgb),
            max(srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2(0.0, -srcTexel.y)), 0).rgb,
                srcTex.SampleLevel(sampLinear, ClampActiveUv(uv + float2(0.0,  srcTexel.y)), 0).rgb)
        )
    );
    return clamp(res, cMin - 0.03, cMax + 0.03);
}

float4 PSEnhance(VSOut i) : SV_Target0
{
    if (OutsideActive(i.uv))
    {
        return float4(0.0, 0.0, 0.0, 1.0);
    }

    float2 uvC = ClampActiveUv(i.uv);
    float scaleDiff = abs(dstSize.x * srcTexel.x - 1.0) + abs(dstSize.y * srcTexel.y - 1.0);
    float3 c = (scaleDiff < 0.002)
        ? srcTex.SampleLevel(sampPoint, uvC, 0).rgb
        : SampleCatmullRomActive(uvC);

    if (deblockStrength <= 0.001 && sharpenStrength <= 0.001)
    {
        return float4(saturate(c), 1.0);
    }

    float2 stepUv = (scaleDiff < 0.002) ? srcTexel : (1.0 / max(dstSize, 1.0));
    float3 n = srcTex.SampleLevel(sampLinear, ClampActiveUv(uvC + float2(0.0, -stepUv.y)), 0).rgb;
    float3 s = srcTex.SampleLevel(sampLinear, ClampActiveUv(uvC + float2(0.0,  stepUv.y)), 0).rgb;
    float3 w = srcTex.SampleLevel(sampLinear, ClampActiveUv(uvC + float2(-stepUv.x, 0.0)), 0).rgb;
    float3 e = srcTex.SampleLevel(sampLinear, ClampActiveUv(uvC + float2( stepUv.x, 0.0)), 0).rgb;

    float lC = Luma709(c);
    float lN = Luma709(n);
    float lS = Luma709(s);
    float lW = Luma709(w);
    float lE = Luma709(e);

    if (deblockStrength > 0.001)
    {
        float maxDiff = max(max(abs(lN - lC), abs(lS - lC)), max(abs(lW - lC), abs(lE - lC)));
        float smoothMask = exp(-(maxDiff * maxDiff) * 900.0) * smoothstep(0.0025, 0.012, maxDiff);
        float3 avg4 = 0.25 * (n + s + w + e);
        c = lerp(c, avg4, saturate(deblockStrength * smoothMask));
        lC = Luma709(c);
    }

    if (sharpenStrength > 0.001)
    {
        float lMn = min(lC, min(min(lN, lS), min(lW, lE)));
        float lMx = max(lC, max(max(lN, lS), max(lW, lE)));
        float3 cMn = min(c, min(min(n, s), min(w, e)));
        float3 cMx = max(c, max(max(n, s), max(w, e)));
        float lAvg = 0.25 * (lN + lS + lW + lE);

        // Dampen amplification on ultra-high-contrast text/outlines; protect dark/bright clipping
        float edgeAtten = saturate(1.0 - max(lMx - lMn - 0.14, 0.0) * 1.9);
        float rcasAmp = saturate(min(lMn, 1.0 - lMx) / max(lMx, 1e-4));
        float lDelta = (lC - lAvg) * sharpenStrength * (0.68 + 0.52 * rcasAmp) * edgeAtten;
        c = clamp(c + lDelta, cMn, cMx);
    }

    return float4(saturate(c), 1.0);
}
"#;

struct NvVsrProcessor {
    video_dev: ID3D11VideoDevice,
    video_ctx: ID3D11VideoContext,
    vp_enum: Option<ID3D11VideoProcessorEnumerator>,
    vp: Option<ID3D11VideoProcessor>,
    cached_dims: (u32, u32, u32, u32),
    ext_accepted: Option<bool>,
}

impl NvVsrProcessor {
    fn try_new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Option<Self> {
        let info = inspect_device_adapter(device)?;
        if !info.is_nvidia {
            return None;
        }
        let video_dev: ID3D11VideoDevice = device.cast().ok()?;
        let video_ctx: ID3D11VideoContext = ctx.cast().ok()?;
        Some(Self {
            video_dev,
            video_ctx,
            vp_enum: None,
            vp: None,
            cached_dims: (0, 0, 0, 0),
            ext_accepted: None,
        })
    }

    fn ensure_processor(&mut self, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<bool, String> {
        if self.cached_dims == (in_w, in_h, out_w, out_h) && self.vp.is_some() && self.vp_enum.is_some() {
            return Ok(self.ext_accepted.unwrap_or(false));
        }
        self.vp = None;
        self.vp_enum = None;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let vp_enum = unsafe { self.video_dev.CreateVideoProcessorEnumerator(&desc) }
            .map_err(|e| format!("CreateVideoProcessorEnumerator failed: {e}"))?;
        let vp = unsafe { self.video_dev.CreateVideoProcessor(&vp_enum, 0) }
            .map_err(|e| format!("CreateVideoProcessor failed: {e}"))?;

        let ext = NvVsrStreamExt::enabled();
        let hr = unsafe {
            self.video_ctx
                .VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            self.video_ctx
                .VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            self.video_ctx.VideoProcessorSetStreamExtension(
                &vp,
                0,
                &NVIDIA_VSR_STREAM_GUID,
                std::mem::size_of::<NvVsrStreamExt>() as u32,
                (&ext as *const NvVsrStreamExt).cast(),
            )
        };
        let accepted = hr >= 0;
        self.ext_accepted = Some(accepted);
        self.vp_enum = Some(vp_enum);
        self.vp = Some(vp);
        self.cached_dims = (in_w, in_h, out_w, out_h);
        Ok(accepted)
    }

    fn upscale(
        &mut self,
        src_tex: &ID3D11Texture2D,
        dst_tex: &ID3D11Texture2D,
        (in_w, in_h): (u32, u32),
        src_active: RECT,
        dst_active: RECT,
        (out_w, out_h): (u32, u32),
    ) -> Result<bool, String> {
        let accepted = self.ensure_processor(in_w, in_h, out_w, out_h)?;
        if !accepted {
            return Ok(false);
        }
        let vp_enum = self.vp_enum.as_ref().ok_or("missing vp_enum")?;
        let vp = self.vp.as_ref().ok_or("missing vp")?;

        let src_res: ID3D11Resource = src_tex.cast().map_err(|e| format!("{e}"))?;
        let dst_res: ID3D11Resource = dst_tex.cast().map_err(|e| format!("{e}"))?;

        let mut in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };

        let mut in_view = None;
        unsafe {
            self.video_dev
                .CreateVideoProcessorInputView(&src_res, vp_enum, &mut in_desc as *mut _, Some(&mut in_view))
        }
        .map_err(|e| format!("CreateVideoProcessorInputView failed: {e}"))?;

        let mut out_view = None;
        unsafe {
            self.video_dev
                .CreateVideoProcessorOutputView(&dst_res, vp_enum, &mut out_desc as *mut _, Some(&mut out_view))
        }
        .map_err(|e| format!("CreateVideoProcessorOutputView failed: {e}"))?;

        let out_view = out_view.ok_or("CreateVideoProcessorOutputView returned None")?;
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(in_view),
            ..Default::default()
        };

        let blt_res = unsafe {
            self.video_ctx
                .VideoProcessorSetOutputBackgroundColor(vp, false, &black);
            self.video_ctx
                .VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src_active));
            self.video_ctx
                .VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst_active));
            self.video_ctx
                .VideoProcessorSetOutputTargetRect(vp, true, Some(&dst_active));
            self.video_ctx
                .VideoProcessorBlt(vp, &out_view, 0, std::slice::from_ref(&stream))
        };
        unsafe {
            ManuallyDrop::drop(&mut stream.pInputSurface);
        }
        blt_res.map_err(|e| format!("VideoProcessorBlt failed: {e}"))?;
        Ok(true)
    }
}

#[derive(Clone, Copy)]
pub struct ClarityConfig {
    pub active_uv: VideoActiveUvSpec,
    pub dst_origin: (f32, f32),
    pub sharpen: f32,
    pub deblock: f32,
}

pub struct EnhanceClarityPass {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    samp_linear: ID3D11SamplerState,
    samp_point: ID3D11SamplerState,
    rs_solid_no_scissor: ID3D11RasterizerState,
}

impl EnhanceClarityPass {
    pub fn new(device: ID3D11Device, ctx: ID3D11DeviceContext) -> Result<Self, String> {
        let compiler = D3DCompilerDll::load()?;
        let vs_blob =
            compiler.compile_hlsl(SHADER_ENHANCE_CLARITY, "EnhanceClarity.hlsl", "VSEnhance", "vs_5_0")?;
        let ps_blob =
            compiler.compile_hlsl(SHADER_ENHANCE_CLARITY, "EnhanceClarity.hlsl", "PSEnhance", "ps_5_0")?;

        let mut vs = None;
        unsafe { device.CreateVertexShader(&vs_blob, None, Some(&mut vs)) }
            .map_err(|e| format!("CreateVertexShader(Enhance) failed: {e}"))?;
        let mut ps = None;
        unsafe { device.CreatePixelShader(&ps_blob, None, Some(&mut ps)) }
            .map_err(|e| format!("CreatePixelShader(Enhance) failed: {e}"))?;

        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<EnhanceCBData>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cb = None;
        unsafe { device.CreateBuffer(&cb_desc, None, Some(&mut cb)) }
            .map_err(|e| format!("CreateBuffer(EnhanceCB) failed: {e}"))?;

        Ok(Self {
            samp_linear: create_sampler_state(&device, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
            samp_point: create_sampler_state(&device, D3D11_FILTER_MIN_MAG_MIP_POINT)?,
            rs_solid_no_scissor: create_solid_no_scissor_rs(&device)?,
            device,
            ctx,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            cb: cb.unwrap(),
        })
    }

    pub fn render_to_rtv(
        &self,
        src: &ID3D11Texture2D,
        src_dims: (u32, u32),
        target_rtv: &ID3D11RenderTargetView,
        dst_dims: (u32, u32),
        cfg: ClarityConfig,
    ) -> Result<(), String> {
        let mut srv = None;
        unsafe {
            self.ctx.OMSetRenderTargets(None, None);
            self.device
                .CreateShaderResourceView(src, None, Some(&mut srv))
        }
        .map_err(|e| format!("CreateSRV(Enhance) failed: {e}"))?;
        let srv = srv.unwrap();

        let cb_data = EnhanceCBData {
            src_texel: [1.0 / (src_dims.0.max(1) as f32), 1.0 / (src_dims.1.max(1) as f32)],
            dst_size: [dst_dims.0.max(1) as f32, dst_dims.1.max(1) as f32],
            active_uv_min: cfg.active_uv.uv_min,
            active_uv_max: cfg.active_uv.uv_max,
            sharpen_strength: cfg.sharpen.clamp(0.0, 1.0),
            deblock_strength: cfg.deblock.clamp(0.0, 0.5),
            _pad0: 0.0,
            _pad1: 0.0,
        };
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(|e| format!("Map EnhanceCB failed: {e}"))?;
            std::ptr::copy_nonoverlapping(&cb_data, mapped.pData as *mut EnhanceCBData, 1);
            self.ctx.Unmap(&self.cb, 0);
        }

        let vp = D3D11_VIEWPORT {
            TopLeftX: cfg.dst_origin.0,
            TopLeftY: cfg.dst_origin.1,
            Width: dst_dims.0.max(1) as f32,
            Height: dst_dims.1.max(1) as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            self.ctx.IASetInputLayout(None);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.ctx.PSSetSamplers(
                0,
                Some(&[Some(self.samp_linear.clone()), Some(self.samp_point.clone())]),
            );
            self.ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
            prepare_fullscreen_draw_state(&self.ctx, &self.rs_solid_no_scissor, vp, target_rtv);
            self.ctx.Draw(3, 0);
            self.ctx.OMSetRenderTargets(None, None);
            let clear_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            self.ctx.PSSetShaderResources(0, Some(&clear_srv));
        }
        Ok(())
    }
}

fn create_rtv_tex(device: &ID3D11Device, w: u32, h: u32) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), String> {
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
        .map_err(|e| format!("CreateTexture2D enhance({w}x{h}) failed: {e}"))?;
    let tex = tex.unwrap();
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }
        .map_err(|e| format!("CreateRTV enhance({w}x{h}) failed: {e}"))?;
    Ok((tex, rtv.unwrap()))
}

pub struct VideoEnhanceEngine {
    device: ID3D11Device,
    nv_vsr: Option<NvVsrProcessor>,
    clarity: EnhanceClarityPass,
    vsr_mid_tex: Option<ID3D11Texture2D>,
    vsr_mid_dims: (u32, u32),
    enhanced_tex: Option<ID3D11Texture2D>,
    enhanced_rtv: Option<ID3D11RenderTargetView>,
    enhanced_dims: (u32, u32),
    pub status: VsrRuntimeStatus,
}

impl VideoEnhanceEngine {
    pub fn new(device: ID3D11Device, ctx: ID3D11DeviceContext) -> Result<Self, String> {
        let nv_vsr = NvVsrProcessor::try_new(&device, &ctx);
        let clarity = EnhanceClarityPass::new(device.clone(), ctx)?;
        Ok(Self {
            device,
            nv_vsr,
            clarity,
            vsr_mid_tex: None,
            vsr_mid_dims: (0, 0),
            enhanced_tex: None,
            enhanced_rtv: None,
            enhanced_dims: (0, 0),
            status: VsrRuntimeStatus::default(),
        })
    }

    fn ensure_vsr_mid(&mut self, w: u32, h: u32) -> Result<&ID3D11Texture2D, String> {
        if self.vsr_mid_dims != (w, h) || self.vsr_mid_tex.is_none() {
            let (tex, _) = create_rtv_tex(&self.device, w, h)?;
            self.vsr_mid_tex = Some(tex);
            self.vsr_mid_dims = (w, h);
        }
        self.vsr_mid_tex.as_ref().ok_or_else(|| "missing vsr_mid_tex".into())
    }

    fn ensure_enhanced_tex(&mut self, w: u32, h: u32) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), String> {
        if self.enhanced_dims != (w, h) || self.enhanced_tex.is_none() || self.enhanced_rtv.is_none() {
            let (tex, rtv) = create_rtv_tex(&self.device, w, h)?;
            self.enhanced_tex = Some(tex);
            self.enhanced_rtv = Some(rtv);
            self.enhanced_dims = (w, h);
        }
        Ok((
            self.enhanced_tex.clone().unwrap(),
            self.enhanced_rtv.clone().unwrap(),
        ))
    }

    fn run_hw_vsr_or_passthrough(
        &mut self,
        src_tex: &ID3D11Texture2D,
        plan: NativeCanvasPlan,
        out_w: u32,
        out_h: u32,
        active_uv: VideoActiveUvSpec,
    ) -> Result<(ID3D11Texture2D, (u32, u32), bool), String> {
        let dst_active = rect_from_uv(active_uv, out_w, out_h);
        let active_ok =
            dst_active.right - dst_active.left >= 16 && dst_active.bottom - dst_active.top >= 16;
        if plan.mpv_w >= 64 && plan.mpv_h >= 64 && active_ok {
            let _ = self.ensure_vsr_mid(out_w, out_h)?;
            if let (Some(nv), Some(mid_tex)) = (self.nv_vsr.as_mut(), self.vsr_mid_tex.as_ref()) {
                let hw_ok = nv
                    .upscale(
                        src_tex,
                        mid_tex,
                        (plan.mpv_w, plan.mpv_h),
                        plan.src_active,
                        dst_active,
                        (out_w, out_h),
                    )
                    .unwrap_or(false);
                if let Some(supported) = nv.ext_accepted {
                    self.status.hw_ext_supported = supported;
                }
                if hw_ok {
                    return Ok((mid_tex.clone(), (out_w, out_h), true));
                }
            }
        }
        Ok((src_tex.clone(), (plan.mpv_w, plan.mpv_h), false))
    }

    /// Enhances a decoded frame (RTX VSR hardware processing when `vsr_enabled` and
    /// NVIDIA VideoProcessor accepts VSR + GPU RCAS/Deblock clarity pass) into an
    /// owned texture `(out_w, out_h)` suitable for downstream optical flow or sampling.
    pub fn process_to_texture(
        &mut self,
        src_tex: &ID3D11Texture2D,
        plan: NativeCanvasPlan,
        out_w: u32,
        out_h: u32,
        active_uv: VideoActiveUvSpec,
    ) -> Result<ID3D11Texture2D, String> {
        self.process_to_texture_with_vsr(src_tex, plan, out_w, out_h, active_uv, true)
    }

    /// Same as [`process_to_texture`], with explicit `vsr_enabled` control so scaling-only
    /// transition frames bypass VSR when disabled.
    pub fn process_to_texture_with_vsr(
        &mut self,
        src_tex: &ID3D11Texture2D,
        plan: NativeCanvasPlan,
        out_w: u32,
        out_h: u32,
        active_uv: VideoActiveUvSpec,
        vsr_enabled: bool,
    ) -> Result<ID3D11Texture2D, String> {
        let (out_tex, out_rtv) = self.ensure_enhanced_tex(out_w, out_h)?;
        if !vsr_enabled {
            self.status.hw_active = false;
            self.status.clarity_active = false;
            self.clarity.render_to_rtv(
                src_tex,
                (plan.mpv_w, plan.mpv_h),
                &out_rtv,
                (out_w, out_h),
                ClarityConfig {
                    active_uv,
                    dst_origin: (0.0, 0.0),
                    sharpen: 0.0,
                    deblock: 0.0,
                },
            )?;
            return Ok(out_tex);
        }
        let (clarity_src, src_dims, did_hw_vsr) =
            self.run_hw_vsr_or_passthrough(src_tex, plan, out_w, out_h, active_uv)?;
        let (sharpen, deblock) = if did_hw_vsr { (0.24, 0.08) } else { (0.46, 0.14) };
        self.clarity.render_to_rtv(
            &clarity_src,
            src_dims,
            &out_rtv,
            (out_w, out_h),
            ClarityConfig {
                active_uv,
                dst_origin: (0.0, 0.0),
                sharpen,
                deblock,
            },
        )?;
        self.status.hw_active = did_hw_vsr;
        self.status.clarity_active = true;
        Ok(out_tex)
    }

    /// Runs VSR + GPU RCAS/Deblock clarity (or crisp Catmull-Rom scale-only when
    /// `vsr_enabled == false`) directly into the target swapchain RTV at `dst_origin`.
    #[allow(clippy::too_many_arguments)]
    pub fn render_to_target_rtv_at(
        &mut self,
        src_tex: &ID3D11Texture2D,
        plan: NativeCanvasPlan,
        dst_origin: (f32, f32),
        dst_dims: (u32, u32),
        target_rtv: &ID3D11RenderTargetView,
        active_uv: VideoActiveUvSpec,
        vsr_enabled: bool,
    ) -> Result<(), String> {
        if !vsr_enabled {
            self.status.hw_active = false;
            self.status.clarity_active = false;
            return self.clarity.render_to_rtv(
                src_tex,
                (plan.mpv_w, plan.mpv_h),
                target_rtv,
                dst_dims,
                ClarityConfig {
                    active_uv,
                    dst_origin,
                    sharpen: 0.0,
                    deblock: 0.0,
                },
            );
        }

        let (clarity_src, src_dims, did_hw_vsr) =
            self.run_hw_vsr_or_passthrough(src_tex, plan, dst_dims.0, dst_dims.1, active_uv)?;
        let (sharpen, deblock) = if did_hw_vsr { (0.28, 0.10) } else { (0.54, 0.18) };
        self.clarity.render_to_rtv(
            &clarity_src,
            src_dims,
            target_rtv,
            dst_dims,
            ClarityConfig {
                active_uv,
                dst_origin,
                sharpen,
                deblock,
            },
        )?;
        self.status.hw_active = did_hw_vsr;
        self.status.clarity_active = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcomp::create_d3d11_device;
    use crate::interp::{create_bgra_texture, read_bgra_texture};

    #[test]
    fn nvidia_vsr_extension_struct_is_12_bytes() {
        assert_eq!(std::mem::size_of::<NvVsrStreamExt>(), 12);
        let ext = NvVsrStreamExt::enabled();
        assert_eq!((ext.version, ext.method, ext.enable), (1, 2, 1));
    }

    #[test]
    fn native_canvas_plan_avoids_preupscale_when_vsr_enabled() {
        // 720x1280 portrait inside 2560x1600 Fit -> active is 900x1600 (1.25x),
        // so mpv canvas shrinks to 2048x1280 (active 720x1280 = 1:1 source pixels!).
        let plan = compute_native_mpv_canvas(720, 1280, 2560, 1600, AspectMode::Fit, true);
        assert!(plan.needs_upscale);
        assert_eq!((plan.mpv_w, plan.mpv_h), (2048, 1280));
        assert_eq!(plan.src_active.bottom - plan.src_active.top, 1280);
        assert_eq!(plan.dst_active.bottom - plan.dst_active.top, 1600);

        // 1080x1920 portrait inside 1920x1200 Fit -> active is 675x1200 (< source),
        // so no upscaling is needed.
        let down = compute_native_mpv_canvas(1080, 1920, 1920, 1200, AspectMode::Fit, true);
        assert!(!down.needs_upscale);
        assert_eq!((down.mpv_w, down.mpv_h), (1920, 1200));

        // When VSR is disabled, canvas remains target resolution directly.
        let off = compute_native_mpv_canvas(720, 1280, 2560, 1600, AspectMode::Fit, false);
        assert!(!off.needs_upscale);
        assert_eq!((off.mpv_w, off.mpv_h), (2560, 1600));
    }

    #[test]
    fn enhance_clarity_sharpens_edges_without_ringing_clip() {
        let (device, ctx) = create_d3d11_device().expect("D3D11 device");
        let mut engine = VideoEnhanceEngine::new(device.clone(), ctx.clone()).expect("enhance engine");
        let (w, h) = (128u32, 128u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let idx = ((y * w + x) * 4) as usize;
                // Soft ramp edge at x=64 so contrast sharpening boosts transition safely
                let val = if x < 62 {
                    60u8
                } else if x > 66 {
                    200u8
                } else {
                    match x {
                        62 => 75,
                        63 => 100,
                        64 => 130,
                        65 => 165,
                        _ => 185,
                    }
                };
                src[idx] = val;
                src[idx + 1] = val;
                src[idx + 2] = val;
                src[idx + 3] = 255;
            }
        }
        let (src_tex, _) = create_bgra_texture(&device, w, h, Some(&src), false).unwrap();
        let (dst_tex, dst_rtv) = create_bgra_texture(&device, w, h, None, true).unwrap();
        let plan = compute_native_mpv_canvas(w, h, w, h, AspectMode::Stretch, true);
        engine
            .render_to_target_rtv_at(
                &src_tex,
                plan,
                (0.0, 0.0),
                (w, h),
                dst_rtv.as_ref().unwrap(),
                VideoActiveUvSpec::default(),
                true,
            )
            .unwrap();
        let (_, _, out) = read_bgra_texture(&device, &ctx, &dst_tex).unwrap();
        assert!(engine.status.clarity_active);
        // Dark side should never overshoot below 60; bright side never above 200 (zero halo!).
        for x in 40..90u32 {
            let v = out[((64 * w + x) * 4) as usize];
            assert!((59..=201).contains(&v), "ringing halo detected at x={x}: {v}");
        }
        let left_edge = out[((64 * w + 63) * 4) as usize];
        let right_edge = out[((64 * w + 65) * 4) as usize];
        assert!(
            left_edge <= 100 && right_edge >= 165,
            "expected crisper edge slope: left={left_edge}, right={right_edge}"
        );
    }

    #[test]
    fn preferred_device_uses_discrete_gpu_and_upscale_pipeline_runs_cleanly() {
        let (device, ctx) = create_d3d11_device().expect("D3D11 device");
        let info = inspect_device_adapter(&device).expect("adapter info");
        eprintln!(
            "[enhance test] adapter='{}' vendor=0x{:04X} vram={}MB",
            info.description, info.vendor_id, info.dedicated_vram_mb
        );
        let mut engine = VideoEnhanceEngine::new(device.clone(), ctx.clone()).expect("engine");
        let plan = compute_native_mpv_canvas(720, 1280, 2560, 1600, AspectMode::Fit, true);
        assert!(plan.needs_upscale);
        // Synthesize a portrait texture in mpv_w x mpv_h (2048x1280) with cyan active strip
        let mut raw = vec![0u8; (plan.mpv_w * plan.mpv_h * 4) as usize];
        for y in plan.src_active.top..plan.src_active.bottom {
            for x in plan.src_active.left..plan.src_active.right {
                let idx = (((y as u32) * plan.mpv_w + (x as u32)) * 4) as usize;
                raw[idx] = 220;
                raw[idx + 1] = 190;
                raw[idx + 2] = 40;
                raw[idx + 3] = 255;
            }
        }
        let (src_tex, _) =
            create_bgra_texture(&device, plan.mpv_w, plan.mpv_h, Some(&raw), true).unwrap();
        let active_uv = compute_active_uv_spec(720, 1280, 2560, 1600, AspectMode::Fit);
        let out_tex = engine
            .process_to_texture(&src_tex, plan, 2560, 1600, active_uv)
            .expect("process_to_texture");
        eprintln!(
            "[enhance test] hw_ext_supported={} hw_active={} clarity_active={}",
            engine.status.hw_ext_supported, engine.status.hw_active, engine.status.clarity_active
        );
        let (ow, oh, out) = read_bgra_texture(&device, &ctx, &out_tex).unwrap();
        assert_eq!((ow, oh), (2560, 1600));
        // Center of enhanced 2560x1600 frame should still hold cyan-blue signal; corners black.
        let center_idx = ((800 * ow + 1280) * 4) as usize;
        assert!(out[center_idx] > 180 && out[center_idx + 1] > 150);
        let corner_idx = ((400 * ow + 100) * 4) as usize;
        assert_eq!((out[corner_idx], out[corner_idx + 1], out[corner_idx + 2]), (0, 0, 0));
    }

    #[test]
    fn vsr_engages_for_native_1080p_and_fg_capped_720p_without_upscale() {
        let (device, ctx) = create_d3d11_device().expect("D3D11 device");
        let mut engine = VideoEnhanceEngine::new(device.clone(), ctx).expect("engine");
        if !inspect_device_adapter(&device).is_some_and(|a| a.is_nvidia) {
            return;
        }
        // Case 1: 1080x1920 vertical drama on 2560x1600 display (900x1600 active <= 1080x1920)
        let plan_1080 = compute_native_mpv_canvas(1080, 1920, 2560, 1600, AspectMode::Fit, true);
        assert!(!plan_1080.needs_upscale);
        let (src_1080, _) =
            create_bgra_texture(&device, plan_1080.mpv_w, plan_1080.mpv_h, None, true).unwrap();
        let (_, rtv_1080) = create_bgra_texture(&device, 2560, 1600, None, true).unwrap();
        let uv_1080 = compute_active_uv_spec(1080, 1920, 2560, 1600, AspectMode::Fit);
        engine
            .render_to_target_rtv_at(
                &src_1080,
                plan_1080,
                (0.0, 0.0),
                (2560, 1600),
                rtv_1080.as_ref().unwrap(),
                uv_1080,
                true,
            )
            .expect("1080p native VSR render");
        assert!(engine.status.hw_ext_supported);
        assert!(engine.status.hw_active, "1080p VSR must engage even without upscaling");
        assert!(engine.status.clarity_active);

        // Case 2: 720x1280 with FG capped pipeline (1920x1200 -> 675x1200 active <= 720x1280)
        let plan_fg = compute_native_mpv_canvas(720, 1280, 1920, 1200, AspectMode::Fit, true);
        assert!(!plan_fg.needs_upscale);
        let (src_fg, _) =
            create_bgra_texture(&device, plan_fg.mpv_w, plan_fg.mpv_h, None, true).unwrap();
        let uv_fg = compute_active_uv_spec(720, 1280, 1920, 1200, AspectMode::Fit);
        let _ = engine
            .process_to_texture_with_vsr(&src_fg, plan_fg, 1920, 1200, uv_fg, true)
            .expect("720p FG-capped VSR texture");
        assert!(engine.status.hw_active, "FG-capped 720p VSR must stay active");

        // Case 3: explicit off clears hw_active and clarity_active
        let _ = engine
            .process_to_texture_with_vsr(&src_fg, plan_fg, 1920, 1200, uv_fg, false)
            .expect("VSR off passthrough");
        assert!(!engine.status.hw_active);
        assert!(!engine.status.clarity_active);
    }
}
