/**
 * 原生窗口「首次回执」与「启动确认」的回归。
 *
 * 两条都对应本轮修的真实缺陷，而且都**驱动真实逻辑并断言副作用**（不是复述表达式）：
 *
 *   A. `runNativeHandoffPoll`：模拟一轮轮询，检查它到底调了 `resumeInline`、
 *      调了 `syncAfterNativeExit`、还是什么都没做；以及过期回执有没有偷偷更新 `alive`。
 *   B. `planNativeLaunchConfirmation`：启动确认必须等到 `phase` 出画，
 *      不能"第一次看到 alive 就成功"；连点不同集要让旧确认作废。
 *
 * 运行：`node scripts/verify-native-exit-plan.mjs`
 */
import {
  createSerializedPoll,
  planNativeHandoffExit,
  planNativeLaunchConfirmation,
  runNativeHandoffPoll,
  runNativeLaunchGate,
  shouldFallbackToInline,
} from '../src/services/nativeExitPlan.ts';

let failures = 0;
const check = (name, got, expected) => {
  if (got !== expected) {
    failures += 1;
    console.error(`FAIL ${name}: 期望 ${expected}，实际 ${got}`);
  }
};
const checkTrue = (name, condition) => {
  if (!condition) {
    failures += 1;
    console.error(`FAIL ${name}`);
  }
};

// ════════════════════════════════════════════════════════════════════════
// A0. 纯判定矩阵（退出成因 → 决策）
// ════════════════════════════════════════════════════════════════════════
const handoff = { episodeId: 'ep-1', at: 1000, sawAlive: false };
const base = { handoff, currentEpisodeId: 'ep-1', pollStartedAt: 1500 };

const cases = [
  ['还在跑', { ...base, status: { alive: true, exit: 'none' } }, 'alive'],
  ['用户主动关窗', { ...base, status: { alive: false, exit: 'closed' } }, 'user-closed'],
  ['异常消失', { ...base, status: { alive: false, exit: 'died' } }, 'fallback'],
  ['应用换窗', { ...base, status: { alive: false, exit: 'replaced' } }, 'replaced'],
  ['启动失败', { ...base, status: { alive: false, exit: 'failed' } }, 'launch-failed'],
  ['没有成因但已不活', { ...base, status: { alive: false, exit: 'none' } }, 'fallback'],
  ['过期回执(早于交接)', { ...base, pollStartedAt: 999, status: { alive: false, exit: 'died' } }, 'stale'],
  ['过期回执不能覆盖用户关窗', { ...base, pollStartedAt: 10, status: { alive: false, exit: 'closed' } }, 'stale'],
  ['换了集(防串集)', { ...base, currentEpisodeId: 'ep-2', status: { alive: false, exit: 'died' } }, 'episode-changed'],
  ['换了集且还在跑', { ...base, currentEpisodeId: 'ep-2', status: { alive: true, exit: 'none' } }, 'alive'],
  ['没有交接', { ...base, handoff: null, status: { alive: false, exit: 'died' } }, 'ignore'],
  ['状态拿不到', { ...base, status: null }, 'ignore'],
];
for (const [name, input, expected] of cases) {
  check(name, planNativeHandoffExit(input), expected);
}
const fallbackSet = ['fallback', 'launch-failed'];
for (const decision of ['ignore', 'alive', 'stale', 'episode-changed', 'user-closed', 'replaced', 'fallback', 'launch-failed']) {
  check(`shouldFallbackToInline(${decision})`, shouldFallbackToInline(decision), fallbackSet.includes(decision));
}
check(
  '同毫秒不算过期',
  planNativeHandoffExit({ ...base, pollStartedAt: 1000, status: { alive: false, exit: 'died' } }),
  'fallback',
);

// ════════════════════════════════════════════════════════════════════════
// A1. 副作用：一次轮询到底做了什么
// ════════════════════════════════════════════════════════════════════════
/** 造一个可观察的 deps + 交接凭据。 */
const makeRuntime = (options) => {
  const calls = { alive: [], resume: 0, sync: 0, logs: [] };
  const ref = { current: options.handoff ?? null };
  const deps = {
    readStatus: async () => options.status,
    now: () => options.now ?? 1500,
    currentEpisodeId: () => options.currentEpisodeId ?? 'ep-1',
    hasPendingAdvance: () => options.pendingAdvance === true,
    syncAfterNativeExit: () => { calls.sync += 1; },
    resumeInline: async () => {
      calls.resume += 1;
      if (options.playRejects) throw new DOMExceptionLike('NotAllowedError');
      return options.playStarted !== false;
    },
    log: (message) => calls.logs.push(message),
    setAlive: (alive) => calls.alive.push(alive),
  };
  return { calls, ref, deps };
};
/** Node 里没有 DOMException 的构造语义依赖，造一个带 name 的即可。 */
class DOMExceptionLike extends Error {
  constructor(name) {
    super(name);
    this.name = name;
  }
}

