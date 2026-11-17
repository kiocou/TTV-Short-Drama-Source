import type { HistoryClearScope, HistoryType, WatchHistoryItem } from '../types/history.ts';

export type HistoryModule = 'featured' | 'recommendation' | 'anime' | 'huangguo';
export type HistoryRegion = 'drama' | 'comic' | 'ai' | 'adult' | 'anime' | 'huangguo';

export function normalizeHistoryType(value: unknown): HistoryType {
  return value === 'recommendation' || value === 'featured' ? value : 'watch';
}

export function historyKey(item: Pick<WatchHistoryItem, 'seriesId' | 'historyType'>): string {
  return `${normalizeHistoryType(item.historyType)}:${item.seriesId}`;
}

export function distinctHistoryByKey<T extends Pick<WatchHistoryItem, 'seriesId' | 'historyType'>>(
  items: readonly T[],
): string[] {
  return [...new Set(items.map(historyKey))];
}

export function isHuangguoHistory(item: Pick<WatchHistoryItem, 'seriesId' | 'channel'>): boolean {
  return item.channel === 'huangguo' || item.seriesId.startsWith('hg:');
}

export function isAnimeHistory(item: Pick<WatchHistoryItem, 'seriesId' | 'channel'>): boolean {
  if (isHuangguoHistory(item)) return false;
  return (
    item.channel === 'anime'
    || item.seriesId.startsWith('dmghg:')
    || item.seriesId.startsWith('bfzy:')
    || item.seriesId.startsWith('anime_')
  );
}

export function classifyHistoryModule(
  item: Pick<WatchHistoryItem, 'seriesId' | 'channel' | 'historyType'>,
): HistoryModule {
  if (isHuangguoHistory(item)) return 'huangguo';
  if (isAnimeHistory(item)) return 'anime';
  return normalizeHistoryType(item.historyType) === 'recommendation' ? 'recommendation' : 'featured';
}

export function historyRegionKey(
  item: Pick<WatchHistoryItem, 'seriesId' | 'channel'>,
): HistoryRegion {
  if (isHuangguoHistory(item)) return 'huangguo';
  if (isAnimeHistory(item)) return 'anime';
  if (item.channel === 'comic' || item.channel === 'ai' || item.channel === 'adult') {
    return item.channel;
  }
  return 'drama';
}

const FEATURED_REGION_LABELS: Record<'drama' | 'comic' | 'ai' | 'adult', string> = {
  drama: '短剧专区',
  comic: '漫剧次元',
  ai: 'AI 剧',
  adult: '神秘小窝',
};

const RECOMMENDATION_REGION_LABELS: Record<'drama' | 'comic' | 'ai' | 'adult', string> = {
  drama: '短剧',
  comic: '漫剧',
  ai: 'AI 剧',
  adult: '神秘小窝',
};

export function historyRegionLabel(
  item: Pick<WatchHistoryItem, 'seriesId' | 'channel' | 'historyType'>,
): string {
  const mod = classifyHistoryModule(item);
  if (mod === 'huangguo') return '黄果专区';
  if (mod === 'anime') return '动漫专区';
  const region = historyRegionKey(item) as 'drama' | 'comic' | 'ai' | 'adult';
  if (mod === 'recommendation') {
    return `短剧推荐 · ${RECOMMENDATION_REGION_LABELS[region]}`;
  }
  return `短剧精选 · ${FEATURED_REGION_LABELS[region]}`;
}

export function matchesHistoryClearScope(
  item: Pick<WatchHistoryItem, 'seriesId' | 'channel' | 'historyType'>,
  scope?: HistoryClearScope,
): boolean {
  if (!scope) return true;
  const mod = classifyHistoryModule(item);
  if (scope === 'non_huangguo') return mod !== 'huangguo';
  return mod === scope;
}
