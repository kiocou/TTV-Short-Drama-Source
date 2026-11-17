/**
 * 推荐页元数据延迟修复的**异步编排**验收。
 *
 * 这里跑的是 `src/stores/useFeedStore.ts` 的真实 hook 代码与真实 `feedQueue` /
 * `feedPrefetch` / `feedRank` / `feedSeriesNav`，只在 esbuild 阶段换掉**依赖**：
 *
 * - `react` → 本文件里的迷你 hook 运行时（只在测试进程里存在，用于驱动 useState /
 *   useRef / useCallback / useEffect，**不是** store 逻辑的第二份实现）；
 * - `services/ipc` → 可编排的取数替身（模拟页、详情、标签、失败、卡住）；
 * - `useAppStore` / `useFavoritesStore` / `usePlaybackStore` → 最小替身；
 * - `@tauri-apps/api/core` → 捕获 `tracePlayback` 打点（用来断言日志只含固定阶段名）。
 *
 * 所有延时都是**模拟值**（`SIM`，毫秒级），用来表达"网慢/标签慢"的先后次序，
 * 不代表真实网络或后端耗时；真实起播等待仍由媒体链路决定，不在本验收范围内。
 */

import { build } from 'esbuild';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, resolve as resolvePath } from 'node:path';

const ROOT = resolvePath(dirname(fileURLToPath(import.meta.url)), '..');

/** 模拟计时（不是真实耗时）。 */
const SIM = { pageMs: 1, detailMs: 1, tagMs: 1, openMs: 1 };

const checks = [];
const check = (name, passed) => checks.push([name, passed]);
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
};

// ───────────────────────────────────────────────────────────────────────────
// 迷你 hook 运行时（依赖替身，不是被测逻辑）
// ───────────────────────────────────────────────────────────────────────────

function createHookRuntime() {
  let component = null;
  let slots = [];
  let cursor = 0;
  let pendingEffects = [];
  let needsRender = false;
  let current = null;

  const depsChanged = (prev, next) => {
    if (prev === undefined || !next) return true;
    if (prev.length !== next.length) return true;
    for (let index = 0; index < prev.length; index += 1) {
      if (!Object.is(prev[index], next[index])) return true;
    }
    return false;
  };

  const useState = (initial) => {
    const index = cursor;
    cursor += 1;
    if (index >= slots.length) {
      slots.push({ value: typeof initial === 'function' ? initial() : initial });
    }
    const slot = slots[index];
    const setState = (next) => {
      const value = typeof next === 'function' ? next(slot.value) : next;
      if (Object.is(value, slot.value)) return;
      slot.value = value;
      needsRender = true;
    };
    return [slot.value, setState];
  };

  const useRef = (initial) => {
    const index = cursor;
    cursor += 1;
    if (index >= slots.length) slots.push({ current: initial });
    return slots[index];
  };

  const useCallback = (fn, deps) => {
    const index = cursor;
    cursor += 1;
    if (index >= slots.length) slots.push({ fn, deps });
    const slot = slots[index];
    if (depsChanged(slot.deps, deps)) {
      slot.fn = fn;
      slot.deps = deps;
    }
    return slot.fn;
  };

  const useEffect = (fn, deps) => {
    const index = cursor;
    cursor += 1;
    if (index >= slots.length) slots.push({ deps: undefined, cleanup: undefined, ran: false });
    const slot = slots[index];
    if (!slot.ran || depsChanged(slot.deps, deps)) pendingEffects.push({ slot, fn, deps });
  };

  const renderOnce = () => {
    cursor = 0;
    pendingEffects = [];
    const result = component();
    const effects = pendingEffects;
    pendingEffects = [];
    for (const { slot, fn, deps } of effects) {
      if (typeof slot.cleanup === 'function') {
        const cleanup = slot.cleanup;
        slot.cleanup = undefined;
        try { cleanup(); } catch { /* 清理失败不影响断言 */ }
      }
      slot.deps = deps;
      slot.ran = true;
      const cleanup = fn();
      slot.cleanup = typeof cleanup === 'function' ? cleanup : undefined;
    }
    current = result;
    return result;
  };

  return {
    useState,
    useRef,
    useCallback,
    useEffect,
    mount(fn) {
      component = fn;
      needsRender = false;
      return renderOnce();
    },
    requestRender() { needsRender = true; },
    flushRender() {
      let guard = 0;
      while (needsRender && guard < 200) {
        needsRender = false;
        renderOnce();
        guard += 1;
      }
    },
    get state() { return current; },
  };
}

