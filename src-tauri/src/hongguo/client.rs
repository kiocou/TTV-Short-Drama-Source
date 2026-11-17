//! 匿名红果客户端：注册、签名请求、播放解析。
//!
//! 移植自 `sdk/client.go` + `sdk/media.go`。整个链路不再需要 Python：
//! 首次调用自动注册一台匿名设备（`tt_encrypt` 信封），之后每次请求都带
//! 六代签名头（gorgon / helios / medusa / ladon / argus）。
//!
//! ## 错误分类
//!
//! 上游用「HTTP 200 + 空 body」表达静默拒绝，用业务码表达具体原因。
//! 这两类必须分开：前者换设备可能恢复，后者换设备没用。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::hongguo::consts::{
    API_HOST, APP_ID, PATH_PLAYBACK_MULTI, PATH_REGISTER, PLAYBACK_API_HOSTS, REGISTER_HOST,
};
use crate::hongguo::device::DeviceProfile;
use crate::hongguo::drm::{derive as derive_key, ContentKey};
use crate::hongguo::media::{parse_fallback_video_info, parse_playback, Playback};
use crate::hongguo::register::{build_register_body, content_md5, parse_register};
use crate::hongguo::sign::{
    device_query_pairs, draw_entropy, reading_request_id, sign_request, Pairs, SearchIdentity,
};

/// 客户端错误。分类决定调用方该重试、换设备还是直接报错。
#[derive(Debug, Clone)]
pub enum HongguoError {
    /// 网络层失败（可重试）。
    Transport(String),
    /// 需要登录，匿名不可用。
    AuthenticationRequired,
    /// 被限流（可重试）。
    RateLimited,
    /// 服务端对匿名设备返回空结果 —— 通常是设备本身不受信任。
    AnonymousLimited,
    /// 设备被拒绝，需要 `reset_device` 后重试。
    DeviceRejected,
    /// 内容下架 / 不存在 / 无权访问。
    ContentUnavailable(String),
    /// 业务码错误。
    Code { code: i64, message: String },
    /// 其他（含解析失败）。
    Other(String),
}

impl std::fmt::Display for HongguoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "网络请求失败：{e}"),
            Self::AuthenticationRequired => write!(f, "内容需要登录，匿名设备无法访问"),
            Self::RateLimited => write!(f, "请求过于频繁，被服务端限流"),
            Self::AnonymousLimited => write!(f, "服务端对匿名设备返回空结果"),
            Self::DeviceRejected => write!(f, "匿名设备被拒绝，需要重置后重新注册"),
            Self::ContentUnavailable(m) => write!(f, "内容不可用：{m}"),
            Self::Code { code, message } => write!(f, "接口错误 {code}：{message}"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for HongguoError {}

impl HongguoError {
    /// 是否值得原样重试。
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::RateLimited)
    }

    /// 是否是「这台匿名设备被服务端拒了」——重置设备后重试才有意义。
    ///
    /// 实测（2026-10-27）：同一台设备注册成功后，过一段时间会开始收到
    /// 「HTTP 200 + 空 body」；此时换一台新设备（重新注册）立刻恢复正常，
    /// 而同一台设备无论重试多少次都是空的。所以这不是可重试错误，
    /// **必须换设备**。
    pub fn needs_device_reset(&self) -> bool {
        matches!(self, Self::AnonymousLimited | Self::DeviceRejected)
    }

    /// 目录类接口（搜索、榜单、详情……）的换设备条件：只有设备被**明确拒绝**才换。
    ///
    /// 空结果（[`Self::AnonymousLimited`]）不换：它同时代表「被限」和「结果本来
    /// 为空」，上游 README 明确说这限制只出现在播放接口。而自 2026-10-29 起
    /// 每台新设备只有约一次播放模型，在目录接口上误换等于白烧一次播放机会。
    /// 与上游 0.2.0 `needsFreshDevice` 的语义一致：仅播放接口额外认
    /// `AnonymousLimited`（见 `resolve_playback`）。
    fn needs_fresh_device(&self) -> bool {
        matches!(self, Self::DeviceRejected)
    }
}

pub type Result<T> = std::result::Result<T, HongguoError>;

const PLAYBACK_APP_ID: &str = "1967";

/// 一次播放解析最多换几台新匿名设备重试。
///
/// 上游 CLI 的 `playbackFreshDevices` 也是 2：服务端自 2026-10-29 起对每台设备
/// 基本只给一次播放模型，新设备的首次请求也可能落空，所以留两次机会。
pub const PLAYBACK_FRESH_DEVICES: usize = 2;

/// 目录类调用（搜索、榜单、详情……）最多换几台新匿名设备重试。
///
/// 对应上游 0.2.0 的 `Options.DeviceResets` 默认值（`defaultDeviceResets = 2`）。
const CATALOG_FRESH_DEVICES: usize = 2;

