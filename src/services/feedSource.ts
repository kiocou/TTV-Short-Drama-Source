import type { HongguoDiscoverChannel, HongguoSeriesDetail } from './ipc';
import type { CatalogFilter, ChannelType, SeasonApiEntry, SeriesItem } from '../types/catalog';
import type { WatchHistoryItem } from '../types/history';
import type { EpisodeItem, SeriesDetail } from '../types/series';
import { getSeriesDetail, MOCK_SERIES_LIST } from './mockData.ts';
import { parseSeasonLabel } from './seasonLabel.ts';

/**
 * 推荐页的频道。
 *
 * `ai` 是站点第 4 个频道段（`/category/ai-drama`），但它**没有 App 通道**：
 * `discover` 的 `tab_type` 只有 -1/39/36/38/32/8。所以 AI 剧的推荐流不能走
 * `hongguo_discover`，必须由本文件用公开网页目录拼（见 `fetchAiFeedPage`）。
 */
export type FeedChannel = 'drama' | 'comic' | 'ai';

/**
 * 推荐流一页的数据来源。
 *
 * 为什么要把"来源"显式化：`ai` 与另外两条频道取数的接口、分页模型、排序语义
 * 全都不同（App 游标 vs 网页页码、混合推荐流 vs 站方热度榜）。如果只按频道名
 * 隐式分支，后来的人看不出"这一页到底是哪条链路给的"，也没法单独给 AI 那条补台架。
 */
export type FeedPageSource = 'app-discover' | 'web-catalog';

export function feedPageSource(channel: FeedChannel): FeedPageSource {
  return channel === 'ai' ? 'web-catalog' : 'app-discover';
}

/**
 * 推荐页对外文案。
 *
 * 现象：AI 剧那条是**站方热度顺序 + 目录顺序**，不是个性化推荐。此前所有频道
 * 共用同一句"为你推荐"，AI 剧进来之后这句话就是假的。
 * 根因：文案写死在组件里，没有跟着数据源走。
 * 取舍：文案作为数据（`FeedChannelCopy`）由 hook 返回值带出，组件只负责渲染；
 * 以后改措辞不用同时改三个组件，也不会出现"某个频道忘了改"。
 */
export interface FeedChannelCopy {
  /** 频道名，用于标题/切换器。 */
  label: string;
  /** 这一页的数据来源与排序说明，直接给用户看。 */
  note: string;
}

export function feedChannelCopy(channel: FeedChannel): FeedChannelCopy {
  switch (channel) {
    case 'ai':
      return { label: 'AI 剧', note: '按站点热度排列，不是个性化推荐' };
    case 'comic':
      return { label: '漫剧', note: '红果漫剧公开榜单' };
    default:
      return { label: '真人短剧', note: '红果真人短剧推荐流' };
  }
}

export interface FeedDetail extends SeriesDetail {
  isMultiSeason: boolean;
}

export type FeedGesture = 'next' | 'previous';

/**
 * 真人推荐单独走 live；recommend 是短剧与漫剧混合流。
 *
 * `ai` 返回 `null`：App 的 `tab_type` 里没有 AI 剧这一档（-1/39/36/38/32/8），
 * 硬塞进 `live` 只会拿到真人剧卡片。调用方必须先看 `feedPageSource()`，
 * 这里的 `null` 是**显式的"没有这条通道"**，不是"取默认值"。
 */
export function discoverChannelForFeed(channel: FeedChannel): HongguoDiscoverChannel | null {
  if (channel === 'ai') return null;
  return channel === 'comic' ? 'comic' : 'live';
}

/** 详情返回的 item.type 是应用频道，与 discover 使用的 tab 字面量不同。 */
export function detailChannelForFeed(channel: FeedChannel): FeedChannel {
  return channel;
}

/** 队列去重，同时丢掉不属于当前频道的条目。 */
export function dedupeFeedItems(items: readonly SeriesItem[], channel: FeedChannel): SeriesItem[] {
  const seen = new Set<string>();
  const result: SeriesItem[] = [];
  for (const item of items) {
    if (!item?.id || !item.title || item.type !== channel || seen.has(item.id)) continue;
    seen.add(item.id);
    result.push(item);
  }
  return result;
}

export function mockFeedItems(channel: FeedChannel): SeriesItem[] {
  return dedupeFeedItems(MOCK_SERIES_LIST, channel);
}

