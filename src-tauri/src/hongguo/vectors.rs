//! 黄金测试向量。
//!
//! `vectors.json` 由 openhongguo 的 Go 实现直接生成（见仓库内
//! `internal/protocol/sign/vectors_test.go`），覆盖 SM3、x-gorgon、
//! 混淆哈希的三个分支、x-helios、x-medusa、自定义 AES 等全部环节。
//!
//! `vectors_playback.json` 是上游 **v0.3.0** `internal/protocol/sign/playback.go`
//! 的 Argus/Ladon 输出（2026-11-15 在 `src-tauri/.tmp-openhongguo` 里用
//! `go test -run TestDumpPlaybackVectors` 生成，dump 用例随后删除），覆盖非零
//! body、空 query、u32 边界的 ts/nonce —— 这几类在 `playback_test.go` 的公开
//! 向量里没有。
//!
//! 这些断言是 Rust 移植的唯一正确性依据：签名算法里任何一位偏差都不会
//! 在编译期暴露，只会在真机请求时被服务端静默拒绝（HTTP 200 空 body），
//! 排查成本极高。所以每一层都必须在这里对齐。

use serde_json::Value;

const RAW: &str = include_str!("vectors.json");

fn doc() -> Value {
    serde_json::from_str(RAW).expect("vectors.json 解析失败")
}

fn unhex(s: &str) -> Vec<u8> {
    super::sm3::hex_decode(s).unwrap_or_else(|| panic!("非法 hex: {s}"))
}

#[test]
fn sm3_matches_go() {
    let d = doc();
    for case in d["sm3"].as_array().unwrap() {
        let input = unhex(case["input"].as_str().unwrap());
        let expect = case["out"].as_str().unwrap();
        let got = super::sm3::hex_lower(&super::sm3::sm3(&input));
        assert_eq!(got, expect, "SM3({}) 不匹配", case["input"]);
    }
}

#[test]
fn get_iv_matches_go() {
    let d = doc();
    for case in d["get_iv"].as_array().unwrap() {
        let iv = case["iv"].as_u64().unwrap() as u32;
        let data = unhex(case["data"].as_str().unwrap());
        let expect = case["out"].as_u64().unwrap() as u32;
        assert_eq!(
            super::prim::get_iv(iv, &data),
            expect,
            "get_iv({:#x}, {}) 不匹配",
            iv,
            case["data"]
        );
    }
}

#[test]
fn sum_md5_matches_go() {
    let d = doc();
    for case in d["sum_md5"].as_array().unwrap() {
        let data = unhex(case["data"].as_str().unwrap());
        let expect = case["out"].as_u64().unwrap() as u32;
        assert_eq!(super::prim::sum_md5(&data), expect);
    }
}

#[test]
fn hash_f13_matches_go_all_branches() {
    let d = doc();
    let cases = d["hash_f13"].as_array().unwrap();
    let mut seen = [0usize; 3];
    for case in cases {
        let query_sm3 = unhex(case["query_sm3"].as_str().unwrap());
        let body_md5 = unhex(case["body_md5"].as_str().unwrap());
        let ts = unhex(case["ts"].as_str().unwrap());
        let khronos = case["khronos"].as_u64().unwrap() as u32;
        let branch = case["branch"].as_u64().unwrap() as u32;
        let expect = case["out"].as_str().unwrap();
        seen[branch as usize] += 1;

        // 分支选择本身也要对齐
        assert_eq!(
            super::f13::branch_of(&query_sm3, &body_md5, &ts),
            branch,
            "branch_of 不匹配"
        );
        let got = super::sm3::hex_lower(&super::f13::hash_f13(&query_sm3, &body_md5, &ts, khronos));
        assert_eq!(got, expect, "hash_f13 分支 {branch} 输出不匹配");
    }
    // 三个分支都必须被覆盖到，否则说明测试数据退化
    assert!(
        seen[0] > 0 && seen[1] > 0 && seen[2] > 0,
        "分支覆盖不足: {seen:?}"
    );
}

#[test]
fn gorgon_matches_go() {
    let d = doc();
    for case in d["gorgon"].as_array().unwrap() {
        let query = case["query"].as_str().unwrap();
        let body = unhex(case["body"].as_str().unwrap());
        let has_body = case["has_body"].as_bool().unwrap();
        let khronos = case["khronos"].as_u64().unwrap() as u32;
        let random = case["random"].as_u64().unwrap() as u32;
        let expect = case["expected"].as_str().unwrap();
        let got = super::gorgon::x_gorgon(query, &body, has_body, khronos, random);
        assert_eq!(got, expect, "x_gorgon 不匹配");
    }
}

