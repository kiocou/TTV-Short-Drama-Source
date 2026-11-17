// 验证「连续 seek 合并」与「缓冲态分级」。
//
// 运行：node --experimental-strip-types scripts/verify-seek-coalesce.mjs
//      （package.json 里应注册为 `verify:seek-coalesce`）
//
// ## 守的是哪次故障（2026-11-10）
//
// 用户连按快进时画面永远停在「缓冲中」。ttv-playback.log 会话 273：
//
//   +4672.046 立即落位 目标=157.7s
//   +4672.219 立即落位 目标=162.7s
//   +4672.391 立即落位 目标=167.7s
//   +4673.469 立即落位 目标=173.5s
//   +4674.246 立即落位 目标=178.9s
//   +4675.426 立即落位 目标=174.9s
//
// 5 秒内 6 次 `video.currentTime` 赋值 = 6 次 MSE 重建解码管线。旧的两道去重
// 都要求目标相同，连按快进目标每次 +5，所以一次都没生效。
//
// 这份用例钉住三件事：
//   1. 空闲后的第一次点击**必须立即落位**（不能为了合并让单击变慢）；
//   2. 窗口内的连按只落位最后一次（debounce），而**不是**丢弃用户最后的意图；
//   3. 同一点反复点只落位一次。
//
// 第 2 条用真实日志序列回放，并把「纯节流」「固定窗口」两种替代方案的结果
// 一并算出来做对照 —— 这样注释里那句「为什么不是节流」有数据可查。

import {
  INITIAL_SEEK_COALESCE_STATE,
  SEEK_COALESCE_SAME_TARGET_EPSILON_SECONDS,
  SEEK_COALESCE_WINDOW_MS,
  describePlaybackStage,
  flushSeek,
  requestSeek,
  resetSeekCoalesce,
} from '../src/services/seekPlan.ts';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

let pass = 0;
let fail = 0;

function check(name, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (ok) pass += 1;
  else fail += 1;
  console.log(
    (ok ? 'PASS ' : 'FAIL ') + name +
    (ok ? '' : '  期望=' + JSON.stringify(expected) + ' 实际=' + JSON.stringify(actual)),
  );
}

/** 只看判定的几个关键字段，避免整对象比较被无关字段（merged/state）带崩。 */
function shape(decision) {
  return {
    kind: decision.kind,
    target: decision.target,
    flushAt: decision.flushAt,
    reason: decision.reason,
    merged: decision.merged,
  };
}

// ── 1. leading：第一次点击必须立即落位 ──────────────────────────────────
const first = requestSeek(INITIAL_SEEK_COALESCE_STATE, 12.5, 1000);
check('首次点击 → 立即落位（leading）', shape(first), {
  kind: 'dispatch', target: 12.5, flushAt: undefined, reason: 'leading', merged: 0,
});

// 距离刚落的点已经超过窗口 + 目标不同 = 用户的独立意图，也要立即生效。
const idleAfter = requestSeek(first.state, 40, 1000 + SEEK_COALESCE_WINDOW_MS + 1);
check('空闲超过窗口再点 → 立即落位', shape(idleAfter), {
  kind: 'dispatch', target: 40, flushAt: undefined, reason: 'leading', merged: 0,
});

// ── 2. pending：窗口内的不同目标一律不落位 ─────────────────────────────
const windowed = requestSeek(first.state, 17.5, 1100);
check('窗口内换目标 → 记 pending，不落位', shape(windowed), {
  kind: 'pending', target: 17.5, flushAt: 1100 + SEEK_COALESCE_WINDOW_MS, reason: undefined, merged: 1,
});
check('pending 期间 lastDispatch 不变（播放头没被动过）', windowed.state.lastDispatch, { target: 12.5, at: 1000 });

// 后到的目标覆盖先到的：用户的意图是"最后停在哪"。
const override = requestSeek(windowed.state, 22.5, 1200);
check('窗口内再换目标 → 覆盖旧 pending', override.state.pending, { target: 22.5, at: 1200 });
check('覆盖后到期时刻后移（debounce）', override.flushAt, 1200 + SEEK_COALESCE_WINDOW_MS);
check('覆盖计数累加', override.merged, 2);

