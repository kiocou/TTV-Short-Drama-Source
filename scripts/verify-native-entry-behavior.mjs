// 执行生产入口与导航桥；IPC/事件均为本地桩，不打开窗口或访问网络。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';

const calls = [];
const listeners = new Map();
let pendingResolve;
let rejectDetail = false;
const detailFor = seriesId => ({
  id: seriesId, title: `剧 ${seriesId}`, type: seriesId.startsWith('dmghg:') || seriesId === '123456789012' ? 'anime' : 'drama',
  episodes: Array.from({ length: 8 }, (_, index) => ({
    id: String(123456780 + index), seriesId, episodeNumber: index + 1, title: `第${index + 1}集`, durationSeconds: 120,
  })),
});
globalThis.window = { __TAURI_INTERNALS__: {} };
globalThis.localStorage = { getItem: () => 'dark' };
globalThis.__entryInvoke = async (command, args) => {
  calls.push({ command, args });
  if (command === 'series_detail') {
    if (rejectDetail) throw new Error('详情不可用');
    return detailFor(args.seriesId);
  }
  if (command === 'playback_open') {
    if (pendingResolve) return pendingResolve;
    return { ...args.input, url: `https://fixture.invalid/${args.input.seriesId}/${args.input.episodeId}.mp4` };
  }
  if (command === 'native_player_open') return { alive: true, episodeId: args.input.episodeId };
  if (command === 'short_drama_app_resolve') return { playUrl: 'C:/fixture/episode.mp4' };
};
globalThis.__entryListen = async (name, callback) => {
  assert.equal(listeners.has(name), false, `事件 ${name} 不得重复订阅`);
  listeners.set(name, callback);
  return () => listeners.delete(name);
};
const bundle = await build({
  stdin: { contents: `export * from './preferredPlayback'; export * from './nativeNavigate'; export * from './playbackRouting';`, resolveDir: fileURLToPath(new URL('../src/services/', import.meta.url)) },
  bundle: true, platform: 'node', format: 'esm', write: false,
  plugins: [{ name: '入口台架', setup(builder) {
    builder.onResolve({ filter: /^@tauri-apps\/api\/(core|event)$/ }, args => ({ path: args.path, namespace: 'fixture' }));
    builder.onLoad({ filter: /.*/, namespace: 'fixture' }, args => ({ contents: args.path.endsWith('/core')
      ? 'export const invoke = (...args) => globalThis.__entryInvoke(...args);'
      : 'export const listen = (...args) => globalThis.__entryListen(...args);' }));
  } }],
});
const api = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`);
const flush = async () => { for (let i = 0; i < 12; i += 1) await new Promise(resolve => setImmediate(resolve)); };
const open = (seriesId, episodeIndex = 5) => api.tryLaunchNativePlayer({
  preferredPlayer: 'mpv', seriesId, episodeId: detailFor(seriesId).episodes[episodeIndex].id,
  startSeconds: 31, autoNext: false,
});

test('四来源首选入口保留全集、后半段索引和来源身份；不等下一集解析', async () => {
  for (const seriesId of ['751234567890', 'hg:100', 'guo:100', 'dmghg:100']) {
    calls.length = 0;
    assert.equal(await open(seriesId), true);
    const input = calls.find(call => call.command === 'native_player_open').args.input;
    assert.equal(input.queue?.length, 8, `${seriesId} 必须带整部剧`);
    assert.equal(input.index, 5);
    assert.equal(input.totalEpisodes, 8);
    assert.equal(input.autoplay, false);
    assert.deepEqual(input.queue.map(item => item.index), [0, 1, 2, 3, 4, 5, 6, 7]);
    assert.ok(input.queue.every(item => item.seriesId === seriesId && item.episodeId));
    assert.ok(input.queue.every(item => item.url === ''));
    assert.equal(input.queue[5].start, 31);
    if (seriesId.includes(':')) assert.ok(input.queue.every(item => item.vid === undefined));
    assert.equal(calls.filter(call => call.command === 'short_drama_app_resolve').length, 0);
  }
});

test('独立首选入口收到need-next时按当前剧解析并携带身份推队列', async () => {
  await open('hg:bridge');
  await flush();
  calls.length = 0;
  assert.ok(listeners.has('native-player-need-next'), '首选入口必须拥有导航桥');
  listeners.get('native-player-need-next')({ payload: { index: 6, ep: 7 } });
  await flush();
  const pushed = calls.find(call => call.command === 'native_player_queue_push')?.args.item;
  assert.equal(pushed?.seriesId, 'hg:bridge');
  assert.equal(pushed?.episodeId, '123456786');
  assert.equal(pushed?.index, 6);
});

test('旧剧异步解析迟到时不得推入新剧队列', async () => {
  await open('hg:old');
  await flush();
  let resolveOld;
  pendingResolve = new Promise(resolve => { resolveOld = resolve; });
  assert.ok(listeners.has('native-player-need-next'));
  const before = calls.length;
  listeners.get('native-player-need-next')({ payload: { index: 7, ep: 8 } });
  await flush();
  assert.ok(calls.slice(before).some(call => call.command === 'playback_open'), '确实发出旧请求后才换剧');
  pendingResolve = undefined;
  await open('guo:new');
  calls.length = 0;
  resolveOld({ url: 'https://fixture.invalid/old.mp4' });
  await flush();
  assert.equal(calls.some(call => call.command === 'native_player_queue_push'), false);
});

test('首选入口旧直链迟到时不会覆盖后一次点击打开的新剧', async () => {
  let resolveOld;
  pendingResolve = new Promise(resolve => { resolveOld = resolve; });
  const oldOpen = open('hg:slow-open');
  await flush();
  pendingResolve = undefined;
  calls.length = 0;
  await open('guo:fast-open');
  resolveOld({ url: 'https://fixture.invalid/old.mp4' });
  await oldOpen;
  const opens = calls.filter(call => call.command === 'native_player_open');
  assert.deepEqual(opens.map(call => call.args.input.seriesId), ['guo:fast-open']);
});

test('全剧队列共享构造保留手动后半段与最后一集以前的元数据', () => {
  const detail = detailFor('hg:manual');
  for (const currentIndex of [5, 7]) {
    const queue = api.buildInitialNativeQueue({
      seriesId: detail.id, seriesTitle: detail.title, seriesType: detail.type,
      episodes: detail.episodes, currentIndex, startSeconds: 27,
    });
    assert.equal(queue.length, 8);
    assert.equal(queue[currentIndex].start, 27);
    assert.deepEqual(queue.map(item => item.index), [0, 1, 2, 3, 4, 5, 6, 7]);
  }
});

test('纯数字动漫剧首开与连播均走动漫来源，队列无红果vid', async () => {
  calls.length = 0;
  await open('123456789012');
  await flush();
  const input = calls.find(call => call.command === 'native_player_open').args.input;
  assert.equal(input.contentKind, 'anime');
  assert.ok(input.queue.every(item => item.contentKind === 'anime' && item.vid === undefined));
  const first = calls.find(call => call.command === 'playback_open');
  assert.equal(first.args.input.isAnime, true);
  calls.length = 0;
  listeners.get('native-player-need-next')({ payload: { seriesId: '123456789012', index: 6, ep: 7 } });
  await flush();
  assert.equal(calls.find(call => call.command === 'playback_open')?.args.input.isAnime, true);
  assert.equal(calls.some(call => call.command === 'short_drama_app_resolve'), false);
});

test('迟到旧来源事件和播放页重新挂载不会夺走首选入口上下文', async () => {
  await open('hg:current');
  await flush();
  api.registerNativeNavigateHost({ getSeriesId: () => 'hg:stale' });
  calls.length = 0;
  listeners.get('native-player-need-next')({ payload: { seriesId: 'hg:old', index: 6, ep: 7 } });
  listeners.get('native-player-navigate')({ payload: { seriesId: 'hg:old', dir: 'prev' } });
  listeners.get('native-player-advance')({ payload: { seriesId: 'hg:old', index: 1, ep: 2 } });
  listeners.get('native-player-queue-timeout')({ payload: { seriesId: 'hg:old', index: 6, ep: 7 } });
  await flush();
  assert.equal(calls.length, 0);
  listeners.get('native-player-need-next')({ payload: { seriesId: 'hg:current', index: 6, ep: 7 } });
  await flush();
  assert.equal(calls.find(call => call.command === 'native_player_queue_push')?.args.item.seriesId, 'hg:current');
  calls.length = 0;
  listeners.get('native-player-navigate')({ payload: { seriesId: 'hg:current', dir: 'prev' } });
  await flush();
  assert.equal(calls.find(call => call.command === 'native_player_send')?.args.command.index, 4, '旧advance不能修改当前剧集索引');
});

test('原生上一集/下一集/指定选集jump带历史身份，保留用户关闭联播的选择', async () => {
  await open('hg:navigate');
  await flush();
  calls.length = 0;
  listeners.get('native-player-navigate')({ payload: { seriesId: 'hg:navigate', dir: 'prev' } });
  await flush();
  const load = calls.find(call => call.command === 'native_player_send')?.args.command;
  assert.equal(load?.seriesId, 'hg:navigate');
  assert.equal(load?.episodeId, '123456784');
  assert.equal(load?.index, 4);
  assert.equal(load?.autoplay, undefined);

  calls.length = 0;
  listeners.get('native-player-navigate')({ payload: { seriesId: 'hg:navigate', dir: 'jump', index: 1 } });
  await flush();
  const jumpLoad = calls.find(call => call.command === 'native_player_send')?.args.command;
  assert.equal(jumpLoad?.seriesId, 'hg:navigate');
  assert.equal(jumpLoad?.episodeId, '123456781');
  assert.equal(jumpLoad?.index, 1);
  assert.equal(jumpLoad?.vid, undefined, '非红果剧集不注入 vid');
});

test('MPV 偏好但这一集没有地址时：不启动原生窗口，判定给出可读原因', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({
    preferredPlayer: 'mpv', seriesId: '751234567890', episodeId: 'ep-1', startSeconds: 3,
  }), false);
  assert.equal(calls.length, 0, '判定阶段就返回，不该发任何 IPC');
  const route = api.planPlaybackRoute({
    preferredPlayer: 'mpv', nativeAvailable: true, seriesId: '751234567890', episodeId: 'ep-1', surface: 'detail',
  });
  assert.equal(route.kernel, 'inline');
  assert.equal(route.reason, 'no-native-address');
  assert.ok(route.notice && route.notice.indexOf('已改用网页播放') >= 0, '回落必须带可读原因');
});

test('信息流形态永不启动独立窗口（判定写在入口之前）', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({
    preferredPlayer: 'mpv', surface: 'feed', seriesId: '751234567890', episodeId: '751234567801',
  }), false);
  assert.equal(calls.length, 0);
});

test('偏好为网页播放器时不弹窗、也不提示', async () => {
  calls.length = 0;
  assert.equal(await api.tryLaunchNativePlayer({
    preferredPlayer: 'webview', seriesId: '751234567890', episodeId: '751234567801',
  }), false);
  assert.equal(calls.length, 0);
  assert.equal(api.planPlaybackRoute({
    preferredPlayer: 'webview', nativeAvailable: true, seriesId: '751234567890', episodeId: '751234567801',
  }).notice, null);
});

test('首选详情获取失败明确拒绝，不打开只有单集的窗口', async () => {
  rejectDetail = true;
  calls.length = 0;
  try { await assert.rejects(open('hg:no-detail'), /详情不可用/); }
  finally { rejectDetail = false; }
  assert.equal(calls.some(call => call.command === 'native_player_open'), false);
});
