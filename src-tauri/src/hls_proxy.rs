//! 本地 HLS 代理：把远端 m3u8/ts 转成本地流，绕过 WebView2 原生不支持 m3u8 的限制。
//!
//! 原理：
//! - `/stream?u=<base64>&t=<token>` 转发远端内容（m3u8 文本会重写分片地址指向本代理）；
//! - `/seg?u=<base64>&t=<token>` 转发 ts 分片原始字节（带 Range 透传）；
//! - 播放器拿到的永远是 `http://127.0.0.1:{port}/...`。
//!
//! 与参考实现（果果剧库 internal/app/ui_forward.go）对齐的三点：
//! 1. **响应头必须在正文之前落定**。此前 m3u8 分支先把头部以 `\r\n\r\n` 收尾，之后
//!    才 `write_all("Content-Length: ...")`——那两行落进了正文，客户端拿到的 body 是
//!    `Content-Length: N\r\n\r\n#EXTM3U...`。hls.js 对播放列表有 `startsWith('#EXTM3U')`
//!    硬校验，必然判 `no EXTM3U delimiter` 直接把整条链路判死。
//! 2. **必须发 CORS 头并处理 OPTIONS 预检**。hls.js 用 XHR 从 WebView origin 拉
//!    `http://127.0.0.1:{port}`，是跨域请求；参考实现 `setForwardCORS` 会同时给出
//!    `Access-Control-Allow-Origin/Headers/Methods/Max-Age` 与 `X-Content-Type-Options`。
//! 3. **不能做开放转发器**。参考实现用「令牌 + 签名 URL + 端点白名单」收口；这里同样
//!    在启动时生成一次性令牌、随 `proxied_url` 下发并在每次请求校验，同时把目标地址
//!    限制为 http(s)——否则本机任意进程、以及 WebView 内被注入的脚本都能拿它当通用
//!    GET 代理（含指向内网与其它 localhost 服务的 SSRF）。base64 只是编码，不是鉴权。

use base64::Engine;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

static PROXY_PORT: OnceLock<u16> = OnceLock::new();
static PROXY_TOKEN: OnceLock<String> = OnceLock::new();
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

const PREFETCH_TTL: Duration = Duration::from_secs(90);
const PREFETCH_MAX_ENTRIES: usize = 32;
const PREFETCH_MAX_ITEM_BYTES: usize = 8 * 1024 * 1024;
const SEG_NEXT_TTL: Duration = Duration::from_secs(120);
const SEG_NEXT_MAX: usize = 512;

#[derive(Clone)]
struct CachedPayload {
    status: u16,
    content_type: Option<String>,
    cache_control: Option<String>,
    body: Vec<u8>,
}

type InflightRx = watch::Receiver<Option<Option<CachedPayload>>>;
type SegNextEntry = (Instant, Option<String>, Option<String>);

static PAYLOAD_CACHE: OnceLock<Mutex<HashMap<String, (Instant, CachedPayload)>>> = OnceLock::new();
static PAYLOAD_INFLIGHT: OnceLock<Mutex<HashMap<String, InflightRx>>> = OnceLock::new();
static SEG_NEXT: OnceLock<Mutex<HashMap<String, SegNextEntry>>> = OnceLock::new();

