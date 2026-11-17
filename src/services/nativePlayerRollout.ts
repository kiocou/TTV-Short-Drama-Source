/**
 * 独立 MPV 播放器总闸。
 *
 * 独立播放器已切换至 `src-tauri/ttv-player/`（Rust + libmpv + D3D11/DirectComposition
 * + GPU 光流补帧 + 晶璃 HUD）并完成与主程序的管道协议对齐，现已正式全量开放。
 */
export const NATIVE_PLAYER_AVAILABLE = true;

/** 已正式上线，保留常量以兼容外部导入，值为空串。 */
export const NATIVE_PLAYER_GRAY_REASON = '';
