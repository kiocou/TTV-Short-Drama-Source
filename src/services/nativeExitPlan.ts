/**
 * 「原生窗口已经不在」时该做什么——纯判定，不碰 DOM、不碰 IPC。
 *
 * ## 为什么把这段判定抽出来
 *
 * 它此前写在 `PlayerControls` 的轮询回调里，三种失败模式全都踩过：
 *   * 用"交接后 N 秒内"当判据 → 用户 5 秒内点 X 被当成异常（无辜重新开播）；
 *     而异常发生在 15 秒后又永远不回退；
 *   * 轮询是在交接**之前**发出的那一拍，回执回来时 `alive:false` 描述的是
 *     上一个会话 → 刚点开就被判死；
 *   * 不校验"交接的是哪一集" → 用户切了集还按旧凭据把上一集播回来（串集）。
 *
 * 抽成纯函数后这些边界可以逐条钉住（`scripts/verify-native-exit-plan.mjs`），
 * 而不是靠"记得别写错"。
 */
import type { NativePlayerStatus } from './ipc';

/** 一次交接的凭据：`handleOpenNative` 成功时写入。 */
export interface NativeHandoff {
  /** 交接时正在播的那一集。 */
  episodeId: string;
  /** 交接时刻（毫秒，`Date.now()`）。 */
  at: number;
  /** 是否见过它真的活过一次。 */
  sawAlive: boolean;
}

export type NativeExitDecision =
  /** 还在跑：继续观察。 */
  | 'alive'
  /** 不是本次交接的会话（或没有交接）：什么都不做。 */
  | 'ignore'
  /** 这一拍在交接之前就发出去了，回执过期：丢弃，等下一拍。 */
  | 'stale'
  /** 交接的那一集已经不是当前集：凭据作废，绝不按旧凭据续播。 */
  | 'episode-changed'
  /** 用户自己关的窗：保持原状。 */
  | 'user-closed'
  /** 应用自己换的窗（`openEpisode`/重开）：保持原状。 */
  | 'replaced'
  /** 异常消失：回退到本页播放。 */
  | 'fallback'
  /** 从未连上（管道失败）：回退到本页播放。 */
  | 'launch-failed';

/**
 * 判定一次轮询的结局。
 *
 * @param pollStartedAt 这一拍**发出**的时刻。必须由调用方在 `await` 之前取，
 *   否则"交接后才发现回执是旧的"这类误判无法避免。
 */
export function planNativeHandoffExit(input: {
  pollStartedAt: number;
  handoff: NativeHandoff | null;
  currentEpisodeId: string | null;
  status: Pick<NativePlayerStatus, 'alive' | 'exit'> | null;
}): NativeExitDecision {
  const { pollStartedAt, handoff, currentEpisodeId, status } = input;
  if (!handoff) return 'ignore';
  if (!status) return 'ignore';
  if (status.alive === true) return 'alive';
  // 过期回执排在"成因"之前：旧会话的成因（比如 died）与本次交接无关。
  if (pollStartedAt < handoff.at) return 'stale';
  if (currentEpisodeId !== handoff.episodeId) return 'episode-changed';
  if (status.exit === 'closed') return 'user-closed';
  if (status.exit === 'replaced') return 'replaced';
  if (status.exit === 'failed') return 'launch-failed';
  /**
   * 走到这里有两种情况，都按异常处理：
   *   * `exit === 'died'` —— 进程没了（被杀/崩溃）；
   *   * `exit === 'none'` —— 还没有任何成因记录，但 `alive` 已经是 false。
   *     后者只会出现在"事件流还没写出成因、状态已经被判死"的极短窗口里；
   *     宁可回退（用户有画面可看）也不要干等一个永远不会来的成因。
   */
  return 'fallback';
}

/** `planNativeHandoffExit` 的结果里，哪些应当触发"把本页画面播回去"。 */
export function shouldFallbackToInline(decision: NativeExitDecision): boolean {
  return decision === 'fallback' || decision === 'launch-failed';
}