fn payload_cache() -> &'static Mutex<HashMap<String, (Instant, CachedPayload)>> {
    PAYLOAD_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn payload_inflight() -> &'static Mutex<HashMap<String, InflightRx>> {
    PAYLOAD_INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn seg_next_map() -> &'static Mutex<HashMap<String, SegNextEntry>> {
    SEG_NEXT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        // 转发的目标正是境外 CDN（含暴风的 `p.bvvvvvvvvv1f.com`、黄果的
        // `*.hzjvlo.cn` / `*.drleao.cn`），必须挂代理：本地代理自己出不去网，
        // 等于把前端的问题原样搬到后端。
        //
        // 用 `with_local_proxy_fallback` 而不是 `with_system_proxy`（2026-11-11）：
        // 系统代理开关（ProxyEnable）为 0 时后者什么都不挂，而用户机器上 Clash
        // 常常只在托盘里跑、并不接管系统代理——那时封面能出（业务链路有兜底）、
        // 播放却起不来。update.rs 的文档本来就把「HLS 转发」列在这条兜底的覆盖
        // 范围里，这里只是与它对齐。客户端是惰性构建的，探测不落在启动路径上。
        crate::update::with_local_proxy_fallback(reqwest::Client::builder())
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126 Safari/537.36")
            .connect_timeout(Duration::from_secs(6))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .tcp_nodelay(true)
            .build()
            .expect("hls proxy client")
    })
}

/// 把远端媒体地址包装成本地代理地址（已带访问令牌）。代理未启动时返回 None。
///
/// 这个端点是**通用 http(s) 转发器**，不只服务 m3u8：非 m3u8 走二进制分支、透传上游
/// `Content-Type`，所以 ts 分片、整段 MP4 与封面图都能从这里走。
pub fn proxied_url(remote: &str) -> Option<String> {
    let port = *PROXY_PORT.get()?;
    let token = PROXY_TOKEN.get()?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(remote.as_bytes());
    Some(format!(
        "http://127.0.0.1:{port}/stream?u={encoded}&t={token}"
    ))
}

/// 构造直通媒体文件的本地代理地址（透传 Range 与可选的红果 CDN 请求头，不做 m3u8 改写）。
pub fn proxied_media_url(remote: &str, hongguo_headers: bool) -> Option<String> {
    let port = *PROXY_PORT.get()?;
    let token = PROXY_TOKEN.get()?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(remote.as_bytes());
    let suffix = if hongguo_headers { "&h=1" } else { "" };
    Some(format!(
        "http://127.0.0.1:{port}/media?u={encoded}&t={token}{suffix}"
    ))
}

/// 把远端封面地址包装成本地代理地址（与 m3u8 共用同一个 `/stream` 端点）。
///
/// **为什么封面也要过代理**：暴风的封面域名解析到境外 IP（实测 `img.bfzypic.com` →
/// `193.148.95.x`，与播放域名 `p.bvvvvvvvvv1f.com` 同一批节点），WebView2 的 `<img>`
/// 直连在部分网络下会超时，而代理挂上系统代理后就能出去——实测同一台机器上「浏览器
/// 打得开、应用封面一张不剩」，差异只在走没走系统代理。
///
/// 空地址与代理未启动都返回 None，调用方保留原地址即可（行为与不改写完全一致）。
pub fn proxied_image_url(remote: &str) -> Option<String> {
    if remote.trim().is_empty() {
        return None;
    }
    proxied_url(remote)
}

/// `proxied_image_url` 的逆操作：把本地代理地址还原成原始封面地址。
///
/// **落库前必须调用**：代理的端口与令牌每次进程启动都是随机的（`bind("127.0.0.1:0")`
/// 与 `Uuid::new_v4()`），一旦把代理地址写进历史 / 收藏，下次启动就是一张死链
/// （403 令牌无效），表现为「历史里的封面一夜之间全空」。所以库里永远存原始地址，
/// 只有交给前端渲染的那一份才改写。
///
/// 不是本地代理地址时原样返回：红果等境内图床的封面从不改写，落库本就该保持原样。
pub fn restore_image_url(proxied: &str) -> String {
    let decoded = proxied
        .strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.split_once("/stream?"))
        .and_then(|(port, query)| {
            (!port.is_empty() && port.chars().all(|c| c.is_ascii_digit())).then_some(query)
        })
        .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("u=")))
        .and_then(decode_url);
    decoded.unwrap_or_else(|| proxied.to_string())
}