const pollCases = [
  {
    name: '过期回执：连 alive 都不更新',
    options: { status: { alive: false, exit: 'died' }, handoff: { episodeId: 'ep-1', at: 2000, sawAlive: false }, now: 1500 },
    expect: (calls, decision) => {
      check('过期回执 decision', decision, 'stale');
      check('过期回执不得更新 alive（否则双声门闸被放松）', calls.alive.length, 0);
      check('过期回执不得续播', calls.resume, 0);
      check('过期回执不得对齐重开', calls.sync, 0);
    },
  },
  {
    name: '还在跑：只更新 alive',
    options: { status: { alive: true, exit: 'none' }, handoff: { episodeId: 'ep-1', at: 1000, sawAlive: false } },
    expect: (calls, decision) => {
      check('alive decision', decision, 'alive');
      check('alive 更新次数', calls.alive.length, 1);
      check('alive 值', calls.alive[0], true);
      check('在跑时不续播', calls.resume, 0);
    },
  },
  {
    name: '用户主动关窗：保持原状',
    options: { status: { alive: false, exit: 'closed' }, handoff: { episodeId: 'ep-1', at: 1000, sawAlive: true } },
    expect: (calls, decision) => {
      check('user-closed decision', decision, 'user-closed');
      check('用户关窗不得续播', calls.resume, 0);
      check('用户关窗不得对齐重开', calls.sync, 0);
      checkTrue('用户关窗要落一条日志', calls.logs.length === 1 && calls.logs[0].includes('user-closed'));
    },
  },
  {
    name: '异常退出且没有联播推进：续播本页',
    options: { status: { alive: false, exit: 'died' }, handoff: { episodeId: 'ep-1', at: 1000, sawAlive: true } },
    expect: (calls, decision) => {
      check('fallback decision', decision, 'fallback');
      check('应调用一次续播', calls.resume, 1);
      check('不该走对齐重开', calls.sync, 0);
      checkTrue('应记"已回退"', calls.logs.some((l) => l.includes('已回退')));
    },
  },
  {
    name: '联播推进后异常退出：按集号对齐重开，不播旧画面',
    options: { status: { alive: false, exit: 'died' }, handoff: { episodeId: 'ep-1', at: 1000, sawAlive: true }, pendingAdvance: true },
    expect: (calls, decision) => {
      check('fallback decision(联播)', decision, 'fallback');
      check('联播推进过：不得直接续播旧画面', calls.resume, 0);
      check('联播推进过：必须调用 syncAfterNativeExit', calls.sync, 1);
      checkTrue('应记"按联播集号对齐"', calls.logs.some((l) => l.includes('按联播集号对齐')));
    },
  },
  {
    name: 'play() 被拒：日志必须是"拒绝"，不是"已回退"',
    options: { status: { alive: false, exit: 'died' }, handoff: { episodeId: 'ep-1', at: 1000, sawAlive: true }, playRejects: true },
    expect: (calls, decision) => {
      check('play 拒绝仍算 fallback', decision, 'fallback');
      const joined = calls.logs.join(' | ');
      checkTrue('拒绝时必须写明原因', joined.includes('续播被拒绝') && joined.includes('NotAllowedError'));
      checkTrue('拒绝时不得声称已回退', !joined.includes('已回退'));
    },
  },
  {
    name: '换集后旧凭据作废：不碰画面',
    options: { status: { alive: false, exit: 'died' }, handoff: { episodeId: 'ep-1', at: 1000, sawAlive: true }, currentEpisodeId: 'ep-2' },
    expect: (calls, decision, ref) => {
      check('episode-changed decision', decision, 'episode-changed');
      check('换集时不续播', calls.resume, 0);
      check('换集时不对齐', calls.sync, 0);
      checkTrue('凭据应被清空', ref.current === null);
    },
  },
];

for (const item of pollCases) {
  const { calls, ref, deps } = makeRuntime(item.options);
  const result = await runNativeHandoffPoll(deps, ref);
  item.expect(calls, result.decision, ref);
}