/** 轮询一次要用到的副作用（全部注入，测试可以直接断言"到底做了什么"）。 */
export interface NativeHandoffDeps {
  /** 读一次原生状态；失败返回 null。 */
  readStatus: () => Promise<NativePlayerStatus | null>;
  /** 当前时刻（毫秒）。 */
  now: () => number;
  /** 本页此刻正在播的集。 */
  currentEpisodeId: () => string | null;
  /** 原生窗口是否靠联播推进过（推进过就必须按集号对齐，不能直接续播旧画面）。 */
  hasPendingAdvance: () => boolean;
  /** 把内嵌对齐到"原生最后看到的那一集"并起播（= `navigateHost.syncAfterNativeExit`）。 */
  syncAfterNativeExit: () => void;
  /** 本页还有可续播的源时把它播回去；返回是否真的播起来了。 */
  resumeInline: () => Promise<boolean>;
  log: (message: string) => void;
  /** 更新"原生在播"状态。**过期回执不许调**（见下）。 */
  setAlive: (alive: boolean) => void;
  /**
   * 这次执行还有效吗（组件是否仍然挂载、请求是否仍是当前请求）。
   *
   * 判定点在**读状态之后、真正产生副作用之前**各查一次：轮询是异步的，
   * 页面可能在 `await` 期间被卸载，那时再 `querySelector('video').play()`
   * 就会去播别的页面的那块元素。
   */
  isValid?: () => boolean;
}

export interface NativePollResult {
  decision: NativeExitDecision;
  status: NativePlayerStatus | null;
}

/**
 * 执行一次轮询并落实副作用。
 *
 * 三条顺序是硬要求（每一条都对应一次真实事故）：
 *   1. **先判过期，再更新 `alive`**。反过来会让一拍"交接前发出、交接后回来"的旧回执
 *      把 `nativeAlive` 打成 false，守门员（原生在播 → 内嵌必须安静）随之失效，
 *      两路声音同时响。
 *   2. **过期回执不产生任何副作用**，包括不写日志、不动画面。
 *   3. 异常消失时走**同一个恢复入口**：联播推进过就 `syncAfterNativeExit()`
 *      按集号对齐重开，而不是把旧一集的画面 play 回来（那会串集）；
 *      没有推进才续播当前源，且**只有真的播起来才写"已回退"**。
 */
export async function runNativeHandoffPoll(
  deps: NativeHandoffDeps,
  handoffRef: { current: NativeHandoff | null },
): Promise<NativePollResult> {
  const pollStartedAt = deps.now();
  const status = await deps.readStatus();
  const valid = () => (deps.isValid ? deps.isValid() : true);

  // 卸载/换请求之后一律不产生任何副作用（连日志都不写）。
  if (!valid()) return { decision: 'ignore', status };

  const handoff = handoffRef.current;

  // ① 过期回执：连 `alive` 都不更新。
  if (handoff && pollStartedAt < handoff.at) {
    return { decision: 'stale', status };
  }
  // ② 查询失败（null）**不等于**"窗口不存在"：不动 `alive`，
  //    否则一次 IPC 抖动就会把双声门闸放松成两路声音。
  if (!status) return { decision: 'ignore', status: null };
  deps.setAlive(status.alive === true);

  if (!handoff) return { decision: 'ignore', status };

  const decision = planNativeHandoffExit({
    pollStartedAt,
    handoff,
    currentEpisodeId: deps.currentEpisodeId(),
    status: status ? { alive: status.alive, exit: status.exit } : null,
  });

  if (decision === 'alive') {
    handoff.sawAlive = true;
    return { decision, status };
  }
  if (decision === 'ignore' || decision === 'stale') return { decision, status };
  if (decision === 'episode-changed') {
    handoffRef.current = null;
    return { decision, status };
  }

  handoffRef.current = null;

  if (!shouldFallbackToInline(decision)) {
    // 用户主动关窗 / 应用换窗：保持原状，只留一条可追溯的日志。
    deps.log(`原生播放器已退出（判定=${decision}），本页保持原状`);
    return { decision, status };
  }

  // 真正动画面之前的最后一道闸：`readStatus` 与这里之间可能已经过了几毫秒。
  if (!valid()) return { decision, status };

  if (deps.hasPendingAdvance()) {
    // 联播推进过：本页的当前集已经落后，绝不能 play 旧画面。
    deps.syncAfterNativeExit();
    deps.log(`原生播放器异常退出（判定=${decision}），已按联播集号对齐重开`);
    return { decision, status };
  }

  if (!valid()) return { decision, status };
  try {
    const started = await deps.resumeInline();
    deps.log(
      started
        ? `原生播放器异常退出（判定=${decision}），已回退到本页播放`
        : `原生播放器异常退出（判定=${decision}），本页没有可续播的源`,
    );
  } catch (error) {
    deps.log(
      `原生播放器异常退出（判定=${decision}），本页续播被拒绝：${
        error instanceof Error ? error.name : String(error)
      }`,
    );
  }
  return { decision, status };
}
// ══════════════════════════════════════════════════════════════════════════
//  「刚打开的原生窗口到底可播了吗」——纯判定
// ══════════════════════════════════════════════════════════════════════════

