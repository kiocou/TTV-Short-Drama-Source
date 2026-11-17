//! `video_model` 专用的真实 Argus/Ladon 签名。
//!
//! 对应 openhongguo `bed14595` 的 `internal/protocol/sign/playback.go`。
//! Argus 551 = protobuf bean → Simon 64/128 → 掩码翻转帧 → 标准 AES-128-CBC；
//! Ladon 是以 `md5(nonce || aid)` 派生轮密钥的 34 轮 ARX。标准 AES 复用
//! `tt_encrypt` 中已有 NIST 向量锁定的实现，不引入新依赖。

use base64::Engine;

use crate::hongguo::medusa::Entropy;
use crate::hongguo::prim::md5;
use crate::hongguo::proto::{proto_key, varint};
use crate::hongguo::sign::{sign_request, Pairs, SignedRequest};
use crate::hongguo::sm3::{hex_lower, sm3};
use crate::hongguo::tt_encrypt::aes128_cbc_encrypt;

const PLAYBACK_LICENSE_ID: &str = "1611921764";
const ARGUS_MASK: [u8; 8] = [0xf2, 0xf7, 0xfc, 0xff, 0xf2, 0xf7, 0xfc, 0xff];
const ARGUS_TAIL: [u8; 9] = [0xea, 0xfb, 0x2c, 0xfe, 0x85, 0x68, 0x51, 0x91, 0x54];

// 三个常量在编译期解出字节。原来走运行期 hex 解码 + `expect`：常量是写死的，
// 那条失败路径不可达，却把整条播放链路挂在一次 panic 上。`hex_const` 对非法
// 字符给 0，只可能在编译期暴露。
const ARGUS_SIGN_KEY: [u8; 32] =
    hex_const("fc78e0a9657a0c748ce51559903ccf03510e51d3cff232d71343e88a321c5304");
const ARGUS_AES_KEY: [u8; 16] = hex_const("d31e3718288a1027baab59f146a09a9c");
const ARGUS_AES_IV: [u8; 16] = hex_const("ea180a0336ed352fcd24e4d50018ae54");

const fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

/// 编译期 hex 解码；`N` 必须等于 `src` 长度的一半。
const fn hex_const<const N: usize>(src: &str) -> [u8; N] {
    let bytes = src.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N && i * 2 + 1 < bytes.len() {
        out[i] = (hex_nibble(bytes[i * 2]) << 4) | hex_nibble(bytes[i * 2 + 1]);
        i += 1;
    }
    out
}

/// 在普通 POST 签名上替换真实 Argus/Ladon，并去掉播放接口不接受的 Medusa。
pub fn sign_playback_post(
    path: &str,
    pairs: &Pairs,
    now_millis: i64,
    device_id: &str,
    body: &[u8],
    entropy: Entropy,
) -> SignedRequest {
    let mut signed = sign_request(path, pairs, now_millis, device_id, Some(body), entropy);
    sign_playback_request(&mut signed, body);
    signed
}

/// 用固定 nonce 签名，供上游黄金向量及请求形态测试使用。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn sign_playback_post_with_nonce(
    path: &str,
    pairs: &Pairs,
    now_millis: i64,
    device_id: &str,
    body: &[u8],
    entropy: Entropy,
    argus_nonce: u32,
    ladon_nonce: u32,
) -> SignedRequest {
    let mut signed = sign_request(path, pairs, now_millis, device_id, Some(body), entropy);
    let ts = khronos(&signed);
    let query = signed.query.clone();
    let aid = playback_aid(&query);
    set_header(
        &mut signed,
        "x-argus",
        argus_header(&query, body, ts, argus_nonce),
    );
    set_header(&mut signed, "x-ladon", ladon_header(ts, ladon_nonce, &aid));
    signed.headers.retain(|(key, _)| key != "x-medusa");
    signed
}

fn sign_playback_request(signed: &mut SignedRequest, body: &[u8]) {
    let ts = khronos(signed);
    let query = signed.query.clone();
    let aid = playback_aid(&query);
    let argus = argus_header(&query, body, ts, random_u32());
    let ladon = ladon_header(ts, random_u32(), &aid);
    set_header(signed, "x-argus", argus);
    set_header(signed, "x-ladon", ladon);
    signed.headers.retain(|(key, _)| key != "x-medusa");
}

