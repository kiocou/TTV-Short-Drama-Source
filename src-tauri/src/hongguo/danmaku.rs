//! 红果弹幕：按时间窗拉取、解析、规整。
//!
//! ## 协议来源
//!
//! 接口与参数移自「真果鉴」的 Go 原生核心（`native/core/ui_playback_danmaku.go`），
//! 那套实现经过实机验证，这里只做 Rust 侧对齐，不改协议语义：
//!
//! ```text
//! POST /novel/commentapi/comment/list/{video_id}/v1/
//! {
//!   comment_source: 601, server_channel: 1000,
//!   group_id: {video_id}, group_type: 30, comment_type: 20,
//!   sort: 1, count: 90, cursor: "",
//!   aid: 8662, compliance_status: 0,
//!   business_param: {
//!     book_id: {series_id}, start_offset_time: {start_ms},
//!     playlet_item_duration: {episode_ms},
//!     need_danmaku_guide_type: [1,3,4,2]
//!   }
//! }
//! ```
//!
//! 接口、参数与返回字段在 2026-11-15 对上游 openhongguo **v0.3.0** 核过一遍
//! （`internal/protocol/danmaku.go` 的 `DanmakuBody` / `ParseDanmaku`、
//! `sdk/model/catalog.go` 的 `Danmaku` / `DanmakuPage`、`internal/protocol/app.go`
//! 的 `PathDanmaku`）：路径、请求体字段、`count: 90`、`cursor` 语义一致。
//! 与上游的差异只剩两处，都是有意的：
//!
//! - `playlet_item_duration` 传调用方给的**集长**（前端本来就传媒体时长，装载期
//!   读不到时长时用「起点 + 一个窗口」兜底），与上游 `durationMS = 集长` 同义；
//! - 位置仍按 `[start_ms, next_ms)` 过滤（上游只排序不过滤）。前端按绝对时间
//!   定位每一条，越界的行会画到错误的画面上，所以这道过滤必须留着。
//!
//! 上游 v0.3.0 把 `DanmakuOptions.StartMS` 改名为 `StartMs`；Rust 侧没有对应的
//! 选项结构，IPC 入参本来就是 `startMs`（serde camelCase），改名不影响这里。
//!
//! ## 为什么按「时间窗」而不是一次拉整集
//!
//! 一集 40~90 秒的短剧弹幕量不大，但**播放器只关心当前可见的一段**：按 30 秒窗口
//! 分页拉，拖动进度条时只补目标窗口那一页，缓存命中即可零等待。整集一次拉完在
//! 长剧（900 集、单集 3 分钟）上会拖慢起播，而且大量数据从没用上。
//!
//! ## 返回规整（与前端共用同一套判据）
//!
//! 服务端回包里有几类必须丢掉的行，不丢会直接表现为「弹幕和画面对不上」：
//!
//! - `common.group_id` 不等于本集 → 串集（同屏混进别的剧的弹幕）；
//! - `common.status` 不是 `"1"` → 已删除/审核中的弹幕；
//! - `expand.offset_time` 落在窗口外 → 前端会按时间点定位，越界即错位；
//! - `text` 为空、`comment_id` 为空或重复 → 渲染层无法分配轨道。
//!
//! 上限 90 条/页、单条 180 字符，与协议侧 `count: 90` 对齐。

use serde_json::{json, Map, Value};

use crate::hongguo::client::HongguoClient;
use crate::hongguo::consts::PATH_DANMAKU_TEMPLATE;
use crate::hongguo::sign::{device_query_pairs, Pairs};

/// 单次请求覆盖的时间窗。与前端 `DANMAKU_WINDOW_MS` 必须一致。
pub const DANMAKU_WINDOW_MS: i64 = 30_000;

/// 单集最大时长（24 小时）。超过视为参数错误，避免拿脏数据打满服务端。
pub const DANMAKU_MAX_DURATION_MS: i64 = 24 * 60 * 60 * 1000;

/// 单页最多条数（与请求体的 `count` 一致）。
pub const DANMAKU_PAGE_LIMIT: usize = 90;

/// 单条弹幕最大字符数，超出截断并补省略号。
const DANMAKU_TEXT_LIMIT: usize = 180;