// ───────────────────────────────────────────────────────────────────────────
// 依赖替身的源码（esbuild 里替换模块）
// ───────────────────────────────────────────────────────────────────────────

const STUB_SOURCES = {
  react: `
const rt = () => globalThis.__FEED_TEST_RUNTIME;
export const useState = initial => rt().useState(initial);
export const useRef = initial => rt().useRef(initial);
export const useCallback = (fn, deps) => rt().useCallback(fn, deps);
export const useEffect = (fn, deps) => { rt().useEffect(fn, deps); };
export const useMemo = factory => factory();
export default { useState, useRef, useCallback, useEffect, useMemo };
`,
  'tauri-core': `
export const invoke = (cmd, args) => globalThis.__FEED_TEST_INVOKE(cmd, args);
export default { invoke };
`,
  ipc: `
export const isTauriEnvironment = () => true;
export function errorText(error, fallback) {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === 'string' && error) return error;
  return fallback;
}
export const ipcService = new Proxy({}, {
  get(_target, key) { return globalThis.__FEED_TEST_IPC[key]; },
});
`,
  app: `
export function useAppStore() { return globalThis.__FEED_TEST_APP; }
`,
  favorites: `
export function useFavoriteActions() { return globalThis.__FEED_TEST_FAVORITES.setMark; }
export function useFavoriteMark() { return undefined; }
`,
  playback: `
export function usePlaybackActions() { return globalThis.__FEED_TEST_PLAYBACK; }
`,
};

/**
 * 设置桩：`useFeedStore` 的播放入口要读 `preferredPlayer`（落点判定的输入之一，
 * 见 services/playbackRouting.ts）。这里给一个明确的应用内偏好，让本台架只测取数/
 * 起播时序，不被提示文案干扰。
 */
STUB_SOURCES.settings = `
export function useSettingsStore() {
  return { settings: { preferredPlayer: 'webview', autoNext: true, danmakuEnabled: false }, updateSettings: () => {} };
}
`;

const stubPlugin = {
  name: 'feed-latency-stubs',
  setup(builder) {
    const map = [
      [/^react$/, 'react'],
      [/^@tauri-apps\/api\/core$/, 'tauri-core'],
      [/services\/ipc$/, 'ipc'],
      [/(^|\/)useAppStore(\.tsx?)?$/, 'app'],
      [/(^|\/)useFavoritesStore(\.tsx?)?$/, 'favorites'],
      [/(^|\/)usePlaybackStore(\.tsx?)?$/, 'playback'],
      [/(^|\/)useSettingsStore(\.tsx?)?$/, 'settings'],
    ];
    for (const [filter, name] of map) {
      builder.onResolve({ filter }, () => ({ path: name, namespace: 'feedstub' }));
    }
    builder.onLoad({ filter: /.*/, namespace: 'feedstub' }, args => ({
      contents: STUB_SOURCES[args.path] ?? '',
      loader: 'js',
    }));
  },
};

const bundle = await build({
  stdin: {
    contents: [
      "export { useFeedStore } from './src/stores/useFeedStore.ts';",
      "export { createCandidateCache, orderFeedTail } from './src/services/feedQueue.ts';",
    ].join('\n'),
    resolveDir: ROOT,
    sourcefile: 'feed-latency-entry.ts',
    loader: 'ts',
  },
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  write: false,
  logLevel: 'silent',
  plugins: [stubPlugin],
});
const bundled = bundle.outputFiles[0].text;
const { useFeedStore, createCandidateCache, orderFeedTail } = await import(
  `data:text/javascript;base64,${Buffer.from(bundled, 'utf8').toString('base64')}`
);

// ───────────────────────────────────────────────────────────────────────────
// 取数替身：可编排的页 / 详情 / 标签
// ───────────────────────────────────────────────────────────────────────────

