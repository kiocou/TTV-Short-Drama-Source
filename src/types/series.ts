import { ChannelType } from './catalog';

export interface EpisodeItem {
  id: string;
  seriesId: string;
  episodeNumber: number;
  title: string;
  durationSeconds: number;
  previewUrl?: string;
  // Real playback URLs are short-lived and are resolved only when an episode opens.
  videoUrl?: string;
  watchedSeconds?: number;
  isFinished?: boolean;
}

export interface VideoQualityOption {
  label: string; // '4K 杜比视界' | '1080P 超清' | '720P 高清' | '自动'
  value: '4k' | '1080p' | '720p' | 'auto';
  resolution: string; // '3840x2160' | '1920x1080' | '1280x720'
  bitrate?: string;
}

export interface PlaybackSource {
  id: string;
  name: string;
  isPrimary: boolean;
  health: 'healthy' | 'degraded' | 'unavailable';
  pingMs: number;
}

export interface SeriesDetail {
  id: string;
  title: string;
  cover: string;
  backdrop?: string;
  type: ChannelType;
  tags: string[];
  origin: string;
  episodesCount: number;
  updateTime?: string;
  description: string;
  actors?: string[];
  director?: string;
  episodes: EpisodeItem[];
  /**
   * 真正能播的集数上限：`episodes[0..playableEpisodes]` 之外的集播不出来。
   *
   * 与 `episodesCount` 是**两个不同的事实**，不要互相顶替：
   * - `episodesCount` = 这部剧有多少集（站方文案「共 102 集全」）；
   * - `playableEpisodes` = 现在开放到第几集（实测同一部剧只有 5）。
   *
   * 缺失（`undefined`）表示这一路源没有给出这个信号，此时**不要**当成"全部可播"，
   * 而是照旧渲染、不置灰——把未知当成已开放，用户点到的失败会静默发生
   * （红果播放页对未开放集同样返回 200 的完整 HTML）。
   */
  playableEpisodes?: number;
  availableQualities: VideoQualityOption[];
  sources: PlaybackSource[];
}
