#!/usr/bin/env node
/**
 * verify-native-address —— 「先取地址、再交给 MPV」的验收（动漫 / 黄果 / 云端 / 红果）。
 *
 * ## 这条脚本钉住什么
 *
 * 1. `needsNativeAddress` 的矩阵：只有「地址可得 + 没有现成直链 + 不是纯数字红果 id」
 *    才多一次解析；红果一次都不多。
 * 2. 动漫（dmghg:）与黄果（hg:）的启动序列：
 *    `series_detail` → `playback_open` → `native_player_open` → `playback_command(stop)`。
 *    **永远不出现 action='play'** —— 那次会话从创建到收尾没有被播放过，所以不存在
 *    第二路音频；stop 在 Rust 侧只做 `media_enhance::stop` 与 `guo_provider::release`，
 *    对动漫 / 黄果是空操作，对一条从未出画的会话没有副作用。
 * 3. 云端（guo:）的会话**不能立刻收**：地址由 guo-core 本地媒体服务按会话提���，
 *    收了 mpv 立刻断流。所以它记成 held：下一次解析顶掉、或窗口关闭时再收。
 * 4. 纯数字红果剧：`playback_open` 一次都不发（Rust 自己解析）。
 * 5. 只有试看片段 / 没有地址：抛可读错误，**不发** `native_player_open`（不静默）。
 *
 * 全部用桩 IPC，不连网、不启动窗口、不碰真实播放器。
 * 运行：node scripts/verify-native-address.mjs
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';

const calls = [];
let previewOnly = false;
let nativeFail = false;
const EPISODES = {
  'dmghg:234': ['ep-a1', 'ep-a2'],
  'bfzy:345': ['ep-b1'],
  'hg:6875': ['1', '2'],
  'guo:site:1': ['ep-g1', 'ep-g2'],
  '751234567890': ['751234567801', '751234567802'],
};
const TYPE_OF = seriesId => (seriesId.startsWith('dmghg:') || seriesId.startsWith('bfzy:') ? 'anime' : 'drama');
function detailFor(seriesId) {
  const ids = EPISODES[seriesId] ?? ['ep-x1'];
  return {
    id: seriesId,
    title: '剧 ' + seriesId,
    type: TYPE_OF(seriesId),
    episodes: ids.map((id, index) => ({ id, seriesId, episodeNumber: index + 1, title: '第' + (index + 1) + '集', durationSeconds: 120 })),
  };
}
globalThis.window = { __TAURI_INTERNALS__: {} };
globalThis.localStorage = { getItem: () => null, setItem: () => {}, removeItem: () => {} };
globalThis.__addrInvoke = async (command, args) => {
  calls.push({ command, args });
  if (command === 'series_detail') return detailFor(args.seriesId);
  if (command === 'playback_open') {
    return {
      sessionId: args.input.sessionId,
      seriesId: args.input.seriesId,
      episodeId: args.input.episodeId,
      position: 0,
      quality: 'auto',
      url: 'https://fixture.invalid/' + args.input.seriesId + '/' + args.input.episodeId + '.m3u8',
      streamKind: 'hls',
      previewOnly,
    };
  }
  if (command === 'native_player_open') {
    if (nativeFail) throw new Error('mpv 起不来');
    return { alive: true, episodeId: args.input.episodeId };
  }
  if (command === 'native_player_queue_push') return { alive: true };
  return undefined;
};
globalThis.__addrListen = async () => () => {};

const bundled = await build({
  stdin: {
    contents: "export * from './preferredPlayback'; export * from './nativeAddress';",
    resolveDir: fileURLToPath(new URL('../src/services/', import.meta.url)),
  },
  bundle: true,
  platform: 'node',
  format: 'esm',
  write: false,
  plugins: [{
    name: '地址解析台架',
    setup(builder) {
      builder.onResolve({ filter: /^@tauri-apps\/api\/(core|event)$/ }, args => ({ path: args.path, namespace: 'fixture' }));
      builder.onLoad({ filter: /.*/, namespace: 'fixture' }, args => ({
        contents: args.path.endsWith('/core')
          ? 'export const invoke = (...args) => globalThis.__addrInvoke(...args);'
          : 'export const listen = (...args) => globalThis.__addrListen(...args);',
      }));
    },
  }],
});
const api = await import('data:text/javascript;base64,' + Buffer.from(bundled.outputFiles[0].text).toString('base64'));
const flush = async () => { for (let i = 0; i < 12; i += 1) await new Promise(resolve => setImmediate(resolve)); };
// trace_ui_log 是链路自带的诊断打点，不属于播放动作：断言只看播放动作。
const LOG_COMMANDS = new Set(['trace_ui_log']);
const commands = () => calls.map(call => call.command).filter(command => !LOG_COMMANDS.has(command));
const stopCalls = () => calls.filter(call => call.command === 'playback_command');
const plays = () => calls.filter(call => call.command === 'playback_command' && call.args.action === 'play');

test('needsNativeAddress：只有非红果 / 动漫 / 无直链才解析', () => {
  const cases = [
    [{ seriesId: '751234567890', episodeId: '751234567801' }, false, '纯数字红果 id'],
    [{ seriesId: '751234567890', episodeId: 'ep-1', type: 'anime' }, true, '纯数字动漫剧'],
    [{ seriesId: 'dmghg:234', episodeId: 'ep-a1' }, true, '动漫来源 dmghg:'],
    [{ seriesId: 'bfzy:345', episodeId: 'ep-b1' }, true, '动漫兜底 bfzy:'],
    [{ seriesId: 'hg:6875', episodeId: '1' }, true, '黄果 hg:'],
    [{ seriesId: 'guo:site:1', episodeId: 'ep-g1' }, true, '云端 guo:'],
    [{ seriesId: 'hg:6875', episodeId: '1', url: '  https://x/y.mp4  ' }, false, '已有直链'],
    [{ seriesId: '751234567890', episodeId: 'ep-1' }, false, '地址不可得（planner 已拦）'],
  ];
  for (const [input, expected, label] of cases) {
    assert.equal(api.needsNativeAddress(input), expected, label);
  }
});

