/**
 * 「搜索页同系列折叠成单卡」的探针（由 `verify-series-collapse.mjs` 打包执行）。
 *
 * 用户原话：*还是全部铺出来的，并没有全部合并到一个卡片里面*。所以这一版的目标是
 * **一个系列只占一张卡**，点进去再选季。
 *
 * 三个最容易错的口径：
 * 1. 代表卡选谁 —— 按**原始下标最小**（搜索相关度最高的那张），不是"季号最小"。
 *    搜「烬九州第三十五季」时前排是第 35 期，按季号取代表会让用户看到第 1 季、
 *    以为自己搜错了；
 * 2. 季数角标只在**季号连续**时才给（持械入宋 1 + 9 那种缺口必须不显示）；
 * 3. 无归属条目（别的剧）各自成卡，不能被顺手并进任何系列。
 */
import { collapseSeriesGroups, groupSeriesItems } from '../src/services/seriesGrouping';
import type { MembershipMap } from '../src/services/seriesGrouping';

interface Row {
  id: string;
  title: string;
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

function row(id: string, title: string): Row {
  return { id, title };
}

const cards = (items: Row[], membership: MembershipMap) =>
  collapseSeriesGroups(items, membership);

// ── (a) 真实数据：35 季折叠成 1 张，代表是第 1 季 ───────────────────
const BODY = '7647789981687106622';
const S2 = '7651961603817999384';
const S3 = '7652205899469048894';
const S4 = '7657786086797610008';
const SAME_NAME_S2 = '7658590108685175832'; // 「烬九州：第二季」74 集，另一部同名
const ZHENGCHEN = '7658214394211339288';    // 「烬九州：征尘录第一季」
const OTHER_98 = '7654579310786776089';     // 「烬九州」98 集，另一部
const OTHER_74 = '7648478038362967065';     // 「烬九州」74 集，另一部
const CHIXIE_S1 = '7671966197985856536';
const CHIXIE_S9 = '7694939439503510590';

/** 首屏 13 条（lead 实测的接口返回顺序）。 */
function feed(): Row[] {
  return [
    row(BODY, '烬九州'),                       // 裸名 = 季表第 1 季
    row(S3, '烬九州第三季'),
    row(S2, '烬九州第二季'),
    row('7655548765004123160', '悍卒！燃烬九州'),
    row('7654433962474884158', '烬九州AI真人版'),
    row(S4, '烬九州第四季'),
    row(SAME_NAME_S2, '烬九州：第二季'),
    row(ZHENGCHEN, '烬九州：征尘录第一季'),
    row(OTHER_98, '烬九州'),
    row('7659763666417355800', '烬九州，风起悍卒'),
    row(OTHER_74, '烬九州'),
    row(CHIXIE_S1, '持械入宋：第一季'),
    row(CHIXIE_S9, '持械入宋：第九季'),
  ];
}

/** 季表 1..35 的中文数字标题（index=1 是裸名）。 */
const CN = ['一', '二', '三', '四', '五', '六', '七', '八', '九', '十'];
function cnNumber(n: number): string {
  if (n <= 10) return CN[n - 1];
  if (n < 20) return `十${CN[n - 11]}`;
  const tens = Math.floor(n / 10);
  const ones = n % 10;
  return `${CN[tens - 1]}十${ones === 0 ? '' : CN[ones - 1]}`;
}

/** 把「首屏 + 季表聚合」拼成 44 条，并回填归属（与 SearchView 的真实数据流一致）。 */
function fullBatch(): { items: Row[]; membership: MembershipMap } {
  const base = feed();
  const existing = new Set(base.map(entry => entry.id));
  const seasonIds: Record<number, string> = { 1: BODY, 2: S2, 3: S3, 4: S4 };
  const membership: MembershipMap = new Map([[BODY, { baseKey: '烬九州', season: 1 }]]);
  const extra: Row[] = [];
  for (let season = 1; season <= 35; season += 1) {
    const id = seasonIds[season] ?? `jjz-${season}`;
    membership.set(id, { baseKey: '烬九州', season });
    if (existing.has(id)) continue;
    const unit = season >= 32 ? '期' : '季';
    extra.push(row(id, season === 1 ? '烬九州' : `烬九州第${cnNumber(season)}${unit}`));
  }
  return { items: [...base, ...extra], membership };
}

{
  const { items, membership } = fullBatch();
  const result = cards(items, membership);

  check('44 条 → 折叠成 8 张卡', result.length, 8);
  // 顺序 = 各"系列/单剧"首次出现的位置
  check('卡片顺序按各自首次出现的位置', result.map(card => card.representative.title), [
    '烬九州',              // 组，落位 0
    '悍卒！燃烬九州',       // 无归属
    '烬九州AI真人版',       // 无归属
    '烬九州：征尘录第一季', // 独立系列
    '烬九州',              // 无归属（98 集那部）
    '烬九州，风起悍卒',     // 无归属
    '烬九州',              // 无归属（74 集那部）
    '持械入宋：第一季',     // 独立系列
  ]);

  const jinGroup = result[0];
  check('烬九州那一张的代表是第 1 季（裸名，落位 0）', jinGroup.representative.id, BODY);
  check('烬九州 seasonCount = 35', jinGroup.seasonCount, 35);
  check('烬九州 memberCount = 36（35 季 + 同名同季的另一部）', jinGroup.memberCount, 36);

  // (b) 同 base 同 season 的另一部不额外占卡，但计入 memberCount
  check('「烬九州：第二季」没有单独占卡',
    result.some(card => card.representative.id === SAME_NAME_S2), false);
  check('但它被算进烬九州那一组里', jinGroup.memberCount, 36);

  // (c) 持械入宋：只有第 1、9 季，季号不连续 → 不给季数
  const chixie = result[7];
  check('持械入宋折叠成 1 张', chixie.representative.id, CHIXIE_S1);
  check('持械入宋 memberCount = 2', chixie.memberCount, 2);
  check('持械入宋 seasonCount 不给（1 与 9 之间有缺口）', chixie.seasonCount, undefined);

  // (e) 无归属条目：各自成卡、memberCount 1、无季数
  const orphan = result[1];
  check('无归属条目 memberCount = 1', orphan.memberCount, 1);
  check('无归属条目不带季数', orphan.seasonCount, undefined);
  check('两张同名的另一部「烬九州」都保留', result
    .filter(card => card.representative.title === '烬九州')
    .map(card => card.representative.id), [BODY, OTHER_98, OTHER_74]);

  // 展开态（groupSeriesItems）行为不变：仍是 44 条
  check('groupSeriesItems 的展开行为不变（44 条）', groupSeriesItems(items, membership).length, 44);
}

// ── (g) 代表按位置、不按季号（lead 指定的钉子）────────────────────
{
  const items = [
    row(S3, '烬九州第三季'),   // 位置 0
    row(BODY, '烬九州'),       // 位置 1，裸名（季号最小）
  ];
  const membership: MembershipMap = new Map([
    [S3, { baseKey: '烬九州', season: 3 }],
    [BODY, { baseKey: '烬九州', season: 1 }],
  ]);
  const result = cards(items, membership);
  check('一组两张 → 折叠成 1 张', result.length, 1);
  check('代表是位置 0 的第三季，不是季号最小的裸名', result[0].representative.id, S3);
  check('季号 1 与 3 之间有缺口 → 不给季数', result[0].seasonCount, undefined);
}

// ── 补充：聚合补进来的季在下标尾部，抢不到代表位 ─────────────────────
{
  const items = [
    row(S3, '烬九州第三季'),            // 用户搜的那一季，位置 0
    row('o1', '别的剧'),
    row(BODY, '烬九州'),                // 聚合补进来（尾部落位）
    row('jjz-2', '烬九州第二季'),
  ];
  const membership: MembershipMap = new Map([
    [S3, { baseKey: '烬九州', season: 3 }],
    [BODY, { baseKey: '烬九州', season: 1 }],
    ['jjz-2', { baseKey: '烬九州', season: 2 }],
  ]);
  const result = cards(items, membership);
  check('补进来的季不抢代表位', result[0].representative.id, S3);
  check('组内 3 条（季号 1,2,3 连续）', result[0].memberCount, 3);
  check('季号 1..3 连续 → seasonCount = 3', result[0].seasonCount, 3);
}

// ── 补充：单季组与空表 ──────────────────────────────────────────────
{
  check('空表 → 空卡', cards([], new Map()), []);

  const single = cards([row(S2, '烬九州第二季')], new Map());
  check('一条分季标题自成一卡', single.length, 1);
  check('单季组不给季数（共 1 季是噪音）', single[0].seasonCount, undefined);

  const bare = cards([row(BODY, '烬九州')], new Map());
  check('裸名无归属也是一卡', bare.length, 1);
  check('裸名无归属不带季数', bare[0].seasonCount, undefined);
}

// ── 补充：季号有重复但仍连续（重复不算缺口）─────────────────────────
{
  const items = [
    row('a1', '烬九州第一季'),
    row('a2', '烬九州第二季'),
    row('a3', '烬九州：第二季'),   // 同 season 的另一部
    row('a4', '烬九州第三季'),
  ];
  const result = cards(items, new Map());
  check('重复季号不造成额外卡片', result.length, 1);
  check('季号 1,2,2,3 → 不同季号 3 个、最大 3 → seasonCount = 3', result[0].seasonCount, 3);
  check('memberCount 计入重复那条', result[0].memberCount, 4);
}

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exitCode = 1;
