/**
 * 播放落点的**唯一判定**：这一集交给独立 MPV 窗口，还是留在应用内播放器。
 *
 * ## 为什么要有这个文件
 *
 * 用户选了「MPV 播放器」之后，仍然会在应用内看到画面，而界面上没有任何解释。
 * 根因不是某个调用点写错了一行，而是**判定散落在每个入口里**，各写各的：
 *
 *   - `DetailView` 走启动闸门，`open()` 返回 `false` 时按"没启动"静默落回网页；
 *   - `ExploreView` / `HistoryView` 是 `if (launched) return;` —— 返回 false 就继续往下播；
 *   - 推荐流（feed）根本不过问偏好，直接 `openEpisode`；
 *   - `PlayerControls` 的原生按钮失败只写一行日志。
 *
 * 于是"为什么这一集是应用内在播"这件事，在任何一处都读不出来。这里把判定收成
 * **纯函数**（不碰 IPC / DOM / 全局状态），每个入口先问落点再决定动作，并且把
 * "为什么没交给 MPV"变成一句可以直接显示的文案。
 *
 * ## 判定表（`scripts/verify-playback-routing.mjs` 逐条锁死）
 *
 * | 入口 | surface | 偏好 webview | 偏好 mpv 且地址可得 | 偏好 mpv 且交不出去 |
 * | --- | --- | --- | --- | --- |
 * | 详情页立即播放 / 选集 | `detail` | inline（不提示） | native | inline + 文案 |
 * | 发现页继续观看 / 随机看 | `explore` | inline（不提示） | native | inline + 文案 |
 * | 历史续播 | `history` | inline（不提示） | native | inline + 文案 |
 * | 主播放器「用 MPV 打开」 | `player` | 不适用（按钮本就不出现） | native | inline + 文案 |
 * | 首页竖屏信息流 | `feed` | inline（不提示） | **inline + 文案**（滑动浏览，设计上不弹独立窗口） | inline + 文案 |
 *
 * "地址可得"与 `tryLaunchNativePlayer` 的两个守卫是同一口径：有直链、有红果 vid、
 * 来源 id 带 `:`（`hg:` / `guo:` / `dmghg:` / `bfzy:`），或本身就是动漫
 * —— 动漫靠来源地址解析，不需要红果 vid。
 *
 * 注意 `feed` 那条是本项目既有且刻意的产品决定（见 `PlayerControls` 的
 * `handleOpenNative` 第二道闸与 `FeedView` 头注释）：独立窗口 + 联播队列 + 补帧
 * 属于主播放器的能力，在滑动流里被误触的代价远大于收益。把它写进判定函数之后，
 * 这条排除不再是"某处没写"，而是**一个可以被断言的理由**。
 */

import {
  extractHongguoVid,
  inferContentKind,
  normalizePreferredPlayer,
} from './preferredPlaybackLogic';

/** 播放落点：`native` = 独立 MPV 窗口；`inline` = 应用内播放器。 */
export type PlaybackKernel = 'native' | 'inline';

/** 发起播放的界面形态。只影响一条规则：信息流按设计不弹独立窗口。 */
export type PlaybackSurface = 'detail' | 'explore' | 'history' | 'feed' | 'player';

/** `inline` 落点的原因。`webview-selected` 之外的每一种都必须带可读文案。 */
export type PlaybackInlineReason =
  /** 用户自己选的是网页播放器：这是他的选择，不该提示。 */
  | 'webview-selected'
  /** 独立播放器总闸关闭（这个构建没有播放器二进制）。 */
  | 'native-unavailable'
  /** 信息流是滑动浏览形态，按设计固定走应用内播放。 */
  | 'feed-surface'
  /** 这一集没有可交给 MPV 的直链 / vid / 来源地址。 */
  | 'no-native-address';

export type PlaybackRouteDecision =
  | { kernel: 'native'; reason: 'mpv-selected' }
  | { kernel: 'inline'; reason: PlaybackInlineReason; notice: string | null };

