// 显式 `.ts` 后缀：本模块要能被 `node --experimental-strip-types` 直接加载
// （调度判据的用例跑在 `src/services/danmaku.timing.test.mjs` 里）。
// 同一手法见 `feedSource.ts` / `feedSeriesNav.ts`；`tsconfig` 已开
// `allowImportingTsExtensions`，Vite 侧照旧解析。
import { tracePlayback } from './playbackTrace.ts';

/**
 * 弹幕：窗口调度、缓存与轨道规划。
 *
 * ## 来源
 *
 * 算法移植自「真果鉴」的 Flutter 实现（`lib/danmaku_models.dart`、
 * `lib/danmaku_overlay.dart` 的 `planDanmaku`）。那边经过实机验证，这里只改
 * 运行宿主（Flutter → Web），判据与常量一个都不改：改了就会出现「同一集在两边
 * 观感不同」这种最难排查的问题。
 *
 * ## 三个常量为什么是这三个值
 *
 * - `WINDOW_MS = 30s`：一页弹幕覆盖 30 秒。短剧单集 40~90 秒，一页基本能覆盖
 *   大半集；窗口再大，拖动进度条时要等整页返回才有弹幕。
 * - `LIFETIME_MS = 8s`：一条弹幕从右飘到左用 8 秒。这是轨道判定公式里的分母，
 *   也是「同一轨道前一条是否已经让开」的唯一时间基准。
 * - 同屏上限 24 条 / 最多 4 条轨道：超出就不显示（`planDanmaku` 直接跳过），
 *   而不是叠着渲染 —— 叠起来会糊成一片，比不显示更糟。
 */

/** 单页覆盖的时间窗（毫秒）。必须与 Rust 侧 `DANMAKU_WINDOW_MS` 一致。 */
export const DANMAKU_WINDOW_MS = 30_000;

/** 一条弹幕从右到左的飞行时长（毫秒）。**适中档**的值，也是轨道判定公式的原始分母。 */
export const DANMAKU_LIFETIME_MS = 8_000;

/**
 * 速度档 → 飞行时长。慢/适中/快三档（2026-11-07 弹幕设置面板）：
 * 适中就是移植自真果鉴的 8s 原值，慢/快向两侧拉——值越大飘得越慢。
 */
export const DANMAKU_SPEED_LIFETIME_MS = [12_000, 8_000, 5_000] as const;

/** 同一时刻最多显示多少条（超过的直接丢弃）。 */
const MAX_ONSCREEN = 24;

/**
 * 最多几条轨道。原来写死 4（密度与可读性的折中），2026-11-07 起「显示区域」
 * 交给了用户：层高随区域滑条变化，轨道数按实际层高 ÷ 行高推导，这里只做
 * **上限护栏**。同屏 24 条的密度上限不变，轨道多了也不会糊成一片。
 */
const MAX_LANES = 12;

/** 轨道内前后两条之间的最小水平间隙（像素）。 */
const LANE_GAP_PX = 24;

/** 单条文本上限（字符）。超出截断并补省略号。 */
const TEXT_LIMIT = 100;

/** 命中缓存的有效期：弹幕列表变化极慢，5 分钟内不重复请求。 */
const PAGE_TTL_MS = 5 * 60_000;

/**
 * 距窗口终点多近时预取下一页。
 *
 * 2026-11-07 稳定性修复的主闸：旧实现只拉「当前位置所在页」，位置跨过 30s
 * 边界的那一刻才发请求 —— 网络往返的几百毫秒里无页可显示，屏幕先清空再恢复，
 * 这就是用户报告的「弹幕有时候会突然消失」。提前 8s 备好下一页（3 倍加速下
 * 也有 ~2.7s 的真实时间余量），边界穿越变成纯缓存命中。
 */
const PREFETCH_LEAD_MS = 8_000;