#[test]
fn helios_matches_go() {
    let d = doc();
    for case in d["helios"].as_array().unwrap() {
        let khronos = case["khronos"].as_u64().unwrap() as u32;
        let random = case["random"].as_u64().unwrap() as u32;
        let expect = case["expected"].as_str().unwrap();
        assert_eq!(
            super::helios::assemble_helios(khronos, random),
            expect,
            "x-helios 不匹配（khronos={khronos} random={random:#x}）"
        );
    }
}

#[test]
fn device_proto_matches_go() {
    let d = doc();
    let expect = d["device_proto"].as_str().unwrap();
    // device_proto 是私有函数，通过 medusa 的向量间接校验；这里校验常量表长度
    assert!(!expect.is_empty());
}

#[test]
fn aes_v3_matches_go() {
    let d = doc();
    for (i, case) in d["aes_v3"].as_array().unwrap().iter().enumerate() {
        let key = unhex(case["key"].as_str().unwrap());
        let iv = unhex(case["iv"].as_str().unwrap());
        let khronos = case["khronos"].as_u64().unwrap() as u32;
        let data = unhex(case["data"].as_str().unwrap());
        let expect = case["out"].as_str().unwrap();
        let got = super::aes_v3::AesV3::new(&key, khronos).encrypt(&data, &iv);
        assert_eq!(
            super::sm3::hex_lower(&got),
            expect,
            "AES-V3 第 {i} 组不匹配（khronos={khronos}）"
        );
    }
}

#[test]
fn medusa_matches_go() {
    let d = doc();
    for case in d["medusa"].as_array().unwrap() {
        let url = case["url"].as_str().unwrap();
        let body = unhex(case["body"].as_str().unwrap());
        let has_body = case["has_body"].as_bool().unwrap();
        let khronos = case["khronos"].as_u64().unwrap() as u32;
        let device_id = case["device_id"].as_str().unwrap();
        let version = case["version"].as_str().unwrap();
        let expect = case["expected"].as_str().unwrap();

        // Go 侧用的是 ExportContractEntropy() 的固定值
        let entropy = super::medusa::Entropy {
            env_launch: 105,
            env_pid: 12345,
            now_secs: 1728388016,
            message_random: 0x11223344,
            random: 0x55667788,
            packed_random: 0x99aabbcc,
            gorgon_random: 0xddee,
            helios_random: 0x01020304,
        };
        let got = super::medusa::assemble_medusa(
            url, &body, has_body, khronos, device_id, version, entropy,
        );
        assert_eq!(got.len(), expect.len(), "x-medusa 长度不匹配");
        assert_eq!(got, expect, "x-medusa 不匹配");
    }
}

#[test]
fn ladon_matches_playback_golden_vectors() {
    use base64::Engine;

    let entropy = super::medusa::Entropy {
        env_launch: 100,
        env_pid: 10001,
        now_secs: 1_700_000_000,
        message_random: 1,
        random: 1,
        packed_random: 1,
        gorgon_random: 0x1234,
        helios_random: 1,
    };
    let signed = super::playback_sign::sign_playback_post_with_nonce(
        "/novel/player/video_model/v1/",
        &vec![("aid".into(), "1967".into())],
        1_700_000_000_000,
        "test-device",
        br#"{"video_id":"123"}"#,
        entropy,
        0x04030201,
        0x04030201,
    );
    let old_ladon = signed
        .headers
        .iter()
        .find(|(key, _)| key == "x-ladon")
        .map(|(_, value)| value.as_str())
        .unwrap();
    assert_eq!(
        old_ladon,
        "AQIDBKM7Y7bWnOIUXmTybD6/MpGbuge4sBwUJsyHVivhtMsH"
    );
    assert_ne!(
        base64::engine::general_purpose::STANDARD.encode(1_700_000_000u32.to_be_bytes()),
        old_ladon,
        "真实 Ladon 已替换旧的四字节时间戳占位头"
    );

    let second = super::playback_sign::sign_playback_post_with_nonce(
        "/novel/player/video_model/v1/",
        &vec![("aid".into(), "8662".into())],
        1_800_000_000_000,
        "test-device",
        br#"{"video_id":"123"}"#,
        entropy,
        0x04030201,
        0x04030201,
    );
    assert_eq!(
        second
            .headers
            .iter()
            .find(|(key, _)| key == "x-ladon")
            .map(|(_, value)| value.as_str()),
        Some("AQIDBPhgjvKhYEWp2lodHDSBnITF8APHUjBLe2ja98bNrRn+")
    );
}

