use sha2::{Digest, Sha256};
use std::ffi::{c_void, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;
use windows::core::{Interface, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_BUFFER,
    D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView, ID3D11VertexShader,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_SRV, D3D11_BUFFER_UAV,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CULL_NONE,
    D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD, D3D11_RASTERIZER_DESC,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_UAV_DIMENSION_BUFFER,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::prefs::{shader_cache_dir, AspectMode};

const SHADER_COMMON: &str = include_str!("../shaders/InterpCommon.hlsli");
const SHADER_PREPROCESS: &str = include_str!("../shaders/PreprocessPyramid.hlsl");
const SHADER_FLOW_COARSE: &str = include_str!("../shaders/FlowCoarse.hlsl");
const SHADER_FLOW_REFINE: &str = include_str!("../shaders/FlowRefine.hlsl");
const SHADER_FLOW_FILTER_OCC: &str = include_str!("../shaders/FlowFilterOcc.hlsl");
const SHADER_SYNTH_SUBPIXEL: &str = include_str!("../shaders/SynthSubpixel.hlsl");

const D3DCOMPILE_OPTIMIZATION_LEVEL3: u32 = 1 << 15;

pub const FD_STATIC_PANEL: u32 = 1;
#[allow(dead_code)]
pub const FD_SCENE_CUT: u32 = 2;
pub const FD_DUPLICATE: u32 = 4;
#[allow(dead_code)]
pub const FD_GLOBAL_FADE: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum InterpContentMode {
    Drama = 0,
    Comic = 1,
    Anime = 2,
}

impl InterpContentMode {
    pub fn from_modes(fg_mode: &str, content_kind: &str) -> Self {
        match fg_mode.trim().to_ascii_lowercase().as_str() {
            "drama" => Self::Drama,
            "comic" => Self::Comic,
            "anime" => Self::Anime,
            _ => match content_kind.trim().to_ascii_lowercase().as_str() {
                "comic" => Self::Comic,
                "anime" => Self::Anime,
                _ => Self::Drama,
            },
        }
    }

    pub fn profile(self) -> InterpProfile {
        match self {
            Self::Drama => InterpProfile {
                flat_zero_bias: 0.024,
                grad_weight: 0.50,
                edge_sharpness: 1.65,
                sub_lock_weight: 1.0,
                dup_mad_max: 0.0004,
                dup_changed_max: 0.002,
                static_mad_max: 0.0025,
                static_changed_max: 0.009,
                cut_mad_min: 0.14,
                cut_changed_min: 0.62,
                cut_grad_min: 0.08,
                max_safe_flow_px: 140.0,
                smooth_prior: 0.040,
                occ_strictness: 1.18,
                sharp_blend: 1.0,
            },
            Self::Comic => InterpProfile {
                flat_zero_bias: 0.036,
                grad_weight: 0.72,
                edge_sharpness: 2.55,
                sub_lock_weight: 1.0,
                dup_mad_max: 0.0010,
                dup_changed_max: 0.004,
                static_mad_max: 0.0055,
                static_changed_max: 0.022,
                cut_mad_min: 0.12,
                cut_changed_min: 0.55,
                cut_grad_min: 0.075,
                max_safe_flow_px: 64.0,
                smooth_prior: 0.048,
                occ_strictness: 1.25,
                sharp_blend: 1.0,
            },
            Self::Anime => InterpProfile {
                flat_zero_bias: 0.030,
                grad_weight: 0.78,
                edge_sharpness: 2.35,
                sub_lock_weight: 0.65,
                dup_mad_max: 0.0022,
                dup_changed_max: 0.009,
                static_mad_max: 0.0032,
                static_changed_max: 0.012,
                cut_mad_min: 0.13,
                cut_changed_min: 0.58,
                cut_grad_min: 0.08,
                max_safe_flow_px: 110.0,
                smooth_prior: 0.045,
                occ_strictness: 1.22,
                sharp_blend: 1.0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InterpProfile {
    pub flat_zero_bias: f32,
    pub grad_weight: f32,
    pub edge_sharpness: f32,
    pub sub_lock_weight: f32,
    pub dup_mad_max: f32,
    pub dup_changed_max: f32,
    pub static_mad_max: f32,
    pub static_changed_max: f32,
    pub cut_mad_min: f32,
    pub cut_changed_min: f32,
    pub cut_grad_min: f32,
    pub max_safe_flow_px: f32,
    pub smooth_prior: f32,
    pub occ_strictness: f32,
    pub sharp_blend: f32,
}

/// Active video region inside `mpv_tex` plus orientation-tuned subtitle protection band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoActiveUvSpec {
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub sub_band_start_y: f32,
    pub sub_band_end_y: f32,
    pub portrait_factor: f32,
}

impl Default for VideoActiveUvSpec {
    fn default() -> Self {
        Self {
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            sub_band_start_y: 0.74,
            sub_band_end_y: 0.98,
            portrait_factor: 0.0,
        }
    }
}

/// Computes normalized active-picture bounds `[uv_min, uv_max]` inside `dst_w x dst_h`
/// so black letterboxes/pillarboxes never bias frame comparison or leak across edges,
/// and chooses portrait (`9:16`) vs landscape (`16:9`) subtitle lock zones.
pub fn compute_active_uv_spec(
    vid_w: u32,
    vid_h: u32,
    dst_w: u32,
    dst_h: u32,
    aspect: AspectMode,
) -> VideoActiveUvSpec {
    let has_vid = vid_w > 0 && vid_h > 0;
    let has_dst = dst_w > 0 && dst_h > 0;
    let is_portrait = if has_vid {
        f64::from(vid_h) > f64::from(vid_w) * 1.04
    } else if has_dst {
        f64::from(dst_h) > f64::from(dst_w) * 1.04
    } else {
        false
    };
    let (sub_band_start_y, sub_band_end_y, portrait_factor) = if is_portrait {
        (0.62, 0.94, 1.0)
    } else {
        (0.74, 0.98, 0.0)
    };

    let mut uv_min = [0.0f32, 0.0f32];
    let mut uv_max = [1.0f32, 1.0f32];
    if aspect == AspectMode::Fit && has_vid && has_dst {
        let src_ar = f64::from(vid_w) / f64::from(vid_h);
        let dst_ar = f64::from(dst_w) / f64::from(dst_h);
        if src_ar < dst_ar * 0.995 {
            let active_w = (src_ar / dst_ar).clamp(0.05, 1.0) as f32;
            let margin = ((1.0 - active_w) * 0.5).clamp(0.0, 0.45);
            uv_min = [margin, 0.0];
            uv_max = [1.0 - margin, 1.0];
        } else if src_ar > dst_ar * 1.005 {
            let active_h = (dst_ar / src_ar).clamp(0.05, 1.0) as f32;
            let margin = ((1.0 - active_h) * 0.5).clamp(0.0, 0.45);
            uv_min = [0.0, margin];
            uv_max = [1.0, 1.0 - margin];
        }
    }

    VideoActiveUvSpec {
        uv_min,
        uv_max,
        sub_band_start_y,
        sub_band_end_y,
        portrait_factor,
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InterpCBData {
    pub src_w: u32,
    pub src_h: u32,
    pub full_w: u32,
    pub full_h: u32,
    pub coarse_w: u32,
    pub coarse_h: u32,
    pub alpha: f32,
    pub content_mode: u32,
    pub flat_zero_bias: f32,
    pub grad_weight: f32,
    pub edge_sharpness: f32,
    pub sub_lock_weight: f32,
    pub dup_mad_max: f32,
    pub dup_changed_max: f32,
    pub static_mad_max: f32,
    pub static_changed_max: f32,
    pub cut_mad_min: f32,
    pub cut_changed_min: f32,
    pub cut_grad_min: f32,
    pub max_safe_flow_px: f32,
    pub level_scale: f32,
    pub smooth_prior: f32,
    pub occ_strictness: f32,
    pub sharp_blend: f32,
    pub active_uv_min: [f32; 2],
    pub active_uv_max: [f32; 2],
    pub sub_band_start_y: f32,
    pub sub_band_end_y: f32,
    pub portrait_factor: f32,
    pub aspect_pad: f32,
}

impl InterpCBData {
    #[allow(clippy::too_many_arguments)]
    pub fn from_mode(
        src_w: u32,
        src_h: u32,
        full_w: u32,
        full_h: u32,
        coarse_w: u32,
        coarse_h: u32,
        alpha: f32,
        level_scale: f32,
        mode: InterpContentMode,
        uv_spec: VideoActiveUvSpec,
    ) -> Self {
        let p = mode.profile();
        Self {
            src_w,
            src_h,
            full_w,
            full_h,
            coarse_w,
            coarse_h,
            alpha,
            content_mode: mode as u32,
            flat_zero_bias: p.flat_zero_bias,
            grad_weight: p.grad_weight,
            edge_sharpness: p.edge_sharpness,
            sub_lock_weight: p.sub_lock_weight,
            dup_mad_max: p.dup_mad_max,
            dup_changed_max: p.dup_changed_max,
            static_mad_max: p.static_mad_max,
            static_changed_max: p.static_changed_max,
            cut_mad_min: p.cut_mad_min,
            cut_changed_min: p.cut_changed_min,
            cut_grad_min: p.cut_grad_min,
            max_safe_flow_px: p.max_safe_flow_px,
            level_scale,
            smooth_prior: p.smooth_prior,
            occ_strictness: p.occ_strictness,
            sharp_blend: p.sharp_blend,
            active_uv_min: uv_spec.uv_min,
            active_uv_max: uv_spec.uv_max,
            sub_band_start_y: uv_spec.sub_band_start_y,
            sub_band_end_y: uv_spec.sub_band_end_y,
            portrait_factor: uv_spec.portrait_factor,
            aspect_pad: 0.0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDecisionData {
    pub mad_mean: f32,
    pub changed_frac: f32,
    pub grad_diff_mean: f32,
    pub bottom_mad: f32,
    pub luma_mean_diff: f32,
    pub non_bottom_mad: f32,
    pub flags: u32,
    pub pad: u32,
}

struct ShaderSpec {
    file_name: &'static str,
    entry: &'static str,
    profile: &'static str,
    source: &'static str,
}

const SHADER_SPECS: [ShaderSpec; 8] = [
    ShaderSpec {
        file_name: "PreprocessPyramid.hlsl",
        entry: "CSPreprocessL0",
        profile: "cs_5_0",
        source: SHADER_PREPROCESS,
    },
    ShaderSpec {
        file_name: "PreprocessPyramid.hlsl",
        entry: "CSDownsample2x",
        profile: "cs_5_0",
        source: SHADER_PREPROCESS,
    },
    ShaderSpec {
        file_name: "PreprocessPyramid.hlsl",
        entry: "CSFrameCompare",
        profile: "cs_5_0",
        source: SHADER_PREPROCESS,
    },
    ShaderSpec {
        file_name: "FlowCoarse.hlsl",
        entry: "CSFlowCoarse",
        profile: "cs_5_0",
        source: SHADER_FLOW_COARSE,
    },
    ShaderSpec {
        file_name: "FlowRefine.hlsl",
        entry: "CSFlowRefine",
        profile: "cs_5_0",
        source: SHADER_FLOW_REFINE,
    },
    ShaderSpec {
        file_name: "FlowFilterOcc.hlsl",
        entry: "CSFlowFilterOcc",
        profile: "cs_5_0",
        source: SHADER_FLOW_FILTER_OCC,
    },
    ShaderSpec {
        file_name: "SynthSubpixel.hlsl",
        entry: "VSMain",
        profile: "vs_5_0",
        source: SHADER_SYNTH_SUBPIXEL,
    },
    ShaderSpec {
        file_name: "SynthSubpixel.hlsl",
        entry: "PSMain",
        profile: "ps_5_0",
        source: SHADER_SYNTH_SUBPIXEL,
    },
];

#[derive(Clone)]
pub struct CompiledShaderBlobs {
    pub blobs: Vec<Vec<u8>>,
    pub cache_hits: usize,
    pub compiled_count: usize,
    pub elapsed_ms: u64,
}

fn expand_shader_source(file_name: &str, raw_src: &str) -> String {
    let mut body = String::with_capacity(SHADER_COMMON.len() + raw_src.len() + 96);
    body.push_str("#line 1 \"InterpCommon.hlsli\"\n");
    body.push_str(SHADER_COMMON);
    body.push('\n');
    body.push_str(&format!("#line 1 \"{file_name}\"\n"));
    for line in raw_src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#include") && trimmed.contains("InterpCommon.hlsli") {
            body.push_str("// included\n");
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    body
}

fn dxbc_cache_path(cache_dir: &Path, spec: &ShaderSpec, expanded: &str) -> PathBuf {
    let key = format!(
        "dxbc-v2|{}|{}|{}|opt3|{}",
        spec.file_name, spec.entry, spec.profile, expanded
    );
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    cache_dir.join(format!("{}-{}.dxbc", spec.entry, hex))
}

type FnD3DCompile = unsafe extern "system" fn(
    p_src_data: *const c_void,
    src_data_size: usize,
    p_source_name: PCSTR,
    p_defines: *const c_void,
    p_include: *mut c_void,
    p_entry_point: PCSTR,
    p_target: PCSTR,
    flags1: u32,
    flags2: u32,
    pp_code: *mut *mut c_void,
    pp_error_msgs: *mut *mut c_void,
) -> HRESULT;

pub(crate) struct D3DCompilerDll {
    module: HMODULE,
    compile: FnD3DCompile,
}

unsafe impl Send for D3DCompilerDll {}
unsafe impl Sync for D3DCompilerDll {}

impl D3DCompilerDll {
    pub(crate) fn load() -> Result<Self, String> {
        let wide: Vec<u16> = "d3dcompiler_47.dll\0".encode_utf16().collect();
        let module = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }
            .map_err(|e| format!("LoadLibraryW(d3dcompiler_47.dll) failed: {e}"))?;
        let proc = unsafe { GetProcAddress(module, PCSTR(c"D3DCompile".as_ptr() as *const u8)) };
        let Some(p) = proc else {
            unsafe {
                let _ = FreeLibrary(module);
            }
            return Err("d3dcompiler_47.dll missing D3DCompile".into());
        };
        let compile = unsafe { std::mem::transmute_copy::<_, FnD3DCompile>(&p) };
        Ok(Self { module, compile })
    }

    pub(crate) fn compile_hlsl(
        &self,
        expanded: &str,
        file_name: &str,
        entry: &str,
        profile: &str,
    ) -> Result<Vec<u8>, String> {
        let cname = CString::new(file_name).unwrap_or_default();
        let centry = CString::new(entry).unwrap_or_default();
        let cprof = CString::new(profile).unwrap_or_default();
        let mut code_ptr: *mut c_void = ptr::null_mut();
        let mut err_ptr: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            (self.compile)(
                expanded.as_ptr() as *const c_void,
                expanded.len(),
                PCSTR(cname.as_ptr() as *const u8),
                ptr::null(),
                ptr::null_mut(),
                PCSTR(centry.as_ptr() as *const u8),
                PCSTR(cprof.as_ptr() as *const u8),
                D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut code_ptr,
                &mut err_ptr,
            )
        };
        let err_text = if !err_ptr.is_null() {
            let blob: ID3DBlob = unsafe { Interface::from_raw(err_ptr) };
            let slice = unsafe {
                std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                )
            };
            Some(String::from_utf8_lossy(slice).into_owned())
        } else {
            None
        };
        if hr.is_err() || code_ptr.is_null() {
            return Err(format!(
                "D3DCompile({file_name}::{entry}) failed {:?}: {}",
                hr,
                err_text.unwrap_or_default()
            ));
        }
        let code_blob: ID3DBlob = unsafe { Interface::from_raw(code_ptr) };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                code_blob.GetBufferPointer() as *const u8,
                code_blob.GetBufferSize(),
            )
        }
        .to_vec();
        Ok(bytes)
    }
}

impl Drop for D3DCompilerDll {
    fn drop(&mut self) {
        if !self.module.is_invalid() {
            unsafe {
                let _ = FreeLibrary(self.module);
            }
        }
    }
}

impl CompiledShaderBlobs {
    pub fn load_or_compile() -> Result<Self, String> {
        let t0 = Instant::now();
        let cache_dir = shader_cache_dir();
        let _ = fs::create_dir_all(&cache_dir);

        struct Prep {
            idx: usize,
            expanded: String,
            cache_path: PathBuf,
            cached: Option<Vec<u8>>,
        }

        let mut preps = Vec::with_capacity(SHADER_SPECS.len());
        let mut any_miss = false;
        let mut cache_hits = 0usize;
        for (idx, spec) in SHADER_SPECS.iter().enumerate() {
            let expanded = expand_shader_source(spec.file_name, spec.source);
            let cache_path = dxbc_cache_path(&cache_dir, spec, &expanded);
            let cached = fs::read(&cache_path).ok().filter(|b| b.len() >= 32);
            if cached.is_some() {
                cache_hits += 1;
            } else {
                any_miss = true;
            }
            preps.push(Prep {
                idx,
                expanded,
                cache_path,
                cached,
            });
        }

        let mut out_blobs: Vec<Option<Vec<u8>>> = vec![None; SHADER_SPECS.len()];
        let mut compiled_count = 0usize;
        if any_miss {
            let compiler = Arc::new(D3DCompilerDll::load()?);
            let misses: Vec<Prep> = preps
                .into_iter()
                .filter_map(|p| {
                    if let Some(b) = p.cached {
                        out_blobs[p.idx] = Some(b);
                        None
                    } else {
                        Some(p)
                    }
                })
                .collect();
            compiled_count = misses.len();

            type CompiledShaderChunk = Result<(usize, PathBuf, Vec<u8>), String>;
            let compiled_results: Vec<CompiledShaderChunk> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = misses
                        .into_iter()
                        .map(|m| {
                            let comp = compiler.clone();
                            s.spawn(move || {
                                let spec = &SHADER_SPECS[m.idx];
                                let bytes = comp.compile_hlsl(
                                    &m.expanded,
                                    spec.file_name,
                                    spec.entry,
                                    spec.profile,
                                )?;
                                Ok((m.idx, m.cache_path, bytes))
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().unwrap_or_else(|_| Err("shader compile panic".into())))
                        .collect()
                });

            for res in compiled_results {
                let (idx, path, bytes) = res?;
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&path, &bytes);
                out_blobs[idx] = Some(bytes);
            }
        } else {
            for p in preps {
                out_blobs[p.idx] = p.cached;
            }
        }

        let blobs: Vec<Vec<u8>> = out_blobs
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| "missing compiled shader blob".to_string())?;

        Ok(Self {
            blobs,
            cache_hits,
            compiled_count,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        })
    }
}

pub struct InterpShaderSet {
    pub cs_preprocess_l0: ID3D11ComputeShader,
    pub cs_downsample_2x: ID3D11ComputeShader,
    pub cs_frame_compare: ID3D11ComputeShader,
    pub cs_flow_coarse: ID3D11ComputeShader,
    pub cs_flow_refine: ID3D11ComputeShader,
    pub cs_flow_filter_occ: ID3D11ComputeShader,
    pub vs_synth: ID3D11VertexShader,
    pub ps_synth: ID3D11PixelShader,
    #[allow(dead_code)]
    pub stats: CompiledShaderBlobs,
}

impl InterpShaderSet {
    pub fn from_blobs(device: &ID3D11Device, blobs: CompiledShaderBlobs) -> Result<Arc<Self>, String> {
        let mk_cs = |idx: usize| -> Result<ID3D11ComputeShader, String> {
            let mut cs = None;
            unsafe { device.CreateComputeShader(&blobs.blobs[idx], None, Some(&mut cs)) }
                .map_err(|e| format!("CreateComputeShader({idx}) failed: {e}"))?;
            cs.ok_or_else(|| format!("CreateComputeShader({idx}) returned None"))
        };
        let mut vs_synth = None;
        unsafe { device.CreateVertexShader(&blobs.blobs[6], None, Some(&mut vs_synth)) }
            .map_err(|e| format!("CreateVertexShader failed: {e}"))?;
        let mut ps_synth = None;
        unsafe { device.CreatePixelShader(&blobs.blobs[7], None, Some(&mut ps_synth)) }
            .map_err(|e| format!("CreatePixelShader failed: {e}"))?;

        Ok(Arc::new(Self {
            cs_preprocess_l0: mk_cs(0)?,
            cs_downsample_2x: mk_cs(1)?,
            cs_frame_compare: mk_cs(2)?,
            cs_flow_coarse: mk_cs(3)?,
            cs_flow_refine: mk_cs(4)?,
            cs_flow_filter_occ: mk_cs(5)?,
            vs_synth: vs_synth.ok_or_else(|| "CreateVertexShader returned None".to_string())?,
            ps_synth: ps_synth.ok_or_else(|| "CreatePixelShader returned None".to_string())?,
            stats: blobs,
        }))
    }
}

struct TexViews {
    w: u32,
    h: u32,
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    uav: Option<ID3D11UnorderedAccessView>,
    #[allow(dead_code)]
    rtv: Option<ID3D11RenderTargetView>,
}

struct PipelineAlloc {
    full_w: u32,
    full_h: u32,
    prev_color: TexViews,
    curr_color: TexViews,
    pyr_prev: Vec<TexViews>,
    pyr_curr: Vec<TexViews>,
    raw_flow: Vec<TexViews>, // indices 0..=4 correspond to L0..=L4 (we use 1..=4)
    clean_flow_l1: TexViews,
    occ_conf_l1: TexViews,
}

pub struct GpuInterpPipeline {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    shaders: Arc<InterpShaderSet>,
    cb: ID3D11Buffer,
    samp_linear: ID3D11SamplerState,
    samp_point: ID3D11SamplerState,
    rs_solid_no_scissor: ID3D11RasterizerState,
    decision_buf: ID3D11Buffer,
    decision_staging: ID3D11Buffer,
    decision_srv: ID3D11ShaderResourceView,
    decision_uav: ID3D11UnorderedAccessView,
    alloc: Option<PipelineAlloc>,
    active_uv: VideoActiveUvSpec,
    pub has_prev_frame: bool,
    pub prepared_pair: bool,
    pub seek_discontinuity: bool,
}

impl GpuInterpPipeline {
    pub fn new(
        device: ID3D11Device,
        ctx: ID3D11DeviceContext,
        shaders: Arc<InterpShaderSet>,
    ) -> Result<Self, String> {
        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<InterpCBData>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cb = None;
        unsafe { device.CreateBuffer(&cb_desc, None, Some(&mut cb)) }
            .map_err(|e| format!("CreateBuffer(CB) failed: {e}"))?;
        let cb = cb.unwrap();

        let samp_linear = create_sampler_state(&device, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?;
        let samp_point = create_sampler_state(&device, D3D11_FILTER_MIN_MAG_MIP_POINT)?;
        let rs_solid_no_scissor = create_solid_no_scissor_rs(&device)?;

        let elem_sz = std::mem::size_of::<FrameDecisionData>() as u32;
        let db_desc = D3D11_BUFFER_DESC {
            ByteWidth: elem_sz,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_UNORDERED_ACCESS.0) as u32,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
            StructureByteStride: elem_sz,
            ..Default::default()
        };
        let init_zero = FrameDecisionData::default();
        let init_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: (&init_zero as *const FrameDecisionData) as *const c_void,
            ..Default::default()
        };
        let mut decision_buf = None;
        unsafe { device.CreateBuffer(&db_desc, Some(&init_data), Some(&mut decision_buf)) }
            .map_err(|e| format!("CreateBuffer(decision) failed: {e}"))?;
        let decision_buf = decision_buf.unwrap();

        let stg_desc = D3D11_BUFFER_DESC {
            ByteWidth: elem_sz,
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            StructureByteStride: elem_sz,
            ..Default::default()
        };
        let mut decision_staging = None;
        unsafe { device.CreateBuffer(&stg_desc, None, Some(&mut decision_staging)) }
            .map_err(|e| format!("CreateBuffer(decision_staging) failed: {e}"))?;
        let decision_staging = decision_staging.unwrap();

        let mut dsrv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D_SRV_DIMENSION_BUFFER,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_SRV {
                    Anonymous1: Default::default(),
                    Anonymous2: Default::default(),
                },
            },
        };
        dsrv_desc.Anonymous.Buffer.Anonymous1.FirstElement = 0;
        dsrv_desc.Anonymous.Buffer.Anonymous2.NumElements = 1;
        let mut decision_srv = None;
        unsafe {
            device.CreateShaderResourceView(&decision_buf, Some(&dsrv_desc), Some(&mut decision_srv))
        }
        .map_err(|e| format!("CreateSRV(decision) failed: {e}"))?;
        let decision_srv = decision_srv.unwrap();

        let duav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: 1,
                    Flags: 0,
                },
            },
        };
        let mut decision_uav = None;
        unsafe {
            device.CreateUnorderedAccessView(
                &decision_buf,
                Some(&duav_desc),
                Some(&mut decision_uav),
            )
        }
        .map_err(|e| format!("CreateUAV(decision) failed: {e}"))?;
        let decision_uav = decision_uav.unwrap();

        Ok(Self {
            device,
            ctx,
            shaders,
            cb,
            samp_linear,
            samp_point,
            rs_solid_no_scissor,
            decision_buf,
            decision_staging,
            decision_srv,
            decision_uav,
            alloc: None,
            active_uv: VideoActiveUvSpec::default(),
            has_prev_frame: false,
            prepared_pair: false,
            seek_discontinuity: false,
        })
    }

    pub fn set_active_uv_spec(&mut self, spec: VideoActiveUvSpec) {
        self.active_uv = spec;
    }

    pub fn reset_frames(&mut self) {
        self.has_prev_frame = false;
        self.prepared_pair = false;
        self.seek_discontinuity = false;
    }

    /// Marks an upcoming stream discontinuity (seek / scrub / frame-step) while
    /// preserving `has_prev_frame` so the display holds the last frame cleanly
    /// without flashing to a blank clear color while mpv decodes the target.
    pub fn mark_seek_discontinuity(&mut self) {
        self.prepared_pair = false;
        self.seek_discontinuity = true;
    }

    pub fn ensure_size(&mut self, full_w: u32, full_h: u32) -> Result<(), String> {
        let w = full_w.max(64);
        let h = full_h.max(64);
        if let Some(a) = &self.alloc {
            if a.full_w == w && a.full_h == h {
                return Ok(());
            }
        }
        self.reset_frames();
        let prev_color = create_tex_views(&self.device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, false, false)?;
        let curr_color = create_tex_views(&self.device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, false, false)?;

        let mut dims = [(w, h); 5];
        for lv in 1..5 {
            dims[lv] = (dims[lv - 1].0.div_ceil(2), dims[lv - 1].1.div_ceil(2));
            dims[lv].0 = dims[lv].0.max(8);
            dims[lv].1 = dims[lv].1.max(8);
        }

        let mut pyr_prev = Vec::with_capacity(5);
        let mut pyr_curr = Vec::with_capacity(5);
        let mut raw_flow = Vec::with_capacity(5);
        for (lw, lh) in dims {
            pyr_prev.push(create_tex_views(
                &self.device,
                lw,
                lh,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                true,
                false,
            )?);
            pyr_curr.push(create_tex_views(
                &self.device,
                lw,
                lh,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                true,
                false,
            )?);
            raw_flow.push(create_tex_views(
                &self.device,
                lw,
                lh,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                true,
                false,
            )?);
        }
        let l1 = dims[1];
        let clean_flow_l1 = create_tex_views(
            &self.device,
            l1.0,
            l1.1,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            true,
            false,
        )?;
        let occ_conf_l1 = create_tex_views(
            &self.device,
            l1.0,
            l1.1,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            true,
            false,
        )?;

        self.alloc = Some(PipelineAlloc {
            full_w: w,
            full_h: h,
            prev_color,
            curr_color,
            pyr_prev,
            pyr_curr,
            raw_flow,
            clean_flow_l1,
            occ_conf_l1,
        });
        Ok(())
    }

    /// Pushes one newly decoded frame into the stream, reusing the previous
    /// frame & pyramid cleanly. Skips optical-flow passes when `compute_flow`
    /// is false (pass-through native FPS mode).
    pub fn push_frame_and_prepare(
        &mut self,
        next_frame: &ID3D11Texture2D,
        mode: InterpContentMode,
        compute_flow: bool,
    ) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            next_frame.GetDesc(&mut desc);
            // Release any lingering RTV binding (e.g. libmpv's last FBO bind) before CopyResource.
            self.ctx.OMSetRenderTargets(None, None);
        }
        self.ensure_size(desc.Width, desc.Height)?;

        if !compute_flow {
            let a = self.alloc.as_ref().unwrap();
            unsafe {
                self.ctx.CopyResource(&a.curr_color.tex, next_frame);
            }
            self.has_prev_frame = true;
            self.prepared_pair = false;
            self.seek_discontinuity = false;
            return Ok(());
        }

        let discontinuous = std::mem::take(&mut self.seek_discontinuity);
        let was_ready = self.has_prev_frame && !discontinuous;
        if was_ready {
            let a = self.alloc.as_mut().unwrap();
            std::mem::swap(&mut a.prev_color, &mut a.curr_color);
            std::mem::swap(&mut a.pyr_prev, &mut a.pyr_curr);
            unsafe {
                self.ctx.CopyResource(&a.curr_color.tex, next_frame);
            }
            self.build_curr_pyramid(mode)?;
            self.run_flow_passes(mode)?;
            self.prepared_pair = true;
        } else {
            let a = self.alloc.as_ref().unwrap();
            unsafe {
                self.ctx.CopyResource(&a.prev_color.tex, next_frame);
                self.ctx.CopyResource(&a.curr_color.tex, next_frame);
            }
            self.build_prev_pyramid(mode)?;
            self.build_curr_pyramid(mode)?;
            self.has_prev_frame = true;
            self.prepared_pair = false;
        }
        Ok(())
    }

    /// Explicitly sets a `(prev, curr)` pair and runs the complete 4-level optical
    /// flow pipeline (primarily used by `--selftest`).
    pub fn prepare_pair(
        &mut self,
        prev: &ID3D11Texture2D,
        curr: &ID3D11Texture2D,
        mode: InterpContentMode,
    ) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            curr.GetDesc(&mut desc);
            self.ctx.OMSetRenderTargets(None, None);
        }
        self.ensure_size(desc.Width, desc.Height)?;
        {
            let a = self.alloc.as_ref().unwrap();
            unsafe {
                self.ctx.CopyResource(&a.prev_color.tex, prev);
                self.ctx.CopyResource(&a.curr_color.tex, curr);
            }
        }
        self.build_prev_pyramid(mode)?;
        self.build_curr_pyramid(mode)?;
        self.run_flow_passes(mode)?;
        self.has_prev_frame = true;
        self.prepared_pair = true;
        Ok(())
    }

    fn build_prev_pyramid(&self, mode: InterpContentMode) -> Result<(), String> {
        let a = self.alloc.as_ref().ok_or("not allocated")?;
        self.build_pyramid_into(&a.prev_color, &a.pyr_prev, a.full_w, a.full_h, mode)
    }

    fn build_curr_pyramid(&self, mode: InterpContentMode) -> Result<(), String> {
        let a = self.alloc.as_ref().ok_or("not allocated")?;
        self.build_pyramid_into(&a.curr_color, &a.pyr_curr, a.full_w, a.full_h, mode)
    }

    fn build_pyramid_into(
        &self,
        color: &TexViews,
        pyr: &[TexViews],
        full_w: u32,
        full_h: u32,
        mode: InterpContentMode,
    ) -> Result<(), String> {
        let l0 = &pyr[0];
        let cb0 = InterpCBData::from_mode(
            l0.w,
            l0.h,
            full_w,
            full_h,
            full_w,
            full_h,
            0.5,
            1.0,
            mode,
            self.active_uv,
        );
        self.update_cb(&cb0);
        self.bind_common_cs();
        unsafe {
            self.ctx
                .CSSetShader(&self.shaders.cs_preprocess_l0, None);
            self.ctx
                .CSSetShaderResources(0, Some(&[Some(color.srv.clone())]));
            let uavs = [l0.uav.clone()];
            self.ctx
                .CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
            self.ctx.Dispatch(groups(l0.w), groups(l0.h), 1);
        }
        self.clear_cs_bindings();

        for lv in 1..5 {
            let src = &pyr[lv - 1];
            let dst = &pyr[lv];
            let cb_lv = InterpCBData::from_mode(
                dst.w,
                dst.h,
                full_w,
                full_h,
                src.w,
                src.h,
                0.5,
                1.0,
                mode,
                self.active_uv,
            );
            self.update_cb(&cb_lv);
            self.bind_common_cs();
            unsafe {
                self.ctx
                    .CSSetShader(&self.shaders.cs_downsample_2x, None);
                self.ctx
                    .CSSetShaderResources(0, Some(&[Some(src.srv.clone())]));
                let uavs = [dst.uav.clone()];
                self.ctx
                    .CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
                self.ctx.Dispatch(groups(dst.w), groups(dst.h), 1);
            }
            self.clear_cs_bindings();
        }
        Ok(())
    }

    fn run_flow_passes(&self, mode: InterpContentMode) -> Result<(), String> {
        let a = self.alloc.as_ref().ok_or("not allocated")?;
        let fw = a.full_w;
        let fh = a.full_h;

        // 1. Frame compare at L2 -> u1 = decisionUav
        let l2 = &a.pyr_curr[2];
        let cb_cmp = InterpCBData::from_mode(
            l2.w,
            l2.h,
            fw,
            fh,
            l2.w,
            l2.h,
            0.5,
            1.0,
            mode,
            self.active_uv,
        );
        self.update_cb(&cb_cmp);
        self.bind_common_cs();
        unsafe {
            self.ctx
                .CSSetShader(&self.shaders.cs_frame_compare, None);
            self.ctx.CSSetShaderResources(
                0,
                Some(&[Some(a.pyr_prev[2].srv.clone()), Some(a.pyr_curr[2].srv.clone())]),
            );
            let uavs = [None, Some(self.decision_uav.clone())];
            self.ctx
                .CSSetUnorderedAccessViews(0, 2, Some(uavs.as_ptr()), None);
            self.ctx.Dispatch(1, 1, 1);
        }
        self.clear_cs_bindings();

        // 2. Coarse optical flow at L4
        let l4p = &a.pyr_prev[4];
        let l4c = &a.pyr_curr[4];
        let rf4 = &a.raw_flow[4];
        let cb4 = InterpCBData::from_mode(
            rf4.w,
            rf4.h,
            fw,
            fh,
            rf4.w,
            rf4.h,
            0.5,
            1.0,
            mode,
            self.active_uv,
        );
        self.update_cb(&cb4);
        self.bind_common_cs();
        unsafe {
            self.ctx.CSSetShader(&self.shaders.cs_flow_coarse, None);
            self.ctx.CSSetShaderResources(
                0,
                Some(&[
                    Some(l4p.srv.clone()),
                    Some(l4c.srv.clone()),
                    Some(self.decision_srv.clone()),
                ]),
            );
            let uavs = [rf4.uav.clone()];
            self.ctx
                .CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
            self.ctx.Dispatch(groups(rf4.w), groups(rf4.h), 1);
        }
        self.clear_cs_bindings();

        // 3. Hierarchical refinement L3 -> L2 -> L1
        for lv in (1..=3).rev() {
            let p = &a.pyr_prev[lv];
            let c = &a.pyr_curr[lv];
            let coarse = &a.raw_flow[lv + 1];
            let dst = &a.raw_flow[lv];
            let cb_ref = InterpCBData::from_mode(
                dst.w,
                dst.h,
                fw,
                fh,
                coarse.w,
                coarse.h,
                0.5,
                1.0,
                mode,
                self.active_uv,
            );
            self.update_cb(&cb_ref);
            self.bind_common_cs();
            unsafe {
                self.ctx.CSSetShader(&self.shaders.cs_flow_refine, None);
                self.ctx.CSSetShaderResources(
                    0,
                    Some(&[
                        Some(p.srv.clone()),
                        Some(c.srv.clone()),
                        Some(coarse.srv.clone()),
                        Some(self.decision_srv.clone()),
                    ]),
                );
                let uavs = [dst.uav.clone()];
                self.ctx
                    .CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
                self.ctx.Dispatch(groups(dst.w), groups(dst.h), 1);
            }
            self.clear_cs_bindings();
        }

        // 4. Joint bilateral smoothing + forward/backward occlusion & subtitle lock at L1
        let l1p = &a.pyr_prev[1];
        let l1c = &a.pyr_curr[1];
        let rf1 = &a.raw_flow[1];
        let cb_flt = InterpCBData::from_mode(
            rf1.w,
            rf1.h,
            fw,
            fh,
            rf1.w,
            rf1.h,
            0.5,
            1.0,
            mode,
            self.active_uv,
        );
        self.update_cb(&cb_flt);
        self.bind_common_cs();
        unsafe {
            self.ctx
                .CSSetShader(&self.shaders.cs_flow_filter_occ, None);
            self.ctx.CSSetShaderResources(
                0,
                Some(&[
                    Some(l1p.srv.clone()),
                    Some(l1c.srv.clone()),
                    Some(rf1.srv.clone()),
                    Some(self.decision_srv.clone()),
                ]),
            );
            let uavs = [a.clean_flow_l1.uav.clone(), a.occ_conf_l1.uav.clone()];
            self.ctx
                .CSSetUnorderedAccessViews(0, 2, Some(uavs.as_ptr()), None);
            self.ctx.Dispatch(groups(rf1.w), groups(rf1.h), 1);
        }
        self.clear_cs_bindings();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_to_rtv(
        &self,
        alpha: f32,
        target_rtv: &ID3D11RenderTargetView,
        viewport_x: f32,
        viewport_y: f32,
        viewport_w: f32,
        viewport_h: f32,
        mode: InterpContentMode,
    ) -> Result<(), String> {
        let a = self.alloc.as_ref().ok_or("not allocated")?;
        let clamped_alpha = if self.prepared_pair {
            alpha.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let l1 = &a.pyr_curr[1];
        let cb = InterpCBData::from_mode(
            a.full_w,
            a.full_h,
            a.full_w,
            a.full_h,
            l1.w,
            l1.h,
            clamped_alpha,
            1.0,
            mode,
            self.active_uv,
        );
        self.update_cb(&cb);

        let vp = D3D11_VIEWPORT {
            TopLeftX: viewport_x,
            TopLeftY: viewport_y,
            Width: viewport_w.max(1.0),
            Height: viewport_h.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            self.ctx.IASetInputLayout(None);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.shaders.vs_synth, None);
            self.ctx.PSSetShader(&self.shaders.ps_synth, None);
            self.ctx
                .PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.ctx.PSSetSamplers(
                0,
                Some(&[Some(self.samp_linear.clone()), Some(self.samp_point.clone())]),
            );
            self.ctx.PSSetShaderResources(
                0,
                Some(&[
                    Some(a.prev_color.srv.clone()),
                    Some(a.curr_color.srv.clone()),
                    Some(a.clean_flow_l1.srv.clone()),
                    Some(a.occ_conf_l1.srv.clone()),
                    Some(self.decision_srv.clone()),
                ]),
            );
            prepare_fullscreen_draw_state(&self.ctx, &self.rs_solid_no_scissor, vp, target_rtv);
            self.ctx.Draw(3, 0);

            // Cleanly unbind outputs and SRVs to prevent hazard warnings next frame.
            self.ctx.OMSetRenderTargets(None, None);
            let clear_srvs: [Option<ID3D11ShaderResourceView>; 5] = [None, None, None, None, None];
            self.ctx.PSSetShaderResources(0, Some(&clear_srvs));
        }
        Ok(())
    }

    pub fn read_decision(&self) -> Result<FrameDecisionData, String> {
        unsafe {
            self.ctx
                .CopyResource(&self.decision_staging, &self.decision_buf);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.decision_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map decision_staging failed: {e}"))?;
            let data = *(mapped.pData as *const FrameDecisionData);
            self.ctx.Unmap(&self.decision_staging, 0);
            Ok(data)
        }
    }

    fn update_cb(&self, data: &InterpCBData) {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .ctx
                .Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .is_ok()
            {
                ptr::copy_nonoverlapping(data, mapped.pData as *mut InterpCBData, 1);
                self.ctx.Unmap(&self.cb, 0);
            }
        }
    }

    fn bind_common_cs(&self) {
        unsafe {
            self.ctx
                .CSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.ctx.CSSetSamplers(
                0,
                Some(&[Some(self.samp_linear.clone()), Some(self.samp_point.clone())]),
            );
        }
    }

    fn clear_cs_bindings(&self) {
        unsafe {
            let empty_srvs: [Option<ID3D11ShaderResourceView>; 4] = [None, None, None, None];
            self.ctx.CSSetShaderResources(0, Some(&empty_srvs));
            let empty_uavs: [Option<ID3D11UnorderedAccessView>; 2] = [None, None];
            self.ctx
                .CSSetUnorderedAccessViews(0, 2, Some(empty_uavs.as_ptr()), None);
        }
    }
}

