/**
 * 「系列剧弹窗的权威链路」（后端 `catalog_series_seasons`）的探针
 * （由 `verify-season-api.mjs` 打包执行）。
 *
 * 断言的是这一版真正新增、也最容易悄悄坏掉的东西：
 * - 季表命中后**不进搜索**（成本契约：打开弹窗从最多 4 次往返降到 1 次）；
 * - 长系列**不被截断**（旧上限 48 是给搜索候选池的，35 季必须一次铺满）；
 * - `available` 的三条口径（`episodeCount === 0`、`disablePlay`、正常）；
 * - 季表为空 / 抛错 / 命令不存在时**静默回退**旧链路，且不抛。
 *
 * 这些事在界面上表现为"弹窗里少了几季"或"点开是空的"，肉眼极难发现，所以把
 * 纯函数与假 IPC 摆在一起断言。
 */
import { fetchSeriesSeasons } from '../src/services/relatedSeries';
import { parseSeasonLabel, seasonUnitGroup } from '../src/services/seasonLabel';
import type { SeasonApiEntry, SeriesItem } from '../src/types/catalog';
import type { SeriesDetail } from '../src/types/series';

declare global {
  // eslint-disable-next-line no-var
  var __FAKE__: {
    fast?: unknown[] | 'reject';
    suggest?: unknown[] | 'reject';
    catalog?: unknown[] | 'reject';
    api?: SeasonApiEntry[] | 'reject' | 'missing';
  };
  // eslint-disable-next-line no-var
  var __CALLS__: string[];
}

let pass = 0;
let fail = 0;

function check(name: string, actual: unknown, expected: unknown): void {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (ok) pass += 1;
  else fail += 1;
  console.log(
    `${ok ? 'PASS ' : 'FAIL '}${name}` +
      (ok ? '' : `  期望=${JSON.stringify(expected)} 实际=${JSON.stringify(actual)}`),
  );
}

function detail(title: string, overrides: Partial<SeriesDetail> = {}): SeriesDetail {
  return {
    id: '1000',
    title,
    cover: '',
    type: 'drama',
    tags: ['玄幻'],
    origin: '红果',
    episodesCount: 80,
    description: '',
    episodes: [],
    availableQualities: [],
    sources: [],
    ...overrides,
  };
}

function item(id: string, title: string, episodesCount = 80): SeriesItem {
  return { id, title, cover: '', type: 'drama', episodesCount, tags: ['玄幻'], origin: '红果' };
}

/**
 * 《烬九州》35 季的真实形状（2026-10-29 实测）：
 * - 第 1~31 季标题写作「第N季」，第 32~35 季写作「第N期」——同一系列混用两种量词；
 * - 第 33、34 季还没上架（0 集且禁播）；
 * - 第 20 季有集数但被站方禁播（`disablePlay` 单独生效，用来钉"禁播也算不可用"）。
 */
function jinJiuzhou(): SeasonApiEntry[] {
  return Array.from({ length: 35 }, (_, index) => {
    const season = index + 1;
    const unit = season >= 32 ? '期' : '季';
    const pending = season === 33 || season === 34;
    return {
      seriesId: `jjz-${season}`,
      title: `烬九州第${season}${unit}`,
      cover: '',
      seasonIndex: season,
      episodeCount: pending ? 0 : 80,
      disablePlay: pending || season === 20,
    };
  });
}

/** 只让季表可用，搜索结果一律为空：用来证明"命中季表就不再搜索"。 */
function onlyApi(entries: SeasonApiEntry[] | 'reject' | 'missing'): void {
  globalThis.__FAKE__ = { fast: [], suggest: [], catalog: [], api: entries };
  globalThis.__CALLS__.length = 0;
}

// ── 0. 「期」与「季」是同一套编号（回退链路要靠它认出后几季）──────────
{
  check('「第三十五期」解析出 35', parseSeasonLabel('烬九州第三十五期'), {
    base: '烬九州',
    season: 35,
    unit: '期',
  });
  check('「期」与「季」同组', seasonUnitGroup('期') === seasonUnitGroup('季'), true);
  check('「部」独立成组（另一种分卷习惯）', seasonUnitGroup('部') === seasonUnitGroup('季'), false);
}

