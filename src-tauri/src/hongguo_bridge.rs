//! 红果原生客户端与既有命令层的桥接。
//!
//! 这个模块把 `hongguo::client` 的能力接到原来的 worker 调用点上：播放直链、
//! 预签名、目录读取都优先走 Rust 原生实现，worker 只作为失败回退保留。
//!
//! ## 为什么从这里切
//!
//! `ensure_stream_cached` 是「取播放直链」的**唯一入口**（`open_stream` 与
//! `resolve` 都经过它）。在这一处换成原生实现，两条播放链路同时受益，
//! 而且上层命令、前端、缓存结构全都不用改。

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::hongguo::client::HongguoClient;
use serde_json::json;

/// 进程内单例。匿名设备档案要跨请求复用，不能每次新建客户端。
static CLIENT: OnceLock<tokio::sync::Mutex<Option<std::sync::Arc<HongguoClient>>>> =
    OnceLock::new();

fn slot() -> &'static tokio::sync::Mutex<Option<std::sync::Arc<HongguoClient>>> {
    CLIENT.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// 原生客户端的设备档案路径。
///
/// `pub(crate)` 是因为 worker 也要读它：`short_drama_app::load_native_credentials`
/// 复用同一台设备，避免出现「原生一台、worker 另一台」的分裂（见那里的说明）。
pub(crate) fn profile_path() -> PathBuf {
    crate::app_storage_root()
        .unwrap_or_else(std::env::temp_dir)
        .join("hongguo-device.json")
}

/// 取（必要时创建）原生客户端。
pub async fn client() -> Result<std::sync::Arc<HongguoClient>, String> {
    let mut guard = slot().lock().await;
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.clone());
    }
    let client = HongguoClient::new(Some(profile_path()))
        .await
        .map_err(|e| e.to_string())?;
    let arc = std::sync::Arc::new(client);
    *guard = Some(arc.clone());
    Ok(arc)
}

/// 原生取「播放直链 + 内容密钥」，产出与 worker `stream` 子命令**完全相同**的
/// payload 形状，因此 `store_stream` 与下游全部不用改。
pub async fn fetch_stream_payload(vid: &str) -> Result<serde_json::Value, String> {
    let client = client().await?;
    let resolved = client
        .resolve_playback(vid)
        .await
        .map_err(|e| e.to_string())?;

    // 必须用 `best_playable_stream` 而不是 `best_stream`：后者只按
    // (高度, 码率) 取最大，会选中同分辨率下码率最高的 **bytevc1** 那一路，
    // 而 Windows 没有 BVC 解码器 —— 整集 copy 出来只能软解，用户看到的就是
    // "关掉增强后画面不流畅"。详见 media.rs 里这个方法的注释。
    let best = resolved
        .playback
        .best_playable_stream()
        .ok_or_else(|| "原生解析没有可用码流".to_string())?;

    // worker 侧 duration_ms 是毫秒，原生模型给的是秒
    let duration_ms = (resolved.playback.duration_seconds.max(0) as i64) * 1000;

    let variants: Vec<serde_json::Value> = resolved
        .playback
        .streams
        .iter()
        .map(|s| {
            json!({
                // `short_drama_app_qualities` 读的是 id/label 这两个键，
                // 缺了它们菜单会全是「原始画质」且无法区分档位。
                "id": s.definition,
                "label": if s.quality.is_empty() { s.definition.clone() } else { s.quality.clone() },
                "definition": s.definition,
                "quality": s.quality,
                "codec": s.codec,
                "width": s.width,
                "height": s.height,
                "bitrate": s.bitrate,
                "size": s.size,
                "url": s.url,
                "backup_url": s.backup_url,
            })
        })
        .collect();

    Ok(json!({
        "ok": true,
        "url": best.url,
        "content_key": resolved
            .content_key
            .as_ref()
            .map(|k| k.hex.clone())
            .unwrap_or_default(),
        "width": best.width,
        "height": best.height,
        "duration_ms": duration_ms,
        "codec": best.codec,
        "bitrate_kbps": best.bitrate / 1000,
        // 直链有效期（unix 秒，来自 `video_model.url_expire`）。
        //
        // 为什么必须带出去：短剧侧以前按固定 10 分钟缓存直链，而这个值实测
        // **约 6 小时**（抓包样本 1794235182 比请求时刻晚 21,679 秒）。固定
        // TTL 只用了真实窗口的 3%，用户隔十几分钟再看同一集就要重签一次
        // （原生链路 150~250ms）。交给缓存层按真实值判定即可。
        "expires_at": resolved.playback.expires_at,
        "variants": variants,
        // worker 里这两个是链路常量；原生链路同样按 App 的 UA/Referer 拉流
        // 与 worker 原来的链路常量一致：CDN 认这个 UA/Referer
        "download_ua": "com.phoenix.read/71332",
        "download_referer": "https://novel.snssdk.com/",
    }))
}

