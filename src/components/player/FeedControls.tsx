import React, { useCallback, useEffect, useId, useRef, useState } from 'react';
import {
  ArrowLeft,
  Bookmark,
  Check,
  ChevronDown,
  ChevronUp,
  Info,
  LoaderCircle,
  Shuffle,
  X,
} from 'lucide-react';
import type { FavoriteMark } from '../../types/favorite';
import { feedEpisodeStep } from '../../services/feedNavigation';
import { playableBoundaryText } from './FeedSlide';

/**
 * 推荐页的右侧操作竖排 + 左上返回 + 「详细」信息面板。
 *
 * ## 2026-11-10 第二次改版：竖排从四个语义压成三个
 *
 * 旧竖排是「上一集 / 下一集 / 追剧 / 换一部」。四颗按钮里混了**两种性质**：
 * 前两颗是「内容导航」，后两颗是「对这部剧的操作」。用户要翻集时会本能地往
 * 竖排里找，但它和「换一部」挤在一起，误触代价完全不同——前者只是换一集，
 * 后者会丢掉当前这部剧。
 *
 * 现在按语义切开：
 *
 * | 位置 | 内容 |
 * | --- | --- |
 * | 右侧竖排 | 收藏 / 详细 —— 对当前短剧的轻量无框交互，零播放器状态订阅 |
 * | 底部控制栏 | 播放 / 进度 / 下一集 / 静音 / 全屏（由 `FeedPlaybackBar` 统一承载） |
 * | 详细面板底部 | 上一集 / 下一集 / 换一部 —— 全是「换内容」的导航 |
 *
 * 上一集/下一集之所以没有彻底删掉：2026-11-10 第四次改版之后，方向键/滚轮/上滑
 * 一律换成「换另一部剧」，本剧内的换集只剩**不操作时的自动连播**。屏幕阅读器
 * 用户够不到手势，也未必等得到自动连播，所以面板里保留两颗可被念出来、可被
 * Tab 到的兜底入口 —— 它们只在本剧内生效，末集不外溢。
 *
 * ## 面板的滚动与键盘
 *
 * 面板容器挂 `data-feed-no-swipe`：`FeedView` 的 `isInteractiveTarget` 认这个
 * 属性，所以面板内滚轮不会被当成"翻集手势"，方向键也不会把面板后面那一集换掉。
 * 这不是重复实现，而是复用同一个白名单。
 */

interface FeedControlsProps {
  episodeIndex: number;
  /** 当前集号（`episode.episodeNumber`，可能因季号而不等于 index + 1）。 */
  episodeNumber: number;
  /** `feed.detail.episodes.length`：列表里能翻的集数边界。 */
  episodeCount: number;
  /** `feed.detail.episodesCount`：站方口径的总集数，只在面板里展示。 */
  totalEpisodes: number;
  favoriteMark?: FavoriteMark;
  /** 只含「解析整集 / 加载详情」；不含后台补页。 */
  busy: boolean;
  canSwitchSeries: boolean;
  /** 面板数据全部来自 `feed.detail` + `feed.currentItem`，不新开请求。 */
  title: string;
  description: string;
  tags: string[];
  origin: string;
  playableEpisodes?: number;
  onPrevious: () => void;
  onNext: () => void;
  onSwitchSeries: () => void;
  onToggleFavorite: () => void;
  onClose: () => void;
}

/** 抖音风右侧悬浮操作（无框重影按钮 + 清晰投影，触控热区 >= 48px）。 */
const ACTION_CLASS =
  'flex min-h-12 min-w-12 flex-col items-center justify-center gap-1 rounded-xl px-1.5 py-1 text-white '
  + 'drop-shadow-[0_2px_8px_rgba(0,0,0,0.85)] transition-all duration-200 ease-[cubic-bezier(0.22,1,0.36,1)] '
  + 'hover:scale-110 active:scale-95 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 '
  + 'disabled:cursor-not-allowed disabled:opacity-45 disabled:hover:scale-100';

