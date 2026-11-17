#!/usr/bin/env node
/**
 * verify-playback-routing —— 「选了 MPV 却还在应用内播」的判定表验收。
 *
 * ## 这条脚本钉住什么
 *
 * 判定收在 src/services/playbackRouting.ts::planPlaybackRoute（纯函数），
 * 但纯函数正确不等于入口会用它。所以这里做两件事：
 *
 *   1. 逐格驱动判定表：偏好 × 播放器在不在 × 界面形态 × 有没有地址，
 *      每一格的落点、原因、文案都必须与模块头注释里的表一致；
 *   2. 静态扫描入口：每个播放入口都必须调用 planPlaybackRoute（带自己的 surface），
 *      并保留「交不出去时对用户说一句」的证据 —— 谁把入口改回 if (launched) return;
 *      那种静默回落，这里立刻红。
 *
 * ## 判定表（与 playbackRouting.ts 头注释逐字对应）
 *
 * | 入口 | surface | 偏好 webview | 偏好 mpv 且地址可得 | 偏好 mpv 且交不出去 |
 * | --- | --- | --- | --- | --- |
 * | 详情页立即播放 / 选集 | detail | inline，不提示 | native | inline + 文案 |
 * | 发现页继续观看 / 随机看 | explore | inline，不提示 | native | inline + 文案 |
 * | 历史续播 | history | inline，不提示 | native | inline + 文案 |
 * | 主播放器「用 MPV 打开」 | player | 不适用 | native | inline + 文案 |
 * | 首页竖屏信息流 | feed | inline，不提示 | inline + 文案 | inline + 文案 |
 *
 * 「地址可得」= 有直链 / 有红果 vid / 来源 id 带冒号 / 本身是动漫，与
 * tryLaunchNativePlayer 的守卫同一口径（那里只是把同一个函数的结论翻译成布尔值）。
 *
 * 退出码：任何一条 ✗ → 1。
 *
 * 运行：node scripts/verify-playback-routing.mjs
 *      （package.json 里对应 npm run verify:playback-routing，由 Lead 统一注册）
 */

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');

let failed = 0;
function check(title, condition, detail = '') {
  if (condition) {
    console.log('✓ ' + title);
    return;
  }
  failed += 1;
  console.error('✗ ' + title + (detail ? ' — ' + detail : ''));
}

const bundled = await build({
  stdin: { contents: "export * from './playbackRouting';", resolveDir: resolve(ROOT, 'src/services') },
  bundle: true,
  platform: 'node',
  format: 'esm',
  write: false,
});
const api = await import('data:text/javascript;base64,' + Buffer.from(bundled.outputFiles[0].text).toString('base64'));

const plan = input => api.planPlaybackRoute(input);
function inlineOf(input) {
  const decision = plan(input);
  assert.equal(decision.kernel, 'inline', JSON.stringify(input));
  return decision;
}
function nativeOf(input) {
  const decision = plan(input);
  assert.equal(decision.kernel, 'native', JSON.stringify(input));
  return decision;
}

const addressable = { seriesId: '751234567890', episodeId: '751234567890' };
const base = { preferredPlayer: 'mpv', nativeAvailable: true };

/* ---------- 1. 偏好 webview：任何形态都不提示、不弹窗 ---------- */
for (const surface of [undefined, 'detail', 'explore', 'history', 'feed', 'player']) {
  const decision = inlineOf({ ...base, preferredPlayer: 'webview', ...addressable, surface });
  check(
    '偏好 webview（surface=' + String(surface) + '）落在应用内且不提示',
    decision.reason === 'webview-selected' && decision.notice === null,
    JSON.stringify(decision),
  );
}
for (const weird of [undefined, null, '', 'MPV', 'webview2', 1, {}]) {
  const decision = inlineOf({ ...base, preferredPlayer: weird, ...addressable });
  check(
    '非法偏好 ' + JSON.stringify(weird) + ' 归一成 webview（不弹窗、不提示）',
    decision.reason === 'webview-selected' && decision.notice === null,
    JSON.stringify(decision),
  );
}

/* ---------- 2. 偏好 mpv + 地址可得：非 feed 形态都走原生 ---------- */
for (const surface of [undefined, 'detail', 'explore', 'history', 'player']) {
  check(
    'mpv + 地址可得（surface=' + String(surface) + '）判 native',
    nativeOf({ ...base, ...addressable, surface }).reason === 'mpv-selected',
  );
}

/* ---------- 3. 「地址可得」的七种来源 ---------- */
const addressCases = [
  ['红果纯数字 id + 数字集号', { seriesId: '751234567890', episodeId: '751234567801' }],
  ['已解析直链（含首尾空格）', { seriesId: '751234567890', episodeId: 'ep-1', url: ' https://fixture.invalid/a.mp4 ' }],
  ['来源前缀 hg:', { seriesId: 'hg:6875', episodeId: '1' }],
  ['来源前缀 guo:', { seriesId: 'guo:site:1', episodeId: '9' }],
  ['动漫来源 dmghg:', { seriesId: 'dmghg:234', episodeId: 'abc' }],
  ['动漫来源 bfzy:', { seriesId: 'bfzy:345', episodeId: 'abc' }],
  ['纯数字剧 + type=anime（动漫靠来源解析）', { seriesId: '123456789012', episodeId: 'ep-1', type: 'anime' }],
];
for (const [name, input] of addressCases) {
  check('地址可得：' + name, api.hasNativeAddress(input) === true);
  nativeOf({ ...base, surface: 'detail', ...input });
}