fn groups(dim: u32) -> u32 {
    dim.div_ceil(8)
}

fn create_tex_views(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    uav_needed: bool,
    rtv_needed: bool,
) -> Result<TexViews, String> {
    let mut bind = D3D11_BIND_SHADER_RESOURCE.0;
    if uav_needed {
        bind |= D3D11_BIND_UNORDERED_ACCESS.0;
    }
    if rtv_needed {
        bind |= D3D11_BIND_RENDER_TARGET.0;
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind as u32,
        ..Default::default()
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .map_err(|e| format!("CreateTexture2D({w}x{h}) failed: {e}"))?;
    let tex = tex.unwrap();

    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv)) }
        .map_err(|e| format!("CreateSRV({w}x{h}) failed: {e}"))?;
    let srv = srv.unwrap();

    let uav = if uav_needed {
        let mut u = None;
        unsafe { device.CreateUnorderedAccessView(&tex, None, Some(&mut u)) }
            .map_err(|e| format!("CreateUAV({w}x{h}) failed: {e}"))?;
        u
    } else {
        None
    };

    let rtv = if rtv_needed {
        let mut r = None;
        unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut r)) }
            .map_err(|e| format!("CreateRTV({w}x{h}) failed: {e}"))?;
        r
    } else {
        None
    };

    Ok(TexViews {
        w,
        h,
        tex,
        srv,
        uav,
        rtv,
    })
}

