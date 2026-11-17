import { createSeriesPrefetcher } from '../src/services/feedPrefetch.ts';
import {
  TAG_FETCH_LIMIT,
  createTagResolver,
  enrichTags,
  itemWeight,
  orderFeedPage,
} from '../src/services/feedRank.ts';

/**
 * 详情预取与标签解析器的验收（按最终 API）。
 *
 * 重点断言三件事：`clear()` 必须切断在途 Promise；失败必须把真实 Error 交回主动
 * 打开；标签解析每次最多 8 条、每代并发 2，旧代 worker 不得消费新代队列。
 */

const checks = [];
const check = (name, passed) => checks.push([name, passed]);
const settle = () => new Promise(resolve => setTimeout(resolve, 0));

const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
};

// ── 预取器 ──────────────────────────────────────────────────────────────
{
  const calls = [];
  const prefetcher = createSeriesPrefetcher({
    load: async (id, channel) => {
      calls.push(`${channel}:${id}`);
      return { id, channel };
    },
  });
  const a = await prefetcher.get('1', 'drama');
  check('get loads on miss', a.id === '1' && calls.length === 1);
  await prefetcher.get('1', 'drama');
  check('cached hit does not load again', calls.length === 1);
  await prefetcher.get('1', 'comic');
  check('cache key includes channel', calls.length === 2);
}

{
  // clear 必须切断在途：否则同一个 id 会复用旧 Promise，拿到旧数据。
  const pending = deferred();
  let calls = 0;
  const prefetcher = createSeriesPrefetcher({
    load: () => {
      calls += 1;
      return calls === 1 ? pending.promise : Promise.resolve({ round: 2 });
    },
  });
  void prefetcher.get('1', 'drama');
  prefetcher.clear();
  const fresh = await prefetcher.get('1', 'drama');
  check('clear cuts the inflight promise (second load happens)', calls === 2 && fresh.round === 2);
  pending.resolve({ round: 1 });
  await settle();
  const after = await prefetcher.get('1', 'drama');
  check('stale result never overwrites the new cache', after.round === 2);
}

{
  const prefetcher = createSeriesPrefetcher({
    load: async () => { throw new Error('HTTP 503'); },
  });
  let error = null;
  try {
    await prefetcher.get('1', 'drama');
  } catch (failure) {
    error = failure;
  }
  check('get surfaces the real error instead of null', error instanceof Error && error.message === 'HTTP 503');
}

{
  let calls = 0;
  const prefetcher = createSeriesPrefetcher({
    load: async () => {
      calls += 1;
      throw new Error('warm failure');
    },
  });
  prefetcher.warm(['1', '2', '3'], 'drama');
  await settle();
  await settle();
  check('warm only touches the two neighbours', calls === 2);
  check('warm swallows failures', prefetcher.pending() === 0);
}

{
  let calls = 0;
  const pending = deferred();
  const prefetcher = createSeriesPrefetcher({
    load: () => {
      calls += 1;
      return pending.promise;
    },
  });
  void prefetcher.get('9', 'drama');
  void prefetcher.get('9', 'drama');
  check('inflight gets are coalesced', calls === 1);
  pending.resolve({ ok: true });
  await settle();
  check('pending drains after resolution', prefetcher.pending() === 0);
}

// ── 标签解析器 ──────────────────────────────────────────────────────────
{
  const loads = [];
  const resolver = createTagResolver({
    load: async id => {
      loads.push(id);
      await settle();
      return ['战神'];
    },
  });
  await resolver.resolve(Array.from({ length: 20 }, (_, index) => `s${index}`));
  check('a single resolve never fetches more than the limit', loads.length === TAG_FETCH_LIMIT);
  check('only fetched ids come back', resolver.cached('s0')?.[0] === '战神' && resolver.cached('s19') === undefined);
}

{
  // 并发上限 2：用可控 deferred 观察同时在跑的条目数。
  let active = 0;
  let peak = 0;
  const gates = [];
  const resolver = createTagResolver({
    load: async id => {
      active += 1;
      peak = Math.max(peak, active);
      const gate = deferred();
      gates.push(gate);
      await gate.promise;
      active -= 1;
      return [id];
    },
  });
  void resolver.resolve(['a', 'b', 'c', 'd']);
  await settle();
  check('at most two tag loads run at once', active <= 2 && peak === 2);
  while (gates.length) gates.shift().resolve(true);
  await settle();
  await settle();
}

