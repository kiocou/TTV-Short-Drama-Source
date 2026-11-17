//! 播放模型（`video_model`）的解析与数据模型。
//!
//! 移植自 `internal/protocol/parse.go` 的播放相关部分。上游字段类型不稳定
//! （同一个键有时是字符串、有时是数字、有时是 JSON 字符串里再套 JSON），
//! 所以这里全程用 `serde_json::Value` + 容错取值，而不是强类型反序列化 ——
//! 强类型会在真实响应上因为一个 `"0"` vs `0` 就整条解析失败。

use serde_json::Value;

/// 一路码流。
#[derive(Debug, Clone, Default)]
pub struct Stream {
    pub definition: String,
    pub quality: String,
    pub codec: String,
    pub width: i32,
    pub height: i32,
    pub bitrate: i64,
    pub size: i64,
    pub url: String,
    pub backup_url: String,
    pub hls: bool,
    pub dash: bool,
    pub protected: bool,
    pub encryption: String,
}

/// 一集的播放模型。
#[derive(Debug, Clone, Default)]
pub struct Playback {
    pub episode_id: String,
    pub video_id: String,
    pub media_type: String,
    pub duration_seconds: i32,
    pub expires_at: i64,
    /// 备播接口（`fallback_api`）：直链过期或失效时，可以拿它**原地换一条新直链**。
    ///
    /// 为什么这个字段比它看起来重要：原生链路的失败回退手段是**换设备**
    /// （`with_fresh_device`），而换设备是**有配额的稀缺资源**——每台匿名设备
    /// 只回一次播放模型，`DeviceRejected` / `AnonymousLimited` 各只允许 2 次。
    /// 直链带时间戳签名（`url_expire`），过期是常态；以前过期只能烧一次换设备
    /// 配额，现在可以先走这条备播接口重签。
    ///
    /// 实测形状是**对象里再套同名键**（2026-11-09，MiniReel 运行时抓包）：
    /// `{"fallback_api": {"fallback_api": "https://vas-lf-x.snssdk.com/video/fplay/…"}}`。
    /// 也见过直接给字符串的形态，两种都收（见 [`fallback_api_of`]）。
    pub fallback_api: String,
    pub streams: Vec<Stream>,
    pub audio_url: String,
    pub protected: bool,
    /// 包装后的密钥材料（`spade_a`），交给 `drm::derive` 解包。
    pub key_material: String,
}

/// 该编码能否被 WebView2 直接解码（不需要额外解码器）。
///
/// 只认 H.264 与 HEVC：Windows 自带 H.264，HEVC 由 HEVC Video Extension 提供
/// （应用已在 WebView2 启动参数里打开 `PlatformHEVCDecoderSupport`）。
/// `bytevc1` / `bytevc2` 是字节自有编码，系统没有对应解码器 —— 硬解不可能命中，
/// 播放时只能软解，表现就是"画面不流畅、发飘"。
fn is_decodable_codec(codec: &str) -> bool {
    let value = codec.trim().to_ascii_lowercase();
    if value.is_empty() {
        // 未知编码不算可直接播放，但也不在这里排除 —— 由调用方的"别无选择"兜底。
        return false;
    }
    value.contains("h264")
        || value.contains("avc")
        || value.contains("hevc")
        || value.contains("h265")
        || value.contains("hvc1")
        || value.contains("hev1")
}

pub fn is_bytevc2_codec(codec: &str) -> bool {
    let value = codec.trim().to_ascii_lowercase();
    value.contains("bytevc2") || value.contains("bvc2") || value.contains("h266")
}

pub fn is_h264_codec(codec: &str) -> bool {
    let value = codec.trim().to_ascii_lowercase();
    !is_bytevc2_codec(&value)
        && !value.contains("h265")
        && !value.contains("hvc1")
        && !value.contains("hevc")
        && (value.contains("h264") || value.contains("avc"))
}

