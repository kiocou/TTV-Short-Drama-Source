//! 短剧/漫剧的 RTX VSR 兼容播放层。
//!
//! 这里不调用 NVIDIA API。实测（同一段 1920x1080 真实漫剧）是：
//! HEVC 直连或 HEVC + MSE 都不会触发 `ToggleNvidiaVpSuperResolution(on=true)`，
//! 只把编码改成 H.264 后同一 WebView2/同一窗口尺寸稳定触发。
//! 因此本模块的职责只是把源流转成 H.264 分片流，让驱动满足 VSR 的条件。
//!
//! 产物放在 `<data>/short-drama-cache/rtx-vsr/<session>/`，以 2 秒 fMP4
//! 分片边转边播；进程退出、换集或调用 stop 时删除，不把临时 HLS 当永久缓存。
//! 原始 URL 由调用方保存为 `backupUrl`，转码进程失败时仍可回退播放。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{atomic::Ordering, Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::media_task::{Cancellation, Registry, Subscription};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

#[derive(Clone)]
struct ServerInfo {
    port: u16,
    token: String,
}

struct Job {
    directory: PathBuf,
    task: Subscription<()>,
    ///
    /// 这条会话的产物是否值得**保留**为缓存。
    ///
    /// 红果短剧的整集 HLS 是「按 vid 转一次、之后任意次都能直接播」的产物（实测整集
    /// 转码只要 5.6 秒，但没必要每集都重转），所以它的目录必须跨会话留存；而 guo /
    /// 公开直链那种按 session 建的临时转码目录用完即弃。
    ///
    /// `false` 时最后一个订阅离开后删目录；`true` 时保留产物，
    /// 由缓存预算（`evict_channels_to_budget`）负责回收。
    persist: bool,
}

fn server_info() -> &'static OnceLock<ServerInfo> {
    static SERVER: OnceLock<ServerInfo> = OnceLock::new();
    &SERVER
}

fn jobs() -> &'static Mutex<HashMap<u64, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<u64, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn transcode_tasks() -> &'static Registry<()> {
    static TASKS: OnceLock<Registry<()>> = OnceLock::new();
    TASKS.get_or_init(Registry::default)
}

fn pending_sessions() -> &'static Mutex<HashMap<u64, Weak<Cancellation>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, Weak<Cancellation>>>> = OnceLock::new();
    SESSIONS.get_or_init(Mutex::default)
}

const PREWARM_SESSION_BASE: u64 = 1_000_000;
const PREWARM_SESSION_SPAN: u64 = 4096;

fn is_prewarm_session(id: u64) -> bool {
    (PREWARM_SESSION_BASE..PREWARM_SESSION_BASE + PREWARM_SESSION_SPAN).contains(&id)
}

fn stopped_sessions() -> &'static Mutex<HashSet<u64>> {
    static STOPPED: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    STOPPED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn mark_session_stopped(id: u64) {
    if id == 0 {
        return;
    }
    let mut guard = stopped_sessions()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if guard.len() >= 512 {
        guard.clear();
    }
    guard.insert(id);
}

fn clear_session_stopped(id: u64) {
    if id == 0 {
        return;
    }
    if let Ok(mut guard) = stopped_sessions().lock() {
        guard.remove(&id);
    }
}

fn is_session_stopped(id: u64) -> bool {
    if id == 0 {
        return false;
    }
    stopped_sessions()
        .lock()
        .map(|guard| guard.contains(&id))
        .unwrap_or(false)
}

pub fn has_pending_foreground() -> bool {
    pending_sessions()
        .lock()
        .map(|guard| {
            guard.iter().any(|(&id, weak)| {
                !is_prewarm_session(id) && weak.upgrade().is_some_and(|token| !token.is_cancelled())
            })
        })
        .unwrap_or(false)
}

pub fn has_active_foreground() -> bool {
    // 只有仍在转码/封包中的前台任务才阻塞预热；已收工的播放在途会话只占槽位、不再抢 CPU/IO。
    let job_active = jobs()
        .lock()
        .map(|guard| {
            guard
                .iter()
                .any(|(&id, job)| !is_prewarm_session(id) && !job.task.is_finished())
        })
        .unwrap_or(false);
    if job_active {
        return true;
    }
    has_pending_foreground()
}

/// 停止所有背景预转任务（例如切换到独立播放器或前台起播让路）。
pub fn stop_prewarms() {
    stop_prewarms_except(None);
}

fn stop_prewarms_except(keep_dir: Option<&std::path::Path>) {
    let mut ids: Vec<u64> = Vec::new();
    let jobs_guard = jobs().lock().ok();
    if let Ok(guard) = pending_sessions().lock() {
        for &id in guard.keys() {
            if !is_prewarm_session(id) {
                continue;
            }
            let same_dir = keep_dir.is_some_and(|dir| {
                jobs_guard
                    .as_ref()
                    .and_then(|jobs_map| jobs_map.get(&id))
                    .is_some_and(|job| job.directory == dir)
            });
            if !same_dir {
                ids.push(id);
            }
        }
    }
    if let Some(guard) = jobs_guard.as_ref() {
        for (&id, job) in guard.iter() {
            if is_prewarm_session(id) && keep_dir != Some(job.directory.as_path()) {
                ids.push(id);
            }
        }
    }
    drop(jobs_guard);
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        stop(id);
    }
}

fn stop_competing_for_foreground(active_id: u64, keep_dir: &std::path::Path) {
    stop_prewarms_except(Some(keep_dir));
    let other_foreground: Vec<u64> = jobs()
        .lock()
        .map(|guard| {
            guard
                .iter()
                .filter(|(&id, job)| {
                    id != active_id && !is_prewarm_session(id) && job.directory != keep_dir
                })
                .map(|(&id, _)| id)
                .collect()
        })
        .unwrap_or_default();
    for id in other_foreground {
        stop(id);
    }
}

/// 在签名之前登记，stop(session_id) 即可取消尚未进入转码的请求。
pub struct SessionRequest {
    id: u64,
    pub cancel: Arc<Cancellation>,
}

pub fn session_request(id: u64) -> SessionRequest {
    clear_session_stopped(id);
    let mut sessions = pending_sessions()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let cancel = sessions
        .get(&id)
        .and_then(Weak::upgrade)
        .filter(|token| !token.is_cancelled())
        .unwrap_or_else(|| Arc::new(Cancellation::default()));
    sessions.insert(id, Arc::downgrade(&cancel));
    SessionRequest { id, cancel }
}

impl Drop for SessionRequest {
    fn drop(&mut self) {
        let mut sessions = pending_sessions()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if Arc::strong_count(&self.cancel) == 1
            && sessions
                .get(&self.id)
                .is_some_and(|token| token.ptr_eq(&Arc::downgrade(&self.cancel)))
        {
            sessions.remove(&self.id);
        }
    }
}

/// 未取得首片交付权时离开，按注册身份摘掉本会话，不能取消同会话后来接管的产物。
struct PendingJob {
    session_id: u64,
    directory: PathBuf,
    task: Subscription<()>,
    persist: bool,
    active: bool,
}

impl PendingJob {
    /// 启动已成功把播放地址交给调用方，解除取消回滚清理守卫。
    fn disarm(mut self) {
        self.active = false;
    }
}

fn schedule_temp_dir_cleanup(task: &Subscription<()>, directory: PathBuf) {
    let finished = task.completion();
    tauri::async_runtime::spawn(async move {
        finished.await;
        // Windows 下 ffmpeg 退出释放 current_dir 目录句柄存在短暂内核延迟，短重试避免残留临时目录。
        for attempt in 0..12 {
            let still_used = jobs()
                .lock()
                .map(|guard| guard.values().any(|job| job.directory == directory))
                .unwrap_or(true);
            if still_used {
                return;
            }
            match std::fs::remove_dir_all(&directory) {
                Ok(()) => return,
                Err(_) if !directory.exists() => return,
                Err(_) if attempt + 1 < 12 => tokio::time::sleep(Duration::from_millis(40)).await,
                Err(_) => return,
            }
        }
    });
}

impl Drop for PendingJob {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let removed = jobs()
            .lock()
            .map(|mut guard| {
                if guard.get(&self.session_id).is_some_and(|job| {
                    job.directory == self.directory && job.task.same_task(&self.task)
                }) {
                    guard.remove(&self.session_id)
                } else {
                    None
                }
            })
            .unwrap_or(None);
        drop(removed);
        if !self.persist
            && !jobs()
                .lock()
                .map(|guard| guard.values().any(|job| job.directory == self.directory))
                .unwrap_or(true)
        {
            schedule_temp_dir_cleanup(&self.task, self.directory.clone());
        }
    }
}

/// 交出地址前最多等多久首段落地。理由见 `start()` 里的等待循环。
/// ⚠️ 2026-11-13 由 1500ms 提到 8000ms。
///
/// 现场（开发态日志 `.app-data/ttv-playback.log`，真实红果 1080p HEVC 源）：
///   60.054 → 67.78  播放器连吃 40 次 503（服务端行为正确：清单里还没有 #EXTINF）
///   67.791          manifestLoadError fatal=true
///   67.792          整集 HLS 播放失败 → 回退整集文件
/// 也就是首片要 **7.8 秒以上**才落地，而播放器的清单重试预算（60 次）在 7.8 秒耗尽。
///
/// 时间线是串行的：后端按住 → 前端才 attach → 播放器才开始重试清单。
/// 与其让播放器用「重试」去碰运气，不如后端直接把「首片已落地」当成返回条件：
/// 快源仍然 1 秒返回（另一集实测 ready=true 等待=1085ms），慢源才用满这 8 秒，
/// 而一旦返回，播放器拉一次清单就能出画 —— 不再有 fatal 与那次整集回退。
/// 2026-11-13 最终定为 1200ms（曾试过 1500 → 8000 → 回到 1200）。
///
/// 8 秒那版是为了「等首片」，但实测这条 HLS 链路在本机产不出首片：
/// 即便 `ready=true`（后端自己读到 #EXTINF），本地服务随后仍连续回 503，
/// 播放器 11 秒后 fatal —— 那 19 秒是纯白等，之后还要回退整集文件。
/// 既然回退路径已经能 4.9 秒出画（worker `-c:v copy` + 后台迁移），
/// HLS 只保留「已转好的产物立刻复用」这条快路径：没有产物就尽快交回外层。
const READY_WAIT: Duration = Duration::from_millis(1200);

fn media_root() -> PathBuf {
    crate::short_drama_app::cache_dir().join("rtx-vsr")
}

/// 产物身份（ArtifactId）—— **一份 HLS 产物目录的不可变名字**。
///
/// ## 为什么需要它（2026-10-20）
///
/// 旧 URL 形态是 `/rtx/{session_id}/index.m3u8`：唯一的身份是**播放会话号**，
/// 而会话号会被反复复用。实测日志里同一个会话号 104 先后指向两份完全不同的
/// 产物：
///
/// ```text
/// [+ 54.6s] 流式开播 vid=7696136695548234777 会话=104
/// [+200.9s] 流式开播 vid=7679731146971696152 会话=104   ← 换集，同一会话号
/// ```
///
/// 后端 `register_reused(104, 新目录)` 会把 104 从旧目录摘走、挂到新目录上。
/// 于是任何仍持有旧 URL 的请求（旧 hls 实例、画中画窗口、自愈续体）在换集后
/// 都会**静默读到另一集的内容**，或者命中一个不存在的分片。
///
/// 目录名本身就是天然不可变的身份（`vid-{vid}` 按内容建、`{session}-{uuid}`
/// 带随机后缀），所以直接把它放回 URL 即可：
///
/// ```text
/// /rtx/{session_id}/{artifact}/index.m3u8
///             ^^^^^^^^^^ 不可变：一旦生成，永远指向同一份产物
/// ```
///
/// `session_id` 保留在路径里只为兼容既有日志与旧地址，**不再参与目录解析**。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactId(String);

