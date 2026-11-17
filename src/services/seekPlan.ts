/**
 * 「用户点了进度条的某个位置」之后，到底该怎么落位。
 *
 * ## 用户报的现象（2026-10-24）
 *
 * 「点击播放器任意位置（比如点击结尾），它会跳转到转码后的最后位置，
 *   而不是跳到点击的对应位置。」
 *
 * ## 根因：进度条的时间基 与 元素的可寻址范围 是两套时间基
 *
 * 进度条显示的 `duration` 是**源片真实时长**（后端下发的权威值，比如 93 秒）。
 * 而播放源是「边转边播」的整集 HLS：清单里此刻只有已经切好的分片，于是
 * `video.seekable.end()` 只到已转位置（比如 20 秒）。
 *
 * MSE 规范要求 seek 目标落在**可寻址范围**内，超出时浏览器把 `currentTime`
 * 夹到范围末端。所以"点结尾"在浏览器看来等于"点 20 秒"。
 *
 * 旧实现为了对付这件事，在超出范围时做了两件都不对的事：
 *
 *   1. 把播放头推到 `seekable.end - 0.05` —— 这就是用户看到的"跳到转码后的
 *      最后位置"。**它把一个"还没转好"的等待，表现成了一个"跳错位置"的错误。**
 *   2. 8 秒后超时，又把播放头推到当时的 `end` —— 于是同一个错误再犯一次，
 *      而且这次是永久的（用户以为已经落位了）。
 *
 * ## 正确语义
 *
 * 用户点哪儿就该落在哪儿。转码没跟上不是用户的错，也不该用"跳到能播的地方"
 * 来假装成功。所以：
 *
 *   - **可寻址范围内**：直接落位（这是绝大多数情况，也是唯一"立即成功"的路径）。
 *   - **超出范围**：进入等待，**播放头留在原处不动**（画面继续播，或暂停在
 *     用户按下的那一刻），等转码推进到目标再落位。期间界面显示目标位置，
 *     让用户知道"你要的位置我记着"。
 *   - **等到超时**：如实告诉用户"这一段还没转好"，并**停在原处**——绝不跳到
 *     末端冒充成功。用户可以再点一次（转码是 13~29 倍实时，通常早就不止了）。
 *
 * ## 为什么抽成纯函数
 *
 * 三个播放面（主播放器、动漫播放器、画中画小窗）都要做同一件事，而它们此前
 * 各写各的：主播放器有等待逻辑、小窗与动漫直接赋值（超出范围就静默夹取）。
 * 抽出来才能被 `verify:seek` 钉住，也才能让三处真正同构。
 */

/**
 * 目标位置落在可寻址范围内时，允许的容差（秒）。
 *
 * `seekable.end` 与用户落点之间本就有几十毫秒误差（分片边界、MSE 时间基），
 * 卡死在等号上会让每次正常拖动都触发一次等待。
 */
export const SEEK_SEEKABLE_TOLERANCE_SECONDS = 0.5;

/**
 * 落位时与转码边界之间必须保留的余量（秒）。
 *
 * ## 为什么需要它（2026-10-27 修「点了进度条，画面反复重播三四次」）
 *
 * 转码没结束时，`seekable.end` 就是"ffmpeg 刚写完的那一片的末尾"。如果落位点
 * 贴着这个边界（旧实现允许 `target <= seekableEnd + 0.5`），会出现：
 *
 *   1. `video.currentTime = target` 成功，播放器开始拉目标分片；
 *   2. 但那一片**此刻正在被写**，本地服务只能返 503（见 `media_enhance` 里
 *      "分片还没落盘"那段），hls.js 于是反复重试；
 *   3. 重试期间 `waiting` / `canplay` / `playing` 反复触发，用户看到的就是
 *      "画面反复播了三四次"，最后停在转码推进后的某个位置。
 *
 * 实测日志（ttv-playback.log，2026-10-27）：
 *
 *   转码已推进到 124.9s，落位到 111.4s
 *   媒体 waiting → canplay → playing   ← 循环 3 次
 *   媒体 waiting → canplay → playing
 *   媒体 waiting → canplay → playing
 *   timeupdate 心跳 位置=121.52s        ← 最终停在 121.52 而不是 111.4
 *
 * 留出一片（源分片实测 1.6~3.2 秒）的余量，落位后目标后面至少还有已写好的
 * 内容，hls.js 不需要为"边界那一片"重试。
 *
 * 取值 4 秒：覆盖最长的分片（3.2 秒）再留一点。代价是"点最后 4 秒会等转码"，
 * 而转码现在 5~6 秒就完成全集，这个等待比"画面反复重播"划算得多。
 */
