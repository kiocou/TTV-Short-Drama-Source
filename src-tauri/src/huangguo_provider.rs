//! 黄果短剧专区 provider（**huangguo ≠ hongguo**，后者是红果）。
//!
//! 形态是「隐藏频道」：频道值 `huangguo`、id 前缀 `hg:`，由 `main.rs` 的四条
//! `if` 分支路由。不进内置源注册表、不进 `enabled_sources`、不进全局搜索合并。
//! 访问控制是前端那个七击手势本身，不挂 18+ 闸门（四个频道全部接入）。
//!
//! 播放复用宿主既有链路：`streams.main` 是标准 AES-128 HLS，交给 `hls_proxy`
//! 转本地流，播放器组件一行都不用改。

use crate::huangguo::{client, mapping, media};
use crate::models::{
    CatalogFilter, CatalogPage, PlaybackSession, SeriesDetail, VideoQualityOption,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const CATALOG_CACHE_TTL: Duration = Duration::from_secs(90);
const CATALOG_CACHE_MAX: usize = 24;
const DETAIL_CACHE_TTL: Duration = Duration::from_secs(300);
const DETAIL_CACHE_MAX: usize = 64;
const PLAYBACK_CACHE_TTL: Duration = Duration::from_secs(90);
const PLAYBACK_CACHE_MAX: usize = 32;

pub struct HuangguoProvider {
    client: reqwest::Client,
    /// 兜底客户端：**只在主链路真的失败之后**才构建。
    ///
    /// 构建它会真探本机代理端口（阻塞、最坏一两秒），所以绝不能放进 `new()`——
    /// 那等于把「启动那一刻代理在不在」烤进整个进程：探不到时这里不写值，
    /// 下次失败会再探（探测侧有 30 秒冷却），代理恢复后自愈。
    fallback: tokio::sync::OnceCell<reqwest::Client>,
    /// 记忆本会话内兜底代理已打通：避免后续每次 catalog / cover_batch / playback 都先耗尽主链路超时。
    prefer_fallback: AtomicBool,
    catalog_cache: Mutex<HashMap<String, (Instant, CatalogPage)>>,
    detail_cache: Mutex<HashMap<String, (Instant, SeriesDetail)>>,
    playback_cache: Mutex<HashMap<String, (Instant, String)>>,
}

/// 站点单页上限。前端一屏 24 张，给到 50 足够，再大只是拖慢首屏。
const MAX_PAGE_SIZE: u32 = 50;

/// 封面并发上限。与前端 batch 12 对齐，一屏 12~24 张 1~2 轮完成。
const COVER_CONCURRENCY: usize = 12;

fn cache_insert_bounded<V>(
    map: &mut HashMap<String, (Instant, V)>,
    key: String,
    value: V,
    ttl: Duration,
    max: usize,
) {
    let now = Instant::now();
    map.retain(|_, (at, _)| now.duration_since(*at) < ttl);
    if map.len() >= max {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(key, (now, value));
}

impl HuangguoProvider {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            client: client::build_client()?,
            fallback: tokio::sync::OnceCell::new(),
            prefer_fallback: AtomicBool::new(false),
            catalog_cache: Mutex::new(HashMap::new()),
            detail_cache: Mutex::new(HashMap::new()),
            playback_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 取兜底客户端；此刻探不到代理就返回 `None`（不缓存失败）。
    async fn fallback_client(&self) -> Option<reqwest::Client> {
        if let Some(existing) = self.fallback.get() {
            return Some(existing.clone());
        }
        let built = client::fallback_client()?;
        // set 失败只说明别的请求抢先建好了，用谁的都是同一个代理。
        let _ = self.fallback.set(built.clone());
        Some(built)
    }

    /// 业务请求：记忆兜底状态，避开重复等待主链路超时；若兜底失败则回退主链路自愈。
    async fn edge(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        if self.prefer_fallback.load(Ordering::Relaxed) {
            if let Some(fallback) = self.fallback_client().await {
                let via_fallback = client::edge(&fallback, method, path, body.clone()).await;
                if via_fallback.is_ok() {
                    return via_fallback;
                }
                let retry_primary = client::edge(&self.client, method, path, body).await;
                if retry_primary.is_ok() {
                    self.prefer_fallback.store(false, Ordering::Relaxed);
                    return retry_primary;
                }
                return via_fallback;
            }
            self.prefer_fallback.store(false, Ordering::Relaxed);
        }
        let primary = client::edge(&self.client, method, path, body.clone()).await;
        if primary.is_ok() {
            return primary;
        }
        let Some(fallback) = self.fallback_client().await else {
            return primary;
        };
        let via_fallback = client::edge(&fallback, method, path, body).await;
        if via_fallback.is_ok() {
            self.prefer_fallback.store(true, Ordering::Relaxed);
        }
        via_fallback
    }

    pub async fn catalog(&self, filter: &CatalogFilter) -> Result<CatalogPage, String> {
        let page = filter.page.max(1);
        let size = filter.page_size.clamp(1, MAX_PAGE_SIZE);
        let keyword = filter
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|word| !word.is_empty());
        let cache_key = format!(
            "{}|{}|{page}|{size}",
            filter.category.trim(),
            keyword.unwrap_or("")
        );
        {
            let guard = self.catalog_cache.lock().await;
            if let Some((at, cached)) = guard.get(&cache_key) {
                if at.elapsed() < CATALOG_CACHE_TTL {
                    return Ok(cached.clone());
                }
            }
        }
        let path = if let Some(word) = keyword {
            // 必须显式带 type=video：省略时站点默认只搜短剧，但吃瓜帖也会混进来。
            format!(
                "/api/search?q={}&page={page}&size={size}&type=video",
                crate::source_registry::percent_encode(word)
            )
        } else if let Some(slug) = mapping::cate_slug(&filter.category) {
            format!("/api/videos/category/{slug}?page={page}&size={size}")
        } else {
            format!("/api/videos?page={page}&size={size}")
        };
        let data = self.edge("GET", &path, None).await?;
        let parsed = mapping::catalog_page(&data, page, mapping::category_names());
        if !parsed.items.is_empty() {
            let mut guard = self.catalog_cache.lock().await;
            cache_insert_bounded(
                &mut guard,
                cache_key,
                parsed.clone(),
                CATALOG_CACHE_TTL,
                CATALOG_CACHE_MAX,
            );
        }
        Ok(parsed)
    }

    pub async fn detail(&self, series_id: &str) -> Result<SeriesDetail, String> {
        let raw = mapping::raw_id(series_id).ok_or("黄果剧集 id 非法。")?;
        {
            let guard = self.detail_cache.lock().await;
            if let Some((at, cached)) = guard.get(raw) {
                if at.elapsed() < DETAIL_CACHE_TTL {
                    return Ok(cached.clone());
                }
            }
        }
        let data = self
            .edge("GET", &format!("/api/videos/{raw}"), None)
            .await?;
        if data.get("title").and_then(Value::as_str).is_none() {
            return Err("黄果详情页未找到剧集信息。".into());
        }
        let parsed = mapping::detail(&data, series_id);
        let mut guard = self.detail_cache.lock().await;
        cache_insert_bounded(
            &mut guard,
            raw.to_string(),
            parsed.clone(),
            DETAIL_CACHE_TTL,
            DETAIL_CACHE_MAX,
        );
        Ok(parsed)
    }

    /// 这条链路只有一路码流（`streams.main`）。`hevc` 是同内容的另一种编码，
    /// 不是清晰度档位，而且 WebView2 未必解得了——不做成可选档位。
    pub async fn qualities(
        &self,
        _series_id: &str,
        _episode_id: &str,
    ) -> Result<Vec<VideoQualityOption>, String> {
        Ok(Vec::new())
    }

    pub async fn open_episode(
        &self,
        session_id: u64,
        series_id: &str,
        episode_id: &str,
        quality: &str,
        position: f64,
    ) -> Result<PlaybackSession, String> {
        let raw = mapping::raw_id(series_id).ok_or("黄果剧集 id 非法。")?;
        let episode: u32 = episode_id
            .trim()
            .parse()
            .map_err(|_| "黄果选集 id 非法。".to_string())?;
        if episode == 0 {
            return Err("黄果选集 id 非法。".into());
        }
        let playback_key = format!("{raw}:{episode}");
        let cached_remote = {
            let guard = self.playback_cache.lock().await;
            guard.get(&playback_key).and_then(|(at, remote)| {
                (at.elapsed() < PLAYBACK_CACHE_TTL).then(|| remote.clone())
            })
        };
        let remote = match cached_remote {
            Some(hit) => hit,
            None => {
                let data = self
                    .edge(
                        "POST",
                        &format!("/api/videos/{raw}/episodes/{episode}/playback"),
                        Some(json!({})),
                    )
                    .await?;
                let fetched = data
                    .get("streams")
                    .and_then(|streams| streams.get("main"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .ok_or("黄果这一集没有可用播放地址。")?
                    .to_string();
                let mut guard = self.playback_cache.lock().await;
                cache_insert_bounded(
                    &mut guard,
                    playback_key,
                    fetched.clone(),
                    PLAYBACK_CACHE_TTL,
                    PLAYBACK_CACHE_MAX,
                );
                fetched
            }
        };

        // ⚠️ 顺序不能反：改写成 `/stream?u=…` 之后前端 `isHlsUrl()` 恒为真，
        // 再分类就永远得到 hls。必须在代理之前认出真实形态。
        let remote = remote.as_str();
        let stream_kind = classify_stream(remote);
        if stream_kind == "hls" {
            crate::hls_proxy::warm_hls(remote);
        }
        let url = crate::hls_proxy::proxied_url(remote).unwrap_or_else(|| remote.to_string());
        Ok(PlaybackSession {
            session_id,
            series_id: series_id.to_string(),
            episode_id: episode_id.to_string(),
            position,
            // 只有一路码流，如实给 auto（与 settings_save 的强制口径一致）。
            quality: if quality.trim().is_empty() {
                "auto".into()
            } else {
                quality.to_string()
            },
            url,
            backup_url: None,
            stream_kind: Some(stream_kind.to_string()),
            // 实测 main 与 preview 是同一个地址，播放列表 38 片 + ENDLIST、
            // 总时长与 episodes[].duration 一致 —— 是整集，不是试看片段。
            preview_only: false,
        })
    }

    /// 取一张封面的本地路径。入参是**远端地址**而不是 series_id：
    /// 列表响应里已经带着 cover，按 id 再查一次详情等于每张卡多一次往返。
    pub async fn cover(&self, url: &str) -> Option<String> {
        if let Some(hit) = media::cached_cover(url) {
            return Some(hit);
        }
        if self.prefer_fallback.load(Ordering::Relaxed) {
            if let Some(fallback) = self.fallback_client().await {
                if let Some(path) = media::cover(&fallback, url).await {
                    return Some(path);
                }
            }
        }
        if let Some(path) = media::cover(&self.client, url).await {
            self.prefer_fallback.store(false, Ordering::Relaxed);
            return Some(path);
        }
        let fallback = self.fallback_client().await?;
        let path = media::cover(&fallback, url).await;
        if path.is_some() {
            self.prefer_fallback.store(true, Ordering::Relaxed);
        }
        path
    }

    /// 批量封面。
    ///
    /// 先同步命中磁盘缓存与去重，未命中再并发拉取；若记住兜底可用则直接走代理，
    /// 否则主链路缺失项合并成一批切换本机代理重试并记住状态。
    pub async fn cover_batch(&self, urls: Vec<String>) -> Vec<Option<String>> {
        if urls.is_empty() {
            return Vec::new();
        }
        let total = urls.len();
        let mut results: Vec<Option<String>> = vec![None; total];
        let mut pending: Vec<(usize, String)> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut dupes: Vec<(usize, usize)> = Vec::new();
        for (index, url) in urls.iter().enumerate() {
            if let Some(hit) = media::cached_cover(url) {
                results[index] = Some(hit);
                continue;
            }
            let stable = url.split('?').next().unwrap_or(url).to_string();
            if let Some(&first_idx) = seen.get(&stable) {
                dupes.push((index, first_idx));
            } else {
                seen.insert(stable, index);
                pending.push((index, url.clone()));
            }
        }
        if !pending.is_empty() {
            let use_fallback = self.prefer_fallback.load(Ordering::Relaxed);
            let first_client = if use_fallback {
                self.fallback_client()
                    .await
                    .unwrap_or_else(|| self.client.clone())
            } else {
                self.client.clone()
            };
            let first_urls: Vec<String> = pending.iter().map(|(_, u)| u.clone()).collect();
            let first_out = Self::fetch_covers(&first_client, first_urls).await;
            for ((slot, _), path) in pending.into_iter().zip(first_out) {
                if path.is_some() {
                    results[slot] = path;
                }
            }
            let missing: Vec<usize> = seen
                .into_values()
                .filter(|&slot| results[slot].is_none())
                .collect();
            if !missing.is_empty() {
                let retry_client = if use_fallback {
                    Some(self.client.clone())
                } else {
                    self.fallback_client().await
                };
                if let Some(retry_client) = retry_client {
                    if !use_fallback {
                        crate::trace::log(format!(
                            "[黄果] 封面主链路缺 {} 张，改走本机代理重试",
                            missing.len()
                        ));
                    }
                    let retry_urls: Vec<String> =
                        missing.iter().map(|index| urls[*index].clone()).collect();
                    let retried = Self::fetch_covers(&retry_client, retry_urls).await;
                    let mut recovered = 0usize;
                    for (slot, path) in missing.into_iter().zip(retried) {
                        if path.is_some() {
                            results[slot] = path;
                            recovered += 1;
                        }
                    }
                    if recovered > 0 && !use_fallback {
                        self.prefer_fallback.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        for (dst_idx, src_idx) in dupes {
            results[dst_idx] = results[src_idx].clone();
        }
        results
    }

    /// 并发拉一批封面，返回值与入参严格同序（失败位保留 `None`）。
    async fn fetch_covers(client: &reqwest::Client, urls: Vec<String>) -> Vec<Option<String>> {
        let total = urls.len();
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(COVER_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for (index, url) in urls.into_iter().enumerate() {
            let client = client.clone();
            let permits = permits.clone();
            tasks.spawn(async move {
                let _permit = permits.acquire().await.ok();
                (index, media::cover(&client, &url).await)
            });
        }
        // 槽位按**入参长度**预开：JoinSet 的完成顺序是乱的，任务 panic 时那一格
        // 也要留 None，绝不能让返回值变短（前端按下标对齐，短一格就整屏串图）。
        let mut results: Vec<Option<String>> = vec![None; total];
        while let Some(joined) = tasks.join_next().await {
            if let Ok((index, path)) = joined {
                if let Some(slot) = results.get_mut(index) {
                    *slot = path;
                }
            }
        }
        results
    }
}

/// 与 `anime_provider::classify_stream` 同一口径：只看路径后缀，忽略 query。
fn classify_stream(url: &str) -> &'static str {
    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        "hls"
    } else {
        "file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_kind_is_decided_from_the_path_not_the_query() {
        assert_eq!(
            classify_stream("https://cdn.example/a/b.m3u8?auth_key=1-0-0-x&via_m=dx150"),
            "hls"
        );
        assert_eq!(
            classify_stream("https://cdn.example/a/b.mp4?auth_key=1"),
            "file"
        );
        assert_eq!(classify_stream("https://cdn.example/a/b"), "file");
    }

    #[test]
    fn page_size_is_clamped_to_the_site_budget() {
        assert_eq!(0u32.clamp(1, MAX_PAGE_SIZE), 1);
        assert_eq!(999u32.clamp(1, MAX_PAGE_SIZE), 50);
        assert_eq!(24u32.clamp(1, MAX_PAGE_SIZE), 24);
    }
}

/// 线上台架：默认 `#[ignore]`，用 `cargo test huangguo_live -- --ignored` 手动跑。
///
/// 与 `hongguo/live.rs` 同一套惯例——站点有 WAF、域名会轮换、还要过代理，
/// 放进常规 `cargo test` 会让离线环境必红。这里验的是**真实链路**：
/// 信封能不能打通、映射出来的字段对不对、封面能不能解密成图片、
/// 播放地址是不是可播的 m3u8。
#[cfg(test)]
mod live {
    use super::*;

    fn provider() -> HuangguoProvider {
        HuangguoProvider::new().expect("客户端构造")
    }

    fn filter(category: &str, keyword: Option<&str>) -> CatalogFilter {
        CatalogFilter {
            channel: mapping::CHANNEL.to_string(),
            source: None,
            category: category.to_string(),
            audience: "全部".to_string(),
            sort: "recommend".to_string(),
            keyword: keyword.map(str::to_string),
            page: 1,
            page_size: 4,
            cursor: None,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn huangguo_live_catalog_covers_every_channel() {
        let provider = provider();
        for (name, _slug) in mapping::CATEGORIES {
            let page = provider
                .catalog(&filter(name, None))
                .await
                .unwrap_or_else(|error| panic!("{name} 目录失败: {error}"));
            assert!(!page.items.is_empty(), "{name} 返回空列表");
            assert!(page.total > 0, "{name} 总数为 0");
            for item in &page.items {
                assert!(item.id.starts_with("hg:"), "id 未加前缀: {}", item.id);
                assert!(!item.title.is_empty());
                assert!(item.cover.starts_with("https://"), "封面地址异常");
                assert_eq!(item.item_type, "huangguo");
            }
            println!("{name}: {} 条 / 共 {}", page.items.len(), page.total);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn huangguo_live_detail_and_playback_resolve() {
        let provider = provider();
        let page = provider.catalog(&filter("全部", None)).await.expect("目录");
        let first = page.items.first().expect("至少一部");
        let detail = provider.detail(&first.id).await.expect("详情");
        assert_eq!(detail.id, first.id);
        assert!(!detail.episodes.is_empty(), "详情没有选集");
        assert_eq!(detail.playable_episodes, None, "不该凭空给可播边界");
        assert!(detail.sources.is_empty());
        assert!(detail.available_qualities.is_empty());

        let episode = &detail.episodes[0];
        let session = provider
            .open_episode(9001, &first.id, &episode.id, "auto", 0.0)
            .await
            .expect("播放解析");
        // 代理未启动时 proxied_url 返回 None，这里拿到的就是远端地址。
        assert!(
            session.url.contains(".m3u8"),
            "播放地址不是 m3u8: {}",
            session.url
        );
        assert_eq!(session.stream_kind.as_deref(), Some("hls"));
        assert!(!session.preview_only, "实测是整集，不该标试看");
        println!("播放地址形态 ok，集时长 {}s", episode.duration_seconds);
    }

    #[tokio::test]
    #[ignore]
    async fn huangguo_live_cover_decrypts_to_a_real_image() {
        let provider = provider();
        let page = provider.catalog(&filter("全部", None)).await.expect("目录");
        let covers: Vec<String> = page.items.iter().map(|item| item.cover.clone()).collect();
        let paths = provider.cover_batch(covers).await;
        let resolved = paths.iter().flatten().count();
        assert!(resolved > 0, "一张封面都没解出来");
        for path in paths.iter().flatten() {
            let bytes = std::fs::read(path).expect("缓存文件可读");
            assert!(
                bytes.starts_with(&[0xFF, 0xD8]) || bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                "解出来的不是图片: {path}"
            );
        }
        println!("封面解密 {resolved}/{} 张", paths.len());
    }

    #[tokio::test]
    #[ignore]
    async fn huangguo_live_search_returns_prefixed_items() {
        let provider = provider();
        let page = provider
            .catalog(&filter("全部", Some("逆袭")))
            .await
            .expect("搜索");
        assert!(!page.items.is_empty(), "搜索返回空");
        assert!(page.items.iter().all(|item| item.id.starts_with("hg:")));
        println!("搜索命中 {} 部 / 共 {}", page.items.len(), page.total);
    }

    /// 锁住「封面偶发空白」的根因修复（2026-11-11）。
    ///
    /// 主客户端只挂系统代理。系统代理开关为 0 时它必然出不去网，此时 provider
    /// 必须靠**惰性探测**的兜底客户端自愈——而不是在 `new()` 里就把「没有代理」
    /// 烤进整个进程。这条用例把两条链路各自的结局打出来，便于下次归因。
    #[tokio::test]
    #[ignore]
    async fn huangguo_live_recovers_when_system_proxy_is_off() {
        let provider = provider();
        let primary =
            client::edge(&provider.client, "GET", "/api/videos?page=1&size=1", None).await;
        println!(
            "主链路（仅系统代理）结局 = {:?}",
            primary.as_ref().map(|_| "ok").map_err(String::as_str)
        );

        // 不论主链路通不通，provider 都必须给出结果。
        let page = provider
            .catalog(&filter("全部", None))
            .await
            .expect("provider 必须能自愈拿到目录");
        assert!(!page.items.is_empty());

        if primary.is_err() {
            let fallback = client::fallback_client().expect("主链路失败时必须探到本机代理");
            let recovered = client::edge(&fallback, "GET", "/api/videos?page=1&size=1", None).await;
            assert!(recovered.is_ok(), "兜底链路也没通：{recovered:?}");
            println!("兜底链路（本机探测代理）结局 = ok，自愈成立");
        } else {
            println!("主链路已通（系统代理或透明代理生效），本次未触发兜底");
        }
    }
}