fn valid_numeric_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 25 && id.chars().all(|c| c.is_ascii_digit())
}

/// 取一页弹幕。
///
/// `start_ms` 是窗口起点；`duration_ms` 是这一集的时长（毫秒），也就是协议里的
/// `playlet_item_duration`。前端传的是媒体时长，装载期读不到时长时用
/// 「起点 + 一个窗口」兜底。上游 v0.3.0 明确要求它是**整集长度**：给成窗口终点
/// 会让服务端以为这一集已经到头，片尾的弹幕永远取不到。
pub async fn fetch_danmaku(
    client: &HongguoClient,
    series_id: &str,
    video_id: &str,
    start_ms: i64,
    duration_ms: i64,
) -> Result<Value, String> {
    if !valid_numeric_id(series_id) || !valid_numeric_id(video_id) {
        return Err("弹幕参数无效：缺少有效的剧集或分集 ID".into());
    }
    if start_ms < 0 || duration_ms <= start_ms || duration_ms > DANMAKU_MAX_DURATION_MS {
        return Err("弹幕时间范围无效".into());
    }

    // 字段与上游 `DanmakuBody` 一致；`serde_json` 没开 `preserve_order`，
    // 对象按字典序序列化，正好与 Go 的 map 编码同序（体只按 MD5 参与签名，
    // 顺序不影响校验，同形只是便于与官方流量对比）。
    let body = json!({
        "comment_source": 601,
        "server_channel": 1000,
        "group_id": video_id,
        "group_type": 30,
        "comment_type": 20,
        "sort": 1,
        "count": DANMAKU_PAGE_LIMIT,
        "cursor": "",
        "aid": 8662,
        "compliance_status": 0,
        "business_param": {
            "book_id": series_id,
            "start_offset_time": start_ms,
            "playlet_item_duration": duration_ms,
            "need_danmaku_guide_type": [1, 3, 4, 2],
        }
    })
    .to_string()
    .into_bytes();

    let path = PATH_DANMAKU_TEMPLATE.replace("{}", video_id);
    // 换设备闸门与上游 `ListDanmaku` 一致：上游把它包在
    // `withFreshDevice(..., playback=false)` 里 —— 设备被明确拒绝时换设备重跑，
    // 空结果不换（弹幕翻到没有内容的那一页本来就回空，误换等于白烧一台设备）。
    // 查询串必须每轮用**当前**档案重算：旧设备算出来的签名在新设备上对不上。
    let value = client
        .with_fresh_device("弹幕", || async {
            let pairs: Pairs = {
                let profile = client.ensure_guest().await?;
                device_query_pairs(&HongguoClient::identity(&profile))
            };
            client
                .request(reqwest::Method::POST, &path, pairs, Some(body.clone()))
                .await
        })
        .await
        .map_err(|e| e.to_string())?;

    parse_danmaku(&value, video_id, start_ms, duration_ms)
}