impl Playback {
    /// 最高档位（按高度取最大；同高度取码率高者）。
    pub fn best_stream(&self) -> Option<&Stream> {
        self.streams.iter().max_by_key(|s| (s.height, s.bitrate))
    }

    /// 是否至少有一路非 `bytevc2` 的有效码流（`bytevc2` 本机解码链不支持）。
    pub fn has_decodable_or_bvc1_stream(&self) -> bool {
        self.streams
            .iter()
            .any(|s| !s.url.trim().is_empty() && !is_bytevc2_codec(&s.codec))
    }

    /// 选一路 **WebView2/ffmpeg/mpv 能解码**的最高档（预签名直链专用）。
    ///
    /// 优先级：H.264/HEVC → 其余非 bytevc2（如 bytevc1）→ bytevc2。
    pub fn best_playable_stream(&self) -> Option<&Stream> {
        let pick_with = |pred: &dyn Fn(&Stream) -> bool| {
            self.streams
                .iter()
                .filter(|s| !s.url.trim().is_empty())
                .filter(|s| pred(s))
                .max_by_key(|s| (s.height, s.bitrate))
        };
        pick_with(&|s| is_decodable_codec(&s.codec))
            .or_else(|| pick_with(&|s| !is_bytevc2_codec(&s.codec)))
            .or_else(|| pick_with(&|_| true))
    }

    /// 按档位选一路可播放的码流，语义与 Go 的 `Playback.Select` 一致。
    ///
    /// - `""` / `"best"`：最高分辨率，同分辨率取最高码率。
    /// - `"worst"`：最低分辨率。
    /// - 其它值：依次匹配分辨率标签（`720p`）、画质标签（`normal`）或编码名，不区分大小写。
    ///
    /// 没有 URL 的流永远不参与选择，否则会选中一条无法播放的记录。
    pub fn select(&self, quality: &str) -> Result<&Stream, String> {
        let mut usable: Vec<&Stream> = self
            .streams
            .iter()
            .filter(|s| !s.url.trim().is_empty())
            .collect();
        if usable.is_empty() {
            return Err("播放模型没有可用的码流地址".into());
        }
        usable.sort_by_key(|s| (stream_height_rank(s), s.bitrate));

        match quality.trim().to_ascii_lowercase().as_str() {
            "" | "best" => Ok(usable[usable.len() - 1]),
            "worst" => Ok(usable[0]),
            _ => usable
                .iter()
                .copied()
                .find(|s| {
                    s.definition.eq_ignore_ascii_case(quality)
                        || s.quality.eq_ignore_ascii_case(quality)
                        || s.codec.eq_ignore_ascii_case(quality)
                })
                .ok_or_else(|| {
                    let offered: Vec<&str> = usable.iter().map(|s| s.definition.as_str()).collect();
                    format!("档位 {quality} 不可用（可选：{}）", offered.join(" "))
                }),
        }
    }
}

/// 排序用的分辨率：显式高度优先，否则取分辨率标签开头的数字（`1080p` → 1080）。
fn stream_height_rank(s: &Stream) -> i64 {
    if s.height > 0 {
        return s.height as i64;
    }
    let digits: String = s
        .definition
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<i64>().unwrap_or(0)
}

/// 取字符串：字符串原样返回，数字转文本，其余空串。
fn flex_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn flex_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn flex_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 上游用 boolean / 数字 / 字符串 / 对象等多种写法表达真值。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().map(|x| x != 0).unwrap_or(true),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return false;
            }
            !matches!(t.to_ascii_lowercase().as_str(), "false" | "0")
        }
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn non_empty(v: Option<&Value>) -> Option<&Value> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::Array(a)) if a.is_empty() => None,
        Some(Value::Object(o)) if o.is_empty() => None,
        Some(other) => Some(other),
    }
}

fn first_of(a: String, b: String) -> String {
    if a.is_empty() {
        b
    } else {
        a
    }
}