export const SEEK_SEGMENT_SAFETY_SECONDS = 4;

/**
 * 转码是否已完成。
 *
 * 判据是 `seekable.end` 已经追上总时长 —— 此时清单带 `#EXT-X-ENDLIST`，
 * 可寻址范围就是全片，**不能**再扣安全余量，否则点片尾最后几秒会永远等不到。
 */
function transcodeFinished(seekableEnd: number, duration: number): boolean {
  // ⚠️ 总时长未知时**不扣余量**：调用方没给 duration，就无从判断"转码是否还在跑"，
  // 此时保持旧的宽松语义（目标落在可寻址范围内即落位）。否则所有不传 duration 的
  // 调用方都会凭空多等 4 秒，那是行为回退而不是修复。
  if (!(duration > 0)) return true;
  return seekableEnd >= duration - SEEK_SEGMENT_SAFETY_SECONDS;
}

export interface SeekTargetInput {
  /** 用户点/拖到的位置（秒）。 */
  requested: number;
  /** 展示用的总时长（权威值，秒）。`0` / 非有限值表示未知。 */
  duration: number;
  /** 元素当前的可寻址末端（秒）。`0` 表示读不到。 */
  seekableEnd: number;
  /** HLS/MSE 在首个分片落入 SourceBuffer 前暂时没有可寻址范围。 */
  isHls?: boolean;
}

export type SeekPlan =
  | {
      /**
       * 立即落位。
       *
       * `target` 已经过 `[0, duration]` 夹取 —— 用户点在进度条最右边之外
       * （pointer 滑出轨道）时不该得到超过时长的位置。
       */
      kind: 'immediate';
      target: number;
    }
  | {
      /**
       * 目标超出可寻址范围：等转码推进。
       *
       * `target` 是用户真正想要的位置，**不是** `seekableEnd`。调用方必须
       * 原样记住它、在转码追上时落位，绝不能改成"先跳到末端"。
       */
      kind: 'wait';
      target: number;
      /** 此刻的可寻址末端，只用于打点与展示进度，不参与落位。 */
      seekableEnd: number;
    };

/**
 * 算出这次点击该怎么落位。
 *
 * 判据只有两条，顺序不能换：
 *   1. 先把目标夹进 `[0, duration]`（用户可能点到轨道外）；
 *   2. 再比较它与可寻址末端 —— 在范围内就立即落位，否则进入等待。
 *
 * `seekableEnd <= 0`（读不到）时按"立即落位"处理：那种情况多见于普通文件源
 * （`seekable` 尚未就绪但赋值不会失败），交给浏览器自己夹取比凭空进入等待好。
 */
