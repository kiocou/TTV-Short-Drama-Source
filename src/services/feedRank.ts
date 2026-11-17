import type { SeriesItem } from '../types/catalog';
import type { WatchHistoryItem } from '../types/history';
import type { FavoriteItem } from '../types/favorite';

/**
 * 推荐流的**本地兴趣加权**（纯逻辑 + 一个有界异步解析器）。
 *
 * ## 这套东西是从哪来的、哪些不是复刻
 *
 * 2026-11-09 的另一个会话（`session-c350ad27`，对另一个短剧客户端 MiniReel 的逆向）
 * 留下了三个可证事实：①抓到的服务端列表请求只按 `online_time` 排序；②客户端本地
 * 有一份 `recommendation_policy` 符号（含"兴趣表 / 权重 / 加权洗牌 / 频道轮转"这类
 * 成员名）；③本地库里有 `history`（带 `positionMs`/`durationMs`）、`favorites`、
 * 每部剧的 `tags`。**下面的系数是我们自己定的**（原权重公式在 AOT 机器码里，没有
 * 拿到），所以这里的实现是"TTV 的等价设计"，不宣称与它一致。
 *
 * ## 为什么只排"还没入队的下一批"
 *
 * 已经展示给用户的队列与当前下标绝不能重排：用户滑到第 3 部、下一页回来一洗牌，
 * 第 3 部就变成了另一部剧——那是"看着看着剧被换掉"。所以排序只作用于
 * `fetchPage` 新拿到的那一页，入队后就不再动。
 *
 * ## 没有画像时保持源顺序
 *
 * 兴趣表为空（没历史、没收藏、或标签一条都没解析出来）时**原样返回**。用随机
 * 打乱"没有依据"的列表，只会让每次进推荐页都不一样，且无法解释。
 */

/** 「有效观看」的门槛（秒）：短剧一集一两分钟，滑过 15 秒才算看过。 */
export const HISTORY_EFFECTIVE_SECONDS = 15;

/** 历史里一部有效观看的剧，给它每个标签加的权重基准。 */
export const HISTORY_TAG_BASE_WEIGHT = 1;

/** 收藏一部剧，给它每个标签加的权重（收藏是比"看过"更强的信号）。 */
export const FAVORITE_TAG_BONUS = 1;

/** 没有匹配到任何兴趣标签的候选，权重就是这个值（= 不额外加权）。 */
export const BASE_ITEM_WEIGHT = 1;

/** 观看完成度（0..1）。未达门槛且未标完成 → 0，即"不算有效观看"。 */
export function historyCompletion(record: WatchHistoryItem): number {
  if (record.isFinished) return 1;
  if (!(record.positionSeconds >= HISTORY_EFFECTIVE_SECONDS)) return 0;
  const duration = record.durationSeconds;
  if (!(duration > 0)) return 0.5;
  return Math.max(0, Math.min(1, record.positionSeconds / duration));
}

export interface InterestInput {
  history: readonly WatchHistoryItem[];
  favorites: readonly FavoriteItem[];
  /** 剧 id → 题材标签。拿不到的 id 直接缺席，不猜。 */
  tagsById: ReadonlyMap<string, readonly string[]>;
}

/**
 * 兴趣表：`标签 → 权重`。
 *
 * 只对"有标签"的剧累计。历史按完成度加权（看完 = 1，看到一半 = 0.5），
 * 收藏另外叠加 `FAVORITE_TAG_BONUS`。没有完成度的记录不贡献权重。
 */
export function buildInterest(input: InterestInput): Map<string, number> {
  const interest = new Map<string, number>();
  const add = (tags: readonly string[] | undefined, weight: number) => {
    if (!tags || !(weight > 0)) return;
    for (const tag of tags) {
      const key = tag?.trim();
      if (!key) continue;
      interest.set(key, (interest.get(key) ?? 0) + weight);
    }
  };
  for (const record of input.history) {
    add(input.tagsById.get(record.seriesId), historyCompletion(record) * HISTORY_TAG_BASE_WEIGHT);
  }
  for (const favorite of input.favorites) {
    add(input.tagsById.get(favorite.seriesId), FAVORITE_TAG_BONUS);
  }
  return interest;
}

/** 候选权重：命中几个标签就加几份对应权重，最后乘在基准上。 */
export function itemWeight(item: Pick<SeriesItem, 'tags'>, interest: ReadonlyMap<string, number>): number {
  let weight = BASE_ITEM_WEIGHT;
  for (const tag of item.tags ?? []) {
    const key = tag?.trim();
    if (!key) continue;
    weight += interest.get(key) ?? 0;
  }
  return Number.isFinite(weight) && weight > 0 ? weight : BASE_ITEM_WEIGHT;
}

/**
 * 加权随机**无放回**排列（Efraimidis–Spirakis）。
 *
 * 每个条目取 `u^(1/w)`（`u ∈ (0,1)`）作键，按键降序：权重大的大概率靠前，
 * 但每次顺序都不同，且不会重复取同一条。`rng` 由调用方注入，测试里可以固定。
 * 键相同时按原下标稳定排序——同一个 rng 序列必须得到同一个顺序。
 */
export function weightedShuffle<T>(
  items: readonly T[],
  rng: () => number,
  weightOf: (item: T) => number,
): T[] {
  const keyed = items.map((item, index) => {
    const weight = weightOf(item);
    const safe = Number.isFinite(weight) && weight > 0 ? weight : BASE_ITEM_WEIGHT;
    // rng 取 (0,1)：0 会让 0^(1/w)=0 恒排最后，用最小正数兜住。
    const sample = Math.min(1 - Number.EPSILON, Math.max(Number.EPSILON, rng()));
    return { item, index, key: Math.pow(sample, 1 / safe) };
  });
  keyed.sort((left, right) => (right.key - left.key) || (left.index - right.index));
  return keyed.map(entry => entry.item);
}