/**
 * 失败后的重试冷却按同窗连续失败次数递增：250ms → 1s → 2s。
 * 后端首窗另有 3 秒等待上限，后台真实请求继续完成；前端不再叠加 3~15 秒空白。
 * 同键在途请求仍单飞，不会形成重试风暴。
 */
const RETRY_COOLDOWNS_MS = [250, 1_000, 2_000] as const;

/** 同时缓存多少页（按离当前播放位置的距离淘汰）。 */
const PAGE_CACHE_LIMIT = 6;
const GLOBAL_PAGE_CACHE_LIMIT = 64;

/**
 * 两次 `ensure` 之间的最小间隔。稳态下用它把「每帧查一次」压成「每 250ms 一次」。
 * 新建加载器那一发不受它约束（由调用方在提交时刻直接发，见 `DanmakuOverlay`）。
 */
export const DANMAKU_ENSURE_INTERVAL_MS = 250;

/** 渲染侧的调度状态。只由 `nextDanmakuTick` 读写。 */
export interface DanmakuTickState {
  /** 上次发起 `ensure` 的时刻（`performance.now()` 口径）。负数 = 还没发过。 */
  lastEnsureAt: number;
  /** 上次排程用的键。空串 = 还没排过（或已作废）。 */
  lastPlanKey: string;
}

/** 新建一份调度状态（换集 / 重新打开弹幕时都要重置）。 */
export function createDanmakuTickState(): DanmakuTickState {
  return { lastEnsureAt: -1, lastPlanKey: '' };
}

export interface DanmakuTickInput {
  nowMs: number;
  positionMs: number;
  durationMs: number;
  /** 弹幕层已量到的宽度（像素）。0 = 还没量出来。 */
  width: number;
  rows: number;
  lifetimeMs: number;
  /**
   * 文本量宽的身份（字号 / 粗体 / 字体）。`planDanmaku` 依赖 `measure`，
   * 量宽一变 `flight.width` 就变；不把它算进排程键，换字体后轨道判定用的还是
   * 旧宽度，会追尾/重叠（而 `rows`/`width` 完全可能不变、`useMemo` 已换新函数）。
   */
  measureKey: string;
  /** 覆盖 `positionMs` 的页（缓存命中才算）。 */
  page: DanmakuPage | null;
}

export interface DanmakuTickDecision {
  /** 本帧要调 `loader.ensure`。 */
  ensure: boolean;
  /** 本帧要用 `page` 重跑 `planDanmaku`。 */
  plan: boolean;
}

/**
 * 一帧的调度判据：要不要去拉页、要不要重排这一页。
 *
 * 键必须覆盖**所有**影响 `planDanmaku` 结果的输入（页身份、宽度、轨道数、飞行时长、
 * 量宽身份）；任一项缺失，对应设置变化后就不会重排。尺寸为 0 时既不排程也不记账 ——
 * 否则「页已到、尺寸还没量出来」那次空排程会被当成已完成，永远不再重排。
 */
export function nextDanmakuTick(
  state: DanmakuTickState,
  input: DanmakuTickInput,
): DanmakuTickDecision {
  const { nowMs, positionMs, durationMs, width, rows, lifetimeMs, measureKey, page } = input;

  const ensure =
    Number.isFinite(durationMs) &&
    durationMs > 0 &&
    Number.isFinite(positionMs) &&
    positionMs >= 0 &&
    (state.lastEnsureAt < 0 || nowMs - state.lastEnsureAt >= DANMAKU_ENSURE_INTERVAL_MS);
  if (ensure) state.lastEnsureAt = nowMs;

  if (!page) {
    // 页隙 / 冷却中：只作废排程键，不清屏（飞行中的弹幕各自按 positionMs - timeMs 定位）。
    state.lastPlanKey = '';
    return { ensure, plan: false };
  }

  if (!Number.isFinite(width) || width <= 0 || rows < 1) return { ensure, plan: false };

  const key = `${page.startMs}|${page.nextMs}|${page.items.length}|${Math.round(width)}|${rows}|${lifetimeMs}|${measureKey}`;
  if (key === state.lastPlanKey) return { ensure, plan: false };
  state.lastPlanKey = key;
  return { ensure, plan: true };
}

