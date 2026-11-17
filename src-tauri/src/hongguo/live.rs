//! 真实网络集成验证。
//!
//! 默认不跑（`#[ignore]`），因为它们会真的连红果接口。需要时显式执行：
//!
//! ```sh
//! cargo test --bin ttv-short-drama live_ -- --ignored --nocapture
//! ```
//!
//! 这些用例存在的意义：签名算法错了不会编译失败，只会被服务端用
//! 「HTTP 200 + 空 body」静默拒绝。只有真打一次请求才能证明整条链路是对的。
//!
//! ## ⚠️ 不要一次跑全部 `live_` 用例（2026-11-09 实测）
//!
//! 它们会**并发**打同一个服务端，而红果对匿名设备是有配额的（每台设备只回一次
//! 播放模型，见 `client.rs` 的设备语义）。一次全跑实测会出现 1-2 个用例红：
//! `live_ai_channel_yields_cards_and_playable_bound` 与
//! `app_search_live_finds_jinjiuzhou` 都曾因同时索页/索搜而被判失败，
//! 但**单独重跑立刻通过**——是限流，不是回归。
//!
//! 判断方法：红了先单独重跑那一条。串行跑或用 `--test-threads=1` 更稳。

use crate::hongguo::catalog::{channel_accepts, DiscoverChannel};
use crate::hongguo::client::HongguoClient;

/// 全球死考第一季第 1 集（openhongguo 文档里的示例剧集）。
const SAMPLE_EPISODE: &str = "7675424396831378456";

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_register_and_resolve_playback() {
    let client = HongguoClient::new(None).await.expect("创建客户端失败");

    // 1) 匿名注册
    let profile = client.ensure_guest().await.expect("匿名注册失败");
    assert!(profile.is_ready(), "注册后 device_id/install_id 必须有效");
    eprintln!(
        "[live] 注册成功 device_id={} install_id={}",
        profile.device_id_str, profile.install_id_str
    );

    // 2) 播放解析
    let started = std::time::Instant::now();
    let resolved = client
        .resolve_playback(SAMPLE_EPISODE)
        .await
        .expect("播放解析失败");
    let elapsed = started.elapsed();

    eprintln!(
        "[live] 播放解析耗时 {}ms，码流 {} 路，加密={}",
        elapsed.as_millis(),
        resolved.playback.streams.len(),
        resolved.playback.protected
    );
    for s in &resolved.playback.streams {
        eprintln!(
            "  - {} / {} / {}x{} / {}kbps / protected={}",
            s.definition,
            s.codec,
            s.width,
            s.height,
            s.bitrate / 1000,
            s.protected
        );
    }

    assert!(!resolved.playback.streams.is_empty(), "必须至少有一路码流");
    assert!(
        resolved.playback.streams.iter().all(|s| !s.url.is_empty()),
        "所有码流都要有地址"
    );
    if resolved.playback.protected {
        let key = resolved
            .content_key
            .as_ref()
            .expect("加密流必须能解出内容密钥");
        assert_eq!(key.hex.len(), 32, "内容密钥应为 32 位 hex");
        eprintln!("[live] 内容密钥 {}... 版本 {}", &key.hex[..8], key.version);
    }
}

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_signature_is_accepted() {
    // 只验证「签名被服务端接受」这一件事：能拿到非空 body 且无业务错误码。
    // 用一个确定存在的分集，请求播放模型。
    let client = HongguoClient::new(None).await.expect("创建客户端失败");
    let result = client.resolve_playback(SAMPLE_EPISODE).await;
    match result {
        Ok(_) => {}
        Err(e) => panic!("签名未被接受：{e}（若是 AnonymousLimited 说明签名算法有偏差）"),
    }
}

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_search_and_series() {
    let client = HongguoClient::new(None).await.expect("创建客户端失败");

    let started = std::time::Instant::now();
    let hits = client.search("末世", 10, 0).await.expect("搜索失败");
    eprintln!(
        "[live] 搜索 {}ms，命中 {} 部",
        started.elapsed().as_millis(),
        hits.len()
    );
    assert!(
        !hits.is_empty(),
        "搜索必须返回结果（空结果说明签名或参数不对）"
    );
    for h in hits.iter().take(3) {
        eprintln!("  - {} / {} / {}集", h.id, h.title, h.episode_count);
    }

    // 取第一部拿详情
    let sid = &hits[0].id;
    let started = std::time::Instant::now();
    let (series, episodes) = client.get_series(sid).await.expect("详情失败");
    eprintln!(
        "[live] 详情 {}ms：{} / {} 集",
        started.elapsed().as_millis(),
        series.hit.title,
        episodes.len()
    );
    assert!(!eps_is_empty(&episodes), "详情必须带分集列表");
    eprintln!("  第 1 集: {} 第{}集", episodes[0].id, episodes[0].number);

    // 用详情拿到的一集去解析播放
    let started = std::time::Instant::now();
    let resolved = client
        .resolve_playback(&episodes[0].id)
        .await
        .expect("播放解析失败");
    eprintln!(
        "[live] 播放解析 {}ms，{} 路码流",
        started.elapsed().as_millis(),
        resolved.playback.streams.len()
    );
    assert!(resolved.playback.protected);
    assert!(resolved.content_key.is_some());
}