/** 一次确认判定的结论。 */
export type NativeLaunchStep =
  /** 继续等下一拍。 */
  | 'continue'
  /** 已经可播（或有明确成因说明不需要兜底）。 */
  | 'ok'
  /** 没能可播，调用方应回退。 */
  | 'failed'
  /** 这次确认已与当前请求无关（被新请求顶掉 / 播的是别的集）。 */
  | 'abandoned';

export interface NativeLaunchPlan {
  step: NativeLaunchStep;
  /** 给日志用的成因文案。 */
  exit: string;
  phase: string;
}

/**
 * 判定"这一次确认能不能算成功"。
 *
 * ## 为什么不能用 `alive` 当判据（2026-11-10 第二轮）
 *
 * `open()` 在 spawn 之后立刻把 `alive` 建为 true，而 `alive_now()` 只回答
 * "进程还在吗" —— 播放器正在加载 `libmpv`、`InitMpv` 时同样是 true。
 * 只看它就等于"第一次轮询即成功"，100ms 活着、500ms 初始化失败的窗口会被完整漏掉。
 *
 * 真正的可播信号是播放器自己算的 `phase`（`GetPhase()`：`hasFramePair` +
 * `core-idle` + `paused-for-cache`）：`ready`/`buffering` 都意味着**已经有帧对**。
 * 所以这里等的是 phase，不是时间。
 */
export function planNativeLaunchConfirmation(input: {
  mySeq: number;
  currentSeq: number;
  status: Pick<NativePlayerStatus, 'alive' | 'exit' | 'phase' | 'episodeId'> | null;
  /** 本次确认对应的集（空串 = 不做身份校验）。 */
  expectedEpisodeId: string;
  /** 观察预算是否已用完。 */
  deadlineReached: boolean;
}): NativeLaunchPlan {
  const { mySeq, currentSeq, status, expectedEpisodeId, deadlineReached } = input;
  if (mySeq !== currentSeq) return { step: 'abandoned', exit: 'superseded', phase: 'unknown' };
  if (!status) return { step: 'failed', exit: 'unknown', phase: 'unknown' };

  const phase = status.phase || 'idle';
  const exit = status.exit || 'none';

  // 身份：窗口报的是别的集 → 这次确认与请求无关（连点不同集）。
  const reported = (status.episodeId || '').trim();
  const expected = expectedEpisodeId.trim();
  if (expected && reported && reported !== expected) {
    return { step: 'abandoned', exit: 'episode-mismatch', phase };
  }

  if (status.alive !== true) {
    // 明确关闭 / 应用换窗：不需要回退。
    if (exit === 'closed' || exit === 'replaced') return { step: 'ok', exit, phase };
    return { step: 'failed', exit: exit === 'none' ? 'died' : exit, phase };
  }
  if (phase === 'failed') return { step: 'failed', exit: 'playback-failed', phase };
  // 有帧对 = 出画了。buffering 只是缓存没跟上，画面是真的。
  if (phase === 'ready' || phase === 'buffering') return { step: 'ok', exit: 'none', phase };
  if (deadlineReached) return { step: 'failed', exit: 'timeout', phase };
  return { step: 'continue', exit: 'none', phase };
}