export function planSeek(input: SeekTargetInput): SeekPlan {
  const raw = Number.isFinite(input.requested) && input.requested > 0 ? input.requested : 0;
  const duration = Number.isFinite(input.duration) && input.duration > 0 ? input.duration : 0;
  const target = duration > 0 ? Math.min(raw, duration) : raw;

  const seekableEnd =
    Number.isFinite(input.seekableEnd) && input.seekableEnd > 0 ? input.seekableEnd : 0;

  // HLS 空范围代表分片尚未进入 MSE，直接写 currentTime 会被浏览器静默夹回原位。
  if (seekableEnd <= 0 && input.isHls) {
    return { kind: 'wait', target, seekableEnd };
  }
  // 普通文件读不到可寻址范围时交给浏览器原生 Range seek。
  if (seekableEnd <= 0) {
    return { kind: 'immediate', target };
  }
  // ⚠️ 2026-10-27 回退「扣一片安全余量」的做法。
  //
  // 那个余量当初是为了避免落位踩在"正在写的分片"上。但它把**可落位边界整体
  // 往前挪了 4 秒**，用户点在自己明明看得见已经转好的位置时，会被判成"未转码"
  // 而进入等待 —— 表现就是"点了进度条不跳"。日志实证：
  //
  //   拖动到未转码区域 目标=41.7s 可寻址末端=40.0s
  //   拖动到未转码区域 目标=18.4s 可寻址末端=3.2s   ← 用户认为这些位置已转好
  //
  // 边界就该以 `seekable.end` 为准：它本来就是"浏览器认为可以 seek 到哪里"。
  // 踩到正在写的分片时，本地服务会返 503、hls.js 自己重试，那是可恢复的；
  // 而"点了完全不响应"是不可接受的。
  if (target <= seekableEnd + SEEK_SEEKABLE_TOLERANCE_SECONDS) {
    return { kind: 'immediate', target };
  }
  return { kind: 'wait', target, seekableEnd };
}

/**
 * 等待转码推进时，每一拍是否已经可以落位。
 *
 * 容差与 `planSeek` 同源：`seekable.end` 是分片边界，比目标少几十毫秒是常态，
 * 要求严格 `>=` 会让最后一片永远等不到。
 */
export function seekableReached(
  seekableEnd: number,
  target: number,
  duration = 0,
): boolean {
  if (!Number.isFinite(seekableEnd) || seekableEnd <= 0) return false;
  if (!Number.isFinite(target)) return false;
  // 与 `planSeek` 同一套判据（不再扣安全余量，理由见那里的注释）。
  return seekableEnd >= target - SEEK_SEEKABLE_TOLERANCE_SECONDS;
}