/// 两次匿名注册之间的最小间隔。
///
/// 注册接口被连续打时会返回「没有有效身份」的响应（实测连续第 3 台起出现），
/// 一旦拿不到身份，播放、搜索、详情全部退回 worker，而 worker 撞同一面墙。
/// 留 2.5 秒既够服务端安顿，又不会让「换设备重试」等到用户失去耐心。
const REGISTER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2500);

/// 注册闸门：串行化进程内的所有注册，并记录上一次注册的时间。
static REGISTER_GATE: std::sync::OnceLock<tokio::sync::Mutex<Option<std::time::Instant>>> =
    std::sync::OnceLock::new();

/// 排队等到下一次允许注册的时刻。持锁跨越 sleep，所以并发注册会被真正串起来。
async fn wait_register_slot() {
    let gate = REGISTER_GATE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut last = gate.lock().await;
    if let Some(prev) = *last {
        let elapsed = prev.elapsed();
        if elapsed < REGISTER_MIN_INTERVAL {
            tokio::time::sleep(REGISTER_MIN_INTERVAL - elapsed).await;
        }
    }
    *last = Some(std::time::Instant::now());
}

/// 播放解析结果（码流 + 已解包的内容密钥）。
#[derive(Debug, Clone)]
pub struct ResolvedPlayback {
    pub playback: Playback,
    pub content_key: Option<ContentKey>,
}

/// 向红果注册一台匿名设备并填充 `device_id` / `install_id` 与服务端 cookie。
async fn register_device_http(
    http: &reqwest::Client,
    mut profile: DeviceProfile,
) -> Result<DeviceProfile> {
    wait_register_slot().await;

    let body = build_register_body(&profile);
    let stub = content_md5(&body);
    let url = format!(
        "https://{REGISTER_HOST}{PATH_REGISTER}?req_id={}&tt_data=a",
        url_encode(&profile.req_id)
    );
    let ua = user_agent(&profile);

    let send_once = |url: String, ua: String, stub: String, body: Vec<u8>| {
        let http = http.clone();
        async move {
            let resp = http
                .post(&url)
                .header("User-Agent", ua)
                .header("Content-Type", "application/octet-stream;tt-data=a")
                .header("log-encode-type", "gzip")
                .header("sdk-version", "2")
                .header("passport-sdk-version", "5051452")
                .header("x-vc-bdturing-sdk-version", "4.0.3.cn")
                .header("x-ss-dp", APP_ID)
                .header("X-SS-STUB", stub)
                .body(body)
                .send()
                .await
                .map_err(|e| HongguoError::Transport(e.to_string()))?;

            let status = resp.status();
            let cookies: Vec<String> = resp
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
                .collect();
            let raw = resp
                .bytes()
                .await
                .map_err(|e| HongguoError::Transport(e.to_string()))?;

            if !status.is_success() {
                return Err(HongguoError::Other(format!("注册接口 HTTP {status}")));
            }
            if raw.is_empty() {
                return Err(HongguoError::AnonymousLimited);
            }
            if std::env::var_os("HONGGUO_DEBUG_REGISTER").is_some() {
                eprintln!(
                    "[hongguo] 注册响应 {} 字节: {}",
                    raw.len(),
                    String::from_utf8_lossy(&raw[..raw.len().min(600)])
                );
            }
            let parsed =
                parse_register(&raw).map_err(|e| HongguoError::Other(format!("注册失败：{e}")))?;
            Ok::<_, HongguoError>((parsed, cookies))
        }
    };

    let (parsed, cookies) = match send_once(url, ua, stub.clone(), body.clone()).await {
        Ok(value) => value,
        Err(error) => {
            crate::trace::log(format!("[红果] 注册失败，按节流窗口重试一次 原因={error}"));
            // 直接依赖 `wait_register_slot()` 等待满 `REGISTER_MIN_INTERVAL`，
            // 不再额外 `sleep(1.5s)` 后再进闸门导致两次叠加成 4 秒。
            wait_register_slot().await;
            send_once(
                format!(
                    "https://{REGISTER_HOST}{PATH_REGISTER}?req_id={}&tt_data=a",
                    url_encode(&profile.req_id)
                ),
                user_agent(&profile),
                stub,
                body,
            )
            .await?
        }
    };

    profile.device_id_str = parsed.device_id;
    profile.install_id_str = parsed.install_id;
    for cookie in cookies {
        let first = cookie.split(';').next().unwrap_or("");
        if let Some((k, v)) = first.split_once('=') {
            match k.trim() {
                "ttreq" => profile.ttreq = v.trim().to_string(),
                "store-region" => profile.store_region = v.trim().to_string(),
                "store-region-src" => profile.store_region_src = v.trim().to_string(),
                _ => {}
            }
        }
    }

    Ok(profile)
}

async fn save_profile_to_path(path: Option<&PathBuf>, profile: &DeviceProfile) -> Result<()> {
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let raw = serde_json::to_vec_pretty(profile)
            .map_err(|e| HongguoError::Other(format!("序列化设备档案失败：{e}")))?;
        tokio::fs::write(path, raw)
            .await
            .map_err(|e| HongguoError::Other(format!("写入设备档案失败：{e}")))?;
    }
    Ok(())
}