/// 把服务端回包规整成前端要的形状。
///
/// 判据与上游 v0.3.0 `protocol.ParseDanmaku` 对齐：
///
/// - 这一页覆盖的播放时间与总数取自 `common_list_info.cursor` 里的 JSON
///   （`start_offset_time` / `end_offset_time` / `danmaku_count`）；
/// - 有没有下一页看 `common_list_info.has_more`，下一页起点看
///   `extra.next_query_danmaku_list_time`；两者都不给时按请求的窗口终点收尾；
/// - 只有缺 `data` 才算坏包。旧实现把「没有下一页」当格式错误报出去 ——
///   最后一页本来就没有下一页，片尾整段弹幕因此显示不出来。
///
/// 失败时返回 `Err` 而不是空列表：前端据此进入「冷却后重试」的分支，
/// 空列表会被误判成「这一集真的没有弹幕」。
fn parse_danmaku(
    value: &Value,
    video_id: &str,
    start_ms: i64,
    duration_ms: i64,
) -> Result<Value, String> {
    let data = value
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| "弹幕回包缺少 data".to_string())?;

    let info = data.get("common_list_info").and_then(Value::as_object);
    // 服务端把这一页的描述塞在一个 JSON 字符串里，不是对象。
    let cursor = info
        .and_then(|info| info.get("cursor"))
        .and_then(Value::as_str)
        .filter(|raw| raw.len() <= 8192)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let window_end = cursor
        .as_ref()
        .and_then(|cursor| cursor.get("end_offset_time"))
        .and_then(as_i64);
    let total = cursor
        .as_ref()
        .and_then(|cursor| cursor.get("danmaku_count"))
        .and_then(as_i64)
        .unwrap_or(0);

    let has_more = info.and_then(|info| info.get("has_more")).and_then(as_bool);
    let next_raw = data
        .get("extra")
        .and_then(Value::as_object)
        .and_then(|extra| extra.get("next_query_danmaku_list_time"))
        .and_then(as_i64);
    // 服务端说没有下一页、或者给的起点不前进，这一页就覆盖到请求的时长为止：
    // 不前进的 next 会让前端永远命中不了下一页、原地重发同一个请求。
    let next_ms = match next_raw {
        Some(next) if has_more != Some(false) && next > start_ms => next.min(duration_ms),
        _ => duration_ms,
    };

    let rows = data
        .get("data_list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut items: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for raw in rows {
        let comment = raw.get("comment").and_then(Value::as_object);
        let Some(comment) = comment else { continue };

        let common = comment.get("common").and_then(Value::as_object);
        let Some(common) = common else { continue };

        if !group_matches(common.get("group_id"), video_id) {
            continue; // 串集
        }
        if !status_is_active(common.get("status")) {
            continue; // 已删除 / 审核中
        }

        let position = comment
            .get("expand")
            .and_then(Value::as_object)
            .and_then(|expand| expand.get("offset_time"))
            .and_then(as_i64);
        let Some(position) = position else { continue };
        if position < start_ms || position >= next_ms {
            continue; // 窗口外，前端按时间点定位会错位
        }

        let id = comment
            .get("comment_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() || id.len() > 120 || !seen.insert(id.clone()) {
            continue;
        }

        let text = common
            .get("content")
            .and_then(Value::as_object)
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = clean_text(text);
        if text.is_empty() {
            continue;
        }

        // 上游 v0.3.0 的 `model.Danmaku` 还带点赞、+1 与发送时间。前端只用
        // id/text/timeMs，其余只新增不改名，旧读取方不受影响。
        let stat = comment.get("stat");
        let likes = stat
            .and_then(|stat| stat.get("digg_count"))
            .and_then(as_i64)
            .unwrap_or(0);
        let plus_ones = stat
            .and_then(|stat| stat.get("add_one_count"))
            .and_then(as_i64)
            .unwrap_or(0);
        let mut item = json!({
            "id": id,
            "text": text,
            "timeMs": position,
            "likes": likes,
            "plusOnes": plus_ones,
        });
        if let Some(created_at) = common
            .get("create_timestamp")
            .and_then(as_i64)
            .filter(|secs| *secs > 0)
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        {
            item["createdAt"] = json!(created_at.to_rfc3339());
        }
        items.push(item);
        if items.len() >= DANMAKU_PAGE_LIMIT {
            break;
        }
    }

    items.sort_by_key(|item| item.get("timeMs").and_then(as_i64).unwrap_or(0));

    Ok(json!({
        "episodeId": video_id,
        "startMs": start_ms,
        "nextMs": next_ms,
        "endMs": window_end.unwrap_or(next_ms),
        "hasMore": has_more.unwrap_or(next_ms < duration_ms),
        "total": total,
        "items": items,
    }))
}

/// 去掉控制字符、压缩空白、超长截断。
///
/// 控制字符必须清掉：`\u{2028}` / `\u{2029}`（行分隔符）混进文本会让浏览器
/// 在渲染时断行，弹幕会突然变成两行、轨道高度算错。
fn clean_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = false;
    for ch in raw.chars() {
        let mapped = if ch.is_control() || ch == '\u{2028}' || ch == '\u{2029}' {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(mapped);
    }
    let trimmed = out.trim();
    let count = trimmed.chars().count();
    if count <= DANMAKU_TEXT_LIMIT {
        return trimmed.to_string();
    }
    let mut cut: String = trimmed.chars().take(DANMAKU_TEXT_LIMIT).collect();
    cut.push('…');
    cut
}

