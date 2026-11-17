import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Maximize, Minimize, Pause, Play, SkipForward, Volume2, VolumeX } from 'lucide-react';
import { usePlaybackActions, usePlaybackStore } from '../../stores/usePlaybackStore';
import { formatTime } from './ProgressBar';

/**
 * 推荐页**专用**的底部控制条。
 *
 * ## 为什么自绘一套，而不是复用 `PlayerControls`
 *
 * 用户原话：「播放控制栏重新设计一套兼容方案，并且对我这个短剧推荐区域的播放器，
 * 不要复用原来的格式，只保留原来的风格就行」。
 *
 * 主播放器的控制条是按「点开一部剧、认真看」设计的：11 个控件（上一集 / 快退 10s /
 * 播放 / 下一集 / 时间 / 音量 / 自动连播 / 倍速 / 弹幕 / 增强 / 选集 / 画中画 /
 * 全屏）排成一条。推荐页是「刷着看」的形态，这些控件的命中率与需求都不同 ——
 * 倍速/弹幕/增强/选集/画中画在推荐流里基本不会被碰，却把每条都得挤进同一行，
 * 于是每一颗都变小、变难按。**真正的问题不是"控件不好看"，是"控件被摆在了不需要
 * 它的场景里"**。
 *
 * 这一条只放刷着看真的会点的五样：播放/暂停、进度、时间码、下一集、全屏。
 *
 * | 不放在这里的东西 | 它在哪里 |
 * | --- | --- |
 * | 静音 | 右侧竖排 `FeedControls`（已有，重复放等于两个入口抢同一件事） |
 * | 上一集 | 「详细」面板底部的兜底入口（2026-11-10 第四次改版后，方向键/上滑 已**不再**换集，只换剧） |
 * | 倍速 / 弹幕 / 增强 / 选集 / 画中画 / 清晰度 | 主播放器 |
 *
 * 样式沿用项目既有风格（white/glass + fluent 阴影、`rounded-full` 托盘），
 * 但布局与主播放器无关 —— 「只保留原来的风格」指的是配色、圆角、动画语汇。
 *
 * ## 高频读数为什么只在这个组件内部订阅
 *
 * `position` 每秒变 4 次。上一轮已经把推荐页主体的播放订阅下沉掉（`FeedStageOverlay`），
 * 现在再多一处 `usePlaybackStore()` 就等于把整棵推荐页又拖回「每秒重渲染 4 次」。
 * 所以：
 * - `FeedView` 只传**动作与低频状态**（`isFullscreen` / `canGoNext`），不订播放进度；
 * - `position` / `duration` 只在本组件里读，重渲染范围就到这条 44px 高的条为止。
 *
 * 动作走 `usePlaybackActions()`（低频专线）：`togglePlay` / `seek` 在专线里，
 * 且它们只在用户点击时换引用。
 *
 * ## 拖动语义：拖动只改读数，松手才落位
 *
 * 一次拖动会经过几十个像素点，每个点都调 `seek()` 就会把几十次
 * `currentTime` 赋值压到播放器上 —— 每次赋值都要清空缓冲重来，画面会一直停在
 * 「缓冲中」。这是 `services/seekPlan.ts` 里那条结论的同源现象，所以这里
 * **只在 `pointerup` 时调一次 `seek()`**，中间过程只更新本地 `preview` 读数。
 *
 * 为什么还要「落位后短暂保留 preview」：`seek()` 之后 store 的 `position` 要等
 * 下一个 `timeupdate`（最多 250ms）才跟上，若立刻回落到 `position`，进度条会
 * 先弹回起点再跳过去 —— 看起来像拖动没生效。`preview` 会保留到 `position`
 * 追上（差值 < 0.75s）或 1.2s 兜底超时。
 */

/** 键盘微调的步长（秒）。与主播放器方向键的 ±5s 同一量级。 */
export const FEED_BAR_SKIP_SECONDS = 5;

