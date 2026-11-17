/**
 * playbackKernel.ts —— 播放内核统一契约（docs/C-播放内核适配面盘点.md §3 的落地第一步）。
 *
 * ## 为什么要有这个文件
 *
 * 现在播放链路直接依赖 `HTMLVideoElement`：`usePlaybackStore.tsx` 把
 * `currentTime` / `buffered` / `readyState` 当成**状态机的输入**（不只是展示值），
 * 全仓库 51 处 `HTMLVideoElement` 引用、跨 10 个文件。想换成本地 LibMPV 内核时，
 * 这些判定无法等价翻译：
 *
 * - `buffered` 没有"分段区间"语义，LibMPV 只给 `demuxer-cache-time`（已缓存到第几秒）
 * - `readyState` 没有 `HAVE_FUTURE_DATA` 这类就绪等级，只能从属性推导阶段
 *
 * 所以先把语义抽在这里：**换内核时改实现，不改业务逻辑**。
 *
 * ## 当前状态（2026-11-09）
 *
 * - 契约：已定义（本文件）
 * - HTML 侧：`snapshotFromVideo()` 已实现，可直接把 `<video>` 映射成 `KernelSnapshot`，
 *   是阶段 1 里 `HtmlKernel` 的内核
 * - LibMPV 侧：`MpvKernel` 未实现；所需字段已由原生播放器经 `progress` 心跳回传
 *   （见 `src-tauri/src/native_player.rs` 的 `PlayerState`）
 * - 接线：`usePlaybackStore` **尚未**改为依赖本契约。阶段 2 才动，且必须用运行时开关
 *   切换、默认仍走 HTML，不能一次性删 hls.js（见 C 报告 §4）
 *
 * 一句话：这个文件是**契约**，不是已经换好的内核。别把它当成"迁移已完成"。
 */

/** 内核阶段。取代 `readyState` 的等级判定。 */
export type KernelPhase = 'idle' | 'loading' | 'ready' | 'buffering' | 'failed';

/**
 * 失败分类。取代挂在元素上的 `__ttvHlsFatal`。
 *
 * 三类必须分开：现有代码强调"自动播放被拦要如实报，不能混进播放源连接受阻"。
 */
export type KernelFailure = 'none' | 'network' | 'decode' | 'source' | 'autoplay';

/** `play()` 的结果。取代捕获 `NotAllowedError`。 */
export type PlayResult = 'ok' | 'autoplay-blocked' | 'error';

/** 画质档位。源侧只有单一路径，实际恒为 'auto'（见 README「已知边界」）。 */
export type QualityLevel = 'auto';

/** 一次装载请求。 */
export interface LoadRequest {
  url: string;
  headers?: Record<string, string>;
  /** 起播位置（秒）。 */
  startSeconds?: number;
  episodeId?: string;
}

/** 状态快照。纯读、无副作用、可高频调用。 */
export interface KernelSnapshot {
  position: number;
  duration: number;
  paused: boolean;
  rate: number;
  /** 0-1 归一。 */
  volume: number;
  muted: boolean;
  /** 已缓存到的秒数（HTML 取 buffered.end(last)，LibMPV 取 demuxer-cache-time）。 */
  cachedUntil: number;
  /** 可跳转上界（HTML 取 seekable.end(last)）。 */
  seekableUntil: number;
  phase: KernelPhase;
  failure: KernelFailure;
  video: { width: number; height: number } | null;
}

/** 播放内核统一接口。HTML 与 LibMPV 各实现一份。 */
export interface PlaybackKernel {
  /** 绑定渲染面。HTML 传 `<video>`；LibMPV 与原生窗口协调。 */
  attach(surface: unknown): void;
  detach(): void;

  load(req: LoadRequest): Promise<void>;
  play(): Promise<PlayResult>;
  pause(): void;
  seek(seconds: number): void;
  setRate(rate: number): void;
  /** 0-1 归一，内核内部换算成 0-100。 */
  setVolume(volume: number): void;
  setMuted(muted: boolean): void;
  setQuality(quality: QualityLevel): void;

  snapshot(): KernelSnapshot;
  subscribe(listener: (snapshot: KernelSnapshot) => void): () => void;
}

/**
 * `<video>` 的最小结构面。
 *
 * 声明成结构类型而不是直接用 `HTMLVideoElement`，是为了让这个函数能在
 * Node 里被验收脚本喂伪对象——否则每次验证都得开一个真浏览器。
 */