/** 面板底部动作：同样是 `rounded-lg`，但尺寸按面板内边距走。 */
const PANEL_ACTION =
  'flex flex-1 items-center justify-center gap-1 rounded-lg border border-white/20 bg-white/10 px-3 py-2 '
  + 'text-xs font-semibold text-white shadow-[inset_0_1px_0_rgba(255,255,255,0.2)] '
  + 'transition-all duration-200 ease-[cubic-bezier(0.22,1,0.36,1)] hover:scale-[1.03] hover:border-white/35 hover:bg-white/20 active:scale-95 '
  + 'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 '
  + 'focus-visible:ring-offset-2 focus-visible:ring-offset-black/60 '
  + 'disabled:cursor-not-allowed disabled:opacity-45';

export const FeedControls: React.FC<FeedControlsProps> = ({
  episodeIndex,
  episodeNumber,
  episodeCount,
  totalEpisodes,
  favoriteMark,
  busy,
  canSwitchSeries,
  title,
  description,
  tags,
  origin,
  playableEpisodes,
  onPrevious,
  onNext,
  onSwitchSeries,
  onToggleFavorite,
  onClose,
}) => {
  const [detailOpen, setDetailOpen] = useState(false);
  const detailButtonRef = useRef<HTMLButtonElement | null>(null);
  const panelRef = useRef<HTMLElement | null>(null);
  const titleId = useId();
  const panelId = useId();

  /** 关闭面板并把焦点还给「详细」按钮：焦点不能掉进 body，否则下一位 Tab 从文档头开始。 */
  const closeDetail = useCallback((restoreFocus = true) => {
    setDetailOpen(false);
    if (restoreFocus) detailButtonRef.current?.focus({ preventScroll: true });
  }, []);

  useEffect(() => {
    if (!detailOpen) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      closeDetail();
    };
    window.addEventListener('keydown', onKeyDown);
    // 面板本身可聚焦（tabIndex=-1）：打开后先把焦点送进去，读屏才会念出标题。
    const focusTimer = window.setTimeout(() => panelRef.current?.focus({ preventScroll: true }), 0);
    return () => {
      window.removeEventListener('keydown', onKeyDown);
      window.clearTimeout(focusTimer);
    };
  }, [closeDetail, detailOpen]);

  const boundary = playableBoundaryText({ episodesCount: totalEpisodes, playableEpisodes });
  const canOpenDetail = Boolean(title);
  /**
   * 「下一集」是否可用：与底部条同一个纯函数判定。
   *
   * 旧口径是 `busy || episodeCount === 0` —— 到末集这颗按钮仍然亮着，点下去
   * `feed.nextEpisode` 是 no-op（不再跨季），用户看到的是"点了没反应"。
   * 现在末集与可播边界处如实禁用，与 `FeedPlaybackBar` 的 canGoNext 完全一致。
   */
  const canGoNext = !busy && feedEpisodeStep(episodeIndex, episodeCount, 'next', playableEpisodes) !== null;

  return (
    <>
      <div
        className="pointer-events-auto absolute right-4 bottom-24 z-40 flex flex-col items-center gap-4 sm:right-5"
        role="group"
        aria-label="播放操作"
        aria-busy={busy}
        data-feed-no-swipe
      >
        <button
          type="button"
          className={ACTION_CLASS}
          onClick={onToggleFavorite}
          aria-label={favoriteMark ? '取消追剧收藏' : '加入追剧收藏'}
          aria-pressed={Boolean(favoriteMark)}
          title={favoriteMark ? '取消追剧收藏' : '加入追剧收藏'}
        >
          <span className="relative">
            <Bookmark className="h-7 w-7" fill={favoriteMark ? 'currentColor' : 'none'} aria-hidden="true" />
            {favoriteMark && (
              <Check className="absolute -right-1 -top-1 h-3.5 w-3.5 rounded-full bg-emerald-500 p-0.5 text-white" aria-hidden="true" />
            )}
          </span>
          <span className="text-[11px] font-semibold">{favoriteMark ? '已追剧' : '追剧'}</span>
        </button>

        <button
          ref={detailButtonRef}
          type="button"
          className={ACTION_CLASS}
          onClick={() => (detailOpen ? closeDetail(false) : setDetailOpen(true))}
          disabled={!canOpenDetail}
          aria-expanded={detailOpen}
          aria-controls={detailOpen ? panelId : undefined}
          aria-label="剧集详细信息"
          title="剧集详细信息"
        >
          <Info className="h-7 w-7" aria-hidden="true" />
          <span className="text-[11px] font-semibold">详细</span>
        </button>
      </div>

      <button
        type="button"
        onClick={onClose}
        className="pointer-events-auto absolute left-4 top-4 z-50 flex h-9 w-9 items-center justify-center rounded-full bg-black/30 text-white/90 backdrop-blur-md transition-colors hover:bg-black/50 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90"
        aria-label="返回短剧精选"
        title="返回短剧精选"
      >
        <ArrowLeft className="h-4.5 w-4.5" aria-hidden="true" />
      </button>

      {detailOpen && (
        <>
          {/* 遮罩挂在 FeedView 自己的层里（z 只在本层比较），不会盖住应用外壳。
              点击即关，且带 `data-feed-no-swipe`：遮罩上的滚轮不该翻集。 */}
          <div
            className="pointer-events-auto absolute inset-0 z-[60] bg-black/50"
            onClick={() => closeDetail()}
            aria-hidden="true"
            data-feed-no-swipe
          />
          <section
            ref={panelRef}
            id={panelId}
            role="dialog"
            // 非模态：播放不停、画面不暂停，用户只是想看一眼信息。
            aria-modal="false"
            aria-labelledby={titleId}
            tabIndex={-1}
            data-feed-no-swipe
            className="pointer-events-auto absolute inset-x-3 bottom-16 z-[61] max-h-[72vh] overflow-y-auto rounded-2xl border border-white/15 bg-slate-950/90 p-5 text-white shadow-fluent backdrop-blur-md animate-fluent-page-in focus:outline-none sm:inset-x-auto sm:right-5 sm:bottom-16 sm:w-[24rem]"
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <h2 id={titleId} className="line-clamp-2 text-base font-bold leading-6">{title}</h2>
                <p className="mt-1 text-xs text-white/75">
                  第 {episodeNumber} 集 · 共 {totalEpisodes} 集
                  {boundary ? ` · ${boundary}` : ''}
                </p>
              </div>
              <button
                type="button"
                onClick={() => closeDetail()}
                className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg text-white/80 transition-colors hover:bg-white/15 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 focus-visible:ring-offset-2 focus-visible:ring-offset-black/60"
                aria-label="关闭详细信息"
                title="关闭详细信息"
              >
                <X className="h-4 w-4" aria-hidden="true" />
              </button>
            </div>

            {/* 简介在这里**不折叠**：用户主动打开面板就是为了看全文，
                再折一次等于让他点两遍。 */}
            {description && (
              <p className="mt-3 whitespace-pre-line text-xs leading-5 text-white/85">{description}</p>
            )}

            {tags.length > 0 && (
              <div className="mt-3 flex flex-wrap gap-1.5">
                {tags.map(tag => (
                  <span
                    key={tag}
                    className="rounded-lg bg-white/10 px-2 py-0.5 text-[11px] font-medium text-white/80"
                  >
                    {tag}
                  </span>
                ))}
              </div>
            )}

            {origin && (
              <p className="mt-3 text-[11px] text-white/60">来源：{origin}</p>
            )}

            {/* 换内容的导航全在这里：与「静音/收藏/详细」分开，误触代价不同。 */}
            <div className="mt-4 flex items-center gap-2">
              <button
                type="button"
                className={PANEL_ACTION}
                onClick={() => { closeDetail(false); onPrevious(); }}
                disabled={busy || episodeIndex <= 0}
                aria-label="上一集"
              >
                <ChevronDown className="h-4 w-4" aria-hidden="true" />
                上一集
              </button>
              <button
                type="button"
                className={PANEL_ACTION}
                onClick={() => { closeDetail(false); onNext(); }}
                disabled={!canGoNext}
                aria-label="下一集"
              >
                <ChevronUp className="h-4 w-4" aria-hidden="true" />
                {busy ? <LoaderCircle className="h-4 w-4 animate-spin" aria-hidden="true" /> : null}
                下一集
              </button>
              <button
                type="button"
                className={PANEL_ACTION}
                onClick={() => { closeDetail(false); onSwitchSeries(); }}
                disabled={busy || !canSwitchSeries}
                aria-label="换一部短剧"
              >
                {busy ? <LoaderCircle className="h-4 w-4 animate-spin" aria-hidden="true" /> : <Shuffle className="h-4 w-4" aria-hidden="true" />}
                换一部
              </button>
            </div>
          </section>
        </>
      )}
    </>
  );
};