export interface PlaybackRouteInput {
  /** 设置里的 `preferredPlayer`；非法值按 `webview` 处理（与设置归一化同源）。 */
  preferredPlayer: unknown;
  /** `NATIVE_PLAYER_AVAILABLE`：由调用方注入，判定本身保持纯函数、可测。 */
  nativeAvailable: boolean;
  seriesId: string;
  episodeId: string;
  /** 调用方已经解析好的直链（动漫/黄果换集等）。 */
  url?: string | null;
  type?: string | null;
  channel?: string | null;
  surface?: PlaybackSurface;
}

/**
 * 回落到应用内时给用户看的一句话。
 *
 * 语气对齐项目既有文案（`DetailView` 的「无法打开 MPV 播放器，已改用网页播放。」）：
 * 先说事实，再说"已经替你改成了什么"，不解释实现、不劝用户改设置。
 */
export const PLAYBACK_INLINE_NOTICES: Record<
  Exclude<PlaybackInlineReason, 'webview-selected'>,
  string
> = {
  'native-unavailable': '当前版本没有可用的 MPV 播放器，已改用网页播放。',
  'feed-surface': '信息流按滑动浏览设计，固定用应用内播放；要用 MPV 请从详情页或历史进入。',
  'no-native-address': '这一集没有可交给 MPV 播放器的地址，已改用网页播放。',
};

/** `tryLaunchNativePlayer` 返回 `false` 时对外统一的那句话（与 `no-native-address` 同因）。 */
export const NATIVE_SKIPPED_NOTICE = PLAYBACK_INLINE_NOTICES['no-native-address'];

/** 原生窗口确实打不开（抛错 / 闸门超时）时的那句话，沿用 `DetailView` 既有文案。 */
export const NATIVE_OPEN_FAILED_NOTICE = '无法打开 MPV 播放器，已改用网页播放。';

/**
 * 这一集有没有可交给原生播放器的地址。
 *
 * 与 `preferredPlayback.tryLaunchNativePlayer` 的两个守卫**同一条口径**：
 * 直链优先；红果剧按数字 id 取分集 vid；带前缀的来源（`hg:` / `guo:` /
 * `dmghg:` / `bfzy:`）由后端按来源解析；动漫不需要 vid。
 */
export function hasNativeAddress(input: {
  seriesId: string;
  episodeId: string;
  url?: string | null;
  type?: string | null;
  channel?: string | null;
}): boolean {
  if ((input.url ?? '').trim()) return true;
  if (extractHongguoVid(input.episodeId, input.seriesId)) return true;
  if ((input.seriesId ?? '').includes(':')) return true;
  return inferContentKind({
    type: input.type,
    channel: input.channel,
    seriesId: input.seriesId,
  }) === 'anime';
}

/**
 * 判定这一集的播放落点。
 *
 * 顺序是有意的：先看用户选了什么，再看播放器在不在，再看这个界面形态允不允许，
 * 最后才看这一集有没有地址。"有没有地址"只在前三关都过了之后才值得算 —— 它也
 * 正是旧实现里唯一会静默返回 `false` 的地方。
 */
export function planPlaybackRoute(input: PlaybackRouteInput): PlaybackRouteDecision {
  if (normalizePreferredPlayer(input.preferredPlayer) !== 'mpv') {
    return { kernel: 'inline', reason: 'webview-selected', notice: null };
  }
  if (!input.nativeAvailable) {
    return {
      kernel: 'inline',
      reason: 'native-unavailable',
      notice: PLAYBACK_INLINE_NOTICES['native-unavailable'],
    };
  }
  if (input.surface === 'feed') {
    return {
      kernel: 'inline',
      reason: 'feed-surface',
      notice: PLAYBACK_INLINE_NOTICES['feed-surface'],
    };
  }
  if (!hasNativeAddress(input)) {
    return {
      kernel: 'inline',
      reason: 'no-native-address',
      notice: PLAYBACK_INLINE_NOTICES['no-native-address'],
    };
  }
  return { kernel: 'native', reason: 'mpv-selected' };
}

/** 这次回落要不要向用户解释；`null` = 用户自己的选择，不必打扰。 */
export function playbackNoticeFor(decision: PlaybackRouteDecision): string | null {
  return decision.kernel === 'inline' ? decision.notice : null;
}
