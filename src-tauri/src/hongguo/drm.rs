//! CENC 内容密钥解包。
//!
//! 移植自 `internal/drm/spade.go`。服务端为每一路码流下发一段不透明的
//! `encrypt_info.spade_a`（base64）。播放器原生库把它按固定置换翻搅后拆成
//! 「版本号 + 内容密钥 + 方案标记」。这里做的是行为兼容的复刻 —— 纯本地计算，
//! 不发任何请求。
//!
//! 它决定播放能否解密：解包错了 ffmpeg 只会报一堆乱码帧，而不是「密钥错误」，
//! 所以必须严格照抄每一处 `wrapping_*`。

/// 一段可用的内容密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentKey {
    /// AES 内容密钥（小写 hex）。
    pub hex: String,
    /// 玩家读取的方案标记（如 `1`）。
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMaterial;

impl std::fmt::Display for InvalidMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "无法识别的密钥材料")
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `popcount` 对 index 的 4 个字节分别计数后相加（对齐播放器的 NEON 实现）。
fn popcount_index(i: usize) -> u8 {
    // Go 侧写的是「四个字节分别 popcount 再相加」，等价于整体 count_ones()。
    // 别真的去逐字节累加 —— 那会把同一批比特数四遍。
    (i as u32).count_ones() as u8
}

fn scramble(buf: &mut [u8], v2: bool) {
    if v2 {
        let last = buf.len().saturating_sub(1);
        let mut i = 0;
        while i + 1 < buf.len() {
            buf.swap(i, i + 1);
            if i == last.saturating_sub(2) {
                break;
            }
            i += 2;
        }
    }

    let mut prev: u8 = 0x55;
    let mut cur: u8 = 0xfa;
    for (i, slot) in buf.iter_mut().enumerate() {
        let b = *slot;
        // Go: term := -byte(0x15 + popcount(uint32(i)))
        let term = (0x15u8).wrapping_add(popcount_index(i)).wrapping_neg();
        if i % 2 == 0 {
            *slot = if v2 {
                (cur ^ b).wrapping_sub(prev).wrapping_add(term)
            } else {
                term.wrapping_add(cur ^ b)
            };
            cur = b;
        } else {
            *slot = if v2 {
                (prev ^ b).wrapping_sub(prev).wrapping_add(term)
            } else {
                term.wrapping_add(prev ^ b)
            };
            prev = b;
        }
    }
}

/// 从 `spade_a` 解出内容密钥。
pub fn derive(spade_a: &str) -> Result<ContentKey, InvalidMaterial> {
    use base64::Engine;
    let trimmed = spade_a.trim();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(trimmed.as_bytes())
        .map_err(|_| InvalidMaterial)?;
    if raw.len() < 4 {
        return Err(InvalidMaterial);
    }

    let marker = (raw[0] ^ raw[1] ^ raw[2]) as i32;
    let prefix_len = marker - 0x30;
    if prefix_len < 1 || prefix_len as usize > raw.len() {
        return Err(InvalidMaterial);
    }
    let out_len = raw.len() as i32 - marker + 0x2f;
    if out_len < 2 || (1 + out_len) as usize > raw.len() {
        return Err(InvalidMaterial);
    }
    let out_len = out_len as usize;

    let mut buf = raw[1..1 + out_len].to_vec();

    let tail = raw.len() as i32 - prefix_len;
    if tail - 2 < 0 || tail as usize > raw.len() {
        return Err(InvalidMaterial);
    }
    let tail = tail as usize;
    let (b16, b14) = (raw[tail - 2], raw[tail - 1]);

    let mut tag = vec![0u8; prefix_len as usize];
    for (i, slot) in tag.iter_mut().enumerate() {
        if i + tail >= raw.len() {
            return Err(InvalidMaterial);
        }
        *slot = b14 ^ b16 ^ raw[i + tail];
    }
    let tag_text = String::from_utf8_lossy(&tag).to_string();
    let v2 = tag_text.starts_with("app_v2") || tag_text.starts_with("web_v2");

    scramble(&mut buf, v2);

    let version_len = hex_digit(buf[0]).ok_or(InvalidMaterial)? as usize;
    if version_len < 1 || version_len >= out_len {
        return Err(InvalidMaterial);
    }
    let key_len = out_len - version_len;
    if key_len < 1 || key_len > buf.len() {
        return Err(InvalidMaterial);
    }
    let hex_key = String::from_utf8_lossy(&buf[1..key_len]).to_lowercase();
    if hex_key.is_empty() || !hex_key.len().is_multiple_of(2) {
        return Err(InvalidMaterial);
    }
    if !hex_key.bytes().all(|c| hex_digit(c).is_some()) {
        return Err(InvalidMaterial);
    }

    Ok(ContentKey {
        hex: hex_key,
        version: String::from_utf8_lossy(&buf[key_len..]).to_string(),
    })
}

