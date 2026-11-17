/**
 * AI 剧推荐流的验收。
 *
 * 为什么单独一个脚本：`verify-feed-source.mjs` 锁的是既有 20 条契约（含 4 条
 * 可播边界 clamped），改动它容易让人以为"旧行为变了"；AI 剧这条是新链路，
 * 单独一份才能一眼看出"进流"到底验了什么。
 *
 * 这条链路没有 App 通道，只能靠公开网页目录拼（`/rank/hot-ai-drama` 5 页 ×
 * 20 条 + `/category/ai-drama` 34 页 × 24 条）。所以验收重点是两件事：
 *
 * 1. `normalizeFeedChannel('ai')` 不再落进 `drama`（旧实现的三元 else 会吃掉它）。
 * 2. 分页游标能从热榜第 1 页一路推到底，**尤其是热榜第 5 页之后必须切到目录段**
 *    —— 那里后端的 `has_more` 是 false，按它判"到底了"会丢掉后面 34 页。
 */
import {
  advanceAiFeedCursor,
  AI_CATALOG_LAST_PAGE,
  AI_HEAT_LAST_PAGE,
  aiFeedFilterForCursor,
  discoverChannelForFeed,
  encodeAiFeedCursor,
  feedChannelCopy,
  feedPageSource,
  fetchAiFeedPage,
  initialAiFeedCursor,
  normalizeFeedChannel,
  parseAiFeedCursor,
} from '../src/services/feedSource.ts';

const series = (id, type = 'ai') => ({
  id,
  title: `剧 ${id}`,
  cover: '',
  type,
  episodesCount: 1,
  tags: [],
  origin: '',
});

/** 按站点真实分页行为造页：热榜 5 页（第 5 页 has_more=false）、目录 34 页。 */
function makeLoader(log) {
  return async filter => {
    log.push(filter);
    if (filter.sort === 'heat') {
      return {
        items: [series(`heat-${filter.page}`)],
        // 后端 `has_more = requested_page < total_pages`：第 5 页返回 false。
        hasMore: filter.page < AI_HEAT_LAST_PAGE,
      };
    }
    return {
      items: [series(`catalog-${filter.page}`)],
      hasMore: filter.page < AI_CATALOG_LAST_PAGE,
    };
  };
}

// ── 游标推到底 ──────────────────────────────────────────────────────────────
const chain = [];
let walker = initialAiFeedCursor();
while (walker && chain.length <= 200) {
  chain.push(walker);
  walker = advanceAiFeedCursor(walker);
}
const heatCursors = chain.filter(c => parseAiFeedCursor(c)?.segment === 'heat');
const catalogCursors = chain.filter(c => parseAiFeedCursor(c)?.segment === 'catalog');

// ── 整条流模拟滑动（用假 loader，不打网络）─────────────────────────────────
const loaderLog = [];
const loader = makeLoader(loaderLog);
const pages = [];
let cursor = null;
for (let guard = 0; guard <= 200; guard += 1) {
  const page = await fetchAiFeedPage(cursor, loader);
  pages.push(page);
  if (!page.hasMore) break;
  cursor = page.cursor;
}
const firstPageFilter = loaderLog[0];
const sixthPageFilter = loaderLog[5];

// ── 过滤：非 ai 条目不能混进 AI 流 ─────────────────────────────────────────
const mixedPage = await fetchAiFeedPage(null, async () => ({
  items: [series('ok'), series('wrong', 'drama')],
  hasMore: true,
}));

