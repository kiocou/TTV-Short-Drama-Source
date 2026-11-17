use crate::models::{FavoriteItem, UserSettings, WatchHistoryItem};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "
                PRAGMA journal_mode = WAL;
                PRAGMA foreign_keys = ON;
                CREATE TABLE IF NOT EXISTS watch_history (
                    series_id TEXT NOT NULL,
                    episode_id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    series_cover TEXT NOT NULL,
                    episode_number INTEGER NOT NULL,
                    total_episodes INTEGER NOT NULL,
                    position_seconds REAL NOT NULL,
                    duration_seconds REAL NOT NULL,
                    progress_percent INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    is_finished INTEGER NOT NULL,
                    channel TEXT,
                    history_type TEXT NOT NULL DEFAULT 'watch',
                    PRIMARY KEY (series_id, history_type)
                );
                CREATE INDEX IF NOT EXISTS idx_watch_history_updated_at
                    ON watch_history(updated_at DESC);
                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS favorites (
                    series_id TEXT PRIMARY KEY NOT NULL,
                    title TEXT NOT NULL,
                    cover TEXT NOT NULL,
                    mark TEXT NOT NULL,
                    channel TEXT,
                    updated_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_favorites_updated_at
                    ON favorites(updated_at DESC);
                ",
            )
            .map_err(|error| error.to_string())?;
        Self::migrate_watch_history_if_needed(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn normalize_history_type(value: Option<&str>) -> &'static str {
        match value.map(str::trim) {
            Some("recommendation") => "recommendation",
            Some("featured") => "featured",
            _ => "watch",
        }
    }

    fn migrate_watch_history_if_needed(connection: &Connection) -> Result<(), String> {
        let mut stmt = connection
            .prepare("PRAGMA table_info(watch_history)")
            .map_err(|error| error.to_string())?;
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        if columns.iter().any(|col| col == "history_type") {
            return Ok(());
        }
        connection
            .execute_batch(
                "
                BEGIN IMMEDIATE;
                CREATE TABLE watch_history_v2 (
                    series_id TEXT NOT NULL,
                    episode_id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    series_cover TEXT NOT NULL,
                    episode_number INTEGER NOT NULL,
                    total_episodes INTEGER NOT NULL,
                    position_seconds REAL NOT NULL,
                    duration_seconds REAL NOT NULL,
                    progress_percent INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    is_finished INTEGER NOT NULL,
                    channel TEXT,
                    history_type TEXT NOT NULL DEFAULT 'watch',
                    PRIMARY KEY (series_id, history_type)
                );
                INSERT INTO watch_history_v2 (
                    series_id, episode_id, title, series_cover, episode_number, total_episodes,
                    position_seconds, duration_seconds, progress_percent, updated_at, is_finished,
                    channel, history_type
                )
                SELECT
                    series_id, episode_id, title, series_cover, episode_number, total_episodes,
                    position_seconds, duration_seconds, progress_percent, updated_at, is_finished,
                    channel, 'watch'
                FROM watch_history;
                DROP TABLE watch_history;
                ALTER TABLE watch_history_v2 RENAME TO watch_history;
                CREATE INDEX IF NOT EXISTS idx_watch_history_updated_at
                    ON watch_history(updated_at DESC);
                COMMIT;
                ",
            )
            .map_err(|error| error.to_string())
    }

    pub fn list_history(&self) -> Result<Vec<WatchHistoryItem>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "历史数据库锁不可用。".to_string())?;
        let mut statement = connection
            .prepare(
                "SELECT series_id, episode_id, title, series_cover, episode_number, total_episodes,
                        position_seconds, duration_seconds, progress_percent, updated_at, is_finished, channel, history_type
                 FROM watch_history ORDER BY updated_at DESC LIMIT 200",
            )
            .map_err(|error| error.to_string())?;
        let mut records = statement
            .query_map([], Self::map_history_row)
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        // 渲染用改写：库里存的是可持久化的原始地址，交给前端的那一份才换成本地代理
        // （境外图床直连会超时，见 `hls_proxy::proxied_image_url`）。写库前会还原，
        // 所以库里永远不会留下代理地址。
        for item in &mut records {
            Self::proxy_history_cover(item);
        }
        Ok(records)
    }

    pub fn get_history_item(
        &self,
        series_id: &str,
        history_type: Option<&str>,
    ) -> Result<Option<WatchHistoryItem>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "历史数据库锁不可用。".to_string())?;
        let mut item = match history_type {
            Some(ht) => connection
                .query_row(
                    "SELECT series_id, episode_id, title, series_cover, episode_number, total_episodes,
                            position_seconds, duration_seconds, progress_percent, updated_at, is_finished, channel, history_type
                     FROM watch_history WHERE series_id = ?1 AND history_type = ?2",
                    params![series_id, Self::normalize_history_type(Some(ht))],
                    Self::map_history_row,
                )
                .optional()
                .map_err(|error| error.to_string())?,
            None => connection
                .query_row(
                    "SELECT series_id, episode_id, title, series_cover, episode_number, total_episodes,
                            position_seconds, duration_seconds, progress_percent, updated_at, is_finished, channel, history_type
                     FROM watch_history WHERE series_id = ?1 ORDER BY updated_at DESC LIMIT 1",
                    params![series_id],
                    Self::map_history_row,
                )
                .optional()
                .map_err(|error| error.to_string())?,
        };
        if let Some(record) = item.as_mut() {
            Self::proxy_history_cover(record);
        }
        Ok(item)
    }

    fn map_history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WatchHistoryItem> {
        let raw_type: Option<String> = row.get(12)?;
        Ok(WatchHistoryItem {
            series_id: row.get(0)?,
            episode_id: row.get(1)?,
            title: row.get(2)?,
            series_cover: row.get(3)?,
            episode_number: row.get(4)?,
            total_episodes: row.get(5)?,
            position_seconds: row.get(6)?,
            duration_seconds: row.get(7)?,
            progress_percent: row.get(8)?,
            updated_at: row.get(9)?,
            is_finished: row.get::<_, i64>(10)? != 0,
            channel: row.get(11)?,
            history_type: Some(Self::normalize_history_type(raw_type.as_deref()).to_string()),
        })
    }

    fn should_proxy_cover(series_id: &str, channel: Option<&str>, url: &str) -> bool {
        let trimmed = url.trim();
        if trimmed.is_empty()
            || crate::huangguo::mapping::is_huangguo_id(series_id)
            || crate::guo_provider::is_guo_id(series_id)
        {
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("data:")
            || lower.starts_with("asset:")
            || lower.starts_with("http://127.0.0.1:")
            || lower.starts_with("http://localhost:")
        {
            return false;
        }
        let path_only = lower.split('?').next().unwrap_or(&lower);
        // HEIC 封面需要由前端 `coverResolver` 走 `short_drama_app_cover_proxy` 解码，不能改成 127.0.0.1 否则正则失配。
        if path_only.ends_with(".heic") {
            return false;
        }
        if crate::anime_provider::is_anime_id(series_id) || channel == Some("anime") {
            return true;
        }
        let host = lower
            .split("://")
            .nth(1)
            .unwrap_or(&lower)
            .split('/')
            .next()
            .unwrap_or("");
        !host.contains("fqnovel")
            && !host.contains("byteimg")
            && !host.contains("snssdk")
            && !host.contains("pstatp")
    }

    fn proxy_cover_if_needed(series_id: &str, channel: Option<&str>, url: &mut String) {
        if !Self::should_proxy_cover(series_id, channel, url) {
            return;
        }
        if let Some(proxied) = crate::hls_proxy::proxied_image_url(url) {
            *url = proxied;
        }
    }

    fn proxy_history_cover(item: &mut WatchHistoryItem) {
        Self::proxy_cover_if_needed(
            &item.series_id,
            item.channel.as_deref(),
            &mut item.series_cover,
        );
    }

    pub fn save_history(&self, item: &WatchHistoryItem) -> Result<(), String> {
        if item.series_id.trim().is_empty() || item.episode_id.trim().is_empty() {
            return Err("历史记录缺少剧集或集数标识。".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "历史数据库锁不可用。".to_string())?;
        // 冲突时是否采用本次的播放进度，取决于本次上报的时长是否可信。
        //
        // duration 为 0 意味着上报时视频元数据还没就绪——前端在 metadata 到达
        // 之前也会保存一次（用于先记住"看到第几集"）。旧实现无条件覆盖，
        // 于是这样一次上报就会把之前正确的时长冲成 0，历史页随即显示成
        // "0分0秒 / 0分0秒"，且进度条永远为空。时长不可信时只更新元数据，
        // position / duration / percent 三件套一律保留库里的旧值。
        let history_type = Self::normalize_history_type(item.history_type.as_deref());
        connection
            .execute(
                "INSERT INTO watch_history (
                    series_id, episode_id, title, series_cover, episode_number, total_episodes,
                    position_seconds, duration_seconds, progress_percent, updated_at, is_finished, channel, history_type
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(series_id, history_type) DO UPDATE SET
                    episode_id = excluded.episode_id,
                    title = CASE WHEN length(trim(excluded.title)) > 0
                        THEN excluded.title ELSE watch_history.title END,
                    series_cover = CASE WHEN length(trim(excluded.series_cover)) > 0
                        THEN excluded.series_cover ELSE watch_history.series_cover END,
                    episode_number = CASE WHEN excluded.episode_number > 0
                        THEN excluded.episode_number ELSE watch_history.episode_number END,
                    total_episodes = CASE WHEN excluded.total_episodes > 0
                        THEN excluded.total_episodes ELSE watch_history.total_episodes END,
                    position_seconds = CASE WHEN excluded.duration_seconds > 0 OR excluded.episode_id != watch_history.episode_id
                        THEN excluded.position_seconds ELSE watch_history.position_seconds END,
                    duration_seconds = CASE WHEN excluded.duration_seconds > 0
                        THEN excluded.duration_seconds
                        WHEN excluded.episode_id = watch_history.episode_id THEN watch_history.duration_seconds
                        ELSE 0 END,
                    progress_percent = CASE WHEN excluded.duration_seconds > 0 OR excluded.episode_id != watch_history.episode_id
                        THEN excluded.progress_percent ELSE watch_history.progress_percent END,
                    updated_at = excluded.updated_at,
                    -- is_finished 同样受“时长是否可信”约束：duration=0 时前端算不出
                    -- 百分比，带上来的一定是 false。若无条件覆盖，用户已经看完的一集
                    -- 会因为一次“时长未知”的上报退回未看完，历史页的徽章凭空消失；
                    -- 但独立播放器播到 ended 时即使缺 duration 也会显式传 is_finished=true。
                    is_finished = CASE
                        WHEN excluded.is_finished != 0 THEN 1
                        WHEN excluded.duration_seconds > 0 OR excluded.episode_id != watch_history.episode_id THEN excluded.is_finished
                        ELSE watch_history.is_finished END,
                    channel = COALESCE(excluded.channel, watch_history.channel)",
                params![
                    item.series_id,
                    item.episode_id,
                    item.title,
                    // 落库前还原成原始地址：代理地址跨进程启动即失效，见 restore_image_url。
                    crate::hls_proxy::restore_image_url(&item.series_cover),
                    item.episode_number,
                    item.total_episodes,
                    if item.position_seconds.is_finite() {
                        item.position_seconds.max(0.0)
                    } else {
                        0.0
                    },
                    if item.duration_seconds.is_finite() {
                        item.duration_seconds.max(0.0)
                    } else {
                        0.0
                    },
                    item.progress_percent.min(100),
                    item.updated_at,
                    item.is_finished as i32,
                    item.channel,
                    history_type,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn remove_history(
        &self,
        series_id: &str,
        history_type: Option<&str>,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "历史数据库锁不可用。".to_string())?;
        match history_type {
            Some(ht) => {
                connection
                    .execute(
                        "DELETE FROM watch_history WHERE series_id = ?1 AND history_type = ?2",
                        params![series_id, Self::normalize_history_type(Some(ht))],
                    )
                    .map_err(|error| error.to_string())?;
            }
            None => {
                connection
                    .execute(
                        "DELETE FROM watch_history WHERE series_id = ?1",
                        params![series_id],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub fn clear_history(&self, scope: Option<&str>) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "历史数据库锁不可用。".to_string())?;
        const IS_HUANGGUO: &str = "(series_id LIKE 'hg:%' OR coalesce(channel, '') = 'huangguo')";
        const IS_ANIME: &str = "(NOT (series_id LIKE 'hg:%' OR coalesce(channel, '') = 'huangguo') AND (coalesce(channel, '') = 'anime' OR series_id LIKE 'dmghg:%' OR series_id LIKE 'bfzy:%' OR series_id LIKE 'anime_%'))";
        match scope {
            None => {
                connection
                    .execute("DELETE FROM watch_history", [])
                    .map_err(|error| error.to_string())?;
            }
            Some("huangguo") => {
                connection
                    .execute(
                        &format!("DELETE FROM watch_history WHERE {IS_HUANGGUO}"),
                        [],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Some("non_huangguo") => {
                connection
                    .execute(
                        &format!("DELETE FROM watch_history WHERE NOT {IS_HUANGGUO}"),
                        [],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Some("anime") => {
                connection
                    .execute(&format!("DELETE FROM watch_history WHERE {IS_ANIME}"), [])
                    .map_err(|error| error.to_string())?;
            }
            Some("recommendation") => {
                connection
                    .execute(
                        &format!("DELETE FROM watch_history WHERE NOT {IS_HUANGGUO} AND NOT {IS_ANIME} AND history_type = 'recommendation'"),
                        [],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Some("featured") => {
                connection
                    .execute(
                        &format!("DELETE FROM watch_history WHERE NOT {IS_HUANGGUO} AND NOT {IS_ANIME} AND history_type != 'recommendation'"),
                        [],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Some(other) => {
                return Err(format!("未知的历史清理范围：{other}"));
            }
        }
        Ok(())
    }

    pub fn settings_get(&self) -> Result<UserSettings, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "设置数据库锁不可用。".to_string())?;
        let raw: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'user_settings'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        match raw {
            Some(value) => serde_json::from_str(&value).map_err(|_| "保存的设置格式无效。".into()),
            None => Ok(UserSettings::default()),
        }
    }

    pub fn settings_save(&self, settings: &UserSettings) -> Result<(), String> {
        let value = serde_json::to_string(settings).map_err(|error| error.to_string())?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| "设置数据库锁不可用。".to_string())?;
        connection
            .execute(
                "INSERT INTO settings(key, value) VALUES ('user_settings', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![value],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn list_favorites(&self, mark: Option<&str>) -> Result<Vec<FavoriteItem>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "收藏数据库锁不可用。".to_string())?;
        let read_rows = |statement: &mut rusqlite::Statement,
                         params: &[&dyn rusqlite::ToSql]|
         -> Result<Vec<FavoriteItem>, String> {
            let mapped = statement
                .query_map(params, |row| {
                    Ok(FavoriteItem {
                        series_id: row.get(0)?,
                        title: row.get(1)?,
                        cover: row.get(2)?,
                        mark: row.get(3)?,
                        channel: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                })
                .map_err(|error| error.to_string())?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())
        };
        let mut records = match mark {
            Some(value) => {
                let mut statement = connection
                    .prepare(
                        "SELECT series_id, title, cover, mark, channel, updated_at
                         FROM favorites WHERE mark = ?1 ORDER BY updated_at DESC",
                    )
                    .map_err(|error| error.to_string())?;
                read_rows(&mut statement, &[&value])?
            }
            None => {
                let mut statement = connection
                    .prepare(
                        "SELECT series_id, title, cover, mark, channel, updated_at
                         FROM favorites ORDER BY updated_at DESC",
                    )
                    .map_err(|error| error.to_string())?;
                read_rows(&mut statement, &[])?
            }
        };
        // 同 list_history：渲染用改写，库里存原始地址。
        for item in &mut records {
            Self::proxy_cover_if_needed(&item.series_id, item.channel.as_deref(), &mut item.cover);
        }
        Ok(records)
    }

    pub fn save_favorite(&self, item: &FavoriteItem) -> Result<(), String> {
        if item.series_id.trim().is_empty() {
            return Err("收藏条目缺少剧集标识。".into());
        }
        // 状态只有三种：未知值是前端 bug 的信号，报错而不是静默改写。
        if !matches!(item.mark.as_str(), "want" | "watching" | "done") {
            return Err(format!("未知的收藏状态：{}", item.mark));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "收藏数据库锁不可用。".to_string())?;
        connection
            .execute(
                "INSERT INTO favorites (series_id, title, cover, mark, channel, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(series_id) DO UPDATE SET
                    title = excluded.title,
                    cover = excluded.cover,
                    mark = excluded.mark,
                    channel = excluded.channel,
                    updated_at = excluded.updated_at",
                params![
                    item.series_id,
                    item.title,
                    // 同 save_history：落库前还原成原始地址。
                    crate::hls_proxy::restore_image_url(&item.cover),
                    item.mark,
                    item.channel,
                    item.updated_at,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn remove_favorite(&self, series_id: &str) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "收藏数据库锁不可用。".to_string())?;
        connection
            .execute(
                "DELETE FROM favorites WHERE series_id = ?1",
                params![series_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::models::WatchHistoryItem;

    fn sample_history(series_id: &str, episode_id: &str) -> WatchHistoryItem {
        WatchHistoryItem {
            series_id: series_id.to_string(),
            episode_id: episode_id.to_string(),
            title: "测试短剧".to_string(),
            series_cover: "https://example.test/cover.jpg".to_string(),
            episode_number: 3,
            total_episodes: 80,
            position_seconds: 42.0,
            duration_seconds: 120.0,
            progress_percent: 35,
            updated_at: 1000,
            is_finished: false,
            channel: Some("drama".to_string()),
            history_type: Some("featured".to_string()),
        }
    }

    #[test]
    fn save_history_preserves_existing_metadata_when_incoming_fields_are_empty() {
        let dir = std::env::temp_dir().join(format!("ttv-storage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(&dir.join("db.sqlite")).unwrap();

        db.save_history(&sample_history("s1", "e3")).unwrap();

        let partial = WatchHistoryItem {
            series_id: "s1".to_string(),
            episode_id: "e3".to_string(),
            title: String::new(),
            series_cover: String::new(),
            episode_number: 3,
            total_episodes: 0,
            position_seconds: 84.0,
            duration_seconds: 120.0,
            progress_percent: 70,
            updated_at: 2000,
            is_finished: false,
            channel: None,
            history_type: Some("featured".to_string()),
        };
        db.save_history(&partial).unwrap();

        let saved = db
            .get_history_item("s1", Some("featured"))
            .unwrap()
            .expect("history row");
        assert_eq!(saved.title, "测试短剧");
        let expected_cover = crate::hls_proxy::proxied_image_url("https://example.test/cover.jpg")
            .unwrap_or_else(|| "https://example.test/cover.jpg".to_string());
        assert_eq!(saved.series_cover, expected_cover);
        assert_eq!(saved.total_episodes, 80);
        assert_eq!(saved.channel.as_deref(), Some("drama"));
        assert!((saved.position_seconds - 84.0).abs() < 1e-6);
        assert_eq!(saved.progress_percent, 70);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_history_resets_same_series_stale_duration_when_switching_episode() {
        let dir = std::env::temp_dir().join(format!("ttv-storage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(&dir.join("db.sqlite")).unwrap();

        db.save_history(&sample_history("s1", "e1")).unwrap();
        let next_ep = WatchHistoryItem {
            series_id: "s1".to_string(),
            episode_id: "e2".to_string(),
            title: "测试短剧".to_string(),
            series_cover: String::new(),
            episode_number: 2,
            total_episodes: 0,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            progress_percent: 0,
            updated_at: 2000,
            is_finished: false,
            channel: None,
            history_type: Some("featured".to_string()),
        };
        db.save_history(&next_ep).unwrap();

        let saved = db
            .get_history_item("s1", Some("featured"))
            .unwrap()
            .expect("history row");
        assert_eq!(saved.episode_id, "e2");
        assert_eq!(saved.episode_number, 2);
        assert!((saved.position_seconds - 0.0).abs() < 1e-6);
        assert!((saved.duration_seconds - 0.0).abs() < 1e-6);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_separates_sections_and_clear_scopes() {
        let dir = std::env::temp_dir().join(format!("ttv-storage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(&dir.join("db.sqlite")).unwrap();

        let featured = sample_history("s1", "e1");
        let rec = WatchHistoryItem {
            history_type: Some("recommendation".to_string()),
            episode_id: "e5".to_string(),
            episode_number: 5,
            updated_at: 2000,
            ..sample_history("s1", "e5")
        };
        let anime = WatchHistoryItem {
            series_id: "dmghg:100".to_string(),
            channel: Some("anime".to_string()),
            updated_at: 3000,
            ..sample_history("dmghg:100", "ep1")
        };
        let huangguo = WatchHistoryItem {
            series_id: "hg:999".to_string(),
            channel: Some("huangguo".to_string()),
            updated_at: 4000,
            ..sample_history("hg:999", "ep1")
        };

        db.save_history(&featured).unwrap();
        db.save_history(&rec).unwrap();
        db.save_history(&anime).unwrap();
        db.save_history(&huangguo).unwrap();

        let all = db.list_history().unwrap();
        assert_eq!(all.len(), 4);

        db.clear_history(Some("recommendation")).unwrap();
        let after_rec = db.list_history().unwrap();
        assert_eq!(after_rec.len(), 3);
        assert!(db
            .get_history_item("s1", Some("featured"))
            .unwrap()
            .is_some());

        db.clear_history(Some("non_huangguo")).unwrap();
        let after_main_clear = db.list_history().unwrap();
        assert_eq!(after_main_clear.len(), 1);
        assert_eq!(after_main_clear[0].series_id, "hg:999");

        db.clear_history(Some("huangguo")).unwrap();
        assert!(db.list_history().unwrap().is_empty());

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_watch_history_table_migrates_without_data_loss() {
        let dir = std::env::temp_dir().join(format!("ttv-storage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("legacy.sqlite");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE watch_history (
                    series_id TEXT PRIMARY KEY NOT NULL,
                    episode_id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    series_cover TEXT NOT NULL,
                    episode_number INTEGER NOT NULL,
                    total_episodes INTEGER NOT NULL,
                    position_seconds REAL NOT NULL,
                    duration_seconds REAL NOT NULL,
                    progress_percent INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    is_finished INTEGER NOT NULL,
                    channel TEXT
                );
                INSERT INTO watch_history VALUES ('s_old', 'e1', '旧剧', '', 1, 10, 12.0, 60.0, 20, 500, 0, 'drama');",
            )
            .unwrap();
        }
        let db = Database::open(&db_path).unwrap();
        let list = db.list_history().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].series_id, "s_old");
        assert_eq!(list[0].history_type.as_deref(), Some("watch"));

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_proxy_cover_skips_heic_huangguo_and_domestic_hosts() {
        assert!(!Database::should_proxy_cover(
            "7707576684714675225",
            Some("drama"),
            "https://p3-reading-sign.fqnovelpic.com/novel-pic/abc~tplv.heic?x-expires=123",
        ));
        assert!(!Database::should_proxy_cover(
            "7705726893818924056",
            Some("drama"),
            "https://p6-novel.byteimg.com/novel-pic/abc~tplv-shrink:640:0.image",
        ));
        assert!(!Database::should_proxy_cover(
            "hg:6875",
            Some("huangguo"),
            "https://cdn.huangguo.test/cover.jpg?auth_key=abc",
        ));
        assert!(!Database::should_proxy_cover(
            "guo:dsd:1",
            None,
            "https://cdn.example/cover.jpg",
        ));
        assert!(Database::should_proxy_cover(
            "dmghg:123",
            Some("anime"),
            "https://p1-ad.adukwai.com/cover.jpg",
        ));
    }
}
