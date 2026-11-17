import assert from 'node:assert/strict';
import fs from 'node:fs';
import { pathToFileURL } from 'node:url';

const { appendUniqueSeries } = await import(
  pathToFileURL(`${process.cwd()}/src/hooks/useInfiniteScroll.ts`).href
);

// 1. appendUniqueSeries 行为契约
{
  const a = { id: 'a', title: 'A' };
  const b = { id: 'b', title: 'B' };
  const c = { id: 'c', title: 'C' };
  const res1 = appendUniqueSeries([a, b], [b, c, { id: '', title: 'bad' }]);
  assert.equal(res1.addedCount, 1);
  assert.deepEqual(res1.items.map(x => x.id), ['a', 'b', 'c']);

  const res2 = appendUniqueSeries([a, b], [a, b]);
  assert.equal(res2.addedCount, 0);
  assert.deepEqual(res2.items.map(x => x.id), ['a', 'b']);
}

// 2. 六处无限滚动视图均接入共享 hook 与几何兜底
const viewsWithFullHook = [
  'src/components/views/ShelfMoreView.tsx',
  'src/components/views/HuangguoView.tsx',
  'src/components/views/AnimeView.tsx',
  'src/components/views/SpotlightView.tsx',
  'src/components/views/SearchView.tsx',
];
for (const file of viewsWithFullHook) {
  const src = fs.readFileSync(file, 'utf8');
  assert(src.includes('useInfiniteScroll('), `${file} 缺少 useInfiniteScroll`);
  assert(src.includes('usePrefetchBackstop('), `${file} 缺少 usePrefetchBackstop`);
}

const explore = fs.readFileSync('src/components/views/ExploreView.tsx', 'utf8');
assert(explore.includes('usePrefetchBackstop('), 'ExploreView 缺少 usePrefetchBackstop');
assert(explore.includes("rootMargin: '1800px 0px'"), 'ExploreView prefetch 提前量不足 1800px');

const catalogStore = fs.readFileSync('src/stores/useCatalogStore.tsx', 'utf8');
assert(
  catalogStore.includes('[hasMore, isLoading, isLoadingMore, pagesLoaded, prefetchMore]'),
  'useCatalogStore 空闲预取应监听 pagesLoaded 而非 items.length',
);

// 3. 短剧推荐无感预热与秒开契约
const feedStore = fs.readFileSync('src/stores/useFeedStore.ts', 'utf8');
assert(feedStore.includes('warmSeriesMedia(afterNextId,'), 'useFeedStore 应链式预热 index + 2');
assert(
  feedStore.includes('prewarmEpisodeRef.current?.(seriesId, firstEp.id, contentType, signal, true)'),
  'useFeedStore 应开启 bypassStreamingWindow',
);
assert(feedStore.includes('cancelNeighborWarm(item.id)'), '切剧不应误杀目标剧正在进行的预转');
assert(feedStore.includes('prefetchStream([epId], contentType)'), '详情预取应同时预签名首集');

const playbackStore = fs.readFileSync('src/stores/usePlaybackStore.tsx', 'utf8');
assert(playbackStore.includes('stopAttachedLoad(currentVideo)'), '切集应保留末帧而不是立即销毁 MSE 黑屏');

const shortDramaApp = fs.readFileSync('src-tauri/src/short_drama_app.rs', 'utf8');
assert(shortDramaApp.includes('open_completed_hls('), 'open_stream 应优先零签名命中本地已完成 HLS');
assert(shortDramaApp.includes('has_completed_hls('), 'prewarm_stream 应跳过已完成 HLS 的重复签名');
assert(shortDramaApp.includes('has_pending_foreground()'), 'prefetch_stream 不应被在途 ffmpeg 阻断');

const videoSurface = fs.readFileSync('src/components/player/VideoSurface.tsx', 'utf8');
assert(
  videoSurface.includes("currentView !== 'feed' &&"),
  'VideoSurface 不应向推荐流页面叠加缓冲蒙层',
);

// 4. 猜你喜欢动效关键帧绝不引入 opacity: 0
const css = fs.readFileSync('src/index.css', 'utf8');
for (const name of ['ttv-pick-hero-in', 'ttv-pick-deck-in', 'ttv-pick-pillar-breathe']) {
  const idx = css.indexOf(`@keyframes ${name}`);
  assert(idx >= 0, `src/index.css 缺少 @keyframes ${name}`);
  const block = css.slice(idx, css.indexOf('\n}\n', idx) + 3);
  assert(!/opacity\s*:/.test(block), `${name} 关键帧禁止操作 opacity`);
}

const randomPick = fs.readFileSync('src/components/common/RandomWatchSection.tsx', 'utf8');
assert(randomPick.includes('ttv-pick-hero-in'), 'RandomWatchSection 缺少 ttv-pick-hero-in');
assert(randomPick.includes('ttv-pick-deck-in'), 'RandomWatchSection 缺少 ttv-pick-deck-in');
assert(randomPick.includes('ttv-pick-sheen'), 'RandomWatchSection 缺少 ttv-pick-sheen');

console.log('verify:infinite-scroll & feed-seamless & random-pick-motion: PASS');