/// 启动本地代理（幂等：已启动则直接返回端口）。
pub async fn start() -> Result<u16, String> {
    if let Some(port) = PROXY_PORT.get() {
        return Ok(*port);
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("HLS 代理绑定失败: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    // 一次性令牌：只在进程内存在，随 proxied_url 下发给播放器。
    PROXY_TOKEN
        .set(uuid::Uuid::new_v4().simple().to_string())
        .ok();
    PROXY_PORT.set(port).ok();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(handle(socket));
        }
    });
    Ok(port)
}

fn decode_url(encoded: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let remote = String::from_utf8(bytes).ok()?;
    // 只允许 http(s)：挡掉 file:// 之类指向本机文件系统的目标。
    if remote.starts_with("http://") || remote.starts_with("https://") {
        Some(remote)
    } else {
        None
    }
}

/// 常量时间比较：逐字节异或累加，避免提前返回泄露匹配前缀长度。
fn token_eq(expected: &str, supplied: &str) -> bool {
    let (expected, supplied) = (expected.as_bytes(), supplied.as_bytes());
    if expected.len() != supplied.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in expected.iter().zip(supplied.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn token_matches(supplied: &str) -> bool {
    match PROXY_TOKEN.get() {
        Some(expected) => token_eq(expected, supplied),
        None => false,
    }
}

/// 跨域头：hls.js 从 WebView origin 拉本代理是跨域请求，缺这些头会被浏览器拦下。
fn cors_headers() -> &'static str {
    "Access-Control-Allow-Origin: *\r\n\
     Access-Control-Allow-Headers: *\r\n\
     Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
     Access-Control-Max-Age: 86400\r\n\
     X-Content-Type-Options: nosniff\r\n"
}

async fn write_plain(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
    keep_alive: bool,
) -> bool {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let head = format!(
        "HTTP/1.1 {status}\r\n{}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
        cors_headers(),
        body.len()
    );
    if socket.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    if socket.write_all(body.as_bytes()).await.is_err() {
        return false;
    }
    let _ = socket.flush().await;
    keep_alive
}

fn cache_get(url: &str) -> Option<CachedPayload> {
    let mut guard = payload_cache().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if let Some((at, payload)) = guard.get(url) {
        if now.duration_since(*at) < PREFETCH_TTL {
            return Some(payload.clone());
        }
        guard.remove(url);
    }
    None
}

fn cache_put(url: String, payload: CachedPayload) {
    if payload.body.len() > PREFETCH_MAX_ITEM_BYTES {
        return;
    }
    let mut guard = payload_cache().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    guard.retain(|_, (at, _)| now.duration_since(*at) < PREFETCH_TTL);
    if guard.len() >= PREFETCH_MAX_ENTRIES {
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(url, (now, payload));
}

async fn fetch_cached_or_join(url: &str) -> Option<CachedPayload> {
    if let Some(hit) = cache_get(url) {
        return Some(hit);
    }
    enum Role {
        Follower(InflightRx),
        Leader(watch::Sender<Option<Option<CachedPayload>>>),
    }
    let role = {
        let mut inflight = payload_inflight().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rx) = inflight.get(url) {
            Role::Follower(rx.clone())
        } else {
            let (tx, rx) = watch::channel(None);
            inflight.insert(url.to_string(), rx);
            Role::Leader(tx)
        }
    };
    match role {
        Role::Follower(mut rx) => {
            let _ = rx.wait_for(|v| v.is_some()).await;
            rx.borrow().clone().flatten()
        }
        Role::Leader(tx) => {
            let fetched = fetch_uncached(url).await;
            if let Some(ref payload) = fetched {
                cache_put(url.to_string(), payload.clone());
            }
            let _ = tx.send(Some(fetched.clone()));
            payload_inflight()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(url);
            fetched
        }
    }
}

async fn fetch_uncached(url: &str) -> Option<CachedPayload> {
    let response = client().get(url).send().await.ok()?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return None;
    }
    let content_len = response.content_length();
    if content_len.is_some_and(|len| len > PREFETCH_MAX_ITEM_BYTES as u64) {
        return None;
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let cache_control = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = response.bytes().await.ok()?;
    if bytes.len() > PREFETCH_MAX_ITEM_BYTES {
        return None;
    }
    Some(CachedPayload {
        status,
        content_type,
        cache_control,
        body: bytes.to_vec(),
    })
}

fn spawn_prefetch_url(url: String) {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return;
    }
    if cache_get(&url).is_some() {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = fetch_cached_or_join(&url).await;
        });
    }
}