const HISTORY_SEED = [{
  seriesId: 'd0-0',
  seriesTitle: '模拟剧 d0-0',
  cover: '',
  channel: 'drama',
  episodeId: 'd0-0-ep1',
  episodeNumber: 1,
  positionSeconds: 100,
  durationSeconds: 100,
  updatedAt: 1,
  isFinished: false,
}];

function createFixture(options = {}) {
  const pageSize = options.pageSize ?? 6;
  const pageCount = options.pageCount ?? 3;
  const tagsById = options.tagsById ?? { 'd0-0': ['战神'] };

  const stats = {
    pages: [],       // { channel, page }
    detail: [],      // 详情往返（hongguo.seriesDetail）
    tags: [],        // 标签往返（series.getDetail）
    opens: [],       // openEpisode 调用
    trace: [],       // tracePlayback 打点
  };
  const heldPages = new Map();     // `${channel}:${page}` → deferred
  const failedPages = new Map();   // `${channel}:${page}` → Error
  const heldDetails = new Map();   // 前缀 → deferred
  const tagGate = { current: null };

  const itemsFor = (channel, pageIndex) => Array.from({ length: pageSize }, (_, n) => ({
    id: `${channel[0]}${pageIndex}-${n}`,
    title: `模拟剧 ${channel} ${pageIndex}-${n}`,
    cover: '',
    type: channel,
    episodesCount: 3,
    tags: [],
    origin: '',
  }));

  const pageIndexFor = (cursor) => (
    cursor === null || cursor === undefined ? 0 : Number(String(cursor).replace(/^tok/, ''))
  );

  const controls = {
    holdPage(channel, page) {
      const gate = deferred();
      heldPages.set(`${channel}:${page}`, gate);
      return gate;
    },
    releasePage(channel, page) {
      const key = `${channel}:${page}`;
      const gate = heldPages.get(key);
      heldPages.delete(key);
      gate?.resolve();
    },
    failPage(channel, page, error) { failedPages.set(`${channel}:${page}`, error); },
    healPage(channel, page) { failedPages.delete(`${channel}:${page}`); },
    gateTags() {
      const gate = deferred();
      tagGate.current = gate;
      return gate;
    },
    releaseTags() {
      const gate = tagGate.current;
      tagGate.current = null;
      gate?.resolve();
    },
    holdDetail(prefix) {
      const gate = deferred();
      heldDetails.set(prefix, gate);
      return gate;
    },
    releaseDetail(prefix) {
      const gate = heldDetails.get(prefix);
      heldDetails.delete(prefix);
      gate?.resolve();
    },
    sim: SIM,
  };

  const ipc = {
    history: { list: async () => options.history ?? HISTORY_SEED },
    favorites: { list: async () => options.favorites ?? [] },
    series: {
      getDetail: async (seriesId) => {
        stats.tags.push(seriesId);
        if (tagGate.current) await tagGate.current.promise;
        await delay(SIM.tagMs);
        return { tags: tagsById[seriesId] ?? [] };
      },
    },
    hongguo: {
      seriesDetail: async (seriesId) => {
        stats.detail.push(seriesId);
        const channel = seriesId[0] === 'c' ? 'comic' : 'drama';
        for (const [prefix, gate] of heldDetails) {
          if (seriesId.startsWith(prefix)) await gate.promise;
        }
        await delay(SIM.detailMs);
        return {
          series: {
            id: seriesId,
            title: seriesId,
            cover: '',
            backdrop: '',
            type: channel,
            tags: tagsById[seriesId] ?? [],
            origin: '',
            brief: '',
          },
          episodes: [1, 2, 3].map(number => ({
            id: `${seriesId}-ep${number}`,
            seriesId,
            number,
            title: `第 ${number} 集`,
            durationSeconds: 60,
          })),
          isMultiSeason: false,
        };
      },
      shortFilter: async (input) => {
        const channel = input.channel;
        const page = pageIndexFor(input.cursor);
        stats.pages.push({ channel, page });
        const key = `${channel}:${page}`;
        const held = heldPages.get(key);
        if (held) await held.promise;
        await delay(SIM.pageMs);
        if (failedPages.has(key)) throw failedPages.get(key);
        const hasMore = page + 1 < pageCount;
        return {
          items: itemsFor(channel, page),
          hasMore,
          cursor: hasMore ? `tok${page + 1}` : null,
        };
      },
      discover: async () => ({ items: [], hasMore: false, cursor: null }),
    },
    catalog: { list: async () => ({ items: [], hasMore: false, total: 0, page: 1 }) },
    diagnostics: { uiLog: async (message) => { stats.trace.push(message); } },
  };

  return { ipc, stats, controls };
}

