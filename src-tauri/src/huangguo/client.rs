//! 边缘信封客户端。
//!
//! 站点把所有 `/api/**` 收成一条 `POST {origin}/api/edge/request`，真实请求装在
//! 信封里：`{method, path, body: base64(JSON), content_type}`。响应恒为
//! `{status, data, msg}`——`status == 1` 才算成功，业务载荷在 `data` 里。
//!
//! 只发 `Accept` 与浏览器 UA：实测封面与 m3u8 都不需要 Referer，playback 下发的
//! `x-cdn-auth` 也不是必需的（三种请求返回字节完全相同，见 `super` 头注释）。

use base64::Engine;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_ORIGIN: &str = "https://huangguoai.com";

/// 线路 origin。
///
/// 上游域名会轮换（它自己有一整套地址发布页机制），所以留一个环境变量口子给
/// 排障用——与 `TTV_ANIME_SOURCE` 同一套惯例。正常使用时不需要设置。
pub fn origin() -> &'static str {
    static ORIGIN: OnceLock<String> = OnceLock::new();
    ORIGIN.get_or_init(|| {
        std::env::var("TTV_HUANGGUO_ORIGIN")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| value.starts_with("https://"))
            .unwrap_or_else(|| DEFAULT_ORIGIN.to_string())
    })
}

/// 业务链路的公共构造器。
fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126 Safari/537.36",
        )
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(300))
        .pool_max_idle_per_host(16)
        .tcp_nodelay(true)
}

/// 主客户端：**只挂系统代理，启动期零探测**。
///
/// ⚠️ 不要改回 `with_local_proxy_fallback`（2026-11-11 修「封面偶发全空」）。
/// 那个函数会当场探测本机代理端口，而本 provider 是在 `setup` 里构造的——
/// 于是「应用启动那一刻代理有没有起来」被烤进整个进程：开机自启跑在 Clash
/// 之前、或用户中途换了代理端口，这一场会话就再也出不去网，重启才好。
/// 探测必须推迟到**真的失败之后**（见 `HuangguoProvider::fallback_client`）。
///
/// 直连 `huangguoai.com` 实测 21 秒超时，所以代理是必需的；系统代理开着时
/// 这条路就够用，没开时由兜底客户端接住。
pub fn build_client() -> Result<reqwest::Client, String> {
    crate::update::with_system_proxy(builder())
        .build()
        .map_err(|error| error.to_string())
}

/// 兜底客户端：用**此刻**探测到的本机代理。探不到返回 `None`。
///
/// 返回 `Option` 而不是「无代理的客户端」是刻意的：调用方据此决定要不要缓存。
/// 探不到时不缓存，下一次失败会再探一遍（探测侧有 30 秒冷却），代理恢复后能自愈。
pub fn fallback_client() -> Option<reqwest::Client> {
    let address = crate::update::detect_local_http_proxy()?;
    let proxy = reqwest::Proxy::all(format!("http://{address}")).ok()?;
    builder().proxy(proxy).build().ok()
}

/// 发一次业务请求，返回 `data`。
///
/// GET 失败重试一次：这条链路要过代理，传输层抖动是常态，而目录/详情都是幂等的。
/// POST（playback）不重试——它在站点侧会计一次播放，重发等于多计一次。
pub async fn edge(
    client: &reqwest::Client,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let (encoded, content_type) = match &body {
        Some(value) => (
            base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(value).map_err(|error| error.to_string())?),
            "application/json",
        ),
        None => (String::new(), ""),
    };
    let envelope = json!({
        "method": method,
        "path": path,
        "body": encoded,
        "content_type": content_type,
    });
    let url = format!("{}/api/edge/request", origin());
    let idempotent = method.eq_ignore_ascii_case("GET") && body.is_none();

    let mut last = String::from("黄果请求未发出");
    for attempt in 0..if idempotent { 2 } else { 1 } {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        match send_once(client, &url, &envelope).await {
            Ok(value) => return Ok(value),
            Err(error) => last = error,
        }
    }
    Err(last)
}

async fn send_once(client: &reqwest::Client, url: &str, envelope: &Value) -> Result<Value, String> {
    // reqwest 没开 json feature（见 Cargo.toml），请求体与响应都自己处理字节。
    let payload = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    let response = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .map_err(|error| format!("黄果线路连接失败：{error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("黄果响应读取失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("黄果线路 HTTP {status}"));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "黄果响应不是 JSON：{}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(120)
                .collect::<String>()
        )
    })?;
    if value.get("status").and_then(Value::as_i64) == Some(1) {
        return Ok(value.get("data").cloned().unwrap_or(Value::Null));
    }
    Err(value
        .get("msg")
        .and_then(Value::as_str)
        .filter(|msg| !msg.trim().is_empty())
        .unwrap_or("黄果接口返回失败")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_defaults_to_the_fixed_line_and_rejects_plain_http() {
        // 环境变量是进程级的，这里只验默认值与「必须是 https」这条规则本身。
        assert!(DEFAULT_ORIGIN.starts_with("https://"));
        assert!(!DEFAULT_ORIGIN.ends_with('/'));
    }

    #[test]
    fn envelope_body_is_base64_of_the_json_payload() {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&json!({})).unwrap());
        // playback 的空 body 是 `{}` 而不是空串：站点按 JSON 解析。
        assert_eq!(encoded, "e30=");
    }
}