const SPADE_CONSTANTS: [u8; 64] = [
    0x4D, 0xD4, 0xC2, 0xE6, 0xB8, 0x31, 0x62, 0x09, 0x0E, 0x52, 0xB3, 0xC7, 0xA6, 0x73, 0x3B, 0xA4,
    0x1C, 0xB2, 0x46, 0x2B, 0x82, 0x9A, 0xB5, 0x8A, 0x19, 0x6B, 0x39, 0xDB, 0x57, 0x17, 0x75, 0x24,
    0xF4, 0x9B, 0xAF, 0x7F, 0x08, 0xE8, 0xD6, 0x8D, 0x26, 0xA7, 0x2E, 0x37, 0xC1, 0xA9, 0x5A, 0x2F,
    0x1F, 0x05, 0xA5, 0x18, 0x92, 0xAE, 0xF2, 0x94, 0x97, 0x32, 0xB6, 0x2A, 0x38, 0xAA, 0xDD, 0x58,
];

fn decode_b64_flexible(input: &str) -> Result<Vec<u8>, InvalidMaterial> {
    use base64::Engine;
    let trimmed = input.trim();
    let pad = (4 - (trimmed.len() % 4)) % 4;
    let padded = if pad > 0 {
        format!("{trimmed}{}", "=".repeat(pad))
    } else {
        trimmed.to_owned()
    };
    base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(padded.as_bytes()))
        .map_err(|_| InvalidMaterial)
}

/// 解密 `fallback_api` 返回的 `key_seed` + `spade` 加密播放地址。
pub fn decrypt_spade_url(cipher_b64: &str, key_seed_b64: &str) -> Result<String, InvalidMaterial> {
    use sha2::{Digest, Sha512};
    let trimmed = cipher_b64.trim();
    if trimmed.is_empty() || trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(trimmed.to_owned());
    }
    let key_seed = decode_b64_flexible(key_seed_b64)?;
    if key_seed.is_empty() {
        return Err(InvalidMaterial);
    }
    let raw = decode_b64_flexible(trimmed)?;
    if raw.len() < 20 || raw[0] != 0xA8 || raw[2] != 0x01 || raw[3] != 0x00 {
        return Err(InvalidMaterial);
    }
    let aligned_len = ((raw.len() - 4) / 16) * 16;
    if aligned_len == 0 {
        return Err(InvalidMaterial);
    }
    let h1 = Sha512::digest(&key_seed);
    let mut seed_buf = Vec::with_capacity(h1.len() + SPADE_CONSTANTS.len());
    seed_buf.extend_from_slice(&h1);
    seed_buf.extend_from_slice(&SPADE_CONSTANTS);
    let h2 = Sha512::digest(&seed_buf);
    let mut plain = crate::hongguo::tt_encrypt::aes128_cbc_decrypt(
        &h2[..16],
        &h2[16..32],
        &raw[4..4 + aligned_len],
    )
    .ok_or(InvalidMaterial)?;
    if let Some(&pad) = plain.last() {
        let pad_len = pad as usize;
        if (1..=16).contains(&pad_len) && pad_len <= plain.len() {
            plain.truncate(plain.len() - pad_len);
        }
    }
    while plain.last() == Some(&0) {
        plain.pop();
    }
    String::from_utf8(plain).map_err(|_| InvalidMaterial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_base64_and_short_material() {
        assert_eq!(derive("!!!not base64!!!"), Err(InvalidMaterial));
        assert_eq!(derive("AAA="), Err(InvalidMaterial)); // 只有 2 字节
    }

    #[test]
    fn popcount_matches_byte_lane_sum() {
        // 0x00000000 -> 0；0x0000000f -> 4；0x0000ffff -> 16
        assert_eq!(popcount_index(0), 0);
        assert_eq!(popcount_index(0xf), 4);
        assert_eq!(popcount_index(0xffff), 16);
    }

    #[test]
    fn decrypts_spade_url_round_trip() {
        use base64::Engine;
        use sha2::{Digest, Sha512};
        let key_seed = b"test-seed-16byte";
        let key_seed_b64 = base64::engine::general_purpose::STANDARD.encode(key_seed);
        let h1 = Sha512::digest(key_seed);
        let mut seed_buf = Vec::new();
        seed_buf.extend_from_slice(&h1);
        seed_buf.extend_from_slice(&SPADE_CONSTANTS);
        let h2 = Sha512::digest(&seed_buf);
        let url = b"https://example.com/play.mp4";
        let pad = 16 - (url.len() % 16);
        let mut padded = url.to_vec();
        padded.extend(std::iter::repeat_n(pad as u8, pad));
        let ct = crate::hongguo::tt_encrypt::aes128_cbc_encrypt(&h2[..16], &h2[16..32], &padded);
        let mut envelope = vec![0xA8, 0x00, 0x01, 0x00];
        envelope.extend_from_slice(&ct);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&envelope);
        assert_eq!(
            decrypt_spade_url(&b64, &key_seed_b64).unwrap(),
            "https://example.com/play.mp4"
        );
    }
}
