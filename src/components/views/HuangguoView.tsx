import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { appendUniqueSeries, useInfiniteScroll, usePrefetchBackstop } from '../../hooks/useInfiniteScroll';
import { ArrowLeft, CheckCircle2, Clock, History, Loader2, Play, RefreshCw, Search, Trash2, X } from 'lucide-react';
import { useAppStore } from '../../stores/useAppStore';
import { useHistoryStore } from '../../stores/useHistoryStore';
import { usePlaybackActions } from '../../stores/usePlaybackStore';
import { useSettingsStore } from '../../stores/useSettingsStore';
import { FluentButton } from '../common/FluentButton';
import { MicaCard } from '../common/MicaCard';
import { CoverImage } from '../common/CoverImage';
import { SeriesCard, SERIES_GRID_CLASS } from '../common/SeriesCard';
import { coverRawUsable, coverResolver, ipcService } from '../../services/ipc';
import { historyKey, isHuangguoHistory, normalizeHistoryType } from '../../services/historyTypes';
import { tryLaunchNativePlayer } from '../../services/preferredPlayback';
import { NATIVE_PLAYER_AVAILABLE } from '../../services/nativePlayerRollout';
import {
  NATIVE_OPEN_FAILED_NOTICE,
  NATIVE_SKIPPED_NOTICE,
  planPlaybackRoute,
} from '../../services/playbackRouting';
import { useHuangguoGate } from '../huangguo/HuangguoGate';
import type { SeriesItem } from '../../types/catalog';

/**
 * 黄果短剧专区。
 *
 * 入口是标题栏品牌区连点七次（见 `services/sevenClick.ts`），进出动画由
 * `HuangguoGate` 的 Portal 遮罩负责。本页**不进导航栏、不进首页聚合、不进全局搜索**。
 *
 * ## 频道状态刻意留在本页，不进 useAppStore
 *
 * 发现页的 `channel` 是全局的，而 `ExploreView` 用 `CHANNEL_COPY[channel]` 直接
 * 取文案（没有兜底）。把 `huangguo` 塞进那个全局字段，只要有任何一条路径让发现页
 * 读到它，就会是一个 `undefined.noun` 崩溃。本页自己持有频道与关键词，
 * 全局 channel 永远看不到 `huangguo`。
 *
 * ## 取数只在页面真的可见时发生
 *
 * 视图是常驻 DOM + display 切换。若在挂载时就取数：① 应用一启动就会替一个隐藏
 * 专区打网络请求；② 骨架屏在 `display:none` 的祖先里创建，动画会永久卡在 0% 帧
 * （项目已记录过的坑）。所以首屏取数由 `currentView === 'huangguo'` 触发。
 *
 * ## 卡片不传 onUnavailable
 *
 * 封面拿不到时保留首字占位，而不是把整张卡移出列表。这个源要走代理，
 * 一次网络抖动不该让整页变空——用户会以为专区坏了。
 */

const CHANNELS = ['全部', 'AI成人短剧', 'AI成人漫剧', 'AI换脸', 'AI魔改'] as const;
const PAGE_SIZE = 24;
const FEED_CACHE_TTL_MS = 3 * 60_000;
const FEED_CACHE_MAX = 12;
const huangguoFeedCache = new Map<string, { at: number; feed: FeedState }>();

function readFeedCache(signature: string): FeedState | null {
  const entry = huangguoFeedCache.get(signature);
  if (!entry) return null;
  if (Date.now() - entry.at >= FEED_CACHE_TTL_MS) {
    huangguoFeedCache.delete(signature);
    return null;
  }
  return entry.feed;
}

function writeFeedCache(signature: string, feed: FeedState): void {
  if (feed.items.length === 0 || feed.loading || feed.error) return;
  if (huangguoFeedCache.size >= FEED_CACHE_MAX) {
    const oldest = huangguoFeedCache.keys().next().value;
    if (oldest !== undefined) huangguoFeedCache.delete(oldest);
  }
  huangguoFeedCache.set(signature, { at: Date.now(), feed });
}

interface FeedState {
  items: SeriesItem[];
  page: number;
  pagesLoaded: number;
  emptyPages: number;
  hasMore: boolean;
  total: number;
  loading: boolean;
  loadingMore: boolean;
  error: string | null;
  loadMoreError: string | null;
}