/* ═══════════════════════════════════════════════════════════════════════════
 * 一、连续 seek 合并（2026-11-10）
 * ═══════════════════════════════════════════════════════════════════════════
 *
 * ## 现象
 *
 * 用户连按快进（或在一秒内连续拖进度条）时，画面永远停在「缓冲中」。
 *
 * `ttv-playback.log` 会话 273 的实测序列（5 秒内 7 次落位）：
 *
 *   +4672.046 立即落位 目标=157.7s 可寻址末端=195.6s 来源=Hr
 *   +4672.047 媒体 waiting 缓冲等待
 *   +4672.087 媒体 canplay 可播放
 *   +4672.219 立即落位 目标=162.7s
 *   +4672.391 立即落位 目标=167.7s
 *   +4673.469 立即落位 目标=173.5s
 *   +4674.246 立即落位 目标=178.9s
 *   +4675.426 立即落位 目标=174.9s
 *
 * ## 根因
 *
 * 每一次 `video.currentTime = x` 都是一次**新的 MSE seek**：已缓冲区间被丢弃、
 * hls.js 从目标分片重新起拉、解码器重建一组帧。连续赋值 N 次 = 画面重建 N 次，
 * 用户看到的就是「一直在缓冲、永远不出来」。
 *
 * 旧实现有两道去重，但它们**都要求目标相同**才生效：
 *
 *   - `|target - video.currentTime| < 0.35`（挡 pointerdown/up 的重复落定）
 *   - `|target - lastSeekTarget| < 0.35 且间隔 < 1200ms`（挡同一点反复点）
 *
 * 连按快进时目标每次都 +5，两道判据都不成立 —— 一次都没挡住。
 *
 * ## 方案：leading + trailing
 *
 *   - **首次**（距上次真正落位已超过窗口，或还没有落位过）：立即落位。
 *     用户点一下必须马上有反应，不能为了合并让单击变慢。
 *   - **窗口内**：不落位，只把目标记进 `pending`（后来的覆盖先前的），
 *     并把到期时刻推到「最后一次请求 + 窗口」。
 *   - **到期**：落位一次，落的是最后一次请求的目标。
 *   - **拖动的终点**（`immediate`）：立即落位。进度条一次拖动会打两次 `seek`
 *     （down 一次、up 一次），两者可以差几十秒；把 up 也压进窗口，用户会看到
 *     "松手后画面停在起点 1.2 秒，然后才跳到终点"——比原来的"松手即跳"更糟。
 *     判据（"刚刚有没有 previewing 写入"）由宿主给，这里只认开关。
 *
 * 于是「连按 5 下」= 期间 0 次落位 + 最终 1 次落位，而不是 5 次；「拖到位松手」
 * 仍然是一次立即落位，没有任何附加延迟。
 *
 * 三条出口在日志里分别记 `触发=leading` / `触发=trailing` / `触发=drag-end`，
 * 复盘时能直接看出"这一下是用户点的、还是合并替他省的"。
 *
 * ## 放弃的替代方案
 *
 * 1. **纯 trailing（延迟一个窗口才落位）**：单击要等 1.2 秒画面才动，用户会
 *    判定「点了没反应」。leading 必须保留。
 * 2. **只做节流（窗口内最多落位一次，被挡下的请求直接丢弃）**：相邻请求间隔
 *    143~1180ms 并不均匀，节流只能挡住一部分。按会话 273 那 6 条立即落位回放，
 *    节流落位 3 次（多一次无谓重建），更要命的是**快速连按会丢意图**：连按 8 下
 *    +5 时它只落位第一下（落在 10s），用户按到 45s 的画面永远不出现。
 *    只有"合并到最终目标"（每次新请求都重置到期时刻，即 debounce）才对。
 *    （以上数字由 `scripts/verify-seek-coalesce.mjs` 回放实测，不是估算。）
 * 3. **在 UI 层（进度条 / 键盘 hook）做连点合并**：主播放器、动画播放器、
 *    画中画小窗各有一份 seek 入口，UI 层合并只能覆盖其中一条，而且无法在
 *    node 里复算。所以合并状态机放在这里，由三个调用方共用同一个语义。
 *
 * ## 为什么窗口是 1200ms（而不是 300/450）
 *
 * 沿用主播放器已有的 `REPEAT_SEEK_WINDOW_MS = 1200`：这段时长此前只用于挡
 * 「同一点反复点」，用户对「1.2 秒内不重复响应」已有体感，不引入新的时间尺度。
 * 日志里同一次连按内的相邻请求间隔是 143ms / 172ms，一次连按 4~8 下会落在
 * 1 秒以内，1.2 秒足以把它们收成一次；而间隔超过 1.2 秒的点击是用户的**独立
 * 意图**（换个位置接着看），合并它等于延迟正常操作。
 *
 * ## 与「等转码」的关系
 *
 * 合并只作用于 `planSeek` 判成 `immediate` 的分支 —— 那是唯一会改变画面的
 * 路径。`wait`（目标还没转好）本来就不碰 `currentTime`，它的轮询器很轻，
 * 而且用户需要立刻看到"你点的位置我记着"，所以不进合并器。
 * ------------------------------------------------------------------------- */

/**
 * 合并窗口（毫秒）。取值理由见上面「为什么窗口是 1200ms」。
 */
export const SEEK_COALESCE_WINDOW_MS = 1200;

/**
 * 「同一个目标」的判定容差（秒）。
 *
 * 与主播放器原 `SAME_TARGET_EPSILON_SECONDS` 同源：大于 MSE 的分片对齐误差
 * （几十毫秒），又小于用户微调的最小意图（拖动时每像素约 0.02~0.08 秒）。
 */
export const SEEK_COALESCE_SAME_TARGET_EPSILON_SECONDS = 0.35;

