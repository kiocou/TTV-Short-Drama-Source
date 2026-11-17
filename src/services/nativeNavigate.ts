import { isTauriEnvironment, ipcService } from './ipc';
import type { NativeQueueItem } from './ipc';
import { contentTypeFor } from './contentType';
import { extractHongguoVid } from './preferredPlaybackLogic';
import { saveNativeFgPreferences } from './preferredPlaybackLogic';
import type { SeriesDetail } from '../types/series';

/**
 * 原生播放器 ⇄ 前端的「联播导航桥」。
 *
 * ## 为什么需要这一层
 *
 * 原生播放器（`NativePlayer.exe`）是独立进程，它只知道自己当前这一集的 url，
 * **不持有剧集列表**。所以任何需要剧集上下文的动作（"下一集是哪一集"）都必须
 * 回到前端来算。整条链路（契约见 `docs/native-player-redesign.md` §3.2 / §3.3）：
 *
 * ```
 * Rust（native_player.rs）          前端（本文件）                     原生播放器
 * ────────────────────────          ────────────────────────────      ─────────────
 * prev/next ──────────────────────► native-player-navigate
 *                                     ├─ store 切到目标集（内嵌保持暂停）
 *                                     └─ 解析直链 → nativeSend{cmd:'load'} ──► 换集
 * requestNext ────────────────────► native-player-need-next
 *                                     ├─ 解析下一集直链
 *                                     └─ nativeQueuePush(item) ──────────► 队列补一项
 * 8 秒没回执 ─────────────────────► native-player-queue-timeout
 * advance ────────────────────────► native-player-advance
 *                                     └─ 前端「当前集」跟上（同样保持暂停）
 * autoplayChanged ────────────────► native-player-autoplay-changed
 *                                     └─ 写 settings.autoNext
 * ```
 *
 * ## 事件名是硬契约
 *
 * `NATIVE_NAVIGATE_EVENTS` 里的 5 个字符串必须与 `src-tauri/src/native_player.rs`
 * 里的 `emit(...)` 逐字一致。改任何一边都要同步另一边 ——
 * `scripts/verify-native-navigate.mjs` 把这条钉死（少一个事件名脚本就红）。
 *
 * ## 为什么由宿主注入句柄（而不是这里直接读 store）
 *
 * 本文件是纯 service，不在 React 树里，拿不到 `usePlaybackStore()`。
 * 由播放页常驻的 `PlayerControls` 调用 `registerNativeNavigateHost()` 注入一个
 * 句柄；bridge 每次处理事件都去句柄里读**最新**快照，绝不缓存剧集列表
 * （缓存会让"用户刚换了剧"之后的联播跑到上一部剧上）。
 *
 * ## Web 环境
 *
 * 非 Tauri 环境一切静默：`startNativeNavigateBridge()` 返回空取消函数，
 * 不 `import('@tauri-apps/api/event')`、不打日志 —— 浏览器里没有原生播放器，
 * 报错只会污染控制台。
 */

// ---------------------------------------------------------------------------
// 契约常量
// ---------------------------------------------------------------------------

/**
 * 事件名（与 Rust 侧逐字一致）。
 *
 * ⚠️ 不要改成模板字符串拼出来的形式：`verify:native-navigate` 直接读源码断言
 * 这五个字面量存在，拼出来的名字既查不到、也会让两侧悄悄错开。
 */
export const NATIVE_NAVIGATE_EVENTS = {
  /** 用户在原生窗口点了上一集/下一集：{ dir: 'prev' | 'next', pos }。 */
  navigate: 'native-player-navigate',
  /** 联播自动推进了一集：{ index, ep, fromIndex }。 */
  advance: 'native-player-advance',
  /** 队列空了，要前端解析下一集：{ index, ep }。 */
  needNext: 'native-player-need-next',
  /** 前端 8 秒没回执：{ index, ep }。 */
  queueTimeout: 'native-player-queue-timeout',
  /** 用户在原生窗口拨了联播开关：{ on }。 */
  autoplayChanged: 'native-player-autoplay-changed',
} as const;