/// 同时接受数字与数字字符串：`offset_time` 在不同接口上两种形态都出现过。
fn as_i64(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).ok();
    }
    value.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

/// 这条弹幕是否处于「正常展示」状态。
///
/// **必须同时接受数字与字符串**：实测接口回的是数字 `1`（2026-11-03 抓包，
/// 18/18 条都是 `Number`，没有一条是 `"1"`）。此前按字符串比较，
/// 于是每一条都被判成「审核中」丢掉 —— 表现为「弹幕已启用但一条都不显示」，
/// 而接口其实返回了 73 条。
fn status_is_active(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        Some(Value::String(s)) => s.trim() == "1",
        // 上游 v0.3.0 的抓包回包里这个字段可以整块缺席
        // （`testdata/protocol/danmaku-response.json` 就没有 status 与 group_id），
        // 缺省只能按「正常展示」处理，否则整页弹幕被丢光。
        Some(Value::Null) | None => true,
        _ => false,
    }
}

/// 分集 id 是否与这一集相符。
///
/// 服务端两种形态都出现过（字符串/数字）：只认字符串会把整页弹幕当「串集」
/// 丢光。缺字段时不拦 —— 请求路径本身已经限定了这一集。
fn group_matches(value: Option<&Value>, expected: &str) -> bool {
    match value {
        Some(Value::String(raw)) => raw.trim() == expected,
        Some(Value::Number(number)) => number.to_string() == expected,
        Some(Value::Null) | None => true,
        _ => false,
    }
}

/// 布尔字段的宽松读取：`true` / `1` / `"true"` / `"1"` 都算真。
/// `has_more` 回的是 bool，但同族的 `cursor`、`totalCount` 都见过字符串形态。
fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => Some(number.as_i64().unwrap_or(0) != 0),
        Value::String(raw) => match raw.trim() {
            "true" | "1" => Some(true),
            "false" | "0" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 进程内限时缓存；同键请求共享一次后台加载，加载期间不受容量清理影响。
pub(crate) struct TimedCache<K, V> {
    entries: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<K, std::sync::Arc<CacheSlot<V>>>>,
    >,
    capacity: usize,
    success_ttl: std::time::Duration,
    failure_ttl: std::time::Duration,
}

struct CacheSlot<V> {
    state: tokio::sync::watch::Sender<CacheState<V>>,
}

struct CacheEntry<V> {
    result: Result<V, String>,
    stored_at: std::time::Instant,
}

#[derive(Clone)]
enum CacheState<V> {
    Pending,
    Done(std::sync::Arc<CacheEntry<V>>),
}

impl<K, V> Clone for TimedCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            capacity: self.capacity,
            success_ttl: self.success_ttl,
            failure_ttl: self.failure_ttl,
        }
    }
}