/** 落位后 preview 最多保留多久（ms）。seek 被 clamp / 失败时不至于一直挂着旧读数。 */
const PREVIEW_HOLD_MS = 1200;

/** preview 与真实位置差多少就认为「已经追上」。 */
const PREVIEW_SETTLE_SECONDS = 0.75;

const clamp01 = (value: number): number => {
  if (!Number.isFinite(value)) return 0;
  if (value <= 0) return 0;
  if (value >= 1) return 1;
  return value;
};

/**
 * 指针横坐标 → 轨道比例（0~1）。
 *
 * 抽成纯函数是为了能确定性验收：轨道宽度为 0（隐藏 / 布局未完成）、指针在
 * 轨道左侧、指针在右侧之外，这三种情况都必须落在 [0,1] 内，否则会算出
 * 负数或超出片长的秒数，`seek()` 会拿到 NaN/越界值。
 */
export function feedBarRatio(clientX: number, rect: { left: number; width: number }): number {
  if (!Number.isFinite(clientX) || !Number.isFinite(rect.left) || !Number.isFinite(rect.width)) return 0;
  if (rect.width <= 0) return 0;
  return clamp01((clientX - rect.left) / rect.width);
}

/** 比例 → 绝对秒数，并按片长收口。片长未知（≤0）时返回 0。 */
export function feedBarTargetSeconds(ratio: number, duration: number): number {
  if (!Number.isFinite(duration) || duration <= 0) return 0;
  return clamp01(ratio) * duration;
}

/**
 * 键盘在进度条上的目标位置。返回 `null` = 这个键不归进度条管
 * （`Tab` / `Enter` / 其他都交给浏览器与按钮本身）。
 *
 * 方向键±`FEED_BAR_SKIP_SECONDS`、`Home` 回片头、`End` 到片尾。
 */
export function feedBarKeyboardTarget(current: number, key: string, duration: number): number | null {
  if (!Number.isFinite(duration) || duration <= 0) return null;
  const from = Number.isFinite(current) ? Math.min(Math.max(current, 0), duration) : 0;
  switch (key) {
    case 'ArrowLeft':
    case 'ArrowDown':
      return Math.max(0, from - FEED_BAR_SKIP_SECONDS);
    case 'ArrowRight':
    case 'ArrowUp':
      return Math.min(duration, from + FEED_BAR_SKIP_SECONDS);
    case 'Home':
      return 0;
    case 'End':
      return duration;
    default:
      return null;
  }
}

interface FeedPlaybackBarProps {
  /** 还能不能往下一集（`feed.detail` 未就绪或正在解析时为 false）。 */
  canGoNext: boolean;
  onNextEpisode: () => void;
  isFullscreen: boolean;
  onToggleFullscreen: () => void;
}

/** 单颗图标按钮：40×40 触控目标、图标 20px、hover 白 15%。 */
const BAR_ICON_BUTTON =
  'flex h-10 w-10 shrink-0 items-center justify-center rounded-full text-white '
  + 'transition-all duration-200 ease-[cubic-bezier(0.22,1,0.36,1)] hover:scale-110 hover:bg-white/20 hover:shadow-[inset_0_1px_0_rgba(255,255,255,0.28)] active:scale-95 '
  + 'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 '
  + 'focus-visible:ring-offset-2 focus-visible:ring-offset-black/60 '
  + 'disabled:cursor-not-allowed disabled:opacity-40';