{
  // clear 之后：旧 worker 立刻退出，不消费新代队列；新代并发仍然 <=2。
  let activeNew = 0;
  let peakAfterClear = 0;
  const firstGates = [];
  const loadedOld = [];
  const resolver = createTagResolver({
    load: async id => {
      const isNew = id.startsWith('new');
      if (isNew) {
        activeNew += 1;
        peakAfterClear = Math.max(peakAfterClear, activeNew);
      } else {
        loadedOld.push(id);
        const gate = deferred();
        firstGates.push({ id, gate });
        await gate.promise;
      }
      if (isNew) {
        await settle();
        activeNew -= 1;
      }
      return [id];
    },
  });
  void resolver.resolve(['old1', 'old2', 'old3', 'old4']);
  await settle();
  resolver.clear();
  void resolver.resolve(['new1', 'new2', 'new3', 'new4']);
  await settle();
  check('new generation starts at most two loads', peakAfterClear <= 2);
  firstGates.forEach(({ gate }) => gate.resolve(true));
  await settle();
  await settle();
  check('old workers stop instead of consuming queued work',
    loadedOld.join() === 'old1,old2');
  check('old generation results are not written into the new cache',
    resolver.cached('old1') === undefined && resolver.cached('old2') === undefined);
  check('new generation keeps its own cache', resolver.cached('new1') !== undefined);
}

{
  let calls = 0;
  const resolver = createTagResolver({
    load: async () => {
      calls += 1;
      throw new Error('HTTP 500');
    },
  });
  await resolver.resolve(['x']);
  await resolver.resolve(['x']);
  check('failure is negative-cached, not retried', calls === 1 && resolver.cached('x')?.length === 0);
}

{
  const resolver = createTagResolver({ load: async id => [id] });
  for (let index = 0; index < 40; index += 1) {
    await resolver.resolve([`b${index}`]);
  }
  check('tag cache stays bounded', resolver.size() <= 32);
}

{
  // 同一个 id 在结果回来之前被两次 resolve：必须合并成一次加载。
  const loads = [];
  const pending = deferred();
  const resolver = createTagResolver({
    load: async id => {
      loads.push(id);
      await pending.promise;
      return ['战神'];
    },
  });
  const first = resolver.resolve(['same']);
  await settle();
  const second = resolver.resolve(['same']);
  pending.resolve(true);
  const [firstResult, secondResult] = await Promise.all([first, second]);
  check('same id resolved concurrently loads exactly once', loads.length === 1);
  check('both concurrent resolves see the tags',
    firstResult.get('same')?.[0] === '战神' && secondResult.get('same')?.[0] === '战神');
}

// ── 真实契约：卡片 tags 为空，补到元数据后权重必须大于 1 ─────────────────
{
  const cards = [
    { id: 'c1', title: '一', cover: '', type: 'drama', episodesCount: 10, tags: [], origin: '' },
    { id: 'c2', title: '二', cover: '', type: 'drama', episodesCount: 10, tags: [], origin: '' },
    { id: 'c3', title: '三', cover: '', type: 'drama', episodesCount: 10, tags: [], origin: '' },
  ];
  const resolver = createTagResolver({
    load: async id => (id === 'c2' ? ['战神'] : ['都市']),
  });
  const tagsById = await resolver.resolve(cards.map(card => card.id));
  const enriched = enrichTags(cards, tagsById);
  const interest = new Map([['战神', 3]]);
  check('app cards really start with empty tags', cards.every(card => card.tags.length === 0));
  check('enrichment gives the matched card a weight above base',
    itemWeight(enriched.find(card => card.id === 'c2'), interest) > 1);
  check('unmatched cards keep the base weight',
    itemWeight(enriched.find(card => card.id === 'c1'), interest) === 1);
  const ordered = orderFeedPage(enriched, { interest, rng: () => 0.5 });
  check('weighted ordering puts the matched card first', ordered[0].id === 'c2');
  check('enrichment does not mutate the source cards', cards.every(card => card.tags.length === 0));
}

const failed = checks.filter(([, passed]) => !passed);
for (const [name, passed] of checks) console.log(`${passed ? 'PASS' : 'FAIL'} ${name}`);
if (failed.length) process.exit(1);
console.log(`${checks.length} passed, 0 failed`);