// ⚠️ pending 非空时即使"距上次落位已超过窗口"也不能插队：
// 那会在一次连按动作中多出一次本可以省掉的落位。
const notInterleave = requestSeek(override.state, 27.5, 1000 + SEEK_COALESCE_WINDOW_MS + 500);
check('pending 在途时不插队（仍走 pending）', notInterleave.kind, 'pending');

// ── 3. flush：到期才落位，且落的是最后一次的目标 ───────────────────────
const early = flushSeek(override.state, 1300);
check('提前唤醒 → not-due，给出真实到期时刻', shape(early), {
  kind: 'not-due', target: undefined, flushAt: 1200 + SEEK_COALESCE_WINDOW_MS, reason: undefined, merged: undefined,
});

const due = flushSeek(override.state, 1200 + SEEK_COALESCE_WINDOW_MS);
check('到期 → 落位最后一次的目标', shape(due), {
  kind: 'dispatch', target: 22.5, flushAt: undefined, reason: 'trailing', merged: 2,
});
check('落位后 pending 清空', due.state.pending, null);
check('落位后 lastDispatch 指向新目标', due.state.lastDispatch.target, 22.5);

check('没有 pending 时 flush → 无事可做', flushSeek(due.state, 99999).reason, 'nothing-pending');

// ── 4. 同一点反复点：只落位一次 ────────────────────────────────────────
//
// 旧实现的症状是「点几次画面跳几次」：每次赋值 currentTime 都重建解码管线。
// 合并器必须连这一半也一起治——只在 leading 落一次，后续同目标全吞掉。
let repeatState = requestSeek(INITIAL_SEEK_COALESCE_STATE, 34, 5000).state;
let repeatDispatches = 1;
let repeatIgnored = 0;
for (let i = 1; i <= 4; i += 1) {
  const decision = requestSeek(repeatState, 34, 5000 + i * 150);
  repeatState = decision.state;
  if (decision.kind === 'dispatch') repeatDispatches += 1;
  if (decision.kind === 'ignored' && decision.reason === 'repeat-target') repeatIgnored += 1;
}
check('连点同一点 5 次 → 只落位 1 次', repeatDispatches, 1);
check('其后 4 次全被判成重复目标', repeatIgnored, 4);
check('重复点不会留 pending（否则到期还会再落一次）', repeatState.pending, null);

// 同一点但已经过了窗口：那是"我又想看这一段"，要重新落位。
const repeatLate = requestSeek(repeatState, 34.1, 5000 + SEEK_COALESCE_WINDOW_MS + 10);
check('同一点但超出窗口 → 重新落位', repeatLate.kind, 'dispatch');

// ── 4b. 拖动终点：必须立即落位，不许压进合并窗口 ──────────────────────
//
// 进度条一次拖动打两次 seek（down 起点 / up 终点），两者可差几十秒。若终点也
// 进合并窗口，用户会看到"松手后停在起点 1.2 秒，然后才跳到终点"——比原来的
// "松手即跳"更糟。判据由 store 给（刚有过 previewing 写入），纯函数只认开关。
const dragDown = requestSeek(INITIAL_SEEK_COALESCE_STATE, 10, 1000);
const dragUp = requestSeek(dragDown.state, 40, 1400, {}, { immediate: true });
check('拖动终点（immediate）→ 立刻落位', shape(dragUp), {
  kind: 'dispatch', target: 40, flushAt: undefined, reason: 'drag-end', merged: 0,
});

// 在途 pending 被终点覆盖：用户已经拖到新位置，旧目标不该再收尾一次。
const pendingBeforeDrag = requestSeek(dragDown.state, 15, 1100).state;
const coverPending = requestSeek(pendingBeforeDrag, 60, 1200, {}, { immediate: true });
check('拖动终点覆盖在途 pending 并立即落位', {
  kind: coverPending.kind,
  target: coverPending.target,
  pending: coverPending.state.pending,
}, { kind: 'dispatch', target: 60, pending: null });

// 但"同目标、窗口内"的吞并优先于 immediate：拖回刚落过的位置不该再落一次。
check('immediate 不打破"同目标窗口内"的吞并',
  requestSeek(dragDown.state, 10, 1300, {}, { immediate: true }).kind, 'ignored');