const EMPTY_FEED: FeedState = {
  items: [], page: 0, pagesLoaded: 0, emptyPages: 0, hasMore: false, total: 0, loading: false, loadingMore: false, error: null, loadMoreError: null,
};

const GridSkeleton: React.FC = () => (
  <div className={SERIES_GRID_CLASS}>
    {Array.from({ length: 12 }, (_, index) => (
      <div key={index} className="flex flex-col gap-2">
        <div className="aspect-[3/4] w-full rounded-xl bg-slate-200/70 animate-pulse" />
        <div className="h-3 w-4/5 rounded bg-slate-200/70 animate-pulse" />
        <div className="h-2.5 w-2/5 rounded bg-slate-200/60 animate-pulse" />
      </div>
    ))}
  </div>
);

function formatDuration(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}分${s}秒`;
}

function formatRelativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  const hours = Math.floor(diff / (3600 * 1000));
  if (hours < 1) return '刚刚观看';
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  return `${days} 天前`;
}

export const HuangguoView: React.FC = () => {
  const { currentView, navigateTo, showToast } = useAppStore();
  const { records, loadHistory, removeRecord, clearHistory } = useHistoryStore();
  const { openEpisode } = usePlaybackActions();
  const { settings, updateSettings } = useSettingsStore();
  const gate = useHuangguoGate();
  const active = currentView === 'huangguo';

  const [channel, setChannel] = useState<string>('全部');
  const [keyword, setKeyword] = useState('');
  const [draft, setDraft] = useState('');
  const [feed, setFeed] = useState<FeedState>(EMPTY_FEED);
  const [showHistory, setShowHistory] = useState(false);
  const [confirmClearHistory, setConfirmClearHistory] = useState(false);

  const scrollRef = useRef<HTMLDivElement>(null);
  const sentinelRef = useRef<HTMLDivElement>(null);
  /** 请求代际：换频道/换关键词后，旧请求回来也不许写进状态。 */
  const generation = useRef(0);
  /** 翻页并发锁：防止 observer 与回退兜底同时触发同一页。 */
  const inFlightRef = useRef(false);
  /** 已取过的「频道+关键词」组合。切走再回来时不重复取数、不清空列表。 */
  const loadedSignature = useRef<string | null>(null);

  const load = useCallback(async (page: number, append: boolean) => {
    if (append && inFlightRef.current) return;
    const ticket = generation.current;
    if (append) {
      inFlightRef.current = true;
    } else {
      inFlightRef.current = false;
    }
    setFeed(previous => ({
      ...previous,
      loading: !append,
      loadingMore: append,
      error: append ? previous.error : null,
      loadMoreError: null,
    }));
    try {
      const result = await ipcService.catalog.list({
        channel: 'huangguo',
        category: channel,
        audience: '全部',
        // 站点没有排序口径，如实固定一档（后端忽略它）。
        sort: 'recommend',
        keyword: keyword || undefined,
        page,
        pageSize: PAGE_SIZE,
      });
      if (ticket !== generation.current) return;
      void ipcService.huangguo.coverPrefetch(result.items.map(item => item.cover));
      setFeed(previous => {
        const merged = append ? appendUniqueSeries(previous.items, result.items) : null;
        const items = merged ? merged.items : result.items;
        const emptyPages = append
          ? (merged!.addedCount === 0 ? previous.emptyPages + 1 : 0)
          : 0;
        const nextFeed: FeedState = {
          items,
          page,
          pagesLoaded: append ? previous.pagesLoaded + 1 : 1,
          emptyPages,
          hasMore: result.hasMore && emptyPages < 3,
          total: result.total,
          loading: false,
          loadingMore: false,
          error: null,
          loadMoreError: null,
        };
        writeFeedCache(`${channel}|${keyword}`, nextFeed);
        return nextFeed;
      });
    } catch (error) {
      if (ticket !== generation.current) return;
      const message = error instanceof Error ? error.message : String(error);
      setFeed(previous => ({
        ...previous,
        loading: false,
        loadingMore: false,
        error: append ? previous.error : message,
        loadMoreError: append ? message : null,
      }));
    } finally {
      if (ticket === generation.current) {
        inFlightRef.current = false;
      }
    }
  }, [channel, keyword]);

  const signature = `${channel}|${keyword}`;

  // 首屏取数只在「页面可见」且「频道/关键词真的换了」时发生。
  // 切走再回来不重复取数：列表与滚动位置都留着，退出动画回来时不会闪一下空白。
  useEffect(() => {
    if (!active) return;
    if (loadedSignature.current === signature) return;
    loadedSignature.current = signature;
    generation.current += 1;
    scrollRef.current?.scrollTo({ top: 0 });
    const cached = readFeedCache(signature);
    if (cached) {
      void ipcService.huangguo.coverPrefetch(cached.items.map(item => item.cover));
      setFeed(cached);
      return;
    }
    setFeed(EMPTY_FEED);
    void load(1, false);
  }, [active, signature, load]);

  const reload = useCallback(() => {
    huangguoFeedCache.delete(signature);
    generation.current += 1;
    setFeed(EMPTY_FEED);
    void load(1, false);
  }, [load, signature]);

  const loadMore = useCallback(() => {
    if (!active || !feed.hasMore || feed.loading || feed.loadingMore || feed.loadMoreError) return;
    void load(feed.page + 1, true);
  }, [active, feed.hasMore, feed.loading, feed.loadingMore, feed.loadMoreError, feed.page, load]);

  const infiniteEnabled = active && feed.hasMore && !feed.loading && !feed.loadMoreError;

  // 无限滚动：哨兵提前 1800px 进视口就取下一页，并由几何兜底防止短暂响应停在底部。
  useInfiniteScroll({
    scrollRef,
    sentinelRef,
    enabled: infiniteEnabled && !feed.loadingMore,
    rootMargin: '1800px 0px',
    onLoadMore: loadMore,
  });

  usePrefetchBackstop({
    scrollRef,
    sentinelRef,
    enabled: infiniteEnabled,
    isLoadingMore: feed.loadingMore,
    progress: feed.pagesLoaded,
    bufferPx: 1800,
    onLoadMore: loadMore,
  });

  const handleClick = useCallback((seriesId: string) => navigateTo('detail', seriesId), [navigateTo]);
  const handlePrefetch = useCallback((seriesId: string) => {
    void ipcService.series.getDetail(seriesId).catch(() => {});
  }, []);

  const submitSearch = useCallback((event: React.FormEvent) => {
    event.preventDefault();
    if (showHistory) return;
    setKeyword(draft.trim());
  }, [draft, showHistory]);

  useEffect(() => {
    if (!active) return;
    void loadHistory();
  }, [active, loadHistory]);

  const huangguoHistory = useMemo(
    () => records.filter(item => isHuangguoHistory(item)),
    [records],
  );

  const visibleHistory = useMemo(() => {
    const q = draft.trim().toLowerCase();
    return q ? huangguoHistory.filter(item => item.title.toLowerCase().includes(q)) : huangguoHistory;
  }, [huangguoHistory, draft]);

  const handleResumeHistory = useCallback((
    seriesId: string,
    episodeId: string,
    position: number,
    title?: string,
    episodeNumber?: number,
    historyType?: 'watch' | 'recommendation' | 'featured',
  ) => {
    void (async () => {
      const route = planPlaybackRoute({
        preferredPlayer: settings.preferredPlayer,
        nativeAvailable: NATIVE_PLAYER_AVAILABLE,
        seriesId,
        episodeId,
        channel: 'huangguo',
        surface: 'history',
      });
      if (route.kernel === 'native') {
        try {
          const launched = await tryLaunchNativePlayer({
            preferredPlayer: settings.preferredPlayer,
            autoNext: settings.autoNext,
            setAutoNext: on => { void updateSettings({ autoNext: on }); },
            surface: 'history',
            seriesId,
            episodeId,
            startSeconds: position,
            title,
            episodeNumber,
            channel: 'huangguo',
            historyType,
          });
          if (launched) return;
          showToast(NATIVE_SKIPPED_NOTICE, 'warning');
        } catch (error) {
          console.warn('MPV 播放器打开失败，回退网页播放', error);
          showToast(NATIVE_OPEN_FAILED_NOTICE, 'error');
        }
      } else if (route.notice) {
        showToast(route.notice, 'warning');
      }
      navigateTo('player', seriesId);
      openEpisode(seriesId, episodeId, position, undefined, undefined, historyType);
    })();
  }, [navigateTo, openEpisode, settings.autoNext, settings.preferredPlayer, showToast, updateSettings]);

  const handleClearHuangguoHistory = useCallback(async () => {
    await clearHistory('huangguo');
    setConfirmClearHistory(false);
    showToast('黄果专区观看历史已清空', 'info');
  }, [clearHistory, showToast]);

  const subtitle = useMemo(() => {
    if (showHistory) return `黄果专属观看历史 · 共 ${huangguoHistory.length} 部`;
    if (keyword) return `搜索「${keyword}」· 共 ${feed.total} 部`;
    if (feed.total > 0) return `${channel === '全部' ? '全部频道' : channel} · 共 ${feed.total} 部`;
    return '独立数据源 · 与首页目录不共享链路';
  }, [showHistory, huangguoHistory.length, keyword, channel, feed.total]);

  return (
    <div className="flex h-full w-full min-w-0 flex-col overflow-hidden">
      <header className="flex shrink-0 flex-col gap-3 border-b border-slate-200/60 px-6 py-4">
        <div className="flex items-center gap-3">
          <button
            type="button"
            onClick={gate.exit}
            title="退出专区"
            aria-label="退出专区"
            className="flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-lg text-slate-500 transition-colors hover:bg-white hover:text-slate-900"
          >
            <ArrowLeft className="h-4 w-4" />
          </button>
          <div className="min-w-0 flex-1">
            <h1 className="flex items-center gap-2 text-lg font-extrabold tracking-tight text-slate-900">
              黄果短剧专区
              <span className="rounded-md bg-amber-100 px-1.5 py-0.5 text-[10px] font-bold text-amber-700">
                独立源
              </span>
            </h1>
            <p className="mt-0.5 truncate text-[11px] text-slate-400">{subtitle}</p>
          </div>
          <form onSubmit={submitSearch} className="relative shrink-0">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-slate-400" />
            <input
              value={draft}
              onChange={event => setDraft(event.target.value)}
              placeholder={showHistory ? '筛选黄果观看历史' : '搜索专区内容'}
              aria-label={showHistory ? '筛选黄果观看历史' : '搜索专区内容'}
              className="h-8 w-52 rounded-lg border border-slate-200/70 bg-white/80 pl-8 pr-7 text-[12px] text-slate-700 outline-none transition-colors placeholder:text-slate-400 focus:border-amber-300 focus:bg-white"
            />
            {draft && (
              <button
                type="button"
                onClick={() => {
                  setDraft('');
                  if (!showHistory) setKeyword('');
                }}
                aria-label="清空搜索"
                className="absolute right-1.5 top-1/2 flex h-5 w-5 -translate-y-1/2 cursor-pointer items-center justify-center rounded text-slate-400 hover:bg-slate-100 hover:text-slate-700"
              >
                <X className="h-3 w-3" />
              </button>
            )}
          </form>
          <button
            type="button"
            onClick={() => (showHistory ? void loadHistory() : reload())}
            title="刷新"
            aria-label="刷新"
            className="flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-lg text-slate-500 transition-colors hover:bg-white hover:text-amber-600"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${!showHistory && feed.loading ? 'animate-spin' : ''}`} />
          </button>
        </div>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-1 rounded-xl border border-slate-200/70 bg-slate-100/90 p-1">
            {CHANNELS.map(item => (
              <button
                key={item}
                type="button"
                onClick={() => {
                  setChannel(item);
                  setShowHistory(false);
                  setConfirmClearHistory(false);
                }}
                className={`rounded-lg px-2.5 py-1 text-[11px] transition-colors cursor-pointer ${
                  !showHistory && item === channel
                    ? 'fluent-convex-tab font-bold text-amber-700'
                    : 'font-medium text-slate-500 hover:bg-white/60 hover:text-slate-900'
                }`}
              >
                {item}
              </button>
            ))}
          </div>

          <button
            type="button"
            onClick={() => {
              setShowHistory(prev => !prev);
              setConfirmClearHistory(false);
              void loadHistory();
            }}
            className={`inline-flex items-center gap-1.5 rounded-xl border px-3 py-1.5 text-[11px] font-bold transition-colors cursor-pointer ${
              showHistory
                ? 'border-amber-300 bg-amber-50 text-amber-700 shadow-xs'
                : 'border-slate-200/70 bg-slate-100/90 text-slate-600 hover:bg-white hover:text-amber-700'
            }`}
          >
            <History className="h-3.5 w-3.5" />
            <span>观看历史</span>
            <span
              className={`rounded-md px-1.5 py-0.5 text-[10px] font-bold ${
                showHistory ? 'bg-amber-200/70 text-amber-800' : 'bg-slate-200/80 text-slate-500'
              }`}
            >
              {huangguoHistory.length}
            </span>
          </button>
        </div>
      </header>

      <div ref={scrollRef} className="min-h-0 min-w-0 flex-1 overflow-x-hidden overflow-y-auto px-4 py-5">
        {showHistory ? (
          <div className="mx-auto flex max-w-4xl flex-col gap-4">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2 text-xs text-slate-500">
                <History className="h-3.5 w-3.5 text-amber-600" />
                <span>黄果专区专属观看历史（不与主观看历史混存）</span>
              </div>
              {huangguoHistory.length > 0 && (
                confirmClearHistory ? (
                  <div className="inline-flex items-center gap-1.5 rounded-xl border border-rose-200/80 bg-rose-50/70 px-2 py-1">
                    <span className="text-[11px] font-medium text-rose-700">确认清空黄果历史？</span>
                    <FluentButton
                      variant="danger"
                      size="sm"
                      onClick={() => void handleClearHuangguoHistory()}
                    >
                      确认
                    </FluentButton>
                    <FluentButton
                      variant="secondary"
                      size="sm"
                      onClick={() => setConfirmClearHistory(false)}
                    >
                      取消
                    </FluentButton>
                  </div>
                ) : (
                  <FluentButton
                    variant="danger"
                    size="sm"
                    icon={<Trash2 className="h-3.5 w-3.5" />}
                    onClick={() => setConfirmClearHistory(true)}
                  >
                    清空黄果历史
                  </FluentButton>
                )
              )}
            </div>

            {visibleHistory.length === 0 ? (
              <div className="flex flex-col items-center justify-center gap-2.5 py-24 text-center">
                <History className="h-10 w-10 text-amber-300" />
                <p className="text-sm font-semibold text-slate-600">
                  {draft.trim() ? `没有匹配「${draft.trim()}」的黄果观看记录` : '暂无黄果专区观看历史'}
                </p>
                <p className="text-xs text-slate-400">在黄果短剧专区播放过的剧集进度会独立记录在这里</p>
                <FluentButton size="sm" onClick={() => setShowHistory(false)}>返回频道内容</FluentButton>
              </div>
            ) : (
              <div className="flex flex-col gap-3">
                {visibleHistory.map(item => {
                  const itemHistoryType = normalizeHistoryType(item.historyType);
                  return (
                    <MicaCard
                      key={historyKey(item)}
                      hoverable
                      className="p-4 flex items-center justify-between gap-4 group"
                    >
                      <div className="flex items-center gap-4 min-w-0">
                        <div
                          onClick={() => handleResumeHistory(item.seriesId, item.episodeId, item.positionSeconds, item.title, item.episodeNumber, itemHistoryType)}
                          className="relative w-16 h-[88px] rounded-xl overflow-hidden shadow-sm flex-shrink-0 cursor-pointer group-hover:scale-105 transition-transform duration-300"
                        >
                          <CoverImage
                            src={item.seriesCover}
                            title={item.title}
                            fallbackChar="黄"
                            placeholderTextClassName="text-base"
                            loading="eager"
                            resolveSrc={coverResolver(item.seriesId, item.seriesCover)}
                            rawUsable={coverRawUsable(item.seriesId)}
                          />
                          <div className="absolute inset-0 bg-black/20 group-hover:bg-black/40 flex items-center justify-center transition-colors">
                            <Play className="w-5 h-5 text-white fill-current opacity-80 group-hover:opacity-100 transform group-hover:scale-110 transition-transform" />
                          </div>
                        </div>

                        <div className="flex flex-col gap-1.5 min-w-0">
                          <div className="flex items-center gap-2 flex-wrap">
                            <h3
                              onClick={() => navigateTo('detail', item.seriesId)}
                              className="text-sm font-bold text-slate-900 truncate hover:text-amber-600 cursor-pointer transition-colors"
                            >
                              {item.title}
                            </h3>
                            <span className="text-[10px] font-bold px-1.5 py-0.5 rounded bg-amber-50 text-amber-700 border border-amber-200/60">
                              黄果专区
                            </span>
                            {item.isFinished ? (
                              <span className="text-[10px] font-bold px-1.5 py-0.5 rounded bg-emerald-50 text-emerald-600 border border-emerald-200/60 flex items-center gap-1">
                                <CheckCircle2 className="w-2.5 h-2.5" /> 已看完
                              </span>
                            ) : (
                              <span className="text-[10px] font-bold px-1.5 py-0.5 rounded bg-slate-100 text-slate-700 border border-slate-200/70">
                                看到第 {item.episodeNumber} 集
                              </span>
                            )}
                          </div>

                          <div className="flex items-center gap-3 text-xs text-slate-400">
                            <span>
                              进度：
                              {item.durationSeconds > 0
                                ? `${formatDuration(item.positionSeconds)} / ${formatDuration(item.durationSeconds)}`
                                : item.positionSeconds > 0
                                  ? `已看 ${formatDuration(item.positionSeconds)}`
                                  : '尚未开始播放'}
                            </span>
                            <span>·</span>
                            <span className="flex items-center gap-1">
                              <Clock className="w-3 h-3" />
                              {formatRelativeTime(item.updatedAt)}
                            </span>
                          </div>

                          <div className="w-48 h-1.5 bg-slate-200/70 rounded-full overflow-hidden mt-0.5">
                            <div
                              className="h-full bg-amber-500 rounded-full"
                              style={{ width: `${item.progressPercent}%` }}
                            />
                          </div>
                        </div>
                      </div>

                      <div className="p-1 bg-slate-100/80 rounded-xl border border-slate-200/60 shadow-inner flex items-center gap-1.5 flex-shrink-0">
                        <FluentButton
                          variant="primary"
                          size="sm"
                          icon={<Play className="w-3 h-3 fill-current" />}
                          onClick={() => handleResumeHistory(item.seriesId, item.episodeId, item.positionSeconds, item.title, item.episodeNumber, itemHistoryType)}
                          className="shadow-sm"
                        >
                          继续播放
                        </FluentButton>

                        <button
                          type="button"
                          onClick={() => void removeRecord(item.seriesId, itemHistoryType)}
                          className="p-1.5 rounded-lg text-slate-400 hover:text-rose-600 hover:bg-white/80 transition-colors cursor-pointer"
                          title="移除此条记录"
                        >
                          <Trash2 className="w-4 h-4" />
                        </button>
                      </div>
                    </MicaCard>
                  );
                })}
              </div>
            )}
          </div>
        ) : feed.loading && feed.items.length === 0 ? (
          <GridSkeleton />
        ) : feed.error && feed.items.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-3 py-24 text-center">
            <p className="text-sm font-semibold text-slate-600">专区内容加载失败</p>
            <p className="max-w-lg break-words text-xs leading-relaxed text-slate-400">{feed.error}</p>
            <p className="max-w-lg text-[11px] leading-relaxed text-slate-400">
              这个源在境外，需要系统代理或本机代理才能访问。
            </p>
            <FluentButton size="sm" onClick={reload}>重试</FluentButton>
          </div>
        ) : feed.items.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-2 py-24 text-center">
            <p className="text-sm font-semibold text-slate-600">
              {keyword ? `没有找到匹配「${keyword}」的内容` : '这个频道暂时没有内容'}
            </p>
            <p className="text-xs text-slate-400">换个频道，或稍后刷新试试</p>
          </div>
        ) : (
          <>
            <div className={SERIES_GRID_CLASS}>
              {feed.items.map((series, index) => (
                <SeriesCard
                  key={series.id}
                  series={series}
                  index={index}
                  onClick={handleClick}
                  onPrefetch={handlePrefetch}
                />
              ))}
            </div>
            <div ref={sentinelRef} className="flex h-14 items-center justify-center">
              {feed.loadingMore && (
                <span className="flex items-center gap-1.5 text-[11px] font-semibold text-slate-400">
                  <Loader2 className="h-3 w-3 animate-spin" />
                  正在加载更多…
                </span>
              )}
              {feed.loadMoreError && !feed.loadingMore && (
                <button
                  type="button"
                  onClick={() => void load(feed.page + 1, true)}
                  className="text-[11px] font-medium text-rose-500 hover:text-rose-600 transition-colors"
                >
                  加载更多失败（{feed.loadMoreError}）· 点击重试
                </button>
              )}
              {!feed.hasMore && !feed.loadMoreError && feed.items.length > 0 && (
                <span className="text-[11px] text-slate-300">已经到底了 · 共 {feed.items.length} 部</span>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
};