impl ArtifactId {
    /// 从目录名构造。只接受 `[A-Za-z0-9._-]` 且非空、不以点开头。
    ///
    /// 这条校验是**安全边界**，不是格式洁癖：`artifact` 直接来自 HTTP 路径，
    /// 若不校验就能用 `..%2f` 之类跳出 `media_root()` 读到任意文件。
    fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > 128 {
            return None;
        }
        // 不允许 `.` / `..`，也不允许以点开头（隐藏文件、相对路径跳转）。
        if raw.starts_with('.') {
            return None;
        }
        let ok = raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        // 连续的点同样要挡：`a..b` 在 Windows 上会被折叠。
        if !ok || raw.contains("..") {
            return None;
        }
        Some(Self(raw.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    /// 解析成绝对目录。**只做拼接，不做存在性判断**（调用方按需检查）。
    fn directory(&self) -> PathBuf {
        media_root().join(&self.0)
    }

    /// 由目录反推身份：只接受直接位于 `media_root()` 下的一级子目录。
    fn from_directory(directory: &std::path::Path) -> Option<Self> {
        let name = directory.file_name()?.to_str()?;
        let candidate = Self::parse(name)?;
        // 必须是 media_root 的直接子目录，避免把深层路径当身份。
        let expected = media_root().join(&candidate.0);
        (expected == directory).then_some(candidate)
    }
}

/// 本地 HLS 播放地址。
///
/// 形态为 `/rtx/{session_id}/{artifact}/index.m3u8`：`artifact` 决定读哪份产物，
/// `session_id` 只是日志与兼容用途。清单里 `init.mp4` 与 `seg-*.m4s` 由
/// `rewrite_local_playlist` 补成**同一目录下的相对路径**，因此它们天然共用同一个
/// ArtifactId —— 不会出现"清单来自 A、分片来自 B"的串线。
fn artifact_url(port: u16, session_id: u64, artifact: &ArtifactId, token: &str) -> String {
    format!(
        "http://127.0.0.1:{port}/rtx/{session_id}/{}/index.m3u8?token={token}",
        artifact.as_str()
    )
}

/// 只处理确实能由 ffmpeg 输入的 http(s) 源。asset/本地文件由红果 worker
/// 直接产出 H.264，不需要在这里二次转码。
pub fn needs_enhancement(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// H.264 视频编码参数：能上硬件就上硬件。
///
/// **为什么这件事对「边转边播」是决定性的**：流式链路的可用性取决于"转码速度
/// 能否跟上播放速度"。CPU 编码（libx264 ultrafast）在 1080p 上要吃掉好几个核，
/// 一旦与整集预取抢起来就会跟不上，播放器随即撞上"分片还没生成"→ 重试耗尽 →
/// 报错。NVENC 把这块负载整体搬到显卡的编码单元，CPU 让出来给解密与下载。
///
/// 实测这台机器（RTX 5060 Laptop）：NVENC 探测约 0.3 秒（每个进程只探一次），
/// 编码 1080p 从 libx264 ultrafast 的 ~1.0 秒/2 秒分段降到 ~0.4 秒。
///
/// 探测失败一律回落 libx264：兼容性优先，VSR 只要求"是 H.264"，不要求谁编的。
/// 码率上限（三档共用）。
///
/// **为什么恒定质量模式还要限码率**：`-cq` / `-crf` 的语义是"给你这个质量，码率
/// 自己想办法"——在源流本身码率很低、画面又简单的短剧上，它会把输出**放到源之上**。
/// 本机实测（同一集、92.7 秒正片）：NVENC cq26 不加上限时输出 **61.3MB**，而这一集
/// 的源流总共只有 10.2MB。补上 `-maxrate` / `-bufsize` 后体积降到 **13.5MB**，
/// 整集转码反而快 243ms（要写盘的字节少了一个数量级），画质仍由 cq26 决定，无可见变化。
///
/// 1100k 的由来：该源平均约 0.9Mbps，留约 20% 余量，快镜头不卡码。
///
/// ⚠️ **已知边界（本轮刻意保留）**：1100k 是按**这一条源**标定的固定值，而三档 × 两条
/// 链路共用它。对码率明显更高的源（高码率档、guo 直链、公开直链），它会把输出硬压到
/// 1100k，表现是"某些集清楚、某些集发糊"。之所以先固定：收益可测、行为可预期；
/// 按源码率自适应（如 `min(源码率 × 1.2, 2500k)`）要先探测输入码率，那是一次
/// 独立改动，本轮不做。
/// 按**源档码率**算码率上限与缓冲窗口（单位 kbps）。
///
/// **为什么不能再用固定值**：上一版是固定 `-maxrate 1100k -bufsize 2200k`，而它是按
/// 单条源（约 0.9Mbps）标定的，源档码率的实际分布很散（实测同一部剧不同集 300kbps
/// ~ 2.5Mbps）。对高于它的源，这等于把 1080p 硬压下去 —— 相对源画质直接掉档：同一段
/// 真人短剧 20 秒、源码率 733kbps，固定 1100k 的 SSIM 只有 **0.9814**，而按源自适应的
/// cq23 + preset p4 是 **0.9917**（体积只多 25%，编码耗时 +24%）。用户看到的"转码后
/// 画质下降很多"就是这个上限造成的。
///
/// 倍率由来（实测标定，源是 H.265）：
///   * H.265 → H.264 同画质经验上要 1.4~1.6× 码率，取 1.4；
///   * 再乘 1.6 给运动与复杂画面的瞬时峰值留余量；
///   * bufsize 取上限的 2 倍（VBV 约束过紧会在快镜头处直接压出块）。
///
/// **2.24× → 3.0×（2026-10-29，用户报"开了 VSR 噪点多"）**：红果源普遍只有
/// 700~900kbps 且带压缩噪点，2.24× 的上限把 cq 卡死 —— 实测同一集 10 秒：
/// 上限 1642k 时 2.2MB / SSIM 0.9943，上限 3000k 时 4.0MB / SSIM 0.9962，
/// 编码耗时 16.5x vs 16.1x（**恒定质量模式下放宽上限几乎不花时间**）。
/// 取 3.0× 而不是完全不设限：不设限时体积到了 5.5MB/10 秒（≈4.4Mbps），
/// 体积再往上换来的 SSIM 只有 0.0009，不划算。
///
/// 取不到源码率时（guo / 公开直链没有档位信息）给一组中性默认：不设上限会被恒定质量
/// 模式放到源的 7 倍，设太紧又会糊，3000k 覆盖 1080p 短剧的常见上限。
pub(crate) fn rate_limits(source_kbps: Option<u32>) -> (u32, u32) {
    let source = match source_kbps {
        Some(value) if value >= 100 => value,
        _ => return (3000, 6000),
    };
    let maxrate = ((source as f64) * 3.0).round() as u32;
    // 下限只用来挡"离谱的小值"（上游码率字段脏数据），**绝不能反过来把低码率源抬高**：
    // 300kbps 的源按 2.24× 是 672k，若下限取 900k，输出就变成源码率的 3 倍 ——
    // 体积膨胀而画质零收益，正是"恒定质量模式把输出放到源之上"在小码率源上复现
    // （审查 R6）。所以再取一次 min(上限, 源码率 × 3)。
    let maxrate = maxrate
        .clamp(150, 8000)
        .min(source.saturating_mul(3).max(150));
    (maxrate, maxrate * 2)
}

/// 把 `rate_limits` 的结果铺成 ffmpeg 参数。给**不经过 `video_encoder_args`** 的
/// 调用点用（`ensure_h264_cache` 的旧缓存迁移、worker 侧经环境变量取同一组值）。
pub(crate) fn rate_lines(source_kbps: Option<u32>) -> Vec<String> {
    let (maxrate, bufsize) = rate_limits(source_kbps);
    vec![
        "-maxrate".to_owned(),
        format!("{maxrate}k"),
        "-bufsize".to_owned(),
        format!("{bufsize}k"),
    ]
}

/// 硬件档参数（NVIDIA NVENC）。
///
/// `-preset p4` + `-cq 23`：上一版是 `p1` + `cq 26`（最快档 + 较低质量目标）。实测同一段
/// 20 秒 1080p 真人短剧（SSIM 相对源、上限都按源档自适应）：`p1 + cq26 + 固定 1100k` 是
/// **0.9814**（1084ms），`p4 + cq23 + 按源自适应` 是 **0.9917**（1345ms）。
/// 编码耗时多 24%，整集仍是 5 秒级（61 秒正片），换来的是肉眼可见的那一档画质。
/// NVENC 的 p1→p4 差距主要在运动估计与码率分配，GPU 上多花的绝对时间远小于 CPU 档位。
///
/// 码率上限**不在这里**：它必须按源档码率算（见 `rate_limits`），由 `video_encoder_args`
/// 追加 —— 固定值就是上一版画质掉档的根因。
const NVENC_ARGS: &[&str] = &[
    "-c:v",
    "h264_nvenc",
    "-preset",
    // p2 而非 p4：整集实测（168 秒 1080p 短剧、含真实码率上限 1407k/2814k）
    //   p4 → 23.4 秒（7.2 倍实时），30.1MB
    //   p2 → 12.7 秒（13.2 倍实时），30.5MB
    // 快 1.84 倍，体积几乎不变，p2 相对 p4 的 SSIM 是 0.97（Y 0.96）。
    //
    // 为什么速度在这里等于功能：整集 HLS 是边转边播，**可拖动范围 = 已转码时长**。
    // p4 时用户出画后要等约 22 秒才能拖到片尾，期间拖动会被"可寻址末端"挡住
    // （日志实证：`拖动到未转码区域 目标=95.5s 可寻址末端=64.1s`）。p2 把这个
    // 窗口缩到约 12 秒，是当前架构下改善"进度条拖不动"最直接的手段。
    //
    // 代价是运动估计精度略降。上面那次 SSIM 是"p2 相对 p4"，不是"相对源"；
    // 短剧以对话为主、运动少，实际观感差异远小于该数字。若哪天要退回画质优先，
    // 把这里改回 p4 即可，其余参数无需变动。
    "p2",
    "-cq",
    // 20 而不是 23：实测同一集 10 秒，cq20 相对源 SSIM 0.9962（cq23 0.9943），
    // 编码耗时 16.1x vs 16.5x —— NVENC 在恒定质量档上的耗时几乎不随 cq 变化。
    "20",
    // CPU 轻降噪（hqdn3d）在实测中会把 1080p NVENC 转码拖慢 2.36×（462ms → 1092ms / 12s），
    // VSR 超分本身带去噪与锐化；去掉逐帧 CPU 滤波，换取首屏与可寻址进度翻倍推进。
    "-pix_fmt",
    "yuv420p",
    "-profile:v",
    "high",
    "-g",
    "24",
    "-keyint_min",
    "15",
    "-sc_threshold",
    "0",
];

/// 软件档（CPU，核数够）：libx264 **veryfast**。
///
/// ⚠️ 上一版是 `ultrafast`，那是画质最差的档 —— 实测同一 20 秒 1080p 片段：
/// `ultrafast + crf20 + 固定 1100k` 的 SSIM 只有 **0.9431**（比硬件档旧参数还低 0.04，
/// 超快档会牺牲大量运动估计与亚像素精度），而 `veryfast + crf20 + 按源自适应` 是
/// **0.9881**、体积反而更小（2.54MB vs 2.88MB），代价是编码耗时 1489ms → 2276ms（+53%）。
/// 也就是说：这一档以前"又慢又糊"。低配档另有 720p 兜底，这里按画质优先取 veryfast。
///
/// 码率上限同样由 `rate_limits` 按源档算，不写死在这里。
const X264_ARGS: &[&str] = &[
    "-c:v",
    "libx264",
    "-preset",
    "veryfast",
    "-crf",
    // 19：与硬件档 cq20 对齐观感，CPU 档的码率上限同样按源 3.0× 走。
    "19",
    "-tune",
    "zerolatency",
    "-pix_fmt",
    "yuv420p",
    "-profile:v",
    "high",
    "-g",
    "24",
    "-keyint_min",
    "15",
    "-sc_threshold",
    "0",
];

/// 软件档参数：给"要产出一份普通 H.264 mp4"的地方复用（旧缓存迁移那条链路用）。
///
/// **为什么需要暴露它**：`short_drama_app::ensure_h264_cache` 此前自己写了一份 libx264
/// 参数，与本表漂移了——少了 `-profile:v high`、没有码率上限、也没有关键帧约束
/// （libx264 默认 keyint 250 帧）。症状是同一部剧里"迁移出来的老集"与"新解析的集"
/// seek 粒度、体积、首帧时间都不一样，正是 `worker.py` 注释点名的那种极难定位的差异。
/// 参数表只留一份，谁要产 H.264 都从这里取。
pub(crate) fn x264_args() -> &'static [&'static str] {
    X264_ARGS
}

/// 低配档：**降分辨率**，同时只把预设降**一档**（`superfast`）而不是降到最差档。
///
/// 理由（本机实测）：只降码率对 CPU 几乎无帮助 —— 1080p cq26→cq34 只从 5499ms 降到
/// 5429ms；而降到 720p 直接到 4027–4691ms。解码像素数决定解码与编码两个阶段的算术量，
/// 减半分辨率等于把这块砍到四分之一。像素少了一半之后，`superfast` 的速度优势变小、
/// 而画质劣势依然存在，所以预设只从 veryfast 退一档、crf 从 20 放到 22，不再用画质最差
/// 的 ultrafast（那一档实测 SSIM 只有 0.9431，用户直接能看到糊）。
const LOW_TIER_ARGS: &[&str] = &[
    "-c:v",
    "libx264",
    "-preset",
    "superfast",
    "-crf",
    "22",
    "-tune",
    "zerolatency",
    "-vf",
    "scale=-2:720",
    "-pix_fmt",
    "yuv420p",
    "-profile:v",
    "high",
    "-g",
    "24",
    "-keyint_min",
    "15",
    "-sc_threshold",
    "0",
];

/// HLS 打包参数（增强链路与短剧流式链路共用同一套节奏）。
///
/// `-hls_init_time 1`：**只对第一个分片生效**的切分时长目标值。不给它时，首片要等满
/// `-hls_time`（2 秒）才写得出来；给了 1 秒，第一片理论上 1 秒就能交给播放器，之后
/// 恢复 2 秒节奏。
///
/// ⚠️ 实测边界必须写清：**本地源**上首片完成时间 652ms → 465ms；而在**真实网络源**上
/// （15fps、`-g 48` ⇒ 关键帧间隔 3.2 秒）**没有可测出的收益** —— HLS 分片只能从关键帧
/// 开始切，`hls_time` / `hls_init_time` 比 GOP 短时形同虚设，首片仍是 3.2 秒一片。
/// 它是"在 GOP ≤ `hls_time` 的源上才会显形"的改动，本身不带来坏处，所以照留。
///
/// `-hls_list_size 0`：保留全部已生成分片，已转好的部分可自由 seek。
///
/// **这里没有 `-force_key_frames`**：它对 NVENC 无效，对 libx264 是与 `-g` 并行的
/// 第二条规则 —— 详见调用点的长注释。
const HLS_PACKAGING_ARGS: &[&str] = &[
    "-f",
    "hls",
    "-hls_time",
    "2",
    "-hls_init_time",
    "1",
    "-hls_list_size",
    "0",
    // `-hls_playlist_type event`：**这一条决定"拖动进度条会不会跳回原处"**。
    //
    // 不写它时，转码中的清单既没有 `#EXT-X-ENDLIST` 也没有 `#EXT-X-PLAYLIST-TYPE`，
    // hls.js 只能按 **live 流**处理 —— 于是它会持续跟随 live edge（最新分片）。
    // 实测症状（ttv-playback.log）：
    //   落位到 111.4s        ← 用户点的位置，代码确实写进去了
    //   timeupdate 位置=121.52s  ← 实际播放在 live edge，跳了 10 秒
    // 加上 event 之后清单带 `#EXT-X-PLAYLIST-TYPE:EVENT`，hls.js 就知道
    // "这是只增不减的点播流"，seek 到已转码部分不会被拉回 edge。
    // 转码结束时 ffmpeg 仍会补 `#EXT-X-ENDLIST`，最终形态就是完整 VOD。
    "-hls_playlist_type",
    "event",
    "-hls_flags",
    "independent_segments+temp_file",
    "-hls_segment_type",
    "fmp4",
];

/// 软件档专用的 HLS 打包参数：与上面**只差**一条 `-force_key_frames`。
///
/// **为什么只给软件档**：`-force_key_frames` 只对软件编码器生效，NVENC 不认这个表达式
/// （见调用点的长注释）。没有它时软件档的关键帧只能落在 `-g 48` 上，15fps 源就是 3.2 秒
/// 一片。实测（libx264 ultrafast、同一集、各 3 次取中位，两组只差这一个参数）：
///
/// | 组 | 首片时长 | 首片体积 | 首片完成 | 整集 |
/// |---|---|---|---|---|
/// | 无 force | 3.2 秒 | 0.71MB | 1028ms | 9218ms |
/// | 有 force | **2.0 秒** | **0.53MB** | 958ms | 8277ms |
///
/// 也就是说：首片覆盖时长与 `hls_time` 对齐、体积小 25%（下载更快），而首片完成时间
/// 的差别在噪声内。这条实测推翻了我先前的判断（"删掉它对软件档也无害"）——它确实有用，
/// 只是**对硬件档没用**。
const HLS_PACKAGING_ARGS_X264: &[&str] = &[
    "-f",
    "hls",
    "-hls_time",
    "2",
    "-hls_init_time",
    "1",
    "-hls_list_size",
    "0",
    // 与硬件档同理：没有 event 类型时 hls.js 会把转码中的清单当 live 流追 edge。
    "-hls_playlist_type",
    "event",
    "-hls_flags",
    "independent_segments+temp_file",
    "-hls_segment_type",
    "fmp4",
    "-force_key_frames",
    "expr:gte(t,n_forced*2)",
];

/// 按档位选 HLS 打包参数：硬件档不带 `-force_key_frames`（无效参数只会有误导性），
/// 软件两档带上（它真的生效，见 `HLS_PACKAGING_ARGS_X264` 的实测表）。
fn hls_packaging_args(ffmpeg: &std::path::Path) -> &'static [&'static str] {
    if detect_encoder_tier(ffmpeg) == EncoderTier::Hardware {
        HLS_PACKAGING_ARGS
    } else {
        HLS_PACKAGING_ARGS_X264
    }
}

/// 增强输出模式：重编码 H.264（VSR 开启）或零转码视频流直拷贝解密封装（VSR 关闭）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    H264,
    Copy,
}

/// 零转码快速封装的 HLS 参数：1 秒切片 + `temp_file` 原子落盘，降低首屏与抢读错误。
const COPY_HLS_PACKAGING_ARGS: &[&str] = &[
    "-f",
    "hls",
    "-hls_time",
    "1",
    "-hls_list_size",
    "0",
    "-hls_playlist_type",
    "event",
    "-hls_flags",
    "independent_segments+temp_file",
    "-hls_segment_type",
    "fmp4",
];

/// 网络源的断流重连参数（输入选项，必须排在 `-i` 之前）。
///
/// ## 为什么需要（2026-10-19，「播着播着转码失败」）
///
/// 日志里的真实退出样本：
///
/// ```text
/// [vsr] 转码进程退出 会话=1000188 状态=exit code: 0xffffff76
///       stderr=[tcp @ ...] Connection to tcp://v5-se-bjtc-reading-video-a.qznovelvod.com
/// [vsr] 起转失败 会话=111 状态=exit code: 0xa7a7ca08
///       stderr=[in#0] Error opening input: Server returned 5XX Server Error
/// ```
///
/// 红果源是 CDN 直链，中途断连 / 5XX 是常态。ffmpeg 默认把"连接断了"直接当成
/// **输入结束** —— 进程退出、产物停在半截且没有 `#EXT-X-ENDLIST`，播放器于是播到
/// 已写出的最后一个分片就停住。用户的处置只能是重新打开这一集：那会重新走
/// `start_video_cache`，发现产物不完整 → 整集重转。所以现象被描述成
/// 「要重新从头播放才能加载完后面」。
///
/// `-reconnect_streamed 1` 让它在**流式输入**（mp4/HLS 直链都属于）断流时重连，
/// 而不是当作 EOF；`-reconnect_delay_max 5` 把重试间隔封顶在 5 秒 ——
/// CDN 切节点通常在这个量级内恢复，再久就该如实判失败（交给上层重转一次）。
///
/// 只对 http(s) 追加：本地文件输入没有这套协议选项，传了只会增加无意义参数。
fn reconnect_args(source_url: &str) -> &'static [&'static str] {
    if source_url.starts_with("http://") || source_url.starts_with("https://") {
        RECONNECT_ARGS
    } else {
        &[]
    }
}

fn network_timeout_args(source_url: &str) -> &'static [&'static str] {
    if source_url.starts_with("http://") || source_url.starts_with("https://") {
        &["-rw_timeout", "15000000"]
    } else {
        &[]
    }
}

fn tls_args(source_url: &str) -> &'static [&'static str] {
    if source_url.starts_with("https://") {
        &["-tls_verify", "0", "-rw_timeout", "15000000"]
    } else {
        &[]
    }
}

const RECONNECT_ARGS: &[&str] = &[
    "-reconnect",
    "1",
    "-reconnect_streamed",
    "1",
    "-reconnect_at_eof",
    "0",
    "-reconnect_delay_max",
    "5",
];

fn video_encoder_args(ffmpeg: &std::path::Path, source_kbps: Option<u32>) -> Vec<String> {
    #[cfg(test)]
    if std::env::var_os("TTV_TEST_TRANSCODE_COPY").is_some() {
        return vec!["-c:v".into(), "copy".into()];
    }
    let tier = detect_encoder_tier(ffmpeg);
    let args = match tier {
        EncoderTier::Hardware => NVENC_ARGS,
        EncoderTier::SoftwareFast => X264_ARGS,
        EncoderTier::SoftwareLow => LOW_TIER_ARGS,
    };
    // 码率上限统一在这里追加（三档共用同一套按源换算），而不是写死进常量 ——
    // 见 `rate_limits` 的实测表：固定值就是上一版画质掉档的根因。
    let mut out: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
    out.extend(rate_lines(source_kbps));
    out
}

/// 硬件能力档位。决定用哪一套编码参数与目标分辨率。
///
/// 用户需求：「如果程序遇到配置低的用户，则自适应」。档位不是"猜"出来的，
/// 而是**实测探测**出来的 —— 与 `nvenc_available` 同一套方法论（真跑一次，
/// 而不是读 `-encoders` 列表，那个在缺卡/驱动过旧时照样会列出编码器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderTier {
    /// 有可用的 NVIDIA 硬件编码：整集 1080p 重编实测约 5.5 秒（92.7 秒正片）。
    Hardware,
    /// 没有硬件编码、但 CPU 核数够（>= 8）：libx264 ultrafast 实测约 11.7 秒。
    SoftwareFast,
    /// 低配机（CPU 核数少）：降到 720p 并要求更低码率，牺牲画质换"能看"。
    SoftwareLow,
}

/// 探测结果落盘文件名（与 NVENC 探测同一套指纹失效机制）。
const TIER_CACHE_FILE: &str = "encoder-tier";

fn tier_cache_path() -> PathBuf {
    crate::short_drama_app::cache_dir().join(TIER_CACHE_FILE)
}

/// 探测结果的**进程内**槽位（与 `nvenc_slot` 同一套做法）。
///
/// 为什么必须有：`detect_encoder_tier` 在热路径上被反复调用（每次开播的
/// `artifact_is_current` → `encoder_fingerprint` → `hls_packaging_args` 各一次，
/// 加上 `video_encoder_args` 自己一次）。只有"落盘缓存"的话，每次都要读一次盘；
/// 而一旦读盘失败或指纹失配，就会退化成**每次开播都真跑一次 ffmpeg 探测**
/// （实测约 1.1 秒）—— 那等于把探测税重新摊回每一集的首屏（审查 R8）。
static TIER_SLOT: OnceLock<std::sync::RwLock<Option<EncoderTier>>> = OnceLock::new();

fn tier_slot() -> &'static std::sync::RwLock<Option<EncoderTier>> {
    TIER_SLOT.get_or_init(|| std::sync::RwLock::new(None))
}