/// 弹幕首窗最多等待 3 秒；真实请求继续完成，后续同键调用可共享结果。
const DANMAKU_FIRST_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const DANMAKU_CACHE_CAPACITY: usize = 128;

static DANMAKU_CACHE: OnceLock<crate::hongguo::danmaku::TimedCache<String, serde_json::Value>> =
    OnceLock::new();

/// 取一页弹幕。成功缓存 5 分钟，失败缓存 2 秒，同键 pending 只发一次真实请求。
pub async fn fetch_danmaku(
    video_id: &str,
    series_id: &str,
    start_ms: i64,
    duration_ms: i64,
) -> Result<serde_json::Value, String> {
    let cache = DANMAKU_CACHE.get_or_init(|| {
        crate::hongguo::danmaku::TimedCache::new(
            DANMAKU_CACHE_CAPACITY,
            std::time::Duration::from_secs(5 * 60),
            std::time::Duration::from_secs(2),
        )
    });
    let key = format!("{series_id}\0{video_id}\0{start_ms}\0{duration_ms}");
    let video_id = video_id.to_string();
    let series_id = series_id.to_string();
    fetch_danmaku_with_timeout(
        cache,
        key,
        DANMAKU_FIRST_WINDOW_TIMEOUT,
        move || async move {
            let client = client().await?;
            crate::hongguo::danmaku::fetch_danmaku(
                &client,
                &series_id,
                &video_id,
                start_ms,
                duration_ms,
            )
            .await
        },
    )
    .await
}

async fn fetch_danmaku_with_timeout<F, Fut>(
    cache: &crate::hongguo::danmaku::TimedCache<String, serde_json::Value>,
    key: String,
    timeout: std::time::Duration,
    load: F,
) -> Result<serde_json::Value, String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'static,
{
    let pending = cache.get_or_load(key, load);
    match tokio::time::timeout(timeout, pending).await {
        Ok(result) => result,
        Err(_) => Err("弹幕首窗请求超时，真实请求仍在后台完成".into()),
    }
}

/// 弹幕请求入参（前端 `ipcService.playback.danmaku`）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DanmakuInput {
    /// 当前分集 vid（红果纯数字）。
    pub vid: String,
    /// 所属剧集 id（红果纯数字）。协议里的 `book_id`。
    pub series_id: String,
    /// 窗口起点（毫秒）。
    pub start_ms: i64,
    /// 窗口终点（毫秒，绝对时间）。协议里的 `playlet_item_duration` 收的是终点。
    pub duration_ms: i64,
}

#[tauri::command]
pub async fn short_drama_app_danmaku(input: DanmakuInput) -> Result<serde_json::Value, String> {
    fetch_danmaku(
        &input.vid,
        &input.series_id,
        input.start_ms,
        input.duration_ms,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn timeout_keeps_real_request_and_same_key_pending() {
        let cache = crate::hongguo::danmaku::TimedCache::new(
            8,
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(2),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let load = {
            let calls = calls.clone();
            let release = release.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    release.notified().await;
                    Ok(json!({"items": [{"text": "late"}]}))
                }
            }
        };
        let error = fetch_danmaku_with_timeout(
            &cache,
            "same".into(),
            std::time::Duration::from_millis(20),
            load,
        )
        .await
        .unwrap_err();
        assert!(error.contains("超时"));

        let loaded = tokio::spawn(async move {
            cache
                .get_or_load("same".into(), || async {
                    Ok(json!({"items": [{"text": "duplicate"}]}))
                })
                .await
        });
        release.notify_waiters();
        let page = loaded.await.unwrap().unwrap();
        assert_eq!(page["items"][0]["text"], "late");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