fn header<'a>(signed: &'a SignedRequest, name: &str) -> &'a str {
    signed
        .headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

fn set_header(signed: &mut SignedRequest, name: &str, value: String) {
    if let Some((_, current)) = signed.headers.iter_mut().find(|(key, _)| key == name) {
        *current = value;
    }
}

/// `sign_request` 实际写进查询串的时间戳（秒）。
///
/// `sign_request` 会把 `khronos` 往上抬几毫秒去挑 F13 分支，所以只能读回它
/// 真正用的那个值。解析失败时退回 `_rticket` 的秒值 —— 两者在正常路径上恒等，
/// 这样签名函数不会因为一个 header 拼装失误而 panic。
fn khronos(signed: &SignedRequest) -> u32 {
    header(signed, "x-khronos")
        .parse::<u32>()
        .unwrap_or_else(|_| u32::try_from(signed.ticket_millis / 1000).unwrap_or_default())
}

/// 取 `chunk` 里从 `at` 起的 8 字节小端整数；越界给 0。
/// 调用点都是 16 字节的 `chunks_exact`/固定常量，越界不可达。
fn le_u64(chunk: &[u8], at: usize) -> u64 {
    let mut buf = [0u8; 8];
    if let Some(bytes) = chunk.get(at..at + 8) {
        buf.copy_from_slice(bytes);
    }
    u64::from_le_bytes(buf)
}

fn random_u32() -> u32 {
    let id = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&id.as_bytes()[..4]);
    u32::from_le_bytes(bytes)
}

