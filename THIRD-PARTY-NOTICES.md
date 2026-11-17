# 第三方组件与许可证清单（THIRD-PARTY NOTICES）

本仓库源代码采用 **PolyForm Noncommercial License 1.0.0**（非商业开源协议，见根目录 `LICENSE`）。以下列出与本项目配合使用或引用的第三方组件，便于读者与分发者确认各自许可证义务；**公开源码仓库已剔除预编译第三方二进制文件（`ffmpeg.exe`、`libmpv-2.dll`、`duanju_core.dll`、嵌入式 CPython 与 Python `.pyd` 轮子）**。

> ⚠️ 本项目本身只做客户端技术研究：不托管任何内容，所有剧集数据与视频流均来自第三方
> 站点 / 接口，版权归各自权利人所有。仅供个人学习与非商业技术研究，禁止任何商业用途。

## 一、随包可执行与运行时

| 组件 | 位置 | 版本 / 形态 | 许可证 |
| --- | --- | --- | --- |
| **ffmpeg** | `src-tauri/resources/mpv/ffmpeg.exe` | Windows x64 静态构建，配置含 `--enable-gpl --enable-version3` | **GPLv3**（因启用 GPL 组件，整体按 GPLv3 分发） |
| **嵌入式 CPython** | `src-tauri/resources/python/`（含 `python312.dll`、`python312.zip`、`libcrypto-3.dll` 等） | Python 3.12 embeddable | PSF License；随附 `resources/python/LICENSE.txt` |
| **guo-core（站源适配内核）** | `src-tauri/guo-core/`（Go 源码）与 `src-tauri/resources/guo-core/duanju_core.dll`（预编译二进制） | 本项目自带，源码整棵入库 | 不在 MIT 范围内（见 `LICENSE` 的范围说明） |
| **第三方站源对接实现** | `src-tauri/resources/shortdrama-worker/worker.py`、`.../liushen/` | 本项目自带（含第三方签名算法实现） | 不在 MIT 范围内（见 `LICENSE` 的范围说明） |
| **openhongguo（红果协议客户端）** | `src-tauri/src/hongguo/`（24 个 Rust 文件，Go 原实现的逐层重写） | 移植基线 = 上游 **0.2.0**（2026-11-04）；含签名（medusa/gorgon/helios/f13/sm3/aes_v3）、设备注册与 `withFreshDevice` 换设备语义 | **Apache-2.0**（见下节义务） |

### openhongguo 的 Apache-2.0 义务

