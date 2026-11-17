//! 封面解密与落盘缓存。
//!
//! 站点封面是 **AES-128-CBC 密文**（实测 `Content-Type: binary/octet-stream`，
//! 字节数恒为 16 的倍数，无图片魔数）。密钥与 IV 是站点固定的 ASCII 串。
//!
//! ## 为什么必须落盘、且缓存键要去掉 query
//!
//! 封面地址带时效签名（`?auth_key=<过期时间>-…`），同一张图每次列表刷新拿到的
//! URL 都不同。拿整条 URL 当缓存键 = 永远不命中 + 目录无限膨胀。所以键只取
//! `scheme + host + path`。也正因为签名会过期，**不能把 URL 交给前端长期持有**，
//! 必须在这里换成稳定的本地文件路径。
//!
//! ## 为什么自带 AES 而不复用 hongguo/tt_encrypt.rs
//!
//! 那个模块只有**加密**方向（`aes128_cbc_encrypt`），没有逆密码；而且它是红果
//! 签名链路的一部分、由黄金向量锁着。本模块要能被整体删除而不碰红果，所以自带
//! 一份逆变换。正确性由两组向量锁定：NIST FIPS-197 的已知答案，以及真实封面
//! 的首块密文/明文对（见 tests）。

use std::path::{Path, PathBuf};
use std::time::Duration;

const IMG_KEY: &[u8; 16] = b"f5d965df75336270";
const IMG_IV: &[u8; 16] = b"97b60394abc2fbe1";

/// 缓存文件数上限。单张实测约 170 KB，600 张约 100 MB。
const COVER_CACHE_MAX: usize = 600;

const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// 逆 S-box 由正表求逆得到，而不是再抄一张表：两张表永远不可能抄歪其一。
const fn build_inv_sbox() -> [u8; 256] {
    let mut inv = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        inv[SBOX[index] as usize] = index as u8;
        index += 1;
    }
    inv
}
const INV_SBOX: [u8; 256] = build_inv_sbox();

const fn gf_mul_const(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        a = (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 };
        b >>= 1;
    }
    product
}

const fn mul_table(factor: u8) -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = gf_mul_const(i as u8, factor);
        i += 1;
    }
    table
}

const MUL9: [u8; 256] = mul_table(9);
const MUL11: [u8; 256] = mul_table(11);
const MUL13: [u8; 256] = mul_table(13);
const MUL14: [u8; 256] = mul_table(14);

static IMG_ROUND_KEYS: std::sync::LazyLock<[u8; 176]> =
    std::sync::LazyLock::new(|| expand_key(IMG_KEY));

fn expand_key(key: &[u8; 16]) -> [u8; 176] {
    let mut schedule = [0u8; 176];
    schedule[..16].copy_from_slice(key);
    let mut rcon = 1u8;
    let mut index = 16;
    while index < 176 {
        let mut word = [
            schedule[index - 4],
            schedule[index - 3],
            schedule[index - 2],
            schedule[index - 1],
        ];
        if index % 16 == 0 {
            word = [
                SBOX[word[1] as usize] ^ rcon,
                SBOX[word[2] as usize],
                SBOX[word[3] as usize],
                SBOX[word[0] as usize],
            ];
            rcon = (rcon << 1) ^ if rcon & 0x80 != 0 { 0x1b } else { 0 };
        }
        for offset in 0..4 {
            schedule[index + offset] = schedule[index - 16 + offset] ^ word[offset];
        }
        index += 4;
    }
    schedule
}

fn add_round_key(state: &mut [u8; 16], key: &[u8]) {
    for (byte, key_byte) in state.iter_mut().zip(key) {
        *byte ^= key_byte;
    }
}

/// 行 r 右移 r 格。状态按输入顺序平铺，所以行 r 的下标是 r, r+4, r+8, r+12。
fn inv_shift_rows(state: &mut [u8; 16]) {
    let original = *state;
    for row in 1..4 {
        for column in 0..4 {
            state[row + 4 * column] = original[row + 4 * ((column + 4 - row) % 4)];
        }
    }
}