/// 探测本机适合哪一档。**只探测一次**（进程内 + 落盘）。
///
/// 判据与理由：
/// 1. **能上硬件就上硬件**：NVENC 把编码负载整体从 CPU 移走，而 CPU 还要同时
///    做解密与下载 —— 这是本机实测 5.5s vs 11.7s 的差距来源。
/// 2. **没有硬件才看 CPU 核数**：libx264 ultrafast 在 8 核以上实测 11.7 秒
///    （92.7 秒正片，约 8 倍实时），仍然够"边转边播"；核数更少时同步转码会
///    明显跟不上播放进度，此时降分辨率比降码率有效（解码像素数直接减半）。
/// 3. **不确定时取保守档**：探测失败宁可当低配（720p 也能看），也不要让低配
///    用户在"一直转圈"里等。画质差一点是可见的遗憾，卡住不动是体验事故。
pub fn detect_encoder_tier(ffmpeg: &std::path::Path) -> EncoderTier {
    // 0) 进程内已有结论：这是热路径上的第一道（也是绝大多数调用会命中的那道）。
    if let Ok(guard) = tier_slot().read() {
        if let Some(tier) = *guard {
            return tier;
        }
    }
    // 盘上已有结论且指纹匹配：直接读（省掉每次冷启动的探测开销）。
    if let Some(fingerprint) = tier_fingerprint() {
        if let Ok(raw) = std::fs::read_to_string(tier_cache_path()) {
            if let Some((saved, tier)) = raw.trim().split_once(' ') {
                if saved == fingerprint {
                    let parsed = match tier {
                        "hardware" => Some(EncoderTier::Hardware),
                        "software-fast" => Some(EncoderTier::SoftwareFast),
                        "software-low" => Some(EncoderTier::SoftwareLow),
                        _ => None,
                    };
                    if let Some(value) = parsed {
                        crate::trace::log(format!("[vsr] 编码档位：读缓存 = {value:?}"));
                        if let Ok(mut slot) = tier_slot().write() {
                            *slot = Some(value);
                        }
                        return value;
                    }
                }
            }
        }
    }
    let tier = if nvenc_available(ffmpeg) {
        EncoderTier::Hardware
    } else if available_parallelism() >= 8 {
        EncoderTier::SoftwareFast
    } else {
        EncoderTier::SoftwareLow
    };
    crate::trace::log(format!(
        "[vsr] 编码档位探测 = {tier:?}（核数={}）",
        available_parallelism()
    ));
    if let Some(fingerprint) = tier_fingerprint() {
        if let Some(parent) = tier_cache_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tag = match tier {
            EncoderTier::Hardware => "hardware",
            EncoderTier::SoftwareFast => "software-fast",
            EncoderTier::SoftwareLow => "software-low",
        };
        let _ = std::fs::write(tier_cache_path(), format!("{fingerprint} {tag}"));
    }
    if let Ok(mut slot) = tier_slot().write() {
        *slot = Some(tier);
    }
    tier
}

/// 档位缓存的失效指纹：ffmpeg 身份 + **CPU 核数**。
///
/// 核数必须进指纹：换机器（或虚拟机调核）会改变分档，而 ffmpeg 没变。
fn tier_fingerprint() -> Option<String> {
    let ffmpeg = crate::short_drama_app::ffmpeg_path().ok()?;
    let base = ffmpeg_fingerprint(&ffmpeg)?;
    Some(format!("{base}-cpu{}", available_parallelism()))
}

/// 本机可用并行度。取不到时给一个**偏保守**的默认（4），理由见 `detect_encoder_tier`。
fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4)
}

///
/// **必须真跑一次**，不能读 `-encoders` 列表：编译进去的 h264_nvenc 在没有 N 卡、
/// 驱动过旧或编码会话占满时照样会列出来，只有实际初始化才暴露。
/// 这与 worker 侧 `video_encoder_args` 的做法一致（那边探测尺寸用 640x360，
/// 因为 64x64 会被 NVENC 直接拒、给出假阴性）。
/// 进程内探测结论。
///
/// 用 `RwLock<Option<bool>>` 而不是 `OnceLock<bool>`：后者一旦写入就再也改不了，
/// 于是设置页的「清空缓存」（用户换显卡/更新驱动后表达"重新探测"的唯一入口）
/// 只能删掉盘上的文件，进程内那份旧结论仍然生效 —— 除非重启应用（审查 E1）。
static NVENC_AVAILABLE: OnceLock<std::sync::RwLock<Option<bool>>> = OnceLock::new();

fn nvenc_slot() -> &'static std::sync::RwLock<Option<bool>> {
    NVENC_AVAILABLE.get_or_init(|| std::sync::RwLock::new(None))
}

/// 探测结果落盘的文件名（放在缓存根目录）。
///
/// 为什么落盘：探测本身要起一次 ffmpeg（实测 **1133ms**），而它发生在**首屏路径上**
/// —— 每次进程冷启动后的第一次播放都会付这笔税。日志里那段 15.7 秒首屏中它占 1.1 秒，
/// 而结果在同一台机器上不会变（显卡与驱动都不动）。落盘之后只有"第一次安装后首播"
/// 才探测，此后直接读文件。
///
/// 失效条件写清：换显卡、换驱动、升级 ffmpeg 都应重新探测。这里用 ffmpeg 的**文件大小
/// 与 mtime** 做指纹——它是随包资源，升级必然变；显卡/驱动变化本应用无法可靠感知，
/// 用户可以用设置页的"清空缓存"强制重探（`cache_clear` 会连带清掉这个文件）。
const NVENC_CACHE_FILE: &str = "h264-nvenc-capability";

fn nvenc_cache_path() -> PathBuf {
    crate::short_drama_app::cache_dir().join(NVENC_CACHE_FILE)
}

/// 读落盘的探测结论。返回 `None` 表示没有可用记录（需重新探测）。
fn read_nvenc_cache(ffmpeg: &std::path::Path) -> Option<bool> {
    let fingerprint = ffmpeg_fingerprint(ffmpeg)?;
    let raw = std::fs::read_to_string(nvenc_cache_path()).ok()?;
    // 格式：`<指纹> <0|1>`。指纹不匹配即视为失效。
    let (saved, verdict) = raw.trim().split_once(' ')?;
    if saved != fingerprint {
        return None;
    }
    Some(verdict.trim() == "1")
}

fn write_nvenc_cache(ffmpeg: &std::path::Path, available: bool) {
    let Some(fingerprint) = ffmpeg_fingerprint(ffmpeg) else {
        return;
    };
    let path = nvenc_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        format!("{fingerprint} {}", if available { 1 } else { 0 }),
    );
}

/// ffmpeg 的身份指纹：大小 + mtime 秒。升级随包 ffmpeg 必然改变其中一个。
fn ffmpeg_fingerprint(ffmpeg: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(ffmpeg).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|delta| delta.as_secs())
        .unwrap_or(0);
    Some(format!("{}-{}", meta.len(), mtime))
}

fn nvenc_available(ffmpeg: &std::path::Path) -> bool {
    // 命中进程内结论直接返回（这是热路径：每次开播都会问一次）。
    if let Ok(guard) = nvenc_slot().read() {
        if let Some(value) = *guard {
            return value;
        }
    }
    let verdict = {
        // 先读落盘结论：命中就完全不付探测成本（省 1133ms 首屏税）。
        if let Some(cached) = read_nvenc_cache(ffmpeg) {
            crate::trace::log(format!(
                "[vsr] 硬件编码：读缓存 = {}",
                if cached { "可用" } else { "不可用" }
            ));
            cached
        } else {
            let probe = std::process::Command::new(ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=black:s=640x360",
                    "-frames:v",
                    "2",
                    "-an",
                    "-c:v",
                    "h264_nvenc",
                    "-preset",
                    "p1",
                    "-cq",
                    "26",
                    "-pix_fmt",
                    "yuv420p",
                    "-profile:v",
                    "high",
                    "-f",
                    "null",
                    "-",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let ok = probe.map(|status| status.success()).unwrap_or(false);
            crate::trace::log(format!(
                "[vsr] 硬件编码探测：h264_nvenc={}",
                if ok {
                    "可用"
                } else {
                    "不可用，回落 libx264"
                }
            ));
            // 落盘：下次冷启动直接读，不再付这 1133ms。
            write_nvenc_cache(ffmpeg, ok);
            ok
        }
    };
    // 写回进程内槽位。用 `write()` 而不是 `get_or_init`：清缓存时会把槽位置回
    // `None`，此时才可能真正重探（审查 E1）。
    if let Ok(mut guard) = nvenc_slot().write() {
        *guard = Some(verdict);
    }
    verdict
}

/// 产物参数指纹文件名：写在 HLS 目录里，用来判断"这份产物是哪套编码参数转出来的"。
///
/// **为什么必须有它**：目录缓存是按 vid 长期复用的（看到 `#EXT-X-ENDLIST` 就直接播），
/// 而参数会随版本调整（2026-10-09 就把固定 1100k 改成按源自适应、预设从 p1 提到 p4）。
/// 没有指纹的话，升级后看到的是**旧参数转出来的旧产物** —— 修复对老集完全不生效，
/// 用户只会觉得"别人的画质变好了、我这几集还是糊的"。
const ENCODER_STAMP_FILE: &str = ".encoder-stamp";

/// 当前编码参数指纹：换算公式版本 + 三档编码参数 + HLS 打包参数。
///
/// 刻意用**可读文本**而不是哈希：排查时可以直接打开产物目录里的这个文件，看到
/// "这一集是按什么参数转出来的"。改了任何影响画面的参数（含 `rate_limits` 的倍数，
/// 所以要同时改 `rate=` 那一段的版本号），指纹就会变、旧产物自动重转。
fn encoder_fingerprint(ffmpeg: &std::path::Path, mode: OutputMode) -> String {
    if mode == OutputMode::Copy {
        return format!("mode=copy|{}", COPY_HLS_PACKAGING_ARGS.join(" "));
    }
    let mut parts: Vec<String> = vec!["rate=v4:3.0x".to_owned()];
    for args in [NVENC_ARGS, X264_ARGS, LOW_TIER_ARGS] {
        parts.push(args.join(" "));
    }
    parts.push(hls_packaging_args(ffmpeg).join(" "));
    parts.join(" | ")
}

/// 目录里的产物是不是**当前参数**转出来的。
///
/// 没有指纹文件的一律算过期：那是上一版留下的产物，重转一次的代价（几秒）远小于
/// 让用户继续看糊掉的画面。
fn artifact_is_current(directory: &std::path::Path, mode: OutputMode) -> bool {
    let Ok(ffmpeg) = crate::short_drama_app::ffmpeg_path() else {
        return true;
    };
    match std::fs::read_to_string(directory.join(ENCODER_STAMP_FILE)) {
        Ok(saved) => saved.trim() == encoder_fingerprint(&ffmpeg, mode).trim(),
        Err(_) => false,
    }
}

fn parse_playlist_duration_ms(body: &str) -> Option<u64> {
    let total_secs: f64 = body
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("#EXTINF:")?;
            rest.split(',').next()?.trim().parse::<f64>().ok()
        })
        .sum();
    if total_secs.is_finite() && total_secs >= 1.0 {
        Some((total_secs * 1000.0).round() as u64)
    } else {
        None
    }
}

fn first_playlist_segment(body: &str) -> Option<&str> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
}

fn is_completed_hls_dir(directory: &std::path::Path, mode: OutputMode) -> bool {
    if !directory.is_dir() || !artifact_is_current(directory, mode) {
        return false;
    }
    let init_ready = directory
        .join("init.mp4")
        .metadata()
        .is_ok_and(|meta| meta.len() > 0);
    if !init_ready {
        return false;
    }
    let Ok(body) = std::fs::read_to_string(directory.join("index.m3u8")) else {
        return false;
    };
    if !body.contains("#EXT-X-ENDLIST") || !playlist_has_segment(&body) {
        return false;
    }
    // 拦截仅含 `#EXTINF:0.000000` 或 24 字节空盒的失效残片目录，避免误判为“已完成缓存”。
    if parse_playlist_duration_ms(&body).is_none() {
        return false;
    }
    let Some(first_seg) = first_playlist_segment(&body) else {
        return false;
    };
    let seg_name = first_seg.split('?').next().unwrap_or(first_seg);
    directory
        .join(seg_name)
        .metadata()
        .is_ok_and(|meta| meta.len() >= 1024)
}

fn playlist_duration_ms(playlist: &std::path::Path) -> Option<u64> {
    let body = std::fs::read_to_string(playlist).ok()?;
    parse_playlist_duration_ms(&body)
}

pub fn has_completed_hls(vid: &str, copy_mode: bool) -> bool {
    let (directory, mode) = if copy_mode {
        (video_copy_dir(vid), OutputMode::Copy)
    } else {
        (video_cache_dir(vid), OutputMode::H264)
    };
    is_completed_hls_dir(&directory, mode)
}

pub async fn open_completed_hls(
    session_id: u64,
    vid: &str,
    copy_mode: bool,
) -> Option<(String, Option<u64>, std::path::PathBuf)> {
    let (directory, mode) = if copy_mode {
        (video_copy_dir(vid), OutputMode::Copy)
    } else {
        (video_cache_dir(vid), OutputMode::H264)
    };
    if !is_completed_hls_dir(&directory, mode) {
        return None;
    }
    clear_session_stopped(session_id);
    stop_competing_for_foreground(session_id, &directory);
    let info = ensure_server().await.ok()?;
    let artifact = ArtifactId::from_directory(&directory)?;
    let url = artifact_url(info.port, session_id, &artifact, &info.token);
    let duration_ms = playlist_duration_ms(&directory.join("index.m3u8"));
    Some((url, duration_ms, directory))
}

/// 启动一条 H.264 HLS 增强流。源流失败时返回 Err，调用方继续用原 URL。
pub async fn start(session_id: u64, source_url: &str) -> Result<String, String> {
    // 这条链路（guo / 公开直链）拿不到档位信息，源码率交给 `rate_limits` 的默认值。
    start_with_key(session_id, source_url, None, None).await
}

/// 红果短剧：按 **vid** 建持久缓存目录的整集 HLS。
///
/// 与 `start_with_key` 的区别只有生命周期，但它正是方案 B 成立的关键：
///   - 目录名是 `vid-<vid>` 而不是 `session-<id>`，**同一集无论打开多少次都是同一份产物**；
///   - 任务标记 `persist = true`，换集 `stop()` 不会删目录；
///   - 产物已完整（含 `#EXT-X-ENDLIST`）时**直接返回地址，连 ffmpeg 都不起**。
///
/// 实测依据：整集转码 5.6 秒（92.7 秒正片，16 倍实时）。转一次之后二次打开的代价应当是
/// 零 —— 起进程、起转码、等首片全部省掉。
pub async fn start_video_cache(
    session_id: u64,
    vid: &str,
    source_url: &str,
    key_hex: Option<&str>,
    source_kbps: Option<u32>,
) -> Result<String, String> {
    start_in_directory(
        session_id,
        source_url,
        key_hex,
        video_cache_dir(vid),
        true,
        source_kbps,
        OutputMode::H264,
        true,
    )
    .await
}

/// 红果短剧（VSR 关闭）：零重编码视频直拷贝解密封装成 1 秒 fMP4 HLS，写入独立 `vid-copy-{vid}` 目录。
pub async fn start_copy_cache(
    session_id: u64,
    vid: &str,
    source_url: &str,
    key_hex: Option<&str>,
) -> Result<String, String> {
    start_in_directory(
        session_id,
        source_url,
        key_hex,
        video_copy_dir(vid),
        true,
        None,
        OutputMode::Copy,
        true,
    )
    .await
}

/// 按**稳定键**建持久产物目录（guo 外部站源的整集 HLS）。
///
/// 与红果 [`start_video_cache`] 的区别只是“键从哪来”：红果有官方 vid，而 guo 源
/// 只有「源:剧:集」，且它的播放地址每次 resolve 都带新的本地 token（不能当键）。
/// 所以这里把调用方给的 `key` 哈希成目录名，其余复用同一套「ENDLIST 即复用 +
/// 编码指纹校验 + 任务表重挂」逻辑。
///
/// 为什么值得持久：guo 链路原来每集都建 `session-<uuid>` 临时目录、换集即删，
/// 同一集回看 / 画中画交接 / 连播预取都没法命中，每次都要重新起 ffmpeg 等首片。
/// 红果早就是「转一次、之后秒开」，这条改动把 guo 拉到同一水平。
pub async fn start_keyed_cache(
    session_id: u64,
    key: &str,
    source_url: &str,
    source_kbps: Option<u32>,
) -> Result<String, String> {
    let vid = format!("guo-{}", stable_key_hash(key));
    crate::trace::log(format!(
        "[vsr] guo 整集缓存键会话={session_id} 目录=vid-{vid}"
    ));
    start_in_directory(
        session_id,
        source_url,
        None,
        video_cache_dir(&vid),
        true,
        source_kbps,
        OutputMode::H264,
        false,
    )
    .await
}