/** 原生播放器状态轮询间隔（毫秒）。宿主与徽标共用同一个口径。 */
export const NATIVE_STATUS_POLL_MS = 2000;

/**
 * `need-next` 的重复抑制窗口。
 *
 * Rust 侧等 8 秒才判超时；在这 8 秒里若原生播放器又送了一次 `requestNext`
 * （换集失败重试、心跳抖动），前端不该再解析一遍 —— 红果一次解析是几百毫秒
 * 到几秒的真实开销，重复解析还会白白吃掉设备配额。窗口取 6 秒：比超时短，
 * 超时之后允许重来。
 */
const NEED_NEXT_DEDUPE_MS = 6000;

// ---------------------------------------------------------------------------
// 宿主句柄契约
// ---------------------------------------------------------------------------

/** 桥需要的剧集最小形状（`EpisodeItem` 的子集，避免 service 反向依赖 types）。 */
export interface NativeNavigateEpisode {
  id: string;
  episodeNumber: number;
  title?: string;
  durationSeconds?: number;
}

/**
 * 联播队列项 = `ipc.ts` 的 `NativeQueueItem`（契约 §3.1 的 `queue.items[]`）。
 *
 * 这里只是换个名字导出，**不另写一份结构**：两处各写一份结构是同名不同形的开始
 * （`NativeQueueItem.url` 是必填的 —— 没直链的项 Rust 会明确报错）。
 */
export type NativeQueueEntry = NativeQueueItem;

/**
 * 宿主（`PlayerControls`）注入的运行时句柄。
 *
 * 所有方法都必须是**读最新值**的实现（宿主内部用 ref 转发），
 * 因为 bridge 只注册一次、不会随剧集切换重新注册。
 */
export interface NativeNavigateHost {
  /** 当前剧的剧集列表快照。 */
  getEpisodes: () => NativeNavigateEpisode[];
  /** 当前集在列表里的 0 基索引（找不到返回 -1）。 */
  getCurrentIndex: () => number;
  getSeriesId: () => string | null;
  getSeriesTitle: () => string;
  /** 剧的频道类型（`drama` / `comic` / `anime` / `ai` …），用于推断 content_type。 */
  getSeriesType: () => string | null;
  /** 当前音量（0..1）。 */
  getVolume: () => number;
  /** 当前播放位置（秒）。 */
  getPosition: () => number;
  /**
   * 把前端「当前集」切到 `index`，并**保持内嵌暂停**。
   *
   * 只在用户**显式**操作（原生窗口里点 ⏮/⏭）时调用 —— 那一下本来就是用户的意图，
   * 前端跟上是对的。
   */
  switchToEpisode: (index: number, startSeconds: number) => void;
  /**
   * 联播自动推进到了第 `ep` 集（原生窗口自己换的集）。
   *
   * 宿主**只记录、不解析内嵌源**：`switchToEpisode` 会真的走一次 openEpisode，
   * 而那会占用 NVENC / worker（AGENTS.md §4 记录过：编码单元是串行的，
   * 后台解析会把正在播的那一路挤慢）。前端此刻也看不到自己的画面（在原生窗口里），
   * 所以这笔开销完全没必要 —— 等用户关掉原生窗口时再一次性对齐
   * （`syncAfterNativeExit`）。
   */
  noteAdvance: (ep: number) => void;
  /** 关闭原生窗口时调用：把内嵌切到记录的集（没有记录则恢复播放）。 */
  syncAfterNativeExit: () => void;
  /** 兜底：立刻把内嵌播放按停（原生窗口在播时前端不许出声）。 */
  ensureEmbeddedPaused: () => void;
  /** 联播开关写进全局设置（`settings.autoNext`）。 */
  setAutoNext: (on: boolean) => void;
  /** 诊断日志（宿主转到 `ipcService.diagnostics.uiLog`）。 */
  log: (message: string) => void;
}

// ---------------------------------------------------------------------------
// 模块状态（单例）
// ---------------------------------------------------------------------------