// ───────────────────────────────────────────────────────────────────────────
// 场景驱动
// ───────────────────────────────────────────────────────────────────────────

let runtime = null;

// `tracePlayback` 只在"看起来像 Tauri 的窗口"里写日志（它检查 window 上的注入标记）。
// 这里给出最小环境，等价于 ipc 替身的 `isTauriEnvironment() === true`，并让
// `@tauri-apps/api/core` 的 invoke 由替身捕获 —— 这样断言的仍是真实打点代码。
globalThis.window = { __TAURI_INTERNALS__: {} };

const step = async (ms = 6, rounds = 4) => {
  await delay(ms);
  for (let index = 0; index < rounds; index += 1) {
    runtime.flushRender();
    await new Promise(resolve => setTimeout(resolve, 0));
  }
  runtime.flushRender();
};

const until = async (predicate, label, timeoutMs = 3000) => {
  const startedAt = Date.now();
  for (;;) {
    if (predicate()) {
      runtime.flushRender();
      return true;
    }
    if (Date.now() - startedAt > timeoutMs) {
      console.log(`FAIL 等待超时：${label}`);
      return false;
    }
    await delay(1);
    runtime.flushRender();
  }
};

const snap = () => runtime.state;

const pageRequests = (fixture, channel, page) => fixture.stats.pages
  .filter(entry => entry.channel === channel && entry.page === page).length;

async function boot(options = {}) {
  const fixture = options.fixture ?? createFixture(options);
  runtime = createHookRuntime();
  const channel = options.channel ?? 'drama';

  const app = {
    currentView: 'feed',
    feedChannel: channel,
    toasts: [],
    openFeed(next) {
      app.feedChannel = next;
      app.currentView = 'feed';
      runtime.requestRender();
    },
    navigateTo(view) {
      app.currentView = view;
      runtime.requestRender();
    },
    showToast(message) { app.toasts.push(message); },
  };

  const playback = {
    currentSeries: null,
    currentEpisode: null,
    openEpisode: async (seriesId, episodeId, position) => {
      fixture.stats.opens.push({ seriesId, episodeId, position });
      await delay(SIM.openMs);
      playback.currentSeries = { id: seriesId };
      playback.currentEpisode = { id: episodeId };
      runtime.requestRender();
    },
  };

  globalThis.__FEED_TEST_RUNTIME = runtime;
  globalThis.__FEED_TEST_IPC = fixture.ipc;
  globalThis.__FEED_TEST_APP = app;
  globalThis.__FEED_TEST_PLAYBACK = playback;
  globalThis.__FEED_TEST_FAVORITES = { setMark: async () => undefined };
  globalThis.__FEED_TEST_INVOKE = (cmd, args) => {
    if (cmd === 'trace_ui_log') fixture.stats.trace.push(args?.message ?? '');
    return Promise.resolve(null);
  };

  runtime.mount(useFeedStore);
  await step();
  return { fixture, app, playback };
}

const navigate = async (app, direction = 'next') => {
  await runtime.state.switchSeries(direction);
  await step();
};

// ───────────────────────────────────────────────────────────────────────────
// S0 纯逻辑：候选缓存 TTL / 频道槽 / 追加段边界
// ───────────────────────────────────────────────────────────────────────────