// ── 5. 非法输入不改变状态 ──────────────────────────────────────────────
const invalid = requestSeek(INITIAL_SEEK_COALESCE_STATE, NaN, 100);
check('目标 NaN → ignored，不污染状态', shape(invalid), {
  kind: 'ignored', target: undefined, flushAt: undefined, reason: 'invalid-target', merged: undefined,
});
check('非法请求后状态仍是初始值', invalid.state, INITIAL_SEEK_COALESCE_STATE);

// ── 6. 不可变性：判定函数不许改传入的状态 ──────────────────────────────
const snapshot = JSON.stringify(override.state);
void requestSeek(override.state, 99, 1234);
void flushSeek(override.state, 999999);
check('requestSeek / flushSeek 不修改传入状态', JSON.stringify(override.state), snapshot);
check('resetSeekCoalesce 回到初始状态', resetSeekCoalesce(), INITIAL_SEEK_COALESCE_STATE);

// ── 7. 真实日志回放（会话 273） ────────────────────────────────────────
//
// 时间用日志里的毫秒绝对时刻（+4672.046 之类），目标就是当日那串读数。
const SESSION_273 = [
  { at: 4672046, target: 157.7 },
  { at: 4672219, target: 162.7 },
  { at: 4672391, target: 167.7 },
  { at: 4673469, target: 173.5 },
  { at: 4674246, target: 178.9 },
  { at: 4675426, target: 174.9 },
];

/**
 * 用合并状态机回放一串 (时刻, 目标)，并像真实 store 那样跑定时器。
 *
 * pump 里用 `timerAt <= upTo` 推进：真实世界里 setTimeout 到点才会触发，
 * 这里把"到点"压缩成按事件时刻推进，保证回放是确定性的。
 */
function replayCoalesced(events, windowMs) {
  let state = INITIAL_SEEK_COALESCE_STATE;
  let timerAt = null;
  const dispatches = [];
  const pump = (upTo) => {
    let guard = 0;
    while (timerAt !== null && timerAt <= upTo) {
      if (++guard > 1000) throw new Error('replay pump 死循环');
      const at = timerAt;
      const decision = flushSeek(state, at, { windowMs });
      state = decision.state;
      if (decision.kind === 'dispatch') {
        dispatches.push({ at, target: decision.target, reason: decision.reason, merged: decision.merged });
        timerAt = null;
      } else if (decision.kind === 'not-due') {
        timerAt = decision.flushAt;
      } else {
        timerAt = null;
      }
    }
  };
  for (const event of events) {
    pump(event.at);
    const decision = requestSeek(state, event.target, event.at, { windowMs });
    state = decision.state;
    if (decision.kind === 'dispatch') {
      dispatches.push({ at: event.at, target: decision.target, reason: decision.reason, merged: decision.merged });
    } else if (decision.kind === 'pending') {
      timerAt = decision.flushAt;
    }
  }
  pump(Number.MAX_SAFE_INTEGER);
  return { dispatches, state };
}

const coalesced = replayCoalesced(SESSION_273, SEEK_COALESCE_WINDOW_MS);
check('会话 273 回放：落位次数从 6 降到 2', coalesced.dispatches.length, 2);
check('会话 273 回放：第一次落位是日志第一条（leading）', coalesced.dispatches[0], {
  at: 4672046, target: 157.7, reason: 'leading', merged: 0,
});
check('会话 273 回放：最终落位是用户最后一次拖到的 174.9s', {
  target: coalesced.dispatches[coalesced.dispatches.length - 1].target,
  reason: coalesced.dispatches[coalesced.dispatches.length - 1].reason,
  merged: coalesced.dispatches[coalesced.dispatches.length - 1].merged,
}, { target: 174.9, reason: 'trailing', merged: 5 });

// ── 8. 对照：为什么不是"纯节流" ───────────────────────────────────────
//
// 「纯节流」= 窗口内只认第一次，后面的直接丢弃、不做收尾。它在会话 273 上
// 落位 3 次（比合并多一次），看起来差别不大 —— 但它在**快速连按**上会把用户
// 的意图整段丢掉：按 8 下只前进第一下的 5 秒。
function replayPureThrottle(events, windowMs) {
  let lastAt = null;
  const dispatches = [];
  for (const event of events) {
    if (lastAt !== null && event.at - lastAt < windowMs) continue;
    lastAt = event.at;
    dispatches.push(event);
  }
  return dispatches;
}

