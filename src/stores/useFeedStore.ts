import { useCallback, useEffect, useRef, useState } from 'react';
import { errorText, ipcService, isTauriEnvironment } from '../services/ipc';
import {
  dedupeFeedItems,
  detailChannelForFeed,
  feedChannelCopy,
  feedPageSource,
  feedDetailFromHongguo,
  feedDetailFromMock,
  fetchAiFeedPage,
  fetchHongguoFeedPage,
  mockFeedItems,
  normalizeFeedChannel,
  type FeedChannel,
  type FeedChannelCopy,
  type FeedDetail,
} from '../services/feedSource';
import { contentTypeFor } from '../services/contentType';
import { buildInterest, createTagResolver, historyCompletion, TAG_FETCH_LIMIT } from '../services/feedRank';
import { createSeriesPrefetcher } from '../services/feedPrefetch';
import { tracePlayback } from '../services/playbackTrace';
import {
  createCandidateCache,
  mergeFeedPage,
  orderFeedTail,
  shouldPrefetchNextPage,
  type FeedPageSlice,
} from '../services/feedQueue';
import {
  feedEpisodeStep,
  type FeedSeriesDirection,
} from '../services/feedNavigation';
import { runSeriesNav } from '../services/feedSeriesNav';
import type { SeriesItem } from '../types/catalog';
import type { FavoriteMark } from '../types/favorite';
import { useAppStore } from './useAppStore';
import { useFavoriteActions, useFavoriteMark } from './useFavoritesStore';
import { usePlaybackActions } from './usePlaybackStore';
import { useSettingsStore } from './useSettingsStore';
import { playbackNoticeFor, planPlaybackRoute } from '../services/playbackRouting';
import { NATIVE_PLAYER_AVAILABLE } from '../services/nativePlayerRollout';

interface FeedState {
  channel: FeedChannel;
  /**
   * 这一页的数据来源说明。
   *
   * 文案作为 hook 返回值的一部分而不是写死在组件里：AI 剧那条是站方热度顺序，
   * 不是个性化推荐，凡是说"为你推荐"的地方都要能按频道换掉这句话。
   */
  channelCopy: FeedChannelCopy;
  items: SeriesItem[];
  currentIndex: number;
  currentItem: SeriesItem | null;
  detail: FeedDetail | null;
  episodeIndex: number;
  isLoading: boolean;
  isOpening: boolean;
  isLoadingMore: boolean;
  hasMore: boolean;
  isCompleted: boolean;
  favoriteMark: FavoriteMark | undefined;
  error: string | null;
  selectChannel: (channel: FeedChannel) => void;
  /** 上一集（仅本剧内；首集时为 no-op）。 */
  previousEpisode: () => void;
  /** 下一集（仅本剧内；末集 / 可播边界处为 no-op，不跨季也不换部）。 */
  nextEpisode: () => void;
  /**
   * 换一部推荐剧。
   *
   * 方向可选：`'next'`（默认）与 `'previous'`。**首部剧不再有"上一部"**，此时
   * 调用是 no-op；末部且没有更多分页时由 store 给出"浏览完"提示。
   * 无论哪个方向，落点都是新剧的**首集 0 秒**。
   */
  switchSeries: (direction?: FeedSeriesDirection) => void;
  /** 悬停预热指定短剧的详情与首集媒体（失败静默）。 */
  warmSeries: (seriesId: string) => void;
  retry: () => void;
  close: () => void;
  toggleFavorite: () => void;
  /**
   * 当前剧的最后一集正常播完。
   *
   * 停留 = 置 `isCompleted`（状态横幅显示"当前短剧已播放完成"）。**不**去找下一季、
   * **不**去换下一部：自动跨内容会让用户"看着看着剧被换掉了"。
   */
  advanceAfterFinished: (sessionId: number) => void;
}

/**
 * 补页失败的唯一文案。
 *
 * 提成常量是为了让"清遗留分页错误"能精确比对：只撤这一条，
 * 不误清真实的播放/详情错误。
 */
const FEED_PAGE_ERROR = '更多推荐加载失败';

const WARM_DETAIL_TTL_MS = 5 * 60_000;
const warmedCandidateCache = createCandidateCache<FeedPageSlice>();
const warmedPageInflight = new Map<FeedChannel, Promise<FeedPageSlice>>();
const warmedDetailCache = new Map<string, { detail: FeedDetail; at: number }>();
const warmedDetailInflight = new Map<string, Promise<FeedDetail | null>>();
const warmedStreamSet = new Set<string>();

async function fetchFeedPageForChannel(
  activeChannel: FeedChannel,
  cursor?: string | null,
): Promise<FeedPageSlice> {
  if (!isTauriEnvironment()) {
    return { items: cursor ? [] : mockFeedItems(activeChannel), hasMore: false, cursor: null };
  }
  if (feedPageSource(activeChannel) === 'web-catalog') {
    const page = await fetchAiFeedPage(
      cursor ?? null,
      filter => ipcService.catalog.list(filter),
    );
    return {
      items: dedupeFeedItems(page.items, activeChannel),
      hasMore: page.hasMore,
      cursor: page.cursor,
    };
  }
  const page = await fetchHongguoFeedPage({
    channel: activeChannel,
    cursor: cursor ?? null,
    loaders: {
      shortFilter: input => ipcService.hongguo.shortFilter(input),
      discover: (discoverChannel, token) => ipcService.hongguo.discover(discoverChannel, token),
    },
  });
  return {
    items: dedupeFeedItems(page.items, activeChannel),
    hasMore: page.hasMore,
    cursor: page.cursor,
  };
}

function readWarmedDetail(seriesId: string, channel: FeedChannel): FeedDetail | null {
  const key = `${channel}:${seriesId}`;
  const entry = warmedDetailCache.get(key);
  if (!entry) return null;
  if (Date.now() - entry.at >= WARM_DETAIL_TTL_MS) {
    warmedDetailCache.delete(key);
    return null;
  }
  return entry.detail;
}

function ensureWarmedFirstPage(channel: FeedChannel): Promise<FeedPageSlice> {
  const cached = warmedCandidateCache.read(channel);
  if (cached && cached.items.length > 0) return Promise.resolve(cached);
  const running = warmedPageInflight.get(channel);
  if (running) return running;
  const promise = fetchFeedPageForChannel(channel)
    .then(page => {
      if (page.items.length > 0) {
        warmedCandidateCache.write(channel, {
          items: page.items,
          cursor: page.cursor,
          hasMore: Boolean(page.hasMore && page.cursor),
        });
      }
      return page;
    })
    .finally(() => {
      if (warmedPageInflight.get(channel) === promise) {
        warmedPageInflight.delete(channel);
      }
    });
  warmedPageInflight.set(channel, promise);
  return promise;
}