/**
 * 把轮询串起来：同一时刻只跑一拍，卸载后不再跑。
 *
 * 为什么需要：`setInterval` 只保证"按时发起"，不保证"上一拍已经回来"。IPC 慢一点
 * 就会叠出两三拍在飞，回执乱序时旧的一拍会把新的一拍结论覆盖掉（`alive` 忽真忽假，
 * 双声门闸跟着抖）。这里的做法是**丢弃而不是排队**：丢一拍没有损失（2 秒后还有下一拍），
 * 排队反而会积压出一串过期结论。
 *
 * `isValid` 一路透传到执行器：卸载之后既不再发起，也不再让在飞的那一拍产生副作用。
 */
export function createSerializedPoll<T>(work: (isValid: () => boolean) => Promise<T>) {
  let busy = false;
  let cancelled = false;
  return {
    get cancelled() {
      return cancelled;
    },
    async tick(): Promise<T | undefined> {
      if (busy || cancelled) return undefined;
      busy = true;
      const isValid = () => !cancelled;
      try {
        return await work(isValid);
      } finally {
        busy = false;
      }
    },
    cancel() {
      cancelled = true;
    },
  };
}

/** 启动闸门要用到的外部能力（全部注入，测试可直接驱动）。 */
export interface NativeLaunchGateDeps {
  /** 读一次原生状态；失败返回 null（= 不知道）。 */
  readStatus: () => Promise<NativePlayerStatus | null>;
  now: () => number;
  sleep: (ms: number) => Promise<void>;
  log: (message: string) => void;
}

export interface NativeLaunchGateResult {
  ok: boolean;
  /**
   * ready / closed / replaced / not-started / open-error / timeout / failed /
   * running（窗口活着但没出画）/ unknown（状态未知）/ abandoned（请求已作废）。
   */
  outcome: string;
  exit: string;
  phase: string;
  /**
   * 调用方可否安全地走网页播放。
   *
   * **只有"确定没有原生窗口在跑"才为 true**。`null` 状态与"窗口还在跑"一律 false ——
   * 这是"超时双播"的唯一防线：宁可只提示，也不能同时开两路。
   */
  canFallback: boolean;
  /** 本次是否真的发起了 `nativeOpen`（`false` = 适配器判定这一集交不了 MPV）。 */
  started: boolean;
}

/**
 * 详情页的新入口：打开 → 等出画 → 只在**确定**没有窗口时回退。
 *
 * ## 三条硬规则（对应三次真实缺陷）
 *
 * 1. **`open()` 返回 `false` 不等于"这次成功了"**：`tryLaunchNativePlayer` 在
 *    "用户选的是网页播放器"或"这一集没有可交给 MPV 的地址/vid"时合法地返回 false。
 *    这不是失败，而是**根本没有开始**。旧闸门忽略了返回值，于是拿上一次遗留的
 *    `exit=closed` 判定"成功"、或拿别的集判定 `abandoned`，两种都让用户点播放
 *    之后什么也没发生（连本该走的网页分流都被吞掉）。
 * 2. **不做自动关闭**。`ipc.playback.nativeClose()` 内部吞掉所有错误、永远 resolve，
 *    把它当"窗口已经死了"的证据是不成立的；而无参的全局关闭也无法保证不误关
 *    用户后来开的新窗口。所以这里只在有**明确 `alive === false`** 时才允许回退。
 * 3. **`null` 状态 = 不知道**，既不是"在跑"也不是"没跑"：不更新任何门闸、不回退。
 */