fn playback_aid(query: &str) -> String {
    query_value(query, "aid").unwrap_or_else(|| "1967".into())
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

pub(crate) fn argus_header(query: &str, body: &[u8], ts: u32, nonce: u32) -> String {
    let aid = playback_aid(query);
    let device_id = query_value(query, "device_id").unwrap_or_default();
    let install_id = query_value(query, "iid").unwrap_or_default();
    argus_header_with_identity(query, body, ts, nonce, &aid, &device_id, &install_id)
}

pub(crate) fn argus_header_with_identity(
    query: &str,
    body: &[u8],
    ts: u32,
    nonce: u32,
    aid: &str,
    device_id: &str,
    install_id: &str,
) -> String {
    let body_md5 = if body.is_empty() { [0; 16] } else { md5(body) };
    let body_hash = sm3(&body_md5);
    let query_hash = if query.is_empty() {
        sm3(&[0; 16])
    } else {
        sm3(query.as_bytes())
    };

    let mut counters = Vec::new();
    counters.extend(pb_varint(1, 1));
    counters.extend(pb_varint(2, 1));
    counters.extend(pb_varint(3, 1));
    counters.extend(pb_varint(7, 0xc792eccc));
    let mut device = Vec::new();
    device.extend(pb_string(1, "NX551J"));
    device.extend(pb_varint(2, 8196));
    device.extend(pb_varint(4, 0x80e0d800));

    let mut bean = Vec::new();
    bean.extend(pb_varint(1, 1077940818));
    bean.extend(pb_varint(2, 2));
    bean.extend(pb_varint(3, nonce as u64));
    bean.extend(pb_string(4, aid));
    bean.extend(pb_string(5, device_id));
    bean.extend(pb_string(6, PLAYBACK_LICENSE_ID));
    bean.extend(pb_string(
        7,
        &query_value(query, "version_name").unwrap_or_default(),
    ));
    bean.extend(pb_string(8, "v04.04.05-ov-android"));
    bean.extend(pb_varint(9, 134744640));
    bean.extend(pb_bytes(10, &[0; 8]));
    bean.extend(pb_varint(11, 0));
    bean.extend(pb_varint(12, (ts as u64) << 1));
    bean.extend(pb_bytes(13, &body_hash[..6]));
    bean.extend(pb_bytes(14, &query_hash[..6]));
    bean.extend(pb_bytes(15, &counters));
    bean.extend(pb_string(16, install_id));
    bean.extend(pb_string(20, "none"));
    bean.extend(pb_varint(21, 738));
    bean.extend(pb_bytes(23, &device));
    bean.extend(pb_varint(25, 2));

    let mut plain = pkcs7(&bean);
    simon_encrypt(&mut plain);
    let mut frame = Vec::with_capacity(ARGUS_MASK.len() + plain.len() + ARGUS_TAIL.len());
    frame.extend_from_slice(&ARGUS_MASK);
    frame.extend_from_slice(&plain);
    frame.extend_from_slice(&ARGUS_TAIL);
    for (i, byte) in frame.iter_mut().enumerate().skip(ARGUS_MASK.len()) {
        *byte ^= ARGUS_MASK[i % ARGUS_MASK.len()];
    }
    frame.reverse();
    frame.extend_from_slice(b"ao");
    let encrypted = aes128_cbc_encrypt(&ARGUS_AES_KEY, &ARGUS_AES_IV, &pkcs7(&frame));
    base64::engine::general_purpose::STANDARD.encode([&[0xf2, 0x81], encrypted.as_slice()].concat())
}

pub(crate) fn ladon_header(ts: u32, nonce: u32, aid: &str) -> String {
    let seed = nonce.to_le_bytes();
    let mut digest_input = seed.to_vec();
    digest_input.extend_from_slice(aid.as_bytes());
    let hex_key = hex_lower(&md5(&digest_input)).into_bytes();

    let mut keys = [0u64; 34];
    let mut k = le_u64(&hex_key, 0);
    let mut pending = [
        le_u64(&hex_key, 8),
        le_u64(&hex_key, 16),
        le_u64(&hex_key, 24),
    ];
    for (i, key) in keys.iter_mut().enumerate() {
        *key = k;
        let x = pending[0].rotate_right(8).wrapping_add(k) ^ i as u64;
        pending = [pending[1], pending[2], x];
        k = x ^ k.rotate_left(3);
    }

    let mut plain = pkcs7(format!("{ts}-{PLAYBACK_LICENSE_ID}-{aid}").as_bytes());
    for chunk in plain.as_chunks_mut::<16>().0 {
        let mut a = le_u64(chunk, 0);
        let mut b = le_u64(chunk, 8);
        for key in keys {
            b = key ^ a.wrapping_add(b.rotate_right(8));
            a = b ^ a.rotate_left(3);
        }
        chunk[..8].copy_from_slice(&a.to_le_bytes());
        chunk[8..].copy_from_slice(&b.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode([seed.as_slice(), plain.as_slice()].concat())
}

fn simon_encrypt(plain: &mut [u8]) {
    let keys = simon_key_schedule();
    for chunk in plain.as_chunks_mut::<16>().0 {
        let mut a = le_u64(chunk, 0);
        let mut b = le_u64(chunk, 8);
        for key in keys {
            (a, b) = (
                b,
                a ^ (b.rotate_left(1) & b.rotate_left(8)) ^ b.rotate_left(2) ^ key,
            );
        }
        chunk[..8].copy_from_slice(&a.to_le_bytes());
        chunk[8..].copy_from_slice(&b.to_le_bytes());
    }
}

fn simon_key_schedule() -> [u64; 72] {
    let mut keys = [0u64; 72];
    for (i, key) in keys.iter_mut().take(4).enumerate() {
        *key = le_u64(&ARGUS_SIGN_KEY, i * 8);
    }
    const Z: u64 = 0x3dc94c3a046d678b;
    for i in 4..keys.len() {
        let mut x = keys[i - 1].rotate_right(3) ^ keys[i - 3];
        x ^= x.rotate_right(1);
        keys[i] = !keys[i - 4] ^ x ^ ((Z >> ((i - 4) % 62)) & 1) ^ 3;
    }
    keys
}

fn pkcs7(data: &[u8]) -> Vec<u8> {
    let padding = 16 - data.len() % 16;
    let mut out = data.to_vec();
    out.resize(out.len() + padding, padding as u8);
    out
}

fn pb_varint(tag: u32, value: u64) -> Vec<u8> {
    [proto_key(tag, 0), varint(value)].concat()
}

fn pb_bytes(tag: u32, body: &[u8]) -> Vec<u8> {
    [proto_key(tag, 2), varint(body.len() as u64), body.to_vec()].concat()
}

fn pb_string(tag: u32, value: &str) -> Vec<u8> {
    pb_bytes(tag, value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argus_empty_query_matches_upstream() {
        assert_eq!(
            argus_header("", b"", 1_700_000_000, 0),
            "8oHyrVYEy1xDCDlx64ko40qC6JC9VYY/ZkjJzEl50iSFDw8Yfph1hupLxfKrA9GswSEz0Yi4goA7OQ4u+H8ECdOaHRbHCQV+QFNgNz4TTRpioctLgrN97GbIKIQgZJjNRNiwbGqa+jIbLonncbTbsGIMv2WphwNMhXazKSewxik7/ecf0/SUOIiVd/3LhLMGeCIy+/ngOQr9VXnvmCb8HyzUbr01+pgwc1OKSfU29+FP4vYSq2WdmLCpvopUNQZhB8U="
        );
    }

    #[test]
    fn argus_explicit_install_id_matches_upstream() {
        // 上游 zfdang 向量刻意让 query 的 iid=111 与 bean 的空 install id 不同。
        assert_eq!(
            argus_header_with_identity(
                "aid=1967&device_id=dev_aaaaaaaa&iid=111",
                b"",
                1_700_000_000,
                659662259,
                "1967",
                "dev_aaaaaaaa",
                "",
            ),
            "8oHyrVYEy1xDCDlx64ko40qC6JC9VYY/ZkjJzEl50iSFDw8Yfph1hupLxfKrA9GswSEz0Yi4goA7OQ4u+H8ECdOaQDL4GduQ3LULSq9Q4iSEGbS3L6G24GXChtnmPr6n46M2PLlKpXvF7uE0lhH4yD+X69FpdM41gQwx5Y5U3NdwXFaxlqYFszyp7dlUE6BEBWqwDsiAH8Jk9TGKr/jfHt4fVmMc9zRyNRdGeV0x6aP//x7aV+8Po8a/9KK3UNG5KEpmIgzRT6NUttkBP9gRNUh3"
        );
    }

    #[test]
    fn argus_binds_query_body_timestamp_and_nonce() {
        let query = "aid=8662&device_id=7000000000000000001&iid=7000000000000000002&version_name=7.3.9.32&ts=1700000000";
        let body = br#"{"video_id":"7543196048185887806"}"#;
        let base = argus_header(query, body, 1_700_000_000, 7);
        assert_ne!(
            argus_header(&format!("{query}&extra=1"), body, 1_700_000_000, 7),
            base
        );
        assert_ne!(
            argus_header(query, &[body.as_slice(), b" "].concat(), 1_700_000_000, 7),
            base
        );
        assert_ne!(argus_header(query, body, 1_700_000_001, 7), base);
        assert_ne!(argus_header(query, body, 1_700_000_000, 8), base);
    }

    #[test]
    fn production_playback_headers_have_real_shape() {
        let signed = sign_playback_post(
            "/novel/player/video_model/v1/",
            &vec![
                ("aid".into(), "8662".into()),
                ("device_id".into(), "7000000000000000001".into()),
                ("iid".into(), "7000000000000000002".into()),
                ("version_name".into(), "7.3.9.32".into()),
            ],
            1_700_000_000_123,
            "7000000000000000001",
            br#"{"req_type":1}"#,
            Entropy {
                now_secs: 1_700_000_000,
                gorgon_random: 0x1234,
                helios_random: 1,
                message_random: 1,
                random: 1,
                packed_random: 1,
                env_launch: 100,
                env_pid: 10001,
            },
        );
        assert!(!signed.headers.iter().any(|(key, _)| key == "x-medusa"));
        for name in [
            "x-argus",
            "x-ladon",
            "x-gorgon",
            "x-helios",
            "x-khronos",
            "x-ss-stub",
        ] {
            assert!(!header(&signed, name).is_empty(), "缺少 {name}");
        }
        assert!(header(&signed, "x-argus").len() >= 64);
        assert!(header(&signed, "x-argus").starts_with("8o"));
    }
}
