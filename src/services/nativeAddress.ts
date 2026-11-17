/**
 * 「先取地址，再交给 MPV」——非红果来源的地址解析层。
 *
 * ## 为什么需要它
 *
 * `native_player.rs::open` 只在**没有直链**时才去解析，而它只会解析红果：
 * `direct_url` 为空 ⇒ 要求 `vid` 非空 ⇒ `ensure_stream_payload_cached(app, vid)`。
 * 前端在拿不到红果 vid 时传的是 `vid: '0'`，于是这一步必然失败 —— 动漫
 * （`dmghg:` / `bfzy:`）、黄果（`hg:`）、云端（`guo:`）**根本进不了 MPV**。
 * 表现就是"选了 MPV，还是回到内置播放器"，只是现在会多一句解释。
 *
 * `ipc.ts::nativeOpen` 里其实已经有一段"非红果先 `playback_open` 拿 session.url"的
 * 分支，但它**没有收尾**：那一次 `playback_open` 会登记一个播放会话（guo 还会拉起
 * 本地媒体服务），而取完地址就没人再提起它 —— 一路空会话留在后端。
 *
 * 所以这一层做两件事：
 *   1. **先解析**：只在"地址可得、但没有现成直链、且不是纯数字红果 id"时才多一次
 *      往返。红果保持原样（Rust 自己解析更快，前端不插一脚）。
 *   2. **后收尾**：解析出来的会话如果只是用来取地址，拿到地址就立刻收掉。
 *
 * ## 谁会真的被收掉、谁不能收
 *
 * `playback_command { action: 'stop' }` 在 Rust 侧只做两件事
 * （`main.rs::playback_command`）：`media_enhance::stop` 与 `guo_provider::release`。
 *
 * | 来源 | 地址由谁提供 | stop 的实际效果 | 收尾时机 |
 * | --- | --- | --- | --- |
 * | 动漫 `dmghg:` / `bfzy:` | 直链或 `hls_proxy` 全局本地代理 | 两者都不涉及 → 空操作 | **立刻**（transient） |
 * | 黄果 `hg:` | `hls_proxy` 全局本地代理 | 同上 → 空操作 | **立刻**（transient） |
 * | 云端 `guo:` | guo-core 本地媒体服务，**按会话存活** | `release` 会释放这条会话 | **不能立刻收**（held）—— 收了 mpv 直接断流 |
 *
 * `guo:` 的地址不是"取一次就完事"的字符串，它背后就是那个会话；因此这一层对它是
 * "交给原生窗口持有"，等下一次解析顶掉它、或原生窗口关闭时再释放
 * （`releaseHeldNativeAddress`）。这也是最该补的那条 IPC：见文件末尾。
 *
 * ## 为什么不会双路出声
 *
 * 这一层**从不播放**：不起 `<video>`、不调 `playback_command { action: 'play' }`、
 * 不碰 `VideoSurface`。它只读 `session.url`，然后把地址交给 `native_player_open`。
 * 拿到 MPV 的成功回执之后（失败也一样）马上 `stop`，所以那次会话既没有音频、
 * 也不会留在后端空转。`scripts/verify-native-address.mjs` 用桩 IPC 逐条锁住：
 * 只允许出现一次 `playback_open` → 一次 `native_player_open` → 一次 `stop`，
 * 且**永远不出现** `play`。
 *
 * ## 还需要 Rust 侧做的一件事
 *
 * 现在只能在前端"解析 + 收尾"，代价是 `guo:` 的会话寿命没有权威归属。
 * 期望的新 IPC（`main.rs` 里加一条命令即可）：
 *
 *   `playback_resolve_address { seriesId, episodeId, quality }`
 *     → `{ url, streamKind, previewOnly }`
 *
 * 语义：**只解析地址，不登记前端播放会话**。动漫/黄果直接返回地址、不创建会话；
 * `guo:` 由 Rust 自己持有那个 guo-core 会话，并在 `native_player_close`（或窗口
 * 自然退出）时 `release`。有了它，这一层的 `lifetime: 'held'` 分支整段可以删掉。
 */

