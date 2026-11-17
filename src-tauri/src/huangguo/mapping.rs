//! 站点 JSON → `crate::models` 契约映射。
//!
//! 契约里有三条**不能想当然**的（都写在 `models.rs` 的字段注释里）：
//! - `rating` 站点不给就是 `None`，绝不填 0（前端 `{series.rating && …}`）；
//! - `playable_episodes` 没有权威信号就是 `None`，不能退化成「全部可播」；
//! - `sources` / `available_qualities` 恒空——这条链路只有一路码流，
//!   硬造档位等于做一个改了没反应的开关。
//!
//! 站点的数值字段类型不统一：feed 的 `duration` 是字符串 `"207"`，详情
//! `episodes[].duration` 是数字 `207`。所以取值一律过 `flex_*`，不要只写 `as_u64`。

use crate::models::{CatalogPage, EpisodeItem, SeriesDetail, SeriesItem};
use serde_json::Value;

/// 频道值。前端 `ChannelType` 与后端 `filter.channel` 共用这一个字面量。
pub const CHANNEL: &str = "huangguo";

/// 剧集 id 前缀。
///
/// **必须加前缀**：黄果 id 是裸数字，与红果 `series_id` 完全同形，而
/// `DetailView` 的红果元数据钩子用 `/^\d{4,25}$/` 判定——裸数字会被它劫持。
/// 前缀同时让后端路由不依赖前端那个重载即空的 `channelBySeriesId`。
pub const ID_PREFIX: &str = "hg:";

pub const ORIGIN_LABEL: &str = "黄果短剧";

/// 四个频道：显示名 → 站点 cate slug。全部接入，不做排除。
pub const CATEGORIES: [(&str, &str); 4] = [
    ("AI成人短剧", "ai-duanju"),
    ("AI成人漫剧", "ai-manju"),
    ("AI换脸", "ai-huanlian"),
    ("AI魔改", "ai-mogai"),
];

pub fn is_huangguo_id(series_id: &str) -> bool {
    raw_id(series_id).is_some()
}

/// 剥前缀拿裸数字 id；不是本源的 id 返回 `None`。
pub fn raw_id(series_id: &str) -> Option<&str> {
    let rest = series_id.strip_prefix(ID_PREFIX)?;
    (!rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())).then_some(rest)
}

pub fn series_id(raw: &str) -> String {
    format!("{ID_PREFIX}{raw}")
}

pub fn category_names() -> Vec<String> {
    CATEGORIES
        .iter()
        .map(|(name, _)| name.to_string())
        .collect()
}