// ── 1. 长系列一次铺满：35 条 → 排除当前季 → 34 条，不截断 ────────────
{
  onlyApi(jinJiuzhou());
  const picks = await fetchSeriesSeasons(detail('烬九州第三十五期', { id: 'jjz-35' }));

  check('35 季排除当前季 → 34 张卡（旧 48 上限未参与）', picks.length, 34);
  check(
    '看第35季：前作 34→1 依次铺开',
    picks.map(pick => pick.season),
    Array.from({ length: 34 }, (_, index) => 34 - index),
  );
  check('当前季（第35季）不在弹窗里', picks.some(pick => pick.season === 35), false);
  check('标题原样透传（不重写量词）', picks.find(pick => pick.season === 32)?.item.title,
    '烬九州第32期');
}

// ── 2. available：0 集、禁播、正常三种情形 ──────────────────────────
{
  onlyApi(jinJiuzhou());
  const picks = await fetchSeriesSeasons(detail('烬九州第三十五期', { id: 'jjz-35' }));

  check('未上架的 33、34 与禁播的 20 判为不可用',
    picks.filter(pick => !pick.available).map(pick => pick.season), [34, 33, 20]);
  check('其余季可点（31 张）', picks.filter(pick => pick.available).length, 31);
  check('0 集即使未禁播也不可用', picks.find(pick => pick.season === 34)?.available, false);
  check('有集数但禁播仍不可用', picks.find(pick => pick.season === 20)?.available, false);
  check('正常季可用', picks.find(pick => pick.season === 32)?.available, true);
}

// ── 3. 季表命中时零搜索调用（成本契约）──────────────────────────────
{
  onlyApi(jinJiuzhou());
  await fetchSeriesSeasons(detail('烬九州第三十一季', { id: 'jjz-31' }));

  check('只调了季表，没打搜索', globalThis.__CALLS__, ['seriesSeasons']);
}

// ── 4. 季表不可用时静默回退旧链路（不抛、不缺季）─────────────────────
{
  // 旧后端 / verify 的 ipc-stub：根本没有这个命令。
  globalThis.__FAKE__ = {
    fast: [item('s32', '烬九州第三十二期')],
    suggest: [],
    catalog: [],
    api: 'missing',
  };
  globalThis.__CALLS__.length = 0;
  const picks = await fetchSeriesSeasons(detail('烬九州第三十一季', { id: 'jjz-31' }));
  check('命令不存在 → 回退搜索，且「期」条目被认出', picks.map(pick => pick.season), [32]);
  check('回退时确实打了搜索', globalThis.__CALLS__.includes('searchFast'), true);
}
{
  // 命令存在但抛错（后端抖动 / 参数不被接受）。
  globalThis.__FAKE__ = {
    fast: [item('s2', '烬九州第二季')],
    suggest: [],
    catalog: [],
    api: 'reject',
  };
  globalThis.__CALLS__.length = 0;
  let threw = false;
  let seasons: number[] = [];
  try {
    const picks = await fetchSeriesSeasons(detail('烬九州第一季', { id: 'jjz-1' }));
    seasons = picks.map(pick => pick.season);
  } catch {
    threw = true;
  }
  check('季表抛错时不冒泡给详情页', threw, false);
  check('季表抛错后回退搜索结果照常', seasons, [2]);
}
{
  // 空表：后端认得这部剧但没给季（例如刚建条目）。
  onlyApi([]);
  const picks = await fetchSeriesSeasons(detail('烬九州第一季', { id: 'jjz-1' }));
  check('空表 → 回退搜索（不当成"没有其它季"直接返回）', picks.length, 0);
  check('空表也会去打搜索', globalThis.__CALLS__.includes('searchFast'), true);
}

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exitCode = 1;
