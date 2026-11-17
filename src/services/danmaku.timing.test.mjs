// 验证「弹幕从点击播放到第一条可见」的时间去哪了。
//
// 运行：node --experimental-strip-types src/services/danmaku.timing.test.mjs
//
// ## 守的是哪次故障
//
// 用户长期反馈：「点击播放后无法立即显示弹幕，等一会才出现」。
//
// 这条链上有四个不同的时间，必须分开量，否则一定会把网络慢当成调度慢、
// 或者把「首条弹幕本身就在第 3 秒」当成 bug：
//
//   t0  加载器建立（= 换集 / 打开弹幕开关的那一刻）
//   t1  首窗请求**发出**              ← 前端调度负责
//   t2  页**到达**（fetch resolve）    ← 网络 / 设备风控负责（线上实测 85ms~3666ms）
//   t3  这一页被**排程**（有 flights） ← 前端调度负责（尺寸、排程键）
//   t4  第一条**可见**                ← 还取决于首条弹幕自己的时间戳
//
// 用例用虚拟时钟 + 假 fetch + 假媒体时钟，对「改前的实现」（判据写死在 rAF 回调里、
// 排程键只看 `page.startMs`）与「改后的实现」（`nextDanmakuTick`）跑同一套场景，
// 把差值归到具体那一段。
//
// ⚠️ 边界：本用例只覆盖**前端调度**。t2 是模拟值，不代表线上网络变快；线上的真值
// 要在 `ttv-playback.log` 里用「弹幕请求开始」与「弹幕页就绪 … 耗时=」两行量
// （本次已补上请求起点打点）。

import {
  DANMAKU_LIFETIME_MS,
  DanmakuLoader,
  createDanmakuTickState,
  danmakuLoadingStartMs,
  danmakuStartMsFromSeconds,
  nextDanmakuTick,
  planDanmaku,
} from './danmaku.ts';
import { readFileSync } from 'node:fs';

let pass = 0;
let fail = 0;

function check(name, ok, detail = '') {
  if (ok) pass += 1;
  else fail += 1;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : '  ' + detail));
}

function checkNear(name, actual, expected, tol, unit = 'ms') {
  const ok = actual !== null && Math.abs(actual - expected) <= tol;
  check(
    name,
    ok,
    `期望=${expected}±${tol}${unit} 实际=${actual === null ? 'null' : Math.round(actual)}${unit}`,
  );
}

// ── 虚拟时钟 ───────────────────────────────────────────────────────────
//
// 全部走虚拟毫秒：`Date.now()` 被指到同一个钟上（`DanmakuLoader` 的缓存 TTL 与
// 失败冷却都读它）。不这么做，「3 秒冷却」在模拟里永远不会到点。

const VCLOCK = { now: 0 };
const realDateNow = Date.now;
const realPerformanceNow = performance.now.bind(performance);
Date.now = () => VCLOCK.now;
performance.now = () => VCLOCK.now;

const MEASURE = text => text.length * 12;
const T0 = 30000; // 页面已加载 30s 后用户点播放（`performance.now()` 的量级）

async function scenario(name, run) {
  DanmakuLoader.resetSharedCacheForTests();
  VCLOCK.now = 0;
  console.log('\n' + name);
  await run();
}

// 静态/纯函数用例不依赖缓存，场景从下面开始。

class Sim {
  constructor(startMs, frameStepMs = 16) {
    VCLOCK.now = startMs;
    this.now = startMs;
    this.q = [];
    this.seq = 0;
    this.frameStep = frameStepMs;
  }

  at(t, fn) {
    this.q.push({ t, fn, n: this.seq++ });
    this.q.sort((a, b) => a.t - b.t || a.n - b.n);
  }

  /** 逐帧回调：首帧在 `fromMs + firstDelayMs`。首帧延迟是**给定假设**，不代表实测成因。 */
  frames(fromMs, cb, untilMs, firstDelayMs = 8) {
    const step = t => {
      if (t > untilMs) return;
      this.at(t, () => cb(t));
      step(t + this.frameStep);
    };
    step(fromMs + firstDelayMs);
  }

  async run(untilMs) {
    for (;;) {
      const ev = this.q[0];
      if (!ev || ev.t > untilMs) break;
      this.q.shift();
      this.now = ev.t;
      VCLOCK.now = ev.t;
      ev.fn();
      // 让 fetch 的 .then/.catch/.finally 在本帧之后、下一帧之前落地（真实时序亦然）。
      await new Promise(r => setImmediate(r));
    }
    this.now = untilMs;
    VCLOCK.now = untilMs;
  }
}

// ── 假 fetch ──────────────────────────────────────────────────────────

function makePageBuilder(allItems) {
  return (startMs, windowMs) => ({
    episodeId: 'vid',
    startMs,
    nextMs: startMs + windowMs,
    total: allItems.length,
    items: allItems.filter(i => i.timeMs >= startMs && i.timeMs < startMs + windowMs),
  });
}

function fakeFetch(sim, { latencyMs, items, failFirst = false }) {
  const requests = [];
  const page = makePageBuilder(items);
  const fetchPage = (sid, vid, startMs, durationMs) => {
    requests.push({ at: sim.now, startMs, durationMs });
    const index = requests.length - 1;
    return new Promise((resolve, reject) => {
      sim.at(sim.now + latencyMs, () => {
        if (failFirst && index === 0) reject(new Error('模拟网络抖动'));
        else resolve(page(startMs, 30000));
      });
    });
  };
  return { requests, fetchPage };
}

