#!/usr/bin/env node
/**
 * `verify:native-player` —— 独立 MPV 播放器（纯 Rust `src-tauri/ttv-player/`）
 * 的架构、渲染/插帧管线、Crystal HUD、跨进程 IPC、资源打包与全量上线门禁验收。
 *
 * 退出码：任意断言失败返回 1；全部通过返回 0。
 */

import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');

let passed = 0;
let failed = 0;

/** 读取仓库内源码文本；不存在时返回 null 并由上层记录失败。 */
function readSource(relativePath) {
  const fullPath = resolve(ROOT, relativePath);
  if (!existsSync(fullPath)) return null;
  return readFileSync(fullPath, 'utf8');
}

function assertCheck(label, ok, detail = '') {
  if (ok) {
    passed += 1;
    console.log(`  ✓ ${label}`);
  } else {
    failed += 1;
    console.error(`  ✗ ${label}${detail ? ` — ${detail}` : ''}`);
  }
}

function requireFile(relativePath) {
  const content = readSource(relativePath);
  assertCheck(`存在关键源码/配置 ${relativePath}`, content !== null, '文件缺失');
  return content ?? '';
}

function checkAll(label, text, needles) {
  const missing = needles.filter((needle) => !text.includes(needle));
  assertCheck(
    label,
    missing.length === 0,
    missing.length ? `缺少: ${missing.join(', ')}` : '',
  );
}

function checkNone(label, text, forbidden) {
  const found = forbidden.filter((needle) => text.includes(needle));
  assertCheck(
    label,
    found.length === 0,
    found.length ? `出现禁止残余: ${found.join(', ')}` : '',
  );
}

console.log('[verify:native-player] 独立 MPV 播放器 (ttv-player) 全链路静态门禁');
console.log(`  根目录 ${ROOT}`);

// 1. 纯 Rust ttv-player 独立子工程文件结构
const ttvCargo = requireFile('src-tauri/ttv-player/Cargo.toml');
const ttvMain = requireFile('src-tauri/ttv-player/src/main.rs');
const ttvDcomp = requireFile('src-tauri/ttv-player/src/dcomp.rs');
const ttvInterp = requireFile('src-tauri/ttv-player/src/interp.rs');
const ttvRender = requireFile('src-tauri/ttv-player/src/render.rs');
const ttvMpv = requireFile('src-tauri/ttv-player/src/mpv.rs');
const ttvIpc = requireFile('src-tauri/ttv-player/src/ipc.rs');
const ttvPrefs = requireFile('src-tauri/ttv-player/src/prefs.rs');
const ttvHudHost = requireFile('src-tauri/ttv-player/src/webview_hud.rs');
const ttvHudHtml = requireFile('src-tauri/ttv-player/assets/hud.html');
requireFile('src-tauri/ttv-player/src/selftest.rs');

checkAll('ttv-player Cargo 工程定义与核心依赖完整', ttvCargo, [
  'name = "ttv-player"',
  'windows',
  'webview2-com',
  'serde_json',
  'sha2',
]);

// 2. D3D11 + DirectComposition + 4 级光流 GPU 补帧管线
checkAll('dcomp.rs 具备 DirectComposition + 无边框全屏 + 窗口贴合/钳制契约', ttvDcomp, [
  'DCompVideoHost',
  'CreateSwapChainForComposition',
  'IDCompositionTarget',
  'CreateVisual',
  'enter_borderless_fullscreen',
  'exit_borderless_fullscreen',
  'clamp_window_to_monitor',
  'compute_minmax_bounds',
  'nchittest_resize_border',
]);

checkAll('interp.rs 具备 DXBC 缓存与 GPU 4 级光流/场景检测管线', ttvInterp, [
  'CompiledShaderBlobs',
  'InterpShaderSet',
  'GpuInterpPipeline',
  'compute_active_uv_spec',
  'InterpContentMode',
  'FD_STATIC_PANEL',
  'FD_SCENE_CUT',
  'FD_DUPLICATE',
  'FD_GLOBAL_FADE',
]);

checkAll('render.rs 上报首帧呈现与管线就绪状态并同步刷新率', ttvRender, [
  'RenderConfigShared',
  'RenderController',
  'has_presented_frame',
  'is_pipeline_ready',
  'notify_frame_presented',
]);