/* ---------- 4. 地址不可得：必须 inline + 文案，不能静默 ---------- */
check('地址不可得：纯数字剧 + 非数字集号', api.hasNativeAddress({ seriesId: '751234567890', episodeId: 'ep-1' }) === false);
const noAddress = inlineOf({ ...base, ...addressable, episodeId: 'ep-1', surface: 'detail' });
check('mpv + 地址不可得 ⇒ inline / no-native-address', noAddress.reason === 'no-native-address', JSON.stringify(noAddress));
check('地址不可得必须带可读文案（已改用网页播放）', typeof noAddress.notice === 'string' && noAddress.notice.indexOf('已改用网页播放') >= 0, String(noAddress.notice));
check('NATIVE_SKIPPED_NOTICE 与 no-native-address 文案同源', api.NATIVE_SKIPPED_NOTICE === api.PLAYBACK_INLINE_NOTICES['no-native-address']);

/* ---------- 5. 总闸关闭：说明原因，不弹窗 ---------- */
const unavailable = inlineOf({ ...base, nativeAvailable: false, ...addressable });
check('mpv + 播放器不可用 ⇒ inline / native-unavailable + 文案', unavailable.reason === 'native-unavailable' && Boolean(unavailable.notice), JSON.stringify(unavailable));

/* ---------- 6. 信息流：设计上固定应用内，即使地址可得 ---------- */
const feed = inlineOf({ ...base, ...addressable, surface: 'feed' });
check('mpv + 信息流 ⇒ inline / feed-surface', feed.reason === 'feed-surface', JSON.stringify(feed));
check('信息流的文案与「没有地址」不是同一句（原因不同，说法不能混）', feed.notice !== api.PLAYBACK_INLINE_NOTICES['no-native-address'] && Boolean(feed.notice));
check('偏好 webview 时信息流不提示（用户自己的选择）', plan({ ...base, preferredPlayer: 'webview', ...addressable, surface: 'feed' }).notice === null);

/* ---------- 7. 只有 inline 才有 notice ---------- */
check('native 落点不携带 notice', api.playbackNoticeFor(nativeOf({ ...base, ...addressable })) === null);
check('playbackNoticeFor 与 decision.notice 一致', api.playbackNoticeFor(noAddress) === noAddress.notice);

/* ---------- 8. 静默回落源头：tryLaunchNativePlayer 只翻译判定 ---------- */
const preferred = readFileSync(resolve(ROOT, 'src/services/preferredPlayback.ts'), 'utf8');
check('tryLaunchNativePlayer 不再自带 NATIVE_PLAYER_AVAILABLE 的裸 return', preferred.indexOf('if (!NATIVE_PLAYER_AVAILABLE) return false;') < 0);
check('tryLaunchNativePlayer 不再自带 preferredPlayer === mpv 的裸 return', preferred.indexOf("normalizePreferredPlayer(input.preferredPlayer) !== 'mpv'") < 0);
check('tryLaunchNativePlayer 调用统一判定', preferred.indexOf('planPlaybackRoute({') >= 0);
check('详情口径的地址复查也走同一个函数', preferred.indexOf('hasNativeAddress({ ...input, type: detail.type })') >= 0);

/* ---------- 9. 播放入口静态扫描：谁都得先问落点 ---------- */
const ENTRIES = [
  ['src/components/views/DetailView.tsx', "surface: 'detail'", 'gate.started'],
  ['src/components/views/ExploreView.tsx', "surface: 'explore'", 'NATIVE_SKIPPED_NOTICE'],
  ['src/components/views/HistoryView.tsx', "surface: 'history'", 'NATIVE_SKIPPED_NOTICE'],
  ['src/stores/useFeedStore.ts', "surface: 'feed'", 'playbackNoticeFor'],
  ['src/components/player/PlayerControls.tsx', "surface: 'player'", 'planPlaybackRoute'],
];
for (const [file, surface, evidence] of ENTRIES) {
  const source = readFileSync(resolve(ROOT, file), 'utf8');
  check(file + ' 调用 planPlaybackRoute', source.indexOf('planPlaybackRoute(') >= 0);
  check(file + ' 声明自己的 surface（' + surface + '）', source.indexOf(surface) >= 0);
  check(file + ' 保留「交不出去要说一句」的证据（' + evidence + '）', source.indexOf(evidence) >= 0);
}

/* ---------- 10. 判定必须出现在回落动作之前 ---------- */
for (const file of ['src/components/views/ExploreView.tsx', 'src/components/views/HistoryView.tsx']) {
  const source = readFileSync(resolve(ROOT, file), 'utf8');
  check(file + ' 的回落分支在判定之后', source.indexOf('planPlaybackRoute(') < source.indexOf('if (launched) return;'));
}

if (failed) {
  console.error('verify-playback-routing: ' + failed + ' failed');
  process.exit(1);
}
console.log('verify-playback-routing: ok');