fn eps_is_empty(v: &[crate::hongguo::catalog::EpisodeHit]) -> bool {
    v.is_empty()
}

#[ignore = "需要真实网络"]
async fn live_duration_check() {
    let client = HongguoClient::new(None).await.expect("client");
    let resolved = client
        .resolve_playback(SAMPLE_EPISODE)
        .await
        .expect("resolve");
    eprintln!(
        "[live] duration_seconds = {}",
        resolved.playback.duration_seconds
    );
    eprintln!("[live] expires_at = {}", resolved.playback.expires_at);
    eprintln!("[live] media_type = {}", resolved.playback.media_type);
    for s in &resolved.playback.streams {
        eprintln!(
            "  {} / {}x{} / {}bps / hls={} dash={}",
            s.definition, s.width, s.height, s.bitrate, s.hls, s.dash
        );
    }
}

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_user_episode_duration() {
    let client = HongguoClient::new(None).await.expect("client");
    for vid in ["7690936226991311897", "7690936062583000089"] {
        match client.resolve_playback(vid).await {
            Ok(r) => eprintln!(
                "[live] {} duration={}s streams={}",
                vid,
                r.playback.duration_seconds,
                r.playback.streams.len()
            ),
            Err(e) => eprintln!("[live] {} 失败: {}", vid, e),
        }
    }
}

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_dump_stream_url() {
    let client = HongguoClient::new(None).await.expect("client");
    let r = client
        .resolve_playback("7690936226991311897")
        .await
        .expect("resolve");
    let best = r.playback.best_stream().expect("best");
    let key = r
        .content_key
        .as_ref()
        .map(|k| k.hex.clone())
        .unwrap_or_default();
    let tmp_dir = std::env::temp_dir();
    std::fs::write(tmp_dir.join("ttv-url.txt"), &best.url).unwrap();
    std::fs::write(tmp_dir.join("ttv-key.txt"), &key).unwrap();
    eprintln!(
        "[live] 已导出 url/key, duration={}s",
        r.playback.duration_seconds
    );
}

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_diagnose_vid() {
    // 新设备（内存档案）测应用里失败的那个 vid
    let fresh = HongguoClient::new(None).await.expect("client");
    let target = "7690208595719244825";
    match fresh.resolve_playback(target).await {
        Ok(r) => eprintln!(
            "[diag] 新设备 OK: streams={} duration={}s",
            r.playback.streams.len(),
            r.playback.duration_seconds
        ),
        Err(e) => eprintln!("[diag] 新设备 失败: {e}"),
    }
    // 用应用里那份落盘档案再测一次
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".app-data")
        .join("hongguo-device.json");
    match HongguoClient::new(Some(path)).await {
        Ok(persisted) => match persisted.resolve_playback(target).await {
            Ok(r) => eprintln!("[diag] 落盘设备 OK: streams={}", r.playback.streams.len()),
            Err(e) => eprintln!("[diag] 落盘设备 失败: {e}"),
        },
        Err(e) => eprintln!("[diag] 载入落盘档案失败: {e}"),
    }
}