/**
 * 合并器的全部状态。不可变：每次判定返回**新**状态，调用方整体替换。
 *
 * 用不可变结构而不是在函数里改传入对象，是为了让 node 验收脚本能对同一串
 * 输入重放出确定的结果（`scripts/verify-seek-coalesce.mjs` 就是这么测的）。
 */
export interface SeekCoalesceState {
  /** 上一次**真正落位**的目标与时刻。`null` = 本次会话还没有落位过。 */
  readonly lastDispatch: { readonly target: number; readonly at: number } | null;
  /** 已记下、等待落位的目标。`null` = 当前没有在等。 */
  readonly pending: { readonly target: number; readonly at: number } | null;
  /** 自上次落位以来被合并掉的请求数。只用于打点，不参与判定。 */
  readonly mergedSinceDispatch: number;
}

/** 初始状态：什么都没发生过。换集 / 离开播放器时回到这里。 */
export const INITIAL_SEEK_COALESCE_STATE: SeekCoalesceState = Object.freeze({
  lastDispatch: null,
  pending: null,
  mergedSinceDispatch: 0,
});

/**
 * 一次判定的结论。
 *
 * - `dispatch`：现在就该写 `video.currentTime`（`reason` 区分是空闲后的首点
 *   还是窗口到期补的最后一击，日志里据此能区分"用户真点了一下"与"合并收尾"）。
 * - `pending`：不许碰播放头，调用方按 `flushAt` 定时再问一次。
 * - `ignored`：这次请求不该产生任何落位（重复点同一点 / 输入非法 / 没有 pending）。
 * - `not-due`：`flushSeek` 被提前唤醒，`flushAt` 还差一点。
 */
export type SeekDispatchDecision = {
  kind: 'dispatch';
  state: SeekCoalesceState;
  target: number;
  merged: number;
  /**
   * 为什么现在就落位：
   *   - `leading`：距上次落位已超过窗口，这是空闲后的第一下；
   *   - `trailing`：合并窗口到期，替一串连按收尾；
   *   - `drag-end`：一次拖动的终点（调用方判定），必须立刻生效。
   *
   * 三者在日志里必须分得开：排查"画面跳了几次"时要能区分"用户真点了一下"、
   * "合并收尾"与"松手落位"。合成一个 `dispatch` 会让日志失去分辨力。
   */
  reason: 'leading' | 'trailing' | 'drag-end';
};

export type SeekPendingDecision = {
  kind: 'pending';
  state: SeekCoalesceState;
  target: number;
  flushAt: number;
  merged: number;
};

export type SeekIgnoredDecision = {
  kind: 'ignored';
  state: SeekCoalesceState;
  reason: 'repeat-target' | 'invalid-target' | 'nothing-pending';
};

export type SeekNotDueDecision = {
  kind: 'not-due';
  state: SeekCoalesceState;
  flushAt: number;
};

/** 四种结论的并集（语义见上方注释）。 */
export type SeekCoalesceDecision =
  | SeekDispatchDecision
  | SeekPendingDecision
  | SeekIgnoredDecision
  | SeekNotDueDecision;

/**
 * requestSeek 的可能结论：**不含 not-due**。
 *
 * 这不是类型洁癖。调用方要写 if (ignored) return; if (pending) return; 之后直接
 * 取 decision.target；把 not-due 留在返回类型里 TS 收窄不掉，每个调用点都得
 * 再补一句"只有 flush 才会 not-due"的断言。
 */
export type SeekRequestDecision = SeekDispatchDecision | SeekPendingDecision | SeekIgnoredDecision;

/** flushSeek 的可能结论：**不含 pending**（到期只会落位或还没到期）。 */
export type SeekFlushDecision = SeekDispatchDecision | SeekIgnoredDecision | SeekNotDueDecision;

