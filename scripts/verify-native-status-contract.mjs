/**
 * 原生播放器状态契约（TS 侧）的一致性验收。
 *
 * 为什么需要：`NativePlayerStatus` 是**跨语言**协议（C# `progress` →
 * Rust `PlayerState`（serde camelCase）→ 前端这个接口）。字段名写错或漏在默认对象
 * 里，失败形态是"界面永远显示未开启"，没有任何报错。Rust 侧的键名由
 * `native_player.rs::fg_contract_tests::player_state_serializes_capability_as_camel_case`
 * 钉住，这里钉 TS 这一半：默认对象必须带齐同名的字段，且类型正确。
 *
 * 运行：`node scripts/verify-native-status-contract.mjs`
 * （内部用 esbuild 现场打包 `src/services/ipc.ts`，不依赖仓库里的残留产物。）
 */
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';

const root = fileURLToPath(new URL('..', import.meta.url));
const outfile = path.join(root, '.verify-p0', 'ipc-contract.mjs');
fs.mkdirSync(path.dirname(outfile), { recursive: true });
await build({
  entryPoints: [path.join(root, 'src', 'services', 'ipc.ts')],
  bundle: true,
  format: 'esm',
  platform: 'node',
  outfile,
  logLevel: 'silent',
});

// 非 Tauri 环境：`nativeStatus()` 会走"没有播放器"的默认分支。
globalThis.window = {};
globalThis.localStorage = { getItem: () => null, setItem: () => undefined };

const { ipcService } = await import(`file://${outfile.replace(/\\/g, '/')}`);

let failures = 0;
const check = (name, condition) => {
  if (!condition) {
    failures += 1;
    console.error(`FAIL ${name}`);
  }
};

const status = await ipcService.playback.nativeStatus();

// ── 1. 补帧能力四字段：名字与类型都对 ────────────────────────────────────
check('有 fgEngine', typeof status.fgEngine === 'string');
check('fgEngine 默认 off', status.fgEngine === 'off');
check('有 displaySync', typeof status.displaySync === 'boolean');
check('displaySync 默认 false', status.displaySync === false);
check('有 displayHz', typeof status.displayHz === 'number');
check('displayHz 默认 0', status.displayHz === 0);
check('有 outFps', typeof status.outFps === 'number');
check('outFps 默认 0', status.outFps === 0);
check('有 outRatio', typeof status.outRatio === 'number');
check('outRatio 默认 1（不是 0，0 会被读成"节奏停了"）', status.outRatio === 1);
check('有 paused', typeof status.paused === 'boolean');
check('paused 默认 false', status.paused === false);

// ── 2. 旧字段仍完整（新增字段不能挤掉兼容面）────────────────────────────
for (const key of ['alive', 'position', 'duration', 'cachedUntil', 'phase', 'failure', 'fgMult', 'fgEffectiveMult', 'exit', 'episodeId']) {
  check(`保留旧字段 ${key}`, key in status);
}
check('fgMult 默认 0（自动匹配屏幕原生刷新率）', status.fgMult === 0);
check('fgMode 默认 auto', status.fgMode === 'auto');
check('fgModeEffective 默认 drama', status.fgModeEffective === 'drama');

// ── 3. 键集与 Rust 序列化同一套 camelCase（多余/缺失都算漂移）─────────────
const capabilityKeys = ['fgEngine', 'fgMode', 'fgModeEffective', 'displaySync', 'displayHz', 'outFps', 'outRatio', 'paused'];
const missing = capabilityKeys.filter((key) => !(key in status));
check(`默认对象缺字段：${missing.join(',') || '无'}`, missing.length === 0);

// ── 4. 引擎名枚举与 C# `GetFgEngine()` 对齐（防止有人写成 2X/3X）─────────
const ALLOWED = ['off', 'gpu-optical-flow', 'gpu-panel-bypass', 'gpu-interp-inactive'];
check('默认引擎名在枚举内', ALLOWED.includes(status.fgEngine));

if (failures > 0) {
  console.error(`verify-native-status-contract: ${failures} 条失败`);
  process.exit(1);
}
console.log('verify-native-status-contract: ok');