/// `video_model` 可能是对象，也可能是「装了 JSON 的字符串」。
fn playback_object(raw: &Value) -> Result<Value, String> {
    // 最多解两层字符串，防止恶意/异常响应里无限嵌套（用 owned 链而非引用链，
    // 避免自引用生命周期问题）
    fn go(v: &Value, depth: u8) -> Result<Value, String> {
        if depth == 0 {
            return Err("播放模型嵌套过深".into());
        }
        match v {
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err("播放模型缺失".into());
                }
                let owned: Value =
                    serde_json::from_str(t).map_err(|e| format!("播放模型不是 JSON: {e}"))?;
                if owned.is_object() {
                    Ok(owned)
                } else {
                    go(&owned, depth - 1)
                }
            }
            Value::Object(_) => Ok(v.clone()),
            _ => Err("播放模型不是对象".into()),
        }
    }
    go(raw, 3)
}

/// 从 `video_meta` 或条目本身取字段。
fn pick(meta: &Value, item: &Value, key: &str) -> Value {
    if let Some(v) = non_empty(meta.get(key)) {
        return v.clone();
    }
    item.get(key).cloned().unwrap_or(Value::Null)
}

fn pick_str(meta: &Value, item: &Value, key: &str) -> String {
    flex_str(&pick(meta, item, key))
}

fn pick_i64(meta: &Value, item: &Value, key: &str) -> i64 {
    flex_i64(&pick(meta, item, key))
}

fn pick_i32(meta: &Value, item: &Value, key: &str) -> i32 {
    pick_i64(meta, item, key) as i32
}

/// 把一条码流条目转成 `Stream`。
fn item_to_stream(item: &Value) -> Stream {
    let meta = item.get("video_meta").cloned().unwrap_or(Value::Null);

    let encrypt_info = non_empty(item.get("encrypt_info"));
    let encryption_method = {
        let direct = flex_str(
            &item
                .get("encryption_method")
                .cloned()
                .unwrap_or(Value::Null),
        );
        if !direct.is_empty() {
            direct
        } else {
            encrypt_info
                .and_then(|e| e.get("encryption_method"))
                .map(flex_str)
                .unwrap_or_default()
        }
    };

    let gear_des_key = flex_str(&item.get("gear_des_key").cloned().unwrap_or(Value::Null));
    let spade_a = flex_str(&item.get("spade_a").cloned().unwrap_or(Value::Null));
    let kid = flex_str(&item.get("kid").cloned().unwrap_or(Value::Null));

    let protected = if let Some(info) = encrypt_info {
        if info.is_object() {
            let encrypt = info.get("encrypt").map(truthy).unwrap_or(false);
            let _method = info
                .get("encryption_method")
                .map(flex_str)
                .unwrap_or_default();
            let info_kid = info.get("kid").map(flex_str).unwrap_or_default();
            let info_spade = info.get("spade_a").map(flex_str).unwrap_or_default();
            encrypt
                || !encryption_method.is_empty()
                || !info_kid.is_empty()
                || !info_spade.is_empty()
        } else {
            true
        }
    } else {
        !gear_des_key.is_empty()
            || !spade_a.is_empty()
            || !kid.is_empty()
            || !encryption_method.is_empty()
            || truthy(&item.get("encrypt").cloned().unwrap_or(Value::Null))
            || non_empty(item.get("check_info")).is_some()
    };

    let mut definition = pick_str(&meta, item, "definition");
    let mut quality = pick_str(&meta, item, "quality");
    let quality_desc = flex_str(&item.get("quality_desc").cloned().unwrap_or(Value::Null));
    if quality.is_empty() && !quality_desc.is_empty() {
        quality = quality_desc;
    }
    let vtype = flex_str(&item.get("vtype").cloned().unwrap_or(Value::Null));
    let mut hls = truthy(&item.get("hls").cloned().unwrap_or(Value::Null))
        || !flex_str(&item.get("m3u8").cloned().unwrap_or(Value::Null)).is_empty();
    if definition.is_empty() && vtype.eq_ignore_ascii_case("m3u8") {
        hls = true;
    }
    if definition.is_empty() {
        definition = String::new();
    }

    let dash = pick_str(&meta, item, "vtype").eq_ignore_ascii_case("dash")
        || vtype.eq_ignore_ascii_case("dash");

    let raw_height = pick_i32(&meta, item, "vheight");
    let def_height = definition
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<i32>()
        .unwrap_or(0);
    let height = if raw_height > 0 {
        raw_height
    } else {
        def_height
    };

    Stream {
        definition,
        quality,
        codec: pick_str(&meta, item, "codec_type"),
        width: pick_i32(&meta, item, "vwidth"),
        height,
        bitrate: pick_i64(&meta, item, "bitrate"),
        size: pick_i64(&meta, item, "size"),
        url: flex_str(&item.get("main_url").cloned().unwrap_or(Value::Null)),
        backup_url: first_of(
            flex_str(&item.get("backup_url").cloned().unwrap_or(Value::Null)),
            flex_str(&item.get("backup_url_1").cloned().unwrap_or(Value::Null)),
        ),
        hls,
        dash,
        protected,
        encryption: encryption_method,
    }
}

