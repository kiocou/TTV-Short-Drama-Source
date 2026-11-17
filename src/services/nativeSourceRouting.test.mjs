// 运行：node src/services/nativeSourceRouting.test.mjs；只模拟 IPC，不启动播放器或访问网络。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';

const calls = [];
let sessionUrl = 'http://127.0.0.1:12345/stream?fixture=vod';
globalThis.window = { __TAURI_INTERNALS__: {} };
globalThis.localStorage = { getItem: () => 'dark' };
globalThis.__nativeRoutingInvoke = async (command, args) => {
  calls.push({ command, args });
  if (command === 'series_detail') return {
    id: args.seriesId, title: '测试剧', type: 'drama',
    episodes: [{ id: args.seriesId === 'hg:6875' ? '1' : '751234567999', episodeNumber: 1 }],
  };
  if (command === 'playback_open') return { ...args.input, url: sessionUrl, streamKind: 'hls' };
  if (command === 'native_player_open') return { alive: true, episodeId: args.input.episodeId };
  if (command === 'short_drama_app_resolve') return { playUrl: 'C:/fixture/episode.mp4', width: 720, height: 1280 };
  if (command === 'short_drama_app_danmaku') return { items: [], total: 0 };
  return undefined;
};
const bundled = await build({
  stdin: {
    contents: `export { ipcService } from './ipc';
      export { tryLaunchNativePlayer, extractHongguoVid } from './preferredPlayback';
      export { buildNativeQueueEntry } from './nativeNavigate';`,
    resolveDir: fileURLToPath(new URL('.', import.meta.url)),
  },
  bundle: true,
  platform: 'node',
  format: 'esm',
  write: false,
  plugins: [{
    name: '本地IPC台架',
    setup(builder) {
      builder.onResolve({ filter: /^@tauri-apps\/api\/core$/ }, () => ({ path: 'ipc', namespace: 'fixture' }));
      builder.onLoad({ filter: /.*/, namespace: 'fixture' }, () => ({
        contents: 'export const invoke = (...args) => globalThis.__nativeRoutingInvoke(...args);',
      }));
    },
  }],
});
const api = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString('base64')}`);

test('非红果系列的数字分集不提取成红果 vid', () => {
  for (const seriesId of ['hg:6875', 'guo:site:123', 'dmghg:234', 'bfzy:345']) {
    assert.equal(api.extractHongguoVid('123456789', seriesId), null, seriesId);
  }
  assert.equal(api.extractHongguoVid('7703500213339573272', '7703500187427146814'), '7703500213339573272');
});

test('设置 MPV 后，黄果第1集先取本源地址再启动原生窗口，并立刻收掉取地址会话', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({
    preferredPlayer: 'mpv', seriesId: 'hg:6875', episodeId: '1', startSeconds: 42,
  }), true);
  // trace_ui_log 是链路自带的诊断打点，不属于播放动作。
  const verbs = calls.map(call => call.command).filter(command => command !== 'trace_ui_log');
  assert.deepEqual(
    verbs,
    ['series_detail', 'playback_open', 'native_player_open', 'playback_command'],
    '黄果必须「取地址 → 启动 → 收尾」三步都走到',
  );
  const [detailCall, openCall, nativeCall, stopCall] = calls.filter(call => call.command !== 'trace_ui_log');
  assert.equal(detailCall.command, 'series_detail');
  assert.equal(openCall.args.input.seriesId, 'hg:6875');
  assert.equal(openCall.args.input.episodeId, '1');
  assert.equal(nativeCall.args.input.url, sessionUrl, 'mpv 必须拿到解析出来的地址');
  assert.equal(nativeCall.args.input.startSeconds, 42);
  assert.equal(stopCall.args.action, 'stop');
  assert.equal(stopCall.args.sessionId, openCall.args.input.sessionId, '收掉的必须是这次取地址的会话');
  assert.equal(
    calls.some(call => call.command === 'playback_command' && call.args.action === 'play'),
    false,
    '取地址的会话从未被播放，不该出现 play',
  );
});

test('手动原生播放入口与动漫入口共用地址分流', async () => {
  for (const input of [
    { vid: '1', seriesId: 'hg:6875', episodeId: '1' },
    { vid: '123456', seriesId: '123456', episodeId: 'ep-1', contentKind: 'anime' },
  ]) {
    calls.length = 0;
    await api.ipcService.playback.nativeOpen(input);
    assert.deepEqual(calls.map(call => call.command), ['playback_open', 'native_player_open']);
    assert.equal(calls[1].args.input.url, sessionUrl);
  }
});

test('已有直链及红果入口不新增解析请求', async () => {
  for (const input of [
    { vid: '1', seriesId: 'hg:6875', episodeId: '1', url: 'https://example.test/local.mp4' },
    { vid: '7703500213339573272', seriesId: '7703500187427146814' },
  ]) {
    calls.length = 0;
    await api.ipcService.playback.nativeOpen(input);
    assert.deepEqual(calls.map(call => call.command), ['native_player_open']);
  }
});

test('本源没有地址时不得启动空白原生窗口', async () => {
  calls.length = 0;
  sessionUrl = '';
  try {
    await assert.rejects(api.ipcService.playback.nativeOpen({ vid: '1', seriesId: 'hg:6875', episodeId: '1' }));
    assert.deepEqual(calls.map(call => call.command), ['playback_open']);
  } finally {
    sessionUrl = 'http://127.0.0.1:12345/stream?fixture=vod';
  }
});

test('黄果换集解析走本源，带地址的数字集号可以入队', async () => {
  calls.length = 0;
  const resolved = await api.ipcService.playback.resolveNative('hg:6875', '2');
  assert.equal(resolved.playUrl, sessionUrl);
  assert.deepEqual(calls.map(call => call.command), ['playback_open']);
  const entry = api.buildNativeQueueEntry({
    seriesId: 'hg:6875', episodeId: '2', url: resolved.playUrl,
    episodeNumber: 2, title: '测试剧', total: 10, index: 1,
  });
  assert.ok(entry);
  assert.equal(entry.seriesId, 'hg:6875');
  assert.equal(entry.episodeId, '2');
  assert.equal(entry.vid, undefined);
});

test('MPV 偏好但这一集没有可交出去的地址：判为应用内，且不发解析请求', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({
    preferredPlayer: 'mpv', seriesId: '1234567890', episodeId: 'ep-7',
  }), false);
  assert.deepEqual(calls, []);
});
test('原生启动不强制重置播放器已有的补帧倍率', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({
    preferredPlayer: 'mpv',
    seriesId: '751234567890',
    episodeId: '751234567999',
  }), true);
  const open = calls.find(call => call.command === 'native_player_open');
  assert.ok(open);
  assert.equal(open.args.input.fgMult, undefined,
    '未显式选档时不得再塞 fgMult=2，覆盖播放器当前的 3X–5X');
});

test('非红果内容不会发送红果弹幕请求', async () => {
  calls.length = 0;
  const page = await api.ipcService.playback.danmaku('hg:6875', '1', 0, 30000);
  assert.deepEqual(page.items, []);
  assert.deepEqual(calls, []);
});