pub fn create_bgra_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    initial_bgra: Option<&[u8]>,
    render_target: bool,
) -> Result<(ID3D11Texture2D, Option<ID3D11RenderTargetView>), String> {
    let mut bind = D3D11_BIND_SHADER_RESOURCE.0;
    if render_target {
        bind |= D3D11_BIND_RENDER_TARGET.0;
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind as u32,
        ..Default::default()
    };
    let init = initial_bgra.map(|bytes| D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr() as *const c_void,
        SysMemPitch: w * 4,
        SysMemSlicePitch: w * h * 4,
    });
    let mut tex = None;
    unsafe {
        device.CreateTexture2D(
            &desc,
            init.as_ref().map(|x| x as *const D3D11_SUBRESOURCE_DATA),
            Some(&mut tex),
        )
    }
    .map_err(|e| format!("CreateTexture2D BGRA({w}x{h}) failed: {e}"))?;
    let tex = tex.unwrap();

    let rtv = if render_target {
        let mut r = None;
        unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut r)) }
            .map_err(|e| format!("CreateRTV BGRA failed: {e}"))?;
        r
    } else {
        None
    };
    Ok((tex, rtv))
}

pub fn read_bgra_texture(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
) -> Result<(u32, u32, Vec<u8>), String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    let w = desc.Width;
    let h = desc.Height;
    let stg_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut stg = None;
    unsafe { device.CreateTexture2D(&stg_desc, None, Some(&mut stg)) }
        .map_err(|e| format!("CreateTexture2D staging failed: {e}"))?;
    let stg = stg.unwrap();
    let res_dst: ID3D11Resource = stg.cast().map_err(|e| format!("{e}"))?;
    let res_src: ID3D11Resource = tex.cast().map_err(|e| format!("{e}"))?;
    unsafe { ctx.CopyResource(&res_dst, &res_src) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { ctx.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|e| format!("Map staging failed: {e}"))?;
    let row_bytes = (w * 4) as usize;
    let mut out = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        unsafe {
            let src_ptr = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
            ptr::copy_nonoverlapping(src_ptr, out.as_mut_ptr().add(y * row_bytes), row_bytes);
        }
    }
    unsafe { ctx.Unmap(&stg, 0) };
    Ok((w, h, out))
}