{
  let clock = 1_000;
  const cache = createCandidateCache({ ttlMs: 5 * 60_000, now: () => clock });
  const slice = (id) => ({ items: [{ id, title: id, cover: '', type: 'drama', episodesCount: 1, tags: [], origin: '' }], cursor: `tok-${id}`, hasMore: true });
  cache.write('drama', slice('a'));
  check('候选缓存：5 分钟内命中同一份切片', cache.read('drama')?.items[0]?.id === 'a');
  clock += 5 * 60_000 - 1;
  check('候选缓存：TTL 内仍然命中', cache.read('drama')?.items[0]?.id === 'a');
  clock += 2;
  check('候选缓存：过期即当没有（重新取）', cache.read('drama') === null);
  cache.write('drama', slice('b'));
  cache.write('comic', slice('c'));
  cache.write('ai', slice('d'));
  check('候选缓存：三个频道槽都在', cache.size() === 3 && cache.read('comic')?.items[0]?.id === 'c');
  cache.write('drama', slice('e'));
  check('候选缓存：第四个频道不挤掉最近的槽，只淘汰最旧', cache.size() === 3
    && cache.read('drama')?.items[0]?.id === 'e');
}

{
  const items = ['a', 'b', 'c', 'd', 'e'].map(id => ({ id, title: id, cover: '', type: 'drama', episodesCount: 1, tags: [], origin: '' }));
  const interest = new Map([['战神', 1]]);
  const tagsById = new Map([['e', ['战神']]]);
  const ordered = orderFeedTail(items, { tailStart: 3, tailEnd: 5, expectedIds: ['d', 'e'], visitedHighWater: 2, interest, tagsById, rng: () => 0.5 });
  check('尾段排序：只在 [tailStart,tailEnd) 内重排，前缀原样', ordered !== null
    && ordered.slice(0, 3).map(item => item.id).join() === 'a,b,c'
    && ordered.slice(3).map(item => item.id).sort().join() === 'd,e');
  check('尾段排序：段内 id 与快照不一致时放弃（不覆盖后续新页）',
    orderFeedTail(items, { tailStart: 3, tailEnd: 5, expectedIds: ['d', 'x'], visitedHighWater: 2, interest }) === null);
  check('尾段排序：用户已走进该段（高水位）时放弃',
    orderFeedTail(items, { tailStart: 3, tailEnd: 5, expectedIds: ['d', 'e'], visitedHighWater: 3, interest }) === null);
  check('尾段排序：段外后续新页只被保留、不被重排',
    orderFeedTail([...items, { ...items[0], id: 'f', title: 'f' }], {
      tailStart: 3, tailEnd: 5, expectedIds: ['d', 'e'], visitedHighWater: 2, interest, tagsById, rng: () => 0.5,
    })?.at(-1)?.id === 'f');
}

// ───────────────────────────────────────────────────────────────────────────
// S1 首屏：列表 → 详情 → 起播；日志只含固定阶段名，不含 URL / 用户数据
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture } = await boot({});
  const state = snap();
  check('首屏：队列到位、第 0 部详情就位、无错误',
    state.items.length === 6 && state.currentIndex === 0 && state.detail?.id === 'd0-0' && state.error === null);
  check('首屏：起播落在首集 0 秒（不读历史）',
    fixture.stats.opens.length >= 1 && fixture.stats.opens[0].position === 0 && fixture.stats.opens[0].episodeId === 'd0-0-ep1');

  const trace = fixture.stats.trace;
  const stages = new Set(trace.map(line => line.replace(/^推荐 /, '').split(' 耗时=')[0]));
  check('日志：确实写出了分段（列表 / 详情 / 起播准备）',
    trace.length >= 3
    && stages.has('首屏列表') && stages.has('详情') && stages.has('起播准备'));
  check('日志：阶段名取自固定集合',
    [...stages].every(stage => ['首屏列表', '首屏完成', '详情', '起播准备', '补页', '尾段排序', '换剧', '重试'].includes(stage)));
  check('日志：不含 URL / 协议 / 长串（不泄露地址与用户数据）',
    !trace.some(line => /https?:|:\/\/|asset:|token=|[A-Za-z0-9_-]{20,}/.test(line)));
}