import type { PlaybackSession } from '../types/playback';
import { generateNextSessionId, ipcService } from './ipc';
import { inferContentKind, isHongguoSeriesId } from './preferredPlaybackLogic';
import { hasNativeAddress } from './playbackRouting';

/**
 * 地址会话的寿命。
 *
 * - `transient`：只用来取地址，mpv 拿到的是**无状态地址**（直链或全局本地代理）→ 立刻收掉。
 * - `held`：地址由这条会话提供（guo-core 本地媒体服务）→ 只能等顶替或窗口关闭再收。
 */
export type NativeAddressLifetime = 'transient' | 'held';

export interface NativeAddressInput {
  seriesId: string;
  episodeId: string;
  /** 调用方已经有的直链；有它就不必解析。 */
  url?: string | null;
  type?: string | null;
  channel?: string | null;
}

/**
 * 这一次启动是否**需要先解析地址**。
 *
 * 三个条件缺一不可，顺序也刻意如此：
 *   1. 没有现成直链（有的话直接用，零往返）；
 *   2. 地址可得（`hasNativeAddress`；不可得时 planner 已经拦下并给了文案）；
 *   3. 不是"纯数字红果剧"——红果由 Rust 的 `ensure_stream_payload_cached` 解析，
 *      前端再解析一次只会多一次往返、多一个会话。
 *
 * 动漫例外：纯数字的动漫剧 id 也长得像红果 id，但它的地址只能从动漫源取，
 * 所以 `kind === 'anime'` 一律算"要解析"。
 */
export function needsNativeAddress(input: NativeAddressInput): boolean {
  if ((input.url ?? '').trim()) return false;
  if (!hasNativeAddress(input)) return false;
  if (inferContentKind(input) === 'anime') return true;
  return !isHongguoSeriesId(input.seriesId);
}

/** 解析出来的地址 + 它背后的会话（收尾时要用）。 */
export interface NativeAddress {
  url: string;
  sessionId: number;
  lifetime: NativeAddressLifetime;
  streamKind: 'hls' | 'file' | null;
}

/** 这一层要用到的外部能力：全部注入，纯逻辑部分可以直接在脚本里驱动。 */
export interface NativeAddressDeps {
  open: (
    seriesId: string,
    episodeId: string,
    quality: string,
    position: number,
    sessionId: number,
    isAnime: boolean,
  ) => Promise<PlaybackSession>;
  /** 收掉会话；Rust 侧 "stop" 只释放增强层与 guo 会话，重复调用无害。 */
  stop: (sessionId: number) => Promise<void>;
  nextSessionId: () => number;
  log: (message: string) => void;
}

export const nativeAddressDeps: NativeAddressDeps = {
  open: (seriesId, episodeId, quality, position, sessionId, isAnime) =>
    ipcService.playback.open(seriesId, episodeId, quality, position, sessionId, isAnime),
  stop: sessionId => ipcService.playback.command(sessionId, 'stop'),
  nextSessionId: () => generateNextSessionId(),
  log: message => {
    // 日志失败绝不能影响播放：`uiLog` 在某些台架里是同步返回的。
    try {
      void Promise.resolve(ipcService.diagnostics.uiLog(message)).catch(() => undefined);
    } catch {
      /* 忽略：诊断日志不是播放链路的一部分 */
    }
  },
};

/** `guo:` 的地址会话要活到"下一次解析或窗口关闭"。 */
function lifetimeFor(seriesId: string): NativeAddressLifetime {
  return seriesId.startsWith('guo:') ? 'held' : 'transient';
}

/**
 * 上一个被顶替、但还挂着的 held 会话（目前只有 `guo:` 会进来）。
 *
 * 模块级单槽而不是 Map：同一时刻只可能有一个原生窗口在播，也就只有一个地址会话
 * 需要延寿。第二部剧开始解析时先把旧的收掉，泄漏上限是 1 条。
 */
let heldAddress: NativeAddress | null = null;

/**
 * 解析这一集的播放地址。
 *
 * 不需要解析（红果 / 已有直链 / 交不出去）时返回 `null` —— 调用方按"没有额外地址"
 * 处理即可，不必区分原因（planner 已经说得比这里更清楚）。
 */