fn register_playlist_prefetch(body: &str, remote: &str) -> Vec<String> {
    let base = remote
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or(remote);
    let mut keys: Vec<String> = Vec::new();
    let mut segments: Vec<String> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            if let Some(index) = trimmed.find("URI=\"") {
                let rest = &trimmed[index + 5..];
                if let Some(end) = rest.find('"') {
                    let abs = absolutize(base, &rest[..end]);
                    if !keys.contains(&abs) {
                        keys.push(abs);
                    }
                }
            }
        } else {
            segments.push(absolutize(base, trimmed));
        }
    }
    if !segments.is_empty() {
        let mut map = seg_next_map().lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        map.retain(|_, (at, _, _)| now.duration_since(*at) < SEG_NEXT_TTL);
        if map.len() + segments.len() > SEG_NEXT_MAX {
            map.clear();
        }
        for i in 0..segments.len() {
            let next1 = segments.get(i + 1).cloned();
            let next2 = segments.get(i + 2).cloned();
            map.insert(segments[i].clone(), (now, next1, next2));
        }
    }
    let mut targets = keys;
    for seg in segments.into_iter().take(2) {
        targets.push(seg);
    }
    targets
}

fn prefetch_following_segments(seg_url: &str) {
    let (next1, next2) = {
        let mut map = seg_next_map().lock().unwrap_or_else(|e| e.into_inner());
        match map.get(seg_url) {
            Some((at, n1, n2)) if at.elapsed() < SEG_NEXT_TTL => (n1.clone(), n2.clone()),
            Some(_) => {
                map.remove(seg_url);
                (None, None)
            }
            None => (None, None),
        }
    };
    if let Some(url) = next1 {
        spawn_prefetch_url(url);
    }
    if let Some(url) = next2 {
        spawn_prefetch_url(url);
    }
}

/// 异步预热指定远端 m3u8 及首个密钥、前两段分片（不阻塞调用方）。
pub fn warm_hls(remote: &str) {
    if !remote.starts_with("http://") && !remote.starts_with("https://") {
        return;
    }
    let remote = remote.to_string();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let Some(payload) = fetch_cached_or_join(&remote).await else {
            return;
        };
        let body = String::from_utf8_lossy(&payload.body);
        if !body.trim_start().starts_with("#EXTM3U") {
            return;
        }
        for target in register_playlist_prefetch(&body, &remote) {
            spawn_prefetch_url(target);
        }
    });
}