// ── 两个 tick 策略 ────────────────────────────────────────────────────
//
// 'fixed'  = 现在的实现（真实 `nextDanmakuTick` + 真实 `planDanmaku`）
// 'legacy' = 改前的实现（判据按改前源码原样搬过来：rAF 里 250ms 闸 + 键只看 startMs）

function makeTicker(policy, loader, opts) {
  const { rows, lifetimeMs, measureKey = '20px|0|A' } = opts;
  const state = policy === 'fixed' ? createDanmakuTickState() : { lastEnsure: 0, lastPageKey: '' };
  const ctx = { flights: [], firstPlanAt: null, firstVisibleAt: null, everVisible: new Set(), planEvents: [] };

  ctx.tick = (t, positionMs, durationMs, width) => {
    let planned = false;
    if (policy === 'fixed') {
      const page = loader.pageFor(positionMs);
      const decision = nextDanmakuTick(state, {
        nowMs: t,
        positionMs,
        durationMs,
        width,
        rows,
        lifetimeMs,
        measureKey,
        page,
      });
      if (decision.ensure) loader.ensure(positionMs, durationMs);
      if (decision.plan && page) {
        ctx.flights = planDanmaku(page.items, { width, rows, measure: MEASURE, lifetimeMs });
        planned = true;
        ctx.planEvents.push({ t, startMs: page.startMs });
      }
    } else if (t - state.lastEnsure > 250 && durationMs > 0) {
      state.lastEnsure = t;
      loader.ensure(positionMs, durationMs);
      const page = loader.pageFor(positionMs);
      const pageKey = page ? `${page.startMs}` : '';
      if (page && pageKey !== state.lastPageKey) {
        state.lastPageKey = pageKey;
        ctx.flights = planDanmaku(page.items, { width, rows, measure: MEASURE, lifetimeMs });
        planned = true;
        ctx.planEvents.push({ t, startMs: page.startMs });
      } else if (!page && state.lastPageKey) {
        state.lastPageKey = '';
      }
    }

    for (const flight of ctx.flights) {
      const elapsed = positionMs - flight.item.timeMs;
      if (elapsed >= 0 && elapsed <= lifetimeMs) {
        ctx.everVisible.add(flight.item.id);
        if (ctx.firstVisibleAt == null) ctx.firstVisibleAt = t;
      }
    }
    if (planned && ctx.firstPlanAt == null) ctx.firstPlanAt = t;
    return ctx;
  };

  return { ctx, state };
}

// ── 场景驱动 ──────────────────────────────────────────────────────────

async function simulate(cfg) {
  // 每次 simulate 都是独立用户场景；legacy/fixed 对照不能互相借缓存。
  DanmakuLoader.resetSharedCacheForTests();
  const {
    policy,
    latencyMs,
    items,
    playFromMs = 0,
    startPositionMs = 0,
    widthUntilMs = 0,
    rowsChangeWithSize = true,
    firstFrameMs = 8,
    failFirst = false,
    enabledAtMs = 0,
    openStartMs = null,
    horizonMs = 15000,
    /** 一次跳转：`{ atMs, toMs }`（相对 t0）。用来覆盖 seek 前后窗口切换。 */
    seek = null,
  } = cfg;

  const rowsSized = cfg.rowsSized ?? (rowsChangeWithSize ? 4 : 1);
  const lifetimeMs = DANMAKU_LIFETIME_MS;
  const sim = new Sim(T0);
  const out = { policy, requests: [], firstPlanAt: null, firstVisibleAt: null, everVisible: new Set() };

  let loader = null;
  let tickerCtx = null;
  let legacyState = null;

  const widthNow = () => (sim.now >= T0 + widthUntilMs ? 900 : 0);
  const rowsNow = () => (sim.now >= T0 + widthUntilMs ? rowsSized : 1);
  const positionNow = () => {
    const rel = sim.now - T0;
    if (rel < playFromMs) return 0;
    const jumped = seek && rel >= seek.atMs;
    if (jumped) return seek.toMs + (rel - seek.atMs);
    return startPositionMs + (rel - playFromMs);
  };
  const durationMs = () => 90000;

  const boot = () => {
    const { requests, fetchPage } = fakeFetch(sim, { latencyMs, items, failFirst });
    loader = new DanmakuLoader('series', 'vid', fetchPage);
    out.requestsRef = requests;
    const created = makeTicker(policy, loader, { rows: rowsNow(), lifetimeMs });
    tickerCtx = created.ctx;
    legacyState = created.state;
    // 改后的实现：提交时刻直接发首窗，并把调度状态记成"刚发过"
    // （`DanmakuOverlay` 建加载器的 effect 就是这么写的）。
    if (policy === 'fixed') {
      loader.ensure(openStartMs ?? positionNow(), Math.max((openStartMs ?? positionNow()) + 30000, 30000));
      created.state.lastEnsureAt = sim.now;
    }
  };

  if (enabledAtMs === 0) boot();
  else sim.at(T0 + enabledAtMs, boot);

  // 尺寸到位那一帧：改前的实现若 rows 变了，rAF effect 会重启（闭包状态归零）。
  if (rowsChangeWithSize && widthUntilMs > 0) {
    sim.at(T0 + widthUntilMs, () => {
      if (policy !== 'legacy' || !legacyState) return;
      legacyState.lastEnsure = 0;
      legacyState.lastPageKey = '';
    });
  }

  sim.frames(
    T0,
    () => {
      if (!loader || !tickerCtx) return;
      tickerCtx.tick(sim.now, openStartMs ?? positionNow(), durationMs(), widthNow());
    },
    T0 + horizonMs,
    firstFrameMs,
  );

  await sim.run(T0 + horizonMs);
  out.requests = [...(out.requestsRef ?? [])];
  out.firstPlanAt = tickerCtx ? tickerCtx.firstPlanAt : null;
  out.firstVisibleAt = tickerCtx ? tickerCtx.firstVisibleAt : null;
  out.everVisible = tickerCtx ? tickerCtx.everVisible : new Set();
  out.planEvents = tickerCtx ? tickerCtx.planEvents : [];
  return out;
}