checkAll('mpv.rs 包含 libmpv 动态查找、Seek 回弹锁与音量防覆写守卫', ttvMpv, [
  'find_mpv_dll',
  'PendingSeekState',
  'volume_lock_ms',
  'classify_mpv_error',
  'notify_frame_presented',
]);

// 3. 07 Crystal HUD 透明承载层与双色极简界面契约
checkAll('webview_hud.rs 内嵌 HUD 且清理父进程 WebView2 环境变量', ttvHudHost, [
  'EMBEDDED_HUD_HTML',
  'clear_inherited_webview2_env',
  'hostLeave',
]);

checkAll('hud.html 保持双主题透明层、拖拽/悬浮菜单保护与 9:16 全屏布局类', ttvHudHtml, [
  'data-theme="dark"',
  'data-theme="light"',
  'background: transparent !important',
  'hudReady',
  'startDrag',
  'fitWindow',
  'scheduleHoverMenuClose',
  'activeRangeDrag',
  'activePickerLock',
  'hostLeave',
  'is-portrait',
  'is-portrait-fullscreen',
  'is-idle',
]);

checkNone('hud.html 未残留设计稿顶条与宽高锁死回归', ttvHudHtml, [
  '.lab-header',
  '.spec-strip',
  'Math.max(frameEl.clientWidth',
]);

// 4. 跨进程 Named Pipe IPC、整剧队列与弹幕契约
checkAll('ipc.rs 兼容宿主历史命令别名并支持双工刷新写管道', ttvIpc, [
  'PipeWriter',
  'LoadRequest',
  'PipeCommand',
  'de_opt_u32_lossy',
  'de_opt_f64_lossy',
]);

checkAll('prefs.rs 包含队列、宿主弹幕单位换算及方向性尺寸策略', ttvPrefs, [
  'AutoplayQueue',
  'QueueEpisode',
  'area_from_host',
  'area_to_host',
  'speed_sec_from_host',
  'ideal_window_size_for_video',
]);

checkAll('ttv-player/src/main.rs 对齐宿主事件、补帧读数与弹幕身份去重', ttvMain, [
  '"loaded"',
  '"tracks"',
  '"fgChanged"',
  '"danmakuSettingsChanged"',
  '"displayHz"',
  '"outRatio"',
  'danmaku_seen_keys',
]);

// 5. 宿主集成、构建暂存与打包绑定（废弃 C# NativePlayer.exe）
const hostNative = requireFile('src-tauri/src/native_player.rs');
const hostMain = requireFile('src-tauri/src/main.rs');
const hostModels = requireFile('src-tauri/src/models.rs');
const tauriConf = requireFile('src-tauri/tauri.conf.json');
const buildRs = requireFile('src-tauri/build.rs');
const integrityPs1 = requireFile('scripts/ensure-integrity.ps1');
const gitignore = requireFile('.gitignore');
const taurignore = requireFile('src-tauri/.taurignore');

checkAll('native_player.rs 解析 ttv-player.exe、规范管道命名并剥离 WebView2 变量', hostNative, [
  'find_player_exe',
  'ttv-player.exe',
  'ttv-player-{}-{}',
  'strip_parent_webview2_env',
]);
checkNone('native_player.rs 不再引导旧 C# NativePlayer.exe 进程', hostNative, [
  'NativePlayer.exe',
]);

checkAll('tauri.conf.json / build.rs / ensure-integrity.ps1 已接入 resources/ttv-player 打包与暂存', tauriConf, [
  '"resources/ttv-player"',
]);
checkAll('ensure-integrity.ps1 自动编译并暂存 ttv-player.exe 与 libmpv-2.dll', integrityPs1, [
  'ttv-player.exe',
  'libmpv-2.dll',
  'resources\\ttv-player',
]);
checkAll('build.rs 同步预编译 ttv-player 资源并注册重跑触发', buildRs, [
  'ttv-player.exe',
  'resources/ttv-player',
]);
checkAll('.gitignore 与 .taurignore 均已忽略 ttv-player 暂存与构建目录', gitignore + '\n' + taurignore, [
  'resources/ttv-player/',
  'ttv-player/target/',
]);

// 6. 灰度全量关停 & 正式上线门禁（不影响自定义源与短剧推荐灰度）
const rolloutTs = requireFile('src/services/nativePlayerRollout.ts');
const ipcTs = requireFile('src/services/ipc.ts');
const settingsStore = requireFile('src/stores/useSettingsStore.tsx');
const settingsView = requireFile('src/components/views/SettingsView.tsx');
const playerControls = requireFile('src/components/player/PlayerControls.tsx');