impl<K, V> TimedCache<K, V>
where
    K: std::hash::Hash + Eq + Clone,
    V: Clone,
{
    pub(crate) fn new(
        capacity: usize,
        success_ttl: std::time::Duration,
        failure_ttl: std::time::Duration,
    ) -> Self {
        Self {
            entries: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            capacity: capacity.max(1),
            success_ttl,
            failure_ttl,
        }
    }

    pub(crate) async fn get_or_load<F, Fut>(&self, key: K, load: F) -> Result<V, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<V, String>> + Send + 'static,
        K: Send + Sync + 'static,
        V: Send + Sync + 'static,
    {
        let (slot, is_new) = {
            let mut entries = self.entries.lock().await;
            if let Some(slot) = entries.get(&key).cloned() {
                let state = slot.state.borrow().clone();
                match state {
                    CacheState::Pending => (slot.clone(), false),
                    CacheState::Done(entry) => {
                        let ttl = if entry.result.is_ok() {
                            self.success_ttl
                        } else {
                            self.failure_ttl
                        };
                        if entry.stored_at.elapsed() < ttl {
                            return entry.result.clone();
                        }
                        entries.remove(&key);
                        (Self::insert_slot(&mut entries, key.clone()), true)
                    }
                }
            } else {
                (Self::insert_slot(&mut entries, key.clone()), true)
            }
        };

        if is_new {
            let entries = self.entries.clone();
            let task_slot = slot.clone();
            let task_key = key.clone();
            let capacity = self.capacity;
            let success_ttl = self.success_ttl;
            let failure_ttl = self.failure_ttl;
            tokio::spawn(async move {
                let entry = std::sync::Arc::new(CacheEntry {
                    result: load().await,
                    stored_at: std::time::Instant::now(),
                });
                let mut entries = entries.lock().await;
                let _ = task_slot.state.send(CacheState::Done(entry));
                entries.retain(|_, slot| match &*slot.state.borrow() {
                    CacheState::Pending => true,
                    CacheState::Done(entry) => {
                        let ttl = if entry.result.is_ok() {
                            success_ttl
                        } else {
                            failure_ttl
                        };
                        entry.stored_at.elapsed() < ttl
                    }
                });
                while entries.len() > capacity {
                    let oldest = entries
                        .iter()
                        .filter(|(key, _)| *key != &task_key)
                        .filter_map(|(key, slot)| match &*slot.state.borrow() {
                            CacheState::Pending => None,
                            CacheState::Done(entry) => Some((key.clone(), entry.stored_at)),
                        })
                        .min_by_key(|(_, stored_at)| *stored_at)
                        .map(|(key, _)| key);
                    match oldest {
                        Some(oldest) => {
                            entries.remove(&oldest);
                        }
                        None => break,
                    }
                }
            });
        }

        let mut receiver = slot.state.subscribe();
        loop {
            if let CacheState::Done(entry) = receiver.borrow_and_update().clone() {
                return entry.result.clone();
            }
            receiver
                .changed()
                .await
                .map_err(|_| "弹幕请求未完成，请稍后重试".to_string())?;
        }
    }

    fn insert_slot(
        entries: &mut std::collections::HashMap<K, std::sync::Arc<CacheSlot<V>>>,
        key: K,
    ) -> std::sync::Arc<CacheSlot<V>> {
        let (state, _) = tokio::sync::watch::channel(CacheState::Pending);
        let slot = std::sync::Arc::new(CacheSlot { state });
        entries.insert(key, slot.clone());
        slot
    }
}