export interface DanmakuItem {
  id: string;
  text: string;
  timeMs: number;
}

export interface DanmakuPage {
  episodeId: string;
  startMs: number;
  nextMs: number;
  total: number;
  items: DanmakuItem[];
}

/**
 * 本次打开的起播目标（store 在 `setCurrentEpisode` 同批提交，播放器只读）。
 *
 * 存在的唯一理由：媒体时钟在装载阶段不可信 —— 换集瞬间它还是上一集的播放头，
 * 之后又会被清零。没有它，首窗只能按 0 发（续播 50s 时白拉一页，要等媒体归位
 * 才发正确的窗口）。它**不是**第二个播放时钟：全部渲染/飞行仍只读媒体时钟，
 * 这里只决定「先拉哪一页」，起播一就绪立刻退回媒体时钟。
 */
export interface DanmakuOpenStart {
  seriesId: string;
  episodeId: string;
  /** 已校验的起播点（毫秒，非负、不超过服务端接受的窗口上限）。 */
  startMs: number;
  /** 与 `uiState.sessionId` 同一口径的会话号。 */
  sessionId: number;
}

/** 窗口终点上限（与 Rust `DANMAKU_MAX_DURATION_MS` 对齐，超了服务端会直接拒绝）。 */
const DANMAKU_WINDOW_CEILING_MS = 24 * 60 * 60 * 1000;

/**
 * 秒 → 起播毫秒（经校验）。非有限、≤0 一律归 0（从头播）；异常大的值夹到
 * 「窗口终点上限 - 一个窗口」，保证首窗请求不会因为起点离谱被服务端整单拒绝。
 */
export function danmakuStartMsFromSeconds(seconds: number): number {
  if (!Number.isFinite(seconds) || seconds <= 0) return 0;
  return Math.min(Math.round(seconds * 1000), DANMAKU_WINDOW_CEILING_MS - DANMAKU_WINDOW_MS);
}

/**
 * 装载阶段该用哪个位置选弹幕页；`null` = 回归媒体时钟。
 *
 * 只靠 `uiState.kind === 'opening'` 判断是错的：`handleWaiting` 在媒体
 * `readyState < 3` 时会把状态直接改成 `buffering`（续播时媒体还是 0、新源在拉），
 * 若把"离开 opening"当成"已归位"，首窗会被 rAF 拉回 0 窗。所以按**本次会话的真实
 * 证据**释放目标，三种任一成立即释放：
 *
 *   1. 本会话 `playing`（真的出画了）；
 *   2. 本会话已上报的播放头到达目标（`alignPlayhead` 写完、`timeupdate` 报回来）；
 *   3. 已知时长且目标贴片尾 —— `alignPlayhead` 会把这种起点夹回 0，等下去是死锁。
 *
 * `uiState` 属于**别的会话**时一律不采信（旧会话的 `playing` 不得释放新目标）。
 */
export function danmakuLoadingStartMs(args: {
  openStart: DanmakuOpenStart | null;
  seriesId: string;
  episodeId: string;
  sessionId: number;
  uiState: { kind: string; sessionId?: number };
  /** 本会话已知时长（秒）。0 = 元数据还没到。 */
  duration: number;
  /** 本会话已上报的播放头（秒）。0 = 还没出画。 */
  position: number;
  /** 贴片尾的判定容差（秒），与 `alignPlayhead` 的 `START_NEAR_END_SECONDS` 同一个。 */
  nearEndSeconds?: number;
}): number | null {
  const { openStart, seriesId, episodeId, sessionId, uiState, duration, position } = args;
  const nearEnd = args.nearEndSeconds ?? 1;
  if (!openStart) return null;
  if (openStart.sessionId !== sessionId) return null;
  if (openStart.seriesId !== seriesId || openStart.episodeId !== episodeId) return null;
  if (!Number.isFinite(openStart.startMs) || openStart.startMs < 0) return null;

  const startSec = openStart.startMs / 1000;
  const stateFromThisSession = uiState.sessionId == null || uiState.sessionId === sessionId;

  if (stateFromThisSession) {
    if (uiState.kind === 'playing') return null;
    if (Number.isFinite(position) && position > 0 && position >= startSec - 1.5) return null;
  }
  // 片尾：目标会被夹回 0（时长已知时能判），继续等这个目标永远不会发生。
  if (Number.isFinite(duration) && duration > 0 && duration - startSec <= nearEnd) return null;

  return openStart.startMs;
}

