import React from 'react';
import { AlertTriangle, CheckCircle2, LoaderCircle } from 'lucide-react';

/**
 * 推荐页的**唯一**状态区。
 *
 * ## 为什么要把状态从播放器遮罩里搬到这一层
 *
 * 用户实机截图暴露的现场是：整屏黑 + 一个「缓冲中...」+ 底部 `00:00 / 00:00`。
 * 那一帧其实处在**首次解析**阶段（向内容源申请整集播放地址，实测约 7 秒），
 * 但播放器 overlay 把「解析中」和「缓冲等待」写成同一句话，用户读到的信息是
 * 「画面卡在 0 秒」——既不知道进度、也不知道还要多久、更不知道页面是否还活着。
 *
 * 所以这里把三件事拆开：
 *
 * | 阶段 | 用户看到的 | 真实在发生什么 |
 * | --- | --- | --- |
 * | `loading` | 正在加载推荐… | 推荐流本身还没回来，库里连剧名都没有 |
 * | `opening` | 正在解析整集… | 已知道是哪部剧哪一集，正在等源站签发播放地址 |
 * | `buffering` | 缓冲中… | 地址已就绪，媒体在积累可播画面 |
 * | `error` | 具体错误 + 出口按钮 | 链路真的断了，给「重试 / 换一部」 |
 * | `completed` | 当前短剧已播放完成 | 正常终态，不是错误 |
 *
 * `opening` 与 `buffering` 分开是这次改造的核心：前者的等待是**源站往返**，
 * 后者是**网络吞吐**，用户能采取的动作完全不同（前者只能等，后者可以换集）。
 *
 * ## 为什么状态推导抽成纯函数
 *
 * 三个阶段的判定是 `error > completed > opening > loading > buffering` 的严格
 * 优先级，只要写进 JSX 就只能开窗口点出来验证。`deriveFeedPhase` 保持纯函数，
 * `scripts/verify-feed-view.mjs` 能确定性锁住这套优先级——尤其是「解析中」
 * 不能被 `loading` 抢先、`error` 不能被任何阶段盖住这两条。
 *
 * ## 关于 `isBuffering` 目前恒为 false
 *
 * `buffering` 的真实信号只存在于 `usePlaybackStore.uiState.kind === 'buffering'`
 * （媒体 `waiting` 事件）。本文件所在的作用域不允许订阅那个 store，`useFeedStore`
 * 也没有媒体缓冲字段。所以 `FeedView` 现在传 `isBuffering: false`，Banner 的这一支
 * 先按「接口就绪、信号待接」落地：等 A 线的 uiState 落定后，只需把 FeedView 里那
 * 一行常量换成真实信号，本文件不用再改。**宁可暂时少一种状态，也不要拿
 * 「pending 与否」去猜缓冲——那会在已经正常播放时也挂着「缓冲中…」。**
 */
export type FeedStatusPhase = 'loading' | 'opening' | 'buffering' | 'error' | 'completed';

/** 主文案。`error` 只用兜底句，真实原因由调用方以 `message` 传入。 */
const STATUS_TEXT: Record<FeedStatusPhase, string> = {
  loading: '正在加载推荐…',
  opening: '正在解析整集…',
  buffering: '缓冲中…',
  error: '出错了',
  completed: '当前短剧已播放完成',
};

/**
 * 副文案：回答「为什么这么慢」。
 *
 * 之所以要写这一行：`opening` 阶段的耗时来自源站往返（实测约 7 秒），比任何
 * 一次真实缓冲都长。不解释的话，用户只会认为播放器坏了。
 */
const STATUS_HINT: Partial<Record<FeedStatusPhase, string>> = {
  loading: '正在取这一频道的推荐列表',
  opening: '正在向内容源申请这一集的播放地址，这一步比缓冲更慢',
  buffering: '播放地址已就绪，正在积累可播放的画面',
};

/**
 * 语义色只用在语义需要的地方：rose = 真错误，emerald = 正常终态，
 * 其余阶段保持 white/glass 底 —— 一个正在加载的页面不该先染上警告色。
 */
const STATUS_TONE: Record<FeedStatusPhase, string> = {
  loading: 'border-white/15 bg-black/55',
  opening: 'border-white/15 bg-black/55',
  buffering: 'border-white/15 bg-black/55',
  error: 'border-rose-400/45 bg-rose-950/75',
  completed: 'border-emerald-400/40 bg-emerald-950/70',
};

export function feedStatusText(phase: FeedStatusPhase): string {
  return STATUS_TEXT[phase];
}

export function feedStatusHint(phase: FeedStatusPhase): string | undefined {
  return STATUS_HINT[phase];
}

export function feedStatusTone(phase: FeedStatusPhase): string {
  return STATUS_TONE[phase];
}