fn inv_mix_columns(state: &mut [u8; 16]) {
    for column in 0..4 {
        let base = column * 4;
        let [a0, a1, a2, a3] = [
            state[base],
            state[base + 1],
            state[base + 2],
            state[base + 3],
        ];
        state[base] =
            MUL14[a0 as usize] ^ MUL11[a1 as usize] ^ MUL13[a2 as usize] ^ MUL9[a3 as usize];
        state[base + 1] =
            MUL9[a0 as usize] ^ MUL14[a1 as usize] ^ MUL11[a2 as usize] ^ MUL13[a3 as usize];
        state[base + 2] =
            MUL13[a0 as usize] ^ MUL9[a1 as usize] ^ MUL14[a2 as usize] ^ MUL11[a3 as usize];
        state[base + 3] =
            MUL11[a0 as usize] ^ MUL13[a1 as usize] ^ MUL9[a2 as usize] ^ MUL14[a3 as usize];
    }
}

fn decrypt_block(schedule: &[u8; 176], block: &[u8; 16]) -> [u8; 16] {
    let mut state = *block;
    add_round_key(&mut state, &schedule[160..176]);
    for round in (1..10).rev() {
        inv_shift_rows(&mut state);
        for byte in state.iter_mut() {
            *byte = INV_SBOX[*byte as usize];
        }
        add_round_key(&mut state, &schedule[round * 16..round * 16 + 16]);
        inv_mix_columns(&mut state);
    }
    inv_shift_rows(&mut state);
    for byte in state.iter_mut() {
        *byte = INV_SBOX[*byte as usize];
    }
    add_round_key(&mut state, &schedule[0..16]);
    state
}

fn aes_cbc_decrypt(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return None;
    }
    let schedule = &*IMG_ROUND_KEYS;
    let mut previous = *IMG_IV;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.as_chunks::<16>().0 {
        let mut cipher = [0u8; 16];
        cipher.copy_from_slice(chunk);
        let plain = decrypt_block(schedule, &cipher);
        for index in 0..16 {
            out.push(plain[index] ^ previous[index]);
        }
        previous = cipher;
    }
    Some(out)
}

/// PKCS#7 去填充。填充不合法时**原样返回**而不是报错：站点偶尔给未加密的图，
/// 宁可让后面的魔数判定去决定，也不要把一张能看的图判成失败。
fn strip_padding(mut buffer: Vec<u8>) -> Vec<u8> {
    let pad = *buffer.last().unwrap_or(&0) as usize;
    if pad > 0
        && pad <= 16
        && buffer.len() >= pad
        && buffer[buffer.len() - pad..]
            .iter()
            .all(|byte| *byte as usize == pad)
    {
        buffer.truncate(buffer.len() - pad);
    }
    buffer
}

/// 图片魔数判定，返回（MIME, 扩展名）。
fn image_kind(buffer: &[u8]) -> Option<(&'static str, &'static str)> {
    if buffer.len() < 12 {
        return None;
    }
    if buffer[0] == 0xFF && buffer[1] == 0xD8 {
        return Some(("image/jpeg", "jpg"));
    }
    if buffer.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some(("image/png", "png"));
    }
    if buffer.starts_with(b"GIF8") {
        return Some(("image/gif", "gif"));
    }
    if buffer.starts_with(b"RIFF") && &buffer[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    None
}

/// 密文 → 明文图片字节。已经是图片就原样返回（站点并非所有图都加密）。
pub fn decrypt_cover(bytes: &[u8]) -> Option<Vec<u8>> {
    if image_kind(bytes).is_some() {
        return Some(bytes.to_vec());
    }
    let plain = strip_padding(aes_cbc_decrypt(bytes)?);
    image_kind(&plain).is_some().then_some(plain)
}

fn cache_dir() -> PathBuf {
    crate::short_drama_app::cache_dir().join("huangguo-covers-v1")
}