/// 把「源:剧:集:档位」折成一个稳定的目录名片段。
///
/// `DefaultHasher` 在标准库里是固定种子的 SipHash——同一进程内、跳进程都稳定，
/// 正好够当缓存键；不保证跳 Rust 版本一致，但那种情况只需重转一次（编码指纹会兜住）。
fn stable_key_hash(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 与 `start` 相同，但可带**解密密钥**。
///
/// 红果的源是 CENC 加密的（`1:encrypt`），ffmpeg 需要 `-decryption_key` 才能读出
/// 画面。加上这个参数之后，这条链路对红果也成立了 —— 而它带来的收益比 guo/直链
/// 那条大得多：
///
/// 红果原先必须把整集下载+解密+转存成本地 mp4 才能播（实测 6.6–22.3 秒），
/// 而改成「解密密钥 + 边解密边转 H.264 HLS」之后，**首片实测 1126ms 就落地**
/// （含 API 往返的总首屏 3.3 秒）。这就是「先把画面出来、播放的同时再加载」。
///
/// `key_hex` 是 16 字节 AES-128 密钥的十六进制串；`None` 表示源未加密。
///
/// `source_kbps` 是**选中档的源码率**（kbps），用来按源算码率上限（见 `rate_limits`）；
/// 拿不到就传 `None`，会退回一组中性默认值。
pub async fn start_with_key(
    session_id: u64,
    source_url: &str,
    key_hex: Option<&str>,
    source_kbps: Option<u32>,
) -> Result<String, String> {
    if source_url.trim().is_empty() || !needs_enhancement(source_url) {
        return Err("源流地址不支持增强转码。".to_owned());
    }
    // 每次会话一个临时目录（用完即弃），与红果的按 vid 持久目录相对。
    let directory = media_root().join(format!("{session_id}-{}", uuid::Uuid::new_v4().simple()));
    start_in_directory(
        session_id,
        source_url,
        key_hex,
        directory,
        false,
        source_kbps,
        OutputMode::H264,
        false,
    )
    .await
}

/// 转码主体：`start_with_key`（临时目录）与 `start_video_cache`（按 vid 持久目录）共用。
#[allow(clippy::too_many_arguments)]
async fn start_in_directory(
    session_id: u64,
    source_url: &str,
    key_hex: Option<&str>,
    directory: PathBuf,
    persist: bool,
    source_kbps: Option<u32>,
    mode: OutputMode,
    hongguo_cdn: bool,
) -> Result<String, String> {
    if source_url.trim().is_empty() || !needs_enhancement(source_url) {
        return Err("源流地址不支持增强转码。".to_owned());
    }
    if is_prewarm_session(session_id) && has_active_foreground() {
        return Err("前台正在起播，让路跳过预转".to_owned());
    }
    if !is_prewarm_session(session_id) {
        stop_competing_for_foreground(session_id, &directory);
    }
    // ⚠️ 必须在**返回地址之前**把产物目录建出来（2026-11-13）。
    //
    // 现场（开发态日志 `.app-data/ttv-playback.log`，会话 104，第 240 集）：
    //   11.537 起播 attachSource 形态=VSR转码HLS
    //   11.558 [vsr] 本地服务：产物不存在 产物=vid-7706561985000770585
    //   11.576 hls.js 错误 manifestLoadError fatal=true
    //   11.577 整集 HLS 播放失败 → 回退整集文件
    //   24.869 resolve 返回 耗时=13302ms 字节=32.9MB   ← 用户看到的黑屏
    //
    // 成因：`start()` 在 1.5 秒就返回了地址（`ready=false`），而 ffmpeg 此时
    // 还在 `detect_encoder_tier` 里真跑一次 NVENC 探测，目录尚未创建。服务端
    // 把「还没建」当成「永久失效」回 410，而 hls.js 对 **4xx 不重试**——
    // 一条正在正常转码的流在 18ms 内被判死，回退路径又要等一次整集 H.264
    // 迁移（13 秒），用户的实际感受就是「开 VSR 黑屏十几秒，最后还没增强」。
    //
    // 预建目录之后，这个窗口里服务端会落到「清单尚未就绪 → 503」，
    // hls.js 按 `manifestLoadingMaxRetry: 20` 自己等到首片，整条回退链不被触发。
    if let Err(error) = std::fs::create_dir_all(&directory) {
        return Err(format!("创建增强产物目录失败：{error}"));
    }
    let request = session_request(session_id);
    request
        .cancel
        .run(async {
            let work_directory = directory.clone();
            let work_url = source_url.to_owned();
            let work_key = key_hex.map(str::to_owned);
            let task = transcode_tasks().subscribe(
                directory.to_string_lossy().into_owned(),
                move |_| async move {
                    transcode_in_directory(
                        session_id,
                        &work_url,
                        work_key.as_deref(),
                        work_directory,
                        persist,
                        source_kbps,
                        mode,
                        hongguo_cdn,
                    )
                    .await
                },
            );
            let pending = PendingJob {
                session_id,
                directory: directory.clone(),
                task: task.clone(),
                persist,
                active: true,
            };
            let previous = {
                let sessions = pending_sessions()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if request.cancel.is_cancelled() {
                    return Err(crate::media_task::CANCELLED.to_owned());
                }
                let previous = jobs()
                    .lock()
                    .map_err(|_| "增强任务表锁不可用。".to_owned())?
                    .insert(
                        session_id,
                        Job {
                            directory: directory.clone(),
                            task: task.clone(),
                            persist,
                        },
                    );
                drop(sessions);
                previous
            };
            drop(previous);
            let info = ensure_server().await?;
            let playlist = directory.join("index.m3u8");
            // 快路径：首个分片落地就把地址交出去（常见 2–4 秒），与官方实现的 on_ready
            // 语义一致。
            //
            // 但**"慢"不等于"失败"**。进程还活着就说明转码确实在跑，只是还没吐出分片；
            // 这种情况下直接把地址交回去，让播放器按自己的重试节奏等首段即可
            // （`hlsAttach.ts` 已补上 manifestLoadingMaxRetry / fragLoadingMaxRetry）。
            // 旧实现在这里按住用户最多 15 秒才返回，是"首次解析很慢"的直接来源——
            // 而它之所以要按住，正是因为当时前端没有重试，早返回必然判死。
            //
            // 只有 ffmpeg 进程真的退出且没留下任何产物，才算起不来，回退原始 URL。
            //
            // 地址钉住**产物身份**（目录名）而不是会话号：会话号会被后续换集复用，
            // 而目录名一旦确定就不再变（见 `ArtifactId` 的说明）。
            let artifact = ArtifactId::from_directory(&directory)
                .ok_or_else(|| "产物目录名无法作为 ArtifactId。".to_string())?;
            let url = artifact_url(info.port, session_id, &artifact, &info.token);
            // 就绪条件同时要求 `init.mp4` 存在。它是 fMP4 的首片（播放列表里的
            // `#EXT-X-MAP`），缺了它 hls.js 连解封装都起不来；只等 playlist 会在
            // "列表已写、init 还没落"的那一瞬把一个必然失败的地址交出去。
            // 上面之所以敢不传 `-hls_fmp4_init_filename`，也是靠这一步兜住落点。
            let init_segment = directory.join("init.mp4");
            // ⚠️ 只判"两个文件都存在"不够（2026-11-08）：ffmpeg 会先写 header 版清单
            // （`#EXTM3U` + `#EXT-X-TARGETDURATION`，几十字节、**一条分片都没有**），
            // 再写 init.mp4，最后才 prime 出第一片。旧判据在这个中间态就返回 Ok，
            // 而本地服务的清单分支对"没有 `#EXTINF`"的清单回 **503**（那条判据本身
            // 是对的：空清单会让 hls.js 判 `LEVEL_EMPTY_ERROR`）。于是后端明明还在等，
            // 却先把一个必然 503 的地址交给了播放器 —— 起播被打回"播放器自己重试清单"，
            // 也就是日志里反复出现的 `下发清单 字节=1/264` 之后干等 3 秒。
            //
            // 现在把就绪条件与**服务端可下发**的判据对齐：清单里至少有一条分片。
            // `#EXTINF` 是唯一标记，与 `serve_artifact_file` 里那条 503 判据同源。
            // 于是"后端返回 Ok"就意味着"播放器拉这一次清单就能拿到可播内容"，
            // 不再把解开的判据差甩给播放器的重试预算去消化。
            //
            // 判据抽成 `playlist_has_segment` 是为了被单测钉住：它一旦与本地服务的
            // 503 判据分叉，就会退回到"后端说好了、服务端说没有"的僵局。
            let ready = || {
                (!persist || artifact_is_current(&directory, mode))
                    && playlist.is_file()
                    && init_segment.is_file()
                    && std::fs::read_to_string(&playlist)
                        .map(|body| playlist_has_segment(&body))
                        .unwrap_or(false)
            };
            // 等待上限。**这个值越小，首开越快**。
            //
            // 时间线是串行的：后端按住 → 前端才 attach → 播放器才开始重试清单。
            // 所以后端多等 1 秒，用户就实打实多等 1 秒——前端并不会因为后端早返回而失败，
            // 它的 `manifestLoadingMaxRetry=4` / `fragLoadingMaxRetry=8`（见 hlsAttach.ts）
            // 本来就是为了"边转边播"准备的：地址先给，首段由播放器自己等到。
            // 1.5 秒足够覆盖本机实测的首段就绪（本地片 0.55s、真实源 2–4s 的常见情况），
            // 剩下的交给播放器重试，总耗时变成 max(后端, 前端) 而不是两者相加。
            let wait_start = tokio::time::Instant::now();
            let deadline = wait_start + READY_WAIT;
            while !ready() && !task.is_finished() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            if ready() || !task.is_finished() {
                // ⚠️ 进程已经退出、且产物**不完整**（没有 `#EXT-X-ENDLIST`）时不能算成功。
                //
                // 那说明 ffmpeg 中途挂了（网络断、密钥错、源返回 403），产物只覆盖开头几秒。
                // 旧实现看到 ready() 为真就返回 Ok(地址)，于是上层不会走 `cached_full_mp4`
                // 回退链路 —— 用户看到的是"播一两秒就卡住、既不报错也不自愈"（审查 R5）。
                // 只有"进程还在跑"或"转完了（ENDLIST 齐）"两种情形才值得把地址交出去。
                if task.is_finished() && !is_completed_hls_dir(&directory, mode) {
                    crate::trace::log(format!(
                        "[vsr] 转码进程已退出且产物不完整或为空分片，按失败处理 会话={session_id}"
                    ));
                    return Err("增强转码提前退出，产物不完整。".to_owned());
                }
                // 这一行是判断"到底谁在拖"的关键：ready=true 说明 ffmpeg 已经在 1.5 秒内
                // 吐出首片，后面的等待全在播放器侧；ready=false 说明后端压根没跟上。
                crate::trace::log(format!(
                    "[vsr] 就绪判定 会话={session_id} ready={} 进程存活={} 等待={}ms 地址={}",
                    ready(),
                    !task.is_finished(),
                    wait_start.elapsed().as_millis(),
                    crate::trace::redact_url(&url)
                ));
                if !ready() {
                    crate::trace::log("[vsr] 首段尚未落地，先交地址交由播放器重试");
                }
                if task.is_finished() {
                    task.wait().await?;
                }
                // 与 stop 使用同一把会话锁，签名完成/起播交接时不能漏掉已到达的取消。
                let sessions = pending_sessions()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if request.cancel.is_cancelled() {
                    return Err(crate::media_task::CANCELLED.to_owned());
                }
                let previous = jobs()
                    .lock()
                    .map_err(|_| "增强任务表锁不可用。".to_owned())?
                    .insert(
                        session_id,
                        Job {
                            directory: directory.clone(),
                            task: task.clone(),
                            persist,
                        },
                    );
                drop(sessions);
                drop(previous);
                pending.disarm();
                return Ok(url);
            }
            crate::trace::log(format!(
                "[vsr] 起转失败 会话={session_id} 等待={}ms 后进程已退出",
                wait_start.elapsed().as_millis()
            ));
            let result = task.wait().await;
            drop(pending);
            result.and_then(|_| Err("增强转码进程已退出。".to_owned()))
        })
        .await
}

#[allow(clippy::too_many_arguments)]
async fn transcode_in_directory(
    session_id: u64,
    source_url: &str,
    key_hex: Option<&str>,
    directory: PathBuf,
    persist: bool,
    source_kbps: Option<u32>,
    mode: OutputMode,
    hongguo_cdn: bool,
) -> Result<(), String> {
    // 检查、清理和起进程都在单个执行体内，预转与前台不能并发删写同一目录。
    if persist {
        if is_completed_hls_dir(&directory, mode) {
            return Ok(());
        }
        if directory.is_dir() && !artifact_is_current(&directory, mode) {
            let _ = std::fs::remove_dir_all(&directory);
        }
        // 前一次中断若留下未收尾（无 ENDLIST）的清单 / init / 分片，必须在起 ffmpeg 前清掉，
        // 否则 `start_in_directory::ready()` 会命中旧清单里的 `#EXTINF` 立刻判就绪，紧接着
        // 新起 ffmpeg 覆写空清单导致播放器连吃 503 并回退整集下载。
        let _ = std::fs::remove_file(directory.join("index.m3u8"));
        let _ = std::fs::remove_file(directory.join("init.mp4"));
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if is_segment(name) || name.ends_with(".tmp") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    // 持久目录要**保留已有产物**：重转时只覆盖分片与清单，不整目录清空，
    // 这样「转了一半就被打断」的那次留下的分片仍可能被复用。
    if !persist {
        let _ = std::fs::remove_dir_all(&directory);
    }
    // ⚠️ 诊断：错误信息**必须带上绝对路径**。
    //
    // 这个报错在 UI 上只表现为「播放源连接受阻 / 一直缓冲」，排查时无法分辨是
    // 父目录不存在、路径超长、还是这条路径本身被拒。带上路径之后，"到底是哪一层
    // 写不进去"一目了然，不必再逐层猜（2026-10-22 实测踩过这个坑）。
    std::fs::create_dir_all(&directory).map_err(|error| {
        let exists = directory.exists();
        format!(
            "创建增强缓存目录失败（路径={}，已存在={}）：{error}",
            directory.display(),
            exists
        )
    })?;

    let ffmpeg = crate::short_drama_app::ffmpeg_path()?;
    let _playlist = directory.join("index.m3u8");
    // 落一枚参数指纹：持久目录（`vid-{vid}`）会被长期复用，而编码参数随版本变化，
    // 复用前必须先比对它 —— 否则升级后用户看到的是旧参数转出来的旧产物（见
    // `ENCODER_STAMP_FILE` 的说明）。写在起转码之前，于是"转了一半"的目录也带指纹。
    let _ = std::fs::write(
        directory.join(ENCODER_STAMP_FILE),
        encoder_fingerprint(&ffmpeg, mode),
    );
    // 记下"转码这一段"的起点。整条首开时间线 = 解析(worker) + 等待就绪(这里)
    // + 播放器拉清单/首片，三者混在一个"很慢"里没法优化，所以每段各自落一行。
    crate::trace::log(format!(
        "[vsr] start 会话={session_id} 模式={} 源={} ffmpeg={}",
        if mode == OutputMode::Copy {
            "copy"
        } else {
            "h264"
        },
        crate::trace::redact_url(source_url),
        ffmpeg.display()
    ));
    let mut command = Command::new(&ffmpeg);
    // 解密密钥是**输入**选项，必须排在 `-i` 之前（与 -tls_verify 同一条规则：
    // 写到 -i 之后会被解析到输出侧，而输出是 HLS 分片、没有解密封装器，等于被忽略，
    // 结果是 ffmpeg 拿到密文解不出画面）。
    if let Some(key) = key_hex.map(str::trim).filter(|value| !value.is_empty()) {
        command.arg("-decryption_key").arg(key);
    }
    // 红果 CDN 请求头必须作为输入选项排在 `-i` 之前，避免首轮被 CDN 拒绝再重试。
    if hongguo_cdn && (source_url.starts_with("http://") || source_url.starts_with("https://")) {
        command
            .arg("-user_agent")
            .arg("com.phoenix.read/71332")
            .arg("-headers")
            .arg("Referer: https://novel.snssdk.com/\r\n");
    }
    // 关键：把 ffmpeg 的工作目录钉死在本次会话目录上。
    //
    // fMP4 的 init 段（`init.mp4`）在 ffmpeg 里是按**相对文件名**写出的，而
    // `-hls_segment_filename` 我们传的是绝对路径 —— 于是只有分片落对了地方，
    // init.mp4 会悄悄掉到**父进程的当前工作目录**（实测：会直接掉在工程根
    // 目录/桌面），会话目录里永远没有它。
    //
    // 后果正好对应用户的两条反馈：
    //   1) `ready()` 要求 index.m3u8 与 init.mp4 同时存在 → 恒为 false →
    //      start() 每次都会死等满 READY_WAIT(1500ms) 才把地址交出去。这就是
    //      "首次打开一直在加载"里后端按住的那一段。
    //   2) 就算地址交给了播放器，`#EXT-X-MAP:URI="init.mp4"` 的请求会 404
    //      （本地服务去会话目录里找它，找不到），hls.js 只能反复重试首片 →
    //      用户看到的就是"一直转圈、加载很长时间"。
    //
    // 实测四种组合：
    //   不传 init 名 + cwd=工程根   → 目录内 init=False（掉到 cwd）
    //   传相对名 init.mp4 + cwd=根  → 目录内 init=False（照样掉到 cwd）
    //   **cwd=会话目录**            → 目录内 init=True ✅
    //   传绝对路径 init + cwd=根     → ffmpeg 直接失败
    //     Failed to open segment '<abs>/init.mp4'
    // 所以唯一稳的组合就是「相对名 + current_dir(会话目录)」。
    command
        .current_dir(&directory)
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        // 输入探测参数（必须是输入选项，所以排在 `-i` **之前**）。
        //
        // ffmpeg 默认 probesize=5,000,000、analyzeduration=5,000,000，也就是说它
        // 会先尽量读满 5MB / 5 秒的源数据才决定"这是什么流、第一帧从哪开始"——
        // 对网络输入这就是纯粹的**白等**：源要先下载几 MB 才可能开始转码。
        // 本链路要处理的只是「一条 H.264/HEVC 视频 + 一条 AAC 音频」这种最普通的
        // mp4/HLS，1MB / 2 秒足够识别，第一帧不必等整段探测。
        //
        // 这是「首次打开要等很久」在转码侧的直接来源之一，且完全无副作用：
        // 探测不足时 ffmpeg 只会退化成"边播边补"，不会失败。
        .arg("-probesize")
        .arg(if mode == OutputMode::Copy {
            "500000"
        } else {
            "1000000"
        })
        .arg("-analyzeduration")
        .arg(if mode == OutputMode::Copy {
            "1000000"
        } else {
            "2000000"
        })
        // `-tls_verify 0` 与 `-rw_timeout` 必须和 `-probesize` 一样排在 `-i`
        // **之前**——它们是 ffmpeg 的**输入**选项，写到 `-i` 之后会被解析到
        // 输出侧（而这条命令的输出是 HLS 分片，没有网络输出流，等于被整个
        // 忽略），输入于是仍走默认证书校验。
        //
        // 随包 ffmpeg 没有 CA 证书链，后果是打开 https 源必然失败：
        //   error:0A000086:lib(20)::reason(134)
        //   Error opening input: I/O error
        // 实测对照（同一条真实 CDN 地址）：
        //   写在 `-i` 之后 → 115ms 失败；写在 `-i` 之前 → 704ms 成功出片。
        //
        // 这条链路上它意味着**增强转码从来没有成功打开过源**：每次播放都在
        // 极短时间内失败退出、上层静默回退原流。表现正是用户报的两条——
        // 开关怎么拨都一样（因为开关开时走的那条路本身就是坏的），以及
        // 「首次打开一直在加载」。
        .args(tls_args(source_url))
        // 15 秒（ffmpeg 的单位是微秒）。原来是 60 秒 —— 源 CDN 一旦卡住不回包，
        // ffmpeg 就干等整整一分钟、一个分片都写不出来；播放器那边只看得到
        // "这一片一直不存在"，用户感知就是"播到一半卡住不动"。
        // 15 秒足够覆盖正常网络抖动，超过它就该判失败：进程退出 → 分片请求拿到
        // 410 → 前端重新开播续转（比干等一分钟好得多）。
        .args(network_timeout_args(source_url))
        // 网络源断流自动重连（**输入选项，必须在 `-i` 之前**）。
        //
        // 实测现场（ttv-playback.log，2026-10-19 多次）：
        //   [vsr] 转码进程退出 会话=1000188 状态=exit code: 0xffffff76
        //        stderr=[tcp @ ...] Connection to tcp://v5-se-bjtc-reading-video-a...
        //   [vsr] 起转失败 会话=111 状态=exit code: 0xa7a7ca08
        //        stderr=Error opening input: Server returned 5XX Server Error
        //
        // 也就是说：转码中途源 CDN 一断，ffmpeg 直接退出、产物停在半截（没有
        // `#EXT-X-ENDLIST`）。播放器只能播到已写出的分片末尾就停住，用户看到的就是
        // "播着播着转码失败，必须重新打开从头播才能加载完后面"——重新开播会重新走
        // `start_video_cache`，发现产物不完整于是整集重转一次。
        //
        // 这三条让 ffmpeg 自己在断流处重连而不是立刻退出：对它而言这是"数据来得慢"，
        // 不是"流结束"。5 秒上限覆盖 CDN 切节点 / 短时拥塞，再长就该判失败了。
        // 只对 http(s) 源追加：本地文件输入没有这套协议选项。
        .args(reconnect_args(source_url))
        .arg("-i")
        .arg(source_url)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?");
    match mode {
        OutputMode::Copy => {
            command.args(["-c:v", "copy", "-c:a", "aac", "-b:a", "128k"]);
        }
        OutputMode::H264 => {
            command
                .args(video_encoder_args(&ffmpeg, source_kbps))
                .args(["-c:a", "aac", "-b:a", "128k"]);
        }
    }
    command
        // 关键帧间隔由三档编码器各自 GOP 参数决定，不重复追加；H264 模式按档位选 HLS 参数，Copy 模式走 1 秒快速 fMP4 分片。
        .args(match mode {
            OutputMode::Copy => COPY_HLS_PACKAGING_ARGS,
            OutputMode::H264 => hls_packaging_args(&ffmpeg),
        })
        // 刻意**不传** `-hls_fmp4_init_filename`。
        //
        // 传绝对路径（`directory.join("init.mp4")`；正斜杠、反斜杠、预创建空文件，
        // 三种都试过）会让随包 ffmpeg 直接以 EACCES 失败：
        //   Failed to open segment '<abs>/init.mp4'  →  Permission denied
        // 结果是**播放列表永远生成不出来**。也就是说这条增强链路从上线起就没生效过：
        // 每次播放都在 ~0.4 秒内挂掉、`job_alive` 转 false、上层静默回退原流——
        // 快得连"卡"都看不出来，所以一直没人发现 guo / 公开直链上根本没有 VSR。
        //
        // 实测对照（`-t 6` 本地片）：不传该参数时 init.mp4 会正确落在播放列表同级
        // 目录，且播放列表里写的就是相对路径 `#EXT-X-MAP:URI="init.mp4"` —— 正好
        // 配合下面本地服务把 token 补回每条分片的重写逻辑。
        .arg("-hls_segment_filename")
        .arg("seg-%05d.m4s")
        .arg("index.m3u8")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);

    let (child, _process_tree) = crate::media_task::spawn_worker(&mut command)?;
    let started = std::time::Instant::now();
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("增强转码退出异常：{error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(300)
            .collect::<String>();
        crate::trace::log(format!(
            "[vsr] 转码进程退出 会话={session_id} 耗时={}ms 状态={} stderr={detail}",
            started.elapsed().as_millis(),
            output.status
        ));
        return Err(format!("增强转码失败：{detail}"));
    }
    crate::trace::log(format!(
        "[vsr] 转码完成 会话={session_id} 耗时={}ms",
        started.elapsed().as_millis()
    ));
    Ok(())
}

/// 停止一条会话的增强流。重复调用是安全的。
///
/// **`persist` 的产物不删目录**：换集时 `playback_command("stop")` 会走到这里，
/// 而对红果的整集 HLS 来说那时转码早已结束、产物是一份可复用的缓存 —— 删掉意味着
/// 用户下一次打开同一集又要重转 5.6 秒。回收改由缓存预算统一负责。
/// 判定"转码停滞"的阈值（秒）。
///
/// 正常转码每 2 秒写一个分片（`-hls_time 2`），所以连续 8 秒没有任何新文件
/// 就意味着至少四个分片的产出被卡住了。取 8 秒而不是更大：用户对"停住"的
/// 容忍度很短；也不取更小，要排除磁盘抖动与 NVENC 的偶发长帧。
const STALL_SECONDS: u64 = 8;

/// 目录内最新文件的"距今多少秒"；读不到目录时返回 `None`。
///
/// 用于判断转码是否还在产出：ffmpeg 每写完一个分片都会重写 `index.m3u8`，
/// 所以"最新 mtime"就是"最后一次真的写出东西"的时刻。
fn directory_idle_seconds(directory: &std::path::Path) -> Option<u64> {
    let mut newest: Option<std::time::SystemTime> = None;
    for entry in std::fs::read_dir(directory).ok()? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        newest = Some(match newest {
            Some(prev) if prev >= modified => prev,
            _ => modified,
        });
    }
    newest?.elapsed().ok().map(|elapsed| elapsed.as_secs())
}