let host: NativeNavigateHost | null = null;
let standaloneHost: NativeNavigateHost | null = null;
let stopStandalone: (() => void) | null = null;
let navigationGeneration = 0;
let prefetchGeneration = 0;
let started = false;
let subscribers = 0;
let unlisteners: Array<() => void> = [];
/** 每次 start/stop 递增：异步注册途中被 stop 掉，回来时据此丢弃句柄。 */
let generation = 0;
/** 去重表：need-next 的 key -> 上次处理时间。 */
const recentNeedNext = new Map<string, number>();
/** 去重表：后台预加载的 key -> 上次处理时间。 */
const recentPrefetch = new Map<string, number>();

// ---------------------------------------------------------------------------
// 事件名 → 负载读取（负载都是 Rust 侧 json! 出来的对象）
// ---------------------------------------------------------------------------

function readNumber(payload: unknown, key: string): number | null {
  if (!payload || typeof payload !== 'object') return null;
  const value = (payload as Record<string, unknown>)[key];
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string' && value.trim() !== '' && Number.isFinite(Number(value))) {
    return Number(value);
  }
  return null;
}

function readString(payload: unknown, key: string): string | null {
  if (!payload || typeof payload !== 'object') return null;
  const value = (payload as Record<string, unknown>)[key];
  return typeof value === 'string' ? value : null;
}

function readBoolean(payload: unknown, key: string): boolean | null {
  if (!payload || typeof payload !== 'object') return null;
  const value = (payload as Record<string, unknown>)[key];
  if (typeof value === 'boolean') return value;
  if (typeof value === 'number') return value !== 0;
  if (typeof value === 'string') {
    const normalized = value.trim().toLowerCase();
    if (normalized === 'true' || normalized === '1' || normalized === 'on') return true;
    if (normalized === 'false' || normalized === '0' || normalized === 'off') return false;
  }
  return null;
}