/// 缓存键：去掉 query 的地址摘要。`auth_key` 每次刷新都变，不能进键。
fn cache_stem(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let stable = url.split('?').next().unwrap_or(url);
    let digest = Sha256::digest(stable.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cached_path(dir: &Path, stem: &str) -> Option<PathBuf> {
    for extension in ["jpg", "png", "webp", "gif"] {
        let candidate = dir.join(format!("{stem}.{extension}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 超出上限时按修改时间淘汰最旧的一批。
///
/// **节流 60 秒**：原实现每写一张就 `read_dir` + 逐个 `metadata()`，一屏 24 张
/// 就是 24 轮全目录 stat（600 个文件时上万次系统调用）。淘汰本来就不需要实时，
/// 一分钟一次足够，且把这笔开销从首屏热路径上挪走。
fn prune(dir: &Path) {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    {
        let mut guard = LAST.lock().unwrap_or_else(|error| error.into_inner());
        if guard.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
            return;
        }
        *guard = Some(std::time::Instant::now());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            path.is_file().then_some((modified, path))
        })
        .collect();
    if files.len() <= COVER_CACHE_MAX {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let excess = files.len() - COVER_CACHE_MAX;
    for (_, path) in files.iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
    crate::trace::log(format!(
        "[黄果] 封面缓存淘汰 {excess} 张（上限 {COVER_CACHE_MAX}）"
    ));
}

/// 取一张封面，返回本地文件路径（前端 `convertFileSrc` 后呈现）。
///
/// 失败一律 `None`：封面是装饰，调用方按「无封面」处置，不该让整屏列表报错。
/// 但**失败原因要落日志**——这条链路要过代理，「封面偶发空白」不落原因就没法归因
/// （2026-11-11 加：此前所有失败都被 `.ok()?` 吞掉，日志里一行证据都没有）。
/// 日志只记去掉 query 的地址：`auth_key` 是时效凭据，不该写进日志文件。
pub fn cached_cover(url: &str) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let dir = cache_dir();
    let stem = cache_stem(url);
    cached_path(&dir, &stem).map(|hit| hit.to_string_lossy().into_owned())
}

pub async fn cover(client: &reqwest::Client, url: &str) -> Option<String> {
    let stable = url.split('?').next().unwrap_or(url);
    let fail = |reason: String| -> Option<String> {
        crate::trace::log(format!("[黄果] 封面失败 {stable} 原因={reason}"));
        None
    };
    if !url.starts_with("https://") {
        return fail("地址不是 https".into());
    }
    let dir = cache_dir();
    let stem = cache_stem(url);
    if let Some(hit) = cached_path(&dir, &stem) {
        return Some(hit.to_string_lossy().into_owned());
    }
    let response = match client
        .get(url)
        .timeout(Duration::from_secs(12))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let kind = if error.is_timeout() {
                "超时"
            } else if error.is_connect() {
                "连接失败（多半是没有可用代理）"
            } else {
                "传输失败"
            };
            return fail(format!("{kind}: {error}"));
        }
    };
    let status = response.status();
    if !status.is_success() {
        return fail(format!("HTTP {status}"));
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return fail(format!("读取字节失败: {error}")),
    };
    let Some(plain) = decrypt_cover(&bytes) else {
        return fail(format!(
            "解密后不是图片（{} 字节，首 8 字节 {}）",
            bytes.len(),
            bytes
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
    };
    let Some((_, extension)) = image_kind(&plain) else {
        return fail("明文缺少图片魔数".into());
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return fail(format!("建缓存目录失败: {error}"));
    }
    let path = dir.join(format!("{stem}.{extension}"));
    if let Err(error) = std::fs::write(&path, &plain) {
        return fail(format!("写缓存失败: {error}"));
    }
    prune(&dir);
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    /// NIST FIPS-197 附录 C.1 的 AES-128 已知答案。
    /// 逆密码写错任何一步（行移位方向、逆列混合系数、轮密钥顺序）这里立刻红。
    #[test]
    fn aes128_matches_the_nist_known_answer() {
        let key = hex("000102030405060708090a0b0c0d0e0f");
        let cipher = hex("69c4e0d86a7b0430d8cdb78070b4c55a");
        let plain = hex("00112233445566778899aabbccddeeff");
        let schedule = expand_key(&key.try_into().unwrap());
        let mut block = [0u8; 16];
        block.copy_from_slice(&cipher);
        assert_eq!(decrypt_block(&schedule, &block).to_vec(), plain);
    }

    #[test]
    fn inv_sbox_really_is_the_inverse_of_sbox() {
        for index in 0..256 {
            assert_eq!(INV_SBOX[SBOX[index] as usize], index as u8);
        }
    }

    /// 真实封面首块（2026-11-11 取证）：密文 → 明文 JPEG 头。
    /// 这一条锁的是**站点常量**（key/IV），与上一条锁算法互补。
    /// 走 CBC 而不是裸块解密：首块明文 = D(密文) ⊕ IV，少了 IV 就不是站点的口径。
    #[test]
    fn real_cover_first_block_decrypts_to_a_jpeg_header() {
        let cipher = hex("093de3b1fc4af46ff223ac4c8bdb076a");
        let expected = hex("ffd8ffe000104a464946000101000001");
        assert_eq!(aes_cbc_decrypt(&cipher).unwrap(), expected);
    }

    /// 跨 3 块的 CBC 黄金向量（用站点 key/IV 生成，44 字节明文 → 48 字节密文）。
    /// 锁的是**链接**与**去填充**：单块向量对这两样都无感。
    #[test]
    fn cbc_chaining_and_unpadding_match_the_generated_vector() {
        let cipher = hex(
            "e8a04cc5d504e4682d214c360d9669b35f664a8af1cc8c0a51f242e11ef3e921\
             7a489db9ed52eebafbefad5b6e966068",
        );
        let plain = hex(
            "89504e470d0a1a0a6875616e6767756f2d6362632d636861696e2d766563746f\
             722d323032362d31312d3131",
        );
        assert_eq!(cipher.len(), 48);
        assert_eq!(decrypt_cover(&cipher).unwrap(), plain);
    }

    #[test]
    fn unencrypted_images_pass_through_untouched() {
        let plain = b"\xFF\xD8\xFF\xE0cover-payload-for-test".to_vec();
        assert!(image_kind(&plain).is_some());
        assert_eq!(decrypt_cover(&plain).unwrap(), plain);
    }

    #[test]
    fn rejects_non_block_aligned_and_non_image_payloads() {
        assert!(aes_cbc_decrypt(&[0u8; 15]).is_none());
        assert!(aes_cbc_decrypt(&[]).is_none());
        // 16 字节全零解出来不是图片，必须判失败而不是返回一堆乱码。
        assert!(decrypt_cover(&[0u8; 16]).is_none());
    }

    #[test]
    fn cache_key_ignores_the_rotating_auth_signature() {
        let a = cache_stem("https://pic.example/a/b.jpg?auth_key=111-0-0-aaa");
        let b = cache_stem("https://pic.example/a/b.jpg?auth_key=222-0-0-bbb");
        let c = cache_stem("https://pic.example/a/other.jpg?auth_key=111-0-0-aaa");
        assert_eq!(a, b, "签名轮换不该让缓存失效");
        assert_ne!(a, c, "不同的图必须是不同的键");
    }

    #[test]
    fn padding_is_only_stripped_when_it_is_valid_pkcs7() {
        assert_eq!(strip_padding(vec![1, 2, 3, 3, 3, 3]), vec![1, 2, 3]);
        // 末尾字节声称 4 但实际只有两个 4：不合法，原样保留。
        assert_eq!(strip_padding(vec![1, 2, 4, 4]), vec![1, 2, 4, 4]);
        assert_eq!(strip_padding(vec![1, 2, 0]), vec![1, 2, 0]);
    }
}
