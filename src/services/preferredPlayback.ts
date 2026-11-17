import { ipcService } from './ipc';
import { readTheme } from './theme';
import { NATIVE_PLAYER_AVAILABLE } from './nativePlayerRollout';
import { buildInitialNativeQueue, registerStandaloneNativeNavigation } from './nativeNavigate';
import type { SeriesDetail } from '../types/series';
import type { HistoryType } from '../types/history';
import {
  inferContentKind,
  interpolationPresetFor,
  extractHongguoVid,
  readSavedNativeFgMult,
  readSavedNativeFgMode,
} from './preferredPlaybackLogic';
import { hasNativeAddress, planPlaybackRoute, type PlaybackSurface } from './playbackRouting';
import { releaseNativeAddress, resolveNativeAddress } from './nativeAddress';

export type { PreferredPlayer, InterpolationPreset, NativeFgMode } from './preferredPlaybackLogic';
export {
  normalizePreferredPlayer,
  extractHongguoVid,
  interpolationPresetFor,
  inferContentKind,
  normalizeNativeFgMult,
  normalizeNativeFgMode,
  readSavedNativeFgMult,
  readSavedNativeFgMode,
  saveNativeFgPreferences,
} from './preferredPlaybackLogic';

export interface NativeLaunchInput {
  preferredPlayer: unknown;
  seriesId: string;
  episodeId: string;
  title?: string;
  episodeNumber?: number;
  startSeconds?: number;
  /** 0–1 的音量；原生播放器要的是 0–100。 */
  volume?: number;
  type?: string | null;
  channel?: string | null;
  historyType?: HistoryType;
  /** 动漫等已经解析好的直链；有它就不必再走红果 vid 解析。 */
  url?: string;
  key?: string;
  /** 发起这次播放的界面形态；`feed` = 滑动信息流（按设计不弹独立窗口）。 */
  surface?: PlaybackSurface;
  detail?: SeriesDetail;
  autoNext?: boolean;
  setAutoNext?: (on: boolean) => void;
}

let launchGeneration = 0;
let pendingLaunch: AbortController | null = null;

/**
 * 若用户选择了 MPV，就弹出原生播放器窗口。
 *
 * 成功返回 `true`（调用方不要再进应用内播放页，否则会两路出声）。
 * 用户选的是网页播放器，或这一集无法交给原生播放器时返回 `false`，调用方走原链路。
 * 启动失败会抛错，调用方可以回退到网页播放并提示用户。
 */