export interface FeedPhaseInput {
  /** `useFeedStore.error`。 */
  error: string | null;
  /** `useFeedStore.isCompleted`。 */
  isCompleted: boolean;
  /** `useFeedStore.isOpening`：整集解析在途。 */
  isOpening: boolean;
  /** `useFeedStore.isLoading`：推荐流或详情在途。 */
  isLoading: boolean;
  /** 已经有 `feed.detail`，即「至少知道这是哪部剧的哪几集」。 */
  hasDetail: boolean;
  /** 媒体 `waiting`。当前作用域拿不到真实信号，见文件头。 */
  isBuffering: boolean;
}

/**
 * 状态优先级（顺序本身就是契约）：
 *
 * 1. `error` —— 断链时不能让转圈盖住错误。
 * 2. `completed && hasDetail` —— 末集正常播完时 store 置 `isCompleted` 而**不清 detail**
 *    （2026-11-10 第四次改版把跨季/换部撤掉了，完成态就地停留）；没有 detail 的
 *    completed 只是首屏还没开始的中间态，不该喊「已播放完成」。
 * 3. `isOpening` —— 解析整集比「加载中」更具体，不能被 `isLoading` 抢先。
 * 4. `isLoading`。
 * 5. `isBuffering` —— 只可能在源就绪之后发生；与前面几项并存时先报更本质的那一步。
 * 6. 其余不渲染（正常播放中，别在画面上留一块常驻提示）。
 */
export function deriveFeedPhase(input: FeedPhaseInput): FeedStatusPhase | null {
  if (input.error) return 'error';
  if (input.isCompleted && input.hasDetail) return 'completed';
  if (input.isOpening) return 'opening';
  if (input.isLoading) return 'loading';
  if (input.isBuffering) return 'buffering';
  return null;
}

export interface FeedStatusBannerProps {
  phase: FeedStatusPhase;
  /** 错误原因或完成说明；只有 `error` 阶段必须给。 */
  message?: string;
  /** `buffering` 的已播进度（如「已播 1:12」）。没有真实进度不要传。 */
  progressText?: string;
  canSwitchSeries?: boolean;
  onSwitchSeries?: () => void;
  onRetry?: () => void;
  retryLabel?: string;
  className?: string;
}

const ACTION_PRIMARY =
  'rounded-lg bg-white px-3 py-2 text-xs font-bold text-slate-900 transition hover:bg-slate-100 '
  + 'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 '
  + 'focus-visible:ring-offset-2 focus-visible:ring-offset-black/60';
const ACTION_SECONDARY =
  'rounded-lg border border-white/35 bg-black/35 px-3 py-2 text-xs font-semibold text-white transition '
  + 'hover:bg-black/55 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 '
  + 'focus-visible:ring-offset-2 focus-visible:ring-offset-black/60';

export const FeedStatusBanner: React.FC<FeedStatusBannerProps> = ({
  phase,
  message,
  progressText,
  canSwitchSeries = false,
  onSwitchSeries,
  onRetry,
  retryLabel,
  className = '',
}) => {
  const isError = phase === 'error';
  const isTerminal = isError || phase === 'completed';
  const headline = isError ? (message || feedStatusText('error')) : feedStatusText(phase);
  const hint = feedStatusHint(phase);
  const tone = feedStatusTone(phase);
  const Icon = isError ? AlertTriangle : phase === 'completed' ? CheckCircle2 : LoaderCircle;

  return (
    // `role` 跟着语义走：错误用 alert（需要立刻被打断），其余阶段用 status（不抢当前朗读）。
    <section
      className={`pointer-events-none absolute inset-x-0 top-16 z-30 flex justify-center px-4 ${className}`}
      role={isError ? 'alert' : 'status'}
      aria-live={isError ? 'assertive' : 'polite'}
      aria-atomic="true"
      data-feed-status={phase}
    >
      <div
        className={`pointer-events-auto flex w-full max-w-md items-start gap-3 rounded-2xl border px-4 py-3 text-white shadow-fluent backdrop-blur-sm animate-fluent-page-in ${tone}`}
      >
        <Icon
          className={`mt-0.5 h-4 w-4 shrink-0 ${phase === 'buffering' || phase === 'opening' || phase === 'loading' ? 'animate-spin' : ''}`}
          aria-hidden="true"
        />
        <div className="min-w-0 flex-1">
          <p className="text-sm font-semibold leading-5">{headline}</p>
          {hint && <p className="mt-1 text-[11px] leading-4 text-white/70">{hint}</p>}
          {progressText && <p className="mt-1 text-[11px] leading-4 text-white/70">{progressText}</p>}
          {isTerminal && (onRetry || onSwitchSeries) && (
            // 出口按钮与错误块一起收进这一区：旧实现是「整屏错误遮罩 + 播放器自己的
            // 缓冲遮罩」两层叠着，用户既看不清哪句是真的，也点不到按钮。
            <div className="mt-2 flex items-center gap-2">
              {canSwitchSeries && onSwitchSeries && (
                <button type="button" onClick={() => onSwitchSeries()} className={ACTION_PRIMARY}>
                  换一部
                </button>
              )}
              {onRetry && (
                <button type="button" onClick={onRetry} className={ACTION_SECONDARY}>
                  {retryLabel || (phase === 'completed' ? '重播' : '重试')}
                </button>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
};