function clamp01(value: number): number {
  if (!Number.isFinite(value)) return 0.8;
  return Math.min(1, Math.max(0, value));
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 统一出口：没有宿主时只静默返回 false（bridge 可能比播放页先起来）。 */
function bridgeLog(message: string): void {
  if (host) {
    host.log(message);
    return;
  }
  // 宿主还没挂上：这是一条"没人接"的诊断，用 console 兜底而不是丢掉。
  // 只在 Tauri 环境里打，Web 环境不制造噪音。
  if (isTauriEnvironment()) {
    console.warn(`[nativeNavigate] ${message}`);
  }
}

// ---------------------------------------------------------------------------
// 句柄注入
// ---------------------------------------------------------------------------

/**
 * 注入宿主句柄（`PlayerControls` 挂载时调用）。
 *
 * 重复注入直接覆盖：播放页重挂时旧句柄已经失效，保留它只会读到过期闭包。
 */
export function registerNativeNavigateHost(next: NativeNavigateHost, takeover = false): void {
  if (standaloneHost && standaloneHost !== next && !takeover) return;
  if (standaloneHost && standaloneHost !== next) {
    stopStandalone?.();
    stopStandalone = null;
    standaloneHost = null;
  }
  host = next;
  navigationGeneration += 1;
  recentNeedNext.clear();
}

/** 首选 MPV 入口仅保存元数据，不为联播启动内嵌媒体解析。 */
export function registerStandaloneNativeNavigation(input: {
  detail: SeriesDetail;
  index: number;
  volume: number;
  startSeconds: number;
  setAutoNext?: (on: boolean) => void;
}): void {
  let index = input.index;
  let position = input.startSeconds;
  const next: NativeNavigateHost = {
    getEpisodes: () => input.detail.episodes,
    getCurrentIndex: () => index,
    getSeriesId: () => input.detail.id,
    getSeriesTitle: () => input.detail.title,
    getSeriesType: () => input.detail.type,
    getVolume: () => input.volume,
    getPosition: () => position,
    switchToEpisode: (value, start) => { index = value; position = start; },
    noteAdvance: ep => { index = input.detail.episodes.findIndex(item => item.episodeNumber === ep); position = 0; },
    ensureEmbeddedPaused: () => {},
    syncAfterNativeExit: () => {},
    setAutoNext: on => input.setAutoNext?.(on),
    log: message => { void ipcService.diagnostics.uiLog(message); },
  };
  registerNativeNavigateHost(next, true);
  standaloneHost = next;
  stopStandalone = startNativeNavigateBridge();
}

/**
 * 撤销注入。
 *
 * 传了句柄就只在**它自己是当前句柄**时撤销 —— 避免卸载顺序错乱时
 * 旧宿主的 cleanup 把新宿主的句柄清掉（React 严格模式下 effect 会跑两遍，
 * 这个防护是必要的）。
 */
export function releaseNativeNavigateHost(target?: NativeNavigateHost): void {
  if (!host) return;
  if (target && host !== target) return;
  host = null;
}

export function isNativeNavigateHostActive(target: NativeNavigateHost): boolean {
  return host === target;
}

// ---------------------------------------------------------------------------
// 广播启动 / 停止（单例 + 引用计数）
// ---------------------------------------------------------------------------

/**
 * 启动 bridge，返回取消订阅函数。
 *
 * 单例守卫：重复调用只是增加引用计数，**不会**重复 `listen`（重复注册会让一次
 * 换集被处理两遍，表现为前端连跳两集）。最后一个订阅者取消时才真正解绑。
 */
export function startNativeNavigateBridge(): () => void {
  subscribers += 1;
  if (!started) {
    started = true;
    void bootstrap();
  }
  let released = false;
  return () => {
    if (released) return;
    released = true;
    subscribers = Math.max(0, subscribers - 1);
    if (subscribers === 0) teardown();
  };
}

async function bootstrap(): Promise<void> {
  if (!isTauriEnvironment()) {
    // Web：不加载 @tauri-apps/api、不打日志。取消函数照样有效（teardown 空转）。
    return;
  }
  const gen = ++generation;
  try {
    const { listen } = await import('@tauri-apps/api/event');
    const bindings: Array<[string, (payload: unknown) => void]> = [
      [NATIVE_NAVIGATE_EVENTS.navigate, handleNavigate],
      [NATIVE_NAVIGATE_EVENTS.advance, handleAdvance],
      [NATIVE_NAVIGATE_EVENTS.needNext, handleNeedNext],
      ['native-player-prefetch-next', handlePrefetchNext],
      [NATIVE_NAVIGATE_EVENTS.queueTimeout, handleQueueTimeout],
      [NATIVE_NAVIGATE_EVENTS.autoplayChanged, handleAutoplayChanged],
      ['native-player-fg-changed', handleFgChanged],
    ];
    for (const [eventName, handler] of bindings) {
      const off = await listen<unknown>(eventName, event => {
        try {
          handler(event.payload);
        } catch (error) {
          // 单个事件处理失败不允许拖垮整条桥（监听器抛错会打断后续事件派发）。
          bridgeLog(`${eventName} 处理失败：${describe(error)}`);
        }
      });
      if (gen !== generation) {
        // 注册途中已经被 teardown：立刻解绑，别留下野句柄。
        try {
          off();
        } catch {
          /* 解绑失败不影响结果 */
        }
        continue;
      }
      unlisteners.push(off);
    }
  } catch (error) {
    bridgeLog(`订阅原生播放器事件失败：${describe(error)}`);
  }
}

function teardown(): void {
  started = false;
  generation += 1;
  for (const off of unlisteners) {
    try {
      off();
    } catch {
      /* 解绑失败不影响结果 */
    }
  }
  unlisteners = [];
  recentNeedNext.clear();
  recentPrefetch.clear();
}

// ---------------------------------------------------------------------------
// 事件处理
// ---------------------------------------------------------------------------

/**
 * 用户在原生窗口点了 ⏮ / ⏭。
 *
 * 两件事必须同时做：
 * 1. 前端「当前集」跟上（否则回到内嵌时对的是旧那一集）；
 * 2. 把目标集的直链解析出来，`load` 下发给原生播放器 —— 播放器自己不会换集，
 *    它只发意图。
 *
 * 解析失败不回滚前端的集号：前端显示"第 N 集"而播放器停在第 N-1 集，
 * 比"两边都停在旧集但日志里有一行失败"更糟。所以只在解析成功后才认为切集成立
 * —— 前端切集放在解析**之前**是因为用户已经在原生窗口按了按钮，前端不动更奇怪；
 * 失败时日志会写清是哪一集没解析出来。
 */
function handleNavigate(payload: unknown): void {
  const current = host;
  if (!current) {
    bridgeLog('收到 native-player-navigate，但播放页没有挂上宿主（忽略）');
    return;
  }
  if (!matchesSource(payload, current)) return;
  const episodes = current.getEpisodes();
  const currentIndex = current.getCurrentIndex();
  if (episodes.length === 0 || currentIndex < 0) {
    current.log('收到上一集/下一集请求，但前端没有当前剧集列表（忽略）');
    return;
  }
  const rawDir = readString(payload, 'dir');
  let targetIndex: number;
  let actionLabel: string;
  if (rawDir === 'jump') {
    const explicitIndex = readNumber(payload, 'index');
    if (explicitIndex === null) {
      current.log('收到 native-player-navigate dir=jump，但缺少 index（忽略）');
      return;
    }
    targetIndex = Math.trunc(explicitIndex);
    actionLabel = `选集跳转（第 ${targetIndex + 1} 集）`;
  } else {
    const dir = rawDir === 'prev' ? 'prev' : 'next';
    targetIndex = dir === 'prev' ? currentIndex - 1 : currentIndex + 1;
    actionLabel = dir === 'prev' ? '上一集' : '下一集';
  }
  if (targetIndex < 0 || targetIndex >= episodes.length) {
    current.log(
      `原生播放器请求${actionLabel}，但已到边界（第 ${currentIndex + 1}/${episodes.length} 集）`,
    );
    return;
  }
  // 上一集回到片头；下一集同样从 0 起（联播不继承上一集的播放位置）。
  current.switchToEpisode(targetIndex, 0);
  current.ensureEmbeddedPaused();
  void pushLoad(targetIndex, 0);
}

/**
 * 联播自动推进了一集（播放器自己换的集）。
 *
 * 这里**不下发 `load`**（播放器已经在自己播新的一集了，再发一次会从头重载），
 * 也**不解析内嵌源**（`noteAdvance` 的注释解释了为什么）。前端只需要记住"看到
 * 第几集了"，等用户关掉原生窗口时一次性对齐。
 *
 * 用 `ep`（集号）而不是 `index` 对齐：Rust 侧在 `queue_push` 之后会把队列重置成
 * 单元素，此时 `advance_index` 只能把 index 夹在 0..0，带出来的 index 可能是 0，
 * 而集号是用户真正看到的那个事实。
 */
function handleAdvance(payload: unknown): void {
  const current = host;
  if (!current) return;
  if (!matchesSource(payload, current)) return;
  const ep = readNumber(payload, 'ep');
  if (ep === null || ep <= 0) {
    bridgeLog('收到 native-player-advance，但负载里没有有效 ep（忽略）');
    return;
  }
  const episodes = current.getEpisodes();
  const matched = episodes.findIndex(item => item.episodeNumber === ep);
  if (matched < 0) {
    current.log(`联播推进到第 ${ep} 集，但前端剧集列表里没有这一集（列表可能刚被换掉）`);
    return;
  }
  if (matched === current.getCurrentIndex()) {
    // 已经对齐：重复的 advance 不该再记一遍。
    return;
  }
  current.noteAdvance(ep);
  current.ensureEmbeddedPaused();
  current.log(`联播推进到第 ${ep} 集（前端待同步，内嵌保持暂停；关闭原生窗口时一次性对齐）`);
}

/**
 * 队列里没有可播的下一集：前端解析后推回 `native_queue_push`。
 *
 * 为什么由前端解析：Rust 侧不持有剧集列表（`native_player.rs` 头注释），
 * 它只知道"还差一集"。剧集 → vid → 直链 这段只有前端（和它背后的 resolve 命令）
 * 能走通。
 */
function handleNeedNext(payload: unknown): void {
  const current = host;
  if (!current) {
    bridgeLog('收到 native-player-need-next，但播放页没有挂上宿主（忽略）');
    return;
  }
  if (!matchesSource(payload, current)) return;
  const episodes = current.getEpisodes();
  const rawIndex = readNumber(payload, 'index');
  const currentIndex = current.getCurrentIndex();
  const targetIndex = rawIndex !== null && rawIndex >= 0 ? rawIndex : currentIndex + 1;
  const episode = episodes[targetIndex];
  if (!episode) {
    current.log(
      `原生播放器要下一集（index=${targetIndex}），但前端只有 ${episodes.length} 集 —— 这部剧播完了`,
    );
    return;
  }
  const ep = readNumber(payload, 'ep') ?? episode.episodeNumber;
  const dedupeKey = `${current.getSeriesId()}:${targetIndex}:${episode.id}`;
  const now = Date.now();
  const last = recentNeedNext.get(dedupeKey) ?? 0;
  if (now - last < NEED_NEXT_DEDUPE_MS) {
    // Rust 侧 8 秒超时内不要重复解析：一次红果解析是真实开销，重复解析还会吃设备配额。
    return;
  }
  recentNeedNext.set(dedupeKey, now);
  void pushQueueItem(targetIndex, episode, ep, false);
}

/** 播放期间后台预解析下一集：不打断当前集，解析完静默预推入播放器队列。 */
function handlePrefetchNext(payload: unknown): void {
  const current = host;
  if (!current || !matchesSource(payload, current)) return;
  const episodes = current.getEpisodes();
  const rawIndex = readNumber(payload, 'index');
  const currentIndex = current.getCurrentIndex();
  const targetIndex = rawIndex !== null && rawIndex >= 0 ? rawIndex : currentIndex + 1;
  const episode = episodes[targetIndex];
  if (!episode) return;
  const ep = readNumber(payload, 'ep') ?? episode.episodeNumber;
  const dedupeKey = `${current.getSeriesId()}:${targetIndex}:${episode.id}`;
  const now = Date.now();
  const last = recentPrefetch.get(dedupeKey) ?? 0;
  if (now - last < 15_000) return;
  recentPrefetch.set(dedupeKey, now);
  void pushQueueItem(targetIndex, episode, ep, true);
}

/** 前端 8 秒没把 `queue_push` 送回去：如实报告，播放器那边已经停住了。 */
function handleQueueTimeout(payload: unknown): void {
  const current = host;
  if (current && !matchesSource(payload, current)) return;
  const index = readNumber(payload, 'index');
  const ep = readNumber(payload, 'ep');
  // 超时后允许下一次重试，清掉去重记录。
  if (index !== null) {
    for (const key of Array.from(recentNeedNext.keys())) {
      if (key.startsWith(`${current?.getSeriesId()}:${index}:`)) recentNeedNext.delete(key);
    }
  }
  const message = `原生播放器等下一集超时（index=${index ?? '?'}，第 ${ep ?? '?'} 集）：播放已停住`;
  if (current) current.log(message);
  else bridgeLog(message);
}

/**
 * 用户在原生窗口拨了联播开关：同步到设置。
 *
 * 落库这件事 Rust 侧已经做了（`native_player.rs` 的 `persist_autoplay`），
 * 这里再写一次是**幂等**的，目的是让前端内存里的 `settings.autoNext` 跟上 ——
 * 否则设置页会继续显示旧值，直到下次冷启动才刷新。
 */
function handleAutoplayChanged(payload: unknown): void {
  const current = host;
  const on = readBoolean(payload, 'on');
  if (on === null) {
    bridgeLog('收到 native-player-autoplay-changed，但负载里没有 on（忽略）');
    return;
  }
  if (!current) {
    bridgeLog(`原生播放器联播开关 → ${on ? '开' : '关'}，但播放页没有挂上宿主（未落库）`);
    return;
  }
  current.setAutoNext(on);
  current.log(`原生播放器联播开关 → ${on ? '开' : '关'}（已同步到设置）`);
}

function handleFgChanged(payload: unknown): void {
  if (!payload || typeof payload !== 'object') return;
  const p = payload as Record<string, unknown>;
  saveNativeFgPreferences(p.mult, p.fgMode);
}

// ---------------------------------------------------------------------------
// 解析 + 下发（唯一的两个"往外写"出口）
// ---------------------------------------------------------------------------

/**
 * 解析某一集的直链并给原生播放器下发 `load`。
 *
 * 红果按 vid 解析，其它来源保留分集 id，由统一 IPC 按剧集来源取地址。
 */
async function pushLoad(index: number, startSeconds: number): Promise<void> {
  const current = host;
  if (!current) return;
  const episodes = current.getEpisodes();
  const episode = episodes[index];
  const seriesId = current.getSeriesId();
  if (!episode || !seriesId) {
    bridgeLog(`换集失败：index=${index} 找不到剧集或剧 id（series=${seriesId ?? 'null'}）`);
    return;
  }
  const vid = current.getSeriesType() === 'anime' ? null : extractHongguoVid(episode.id, seriesId);
  const requestGeneration = ++navigationGeneration;
  try {
    const resolved = await ipcService.playback.resolveNative(
      seriesId,
      vid ?? episode.id,
      contentTypeFor(current.getSeriesType()),
      'auto',
      undefined,
      current.getSeriesType() === 'anime',
    );
    if (host !== current || current.getSeriesId() !== seriesId || requestGeneration !== navigationGeneration) return;
    const url = (resolved?.playUrl ?? '').trim();
    if (!url) throw new Error('解析结果没有直链');
    const volume = Math.round(clamp01(current.getVolume()) * 100);
    await ipcService.playback.nativeSend({
      cmd: 'load',
      seriesId,
      episodeId: episode.id,
      ...(vid ? { vid } : {}),
      url,
      start: Math.max(0, startSeconds),
      title: current.getSeriesTitle(),
      ep: episode.episodeNumber,
      /**
       * ⚠️ `total` 在这个协议里是**本集时长（秒）**，不是总集数 ——
       * `Program.cs::ApplyLoad` 里是 `totalSeconds = Math.Max(0.1, req.total)`。
       * 总集数走契约 §3.1 新增的 `totalEps`。把集数塞进 `total` 会让进度条
       * 一开播就顶到头上。
       */
      total: Math.max(0, episode.durationSeconds ?? 0),
      totalEps: episodes.length,
      index,
      vw: resolved?.width && resolved.width > 0 ? resolved.width : undefined,
      vh: resolved?.height && resolved.height > 0 ? resolved.height : undefined,
      volume,
      kind: current.getSeriesType() ?? undefined,
    });
    current.log(`原生播放器已换到第 ${episode.episodeNumber} 集（index=${index}）`);
  } catch (error) {
    current.log(`原生播放器换集失败（第 ${episode.episodeNumber} 集）：${describe(error)}`);
  }
}

/**
 * 解析某一集的直链并推回联播队列（`native_queue_push`）。
 *
 * 这是"队列里没有可播项"那条路的回执：Rust 拿不到 vid/直链（它不持有剧集列表），
 * 只有前端能把 `episodes[index]` 变成一条可播的 url。8 秒内推不回去，Rust 会
 * emit `native-player-queue-timeout`，播放器停在"等下一集"上。
 */
async function pushQueueItem(
  index: number,
  episode: NativeNavigateEpisode,
  episodeNumber: number,
  isPrefetch = false,
): Promise<void> {
  const current = host;
  if (!current) return;
  const seriesId = current.getSeriesId();
  if (!seriesId) {
    bridgeLog('推队列失败：没有当前剧 id');
    return;
  }
  const vid = current.getSeriesType() === 'anime' ? null : extractHongguoVid(episode.id, seriesId);
  const navGen = navigationGeneration;
  const pfGen = isPrefetch ? ++prefetchGeneration : ++navigationGeneration;
  try {
    const resolved = await ipcService.playback.resolveNative(
      seriesId,
      vid ?? episode.id,
      contentTypeFor(current.getSeriesType()),
      'auto',
      undefined,
      current.getSeriesType() === 'anime',
    );
    if (
      host !== current
      || current.getSeriesId() !== seriesId
      || (isPrefetch
        ? (pfGen !== prefetchGeneration || navGen !== navigationGeneration)
        : pfGen !== navigationGeneration)
    ) {
      return;
    }
    const url = (resolved?.playUrl ?? '').trim();
    if (!url) throw new Error('解析结果没有直链');
    const item = buildNativeQueueEntry({
      url,
      seriesId,
      contentKind: current.getSeriesType() ?? undefined,
      episodeId: episode.id,
      episodeNumber,
      title: current.getSeriesTitle(),
      total: current.getEpisodes().length,
      index,
      width: resolved?.width,
      height: resolved?.height,
    });
    if (!item) throw new Error(`第 ${episodeNumber} 集缺少可播放地址（${episode.id}）`);
    await ipcService.playback.nativeQueuePush(isPrefetch ? { ...item, prefetch: true } : item);
    current.log(
      isPrefetch
        ? `下一集（第 ${episodeNumber} 集）已提前预加载进队列（index=${index}）`
        : `下一集（第 ${episodeNumber} 集）已入队（index=${index}）`,
    );
  } catch (error) {
    if (!isPrefetch) {
      current.log(`下一集（第 ${episodeNumber} 集）解析失败：${describe(error)}`);
    }
  }
}

function matchesSource(payload: unknown, current: NativeNavigateHost): boolean {
  const seriesId = readString(payload, 'seriesId');
  return !seriesId || seriesId === current.getSeriesId();
}

/**
 * 构造一条联播队列项（`PlayerControls` 打开原生播放器时用它拼初始队列）。
 *
 * `url` 必须由调用方**先解析好**再传：`NativeQueueItem.url` 是必填的，队列里放一项
 * 没有直链的项，等播放器真的播到它时会明确报错（而不是静默跳过）——所以这里
 * 不做"占位项"，没有地址就返回 null；其它源的分集无需红果 vid。
 *
 * 多带的 `episodeId` / `vid` 不是 §3.1 的最小字段，但 Rust 侧 `item_episode_id`
 * 会优先采用它们：联播换集后观看历史能落到**正确那一集**，否则会沿用旧
 * episodeId（`native_player.rs` 里标注的已知缺口）。老播放器忽略未知键。
 */
export function buildNativeQueueEntry(input: {
  url: string;
  seriesId?: string;
  contentKind?: string;
  episodeId: string;
  episodeNumber: number;
  title: string;
  total: number;
  index: number;
  startSeconds?: number;
  width?: number;
  height?: number;
}): NativeQueueEntry | null {
  const vid = input.contentKind === 'anime' ? null : extractHongguoVid(input.episodeId, input.seriesId);
  const url = (input.url ?? '').trim();
  if (!url) return null;
  return {
    url,
    vid: vid ?? undefined,
    seriesId: input.seriesId,
    contentKind: input.contentKind,
    episodeId: input.episodeId,
    title: input.title,
    ep: input.episodeNumber,
    total: input.total,
    vw: input.width && input.width > 0 ? input.width : undefined,
    vh: input.height && input.height > 0 ? input.height : undefined,
    start: Math.max(0, input.startSeconds ?? 0),
    index: input.index,
  };
}

/** 保留前后所有集的元数据；下一集预解析由 Rust 的异步预取负责。 */
export function buildInitialNativeQueue(input: {
  seriesId: string;
  seriesTitle: string;
  seriesType: string | null;
  episodes: NativeNavigateEpisode[];
  currentIndex: number;
  startSeconds: number;
}): NativeQueueItem[] {
  if (!input.episodes[input.currentIndex]) return [];
  return input.episodes.map((episode, index) => ({
    url: '',
    vid: input.seriesType === 'anime' ? undefined : extractHongguoVid(episode.id, input.seriesId) ?? undefined,
    seriesId: input.seriesId,
    contentKind: input.seriesType ?? undefined,
    episodeId: episode.id,
    title: input.seriesTitle,
    ep: episode.episodeNumber,
    total: input.episodes.length,
    index,
    start: index === input.currentIndex ? Math.max(0, input.startSeconds) : 0,
  }));
}
