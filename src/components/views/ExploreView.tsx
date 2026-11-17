import React, { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { useCatalogStore } from '../../stores/useCatalogStore';
import { useAppStore, ShelfKind } from '../../stores/useAppStore';
import { usePlaybackActions } from '../../stores/usePlaybackStore';
import { useAnimePlayerActions } from '../../stores/useAnimePlayerStore';
import { useSettingsStore } from '../../stores/useSettingsStore';
import { ipcService, coverResolver, type HongguoActorRankItem, coverRawUsable } from '../../services/ipc';
import { shelfFeedSupports, useShelfFeed } from '../../stores/useShelfFeed';
import { StatusBadge } from '../common/StatusBadge';
import { FluentButton } from '../common/FluentButton';
import { CoverImage } from '../common/CoverImage';
import { SeriesCard, SERIES_GRID_CLASS } from '../common/SeriesCard';
import { SeriesCardSkeleton } from '../common/SeriesCardSkeleton';
import { HomeShelf } from '../common/HomeShelf';
import { HomeShelfSection } from '../common/HomeShelfSection';
import { RankingSection } from '../common/RankingSection';
import { PersonDialog } from '../common/PersonDialog';
import { ShortFilterDialog } from '../common/ShortFilterDialog';
import { RandomWatchSection, hasRandomCandidates, PICK_CARD_SHELL } from '../common/RandomWatchSection';
import { CategoryBar } from '../common/CategoryBar';
import { BackToTop } from '../common/BackToTop';
import { usePrefetchBackstop } from '../../hooks/useInfiniteScroll';
import { ShelfBadge } from '../common/ShelfBadge';
import { SeriesItem } from '../../types/catalog';
import { contentTypeFor } from '../../services/contentType';
import { tryLaunchNativePlayer, normalizePreferredPlayer } from '../../services/preferredPlayback';
import {
  NATIVE_OPEN_FAILED_NOTICE,
  NATIVE_SKIPPED_NOTICE,
  planPlaybackRoute,
} from '../../services/playbackRouting';
import { NATIVE_PLAYER_AVAILABLE } from '../../services/nativePlayerRollout';
import { isHongguoSeriesId } from '../../services/preferredPlaybackLogic';
import { SeriesDetail } from '../../types/series';
import {
  Flame,
  Sparkles,
  ChevronRight,
  Play,
  TrendingUp,
  Clock,
  Moon,
  Compass,
  CalendarDays,
  Users,
  LayoutGrid,
  Bot,
} from 'lucide-react';

/**
 * 各频道的标题与量词。
 *
 * 刻意集中一处而不是散在 JSX 里写三元：`channel` 现在有四个值，再散着写
 * `channel === 'comic' ? 漫剧 : 短剧` 的话，新加的 `adult` 会静默落进"短剧"
 * 那个 else 分支——用户看到的是"精选短剧推荐"顶着 18+ 内容。
 */
const CHANNEL_COPY: Record<string, { title: string; noun: string }> = {
  drama: { title: '精选短剧推荐', noun: '短剧' },
  comic: { title: '精选漫剧推荐', noun: '漫剧' },
  ai: { title: '精选 AI 剧推荐', noun: 'AI 剧' },
  adult: { title: '神秘小窝', noun: '内容' },
  anime: { title: '动漫推荐', noun: '动漫' },
};

/**
 * 18+ 专区是否可用：**内置源里已经没有 18+ 来源，恒为 false**（2026-11-02）。
 *
 * 源收敛时 18 个站源全部下架，`adultTabSources()` 因此恒返回空——再留着入口
 * 就是"点进去永远空空如也"。这里把开关钉死：tab 不渲染，落到该频道的守卫
 * （下面的 effect）会把 channel 退回短剧专区。
 *
 * 保留整条 adult 分支而不是物理删除：插件系统上线后，第三方来源里完全可能
 * 有成人内容，届时把常量翻回 true 即可恢复（后端门闩 `source_is_adult` 也仍然
 * 认得那些旧 id，安全性不靠这个常量兜底）。
 */
const ADULT_SOURCES_AVAILABLE = false;

/**
 * 「还剩 8 分 20 秒」。
 *
 * 续播卡上真正有用的数字是"还要花多久"，不是"已经花了多少"——后者进度条已经
 * 画出来了。拿不到时长（源没上报、`durationSeconds` 为 0）时返回空串，由调用方
 * 决定不渲染，而不是硬凑一个"还剩 0 秒"。
 */
function formatRemaining(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '';
  const total = Math.round(seconds);
  if (total < 60) return `还剩 ${total} 秒`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) {
    const rest = total % 60;
    return rest > 0 ? `还剩 ${minutes} 分 ${rest} 秒` : `还剩 ${minutes} 分钟`;
  }
  return `还剩 ${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分`;
}