// ── 9. 长连按：8 下 +5，间隔 150ms ────────────────────────────────────
const longPress = [];
for (let i = 0; i < 8; i += 1) longPress.push({ at: i * 150, target: 10 + i * 5 });
const longPressResult = replayCoalesced(longPress, SEEK_COALESCE_WINDOW_MS);
check('连按 8 下 → 只落位 2 次（首下 + 收尾）', longPressResult.dispatches.length, 2);
check('连按 8 下 → 收尾落在最后一下的目标 45s', longPressResult.dispatches[1].target, 45);

const throttled = replayPureThrottle(SESSION_273, SEEK_COALESCE_WINDOW_MS);
const throttledLongPress = replayPureThrottle(longPress, SEEK_COALESCE_WINDOW_MS);
check('对照·纯节流在会话 273 上落在 3 次（比合并多一次重建）', throttled.length, 3);
check('对照·纯节流在 8 连按上只落位 1 次，且目标是首下的 10s（用户的 45s 被丢掉）', {
  count: throttledLongPress.length,
  last: throttledLongPress[throttledLongPress.length - 1].target,
}, { count: 1, last: 10 });
console.log(
  `     对照数据（会话 273，6 条立即落位）：合并 ${coalesced.dispatches.length} 次（${coalesced.dispatches
    .map((entry) => entry.target.toFixed(1) + 's')
    .join(' → ')}）· 纯节流 ${throttled.length} 次（${throttled
    .map((entry) => entry.target.toFixed(1) + 's')
    .join(' → ')}）· 8 连按：合并 ${longPressResult.dispatches.length} 次到 ${longPressResult.dispatches[1].target.toFixed(1)}s，纯节流 1 次到 ${throttledLongPress[0].target.toFixed(1)}s`,
);

// ── 10. 常量钉住（改动等于改变行为） ───────────────────────────────────
check('合并窗口 = 1200ms（沿用 REPEAT_SEEK_WINDOW_MS）', SEEK_COALESCE_WINDOW_MS, 1200);
check('同目标容差 = 0.35s（沿用 SAME_TARGET_EPSILON_SECONDS）', SEEK_COALESCE_SAME_TARGET_EPSILON_SECONDS, 0.35);

