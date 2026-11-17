/**
 * 「搜索页系列聚合」的探针（由 `verify-search-season-expand.mjs` 打包执行）。
 *
 * 断言的是这一版新增的两件事，它们在界面上都只说一句话——"搜到了这个系列，但只出
 * 了三条"：
 * - 锚点挑选：一个系列只能产生**一个**锚点（多打的后端请求纯属浪费），且要挑到
 *   最像系列入口的那一条；
 * - 并入结果：整季补进来后按 id 去重，已存在的首屏卡保留原样（不覆盖 origin/封面），
 *   未上架的季不进来。
 *
 * fixture 用的是实测真实数据（《烬九州》App 搜索结果 + 35 条权威季表）。
 */
import { expandSeriesSeasons, pickSeasonAnchors } from '../src/services/searchSeasonExpand';
import { parseSeasonLabel, SEASON_SEPARATORS } from '../src/services/seasonLabel';
import type { SeasonApiEntry, SeriesItem } from '../src/types/catalog';

declare global {
  // eslint-disable-next-line no-var
  var __SEASONS__: Record<string, SeasonApiEntry[] | 'reject'>;
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

function item(id: string, title: string, episodesCount = 100): SeriesItem {
  return { id, title, cover: '', type: 'drama', episodesCount, tags: ['玄幻'], origin: '红果' };
}

/**
 * 与 `SearchView.tsx` 里那个 `mergeById` **逐字同语义**的镜像。
 *
 * 那边是组件内的局部函数（不导出，也不该为一个测试改变导出面），所以这里复制规则：
 * `base` 的顺序与元素优先，`extra` 只追加没见过的 id。两边一旦漂移，这组用例就失去
 * 意义——所以注释写在这里，改动任何一边都要同步。
 */
function mergeById(base: SeriesItem[], extra: SeriesItem[]): SeriesItem[] {
  if (extra.length === 0) return base;
  const seen = new Set(base.map(entry => entry.id));
  const added = extra.filter(entry => !seen.has(entry.id));
  return added.length > 0 ? [...base, ...added] : base;
}

// ── 实测数据：《烬九州》App 搜索返回的那三条 ──────────────────────────
const BODY_ID = '7647789981687106622';
const S34_ID = '7700456247924886553';
const S35_ID = '7700998944494455832';
const body = item(BODY_ID, '烬九州', 98);
const s34 = item(S34_ID, '烬九州：三十四季', 168);
const s35 = item(S35_ID, '烬九州第三十五期', 168);

/** 真实形态的季表：35 条、index 连续 1..35，第一季就是本体那条。 */
function jinJiuzhouSeasons(): SeasonApiEntry[] {
  const known: Record<number, string> = { 1: BODY_ID, 34: S34_ID, 35: S35_ID };
  return Array.from({ length: 35 }, (_, index) => {
    const season = index + 1;
    return {
      seriesId: known[season] ?? `jjz-${season}`,
      title: `烬九州第${season}${season >= 32 ? '期' : '季'}`,
      cover: '',
      seasonIndex: season,
      episodeCount: 120,
      disablePlay: false,
    };
  });
}

// ── 0. 锚点规则依赖的解析边界（必须与 provider.rs::SEASON_SEPARATORS 一致）──
//
// 锚点挑选完全建立在 `parseSeasonLabel` 上：它多认一种写法，搜索页就多认一个系列；
// 它多拒绝一种，就少补一整个系列。Rust 侧的 `search_season` 是同一套判定，两处
// 不一致的表现是"后端补了那一季、前端认不出来"。这里钉住前端这一半。
{
  check('省略「第」+ 全角冒号', parseSeasonLabel('烬九州：三十四季'),
    { base: '烬九州', season: 34, unit: '季' });
  check('省略「第」+ 空白', parseSeasonLabel('烬九州 三十四季'),
    { base: '烬九州', season: 34, unit: '季' });
  check('省略「第」+ 中点 + 「期」', parseSeasonLabel('烬九州·三十四期'),
    { base: '烬九州', season: 34, unit: '期' });
  check('标题以季号开头（base 为空）被拒', parseSeasonLabel('三十四季'), null);
  check('尾部不是量词被拒', parseSeasonLabel('第二季的逆袭'), null);
  check('裸名被拒', parseSeasonLabel('烬九州'), null);
  check('分隔符常量与 Rust 定稿逐字一致', [...SEASON_SEPARATORS].join(''), '：:·-—｜|/、');
}

// ── (a) 一个系列只产生一个锚点 ──────────────────────────────────────
{
  const anchors = pickSeasonAnchors([body, s34, s35]);
  /**
   * 「烬九州：三十四季」靠 2026-10-29 那次放宽解析才认得出来（季号前是全角冒号，
   * 没有「第」）。此前它解析为 null，这一组的锚点会落到「第三十五期」上 —— 规则本身
   * 没错，只是喂进去的线索少了一条。
   */
  check('三条同系列 → 1 个锚点', anchors.length, 1);
  check('锚点是季号最小的第34季那条', anchors.map(anchor => anchor.id), [S34_ID]);
  check('锚点解析出的 base 是「烬九州」', parseSeasonLabel(anchors[0].title)?.base, '烬九州');
  check('裸名本体不进锚点组（解析不出季号）',
    anchors.some(anchor => anchor.id === BODY_ID), false);
}

// ── (a2) 同组内取季号最小的那条（规则本身）────────────────────────
{
  const list = [
    item('x35', '烬九州第三十五期'),
    item('x34', '《烬九州》第三十四季'),
    item('x02', '烬九州第二季'),
  ];
  check('同组取季号最小 → 第2季那条', pickSeasonAnchors(list).map(anchor => anchor.id), ['x02']);
  check('limit 生效：默认最多 2 个', pickSeasonAnchors([
    item('a1', '甲剧第二季'),
    item('b1', '乙剧第二季'),
    item('c1', '丙剧第二季'),
  ]).length, 2);
  check('limit=1 只给一个', pickSeasonAnchors([
    item('a1', '甲剧第二季'),
    item('b1', '乙剧第二季'),
  ], 1).length, 1);
}

// ── (a3) 一条季号都解析不出 → 退回第 1 条 ───────────────────────────
{
  check('全裸名时退回第 1 条', pickSeasonAnchors([body, item('z', '火线追击')]).map(a => a.id), [BODY_ID]);
  check('空列表没有锚点', pickSeasonAnchors([]), []);
}

// ── (a4) 3 条首屏 → 季表 35 条 → 合并后 35 条，本体保留原卡 ─────────
{
  // 两个 id 都指向同一张季表：锚点无论落在 34 季还是 35 期，拿到的都是这份。
  globalThis.__SEASONS__ = {
    [S34_ID]: jinJiuzhouSeasons(),
    [S35_ID]: jinJiuzhouSeasons(),
  };
  const expanded = await expandSeriesSeasons([body, s34, s35], 'drama');
  check('季表 35 条全部转成条目', expanded.items.length, 35);
  check('并入条目的来源标记为系列列表', expanded.items.every(entry => entry.origin === '系列列表'), true);
  check('并入条目的频道取自调用方', expanded.items.every(entry => entry.type === 'drama'), true);
  // 归属表是分组（`seriesGrouping.ts`）的第一优先依据：裸名的第 1 季只能靠它认回来。
  check('裸名本体也带回归属（第1季）', expanded.membership.get(BODY_ID),
    { baseKey: '烬九州', season: 1 });
  check('第35期带回归属（第35季）', expanded.membership.get(S35_ID),
    { baseKey: '烬九州', season: 35 });
  check('归属条数与并入条数一致', expanded.membership.size, expanded.items.length);

  const merged = mergeById([body, s34, s35], expanded.items);
  check('合并后总计 35 条（按 id 去重）', merged.length, 35);
  check('本体保留首屏原卡（origin 未被覆盖）', merged.find(entry => entry.id === BODY_ID)?.origin, '红果');
  check('第34季也保留首屏原卡', merged.find(entry => entry.id === S34_ID)?.origin, '红果');
  check('首屏顺序不变（前三条仍是原来的）', merged.slice(0, 3).map(entry => entry.id),
    [BODY_ID, S34_ID, S35_ID]);
}

// ── (a5) 换哪一条当锚点，结果都一样（防止锚点规则悄悄退化）──────────
{
  globalThis.__SEASONS__ = {
    [S34_ID]: jinJiuzhouSeasons(),
    [S35_ID]: jinJiuzhouSeasons(),
  };
  const byS34 = await expandSeriesSeasons([s34], 'drama');
  const byS35 = await expandSeriesSeasons([s35], 'drama');
  check('以第34季为锚点 → 35 条', byS34.items.length, 35);
  check('以第35期为锚点 → 35 条', byS35.items.length, 35);
  check('两次结果的 id 序列一致', byS34.items.map(entry => entry.id), byS35.items.map(entry => entry.id));
}

// ── (b) 两个不同系列 → 2 个锚点、互不干扰 ───────────────────────────
{
  const list = [
    item('x35', '烬九州第三十五期'),
    item('p2', '聚宝盆第二季'),
    item('p3', '聚宝盆第三季'),
  ];
  const anchors = pickSeasonAnchors(list);
  check('两个系列 → 2 个锚点', anchors.map(anchor => anchor.id), ['x35', 'p2']);

  globalThis.__SEASONS__ = {
    x35: [
      { seriesId: 'x35', title: '烬九州第三十五期', cover: '', seasonIndex: 35, episodeCount: 168, disablePlay: false },
      { seriesId: 'x34', title: '烬九州第三十四季', cover: '', seasonIndex: 34, episodeCount: 168, disablePlay: false },
    ],
    p2: [
      { seriesId: 'p2', title: '聚宝盆第二季', cover: '', seasonIndex: 2, episodeCount: 60, disablePlay: false },
      { seriesId: 'p3', title: '聚宝盆第三季', cover: '', seasonIndex: 3, episodeCount: 60, disablePlay: false },
    ],
  };
  const expanded = await expandSeriesSeasons(list, 'drama');
  check('两路季表都并入', expanded.items.map(entry => entry.id).sort(), ['p2', 'p3', 'x34', 'x35']);
}

// ── (c) 未上架的季不进列表 ─────────────────────────────────────────
{
  const anchor = item('anchor', '聚宝盆第二季');
  globalThis.__SEASONS__ = {
    anchor: Array.from({ length: 25 }, (_, index) => {
      const season = index + 1;
      // 第 7 季没有内容、第 8 季被站方禁播：两种"点开没内容"都要挡。
      const pending = season === 7;
      const blocked = season === 8;
      return {
        seriesId: `s-${season}`,
        title: `聚宝盆第${season}季`,
        cover: '',
        seasonIndex: season,
        episodeCount: pending ? 0 : 60,
        disablePlay: pending || blocked,
      };
    }),
  };
  const expanded = await expandSeriesSeasons([anchor], 'drama');
  check('25 季里 2 条未上架 → 只留 23 条', expanded.items.length, 23);
  check('0 集的第7季被挡掉', expanded.items.some(entry => entry.id === 's-7'), false);
  check('禁播的第8季被挡掉', expanded.items.some(entry => entry.id === 's-8'), false);
}

// ── (d) 季表抛错 → 空数组、不抛 ────────────────────────────────────
{
  globalThis.__SEASONS__ = { anchor: 'reject' };
  let threw = false;
  let expanded: { items: SeriesItem[] } = { items: [] };
  try {
    expanded = await expandSeriesSeasons([item('anchor', '聚宝盆第二季')], 'drama');
  } catch {
    threw = true;
  }
  check('季表抛错不冒泡给搜索页', threw, false);
  check('季表抛错返回空数组', expanded.items, []);
}

// ── (e) 跨锚点重复 id 只保留一条 ───────────────────────────────────
{
  const shared: SeasonApiEntry = {
    seriesId: 'dup-1', title: '烬九州第三季', cover: '', seasonIndex: 3, episodeCount: 100, disablePlay: false,
  };
  globalThis.__SEASONS__ = {
    a35: [shared, { seriesId: 'a-only', title: '烬九州第四季', cover: '', seasonIndex: 4, episodeCount: 100, disablePlay: false }],
    p2: [shared, { seriesId: 'p-only', title: '聚宝盆第四季', cover: '', seasonIndex: 4, episodeCount: 100, disablePlay: false }],
  };
  const expanded = await expandSeriesSeasons([
    item('a35', '烬九州第三十五期'),
    item('p2', '聚宝盆第二季'),
  ], 'drama');
  check('重复 id 只保留一条', expanded.items.map(entry => entry.id), ['dup-1', 'a-only', 'p-only']);
}

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exitCode = 1;