/// 转码是否**已经开始写出分片**（至少落了第一片）。
///
/// 停滞判据必须带上这个前提：全新会话在首片落地前本来就要等 2~4 秒（源越远越久），
/// 那段时间目录里一个文件都没有，把它当"停滞"会误杀一次正常的起转。
/// 只有已经开始写、却突然停住，才是真的要处置的那种卡死。
fn transcode_started_writing(directory: &std::path::Path) -> bool {
    directory.join("seg-00000.m4s").is_file()
}

pub fn stop(session_id: u64) {
    mark_session_stopped(session_id);
    let mut sessions = pending_sessions()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(token) = sessions
        .remove(&session_id)
        .and_then(|token| token.upgrade())
    {
        token.cancel();
    }
    let job = jobs()
        .lock()
        .ok()
        .and_then(|mut guard| guard.remove(&session_id));
    drop(sessions);
    drop(job);
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.persist && self.task.subscriber_count() == 1 {
            schedule_temp_dir_cleanup(&self.task, self.directory.clone());
        }
    }
}

/// 清掉落盘的硬件编码探测结论，让下一次播放重新探测。
///
/// 设置页的"清空缓存"会调它：那是用户能表达"环境变了、重来一遍"的唯一入口，
/// 而探测结果的指纹只覆盖随包 ffmpeg，覆盖不到显卡与驱动。
pub fn clear_encoder_capability_cache() {
    let _ = std::fs::remove_file(nvenc_cache_path());
    let _ = std::fs::remove_file(tier_cache_path());
    // **同时清进程内槽位**。只删盘上文件的话，本次运行里 `nvenc_slot()` 仍是旧结论，
    // 探测要等重启才会重做 —— 而用户点"清空缓存"的动机恰恰是"环境变了，现在重来"
    // （换显卡、更新驱动）。审查 E1 指出的就是这个缺口。
    if let Ok(mut guard) = nvenc_slot().write() {
        *guard = None;
    }
    // 档位结论的进程内槽位同样要清 —— 否则"重新探测"只对 NVENC 那半生效，
    // 分档（Hardware / SoftwareFast / SoftwareLow）仍是旧结论。
    if let Ok(mut slot) = tier_slot().write() {
        *slot = None;
    }
}

/// 某一集的整集 HLS 缓存目录（`rtx-vsr/vid-{vid}`）。
///
/// 暴露给上层是为了两件事：命中复用/起转之后把它的 mtime 推到当下（缓存淘汰的
/// 唯一保护），以及测试里直接断言目录形态。
pub fn video_cache_dir(vid: &str) -> PathBuf {
    media_root().join(format!("vid-{vid}"))
}

/// 某一集的零转码拷贝 HLS 缓存目录（`rtx-vsr/vid-copy-{vid}`），与 VSR 重编码缓存互不串扰。
pub fn video_copy_dir(vid: &str) -> PathBuf {
    media_root().join(format!("vid-copy-{vid}"))
}

/// 清掉残留的增强产物。
///
/// `keep_persistent` 决定 `vid-{vid}` 的整集 HLS 目录留不留：
///   * **启动路径传 `true`** —— 那些目录是"同一集第二次打开零等待"的全部依据
///     （按 vid 存、换集不删、带参数指纹）。旧实现无条件 `remove_dir_all(media_root())`，
///     等于把持久缓存降级成"单进程内有效"：每次重启、每次升级，所有看过的集都要重转，
///     而且用户完全看不出为什么"昨天秒开的今天又要等"（审查 R2）。
///   * **设置页「清空缓存」传 `false`** —— 用户主动要求释放空间，那时应当真的清干净。
///
/// 另外**不在持锁状态下删目录**：清缓存时 `rtx-vsr` 可能是 GB 级，而本地 HLS 服务的
/// 每个分片请求都要拿这把锁 —— 持锁删盘会让正在播的画面卡住好几秒（审查 R9）。
pub fn cleanup_all(keep_persistent: bool) {
    let mut abandoned: Vec<PathBuf> = Vec::new();
    if let Ok(mut guard) = jobs().lock() {
        for (_, job) in guard.drain() {
            if !job.persist || !keep_persistent {
                abandoned.push(job.directory.clone());
            }
        }
    }
    for directory in abandoned {
        let _ = std::fs::remove_dir_all(directory);
    }
    if keep_persistent {
        // 只清根目录下的**临时**残留（`session-<id>-<uuid>`、早期版本的裸数字目录）。
        let Ok(entries) = std::fs::read_dir(media_root()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("vid-") {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
        return;
    }
    let _ = std::fs::remove_dir_all(media_root());
}

async fn ensure_server() -> Result<ServerInfo, String> {
    if let Some(info) = server_info().get() {
        return Ok(info.clone());
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("增强播放服务绑定失败：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let info = ServerInfo {
        port,
        token: uuid::Uuid::new_v4().simple().to_string(),
    };
    let accepted = info.clone();
    server_info()
        .set(info.clone())
        .map_err(|_| "增强播放服务状态重复初始化。".to_string())?;
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let server = accepted.clone();
            tokio::spawn(async move {
                let _ = handle(socket, server).await;
            });
        }
    });
    Ok(info)
}

async fn handle(mut socket: TcpStream, server: ServerInfo) -> std::io::Result<()> {
    let mut request = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") || request.len() > 64 * 1024 {
            break;
        }
    }
    let request = String::from_utf8_lossy(&request).into_owned();
    let first = request.lines().next().unwrap_or_default();
    let method = first
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let target = first.split_whitespace().nth(1).unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method == "OPTIONS" {
        write_head(
            &mut socket,
            "204 No Content",
            &cors_headers(),
            0,
            None,
            true,
        )
        .await?;
        return Ok(());
    }
    let token = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .unwrap_or_default();
    if token != server.token {
        // 前端若还在用上一轮会话的地址，这里会成片 403，表现为"播放器一直转圈"。
        crate::trace::log("[vsr] 本地服务：令牌无效，拒绝请求（多半是过期会话地址）");
        write_plain(&mut socket, "403 Forbidden", "增强播放令牌无效").await?;
        return Ok(());
    }
    let Some(file_name) = path.rsplit('/').next() else {
        write_plain(&mut socket, "404 Not Found", "增强播放资源不存在").await?;
        return Ok(());
    };
    // 路径形态：/rtx/{session_id}/{artifact}/{file}
    //
    // ⚠️ 读文件时**按 artifact 定位**，session_id 不参与解析（2026-10-20）。
    //
    // 旧实现只认 session_id，去任务表里查目录 —— 而任务表是可变的：
    // `register_reused` 会把同一个会话号从旧目录摘走挂到新目录。于是换集之后，
    // 任何仍持有旧 URL 的请求（旧 hls 实例、画中画窗口、自愈续体）都会静默
    // 读到**另一集的内容**，或者命中一个不存在的分片。
    //
    // artifact 是目录名，天然不可变：`vid-{vid}` 按内容建、`{session}-{uuid}`
    // 带随机后缀。所以它可以直接当身份用，不需要任何查找表。
    //
    // 形态判定**必须按段数**，不能看 `nth(3)` 是否存在 ——
    // `/rtx/104/index.m3u8` 的 `nth(3)` 恰好是文件名 `index.m3u8`，
    // 拿它当 artifact 会让旧地址走进新分支、把文件名当目录名。
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // 新形态：["rtx", session, artifact, file]；旧形态：["rtx", session, file]
    let is_new_shape = segments.len() >= 4;
    if !is_new_shape {
        // 旧形态 /rtx/{session_id}/{file}：兼容一个版本。
        //
        // 这类地址只可能来自升级前已经打开的播放器。它按会话表解析，
        // 因此**保留旧的串线风险**，但会打一条日志便于确认何时可以删除。
        let Ok(session_id) = segments.get(1).copied().unwrap_or_default().parse::<u64>() else {
            write_plain(&mut socket, "404 Not Found", "增强播放资源不存在").await?;
            return Ok(());
        };
        if !matches!(file_name, "index.m3u8" | "init.mp4") && !is_segment(file_name) {
            write_plain(&mut socket, "404 Not Found", "增强播放资源不存在").await?;
            return Ok(());
        }
        crate::trace::log(format!(
            "[vsr] 本地服务：收到旧形态地址（按会话表解析）会话={session_id} 文件={file_name}"
        ));
        let Some(directory) = jobs()
            .lock()
            .ok()
            .and_then(|guard| guard.get(&session_id).map(|job| job.directory.clone()))
        else {
            write_plain(&mut socket, "410 Gone", "增强播放会话已结束").await?;
            return Ok(());
        };
        return serve_artifact_file(
            &mut socket,
            &directory,
            session_id,
            file_name,
            &server.token,
            &method,
            &request,
        )
        .await;
    }
    let artifact_raw = segments[2];
    // session_id 仍然解析一遍：只用于日志与旧任务状态判断，**不决定目录**。
    let session_id = segments[1].parse::<u64>().unwrap_or(0);
    let Some(artifact) = ArtifactId::parse(artifact_raw) else {
        write_plain(&mut socket, "404 Not Found", "增强播放资源不存在").await?;
        return Ok(());
    };
    if !matches!(file_name, "index.m3u8" | "init.mp4") && !is_segment(file_name) {
        write_plain(&mut socket, "404 Not Found", "增强播放资源不存在").await?;
        return Ok(());
    }
    if session_id > 0
        && is_session_stopped(session_id)
        && !jobs()
            .lock()
            .map(|guard| guard.contains_key(&session_id))
            .unwrap_or(false)
    {
        crate::trace::log(format!(
            "[vsr] 本地服务：拦截已停止会话的残留请求 会话={session_id} 产物={} 文件={file_name}（返 410 Gone）",
            artifact.as_str()
        ));
        write_plain(&mut socket, "410 Gone", "增强播放会话已结束").await?;
        return Ok(());
    }
    let directory = artifact.directory();
    if !directory.is_dir() {
        // 目录不存在的两种成因必须分开（2026-11-13 修正）：
        //
        //   - **转码刚起、目录还没建**：`start()` 会在首片落地前就把地址交出去
        //     （`ready=false`），此刻请求清单是正常时序，不是失效。旧实现一律
        //     回 410，而 hls.js 对 4xx **不重试**、当场 fatal —— 现场日志里
        //     11.558 请求、11.576 判死，18ms 就把一条正在正常转码的流杀掉，
        //     随后回退整集文件又等 13 秒（用户看到的黑屏）。
        //   - **真被缓存预算淘汰 / 从未存在**：410 才诚实，让它立刻结束、
        //     由上层走既有的重新解析链路。
        //
        // 判据用父目录（`rtx-vsr`）在不在：根在说明链路活着、只是这一份还没
        // 落盘 → 503，让播放器按 `manifestLoadingMaxRetry` 继续等；根都不在
        // 说明整条增强链路已退出 → 410。
        let root_exists = directory
            .parent()
            .map(|parent| parent.is_dir())
            .unwrap_or(false);
        crate::trace::log(format!(
            "[vsr] 本地服务：产物目录不存在 产物={} 会话={session_id} 文件={file_name} 根目录在={root_exists}",
            artifact.as_str()
        ));
        if root_exists {
            write_plain(
                &mut socket,
                "503 Service Unavailable",
                "增强播放产物尚未就绪",
            )
            .await?;
        } else {
            write_plain(&mut socket, "410 Gone", "增强播放产物不存在").await?;
        }
        return Ok(());
    }
    return serve_artifact_file(
        &mut socket,
        &directory,
        session_id,
        file_name,
        &server.token,
        &method,
        &request,
    )
    .await;
}

