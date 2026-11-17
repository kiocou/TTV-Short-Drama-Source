#!/usr/bin/env node
/**
 * `verify:feed-series-nav` —— 「短剧推荐」页换剧/换集语义的确定性验收。
 *
 * 这一版验收针对「短剧推荐」的四条硬要求：
 *
 *   1. 上下方向键切**不同的短剧**，不是切不同的集；
 *   2. 滚轮向下 = 下一部；手指/鼠标**上滑** = 下一部（两者对"正方向"的定义相反）；
 *   3. 每次进入另一部推荐剧都从**第 1 集 0 秒**开始（首次进入、前进换剧、反向回
 *      上一部都一样），**不从观看历史强行恢复**；
 *   4. 停留时自动续播本剧第 2、3 集（推荐页强制连播，普通播放器仍听设置）；
 *      最后一集播完停留完成状态；手动「下一集」只在本剧内，末集不外溢。
 *
 * 分两层：
 *
 * - **行为层**：`feedNavigation.ts` / `feedSource.ts` 里的判定全是纯函数，
 *   这里直接跑边界（首部/末部/末集/可播边界/分页补页/节流窗口）。这一层是真正的
 *   行为验证，改错一个 `<` 就会红。
 * - **结构层**：`useFeedStore` 与 `usePlaybackStore` 的**接线**（谁调谁、哪条
 *   分支还在不在）只能读源码断言 —— 它们的故障形态是"某条旧分支被重新接回来"。
 *
 * ⚠️ 诚实边界：本脚本**不**证明真机行为（真实媒体 ended、真实 WebView2 事件顺序）。
 * 那部分要么跑 CDP e2e（`verify:feed-season-e2e`，需要运行中的实例），要么由用户
 * 自己点。静态断言绿 ≠ 真机验证过。
 *
 * 运行：node --experimental-strip-types scripts/verify-feed-series-nav.mjs
 */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  FEED_NAV_THROTTLE_MS,
  feedEpisodeStep,
  feedNavAllowed,
  feedPlayableLimit,
  feedSeriesIndex,
} from '../src/services/feedNavigation.ts';
import { FEED_WHEEL_MIN_DELTA, feedGestureDirection, feedWheelDirection } from '../src/services/feedSource.ts';
import { effectiveAutoNext } from '../src/services/autoNext.ts';
import { runSeriesNav } from '../src/services/feedSeriesNav.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');

let passed = 0;
const failures = [];

function check(name, ok, detail = '') {
  if (ok) {
    passed += 1;
    return;
  }
  failures.push({ name, detail });
}

function equal(name, actual, expected) {
  const ok = Object.is(actual, expected);
  check(name, ok, ok ? '' : `实际=${JSON.stringify(actual)} 期望=${JSON.stringify(expected)}`);
}

function readCode(relativePath) {
  const raw = readFileSync(resolve(ROOT, relativePath), 'utf8');
  // 注释里到处在**引用要被防住的旧写法**（"旧实现在这里调 continueAfterSeason"），
  // 不剥注释做包含判断，命中的永远是注释而不是代码。
  return raw
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/^\s*\/\/.*$/gm, '');
}

/** 截取具名箭头函数的函数体（从签名行到配平的 `}`），够用即可。 */
function extractBody(source, signature) {
  const start = source.indexOf(signature);
  if (start < 0) return null;
  const open = source.indexOf('{', start);
  if (open < 0) return null;
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    const ch = source[i];
    if (ch === '{') depth += 1;
    else if (ch === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(open, i + 1);
    }
  }
  return null;
}

// ───────────────────────────────────────────────────────────────────────────
// 1. 换剧下标：不绕回、不重播、分页边界先补页再判定
// ───────────────────────────────────────────────────────────────────────────