// ───────────────────────────────────────────────────────────────────────────
// 候选来源与「游标属于谁」
//
// 真人 / 漫剧的首选不再是发现流，而是**短剧筛选**（`hongguo_short_filter`）：
// 它接受 `selections`（`genre` + `sort`），服务端按 `online_time` 返回，
// 正是那份逆向记录里唯一被实测到的服务端排序语义。发现流（`discover`）仍然
// 保留为**首屏失败时的退路**。
//
// 两条链路的游标形状不同（筛选是 `s1.…`、发现是 `d1.…`），绝不能互相喂：
// 拿筛选游标去调发现流只会得到「游标不合法」。所以游标在离开本模块时一律
// 带上来源前缀，`parseFeedCursor` 负责把它解回来源 + 原始令牌；没有前缀的
// 旧游标按令牌自身的前缀推断（`s1.` / `d1.`），推断不出就按发现流。
// ───────────────────────────────────────────────────────────────────────────

export type FeedCandidateSource = 'short-filter' | 'discover';

/** 短剧筛选的 `selections`：只钉 genre 与 sort，其余维度留空（= 全部）。 */
export function feedSelectionsFor(channel: FeedChannel): Record<string, string[]> | null {
  switch (channel) {
    case 'comic':
      // `comic_series` 是服务端识别"漫剧场景"的单值（见 catalog.rs 的 scene 判定）。
      return { genre: ['comic_series'], sort: ['online_time'] };
    case 'drama':
      return { genre: ['short_play'], sort: ['online_time'] };
    default:
      // AI 剧没有 App 通道（`discover` 的 tab_type 里没有这一档），仍走网页目录。
      return null;
  }
}

const SHORT_FILTER_TAG = 'sf:';
const DISCOVER_TAG = 'dv:';

export interface FeedSourceCursor {
  source: FeedCandidateSource;
  token: string;
}

/** 给游标打上来来源前缀；令牌为空即没有下一页。 */
export function encodeFeedCursor(source: FeedCandidateSource, token: string | null | undefined): string | null {
  const value = token?.trim() ?? '';
  if (!value) return null;
  return `${source === 'short-filter' ? SHORT_FILTER_TAG : DISCOVER_TAG}${value}`;
}

/**
 * 解开游标。`null` = 首屏。
 *
 * 兼容三种输入：带前缀（本模块产出）、裸的 `s1.`/`d1.` 令牌（旧代码/历史调用）、
 * 其它不认识的字符串（当作发现流，宁可续一次错页也不要凭空重置到第一页）。
 */
export function parseFeedCursor(raw: string | null | undefined): FeedSourceCursor | null {
  const text = raw?.trim() ?? '';
  if (!text) return null;
  if (text.startsWith(SHORT_FILTER_TAG)) {
    const token = text.slice(SHORT_FILTER_TAG.length).trim();
    return token ? { source: 'short-filter', token } : null;
  }
  if (text.startsWith(DISCOVER_TAG)) {
    const token = text.slice(DISCOVER_TAG.length).trim();
    return token ? { source: 'discover', token } : null;
  }
  if (text.startsWith('s1.')) return { source: 'short-filter', token: text };
  return { source: 'discover', token: text };
}

export interface FeedHongguoPage {
  items: SeriesItem[];
  hasMore: boolean;
  cursor: string | null;
}

/** 取一页的两条真实链路（注入是为了能在 Node 里直接跑验收）。 */
export interface FeedHongguoLoaders {
  shortFilter(input: {
    selections: Record<string, string[]>;
    cursor: string | null;
    channel: FeedChannel;
  }): Promise<FeedHongguoPage>;
  discover(channel: HongguoDiscoverChannel, cursor: string | null): Promise<FeedHongguoPage>;
}

export interface FeedHongguoPageResult extends FeedHongguoPage {
  /** 这一页实际由谁提供；下一页据此续。 */
  source: FeedCandidateSource;
}

/**
 * 取一页红果候选（真人 / 漫剧）。
 *
 * 首屏：先短剧筛选；**抛错或空手**才退发现流。退路只发生在这里 ——
 * 第 2 页往后失败一律抛出，绝不能"退回首屏"（那会让游标回到 0 并重复推荐
 * 已经看过的内容，用户观感是"往下滑又从头开始"）。
 */