const rel = t => (t == null ? null : t - T0);

// ── 用例 ──────────────────────────────────────────────────────────────

const DENSE = Array.from({ length: 300 }, (_, i) => ({
  id: `d${i}`,
  text: `弹幕${i}`,
  timeMs: 100 + i * 100, // 每 100ms 一条：只要页到了，立刻就有可见弹幕
}));

const DENSE_LONG = Array.from({ length: 900 }, (_, i) => ({
  id: `L${i}`,
  text: `弹幕${i}`,
  timeMs: 100 + i * 100, // 覆盖 90 秒整集
}));

console.log('=== 1. nextDanmakuTick 判据（纯函数）===');

{
  const page = { episodeId: 'v', startMs: 0, nextMs: 30000, total: 3, items: DENSE.slice(0, 3) };
  const base = { positionMs: 1000, durationMs: 60000, width: 900, rows: 4, lifetimeMs: 8000, measureKey: '20px|0|A', page };

  const s1 = createDanmakuTickState();
  check('从未发过 → 本帧就发 ensure（首窗不等 250ms 闸）', nextDanmakuTick(s1, { ...base, nowMs: 1000 }).ensure);
  check('250ms 内不重复发 ensure', nextDanmakuTick(s1, { ...base, nowMs: 1100 }).ensure === false);
  check('满 250ms 再发 ensure', nextDanmakuTick(s1, { ...base, nowMs: 1250 }).ensure === true);

  const s2 = createDanmakuTickState();
  check(
    '宽度 0 → 本帧不排程（planDanmaku 必然返回空数组）',
    nextDanmakuTick(s2, { ...base, nowMs: 1000, width: 0 }).plan === false,
  );
  check('宽度 0 时**不记账**，宽度迟到后仍能排程', s2.lastPlanKey === '');
  check('宽度到位 → 立刻排程', nextDanmakuTick(s2, { ...base, nowMs: 1016, width: 900 }).plan === true);

  const s3 = createDanmakuTickState();
  nextDanmakuTick(s3, { ...base, nowMs: 1000 });
  check('同一页同一尺寸 → 不重复排程', nextDanmakuTick(s3, { ...base, nowMs: 1016 }).plan === false);
  check(
    '宽度变化 → 必须重排（轨道判定公式的分母变了）',
    nextDanmakuTick(s3, { ...base, nowMs: 1032, width: 640 }).plan === true,
  );
  check(
    '轨道数变化 → 必须重排',
    nextDanmakuTick(s3, { ...base, nowMs: 1048, width: 640, rows: 8 }).plan === true,
  );
  check(
    '速度档（飞行时长）变化 → 必须重排',
    nextDanmakuTick(s3, { ...base, nowMs: 1064, width: 640, rows: 8, lifetimeMs: 12000 }).plan === true,
  );
  check(
    '量宽身份（粗体）变化、width/rows 不变 → 必须重排',
    nextDanmakuTick(s3, { ...base, nowMs: 1080, width: 640, rows: 8, lifetimeMs: 12000, measureKey: '20px|1|A' }).plan === true,
  );
  check(
    '量宽身份（字体）变化、width/rows 不变 → 必须重排',
    nextDanmakuTick(s3, { ...base, nowMs: 1096, width: 640, rows: 8, lifetimeMs: 12000, measureKey: '20px|1|B' }).plan === true,
  );
  check(
    '量宽身份（字号）变化但 rows 不变 → 必须重排',
    nextDanmakuTick(s3, { ...base, nowMs: 1112, width: 640, rows: 8, lifetimeMs: 12000, measureKey: '24px|1|B' }).plan === true,
  );
  check(
    '量宽身份不变 → 不重复排程',
    nextDanmakuTick(s3, { ...base, nowMs: 1128, width: 640, rows: 8, lifetimeMs: 12000, measureKey: '24px|1|B' }).plan === false,
  );

  const s4 = createDanmakuTickState();
  nextDanmakuTick(s4, { ...base, nowMs: 1000 });
  check('页隙（缓存未命中 / 冷却中）→ 不排程', nextDanmakuTick(s4, { ...base, nowMs: 1016, page: null }).plan === false);
  check('页隙时排程键作废，同页回来后能重新排程', s4.lastPlanKey === '');
  check('页回来后同一页可以再排一次', nextDanmakuTick(s4, { ...base, nowMs: 1032 }).plan === true);
}