async fn handle(mut socket: tokio::net::TcpStream) {
    let mut first_request = true;
    loop {
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        let timeout_dur = if first_request {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(30)
        };
        first_request = false;
        let header_ok = tokio::time::timeout(timeout_dur, async {
            while let Ok(count) = socket.read(&mut chunk).await {
                if count == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..count]);
                if buf.len() > 64 * 1024 {
                    return false;
                }
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if !header_ok {
            return;
        }
        let keep_alive = handle_one(&mut socket, &buf).await;
        if !keep_alive {
            return;
        }
    }
}

async fn handle_one(socket: &mut tokio::net::TcpStream, buf: &[u8]) -> bool {
    let request = String::from_utf8_lossy(buf).into_owned();
    let mut request_lines = request.lines();
    let first_line = request_lines.next().unwrap_or_default();
    let method = first_line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let path = first_line.split_whitespace().nth(1).unwrap_or_default();
    let client_wants_close = request.lines().any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("connection") && v.trim().eq_ignore_ascii_case("close")
        })
    });
    let keep_alive = !client_wants_close;

    // 预检与探测：OPTIONS 直接回 204，HEAD 只回头不回正文（参考实现同样显式处理）。
    if method == "OPTIONS" {
        let conn = if keep_alive { "keep-alive" } else { "close" };
        let head = format!(
            "HTTP/1.1 204 No Content\r\n{}Content-Length: 0\r\nConnection: {conn}\r\n\r\n",
            cors_headers()
        );
        if socket.write_all(head.as_bytes()).await.is_err() {
            return false;
        }
        let _ = socket.flush().await;
        return keep_alive;
    }

    // 解析 Range 头（透传给上游，实现拖动进度条）
    let range = request
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string());

    let query = match path.split_once('?') {
        Some((_, query)) => query,
        None => "",
    };
    let param = |name: &str| -> &str {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .unwrap_or_default()
    };

    // 令牌校验先于一切：没有它，这个端口就是一个本机开放转发器。
    if !token_matches(param("t")) {
        return write_plain(socket, "403 Forbidden", "HLS 代理令牌无效", keep_alive).await;
    }
    let Some(remote) = decode_url(param("u")) else {
        return write_plain(
            socket,
            "400 Bad Request",
            "HLS 代理目标地址无效",
            keep_alive,
        )
        .await;
    };

    let is_media = path.starts_with("/media");
    let is_seg = path.starts_with("/seg");
    let has_custom_headers = param("h") == "1";
    let remote_path = remote.split('?').next().unwrap_or(&remote);
    let looks_m3u8 = !is_media && remote_path.ends_with(".m3u8");

    if is_seg {
        prefetch_following_segments(&remote);
    }

    // 无 Range、无红果私有头且非 /media：走内存预取缓存/并发去重，命中时零网络往返。
    if !is_media && !has_custom_headers && range.is_none() && method == "GET" {
        if let Some(cached) = cache_get(&remote) {
            if looks_m3u8
                || cached
                    .content_type
                    .as_deref()
                    .is_some_and(|ct| ct.contains("mpegurl"))
            {
                let body = String::from_utf8_lossy(&cached.body).to_string();
                for target in register_playlist_prefetch(&body, &remote) {
                    spawn_prefetch_url(target);
                }
                let rewritten = rewrite_playlist(&body, &remote);
                let conn = if keep_alive { "keep-alive" } else { "close" };
                let head = format!(
                    "HTTP/1.1 200 OK\r\n{}Content-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
                    cors_headers(),
                    rewritten.len()
                );
                if socket.write_all(head.as_bytes()).await.is_err()
                    || socket.write_all(rewritten.as_bytes()).await.is_err()
                {
                    return false;
                }
                let _ = socket.flush().await;
                return keep_alive;
            }
            let conn = if keep_alive { "keep-alive" } else { "close" };
            let cache_control = cached
                .cache_control
                .as_deref()
                .unwrap_or("private, no-store");
            let mut head = format!(
                "HTTP/1.1 {}\r\n{}Cache-Control: {}\r\nContent-Length: {}\r\n",
                cached.status,
                cors_headers(),
                cache_control,
                cached.body.len()
            );
            if let Some(ct) = cached.content_type.as_deref() {
                head.push_str(&format!("Content-Type: {ct}\r\n"));
            }
            head.push_str(&format!("Connection: {conn}\r\n\r\n"));
            if socket.write_all(head.as_bytes()).await.is_err()
                || socket.write_all(&cached.body).await.is_err()
            {
                return false;
            }
            let _ = socket.flush().await;
            return keep_alive;
        }
    }

    let mut request_builder = client().get(&remote);
    if has_custom_headers {
        request_builder = request_builder
            .header("User-Agent", "com.phoenix.read/71332")
            .header("Referer", "https://novel.snssdk.com/");
    }
    if is_media {
        request_builder = request_builder.timeout(Duration::from_secs(120));
    }
    if let Some(range) = &range {
        request_builder = request_builder.header("Range", range);
    }
    let response = match request_builder.send().await {
        Ok(response) => response,
        Err(error) => {
            return write_plain(
                socket,
                "502 Bad Gateway",
                &format!("上游请求失败：{error}"),
                keep_alive,
            )
            .await;
        }
    };

    let is_m3u8 = !is_media
        && (looks_m3u8
            || response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|content_type| content_type.contains("mpegurl")));

    if is_m3u8 {
        // m3u8 文本：重写相对/绝对分片地址指向本代理，并立即异步预热密钥与前两片分片。
        let raw_bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                return write_plain(
                    socket,
                    "502 Bad Gateway",
                    &format!("上游播放列表读取失败：{error}"),
                    keep_alive,
                )
                .await;
            }
        };
        if !has_custom_headers && range.is_none() {
            cache_put(
                remote.clone(),
                CachedPayload {
                    status: 200,
                    content_type: Some("application/vnd.apple.mpegurl".to_string()),
                    cache_control: None,
                    body: raw_bytes.to_vec(),
                },
            );
        }
        let body = String::from_utf8_lossy(&raw_bytes).to_string();
        for target in register_playlist_prefetch(&body, &remote) {
            spawn_prefetch_url(target);
        }
        let rewritten = rewrite_playlist(&body, &remote);
        let conn = if keep_alive { "keep-alive" } else { "close" };
        let head = format!(
            "HTTP/1.1 200 OK\r\n{}Content-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
            cors_headers(),
            rewritten.len()
        );
        if socket.write_all(head.as_bytes()).await.is_err() {
            return false;
        }
        if method != "HEAD" && socket.write_all(rewritten.as_bytes()).await.is_err() {
            return false;
        }
        let _ = socket.flush().await;
        return keep_alive;
    }

    // ts 分片等二进制：沿用上游的状态码与长度类头（含 Range/206 语义）。
    // 缓存策略**优先沿用上游**：封面这类小图必须让浏览器缓存——列表卡片的封面会反复
    // 进出视口，一律 no-store 会让每次滚动都整列重新拉取。ts 分片上游一般不给
    // cache-control，回退 no-store，行为与之前完全一致。
    let cache_control = response
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("private, no-store");
    let has_content_length = response.headers().contains_key("content-length");
    let can_keep_alive = keep_alive && (has_content_length || method == "HEAD");
    let conn = if can_keep_alive {
        "keep-alive"
    } else {
        "close"
    };
    let mut head = format!(
        "HTTP/1.1 {}\r\n{}Cache-Control: {}\r\n",
        response.status(),
        cors_headers(),
        cache_control
    );
    for key in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
    ] {
        if let Some(value) = response.headers().get(key).and_then(|v| v.to_str().ok()) {
            head.push_str(&format!("{}: {}\r\n", title_case(key), value));
        }
    }
    head.push_str(&format!("Connection: {conn}\r\n\r\n"));
    if socket.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    if method != "HEAD" {
        // **流式**转发，绝不 `response.bytes()` 全量读进内存。
        let mut stream = response;
        loop {
            match stream.chunk().await {
                Ok(Some(chunk)) => {
                    if socket.write_all(&chunk).await.is_err() {
                        // 播放器已主动断开（换集/关闭）：停读上游即可。
                        return false;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    // 上游中途断开：连接已声明长度，直接关闭让播放器重试分片。
                    return false;
                }
            }
        }
    }
    let _ = socket.flush().await;
    can_keep_alive
}