equal('下一部：0/3 → 1', feedSeriesIndex(0, 3, 'next'), 1);
equal('下一部：1/3 → 2', feedSeriesIndex(1, 3, 'next'), 2);
equal('下一部：末部（2/3）返回 null，不绕回第 0 部', feedSeriesIndex(2, 3, 'next'), null);
equal('上一部：2/3 → 1', feedSeriesIndex(2, 3, 'previous'), 1);
equal('上一部：首部（0/3）返回 null，不绕回末部', feedSeriesIndex(0, 3, 'previous'), null);
equal('上一部：1/3 → 0', feedSeriesIndex(1, 3, 'previous'), 0);
equal('只有一部时两个方向都是 null（不重播当前部）',
  `${feedSeriesIndex(0, 1, 'next')}/${feedSeriesIndex(0, 1, 'previous')}`,
  'null/null');
equal('空队列返回 null', feedSeriesIndex(0, 0, 'next'), null);
equal('非法下标返回 null', feedSeriesIndex(NaN, 3, 'next'), null);
// 分页边界：队列到底 → null；补一页之后再算就有目标了（store 里"先补页再重算"的依据）。
equal('分页边界：到底且未补页时为 null', feedSeriesIndex(2, 3, 'next'), null);
equal('分页边界：补页后 items=5 时目标为 3', feedSeriesIndex(2, 5, 'next'), 3);
equal('分页边界：补页回来是空页（items 没变）仍为 null', feedSeriesIndex(2, 3, 'next'), null);

// ───────────────────────────────────────────────────────────────────────────
// 2. 本剧内的换集：末集不外溢、可播边界夹紧
// ───────────────────────────────────────────────────────────────────────────

equal('下一集：第 1 集 → 第 2 集（3 集）', feedEpisodeStep(0, 3, 'next'), 1);
equal('下一集：末集（2/3）返回 null（不跨季、不换部）', feedEpisodeStep(2, 3, 'next'), null);
equal('上一集：第 1 集返回 null', feedEpisodeStep(0, 3, 'previous'), null);
equal('上一集：第 2 集 → 第 1 集', feedEpisodeStep(1, 3, 'previous'), 0);
equal('可播边界：详情 10 集但只开放前 2 集时，第 2 集（下标 1）处即停',
  feedEpisodeStep(1, 10, 'next', 2), null);
equal('可播边界：开放前 2 集时下标 0 仍能进下标 1',
  feedEpisodeStep(0, 10, 'next', 2), 1);
equal('可播边界：边界大于总集数时按总集数算', feedEpisodeStep(2, 3, 'next', 99), null);
equal('可播边界：边界为 0 视为未提供（否则"下一集"永远不可用）',
  feedEpisodeStep(0, 3, 'next', 0), 1);
equal('可播边界：NaN 视为未提供', feedEpisodeStep(0, 3, 'next', NaN), 1);
equal('可播边界：undefined 视为未提供', feedEpisodeStep(0, 3, 'next', undefined), 1);
equal('可播边界：非法集数返回 null', feedEpisodeStep(0, 0, 'next'), null);
equal('可播边界收口：min(总集数, 边界)', feedPlayableLimit(10, 5), 5);
equal('可播边界收口：边界缺失用总集数', feedPlayableLimit(10, undefined), 10);

// ───────────────────────────────────────────────────────────────────────────
// 3. 自动连播的有效值：推荐页强制、普通播放器听设置、不写回设置
// ───────────────────────────────────────────────────────────────────────────

equal('连播：推荐页即便用户关掉设置也有效', effectiveAutoNext(false, 'feed'), true);
equal('连播：推荐页默认值同样有效', effectiveAutoNext(undefined, 'feed'), true);
equal('连播：普通播放器尊重关闭', effectiveAutoNext(false, 'player'), false);
equal('连播：普通播放器默认开', effectiveAutoNext(true, 'player'), true);
equal('连播：设置缺字段（老库）落到开', effectiveAutoNext(undefined, 'explore'), true);
equal('连播：详情页同样尊重关闭', effectiveAutoNext(false, 'detail'), false);

// ───────────────────────────────────────────────────────────────────────────
// 4. 键鼠映射
// ───────────────────────────────────────────────────────────────────────────