console.log('\n=== 1b. 起播目标：秒→毫秒校验 + 装载期选页判据 ===');
{
  check('校验·非有限 → 0（从头播）', danmakuStartMsFromSeconds(Number.NaN) === 0);
  check('校验·负数 → 0', danmakuStartMsFromSeconds(-3) === 0);
  check('校验·50s → 50000ms', danmakuStartMsFromSeconds(50) === 50000);
  check('校验·小数四舍五入', danmakuStartMsFromSeconds(50.4) === 50400);
  check(
    '校验·离谱大值夹到窗口上限内（不产生必被服务端拒绝的请求）',
    danmakuStartMsFromSeconds(86400 * 2) === 86400000 - 30000,
  );

  const openStart = { seriesId: 's1', episodeId: 'e5', startMs: 50000, sessionId: 7 };
  /** store 的装载期：opening 之后 `handleWaiting` 会把 kind 改成 buffering，媒体仍是 0。 */
  const loading = { duration: 0, position: 0 };
  const gate = {
    openStart,
    seriesId: 's1',
    episodeId: 'e5',
    sessionId: 7,
    uiState: { kind: 'opening', sessionId: 7 },
    ...loading,
  };
  check('判据·续播装载期（opening）→ 用 50000', danmakuLoadingStartMs(gate) === 50000);
  check(
    '判据·**opening→buffering 且媒体仍 0** → 仍用 50000（旧实现会在这里退回 0 窗）',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'buffering', sessionId: 7 } }) === 50000,
  );
  check(
    '判据·元数据已到（duration=90）但播放头仍是 0 → 仍用 50000',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'buffering', sessionId: 7 }, duration: 90 }) === 50000,
  );
  check(
    '判据·从头播的 0 也是有效目标（不是 null）',
    danmakuLoadingStartMs({ ...gate, openStart: { ...openStart, startMs: 0 } }) === 0,
  );
  check(
    '判据·本会话 playing → 回归媒体时钟',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'playing', sessionId: 7 } }) === null,
  );
  check(
    '判据·本会话播放头已到目标（buffering@50.2s）→ 回归媒体时钟',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'buffering', sessionId: 7 }, duration: 90, position: 50.2 }) === null,
  );
  check(
    '判据·起播后普通 buffering@62s → 回归媒体时钟（不守最初 50s）',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'buffering', sessionId: 7 }, duration: 90, position: 62 }) === null,
  );
  check(
    '判据·暂停态（paused）且播放头已越过目标 → 回归媒体时钟',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'paused', sessionId: 7 }, duration: 90, position: 70 }) === null,
  );
  check(
    '判据·片尾目标（59.5s / 时长 60.4s，alignPlayhead 会夹回 0）→ 释放，不死锁',
    danmakuLoadingStartMs({ ...gate, openStart: { ...openStart, startMs: 59500 }, duration: 60.4 }) === null,
  );
  check('判据·换集（集身份不匹配）→ null', danmakuLoadingStartMs({ ...gate, episodeId: 'e6' }) === null);
  check('判据·换剧（剧身份不匹配）→ null', danmakuLoadingStartMs({ ...gate, seriesId: 's2' }) === null);
  check('判据·上一会话的旧目标 → null', danmakuLoadingStartMs({ ...gate, sessionId: 8 }) === null);
  check(
    '判据·uiState 还是**旧会话**的 playing → 不采信它，目标保留（否则会退回旧时钟）',
    danmakuLoadingStartMs({ ...gate, uiState: { kind: 'playing', sessionId: 6 } }) === 50000,
  );
  check('判据·没有目标 → null', danmakuLoadingStartMs({ ...gate, openStart: null }) === null);
  check(
    '判据·目标含非法 startMs → null',
    danmakuLoadingStartMs({ ...gate, openStart: { ...openStart, startMs: Number.NaN } }) === null,
  );
}

console.log('\n=== 1c. store 状态序列 → 选页 → 实际请求窗口（逻辑层，非 store 运行时）===');
{
  // 把 store 的真实状态推进顺序喂给判据，再喂给主循环的选页逻辑，看**请求窗口**：
  //   opening@0 → buffering@0（媒体还是 0）→ playing@50.2 → buffering@62 开弹幕
  const requests = [];
  const loader = new DanmakuLoader('s1', 'e5', (sid, vid, startMs, durationMs) => {
    requests.push(startMs);
    return Promise.resolve({ episodeId: vid, startMs, nextMs: startMs + 30000, total: 0, items: [] });
  });
  const ticker = createDanmakuTickState();
  let now = 1000;
  const openStart = { seriesId: 's1', episodeId: 'e5', startMs: 50000, sessionId: 7 };

  const frame = async step => {
    const target = danmakuLoadingStartMs({
      openStart,
      seriesId: 's1',
      episodeId: 'e5',
      sessionId: 7,
      uiState: step.uiState,
      duration: step.duration,
      position: step.position,
    });
    const selectionMs = target ?? step.mediaMs;
    const durationMs = step.duration > 0 ? step.duration * 1000 : selectionMs + 30000;
    const page = loader.pageFor(selectionMs);
    const decision = nextDanmakuTick(ticker, {
      nowMs: now,
      positionMs: selectionMs,
      durationMs,
      width: 900,
      rows: 4,
      lifetimeMs: DANMAKU_LIFETIME_MS,
      measureKey: 'm',
      page,
    });
    if (decision.ensure) loader.ensure(selectionMs, durationMs);
    now += 300; // 每「步」跨过 250ms 闸，确保该发的时候真的会发
    await new Promise(r => setImmediate(r));
  };

  await frame({ uiState: { kind: 'opening', sessionId: 7 }, duration: 0, position: 0, mediaMs: 0 });
  check('序列·装载期只拉 30000 窗', JSON.stringify(requests) === '[30000]', JSON.stringify(requests));

  await frame({ uiState: { kind: 'buffering', sessionId: 7 }, duration: 0, position: 0, mediaMs: 0 });
  await frame({ uiState: { kind: 'buffering', sessionId: 7 }, duration: 90, position: 0, mediaMs: 0 });
  check(
    '序列·opening→buffering 且媒体 0 时**不**去拉 0 窗（回归点）',
    JSON.stringify(requests) === '[30000]',
    JSON.stringify(requests),
  );

  await frame({ uiState: { kind: 'playing', sessionId: 7 }, duration: 90, position: 50.2, mediaMs: 50200 });
  check('序列·就绪后回归时钟，仍在同一窗（不重复请求）', JSON.stringify(requests) === '[30000]', JSON.stringify(requests));

  loader.dispose();
  const requests2 = [];
  const loader2 = new DanmakuLoader('s1', 'e5', (sid, vid, startMs, durationMs) => {
    requests2.push(startMs);
    return Promise.resolve({ episodeId: vid, startMs, nextMs: startMs + 30000, total: 0, items: [] });
  });
  const ticker2 = createDanmakuTickState();
  const frame2 = async step => {
    const target = danmakuLoadingStartMs({
      openStart,
      seriesId: 's1',
      episodeId: 'e5',
      sessionId: 7,
      uiState: step.uiState,
      duration: step.duration,
      position: step.position,
    });
    const selectionMs = target ?? step.mediaMs;
    const durationMs = step.duration > 0 ? step.duration * 1000 : selectionMs + 30000;
    const page = loader2.pageFor(selectionMs);
    const decision = nextDanmakuTick(ticker2, {
      nowMs: now,
      positionMs: selectionMs,
      durationMs,
      width: 900,
      rows: 4,
      lifetimeMs: DANMAKU_LIFETIME_MS,
      measureKey: 'm',
      page,
    });
    if (decision.ensure) loader2.ensure(selectionMs, durationMs);
    now += 300;
    await new Promise(r => setImmediate(r));
  };
  // 播放中关掉再打开弹幕 = 新建加载器 + 一次 tick；此时媒体在 62s。
  await frame2({ uiState: { kind: 'buffering', sessionId: 7 }, duration: 90, position: 62, mediaMs: 62000 });
  check(
    '序列·起播后 buffering@62s 开弹幕 → 请求 60000 窗（不是最初 30000）',
    JSON.stringify(requests2) === '[60000]',
    JSON.stringify(requests2),
  );
  loader2.dispose();
}