pub(crate) fn create_sampler_state(
    device: &ID3D11Device,
    filter: windows::Win32::Graphics::Direct3D11::D3D11_FILTER,
) -> Result<ID3D11SamplerState, String> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: filter,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let mut s = None;
    unsafe { device.CreateSamplerState(&desc, Some(&mut s)) }
        .map_err(|e| format!("CreateSamplerState failed: {e}"))?;
    Ok(s.unwrap())
}

pub(crate) fn create_solid_no_scissor_rs(device: &ID3D11Device) -> Result<ID3D11RasterizerState, String> {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        FrontCounterClockwise: false.into(),
        DepthClipEnable: false.into(),
        ScissorEnable: false.into(),
        ..Default::default()
    };
    let mut rs = None;
    unsafe { device.CreateRasterizerState(&desc, Some(&mut rs)) }
        .map_err(|e| format!("CreateRasterizerState failed: {e}"))?;
    Ok(rs.unwrap())
}

/// Resets RS/OM/optional shader stages left behind by external renderers (e.g.
/// libmpv/libplacebo sharing the immediate D3D11 context with an active scissor
/// clipped to the staging texture resolution).
pub(crate) unsafe fn prepare_fullscreen_draw_state(
    ctx: &ID3D11DeviceContext,
    rs_solid_no_scissor: &ID3D11RasterizerState,
    vp: D3D11_VIEWPORT,
    target_rtv: &ID3D11RenderTargetView,
) {
    ctx.GSSetShader(None, None);
    ctx.HSSetShader(None, None);
    ctx.DSSetShader(None, None);
    ctx.OMSetBlendState(None, None, 0xFFFF_FFFF);
    ctx.OMSetDepthStencilState(None, 0);
    ctx.RSSetState(Some(rs_solid_no_scissor));
    ctx.RSSetScissorRects(None);
    ctx.RSSetViewports(Some(&[vp]));
    ctx.OMSetRenderTargets(Some(&[Some(target_rtv.clone())]), None);
}