`src-tauri/src/hongguo/` 是对 [beilunyang/openhongguo](https://github.com/beilunyang/openhongguo)
（Apache-2.0）Go 实现的 Rust 移植，移植基线为上游 0.2.0（2026-11-04）。

- **上游项目页**：<https://github.com/beilunyang/openhongguo>
- **许可证全文**：<https://www.apache.org/licenses/LICENSE-2.0>
- **修改说明**：本项目不是原样分发上游代码，而是按 Rust/tokio 技术栈逐层重写
  （协议常量、签名算法、注册流程、目录/搜索/榜单、播放模型、弹幕各层分别落点）。
  因此保留了上游的**协议语义与常量取值**，但实现代码为本项目原创。
- **为何在此声明**：上游仓库内没有单独的 `NOTICE` 文件，Apache-2.0 §4 要求
  衍生作品保留版权与许可声明；此前该声明只写在 `src-tauri/src/hongguo/mod.rs`
  的模块注释里，对源码读者可见，但**发布物（安装包、Release 页）里看不到**。
  本条目即为发布物侧的归属声明。
- **再分发义务**：重新分发本应用的安装包时，请保留本文件与本节声明；若同时分发
  源码，须一并提供 Apache-2.0 许可证全文。

### ffmpeg 的 GPLv3 义务

- 分发的是**未修改的官方构建**，无本地补丁。
- 源码获取：<https://ffmpeg.org/download.html>（对应版本号见应用内「设置 → 关于」或
  构建脚本；如需精确对应，可对 `ffmpeg.exe` 执行 `ffmpeg -version` 查看构建配置行）。
- 重新分发本应用（含 `ffmpeg.exe`）时，必须同时满足 GPLv3 的声明与源码提供义务；
  若你不打算承担该义务，请在打包前移除 `src-tauri/resources/mpv/`。

### 黄果短剧专区（`src-tauri/src/huangguo/`）的净室声明

`src-tauri/src/huangguo/` 与 `huangguo_provider.rs` 是**本项目原创实现**，
不含任何第三方代码。

- **协议事实的来源**：边缘信封的请求/响应形态、封面 AES-128-CBC 的密钥与 IV、
  各接口路径与字段语义，均由**对公开站点的互操作性实测**确定
  （2026-11-11：信封往返、封面带/不带 Referer 的字节对照、播放列表结构核对）。
  这些是接口事实与常量取值，不是表达性代码。
- **与 HGhub 的关系**：调研阶段阅读过 HGhub 客户端源码（`huangguo-client`，
  **PolyForm-Noncommercial-1.0.0**，禁止商用）。为确保净室实现，本模块
  **没有拷贝其任何源码**：本模块自带 AES 逆密码（由 NIST FIPS-197 向量与
  真实封面向量双重锁定）、自写信封客户端与映射层，播放走本项目既有的
  `hls_proxy`，不使用上游的 `hg.localhost` 自定义协议。
  `scripts/verify-huangguo.mjs` 会扫描上游实现标识，防止后续改动把代码带进来。
- **内容归属**：本应用不托管任何内容。该专区的剧集数据与视频流均来自第三方站点，
  版权归各自权利人所有。入口为标题栏品牌区连点七次的隐藏手势，仅供个人学习与
  技术研究。

## 二、Python 依赖（`resources/shortdrama-worker/site-packages/`）

| 包 | 版本（实测） | 许可证 |
| --- | --- | --- |
| requests | 2.34.2 | Apache-2.0 |
| urllib3 | 2.7.0 | MIT |
| certifi | — | MPL-2.0 |
| charset_normalizer | — | MIT |
| idna | — | BSD-3-Clause |
| multidict | — | Apache-2.0 |
| h2 / hpack / hyperframe | — | MIT |
| grpclib | — | BSD-3-Clause |
| stringcase | 1.2.0 | MIT |
| pycryptodome / pycryptodomex | 3.23.0 | Public Domain / BSD-2-Clause（见包内 `LICENSE.rst`） |
| gmssl | 3.2.2 | MIT |

各包的完整许可证文本随包分发在 `site-packages/<包名>*.dist-info/licenses/` 或包根目录；
本清单只做索引，不替代原始文本。

## 三、前端与后端依赖

- **前端**（`package.json`，构建时打包进 `dist/`）：React 19（MIT）、React DOM（MIT）、
  hls.js（Apache-2.0）、lucide-react（ISC）、clsx（MIT）；构建工具链 Vite / TypeScript /
  Tailwind CSS / PostCSS（各自 MIT）。
- **Rust**（`src-tauri/Cargo.toml`，静态链接进可执行文件）：Tauri 2（MIT / Apache-2.0）、
  tokio（MIT）、serde（MIT / Apache-2.0）、reqwest（MIT / Apache-2.0）、
  windows-sys（MIT / Apache-2.0）等。完整清单与版本见 `src-tauri/Cargo.lock`；
  许可证以各 crate 的 `license` 字段为准。

## 四、再分发前的检查清单

1. 若你在本地打包时额外放入了 `ffmpeg.exe`（`src-tauri/resources/mpv/`），须一并履行 GPLv3 声明与源码提供义务。
2. 若在打包产物中包含 `src-tauri/resources/shortdrama-worker/` 或 `src-tauri/guo-core/`，请注意其涉及非公开第三方接口适配，仅限个人学习且不得商用或违规分发。
3. 保留本文件与 `LICENSE`（PolyForm Noncommercial 1.0.0），以及 `src-tauri/src/hongguo/` 的 `openhongguo` Apache-2.0 归属声明。
4. 不要移除应用内的免责声明与「仅供个人学习」提示。
   但归属声明的存续靠这道人工检查。