function ensureWarmedDetail(seriesId: string, channel: FeedChannel): Promise<FeedDetail | null> {
  const cached = readWarmedDetail(seriesId, channel);
  if (cached) return Promise.resolve(cached);
  const key = `${channel}:${seriesId}`;
  const running = warmedDetailInflight.get(key);
  if (running) return running;
  const promise = ipcService.hongguo
    .seriesDetail(seriesId, detailChannelForFeed(channel))
    .then(raw => {
      const detail = feedDetailFromHongguo(raw);
      if (!detail || detail.type !== channel || detail.episodes.length === 0) return null;
      if (warmedDetailCache.size >= 8) {
        const oldest = warmedDetailCache.keys().next().value;
        if (oldest !== undefined) warmedDetailCache.delete(oldest);
      }
      warmedDetailCache.set(key, { detail, at: Date.now() });
      return detail;
    })
    .catch(() => null)
    .finally(() => {
      if (warmedDetailInflight.get(key) === promise) {
        warmedDetailInflight.delete(key);
      }
    });
  warmedDetailInflight.set(key, promise);
  return promise;
}

/**
 * 在发现页空闲或悬停「短剧推荐」时预先拉取首屏列表、首部详情与首集预签名/媒体，
 * 让首次点击推荐页不再串行等待多段冷启动往返。失败一律静默忽略。
 */
export async function warmFeedEntry(options?: {
  channel?: FeedChannel;
  prewarmEpisode?: (
    seriesId: string,
    episodeId: string,
    contentType?: number,
    signal?: AbortSignal,
    bypassStreamingWindow?: boolean,
  ) => Promise<boolean>;
  prewarmMedia?: boolean;
}): Promise<void> {
  if (!isTauriEnvironment()) return;
  const channel = options?.channel ?? 'drama';
  try {
    const page = await ensureWarmedFirstPage(channel);
    const first = page.items[0];
    if (!first) return;
    const detail = await ensureWarmedDetail(first.id, channel);
    const firstEp = detail?.episodes[0];
    if (!firstEp) return;
    const contentType = contentTypeFor(channel);
    if (!warmedStreamSet.has(firstEp.id)) {
      if (warmedStreamSet.size >= 32) warmedStreamSet.clear();
      warmedStreamSet.add(firstEp.id);
      void ipcService.playback.prefetchStream([firstEp.id], contentType);
    }
    if (options?.prewarmMedia && options.prewarmEpisode) {
      void options.prewarmEpisode(first.id, firstEp.id, contentType, undefined, false);
    }
  } catch {
    // 预热失败不打扰用户，正式进入时按正常链路兜底
  }
}

/**
 * 推荐流轻量阶段日志。
 *
 * 走与播放链路同一条 `tracePlayback`（落进 ttv-playback.log），阶段名取自固定集合，
 * 只额外带毫秒与极少量整数/常量（`来源=缓存`、`追加=6`）。**不写** URL、游标、剧名、
 * 设备号或任何用户数据 —— 这条日志会被用户导出外发。
 *
 * 目的是把"慢在列表、详情还是标签"一次分清楚，而不是再翻整份日志对时间戳。
 */
function traceFeedStage(stage: string, startedAt: number, detail?: string): void {
  const ms = Math.max(0, Math.round(performance.now() - startedAt));
  tracePlayback(`推荐 ${stage} 耗时=${ms}ms${detail ? ` ${detail}` : ''}`);
}

/**
 * 推荐页的**队列**与**本剧集数**状态机。
 *
 * 三种"往后"互相独立，边界完全不同：
 *
 * | 场景 | 谁推进 | 边界行为 |
 * | --- | --- | --- |
 * | 本集播完（连播） | `usePlaybackStore.handleEnded` → `playNextEpisode` | **同一部剧**的下一集；末集置 `finishedAll` |
 * | 「详细」面板的「下一集」 | `nextEpisode` | 只在本剧内；末集 / 可播边界处 no-op |
 * | 方向键 / 滚轮 / 拖滑 / 「换一部」 | `switchSeries` | 换另一部剧，落点**首集 0 秒** |
 *
 * **不读观看历史**：首屏与每次换剧都从首集 0 秒开始，不从历史里"续播到第 8 集"。
 * 历史仍在历史页 / 详情页生效，那条链路走 `usePlaybackStore`，与本文件无关。
 */