equal('滚轮：向下（deltaY>0）= 下一部', feedWheelDirection(0, 120), 'next');
equal('滚轮：向上（deltaY<0）= 上一部', feedWheelDirection(0, -120), 'previous');
equal('滚轮：小增量（触控板抖动）不触发', feedWheelDirection(0, FEED_WHEEL_MIN_DELTA - 1), null);
equal('滚轮：恰好到阈值触发', feedWheelDirection(0, FEED_WHEEL_MIN_DELTA), 'next');
equal('滚轮：以横向为主的轨迹不算上下', feedWheelDirection(300, 120), null);
equal('拖滑：上滑（deltaY<0）= 下一部', feedGestureDirection(0, -80), 'next');
equal('拖滑：下滑（deltaY>0）= 上一部', feedGestureDirection(0, 80), 'previous');
equal('拖滑：轻点/短位移不触发', feedGestureDirection(0, -20), null);
equal('拖滑：以横向为主的轨迹不算上下', feedGestureDirection(-200, -80), null);
check('两套映射对"下一部"的正方向确实相反（这是不能共用公式的证据）',
  feedWheelDirection(0, 120) === 'next' && feedGestureDirection(0, 120) === 'previous');

// ───────────────────────────────────────────────────────────────────────────
// 6. 节流窗口（状态在 ref 里，重渲染不重置）
// ───────────────────────────────────────────────────────────────────────────

equal('节流：从未导航过时放行', feedNavAllowed(0, 1_000), true);
equal('节流：刚导航过（+100ms）拦截', feedNavAllowed(1_000, 1_100), false);
equal('节流：窗口边界（+550ms）放行', feedNavAllowed(1_000, 1_000 + FEED_NAV_THROTTLE_MS), true);
equal('节流：更久之后放行', feedNavAllowed(1_000, 9_000), true);
equal('节流：时间戳非法（NaN）视为未导航', feedNavAllowed(NaN, 1_000), true);
equal('节流：now 非法时不放行（不产生连锁触发）', feedNavAllowed(1_000, NaN), false);
equal('节流：阈值仍是 550ms（第三次改版实测值，不悄悄调松）', FEED_NAV_THROTTLE_MS, 550);

// ───────────────────────────────────────────────────────────────────────────
// 6b. 真的异步：连续两次补页失败、第三次成功（重试不得重播当前剧）
//
// 这一组不是源码扫描：它真的 `await` 一个假 loader，按计划让前两次补页失败、
// 第三次补进 2 部，然后看"开播下标"到底被写了几次、欠页标记怎么走。
// ───────────────────────────────────────────────────────────────────────────

const PAGE_ERROR_FALLBACK = '更多推荐加载失败';

/**
 * 假环境：`appendPage` 在 store 里**不会抛**（`appendNextPage` 自己 catch），
 * 失败的表现是"条目没变 + hasMore 仍为真 + 记一条**实际文本**的分页错误"。
 *
 * 失败文本刻意用 `Error('HTTP 503')` 走真实 `errorText` 的口径（message 优先），
 * 而不是那个通用常量：清理若拿常量比对，就会像修之前那样永远不命中。
 */