export async function resolveNativeAddress(
  input: NativeAddressInput,
  deps: NativeAddressDeps = nativeAddressDeps,
): Promise<NativeAddress | null> {
  if (!needsNativeAddress(input)) return null;
  const isAnime = inferContentKind(input) === 'anime';
  const sessionId = deps.nextSessionId();
  deps.log(
    `原生地址解析开始 来源=${input.seriesId.split(':')[0]} 会话=${sessionId} 动漫=${isAnime}`,
  );
  const session = await deps.open(input.seriesId, input.episodeId, 'auto', 0, sessionId, isAnime);
  const url = (session?.url ?? '').trim();
  if (!url || session.previewOnly) {
    // 取不到就**抛**：入口会据此给出"没有可播放的整集地址"的文案，而不是静默回落。
    await stopQuietly(session?.sessionId ?? sessionId, deps);
    throw new Error('该来源未返回可播放的整集地址。');
  }
  const address: NativeAddress = {
    url,
    sessionId: session.sessionId ?? sessionId,
    lifetime: lifetimeFor(input.seriesId),
    streamKind: session.streamKind ?? null,
  };
  deps.log(
    `原生地址解析完成 会话=${address.sessionId} 形态=${address.streamKind ?? 'file'} 寿命=${address.lifetime}`,
  );
  return address;
}

async function stopQuietly(sessionId: number, deps: NativeAddressDeps): Promise<void> {
  try {
    await deps.stop(sessionId);
  } catch (error) {
    // 会话可能已被后端回收（retain 只留最近 8 条），或本来就是空操作来源。
    // 收尾失败不能让"已经交给 MPV 的播放"变成一次失败。
    deps.log(
      `原生地址会话收尾失败 会话=${sessionId} ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

/**
 * 交给 MPV 之后收尾。
 *
 * - `transient`：立刻 `stop`。这条会话从创建到收尾**从来没有被播放过**，
 *   也没有任何 `<video>` 指向它的地址，所以不存在第二路音频。
 * - `held`：先记账（顶掉上一条），等 `releaseHeldNativeAddress` 或下一次解析。
 *   收掉它等于掐断 mpv 正在读的本地媒体服务，所以这一步必须延后。
 *
 * 调用点必须是"无论启动成功、失败还是被顶替都要走到"的位置（`finally`）。
 * 失败一次就留一路空会话，比多解析一次更难查。
 */
/** 收尾时的上下文。 */
export interface ReleaseNativeAddressOptions {
  /**
   * 原生窗口是否已经接手这条地址。
   *
   * `false` = 启动失败 / 被顶替 / 提前退出：此时 held 会话也没有存在的意义
   * （没人会去读它），一律按 transient 立刻收掉 —— 否则失败一次就多留一路
   * 后台空会话。
   */
  handedOff?: boolean;
}

export async function releaseNativeAddress(
  address: NativeAddress | null,
  options: ReleaseNativeAddressOptions = {},
  deps: NativeAddressDeps = nativeAddressDeps,
): Promise<void> {
  if (!address) {
    // 没有解析过：顺手把上一条 held 会话留给下一次；这里不主动收，
    // 否则"红果启动"会把正在播的 guo 会话掐掉。
    return;
  }
  if (address.lifetime === 'held' && options.handedOff) {
    const previous = heldAddress;
    heldAddress = address;
    if (previous && previous.sessionId !== address.sessionId) {
      await stopQuietly(previous.sessionId, deps);
    }
    return;
  }
  // transient，或 held 但没能交出去：都没人会读这条地址了。
  await stopQuietly(address.sessionId, deps);
}

/**
 * 收掉"随窗口存活"的地址会话（目前只有 `guo:`）。
 *
 * 原生窗口关闭时调用。晚一点或早一点都不会影响正在播的那一路：调用它的前提是
 * 那个窗口已经不在了。幂等 —— 没有 held 会话时什么都不做。
 */
export async function releaseHeldNativeAddress(
  deps: NativeAddressDeps = nativeAddressDeps,
): Promise<void> {
  const address = heldAddress;
  heldAddress = null;
  if (address) await stopQuietly(address.sessionId, deps);
}
