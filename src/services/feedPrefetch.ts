/**
 * 下一部推荐剧的**详情预取**（只缓存元数据，不碰媒体）。
 *
 * 媒体解析/转码由播放链路的"下一集预取"负责；两路同时抢带宽只会把正在播的
 * 那一路拖慢。
 *
 * 约束：邻近最多 2 部、同一 (channel,id) 只发一次、缓存 TTL 到期即当没有。
 * `get` **保留失败原因**（主动打开要能看到真实错误，且不会再发一次同样的请求）；
 * 后台 `warm` 自己吞掉异常。
 *
 * 换代语义（`reset`）：**保留已完成的缓存，只作废在途请求**。详情是元数据，
 * 跨代复用不会串频道（key 里带频道）；而在途请求必须切断，否则新代同一个 id 会
 * 复用上一代的 Promise，拿到的结果写进新代的缓存。
 */

const PREFETCH_CACHE_MAX = 12;
const PREFETCH_NEIGHBORS = 2;

/** 缓存详情有效期：与首屏候选同口径（5 分钟）。过期即重新取，避免无限期吃旧元数据。 */
export const PREFETCH_TTL_MS = 5 * 60_000;

export interface SeriesPrefetcher<T> {
  /** 命中缓存即返回；在途则共享同一个 Promise；失败按原样 reject。 */
  get(seriesId: string, channel: string, isCurrent?: () => boolean): Promise<T>;
  /** 后台预取；失败静默。 */
  warm(seriesIds: readonly string[], channel: string, isCurrent?: () => boolean): void;
  /** 这一部是否在有效缓存里（打点与"改用缓存"的判据，不触发加载）。 */
  cached(seriesId: string, channel: string): boolean;
  /** 同步读取未过期的缓存详情（不触发加载，未命中返回 undefined）。 */
  peek(seriesId: string, channel: string): T | undefined;
  /**
   * 换代作废。
   *
   * `keepCache: true` = 保留已完成缓存、只清在途并递增 epoch（换剧/离开推荐页用的
   * 就是这个口径）；不传 = 缓存与在途一起清（切源、真正要丢数据时用）。
   */
  reset(options?: { keepCache?: boolean }): void;
  /** 作废：清缓存、清在途、递增 epoch（旧结果不得写回新缓存）。 */
  clear(): void;
  size(): number;
  pending(): number;
}

export function createSeriesPrefetcher<T>(options: {
  load: (seriesId: string, channel: string) => Promise<T>;
  ttlMs?: number;
  now?: () => number;
}): SeriesPrefetcher<T> {
  const ttlMs = options.ttlMs ?? PREFETCH_TTL_MS;
  const now = options.now ?? (() => Date.now());
  let epoch = 0;
  const cache = new Map<string, { value: T; at: number }>();
  const inflight = new Map<string, Promise<T>>();
  const keyOf = (channel: string, id: string) => `${channel}::${id}`;

  /** 有效缓存才返回；过期的顺手删掉（读路径不许把过期值当命中）。 */
  const fresh = (key: string): T | undefined => {
    const hit = cache.get(key);
    if (!hit) return undefined;
    if (now() - hit.at > ttlMs) {
      cache.delete(key);
      return undefined;
    }
    return hit.value;
  };

  const remember = (key: string, value: T) => {
    cache.delete(key);
    cache.set(key, { value, at: now() });
    while (cache.size > PREFETCH_CACHE_MAX) {
      const oldest = cache.keys().next();
      if (oldest.done) break;
      cache.delete(oldest.value);
    }
  };

  const start = (seriesId: string, channel: string, isCurrent?: () => boolean): Promise<T> => {
    const key = keyOf(channel, seriesId);
    const running = inflight.get(key);
    if (running) return running;
    const startedAt = epoch;
    const request = options.load(seriesId, channel)
      .then(value => {
        if (startedAt === epoch && (isCurrent?.() ?? true)) remember(key, value);
        return value;
      })
      .finally(() => {
        // 只删自己那一份：reset 之后新起的请求不能被旧的 finally 清掉。
        if (inflight.get(key) === request) inflight.delete(key);
      });
    inflight.set(key, request);
    return request;
  };

  return {
    get(seriesId, channel, isCurrent) {
      const key = keyOf(channel, seriesId);
      const cached = fresh(key);
      if (cached !== undefined) {
        // LRU 触碰：把命中的那条挪到队尾。
        const hit = cache.get(key)!;
        cache.delete(key);
        cache.set(key, hit);
        return Promise.resolve(cached);
      }
      return start(seriesId, channel, isCurrent);
    },
    warm(seriesIds, channel, isCurrent) {
      const targets = seriesIds
        .filter(id => Boolean(id))
        .slice(0, PREFETCH_NEIGHBORS)
        .filter(id => fresh(keyOf(channel, id)) === undefined && !inflight.has(keyOf(channel, id)));
      for (const id of targets) {
        void start(id, channel, isCurrent).catch(() => undefined);
      }
    },
    cached(seriesId, channel) {
      return fresh(keyOf(channel, seriesId)) !== undefined;
    },
    peek(seriesId, channel) {
      return fresh(keyOf(channel, seriesId));
    },
    reset(resetOptions) {
      epoch += 1;
      // 必须切断在途：否则 reset 之后同一个 id 会复用旧 Promise，拿到旧数据。
      inflight.clear();
      if (resetOptions?.keepCache !== true) cache.clear();
    },
    clear() {
      epoch += 1;
      cache.clear();
      inflight.clear();
    },
    size() {
      return cache.size;
    },
    pending() {
      return inflight.size;
    },
  };
}