// ── 11. 接入断言：store 真的用了这套状态机 ────────────────────────────
//
// 纯函数全绿但没接进播放链路，是这类修复最常见的假通过。
const store = readFileSync(join(import.meta.dirname, '..', 'src/stores/usePlaybackStore.tsx'), 'utf8');
check('主播放器 seek 走 requestSeek', /requestSeek\(\s*seekCoalesceRef\.current/.test(store), true);
check('到期落位走 flushSeek', /flushSeek\(\s*seekCoalesceRef\.current/.test(store), true);
check('换集会清空合并状态', /cancelSeekCoalesce\(\);[\s\S]{0,1200}?runOpenEpisode/.test(store) || /openEpisode[\s\S]{0,1200}?cancelSeekCoalesce\(\)/.test(store), true);
check('离开播放器会清空合并状态', /stopPlayback = useCallback[\s\S]{0,2000}?cancelSeekCoalesce\(\)/.test(store), true);

// ⚠️ 行为契约：合并到期后的落位必须**重新问一次**当前元素的可寻址范围。
//
// 不能拿 1.2 秒前那次判定的结论直接写播放头 —— 这 1.2 秒里源可能已被换掉
// （切清晰度 / 看门狗重装载 / 增强切链路）。所以 `dispatchSeek` 内部必须出现
// `planSeek`，且 trailing 分支只能经它落位（不能有自己的 currentTime 赋值）。
check('合并到期后重新跑 planSeek（不信任 1.2 秒前的可寻址判定）',
  /const dispatchSeek = \([\s\S]{0,900}?const plan = planSeek\(\{/.test(store), true);
check('trailing 落位与 leading 走同一条落位路径（dispatchSeek）',
  /const flushed = flushSeek\([\s\S]{0,1400}?dispatchSeek\(video, flushed\.target/.test(store), true);
check('flush 回调里没有第二处 currentTime 赋值',
  /const flushed = flushSeek\([\s\S]{0,1400}?video\.currentTime =/.test(store), false);

// 暴露给竖滑流（B）的三个只读派生标量：只读 uiState，不新增状态源。
check('store 暴露 isBuffering', /isBuffering: boolean;/.test(store) && /\n\s+isBuffering,\r?\n/.test(store), true);
check('isBuffering 是纯派生而不是新状态', /const isBuffering = uiState\.kind === 'buffering';/.test(store), true);
check('isBuffering 不含 opening（两者是两种等待）', /const isBuffering = [^;\n]*'opening'/.test(store), false);
check('store 暴露 stageLabel 且形状为 opening|buffering|null',
  /stageLabel: 'opening' \| 'buffering' \| null;/.test(store), true);
check('stageLabel 由 uiState 派生（不另存一份）',
  /const stageLabel: 'opening' \| 'buffering' \| null =\s*\r?\n\s*uiState\.kind === 'opening'/.test(store), true);
check('分级字段不进低频动作专线（否则缓冲一切换就广播全站）',
  /\| 'isBuffering' \| 'stageLabel' \| 'stageDetail'/.test(store), true);

check('exit 前：拖动终点判据只消费一次',
  /lastSeekPreviewAtRef\.current = 0/.test(store), true);

// 播放器只展示视频，不挂 poster 封面以免切集/停播时闪出封面。
check('播放器不挂 video.poster（只展示视频）', /video\.setAttribute\('poster'/.test(store), false);
check('主动清除残留 poster 属性', /video\.removeAttribute\('poster'\)/.test(store), true);

// 两段等待在日志里必须可区分 —— 这是"分级"能被复盘的前提。
check('阶段切换有打点（opening 耗时 / buffering 前方缓冲）',
  /阶段进入 buffering/.test(store) && /阶段 opening→/.test(store), true);

// ── 12. 缓冲态分级：opening 与 buffering 必须说两句不同的话 ────────────
const opening = describePlaybackStage({ kind: 'opening' });
const buffering = describePlaybackStage({ kind: 'buffering' });
check('opening 的文案说明"正在解析整集"', opening.label, '正在解析整集…');
check('buffering 的文案说明"缓冲中"', buffering.label, '缓冲中…');
check('两者不再共用同一句话', opening.label === buffering.label, false);
check('opening 无解析百分比时不编造数字', /%/.test(opening.detail), false);
check('opening 有百分比时如实报数', describePlaybackStage({ kind: 'opening', preparePercent: 43.6 }).detail.includes('44%'), true);
check('buffering 有前方余量时如实报数', describePlaybackStage({ kind: 'buffering', bufferedAheadSeconds: 6.8 }).detail, '前方已缓冲 6 秒，正在追播放头');
check('buffering 无余量时不编造', describePlaybackStage({ kind: 'buffering' }).detail, '正在拉取下一段数据');
check('buffering 余量接近 0 时不报"已缓冲 0 秒"', describePlaybackStage({ kind: 'buffering', bufferedAheadSeconds: 0.2 }).detail, '正在拉取下一段数据');

// 每个 kind 都必须有非空 label：宿主不该拿到空串往胶囊里塞。
const ALL_KINDS = ['idle', 'opening', 'buffering', 'playing', 'recovering', 'ended', 'error'];
const emptyLabels = ALL_KINDS.filter((kind) => describePlaybackStage({ kind }).label.length === 0);
check('所有阶段都有非空 label', emptyLabels, []);
check('错误阶段区分可恢复与否', {
  recoverable: describePlaybackStage({ kind: 'error', recoverable: true }).detail,
  fatal: describePlaybackStage({ kind: 'error', recoverable: false }).detail,
}, {
  recoverable: '可以重试，也可以换一集',
  fatal: '换一集试试，或在详情页切换来源',
});

console.log('');
console.log(pass + ' passed, ' + fail + ' failed');
if (fail > 0) process.exit(1);