function makeNavEnv(plan, options = {}) {
  const state = {
    index: options.index ?? 2,
    count: options.count ?? 3,
    hasMore: options.hasMore ?? true,
    current: options.current ?? true,
    pending: false,
    error: null,
    pageErrorText: null,
    opened: [],
    attempts: 0,
    settled: 0,
    exhausted: 0,
    pendingWrites: 0,
  };
  const clearPageError = () => {
    if (state.pageErrorText === null) return;
    if (state.error === state.pageErrorText) state.error = null;
    state.pageErrorText = null;
  };
  const hooks = {
    readIndex: () => state.index,
    readItemCount: () => state.count,
    readHasMore: () => state.hasMore,
    isCurrent: () => state.current,
    appendPage: async () => {
      const step = plan[state.attempts] ?? { fails: new Error('HTTP 503') };
      state.attempts += 1;
      // 真实异步边界：1ms 后才出结果，期间别的动作（换频道/换剧）可以插进来。
      await new Promise(resolveStep => setTimeout(resolveStep, 1));
      if (step.fails) {
        // 与 store 的 catch 分支一致：`errorText(loadError, FEED_PAGE_ERROR)`，
        // Error 的 message 优先 —— 于是写进去的是 'HTTP 503'。
        const text = step.fails instanceof Error && step.fails.message
          ? step.fails.message
          : PAGE_ERROR_FALLBACK;
        state.error = text;
        state.pageErrorText = text;
        return;
      }
      clearPageError();
      state.count += step.adds ?? 0;
      state.hasMore = Boolean(step.hasMore);
    },
    openIndex: async (index) => {
      state.opened.push(index);
      state.index = index;
    },
    setPendingPage: (pending) => {
      state.pendingWrites += 1;
      state.pending = pending;
    },
    onExhausted: () => { state.exhausted += 1; },
    onPageSettled: () => {
      clearPageError();
      state.settled += 1;
    },
  };
  return { state, hooks };
}

// 场景 A：连续两次补页失败（真实文本 'HTTP 503'），第三次成功
{
  const env = makeNavEnv([
    { fails: new Error('HTTP 503') },
    { fails: new Error('HTTP 503') },
    { adds: 2, hasMore: false },
  ]);
  await runSeriesNav('next', env.hooks);
  equal('连续失败①：欠页标记保持（下次重试才会继续补页）', env.state.pending, true);
  equal('连续失败①：没有打开任何剧（绝不重播当前剧）', env.state.opened.length, 0);
  equal('连续失败①：不喊"最后一部"', env.state.exhausted, 0);
  equal('连续失败①：错误区写的是真实文本（HTTP 503），不是通用常量',
    env.state.error, 'HTTP 503');
  await runSeriesNav('next', env.hooks);
  equal('连续失败②：欠页标记仍然保持（清早了就会掉进重播分支）', env.state.pending, true);
  equal('连续失败②：仍然没有打开任何剧', env.state.opened.length, 0);
  await runSeriesNav('next', env.hooks);
  equal('补页成功：欠页标记清掉', env.state.pending, false);
  equal('补页成功：只开一次，且是下一部（下标 3，不是重播当前 2）',
    env.state.opened.join(','), '3');
  equal('补页成功：当前下标前进到 3', env.state.index, 3);
  equal('补页成功：按"写入时的实际文本"清掉遗留分页错误', env.state.error, null);
  equal('补页成功：分页错误记账也被清（不会残留到下一次）', env.state.pageErrorText, null);
  equal('补页成功：一共补页 3 次', env.state.attempts, 3);
}

// 场景 B：补页成功但一个新条目都没有、而且已经没更多页 → 收尾，不重播
{
  const env = makeNavEnv([{ adds: 0, hasMore: false }]);
  await runSeriesNav('next', env.hooks);
  equal('空页到底：欠页标记清掉（否则之后每次重试都补同一页）', env.state.pending, false);
  equal('空页到底：不打开任何剧', env.state.opened.length, 0);
  equal('空页到底：清掉遗留分页错误', env.state.error, null);
  equal('空页到底：给"最后一部"提示恰好一次', env.state.exhausted, 1);
}

// 场景 C：补页期间用户换走了（下标变了）→ 放弃这次导航
{
  const env = makeNavEnv([]);
  const hooks = {
    ...env.hooks,
    appendPage: async () => {
      await new Promise(resolveStep => setTimeout(resolveStep, 1));
      env.state.index = 1;
      env.state.count = 5;
    },
  };
  await runSeriesNav('next', hooks);
  equal('补页期间用户换走：不把用户偷切到下一部', env.state.opened.length, 0);
  equal('补页期间用户换走：下标仍是用户选的那个', env.state.index, 1);
}

