/**
 * 「短剧推荐」页的导航判定（纯函数）。
 *
 * 只负责三件事：换哪一部、本剧内换哪一集、以及两次导航之间的最小间隔。
 * 事件如何翻译成方向、状态存在哪里，都由调用方决定（见 `FeedView` /
 * `useFeedStore`），这样边界可以直接被 `scripts/verify-feed-series-nav.mjs` 跑到。
 *
 * 语义约定：
 * - **换剧**只由方向键 / 滚轮 / 拖滑 / 「换一部」触发，落点是新剧的首集 0 秒；
 * - **换集**只在本剧内，末集与可播边界处返回 `null`（调用方据此不动）；
 * - 本剧下一集平时由自动连播推进（`usePlaybackStore` 的 ended 链路）。
 */

/** 换内容的方向。 */
export type FeedSeriesDirection = 'next' | 'previous';

/**
 * 换剧的目标下标；`null` = 这个方向走不动（已在首部 / 已在末部）。
 *
 * 不绕回：末部再按「下一部」停在原地，由调用方决定补页或提示；首部再按
 * 「上一部」也停在原地。也不重播当前部——`null` 就是"没有另一部可去"。
 */
export function feedSeriesIndex(
  currentIndex: number,
  itemCount: number,
  direction: FeedSeriesDirection,
): number | null {
  if (!Number.isFinite(currentIndex) || itemCount <= 0) return null;
  const step = direction === 'previous' ? -1 : 1;
  const target = Math.trunc(currentIndex) + step;
  if (target < 0 || target >= itemCount) return null;
  return target;
}

/**
 * 本剧内的「上/下一集」目标下标；`null` = 停在边界，不跨季、不跨剧。
 *
 * `playableEpisodes` 是源给的可播边界（见 `episodeAvailability`）：详情列了 102 集
 * 但站方只开放前 5 集时，第 5 集之后的播放页是"200 但无播放数据"的静默失败，
 * 所以边界必须前置到按钮上。缺失 / 非法时退回集数本身。
 */
export function feedEpisodeStep(
  episodeIndex: number,
  episodeCount: number,
  direction: FeedSeriesDirection,
  playableEpisodes?: number,
): number | null {
  if (!Number.isFinite(episodeIndex) || episodeCount <= 0) return null;
  const limit = feedPlayableLimit(episodeCount, playableEpisodes);
  const step = direction === 'previous' ? -1 : 1;
  const target = Math.trunc(episodeIndex) + step;
  if (target < 0 || target >= limit) return null;
  return target;
}

/** 可播边界的收口；非法值（NaN / Infinity / 0 / 负数）视为未提供。 */
export function feedPlayableLimit(episodeCount: number, playableEpisodes?: number): number {
  if (!Number.isFinite(episodeCount) || episodeCount <= 0) return 0;
  if (playableEpisodes === undefined || playableEpisodes === null) return episodeCount;
  if (!Number.isFinite(playableEpisodes)) return episodeCount;
  const floored = Math.floor(playableEpisodes);
  if (floored < 1) return episodeCount;
  return Math.min(episodeCount, floored);
}

/**
 * 两次导航之间的最小间隔（毫秒）。
 *
 * 一次上滑与一次方向键可能落在相邻几十毫秒内，而 store 只挡得住并发、挡不住
 * 先后到达的两次顺序调用。状态由调用方放在 ref 里（`FeedView` 的
 * `lastNavAtRef`），不能放在 effect 闭包里——effect 重跑会把时间戳重置成 0，
 * 节流静默失效。
 */
export const FEED_NAV_THROTTLE_MS = 550;

export function feedNavAllowed(
  lastNavAt: number,
  now: number,
  minInterval: number = FEED_NAV_THROTTLE_MS,
): boolean {
  if (!Number.isFinite(now)) return false;
  if (!Number.isFinite(lastNavAt) || lastNavAt <= 0) return true;
  return now - lastNavAt >= minInterval;
}