/// 供诊断面板用的摘要（不把整页弹幕倒进日志）。
pub fn summarize(page: &Value) -> Map<String, Value> {
    let mut summary = Map::new();
    if let Some(items) = page.get("items").and_then(Value::as_array) {
        summary.insert("count".into(), json!(items.len()));
    }
    if let Some(next) = page.get("nextMs").and_then(as_i64) {
        summary.insert("nextMs".into(), json!(next));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_result_is_reused_without_reloading() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let cache =
            TimedCache::<String, Value>::new(8, Duration::from_secs(300), Duration::from_secs(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let load = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({"items": []})) }
            }
        };
        let first = cache
            .get_or_load("same".into(), load.clone())
            .await
            .unwrap();
        let second = cache.get_or_load("same".into(), load).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failure_is_cached_briefly_then_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let cache = TimedCache::<String, Value>::new(
            8,
            Duration::from_secs(300),
            Duration::from_millis(40),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let load = {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err("timeout fixture".to_string())
                }
            }
        };
        assert_eq!(
            cache
                .get_or_load("same".into(), load.clone())
                .await
                .unwrap_err(),
            "timeout fixture"
        );
        assert_eq!(
            cache
                .get_or_load("same".into(), load.clone())
                .await
                .unwrap_err(),
            "timeout fixture"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            cache.get_or_load("same".into(), load).await.unwrap_err(),
            "timeout fixture"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn success_expires_and_completed_entries_are_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let expiring =
            TimedCache::<String, Value>::new(8, Duration::from_millis(30), Duration::from_secs(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let load = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({"items": []})) }
            }
        };
        expiring
            .get_or_load("same".into(), load.clone())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        expiring.get_or_load("same".into(), load).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let bounded =
            TimedCache::<String, Value>::new(1, Duration::from_secs(300), Duration::from_secs(300));
        for key in ["first", "second"] {
            bounded
                .get_or_load(key.into(), || async { Ok(json!({"items": []})) })
                .await
                .unwrap();
        }
        let second_calls = Arc::new(AtomicUsize::new(0));
        bounded
            .get_or_load("second".into(), {
                let calls = second_calls.clone();
                move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Ok(json!({"items": []})) }
                }
            })
            .await
            .unwrap();
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cache_key_is_exact_and_capacity_keeps_pending() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::Notify;

        let cache =
            TimedCache::<String, Value>::new(1, Duration::from_secs(300), Duration::from_secs(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let pending = tokio::spawn({
            let cache = cache.clone();
            let calls = calls.clone();
            let started = started.clone();
            let release = release.clone();
            async move {
                cache
                    .get_or_load("series-a|vid-a|0|30000".into(), move || {
                        let calls = calls.clone();
                        let started = started.clone();
                        let release = release.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            started.notify_one();
                            release.notified().await;
                            Ok(json!({"items": ["pending"]}))
                        }
                    })
                    .await
            }
        });
        started.notified().await;
        cache
            .get_or_load("series-a|vid-a|30000|60000".into(), || async {
                Ok(json!({"items": ["other"]}))
            })
            .await
            .unwrap();
        let same_pending = tokio::spawn({
            let cache = cache.clone();
            let calls = calls.clone();
            async move {
                cache
                    .get_or_load("series-a|vid-a|0|30000".into(), move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        async { Ok(json!({"items": ["duplicate"]})) }
                    })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        release.notify_waiters();
        assert_eq!(pending.await.unwrap().unwrap()["items"][0], "pending");
        assert_eq!(same_pending.await.unwrap().unwrap()["items"][0], "pending");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// 接口回的是数字 1（实测 18/18），这里锁住数字与字符串两种形态都要认。
    #[tokio::test]
    async fn same_key_shares_one_pending_load() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::Notify;

        let cache =
            TimedCache::<String, Value>::new(8, Duration::from_secs(300), Duration::from_secs(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let load = {
            let calls = calls.clone();
            let started = started.clone();
            let release = release.clone();
            move || {
                let calls = calls.clone();
                let started = started.clone();
                let release = release.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Ok(json!({"items": [{"text": "shared"}]}))
                }
            }
        };

        let first = tokio::spawn({
            let cache = cache.clone();
            let load = load.clone();
            async move { cache.get_or_load("same".into(), load).await }
        });
        started.notified().await;
        let second = tokio::spawn({
            let cache = cache.clone();
            let load = load.clone();
            async move { cache.get_or_load("same".into(), load).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.notify_waiters();

        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn status_accepts_number_and_string() {
        assert!(status_is_active(Some(&json!(1))));
        assert!(status_is_active(Some(&json!("1"))));
        assert!(status_is_active(Some(&json!(" 1 "))));
        assert!(!status_is_active(Some(&json!(0))));
        assert!(!status_is_active(Some(&json!("0"))));
        assert!(!status_is_active(Some(&json!(2))));
        // 缺字段按正常展示处理（上游抓包回包里没有 status 与 group_id）。
        assert!(status_is_active(None));
        assert!(status_is_active(Some(&Value::Null)));
    }

    /// 一页真实骨架（字段值按 2026-11-03 抓包）必须能解析出弹幕。
    #[test]
    fn parses_real_row_shape() {
        let page: Value = serde_json::from_str(
            r#"{"episodeId":"v1","startMs":0,"nextMs":30000,"total":1,"items":[]}"#,
        )
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 0);

        let raw: Value = serde_json::from_str(
            r#"{"data":{
                "extra":{"next_query_danmaku_list_time":30000},
                "common_list_info":{"cursor":"{\"danmaku_count\":2}"},
                "data_list":[
                  {"comment":{"comment_id":"c1","common":{"group_id":"v1","status":1,
                     "content":{"text":"第一条"}},"expand":{"offset_time":500}}},
                  {"comment":{"comment_id":"c2","common":{"group_id":"v1","status":0,
                     "content":{"text":"已删除"}},"expand":{"offset_time":900}}},
                  {"comment":{"comment_id":"c3","common":{"group_id":"v2","status":1,
                     "content":{"text":"串集"}},"expand":{"offset_time":1200}}}
                ]}}"#,
        )
        .unwrap();
        let out = parse_danmaku(&raw, "v1", 0, 30000).unwrap();
        let items = out["items"].as_array().unwrap();
        // 只保留 c1：c2 已删除、c3 是别的集
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["text"], "第一条");
        assert_eq!(items[0]["timeMs"], 500);
    }

    /// 数字形态的 group_id 也必须认：服务端两种形态都出现过，只收字符串会把
    /// 整页弹幕当「串集」丢光（与 `status` 那次事故同一类）。
    #[test]
    fn numeric_group_id_is_accepted() {
        let raw: Value = serde_json::from_str(
            r#"{"data":{"data_list":[
                {"comment":{"comment_id":"c1","common":{"group_id":123,"status":1,
                   "content":{"text":"数字集"}},"expand":{"offset_time":700}}}
             ]}}"#,
        )
        .unwrap();
        let out = parse_danmaku(&raw, "123", 0, 30000).unwrap();
        assert_eq!(out["items"].as_array().unwrap().len(), 1);
        assert_eq!(out["items"][0]["text"], "数字集");
    }

    /// 最后一页：服务端不再给 `next_query_danmaku_list_time` 时按窗口终点收尾。
    /// 旧实现这里报错，片尾整段弹幕因此显示不出来。
    #[test]
    fn last_page_without_next_ends_at_window_end() {
        let raw: Value = serde_json::from_str(
            r#"{"data":{"common_list_info":{"has_more":false,"cursor":"{\"danmaku_count\":9,\"end_offset_time\":60000}"},"data_list":[]}}"#,
        )
        .unwrap();
        let out = parse_danmaku(&raw, "v1", 30000, 60000).unwrap();
        assert_eq!(out["nextMs"], 60000);
        assert_eq!(out["endMs"], 60000);
        assert_eq!(out["hasMore"], false);
        assert_eq!(out["total"], 9);
    }

    /// `next` 不前进时按窗口终点收尾：否则前端按 `nextMs` 判定覆盖区间，
    /// 页永远覆盖不了当前位置，会一直重发同一页。
    #[test]
    fn non_advancing_next_falls_back_to_window_end() {
        let raw: Value = serde_json::from_str(
            r#"{"data":{"common_list_info":{"has_more":true},"extra":{"next_query_danmaku_list_time":30000},"data_list":[]}}"#,
        )
        .unwrap();
        let out = parse_danmaku(&raw, "v1", 30000, 60000).unwrap();
        assert_eq!(out["nextMs"], 60000);
        assert_eq!(out["hasMore"], true);
    }

    /// 上游 v0.3.0 的 `model.Danmaku` 字段（likes / plus_ones / created_at）能取到，
    /// 且不改动前端在用的 id/text/timeMs。
    #[test]
    fn upstream_item_fields_are_parsed() {
        let raw: Value = serde_json::from_str(
            r#"{"data":{"common_list_info":{"has_more":true,"cursor":"{\"danmaku_count\":12}"},"extra":{"next_query_danmaku_list_time":30000},"data_list":[
                {"comment":{"comment_id":"c1","common":{"group_id":"v1","status":1,
                   "content":{"text":"字段"},"create_timestamp":1700000000},
                   "expand":{"offset_time":1500},
                   "stat":{"digg_count":7,"add_one_count":3}}}
             ]}}"#,
        )
        .unwrap();
        let out = parse_danmaku(&raw, "v1", 0, 135000).unwrap();
        let item = &out["items"][0];
        assert_eq!(item["likes"], 7);
        assert_eq!(item["plusOnes"], 3);
        assert_eq!(item["createdAt"], "2023-11-14T22:13:20+00:00");
        assert_eq!(out["nextMs"], 30000);
        assert_eq!(out["hasMore"], true);
    }

    #[test]
    fn path_template_matches_upstream() {
        assert_eq!(
            PATH_DANMAKU_TEMPLATE.replace("{}", "7543196048185887806"),
            "/novel/commentapi/comment/list/7543196048185887806/v1/"
        );
    }
}