export interface DanmakuFlight {
  item: DanmakuItem;
  text: string;
  lane: number;
  width: number;
}

/**
 * 按时间点把弹幕分配到轨道上。
 *
 * 三条判据（缺一条都会看到重叠或追尾）：
 *
 * 1. 该轨道还空着；
 * 2. 距上一条已超过 `LIFETIME_MS`（上一条早已飞出屏幕）；
 * 3. 上一条**已经整体进入屏幕**，且这一条**不会在屏幕上追上它**。
 *
 * 第 3 条是两段不等式，对应「前车尾部离开右边缘」与「后车头部到前车尾部的距离
 * 始终不小于 `LANE_GAP_PX`」。两条同时成立才能放同一轨道 —— 只检查其中一条，
 * 长弹幕跟短弹幕会撞在一起。
 *
 * `measure` 由调用方提供（DOM 里通常是 canvas `measureText`），因为文本宽度只有
 * 渲染环境知道，算法本身不该依赖 DOM。
 */
export function planDanmaku(
  items: DanmakuItem[],
  options: {
    width: number;
    rows: number;
    measure: (text: string) => number;
    /** 一条弹幕的飞行时长。缺省 = 适中档（8s 原值）。 */
    lifetimeMs?: number;
  },
): DanmakuFlight[] {
  const { width, rows, measure } = options;
  const lifetimeMs = options.lifetimeMs ?? DANMAKU_LIFETIME_MS;
  if (!Number.isFinite(width) || width <= 0 || rows < 1 || !Number.isFinite(lifetimeMs) || lifetimeMs <= 0) {
    return [];
  }

  const lanes: (DanmakuFlight | null)[] = new Array(
    Math.min(Math.max(rows, 1), MAX_LANES),
  ).fill(null);
  const sorted = [...items].sort((a, b) => a.timeMs - b.timeMs);
  const seen = new Set<string>();
  const flights: DanmakuFlight[] = [];

  for (const item of sorted) {
    if (!item.id || !item.text.trim() || item.timeMs < 0 || seen.has(item.id)) continue;
    seen.add(item.id);

    // 同屏密度上限：统计最近 8 秒内已经排进去的条数。
    let recent = 0;
    for (let i = flights.length - 1; i >= 0; i -= 1) {
      if (item.timeMs - flights[i].item.timeMs < lifetimeMs) recent += 1;
      else break;
    }
    if (recent >= MAX_ONSCREEN) continue;

    const chars = [...item.text];
    const text = chars.length > TEXT_LIMIT
      ? `${chars.slice(0, TEXT_LIMIT).join('')}…`
      : item.text;
    const size = Math.ceil(measure(text)) + 8;

    const lane = lanes.findIndex(previous => {
      if (!previous) return true;
      const gap = item.timeMs - previous.item.timeMs;
      if (gap >= lifetimeMs) return true;
      const previousTailClear =
        ((width + previous.width) * gap) / lifetimeMs - previous.width >= LANE_GAP_PX;
      const noCatchUp =
        width - ((width + size) * (lifetimeMs - gap)) / lifetimeMs >= LANE_GAP_PX;
      return previousTailClear && noCatchUp;
    });

    if (lane < 0) continue;
    const flight: DanmakuFlight = { item, text, lane, width: size };
    lanes[lane] = flight;
    flights.push(flight);
  }

  return flights;
}