#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_dump_cover_url() {
    let client = HongguoClient::new(None).await.expect("client");
    let hits = client.search("末世", 5, 0).await.expect("search");
    let mut out = String::new();
    for h in &hits {
        out.push_str(&h.cover);
        out.push('\n');
    }
    std::fs::write(std::env::temp_dir().join("ttv-covers.txt"), &out).unwrap();
    eprintln!("[live] 导出 {} 条封面 URL", hits.len());
    if let Some(first) = hits.first() {
        eprintln!(
            "[live] 首条: {}",
            first.cover.chars().take(160).collect::<String>()
        );
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 发现流（推荐页的 App 数据源）
// ───────────────────────────────────────────────────────────────────────────

/// 真人剧发现页（`discover` 的 `tab_type=39`）。
///
/// ## 为什么补这一条
///
/// `hongguo_discover` 是推荐页**真人剧 / 漫剧**唯一的取数入口
/// （`useFeedStore::fetchPage` → `ipcService.hongguo.discover`），而本文件原有的
/// 网络台架全部覆盖注册 / 播放 / 搜索 / 榜单 / 详情，**一条都没碰过 discover**。
/// 这条链路的失败形态是"HTTP 200 + 空 cell"：签名错、`tab_type` 写错、或服务端
/// 改了 cell 结构，前端看到的都是同一句「当前频道暂时没有可播放的推荐」——
/// 与"这一频道真没内容"完全无法区分，只能靠真打一次请求分辨。
///
/// 因此断言两件事：
/// 1. 至少出 1 条（空页 = 链路没打通，不是正常的空数据）；
/// 2. 条目的 `content_type` 与频道相符——发现流是混合流，服务端偶发串味行
///    （见 `catalog.rs::channel_accepts`），逐条校验是这个闸门的现场演练。
///    `None`（源没给标记）放行，理由同 `channel_accepts` 的注释。
///
/// ⚠️ 不要和其它 `live_` 用例一起跑：红果对匿名设备有配额（见文件头注释）。
/// 单独执行：
///
/// ```sh
/// cd src-tauri && cargo test live_discover -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "需要真实网络"]
async fn live_discover_live_channel() {
    let client = HongguoClient::new(None).await.expect("创建客户端失败");
    let started = std::time::Instant::now();
    let page = client
        .discover_page(DiscoverChannel::LiveDrama, None)
        .await
        .expect("真人剧发现页失败（空 body 通常意味着签名或 tab_type 被服务端拒绝）");
    eprintln!(
        "[live] 发现页(真人剧) {}ms，{} 条，has_more={}，next_cursor={}",
        started.elapsed().as_millis(),
        page.hits.len(),
        page.has_more,
        page.next_cursor.is_some()
    );
    assert!(
        !page.hits.is_empty(),
        "真人剧发现页至少要出 1 条（空页说明链路没打通，不是正常的空数据）"
    );

    for hit in page.hits.iter().take(3) {
        eprintln!(
            "  - {} / {} / content_type={:?}",
            hit.id, hit.title, hit.content_type
        );
    }

    let mismatched: Vec<String> = page
        .hits
        .iter()
        .filter(|hit| !channel_accepts("drama", hit.content_type))
        .map(|hit| format!("{} ({:?})", hit.title, hit.content_type))
        .collect();
    assert!(
        mismatched.is_empty(),
        "真人剧发现页混进了其它内容类型: {mismatched:?}"
    );
}