// 场景 D：队列里还有下一部时根本不补页
{
  const env = makeNavEnv([], { index: 0, count: 3, hasMore: true });
  await runSeriesNav('next', env.hooks);
  equal('还有下一部：直接打开下标 1', env.state.opened.join(','), '1');
  equal('还有下一部：不触发补页', env.state.attempts, 0);
  await runSeriesNav('next', env.hooks);
  equal('再按一次：1 → 2', env.state.opened.join(','), '1,2');
  equal('上一部方向：从 2 回到 1', (await runSeriesNav('previous', env.hooks), env.state.opened.join(',')), '1,2,1');
}

// 场景 E：补页期间换了频道（**下标恰好相同**、代数已变）→ 整段续体作废
//
// 这是最容易漏的一类：新旧频道都停在第 0 部，只比下标会"看起来一切正常"，
// 于是旧续体会对着新频道清欠页标记、或喊"列表到底了"。
{
  const env = makeNavEnv([{ adds: 2, hasMore: false }], { index: 0, count: 1, hasMore: true });
  const hooks = {
    ...env.hooks,
    appendPage: async () => {
      await new Promise(resolveStep => setTimeout(resolveStep, 1));
      // 换频道：新一代在跑，下标仍然是 0、条目也"变多了"，但这段续体已无效。
      env.state.current = false;
      env.state.count = 3;
    },
  };
  const writesBefore = env.state.pendingWrites;
  await runSeriesNav('next', hooks);
  equal('代数已变：不打开任何剧', env.state.opened.length, 0);
  equal('代数已变：不加欠页标记也不清它（一个字段都不碰）',
    env.state.pendingWrites, writesBefore);
  equal('代数已变：不触发补页收尾（onPageSettled）', env.state.settled, 0);
  equal('代数已变：不喊"最后一部"', env.state.exhausted, 0);
}

// 场景 F：进门时代数就已经作废（换频道后残留的调用）→ 一个副作用都不发生
{
  const env = makeNavEnv([], { index: 0, count: 3, hasMore: true, current: false });
  await runSeriesNav('next', env.hooks);
  equal('进门即过期：不打开、不补页、不写标记、不提示',
    `${env.state.opened.length}/${env.state.attempts}/${env.state.pendingWrites}/${env.state.exhausted}`,
    '0/0/0/0');
}

// ───────────────────────────────────────────────────────────────────────────
// 7. store 接线：旧的三条跨内容分支必须真的消失
// ───────────────────────────────────────────────────────────────────────────

const feedStoreCode = readCode('src/stores/useFeedStore.ts');
const openSeriesBody = extractBody(feedStoreCode, 'const openSeriesAt = useCallback(async');
const loadInitialBody = extractBody(feedStoreCode, 'const loadInitial = useCallback(async');
const switchBody = extractBody(feedStoreCode, 'const switchSeries = useCallback(async');
const nextEpisodeBody = extractBody(feedStoreCode, 'const nextEpisode = useCallback(() =>');
const prevEpisodeBody = extractBody(feedStoreCode, 'const previousEpisode = useCallback(() =>');
const finishedBody = extractBody(feedStoreCode, 'const advanceAfterFinished = useCallback(');
const retryBody = extractBody(feedStoreCode, 'const retry = useCallback(() =>');

check('能抽到 openSeriesAt / loadInitial / switchSeries 函数体',
  Boolean(openSeriesBody && loadInitialBody && switchBody),
  '签名被改名或重构 —— 本脚本的结构断言需要同步');
check('能抽到 nextEpisode / previousEpisode / advanceAfterFinished / retry 函数体',
  Boolean(nextEpisodeBody && prevEpisodeBody && finishedBody && retryBody));

check('跨季分支已删除：整个 store 里不再有 continueAfterSeason',
  !feedStoreCode.includes('continueAfterSeason'),
  '自动跨季又回来了 —— 末集播完会去下一季');
check('换剧不再查季列表：store 里不再有 seriesSeasons',
  !feedStoreCode.includes('seriesSeasons'));
check('换剧不再读历史：store 里不再有 latestHistoryForFeed / pickResumeEpisode',
  !feedStoreCode.includes('latestHistoryForFeed(') && !feedStoreCode.includes('pickResumeEpisode('),
  '历史续播又接回来了 —— 换一部会落进第 8 集');