/// 显示名 → slug。空 / 「全部」返回 `None`，表示走全站 feed。
pub fn cate_slug(category: &str) -> Option<&'static str> {
    let trimmed = category.trim();
    if trimmed.is_empty() || trimmed == "全部" {
        return None;
    }
    CATEGORIES
        .iter()
        .find(|(name, slug)| *name == trimmed || *slug == trimmed)
        .map(|(_, slug)| *slug)
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn flex_f64(value: &Value, key: &str) -> f64 {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn flex_u32(value: &Value, key: &str) -> u32 {
    let parsed = flex_f64(value, key);
    if parsed.is_finite() && parsed > 0.0 {
        parsed as u32
    } else {
        0
    }
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 评分：0-10 量纲原样透传。缺失或非正数一律 `None`。
fn rating(value: &Value) -> Option<f64> {
    let score = flex_f64(value, "score");
    (score.is_finite() && score > 0.0).then_some(score)
}

pub fn series_item(value: &Value) -> Option<SeriesItem> {
    let raw = match value.get("id") {
        Some(Value::Number(number)) => number.as_i64()?.to_string(),
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        _ => return None,
    };
    let title = text(value, "title");
    if title.is_empty() {
        return None;
    }
    let brief = text(value, "description");
    Some(SeriesItem {
        id: series_id(&raw),
        title,
        // 保留原始远端密文地址：解密与落盘由 media::cover 负责，
        // 这里绝不存代理地址或本地路径（端口/令牌每次启动都变）。
        cover: text(value, "cover"),
        item_type: CHANNEL.to_string(),
        episodes_count: flex_u32(value, "episode_count"),
        latest_episode_title: None,
        tags: string_list(value, "tags"),
        origin: ORIGIN_LABEL.to_string(),
        brief: (!brief.is_empty()).then_some(brief),
        rating: rating(value),
    })
}

pub fn catalog_page(data: &Value, page: u32, categories: Vec<String>) -> CatalogPage {
    let items: Vec<SeriesItem> = data
        .get("items")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(series_item).collect())
        .unwrap_or_default();
    let pagination = data.get("pagination").cloned().unwrap_or(Value::Null);
    let pages = flex_u32(&pagination, "pages");
    let total = flex_u32(&pagination, "total") as usize;
    CatalogPage {
        // 站点偶有脏数据被 series_item 滤掉，total 用站点给的总数而不是 items.len()，
        // 否则分页器会在中途以为「到底了」。
        total: if total > 0 { total } else { items.len() },
        has_more: pages > page,
        page,
        items,
        categories,
        next_cursor: None,
        source: ORIGIN_LABEL.to_string(),
        degraded: false,
    }
}

pub fn detail(data: &Value, id: &str) -> SeriesDetail {
    let episodes: Vec<EpisodeItem> = data
        .get("episodes")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let number = flex_u32(row, "ep_num");
                    if number == 0 {
                        return None;
                    }
                    let title = text(row, "title");
                    Some(EpisodeItem {
                        // 集号即 id：series_id 已经携带来源，不需要再编一层。
                        id: number.to_string(),
                        series_id: id.to_string(),
                        episode_number: number,
                        title: if title.is_empty() {
                            format!("第{number}集")
                        } else {
                            title
                        },
                        duration_seconds: flex_f64(row, "duration"),
                        preview_url: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let count = flex_u32(data, "episode_count");
    SeriesDetail {
        id: id.to_string(),
        title: text(data, "title"),
        cover: text(data, "cover"),
        item_type: CHANNEL.to_string(),
        tags: string_list(data, "tags"),
        origin: ORIGIN_LABEL.to_string(),
        episodes_count: if count > 0 {
            count
        } else {
            episodes.len() as u32
        },
        description: text(data, "description"),
        episodes,
        // `episodes[].status` 实测全为 1，锁集语义未证实 → 不给信号。
        // 前端按「未知」处理，不会据此锁集，也不会据此宣称全部可播。
        playable_episodes: None,
        available_qualities: Vec::new(),
        sources: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card() -> Value {
        json!({
            "id": 12, "title": "  某剧  ", "cover": "https://pic.example/a.jpg?auth_key=1",
            "description": "简介", "episode_count": 21, "total_episodes": 21,
            "score": 9.6, "duration": "207", "tags": ["逆袭", "", "都市"]
        })
    }

    #[test]
    fn ids_are_prefixed_so_the_hongguo_numeric_hook_cannot_hijack_them() {
        let item = series_item(&card()).unwrap();
        assert_eq!(item.id, "hg:12");
        assert!(is_huangguo_id(&item.id));
        assert_eq!(raw_id(&item.id), Some("12"));
        // 裸数字、空前缀、非数字都不是本源的 id。
        assert!(!is_huangguo_id("12"));
        assert!(!is_huangguo_id("hg:"));
        assert!(!is_huangguo_id("hg:abc"));
        assert!(!is_huangguo_id("guo:huangguoai:12"));
    }

    #[test]
    fn rating_is_none_when_the_site_gives_no_score() {
        let mut value = card();
        value["score"] = json!(0);
        assert_eq!(series_item(&value).unwrap().rating, None);
        value["score"] = Value::Null;
        assert_eq!(series_item(&value).unwrap().rating, None);
        value.as_object_mut().unwrap().remove("score");
        assert_eq!(series_item(&value).unwrap().rating, None);
        // 字符串评分也要认（站点数值类型不统一）。
        value["score"] = json!("8.5");
        assert_eq!(series_item(&value).unwrap().rating, Some(8.5));
    }

    #[test]
    fn cover_keeps_the_original_remote_url() {
        let item = series_item(&card()).unwrap();
        assert_eq!(item.cover, "https://pic.example/a.jpg?auth_key=1");
    }

    #[test]
    fn duration_is_read_from_both_string_and_number_shapes() {
        let data = json!({
            "id": 12, "title": "某剧", "episode_count": 2,
            "episodes": [
                {"ep_num": 1, "title": "第1集", "duration": 207, "status": 1},
                {"ep_num": 2, "title": "", "duration": "181", "status": 1}
            ]
        });
        let parsed = detail(&data, "hg:12");
        assert_eq!(parsed.episodes.len(), 2);
        assert_eq!(parsed.episodes[0].duration_seconds, 207.0);
        assert_eq!(parsed.episodes[1].duration_seconds, 181.0);
        assert_eq!(parsed.episodes[1].title, "第2集");
        assert_eq!(parsed.episodes[0].id, "1");
    }

    #[test]
    fn detail_never_invents_playable_bounds_qualities_or_sources() {
        let parsed = detail(&json!({"id": 12, "title": "某剧", "episodes": []}), "hg:12");
        assert_eq!(parsed.playable_episodes, None);
        assert!(parsed.available_qualities.is_empty());
        assert!(parsed.sources.is_empty());
    }

    #[test]
    fn pagination_uses_the_site_total_not_the_filtered_item_count() {
        let data = json!({
            "items": [card()],
            "pagination": {"page": 1, "pages": 1688, "size": 1, "total": 3375}
        });
        let page = catalog_page(&data, 1, category_names());
        assert_eq!(page.total, 3375);
        assert!(page.has_more);
        assert!(!page.degraded);
        assert_eq!(page.categories.len(), 4);
        let last = catalog_page(&data, 1688, category_names());
        assert!(!last.has_more);
    }

    #[test]
    fn every_upstream_channel_is_addressable() {
        assert_eq!(cate_slug("AI成人短剧"), Some("ai-duanju"));
        assert_eq!(cate_slug("AI成人漫剧"), Some("ai-manju"));
        assert_eq!(cate_slug("AI换脸"), Some("ai-huanlian"));
        assert_eq!(cate_slug("AI魔改"), Some("ai-mogai"));
        assert_eq!(cate_slug("ai-mogai"), Some("ai-mogai"));
        assert_eq!(cate_slug("全部"), None);
        assert_eq!(cate_slug(""), None);
        assert_eq!(cate_slug("不存在的频道"), None);
    }

    #[test]
    fn cards_without_a_title_or_id_are_dropped() {
        assert!(series_item(&json!({"id": 1})).is_none());
        assert!(series_item(&json!({"title": "无 id"})).is_none());
    }
}