console.log('\n=== 1d. store / VideoSurface 接线（静态检查；Node 里跑不起整个 Provider）===');
{
  const storeSrc = readFileSync(new URL('../stores/usePlaybackStore.tsx', import.meta.url), 'utf8')
    .replace(/\r/g, '');
  const surfaceSrc = readFileSync(
    new URL('../components/player/VideoSurface.tsx', import.meta.url),
    'utf8',
  ).replace(/\r/g, '');

  const bump = storeSrc.indexOf('setCurrentEpisode(ep);');
  const target = storeSrc.indexOf('setOpenStart({', bump);
  check(
    'store·setOpenStart 紧跟 setCurrentEpisode（同批提交）',
    bump >= 0 && target > bump && target - bump < 600,
    `距离=${target - bump}`,
  );
  check(
    'store·目标带剧/集/会话身份且起播秒数经校验',
    /setOpenStart\(\{\s*seriesId,\s*episodeId: ep\.id,\s*startMs: danmakuStartMsFromSeconds\(startPosition\),\s*sessionId: newSessionId,/.test(
      storeSrc,
    ),
  );
  check('store·只读暴露到播放器上下文', /danmakuEnabled,\s*setDanmakuEnabled,\s*openStart,/.test(storeSrc));
  check('store·不进低频动作专线（不广播给发现/详情/历史）', /[|]\s*'openStart'/.test(storeSrc));
  check(
    'store·`playing`（本会话真实出画）时释放起播目标',
    /setOpenStart\(prev => \(prev && prev\.sessionId === activeSessionRef\.current \? null : prev\)\)/.test(storeSrc),
  );
  check(
    'store·释放点在本会话 `playing` 的迟到事件守卫之后',
    (() => {
      const playing = storeSrc.indexOf('const handlePlaying = () => {');
      const guard = storeSrc.indexOf('isStaleElementSession(video, activeSessionRef.current)', playing);
      const release = storeSrc.indexOf('setOpenStart(prev =>', playing);
      return playing >= 0 && guard > playing && release > guard;
    })(),
  );
  check(
    'VideoSurface·读取 openStart 并用 danmakuLoadingStartMs 判定',
    /\bopenStart,/.test(surfaceSrc) && /danmakuLoadingStartMs\(\{/.test(surfaceSrc),
  );
  check(
    'VideoSurface·判据传入剧/集/会话/uiState',
    /seriesId: currentSeries\?\.id \?\? ''/.test(surfaceSrc) &&
      /episodeId: currentEpisode\?\.id \?\? ''/.test(surfaceSrc) &&
      /sessionId,/.test(surfaceSrc) &&
      /uiState,/.test(surfaceSrc),
  );
  check(
    'VideoSurface·另传释放目标的真实证据（本会话播放头/时长/片尾容差）',
    /\bposition,\s*duration,\s*nearEndSeconds: START_NEAR_END_SECONDS,/.test(surfaceSrc) &&
      /import \{ START_NEAR_END_SECONDS \} from '\.\.\/\.\.\/services\/playbackStart'/.test(surfaceSrc),
  );
}

await scenario('=== 2. 冷启动：首窗请求起点（t1）===', async () => {
  const legacy = await simulate({ policy: 'legacy', latencyMs: 180, items: DENSE, playFromMs: 2500 });
  const fixed = await simulate({ policy: 'fixed', latencyMs: 180, items: DENSE, playFromMs: 2500 });
  checkNear('改前：首窗请求在首个 rAF 之后', rel(legacy.requests[0].at), 8, 20);
  check('改后：首窗请求在加载器建立的同一时刻', rel(fixed.requests[0].at) === 0, `实际=+${rel(fixed.requests[0].at)}ms`);
});

await scenario('=== 3. 冷启动 + 首帧 rAF 被推迟（假设 400ms，成因未测）===', async () => {
  const slowFrame = { latencyMs: 180, items: DENSE, playFromMs: 2500, firstFrameMs: 400 };
  const legacy = await simulate({ policy: 'legacy', ...slowFrame });
  const fixed = await simulate({ policy: 'fixed', ...slowFrame });
  checkNear('改前：首帧推迟 400ms → 首窗请求也晚 400ms', rel(legacy.requests[0].at), 400, 20);
  check('改后：请求起点与首帧无关', rel(fixed.requests[0].at) === 0, `实际=+${rel(fixed.requests[0].at)}ms`);

  // 页到达 → 排程（t3 - t2）：这是纯前端那一段，和网络无关。
  // 改后的排程仍靠 rAF，所以同样被推迟到首帧；区别是**请求已经在提交
  // 时刻发出去了**（t1 不受影响），而改前连 t1 都被推迟。
  const legacyPlanLag = legacy.firstPlanAt - (legacy.requests[0].at + 180);
  const fixedPlanLag = fixed.firstPlanAt - (fixed.requests[0].at + 180);
  check('改后：排程最多被同一次首帧推迟（请求不受牵连）', fixedPlanLag <= 400 - 180 + 16, `实际=${Math.round(fixedPlanLag)}ms`);
  check('改前：页到达后要等下一次 250ms 闸才排程（且闸从被推迟的首帧起算）', legacyPlanLag > 32, `实际=${Math.round(legacyPlanLag)}ms`);

  // 本场景首条可见还是被「媒体 2.5s 才起步 + 首条在 100ms」压住，两版相同 —— 这正是
  // 不能把「首条可见晚」一律当调度 bug 的原因：先分清是哪一段。
  check('本场景首条可见由媒体起步时间决定（两版相同）',
    rel(legacy.firstVisibleAt) === rel(fixed.firstVisibleAt),
    `改前=${rel(legacy.firstVisibleAt)}ms 改后=${rel(fixed.firstVisibleAt)}ms`);
});

await scenario('=== 3b. 媒体已在播（回放中打开弹幕开关 / 预取页）：页到达即决定首条可见 ===', async () => {
  const rolling = { latencyMs: 180, items: DENSE, playFromMs: 0 };
  const legacy = await simulate({ policy: 'legacy', ...rolling });
  const fixed = await simulate({ policy: 'fixed', ...rolling });
  const fixedPlanLag3b = fixed.firstPlanAt - (fixed.requests[0].at + 180);
  check('改后：首帧正常时页到达后一帧内就排程（≤32ms）', fixedPlanLag3b <= 32, `实际=${Math.round(fixedPlanLag3b)}ms`);
  checkNear('改后：首条可见 = 往返 + 一帧', rel(fixed.firstVisibleAt), 180 + 16, 40);
  check('改后比改前早可见（差值 = 排程闸，本场景约一帧到 250ms 之间）',
    rel(fixed.firstVisibleAt) < rel(legacy.firstVisibleAt),
    `改后=${rel(fixed.firstVisibleAt)}ms 改前=${rel(legacy.firstVisibleAt)}ms`);
  console.log(`     实测差：改后 +${rel(fixed.firstVisibleAt)}ms，改前 +${rel(legacy.firstVisibleAt)}ms`);
});

await scenario('=== 4. 页先到、尺寸还是 0（视图仍隐藏 / 播放器尺寸未就绪）===', async () => {
  const cfg = { latencyMs: 150, items: DENSE, playFromMs: 0, widthUntilMs: 500, rowsChangeWithSize: false };
  const legacy = await simulate({ policy: 'legacy', ...cfg });
  const fixed = await simulate({ policy: 'fixed', ...cfg });
  checkNear('改后：宽度一到位就排程（不等到下一个 30s 窗口）', rel(fixed.firstVisibleAt), 500 + 24, 80);
  check(
    '改前：这一页被记成"已排过"，一直空到 30 秒页边界（本模拟窗内一条都不出）',
    legacy.firstVisibleAt == null,
    `实际=+${rel(legacy.firstVisibleAt)}ms`,
  );
  check('改前同样只发了一次请求（不是请求问题，是排程问题）', legacy.requests.length === 1);
});

await scenario('=== 5. 同上但尺寸到位时轨道数也变（改前被 effect 重启"救回"）===', async () => {
  const cfg = { latencyMs: 150, items: DENSE, playFromMs: 0, widthUntilMs: 500, rowsChangeWithSize: true };
  const legacy = await simulate({ policy: 'legacy', ...cfg });
  const fixed = await simulate({ policy: 'fixed', ...cfg });
  check('改前只在 rows 变化时被迫重排 → 第 4 组正是「尺寸变、轨道数不变」这一角的漏洞',
    legacy.firstVisibleAt != null && rel(legacy.firstVisibleAt) <= 700, `实际=+${rel(legacy.firstVisibleAt)}ms`);
  check('改后在两种情况下同样快', rel(fixed.firstVisibleAt) <= 700, `实际=+${rel(fixed.firstVisibleAt)}ms`);
});

await scenario('=== 6. 慢响应（线上日志实测最坏 3666ms）：不伪造、不把未来弹幕提前 ===', async () => {
  const items = [
    { id: 'old', text: '已经飞了3秒的', timeMs: 500 },
    { id: 'fresh', text: '刚到点的', timeMs: 3000 },
  ];
  const fixed = await simulate({ policy: 'fixed', latencyMs: 3666, items, playFromMs: 0 });
  // 页在 +3666ms 到，媒体位置 3666ms：500ms 那条已飞 3166ms（仍在 8s 飞行窗内 → 移动中可见），
  // 3000ms 那条刚过 666ms。首条可见就是「页到达那一刻」，没有把时间戳改到未来。
  checkNear('慢页到达即刻显示飞行窗内的存量弹幕（时间戳不动）', rel(fixed.firstVisibleAt), 3666 + 16, 80);
  check('两条都进了可见集合（都还在飞行窗内）', fixed.everVisible.has('old') && fixed.everVisible.has('fresh'));
});

{
  const items = [
    { id: 'expired', text: '早该飞完了', timeMs: 500 },
    { id: 'future', text: '时间点在后', timeMs: 9500 },
  ];
  // 媒体从 10s 起（续播 / 跳转）：500ms 那条已超出 8s 飞行窗。
  const fixed = await simulate({ policy: 'fixed', latencyMs: 150, items, playFromMs: 0, startPositionMs: 10000 });
  check(
    '超出飞行窗的旧弹幕不显示（不强行把它拉到当前时刻）',
    fixed.everVisible.has('expired') === false,
    `实际=${[...fixed.everVisible].join(',')}`,
  );
  check(
    '仍在飞行窗内的那条正常显示（10s 处飞了 0.5s）',
    fixed.everVisible.has('future') === true,
    `实际=${[...fixed.everVisible].join(',')}`,
  );
}

await scenario('=== 7. 续播：起播点直接决定首窗，不白拉 0 窗 ===', async () => {
  const cfg = { policy: 'fixed', latencyMs: 180, items: DENSE_LONG, playFromMs: 2500, startPositionMs: 50000, openStartMs: 50000 };
  const current = await simulate(cfg);
  const windows = current.requests.map(r => r.startMs);
  // simulate 的 boot 对应主线已暴露的 openStart：首窗直接选起播点所在窗口。
  check('首窗直接请求 30000ms 窗口', windows[0] === 30000, `实际窗口=${windows.join(',')}`);
  check('不为 0 窗口白跑一次请求', windows.includes(0) === false, `实际窗口=${windows.join(',')}`);
  checkNear('首条可见只取决于首窗往返 + 一帧', rel(current.firstVisibleAt), 180 + 16, 80);
});

await scenario('=== 8. 空页：不请求风暴，也不显示任何东西 ===', async () => {
  const fixed = await simulate({ policy: 'fixed', latencyMs: 120, items: [], playFromMs: 0, horizonMs: 6000 });
  check('空页 → 一条都不显示', fixed.everVisible.size === 0);
  check('空页 → 6 秒内只有一个请求（缓存命中即单飞，不会反复打接口）',
    fixed.requests.length === 1, `实际=${fixed.requests.length}`);
});

await scenario('=== 9. 失败重试：短冷却后复用同键后台结果 ===', async () => {
  const fixed = await simulate({ policy: 'fixed', latencyMs: 90, items: DENSE, playFromMs: 0, failFirst: true, horizonMs: 3000 });
  check('首请求 + 短冷却后重试 = 2 次', fixed.requests.length === 2, `实际=${fixed.requests.length}`);
  check('重试不早于 250ms 冷却',
    fixed.requests[1] != null && fixed.requests[1].at - fixed.requests[0].at >= 250,
    `间隔=${fixed.requests[1] ? fixed.requests[1].at - fixed.requests[0].at : 'null'}ms`);
  check('重试的是同一窗口（不跳窗口浪费请求）', fixed.requests[0].startMs === fixed.requests[1].startMs);
  check('重试成功后立刻有可见弹幕', fixed.firstVisibleAt != null);
});

await scenario('=== 10. 弹幕开关：关着不发请求，打开即刻发 ===', async () => {
  const fixed = await simulate({ policy: 'fixed', latencyMs: 150, items: DENSE, playFromMs: 0, enabledAtMs: 800 });
  check('开关打开前零请求', rel(fixed.requests[0].at) >= 800, `实际=+${rel(fixed.requests[0].at)}ms`);
  check('打开那一刻就发（不等下一帧）', rel(fixed.requests[0].at) < 816, `实际=+${rel(fixed.requests[0].at)}ms`);
  check('打开后立刻能出弹幕', fixed.firstVisibleAt != null);
});

await scenario('=== 11. 换集：旧请求按剧集身份入缓存，切回可复用 ===', async () => {
  const sim = new Sim(T0);
  const { requests: oldRequests, fetchPage: oldFetchRaw } = fakeFetch(sim, { latencyMs: 800, items: DENSE });
  const oldFetch = (sid, vid, start, duration) => oldFetchRaw(sid, vid, start, duration).then(page => ({ ...page, episodeId: vid }));
  const oldLoader = new DanmakuLoader('series', 'vid-old', oldFetch);
  oldLoader.ensure(0, 30000);

  let newLoader = null;
  let newRequests = null;
  sim.at(T0 + 300, () => {
    oldLoader.dispose();
    const created = fakeFetch(sim, { latencyMs: 150, items: DENSE });
    const fetchNew = (sid, vid, start, duration) => created.fetchPage(sid, vid, start, duration).then(page => ({ ...page, episodeId: vid }));
    newLoader = new DanmakuLoader('series', 'vid-new', fetchNew);
    newRequests = created.requests;
    newLoader.ensure(0, 30000);
  });

  await sim.run(T0 + 1600);

  check('旧集请求确实发出过一次', oldRequests.length === 1);
  check('旧集响应保存在自己的缓存键下（不会冒充新集）', oldLoader.pageFor(0) !== null);
  check('新集只看到自己的页，看不到旧集内容', newLoader.pageFor(0) !== null && newLoader.pageFor(0).episodeId === 'vid-new');
  check('旧集迟到响应不会引发新集重复请求', newRequests.length === 1, `实际=${newRequests.length}`);
  let resumedCalls = 0;
  const resumed = new DanmakuLoader('series', 'vid-old', async () => {
    resumedCalls += 1;
    throw new Error('缓存命中时不应再请求');
  });
  resumed.ensure(0, 30000);
  check('切回旧集直接复用在途完成的页（零新请求）', resumedCalls === 0 && resumed.pageFor(0) !== null);
});

await scenario('=== 11b. 同集同窗口跨实例并发也只发一次 ===', async () => {
  let calls = 0;
  let release;
  const fetchPage = () => {
    calls += 1;
    return new Promise(resolve => { release = () => resolve({ episodeId: 'vid', startMs: 0, nextMs: 30000, total: 0, items: [] }); });
  };
  const first = new DanmakuLoader('series', 'vid', fetchPage);
  const second = new DanmakuLoader('series', 'vid', fetchPage);
  first.ensure(0, 90000);
  second.ensure(0, 90000);
  check('两个加载器共享同一个在途请求', calls === 1, `实际=${calls}`);
  release();
  await new Promise(setImmediate);
  check('一次结果同时可被两个实例读取', first.pageFor(0) !== null && second.pageFor(0) !== null);
});

await scenario('=== 11c. 同一集同一窗口跨时长参数复用，不同窗口仍隔离 ===', async () => {
  const calls = [];
  const loader = new DanmakuLoader('series', 'vid', async (sid, vid, start, duration) => {
    calls.push({ start, duration });
    return { episodeId: vid, startMs: start, nextMs: Math.min(start + 30000, duration), total: 0, items: [] };
  });
  loader.ensure(60000, 90000);
  await new Promise(setImmediate);
  check('首窗边界由服务端返回', loader.pageFor(60000)?.nextMs === 90000);
  loader.ensure(60000, 95000);
  await new Promise(setImmediate);
  check('同一窗口的时长估计变化不重复请求', calls.length === 1);
  loader.ensure(0, 90000);
  await new Promise(setImmediate);
  check('另一个窗口仍是独立请求', calls.length === 2 && loader.pageFor(0)?.nextMs === 30000);
});

await scenario('=== 12. 跨 30s 窗口边界：预取必须仍然生效（不能引入"到边界才拉页"的回退）===', async () => {
  const fixed = await simulate({
    policy: 'fixed',
    latencyMs: 180,
    items: DENSE_LONG,
    playFromMs: 0,
    rowsSized: 12,
    horizonMs: 34000,
  });
  const windows = fixed.requests.map(r => r.startMs);
  check('首窗 + 预取 = 恰好两次请求', fixed.requests.length === 2, `实际=${windows.join(',')}`);
  const prefetch = fixed.requests.find(r => r.startMs === 30000);
  const prefetchAtPosition = prefetch ? prefetch.at - T0 : null;
  check('下一窗在 22s 前就预取（不是跨过边界才拉）',
    prefetchAtPosition != null && prefetchAtPosition >= 22000 && prefetchAtPosition < 30000,
    `实际位置=${prefetchAtPosition}ms`);
  const boundaryPlan = fixed.planEvents.find(e => e.startMs === 30000);
  check('跨边界那一帧就用缓存页排程（不重新请求）',
    boundaryPlan != null && boundaryPlan.t - T0 >= 30000 && boundaryPlan.t - T0 <= 30032,
    `实际=+${boundaryPlan ? boundaryPlan.t - T0 : 'null'}ms`);
});

await scenario('=== 13. seek 后退到未缓存的窗口：只补目标窗口，不重拉当前窗口 ===', async () => {
  const fixed = await simulate({
    policy: 'fixed',
    latencyMs: 180,
    items: DENSE_LONG,
    playFromMs: 0,
    startPositionMs: 40000,
    seek: { atMs: 300, toMs: 5000 },
    horizonMs: 4000,
  });
  const windows = fixed.requests.map(r => r.startMs);
  check('起播先拉 30s 窗口（40s 处）', windows[0] === 30000, `实际=${windows.join(',')}`);
  const target = fixed.requests.find(r => r.startMs === 0);
  check('后退到 5s 只补 0 窗口一次', windows.filter(w => w === 0).length === 1, `实际=${windows.join(',')}`);
  check('补窗请求紧跟在 seek 之后（不等到 30s 页边界）',
    target != null && target.at - T0 <= 300 + 260,
    `实际=+${target ? target.at - T0 : 'null'}ms`);
  check('seek 后很快重新出弹幕', rel(fixed.firstVisibleAt) <= 300 + 260 + 180 + 40,
    `实际=+${rel(fixed.firstVisibleAt)}ms`);
});

Date.now = realDateNow;
performance.now = realPerformanceNow;

console.log(`\n结果：${pass} 通过 / ${fail} 失败`);
if (fail > 0) process.exitCode = 1;