export async function fetchHongguoFeedPage(input: {
  channel: FeedChannel;
  cursor?: string | null;
  loaders: FeedHongguoLoaders;
}): Promise<FeedHongguoPageResult> {
  const { channel, loaders } = input;
  const selections = feedSelectionsFor(channel);
  if (!selections) throw new Error('AI 剧没有 App 候选通道，应走网页目录');
  const cursored = parseFeedCursor(input.cursor);

  if (cursored) {
    if (cursored.source === 'short-filter') {
      const page = await loaders.shortFilter({ selections, cursor: cursored.token, channel });
      return {
        items: page.items ?? [],
        hasMore: Boolean(page.hasMore),
        cursor: encodeFeedCursor('short-filter', page.cursor),
        source: 'short-filter',
      };
    }
    const page = await loaders.discover(discoverChannelForFeed(channel) as HongguoDiscoverChannel, cursored.token);
    return {
      items: page.items ?? [],
      hasMore: Boolean(page.hasMore),
      cursor: encodeFeedCursor('discover', page.cursor),
      source: 'discover',
    };
  }

  try {
    const page = await loaders.shortFilter({ selections, cursor: null, channel });
    const items = page.items ?? [];
    if (items.length > 0) {
      return {
        items,
        hasMore: Boolean(page.hasMore),
        cursor: encodeFeedCursor('short-filter', page.cursor),
        source: 'short-filter',
      };
    }
    // 空手也要退：`selections` 里的值可能被服务端认成"没有结果"，
    // 但那是配置问题，不该让推荐页空着。
  } catch {
    // 退路在下面
  }
  const fallback = await loaders.discover(discoverChannelForFeed(channel) as HongguoDiscoverChannel, null);
  return {
    items: fallback.items ?? [],
    hasMore: Boolean(fallback.hasMore),
    cursor: encodeFeedCursor('discover', fallback.cursor),
    source: 'discover',
  };
}

/** 最近的同频道红果记录；旧版缺频道时只按真人剧回退，排除动漫与 guo 站源。 */
export function latestHistoryForFeed(
  records: readonly WatchHistoryItem[],
  channel: FeedChannel,
): WatchHistoryItem | undefined {
  return records
    .filter(record => {
      if (!record.seriesId || record.seriesId.startsWith('guo:') || record.seriesId.startsWith('anime_')) {
        return false;
      }
      if (record.channel) return record.channel === channel;
      return channel === 'drama';
    })
    .sort((left, right) => right.updatedAt - left.updatedAt)[0];
}

/**
 * 未完成记录按 episode id 续播；源 id 改变时按集号回退匹配。
 *
 * `playableEpisodes`：这一路源给的可播边界（见 `episodeAvailability`）。
 *
 * 为什么续播也要夹这个边界：历史里记着"看到第 8 集"，而站方现在只开放前 5 集时，
 * 按老索引打开第 8 集在红果侧是**静默失败**（播放页回 200 但没有播放数据），
 * 用户点「继续观看」什么都发生不了，也看不到任何原因。夹到边界内至少能真的
 * 播起来；返回值里 `clamped` 告诉调用方"这不是记录里那一集"，UI 可以据此提示。
 */
export function pickResumeEpisode(
  episodes: readonly Pick<EpisodeItem, 'id' | 'episodeNumber' | 'durationSeconds'>[],
  history: WatchHistoryItem | undefined,
  playableEpisodes?: number,
): { index: number; position: number; finished: boolean; clamped: boolean } {
  if (!episodes.length || !history) {
    return { index: 0, position: 0, finished: false, clamped: false };
  }
  let index = episodes.findIndex(episode => episode.id === history.episodeId);
  if (index < 0 && history.episodeNumber > 0) {
    index = episodes.findIndex(episode => episode.episodeNumber === history.episodeNumber);
  }
  if (index < 0) return { index: 0, position: 0, finished: false, clamped: false };
  const limit =
    playableEpisodes === undefined || !Number.isFinite(playableEpisodes)
      ? episodes.length
      : Math.max(1, Math.min(episodes.length, Math.floor(playableEpisodes)));
  if (index >= limit) {
    // 落在不可播区：回到边界内最后一集的开头重新看，而不是"续播"到一个
    // 播不出来的位置。位置归零是必然的——那一集本来就没被看过。
    return { index: limit - 1, position: 0, finished: false, clamped: true };
  }
  if (history.isFinished) {
    return index < episodes.length - 1
      ? { index: index + 1, position: 0, finished: false, clamped: false }
      : { index, position: 0, finished: true, clamped: false };
  }
  const episode = episodes[index];
  const rawPosition = Number.isFinite(history.positionSeconds) ? Math.max(0, history.positionSeconds) : 0;
  const position = episode.durationSeconds > 0 ? Math.min(rawPosition, episode.durationSeconds) : rawPosition;
  return { index, position, finished: false, clamped: false };
}