/// 按目录提供清单/分片。`directory` 已由调用方按 **ArtifactId** 解析完毕。
///
/// `method` 与 `request` 从外层 handler 传进来：清单分支要判 HEAD，分片分支要读
/// Range 头。它们属于**这一次 HTTP 请求**，与"读哪份产物"无关，所以不该由本函数
/// 重新解析。
#[allow(clippy::too_many_lines)]
async fn serve_artifact_file(
    socket: &mut TcpStream,
    directory: &std::path::Path,
    session_id: u64,
    file_name: &str,
    token: &str,
    method: &str,
    request: &str,
) -> std::io::Result<()> {
    let file_path = directory.join(file_name);
    if file_name == "index.m3u8" {
        let body = std::fs::read_to_string(&file_path).unwrap_or_default();
        let rewritten = rewrite_local_playlist(&body, token);
        // ⚠️ 清单还没写出有效内容时**必须返 5xx**，绝不能返 200 空清单。
        //
        // 为什么：hls.js 对"拿到了完整响应但内容是空的"的解读只有一个 ——
        // `manifestParsingError`，而它走 `handleManifestParsingError`，**那条路径里
        // 没有任何重试逻辑**，`fatal` 直接为 true。也就是说返 200 空清单等于把一个
        // 必死信号交给播放器，它连等的余地都没有。
        //
        // 而"暂时还没准备好"应当用 **503** 表达：hls.js 的 `manifestLoadError` 会走
        // `shouldRetry` → `retryForHttpStatus`，**对 5xx 重试、对 4xx 不重试**，因此
        // 配上调用方的 `manifestLoadingMaxRetry: 4` 正好让它自己等到 ffmpeg 吐出首片。
        //
        // ⚠️ 2026-10-20 更正：这里原本写着"分片路径上的 404 之所以没事，是因为
        // hls.js 对分片另有 `fragLoadingMaxRetry` 那条不同的重试路径"——**那句是错的**。
        // `fragLoadingMaxRetry` 只管"重试几次"，而"要不要重试"由同一个
        // `retryForHttpStatus()` 决定，它对所有 4xx 一律返回 false。也就是说
        // 分片路径上的 404 与 410 一样，第一次就 fatal，重试预算一次都用不上。
        //
        // 空清单只有一种成因：ffmpeg 刚起、还没来得及 prime 出播放列表。实测从起进程
        // 到清单首次含 `#EXTINF` 是 1011ms，这段窗口踩中的概率不低——用户那次 15.7 秒
        // 首屏里就正好撞上（日志 `下发清单 会话=102 字节=1`）。
        //
        // 判据是**没有 `#EXTINF`**，不是"文件为空"：ffmpeg 会先把 header 版清单
        // （`#EXTM3U` + `#EXT-X-TARGETDURATION`，约 40~120 字节）写出来，那段时间
        // 清单非空但没有一条分片。返 200 的话 hls.js 会报 `LEVEL_EMPTY_ERROR`
        // （无 ENDLIST 被判成 live 流）→ 落到 `levelLoadingMaxRetry`，重试预算被花在
        // 另一条路径上；返 503 才回到注释上面写的那条 `manifestLoadingMaxRetry`。
        if !playlist_has_segment(&rewritten) {
            // 清单里一条分片都没有：只能是"刚起转、还没写出来"。
            //
            // 这里刻意**不套停滞判据**：连第一片都没落时 `transcode_started_writing`
            // 恒为假，套上去只会让判断永远不成立、代码读起来自相矛盾。
            // 这种状态的出口交给播放器：503 会让 hls.js 按 `manifestLoadingMaxRetry`
            // 重试；重试耗尽就 fatal → 前端重新 open_stream（重新签名、可能换到更快的
            // CDN 节点），比在后端猜"是不是卡了"更准确。
            crate::trace::log(format!(
                "[vsr] 本地服务：清单尚未就绪 会话={session_id}（返 503 让播放器重试）"
            ));
            write_plain(socket, "503 Service Unavailable", "增强播放清单尚未就绪").await?;
            return Ok(());
        }
        // 播放器每次重新拉清单都会打一行：清单已经被拉了几次，直接反映"卡了多久"。
        crate::trace::log(format!(
            "[vsr] 本地服务：下发清单 会话={session_id} 字节={}",
            rewritten.len()
        ));
        let headers = format!(
            "{}Content-Type: application/vnd.apple.mpegurl\r\nAccept-Ranges: bytes\r\n",
            cors_headers()
        );
        write_head(
            socket,
            "200 OK",
            &headers,
            rewritten.len() as u64,
            None,
            method == "HEAD",
        )
        .await?;
        if method != "HEAD" {
            socket.write_all(rewritten.as_bytes()).await?;
        }
        socket.flush().await?;
        return Ok(());
    }
    let Ok(metadata) = std::fs::metadata(&file_path) else {
        // 分片还没落盘、播放器却在要它——这就是"一直加载等待"最直接的一行证据。
        //
        // ⚠️ 但要分两种情形：
        //   - 转码进程还活着：确实是"还没写到"，返 503 让 hls.js 按
        //     `fragLoadingMaxRetry`（24 次 × 300ms）继续等 —— 它多半马上就会写出来；
        //   - 转码进程**已经退出**：这个分片永远不会出现了（源断连 / 5XX 导致 ffmpeg
        //     中途退出）。继续返 5xx 只会让播放器白等满 7 秒重试预算才 fatal，
        //     用户看到的就是"卡住不动"。
        //     410 是 4xx，hls.js 对 4xx 不重试、立刻 fatal → 前端马上进入
        //     「重新 open_stream 续转」的自愈，中断时间从 7 秒降到 1 秒内。
        //
        // ⚠️ 判据必须是**产物是否完整**（清单里有 `#EXT-X-ENDLIST`），
        // 不能用 `job.done`。
        //
        // 2026-10-20 现场：正在播放的会话 106 走的是「同一 vid 正在转码 → 共用产物」
        // 分支（`register_reused` 把它挂到真正在转的 1001935 的目录上），而
        // `register_reused` 会把 `done` 直接置 true —— 它只表示"这个会话号没有
        // 自己的 ffmpeg"，**不表示产物转完了**。拿 done 当"转码已结束"，就会把
        // 一份正在写的产物判成结束：播放器要下一片 → 文件还没写出来 → 410 →
        // hls.js 对 4xx 不重试、立刻 fatal → 自愈重开又是同一个 410，于是
        // 每 0.4 秒刷一条 fragLoadError、最后彻底卡住。
        let artifact_complete = std::fs::read_to_string(directory.join("index.m3u8"))
            .map(|body| body.contains("#EXT-X-ENDLIST"))
            .unwrap_or(false);
        // 转码是否停滞：目录超过 STALL_SECONDS 没有新文件。
        //
        // 判据用**目录内最新文件时间**而不是分片序号：正常转码每 2 秒写一片，
        // 长时间无新文件就是停滞。只在"已经开始写过分片"时才有意义 ——
        // 连第一片都没落时该由"还没写到"处理，不能套停滞判据。
        let stalled = transcode_started_writing(directory)
            && directory_idle_seconds(directory)
                .map(|idle| idle >= STALL_SECONDS)
                .unwrap_or(false);
        // 停滞时先停掉这一路转码，再复查它是否真的已从任务表移除。
        //
        // `stop()` 内的 `task.abort()` 只取消 tokio 任务、靠 `kill_on_drop` 连带
        // 终止子进程，**并不等待进程退出**。若 abort 后会话仍在表里（拿锁失败、
        // 或目录被别的会话重新挂上），产物还可能继续产出，此时返 410 就是谎报
        // "永久失效"，会把用户正常播放的一集打断。
        let still_registered = if stalled {
            stop(session_id);
            jobs()
                .lock()
                .map(|guard| guard.contains_key(&session_id))
                .unwrap_or(true)
        } else {
            true
        };
        let decision = segment_status(artifact_complete, stalled, still_registered);
        crate::trace::log(format!(
            "[vsr] 本地服务：分片缺失 会话={session_id} 文件={file_name} 产物完整={artifact_complete} 停滞={stalled} 会话仍在表={still_registered} → {}",
            decision.http_status()
        ));
        let body = match decision {
            SegmentStatus::MissingForever => "增强播放分片缺失（产物已完整）",
            SegmentStatus::TranscodeStalled => "增强播放转码停滞",
            SegmentStatus::StalledButStillRunning => "增强播放转码停滞待重试",
            SegmentStatus::NotReadyYet => "增强播放分片尚未就绪",
        };
        write_plain(socket, decision.http_status(), body).await?;
        return Ok(());
    };
    let size = metadata.len();
    // ⚠️ 文件存在但**长度为 0**：ffmpeg 刚创建分片、还没写进数据。
    //
    // 2026-10-20 现场（用户第 7 集，画面正常播但日志每 0.4 秒一条 fragLoadError，
    // 本地服务一条日志都没有）：这种"半成品文件"会一路走到下面的 Range 判断，
    // `parse_range` 得到 `(0, 0-1)` → `start > end` → **416 Range Not Satisfiable**，
    // 而 416 是 4xx，hls.js 不重试内容、直接算 fragLoadError，24 次后 fatal。
    // 播放器那边看到的就是"转码跟不上/卡一下"。
    //
    // 正确语义是"还没好、请稍等"——返 503（5xx 会让 hls.js 重试），并且打点留痕
    // （这个分支此前完全静默，正是这次排查卡住的原因）。
    if size == 0 {
        crate::trace::log(format!(
            "[vsr] 本地服务：分片长度为 0（正在写入） 会话={session_id} 文件={file_name}（返 503 让播放器重试）"
        ));
        write_plain(socket, "503 Service Unavailable", "增强播放分片正在写入").await?;
        return Ok(());
    }
    let range = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().to_string())
    });
    let (start, end) = parse_range(range.as_deref(), size).unwrap_or((0, size.saturating_sub(1)));
    if start > end || start >= size {
        // 这个分支以前是静默的：播放器那边只看到 fragLoadError，后端一行日志都没有，
        // 排查时完全不知道请求到底卡在哪。416 说明 Range 与文件长度对不上
        // （典型是文件正在被追加写），如实记一条。
        crate::trace::log(format!(
            "[vsr] 本地服务：Range 不满足 会话={session_id} 文件={file_name} start={start} end={end} size={size}"
        ));
        write_head(
            socket,
            "416 Range Not Satisfiable",
            &cors_headers(),
            0,
            Some((0, size)),
            true,
        )
        .await?;
        return Ok(());
    }
    let content_type = if file_name.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp4"
    };
    let mut headers = cors_headers();
    headers.push_str(&format!("Content-Type: {content_type}\r\n"));
    headers.push_str("Accept-Ranges: bytes\r\n");
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    write_head(
        socket,
        status,
        &headers,
        end - start + 1,
        Some((start, end)),
        method == "HEAD",
    )
    .await?;
    if method != "HEAD" {
        let mut file = std::fs::File::open(&file_path)?;
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(start))?;
        let mut remaining = end - start + 1;
        let mut buffer = [0u8; 64 * 1024];
        while remaining > 0 {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let count = count.min(remaining as usize);
            socket.write_all(&buffer[..count]).await?;
            remaining -= count as u64;
        }
    }
    socket.flush().await
}

/// 清单里是否**已经有至少一条分片**（`#EXTINF` 是唯一标记）。
///
/// ## 为什么单独抽出来（2026-11-08）
///
/// 这个判据被**两处**用来回答"现在能不能把地址交出去"：
///   1. `start_in_directory` 的 `ready()`：决定后端还要不要按住用户等首片；
///   2. `serve_artifact_file` 的清单分支：决定返 200 还是 503。
///
/// 两者必须永远一致。旧实现里 ① 只判"playlist 与 init.mp4 都存在"、② 判
/// "#EXTINF"，于是 ffmpeg 写出 header 版清单（几十字节、零分片）的那个中间态里，
/// ① 认为"好了"并把地址交出去，② 立刻回 503 —— 播放器只能自己重试清单，
/// 日志现场就是 `下发清单 字节=264` → 503 → 3 秒后才有 1045 字节。
///
/// 抽成纯函数之后，"两处判据同源"从注释承诺变成**可单测的结构**。
fn playlist_has_segment(body: &str) -> bool {
    body.contains("#EXTINF")
}

