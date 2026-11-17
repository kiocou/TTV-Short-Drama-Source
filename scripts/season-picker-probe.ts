/**
 * 「搜索页选季窗口」数据层的探针（由 `verify-season-picker.mjs` 打包执行）。
 *
 * 三件事必须钉死：
 * 1. `fetchAllSeriesSeasons` **不能排除代表那一季**（详情页的 `fetchSeriesSeasons` 才排除）——
 *    排掉它，用户点开弹窗会以为这个系列少了第一季；
 * 2. 季表不可用时回退旧搜索链路，要静默、不抛、且代表季仍在；
 * 3. `seriesFoldedCount`（= `memberCount`）要能一路挂到卡片上：它是"点了先弹选季"
 *    的开关，而 `seasonCount` 在有季号缺口时是 undefined，不能拿它当开关。
 */
import { fetchAllSeriesSeasons } from '../src/services/relatedSeries';
import { collapseSeriesGroups } from '../src/services/seriesGrouping';
import type { MembershipMap } from '../src/services/seriesGrouping';
import type { SeasonApiEntry, SeriesItem } from '../src/types/catalog';

declare global {
  // eslint-disable-next-line no-var
  var __FAKE__: {
    fast?: unknown[] | 'reject';
    suggest?: unknown[] | 'reject';
    catalog?: unknown[] | 'reject';
    api?: SeasonApiEntry[] | 'reject' | 'missing';
  };
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

const BODY = '7647789981687106622';
const S34 = '7700456247924886553';
const S35 = '7700998944494455832';

/** 季表：35 季，index 连续；33、34 未上架；20 禁播但 80 集。 */
function seasons(): SeasonApiEntry[] {
  return Array.from({ length: 35 }, (_, index) => {
    const season = index + 1;
    const known: Record<number, string> = { 1: BODY, 34: S34, 35: S35 };
    const pending = season === 33 || season === 34;
    return {
      seriesId: known[season] ?? `jjz-${season}`,
      title: season === 1 ? '烬九州' : `烬九州第${season}${season >= 32 ? '期' : '季'}`,
      cover: '',
      seasonIndex: season,
      episodeCount: pending ? 0 : 80,
      disablePlay: pending || season === 20,
    };
  });
}

/** 只让季表可用（搜索三路返回空），用来隔离"映射"这件事。 */
function onlyApi(entries: SeasonApiEntry[] | 'reject' | 'missing'): void {
  globalThis.__FAKE__ = { fast: [], suggest: [], catalog: [], api: entries };
}

// ── (a) 季表映射：含代表季、升序、available 判定 ────────────────────
{
  onlyApi(seasons());
  const picks = await fetchAllSeriesSeasons(BODY, '烬九州', 'drama');

  check('35 条季表 → 35 张季卡（一条不排除）', picks.length, 35);
  check('季号升序 1..35', picks.map(pick => pick.season), Array.from({ length: 35 }, (_, i) => i + 1));
  check('代表那一季在列表里（第1季）', picks[0].item.id, BODY);
  check('代表季的标题是裸名「烬九州」', picks[0].item.title, '烬九州');
  check('未上架的 33、34 判为不可用',
    picks.filter(pick => !pick.available).map(pick => pick.season), [20, 33, 34]);
  check('0 集但未禁播也不可用', picks.find(pick => pick.season === 33)?.available, false);
  check('有集数但禁播也不可用', picks.find(pick => pick.season === 20)?.available, false);
  check('正常季可用', picks.find(pick => pick.season === 1)?.available, true);
  check('季卡来源标记为系列列表', picks.every(pick => pick.item.origin === '系列列表'), true);
  check('季卡频道取自调用方', picks.every(pick => pick.item.type === 'drama'), true);
}

// ── (b) 与详情页的语义差异：不排除"当前季" ─────────────────────────
{
  onlyApi(seasons());
  // 用户点的是第 35 期那张折叠卡 → 弹窗里第 35 期必须还在。
  const picks = await fetchAllSeriesSeasons(S35, '烬九州第三十五期', 'drama');
  check('点第35期 → 35 期仍在列表里', picks.some(pick => pick.item.id === S35), true);
  check('仍然是 35 张（没有排除任何一条）', picks.length, 35);
}

// ── (c) 季表为空 → 回退搜索链路，代表季补回来 ──────────────────────
{
  globalThis.__FAKE__ = {
    fast: [item('jjz-2', '烬九州第二季'), item('jjz-3', '烬九州第三季')],
    suggest: [],
    catalog: [],
    api: [],
  };
  const picks = await fetchAllSeriesSeasons(BODY, '烬九州', 'drama');
  check('空表 → 回退后仍有代表季（第1季）', picks[0].item.id, BODY);
  check('回退结果按季号升序', picks.map(pick => pick.season), [1, 2, 3]);
  check('回退里代表季判为可用', picks[0].available, true);
}

// ── (d) 季表抛错 → 不抛、仍回退 ────────────────────────────────────
{
  globalThis.__FAKE__ = {
    fast: [item('jjz-2', '烬九州第二季')],
    suggest: [],
    catalog: [],
    api: 'reject',
  };
  let threw = false;
  let picks: Awaited<ReturnType<typeof fetchAllSeriesSeasons>> = [];
  try {
    picks = await fetchAllSeriesSeasons(BODY, '烬九州', 'drama');
  } catch {
    threw = true;
  }
  check('季表抛错不冒泡到弹窗', threw, false);
  check('抛错后回退结果含代表季', picks.map(pick => pick.season), [1, 2]);
}

// ── (e) 旧后端（命令不存在）+ 搜索也空 → 空数组 ────────────────────
{
  globalThis.__FAKE__ = { fast: 'reject', suggest: 'reject', catalog: [], api: 'missing' };
  let threw = false;
  let picks: Awaited<ReturnType<typeof fetchAllSeriesSeasons>> = [];
  try {
    picks = await fetchAllSeriesSeasons('unknown-id', '某部不存在季的剧', 'drama');
  } catch {
    threw = true;
  }
  check('全链路不可用时不抛', threw, false);
  check('全链路不可用时仍给出代表季一条', picks.map(pick => pick.item.id), ['unknown-id']);
}

// ── (f) seriesFoldedCount 传递（镜像 SearchView.applyItems 的映射）──
{
  /** 与 `SearchView.applyItems` 逐字同语义的镜像（那边是组件内闭包，无法直接 import）。 */
  const toCards = (items: SeriesItem[], membership: MembershipMap): SeriesItem[] =>
    collapseSeriesGroups(items, membership).map(({ representative, memberCount, seasonCount }) => {
      if (memberCount <= 1 && !(seasonCount && seasonCount > 1)) return representative;
      const card: SeriesItem = { ...representative, seriesFoldedCount: memberCount };
      if (seasonCount && seasonCount > 1) card.seasonCount = seasonCount;
      return card;
    });

  const feed = [item(BODY, '烬九州', 115), item(S34, '烬九州第三十四季', 168)];
  const extra = seasons()
    .filter(entry => entry.seriesId !== BODY && entry.seriesId !== S34)
    .map(entry => item(entry.seriesId, entry.title, entry.episodeCount));
  const membership: MembershipMap = new Map(
    seasons().map(entry => [entry.seriesId, { baseKey: '烬九州', season: entry.seasonIndex }]),
  );

  const cards = toCards([...feed, ...extra], membership);
  check('折叠成 1 张卡', cards.length, 1);
  check('卡上挂了 seriesFoldedCount = 35', cards[0].seriesFoldedCount, 35);
  check('卡上也挂了 seasonCount = 35', cards[0].seasonCount, 35);
  check('代表是首屏第 1 条（位置最小）', cards[0].id, BODY);

  // 无归属的普通剧不挂这两个字段（否则每张卡都会被当成"系列"）。
  const plain = toCards([item('p1', '别的剧'), item('p2', '又一部')], new Map());
  check('普通剧不带 seriesFoldedCount', plain.map(card => card.seriesFoldedCount), [undefined, undefined]);
  check('普通剧不带 seasonCount', plain.map(card => card.seasonCount), [undefined, undefined]);

  // 季号有缺口（1 + 9）→ seasonCount 不给，但 seriesFoldedCount 仍然 > 1（照样弹窗）。
  const gapped = toCards(
    [item('c1', '持械入宋：第一季'), item('c9', '持械入宋：第九季')],
    new Map(),
  );
  check('有缺口的系列仍折叠成 1 张', gapped.length, 1);
  check('有缺口 → seasonCount 不给', gapped[0].seasonCount, undefined);
  check('但 seriesFoldedCount = 2（这就是弹窗开关）', gapped[0].seriesFoldedCount, 2);
}

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exitCode = 1;