export function feedDetailFromHongguo(detail: HongguoSeriesDetail): FeedDetail {
  const series = detail.series;
  return {
    ...series,
    id: series.id,
    title: series.title,
    cover: series.cover,
    backdrop: series.backdrop,
    type: series.type,
    episodesCount: detail.episodes.length || series.episodesCount,
    tags: series.tags ?? [],
    origin: series.origin || '红果推荐',
    description: series.brief ?? '',
    episodes: detail.episodes.map((episode): EpisodeItem => ({
      id: episode.id,
      seriesId: episode.seriesId || series.id,
      episodeNumber: episode.number,
      title: episode.title || `第 ${episode.number} 集`,
      durationSeconds: episode.durationSeconds,
    })),
    availableQualities: [{ label: '自动', value: 'auto', resolution: '自适应' }],
    sources: [],
    isMultiSeason: detail.isMultiSeason,
  };
}

export function feedDetailFromMock(series: SeriesItem): FeedDetail | null {
  if (!MOCK_SERIES_LIST.some(item => item.id === series.id)) return null;
  const detail = getSeriesDetail(series.id);
  return detail ? { ...detail, isMultiSeason: false } : null;
}

/** 选季按结构化 seasonIndex，缺失时才按标题回退；跳过未上架或禁播季。 */
export function nextPlayableSeason(
  entries: readonly SeasonApiEntry[],
  currentSeason: number,
  currentSeriesId?: string,
): SeasonApiEntry | null {
  return entries
    .filter(entry => {
      if (!entry.seriesId || entry.seriesId === currentSeriesId || entry.disablePlay || entry.episodeCount <= 0) {
        return false;
      }
      const season = entry.seasonIndex > 0
        ? entry.seasonIndex
        : parseSeasonLabel(entry.title)?.season ?? 0;
      return season > currentSeason;
    })
    .sort((left, right) => {
      const leftSeason = left.seasonIndex > 0 ? left.seasonIndex : parseSeasonLabel(left.title)?.season ?? Infinity;
      const rightSeason = right.seasonIndex > 0 ? right.seasonIndex : parseSeasonLabel(right.title)?.season ?? Infinity;
      return leftSeason - rightSeason;
    })[0] ?? null;
}

export function seasonItemFromApi(entry: SeasonApiEntry, channel: FeedChannel): SeriesItem {
  return {
    id: entry.seriesId,
    title: entry.title,
    cover: entry.cover,
    type: channel,
    episodesCount: entry.episodeCount,
    tags: [],
    origin: '系列列表',
  };
}

export function seasonNumberFromTitle(title: string): number {
  return parseSeasonLabel(title)?.season ?? 1;
}

/**
 * 拖滑/触摸的方向：**上滑 = 下一部**。
 *
 * 忽略点击、短距离移动和以横向为主的轨迹（56px 与 1.25 倍横纵比是第三版实测
 * 定下的值，误触率与"滑不动"的平衡点）。物理方向与语义在这里绑定：
 * `deltaY < 0`（手指向上）读作"往后翻"。
 */
export function feedGestureDirection(deltaX: number, deltaY: number): FeedGesture | null {
  if (Math.abs(deltaY) < 56 || Math.abs(deltaY) < Math.abs(deltaX) * 1.25) return null;
  return deltaY < 0 ? 'next' : 'previous';
}