/// ffmpeg 的相对分片地址不带查询串；令牌必须在播放列表阶段补回去。
fn rewrite_local_playlist(body: &str, token: &str) -> String {
    body.lines()
        .map(|line| {
            if line.starts_with('#') {
                if line.starts_with("#EXT-X-MAP:URI=\"") {
                    format!("#EXT-X-MAP:URI=\"init.mp4?token={token}\"")
                } else {
                    line.to_owned()
                }
            } else if line.trim().is_empty() {
                line.to_owned()
            } else {
                format!("{line}?token={token}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn is_segment(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("seg-") else {
        return false;
    };
    let Some(number) = rest.strip_suffix(".m4s") else {
        return false;
    };
    number.len() == 5 && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_range(value: Option<&str>, size: u64) -> Option<(u64, u64)> {
    let value = value?;
    let rest = value.strip_prefix("bytes=")?;
    let (start, end) = rest.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        return Some((size.saturating_sub(suffix), size.saturating_sub(1)));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        end.parse::<u64>().ok()?.min(size.saturating_sub(1))
    };
    Some((start, end))
}

fn cors_headers() -> String {
    "Access-Control-Allow-Origin: *\r\n\
     Access-Control-Allow-Headers: *\r\n\
     Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
     Access-Control-Expose-Headers: Content-Length,Content-Range,Accept-Ranges\r\n\
     X-Content-Type-Options: nosniff\r\n"
        .to_owned()
}

/// 整集 HLS 产物缺失时该回哪个 HTTP 状态。
///
/// 抽成纯函数是为了能被单测钉住：这一处判断直接决定播放器是"继续等"还是
/// "立刻判死"，而两者在 hls.js 里由同一个开关控制 —— `retryForHttpStatus()`
/// 只对 5xx（与超时）重试，对**所有** 4xx 一律不重试。
///
/// 四个分支的语义严格分开：
///
/// | 情形 | 状态 | 理由 |
/// |---|---|---|
/// | 产物已完整（含 `#EXT-X-ENDLIST`），这一片确实不存在 | 410 | 永久失效，重试无意义，前端应重新开播 |
/// | 转码停滞超时，且会话已确认从任务表移除 | 410 | 进程已死、产物不会再增长，只能续转 |
/// | 转码停滞超时，但会话仍在任务表里 | 503 | stop 未生效，产物**仍可能**继续产出 |
/// | 其余（还没写到） | 503 | 常态，让 hls.js 用自己的重试预算等 |
#[derive(Debug, PartialEq, Eq)]
enum SegmentStatus {
    /// 产物完整但缺这一片：永久失效。
    MissingForever,
    /// 转码停滞且已确认停止：需要前端重新开播续转。
    TranscodeStalled,
    /// 会话还在跑、分片只是还没写到：让播放器继续等。
    NotReadyYet,
    /// 停滞但 stop 未生效：仍按"等着"处理，不能谎报永久失效。
    StalledButStillRunning,
}

impl SegmentStatus {
    fn http_status(&self) -> &'static str {
        match self {
            // 4xx：hls.js 不重试，立刻 fatal → 前端进入「重新 open_stream 续转」。
            Self::MissingForever => "410 Gone",
            Self::TranscodeStalled => "410 Gone",
            // 5xx：hls.js 按 fragLoadingMaxRetry（24 次 × 300ms）继续等。
            Self::NotReadyYet => "503 Service Unavailable",
            Self::StalledButStillRunning => "503 Service Unavailable",
        }
    }

    /// 这个状态是否会让 hls.js 重试。
    ///
    /// 只在测试里用：它是对"4xx 不重试、5xx 重试"这条外部约定的**双向断言**，
    /// 生产代码不需要（生产只关心发哪个状态码）。
    #[cfg(test)]
    fn retried_by_hls(&self) -> bool {
        // hls.js 1.7.3：retryForHttpStatus 对 4xx 返回 false、对 5xx 返回 true。
        self.http_status().starts_with('5')
    }
}

/// 判定分片缺失时的响应语义。
///
/// * `artifact_complete` — 清单里已有 `#EXT-X-ENDLIST`（产物转完了）
/// * `stalled` — 目录超过 `STALL_SECONDS` 没有新产出
/// * `still_registered` — 会话是否仍在任务表里（`stop()` 之后复查）
fn segment_status(artifact_complete: bool, stalled: bool, still_registered: bool) -> SegmentStatus {
    if artifact_complete {
        // 产物完整却没有这一片：重试永远等不到，如实报永久失效。
        return SegmentStatus::MissingForever;
    }
    if stalled {
        return if still_registered {
            SegmentStatus::StalledButStillRunning
        } else {
            SegmentStatus::TranscodeStalled
        };
    }
    SegmentStatus::NotReadyYet
}

async fn write_plain(socket: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let mut headers = cors_headers();
    headers.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    write_head(socket, status, &headers, body.len() as u64, None, false).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.flush().await
}

async fn write_head(
    socket: &mut TcpStream,
    status: &str,
    headers: &str,
    length: u64,
    range: Option<(u64, u64)>,
    head_only: bool,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n{headers}");
    if let Some((start, end)) = range {
        // 调用方已将 length 算成 end-start+1；这里补 Content-Range。
        if status.starts_with("206") {
            head.push_str(&format!("Content-Range: bytes {start}-{end}/*\r\n"));
        }
    }
    head.push_str(&format!(
        "Content-Length: {length}\r\nConnection: close\r\n\r\n"
    ));
    socket.write_all(head.as_bytes()).await?;
    socket.flush().await?;
    let _ = head_only;
    Ok(())
}

/// 预转的**在途闸门**（同时最多 1 路）。
///
/// 为什么必须放在 Rust 侧：预转的产物与"正在播的那一集"共用同一块编码单元，而
/// 实测 NVENC **完全串行**（两路 9455+9407ms、四路 20060ms × 4，总时长不因并发减少，
/// 只是把正在播的那一路挤慢 → 分片来不及生成 → hls.js 重试耗尽报错）。
/// 前端的 `MAX_PREWARM_INFLIGHT = 1` 只管得住它自己那条路，画中画、悬停预取、
/// 连播预转各有入口 —— 闸门放在这里才能对所有调用方生效（审查 R3）。
static PREWARM_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 在途计数的配对释放器：无论函数从哪条路径返回（含提前 return）都会减回去。
struct PrewarmGuard;

impl Drop for PrewarmGuard {
    fn drop(&mut self) {
        PREWARM_INFLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// **预转**某一集的整集 HLS，不占用任何播放会话。
///
/// ## 为什么需要它（用户需求：「集与集之间无缝切换」）
///
/// 方案 B 的播放源是整集 HLS，转码耗时实测 5.6 秒（92.7 秒正片）。若这一集等到
/// 用户真的点开/连播到它才开始转，那 5.6 秒就是黑屏等待。
///
/// 而一集正片 50–140 秒，**上一集播放期间完全来得及把下一集转好**。这个函数就是
/// 做这件事：起一路转码写进 `vid-{vid}` 目录（与 `start_video_cache` 同一个目录），
/// 转完自然退出。等用户真的切过去时，`start_video_cache` 命中 `#EXT-X-ENDLIST`
/// 直接返回地址 —— 切换不需要任何等待。
///
/// ## 与播放任务的区别
///
/// - **不占会话号**：用一个专用的大数段做 key，且 `persist = true`。播放会话的
///   `stop()` 不会误删它，8 路淘汰也不会选中它。
/// - **等待整集完成**：调用方后台执行，订阅和预转闸门一直保留到真正转完。
/// - **同产物共享**：预转和前台订阅同一个执行体，已完成缓存直接复用。
///
/// 失败一律静默（只记日志）：预转是纯优化，失败时用户走的就是"现场转"那条老路。
/// 零转码预转在途闸门（最多允许 2 路并发，避免占用 NVENC 串行通道）。
static PREWARM_COPY_INFLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 零转码在途计数的配对释放器。
struct PrewarmCopyGuard;

impl Drop for PrewarmCopyGuard {
    fn drop(&mut self) {
        PREWARM_COPY_INFLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 当前是否允许接纳一条新的后台预转/Copy 预封包任务（无前台播放竞争且无在途预转）。
pub fn can_accept_prewarm() -> bool {
    !has_active_foreground()
        && PREWARM_INFLIGHT.load(Ordering::Acquire) == 0
        && PREWARM_COPY_INFLIGHT.load(Ordering::Acquire) == 0
}

/// **预封装**某一集的零转码 HLS（`-c:v copy`），串行限流为 1 路，不与前台播放及 H.264 预转抢带宽。
pub async fn prewarm_copy_cache(
    vid: &str,
    source_url: &str,
    key_hex: Option<&str>,
    session_slot: u64,
) -> bool {
    if has_active_foreground() {
        crate::trace::log(format!("[vsr] Copy预转跳过（前台正在起播）vid={vid}"));
        return false;
    }
    if PREWARM_INFLIGHT.load(Ordering::Acquire) > 0 {
        crate::trace::log(format!("[vsr] Copy预转跳过（已有H264预转在途）vid={vid}"));
        return false;
    }
    if PREWARM_COPY_INFLIGHT.fetch_add(1, Ordering::AcqRel) > 0 {
        PREWARM_COPY_INFLIGHT.fetch_sub(1, Ordering::AcqRel);
        crate::trace::log(format!("[vsr] Copy预转跳过（已达并发上限）vid={vid}"));
        return false;
    }
    let _guard = PrewarmCopyGuard;

    struct PrewarmSession(u64);
    impl Drop for PrewarmSession {
        fn drop(&mut self) {
            stop(self.0);
        }
    }
    let req = session_request(session_slot);
    let _session = PrewarmSession(session_slot);
    crate::trace::log(format!("[vsr] Copy预转开始 vid={vid} 槽位={session_slot}"));
    match start_copy_cache(session_slot, vid, source_url, key_hex).await {
        Ok(_) => {
            let task = jobs()
                .lock()
                .ok()
                .and_then(|guard| guard.get(&session_slot).map(|job| job.task.clone()));
            if let Some(task) = task {
                if let Err(error) = req.cancel.run(task.wait()).await {
                    if error != crate::media_task::CANCELLED {
                        crate::trace::log(format!("[vsr] Copy预转失败（静默）vid={vid}：{error}"));
                    }
                    return false;
                }
            }
            true
        }
        Err(error) => {
            crate::trace::log(format!("[vsr] Copy预转未启动（静默）vid={vid}：{error}"));
            false
        }
    }
}

pub async fn prewarm_video_cache(
    vid: &str,
    source_url: &str,
    key_hex: Option<&str>,
    session_slot: u64,
    source_kbps: Option<u32>,
) -> bool {
    if has_active_foreground() {
        crate::trace::log(format!("[vsr] 预转跳过（前台正在起播）vid={vid}"));
        return false;
    }
    if PREWARM_COPY_INFLIGHT.load(Ordering::Acquire) > 0 {
        crate::trace::log(format!("[vsr] 预转跳过（已有Copy预转在途）vid={vid}"));
        return false;
    }
    // 闸门：已经有预转在跑就不再接新的（见 `PREWARM_INFLIGHT` 的说明）。
    // 放在最前面——比"查产物"还早，避免多路调用同时走到起 ffmpeg 那一步。
    if PREWARM_INFLIGHT.fetch_add(1, Ordering::AcqRel) > 0 {
        PREWARM_INFLIGHT.fetch_sub(1, Ordering::AcqRel);
        crate::trace::log(format!("[vsr] 预转跳过（已有预转在途）vid={vid}"));
        return false;
    }
    // 从这里开始，任何 return 都会由 guard 把计数减回去。
    let _guard = PrewarmGuard;

    struct PrewarmSession(u64);
    impl Drop for PrewarmSession {
        fn drop(&mut self) {
            stop(self.0);
        }
    }
    let req = session_request(session_slot);
    let _session = PrewarmSession(session_slot);
    crate::trace::log(format!("[vsr] 预转开始 vid={vid} 槽位={session_slot}"));
    match start_video_cache(session_slot, vid, source_url, key_hex, source_kbps).await {
        Ok(_) => {
            let task = jobs()
                .lock()
                .ok()
                .and_then(|guard| guard.get(&session_slot).map(|job| job.task.clone()));
            if let Some(task) = task {
                if let Err(error) = req.cancel.run(task.wait()).await {
                    if error != crate::media_task::CANCELLED {
                        crate::trace::log(format!("[vsr] 预转失败（静默）vid={vid}：{error}"));
                    }
                    return false;
                }
            }
            true
        }
        Err(error) => {
            crate::trace::log(format!("[vsr] 预转未启动（静默）vid={vid}：{error}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 共用全局 `jobs()` / `pending_sessions()` / 测试环境变量的用例串行化，避免并行线程互杀会话。
    static MEDIA_STATE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn tls_verification_option_is_only_for_https() {
        assert!(tls_args("http://127.0.0.1:1/local.mp4").is_empty());
        assert_eq!(
            tls_args("https://cdn.test/video.mp4"),
            &["-tls_verify", "0", "-rw_timeout", "15000000"]
        );
        assert_eq!(
            network_timeout_args("http://127.0.0.1:1/local.mp4"),
            &["-rw_timeout", "15000000"]
        );
        assert!(network_timeout_args("local.mp4").is_empty());
    }

    #[tokio::test]
    async fn stopping_one_session_keeps_shared_transcode_alive() {
        let _lock = MEDIA_STATE_TEST_LOCK.lock().await;
        let registry = Registry::default();
        let task = registry.subscribe("shared-transcode".into(), |_| {
            std::future::pending::<Result<(), String>>()
        });
        let completion = task.completion();
        for id in [9_811_001, 9_811_002] {
            jobs().lock().unwrap().insert(
                id,
                Job {
                    directory: PathBuf::from("fixture-shared"),
                    task: task.clone(),
                    persist: true,
                },
            );
        }
        drop(task);
        stop(9_811_001);
        let remaining = jobs().lock().unwrap().get(&9_811_002).unwrap().task.clone();
        assert!(
            !remaining.is_finished(),
            "取消一个会话不能取消另一个会话的转码"
        );
        drop(remaining);
        stop(9_811_002);
        tokio::time::timeout(Duration::from_secs(1), completion)
            .await
            .expect("最后会话离开应取消转码");
    }

    #[tokio::test]
    async fn stop_cancels_before_signature_and_at_transcode_handoff() {
        let session = session_request(9_811_003);
        stop(9_811_003);
        assert_eq!(
            session.cancel.run(async { Ok(()) }).await,
            Err(crate::media_task::CANCELLED.to_owned())
        );
        // 起播内部与签名入口共用令牌，即使 stop 在两阶段之间到达也不能漏掉。
        let outer = session_request(9_811_004);
        let inner = session_request(9_811_004);
        assert!(Arc::ptr_eq(&outer.cancel, &inner.cancel));
        stop(9_811_004);
        assert!(outer.cancel.is_cancelled() && inner.cancel.is_cancelled());
        let retry = session_request(9_811_004);
        assert!(!retry.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn unready_cancel_does_not_remove_a_later_session_job() {
        let _lock = MEDIA_STATE_TEST_LOCK.lock().await;
        let registry = Registry::default();
        let task = registry.subscribe("same-transcode".into(), |_| {
            std::future::pending::<Result<(), String>>()
        });
        let old = jobs().lock().unwrap().insert(
            42,
            Job {
                directory: PathBuf::from("old"),
                task: task.clone(),
                persist: true,
            },
        );
        drop(old);
        let pending = PendingJob {
            session_id: 42,
            directory: PathBuf::from("old"),
            task: task.clone(),
            persist: true,
            active: true,
        };
        jobs().lock().unwrap().insert(
            42,
            Job {
                directory: PathBuf::from("new"),
                task: task.clone(),
                persist: true,
            },
        );
        drop(pending);
        assert!(
            jobs().lock().unwrap().contains_key(&42),
            "旧请求离开不能取消后来接管的会话"
        );
        drop(jobs().lock().unwrap().remove(&42));
        drop(task);
    }

    #[tokio::test]
    async fn temporary_directory_is_removed_only_after_all_observers_leave() {
        let _lock = MEDIA_STATE_TEST_LOCK.lock().await;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/playback-parallel-20261112/media/temp-directory-test");
        let directory = root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("marker"), b"keep until last").unwrap();
        let task = Registry::default().subscribe("temp-directory".into(), |_| {
            std::future::pending::<Result<(), String>>()
        });
        for id in [43, 44] {
            jobs().lock().unwrap().insert(
                id,
                Job {
                    directory: directory.clone(),
                    task: task.clone(),
                    persist: false,
                },
            );
        }
        drop(task);
        stop(43);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            directory.join("marker").is_file(),
            "仍有会话使用时不能删临时产物"
        );
        stop(44);
        tokio::time::timeout(Duration::from_secs(1), async {
            while directory.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("最后使用者离开后应回收临时目录");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_transcode_cancels_and_cleans_temporary_artifact() {
        let _lock = MEDIA_STATE_TEST_LOCK.lock().await;
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/playback-parallel-20261112/native/media/motion-24.mp4");
        if !source.is_file() {
            eprintln!("缺少本地转码样本，跳过真实转码测试");
            return;
        }
        let session = 9_811_009;
        let directory = media_root().join(format!("test-{session}"));
        let _ = std::fs::remove_dir_all(&directory);
        unsafe {
            std::env::set_var("TTV_TEST_TRANSCODE_COPY", "1");
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let body = tokio::fs::read(source).await.unwrap();
            for _ in 0..8 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n")
                        && request.len() < 16384
                    {
                        let Ok(Ok(count)) = tokio::time::timeout(
                            Duration::from_secs(2),
                            tokio::io::AsyncReadExt::read(&mut stream, &mut chunk),
                        )
                        .await
                        else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..count]);
                    }
                    let request = String::from_utf8_lossy(&request);
                    let range = request
                        .lines()
                        .find_map(|line| line.strip_prefix("Range: bytes="))
                        .unwrap_or("0-");
                    let (first, last) = range.split_once('-').unwrap_or(("0", ""));
                    let first: usize = first.parse().unwrap_or(0);
                    let last = last.parse().unwrap_or(body.len() - 1).min(body.len() - 1);
                    use tokio::io::AsyncWriteExt;
                    let header = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {first}-{last}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len(), last + 1 - first);
                    let _ = stream.write_all(header.as_bytes()).await;
                    // 慢速滴灌下发分片：init.mp4 已落在产物目录内，若一次性发完首个分片会在 40ms 内
                    // 满足 ready() 并返回 Ok(url)，来不及测到"转码进行中取消"。每片暂停确保取消命中在途阶段。
                    let slice = &body[first..=last];
                    for chunk in slice.chunks(32 * 1024) {
                        if stream.write_all(chunk).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(80)).await;
                    }
                });
            }
        });
        let source_url = format!("http://127.0.0.1:{port}/motion.mp4");
        let started = start_in_directory(
            session,
            &source_url,
            None,
            directory.clone(),
            false,
            None,
            OutputMode::H264,
            false,
        );
        tokio::pin!(started);
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if directory.is_dir() {
                    break true;
                }
                if let Ok(error) =
                    tokio::time::timeout(Duration::from_millis(20), started.as_mut()).await
                {
                    panic!("真实转码未建立目录就结束：{error:?}");
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("真实转码五秒未建起目录"));
        assert!(outcome);
        tokio::select! {
            res = started.as_mut() => panic!("取消前不应提前结束：{res:?}"),
            _ = tokio::time::sleep(Duration::from_millis(120)) => {}
        }
        stop(session);
        tokio::time::timeout(Duration::from_secs(3), started)
            .await
            .expect("取消后转码调用必须退出")
            .expect_err("取消不能被误报为转码成功");
        tokio::time::timeout(Duration::from_secs(3), async {
            while directory.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("临时转码目录应在取消后清理");
        server.abort();
        unsafe {
            std::env::remove_var("TTV_TEST_TRANSCODE_COPY");
        }
    }

    /// 低配档必须**降分辨率**，而不是只降码率。
    ///
    /// 这是「遇到配置低的用户则自适应」唯一可被单测钉住的一面（探测本身依赖真实
    /// 硬件）。低配档的全部性能保障就是这条 `-vf scale=-2:720` —— 实测只降码率对
    /// CPU 几乎无帮助（1080p cq26→cq34 只从 5499ms 降到 5429ms），而降分辨率到
    /// 720p 能到 4027–4691ms。改参数时若把它删掉，低配用户会立刻退回"一直转圈"。
    #[test]
    fn low_tier_drops_resolution() {
        assert!(
            LOW_TIER_ARGS.contains(&"-vf"),
            "低配档缺少 -vf，等于只降码率、对 CPU 无实质帮助"
        );
        assert!(
            LOW_TIER_ARGS
                .iter()
                .any(|value| value.contains("scale=-2:720")),
            "低配档的分辨率目标应为 720p"
        );
    }

    /// 三档都必须钉住关键帧间隔。
    ///
    /// 缺了它 HLS 分片会落到源流的关键帧间隔上（实测 8.33 秒），`hls_time=2` 形同
    /// 虚设 —— 那正是「前 8 秒转换」的由来。三档共用同一套分片节奏，用户换机器不
    /// 该改变"多久能看到画面"。
    #[test]
    fn every_tier_pins_keyframe_interval() {
        for (name, args) in [
            ("hardware", NVENC_ARGS),
            ("software-fast", X264_ARGS),
            ("software-low", LOW_TIER_ARGS),
        ] {
            for flag in ["-g", "-keyint_min", "-sc_threshold"] {
                assert!(
                    args.contains(&flag),
                    "{name} 档缺少 {flag}，分片时长会退化成源流关键帧间隔"
                );
            }
        }
    }

    /// 三档参数必须两两不同 —— 否则"自适应"是假的。
    #[test]
    fn tiers_are_actually_distinct() {
        assert_ne!(NVENC_ARGS, X264_ARGS, "硬件档与软件档参数相同");
        assert_ne!(X264_ARGS, LOW_TIER_ARGS, "软件快档与低配档参数相同");
    }

    /// 码率上限必须**跟着源档码率**走，而不是写死一个数。
    ///
    /// 实测（同一段 20 秒 1080p 真人短剧，SSIM 相对源）：固定 `-maxrate 1100k`（上一版）
    /// 是 **0.9814**（用户报的"转码后画质下降很多"），按源 2.24× 自适应是 **0.9917**
    /// （体积只多 25%），完全不设上限是 0.9950 但体积是源的 7.3 倍（8.53MB vs 1.16MB）。
    /// 所以两件事都要钉住：上限存在（否则体积失控）、且按源算（否则高码率源掉画质）。
    #[test]
    fn rate_limits_follow_the_source() {
        // 实测样本的源档是 733kbps：3.0× = 2199k，bufsize = 2× = 4398k
        assert_eq!(rate_limits(Some(733)), (2199, 4398));
        // 源码率翻倍，上限必须跟着翻倍（证明它不是固定值）
        assert_eq!(rate_limits(Some(1466)), (4398, 8796));
        // 拿不到源码率 ⇒ 中性默认值
        assert_eq!(rate_limits(None), (3000, 6000));
        // 荒谬的小值按"拿不到"处理（上游字段出过 100 这种不可信的值）
        assert_eq!(rate_limits(Some(0)), (3000, 6000));
        // 低码率源（实测档位表里出现过 300kbps 档）**不能被下限抬高**：
        // 3.0×300 = 900，等于源码率的 3 倍，这是倍率本身的上限，不是下限抬的。
        assert_eq!(rate_limits(Some(300)), (900, 1800));
        // 边界：低于 100k 的"码率"一律视为脏数据（不是码率），按拿不到处理；
        // 极高值钳到 8000k。
        assert_eq!(rate_limits(Some(10)), (3000, 6000));
        assert_eq!(rate_limits(Some(100)), (300, 600));
        assert_eq!(rate_limits(Some(99_999)).0, 8000);
    }

    /// 上限是**运行时追加**到编码参数里的（常量表里没有它）。
    ///
    /// 这一条守着"常量改了、追加逻辑没改"的漂移：`video_encoder_args` 调的就是它。
    #[test]
    fn rate_limits_are_appended_to_encoder_args() {
        let lines = rate_lines(Some(733));
        assert_eq!(lines, vec!["-maxrate", "2199k", "-bufsize", "4398k"]);
    }

    /// 首片必须比 `-hls_time` 更早切出去，且关键帧节奏只能有一条规则。
    ///
    /// `-hls_init_time 1` 是"首片多早写得出来"的一个手段：不给它，首片要等满
    /// `-hls_time`（2 秒）才有机会落盘（本地源实测 652ms → 465ms）。⚠️ 但它在
    /// **GOP 比 `hls_time` 长的源上不起作用** —— 网络源实测（15fps、`-g 48` ⇒ GOP
    /// 3.2 秒）首片仍是 3.2 秒一片、完成时间在噪声内。它属于"GOP ≤ hls_time 才显形"
    /// 的改动，留着无害；别把它当成首屏时间的保证。
    ///
    /// `-force_key_frames` 必须**不在**这套参数里：它对 NVENC 无效（实测分片照样
    /// 8.33 秒），而对 libx264 又是与 `-g` 并行的第二条规则（不同帧率下两者不重合，
    /// 取并集＝无谓更密的关键帧）。软件档删掉它的实测代价与收益见调用点注释。
    /// `-force_key_frames` 的取舍：**硬件档不许有、软件档必须有**。
    ///
    /// 硬件档有它是纯误导（NVENC 不认表达式，实测分片照样 8.33 秒）；软件档没有它时
    /// 关键帧只能落在 `-g 48` 上（15fps 源＝3.2 秒），实测首片 3.2→2.0 秒、体积 -25%。
    #[test]
    fn hls_packaging_keeps_first_segment_early() {
        assert!(
            HLS_PACKAGING_ARGS.contains(&"-hls_init_time"),
            "缺少 -hls_init_time，首片要等满 -hls_time 才出得来"
        );
        assert!(
            !HLS_PACKAGING_ARGS.contains(&"-force_key_frames"),
            "硬件档不该有 -force_key_frames：NVENC 不认这个表达式，写它是误导"
        );
        assert!(
            HLS_PACKAGING_ARGS_X264.contains(&"-force_key_frames"),
            "软件档需要 -force_key_frames：实测首片 3.2→2.0 秒、首片体积 -25%"
        );
    }

    /// Rust 与 worker 的三档编码参数必须逐项一致。
    ///
    /// 参数表在两个文件里各有一份（这里是 `NVENC_ARGS` / `X264_ARGS` / `LOW_TIER_ARGS`，
    /// worker 是 `_NVENC_ARGS` / `_X264_ARGS` / `_X264_LOW_ARGS`），此前已经漂移过两次
    /// （GOP 一次、`-profile:v high` 与码率上限又一次），症状是"同一条链路换集后分片粒度、
    /// 体积、首屏突然变样"，极难定位。worker 那边的注释写着"必须保持一致"，但那条承诺
    /// 过去完全靠人盯着 —— 这个测试把它变成**会失败的检查**。
    ///
    /// 只比元素的多重集、不比顺序：两边都是输出侧选项，顺序没有语义（低配档的
    /// `-vf scale=-2:720` 位置就与 Rust 侧不同）。读不到 `worker.py` 时直接跳过
    /// （打包后的运行环境里没有源码，这个测试只在开发机上才有意义）。
    #[test]
    fn worker_encoder_args_match_rust() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources/shortdrama-worker/worker.py");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        for (python_name, rust_args) in [
            ("_NVENC_ARGS", NVENC_ARGS),
            ("_X264_ARGS", X264_ARGS),
            ("_X264_LOW_ARGS", LOW_TIER_ARGS),
        ] {
            let mut worker = python_list_literals(&text, python_name)
                .unwrap_or_else(|| panic!("worker.py 里找不到 {python_name} 的列表字面量"));
            let mut rust: Vec<String> = rust_args.iter().map(|value| (*value).to_owned()).collect();
            worker.sort();
            rust.sort();
            assert_eq!(
                worker, rust,
                "{python_name} 与 Rust 侧同名档位的参数漂移了（两边必须同一套）"
            );
        }
    }

    /// 清单"就绪"的判据必须与**服务端可下发**的判据同源。
    ///
    /// ## 这条守的是什么（2026-11-08）
    ///
    /// `start_in_directory` 的 `ready()` 决定"后端还要不要按住用户等首片"，
    /// `serve_artifact_file` 的清单分支决定"返 200 还是 503"。两者一度分叉：
    ///
    /// ```text
    /// ready()            → playlist.is_file() && init.mp4.is_file()
    /// 服务端清单分支      → rewritten.contains("#EXTINF")
    /// ```
    ///
    /// ffmpeg 写清单的顺序是「先 header（`#EXTM3U` + TARGETDURATION，零分片）→
    /// 再 init.mp4 → 最后 prime 首片」。于是中间态里 `ready()` 已经是 true，
    /// 后端把地址交了出去，而服务端对同一份清单回 503 —— 播放器只能自己重试。
    ///
    /// 实测（ttv-playback.log，会话 114，真实红果源）：
    ///   410.747 `下发清单 字节=264`（header 版）→ 503
    ///   413.749 `下发清单 字节=1045`（**3 秒后**才有分片）→ loadeddata 3040ms
    ///   416.746 首片迟到，再等一轮 → `出画 7229ms`
    ///
    /// 也就是说这次开播里约 3 秒是在「后端已经说就绪、服务端还没内容」的缝里空等。
    #[test]
    fn ready_condition_matches_served_playlist() {
        // header 版清单：ffmpeg 刚起、还没有任何分片。**不算就绪** ——
        // 这正是旧实现误判的那个中间态。
        let header_only = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n";
        assert!(
            !playlist_has_segment(header_only),
            "零分片的 header 版清单不能算就绪：服务端对它会回 503"
        );

        // 空清单（文件刚被创建）同理。
        assert!(!playlist_has_segment(""), "空清单不能算就绪");

        // 真正可播的清单：有 #EXTINF 与分片行。
        let playable = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:3.840000,\nseg-00000.m4s\n";
        assert!(
            playlist_has_segment(playable),
            "含分片的清单必须算就绪，否则后端会白等到超时"
        );
    }

    /// 分片缺失的四种情形必须映射到正确的 HTTP 语义。
    ///
    /// 这条测试守的是一个**曾经真实存在的缺陷**：分片"还没写到"时返 404，
    /// 而注释声称"404 让 hls.js 按 fragLoadingMaxRetry 继续等"。hls.js 1.7.3 的
    /// `retryForHttpStatus()`（`src/utils/error-helper.ts:86`）对**所有 4xx**
    /// 返回 false —— 404 与 410 效果完全相同：首次请求就 fatal，24 次重试预算
    /// 一次都不会用到。于是"分片晚两秒落地"这种常态被升级成致命错误。
    ///
    /// 实测代价（ttv-playback.log，会话 104）：100.8s → 247.2s 累计约 150 条
    /// `fragLoadError`，界面锁在缓冲态 158 秒。
    #[test]
    fn segment_missing_status_semantics() {
        use SegmentStatus::*;

        // 产物已完整、这一片确实没有 → 永久失效，不该重试。
        assert_eq!(segment_status(true, false, true), MissingForever);
        assert_eq!(segment_status(true, false, true).http_status(), "410 Gone");
        assert!(!segment_status(true, false, true).retried_by_hls());

        // 产物完整但不完整的清单也判永久失效（artifact_complete 优先）。
        assert_eq!(segment_status(true, true, true), MissingForever);

        // 停滞且已确认停止 → 只能续转，410 正确。
        assert_eq!(segment_status(false, true, false), TranscodeStalled);
        assert_eq!(segment_status(false, true, false).http_status(), "410 Gone");

        // 停滞但 stop 未生效 → 产物还可能产出，不能谎报永久失效。
        assert_eq!(segment_status(false, true, true), StalledButStillRunning);
        assert_eq!(
            segment_status(false, true, true).http_status(),
            "503 Service Unavailable"
        );
        assert!(segment_status(false, true, true).retried_by_hls());

        // 常态：分片还没写到 → 必须让 hls.js 继续等（5xx 才会重试）。
        assert_eq!(segment_status(false, false, true), NotReadyYet);
        assert_eq!(
            segment_status(false, false, true).http_status(),
            "503 Service Unavailable"
        );
        assert!(segment_status(false, false, true).retried_by_hls());
    }

    /// "还没写到"的分片必须让 hls.js 重试，否则一切退避都形同虚设。
    ///
    /// 单独钉一条：这条一旦回归，用户看到的正是「画面停住、进度条不动、
    /// 日志里每 400ms 一条 fragLoadError」那种卡死。
    #[test]
    fn not_ready_segment_is_retried_by_hls() {
        assert!(
            SegmentStatus::NotReadyYet.retried_by_hls(),
            "分片未就绪必须返 5xx —— hls.js 对 4xx 不重试，重试预算完全用不上"
        );
        assert!(
            !SegmentStatus::MissingForever.retried_by_hls(),
            "产物完整后仍缺片属于永久失效，不该让播放器空转重试"
        );
    }

    /// ArtifactId 必须拒绝一切能跳出 `media_root()` 的输入。
    ///
    /// 这条是**安全边界**：`artifact` 直接来自 HTTP 路径。若不校验，就能用
    /// `..` / 绝对路径 / 交替分隔符读到任意文件（播放器是个本地服务，
    /// 但同机的其它程序可以扫 127.0.0.1 的端口）。
    #[test]
    fn artifact_id_rejects_path_escape() {
        for bad in [
            "",
            ".",
            "..",
            "../etc",
            "..\\windows",
            "a/b",
            "a\\b",
            ".hidden",
            "a..b",
            "C:\\Windows",
            "/abs/path",
            "with space",
            "with*star",
            "中文目录",
        ] {
            assert!(
                ArtifactId::parse(bad).is_none(),
                "ArtifactId 不该接受 {bad:?}（越界风险）"
            );
        }
        // 长度上限：128 是为了挡住超长名导致的路径拼接异常。
        assert!(ArtifactId::parse(&"a".repeat(129)).is_none());
    }

    /// 真实产物目录名必须能被原样接受。
    ///
    /// 两种命名都来自生产代码：`vid-{vid}`（按内容建、跨会话复用）与
    /// `{session_id}-{uuid}`（临时目录）。它们就是"不可变身份"本身。
    #[test]
    fn artifact_id_accepts_real_directory_names() {
        for good in [
            "vid-7679731146971696152",
            "vid-guo-0123456789abcdef",
            "104-3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c",
        ] {
            let id = ArtifactId::parse(good).unwrap_or_else(|| panic!("应接受 {good:?}"));
            assert_eq!(id.as_str(), good);
        }
    }

    /// **本次修复的核心回归**：URL 里必须能唯一确定产物，且与会话号解耦。
    ///
    /// 旧形态 `/rtx/{session}/index.m3u8` 的身份只有会话号，而会话号会被
    /// `register_reused` 复用 —— 实测同一个 104 先后指向两份不同 vid 的产物。
    /// 新形态把目录名放进路径，会话号只是兼容字段。
    #[test]
    fn artifact_url_pins_content_not_session() {
        let a = ArtifactId::parse("vid-1111111111111111111").unwrap();
        let b = ArtifactId::parse("vid-2222222222222222222").unwrap();
        // 同一个会话号、不同产物 → 必须是两个不同的 URL。
        let url_a = artifact_url(49701, 104, &a, "tok");
        let url_b = artifact_url(49701, 104, &b, "tok");
        assert_ne!(
            url_a, url_b,
            "同一会话号必须能区分不同产物，否则换集后旧 URL 会读到新内容"
        );
        assert!(url_a.contains("vid-1111111111111111111"));
        assert!(url_a.contains("/rtx/104/"));
        assert!(url_a.ends_with("/index.m3u8?token=tok"));
        // 不同会话号、同一产物 → 指向同一份内容（会话号不参与定位）。
        let url_a2 = artifact_url(49701, 999, &a, "tok");
        assert!(url_a2.contains("vid-1111111111111111111"));
    }

    /// 清单重写必须让 `init.mp4` 与分片保持**相对路径**。
    ///
    /// 这是身份绑定能被分片继承的前提：相对 URI 由播放器按清单自己的目录解析，
    /// 于是清单来自哪个 artifact、分片就一定来自同一个 artifact —— 不会出现
    /// "清单是 A 的、分片是 B 的"那种串线。
    #[test]
    fn playlist_rewrite_keeps_segments_relative_to_artifact() {
        let body = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:3.2,\nseg-00000.m4s\n";
        let rewritten = rewrite_local_playlist(body, "tok");
        // init 与分片都只补 token，**不补任何目录前缀**。
        assert!(rewritten.contains("#EXT-X-MAP:URI=\"init.mp4?token=tok\""));
        assert!(rewritten.contains("seg-00000.m4s?token=tok"));
        assert!(
            !rewritten.contains("/rtx/"),
            "清单里不该出现绝对路径 —— 那会让分片脱离当前 artifact"
        );
    }

    /// 取出 Python 源码里 `<name> = [ ... ]` 这一段之间所有字符串字面量的值。
    ///
    /// 只够应付当前这份列表（纯双引号字面量、无嵌套括号），刻意不写通用解析器：
    /// 它服务于上面那条一致性断言，不是通用的 Python 解析。
    fn python_list_literals(text: &str, name: &str) -> Option<Vec<String>> {
        let start = text.find(&format!("{name} = ["))?;
        let rest = &text[start..];
        let end = rest.find(']')?;
        Some(
            rest[..end]
                .split('"')
                .skip(1)
                .step_by(2)
                .map(|piece| piece.to_owned())
                .collect(),
        )
    }

    #[tokio::test]
    #[ignore = "真实源流冒烟：设置 TTV_VSR_SMOKE_SOURCE=http://.../video.mp4 后手动运行"]
    async fn enhanced_hls_smoke() {
        let source = std::env::var("TTV_VSR_SMOKE_SOURCE").expect("缺少 TTV_VSR_SMOKE_SOURCE");
        let url = start(0x565352, &source).await.expect("增强流应能启动");
        println!("TTV_VSR_SMOKE_URL={url}");
        let body = reqwest::get(&url)
            .await
            .expect("增强播放列表应可读")
            .text()
            .await
            .expect("播放列表正文");
        assert!(body.starts_with("#EXTM3U"));
        if std::env::var_os("TTV_VSR_SMOKE_HOLD").is_some() {
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
        stop(0x565352);
    }

    #[test]
    fn range_parser_supports_browser_requests() {
        assert_eq!(parse_range(Some("bytes=0-1"), 10), Some((0, 1)));
        assert_eq!(parse_range(Some("bytes=8-"), 10), Some((8, 9)));
        assert_eq!(parse_range(Some("bytes=-2"), 10), Some((8, 9)));
        assert_eq!(parse_range(Some("bytes=20-30"), 10), Some((20, 9)));
    }

    #[test]
    fn playlist_rewrite_forces_local_init_and_tokens_segments() {
        let raw = "#EXTM3U\n#EXT-X-MAP:URI=\"C:\\\\cache\\\\init.mp4\"\nseg-00000.m4s\n";
        let rewritten = rewrite_local_playlist(raw, "tok");
        assert!(rewritten.contains("#EXT-X-MAP:URI=\"init.mp4?token=tok\""));
        assert!(rewritten.contains("seg-00000.m4s?token=tok"));
    }

    #[test]
    fn enhancement_only_targets_http_sources() {
        assert!(needs_enhancement("https://cdn.example/video.mp4"));
        assert!(needs_enhancement("http://127.0.0.1:57225/stream?u=x"));
        assert!(!needs_enhancement("asset:///C:/video.mp4"));
    }

    #[test]
    fn copy_mode_uses_isolated_dir_and_distinct_fingerprint() {
        let h264 = video_cache_dir("12345");
        let copy = video_copy_dir("12345");
        assert_ne!(h264, copy);
        assert!(copy
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("vid-copy-"));
        assert!(ArtifactId::from_directory(&copy).is_some());
        let dummy = std::path::Path::new("ffmpeg.exe");
        assert_ne!(
            encoder_fingerprint(dummy, OutputMode::Copy),
            encoder_fingerprint(dummy, OutputMode::H264)
        );
    }

    #[test]
    fn stopped_session_is_tracked_and_cleared_on_restart() {
        let sid = 9_811_055;
        stop(sid);
        assert!(is_session_stopped(sid));
        let _req = session_request(sid);
        assert!(!is_session_stopped(sid));
    }

    #[test]
    fn prewarm_yields_and_hls_flags_keep_atomic_temp_file() {
        assert!(HLS_PACKAGING_ARGS.contains(&"independent_segments+temp_file"));
        assert!(HLS_PACKAGING_ARGS_X264.contains(&"independent_segments+temp_file"));
        assert!(!NVENC_ARGS.iter().any(|arg| arg.contains("hqdn3d")));
        assert!(!X264_ARGS.iter().any(|arg| arg.contains("hqdn3d")));

        let _lock = MEDIA_STATE_TEST_LOCK.blocking_lock();
        let fg_id = 9_811_056;
        let _fg = session_request(fg_id);
        let prewarm_id = PREWARM_SESSION_BASE + 7;
        assert!(is_prewarm_session(prewarm_id));
        assert!(has_active_foreground());
        assert!(!can_accept_prewarm());
        stop(fg_id);
        assert!(!has_active_foreground());
        assert!(can_accept_prewarm());

        // 已完成的前台 Job 不应阻塞下一集的后台预热
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let registry = Registry::default();
            let done_task = registry.subscribe("done-fg".into(), |_| async { Ok(()) });
            let _ = done_task.wait().await;
            jobs().lock().unwrap().insert(
                fg_id,
                Job {
                    directory: PathBuf::from("fixture-done"),
                    task: done_task,
                    persist: true,
                },
            );
            assert!(!has_active_foreground());
            assert!(!has_pending_foreground());
            assert!(can_accept_prewarm());
            stop(fg_id);
        });

        let root = std::env::temp_dir().join(format!("ttv-completed-hls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("init.mp4"), vec![1u8; 64]).unwrap();
        if let Ok(ffmpeg) = crate::short_drama_app::ffmpeg_path() {
            std::fs::write(
                root.join(ENCODER_STAMP_FILE),
                encoder_fingerprint(&ffmpeg, OutputMode::H264),
            )
            .unwrap();
        }
        // 0.000s / 24B 毒化缓存目录必须被拒绝（防止误当已完成缓存导致播放器一直 loadstart）
        std::fs::write(root.join("seg-00000.m4s"), vec![0u8; 24]).unwrap();
        std::fs::write(
            root.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:0.000000,\nseg-00000.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert!(!is_completed_hls_dir(&root, OutputMode::H264));
        // 合法 >=1s 且 >=1024B 分片必须被接纳
        std::fs::write(root.join("seg-00000.m4s"), vec![7u8; 2048]).unwrap();
        std::fs::write(
            root.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:2.5,\nseg-00000.m4s\n#EXTINF:3.25,\nseg-00001.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(playlist_duration_ms(&root.join("index.m3u8")), Some(5750));
        assert!(is_completed_hls_dir(&root, OutputMode::H264));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn start_in_directory_keeps_job_and_playlist_alive_after_return() {
        let _lock = MEDIA_STATE_TEST_LOCK.lock().await;
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/playback-parallel-20261112/native/media/motion-24.mp4");
        if !source.is_file() {
            eprintln!("缺少本地转码样本，跳过 start_in_directory 存活回归测试");
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = tokio::fs::read(&source).await.unwrap();
        let _server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 16384 {
                        let Ok(Ok(count)) = tokio::time::timeout(
                            Duration::from_secs(2),
                            tokio::io::AsyncReadExt::read(&mut stream, &mut chunk),
                        )
                        .await
                        else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..count]);
                    }
                    let req = String::from_utf8_lossy(&request);
                    let range = req
                        .lines()
                        .find_map(|line| line.strip_prefix("Range: bytes="))
                        .unwrap_or("0-");
                    let (first, last) = range.split_once('-').unwrap_or(("0", ""));
                    let first: usize = first.parse().unwrap_or(0);
                    let last = last
                        .parse()
                        .unwrap_or(body.len().saturating_sub(1))
                        .min(body.len().saturating_sub(1));
                    use tokio::io::AsyncWriteExt;
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {first}-{last}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        last + 1 - first
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&body[first..=last]).await;
                });
            }
        });

        let run_tag = uuid::Uuid::new_v4().simple().to_string();
        for (idx, persist) in [(0u64, true), (1u64, false)] {
            let session = 9_811_080 + idx;
            let directory = media_root().join(format!("test-keepalive-{run_tag}-{persist}"));
            let _ = std::fs::remove_dir_all(&directory);
            let prev_copy_env = std::env::var_os("TTV_TEST_TRANSCODE_COPY");
            unsafe {
                std::env::set_var("TTV_TEST_TRANSCODE_COPY", "1");
            }
            let source_url = format!("http://127.0.0.1:{port}/motion.mp4");
            let url = start_in_directory(
                session,
                &source_url,
                None,
                directory.clone(),
                persist,
                None,
                OutputMode::H264,
                false,
            )
            .await
            .expect("start_in_directory 应成功返回 HLS 地址");
            match prev_copy_env {
                Some(val) => unsafe { std::env::set_var("TTV_TEST_TRANSCODE_COPY", val) },
                None => unsafe { std::env::remove_var("TTV_TEST_TRANSCODE_COPY") },
            }
            assert!(url.contains("/index.m3u8?"), "unexpected url: {url}");

            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(
                jobs().lock().unwrap().contains_key(&session),
                "persist={persist} 返回 Ok 后会话不应被 PendingJob::drop 移除"
            );
            assert!(
                directory.join("index.m3u8").is_file(),
                "persist={persist} 返回 Ok 后 index.m3u8 不应被 PendingJob::drop 删除"
            );
            assert!(
                directory.join("init.mp4").is_file(),
                "persist={persist} 返回 Ok 后 init.mp4 必须留在产物目录内（不能落去父目录）"
            );

            stop(session);
            if !persist {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while directory.exists() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("persist=false 调用 stop 后临时目录应被清理");
            } else {
                let _ = std::fs::remove_dir_all(&directory);
            }
        }
    }
}