// ───────────────────────────────────────────────────────────────────────────
// S2 慢标签不阻塞：页一到就入队，排序在后台；只重排本次追加段
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture } = await boot({ pageSize: 6, pageCount: 3 });
  await until(() => snap().detail !== null, '首屏详情');
  const gate = fixture.controls.gateTags();

  // 到第 3 部：距末端 3 条 → 触发后台提前补页。
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  check('提前补页：第 1 页在途/已回，队列变长', await until(() => snap().items.length === 12, '第 1 页入队'));
  const appended = snap().items.slice(6);
  check('慢标签：标签还没回来就已经入队（不等标签）',
    fixture.stats.tags.length >= 2 && snap().items.length === 12);

  const beforeSwitch = snap().items.slice(0, 6).map(item => item.id).join();
  await navigate(null, 'next');
  check('慢标签：标签在途时仍能往下换剧（导航不被阻塞）', snap().currentIndex === 4);

  fixture.controls.releaseTags();
  await step(8);
  const after = snap().items;
  check('慢标签回来自后排：前缀一条不动', after.slice(0, 6).map(item => item.id).join() === beforeSwitch);
  check('后台排序：只作用于本次追加段（同一批 id，长度不变）',
    after.length === 12
    && after.slice(6).map(item => item.id).sort().join() === appended.map(item => item.id).sort().join());
  check('后台排序：确实执行过（日志有「尾段排序」，不是静默放弃）',
    fixture.stats.trace.some(line => line.startsWith('推荐 尾段排序 耗时=')));
}

// ───────────────────────────────────────────────────────────────────────────
// S3 访问高水位：用户已走进该段，标签晚到也不许重排
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture } = await boot({ pageSize: 6, pageCount: 3 });
  await until(() => snap().detail !== null, '首屏详情');
  fixture.controls.gateTags();
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  await until(() => snap().items.length === 12, '第 1 页入队');
  const pendingOrder = snap().items.map(item => item.id).join();

  // 走进追加段（下标 6、7）：高水位越过 tailStart，晚到的排序必须放弃。
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  check('高水位场景：已走到追加段内', snap().currentIndex === 7);
  fixture.controls.releaseTags();
  await step(8);
  check('高水位保护：已访问过的段不被重排（顺序逐条一致）',
    snap().items.map(item => item.id).join() === pendingOrder);
  check('高水位保护：没有偷偷执行排序', !fixture.stats.trace.some(line => line.startsWith('推荐 尾段排序')));
}

// ───────────────────────────────────────────────────────────────────────────
// S4 后台补页在途 + 前台到边界：复用同一个 Promise，只请求一次
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture } = await boot({ pageSize: 6, pageCount: 4 });
  const hold = fixture.controls.holdPage('drama', 1);
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  check('后台补页：请求已发出并在途', await until(() => pageRequests(fixture, 'drama', 1) === 1, '第 1 页请求'));
  check('后台补页：在途时正在补页（isLoadingMore）', snap().isLoadingMore === true);

  await navigate(null, 'next');
  await navigate(null, 'next');
  check('后台补页在途：用户已滑到最后一部', snap().currentIndex === 5);

  const boundary = snap().switchSeries('next');
  await step(8);
  check('前台接管：此时还没有第二条页请求（复用同一任务）',
    pageRequests(fixture, 'drama', 1) === 1 && snap().currentIndex === 5);

  hold.resolve();
  await boundary;
  await step(10);
  check('前台接管：页回来后继续换剧，只请求过 1 次',
    snap().currentIndex === 6 && snap().items.length === 12
    && pageRequests(fixture, 'drama', 1) === 1 && snap().error === null);
}

// ───────────────────────────────────────────────────────────────────────────
// S5 错误隔离：后台失败不盖界面，前台失败可见；重试能恢复
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture } = await boot({ pageSize: 6, pageCount: 4 });
  fixture.controls.failPage('drama', 1, new Error('HTTP 503'));
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  await until(() => pageRequests(fixture, 'drama', 1) === 1, '后台补页失败');
  await step(6);
  check('错误隔离：后台补页失败不改界面（error 为空）', snap().error === null);
  check('错误隔离：后台失败不打断当前剧', snap().currentIndex === 3 && snap().items.length === 6);

  await navigate(null, 'next');
  await navigate(null, 'next');
  await runtime.state.switchSeries('next');
  await step(8);
  check('前台失败：错误可见（用户主动等这一页）', snap().error === 'HTTP 503');
  check('前台失败：不跳剧、不空队列', snap().currentIndex === 5 && snap().items.length === 6);

  fixture.controls.healPage('drama', 1);
  runtime.state.retry();
  await until(() => snap().items.length === 12, '重试补页成功');
  check('重试：补页成功、错误清掉、继续换到第 6 部',
    snap().items.length === 12 && snap().currentIndex === 6 && snap().error === null);
}