/**
 * 滚轮的方向：**向下 = 下一部**。
 *
 * 为什么不能复用 `feedGestureDirection`：两者对"正方向"的定义恰好相反。
 * 拖滑是**位移**语义（手指上移 → deltaY 为负 → 下一部），滚轮是**滚动量**语义
 * （滚轮向下 → deltaY 为正 → 下一部）。混用一个公式必然有一路写反——旧实现就是
 * 让滚轮共用拖滑公式，结果"向下滚"读到的是上一部，用户感知成"滚反了"。
 *
 * 阈值 40px：一个标准滚轮档位是 100~120，40 足够挡住触控板的高频小增量；
 * 再叠加 550ms 闸门，一次触控板甩动能切的数量因此有上界。
 */
export function feedWheelDirection(deltaX: number, deltaY: number): FeedGesture | null {
  if (Math.abs(deltaY) < FEED_WHEEL_MIN_DELTA) return null;
  if (Math.abs(deltaY) < Math.abs(deltaX) * 1.25) return null;
  return deltaY > 0 ? 'next' : 'previous';
}

/** 滚轮判定阈值（px）。 */
export const FEED_WHEEL_MIN_DELTA = 40;

/**
 * 把任意来源的类型字段归一成推荐页频道。
 *
 * 现象（本文件旧实现）：`channel === 'comic' ? 'comic' : 'drama'`。三元天然带
 * else，AI 剧从收藏 / 历史进推荐页时 `type` 是 `'ai'`，却被 else 吃掉变成
 * `'drama'` —— 用户点开一部 AI 剧，推荐流里播的是真人剧，且没有任何报错。
 *
 * 根因：归一化当初只有两个频道，"不是漫剧就是真人剧"被当成了不变量。
 *
 * 取舍：改成显式映射表。未知值（含 `anime` / 脏历史数据）仍然回落到 `drama`——
 * 推荐页不该因为一条脏记录变成空白；但 `ai` 不再经过 else，加频道时也不会被
 * 静默归进旧口径。
 */
export function normalizeFeedChannel(channel: ChannelType | string | undefined): FeedChannel {
  switch (channel) {
    case 'comic':
      return 'comic';
    case 'ai':
      return 'ai';
    default:
      return 'drama';
  }
}

// ───────────────────────────────────────────────────────────────────────────
// AI 剧推荐流（公开网页目录）
//
// App 没有 AI 剧的 discover 通道，这条流只能由两个公开网页段拼出来：
//
// | 段 | 路由 | 每页 | 页数 | 排序 |
// | --- | --- | --- | --- | --- |
// | 热度榜 | `/rank/hot-ai-drama` | 20 | 5 | 站方热度 |
// | 目录 | `/category/ai-drama` | 24 | 34 | 站方目录顺序 |
//
// `catalog_list` 的 `page` 是**站点页码**（后端 `items.truncate(page_size)`，
// 不会按 pageSize 重新切片），所以这里的分页必须按站点页走，不能自己算偏移。
// 结果是：热榜段最多 100 部，目录段最多 816 部，合计 916 部可滑。
// ───────────────────────────────────────────────────────────────────────────

/** `/rank/hot-ai-drama` 实测每页 20 条、共 5 页。 */
export const AI_HEAT_PAGE_SIZE = 20;
export const AI_HEAT_LAST_PAGE = 5;
/** `/category/ai-drama` 实测每页 24 条、共 34 页。 */
export const AI_CATALOG_PAGE_SIZE = 24;
export const AI_CATALOG_LAST_PAGE = 34;
/**
 * 热榜段请求的 pageSize。
 *
 * 传 30 是因为任务书对首屏的要求；但后端只做 `truncate`，站点给 20 条就还是
 * 20 条 —— 这个值**不会**把热榜页撑大，别拿它当"每页 30 部"的承诺。
 */
const AI_HEAT_REQUEST_PAGE_SIZE = 30;

/** 一条 AI 推荐流游标。段 + 站点页码，没有第二种解释。 */
export interface AiFeedCursor {
  segment: 'heat' | 'catalog';
  page: number;
}

const AI_CURSOR_PATTERN = /^ai:(heat|catalog):([1-9]\d{0,2})$/;

export function encodeAiFeedCursor(cursor: AiFeedCursor): string {
  return `ai:${cursor.segment}:${cursor.page}`;
}

/**
 * 解析游标；不认识 / 越界一律 `null`。
 *
 * 越界按"段内页数上限"卡：目录段写 99 页时不能当成合法游标继续打站点——
 * 那只会拿到空页并让无限滚动看起来"卡住了"。
 */