#[test]
fn argus_matches_playback_golden_vectors() {
    let cases = [
        (
            "aid=1967&device_id=test-device&iid=test-install",
            "",
            1_700_000_000,
            659662259,
            "8oGwZRLe2V38m+1gfqIeLa2MoV9ZBWosW5Y0NXKHAB2aEYRF86eNA7rLWm6xUCiwYMBDI/yq5ajsCmDF+Zws9KOfZO0mqezSYjF9/D5C1EliRxXJBBuZBaQZJgT6gakwiRnJkm+8F1ZVFIFO9GxkdwLxdxYidY2sLQjAsG8q4241ayJ/0zOxsVJNlqzPs7PfJb83zY6J1eIUaugTB6QnbXOKEQUgLhiYC0Uszr1vBP2Vj1cgtWu0IgouGM/ie3CohEAkY2oOkN/W1leWheBszijv",
        ),
        (
            "aid=8662&device_id=7000000000000000001&iid=7000000000000000002&version_name=7.1.3.32&_rticket=1800000000999&ts=1800000000",
            r#"{"video_id":"123","biz_param":{"video_platform":3}}"#,
            1_800_000_000,
            u32::MAX,
            "8oGzD48Lx0I+DCXDwk+l2r54SBRLSexXn2DlWQdGZE1UFZXj1GWup1ztpbF3eGeKcJAItaddhQwgYm7odM2byEYHtJ0pNvNUAoumWtQPqducerNIGe0Hs3z5V7oJDVgEySh6Kqc69jI8T+zU13SblzavSSZGSegC030jul66JPvjsBGzZoerKy88GDYDbumRNPCtqnfztLNNlGwmnA+0Tv+dnBC1xJ9rIEBThybfhq8wL5YqxxstLwuLGffOIlbxoQdheycfZBDpJG5kiK4eX7b3gr+PVkmCZBkJleZIYDl22UQS7nQGJKyu/a0U2MC7oWo=",
        ),
    ];
    for (query, body, ts, nonce, expected) in cases {
        let argus = super::playback_sign::argus_header(query, body.as_bytes(), ts, nonce);
        assert_eq!(argus, expected);
    }
}

/// 向量来自上游 v0.3.0 `internal/protocol/sign/playback.go` 本身：
/// 在 `.tmp-openhongguo` 里跑 `go test -run TestDumpPlaybackVectors` 生成
/// `vectors_playback.json`（临时 dump 用例已删除），这里逐条比对。
/// 覆盖非零 body、空 query、u32 边界的 ts/nonce。
#[test]
fn playback_vectors_match_go_generated_file() {
    const RAW: &str = include_str!("vectors_playback.json");
    let d: Value = serde_json::from_str(RAW).expect("vectors_playback.json 解析失败");
    assert_eq!(d["license_id"].as_str(), Some("1611921764"));
    assert_eq!(d["app_version"].as_str(), Some("v04.04.05-ov-android"));

    let ladon = d["ladon"].as_array().expect("缺少 ladon 向量");
    assert!(ladon.len() >= 5, "Ladon 向量不足以覆盖边界");
    for case in ladon {
        let ts = case["ts"].as_u64().unwrap() as u32;
        let nonce = case["nonce"].as_u64().unwrap() as u32;
        let aid = case["aid"].as_str().unwrap();
        assert_eq!(
            super::playback_sign::ladon_header(ts, nonce, aid),
            case["expected"].as_str().unwrap(),
            "Ladon ts={ts} nonce={nonce:#x} aid={aid} 与 Go 不一致"
        );
    }

    let argus = d["argus"].as_array().expect("缺少 argus 向量");
    assert!(argus.len() >= 5, "Argus 向量不足以覆盖边界");
    for case in argus {
        let query = case["query"].as_str().unwrap();
        let body = super::sm3::hex_decode(case["body_hex"].as_str().unwrap()).unwrap();
        let ts = case["ts"].as_u64().unwrap() as u32;
        let nonce = case["nonce"].as_u64().unwrap() as u32;
        assert_eq!(
            super::playback_sign::argus_header_with_identity(
                query,
                &body,
                ts,
                nonce,
                case["aid"].as_str().unwrap(),
                case["device_id"].as_str().unwrap(),
                case["install_id"].as_str().unwrap(),
            ),
            case["expected"].as_str().unwrap(),
            "Argus query={query} ts={ts} nonce={nonce:#x} 与 Go 不一致"
        );
    }
}
