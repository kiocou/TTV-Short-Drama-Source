//! 黄果短剧专区（**huangguo ≠ hongguo**）数据源。
//!
//! 目录名只差一个字母，读代码时务必看清：`hongguo/` 是红果（原生签名链路，
//! 24 个文件、黄金向量锁签名），本模块是黄果，两者没有任何共享代码。
//!
//! ## 为什么自包含
//!
//! 这个源是「隐藏频道」形态：不进 `source_registry::BUILTIN_SOURCES`（那里的
//! 测试断言内置源恒为 2 条）、不进 `enabled_sources`（`normalize_enabled_sources`
//! 无条件返回 `["hongguo"]`）、不进全局搜索的 `merge_search_sources`。
//! 删掉本目录 + `huangguo_provider.rs` + `main.rs` 里四条 `if` 分支即可完整移除，
//! 不会在别的模块里留下半截改动。
//!
//! ## 分层
//!
//! - `client` —— 边缘信封（所有 `/api/**` 收成一条 POST）
//! - `media` —— 封面 AES-128-CBC 解密与落盘缓存
//! - `mapping` —— 站点 JSON → `crate::models` 契约
//!
//! ## 取证结论（2026-11-11 实测，与接入前的假设有三处不同）
//!
//! 1. **不需要 Referer**：封面与 m3u8 各做「带 / 不带」对照，返回字节完全相同。
//!    所以 `hls_proxy` 一行都不用改。
//! 2. **不需要 `x-cdn-auth`**：playback 会下发这个头，但媒体请求不带它同样 200。
//! 3. **`streams.main` 是标准 AES-128 HLS VOD**（38 片 + `#EXT-X-ENDLIST`，
//!    总时长与 `episodes[].duration` 一致），不是试看片段。`hls_proxy` 的
//!    `rewrite_playlist_with` 已经会重写 `#EXT-X-KEY` 的 URI，直接可用。
//!
//! 另外两条如实记录：`episodes[].status` 实测全为 1，锁集语义未证实，因此
//! **一律不锁集**；`sort` 参数站点未提供对应口径，本模块忽略它（只支持频道、
//! 关键词与分页）。

pub mod client;
pub mod mapping;
pub mod media;