const checks = [
  ['ai normalize stays on ai instead of falling back to drama',
    normalizeFeedChannel('ai') === 'ai'],
  ['drama and comic keep their own channels',
    normalizeFeedChannel('drama') === 'drama' && normalizeFeedChannel('comic') === 'comic'],
  ['unknown channels still fall back to drama (dirty history must not blank the feed)',
    normalizeFeedChannel('anime') === 'drama' && normalizeFeedChannel(undefined) === 'drama'
      && normalizeFeedChannel('mystery') === 'drama'],
  ['ai has no App discover channel',
    discoverChannelForFeed('ai') === null
      && discoverChannelForFeed('drama') === 'live'
      && discoverChannelForFeed('comic') === 'comic'],
  ['ai declares the web catalog page source',
    feedPageSource('ai') === 'web-catalog'
      && feedPageSource('drama') === 'app-discover'
      && feedPageSource('comic') === 'app-discover'],
  ['ai copy admits the ordering is站方热度 rather than personalized',
    feedChannelCopy('ai').note.includes('热度')
      && !feedChannelCopy('ai').note.includes('为你推荐')
      && feedChannelCopy('drama').label === '真人短剧'],
  ['cursor starts on the heat section',
    initialAiFeedCursor() === 'ai:heat:1'],
  ['heat section covers exactly 5 pages then switches to the catalog',
    heatCursors.length === AI_HEAT_LAST_PAGE
      && advanceAiFeedCursor('ai:heat:5') === 'ai:catalog:1'],
  ['catalog section covers exactly 34 pages',
    catalogCursors.length === AI_CATALOG_LAST_PAGE
      && advanceAiFeedCursor(`ai:catalog:${AI_CATALOG_LAST_PAGE}`) === null],
  ['cursor reaches the end without repeats or gaps',
    chain.length === AI_HEAT_LAST_PAGE + AI_CATALOG_LAST_PAGE
      && new Set(chain).size === chain.length
      && chain[0] === 'ai:heat:1'
      && chain[chain.length - 1] === `ai:catalog:${AI_CATALOG_LAST_PAGE}`],
  ['malformed or out-of-range cursors restart from the first page instead of ending the feed',
    parseAiFeedCursor('ai:heat:99') === null
      && parseAiFeedCursor('garbage') === null
      && advanceAiFeedCursor('garbage') === 'ai:heat:1'
      && advanceAiFeedCursor(`ai:catalog:${AI_CATALOG_LAST_PAGE + 1}`) === 'ai:heat:1'],
  ['first page requests the hot-ai-drama rank with the required page size',
    firstPageFilter?.channel === 'ai'
      && firstPageFilter?.sort === 'heat'
      && firstPageFilter?.page === 1
      && firstPageFilter?.pageSize === 30
      && firstPageFilter?.category === '全部'
      && firstPageFilter?.audience === '全部'],
  ['page six switches to the ai-drama catalog route',
    sixthPageFilter?.channel === 'ai'
      && sixthPageFilter?.sort === 'recommend'
      && sixthPageFilter?.page === 1
      && sixthPageFilter?.pageSize === 24],
  ['heat page 5 keeps feeding the queue even though the backend reports has_more=false',
    pages[4]?.hasMore === true && pages[4]?.cursor === 'ai:catalog:1'],
  ['a full scroll walks 39 pages and then stops',
    pages.length === AI_HEAT_LAST_PAGE + AI_CATALOG_LAST_PAGE
      && pages[pages.length - 1].hasMore === false
      && pages[pages.length - 1].cursor === null],
  ['non-ai cards are dropped from the ai queue',
    mixedPage.items.length === 1 && mixedPage.items[0].id === 'ok'],
  ['filter builder never leaks a sorting mode the site ignores',
    aiFeedFilterForCursor({ segment: 'heat', page: 3 }).sort === 'heat'
      && aiFeedFilterForCursor({ segment: 'catalog', page: 3 }).sort === 'recommend'
      && encodeAiFeedCursor({ segment: 'heat', page: 3 }) === 'ai:heat:3'],
];

const failed = checks.filter(([, passed]) => !passed);
for (const [name, passed] of checks) console.log(`${passed ? 'PASS' : 'FAIL'} ${name}`);
if (failed.length) process.exit(1);
console.log(`${checks.length} passed, 0 failed`);