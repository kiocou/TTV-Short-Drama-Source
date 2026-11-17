/**
 * 推荐流队列的纯逻辑：合并一页、只排未访问过的尾段、提前补页判定、首屏候选缓存。
 *
 * 抽出来的理由是可测：原先 `appendNextPage` 先 await 标签解析（最多 8 部公开详情、
 * 并发 2）再入队，边界换剧因此在等一串与导航无关的网络往返。这里把时序做成纯函数，
 * `scripts/verify-feed-latency.mjs` 用 deferred 直接断言。
 *
 * 边界（改这里之前先读）：
 * 1. 已展示的前缀永不重排；排序只作用于"本次追加段"（`tailStart`..`tailEnd`），
 *    且整段必须在 `visitedHighWater` 之后。段外的后续新页一条都不许动。
 * 2. `currentIndex` **不足以**表示"没访问过"（用户可以往前再后退），必须用高水位。
 * 3. 合并与任何 await 无关：页一到就入队。
 * 4. 候选缓存只由首屏取数写入**原始首屏切片**；追加页不回写整条无限队列。
 */

import type { SeriesItem } from '../types/catalog';
import { enrichTags, orderFeedPage } from './feedRank.ts';

export interface FeedPageSlice {
  items: SeriesItem[];
  cursor: string | null;
  hasMore: boolean;
}

/** 距队列末端不足这个数就开始补页。 */
export const PAGE_PREFETCH_AHEAD = 3;

/** 首屏候选缓存有效期：过了就当没有。 */
export const CANDIDATE_TTL_MS = 5 * 60_000;

const CANDIDATE_CACHE_MAX = 3;

/**
 * 把一页候选接到队尾（同 id 只留第一次出现的那条），返回追加段起点。
 * 不接收频道：频道过滤是取数层职责，输入已是过滤后的候选。
 */
export function mergeFeedPage(
  existing: readonly SeriesItem[],
  pageItems: readonly SeriesItem[],
): { items: SeriesItem[]; added: SeriesItem[]; tailStart: number } {
  const seen = new Set(existing.map(item => item.id));
  const added: SeriesItem[] = [];
  for (const item of pageItems) {
    if (!item?.id || seen.has(item.id)) continue;
    seen.add(item.id);
    added.push(item);
  }
  return {
    items: added.length ? [...existing, ...added] : existing.slice(),
    added,
    tailStart: existing.length,
  };
}

/**
 * 只排"本次追加段"，前缀、已访问区、段外的后续新页原样保留。
 *
 * 返回 `null` = 什么都没做（调用方不要 `setItems`）：
 * - 画像为空（降级为源顺序）；
 * - 段边界越界 / 段内不足 2 条；
 * - `tailStart <= visitedHighWater`：段里混着已看过的条目，重排会把看过的剧换掉；
 * - 段内 id 与 `expectedIds` 不一致：标签解析这段时间里队列被改过，放弃。
 *
 * `tailEnd` 与 `expectedIds` 缺一不可：只给 `tailStart` 时函数会吃掉"后来的新页"，
 * 一次晚到的标签解析就能把用户还没看到的下一页整段洗掉。切片式取段 + 原样拼回，
 * 保证"晚到的排序永远只影响它当初那一页"。
 */
export function orderFeedTail(
  items: readonly SeriesItem[],
  options: {
    tailStart: number;
    tailEnd: number;
    /** 追加当时这一段各条目的 id（顺序与长度都要对得上）。 */
    expectedIds?: readonly string[];
    /** 已经打开过的最大下标；整段必须严格在它之后。 */
    visitedHighWater: number;
    interest?: ReadonlyMap<string, number>;
    tagsById?: ReadonlyMap<string, readonly string[]>;
    rng?: () => number;
  },
): SeriesItem[] | null {
  const { tailStart, tailEnd, expectedIds, visitedHighWater, interest, tagsById, rng } = options;
  if (!interest || interest.size === 0) return null;
  if (tailStart < 0 || tailEnd > items.length || tailEnd - tailStart < 2) return null;
  if (tailStart <= visitedHighWater) return null;
  if (expectedIds && (expectedIds.length !== tailEnd - tailStart
    || expectedIds.some((id, offset) => items[tailStart + offset]?.id !== id))) return null;
  const tail = items.slice(tailStart, tailEnd);
  const enriched = tagsById && tagsById.size > 0 ? enrichTags(tail, tagsById) : tail;
  const ordered = orderFeedPage(enriched, { interest, rng });
  if (!ordered.some((item, index) => item.id !== tail[index]?.id)) return null;
  return [...items.slice(0, tailStart), ...ordered, ...items.slice(tailEnd)];
}

/** 现在是否该提前补页（最多提前一页）。 */
export function shouldPrefetchNextPage(state: {
  currentIndex: number;
  itemCount: number;
  hasMore: boolean;
  pageInFlight: boolean;
}): boolean {
  const { currentIndex, itemCount, hasMore, pageInFlight } = state;
  if (!hasMore || pageInFlight) return false;
  if (itemCount <= 0 || currentIndex < 0) return false;
  return itemCount - currentIndex <= PAGE_PREFETCH_AHEAD;
}

export interface CandidateCache<T extends FeedPageSlice> {
  read(channel: string): T | null;
  write(channel: string, page: T): void;
  clear(): void;
  size(): number;
}

/**
 * 首屏候选缓存：只存元数据（原始首屏卡片 + 原始游标 + hasMore），不碰任何媒体产物。
 *
 * 命中即复用、**不再后台刷新**：缓存里的游标就是第 1 页末尾，追加页从它继续往后取。
 * 若在这里回写"追加后的整条无限队列"，游标会跟着前进，下次进页面直接落到第 N 页，
 * 与提前翻页的补页任务互相踩。过期（默认 5 分钟）才当没有、重新取。
 *
 * 按频道分槽保留（最多 3 个）：切频道不清空别人的槽，切回来仍然能命中。
 */
export function createCandidateCache<T extends FeedPageSlice>(options?: {
  ttlMs?: number;
  now?: () => number;
}): CandidateCache<T> {
  const ttlMs = options?.ttlMs ?? CANDIDATE_TTL_MS;
  const now = options?.now ?? (() => Date.now());
  let entries = new Map<string, { page: T; at: number }>();

  return {
    read(channel) {
      const hit = entries.get(channel);
      if (!hit) return null;
      if (now() - hit.at > ttlMs) {
        entries.delete(channel);
        return null;
      }
      entries.delete(channel);
      entries.set(channel, hit);
      return hit.page;
    },
    write(channel, page) {
      if (!page || page.items.length === 0) return;
      entries.delete(channel);
      entries.set(channel, { page, at: now() });
      while (entries.size > CANDIDATE_CACHE_MAX) {
        const oldest = entries.keys().next();
        if (oldest.done) break;
        entries.delete(oldest.value);
      }
    },
    clear() {
      entries = new Map();
    },
    size() {
      return entries.size;
    },
  };
}