export async function tryLaunchNativePlayer(input: NativeLaunchInput): Promise<boolean> {
  const generation = ++launchGeneration;
  pendingLaunch?.abort();
  /**
   * 落点判定只有一份：`planPlaybackRoute`。
   *
   * 偏好不是 mpv、总闸关闭、信息流形态、这一集没有地址 —— 四种都在那里决定，
   * 并且各自带一句可显示的文案。这里只把结果翻译成调用方要的布尔值，**不再**
   * 自己判一遍：旧实现把其中两种写成裸 `return false`，调用方拿到的信息量与
   * “用户选的是网页播放器”完全相同，于是静默落回内置播放器。
   */
  if (planPlaybackRoute({
    preferredPlayer: input.preferredPlayer,
    nativeAvailable: NATIVE_PLAYER_AVAILABLE,
    seriesId: input.seriesId,
    episodeId: input.episodeId,
    url: input.url,
    type: input.type,
    channel: input.channel,
    surface: input.surface,
  }).kernel !== 'native') return false;
  const launch = new AbortController();
  pendingLaunch = launch;
  let detail: SeriesDetail;
  try {
    detail = input.detail ?? await ipcService.series.getDetail(input.seriesId);
  } catch (error) {
    if (generation !== launchGeneration) return true;
    throw error;
  }
  if (generation !== launchGeneration) return true;
  const index = detail.episodes.findIndex(episode => episode.id === input.episodeId);
  if (detail.id !== input.seriesId || index < 0) throw new Error('当前剧集详情中找不到要播放的分集。');
  const kind = inferContentKind({ ...input, type: detail.type });
  const preset = interpolationPresetFor(kind);
  const vid = kind === 'anime' ? null : extractHongguoVid(input.episodeId, input.seriesId);
  const directUrl = (input.url ?? '').trim();

  /**
   * 详情返回的 `type` 可能推翻调用方给的猜测（历史记录没带 channel 的动漫、
   * 来源把漫剧标成真人剧都会这样），所以按**详情口径**再判一次地址。
   * 仍然只有这一个函数说了算 —— 否则“能不能交出去”会重新出现第二种说法。
   */
  if (!hasNativeAddress({ ...input, type: detail.type })) return false;

  const savedFgMult = readSavedNativeFgMult();
  const savedFgMode = readSavedNativeFgMode();

  /**
   * **先取地址，再启动**（见 `services/nativeAddress.ts`）。
   *
   * 原生窗口只在没有直链时才解析，而它只会解析红果（`vid` 必须是红果 vid）。
   * 动漫 / 黄果 / 云端这三类来源拿不到红果 vid，旧路径传 `vid: '0'` 必然失败 ——
   * 用户看到的就是"选了 MPV 依旧回到内置播放器"。
   *
   * 这里只在"地址可得、没有现成直链、且不是纯数字红果 id"时多一次往返；
   * 红果仍然交给 Rust 自己解析，一次都不多。
   */
  const address = await resolveNativeAddress({
    seriesId: input.seriesId,
    episodeId: input.episodeId,
    url: directUrl,
    type: detail.type,
    channel: input.channel,
  });
  // 解析是异步的：这期间用户可能已经点了别的集，或本视图已卸载。
  if (generation !== launchGeneration || launch.signal.aborted) {
    await releaseNativeAddress(address);
    return true;
  }

  let handedOff = false;
  try {
    await ipcService.playback.nativeOpen({
      vid: vid ?? '0',
      seriesId: input.seriesId,
      episodeId: input.episodeId,
      title: detail.title,
      seriesCover: detail.cover,
      channel: detail.type || input.channel || kind,
      historyType: input.historyType,
      episode: detail.episodes[index].episodeNumber,
      startSeconds: Math.max(0, input.startSeconds ?? 0),
      volume: Math.round(Math.max(0, Math.min(1, input.volume ?? 0.8)) * 100),
      fg: preset.fg,
      ...(savedFgMult !== undefined ? { fgMult: savedFgMult } : {}),
      ...(savedFgMode !== undefined ? { fgMode: savedFgMode } : {}),
      contentKind: kind,
      url: directUrl || address?.url || undefined,
      key: input.key,
      queue: buildInitialNativeQueue({
        seriesId: detail.id,
        seriesTitle: detail.title,
        seriesType: kind,
        episodes: detail.episodes,
        currentIndex: index,
        startSeconds: input.startSeconds ?? 0,
      }),
      index,
      totalEpisodes: detail.episodes.length,
      autoplay: input.autoNext ?? true,
      autoplayDelay: 0,
      // 启动那一刻就对齐应用主题：播放器是独立进程，读不到本页 localStorage，
      // 不带这个字段它只会去读注册表里的系统偏好（见 nativeThemeSync.ts 的注释）。
      theme: readTheme(),
    }, launch.signal);
    handedOff = true;
  } catch (error) {
    if (launch.signal.aborted) return true;
    throw error;
  } finally {
    /**
     * 只用来取地址的会话**必须**在这里收掉：成功、失败、被顶替都走这一段。
     * 留着它就是一路没人播放的空会话；`guo:` 的会话会让地址失效，所以那边
     * 由 `releaseNativeAddress` 记成"随窗口存活"，等窗口关闭再收。
     */
    // `handedOff` 决定 `guo:` 的地址会话要不要延寿：启动失败就没人会读它。
    await releaseNativeAddress(address, { handedOff });
  }
  if (generation !== launchGeneration) return true;
  registerStandaloneNativeNavigation({
    detail,
    index,
    volume: input.volume ?? 0.8,
    startSeconds: input.startSeconds ?? 0,
    setAutoNext: input.setAutoNext,
  });
  return true;
}