interface CacheEntry {
  page: DanmakuPage;
  at: number;
}

/**
 * 弹幕分页加载器：负责「当前播放位置需要哪一页」以及缓存/冷却。
 *
 * 不持有 DOM、不持有 video 元素 —— 调用方把 `position` 喂进来即可，这样
 * 播放器、小窗（将来）可以各用一份实例而共享同一套判据。
 */
export class DanmakuLoader {
  /**
   * 页缓存是模块级共享的：React 的开关/换集只决定**显示哪一集**，
   * 不再决定这份数据是否还存在。缓存键含剧、集、窗口，不会串内容。
   */
  private static pages = new Map<string, CacheEntry>();
  private static pending = new Map<string, Promise<void>>();
  private static cooldownUntil = new Map<string, number>();
  private static failures = new Map<string, number>();

  private pages = DanmakuLoader.pages;
  private pending = DanmakuLoader.pending;
  private cooldownUntil = DanmakuLoader.cooldownUntil;
  private failures = DanmakuLoader.failures;
  /**
   * 已销毁：换集/关开关后，这个实例不再发新请求。
   * 共享的在途请求继续完成并写入缓存，切回同一集时可直接使用。
   */
  private disposed = false;

  private readonly seriesId: string;
  private readonly episodeId: string;
  private readonly fetchPage: (
    seriesId: string,
    vid: string,
    startMs: number,
    durationMs: number,
  ) => Promise<DanmakuPage>;

  // 显式字段而非参数属性：参数属性是语法糖，`node --experimental-strip-types`
  // 的 strip-only 模式直接拒绝（本模块的计时用例要能直接加载它）。
  constructor(
    seriesId: string,
    episodeId: string,
    fetchPage: (
      seriesId: string,
      vid: string,
      startMs: number,
      durationMs: number,
    ) => Promise<DanmakuPage>,
  ) {
    this.seriesId = seriesId;
    this.episodeId = episodeId;
    this.fetchPage = fetchPage;
  }

  private cacheKey(start: number, durationMs: number): string {
    const windowEnd = Math.min(start + DANMAKU_WINDOW_MS, Math.round(durationMs));
    return `${this.seriesId}:${this.episodeId}:${start}:${windowEnd}`;
  }

  /** 覆盖 `positionMs` 的页（命中缓存才算）。 */
  pageFor(positionMs: number): DanmakuPage | null {
    for (const [key, entry] of this.pages) {
      if (!key.startsWith(`${this.seriesId}:${this.episodeId}:`)) continue;
      if (positionMs >= entry.page.startMs && positionMs < entry.page.nextMs) {
        return entry.page;
      }
    }
    return null;
  }

  /**
   * 确保 `positionMs` 所在的那一页已在加载或已就绪；临近窗口终点时**预取下一页**。
   *
   * 窗口起点按 30 秒对齐，这样拖动进度条回来时能命中同一页，而不是因为
   * 「从 12.3 秒开始」和「从 12.4 秒开始」算成两页、把缓存打散。
   */
  ensure(positionMs: number, durationMs: number): void {
    if (this.disposed) return;
    if (!Number.isFinite(positionMs) || positionMs < 0) return;
    if (!Number.isFinite(durationMs) || durationMs <= 0) return;
    const start = Math.floor(positionMs / DANMAKU_WINDOW_MS) * DANMAKU_WINDOW_MS;
    this.requestWindow(start, positionMs, durationMs);

    // 预取：位置进入本窗口的最后 8 秒就把下一页备好（见 PREFETCH_LEAD_MS 注释）。
    // 已是最后一页（nextStart ≥ 集长）时不发——服务端会返回空页，纯浪费。
    const nextStart = start + DANMAKU_WINDOW_MS;
    if (
      positionMs >= start + DANMAKU_WINDOW_MS - PREFETCH_LEAD_MS &&
      nextStart < durationMs
    ) {
      this.requestWindow(nextStart, positionMs, durationMs);
    }
  }