/** 可选的判定参数（验收脚本用短窗口/零容差复现边界，生产不传）。 */
export interface SeekCoalesceConfig {
  windowMs?: number;
  sameTargetEpsilonSeconds?: number;
}

/**
 * `requestSeek` 的行为开关（不是状态，不进状态机）。
 *
 * 只影响"这一次请求怎么判"，不改变已积累的窗口语义 —— 所以它是参数而不是
 * 状态字段：拖动终点不会被"记住"，下一次连按仍然按窗口合并。
 */
export interface SeekRequestOptions {
  /**
   * 这是**一次拖动的终点**：立刻落位，不压进合并窗口。
   *
   * 判据由调用方给（主播放器用"刚刚有过 previewing 写入"），因为"拖动结束"
   * 这件事只有 DOM 事件能判定，纯函数不该去猜时间戳的来历。
   */
  immediate?: boolean;
}

function resolveWindowMs(config: SeekCoalesceConfig): number {
  const value = config.windowMs;
  if (typeof value === 'number' && Number.isFinite(value) && value >= 0) return value;
  return SEEK_COALESCE_WINDOW_MS;
}

function resolveEpsilon(config: SeekCoalesceConfig): number {
  const value = config.sameTargetEpsilonSeconds;
  if (typeof value === 'number' && Number.isFinite(value) && value >= 0) return value;
  return SEEK_COALESCE_SAME_TARGET_EPSILON_SECONDS;
}

/**
 * 用户又点了一次（或连按的一下）。
 *
 * 三条分支，顺序不能换：
 *   1. **同一目标、窗口内** → 吞掉。这不是新的意图，落位也是原地重来。
 *      顺带清空 `pending`：用户已经回到刚落过的位置，之前挂着的那个目标不再是
 *      他要的。
 *   2. **空闲够久** → 立即落位（leading）。`pending` 非空时不走这条：那说明
 *      上一串动作还没收尾，中途插队会多出一次本可以省掉的落位。
 *   3. **其余** → 记下 `pending` 并覆盖旧目标，`flushAt = now + window`。
 *      每次覆盖都重置计时，这就是 debounce：只要用户还在按，落位就一直往后推。
 */
export function requestSeek(
  state: SeekCoalesceState,
  target: number,
  nowMs: number,
  config: SeekCoalesceConfig = {},
  options: SeekRequestOptions = {},
): SeekRequestDecision {
  if (!Number.isFinite(target) || !Number.isFinite(nowMs)) {
    return { kind: 'ignored', state, reason: 'invalid-target' };
  }
  const windowMs = resolveWindowMs(config);
  const epsilon = resolveEpsilon(config);
  const last = state.lastDispatch;

  // 同一目标 + 窗口内：原地连点。这条**优先于** `immediate`——拖动回到刚落过的
  // 位置时不需要再落一次，那只会白重建一次解码管线。
  if (last && Math.abs(last.target - target) < epsilon && nowMs - last.at < windowMs) {
    return { kind: 'ignored', reason: 'repeat-target', state: { ...state, pending: null } };
  }

  // 拖动的终点是用户的**最终意图**：它必须立刻落位，不能压进合并窗口。
  //
  // ## 为什么这条不能省（2026-11-10）
  //
  // 进度条一次拖动会打两次 `seek`：pointerdown 一次、pointerup 一次
  // （`ProgressBar` 的 64 / 88 行）。down 的目标是起点、up 的是终点，两者
  // 可以差几十秒。若 up 走进合并窗口，用户会看到"松手后画面停在起点 1.2 秒，
  // 然后才跳到终点"——比原来的"松手即跳"更糟。
  //
  // 连按快进不走这条：键盘 ← / → 没有 previewing 阶段，连续按下之间的请求
  // 全部走 pending 合并，这正是要的。判据由调用方给（见 store 的
  // `SEEK_DRAG_TAIL_MS`），纯函数不猜。
  if (options.immediate) {
    return {
      kind: 'dispatch',
      reason: 'drag-end',
      target,
      merged: state.mergedSinceDispatch,
      state: { lastDispatch: { target, at: nowMs }, pending: null, mergedSinceDispatch: 0 },
    };
  }

  const idleFor = last ? nowMs - last.at : Number.POSITIVE_INFINITY;
  if (!state.pending && idleFor >= windowMs) {
    return {
      kind: 'dispatch',
      reason: 'leading',
      target,
      merged: state.mergedSinceDispatch,
      state: { lastDispatch: { target, at: nowMs }, pending: null, mergedSinceDispatch: 0 },
    };
  }

  const merged = state.mergedSinceDispatch + 1;
  return {
    kind: 'pending',
    target,
    flushAt: nowMs + windowMs,
    merged,
    state: { ...state, pending: { target, at: nowMs }, mergedSinceDispatch: merged },
  };
}