export function parseAiFeedCursor(raw: string | null | undefined): AiFeedCursor | null {
  const text = raw?.trim() ?? '';
  if (!text) return null;
  const match = AI_CURSOR_PATTERN.exec(text);
  if (!match) return null;
  const page = Number(match[2]);
  const segment = match[1] as AiFeedCursor['segment'];
  const lastPage = segment === 'heat' ? AI_HEAT_LAST_PAGE : AI_CATALOG_LAST_PAGE;
  if (!Number.isInteger(page) || page < 1 || page > lastPage) return null;
  return { segment, page };
}

export function initialAiFeedCursor(): string {
  return encodeAiFeedCursor({ segment: 'heat', page: 1 });
}

/**
 * 下一页游标；`null` = AI 剧推荐流真的到底了。
 *
 * 段切换是这里唯一容易写错的地方：热榜第 5 页之后**还有**目录 34 页，
 * 所以 `advance` 不能看后端的 `hasMore`（那一条在第 5 页是 false），
 * 只能按段内页数推进。空 / 坏游标退回首屏，而不是当成"到底了"——
 * 把解析失败呈现成"没有更多推荐"会让用户以为这条频道没内容。
 */
export function advanceAiFeedCursor(raw: string | null | undefined): string | null {
  const cursor = parseAiFeedCursor(raw);
  if (!cursor) return initialAiFeedCursor();
  if (cursor.segment === 'heat') {
    return cursor.page < AI_HEAT_LAST_PAGE
      ? encodeAiFeedCursor({ segment: 'heat', page: cursor.page + 1 })
      : encodeAiFeedCursor({ segment: 'catalog', page: 1 });
  }
  return cursor.page < AI_CATALOG_LAST_PAGE
    ? encodeAiFeedCursor({ segment: 'catalog', page: cursor.page + 1 })
    : null;
}

/** 游标 → `catalog.list` 的筛选条件。 */
export function aiFeedFilterForCursor(cursor: AiFeedCursor): CatalogFilter {
  const heat = cursor.segment === 'heat';
  return {
    channel: 'ai',
    category: '全部',
    audience: '全部',
    sort: heat ? 'heat' : 'recommend',
    page: cursor.page,
    pageSize: heat ? AI_HEAT_REQUEST_PAGE_SIZE : AI_CATALOG_PAGE_SIZE,
  };
}

/** `fetchAiFeedPage` 需要的一页数据；由调用方注入 IPC，避免本文件值依赖 `ipc.ts`。 */
export interface AiFeedLoaderPage {
  items: readonly SeriesItem[];
  hasMore: boolean;
}

export type AiFeedLoader = (filter: CatalogFilter) => Promise<AiFeedLoaderPage>;

export interface FeedPageResult {
  items: SeriesItem[];
  hasMore: boolean;
  cursor: string | null;
}

/**
 * AI 剧推荐流的一页。
 *
 * `load` 由调用方注入（`useFeedStore` 传 `filter => ipcService.catalog.list(filter)`）：
 * 本文件被 `scripts/verify-feed-source.mjs` 用 `node --experimental-strip-types` 直接
 * 跑，而 `ipc.ts` 内部的 import 不带 `.ts` 后缀，一旦在这里值导入它，验收脚本会
 * 在解析阶段就崩。依赖注入同时让分页推进能用假 loader 做纯函数验收。
 */
export async function fetchAiFeedPage(
  cursor: string | null | undefined,
  load: AiFeedLoader,
): Promise<FeedPageResult> {
  const state = parseAiFeedCursor(cursor) ?? { segment: 'heat', page: 1 };
  const page = await load(aiFeedFilterForCursor(state));
  const items = (page.items ?? []).filter(item => item?.id && item.title && item.type === 'ai');
  const next = advanceAiFeedCursor(encodeAiFeedCursor(state));
  // 段内还有没有下一页：热榜段靠游标（段末直接切目录），目录段才信后端的 hasMore。
  // 直接统一信 hasMore 会让热榜第 5 页（后端 has_more=false）把后面 34 页目录全丢掉。
  const hasMore = state.segment === 'heat'
    ? items.length > 0 && next !== null
    : Boolean(page.hasMore) && next !== null;
  return { items, hasMore, cursor: hasMore ? next : null };
}