const SHADER_LINEAR_BLIT: &str = r#"
Texture2D<float4> srcTex : register(t0);
SamplerState sampLinear : register(s0);
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut VSBlit(uint id : SV_VertexID) {
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
float4 PSBlit(VSOut i) : SV_Target0 {
    return float4(srcTex.SampleLevel(sampLinear, i.uv, 0).rgb, 1.0);
}
"#;

/// Lightweight bilinear stretch pass used during fallback when the swapchain
/// viewport differs from the mpv staging texture (e.g. >1920px fullscreen
/// startup before compute optical-flow shaders finish warming up).
pub struct LinearBlitPass {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    samp_linear: ID3D11SamplerState,
    rs_solid_no_scissor: ID3D11RasterizerState,
}

impl LinearBlitPass {
    pub fn new(device: ID3D11Device, ctx: ID3D11DeviceContext) -> Result<Self, String> {
        let compiler = D3DCompilerDll::load()?;
        let vs_blob = compiler.compile_hlsl(SHADER_LINEAR_BLIT, "LinearBlit.hlsl", "VSBlit", "vs_5_0")?;
        let ps_blob = compiler.compile_hlsl(SHADER_LINEAR_BLIT, "LinearBlit.hlsl", "PSBlit", "ps_5_0")?;
        let mut vs = None;
        unsafe { device.CreateVertexShader(&vs_blob, None, Some(&mut vs)) }
            .map_err(|e| format!("CreateVertexShader(LinearBlit) failed: {e}"))?;
        let mut ps = None;
        unsafe { device.CreatePixelShader(&ps_blob, None, Some(&mut ps)) }
            .map_err(|e| format!("CreatePixelShader(LinearBlit) failed: {e}"))?;
        let samp_linear = create_sampler_state(&device, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?;
        let rs_solid_no_scissor = create_solid_no_scissor_rs(&device)?;
        Ok(Self {
            device,
            ctx,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            samp_linear,
            rs_solid_no_scissor,
        })
    }

    pub fn blit_to_rtv(
        &self,
        src: &ID3D11Texture2D,
        target_rtv: &ID3D11RenderTargetView,
        viewport_x: f32,
        viewport_y: f32,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Result<(), String> {
        let mut srv = None;
        unsafe {
            self.ctx.OMSetRenderTargets(None, None);
            self.device.CreateShaderResourceView(src, None, Some(&mut srv))
        }
        .map_err(|e| format!("CreateSRV(LinearBlit) failed: {e}"))?;
        let srv = srv.unwrap();
        let vp = D3D11_VIEWPORT {
            TopLeftX: viewport_x,
            TopLeftY: viewport_y,
            Width: viewport_w.max(1.0),
            Height: viewport_h.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            self.ctx.IASetInputLayout(None);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx
                .PSSetSamplers(0, Some(&[Some(self.samp_linear.clone())]));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cb_and_decision_struct_sizes_match_hlsl() {
        assert_eq!(std::mem::size_of::<InterpCBData>(), 128);
        assert_eq!(std::mem::size_of::<FrameDecisionData>(), 32);
    }

    #[test]
    fn active_uv_bounds_match_16_9_and_9_16_fit_cover_stretch() {
        // 9:16 portrait inside 16:10 (1920x1200) fullscreen Fit -> centered pillarboxed 35.16% strip
        let s9 = compute_active_uv_spec(1080, 1920, 1920, 1200, AspectMode::Fit);
        assert_eq!(s9.portrait_factor, 1.0);
        assert_eq!((s9.sub_band_start_y, s9.sub_band_end_y), (0.62, 0.94));
        let w_frac = s9.uv_max[0] - s9.uv_min[0];
        assert!((w_frac - 0.3515625).abs() < 0.002);
        assert_eq!(s9.uv_min[1], 0.0);
        assert_eq!(s9.uv_max[1], 1.0);

        // 16:9 landscape inside 16:10 fullscreen Fit -> top/bottom 5% black bars excluded
        let s16 = compute_active_uv_spec(1920, 1080, 1920, 1200, AspectMode::Fit);
        assert_eq!(s16.portrait_factor, 0.0);
        assert_eq!((s16.sub_band_start_y, s16.sub_band_end_y), (0.74, 0.98));
        assert!((s16.uv_min[1] - 0.05).abs() < 0.002);
        assert!((s16.uv_max[1] - 0.95).abs() < 0.002);

        // Cover / Stretch always span full [0, 1] active UVs while keeping portrait subtitle band
        let s9_cover = compute_active_uv_spec(1080, 1920, 1920, 1200, AspectMode::Cover);
        assert_eq!(s9_cover.uv_min, [0.0, 0.0]);
        assert_eq!(s9_cover.uv_max, [1.0, 1.0]);
        assert_eq!(s9_cover.portrait_factor, 1.0);
    }

    #[test]
    fn mode_resolution_respects_overrides_and_kind() {
        assert_eq!(
            InterpContentMode::from_modes("auto", "comic"),
            InterpContentMode::Comic
        );
        assert_eq!(
            InterpContentMode::from_modes("auto", "anime"),
            InterpContentMode::Anime
        );
        assert_eq!(
            InterpContentMode::from_modes("auto", "drama"),
            InterpContentMode::Drama
        );
        assert_eq!(
            InterpContentMode::from_modes("anime", "drama"),
            InterpContentMode::Anime
        );
    }
}