// ════════════════════════════════════════════════════════════════════════
// B. 启动确认：必须等到 phase 出画
// ════════════════════════════════════════════════════════════════════════
const launchCases = [
  {
    name: '刚 spawn（alive=true, phase=idle）→ 继续等，不能算成功',
    input: { status: { alive: true, exit: 'none', phase: 'idle', episodeId: 'ep-1' } },
    expected: 'continue',
  },
  {
    name: '加载 DLL/InitMpv（phase=loading）→ 继续等',
    input: { status: { alive: true, exit: 'none', phase: 'loading', episodeId: 'ep-1' } },
    expected: 'continue',
  },
  {
    name: '首次轮询看到 alive 就 return 的老行为已被禁止（idle 不算成功）',
    input: { status: { alive: true, exit: 'none', phase: 'idle', episodeId: 'ep-1' }, deadlineReached: true },
    expected: 'failed',
  },
  { name: 'phase=ready → 成功', input: { status: { alive: true, exit: 'none', phase: 'ready', episodeId: 'ep-1' } }, expected: 'ok' },
  { name: 'phase=buffering（有帧对）→ 成功', input: { status: { alive: true, exit: 'none', phase: 'buffering', episodeId: 'ep-1' } }, expected: 'ok' },
  { name: 'phase=failed → 失败', input: { status: { alive: true, exit: 'none', phase: 'failed', episodeId: 'ep-1' } }, expected: 'failed' },
  { name: '初始化失败退出（exit=died）→ 失败', input: { status: { alive: false, exit: 'died', phase: 'loading', episodeId: 'ep-1' } }, expected: 'failed' },
  { name: '管道失败（exit=failed）→ 失败', input: { status: { alive: false, exit: 'failed', phase: 'idle', episodeId: 'ep-1' } }, expected: 'failed' },
  { name: '用户随即关窗（exit=closed）→ 不兜底', input: { status: { alive: false, exit: 'closed', phase: 'ready', episodeId: 'ep-1' } }, expected: 'ok' },
  {
    name: '连点不同集：窗口报别的集 → 本次确认作废',
    input: { status: { alive: true, exit: 'none', phase: 'ready', episodeId: 'ep-2' } },
    expected: 'abandoned',
  },
  {
    name: '被新请求顶掉（seq 落后）→ 作废',
    input: { status: { alive: true, exit: 'none', phase: 'ready', episodeId: 'ep-1' }, seqBehind: true },
    expected: 'abandoned',
  },
  { name: '状态读不到 → 失败', input: { status: null }, expected: 'failed' },
];

for (const item of launchCases) {
  const plan = planNativeLaunchConfirmation({
    mySeq: item.input.seqBehind ? 1 : 2,
    currentSeq: 2,
    status: item.input.status,
    expectedEpisodeId: 'ep-1',
    deadlineReached: item.input.deadlineReached === true,
  });
  check(item.name, plan.step, item.expected);
}

// 成因文案：初始化失败要能说清是 died 而不是被当成超时。
const diedPlan = planNativeLaunchConfirmation({
  mySeq: 2,
  currentSeq: 2,
  status: { alive: false, exit: 'died', phase: 'loading', episodeId: 'ep-1' },
  expectedEpisodeId: 'ep-1',
  deadlineReached: false,
});
check('初始化失败成因文案', diedPlan.exit, 'died');
check('初始化失败阶段文案', diedPlan.phase, 'loading');
// C. 受控集成：详情页闸门（open → 等出画 → 只在确定没窗口时回退）
// ════════════════════════════════════════════════════════════════════════
/**
 * 造一组 deps。`statuses` 按序消费，最后一个会重复使用（模拟"状态一直不变"）。
 * `open` 由各用例自己给，返回值语义与真实 `tryLaunchNativePlayer` 一致
 * （`false` = 这一集交不了 MPV，压根没发起）。
 */
const makeGate = (statuses) => {
  const state = { now: 0, logs: [], reads: 0, opened: 0, index: 0 };
  const deps = {
    readStatus: async () => {
      state.reads += 1;
      const status = statuses[Math.min(state.index, statuses.length - 1)];
      state.index += 1;
      return status;
    },
    now: () => state.now,
    sleep: async (ms) => {
      state.now += ms;
    },
    log: (message) => state.logs.push(message),
  };
  return { state, deps };
};
const loading = { alive: true, exit: 'none', phase: 'loading', episodeId: 'ep-1' };
const ready = { alive: true, exit: 'none', phase: 'ready', episodeId: 'ep-1' };