// ───────────────────────────────────────────────────────────────────────────
// S6 换频道：旧代在途详情的续体作废
// ───────────────────────────────────────────────────────────────────────────

{
  const fixture = createFixture({});
  fixture.controls.holdDetail('d0');
  const bound = fixture;
  const { app } = await boot({ fixture: bound });
  check('旧代：旧频道详情被卡住时队列已摆出来', snap().items.length === 6 && snap().detail === null);

  app.openFeed('comic');
  await until(() => snap().detail?.id?.startsWith('c'), '新频道详情');
  check('旧代：新频道已就位', snap().currentItem?.id?.startsWith('c') && snap().detail?.id?.startsWith('c'));

  bound.controls.releaseDetail('d0');
  await step(10);
  check('旧代作废：旧频道详情回来不会写进新代', snap().detail?.id?.startsWith('c'));
  check('旧代作废：错误状态不被旧续体污染', snap().error === null);
  check('旧代作废：两个频道各自只取过一次首屏',
    pageRequests(bound, 'drama', 0) === 1 && pageRequests(bound, 'comic', 0) === 1);
}

// ───────────────────────────────────────────────────────────────────────────
// S7 首屏候选缓存：命中不重取；切频道不清别人的槽
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture, app } = await boot({});
  check('候选缓存：首屏取过一次', pageRequests(fixture, 'drama', 0) === 1);

  app.navigateTo('explore');
  await step();
  app.openFeed('drama');
  await step(10);
  check('候选缓存：同频道再次进入直接用缓存（不再取列表）',
    pageRequests(fixture, 'drama', 0) === 1 && snap().items.length === 6 && snap().currentIndex === 0);
  check('候选缓存命中：详情仍然零往返（第 0 部已在详情缓存）',
    fixture.stats.detail.filter(id => id === 'd0-0').length === 1);

  app.openFeed('comic');
  await until(() => snap().items[0]?.id?.startsWith('c'), 'comic 首屏');
  check('候选缓存：换频道取自己的首屏', pageRequests(fixture, 'comic', 0) === 1);

  app.openFeed('drama');
  await step(10);
  check('候选缓存：切回原频道仍命中（切频道没有清空别的槽）',
    pageRequests(fixture, 'drama', 0) === 1 && snap().items[0]?.id?.startsWith('d'));
}

// ───────────────────────────────────────────────────────────────────────────
// S8 换频道：旧代在途补页的续体作废，且不清新代的闸门
// ───────────────────────────────────────────────────────────────────────────

{
  const { fixture, app } = await boot({ pageSize: 6, pageCount: 4 });
  const hold = fixture.controls.holdPage('drama', 1);
  await navigate(null, 'next');
  await navigate(null, 'next');
  await navigate(null, 'next');
  await until(() => pageRequests(fixture, 'drama', 1) === 1, '旧代第 1 页在途');

  app.openFeed('comic');
  await until(() => snap().items.length === 6 && snap().items[0]?.id?.startsWith('c'), '新频道首屏');
  check('旧代补页：新代从头开始（队列只有新频道）',
    snap().items.every(item => item.id.startsWith('c')) && snap().isLoadingMore === false);

  hold.resolve();
  await step(10);
  check('旧代补页：旧续体不往新代队列里塞内容',
    snap().items.length === 6 && snap().items.every(item => item.id.startsWith('c')));
  check('旧代补页：旧续体不清新代闸门（isLoadingMore 仍为 false）', snap().isLoadingMore === false);
  check('旧代补页：旧频道不再追加第二次请求', pageRequests(fixture, 'drama', 1) === 1);
}

// ───────────────────────────────────────────────────────────────────────────

const failed = checks.filter(([, passed]) => !passed);
for (const [name, passed] of checks) console.log(`${passed ? 'PASS' : 'FAIL'} ${name}`);
if (failed.length) process.exit(1);
console.log(`${checks.length} passed, 0 failed（延时均为模拟值）`);