/**
 * 对这一页候选做排序；没有画像时保持源顺序。
 *
 * 只接受"还没入队"的条目——已展示队列绝不能走进这里（见文件头）。
 */
export function orderFeedPage(
  items: readonly SeriesItem[],
  options: { interest?: ReadonlyMap<string, number>; rng?: () => number },
): SeriesItem[] {
  const interest = options.interest;
  if (!interest || interest.size === 0 || items.length < 2) return items.slice();
  const rng = options.rng ?? Math.random;
  return weightedShuffle(items, rng, item => itemWeight(item, interest));
}

// ───────────────────────────────────────────────────────────────────────────
// 标签：候选卡片本身不带标签，只能按需取
// ───────────────────────────────────────────────────────────────────────────

/**
 * 把已知标签合并进候选（返回新数组，不改入参）。
 *
 * 没有命中的候选原样保留 `tags: []`——排序时按基准权重处理，不编标签。
 */
export function enrichTags(
  items: readonly SeriesItem[],
  tagsById: ReadonlyMap<string, readonly string[]>,
): SeriesItem[] {
  if (tagsById.size === 0) return items.slice();
  return items.map(item => {
    const tags = tagsById.get(item.id);
    if (!tags || tags.length === 0) return item;
    return { ...item, tags: [...tags] };
  });
}

/** 一次最多补几部候选的标签。 */
export const TAG_FETCH_LIMIT = 8;
/** 补标签的并发上限。 */
export const TAG_FETCH_CONCURRENCY = 2;
/** 标签缓存条目上限（负结果也占位，随 LRU 一起淘汰）。 */
const TAG_CACHE_MAX = 32;

export interface TagResolver {
  /** 已拿到的标签（空数组 = 已知没有标签/取失败）。 */
  cached(seriesId: string): readonly string[] | undefined;
  /** 直接写入已知标签（列表卡片或已打开的详情里就有）。 */
  remember(seriesId: string, tags: readonly string[]): void;
  /** 批量取；只返回有标签的 id。失败/无标签记为空并缓存，避免重复请求。 */
  resolve(seriesIds: readonly string[]): Promise<Map<string, readonly string[]>>;
  clear(): void;
  size(): number;
}

interface ResolverGeneration {
  cache: Map<string, readonly string[]>;
  queue: Set<string>;
  /** 已被 worker 取走、结果还没回来的 id：这期间同名请求必须合并到同一次加载。 */
  inflight: Set<string>;
  running: Promise<void> | null;
}

/**
 * 有界标签缓存 + 并发受限解析 + 并发调用合并。
 *
 * 每一代（`clear()` 之后）是**独立 state**：旧 worker 在处理完当前条目后会看到
 * `generation !== current` 并立即退出，不会继续消费新代的队列，也不会把结果写进
 * 新缓存。`running` 的清理按身份比较，旧 finally 不清新代的 running。
 *
 * 负结果（失败 / 确实没有标签）以空数组进缓存，随 LRU 一起淘汰 —— 因此"不重试"
 * 这件事天然有界，不需要一个会无限增长的 attempted 集合。
 */
export function createTagResolver(options: {
  load: (seriesId: string) => Promise<readonly string[] | null>;
}): TagResolver {
  let generation: ResolverGeneration = { cache: new Map(), queue: new Set(), inflight: new Set(), running: null };

  const rememberInto = (state: ResolverGeneration, id: string, tags: readonly string[]) => {
    state.cache.set(id, tags);
    while (state.cache.size > TAG_CACHE_MAX) {
      const oldest = state.cache.keys().next();
      if (oldest.done) break;
      state.cache.delete(oldest.value);
    }
  };

  const drain = async (state: ResolverGeneration): Promise<void> => {
    const worker = async () => {
      while (generation === state) {
        const next = state.queue.values().next();
        if (next.done) return;
        const id = next.value;
        state.queue.delete(id);
        // 从被取走到结果回来之间登记在途：这期间同 id 不得再入队。
        state.inflight.add(id);
        const tags = await options.load(id).catch(() => null);
        state.inflight.delete(id);
        // 旧代的 worker 到此为止：不写新缓存、不再碰新队列。
        if (generation !== state) return;
        rememberInto(state, id, tags && tags.length ? tags : []);
      }
    };
    await Promise.all(Array.from({ length: TAG_FETCH_CONCURRENCY }, worker));
  };

  return {
    cached(id) {
      return generation.cache.get(id);
    },
    remember(seriesId, tags) {
      if (!seriesId || tags.length === 0) return;
      rememberInto(generation, seriesId, tags);
    },
    async resolve(seriesIds) {
      const state = generation;
      for (const id of seriesIds.slice(0, TAG_FETCH_LIMIT)) {
        if (!id || state.cache.has(id) || state.queue.has(id) || state.inflight.has(id)) continue;
        state.queue.add(id);
      }
      if (!state.running && state.queue.size > 0) {
        const task = drain(state);
        const wrapped = task.finally(() => {
          // 身份比较：旧代的 finally 不得清掉新代的 running。
          if (generation === state && state.running === wrapped) state.running = null;
        });
        state.running = wrapped;
      }
      if (state.running) await state.running;
      const result = new Map<string, readonly string[]>();
      for (const id of seriesIds) {
        const tags = state.cache.get(id);
        if (tags && tags.length) result.set(id, tags);
      }
      return result;
    },
    clear() {
      generation = { cache: new Map(), queue: new Set(), inflight: new Set(), running: null };
    },
    size() {
      return generation.cache.size;
    },
  };
}