export async function runNativeLaunchGate(
  input: {
    episodeId: string;
    isCurrentRequest: () => boolean;
    /** 真正发起 `nativeOpen`；返回 `false` 表示"这一集不会交给 MPV"。 */
    open: () => Promise<unknown>;
    budgetMs?: number;
    intervalMs?: number;
  },
  deps: NativeLaunchGateDeps,
): Promise<NativeLaunchGateResult> {
  const budgetMs = Math.max(0, input.budgetMs ?? 5000);
  const intervalMs = Math.max(10, input.intervalMs ?? 250);
  const deadline = deps.now() + budgetMs;

  const settle = async (kind: string, started: boolean): Promise<NativeLaunchGateResult> => {
    const status = await deps.readStatus();
    if (!input.isCurrentRequest()) {
      return { ok: false, outcome: 'abandoned', exit: 'superseded', phase: 'unknown', canFallback: false, started };
    }
    if (!status) {
      return { ok: false, outcome: 'unknown', exit: kind, phase: 'unknown', canFallback: false, started };
    }
    // 只有明确 alive=false 才算"没有窗口在跑"。
    const idle = status.alive === false;
    return {
      ok: false,
      outcome: idle ? kind : 'not-started',
      exit: status.exit || kind,
      phase: status.phase || 'unknown',
      canFallback: idle,
      started,
    };
  };

  let started = true;
  try {
    const opened = await input.open();
    if (opened === false) started = false;
  } catch (error) {
    // 抛错也可能发生在"复用已有进程、queue/load 发送失败"之后，不能假定没启动。
    if (!input.isCurrentRequest()) {
      return { ok: false, outcome: 'abandoned', exit: 'superseded', phase: 'unknown', canFallback: false, started: true };
    }
    deps.log(
      `原生播放器打开失败（${error instanceof Error ? error.message : String(error)}）`,
    );
    return settle('open-error', true);
  }

  if (!started) {
    // 本次根本没发起：判断"有没有别的原生窗口在跑"，决定能不能走网页分流。
    return settle('not-started', false);
  }

  let lastExit = 'none';
  let lastPhase = 'idle';
  let lastStatus: NativePlayerStatus | null = null;

  for (;;) {
    if (!input.isCurrentRequest()) {
      return { ok: false, outcome: 'abandoned', exit: 'superseded', phase: lastPhase, canFallback: false, started: true };
    }
    lastStatus = await deps.readStatus();
    const plan = planNativeLaunchConfirmation({
      mySeq: 0,
      currentSeq: 0,
      status: lastStatus,
      expectedEpisodeId: input.episodeId,
      deadlineReached: deps.now() >= deadline,
    });
    lastExit = plan.exit;
    lastPhase = plan.phase;

    if (plan.step === 'ok') {
      return {
        ok: true,
        outcome: plan.exit === 'none' ? 'ready' : plan.exit,
        exit: plan.exit,
        phase: plan.phase,
        canFallback: true,
        started: true,
      };
    }
    if (plan.step === 'abandoned') {
      return { ok: false, outcome: 'abandoned', exit: plan.exit, phase: plan.phase, canFallback: false, started: true };
    }
    if (plan.step === 'failed') break;
    await deps.sleep(intervalMs);
  }

  if (!lastStatus) {
    // 全程读不到状态：不知道窗口在不在 —— 不回退。
    deps.log('原生播放器状态不可用，本次不自动改用网页播放');
    return { ok: false, outcome: 'unknown', exit: lastExit, phase: lastPhase, canFallback: false, started: true };
  }
  if (lastStatus.alive === true) {
    deps.log(
      `原生播放器未能在 ${budgetMs}ms 内出画（${lastExit}/${lastPhase}），仍在运行；本次不自动改用网页播放`,
    );
    return { ok: false, outcome: 'running', exit: lastExit, phase: lastPhase, canFallback: false, started: true };
  }
  deps.log(`原生播放器会话已结束（${lastExit}），回退应用内播放`);
  return {
    ok: false,
    outcome: lastExit === 'timeout' ? 'timeout' : 'failed',
    exit: lastExit,
    phase: lastPhase,
    canFallback: true,
    started: true,
  };
}