/**
 * 定时器到点：落位 pending 的最后目标。
 *
 * `nowMs < pending.at + window` 时返回 `not-due` 而不是硬落位：调用方的定时器
 * 可能因为主线程卡顿提前触发（`setTimeout` 只会晚到，但可以被手动提前调用），
 * 提前落位等于把 debounce 变成节流，合并就失效了。调用方按 `flushAt` 重挂。
 */
export function flushSeek(
  state: SeekCoalesceState,
  nowMs: number,
  config: SeekCoalesceConfig = {},
): SeekFlushDecision {
  const pending = state.pending;
  if (!pending) return { kind: 'ignored', state, reason: 'nothing-pending' };
  const windowMs = resolveWindowMs(config);
  const dueAt = pending.at + windowMs;
  if (!Number.isFinite(nowMs) || nowMs < dueAt) {
    return { kind: 'not-due', state, flushAt: dueAt };
  }
  return {
    kind: 'dispatch',
    reason: 'trailing',
    target: pending.target,
    merged: state.mergedSinceDispatch,
    state: { lastDispatch: { target: pending.target, at: nowMs }, pending: null, mergedSinceDispatch: 0 },
  };
}

/**
 * 换集 / 离开播放器 / 转到「等转码」分支时清空。
 *
 * 不清的后果有两条，都会真的发生：
 *   1. 上一集 pending 的目标会在 1.2 秒后落到**新一集**的播放头上；
 *   2. `lastDispatch` 仍挂着，新一集里用户的第一次点击会被判成「重复点同一位置」
 *      而吞掉 —— 表现是"换集后第一次点进度条没反应"。
 */
export function resetSeekCoalesce(): SeekCoalesceState {
  return INITIAL_SEEK_COALESCE_STATE;
}

/* ═══════════════════════════════════════════════════════════════════════════
 * 二、「缓冲中」的分级（2026-11-10）
 * ═══════════════════════════════════════════════════════════════════════════
 *
 * ## 现象
 *
 * 用户截图里的「缓冲中… 00:00 / 00:00」其实混着**两件完全不同的事**：
 *
 *   - `uiState.kind === 'opening'`：这一集还在解析（服务端返回播放地址 + 本地
 *     起转码），项目内实测 7.4 秒（见 usePlaybackStore 里"解析那 7.4s 早就被
 *     预取吃掉了"那段）。屏幕上没有任何画面，时间码是 00:00。
 *   - `uiState.kind === 'buffering'`：源已经在播，只是下一段分片没到位。
 *     播放头有值，画面通常是最后一帧。
 *
 * 两者共用「缓冲中」文案，用户无法判断"是在等它开始"还是"它卡了一下"，
 * 也无从决定"要不要再等一下"。
 *
 * ## 这份函数只做一件事：把状态翻成人话
 *
 * 文案与判定放在纯函数里，而不是散在三个播放面（主播放器 / 动画播放器 /
 * 画中画小窗）各写一份 —— 三处早晚会说出三种不一样的话。`label` 用于胶囊
 * 那种一行位置，`detail` 用于状态卡（竖滑流 FeedView 的信息区）。
 *
 * ⚠️ 边界：当前 `VideoSurface` / `AnimeVideoSurface` 的加载胶囊**仍然**只读
 * `uiState.kind` 并显示固定的「缓冲中…」。本函数是给宿主层（FeedView 的状态
 * 区、播放诊断面板）用的；让播放器本体改文案要动那两个组件，不在本次作用域内。
 * 因此 store 另外在 `opening` ↔ `buffering` 切换时**打点**，日志里从此能区分
 * 这两段，下次实测可以拿它对时间。
 * ------------------------------------------------------------------------- */