/// 匿名红果客户端。
pub struct HongguoClient {
    pub(crate) http: reqwest::Client,
    profile: Arc<Mutex<DeviceProfile>>,
    profile_path: Option<PathBuf>,
    /// 设备档案的代数：每完成一次重置（手动或协调式）就 +1。
    ///
    /// 并发的失败调用拿它判断「失败期间有没有别的调用已经换过设备」，
    /// 避免同一批并发请求重复注册多台新设备（对应上游 0.2.0 的
    /// `deviceResetter` 代数协调）。
    reset_gen: Mutex<u64>,
    /// 后台预热的下一台可用匿名设备（在用户观看期间静默注册，换设备时 0ms 切换）。
    spare_profile: Arc<Mutex<Option<DeviceProfile>>>,
    /// 当前设备是否已经消费过一次播放模型额度（服务端对每台匿名设备仅放行约 1 次）。
    playback_spent: Arc<AtomicBool>,
    /// 防止后台预热重复拉起多个注册协程。
    spare_warming: Arc<AtomicBool>,
    /// 串行化匿名播放解析，避免并发调用同时消费同一台单次配额设备。
    playback_gate: Arc<tokio::sync::Mutex<()>>,
}

impl HongguoClient {
    /// 创建客户端。给出 `profile_path` 时设备档案会落盘复用，
    /// 否则只在内存里活到进程结束。
    pub async fn new(profile_path: Option<PathBuf>) -> Result<Self> {
        let profile = match &profile_path {
            Some(path) if path.exists() => {
                let raw = tokio::fs::read(path)
                    .await
                    .map_err(|e| HongguoError::Other(format!("读取设备档案失败：{e}")))?;
                serde_json::from_slice::<DeviceProfile>(&raw)
                    .map_err(|e| HongguoError::Other(format!("设备档案损坏：{e}")))?
            }
            _ => DeviceProfile::new_anonymous(),
        };

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| HongguoError::Other(format!("HTTP 客户端初始化失败：{e}")))?;

