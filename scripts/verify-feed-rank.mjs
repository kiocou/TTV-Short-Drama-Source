import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import {
  BASE_ITEM_WEIGHT,
  FAVORITE_TAG_BONUS,
  HISTORY_EFFECTIVE_SECONDS,
  buildInterest,
  historyCompletion,
  itemWeight,
  orderFeedPage,
  weightedShuffle,
} from '../src/services/feedRank.ts';
import { dedupeFeedItems } from '../src/services/feedSource.ts';

/**
 * 本地兴趣排序的验收。
 *
 * 只断言**我们自己定义的**系数与行为：原记录里那套权重公式没有拿到，所以这里
 * 检查的是"门槛/累加/无画像保持源顺序/可复现"这些能被解释的性质，不是复刻精度。
 */

const here = dirname(fileURLToPath(import.meta.url));
const storeSource = readFileSync(resolve(here, '../src/stores/useFeedStore.ts'), 'utf8');

const item = (id, tags = [], type = 'drama') => ({
  id, title: id, cover: '', type, episodesCount: 2, tags, origin: '',
});
const history = (seriesId, patch = {}) => ({
  seriesId,
  episodeId: `${seriesId}-1`,
  title: seriesId,
  seriesCover: '',
  episodeNumber: 1,
  totalEpisodes: 10,
  positionSeconds: 30,
  durationSeconds: 60,
  progressPercent: 50,
  updatedAt: 0,
  isFinished: false,
  ...patch,
});
const favorite = (seriesId, patch = {}) => ({
  seriesId, title: seriesId, cover: '', mark: 'want', channel: 'drama', updatedAt: 0, ...patch,
});

const checks = [];
const check = (name, passed) => checks.push([name, passed]);

// ── 有效观看门槛 ────────────────────────────────────────────────────────
check('below the watch threshold does not count', historyCompletion(history('a', { positionSeconds: 14 })) === 0);
check('at the threshold counts by completion', Math.abs(historyCompletion(history('a', { positionSeconds: 30 })) - 0.5) < 1e-9);
check('finished counts as full', historyCompletion(history('a', { isFinished: true, positionSeconds: 1 })) === 1);
check('unknown duration still counts when watch time passes', historyCompletion(history('a', { durationSeconds: 0 })) === 0.5);

// ── 兴趣表 ──────────────────────────────────────────────────────────────
{
  const tagsById = new Map([
    ['a', ['战神', '都市']],
    ['b', ['战神']],
    ['c', ['玄幻']],
  ]);
  const interest = buildInterest({
    history: [
      history('a', { positionSeconds: 60, durationSeconds: 60 }), // 1.0
      history('b', { positionSeconds: 15, durationSeconds: 60 }), // 0.25
      history('c', { positionSeconds: 15, durationSeconds: 60 }), // 0.25 → 玄幻
    ],
    favorites: [],
    tagsById,
  });
  check('history accumulates tag weight by completion',
    Math.abs((interest.get('都市') ?? 0) - 1) < 1e-9
    && Math.abs((interest.get('战神') ?? 0) - (1 + 0.25)) < 1e-9
    && Math.abs((interest.get('玄幻') ?? 0) - 0.25) < 1e-9);
  const withFavorite = buildInterest({
    history: [history('a', { positionSeconds: 60, durationSeconds: 60 })],
    favorites: [favorite('a')],
    tagsById,
  });
  check('favorites add their own bonus',
    Math.abs((withFavorite.get('都市') ?? 0) - (1 + FAVORITE_TAG_BONUS)) < 1e-9);
  check('series without tags contribute nothing', (interest.get('不存在') ?? 0) === 0);
  check('interest only carries tags that were resolved', interest.has('玄幻'));
}
check('empty history and favorites yield an empty profile', buildInterest({
  history: [], favorites: [], tagsById: new Map(),
}).size === 0);

// ── 候选权重 ────────────────────────────────────────────────────────────
{
  const interest = new Map([['战神', 2]]);
  check('candidate without matching tag keeps base weight', itemWeight(item('x', ['都市']), interest) === BASE_ITEM_WEIGHT);
  check('candidate with matching tag is heavier', itemWeight(item('y', ['战神', '都市']), interest) === 3);
  check('missing tags array still yields base weight', itemWeight({ tags: [] }, interest) === BASE_ITEM_WEIGHT);
}

// ── 无画像保持源顺序；有画像可复现 ──────────────────────────────────────
{
  const items = [item('a', ['都市']), item('b', ['战神']), item('c', [])];
  check('no profile preserves source order',
    orderFeedPage(items, { interest: new Map() }).map(entry => entry.id).join() === 'a,b,c');
  check('single item is untouched', orderFeedPage([items[0]], { interest: new Map([['都市', 5]]) }).length === 1);

  const interest = new Map([['战神', 4], ['都市', 1]]);
  const rng = (() => {
    let state = 7;
    return () => {
      state = (state * 16807) % 2147483647;
      return state / 2147483647;
    };
  })();
  const first = weightedShuffle(items, rng, candidate => itemWeight(candidate, interest)).map(entry => entry.id);
  const second = weightedShuffle(items, (() => {
    let state = 7;
    return () => {
      state = (state * 16807) % 2147483647;
      return state / 2147483647;
    };
  })(), candidate => itemWeight(candidate, interest)).map(entry => entry.id);
  check('same rng sequence gives the same order', first.join() === second.join());
  check('shuffle keeps every candidate exactly once', first.slice().sort().join() === 'a,b,c');
  check('zero replacement check for duplicates', weightedShuffle([item('a'), item('a')], () => 0.5, () => 1).length === 2);
}

