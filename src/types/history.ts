import { ChannelType } from './catalog';

export type HistoryType = 'watch' | 'recommendation' | 'featured';

export type HistoryClearScope = 'non_huangguo' | 'featured' | 'recommendation' | 'anime' | 'huangguo';

export interface WatchHistoryItem {
  seriesId: string;
  episodeId: string;
  title: string;
  seriesCover: string;
  episodeNumber: number;
  totalEpisodes: number;
  positionSeconds: number;
  durationSeconds: number;
  progressPercent: number;
  updatedAt: number; // timestamp
  isFinished: boolean;
  channel?: ChannelType;
  /** 未提供时按旧版普通观看记录处理。 */
  historyType?: HistoryType;
}