// C1. loading → ready（预算内）→ 成功
{
  const { state, deps } = makeGate([loading, loading, ready]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => { state.opened += 1; }, budgetMs: 5000, intervalMs: 250 },
    deps,
  );
  check('C1 成功', result.ok, true);
  check('C1 outcome', result.outcome, 'ready');
  check('C1 确实发起过', result.started, true);
}

// C2. 超时但窗口仍在跑 → 只提示，不回退（旧行为会双播）
{
  const { state, deps } = makeGate([loading]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => { state.opened += 1; }, budgetMs: 500, intervalMs: 250 },
    deps,
  );
  check('C2 不算成功', result.ok, false);
  check('C2 仍活着 → outcome=running', result.outcome, 'running');
  check('C2 不得回退（会双播）', result.canFallback, false);
  checkTrue('C2 日志说明仍在运行', state.logs.some((l) => l.includes('仍在运行')));
}

// C3. open 抛错 + 请求已被顶掉 → 不兜底、不导航
{
  const { deps } = makeGate([{ alive: false, exit: 'none', phase: 'idle', episodeId: '' }]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => false, open: async () => { throw new Error('boom'); } },
    deps,
  );
  check('C3 旧请求作废', result.outcome, 'abandoned');
  check('C3 不得回退', result.canFallback, false);
}

// C4. open 抛错 + 明确没有窗口在跑 → 允许回退并提示
{
  const { deps } = makeGate([{ alive: false, exit: 'none', phase: 'idle', episodeId: '' }]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => { throw new Error('boom'); } },
    deps,
  );
  check('C4 outcome=open-error', result.outcome, 'open-error');
  check('C4 可以回退', result.canFallback, true);
}

// C5. 窗口已开到别的集 → abandoned，不动它
{
  const { deps } = makeGate([{ alive: true, exit: 'none', phase: 'ready', episodeId: 'ep-2' }]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => true },
    deps,
  );
  check('C5 outcome=abandoned', result.outcome, 'abandoned');
  check('C5 不得回退播 A', result.canFallback, false);
}

// C6. open 返回 false（这一集交不了 MPV）+ 明确没有窗口 → 立即走网页分流
{
  const { state, deps } = makeGate([{ alive: false, exit: 'none', phase: 'idle', episodeId: '' }]);
  const started = Date.now();
  const result = await runNativeLaunchGate(
    { episodeId: 'anime-42', isCurrentRequest: () => true, open: async () => false, budgetMs: 5000, intervalMs: 250 },
    deps,
  );
  check('C6 outcome=not-started', result.outcome, 'not-started');
  check('C6 未发起', result.started, false);
  check('C6 可以回退', result.canFallback, true);
  check('C6 只读一次状态', state.reads, 1);
  checkTrue('C6 必须立即返回（不睡满 5 秒）', Date.now() - started < 200);
}

// C7. open=false + 历史里只有 closed 快照 → 不能当成"本次成功"
{
  const { deps } = makeGate([{ alive: false, exit: 'closed', phase: 'ready', episodeId: 'ep-9' }]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => false },
    deps,
  );
  check('C7 不得判成功', result.ok, false);
  check('C7 走网页分流', result.canFallback, true);
  check('C7 outcome', result.outcome, 'not-started');
}

// C8. open=false + 另一个会话仍在跑 → 不重复播放
{
  const { deps } = makeGate([{ alive: true, exit: 'none', phase: 'ready', episodeId: 'ep-9' }]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => false },
    deps,
  );
  check('C8 不回退', result.canFallback, false);
  check('C8 outcome', result.outcome, 'not-started');
}

// C9. open=false + 状态未知（null）→ 不回退
{
  const { deps } = makeGate([null]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => false },
    deps,
  );
  check('C9 outcome=unknown', result.outcome, 'unknown');
  check('C9 判断不出就不能回退', result.canFallback, false);
}

// C10. 轮询里状态读不到（null）→ 超时后按 unknown 处理，不回退
{
  const { deps } = makeGate([null]);
  const result = await runNativeLaunchGate(
    { episodeId: 'ep-1', isCurrentRequest: () => true, open: async () => true, budgetMs: 250, intervalMs: 250 },
    deps,
  );
  check('C10 不算成功', result.ok, false);
  check('C10 outcome=unknown', result.outcome, 'unknown');
  check('C10 不回退', result.canFallback, false);
}