/** 播放阶段，与 `PlaybackUiState.kind` 一一对应（多一个 `idle` 之外的都不加）。 */
export type PlaybackStage = 'idle' | 'opening' | 'buffering' | 'playing' | 'recovering' | 'ended' | 'error';

/** 喂给 `describePlaybackStage` 的最小快照（不引入 `PlaybackUiState` 的联合类型依赖）。 */
export interface PlaybackStageInput {
  kind: PlaybackStage;
  /** 播放头前方已缓冲的秒数（`buffering` 才有意义）。 */
  bufferedAheadSeconds?: number;
  /** 重新装载的第几次尝试（`recovering`）。 */
  attempt?: number;
  /** 解析完成百分比 0~100；未知时省略，文案就不报数。 */
  preparePercent?: number;
  /** 错误是否可恢复（`error`）。 */
  recoverable?: boolean;
}

/** 分级后的两行文案。`detail` 为空串表示"没有补充可说"。 */
export interface PlaybackStageText {
  stage: PlaybackStage;
  label: string;
  detail: string;
}

/**
 * 把阶段翻成用户能据此做决定的话。
 *
 * 数字只用**已知**的：`preparePercent` 没给就不编造百分比，`bufferedSeconds`
 * 没给就不报秒数。这与项目「诚实报告边界」是同一条规矩 —— 报一个假的 0%
 * 比不报更糟，用户会以为解析卡住了。
 */
export function describePlaybackStage(input: PlaybackStageInput): PlaybackStageText {
  const stage = input.kind;
  switch (stage) {
    case 'idle':
      return { stage, label: '准备中…', detail: '' };
    case 'opening': {
      const percent = input.preparePercent;
      const hasPercent = typeof percent === 'number' && Number.isFinite(percent) && percent > 0;
      return {
        stage,
        label: '正在解析整集…',
        detail: hasPercent
          ? `服务端还在准备这一集（${Math.min(100, Math.round(percent))}%），拿到地址就起播`
          : '服务端还在准备这一集，拿到播放地址就起播',
      };
    }
    case 'buffering': {
      // 报的是**播放头前方的余量**，不是 buffered 末端。用户问"还要转多久"时
      // 想知道的正是这个：末端 40 秒、播放头在 38 秒，那 40 毫无意义。
      const ahead = input.bufferedAheadSeconds;
      const known = typeof ahead === 'number' && Number.isFinite(ahead) && ahead > 0.5;
      return {
        stage,
        label: '缓冲中…',
        detail: known ? `前方已缓冲 ${Math.floor(ahead)} 秒，正在追播放头` : '正在拉取下一段数据',
      };
    }
    case 'playing':
      return { stage, label: '播放中', detail: '' };
    case 'recovering': {
      const attempt = input.attempt;
      const known = typeof attempt === 'number' && Number.isFinite(attempt) && attempt > 0;
      return {
        stage,
        label: '画面停顿，正在恢复…',
        detail: known ? `第 ${Math.round(attempt)} 次自动重试` : '自动重试中',
      };
    }
    case 'ended':
      return { stage, label: '本集播完', detail: '' };
    case 'error':
      return {
        stage,
        label: '播放出错',
        detail: input.recoverable ? '可以重试，也可以换一集' : '换一集试试，或在详情页切换来源',
      };
    default:
      return { stage: 'idle', label: '准备中…', detail: '' };
  }
}