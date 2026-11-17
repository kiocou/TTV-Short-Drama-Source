import {
  encodeFeedCursor,
  feedSelectionsFor,
  fetchHongguoFeedPage,
  parseFeedCursor,
} from '../src/services/feedSource.ts';

/**
 * 游标来源与首屏退路的验收。
 *
 * 这里必须真跑异步函数（假 loader），因为最容易错的地方就是「退路之后游标是谁的」：
 * 拿短剧筛选的游标去喂发现流，或者一次失败就退回第一页，都是用户能立刻感觉到的
 * 退化（"往下滑又从头开始"）。
 */

const item = (id, type = 'drama') => ({
  id, title: id, cover: '', type, episodesCount: 2, tags: [], origin: '',
});

const checks = [];
const check = (name, passed) => checks.push([name, passed]);

// ── 纯函数：编码 / 解析 ──────────────────────────────────────────────────
check('encode tags short-filter cursor', encodeFeedCursor('short-filter', 's1.abc') === 'sf:s1.abc');
check('encode tags discover cursor', encodeFeedCursor('discover', 'd1.abc') === 'dv:d1.abc');
check('empty token has no cursor', encodeFeedCursor('discover', '') === null
  && encodeFeedCursor('short-filter', null) === null);
check('parse short-filter tagged cursor', (() => {
  const parsed = parseFeedCursor('sf:s1.abc');
  return parsed?.source === 'short-filter' && parsed.token === 's1.abc';
})());
check('parse discover tagged cursor', (() => {
  const parsed = parseFeedCursor('dv:d1.abc');
  return parsed?.source === 'discover' && parsed.token === 'd1.abc';
})());
check('legacy s1 token is inferred as short-filter', parseFeedCursor('s1.legacy')?.source === 'short-filter');
check('legacy d1 token is inferred as discover', parseFeedCursor('d1.legacy')?.source === 'discover');
check('unknown token stays on discover rather than resetting', (() => {
  const parsed = parseFeedCursor('opaque-token');
  return parsed?.source === 'discover' && parsed.token === 'opaque-token';
})());
check('blank cursor means first page', parseFeedCursor('') === null && parseFeedCursor(null) === null);

// ── selections：只钉 genre + sort ───────────────────────────────────────
check('drama selections pin short_play + online_time', (() => {
  const selections = feedSelectionsFor('drama');
  return selections?.genre?.join() === 'short_play' && selections?.sort?.join() === 'online_time';
})());
check('comic selections pin comic_series + online_time', (() => {
  const selections = feedSelectionsFor('comic');
  return selections?.genre?.join() === 'comic_series' && selections?.sort?.join() === 'online_time';
})());
check('ai has no app selections', feedSelectionsFor('ai') === null);

// ── 异步：首屏优先短剧筛选 ──────────────────────────────────────────────
const makeLoaders = (behaviour) => {
  const calls = { shortFilter: [], discover: [] };
  const loaders = {
    async shortFilter(input) {
      calls.shortFilter.push(input);
      return behaviour.shortFilter(input);
    },
    async discover(channel, cursor) {
      calls.discover.push({ channel, cursor });
      return behaviour.discover(channel, cursor);
    },
  };
  return { loaders, calls };
};

{
  const { loaders, calls } = makeLoaders({
    shortFilter: () => ({ items: [item('a')], hasMore: true, cursor: 's1.next' }),
    discover: () => ({ items: [item('fallback')], hasMore: false, cursor: null }),
  });
  const page = await fetchHongguoFeedPage({ channel: 'drama', cursor: null, loaders });
  check('first page uses short filter', page.source === 'short-filter'
    && page.items[0].id === 'a' && calls.discover.length === 0);
  check('first page passes selections and no cursor', calls.shortFilter[0]?.cursor === null
    && calls.shortFilter[0]?.selections?.genre?.join() === 'short_play'
    && calls.shortFilter[0]?.channel === 'drama');
  check('short filter cursor keeps its own source', page.cursor === 'sf:s1.next');
}

{
  const { loaders, calls } = makeLoaders({
    shortFilter: () => { throw new Error('HTTP 503'); },
    discover: () => ({ items: [item('fallback')], hasMore: true, cursor: 'd1.next' }),
  });
  const page = await fetchHongguoFeedPage({ channel: 'drama', cursor: null, loaders });
  check('first page falls back to discover when short filter fails', page.source === 'discover'
    && page.items[0].id === 'fallback' && calls.discover.length === 1);
  check('fallback cursor is tagged discover (never reused as filter cursor)', page.cursor === 'dv:d1.next');
  check('fallback discover starts at first page', calls.discover[0]?.cursor === null);
}

{
  const { loaders, calls } = makeLoaders({
    shortFilter: () => ({ items: [], hasMore: false, cursor: null }),
    discover: () => ({ items: [item('fallback')], hasMore: false, cursor: null }),
  });
  const page = await fetchHongguoFeedPage({ channel: 'comic', cursor: null, loaders });
  check('empty short filter result also falls back', page.source === 'discover' && calls.discover.length === 1);
  check('comic fallback uses the comic discover channel', calls.discover[0]?.channel === 'comic');
}

// ── 异步：第 2 页只续它自己的来源，失败不退回首屏 ────────────────────────
{
  const { loaders, calls } = makeLoaders({
    shortFilter: () => ({ items: [item('a2')], hasMore: false, cursor: null }),
    discover: () => { throw new Error('should not be called'); },
  });
  const page = await fetchHongguoFeedPage({ channel: 'drama', cursor: 'sf:s1.next', loaders });
  check('tagged filter cursor continues on short filter', page.source === 'short-filter'
    && page.items[0].id === 'a2' && calls.discover.length === 0);
  check('filter continuation replays the same selections', calls.shortFilter[0]?.selections?.genre?.join() === 'short_play');
}

{
  const { loaders, calls } = makeLoaders({
    shortFilter: () => { throw new Error('should not be called'); },
    discover: () => { throw new Error('HTTP 500'); },
  });
  let rejected = false;
  try {
    await fetchHongguoFeedPage({ channel: 'drama', cursor: 'dv:d1.next', loaders });
  } catch {
    rejected = true;
  }
  check('later-page failure rejects instead of restarting from page 1', rejected
    && calls.shortFilter.length === 0 && calls.discover.length === 1);
  check('later-page discover keeps its own cursor', calls.discover[0]?.cursor === 'd1.next');
}

{
  let threw = false;
  try {
    await fetchHongguoFeedPage({ channel: 'ai', cursor: null, loaders: makeLoaders({}).loaders });
  } catch {
    threw = true;
  }
  check('ai channel is rejected from the app candidate path', threw);
}

const failed = checks.filter(([, passed]) => !passed);
for (const [name, passed] of checks) console.log(`${passed ? 'PASS' : 'FAIL'} ${name}`);
if (failed.length) process.exit(1);
console.log(`${checks.length} passed, 0 failed`);