/// 把 m3u8 里所有分片地址（含 #EXT-X-KEY 的 URI）改写成本代理地址。
fn rewrite_playlist(body: &str, remote: &str) -> String {
    rewrite_playlist_with(
        body,
        remote,
        PROXY_PORT.get().copied().unwrap_or(0),
        PROXY_TOKEN.get().map(String::as_str).unwrap_or_default(),
    )
}

/// 纯函数版本（不读全局状态），便于单测。
fn rewrite_playlist_with(body: &str, remote: &str, port: u16, token: &str) -> String {
    let base = remote
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or(remote);
    let encode = |absolute: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(absolute.as_bytes())
    };
    let mut rewritten = String::with_capacity(body.len() + 512);
    for line in body.lines() {
        let trimmed = line.trim();
        let replaced = if trimmed.is_empty() || trimmed.starts_with('#') {
            // #EXT-X-KEY / #EXT-X-MAP 里的 URI 也要重写，否则密钥请求绕过代理而失败。
            match trimmed.find("URI=\"") {
                Some(index) => {
                    let head = &trimmed[..index + 5];
                    let rest = &trimmed[index + 5..];
                    match rest.find('"') {
                        Some(end) => {
                            let absolute = absolutize(base, &rest[..end]);
                            format!(
                                "{head}http://127.0.0.1:{port}/seg?u={}&t={token}\"{}",
                                encode(&absolute),
                                &rest[end + 1..]
                            )
                        }
                        None => line.to_string(),
                    }
                }
                None => line.to_string(),
            }
        } else {
            let absolute = absolutize(base, trimmed);
            format!(
                "http://127.0.0.1:{port}/seg?u={}&t={token}",
                encode(&absolute)
            )
        };
        rewritten.push_str(&replaced);
        rewritten.push('\n');
    }
    rewritten
}