export function useFeedStore(): FeedState {
  const { currentView, feedChannel, openFeed, navigateTo, showToast } = useAppStore();
  const { currentSeries, currentEpisode, openEpisode, prewarmEpisode } = usePlaybackActions();
  const { settings } = useSettingsStore();
  const setMark = useFavoriteActions();

  const [items, setItems] = useState<SeriesItem[]>([]);
  const [currentIndex, setCurrentIndex] = useState(0);
  const [currentItem, setCurrentItem] = useState<SeriesItem | null>(null);
  const [detail, setDetail] = useState<FeedDetail | null>(null);
  const [episodeIndex, setEpisodeIndex] = useState(0);
  const [isLoading, setIsLoading] = useState(false);
  const [isOpening, setIsOpening] = useState(false);
  const [isLoadingMore, setIsLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(false);
  const [isCompleted, setIsCompleted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const favoriteMark = useFavoriteMark(currentItem?.id ?? '');

  /*
   * `feedChannel` 是 App 那边的字符串状态，读回来先过一次归一化再当推荐页频道用。
   * 这一步是 AI 剧进流的关口：旧实现 `x === 'comic' ? ... : 'drama'` 会把 'ai'
   * 静默归成 'drama'，于是同一部 AI 剧在收藏里点进来，推荐流按真人剧取数。
   */
  const channel = normalizeFeedChannel(feedChannel);

  const settingsRef = useRef(settings);
  settingsRef.current = settings;
  /** 每个回落原因只解释一次：信息流每次滑片都弹一句就等于没有提示。 */
  const feedRouteNoticeRef = useRef<Set<string>>(new Set());
  const itemsRef = useRef(items);
  const currentIndexRef = useRef(currentIndex);
  const currentItemRef = useRef(currentItem);
  const detailRef = useRef(detail);
  const episodeIndexRef = useRef(episodeIndex);
  const favoriteMarkRef = useRef(favoriteMark);
  const openEpisodeRef = useRef(openEpisode);
  const prewarmEpisodeRef = useRef(prewarmEpisode);
  const neighborWarmTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const neighborWarmAbortRef = useRef<AbortController | null>(null);
  const warmingSeriesIdRef = useRef<string | null>(null);
  const prefetchedStreamVidsRef = useRef<Set<string>>(new Set());
  const cancelNeighborWarm = useCallback((preserveSeriesId?: string) => {
    if (neighborWarmTimerRef.current) {
      clearTimeout(neighborWarmTimerRef.current);
      neighborWarmTimerRef.current = null;
    }
    if (preserveSeriesId && warmingSeriesIdRef.current === preserveSeriesId) {
      return;
    }
    warmingSeriesIdRef.current = null;
    if (neighborWarmAbortRef.current) {
      neighborWarmAbortRef.current.abort();
      neighborWarmAbortRef.current = null;
    }
  }, []);
  const channelRef = useRef<FeedChannel>(channel);
  const cursorRef = useRef<string | null>(null);
  const hasMoreRef = useRef(false);
  const loadingMoreRef = useRef(false);
  const loadingRef = useRef(false);
  const openingRef = useRef(false);
  const generationRef = useRef(0);
  /** 单剧打开序号：快速连续切剧时让旧剧的详情与媒体起播静默作废，不锁死滑动操作。 */
  const seriesSeqRef = useRef(0);
  /**
   * 导航锁：存的是本次导航所属的 `generationRef` 值，`null` = 空闲。
   *
   * 按代数记账而不是布尔值：`loadInitial` 会换代，旧导航的 `finally` 若还按布尔值
   * 清锁，就会把**新代**导航的锁一起放开。
   */
  const navLockRef = useRef<number | null>(null);
  /** 上一次导航因补页失败而没走成：`retry()` 据此重试补页而不是重播本剧。 */
  const pendingPageRef = useRef(false);
  /**
   * 上一次补页**实际写入**的错误文本（`errorText(loadError, …)` 的结果）。
   *
   * 清理时必须按这段文本比对：失败原因可能是 `'HTTP 503'`、`'timeout'` 等任意
   * 文本，拿常量去比较永远不命中（那正是清理失效的原因）。
   */
  const pageErrorRef = useRef<string | null>(null);
  const finishedSessionRef = useRef<number | null>(null);
  /**
   * 本地兴趣表：`题材标签 → 权重`（见 `services/feedRank.ts`）。
   *
   * 只用于**排序还没入队的下一页**，不参与"从第几集起播"。为空时保持源顺序
   * ——没画像就随机打乱，等于每次进推荐页都不一样，且无法解释。
   */
  const interestRef = useRef<Map<string, number>>(new Map());
  /**
   * 历史/收藏条目的标签解析器：一次最多 8 条、并发 2、32 条 LRU。
   *
   * 为什么要单独一层：App 直连的列表卡片**不带题材标签**（`app_search_hit_to_item`
   * 明确留空），标签只在详情里。为了排序去全量拉库或阻塞首集都是错的，所以这里
   * 限量、后台、可失败降级。
   */
  const tagResolverRef = useRef(
    createTagResolver({
      // 走公开详情：`series_detail` 保留真实 tags；App 直连的 `hongguo_series_detail`
      // 用 `app_search_hit_to_item` 映射，tags 恒为空，拿它取标签永远取不到。
      load: async seriesId => {
        const detail = await ipcService.series.getDetail(seriesId);
        return detail.tags ?? [];
      },
    }),
  );
  /** 详情加载的转发 ref：预取器在 `loadDetailFor` 之前创建。 */
  const loadDetailRef = useRef<(seriesId: string, channel: FeedChannel) => Promise<FeedDetail>>(
    () => Promise.reject(new Error('详情链路尚未就绪')),
  );
  const prefetcherRef = useRef(
    createSeriesPrefetcher<FeedDetail>({
      // 与正式起播同一条链路、同一份校验，不做第二套。
      load: (seriesId, channel) => loadDetailRef.current(seriesId, channel as FeedChannel),
    }),
  );

  itemsRef.current = items;
  currentIndexRef.current = currentIndex;
  currentItemRef.current = currentItem;
  detailRef.current = detail;
  episodeIndexRef.current = episodeIndex;
  favoriteMarkRef.current = favoriteMark;
  openEpisodeRef.current = openEpisode;
  prewarmEpisodeRef.current = prewarmEpisode;
  channelRef.current = channel;
  hasMoreRef.current = hasMore;

  /**
   * 已访问高水位：曾经打开过的最大下标。
   *
   * 不能用 `currentIndex` 判断"没访问过"：用户可以往前翻再退回来，后退时
   * 后段其实早已看过，按 `currentIndex` 判就会把看过的剧重排掉。
   */
  const visitedHighWaterRef = useRef(-1);
  /**
   * 首屏候选缓存：只存**原始首屏切片**（卡片 + 原始游标 + hasMore），不存任何媒体产物。
   *
   * 追加页**绝不**回写这里（那会把游标推到第 N 页，与提前翻页的补页任务互相踩）。
   * 5 分钟内复用，过期才重新取；按频道分槽，切频道不清空别人的槽。
   */
  const candidateCacheRef = useRef(createCandidateCache<FeedPageSlice>());
  /**
   * 当前在途的补页任务。
   *
   * 后台提前补页与"用户正好滑到边界"必须复用同一个 Promise：后台还在跑时边界导航
   * 若直接 `return []`，用户看到的是"滑了一下没反应"，得再滑一次才动。
   */
  const pageJobRef = useRef<{ promise: Promise<SeriesItem[]>; foreground: boolean } | null>(null);
  /**
   * `maybePrefetchPage` 的转发 ref。
   *
   * `startPageJob` / `openSeriesAt` 都定义在它之前，而 `const` 箭头函数不提升 ——
   * 直接引用只是"调用时机在渲染之后"这个偶然事实在兜着。用 ref 转发把它变成显式的
   * （与 `scheduleNextEpisodePrewarmRef` 同一套写法）。
   */
  const maybePrefetchPageRef = useRef<(requestId: number) => void>(() => {});

  const setLoading = (value: boolean) => {
    loadingRef.current = value;
    setIsLoading(value);
  };

  /**
   * 写入"当前是哪一部/哪一集"。
   *
   * 没有 season 参数：推荐流不跨季，剧内集号一律以 `detail.episodes` 的下标为准
   * （季剧的 `episodeNumber` 可能不从 1 起，展示交给 `FeedSlide`）。
   */
  const setCurrent = (item: SeriesItem, index: number, nextDetail: FeedDetail | null) => {
    currentIndexRef.current = index;
    currentItemRef.current = item;
    detailRef.current = nextDetail;
    setCurrentIndex(index);
    setCurrentItem(item);
    setDetail(nextDetail);
  };

  /**
   * 取详情（正式起播与预取共用这一条，校验也只有这一份）。
   *
   * 为什么按 id 而不是按卡片：预取手上只有 id；mock 分支的 `feedDetailFromMock`
   * 也只用 id。
   */
  const loadDetailFor = useCallback(async (seriesId: string, channel: FeedChannel): Promise<FeedDetail> => {
    // 捕获当代：await 之后可能已经换频道/离开，旧结果不得写进新的标签缓存。
    const requestId = generationRef.current;
    const result = isTauriEnvironment()
      ? feedDetailFromHongguo(await ipcService.hongguo.seriesDetail(seriesId, detailChannelForFeed(channel)))
      : feedDetailFromMock({ id: seriesId } as SeriesItem);
    if (!result) throw new Error('没有找到这部短剧的播放详情。');
    if (result.type !== channel) throw new Error('推荐内容与当前频道不一致。');
    if (result.episodes.length === 0) throw new Error('这部短剧暂时没有可播放的集数。');
    // mock / 公开详情里的标签是真实的：记下来，排序时不必再拉一次。
    if (requestId === generationRef.current && result.tags?.length) {
      tagResolverRef.current.remember(seriesId, result.tags);
    }
    return result;
  }, []);
  loadDetailRef.current = loadDetailFor;

  /**
   * 起播某一集。
   *
   * 返回**真实的开局结果**（成功 `true` / 失败 `false`）：调用方要据此决定
   * "起播之后再跑后台任务"，而不是在 `void openEpisode(...)` 还没落地时就跑。
   */
  const startEpisode = useCallback((
    item: SeriesItem,
    nextDetail: FeedDetail,
    index: number,
    position: number,
  ): Promise<boolean> => {
    const episode = nextDetail.episodes[index];
    if (!episode) throw new Error('找不到要播放的这一集。');
    /**
     * 信息流也是一等入口，判定必须走同一个函数。
     *
     * `surface: 'feed'` 在 `planPlaybackRoute` 里固定判 `inline`：独立窗口 + 联播
     * 队列 + 补帧属于主播放器的能力，在滑动流里被误触的代价远大于收益
     * （`PlayerControls.handleOpenNative` 因此还有第二道闸）。这条排除以前是
     * 「某处没写」，现在是一个带文案的理由：选了 MPV 的用户第一次在这里起播时
     * 被告知一次，之后不再啰嗦。
     */
    const route = planPlaybackRoute({
      preferredPlayer: settingsRef.current.preferredPlayer,
      nativeAvailable: NATIVE_PLAYER_AVAILABLE,
      seriesId: item.id,
      episodeId: episode.id,
      type: nextDetail.type,
      channel: channelRef.current,
      surface: 'feed',
    });
    const routeNotice = playbackNoticeFor(route);
    if (routeNotice && !feedRouteNoticeRef.current.has(route.reason)) {
      feedRouteNoticeRef.current.add(route.reason);
      showToast(routeNotice, 'warning');
    }
    const requestId = generationRef.current;
    const seq = seriesSeqRef.current;
    episodeIndexRef.current = index;
    setEpisodeIndex(index);
    setIsCompleted(false);
    openingRef.current = true;
    setIsOpening(true);
    return openEpisodeRef.current(item.id, episode.id, position, undefined, nextDetail)
      .then(() => requestId === generationRef.current && seriesSeqRef.current === seq)
      .catch((playError: unknown) => {
        if (requestId === generationRef.current && seriesSeqRef.current === seq) {
          setError(errorText(playError, '无法开始播放'));
        }
        return false;
      })
      .finally(() => {
        if (requestId !== generationRef.current || seriesSeqRef.current !== seq) return;
        openingRef.current = false;
        setIsOpening(false);
      });
  }, []);

  /**
   * 后台刷一遍兴趣表（历史 + 收藏 → 标签权重）。
   *
   * 同一代只跑一次：换剧不会各并发一批。失败一律降级为"没有画像"（按源顺序），
   * 且**不参与起播集号**——「每次换剧从首集 0 秒」是硬约束。
   */
  const interestJobRef = useRef<{ generation: number; promise: Promise<void> } | null>(null);
  const refreshInterest = useCallback((requestId: number): Promise<void> => {
    const running = interestJobRef.current;
    if (running && running.generation === requestId) return running.promise;
    const channel = channelRef.current;
    const promise = (async () => {
      try {
        const [history, favorites] = await Promise.all([
          ipcService.history.list(),
          ipcService.favorites.list(),
        ]);
        if (requestId !== generationRef.current || channelRef.current !== channel) return;
        const usable = (id: string | undefined): id is string => Boolean(id)
          && !id!.startsWith('guo:') && !id!.startsWith('anime_')
          && !id!.startsWith('dmghg:') && !id!.startsWith('bfzy:');
        // 只认同频道（旧记录没有 channel 时按真人剧处理，与历史脉络一致），
        // 绝不把任意历史强标成当前频道。
        const inChannel = (value: string | null | undefined) => (
          value ? value === channel : channel === 'drama'
        );
        const scopedHistory = history.filter(record => usable(record.seriesId) && inChannel(record.channel));
        const scopedFavorites = favorites.filter(favorite => usable(favorite.seriesId) && inChannel(favorite.channel));
        /*
         * 种子只挑**有效观看**（>=15s 或看完）与收藏：
         * 前 8 条"快速划过"的历史会把收藏挤出去，那样画像只反映误触。
         * 已知标签先写进解析器（列表卡片 / 已打开的详情），避免重复拉取。
         */
        for (const item of itemsRef.current) {
          if (item.tags?.length) tagResolverRef.current.remember(item.id, item.tags);
        }
        const openDetail = detailRef.current;
        if (openDetail?.tags?.length) tagResolverRef.current.remember(openDetail.id, openDetail.tags);
        const effectiveHistory = scopedHistory.filter(record => historyCompletion(record) > 0);
        const ids: string[] = [];
        const push = (id?: string) => {
          if (!id || ids.includes(id)) return;
          ids.push(id);
        };
        scopedFavorites.slice(0, TAG_FETCH_LIMIT).forEach(favorite => push(favorite.seriesId));
        effectiveHistory.slice(0, TAG_FETCH_LIMIT).forEach(record => push(record.seriesId));
        const tagsById = await tagResolverRef.current.resolve(ids.slice(0, TAG_FETCH_LIMIT));
        if (requestId !== generationRef.current || channelRef.current !== channel) return;
        interestRef.current = buildInterest({
          history: scopedHistory,
          favorites: scopedFavorites,
          tagsById,
        });
      } catch {
        // 画像缺失是允许的状态：`orderFeedPage` 会原样返回源顺序。
      }
    })();
    interestJobRef.current = { generation: requestId, promise };
    return promise;
  }, []);

  /** 后台预热指定短剧的详情并触发首集媒体预热；若前台仍在起播转码则自动延后重试（滑走即中止）。 */
  const warmSeriesMedia = useCallback((
    seriesId: string,
    channel: FeedChannel,
    isCurrent: () => boolean,
    signal?: AbortSignal,
    retriesLeft = 24,
    onWarmed?: () => void,
  ) => {
    warmingSeriesIdRef.current = seriesId;
    void prefetcherRef.current.get(seriesId, channel, isCurrent)
      .then(nextDetail => {
        if (!isCurrent() || signal?.aborted) {
          if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
          return;
        }
        const firstEp = nextDetail.episodes[0];
        if (!firstEp) {
          if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
          return;
        }
        const contentType = contentTypeFor(channel);
        const retryWarm = (remaining: number) => {
          if (!isCurrent() || signal?.aborted) {
            if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
            return;
          }
          void Promise.resolve(
            prewarmEpisodeRef.current?.(seriesId, firstEp.id, contentType, signal, true),
          ).then(started => {
            if (!isCurrent() || signal?.aborted) {
              if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
              return;
            }
            if (started !== false) {
              if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
              onWarmed?.();
              return;
            }
            if (remaining <= 0) {
              if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
              return;
            }
            neighborWarmTimerRef.current = setTimeout(() => {
              neighborWarmTimerRef.current = null;
              retryWarm(remaining - 1);
            }, 600);
          });
        };
        retryWarm(retriesLeft);
      })
      .catch(() => {
        if (warmingSeriesIdRef.current === seriesId) warmingSeriesIdRef.current = null;
      });
  }, []);

  /** 仅预取邻近下一部的详情与首集预签名，不动编码器与下载带。 */
  const prefetchNeighborDetails = useCallback((index: number, requestId: number) => {
    const channel = channelRef.current;
    const contentType = contentTypeFor(channel);
    const isCurrent = () => requestId === generationRef.current;
    const ids = [itemsRef.current[index + 1]?.id, itemsRef.current[index + 2]?.id]
      .filter((id): id is string => Boolean(id));
    if (!ids.length) return;
    prefetcherRef.current.warm(ids, channel, isCurrent);
    if (!isTauriEnvironment()) return;
    for (const id of ids) {
      if (id.startsWith('guo:') || id.startsWith('hg:') || id.startsWith('anime_')) continue;
      void prefetcherRef.current.get(id, channel, isCurrent)
        .then(nextDetail => {
          if (!isCurrent()) return;
          const epId = nextDetail.episodes[0]?.id;
          if (!epId || prefetchedStreamVidsRef.current.has(epId)) return;
          prefetchedStreamVidsRef.current.add(epId);
          void ipcService.playback.prefetchStream([epId], contentType);
        })
        .catch(() => undefined);
    }
  }, []);

  /** 起播成功后预热"邻近下一部"的详情及接下来两部的首集媒体（短防抖避开快速连滑，忙碌时自动顺延重试）。 */
  const prefetchNeighbors = useCallback((index: number, requestId: number) => {
    prefetchNeighborDetails(index, requestId);
    cancelNeighborWarm();
    const nextId = itemsRef.current[index + 1]?.id;
    const afterNextId = itemsRef.current[index + 2]?.id;
    if (!nextId) return;
    neighborWarmTimerRef.current = setTimeout(() => {
      neighborWarmTimerRef.current = null;
      if (requestId !== generationRef.current || currentIndexRef.current !== index) return;
      const controller = new AbortController();
      neighborWarmAbortRef.current = controller;
      const activeChannel = channelRef.current;
      const stillActive = () => requestId === generationRef.current && !controller.signal.aborted;
      warmSeriesMedia(
        nextId,
        activeChannel,
        stillActive,
        controller.signal,
        24,
        afterNextId
          ? () => {
              if (!stillActive() || currentIndexRef.current !== index) return;
              warmSeriesMedia(afterNextId, activeChannel, stillActive, controller.signal, 10);
            }
          : undefined,
      );
    }, 60);
  }, [cancelNeighborWarm, prefetchNeighborDetails, warmSeriesMedia]);

  /**
   * 后台给"本次追加段"补标签并排序，**绝不阻塞导航**。
   *
   * 只动 `tailStart`..`tailEnd` 这一段，段外（前缀、已访问区、后续新追加页）一条不改。
   * 四道保护缺一不可：
   * - `tailStart <= visitedHighWater`：段里混着已看过的条目 → 放弃；
   * - 段内 id 与追加当时的快照 (`expectedIds`) 必须逐条一致 → 不一致（队列被改过）放弃；
   * - 段边界超出当前队列长度 → 放弃，绝不越界重排后续新页；
   * - 代数 / 频道核对：换频道后旧续体一个字段都不写。
   *
   * 这里**不回写**首屏候选缓存：候选缓存只认原始首屏切片与原始游标。
   */
  const orderTailLater = useCallback(async (
    requestId: number,
    expectedIds: readonly string[],
    tailStart: number,
    tailEnd: number,
    channel: FeedChannel,
  ) => {
    const interest = interestRef.current;
    if (interest.size === 0) return;
    if (tailEnd - tailStart < 2) return;
    if (tailStart <= visitedHighWaterRef.current) return;
    const startedAt = performance.now();
    const missing = itemsRef.current
      .slice(tailStart, tailEnd)
      .filter(item => !item.tags?.length)
      .slice(0, TAG_FETCH_LIMIT)
      .map(item => item.id);
    const tagsById = missing.length
      ? await tagResolverRef.current.resolve(missing)
      : new Map<string, readonly string[]>();
    if (requestId !== generationRef.current || channelRef.current !== channel) return;
    const latest = itemsRef.current;
    if (latest.length < tailEnd) return;
    const ordered = orderFeedTail(latest, {
      tailStart,
      tailEnd,
      expectedIds,
      visitedHighWater: visitedHighWaterRef.current,
      interest,
      tagsById,
    });
    if (!ordered) return;
    traceFeedStage('尾段排序', startedAt, `段=${tailEnd - tailStart}`);
    itemsRef.current = ordered;
    setItems(ordered);
  }, []);

  /**
   * 换代 / 离开推荐页时作废的只有**代内状态**。
   *
   * - 兴趣表、标签解析器：每代都清（标签缓存只服务它这一代）。
   * - 详情预取：保留已完成缓存、切断在途（`reset({ keepCache: true })`）。切回来
   *   同一个频道的剧仍然零往返，但上一代在途的 Promise 结果不得写进新代缓存。
   * - 首屏候选缓存**不清**：按频道分 3 个槽，切频道不连累别人，另有 TTL 兜底。
   */
  const resetFeedProfile = useCallback(() => {
    cancelNeighborWarm();
    prefetchedStreamVidsRef.current.clear();
    interestJobRef.current = null;
    interestRef.current = new Map();
    tagResolverRef.current.clear();
    prefetcherRef.current.reset({ keepCache: true });
  }, [cancelNeighborWarm]);

  /**
   * 打开队列里第 `index` 部剧，起播点**恒定**是首集 0 秒。
   *
   * 首次进入、前进换剧、反向回上一部都走这里，不读观看历史。剧内第 2、3 集由
   * 不操作时的自动连播推进（`usePlaybackStore` 的 ended 链路），不经过这里。
   *
   * 分段计时在本层自己起一个 `performance.now()`：只算"详情 + 起播准备"这两段，
   * 列表段由调用方（首屏 / 补页）各自计时，互不覆盖。
   */
  const openSeriesAt = useCallback(async (index: number, requestId: number) => {
    const item = itemsRef.current[index];
    if (!item || requestId !== generationRef.current) return;
    // 若正在后台预热的恰好是即将切入的这一部剧，保留其 ffmpeg 进程给 openStream 复用，避免拦腰掐断重头再转。
    cancelNeighborWarm(item.id);
    const seq = ++seriesSeqRef.current;
    // 新一轮切剧立刻释放上一轮残留的 opening 状态，确保连续滑动立即生效。
    openingRef.current = false;
    setIsOpening(false);
    // 打开即"访问过"：高水位只增不减，后台排序据此绕开已访问区。
    visitedHighWaterRef.current = Math.max(visitedHighWaterRef.current, index);
    const channel = channelRef.current;
    const instantDetail = prefetcherRef.current.peek(item.id, channel) ?? readWarmedDetail(item.id, channel);
    if (instantDetail) {
      if (instantDetail.tags?.length) tagResolverRef.current.remember(item.id, instantDetail.tags);
      setCurrent(item, index, instantDetail);
      setLoading(false);
    } else {
      setCurrent(item, index, null);
      setLoading(true);
    }
    setError(null);
    setIsCompleted(false);

    try {
      const detailStarted = performance.now();
      const detailHit = Boolean(instantDetail) || prefetcherRef.current.cached(item.id, channel);
      const inflightDetail = !instantDetail ? warmedDetailInflight.get(`${channel}:${item.id}`) : undefined;
      const nextDetail = instantDetail
        ?? (inflightDetail ? await inflightDetail : null)
        ?? await prefetcherRef.current.get(
          item.id,
          channel,
          () => requestId === generationRef.current && seriesSeqRef.current === seq,
        );
      if (requestId !== generationRef.current || seriesSeqRef.current !== seq) return;
      traceFeedStage('详情', detailStarted, `来源=${detailHit ? '缓存' : '网络'}`);
      setCurrent(item, index, nextDetail);
      setLoading(false);
      // 先只预取邻近剧目详情，等本剧起播完成后再预热下一部媒体，避免首帧与背景转码争抢资源。
      prefetchNeighborDetails(index, requestId);
      // 换剧/首进一律首集 0 秒，不从历史恢复。
      const opening = startEpisode(item, nextDetail, 0, 0);
      // **起播成功**且代数与剧都没变，才跑兴趣画像、邻集媒体预热与翻页预取。
      void opening.then(opened => {
        if (!opened) return;
        traceFeedStage('起播准备', detailStarted, `位置=${index}`);
        if (requestId !== generationRef.current || seriesSeqRef.current !== seq || currentItemRef.current?.id !== item.id) return;
        prefetchNeighbors(index, requestId);
        void refreshInterest(requestId);
        maybePrefetchPageRef.current(requestId);
      });
    } catch (loadError) {
      if (requestId !== generationRef.current || seriesSeqRef.current !== seq) return;
      setLoading(false);
      setError(errorText(loadError, '推荐内容加载失败'));
    }
  }, [cancelNeighborWarm, prefetchNeighborDetails, prefetchNeighbors, refreshInterest, startEpisode]);

  const fetchPage = useCallback(async (cursor?: string | null) => {
    return fetchFeedPageForChannel(channelRef.current, cursor);
  }, []);

  /**
   * 清掉**上一次补页**留下的错误。
   *
   * 匹配用 `pageErrorRef` 里记的**那一次实际写入的文本**，不是常量：
   * `errorText(loadError, fallback)` 在 `loadError` 是 `Error('HTTP 503')` 时
   * 返回的是 `'HTTP 503'`，用 `prev === FEED_PAGE_ERROR` 比对永远不命中，等于
   * 清理失效。同时这一步也不会误清后来的真实播放/详情错误——文本对不上就不动。
   */
  const clearPageError = useCallback(() => {
    const text = pageErrorRef.current;
    if (!text) return;
    pageErrorRef.current = null;
    setError(prev => (prev === text ? null : prev));
  }, []);

  /**
   * 补一页候选。
   *
   * 两条时序上的硬要求：
   * - **合并即入队**：页一到就进队列，标签解析与排序全部挪到后台（`orderTailLater`）。
   *   旧实现先 `await enrichAndOrder`，于是"滑到边界换剧"要等最多 8 部详情往返。
   * - **在途复用**：后台提前补页与用户正好滑到边界共用同一个 Promise
   *   （`pageJobRef`）。后台还在跑时边界导航直接 `return []`，用户看到的是
   *   "滑了一下没反应"，得再滑一次。
   *
   * `foreground` 决定错误可见性：后台补页失败只记账（供 `retry()` 重试），绝不
   * 把正在播放的界面盖成错误；用户主动分页失败才显式提示。同一个任务被前台
   * 接管时（`foreground` 升级）仍按前台口径提示。
   */
  const startPageJob = useCallback((requestId: number, foreground: boolean): Promise<SeriesItem[]> => {
    const previousCursor = cursorRef.current;
    const channel = channelRef.current;
    if (!hasMoreRef.current || !previousCursor) return Promise.resolve([]);
    loadingMoreRef.current = true;
    setIsLoadingMore(true);
    const startedAt = performance.now();
    const job: { promise: Promise<SeriesItem[]>; foreground: boolean } = { promise: Promise.resolve([]), foreground };
    job.promise = (async () => {
      try {
        const page = await fetchPage(previousCursor);
        // 代数 / 频道 / 游标三重身份：任何一项变了都不是"这一页"了。
        if (requestId !== generationRef.current || channelRef.current !== channel) return [];
        if (cursorRef.current !== previousCursor) return [];
        const merged = mergeFeedPage(itemsRef.current, page.items);
        const nextItems = dedupeFeedItems(merged.items, channel);
        const tailStart = merged.tailStart;
        // 队列只增不减：前缀是已经过 dedupe 的旧条目，追加段 = 新页活下来的那些。
        const tailEnd = nextItems.length;
        cursorRef.current = page.cursor;
        hasMoreRef.current = Boolean(page.hasMore && page.cursor && page.cursor !== previousCursor);
        setHasMore(hasMoreRef.current);
        itemsRef.current = nextItems;
        setItems(nextItems);
        traceFeedStage('补页', startedAt, `追加=${tailEnd - tailStart}`);
        // 候选缓存**不**回写：它只认原始首屏切片与原始游标（见 feedQueue 头注释）。
        // 上一次补页留下的错误必须撤掉，否则"加载成功但错误还挂着"。
        clearPageError();
        // 标签 + 排序全部后台：不挡导航，且只作用于"本次追加段"。
        void orderTailLater(
          requestId,
          nextItems.slice(tailStart, tailEnd).map(item => item.id),
          tailStart,
          tailEnd,
          channel,
        );
        // 邻近详情预取（最多两部，合并请求）：让下一次换剧命中缓存。
        prefetchNeighbors(currentIndexRef.current, requestId);
        return merged.added;
      } catch (loadError) {
        if (requestId === generationRef.current) {
          const text = errorText(loadError, FEED_PAGE_ERROR);
          pageErrorRef.current = text;
          // 后台失败不改界面状态：正在播放的那部剧不该因为"下一页没取到"变红。
          // 只有用户主动等这一页（前台）时错误才可见。
          if (job.foreground) setError(text);
        }
        return [];
      } finally {
        // 只有"仍属于当前代数"的补页才有资格放开闸门：换频道后新一代已经在跑，
        // 旧续体把 `loadingMoreRef` 清掉会让新频道的补页被重复发放。
        if (requestId === generationRef.current) {
          loadingMoreRef.current = false;
          setIsLoadingMore(false);
        }
        // 身份校验：新一代可能已经换掉了 pageJob，旧续体不许清别人的任务。
        if (pageJobRef.current === job) pageJobRef.current = null;
      }
    })();
    pageJobRef.current = job;
    return job.promise;
  }, [clearPageError, fetchPage, orderTailLater, prefetchNeighbors]);

  /**
   * 补页入口：在途任务复用；用户主动等待时把任务升级为前台（错误可见）。
   *
   * 注意 `loadingMoreRef` 的闸门**只**加在"新起任务"上——命中在途任务时必须返回
   * 那个 Promise，而不是被闸门挡成 `[]`。后台在途时用户滑到边界，走的就是这一支：
   * 等同一个 Promise，请求数仍然是 1。
   */
  const appendNextPage = useCallback((requestId: number, options?: { foreground?: boolean }): Promise<SeriesItem[]> => {
    const foreground = options?.foreground === true;
    const running = pageJobRef.current;
    if (running) {
      if (foreground) running.foreground = true;
      return running.promise;
    }
    if (loadingMoreRef.current) return Promise.resolve([]);
    return startPageJob(requestId, foreground);
  }, [startPageJob]);

  /**
   * 距末端不足 `PAGE_PREFETCH_AHEAD` 时提前补一页（后台口径，最多一页）。
   *
   * 不在补页任务内部再次调用自己：`loadingMoreRef` 在任务期间为真，加上补页后
   * `itemCount` 变大，条件自然不再成立 —— 不构成自动无限取数。
   */
  const maybePrefetchPage = useCallback((requestId: number) => {
    if (!shouldPrefetchNextPage({
      currentIndex: currentIndexRef.current,
      itemCount: itemsRef.current.length,
      hasMore: hasMoreRef.current,
      pageInFlight: loadingMoreRef.current,
    })) return;
    void appendNextPage(requestId);
  }, [appendNextPage]);
  maybePrefetchPageRef.current = maybePrefetchPage;

  const loadInitial = useCallback(async () => {
    const requestId = ++generationRef.current;
    const activeChannel = channelRef.current;
    const loadStarted = performance.now();
    loadingMoreRef.current = false;
    // 每代清补页任务：旧代在途的 Promise 属于旧游标，不得被新代复用。
    pageJobRef.current = null;
    // 换代即作废：兴趣表/标签每代清；详情缓存保留、在途切断；候选缓存按频道留槽。
    seriesSeqRef.current += 1;
    resetFeedProfile();
    loadingRef.current = true;
    openingRef.current = false;
    cursorRef.current = null;
    hasMoreRef.current = false;
    visitedHighWaterRef.current = -1;
    // 换代即重置导航状态：新的一页没有任何"欠页"，也没有合法持有者。
    navLockRef.current = null;
    pendingPageRef.current = false;
    pageErrorRef.current = null;
    itemsRef.current = [];
    setItems([]);
    currentIndexRef.current = 0;
    currentItemRef.current = null;
    detailRef.current = null;
    setCurrentIndex(0);
    setCurrentItem(null);
    setDetail(null);
    setLoading(true);
    setIsOpening(false);
    setIsLoadingMore(false);
    setHasMore(false);
    setIsCompleted(false);
    setError(null);

    // 首屏候选缓存命中：直接用上次那份**原始首屏切片**摆出来，游标也停在它原本的位置，
    // 第 0 部照旧首集 0 秒起播。这里**不再**跑后台首屏刷新：刷新会把多页游标退回第 1 页，
    // 与提前翻页的补页任务互相踩（旧游标一回来就把新页整段冲掉）。
    const seeded = candidateCacheRef.current.read(activeChannel) ?? warmedCandidateCache.read(activeChannel);
    if (seeded && seeded.items.length > 0) {
      candidateCacheRef.current.write(activeChannel, seeded);
      itemsRef.current = seeded.items;
      setItems(seeded.items);
      cursorRef.current = seeded.cursor;
      hasMoreRef.current = Boolean(seeded.hasMore && seeded.cursor);
      setHasMore(hasMoreRef.current);
      traceFeedStage('首屏列表', loadStarted, '来源=缓存');
      await openSeriesAt(0, requestId);
      traceFeedStage('首屏完成', loadStarted);
      return;
    }

    try {
      // 不再 `loadHistory()`：推荐流的入口点与观看历史无关（见文件头）。
      // 首屏只有一条取数，队列里也不插"最近一部历史"。
      const page = await (warmedPageInflight.get(activeChannel) ?? fetchPage());
      if (requestId !== generationRef.current) return;
      const queued = dedupeFeedItems(page.items, channelRef.current);
      itemsRef.current = queued;
      setItems(queued);
      cursorRef.current = page.cursor;
      hasMoreRef.current = Boolean(page.hasMore && page.cursor);
      setHasMore(hasMoreRef.current);
      traceFeedStage('首屏列表', loadStarted, '来源=网络');
      // 只写原始首屏切片 + 原始游标；追加页不回写（见 feedQueue 头注释）。
      candidateCacheRef.current.write(activeChannel, {
        items: queued,
        cursor: page.cursor,
        hasMore: hasMoreRef.current,
      });

      if (queued.length === 0) {
        setLoading(false);
        setError('当前频道暂时没有可播放的推荐。');
        return;
      }
      await openSeriesAt(0, requestId);
      traceFeedStage('首屏完成', loadStarted);
    } catch (loadError) {
      if (requestId !== generationRef.current) return;
      setLoading(false);
      setError(errorText(loadError, '推荐内容加载失败'));
    }
  }, [fetchPage, openSeriesAt, resetFeedProfile]);

  useEffect(() => {
    if (currentView !== 'feed') {
      generationRef.current += 1;
      resetFeedProfile();
      loadingRef.current = false;
      openingRef.current = false;
      return;
    }
    void loadInitial();
    return () => {
      generationRef.current += 1;
      resetFeedProfile();
    };
  }, [currentView, feedChannel, loadInitial, resetFeedProfile]);

  useEffect(() => {
    if (currentView !== 'feed' || !currentItem || !detail || currentSeries?.id !== currentItem.id || !currentEpisode) return;
    const index = detail.episodes.findIndex(episode => episode.id === currentEpisode.id);
    if (index >= 0 && index !== episodeIndexRef.current) {
      episodeIndexRef.current = index;
      setEpisodeIndex(index);
    }
  }, [currentView, currentSeries, currentEpisode, currentItem, detail]);

  /**
   * 换一部推荐剧（方向键 / 滚轮 / 滑动手势 / 「换一部」/ 重试补页的唯一出口）。
   *
   * 判定本身在 `services/feedSeriesNav.ts`（async，可被脚本直接跑），这里只提供
   * 真实副作用与两把状态：
   *
   * - `navLockRef`：**按代数记账**的导航锁。存的是本次导航的 requestId，释放时
   *   只有"锁还属于自己"才清——`loadInitial` 换代后，旧导航的 `finally` 不会
   *   把新代的锁放开。
   * - `pendingPageRef`：还欠一页。补页失败（`hasMore` 仍为真）时保持为真，
   *   `retry()` 据此再次走这条路径，而不是把当前剧重播一遍。
   *
   * 队列到底 ≠ 本剧播完：末尾**不写** `error` / `isCompleted`，最多给一条轻提示。
   */
  const switchSeries = useCallback(async (direction: FeedSeriesDirection = 'next') => {
    if (navLockRef.current !== null) return;
    if (itemsRef.current.length === 0 && loadingRef.current) return;
    const requestId = generationRef.current;
    navLockRef.current = requestId;
    const navStarted = performance.now();
    try {
      await runSeriesNav(direction, {
        readIndex: () => currentIndexRef.current,
        readItemCount: () => itemsRef.current.length,
        readHasMore: () => hasMoreRef.current,
        isCurrent: () => requestId === generationRef.current,
        // 前台口径：用户就在等这一步，失败必须可见（后台补页失败则不打扰）。
        appendPage: () => appendNextPage(requestId, { foreground: true }).then(() => undefined),
        openIndex: index => openSeriesAt(index, requestId),
        setPendingPage: pending => { pendingPageRef.current = pending; },
        onExhausted: () => showToast('已经是最后一部推荐了', 'info'),
        onPageSettled: clearPageError,
      });
    } finally {
      if (navLockRef.current === requestId) navLockRef.current = null;
      traceFeedStage('换剧', navStarted, `方向=${direction}`);
      if (requestId === generationRef.current) maybePrefetchPage(requestId);
    }
  }, [appendNextPage, clearPageError, maybePrefetchPage, openSeriesAt, showToast]);

  /**
   * 上一集：只在本剧内。首集时 no-op（不再"回到上一部"）。
   */
  const previousEpisode = useCallback(() => {
    if (loadingRef.current || openingRef.current) return;
    const item = currentItemRef.current;
    const nextDetail = detailRef.current;
    if (!item || !nextDetail) return;
    const nextIndex = feedEpisodeStep(
      episodeIndexRef.current,
      nextDetail.episodes.length,
      'previous',
      nextDetail.playableEpisodes,
    );
    if (nextIndex === null) return;
    startEpisode(item, nextDetail, nextIndex, 0);
  }, [startEpisode]);

  /**
   * 下一集：只在本剧内。
   *
   * 末集 / 可播边界处 **no-op**，不跨季也不换部；自动连播走
   * `usePlaybackStore` 的 ended 链路，不经过这里。
   */
  const nextEpisode = useCallback(() => {
    if (loadingRef.current || openingRef.current) return;
    const item = currentItemRef.current;
    const nextDetail = detailRef.current;
    if (!item || !nextDetail) return;
    const nextIndex = feedEpisodeStep(
      episodeIndexRef.current,
      nextDetail.episodes.length,
      'next',
      nextDetail.playableEpisodes,
    );
    if (nextIndex === null) return;
    startEpisode(item, nextDetail, nextIndex, 0);
  }, [startEpisode]);

  /**
   * 本剧最后一集正常播完（`usePlaybackStore.finishedAll` → `FeedCompletionBridge`）。
   *
   * 停留：只置 `isCompleted`，让状态横幅说"当前短剧已播放完成"并给出「重播」。
   * 不查季列表、不换下一部。
   */
  const advanceAfterFinished = useCallback((sessionId: number) => {
    if (currentView !== 'feed' || finishedSessionRef.current === sessionId) return;
    finishedSessionRef.current = sessionId;
    setIsCompleted(true);
  }, [currentView]);

  /**
   * 重试按钮的行为取决于**失败发生在哪一层**：
   *
   * - 上一次导航因为补页失败没走成（`pendingPageRef` 为真）→ 直接再走一遍
   *   `switchSeries('next')`。欠页标记**不在这里清**：由 `runSeriesNav` 在"真的
   *   没页可补"或"换到下一部"时统一清。清早了会让连续第二次补页失败后，
   *   用户再点重试就落进下面的重播分支，把正在看的剧从首集重开；
   * - 其它情况（加载失败 / 播完后的「重播」）→ 当前这部剧首集 0 秒重来。
   *
   * 补页路径也不复制 `appendNextPage` / `openSeriesAt`：串行锁与"下标没被用户
   * 改过"的核对都在 `switchSeries` 里，复制一份就等于两套并发语义。
   */
  const retry = useCallback(() => {
    if (pendingPageRef.current) {
      void switchSeries('next');
      return;
    }
    if (currentIndexRef.current < 0 || !currentItemRef.current) {
      void loadInitial();
      return;
    }
    const retryStarted = performance.now();
    void openSeriesAt(currentIndexRef.current, generationRef.current)
      .finally(() => traceFeedStage('重试', retryStarted));
  }, [loadInitial, openSeriesAt, switchSeries]);

  const close = useCallback(() => {
    // 离开推荐页：代内状态作废（标签/兴趣清、详情在途切断但缓存保留）。
    resetFeedProfile();
    navigateTo('explore');
  }, [navigateTo, resetFeedProfile]);
  /*
   * 切频道。
   *
   * 2026-11-10 收口：`useAppStore` 的 `FeedChannel` 已补齐 `'ai'`，原先那行
   * `openFeed as unknown as (...)` 的类型桥接随之删除——它存在的唯一理由是
   * App store 那份联合还停在两值，而 `openFeed` 运行时本就是
   * `setFeedChannel(channel) + setCurrentView('feed')`，'ai' 会原样进状态。
   * 现在两处联合一致，编译器能直接看出这个调用的合法性，不需要桥接。
   */
  const selectChannel = useCallback((nextChannel: FeedChannel) => {
    openFeed(nextChannel);
  }, [openFeed]);
  const toggleFavorite = useCallback(() => {
    const item = currentItemRef.current;
    if (!item) return;
    void setMark(item.id, item.title, item.cover, favoriteMarkRef.current ? null : 'want', item.type)
      .catch((saveError: unknown) => setError(errorText(saveError, '追剧状态保存失败')));
  }, [setMark]);

  const warmSeries = useCallback((seriesId: string) => {
    if (!seriesId) return;
    const requestId = generationRef.current;
    warmSeriesMedia(seriesId, channelRef.current, () => requestId === generationRef.current);
  }, [warmSeriesMedia]);

  return {
    channel,
    channelCopy: feedChannelCopy(channel),
    items,
    currentIndex,
    currentItem,
    detail,
    episodeIndex,
    isLoading,
    isOpening,
    isLoadingMore,
    hasMore,
    isCompleted,
    favoriteMark,
    error,
    selectChannel,
    previousEpisode,
    nextEpisode,
    switchSeries,
    warmSeries,
    retry,
    close,
    toggleFavorite,
    advanceAfterFinished,
  };
}