{
  // 固定 rng：权重大的必须排在前面（Efraimidis–Spirakis 的性质）。
  const heavy = item('heavy', ['战神']);
  const light = item('light', ['都市']);
  const interest = new Map([['战神', 9], ['都市', 0.1]]);
  const order = weightedShuffle([light, heavy], () => 0.5, candidate => itemWeight(candidate, interest));
  check('heavier weight sorts earlier under a fixed rng', order[0].id === 'heavy');
}

// ── 去重 / 频道隔离依然成立（排序不得破坏这两条） ──────────────────────
{
  const duplicate = item('a', ['战神']);
  const comic = item('b', [], 'comic');
  const filtered = dedupeFeedItems([item('a'), duplicate, comic], 'drama');
  check('dedupe keeps one entry per id', filtered.length === 1 && filtered[0].id === 'a');
  check('channel filter drops other channels', filtered.every(entry => entry.type === 'drama'));
}

// ── 结构性保证：历史只影响排序，不影响起播集号 ──────────────────────────
{
  const start = storeSource.indexOf('const openSeriesAt');
  const end = storeSource.indexOf('const fetchPage');
  const body = start >= 0 && end > start ? storeSource.slice(start, end) : '';
  check('openSeriesAt always starts at episode 0 position 0', body.includes('startEpisode(item, nextDetail, 0, 0)'));
  check('openSeriesAt never consults watch history for the start point',
    !body.includes('pickResumeEpisode') && !body.includes('history.list'));
  check('interest refresh is kicked off after start, not before',
    body.indexOf('startEpisode(item, nextDetail, 0, 0)') < body.indexOf('refreshInterest(requestId)'));
}

{
  const start = storeSource.indexOf('const startPageJob');
  const end = storeSource.indexOf('const loadInitial');
  const body = start >= 0 && end > start ? storeSource.slice(start, end) : '';
  const mergeAt = body.indexOf('mergeFeedPage(itemsRef.current');
  check('incoming page merges into the queue before any tag work (no await in front of the merge)',
    mergeAt >= 0 && mergeAt < body.indexOf('orderTailLater('));
  check('the already shown queue is never reordered in the store',
    !body.includes('orderFeedPage(') && !body.includes('orderFeedTail(itemsRef.current'));
  check('queue merge still dedupes through dedupeFeedItems', body.includes('dedupeFeedItems(merged.items'));
}

{
  const queueSource = readFileSync(resolve(here, '../src/services/feedQueue.ts'), 'utf8');
  const start = storeSource.indexOf('const orderTailLater');
  const end = storeSource.indexOf('const resetFeedProfile');
  const body = start >= 0 && end > start ? storeSource.slice(start, end) : '';
  check('ordering only runs when a profile exists', body.includes('interest.size === 0'));
  check('candidate tags are capped per page', body.includes('slice(0, TAG_FETCH_LIMIT)'));
  check('ordering merges fetched tags instead of guessing', queueSource.includes('enrichTags(tail, tagsById)'));
  check('ordering is bounded to the appended slice (tailStart..tailEnd)',
    body.includes('tailEnd') && queueSource.includes('tailStart, tailEnd'));
}

{
  const rankSource = readFileSync(resolve(here, '../src/services/feedRank.ts'), 'utf8');
  check('tag cache has a hard bound', rankSource.includes('TAG_CACHE_MAX'));
  check('tag fetch limit is a single batch constant', rankSource.includes('TAG_FETCH_LIMIT'));
  check('resolver guards stale generations before writing', rankSource.includes('generation !== state'));
}
{
  const start = storeSource.indexOf('const loadDetailFor');
  const end = storeSource.indexOf('loadDetailRef.current = loadDetailFor');
  const body = start >= 0 && end > start ? storeSource.slice(start, end) : '';
  check('detail load captures the generation before await', body.includes('const requestId = generationRef.current'));
  check('stale detail results cannot write into the new tag cache',
    body.includes('requestId === generationRef.current && result.tags'));
  check('generation guard is evaluated before remember',
    body.indexOf('requestId === generationRef.current') < body.indexOf('remember(seriesId, result.tags)'));
}

{
  const rankSource = readFileSync(resolve(here, '../src/services/feedRank.ts'), 'utf8');
  check('resolver skips ids that are already in flight', rankSource.includes('state.inflight.has(id)'));
  check('resolver registers in-flight around the load',
    rankSource.includes('state.inflight.add(id)') && rankSource.includes('state.inflight.delete(id)'));
  check('running cleanup compares promise identity', rankSource.includes('state.running === wrapped'));
}

{
  const start = storeSource.indexOf('const refreshInterest');
  const end = storeSource.indexOf('const prefetchNeighbors');
  const body = start >= 0 && end > start ? storeSource.slice(start, end) : '';
  check('interest seeds skip ineffective history', body.includes('historyCompletion(record) > 0'));
  check('favorites are seeded before history (so they are not crowded out)',
    body.indexOf('scopedFavorites.slice(0, TAG_FETCH_LIMIT)') < body.indexOf('effectiveHistory.slice(0, TAG_FETCH_LIMIT)'));
  check('interest works without the Tauri-only guard (mock path runs the same code)',
    !body.includes('isTauriEnvironment()'));
  check('known card tags are remembered before resolving', body.includes('tagResolverRef.current.remember(item.id, item.tags)'));
}
check('effective watch threshold is the documented 15s', HISTORY_EFFECTIVE_SECONDS === 15);

const failed = checks.filter(([, passed]) => !passed);
for (const [name, passed] of checks) console.log(`${passed ? 'PASS' : 'FAIL'} ${name}`);
if (failed.length) process.exit(1);
console.log(`${checks.length} passed, 0 failed`);