  /** 拉一页（带缓存命中 / 在途单飞 / 递增冷却三道闸）。 */
  private requestWindow(start: number, positionMs: number, durationMs: number): void {
    const key = this.cacheKey(start, durationMs);
    if (this.pageFor(start) || this.pending.has(key)) return;

    const cooldown = this.cooldownUntil.get(key);
    if (cooldown && Date.now() < cooldown) return;

    /*
     * 请求起点单独打一行，且带剧/集身份：只有「页就绪」时，
     * `已启用 → 就绪` 的差值是「前端何时发出」+「服务端多久回包」的和，
     * 日志里分不开；并发多集时也无法配对。不带 URL / 凭据。
     */
    const requestedAt = performance.now();
    const where = `剧=${this.seriesId} 集=${this.episodeId} 起点=${start}ms 终点=${Math.round(durationMs)}ms`;
    tracePlayback(`弹幕请求开始 ${where}`);

    const task = this.fetchPage(this.seriesId, this.episodeId, start, durationMs)
      .then(page => {
        this.pages.set(key, { page, at: Date.now() });
        this.cooldownUntil.delete(key);
        this.failures.delete(key);
        this.prune(positionMs);
        // 打点：弹幕"看不到"时要能立刻区分「没取到」和「取到了没画」。
        tracePlayback(
          `弹幕页就绪 ${where} 条数=${page.items.length} 窗口=${page.startMs}-${page.nextMs} 耗时=${Math.round(performance.now() - requestedAt)}ms`,
        );
      })
      .catch((error: unknown) => {
        const attempts = this.failures.get(key) ?? 0;
        this.failures.set(key, attempts + 1);
        const cooldown = RETRY_COOLDOWNS_MS[Math.min(attempts, RETRY_COOLDOWNS_MS.length - 1)];
        this.cooldownUntil.set(key, Date.now() + cooldown);
        tracePlayback(
          `弹幕页失败 ${where} 第${attempts + 1}次 冷却${cooldown / 1000}s 耗时=${Math.round(performance.now() - requestedAt)}ms 原因=${error instanceof Error ? error.message : String(error)}`,
        );
      })
      .finally(() => {
        this.pending.delete(key);
      });

    this.pending.set(key, task);
  }

  /** 只保留离当前位置最近的几页，避免长剧把内存撑大。 */
  private prune(positionMs: number): void {
    const now = Date.now();
    for (const [start, entry] of [...this.pages]) {
      if (now - entry.at > PAGE_TTL_MS) this.pages.delete(start);
    }
    if (this.pages.size <= PAGE_CACHE_LIMIT) return;
    const sorted = [...this.pages.entries()].sort(
      (a, b) => Math.abs(a[1].page.startMs - positionMs) - Math.abs(b[1].page.startMs - positionMs),
    );
    for (const [start] of sorted.slice(PAGE_CACHE_LIMIT)) {
      this.pages.delete(start);
    }
    if (this.pages.size <= GLOBAL_PAGE_CACHE_LIMIT) return;
    const oldest = [...this.pages.entries()].sort((a, b) => a[1].at - b[1].at);
    for (const [key] of oldest.slice(0, this.pages.size - GLOBAL_PAGE_CACHE_LIMIT)) {
      this.pages.delete(key);
    }
  }

  /** 测试后清掉模块级缓存；生产代码不靠销毁组件清数据。 */
  static resetSharedCacheForTests(): void {
    DanmakuLoader.pages.clear();
    DanmakuLoader.pending.clear();
    DanmakuLoader.cooldownUntil.clear();
    DanmakuLoader.failures.clear();
  }

  dispose(): void {
    this.disposed = true;
    // 成功页、在途请求与冷却均由共享仓库继续持有。旧实现在这里全部清空，
    // 导致一次关开/换集就把 5 分钟缓存变成一次性缓存，并重新打同一接口。
  }
}