check('首屏不再 loadHistory()（历史与推荐流入口点无关）',
  Boolean(loadInitialBody) && !loadInitialBody.includes('loadHistory'));
check('首屏队列不再插"最近一部历史"（历史别名不再出现）',
  Boolean(loadInitialBody) && !loadInitialBody.includes('history'));

check('openSeriesAt 从首集 0 秒起播（写死 0,0，不再有起播点包装函数）',
  Boolean(openSeriesBody) && /startEpisode\(item,\s*nextDetail,\s*0,\s*0\)/.test(openSeriesBody));
check('openSeriesAt 仍把 detail 一起交给 openEpisode（省一次往返）',
  Boolean(openSeriesBody) && openSeriesBody.includes('startEpisode('));
check('retry 在欠页时**直接复用** switchSeries，不提前清标记、不复制补页逻辑',
  Boolean(retryBody) && /if \(pendingPageRef\.current\) \{[\s\S]{0,60}switchSeries\('next'\)/.test(retryBody)
  && !retryBody.includes('appendNextPage(')
  && !retryBody.includes('openSeriesAt(nextIndex'),
  'retry 自己又写了一份补页/开播 —— 串行锁与下标核对被绕过，连续失败后还会重播当前剧');
check('retry 不提前把 pendingPageRef 置假（清标记只在 runSeriesNav 里做）',
  Boolean(retryBody) && !/pendingPageRef\.current\s*=\s*false/.test(retryBody));
check('retry 仍保留首集 0 秒重播兜底（加载失败 / 播完后的「重播」）',
  Boolean(retryBody) && retryBody.includes('openSeriesAt(currentIndexRef.current'));

check('switchSeries 的判定交给 runSeriesNav（补页/下标核对只有一处）',
  Boolean(switchBody) && switchBody.includes('runSeriesNav('));
check('switchSeries 的导航锁按代数记账，旧 finally 不放开新代锁',
  Boolean(switchBody)
  && switchBody.includes('navLockRef.current = requestId')
  && switchBody.includes('if (navLockRef.current === requestId) navLockRef.current = null;'));
check('loadInitial 换代时重置导航状态（锁与欠页标记）',
  Boolean(loadInitialBody) && loadInitialBody.includes('navLockRef.current = null')
  && loadInitialBody.includes('pendingPageRef.current = false'));
check('switchSeries 没有下一部时**不改**播放状态（不置完成、不弹错误字符串）',
  Boolean(switchBody) && !switchBody.includes('setIsCompleted')
  && !/setError\(\s*'/.test(switchBody),
  '队列到底又被当成"本剧播完" —— 正常播放时按 ↓ 会弹出错误/重播遮罩');
check('队列到底只给一条轻提示（showToast），不写错误区',
  Boolean(switchBody) && switchBody.includes('showToast('));
check('switchSeries 把"是否仍属当前代数"传给 runSeriesNav',
  Boolean(switchBody) && switchBody.includes('isCurrent: () => requestId === generationRef.current'));

// ── 分页错误记账：必须按"当次写入的实际文本"清理 ──────────────────────────
const appendBody = extractBody(feedStoreCode, 'const startPageJob = useCallback(');
check('补页任务失败先把"实际写入的文本"记进 pageErrorRef',
  Boolean(appendBody) && appendBody.includes('pageErrorRef.current = text')
  && appendBody.includes('errorText(loadError, FEED_PAGE_ERROR)'),
  '只写 setError 不记账 —— 清理时无从比对');
check('清理按 pageErrorRef 的实际文本比对（不是拿常量比 prev）',
  feedStoreCode.includes('const text = pageErrorRef.current;')
  && feedStoreCode.includes('prev === text ? null : prev')
  && !feedStoreCode.includes('prev === FEED_PAGE_ERROR'),
  '`prev === FEED_PAGE_ERROR` 在 Error("HTTP 503") 这类真实文本下永远不命中，清理等于没做');
check('补页任务成功分支走 clearPageError（清完把记账也置空）',
  Boolean(appendBody) && appendBody.includes('clearPageError()'));
check('补页任务的 finally 只在当前代数才放开 loadingMore',
  Boolean(appendBody)
  && /if \(requestId === generationRef\.current\) \{\s*loadingMoreRef\.current = false;/.test(appendBody),
  '换频道后旧续体放开闸门 —— 新频道的补页会被重复发放');
check('补页任务后台失败不写界面错误（只有前后台口径下的前台失败才可见）',
  Boolean(appendBody) && appendBody.includes('job.foreground'),
  '后台补页失败会把正在看的画面盖成错误');
const appendEntryStart = feedStoreCode.indexOf('const appendNextPage');
const appendEntryEnd = feedStoreCode.indexOf('const maybePrefetchPage = useCallback');
const appendEntryBody = appendEntryStart >= 0 && appendEntryEnd > appendEntryStart
  ? feedStoreCode.slice(appendEntryStart, appendEntryEnd)
  : null;
check('补页在途时前台导航复用同一个 Promise（不 return []）',
  Boolean(appendEntryBody) && appendEntryBody.includes('return running.promise;'),
  '后台补页在途时滑到边界会"滑了一下没反应"');
check('loadInitial 换代时也重置分页错误记账',
  Boolean(loadInitialBody) && loadInitialBody.includes('pageErrorRef.current = null'));
check('switchSeries 使用 navLockRef 串行化翻页且不被开播中阻塞切剧',
  Boolean(switchBody) && switchBody.includes('navLockRef.current') && !switchBody.includes('openingRef.current'));

check('nextEpisode 用 feedEpisodeStep（不再自己 +1 后越界访问）',
  Boolean(nextEpisodeBody) && nextEpisodeBody.includes('feedEpisodeStep('));
check('nextEpisode 在边界处直接 return（末集/可播边界都不外溢）',
  Boolean(nextEpisodeBody) && /if\s*\(nextIndex === null\)\s*return;/.test(nextEpisodeBody));
check('nextEpisode 不再调用任何"换内容"的函数',
  Boolean(nextEpisodeBody)
  && !nextEpisodeBody.includes('continueAfterSeason')
  && !nextEpisodeBody.includes('switchSeries')
  && !nextEpisodeBody.includes('seriesSeasons'));
check('nextEpisode 把可播边界传进去了',
  Boolean(nextEpisodeBody) && nextEpisodeBody.includes('playableEpisodes'));

check('previousEpisode 在首集处直接 return（不绕回上一部）',
  Boolean(prevEpisodeBody) && /if\s*\(nextIndex === null\)\s*return;/.test(prevEpisodeBody)
  && prevEpisodeBody.includes("'previous'"));

check('advanceAfterFinished 只置完成态（停留），不再继续任何内容',
  Boolean(finishedBody) && finishedBody.includes('setIsCompleted(true)')
  && !finishedBody.includes('continueAfterSeason')
  && !finishedBody.includes('switchSeries')
  && !finishedBody.includes('openSeriesAt'));
check('advanceAfterFinished 仍有会话去重（一次播完只结算一次）',
  Boolean(finishedBody) && finishedBody.includes('finishedSessionRef.current === sessionId'));

check('startEpisode 仍带 detailOverride（verify:feed-playback 同一条契约）',
  /openEpisodeRef\.current\(item\.id,\s*episode\.id,\s*position,\s*undefined,\s*nextDetail\)/.test(feedStoreCode));

// ───────────────────────────────────────────────────────────────────────────
// 8. 播放器侧：自动连播必须留在**同一部剧**里
// ───────────────────────────────────────────────────────────────────────────

const playbackCode = readCode('src/stores/usePlaybackStore.tsx');
const playNextBody = extractBody(playbackCode, 'const playNextEpisode = useCallback(() =>');
const endedBody = extractBody(playbackCode, 'const handleEnded = () =>');

check('能抽到 playNextEpisode / handleEnded 函数体', Boolean(playNextBody && endedBody));
check('自动下一集用 currentSeries.id（不会切到另一部剧）',
  Boolean(playNextBody) && /openEpisode\(currentSeries\.id,\s*nextEp\.id,\s*0\)/.test(playNextBody),
  '换集时把 seriesId 换掉了 —— 连播会悄悄换剧');
check('自动下一集的边界是"本剧最后一集"（下标 +1 越界就不动）',
  Boolean(playNextBody) && playNextBody.includes('currentSeries.episodes.length - 1'));
check('末集播完置 finishedAll（停留完成态的信号源）',
  Boolean(endedBody) && endedBody.includes('setFinishedAll(true)'));
check('handleEnded 里没有任何换剧调用（换剧只由用户手势发起）',
  Boolean(endedBody) && !endedBody.includes('switchSeries') && !endedBody.includes('openSeriesAt'));
check('handleEnded 仍受连播开关约束（ctxEnded.autoNext）',
  Boolean(endedBody) && endedBody.includes('ctxEnded.autoNext'));

check('事件上下文的 autoNext 取"有效值"（推荐页强制连播）',
  playbackCode.includes('autoNext: effectiveAutoNext(settings.autoNext, currentView)'),
  '仍然直接用 settings.autoNext —— 用户关掉连播后推荐页停留不续播');
check('两处 handlerCtxRef 赋值都走 effectiveAutoNext（初次与每次渲染）',
  (playbackCode.match(/autoNext: effectiveAutoNext\(settings\.autoNext, currentView\)/g) || []).length === 2);
check('没有把"推荐页强制连播"写回用户设置（updateSettings 里不出现 autoNext）',
  !/updateSettings\(\{[^}]*autoNext/.test(playbackCode));
check('播放器侧直接读 useAppStore 的 currentView（不新建视图/窗口 API，也不留空转 ref）',
  playbackCode.includes('const { currentView } = useAppStore();')
  && !playbackCode.includes('currentViewRef'));

// ───────────────────────────────────────────────────────────────────────────
// 9. 组件侧接线：末集按钮禁用、事件参数隔离
// ───────────────────────────────────────────────────────────────────────────

const controlsCode = readCode('src/components/player/FeedControls.tsx');
const viewCode = readCode('src/components/views/FeedView.tsx');
const bannerCode = readCode('src/components/player/FeedStatusBanner.tsx');

check('详情面板「下一集」用 feedEpisodeStep 判边界（与底条同一口径）',
  controlsCode.includes("feedEpisodeStep(episodeIndex, episodeCount, 'next', playableEpisodes) !== null"),
  '仍然只判 busy/episodeCount —— 末集按钮亮着却点不动');
check('详情面板「下一集」的 disabled 用的是 canGoNext',
  controlsCode.includes('disabled={!canGoNext}'));
check('底条 canGoNext 与面板同一函数',
  viewCode.includes('feedEpisodeStep(') && viewCode.includes('const canGoNext = !busy &&'));
check('「换一部」入口用箭头函数隔离事件参数（不会把 MouseEvent 当方向）',
  bannerCode.includes('onClick={() => onSwitchSeries()}')
  && viewCode.includes("onSwitchSeries={() => feed.switchSeries('next')}")
  && controlsCode.includes('onSwitchSeries();'),
  '直接 onClick={onSwitchSeries} 会把事件对象当方向传进去');
check('feedNavigation 不再提供起播点/方向归一化包装',
  !viewCode.includes('feedSeriesEntry') && !viewCode.includes('normalizeFeedDirection')
  && !controlsCode.includes('normalizeFeedDirection'));

// ───────────────────────────────────────────────────────────────────────────
// 报告
// ───────────────────────────────────────────────────────────────────────────

for (const item of failures) {
  console.error(`FAIL ${item.name}${item.detail ? `\n     ${item.detail}` : ''}`);
}
const total = passed + failures.length;
console.log(`\nverify:feed-series-nav —— ${passed}/${total} 通过, ${failures.length} 失败`);
process.exit(failures.length === 0 ? 0 : 1);