// ════════════════════════════════════════════════════════════════════════
// C.5 真实适配器语义（用 esbuild 打包的 src/services/preferredPlayback.ts）
// ════════════════════════════════════════════════════════════════════════
// 这里**不**用理想 deps：直接把真实的 `tryLaunchNativePlayer` 塞进闸门，
// 确认"合法返回 false"的两种形态都被当成 not-started，而不是本次成功。
{
  globalThis.window = globalThis.window ?? {};
  globalThis.localStorage = {
    getItem: () => null,
    setItem: () => undefined,
  };
  // 现打现用：不依赖仓库里是否留着上次的产物（`.verify-p0/` 是 gitignore 的）。
  const { build } = await import('esbuild');
  const { fileURLToPath } = await import('node:url');
  const path = await import('node:path');
  const root = fileURLToPath(new URL('..', import.meta.url));
  const outfile = path.join(root, '.verify-p0', 'tlaunch.mjs');
  await build({
    entryPoints: [path.join(root, 'src', 'services', 'preferredPlayback.ts')],
    bundle: true,
    format: 'esm',
    platform: 'node',
    outfile,
    logLevel: 'silent',
  });
  const real = await import(`file://${outfile.replace(/\\/g, '/')}`);
  const notStartedCases = [
    ['用户选的是网页播放器', { preferredPlayer: 'webview', seriesId: 's', episodeId: 'ep-1' }],
    ['这一集没有可交给 MPV 的地址/vid', { preferredPlayer: 'mpv', seriesId: 's', episodeId: 'anime-42' }],
  ];
  for (const [name, input] of notStartedCases) {
    const value = await real.tryLaunchNativePlayer(input);
    check(`真实适配器：${name} → false`, value, false);
    const { deps } = makeGate([{ alive: false, exit: 'closed', phase: 'ready', episodeId: 'ep-9' }]);
    const result = await runNativeLaunchGate(
      { episodeId: input.episodeId, isCurrentRequest: () => true, open: () => real.tryLaunchNativePlayer(input) },
      deps,
    );
    check(`真实适配器：${name} → not-started`, result.outcome, 'not-started');
    check(`真实适配器：${name} → 不判成功`, result.ok, false);
    check(`真实适配器：${name} → 走网页分流`, result.canFallback, true);
  }
}

// ════════════════════════════════════════════════════════════════════════
// D. 串行轮询 + 卸载后迟到副作用
// ════════════════════════════════════════════════════════════════════════
{
  let runs = 0;
  let release;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  const poller = createSerializedPoll(async () => {
    runs += 1;
    await gate;
  });
  const first = poller.tick();
  const second = poller.tick();
  const third = poller.tick();
  release();
  await Promise.all([first, second, third]);
  check('D1 重叠 tick 只执行一次', runs, 1);
}
{
  const calls = { alive: [], resume: 0, sync: 0, logs: [] };
  const ref = { current: { episodeId: 'ep-1', at: 0, sawAlive: true } };
  let releaseRead;
  const gate = new Promise((resolve) => {
    releaseRead = resolve;
  });
  const deps = {
    readStatus: async () => {
      await gate;
      return { alive: false, exit: 'died', phase: 'ready', episodeId: 'ep-1' };
    },
    now: () => 10,
    currentEpisodeId: () => 'ep-1',
    hasPendingAdvance: () => false,
    syncAfterNativeExit: () => { calls.sync += 1; },
    resumeInline: async () => { calls.resume += 1; return true; },
    log: (m) => calls.logs.push(m),
    setAlive: (a) => calls.alive.push(a),
    isValid: () => false,
  };
  releaseRead();
  const result = await runNativeHandoffPoll(deps, ref);
  check('D2 卸载后判定', result.decision, 'ignore');
  check('D2 不得更新 alive', calls.alive.length, 0);
  check('D2 不得续播', calls.resume, 0);
  check('D2 不得对齐重开', calls.sync, 0);
}
{
  const calls = { alive: [] };
  const ref = { current: null };
  const deps = {
    readStatus: async () => null,
    now: () => 10,
    currentEpisodeId: () => null,
    hasPendingAdvance: () => false,
    syncAfterNativeExit: () => {},
    resumeInline: async () => true,
    log: () => {},
    setAlive: (a) => calls.alive.push(a),
  };
  const result = await runNativeHandoffPoll(deps, ref);
  check('D3 null 状态', result.decision, 'ignore');
  check('D3 null 不得写 alive', calls.alive.length, 0);
}

if (failures > 0) {
  console.error(`verify-native-exit-plan: ${failures} 条失败`);
  process.exit(1);
}
console.log('verify-native-exit-plan: ok');