export interface VideoLike {
  currentTime: number;
  duration: number;
  paused: boolean;
  playbackRate: number;
  volume: number;
  muted: boolean;
  readyState: number;
  videoWidth: number;
  videoHeight: number;
  buffered: { length: number; end(index: number): number };
  seekable: { length: number; end(index: number): number };
  error: { code?: number } | null;
}

/** 取最后一段缓冲的末端秒数；没有缓冲或有空洞时返回 -1。 */
export function bufferedEndSeconds(video: VideoLike | null | undefined): number {
  if (!video || !video.buffered || video.buffered.length <= 0) return -1;
  try {
    const value = video.buffered.end(video.buffered.length - 1);
    return Number.isFinite(value) && value >= 0 ? value : -1;
  } catch {
    // 尚未就绪的媒体元素访问 buffered 可能抛错（现有 store 里也有同样的兜底）。
    return -1;
  }
}

/** 取可跳转上界的秒数；拿不到时返回 -1。 */
export function seekableEndSeconds(video: VideoLike | null | undefined): number {
  if (!video || !video.seekable || video.seekable.length <= 0) return -1;
  try {
    const value = video.seekable.end(video.seekable.length - 1);
    return Number.isFinite(value) && value >= 0 ? value : -1;
  } catch {
    return -1;
  }
}

/** 失败分类：把 `MediaError.code` 映射成内核语义。 */
export function failureFromMediaError(
  error: { code?: number } | null | undefined,
): KernelFailure {
  if (!error) return 'none';
  switch (error.code) {
    case 2: // MEDIA_ERR_NETWORK
      return 'network';
    case 3: // MEDIA_ERR_DECODE
      return 'decode';
    case 4: // MEDIA_ERR_SRC_NOT_SUPPORTED
      return 'source';
    default:
      return 'decode';
  }
}

/**
 * 从 `<video>` 当前状态推导内核阶段。
 *
 * 映射规则（对应现有 store 的三处判定）：
 * - 有 `error` → failed
 * - `readyState < 2`（HAVE_CURRENT_DATA 都没到）→ loading
 * - `readyState < 3` 或缓冲末端没越过播放位置 → buffering
 * - 其余 → ready
 *
 * 注意：`readyState >= 3` 原本就是"起播成功"的判据（`usePlaybackStore.tsx:626`），
 * 这里保持同一根阈值，避免迁移后两条判定打架。
 */
export function phaseFromVideo(video: VideoLike | null | undefined): KernelPhase {
  if (!video) return 'idle';
  if (video.error) return 'failed';
  if (video.readyState < 2) return 'loading';
  if (video.readyState < 3) return 'buffering';
  const cached = bufferedEndSeconds(video);
  // 缓冲末端追不上播放位置时，画面上其实是在等数据。
  if (cached >= 0 && cached + 0.25 < video.currentTime) return 'buffering';
  return 'ready';
}

/** 把 `<video>` 映射成内核快照。HTML 侧实现的内核就是这个函数。 */
export function snapshotFromVideo(video: VideoLike | null | undefined): KernelSnapshot {
  if (!video) {
    return {
      position: 0,
      duration: 0,
      paused: true,
      rate: 1,
      volume: 1,
      muted: false,
      cachedUntil: -1,
      seekableUntil: -1,
      phase: 'idle',
      failure: 'none',
      video: null,
    };
  }
  const width = Number.isFinite(video.videoWidth) ? video.videoWidth : 0;
  const height = Number.isFinite(video.videoHeight) ? video.videoHeight : 0;
  return {
    position: Number.isFinite(video.currentTime) ? video.currentTime : 0,
    duration: Number.isFinite(video.duration) && video.duration > 0 ? video.duration : 0,
    paused: video.paused,
    rate: Number.isFinite(video.playbackRate) && video.playbackRate > 0 ? video.playbackRate : 1,
    volume: Number.isFinite(video.volume) ? video.volume : 1,
    muted: video.muted,
    cachedUntil: bufferedEndSeconds(video),
    seekableUntil: seekableEndSeconds(video),
    phase: phaseFromVideo(video),
    failure: failureFromMediaError(video.error),
    video: width > 0 && height > 0 ? { width, height } : null,
  };
}