export const FeedPlaybackBar: React.FC<FeedPlaybackBarProps> = ({
  canGoNext,
  onNextEpisode,
  isFullscreen,
  onToggleFullscreen,
}) => {
  const { isPlaying, togglePlay, seek, isMuted, toggleMute } = usePlaybackActions();
  // 只在这条 44px 高的条里订高频读数，见文件头。
  const { position, duration } = usePlaybackStore();

  const trackRef = useRef<HTMLDivElement | null>(null);
  /** 拖动 / 落位过渡期的读数；`null` = 跟真实 `position`。 */
  const [preview, setPreview] = useState<number | null>(null);
  const draggingRef = useRef(false);
  const holdTimerRef = useRef<number | null>(null);

  const hasDuration = Number.isFinite(duration) && duration > 0;
  const displayPosition = preview ?? (Number.isFinite(position) ? position : 0);
  const safePosition = hasDuration ? Math.min(Math.max(displayPosition, 0), duration) : Math.max(displayPosition, 0);
  const ratio = hasDuration ? clamp01(safePosition / duration) : 0;

  /** 落位后真实位置追上来了就撤掉 preview（兜底交给 `PREVIEW_HOLD_MS`）。 */
  useEffect(() => {
    if (preview === null || draggingRef.current) return;
    if (Math.abs(position - preview) < PREVIEW_SETTLE_SECONDS) setPreview(null);
  }, [position, preview]);

  useEffect(() => () => {
    if (holdTimerRef.current !== null) window.clearTimeout(holdTimerRef.current);
  }, []);

  /** 指针位置 → 目标秒数。轨道还没布局（宽度 0）时退回 0，不产生 NaN。 */
  const targetAt = useCallback((clientX: number): number => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect) return 0;
    return feedBarTargetSeconds(feedBarRatio(clientX, { left: rect.left, width: rect.width }), duration);
  }, [duration]);

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!hasDuration) return;
    // 阻止默认：否则在 WebView2 里按下会被当成文本选择/拖动起点。
    event.preventDefault();
    // pointer capture：指针滑出这条 4px 高的细槽后仍能继续拖，松手自动结束，
    // 不需要在 window 上挂一对悬挂的监听器。
    event.currentTarget.setPointerCapture(event.pointerId);
    draggingRef.current = true;
    if (holdTimerRef.current !== null) {
      window.clearTimeout(holdTimerRef.current);
      holdTimerRef.current = null;
    }
    setPreview(targetAt(event.clientX));
  };

  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    // **只更新读数，不 seek**：见文件头「拖动语义」。
    if (!draggingRef.current) return;
    if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
    setPreview(targetAt(event.clientX));
  };

  const onPointerUp = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!draggingRef.current) return;
    draggingRef.current = false;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    const target = targetAt(event.clientX);
    setPreview(target);
    // 整次拖动**只有这一处**落位。
    seek(target);
    holdTimerRef.current = window.setTimeout(() => {
      holdTimerRef.current = null;
      setPreview(null);
    }, PREVIEW_HOLD_MS);
  };

  /** 取消（如系统抢走指针）：撤掉读数、不落位，保持原来的播放位置。 */
  const onPointerCancel = () => {
    if (!draggingRef.current) return;
    draggingRef.current = false;
    setPreview(null);
  };

  const onTrackKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const target = feedBarKeyboardTarget(safePosition, event.key, duration);
    if (target === null) return;
    // 方向键在这里被消费掉：外层 `FeedView` 的 ArrowUp/ArrowDown 现在是「换一部」
    // （第四次改版），而焦点停在进度条上时用户要的是微调位置。条根挂了
    // `data-feed-no-swipe`，`blocksFeedKeys` 据此把外层监听挡掉，所以不会一次
    // 按键做两件事（既微调了位置又换了一部剧）。
    event.preventDefault();
    setPreview(target);
    seek(target);
    if (holdTimerRef.current !== null) window.clearTimeout(holdTimerRef.current);
    holdTimerRef.current = window.setTimeout(() => {
      holdTimerRef.current = null;
      setPreview(null);
    }, PREVIEW_HOLD_MS);
  };

  const maxSeconds = hasDuration ? Math.round(duration) : 0;
  const nowSeconds = Math.round(safePosition);
  const timeText = `${formatTime(safePosition)} / ${formatTime(duration)}`;

  return (
    <div
      // `role="toolbar"`：读屏会把这一串当成一个可 Tab 的操作组，而不是散落的按钮。
      role="toolbar"
      aria-label="播放控制"
      // 让 `FeedView` 的滚轮/方向键白名单生效：条上的操作不会顺手换走一部剧。
      data-feed-no-swipe
      className="pointer-events-auto absolute inset-x-0 bottom-0 z-30 flex flex-col bg-gradient-to-t from-black/95 via-black/80 to-black/45 backdrop-blur-md text-white animate-fluent-page-in"
      data-feed-playback-bar
    >
      <div ref={trackRef} className="-mt-2 flex h-3.5 w-full items-end">
        <div
          role="slider"
          tabIndex={0}
          aria-label="播放进度"
          aria-orientation="horizontal"
          aria-valuemin={0}
          aria-valuemax={maxSeconds}
          aria-valuenow={nowSeconds}
          aria-valuetext={timeText}
          aria-disabled={!hasDuration}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerCancel}
          onKeyDown={onTrackKeyDown}
          className="group relative flex h-3.5 w-full cursor-pointer touch-none items-end focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 aria-disabled:cursor-not-allowed"
        >
          {/* 贴边细条：常态 2.5px、悬停展开到 4px，热区 14px，用 `scaleX` 避免拖动时整条控制栏重排。 */}
          <span className="absolute inset-x-0 bottom-0 h-[2.5px] bg-white/25 transition-[height] duration-150 group-hover:h-1" aria-hidden="true" />
          <span
            className="absolute bottom-0 left-0 h-[2.5px] origin-left bg-white transition-[height,transform] duration-150 group-hover:h-1"
            style={{ width: '100%', transform: `scaleX(${ratio})` }}
            aria-hidden="true"
          />
          <span
            className="absolute -bottom-[3px] h-2.5 w-2.5 rounded-full bg-white shadow opacity-0 transition-opacity duration-150 group-hover:opacity-100 group-focus-visible:opacity-100"
            style={{ left: `clamp(0px, calc(${ratio * 100}% - 5px), calc(100% - 10px))` }}
            aria-hidden="true"
          />
        </div>
      </div>

      <div className="flex h-11 items-center justify-between gap-3 px-3.5">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={togglePlay}
            className={BAR_ICON_BUTTON}
            aria-label={isPlaying ? '暂停' : '播放'}
            title={isPlaying ? '暂停' : '播放'}
          >
            {isPlaying
              ? <Pause className="h-5 w-5" aria-hidden="true" />
              : <Play className="h-5 w-5" aria-hidden="true" />}
          </button>

          <span className="shrink-0 text-xs font-semibold tabular-nums text-white/85" data-feed-timecode>
            {timeText}
          </span>
        </div>

        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={onNextEpisode}
            disabled={!canGoNext}
            className={BAR_ICON_BUTTON}
            aria-label="下一集"
            title="下一集"
          >
            <SkipForward className="h-5 w-5" aria-hidden="true" />
          </button>

          <button
            type="button"
            onClick={toggleMute}
            className={BAR_ICON_BUTTON}
            aria-label={isMuted ? '取消静音' : '静音'}
            title={isMuted ? '取消静音' : '静音'}
            aria-pressed={isMuted}
          >
            {isMuted
              ? <VolumeX className="h-5 w-5" aria-hidden="true" />
              : <Volume2 className="h-5 w-5" aria-hidden="true" />}
          </button>

          <button
            type="button"
            onClick={onToggleFullscreen}
            className={BAR_ICON_BUTTON}
            aria-label={isFullscreen ? '退出全屏' : '全屏'}
            title={isFullscreen ? '退出全屏' : '全屏'}
          >
            {isFullscreen
              ? <Minimize className="h-5 w-5" aria-hidden="true" />
              : <Maximize className="h-5 w-5" aria-hidden="true" />}
          </button>
        </div>
      </div>
    </div>
  );
};