fn title_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (index, part) in key.split('-').enumerate() {
        if index > 0 {
            out.push('-');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

fn absolutize(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    // 协议相对地址（//host/path）：补上 base 的 scheme，否则会被拼成 base 下的路径。
    if let Some(rest) = url.strip_prefix("//") {
        if let Some(scheme_end) = base.find("://") {
            return format!("{}://{rest}", &base[..scheme_end]);
        }
        return format!("https://{rest}");
    }
    if url.starts_with('/') {
        if let Some(scheme_end) = base.find("://") {
            if let Some(host_end) = base[scheme_end + 3..].find('/') {
                return format!("{}{}", &base[..scheme_end + 3 + host_end], url);
            }
            return format!("{base}{url}");
        }
    }
    format!("{base}/{url}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 还原必须是改写的**逆操作**：代理端口与令牌每次进程启动都不同（`bind("127.0.0.1:0")`
    /// 与 `Uuid::new_v4()`），把代理地址落进历史 / 收藏就等于存了一张下次启动必死的链
    /// （403 令牌无效，表现为「历史里的封面一夜之间全空」）。
    #[test]
    fn restore_image_url_round_trips_through_proxy() {
        tokio::runtime::Builder::new_current_thread()
            // 必须显式开 IO driver：current_thread 默认不带，`TcpListener::bind` 会直接
            // panic（实测报在 tokio 的 listener.rs，提示 there is no reactor running）。
            .enable_io()
            .build()
            .expect("runtime")
            .block_on(start())
            .expect("本地代理应能启动");
        let raw = "https://img.bfzypic.com/a.jpg";
        let proxied = proxied_image_url(raw).expect("代理已启动应能改写");
        assert_eq!(restore_image_url(&proxied), raw);
    }

    /// 还原对**非代理地址**必须恒等：红果等境内图床的封面从不改写，落库本就该保持原样。
    /// 这条不依赖代理是否已启动（一旦启动就不再关闭），所以在任何执行顺序下都成立。
    #[test]
    fn restore_image_url_leaves_plain_urls_alone() {
        let raw = "https://img.bfzypic.com/a.jpg?x=1";
        assert_eq!(restore_image_url(raw), raw);
    }

    #[test]
    fn absolutize_handles_protocol_relative_and_root_paths() {
        // 契约：base 是"目录"（调用方已用 rsplit_once('/') 去掉文件名）。
        let base = "https://cdn.example/a/b";
        assert_eq!(
            absolutize(base, "//other.example/x.ts"),
            "https://other.example/x.ts"
        );
        assert_eq!(
            absolutize(base, "/root/x.ts"),
            "https://cdn.example/root/x.ts"
        );
        assert_eq!(absolutize(base, "x.ts"), "https://cdn.example/a/b/x.ts");
        assert_eq!(
            absolutize(base, "https://z.example/y.ts"),
            "https://z.example/y.ts"
        );
    }

    #[test]
    fn decode_url_rejects_non_http_schemes() {
        let encode =
            |value: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes());
        assert_eq!(
            decode_url(&encode("https://cdn.example/a.ts")),
            Some("https://cdn.example/a.ts".to_string())
        );
        assert_eq!(decode_url(&encode("file:///C:/Windows/win.ini")), None);
        assert_eq!(decode_url(&encode("ftp://example.com/a.ts")), None);
        assert_eq!(decode_url("not-base64!!"), None);
    }

    #[test]
    fn rewrite_playlist_points_segments_and_keys_at_local_proxy() {
        let body = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:1.0,\na.ts\n#EXT-X-ENDLIST\n";
        let rewritten =
            rewrite_playlist_with(body, "https://cdn.example/dir/index.m3u8", 45678, "tok");
        assert!(rewritten.starts_with("#EXTM3U\n"));
        assert!(rewritten.contains("http://127.0.0.1:45678/seg?u="));
        // 分片地址与密钥 URI 都必须带令牌，否则播放器拉到分片会被 403。
        assert!(rewritten.contains("&t=tok\""));
        assert!(rewritten.contains("URI=\"http://127.0.0.1:45678/seg?u="));
        // 关键行本身不能被改成地址
        assert!(rewritten.contains("#EXTINF:1.0,"));
        assert!(rewritten.contains("#EXT-X-ENDLIST"));
        // 首行必须仍是 #EXTM3U，供 hls.js 做 startsWith('#EXTM3U') 校验
        assert_eq!(rewritten.lines().next(), Some("#EXTM3U"));
    }

    #[test]
    fn token_eq_requires_exact_token() {
        assert!(token_eq("secret-token", "secret-token"));
        assert!(!token_eq("secret-token", "secret-toke"));
        assert!(!token_eq("secret-token", "secret-tokenx"));
        assert!(!token_eq("secret-token", ""));
        assert!(!token_eq("secret-token", "other"));
    }

    #[test]
    fn proxied_media_url_encodes_headers_flag_and_keeps_stream_untouched() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试运行时应能构建")
            .block_on(start())
            .expect("本地代理应能启动");
        let raw = "https://cdn.example/video/ep1.mp4";
        let plain = proxied_media_url(raw, false).expect("媒体代理应能生成");
        let with_headers = proxied_media_url(raw, true).expect("媒体代理应能生成");
        let stream = proxied_url(raw).expect("HLS 代理应能生成");
        assert!(plain.contains("/media?u="));
        assert!(!plain.contains("&h=1"));
        assert!(with_headers.contains("/media?u="));
        assert!(with_headers.ends_with("&h=1"));
        assert!(stream.contains("/stream?u="));
        assert!(!stream.contains("/media"));
    }

    #[test]
    fn register_playlist_prefetch_extracts_key_initial_segments_and_next_links() {
        let body = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:2.0,\n0.ts\n#EXTINF:2.0,\n1.ts\n#EXTINF:2.0,\n2.ts\n#EXT-X-ENDLIST\n";
        let targets = register_playlist_prefetch(body, "https://cdn.example/vod/index.m3u8");
        assert_eq!(
            targets,
            vec![
                "https://cdn.example/vod/key.bin",
                "https://cdn.example/vod/0.ts",
                "https://cdn.example/vod/1.ts",
            ]
        );
        let map = seg_next_map().lock().unwrap();
        let entry = map.get("https://cdn.example/vod/0.ts").unwrap();
        assert_eq!(entry.1.as_deref(), Some("https://cdn.example/vod/1.ts"));
        assert_eq!(entry.2.as_deref(), Some("https://cdn.example/vod/2.ts"));
    }
}