export const ExploreView: React.FC = () => {
  const {
    channel,
    category,
    audience,
    sort,
    categories,
    items,
    continueWatching,
    historySeriesIds,
    isLoading,
    isLoadingMore,
    hasMore,
    dedupedStop,
    pagesLoaded,
    error,
    activeSources,
    setChannel,
    setCategory,
    setSort,
    setAudience,
    refreshCatalog,
    loadMore,
    prefetchMore,
    refreshContinueWatching,
  } = useCatalogStore();

  const { currentView, navigateTo, openShelf, openSpotlight, showToast } = useAppStore();
  // 只订阅低频动作专线。整表订阅会让本页上百张卡片跟着播放进度（约 4 次/秒）重渲染。
  const { openEpisode, prewarmEpisode } = usePlaybackActions();
  const { open: openAnimeEpisode } = useAnimePlayerActions();
  const { settings, updateSettings } = useSettingsStore();
  const showAdultSources = ADULT_SOURCES_AVAILABLE && settings.showAdultSources;

  /**
   * 关掉 18+ 总开关时，若正停在神秘小窝就退回短剧专区。
   *
   * tab 会在开关关闭时消失，但 `channel` 还停在 `adult`——不拉回来的话，
   * `sources` 变成空数组，页面会既没有 tab 高亮、也没有任何卡片，用户看到的是
   * "发现页坏了"而不是"你刚关掉了一个开关"。
   */
  useEffect(() => {
    if (!showAdultSources && channel === 'adult') setChannel('drama');
  }, [showAdultSources, channel, setChannel]);

  // 回到发现页就刷新"继续观看"：横幅的数据来自历史记录，而目录本身不会因为
  // 看了几集而变化，不单独刷新就会一直停在启动那一刻的旧进度。
  useEffect(() => {
    if (currentView === 'explore') void refreshContinueWatching();
  }, [currentView, refreshContinueWatching]);

  /**
   * 列表滚动容器。
   *
   * 这里曾经还有一个 `isContinueDismissed`：让用户叉掉「继续观看」横幅。并轨之后
   * 那个叉号被移除了 —— 它原本是"可选横幅"时代的交互，而现在这一行是页首固定的
   * "为你"区，叉掉一张只会让另一张变宽，语义上不成立。状态随之删除，别再加回来。
   */
  const scrollContainerRef = useRef<HTMLDivElement | null>(null);
  /** 「发现更多」目录区，见下面换筛选时的那段滚动逻辑。 */
  const catalogSectionRef = useRef<HTMLDivElement | null>(null);
  /** 上一轮的频道 / 筛选值：用来分辨这次变化该不该弹回页首。 */
  const prevScrollKeyRef = useRef({ channel, category, audience, sort });

  /**
   * 切换筛选之后的滚动位置。
   *
   * 老逻辑是一律 `scrollTo(0)`，理由写在原来的注释里："筛选栏是当前结果的上下文，
   * 若保留旧滚动位置，用户会看到新结果却找不到刚才点过的题材"。
   *
   * **那条理由在筛选条常驻（sticky）之后失效了**：点过的题材就在视野里的条上亮着，
   * 不需要靠"回到页首"来证明页面更新过。于是原来的行为反而变成了障碍——用户在
   * 目录滚了五屏后换个题材，会被一把甩回三屏之外的货架，再滚下来才看得到结果。
   *
   * 现在按"用户此刻在哪儿"分流：
   *   - 换频道（短剧 ⇄ 漫剧）：整页内容全换，回页首——这一条不能省，否则会停在
   *     上一频道某一屏上，看到的却是另一个频道的中段；
   *   - 换题材 / 体裁 / 排序，且用户已经在目录区里：停在目录区顶部，新结果第一眼
   *     就能看到；
   *   - 换题材 / 体裁 / 排序，但用户还在上面的货架区：照旧回页首（他本来就在顶部
   *     附近，回顶没有代价，也避免一次莫名其妙的跳动）。
   */
  useEffect(() => {
    const container = scrollContainerRef.current;
    if (!container) return;

    const prev = prevScrollKeyRef.current;
    const channelChanged = prev.channel !== channel;
    const filtersChanged =
      prev.category !== category || prev.audience !== audience || prev.sort !== sort;
    prevScrollKeyRef.current = { channel, category, audience, sort };
    if (!channelChanged && !filtersChanged) return;

    if (!channelChanged) {
      const section = catalogSectionRef.current;
      if (section) {
        // 目录区在**文档流**里的位置：不受当前滚动量影响，所以先补回 scrollTop。
        const sectionTop =
          section.getBoundingClientRect().top -
          container.getBoundingClientRect().top +
          container.scrollTop;
        if (container.scrollTop > sectionTop + 24) {
          container.scrollTo({ top: Math.max(0, sectionTop - 8), behavior: 'instant' });
          return;
        }
      }
    }

    container.scrollTo({ top: 0, behavior: 'instant' });
  }, [channel, category, audience, sort]);

  /**
   * 封面不可得的卡片 id：从当前列表移除、不再展示。
   *
   * 判定条件是"确定不可得"——CoverImage 的三次加载重试已耗尽，或 guo 源的
   * 封面解析（Rust 侧带源侧 Referer 的下载）失败。切源/换筛选时整体重来：
   * 消失的卡片在下一次列表里重新获得一次出现机会（网络抖动不该永久除名）。
   */
  const [hiddenSeriesIds, setHiddenSeriesIds] = useState<Set<string>>(new Set());

  /**
   * 红果 App 的三块辅助数据：短剧榜、上新日历、演员粉丝榜。
   *
   * 与上面的「正在热播 / 新剧」（`useShelfFeed`，走 App 的榜单/最新 cell）不是同一
   * 份东西：`short_rank` 是**排行榜**接口，带名次、热度文案与「平台推荐」行；
   * `new_releases` 是**日历**接口，能区分「已上线」与「预告某天上线」；
   * `ranking` 是**演员**维度，列表里那两条都没有。
   *
   * 三块各自失败就各自留空：它们是首页的增补区，任何一块挂掉都不该影响目录网格。
   * 只对红果口径的频道（短剧 / 漫剧）取数——动漫专区是独立数据源，没有这些接口。
   */
  const [newReleaseRow, setNewReleaseRow] = useState<SeriesItem[]>([]);
  const [discoverRow, setDiscoverRow] = useState<SeriesItem[]>([]);
  /** AI 剧专用的 H5 热度榜（App 榜单没有 ai 口径，见下面的 effect）。 */
  const [aiRankRow, setAiRankRow] = useState<SeriesItem[]>([]);
  const [actorRank, setActorRank] = useState<HongguoActorRankItem[]>([]);
  const [showMoreHome, setShowMoreHome] = useState(false);
  /** 正在看主页的演员 id（null = 没有弹窗）。 */
  const [actorUserId, setActorUserId] = useState<string | null>(null);
  /** 筛选弹窗（App 原生筛选面板）。只有短剧 / 漫剧有对应档位。 */
  const [filterOpen, setFilterOpen] = useState(false);
  useEffect(() => {
    // adult / anime 整段跳过：发现流 / 上新日历 / 星粉榜这三个区块都是
    // **真人+漫剧两种口径**的 App 接口，它们连目录口径都没有。
    if (!showMoreHome || channel === 'adult' || channel === 'anime') {
      setNewReleaseRow([]);
      setActorRank([]);
      setDiscoverRow([]);
      setAiRankRow([]);
      return;
    }
    let alive = true;

    // AI 剧走另一条：三个 App 区块它一个都用不了（没有 ai 的 tab_type），
    // 但公开网页有真榜单 `/rank/hot-ai-drama`（实测 20 条/页 × 5 页）。
    // 没有这一段，AI 剧的「更多内容」区块会只剩目录、四个分区全空——
    // 那不是"内容少"，是"没去取"。
    if (channel === 'ai') {
      setNewReleaseRow([]);
      setActorRank([]);
      setDiscoverRow([]);
      void ipcService.catalog
        .list({
          channel: 'ai',
          category: '全部',
          audience: '全部',
          sort: 'heat',
          page: 1,
          pageSize: 12,
        })
        .then(page => {
          if (alive) setAiRankRow(page.items);
        })
        .catch(() => {
          if (alive) setAiRankRow([]);
        });
      return () => {
        alive = false;
      };
    }

    // 榜单与日历按频道取（漫剧走漫剧的榜）；演员榜只有真人剧口径有意义。
    const rankChannel = channel === 'comic' ? 'comic' : 'drama';

    // App 的发现流：漫剧频道取漫剧 tab，其余取推荐 tab。
    void ipcService.hongguo
      // **不能传 `recommend`**：那是真人 + 漫剧的混合流，短剧专区里会掺进漫画。
      .discover(rankChannel === 'comic' ? 'comic' : 'live')
      .then(page => {
        if (!alive) return;
        setDiscoverRow(page.items);
      })
      .catch(() => {
        if (alive) setDiscoverRow([]);
      });

    // 上新日历：**多页填充**（2026-11-07「有时候只有一个卡片，有时候很多」）。
    // 这颗接口是全站混合流，且每天每个时刻的"已上线 vs 预告"配比都不一样——
    // 内容类型过滤只放行 `subscribe_data.content_type` 确证相符的已上线条目，
    // 单页过滤后剩几条纯看运气。连翻最多 3 页、凑够 10 条就收手：请求代价
    // （3 次往返）换这一栏的卡片数量稳定，值得。
    void (async () => {
      try {
        const acc: Array<{ series: SeriesItem }> = [];
        let cursor: string | null = null;
        for (let page = 0; page < 3; page += 1) {
          const res = await ipcService.hongguo.newReleases({
            channel: rankChannel,
            cursor: cursor ?? undefined,
          });
          acc.push(...res.items);
          if (!res.hasMore || acc.length >= 10) break;
          cursor = res.cursor ?? null;
        }
        if (alive) setNewReleaseRow(acc.map(row => row.series));
      } catch {
        if (alive) setNewReleaseRow([]);
      }
    })();

    // 演员粉丝榜只在真人剧频道展示（2026-11-07）：红果星粉榜是真人演员口径，
    // 漫剧次元里挂真人演员榜，就是用户报告的"漫剧区看到真人内容"。
    if (rankChannel === 'drama') {
      void ipcService.hongguo
        .ranking({ channel: rankChannel })
        .then(page => {
          if (!alive) return;
          setActorRank(page.items.slice(0, 8));
        })
        .catch(() => {
          if (alive) setActorRank([]);
        });
    } else {
      setActorRank([]);
    }

    return () => {
      alive = false;
    };
  }, [channel, showMoreHome]);

  useEffect(() => {
    setHiddenSeriesIds(new Set());
  }, [activeSources, channel, category, audience, sort]);

/**
   * 封面不可得就把卡片移出列表。
   *
   * 用 `useCallback` + 空依赖：`SeriesCard` 是 `React.memo` 的，内联箭头函数会让 props
   * 每次渲染都不相等，memo 直接失效。上百张卡时这个差别就体现在滚动帧耗时上。
   */
  const markSeriesUnavailable = useCallback((seriesId: string) => {
    setHiddenSeriesIds(prev => {
      if (prev.has(seriesId)) return prev;
      const next = new Set(prev);
      next.add(seriesId);
      return next;
    });
  }, []);

  /**
   * 卡片点击：全部卡片共用这一个回调（卡片自己带上 seriesId）。
   *
   * `navigateTo` 已是稳定引用（见 useAppStore），所以这个回调也稳定。
   */
  const handleCardClick = useCallback((seriesId: string) => {
    navigateTo('detail', seriesId);
  }, [navigateTo]);
  const handleCardPrefetch = useCallback((seriesId: string) => {
    void ipcService.series.getDetail(seriesId).catch(() => {});
  }, []);
  /**
   * 进入某个分区的「更多」页。
   *
   * 频道是这一步的全部难点：`ShelfMoreView` 内部挂的是**另一份** `CatalogProvider`，
   * 它读不到发现页的频道状态，所以必须在这一刻把当前频道一起交出去。
   * 少传这一下，用户从「漫剧次元」点进去会看到短剧列表。
   *
   * 不再有刷新动作：两颗刷新按钮已按要求移除（它们刷新的是首屏那 6 张预览卡，
   * 而用户真正要的是"这一栏还有什么"，入口交给「更多」）。
   */
  const handleShelfMore = useCallback((kind: ShelfKind) => {
    openShelf(kind, channel);
  }, [openShelf, channel]);

  /**
   * 实际展示的卡片：无封面地址的条目直接过滤（guo 源的封面不来自该字段，
   * 走 CoverImage 的异步解析，不过滤）；加载失败的按 id 移除。
   */
  const visibleItems = items.filter(item => {
    /**
     * 频道必须匹配（2026-10-26 修「漫剧区的猜你喜欢显示短剧」）。
     *
     * 目录是异步聚合的，慢源超过 6000ms 时会「先用已有结果渲染」—— 那批"已有
     * 结果"是**上一个频道**的条目。`visibleItems` 既是货架/网格的来源，也是
     * 「猜你喜欢」的候选池，于是漫剧栏会拿短剧去推荐（本次实机：短剧区和漫剧区
     * 的猜你喜欢是同一部《人到中年》）。
     *
     * `adult` 是聚合名单、条目的 `type` 由源决定，不能按频道过滤。
     */
    if (channel !== 'adult' && item.type !== channel) return false;
    if (hiddenSeriesIds.has(item.id)) return false;
    if (!item.id.startsWith('guo:') && !(item.cover || '').trim()) return false;
    return true;
  });
  /**
   * 货架的数据源分两路。
   *
   * 短剧 / 漫剧走**红果 App 的榜单与最新上架**（与「更多」页同源、同一份首屏
   * 缓存）；18+ 没有对应的红果口径（那是本机侧"只启用成人源"的聚合概念），
   * 继续从本机启用源聚合出来的目录里切。
   *
   * 切出来那一套只服务神秘小窝；短剧/漫剧两栏不再碰 `visibleItems` —— 这也是
   * 用户报告"货架与它「更多」页内容对不上"的根源。
   */
  const feedShelves = shelfFeedSupports(channel);
  const showHomeSections = visibleItems.length >= 3;
  const hotItems = showHomeSections ? visibleItems.slice(0, 6) : [];
  const newItems = visibleItems.length >= 10 ? visibleItems.slice(6, 12) : [];
  const catalogStart = newItems.length > 0 ? 12 : showHomeSections ? 6 : 0;
  /**
   * 目录网格要不要让出前 12 条。
   *
   * 旧逻辑把这些条目藏起来，只因为"它们已经出现在货架上"；货架换成独立数据源后
   * 两者再无关系，继续让位等于**凭白吞掉 12 部剧**。
   */
  const catalogItems = feedShelves ? visibleItems : visibleItems.slice(catalogStart);

  /**
   * 「继续观看」与「猜你喜欢」并轨成同一栏的排布判定。
   *
   * `hasRandomCandidates` 用的是 `RandomWatchSection` 导出的**同一个筛选口径**，
   * 不是宿主自己再写一遍 `item.id && item.title`：口径一旦分叉，宿主会按"有内容"
   * 排成两列，而组件实际返回 `null`，结果「继续观看」孤零零占着半屏。
   *
   * 只剩一块时那一块独占整行——不为了对齐而留半屏空白。
   */
  const showContinueSlot = Boolean(continueWatching);
  /**
   * 「猜你喜欢」槽位是否占位。
   *
   * ## 为什么要把"还在加载"也算成占位（2026-10-18）
   *
   * 旧判定只有 `hasRandomCandidates(visibleItems)` —— 它看的是**已经到手**的
   * 目录条目。而目录是异步来的（要等 worker 冷启动），于是首屏时序变成：
   *
   *   1. 「继续观看」先渲染，因为 `mergedTwoColumns` 为 false，它**独占整行**；
   *   2. 目录数据到达 → `showRandomSlot` 翻成 true → 网格从一列变两列，
   *      「猜你喜欢」整块插进来；
   *   3. 用户看到的是「别的卡片都加载完了，这一块最后才弹出来」。
   *
   * 现在：只要**可能**会有候选（首屏还在 loading 时按"会有"处理），就先占位。
   * 骨架高度与真实卡片一致，数据到达只是把骨架换成内容，布局不移动。
   *
   * 只有"确实不可能有候选"才彻底不占位：
   *   - 首屏已加载完（`!isLoading`），且
   *   - 现有条目里没有任何合格候选（全被 historySeriesIds 排除了）。
   * 那种情况是永久的，留一个空槽没有意义。
   */
  /**
   * 「猜你喜欢」的候选池：目录条目 + 两路货架。
   *
   * ## 为什么要把货架并进来（2026-10-26 修「漫剧区的猜你喜欢无法显示」）
   *
   * 实机现场（0.2.40 安装版，漫剧次元）：
   *
   *   发现更多漫剧 (0 部)
   *   目录加载失败：目录数据加载失败（1 个源均无响应）—— hongguo: 等待超过 6 秒
   *
   * 而同一页的「正在热播 / 新剧」走的是**另一条接口**（红果 App 榜单），数据正常。
   * 此前候选池只看目录条目，目录一挂整块「猜你喜欢」就消失了 —— 用户看到的正是
   * "漫剧区的猜你喜欢不显示"。
   *
   * 现在把两路货架并进候选池：目录失败时仍有内容可推荐，目录正常时候选更多。
   * 去重后保持出现顺序（目录优先），保证"换一批"有足够多的轮换空间。
   */
  const hotShelf = useShelfFeed('hot', channel);
  const newShelf = useShelfFeed('new', channel);
  const randomPool = useMemo(() => {
    const seen = new Set<string>();
    const pool: SeriesItem[] = [];
    for (const item of [...visibleItems, ...hotShelf.items, ...newShelf.items]) {
      if (!item.id || seen.has(item.id)) continue;
      // 频道兜底（与 `visibleItems` 同一口径）：货架条目理论上是后端按频道
      // 过滤过的，但候选池是三路数据并流，任何一路混进串味条目，用户看到的
      // 就是"漫剧区的猜你喜欢推荐真人剧"——在池口再拦一遍，2026-11-07。
      if (channel !== 'adult' && item.type !== channel) continue;
      seen.add(item.id);
      pool.push(item);
    }
    return pool;
  }, [visibleItems, hotShelf.items, newShelf.items, channel]);

  const showRandomSlot = isLoading || hasRandomCandidates(randomPool, historySeriesIds);
  const mergedTwoColumns = showContinueSlot && showRandomSlot;

  /**
   * 进度条入场：从 0 长到真实进度，而不是一上来就杵在那儿。
   *
   * 两条约束必须守住：
   *   1. **用 transition，不用 keyframes。** 视图是常驻 DOM + `display:none` 的，
   *      在隐藏祖先里创建的 CSS animation 会永久卡在 0% 帧（见 `tailwind.config.js`
   *      里 `fluent-card-in` 那条长注释）。
   *   2. **先落回 0，隔一帧再抬起。** 同一批 state 更新会被 React 合并成一次渲染，
   *      合并后宽度根本没有"从 A 变到 B"的过程，过渡不会触发。
   *
   * 依赖里带 `currentView`：切回发现页时重放一次 —— 那才是用户真正看见它的时刻，
   * 而组件挂载时视图还是 `display:none`，那时跑动画等于白跑。
   */
  const [progressSettled, setProgressSettled] = useState(false);
  useEffect(() => {
    if (currentView !== 'explore' || !continueWatching) {
      setProgressSettled(false);
      return;
    }
    const timer = window.setTimeout(() => setProgressSettled(true), 60);
    return () => window.clearTimeout(timer);
  }, [currentView, continueWatching?.updatedAt]);

  /**
   * 「继续观看」那一行的文案。
   *
   * `WatchHistoryItem` 里有四个字段一直是死的，这里把它们捡回来：
   *   - `totalEpisodes` → "共 86 集"。用户续播时最想知道的就是"还剩多少要看"，
   *     而原来只给了"看到第 12 集"，缺的正是分母；
   *   - `durationSeconds - positionSeconds` → "还剩 8 分 20 秒"。这是续播场景里
   *     唯一真正有用的数字，比"已看 42%"有用得多；
   *   - 原来那行 `已看 42%` 和下面的进度条**说的是同一件事**，纯冗余，删掉；
   *   - `isFinished` → 看完的剧不该再喊"断点续播"，那是"重新播放"。
   */
  const resume = (() => {
    if (!continueWatching) return null;
    const totalEpisodes = continueWatching.totalEpisodes || 0;
    const remaining = Math.max(
      0,
      (continueWatching.durationSeconds || 0) - (continueWatching.positionSeconds || 0),
    );
    return {
      episodeLabel: totalEpisodes > 0
        ? `第 ${continueWatching.episodeNumber} 集 · 共 ${totalEpisodes} 集`
        : `第 ${continueWatching.episodeNumber} 集`,
      remainingLabel: continueWatching.isFinished ? '已看完' : formatRemaining(remaining),
      actionLabel: continueWatching.isFinished ? '重新播放' : '断点续播',
    };
  })();

  /**
   * 首页「继续观看」那张卡的**预签名 + 预转整集 HLS**。
   *
   * ## 为什么只做预签名不够（2026-11-08）
   *
   * 预签名只省掉「两次 App API 往返」（固定 2.2~2.5 秒），而用户点下去**真正等的**
   * 是转码：冷集首开要等 ffmpeg 起进程、写出 init 与首片，实测 `出画 4650ms`。
   * 命中已转好的产物时同一条链路只要 `257ms` —— 差 18 倍，而这 18 倍完全取决于
   * 用户点进来的那一刻盘上有没有 `vid-{vid}/index.m3u8`。
   *
   * 这张卡**从打开应用就一直在屏幕上**，预热窗口长达几分钟，比详情页（用户可能
   * 两秒就点进去）充分得多。此前它却只做了最便宜的那一半，等于把最贵的一段
   * 留给点击之后。现在把整集 HLS 预转也发起来。
   *
   * ## 三条边界
   *
   * 1. **不预转动漫 / guo 源**：动漫是流式 m3u8（无产物可转），guo 走 guo-core
   *    本地媒体服务，各有自己的预取链路。
   * 2. **走 store 的 `prewarmEpisode` 而不是直接调 IPC**：那条路径带着
   *    「播放中不预热」的闸门（`streamingInFlight()`）。首页常驻 DOM，用户在播放器里
   *    时 `currentView` 仍是 explore 的相邻渲染也可能触发这里 —— 直接发 IPC 会在
   *    正在播的那一集旁边凭空起第二路 ffmpeg，抢 NVENC（实测串行，两边都变慢）。
   * 3. **必须先签名再预转**：`prewarmStreamNative` 内部会调 `ensure_stream_cached`，
   *    与 `prefetchStream` 共用同一张在途表。两者同时发车会变成 leader/follower
   *    排队（白等一轮 API 往返），所以预转延后一小段，让签名先落地。
   *
   * 漫剧（comic）的 contentType 是 1004，与详情页 / 播放链路一致；短剧为 1。
   * 历史记录里的 `channel` 是落库值，缺失时按短剧处理。
   */
  const warmResumeRef = useRef<string | null>(null);
  /**
   * 供延时预热读到**最新**的视图状态。
   *
   * 预转延后 1.2 秒发车（理由见下），而定时器闭包捕获的是发车那一刻的
   * `currentView`。用户在这 1.2 秒里点进播放器时，闭包里的值仍是 `explore`，
   * 于是"首页后台预热"会和"播放器里正在播的那一集"抢同一块编码单元。
   * 用 ref 在回调触发时读真值，与详情页那条预热 effect 同一套写法。
   */
  const currentViewRef = useRef(currentView);
  currentViewRef.current = currentView;
  useEffect(() => {
    if (currentView !== 'explore' || !continueWatching) return;
    const item = continueWatching;
    if (!isHongguoSeriesId(item.seriesId)) return;
    // 动漫（dmghg / bfzy 前缀或 anime 频道）走 anime 分支的解析链路，预热对它无意义。
    if (item.channel === 'anime') return;
    // AI 剧只有公开网页目录这一条链路，`prefetchStream` 打的是 App API 的
    // content_type 口径（1 / 1004），对它既不命中也会白白占一次往返。
    if (item.channel === 'ai') return;
    if (item.seriesId.startsWith('dmghg:') || item.seriesId.startsWith('bfzy:')) return;
    // guo 源：播放走 guo-core 本地媒体服务，预转命令对它无效（也拿不到红果 vid）。
    if (item.seriesId.startsWith('guo:')) return;
    // 同一集只预热一次：continueWatching 会随进度更新而频繁变化，不设闸门会反复重发。
    const key = `${item.seriesId}:${item.episodeId}`;
    if (warmResumeRef.current === key) return;
    warmResumeRef.current = key;
    const contentType = contentTypeFor(item.channel);
    // 详情也要先暖上：`runOpenEpisode` 的第一跳就是 `series.getDetail`，日志实测
    // 「打开集」到「流式开播 请求开始」之间有 **640ms** 纯等待，全部花在这次往返上。
    // 首页的 `warmTopDetails` 只覆盖货架前 3 条，与「继续观看」指向的剧通常不是同一部。
    // `getDetail` 自带在途去重与缓存，用户真点下去时命中即可零等待。
    void ipcService.series.getDetail(item.seriesId).catch(() => undefined);
    void ipcService.playback.prefetchStream([item.episodeId], contentType);
    if (NATIVE_PLAYER_AVAILABLE && normalizePreferredPlayer(settings.preferredPlayer) === 'mpv') {
      return;
    }
    // 延后 1.2 秒：与详情页同一条理由（见 DetailView 的预热 effect）——先让签名跑完，
    // 预转命中同一条缓存，串行发起等于白等一轮 2.2 秒的 API 往返。
    const timer = window.setTimeout(() => {
      // 定时器到点时用户可能已经点走 / 切到别的页：再复查一次视图。
      if (currentViewRef.current !== 'explore') return;
      prewarmEpisode(item.seriesId, item.episodeId, contentType);
    }, 1200);
    return () => window.clearTimeout(timer);
  }, [currentView, continueWatching, prewarmEpisode, settings.preferredPlayer]);

  const startPreferredPlayback = useCallback(async (input: {
    seriesId: string;
    episodeId: string;
    startSeconds?: number;
    title?: string;
    episodeNumber?: number;
    type?: string | null;
    channel?: string | null;
  }) => {
    /** 落点判定统一口径（见 `services/playbackRouting.ts`）：先问落点，再决定动作。 */
    const route = planPlaybackRoute({
      preferredPlayer: settings.preferredPlayer,
      nativeAvailable: NATIVE_PLAYER_AVAILABLE,
      seriesId: input.seriesId,
      episodeId: input.episodeId,
      type: input.type,
      channel: input.channel,
      surface: 'explore',
    });
    if (route.kernel === 'native') {
      try {
        const launched = await tryLaunchNativePlayer({
          preferredPlayer: settings.preferredPlayer,
          autoNext: settings.autoNext,
          setAutoNext: on => { void updateSettings({ autoNext: on }); },
          surface: 'explore',
          ...input,
        });
        if (launched) return;
        // 计划说能交、实际没交出去：必须说出来，不能悄悄换成内置播放器。
        showToast(NATIVE_SKIPPED_NOTICE, 'warning');
      } catch (error) {
        console.warn('MPV 播放器打开失败，回退网页播放', error);
        showToast(NATIVE_OPEN_FAILED_NOTICE, 'error');
      }
    } else if (route.notice) {
      showToast(route.notice, 'warning');
    }
    if (
      input.type === 'anime'
      || input.channel === 'anime'
      || input.seriesId.startsWith('dmghg:')
      || input.seriesId.startsWith('bfzy:')
    ) {
      void openAnimeEpisode(input.seriesId, input.episodeId, input.startSeconds || 0);
      return;
    }
    navigateTo('player', input.seriesId);
    openEpisode(input.seriesId, input.episodeId, input.startSeconds || 0);
  }, [navigateTo, openAnimeEpisode, openEpisode, settings.preferredPlayer, settings.autoNext, updateSettings]);

  const handleRandomWatch = useCallback((series: SeriesItem, detail: SeriesDetail) => {
    /**
     * 选哪一集：**有进度就回到那一集，不再排除 `isFinished`**。
     *
     * 与 DetailView 的 `lastWatchedEp` 同一语义（2026-10-19 修「断点续播时有时无」）：
     * 旧写法排除 `isFinished`，而短剧一集只有 33–68 秒、看到 95% 就置位，
     * 于是"随机看"会把人带去第一集，而不是上次看到的地方。
     * 按集号取最大者 = 看得最远的那一集。
     */
    const episode =
      [...detail.episodes]
        .filter(item => (item.watchedSeconds || 0) > 0)
        .sort((left, right) => right.episodeNumber - left.episodeNumber)[0] ||
      detail.episodes[0];
    if (!episode) return;
    void startPreferredPlayback({
      seriesId: series.id,
      episodeId: episode.id,
      startSeconds: episode.watchedSeconds || 0,
      title: detail.title,
      episodeNumber: episode.episodeNumber,
      type: detail.type,
    });
  }, [startPreferredPlayback]);
  const loadMoreSentinelRef = useRef<HTMLDivElement | null>(null);

  /**
   * 补一次提交后的自检：卡片追加后如果哨兵**仍在**可视区，就直接再提交一页。
   *
   * IntersectionObserver 只在“穿过阀值”时回调，而提交追加的内容可能不足以把哨兵推出
   * rootMargin（典型：本轮只多出两三条，其余源都已到底）。这种情况下既不会产生新的
   * 交叉事件，`loadMore` 也不会自己再来一次 —— 表现为“滚到底停住了，再往上推一下
   * 才有反应”。真正无限流不能有这种一推一顿，所以按实际几何位置兜一次底。
   */
  const handleBackstopLoadMore = useCallback(() => {
    void loadMore('');
  }, [loadMore]);

  usePrefetchBackstop({
    scrollRef: scrollContainerRef,
    sentinelRef: loadMoreSentinelRef,
    enabled: hasMore && !error,
    isLoadingMore,
    progress: pagesLoaded,
    bufferPx: 1400,
    onLoadMore: handleBackstopLoadMore,
  });

  useEffect(() => {
    const sentinel = loadMoreSentinelRef.current;
    const root = scrollContainerRef.current;
    if (!sentinel || !root || !hasMore) return;

    // 两条线，职责不同：
    // ① `prefetch` 哨兵离底部 1800px 就触发，**不碰任何界面状态**——它只是把请求提前
    //    发出，让滚动不再等网络。store 里还有一条空闲预取做双保险，两者靠
    //    `beginFetch` 的"已有在途就复用同一个 Promise"天然幂等，不会重复打站方。
    // ② `commit` 哨兵在 800px 提前提交那一页；此时数据多数已在手，提交是同步的。
    //    所以底部提示条只在网络确实没赶上时闪一下，而不是每次滚动都停一下。
    //
    // 注意不再拿 isLoading / isLoadingMore 当守卫：它们进依赖会让 observer 随每次
    // 提交重建，而哨兵若仍在窗口内会立刻再触发一次，等于把提交时机交给重建节奏。
    // 单飞由 store 的 isLoadingMore 负责，这里不需要重复把关。
    const prefetchObserver = new IntersectionObserver(
      entries => {
        if (entries.some(entry => entry.isIntersecting)) prefetchMore('');
      },
      { root, rootMargin: '1800px 0px', threshold: 0 },
    );
    const commitObserver = new IntersectionObserver(
      entries => {
        if (entries.some(entry => entry.isIntersecting)) {
          void loadMore('');
        }
      },
      { root, rootMargin: '800px 0px', threshold: 0.01 },
    );
    prefetchObserver.observe(sentinel);
    commitObserver.observe(sentinel);
    return () => {
      prefetchObserver.disconnect();
      commitObserver.disconnect();
    };
  }, [hasMore, loadMore, prefetchMore]);


  return (
    <div ref={scrollContainerRef} className="relative flex h-full min-w-0 flex-1 flex-col gap-5 overflow-x-hidden overflow-y-auto p-5 select-none" aria-busy={isLoading}>
      {/*
        加载进度线：贴在页首内容顶部（筛选按钮上方），**随页面滚动，不吸顶**。

        之前是 `sticky top-0`：目录在深处重载时（典型：滚到底点了「重试」），这条
        线会钉在视口最上沿、悬在正在浏览的卡片上不动——用户报告"进度条位置不正确，
        上下滚动它不会动，出现错位"（2026-11-07）。改回普通流内元素：h-0 + 负
        margin 让它不占版心高度，出现/消失都不推挤布局；位置永远在页首该在的地方，
        滚上去了自然看得见，滚下去了就跟着走。
      */}
      {isLoading && (
        <div className="pointer-events-none -mb-5 h-0">
          <div className="relative h-0.5 w-full overflow-hidden rounded-full bg-blue-100/60">
            <div className="absolute inset-y-0 left-0 w-1/3 animate-[loading-progress_1.2s_ease-in-out_infinite] rounded-full bg-blue-500" />
          </div>
        </div>
      )}
      {/* 顶部：频道 Tab 与 核心筛选 */}
      <div className="flex min-w-0 flex-col gap-3.5">
          {/* 频道切换与核心筛选 */}
        <div className="flex min-w-0 flex-wrap items-center gap-2.5">
          {/* 短剧 / 漫剧 Tab (嵌入式凹槽托盘) */}
          <div className="p-1 bg-slate-100/90 rounded-xl border border-slate-200/70 shadow-inner flex items-center gap-1">
            <button
              type="button"
              onClick={() => setChannel('drama')}
              className={`flex items-center gap-1.5 px-4 py-1.5 rounded-lg text-xs font-bold transition-all duration-150 cursor-pointer ${
                channel === 'drama'
                  ? 'fluent-convex-tab text-blue-600'
                  : 'text-slate-600 hover:text-slate-900 hover:bg-white/50'
              }`}
            >
              <span>短剧专区</span>
            </button>
            <button
              type="button"
              onClick={() => setChannel('comic')}
              className={`flex items-center gap-1.5 px-4 py-1.5 rounded-lg text-xs font-bold transition-all duration-150 cursor-pointer ${
                channel === 'comic'
                  ? 'fluent-convex-tab text-blue-600'
                  : 'text-slate-600 hover:text-slate-900 hover:bg-white/50'
              }`}
            >
              <Sparkles className="w-3.5 h-3.5" />
              <span>漫剧次元</span>
            </button>
            {/* AI 剧频道：站点的第 4 个频道段（`/category/ai-drama`）。
                内容全部来自公开网页目录，不走 App 的发现流——那条链路只有真人/
                漫剧两种 tab_type，硬塞会把真人剧混进来。 */}
            <button
              type="button"
              onClick={() => setChannel('ai')}
              className={`flex items-center gap-1.5 px-4 py-1.5 rounded-lg text-xs font-bold transition-all duration-150 cursor-pointer ${
                channel === 'ai'
                  ? 'fluent-convex-tab text-violet-600'
                  : 'text-slate-600 hover:text-slate-900 hover:bg-white/50'
              }`}
            >
              <Bot className="w-3.5 h-3.5" />
              <span>AI 剧</span>
            </button>
            {/* 神秘小窝：只在设置页打开 18+ 总开关后才出现，且**不留痕**——
                用户关掉开关时这个 tab 整体消失，而不是留在那儿点进去空空如也。
                它用玫红色而不是蓝色：这是与前两个专区语义完全不同的一类内容，
                颜色上就该一眼分得开。 */}
            {showAdultSources && (
              <button
                type="button"
                onClick={() => setChannel('adult')}
                className={`flex items-center gap-1.5 px-4 py-1.5 rounded-lg text-xs font-bold transition-all duration-150 cursor-pointer ${
                  channel === 'adult'
                    ? 'fluent-convex-tab text-rose-600'
                    : 'text-slate-600 hover:text-slate-900 hover:bg-white/50'
                }`}
              >
                <Moon className="w-3.5 h-3.5" />
                <span>神秘小窝</span>
              </button>
            )}
          </div>

        </div>

        {/* 常驻题材条：题材平铺成 chips（一步换品类），体裁与进阶筛选收在右侧
            那颗按钮的面板里。整条 `sticky top-0`，滚到页面深处也够得到——容器、
            溢出箭头与关闭时机全部收口在 CategoryBar，见那里的注释。

            `scrollRootRef` 只用来判断"这条已经吸顶了没有"（吸顶后底色加深、
            投影出来）。不给也能用，只是那条一直停在淡底状态。 */}
        {/* 排序档：**只有「热度榜」是真开关**。
            `sort=heat` 会让后端走 `/rank/hot-{频道}` 这条独立路由（实测每页 20 部
            × 5 页），而不是 `/category/*`。而 `latest` 不做入口：公开网页目录
            实测不认任何排序参数（`?sort=online_time` / `?sort=latest` / `?order=new`
            返回的卡片与不带参数完全相同），给它一个按钮就是又造一个"点了没反应"。
            动漫在 H5 没有榜单，一并隐藏。 */}
        {(channel === 'drama' || channel === 'comic' || channel === 'ai') && (
          <div className="ml-auto flex items-center gap-1 rounded-xl border border-slate-200/70 bg-slate-100/80 p-1 shadow-inner">
            {([
              ['recommend', '综合'],
              ['heat', '热度榜'],
            ] as const).map(([value, label]) => (
              <button
                key={value}
                type="button"
                onClick={() => setSort(value)}
                aria-pressed={sort === value}
                className={`rounded-lg px-3 py-1 text-[11px] font-bold transition-all duration-150 cursor-pointer ${
                  sort === value
                    ? 'fluent-convex-tab text-blue-600'
                    : 'text-slate-500 hover:bg-white/60 hover:text-slate-800'
                }`}
              >
                {label}
              </button>
            ))}
          </div>
        )}
        <CategoryBar
          categories={categories}
          value={category}
          onChange={setCategory}
          audience={audience}
          onAudienceChange={setAudience}
          scrollRootRef={scrollContainerRef}
          onAdvanced={
            // 「高级筛选」是 App 原生筛选面板（`hongguo_short_filter_panel`），
            // 只有真人/漫剧两种口径。AI 剧没有对应档位，与其弹一个永远空的
            // 面板，不如不给入口。
            channel !== 'anime' && channel !== 'adult' && channel !== 'ai'
              ? () => setFilterOpen(true)
              : undefined
          }
        />
      </div>

      {/*
        「继续观看」+「猜你喜欢」并轨成同一栏。

        这两块原本各占一整行：上面那条横幅右侧本来就是一大片空白，下面那个随机推荐
        又被压在 100px 的小条带里、封面只有 48×64、字号 10–11px。并轨后共用同一行
        高度——页面纵向预算不但没增加，还少了一个独立区段。

        两张卡共用 `PICK_CARD_SHELL`（从 `RandomWatchSection` 导出，见那里的注释），
        差异只允许留在配色：**蓝 = 进度语义，紫 = 随机语义**。左侧那条 3px 色标是
        并排时唯一需要被看见的区别，其余骨架逐字一致。

        这一轮去掉了两样东西：
          - **继续观看的叉号**。它是"可选横幅"时代的遗留 —— 现在这一行是页首固定的
            "为你"区，收起一张卡只会让另一张变宽，语义上不成立。
          - 外层的 `justify-between`。动作区靠 `flex-1` 的信息列自然顶到右边，不需要
            外层再分一次位置。

        窄窗口（<1180px）自动退回上下堆叠，信息层级与合并前一致。
      */}
      {(showContinueSlot || showRandomSlot) && (
        <div className={`grid shrink-0 grid-cols-1 gap-4 ${mergedTwoColumns ? 'min-[1180px]:grid-cols-2' : ''}`}>
          {showContinueSlot && continueWatching && resume && (
            <div
              onClick={() => {
                void startPreferredPlayback({
                  seriesId: continueWatching.seriesId,
                  episodeId: continueWatching.episodeId,
                  startSeconds: continueWatching.positionSeconds,
                  title: continueWatching.title,
                  episodeNumber: continueWatching.episodeNumber,
                  channel: continueWatching.channel,
                });
              }}
              className={`${PICK_CARD_SHELL} cursor-pointer bg-gradient-to-r from-blue-50/80 via-white/92 to-white/88`}
            >
              {/* 左侧色标：与「猜你喜欢」的紫条对称，是并排时唯一允许存在的语义色差 */}
              <span aria-hidden="true" className="absolute inset-y-3 left-0 w-[3px] rounded-r-full bg-blue-500" />

              {/* 封面：严格 3:4（128×96），与「猜你喜欢」逐像素等高 */}
              <div className="relative h-32 w-24 shrink-0 overflow-hidden rounded-xl border border-white/90 bg-slate-100 shadow-xs">
                <CoverImage
                  key={continueWatching.seriesId}
                  src={continueWatching.seriesCover}
                  title={continueWatching.title}
                  placeholderTextClassName="text-2xl"
                  className="transition-transform duration-300 group-hover:scale-105"
                  loading="eager"
                
                resolveSrc={coverResolver(continueWatching.seriesId, continueWatching.seriesCover)}
          rawUsable={coverRawUsable(continueWatching.seriesId)}/>
                <div className="absolute inset-0 bg-black/10 transition-colors group-hover:bg-black/0" />
              </div>

              <div className="flex min-w-0 flex-1 flex-col justify-center gap-1.5">
                {/* `self-start`：徽章是 inline-flex，不给它定位就会被列方向拉伸成整条 */}
                <StatusBadge
                  label={continueWatching.isFinished ? '已看完' : '继续观看'}
                  variant="blue"
                  size="sm"
                  dot
                  className="self-start"
                />
                <span
                  className="truncate text-base font-extrabold tracking-tight text-slate-900"
                  title={continueWatching.title}
                >
                  {continueWatching.title}
                </span>
                <p className="truncate text-[11.5px] font-medium text-slate-500">
                  {resume.episodeLabel}
                </p>
                {/* 进度条宽度跟着并轨栏一起收放；右侧补上真正有用的那个数字。
                    宽度由 `progressSettled` 驱动做一次 0 → 真实值的入场，
                    所以这里给 700ms 而不是常用的 300ms。 */}
                <div className="mt-0.5 flex items-center gap-2.5">
                  <div className="h-1.5 min-w-0 max-w-[280px] flex-1 overflow-hidden rounded-full bg-slate-200/80 sm:max-w-[360px]">
                    <div
                      className="h-full rounded-full bg-blue-600 transition-all duration-700 ease-out"
                      style={{
                        width: progressSettled
                          ? `${Math.max(continueWatching.progressPercent || 0, continueWatching.positionSeconds > 0 ? 3 : 0)}%`
                          : '0%',
                      }}
                    />
                  </div>
                  {resume.remainingLabel ? (
                    <span className="shrink-0 text-[10.5px] font-semibold text-slate-400">
                      {resume.remainingLabel}
                    </span>
                  ) : null}
                </div>
              </div>

              <div className="flex shrink-0 items-center">
                <FluentButton
                  variant="primary"
                  size="sm"
                  icon={<Play className="w-3.5 h-3.5 fill-current" />}
                  onClick={(e) => {
                    e.stopPropagation();
                    void startPreferredPlayback({
                      seriesId: continueWatching.seriesId,
                      episodeId: continueWatching.episodeId,
                      startSeconds: continueWatching.positionSeconds,
                      title: continueWatching.title,
                      episodeNumber: continueWatching.episodeNumber,
                      channel: continueWatching.channel,
                    });
                  }}
                >
                  {resume.actionLabel}
                </FluentButton>
              </div>
            </div>
          )}

          {showRandomSlot && (
            <RandomWatchSection
              items={randomPool}
              excludeIds={historySeriesIds}
              onWatch={handleRandomWatch}
              onDetail={handleCardClick}
              isMerged={mergedTwoColumns}
              isLoading={isLoading}
            />
          )}
        </div>
      )}

      {/* 首页首屏只保留热播与新剧，榜单和日历收进“更多内容”避免信息堆叠。 */}
      {feedShelves ? (
        <>
          {/* 两栏直接吃红果 App 的榜单 / 最新上架（与各自「更多」页同源）。
              `key` 必须带 channel：换频道要整体重挂，否则会有一帧显示上一频道的
              内容（`useShelfFeed` 的状态只随挂载初始化）。 */}
          <HomeShelfSection
            key={`hot-${channel}`}
            kind="hot"
            channel={channel}
            title="正在热播"
            subtitle={`红果热播榜 · ${CHANNEL_COPY[channel].noun}`}
            icon={Flame}
            iconTone="rose"
            onMore={() => handleShelfMore('hot')}
            onClick={handleCardClick}
          />

          <HomeShelfSection
            key={`new-${channel}`}
            kind="new"
            channel={channel}
            title="新剧"
            subtitle={`红果最新上架 · ${CHANNEL_COPY[channel].noun}`}
            accent="blue"
            icon={Sparkles}
            iconTone="blue"
            onMore={() => handleShelfMore('new')}
            onClick={handleCardClick}
          />
        </>
      ) : (
        <>
          {/* 神秘小窝：红果 App 接口没有 18+ 口径，仍从本机启用源聚合出来的目录切。
              `loading` 必须传：`HomeShelf` 在 `!loading && items.length === 0`
              时返回 null（不占位），而这里的数据是异步聚合来的 —— 不传 loading
              的话，首屏会先没有这两栏、等目录到达才整块插入，把下方网格顶下去。
              与「猜你喜欢」那次是同一类抖动（2026-10-18 一并修）。 */}
          <HomeShelf
            title="正在热播"
            subtitle={
              // AI 剧没有 App 榜单口径（`rankingSupports('ai')` 为 false），
              // 这一栏排的是公开目录本身，不是"热度靠前"——文案不能说反了。
              channel === 'ai'
                ? 'AI 剧公开目录 · 按站方顺序'
                : `当前专区热度靠前的${CHANNEL_COPY[channel].noun}`
            }
            icon={Flame}
            iconTone="rose"
            items={hotItems}
            loading={isLoading}
            onClick={handleCardClick}
            onUnavailable={markSeriesUnavailable}
            onMore={() => handleShelfMore('hot')}
          />

          <HomeShelf
            title="新剧"
            subtitle="最近加入的内容"
            icon={Sparkles}
            iconTone="blue"
            items={newItems}
            loading={isLoading}
            onClick={handleCardClick}
            onUnavailable={markSeriesUnavailable}
            onMore={() => handleShelfMore('new')}
            accent="blue"
          />
        </>
      )}

      <section className="flex shrink-0 flex-col gap-3 rounded-2xl border border-white/70 bg-white/35 p-3">
        <button
          type="button"
          onClick={() => setShowMoreHome(value => !value)}
          className="flex w-full items-center justify-between rounded-xl px-2 py-1 text-left transition-colors hover:bg-white/60"
          aria-expanded={showMoreHome}
        >
          <span className="flex items-center gap-2">
            <ShelfBadge icon={Compass} tone="violet" />
            <span>
              <span className="block text-sm font-extrabold text-slate-800">更多内容</span>
              <span className="mt-0.5 block text-[11px] text-slate-400">榜单、上新日历与专题入口</span>
            </span>
          </span>
          <ChevronRight className={`h-4 w-4 text-slate-400 transition-transform ${showMoreHome ? 'rotate-90' : ''}`} />
        </button>

        {showMoreHome && (
          <div className="flex flex-col gap-5 pt-1">
            {/* 专题入口覆盖短剧 / 漫剧 / AI 剧三个红果频道 */}
            {channel !== 'adult' && channel !== 'anime' && (
              <div className="flex flex-wrap items-center gap-2 rounded-xl bg-white/55 p-2">
                <span className="mr-1 text-[11px] font-semibold text-slate-400">专题</span>
                <button type="button" onClick={() => openSpotlight('recommend', channel)} className="inline-flex items-center gap-1 rounded-lg px-2.5 py-1.5 text-xs font-semibold text-slate-600 transition hover:bg-white hover:text-blue-600">
                  <Flame className="h-3 w-3" /> 综合推荐
                </button>
                <button type="button" onClick={() => openSpotlight('latest', channel)} className="inline-flex items-center gap-1 rounded-lg px-2.5 py-1.5 text-xs font-semibold text-slate-600 transition hover:bg-white hover:text-blue-600">
                  <Clock className="h-3 w-3" /> 最新上线
                </button>
                <button type="button" onClick={() => openSpotlight('heat', channel)} className="inline-flex items-center gap-1 rounded-lg px-2.5 py-1.5 text-xs font-semibold text-slate-600 transition hover:bg-white hover:text-blue-600">
                  <TrendingUp className="h-3 w-3" /> 热度榜单
                </button>
              </div>
            )}

            <RankingSection
              key={`rank-${channel}`}
              channel={channel}
              onClick={handleCardClick}
              onMore={() => openSpotlight('heat', channel)}
            />

            {/* AI 剧的 H5 热度榜：App 榜单没有 ai 口径，这条走 `/rank/hot-ai-drama`。 */}
            {aiRankRow.length > 0 && (
              <HomeShelf
                title="AI 剧热度榜"
                subtitle="红果热度榜 · 服务端排序"
                icon={Flame}
                iconTone="violet"
                items={aiRankRow}
                onMore={() => openSpotlight('heat', 'ai')}
                onClick={handleCardClick}
              />
            )}

            {discoverRow.length > 0 && (
              <HomeShelf
                title={`App 推荐${CHANNEL_COPY[channel].noun}`}
                subtitle="红果发现流 · 实时刷新"
                icon={Compass}
                iconTone="violet"
                items={discoverRow}
                onClick={handleCardClick}
              />
            )}

            {newReleaseRow.length > 0 && (
              <HomeShelf
                title="上新日历"
                subtitle={`今天上线的${CHANNEL_COPY[channel].noun}`}
                icon={CalendarDays}
                iconTone="teal"
                items={newReleaseRow}
                onClick={handleCardClick}
                accent="blue"
              />
            )}

            {actorRank.length > 0 && (
              <section className="flex shrink-0 flex-col gap-3">
                <div className="flex items-start gap-2">
                  <span className="mt-1 h-5 w-1 rounded-full bg-amber-500" aria-hidden="true" />
                  <ShelfBadge icon={Users} tone="amber" />
                  <div>
                    <h2 className="text-base font-extrabold tracking-tight text-slate-900">演员粉丝榜</h2>
                    <p className="mt-0.5 text-[11px] text-slate-400">红果星粉榜 · 按得票排序</p>
                  </div>
                </div>
                <div className="flex gap-3 overflow-x-auto pb-1 ttv-scroll ttv-no-scrollbar">
                  {actorRank.map(actor => (
                    <button
                      key={`${actor.userId}-${actor.index}`}
                      type="button"
                      onClick={() => actor.userId && setActorUserId(actor.userId)}
                      className="flex w-32 shrink-0 flex-col items-center gap-2 rounded-2xl border border-white/80 bg-white/88 p-3 text-center shadow-fluent transition-colors hover:bg-white cursor-pointer"
                      title={actor.actorName}
                    >
                      <div className="relative h-16 w-16 overflow-hidden rounded-full border border-white bg-slate-100">
                        <CoverImage
                          src={actor.actorAvatar}
                          title={actor.actorName}
                          placeholderTextClassName="text-lg"
                          className="h-full w-full object-cover"
                        />
                      </div>
                      <span className="w-full truncate text-xs font-bold text-slate-800">{actor.actorName || '未署名'}</span>
                      <span className="text-[10.5px] text-slate-400">{actor.voteCount > 0 ? `${actor.voteCount} 票` : `第 ${actor.index} 位`}</span>
                    </button>
                  ))}
                </div>
              </section>
            )}
          </div>
        )}
      </section>

      {/* 剧集目录网格 (更舒展大气的卡片尺寸：4~6列排布)
          `catalogSectionRef` 只服务于上面那段"换题材后停在哪儿"的逻辑。 */}
      <div ref={catalogSectionRef} className="flex flex-col gap-3">
        <div className={`flex items-center justify-between ${showHomeSections && catalogItems.length === 0 ? 'hidden' : ''}`}>
          <h2 className="text-sm font-bold text-slate-800 flex items-center gap-1.5">
            <ShelfBadge icon={LayoutGrid} tone="indigo" />
            <span>发现更多{CHANNEL_COPY[channel].noun}</span>
            {/* 当前题材在这里再写一遍：滚到目录深处时，标题是唯一说明"这 300 部
                为什么是这 300 部"的地方（条虽然吸顶，但远在视野上方）。 */}
            {categories.length > 0 && category !== categories[0] && (
              <span className="rounded-md bg-blue-50 px-1.5 py-0.5 text-[11px] font-bold text-blue-600">
                {category}
              </span>
            )}
            <span className="text-xs font-normal text-slate-400">({catalogItems.length} 部)</span>
          </h2>
          <span className="text-[11px] text-slate-400">向下滚动自动加载</span>
        </div>

        {/* 骨架屏加载态 (保持相同舒适比例) */}
        {isLoading && items.length === 0 ? (
          <div className={SERIES_GRID_CLASS}>
            {Array.from({ length: 12 }).map((_, i) => <SeriesCardSkeleton key={i} />)}
          </div>
        ) : error && items.length === 0 ? (
          <div className="py-20 flex flex-col items-center justify-center text-center gap-3">
            <p className="text-sm font-medium text-slate-600">目录加载失败</p>
            {/* 真实原因必须露出来。原来只有上面那一行通用文案，用户截图里只剩
                "目录加载失败"四个字 —— 到底是源全挂了、超时、还是命令不存在，
                一个都分不出来，只能靠猜。 */}
            <p className="text-xs text-slate-400 max-w-lg leading-relaxed break-words">{error}</p>
            <FluentButton size="sm" onClick={() => refreshCatalog('')}>重试</FluentButton>
          </div>
        ) : items.length === 0 ? (
          /* 空结果态 */
          <div className="py-20 flex flex-col items-center justify-center text-center gap-3">
            <img src="/app-icon.png" alt="" className="w-12 h-12 object-contain opacity-60" draggable={false} />
            {channel === 'adult' ? (
              <>
                <p className="text-sm font-medium text-slate-600">还没有勾选成人内容源</p>
                <p className="text-xs text-slate-400">到「系统设置 → 视频源 → 18+ 成人内容」里勾选，这里就会出现内容</p>
              </>
            ) : (
              <>
                <p className="text-sm font-medium text-slate-600">没有找到匹配的{CHANNEL_COPY[channel].noun}</p>
                <p className="text-xs text-slate-400">尝试更换关键词或分类筛选项</p>
              </>
            )}
          </div>
        ) : (
          /* 舒展大气的剧集卡片网格 (带级联入场动画与平滑交互) */
          <>
          {isLoading && (
            <div className="mb-2 flex items-center gap-2 text-[11px] font-semibold text-blue-600">
              <span className="w-3 h-3 rounded-full border-2 border-blue-500/25 border-t-blue-600 animate-spin" />
              <span>正在切换，卡片马上回来…</span>
            </div>
          )}
          {/* 切换题材/排序：原地铺一层与真实卡片同构的骨架网格，而不是把旧卡片压暗。
              压暗（`opacity-40 saturate-50`）留下的是"脏灰的旧封面"，与骨架、与旁边
              的卡片都是两种材质，整页看起来像加载坏了；骨架与卡片同构，读起来只是
              "这批结果正在换"。张数取旧卡片数（不是固定的 12）——换回去的那一刻
              网格高度一致，滚动位置不会跳。骨架不可点，用户也不会点进上一批的剧。 */}
          {isLoading ? (
            <div className={SERIES_GRID_CLASS}>
              {Array.from({ length: catalogItems.length }).map((_, i) => (
                <SeriesCardSkeleton key={i} />
              ))}
            </div>
          ) : (
            <div className={`${SERIES_GRID_CLASS} ttv-grid-in`}>
              {catalogItems.map((series, index) => (
                <SeriesCard
                  key={series.id}
                  series={series}
                  index={index}
                  onClick={handleCardClick}
                  onUnavailable={markSeriesUnavailable}
                  onPrefetch={handleCardPrefetch}
                />
              ))}
            </div>
          )}
          </>
        )}

        <div ref={loadMoreSentinelRef} className="min-h-12 flex items-center justify-center pt-1" aria-live="polite">
          {isLoadingMore && (
            <div className="px-4 py-2 rounded-xl bg-white/70 border border-slate-200/70 text-xs text-slate-500 shadow-xs">
              正在加载更多内容…
            </div>
          )}
          {!hasMore && items.length > 0 && (
            error ? (
              // 翻页连续失败后熔断（见 loadMoreFailuresRef）会走到这里：光写
              // “已加载全部”是撒谎，必须把重试入口给回用户。
              <button
                type="button"
                onClick={() => { void refreshCatalog(''); }}
                className="px-3 py-1.5 rounded-xl bg-white/70 border border-slate-200/70 text-xs text-slate-500 shadow-xs hover:text-slate-700 hover:bg-white cursor-pointer"
              >
                加载更多失败，点此重试
              </button>
            ) : dedupedStop ? (
              // 站方说还有下一页，但整页卡片都被去重吃掉了（多源页码错位，或
              // 站方对越界页码回退到同一页）。继续自动请求只会空转，所以停了；
              // 但它**不是**"已加载全部"，给一条重试让用户能再推一次。
              <button
                type="button"
                onClick={() => { void refreshCatalog(''); }}
                className="px-3 py-1.5 rounded-xl bg-white/70 border border-slate-200/70 text-xs text-slate-500 shadow-xs hover:text-slate-700 hover:bg-white cursor-pointer"
              >
                没有更多新内容了，点此重新加载
              </button>
            ) : (
              <span className="text-[11px] text-slate-400">已加载全部公开内容</span>
            )
          )}
        </div>
      </div>

      {/* 回到顶部：滚过阈值才出现。作为滚动容器的最后一个粘性子元素，不占独立行高。 */}
      <BackToTop targetRef={scrollContainerRef} />

      {/* 演员主页：由演员粉丝榜的卡片打开。点作品走 `handleCardClick`，
          这样折叠过的系列同样会先弹选季窗口，行为与首页其它入口一致。 */}
      {actorUserId && (
        <PersonDialog
          userId={actorUserId}
          onOpenSeries={seriesId => {
            setActorUserId(null);
            handleCardClick(seriesId);
          }}
          onClose={() => setActorUserId(null)}
        />
      )}

      {/* 筛选弹窗：结果里点卡片同样走 `handleCardClick`，与其它入口行为一致。 */}
      {filterOpen && (
        <ShortFilterDialog
          channel={channel === 'comic' ? 'comic' : 'drama'}
          onOpenSeries={seriesId => {
            setFilterOpen(false);
            handleCardClick(seriesId);
          }}
          onClose={() => setFilterOpen(false)}
        />
      )}
    </div>
  );
};
