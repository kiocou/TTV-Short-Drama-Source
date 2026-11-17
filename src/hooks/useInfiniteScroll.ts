import { useEffect, useRef, type RefObject } from 'react';
import type { SeriesItem } from '../types/catalog';

interface InfiniteScrollOptions {
  scrollRef: RefObject<HTMLElement | null>;
  sentinelRef: RefObject<HTMLElement | null>;
  enabled: boolean;
  rootMargin?: string;
  onLoadMore: () => void;
}

interface PrefetchBackstopOptions {
  scrollRef: RefObject<HTMLElement | null>;
  sentinelRef: RefObject<HTMLElement | null>;
  enabled: boolean;
  isLoadingMore: boolean;
  /** 每次落地一页就递增的进度信号（即使去重后无新卡也会递增），防止观察器在原地停摆。 */
  progress: number;
  bufferPx?: number;
  onLoadMore: () => void;
}

/**
 * 共享无限滚动观察器：在用户滚到哨兵前 `rootMargin`（默认 1800px，约两屏）提前拉取下一页，
 * 让列表在拉到底之前始终保有缓冲内容，不卡顿等待。
 */
export function useInfiniteScroll({
  scrollRef,
  sentinelRef,
  enabled,
  rootMargin = '1800px 0px',
  onLoadMore,
}: InfiniteScrollOptions): void {
  const onLoadMoreRef = useRef(onLoadMore);
  onLoadMoreRef.current = onLoadMore;

  useEffect(() => {
    const sentinel = sentinelRef.current;
    const root = scrollRef.current;
    if (!sentinel || !root || !enabled) return;
    const observer = new IntersectionObserver(
      entries => {
        if (entries.some(entry => entry.isIntersecting)) onLoadMoreRef.current();
      },
      { root, rootMargin, threshold: 0 },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [enabled, rootMargin, scrollRef, sentinelRef]);
}

/**
 * 几何兜底：每落地一页实测哨兵距视口底部的距离；若单页条数较少或去重后未能将哨兵推出
 * 缓冲线，则继续自动续页，避免“滚到底卡住不动”。
 */
export function usePrefetchBackstop({
  scrollRef,
  sentinelRef,
  enabled,
  isLoadingMore,
  progress,
  bufferPx = 1800,
  onLoadMore,
}: PrefetchBackstopOptions): void {
  const onLoadMoreRef = useRef(onLoadMore);
  onLoadMoreRef.current = onLoadMore;

  useEffect(() => {
    if (!enabled || isLoadingMore || progress === 0) return;
    const sentinel = sentinelRef.current;
    const root = scrollRef.current;
    if (!sentinel || !root) return;
    const rootRect = root.getBoundingClientRect();
    // 隐藏页（display: none）宽高为 0，不能误判成“已经触底”。
    if (rootRect.height <= 0) return;
    const distance = sentinel.getBoundingClientRect().top - rootRect.bottom;
    if (distance <= bufferPx) onLoadMoreRef.current();
  }, [bufferPx, enabled, isLoadingMore, progress, scrollRef, sentinelRef]);
}

/** 追加一页条目并按 id 保序去重，返回合并后列表及本次净增数量。 */
export function appendUniqueSeries(
  previous: readonly SeriesItem[],
  incoming: readonly SeriesItem[],
): { items: SeriesItem[]; addedCount: number } {
  const seen = new Set(previous.map(item => item.id));
  const fresh: SeriesItem[] = [];
  for (const item of incoming) {
    if (!item?.id || seen.has(item.id)) continue;
    seen.add(item.id);
    fresh.push(item);
  }
  return {
    items: fresh.length > 0 ? [...previous, ...fresh] : previous.slice(),
    addedCount: fresh.length,
  };
}