/// `video_list` 可能是数组，也可能是按档位名索引的对象。
fn parse_items(raw: &Value) -> Vec<Value> {
    match raw {
        Value::Array(a) => a.clone(),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            keys.into_iter()
                .map(|k| {
                    let mut item = o[k].clone();
                    if let Value::Object(ref mut map) = item {
                        // 键名即档位：条目自己没有 definition 时用键名
                        let has = map.get("definition").map(|v| !flex_str(v).is_empty())
                            == Some(true)
                            || map
                                .get("video_meta")
                                .and_then(|m| m.get("definition"))
                                .map(|v| !flex_str(v).is_empty())
                                == Some(true);
                        if !has {
                            map.insert("definition".into(), Value::String(k.clone()));
                        }
                    }
                    item
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn wrapped_key(item: &Value) -> String {
    if let Some(info) = non_empty(item.get("encrypt_info")) {
        if let Some(k) = info.get("spade_a") {
            let s = flex_str(k);
            if !s.is_empty() {
                return s;
            }
        }
    }
    flex_str(&item.get("spade_a").cloned().unwrap_or(Value::Null))
}

/// 取 `fallback_api`。
///
/// 两种实测形态都要收：
/// - 对象里再套同名键（2026-11-09 抓包的实际形状）：
///   `{"fallback_api": {"fallback_api": "https://vas-lf-x.snssdk.com/…"}}`
/// - 直接给字符串：`{"fallback_api": "https://…"}`
///
/// 内层键名不稳定，除 `fallback_api` 外还见过 `url` / `fallback_url` 一类，
/// 所以按候选表逐个试，取第一个非空值。取不到就是空串——不要编一个地址出来。
fn fallback_api_of(body: &Value) -> String {
    let outer = body.get("fallback_api").cloned().unwrap_or(Value::Null);
    let direct = flex_str(&outer);
    if !direct.is_empty() {
        return direct;
    }
    let Some(obj) = outer.as_object() else {
        return String::new();
    };
    for key in ["fallback_api", "fallback_url", "url", "play_url"] {
        let value = flex_str(&obj.get(key).cloned().unwrap_or(Value::Null));
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

/// 解析 `video_model` 响应体。
pub fn parse_playback(raw: &[u8], episode_id: &str) -> Result<Playback, String> {
    let env: Value = serde_json::from_slice(raw).map_err(|e| format!("播放响应不是 JSON: {e}"))?;

    let code = flex_str(&env.get("code").cloned().unwrap_or(Value::Null));
    if !code.is_empty() && code != "0" {
        let msg = flex_str(&env.get("message").cloned().unwrap_or(Value::Null));
        return Err(format!("播放接口返回错误码 {code}：{msg}"));
    }

    let data = env
        .get("data")
        .ok_or_else(|| "响应里没有 data.video_model".to_string())?;
    let video_model = data
        .get("video_model")
        .or_else(|| {
            data.get(episode_id)
                .and_then(|entry| entry.get("video_model"))
        })
        .or_else(|| {
            data.as_object()
                .and_then(|map| map.values().find_map(|entry| entry.get("video_model")))
        })
        .cloned()
        .ok_or_else(|| "响应里没有 data.video_model".to_string())?;
    let body = playback_object(&video_model)?;
    playback_from_body(&body, episode_id)
}

/// 解析 `fallback_api` 返回的 `{"video_info":{"data":...}}`，并按 `key_seed` 解密播放地址。
pub fn parse_fallback_video_info(raw: &[u8], episode_id: &str) -> Result<Playback, String> {
    let env: Value = serde_json::from_slice(raw).map_err(|e| format!("备播响应不是 JSON: {e}"))?;
    let code = flex_str(&env.get("code").cloned().unwrap_or(Value::Null));
    if !code.is_empty() && code != "0" {
        let msg = flex_str(&env.get("message").cloned().unwrap_or(Value::Null));
        return Err(format!("备播接口返回错误码 {code}：{msg}"));
    }
    let body = env
        .get("video_info")
        .and_then(|info| info.get("data"))
        .cloned()
        .ok_or_else(|| "备播响应缺少 video_info.data".to_string())?;
    let mut pb = playback_from_body(&body, episode_id)?;
    let key_seed = flex_str(&body.get("key_seed").cloned().unwrap_or(Value::Null));
    if !key_seed.is_empty() {
        for s in &mut pb.streams {
            if let Ok(decoded) = crate::hongguo::drm::decrypt_spade_url(&s.url, &key_seed) {
                if !decoded.is_empty() {
                    s.url = decoded;
                }
            }
            if !s.backup_url.is_empty() {
                if let Ok(decoded) =
                    crate::hongguo::drm::decrypt_spade_url(&s.backup_url, &key_seed)
                {
                    if !decoded.is_empty() {
                        s.backup_url = decoded;
                    }
                }
            }
        }
        pb.streams
            .retain(|s| s.url.starts_with("http://") || s.url.starts_with("https://"));
        if pb.streams.is_empty() {
            return Err("备播响应地址解密失败".into());
        }
    }
    Ok(pb)
}

fn playback_from_body(body: &Value, episode_id: &str) -> Result<Playback, String> {
    let video_list = body.get("video_list").cloned().unwrap_or(Value::Null);
    let mut items = Vec::new();
    if let Some(list) = non_empty(Some(&video_list)) {
        items = parse_items(list);
    }

    let mut audio_url = String::new();
    if items.is_empty() {
        // 长片走 DASH：视频轨与音频轨分开下发，没有 video_list
        if let Some(dynamic) = body.get("dynamic_video").and_then(|d| non_empty(Some(d))) {
            if let Some(vl) = dynamic
                .get("dynamic_video_list")
                .and_then(|v| non_empty(Some(v)))
            {
                items = match vl {
                    Value::Array(a) => a.clone(),
                    _ => Vec::new(),
                };
            }
            // 内层先把类型收窄成切片，避免多层 if let（let-chain 需 Rust 2024）。
            if let Some(al) = dynamic
                .get("dynamic_audio_list")
                .and_then(|v| non_empty(Some(v)))
            {
                // `al` 在两个 if let 里各自使用（第二个只是判别类型），clippy
                // 建议的合并写法是 let-chain（Rust 2024 语法），这里保持分步。
                let first = match al {
                    Value::Array(a) => a.first(),
                    _ => None,
                };
                if let Some(first) = first {
                    audio_url = flex_str(&first.get("main_url").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    let fallback_api = fallback_api_of(body);

    if items.is_empty() {
        // 错误信息要能回答"接下来会怎样"。`video_model` 里有 `fallback_api` 时，
        // 说明这次失败**有备播可走**——上层 `fetch_stream_payload_with_fallback`
        // 会落到 worker，而 worker 的 `fetch_stream` 正是走
        // `multi_video_model → fallback_api → GET video_info.data` 这条老路
        // （见 `resources/shortdrama-worker/worker.py:943`）。
        //
        // 两条路的分工是刻意的：原生链路直读 `video_model.video_list[]` 的**明文**
        // `main_url`（实测 138ms）；worker 那条要再发一次 GET 并用 `key_seed`
        // 解密（实测 2.16s）。所以原生先试，失败才交给 worker。
        return Err(if fallback_api.is_empty() {
            "播放模型不可用（没有任何码流）".into()
        } else {
            "播放模型没有视频码流，但存在 fallback_api（将由 worker 兜底重取）".into()
        });
    }

    let mut out = Playback {
        episode_id: episode_id.to_string(),
        video_id: flex_str(&body.get("video_id").cloned().unwrap_or(Value::Null)),
        media_type: flex_str(&body.get("media_type").cloned().unwrap_or(Value::Null)),
        duration_seconds: (flex_f64(&body.get("video_duration").cloned().unwrap_or(Value::Null))
            + 0.5) as i32,
        expires_at: flex_i64(&body.get("url_expire").cloned().unwrap_or(Value::Null)),
        fallback_api,
        streams: Vec::new(),
        audio_url,
        protected: false,
        key_material: String::new(),
    };

    let mut protected_count = 0usize;
    for item in &items {
        let stream = item_to_stream(item);
        if stream.url.is_empty() {
            continue;
        }
        if stream.protected {
            protected_count += 1;
        }
        if out.key_material.is_empty() {
            out.key_material = wrapped_key(item);
        }
        out.streams.push(stream);
    }

    if out.streams.is_empty() {
        return Err("播放模型不可用（码流都没有地址）".into());
    }
    out.protected = protected_count == out.streams.len();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_video_model() {
        let raw = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_id\":\"v1\",\"video_duration\":209.4,\"video_list\":{\"1080p\":{\"main_url\":\"https://x/a.mp4\",\"vheight\":1080,\"bitrate\":628000,\"encrypt_info\":{\"encrypt\":true,\"encryption_method\":\"cenc-aes-ctr\",\"spade_a\":\"AAA\"}}}}"}
        }"#;
        let pb = parse_playback(raw, "ep1").unwrap();
        assert_eq!(pb.video_id, "v1");
        assert_eq!(pb.duration_seconds, 209);
        assert_eq!(pb.streams.len(), 1);
        assert_eq!(pb.streams[0].definition, "1080p");
        assert!(pb.streams[0].protected);
        assert_eq!(pb.key_material, "AAA");
        assert_eq!(pb.streams[0].height, 1080);
    }

    /// `fallback_api` 的**实测形状是对象里再套同名键**（2026-11-09 抓包）。
    ///
    /// 这个字段以前完全没解析：直链过期后唯一的回退是换设备，而换设备有配额
    /// （每台匿名设备只回一次播放模型，两处各限 2 次）。有了备播地址就不必烧配额。
    #[test]
    fn parses_nested_fallback_api_object() {
        let raw = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_id\":\"v1\",\"url_expire\":1794235182,\"fallback_api\":{\"fallback_api\":\"https://vas-lf-x.snssdk.com/video/fplay/1/abc/v03ebeg10000?aid=8662\"},\"video_list\":[{\"main_url\":\"https://x/a.mp4\",\"vheight\":1080}]}"}
        }"#;
        let pb = parse_playback(raw, "ep1").unwrap();
        assert_eq!(
            pb.fallback_api,
            "https://vas-lf-x.snssdk.com/video/fplay/1/abc/v03ebeg10000?aid=8662"
        );
        assert_eq!(pb.expires_at, 1794235182);
    }

    /// 也见过直接给字符串的形态，同样要收。
    #[test]
    fn parses_flat_fallback_api_string() {
        let raw = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_list\":[{\"main_url\":\"https://x/a.mp4\"}],\"fallback_api\":\"https://fallback.example/fplay/1/xyz\"}"}
        }"#;
        let pb = parse_playback(raw, "ep1").unwrap();
        assert_eq!(pb.fallback_api, "https://fallback.example/fplay/1/xyz");
    }

    /// 没有这个字段时必须是**空串**，不能编一个地址出来冒充可回退。
    #[test]
    fn missing_fallback_api_stays_empty() {
        let raw = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_list\":[{\"main_url\":\"https://x/a.mp4\"}]}"}
        }"#;
        let pb = parse_playback(raw, "ep1").unwrap();
        assert!(pb.fallback_api.is_empty());
    }

    /// `fallback_api` 才有的形态：`video_model` 里没有 `video_list`。
    ///
    /// 这正是 worker 兜底那条路的入口（`worker.py:943` 的
    /// `multi_video_model → fallback_api → GET video_info.data`）。错误信息必须
    /// 能区分"彻底没码流"与"没码流但有备播可用" —— 前者是死路，后者交给 worker
    /// 还会成功。两句话在日志里长得一样就没法排查。
    #[test]
    fn empty_video_list_reports_fallback_availability() {
        let with_fallback = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_id\":\"v1\",\"fallback_api\":\"https://vas-lf-x.snssdk.com/video/fplay/1/abc\"}"}
        }"#;
        let err = parse_playback(with_fallback, "ep1").unwrap_err();
        assert!(err.contains("fallback_api"), "{err}");

        let without = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_id\":\"v1\"}"}
        }"#;
        let err = parse_playback(without, "ep1").unwrap_err();
        assert!(!err.contains("fallback_api"), "{err}");
        assert!(err.contains("没有任何码流"), "{err}");
    }

    /// **明文直链不能被判成受保护**（2026-11-09 运行时抓包实证）。
    ///
    /// 早期审计的结论是「`main_url` 是密文，需 `gear_des_key` + `spade_a` 解密，
    /// HLS 分片走 `cenc-aes-ctr`」。抓包推翻了这个结论：`video_model` 的
    /// `video_list[]` 里根本没有 `encrypt_info` / `gear_des_key` / `spade_a`，
    /// `main_url` 就是一条明文 HTTPS 直链（`media_type=video`、`enable_ssl=true`、
    /// `url_expire` 六小时后）。
    ///
    /// 这条测试锁的是**判定不能比证据更宽**：真出现上述任一加密字段才算加密。
    /// 判宽了会让播放白白走 `drm::derive` 解包，拿一个不存在的 key 去解密一条
    /// 明文流，结果是整集播不出来——而且现场看到的现象和"链接失效"一模一样。
    #[test]
    fn plaintext_main_url_is_not_flagged_protected() {
        let raw = br#"{
            "code": 0,
            "data": {"video_model": "{\"video_id\":\"v1\",\"media_type\":\"video\",\"enable_ssl\":true,\"version\":3,\"url_expire\":1794235182,\"video_list\":[{\"main_url\":\"https://v3-web.douyinvod.com/abc/video.mp4?sign=x\",\"vheight\":1080,\"vwidth\":1920,\"bitrate\":1200000}]}"}
        }"#;
        let pb = parse_playback(raw, "ep1").unwrap();
        assert_eq!(pb.streams.len(), 1);
        assert!(!pb.protected, "明文直链整体不该标记受保护");
        assert!(!pb.streams[0].protected, "单路码流同理");
        assert!(pb.key_material.is_empty(), "没有 spade_a 就不该有密钥材料");
        assert!(pb.streams[0]
            .url
            .starts_with("https://v3-web.douyinvod.com/"));
        assert_eq!(pb.media_type, "video");
        assert_eq!(pb.expires_at, 1794235182);
    }

    #[test]
    fn error_code_surfaces() {
        let raw = r#"{"code": 101000, "message": "服务异常"}"#.as_bytes();
        let err = parse_playback(raw, "ep1").unwrap_err();
        assert!(err.contains("101000"), "{err}");
    }

    #[test]
    fn empty_body_is_reported_as_anonymous_limited_shape() {
        // 空 body 在 client 层会被识别为静默拒绝，这里只保证解析器不 panic
        assert!(parse_playback(b"", "ep1").is_err());
    }

    #[test]
    fn parses_multi_video_model_envelope() {
        let raw = br#"{
            "code": 0,
            "data": {
                "7708275438275005502": {
                    "video_model": {
                        "video_id": "v_multi",
                        "video_duration": 120.0,
                        "video_list": [
                            {
                                "main_url": "https://cdn.example/1080.mp4",
                                "vheight": 1080,
                                "vwidth": 1920,
                                "bitrate": 800000,
                                "encrypt_info": {
                                    "encrypt": true,
                                    "encryption_method": "cenc-aes-ctr",
                                    "spade_a": "AAA"
                                }
                            }
                        ]
                    }
                }
            }
        }"#;
        let pb = parse_playback(raw, "7708275438275005502").unwrap();
        assert_eq!(pb.video_id, "v_multi");
        assert_eq!(pb.duration_seconds, 120);
        assert_eq!(pb.streams.len(), 1);
        assert!(pb.protected);
        assert_eq!(pb.key_material, "AAA");
    }

    fn stream(definition: &str, height: i32, bitrate: i64, url: &str) -> Stream {
        Stream {
            definition: definition.into(),
            height,
            bitrate,
            url: url.into(),
            ..Default::default()
        }
    }

    fn playback(streams: Vec<Stream>) -> Playback {
        Playback {
            streams,
            ..Default::default()
        }
    }

    #[test]
    fn select_defaults_to_highest_resolution_then_bitrate() {
        let pb = playback(vec![
            stream("720p", 720, 900_000, "u720"),
            stream("1080p", 1080, 1_200_000, "u1080a"),
            stream("1080p", 1080, 2_000_000, "u1080b"),
        ]);
        assert_eq!(pb.select("").unwrap().url, "u1080b");
        assert_eq!(pb.select("best").unwrap().url, "u1080b");
    }

    #[test]
    fn select_worst_picks_lowest_resolution() {
        let pb = playback(vec![
            stream("1080p", 1080, 2_000_000, "u1080"),
            stream("480p", 480, 400_000, "u480"),
        ]);
        assert_eq!(pb.select("worst").unwrap().url, "u480");
    }

    #[test]
    fn select_matches_definition_case_insensitively() {
        let pb = playback(vec![
            stream("720p", 720, 900_000, "u720"),
            stream("1080p", 1080, 2_000_000, "u1080"),
        ]);
        assert_eq!(pb.select("720P").unwrap().url, "u720");
    }

    #[test]
    fn select_skips_streams_without_url() {
        let pb = playback(vec![
            stream("1080p", 1080, 2_000_000, ""),
            stream("720p", 720, 900_000, "u720"),
        ]);
        assert_eq!(pb.select("").unwrap().url, "u720");
    }

    #[test]
    fn select_unknown_quality_lists_available() {
        let pb = playback(vec![stream("720p", 720, 900_000, "u720")]);
        let err = pb.select("4k").unwrap_err();
        assert!(err.contains("4k") && err.contains("720p"), "{err}");
    }

    #[test]
    fn select_errors_when_no_stream_has_url() {
        let pb = playback(vec![stream("720p", 720, 900_000, "  ")]);
        assert!(pb.select("").is_err());
    }
}