        Ok(Self {
            http,
            profile: Arc::new(Mutex::new(profile)),
            profile_path,
            reset_gen: Mutex::new(0),
            spare_profile: Arc::new(Mutex::new(None)),
            playback_spent: Arc::new(AtomicBool::new(false)),
            spare_warming: Arc::new(AtomicBool::new(false)),
            playback_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    async fn save_profile(&self, profile: &DeviceProfile) -> Result<()> {
        save_profile_to_path(self.profile_path.as_ref(), profile).await
    }

    /// 异步预注册一台备用匿名设备：在当前播放间隙静默过完 2.5s 注册闸门，
    /// 让下一集 / 换设备重试跳过同步等待。仅在落盘持久化客户端启用，避免单元测试发网请求。
    fn spawn_warm_spare(&self) {
        if self.profile_path.is_none() || self.spare_warming.load(Ordering::Acquire) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.spare_warming.swap(true, Ordering::AcqRel) {
            return;
        }
        let http = self.http.clone();
        let spare = Arc::clone(&self.spare_profile);
        let warming = Arc::clone(&self.spare_warming);
        handle.spawn(async move {
            struct Guard(Arc<AtomicBool>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _guard = Guard(warming);
            if spare.lock().await.is_some() {
                return;
            }
            if let Ok(profile) = register_device_http(&http, DeviceProfile::new_anonymous()).await {
                let id = profile.device_id_str.clone();
                let mut guard = spare.lock().await;
                if guard.is_none() {
                    *guard = Some(profile);
                    crate::trace::log(format!("[红果] 备用匿名设备预热就绪 device_id={id}"));
                }
            }
        });
    }

    /// 如果备用匿名设备已就绪，则立即切换上岗并返回新档案（0ms，零网络等待）。
    async fn take_warm_spare(&self) -> Option<DeviceProfile> {
        self.spare_profile.lock().await.take()
    }

    async fn digest(&self) -> String {
        let p = self.profile.lock().await;
        if p.device_id_str.is_empty() {
            p.cdid.replace('-', "")
        } else {
            p.device_id_str.clone()
        }
    }

    /// 确保已有可用匿名设备（必要时注册）。
    pub async fn ensure_guest(&self) -> Result<DeviceProfile> {
        {
            let p = self.profile.lock().await;
            if p.is_ready() {
                self.spawn_warm_spare();
                return Ok(p.clone());
            }
        }

        if let Some(spare) = self.take_warm_spare().await {
            self.save_profile(&spare).await?;
            *self.profile.lock().await = spare.clone();
            self.playback_spent.store(false, Ordering::Release);
            self.spawn_warm_spare();
            return Ok(spare);
        }

        let current = self.profile.lock().await.clone();
        let registered = register_device_http(&self.http, current).await?;
        self.save_profile(&registered).await?;
        *self.profile.lock().await = registered.clone();
        self.playback_spent.store(false, Ordering::Release);
        self.spawn_warm_spare();
        Ok(registered)
    }

    pub(crate) fn identity(profile: &DeviceProfile) -> SearchIdentity {
        SearchIdentity {
            device_id: profile.device_id_str.clone(),
            install_id: profile.install_id_str.clone(),
            cdid: profile.cdid.clone(),
            openudid: profile.openudid.clone(),
            device: profile.device_or_default(),
        }
    }

    /// 发一次已签名的请求；设备被拒时**自动换一台设备重试一次**。
    ///
    /// 为什么必须自动做：匿名设备会被服务端风控（实测同一台设备先能用、
    /// 一段时间后开始收到空 body）。若不在这一层兜住，用户就会看到
    /// 「刚才还能播，现在全失败」，而且重启应用也没用（档案是落盘的）。
    pub(crate) async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        pairs: Pairs,
        body: Option<Vec<u8>>,
    ) -> Result<Value> {
        self.request_inner(method, path, pairs, body, false).await
    }

    async fn request_playback(&self, pairs: Pairs, body: Vec<u8>) -> Result<Value> {
        self.request_inner_with_hosts(
            reqwest::Method::POST,
            PATH_PLAYBACK_MULTI,
            pairs,
            Some(body),
            false,
            PLAYBACK_API_HOSTS,
            PLAYBACK_APP_ID,
        )
        .await
    }

    async fn request_inner(
        &self,
        method: reqwest::Method,
        path: &str,
        pairs: Pairs,
        body: Option<Vec<u8>>,
        playback: bool,
    ) -> Result<Value> {
        let hosts = if playback {
            PLAYBACK_API_HOSTS
        } else {
            &[API_HOST]
        };
        let app_id = if playback { PLAYBACK_APP_ID } else { APP_ID };
        self.request_inner_with_hosts(method, path, pairs, body, playback, hosts, app_id)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_inner_with_hosts(
        &self,
        method: reqwest::Method,
        path: &str,
        pairs: Pairs,
        body: Option<Vec<u8>>,
        playback: bool,
        hosts: &[&str],
        header_app_id: &str,
    ) -> Result<Value> {
        let profile = self.ensure_guest().await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let entropy = draw_entropy(now_ms);

        let signed = if playback {
            crate::hongguo::playback_sign::sign_playback_post(
                path,
                &pairs,
                now_ms,
                &profile.device_id_str,
                body.as_deref().unwrap_or_default(),
                entropy,
            )
        } else {
            sign_request(
                path,
                &pairs,
                now_ms,
                &profile.device_id_str,
                body.as_deref(),
                entropy,
            )
        };

        let mut last_resp_err: Option<HongguoError> = None;
        let mut resp_ok = None;
        for (idx, host) in hosts.iter().enumerate() {
            let url = format!("https://{host}{path}?{}", signed.query);
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header("User-Agent", user_agent(&profile))
                .header("x-ss-dp", header_app_id)
                .header("lc", "101")
                .header("x-reading-request", reading_request_id(now_ms, rand_i31()));

            for (k, v) in &signed.headers {
                req = req.header(k, v);
            }
            if !profile.ttreq.is_empty() {
                req = req.header("Cookie", profile.guest_cookie());
            }
            if let Some(b) = &body {
                req = req.body(b.clone());
            }

            match req.send().await {
                Ok(r) => {
                    let st = r.status();
                    if st.is_server_error() && idx + 1 < hosts.len() {
                        crate::trace::log(format!(
                            "[红果] 播放域名 {host} HTTP {st}，切换备用线路"
                        ));
                        last_resp_err = Some(HongguoError::Other(format!("HTTP {st}")));
                        continue;
                    }
                    resp_ok = Some(r);
                    break;
                }
                Err(e) => {
                    let err = HongguoError::Transport(e.to_string());
                    if idx + 1 < hosts.len() {
                        crate::trace::log(format!(
                            "[红果] 播放域名 {host} 请求失败，切换备用线路：{e}"
                        ));
                        last_resp_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        let resp = match resp_ok {
            Some(r) => r,
            None => {
                return Err(last_resp_err.unwrap_or_else(|| HongguoError::Other("请求失败".into())))
            }
        };

        let status = resp.status();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| HongguoError::Transport(e.to_string()))?;

        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(HongguoError::AuthenticationRequired);
        }
        if status.as_u16() == 429 {
            return Err(HongguoError::RateLimited);
        }
        if !status.is_success() {
            return Err(HongguoError::Other(format!("HTTP {status}")));
        }

        let trimmed = String::from_utf8_lossy(&raw).trim().to_string();
        if trimmed.is_empty() {
            // 静默拒绝：换设备可能恢复
            return Err(HongguoError::AnonymousLimited);
        }

        let value: Value = serde_json::from_str(&trimmed)
            .map_err(|e| HongguoError::Other(format!("响应不是 JSON：{e}")))?;

        // 业务码
        let code = match value.get("code") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
            _ => 0,
        };
        if code != 0 {
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            return Err(classify(code, message));
        }

        // BaseResp.StatusCode 也可能带设备拒绝码
        if let Some(base) = value.get("BaseResp").and_then(|b| b.get("StatusCode")) {
            let st = base.as_i64().unwrap_or(0);
            if st == 263175 || st == 263176 {
                return Err(HongguoError::DeviceRejected);
            }
            if st != 0 {
                return Err(HongguoError::Code {
                    code: st,
                    message: "BaseResp".into(),
                });
            }
        }

        Ok(value)
    }

    /// 解析一集的播放模型，并解出内容密钥。
    ///
    /// 这是「点播放」的核心：一次往返拿到全部码流地址（不像旧链路要两次），
    /// 密钥在本地解包，不发额外请求。
    ///
    /// ## 空结果要换设备重试（2026-10-29 起的服务端限制）
    ///
    /// 上游 README 记着：从 2026-10-29 起，服务端基本只给每台匿名设备返回**一次**
    /// 播放模型，之后同一台设备再请求只有空 body（`ANONYMOUS_LIMITED`）；
    /// **新设备的第一次请求也不保证成功**。所以遇到这个错误就换新设备重试，
    /// 最多换 [`PLAYBACK_FRESH_DEVICES`] 台。这个限制只出现在播放接口，
    /// 搜索、分集等接口不受影响，别在那些路径上跟着换设备，白白烧设备。
    ///
    /// 必须在这一层重置：查询串里的 iid/device_id 是设备相关的，如果在下层
    /// （request）重置，上层的 pairs 仍是旧设备算出来的，签名永远对不上 ——
    /// 那正是"重置了还是失败"的原因。
    pub async fn resolve_playback(&self, episode_id: &str) -> Result<ResolvedPlayback> {
        let _playback_lock = self.playback_gate.lock().await;
        // 如果当前设备已知消费过播放额度、且后台备用匿名设备已经注册好，
        // 直接 0ms 切换上岗并继续后台补充下一台 —— 省掉一次必然返空的请求与 2.5s 同步等待。
        if self.playback_spent.load(Ordering::Acquire) {
            self.promote_warm_spare_if_spent().await;
        }
        let mut gen = self.reset_generation().await;
        let mut last_error = match self.resolve_playback_once(episode_id).await {
            Ok(resolved) => {
                self.mark_playback_spent_and_warm();
                return Ok(resolved);
            }
            Err(error) if error.needs_device_reset() => error,
            Err(error) => return Err(error),
        };

        for attempt in 0..PLAYBACK_FRESH_DEVICES {
            crate::trace::log(format!(
                "[红果] 播放模型为空，换设备重试 vid={episode_id} 第{}次 原因={last_error}",
                attempt + 1
            ));
            // 协调式换设备：并发的失败调用只注册一台新设备，其余复用。
            match self.fresh_device(gen).await {
                Ok(Some(profile)) => {
                    crate::trace::log(format!(
                        "[红果] 已换新设备 device_id={}",
                        profile.device_id_str
                    ));
                }
                Ok(None) => {
                    crate::trace::log("[红果] 并发调用已换好新设备，直接复用");
                }
                Err(reset_error) => {
                    crate::trace::log(format!("[红果] 重置设备失败：{reset_error}"));
                    return Err(last_error);
                }
            }
            // 快照要在重跑之前刷新：下一次失败要拿它比较，才知道中途有没有别人换过。
            gen = self.reset_generation().await;
            match self.resolve_playback_once(episode_id).await {
                Ok(resolved) => {
                    self.mark_playback_spent_and_warm();
                    return Ok(resolved);
                }
                // 新设备同样可能立刻被限，继续下一台。
                Err(error) if error.needs_device_reset() => last_error = error,
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }

    fn mark_playback_spent_and_warm(&self) {
        self.playback_spent.store(true, Ordering::Release);
        self.spawn_warm_spare();
    }

    async fn promote_warm_spare_if_spent(&self) -> Option<DeviceProfile> {
        let mut gen_guard = self.reset_gen.lock().await;
        if !self.playback_spent.load(Ordering::Acquire) {
            return None;
        }
        let spare = self.take_warm_spare().await?;
        *self.profile.lock().await = spare.clone();
        self.playback_spent.store(false, Ordering::Release);
        *gen_guard = gen_guard.wrapping_add(1);
        drop(gen_guard);
        let _ = self.save_profile(&spare).await;
        crate::trace::log(format!(
            "[红果] 预热备用设备提前上岗 device_id={}",
            spare.device_id_str
        ));
        self.spawn_warm_spare();
        Some(spare)
    }

    async fn resolve_playback_once(&self, episode_id: &str) -> Result<ResolvedPlayback> {
        if !valid_id(episode_id) {
            return Err(HongguoError::Other(format!("非法的分集 ID：{episode_id}")));
        }

        let pairs: Pairs = {
            let profile = self.ensure_guest().await?;
            let mut pairs = device_query_pairs(&Self::identity(&profile));
            // 红果 8662 自 2026-11-16 起对匿名 video_model 静默回空，同设备凭据以 1967 口径仍可正常取流。
            if let Some((_, aid)) = pairs.iter_mut().find(|(k, _)| k == "aid") {
                *aid = PLAYBACK_APP_ID.into();
            }
            pairs
        };

        // 红果 8662 + video_model 自 2026-11-16 起对匿名设备返回空响应，
        // multi_video_model/v1 + aid=1967 返回同结构明文 video_list 与 spade_a。
        let body = format!(
            r#"{{"biz_param":{{"detail_page_version":0,"device_level":3,"disable_digg_stat":false,"need_all_video_definition":true,"need_mp4_align":false,"use_os_player":false,"use_server_dns":false,"video_platform":1024}},"mixed_video_id_map":{{"1":["{episode_id}"]}}}}"#
        )
        .into_bytes();

        let value = self.request_playback(pairs, body).await?;

        // 复用解析器：把它重新包成 {"code":0,"data":{"video_model":...}}
        let wrapped = serde_json::to_vec(&value)
            .map_err(|e| HongguoError::Other(format!("回包序列化失败：{e}")))?;
        let mut playback = match parse_playback(&wrapped, episode_id) {
            Ok(pb) => pb,
            Err(err) => {
                // `video_list` 为空但带回 `fallback_api` 时直接原地请求备播接口。
                if let Some(fb) = extract_fallback_api(&value, episode_id) {
                    if let Some(pb) = self.fetch_fallback_playback(&fb, episode_id).await {
                        pb
                    } else {
                        return Err(HongguoError::Other(format!("播放模型解析失败：{err}")));
                    }
                } else {
                    return Err(HongguoError::Other(format!("播放模型解析失败：{err}")));
                }
            }
        };

        // 主模型仅返回 bytevc2（ffmpeg/mpv 无法解码）时，切到 fallback_api 换取明文 H.264 / H.265。
        if !playback.has_decodable_or_bvc1_stream() && !playback.fallback_api.is_empty() {
            if let Some(fb_pb) = self
                .fetch_fallback_playback(&playback.fallback_api, episode_id)
                .await
            {
                let mut merged = fb_pb;
                if merged.duration_seconds <= 0 {
                    merged.duration_seconds = playback.duration_seconds;
                }
                if merged.expires_at <= 0 {
                    merged.expires_at = playback.expires_at;
                }
                if merged.fallback_api.is_empty() {
                    merged.fallback_api = playback.fallback_api.clone();
                }
                playback = merged;
            }
        }

        let content_key = if playback.protected && !playback.key_material.is_empty() {
            derive_key(&playback.key_material).ok()
        } else {
            None
        };

        Ok(ResolvedPlayback {
            playback,
            content_key,
        })
    }

    async fn fetch_fallback_playback(
        &self,
        fallback_api: &str,
        episode_id: &str,
    ) -> Option<Playback> {
        for candidate in fallback_candidate_urls(fallback_api) {
            let Ok(resp) = self
                .http
                .get(&candidate)
                .header(reqwest::header::USER_AGENT, "com.phoenix.read/71332")
                .send()
                .await
            else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(bytes) = resp.bytes().await else {
                continue;
            };
            if let Ok(pb) = parse_fallback_video_info(&bytes, episode_id) {
                if pb.has_decodable_or_bvc1_stream() {
                    return Some(pb);
                }
            }
        }
        None
    }

    /// 当前设备档案快照（诊断用）。
    pub async fn profile_snapshot(&self) -> DeviceProfile {
        self.profile.lock().await.clone()
    }

    /// 当前 device_id（写入签名头时用）。
    pub async fn device_id(&self) -> String {
        self.digest().await
    }

    /// 丢弃当前设备并重新注册（手动重置）。
    ///
    /// 手动重置同样递增代数：并发中已失败的调用会看到代数变化，
    /// 复用新设备重试，而不是再注册一台。
    pub async fn reset_device(&self) -> Result<DeviceProfile> {
        let profile = self.rebuild_device().await?;
        let mut gen = self.reset_gen.lock().await;
        *gen = gen.wrapping_add(1);
        Ok(profile)
    }

    /// 目录类调用的换设备重试外壳（对应上游 0.2.0 的 `withFreshDevice`）。
    ///
    /// 只在设备被**明确拒绝**时换设备重跑 `once`，最多
    /// [`CATALOG_FRESH_DEVICES`] 次；空结果（`AnonymousLimited`）不换，
    /// 理由见 [`HongguoError::needs_fresh_device`]。并发的失败调用由
    /// [`Self::fresh_device`] 协调，只注册一台新设备。
    ///
    /// `label` 只用于日志。必须在这一层（换完设备后重新构造查询串）重试，
    /// 不能在下层 `request` 里换：查询串里的 iid/device_id 是设备相关的，
    /// 旧 pairs 的签名永远对不上新设备。
    pub(crate) async fn with_fresh_device<T, F, Fut>(&self, label: &str, once: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut gen = self.reset_generation().await;
        let mut last_error = match once().await {
            Ok(value) => return Ok(value),
            Err(error) if error.needs_fresh_device() => error,
            Err(error) => return Err(error),
        };

        for attempt in 0..CATALOG_FRESH_DEVICES {
            match self.fresh_device(gen).await {
                Ok(Some(profile)) => crate::trace::log(format!(
                    "[红果] {label}设备被拒，已换新设备 device_id={}（第{}次）",
                    profile.device_id_str,
                    attempt + 1
                )),
                Ok(None) => crate::trace::log(format!(
                    "[红果] {label}设备被拒，复用并发调用换好的新设备（第{}次）",
                    attempt + 1
                )),
                Err(reset_error) => {
                    crate::trace::log(format!("[红果] {label}换设备失败：{reset_error}"));
                    return Err(last_error);
                }
            }

            // 快照要在重跑之前刷新，理由同上。
            gen = self.reset_generation().await;
            match once().await {
                Ok(value) => return Ok(value),
                Err(error) if error.needs_fresh_device() => last_error = error,
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }

    /// 失败调用发起前的代数快照。
    async fn reset_generation(&self) -> u64 {
        *self.reset_gen.lock().await
    }

    /// 协调式换设备（对应上游 0.2.0 的 `deviceResetter.resetFrom`）。
    ///
    /// `gen` 是本次调用发起前的代数快照：仍然相等说明失败期间没人换过设备，
    /// 由本次完成注册；已经变化说明并发的其他调用换过了，直接复用新设备
    /// （返回 `Ok(None)`），保证同一批并发失败只注册一台。注册期间持锁，
    /// 其他失败调用在锁上排队，醒来后看到代数已变即走复用分支。
    async fn fresh_device(&self, gen: u64) -> Result<Option<DeviceProfile>> {
        let mut current = self.reset_gen.lock().await;
        if *current != gen {
            return Ok(None);
        }
        if let Some(spare) = self.take_warm_spare().await {
            *self.profile.lock().await = spare.clone();
            self.playback_spent.store(false, Ordering::Release);
            *current = current.wrapping_add(1);
            drop(current);
            let _ = self.save_profile(&spare).await;
            self.spawn_warm_spare();
            return Ok(Some(spare));
        }
        let profile = self.rebuild_device().await?;
        self.playback_spent.store(false, Ordering::Release);
        *current = current.wrapping_add(1);
        Ok(Some(profile))
    }

    /// 丢档 + 重新注册的公共实现，不碰代数。
    async fn rebuild_device(&self) -> Result<DeviceProfile> {
        {
            let mut p = self.profile.lock().await;
            *p = DeviceProfile::new_anonymous();
        }
        if let Some(path) = &self.profile_path {
            let _ = tokio::fs::remove_file(path).await;
        }
        self.ensure_guest().await
    }
}

fn classify(code: i64, message: String) -> HongguoError {
    match code {
        111104 => HongguoError::DeviceRejected,
        101000 => HongguoError::ContentUnavailable(format!("服务异常（{code}）")),
        110001 => HongguoError::ContentUnavailable("播放模型未知异常（110001）".into()),
        100103 => HongguoError::Code { code, message },
        263175 | 263176 => HongguoError::DeviceRejected,
        _ => HongguoError::Code { code, message },
    }
}

fn valid_id(id: &str) -> bool {
    let len = id.len();
    (4..=25).contains(&len) && id.chars().all(|c| c.is_ascii_digit())
}

pub(crate) fn user_agent(profile: &DeviceProfile) -> String {
    let d = profile.device_or_default();
    format!(
        "com.phoenix.read/73932 (Linux; U; Android {}; zh_CN; {}; Build/{}; Cronet/TTNetVersion:8d40f833 2026-03-03 QuicVersion:21ac1950 2025-11-18)",
        d.os_version, d.model, d.build
    )
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn rand_i31() -> i32 {
    let id = uuid::Uuid::new_v4();
    let b = id.as_bytes();
    (u32::from_be_bytes([b[0], b[1], b[2], b[3]]) & 0x7fff_ffff) as i32
}

fn extract_fallback_api(value: &Value, episode_id: &str) -> Option<String> {
    let data = value.get("data")?;
    let vm = data
        .get("video_model")
        .or_else(|| data.get(episode_id).and_then(|e| e.get("video_model")))
        .or_else(|| {
            data.as_object()
                .and_then(|m| m.values().find_map(|e| e.get("video_model")))
        })?;
    let obj: Value = match vm {
        Value::String(s) => serde_json::from_str(s).ok()?,
        Value::Object(_) => vm.clone(),
        _ => return None,
    };
    let fb = obj.get("fallback_api")?;
    let s = match fb {
        Value::String(v) => v.trim().to_owned(),
        Value::Object(m) => m
            .get("fallback_api")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned(),
        Value::Array(a) => a
            .first()
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned(),
        _ => String::new(),
    };
    (!s.is_empty()).then_some(s)
}

fn rewrite_query_param(url: &str, key: &str, value: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let prefix = format!("{key}=");
    let mut found = false;
    let params: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            if p.starts_with(&prefix) {
                found = true;
                format!("{prefix}{value}")
            } else {
                p.to_owned()
            }
        })
        .collect();
    if found {
        format!("{base}?{}", params.join("&"))
    } else if params.is_empty() {
        format!("{base}?{prefix}{value}")
    } else {
        format!("{base}?{}&{prefix}{value}", params.join("&"))
    }
}

fn strip_query_params(url: &str, keys: &[&str]) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let name = p.split('=').next().unwrap_or("");
            !keys.contains(&name)
        })
        .collect();
    if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

pub(crate) fn fallback_candidate_urls(raw_url: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |u: String| {
        if !u.is_empty() && !out.contains(&u) {
            out.push(u);
        }
    };
    if !raw_url.contains("auth_query=") {
        let cleaned = strip_query_params(raw_url, &["encode_user_tag", "force_fids"]);
        let plain = rewrite_query_param(
            &rewrite_query_param(&cleaned, "codec_type", "0"),
            "stream_type",
            "plain",
        );
        let h264_enc = rewrite_query_param(&cleaned, "codec_type", "0");
        let hevc = rewrite_query_param(&cleaned, "codec_type", "1");
        push(plain);
        push(h264_enc);
        push(hevc);
    } else {
        // `auth_query=stream_type,force_fids` 时不可删改这两个参数，只将 codec_type 改为 h265(1)。
        push(rewrite_query_param(raw_url, "codec_type", "1"));
    }
    push(raw_url.to_owned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_id_requires_digits() {
        assert!(valid_id("7698309376561400856"));
        assert!(!valid_id("abc123"));
        assert!(!valid_id("12"));
        assert!(!valid_id(""));
    }

    #[test]
    fn classify_maps_known_codes() {
        assert!(matches!(
            classify(111104, String::new()),
            HongguoError::DeviceRejected
        ));
        assert!(matches!(
            classify(101000, String::new()),
            HongguoError::ContentUnavailable(_)
        ));
    }

    #[test]
    fn user_agent_matches_client_shape() {
        let p = DeviceProfile::new_anonymous();
        let ua = user_agent(&p);
        assert!(ua.starts_with("com.phoenix.read/73932"));
        assert!(ua.contains("Android 14"));
        assert!(ua.contains("23127PN0CC"));
    }

    #[tokio::test]
    async fn fresh_device_promotes_warm_spare_without_network() {
        let client = HongguoClient::new(None).await.expect("client");
        let mut spare = DeviceProfile::new_anonymous();
        spare.device_id_str = "999888777666555".into();
        spare.install_id_str = "111222333444555".into();
        *client.spare_profile.lock().await = Some(spare);
        client.playback_spent.store(true, Ordering::Release);

        let gen = client.reset_generation().await;
        let promoted = client
            .fresh_device(gen)
            .await
            .expect("fresh_device")
            .expect("spare promoted");
        assert_eq!(promoted.device_id_str, "999888777666555");
        assert!(!client.playback_spent.load(Ordering::Acquire));
        assert_eq!(client.reset_generation().await, gen + 1);
        assert!(client.fresh_device(gen).await.expect("stale gen").is_none());
    }

    #[tokio::test]
    async fn promote_warm_spare_if_spent_respects_flag() {
        let client = HongguoClient::new(None).await.expect("client");
        let mut spare = DeviceProfile::new_anonymous();
        spare.device_id_str = "123123123123".into();
        spare.install_id_str = "456456456456".into();
        *client.spare_profile.lock().await = Some(spare);

        assert!(client.promote_warm_spare_if_spent().await.is_none());
        client.playback_spent.store(true, Ordering::Release);
        let promoted = client
            .promote_warm_spare_if_spent()
            .await
            .expect("promoted when spent");
        assert_eq!(promoted.device_id_str, "123123123123");
        assert!(!client.playback_spent.load(Ordering::Acquire));
    }

    #[test]
    fn fallback_candidates_prefer_plain_h264_unless_auth_query_locks_params() {
        let unlocked = "https://vas-lf-x.snssdk.com/video/fplay/1/x/v?aid=1967&codec_type=4&encode_user_tag=a&force_fids=b&stream_type=encrypt";
        let cands = fallback_candidate_urls(unlocked);
        assert!(cands[0].contains("codec_type=0"));
        assert!(cands[0].contains("stream_type=plain"));
        assert!(!cands[0].contains("force_fids="));
        assert!(!cands[0].contains("encode_user_tag="));

        let locked = "https://vas-lf-x.snssdk.com/video/fplay/1/x/v?aid=1967&auth_query=c3RyZWFtX3R5cGUsZm9yY2VfZmlkcw&codec_type=4&force_fids=b&stream_type=encrypt";
        let locked_cands = fallback_candidate_urls(locked);
        assert!(locked_cands[0].contains("codec_type=1"));
        assert!(locked_cands[0].contains("force_fids=b"));
        assert!(locked_cands[0].contains("stream_type=encrypt"));
    }
}