assertCheck(
  'Rust native_player::NATIVE_PLAYER_AVAILABLE = true',
  /pub\s+const\s+NATIVE_PLAYER_AVAILABLE\s*:\s*bool\s*=\s*true\s*;/.test(hostNative),
  'Rust 总闸仍为 false',
);
assertCheck(
  '前端 nativePlayerRollout::NATIVE_PLAYER_AVAILABLE = true 且灰度原因为空',
  /export\s+const\s+NATIVE_PLAYER_AVAILABLE\s*=\s*true\s*;/.test(rolloutTs)
    && /export\s+const\s+NATIVE_PLAYER_GRAY_REASON\s*=\s*''\s*;/.test(rolloutTs),
  '前端总闸或灰度文案尚未清理',
);
assertCheck(
  'Rust UserSettings::native_player_enabled 默认 true',
  hostModels.includes('default_true') && hostModels.includes('native_player_enabled'),
  'models.rs 未将 native_player_enabled 默认值切为 true',
);
assertCheck(
  '前端 DEFAULT_SETTINGS.nativePlayerEnabled = true',
  /nativePlayerEnabled:\s*true\b/.test(ipcTs),
  'ipc.ts DEFAULT_SETTINGS.nativePlayerEnabled 非 true',
);
assertCheck(
  'Rust settings_save 允许直接保存 preferred_player = "mpv"（无二级灰度开关门槛）',
  hostMain.includes('"mpv" if crate::native_player::NATIVE_PLAYER_AVAILABLE => "mpv".into()')
    && !/\bsettings\.native_player_enabled\b[^=;]*[&|!]/.test(hostMain),
  'src-tauri/src/main.rs 仍存在二级灰度门槛',
);
assertCheck(
  '前端 useSettingsStore 允许直接使用 preferredPlayer = "mpv"',
  /preferredPlayer:\s*NATIVE_PLAYER_AVAILABLE\s*&&\s*merged\.preferredPlayer\s*===\s*'mpv'/.test(settingsStore),
  'useSettingsStore 仍要求二级开关或阻断 mpv',
);
assertCheck(
  'SettingsView 展示正式「MPV 播放器」选项并移除独立播放器灰度项',
  settingsView.includes("label: 'MPV 播放器'")
    && !settingsView.includes('重构中')
    && !settingsView.includes('独立 MPV 播放器（灰度）'),
  'SettingsView 仍残留灰度开关或「重构中」标签',
);
assertCheck(
  'PlayerControls 原生播放入口处于启用状态',
  /NATIVE_PLAYER_ENTRY_ENABLED\s*=\s*true\b/.test(playerControls),
  'NATIVE_PLAYER_ENTRY_ENABLED 非 true',
);

const playerBuildRs = requireFile('src-tauri/ttv-player/build.rs');
const playerMainRs = requireFile('src-tauri/ttv-player/src/main.rs');
const playerHudHtml = requireFile('src-tauri/ttv-player/assets/hud.html');
const playerIcoPath = resolve(ROOT, 'src-tauri/ttv-player/assets/ttv-player.ico');

assertCheck(
  '存在独立 MPV 播放器多分辨率项目图标 assets/ttv-player.ico',
  existsSync(playerIcoPath) && readFileSync(playerIcoPath).length >= 4096,
  '缺少或损坏的 assets/ttv-player.ico',
);
checkAll(
  'ttv-player build.rs 与 main.rs 完整绑定 Win32 图标、零等待无缝连播与小窗还原',
  `${playerBuildRs}\n${playerMainRs}`,
  ['ttv-player.ico', '1 ICON', 'WM_SETICON', 'EofAutoplayAction', 'prefetchNext', 'restoreWindow'],
);
checkAll(
  'ttv-player HUD 具备锁屏自动隐藏、控制栏弹幕开关、全量红果表情映射与 Crystal 小窗模式',
  playerHudHtml,
  [
    '.player-frame.is-locked.is-idle .lock-fab { opacity: 0;',
    'id="btnDanmaku"',
    'EMOTE_MAP',
    "'送花': '🌹'",
    'id="pipHud"',
    'classList.toggle(\'is-pip\'',
    'restoreWindow',
  ],
);

console.log(`\n[verify:native-player] 通过 ${passed} · 失败 ${failed}`);
if (failed > 0) {
  process.exit(1);
}
console.log('verify-native-player: ok');