test('动漫：解析出地址再交给 MPV，随后立刻收掉会话，且从不播放', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: 'dmghg:234', episodeId: 'ep-a1', startSeconds: 12 }), true);
  await flush();
  assert.deepEqual(commands(), ['series_detail', 'playback_open', 'native_player_open', 'playback_command']);
  const open = calls.find(call => call.command === 'native_player_open').args.input;
  assert.equal(open.url, 'https://fixture.invalid/dmghg:234/ep-a1.m3u8', '必须把解析出的直链交给 mpv');
  assert.equal(open.vid, '0', '动漫没有红果 vid，vid 只能是占位值');
  const stop = stopCalls()[0];
  assert.equal(stop.args.action, 'stop');
  assert.equal(stop.args.sessionId, calls.find(call => call.command === 'playback_open').args.input.sessionId, '收掉的必须是同一次解析会话');
  assert.equal(plays().length, 0, '这次会话从未被播放，不该出现 play');
});

test('黄果：与动漫同一口径（解析、启动、收尾）', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: 'hg:6875', episodeId: '1', startSeconds: 42 }), true);
  await flush();
  assert.deepEqual(commands(), ['series_detail', 'playback_open', 'native_player_open', 'playback_command']);
  assert.equal(calls.find(call => call.command === 'native_player_open').args.input.url, 'https://fixture.invalid/hg:6875/1.m3u8');
  assert.equal(plays().length, 0);
});

test('云端：会话随窗口存活，先不收；被下一次解析顶掉时才收', async () => {
  calls.length = 0;
  await api.releaseHeldNativeAddress();
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: 'guo:site:1', episodeId: 'ep-g1' }), true);
  await flush();
  assert.deepEqual(commands(), ['series_detail', 'playback_open', 'native_player_open'], 'guo 的会话不能立刻 stop');
  const firstSession = calls.find(call => call.command === 'playback_open').args.input.sessionId;
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: 'guo:site:1', episodeId: 'ep-g2' }), true);
  await flush();
  const superseded = stopCalls();
  assert.equal(superseded.length, 1, '顶替时必须收掉上一条 held 会话');
  assert.equal(superseded[0].args.sessionId, firstSession);
  assert.equal(plays().length, 0);
  calls.length = 0;
  await api.releaseHeldNativeAddress();
  assert.equal(stopCalls().length, 1, '窗口关闭时收掉当前 held 会话');
});

test('云端启动失败时：不延寿，立刻收掉会话（不留后台空会话）', async () => {
  calls.length = 0;
  await api.releaseHeldNativeAddress();
  calls.length = 0;
  nativeFail = true;
  try {
    await assert.rejects(
      api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: 'guo:site:1', episodeId: 'ep-g1' }),
      /mpv 起不来/,
    );
  } finally {
    nativeFail = false;
  }
  await flush();
  assert.equal(calls.some(call => call.command === 'native_player_open'), true, '确实尝试过启动');
  assert.equal(stopCalls().length, 1, '启动失败也要收掉 guo 会话，不能留在后台');
  assert.equal(plays().length, 0);
});
test('纯数字红果剧：一次 playback_open 都不发（交给 Rust 解析）', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: '751234567890', episodeId: '751234567801' }), true);
  await flush();
  assert.deepEqual(commands(), ['series_detail', 'native_player_open'], '红果不许被前端多解析一次');
  const open = calls.find(call => call.command === 'native_player_open').args.input;
  assert.equal(open.url, undefined, '红果不带直链，由 Rust 用 vid 解析');
  assert.equal(open.vid, '751234567801');
});

test('只有试看片段时：抛可读错误、不发 native_player_open、并收掉会话', async () => {
  calls.length = 0;
  previewOnly = true;
  try {
    await assert.rejects(
      api.tryLaunchNativePlayer({ preferredPlayer: 'mpv', seriesId: 'bfzy:345', episodeId: 'ep-b1' }),
      /没有可播放的整集地址|未返回可播放的整集地址/,
    );
  } finally {
    previewOnly = false;
  }
  await flush();
  assert.equal(calls.some(call => call.command === 'native_player_open'), false, '没有地址就绝不启动空白窗口');
  assert.equal(stopCalls().length, 1, '失败路径也要收掉刚建的会话，不能留空会话');
});

test('入口接线：三个入口都经由 tryLaunchNativePlayer 走这一层', () => {
  const files = [
    'src/components/views/DetailView.tsx',
    'src/components/views/ExploreView.tsx',
    'src/components/views/HistoryView.tsx',
  ];
  for (const file of files) {
    const source = readFileSync(new URL('../' + file, import.meta.url), 'utf8');
    assert.ok(source.includes('tryLaunchNativePlayer('), file + ' 必须走统一原生入口');
  }
  const preferred = readFileSync(new URL('../src/services/preferredPlayback.ts', import.meta.url), 'utf8');
  assert.ok(preferred.includes('resolveNativeAddress('), '原生启动必须先取地址');
  assert.ok(preferred.includes('releaseNativeAddress('), '取地址的会话必须收尾');
});
