/**
 * 「搜索页同系列聚块」的探针（由 `verify-series-grouping.mjs` 打包执行）。
 *
 * 断言的是用户直接能看见的那件事：搜《烬九州》时，它的各季必须**连续挨着**，
 * 组内从第 1 季往上排；而**别的剧一条都不能被排动**。
 *
 * 两个最容易错的地方：
 * 1. 裸名第 1 季（季表 index=1 的标题就是「烬九州」）只能靠季表回填的归属认回来，
 *    纯按标题解析会把它当成"别的剧"留在块外；
 * 2. 「烬九州：第二季」(74 集) 与「烬九州第二季」(100 集) 是同 base 同 season 的
 *    **两部不同剧**，必须都留着、且相对顺序稳定（不能让列表在两次渲染之间翻转）。
 */
import { groupSeriesItems, mergeMembership } from '../src/services/seriesGrouping';
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

function membershipOf(entries: [string, string, number][]): MembershipMap {
  const map: MembershipMap = new Map();
  for (const [id, baseKey, season] of entries) map.set(id, { baseKey, season });
  return map;
}

const titles = (items: Row[]): string[] => items.map(entry => entry.title);

// ── (a) 裸名第 1 季靠 membership 归组（纯标题解析认不出它）──────────
{
  const bare = row('X', '烬九州');       // 裸名：parseSeasonLabel → null
  const s2 = row('Y', '烬九州第二季');
  const membership = membershipOf([
    ['X', '烬九州', 1],
    ['Y', '烬九州', 2],
  ]);
  check('裸名 + 第2季 → 仍是一个组', titles(groupSeriesItems([bare, s2], membership)),
    ['烬九州', '烬九州第二季']);
  check('裸名排在第2季之前（season 1 < 2）',
    titles(groupSeriesItems([s2, bare], membership)), ['烬九州', '烬九州第二季']);
  // 没有归属时，裸名解析不出季号 → 留在原位当"别的剧"（这正是要防的那种观感）。
  check('没有归属时裸名不被并组', titles(groupSeriesItems([bare, s2], new Map())),
    ['烬九州', '烬九州第二季']);
}

// ── (b) 分散的三个季：组落位 = 最小下标，组内按季号升序 ─────────────
{
  const items = [
    row('s3', '烬九州第三季'),   // 位置 0
    row('o1', '别的剧'),
    row('s1', '烬九州第一季'),   // 位置 2
    row('o2', '另一部剧'),
    row('o3', '第三部剧'),
    row('s2', '烬九州第二季'),   // 位置 5
  ];
  check('三个季聚到位置 0，组内 1,2,3', titles(groupSeriesItems(items, new Map())),
    ['烬九州第一季', '烬九州第二季', '烬九州第三季', '别的剧', '另一部剧', '第三部剧']);
}

// ── (c) 两个不同系列交错：各自聚块，块序按各自最小下标 ─────────────
{
  const items = [
    row('a1', '甲剧第一季'),
    row('b2', '乙剧第二季'),
    row('a2', '甲剧第二季'),
    row('b1', '乙剧第一季'),
  ];
  check('交错的两个系列各自聚块', titles(groupSeriesItems(items, new Map())),
    ['甲剧第一季', '甲剧第二季', '乙剧第一季', '乙剧第二季']);
}

// ── (d) 无归属条目相对顺序不变 ─────────────────────────────────────
{
  const items = [
    row('o1', '其他剧1'),
    row('s2', '烬九州第二季'),
    row('o2', '其他剧2'),
    row('s1', '烬九州第一季'),
    row('o3', '其他剧3'),
  ];
  check('其他剧的相对顺序不动，只把两个季聚到组落位',
    titles(groupSeriesItems(items, new Map())),
    ['其他剧1', '烬九州第一季', '烬九州第二季', '其他剧2', '其他剧3']);
}

// ── (e) 「季/期」同组、「部」独立 ──────────────────────────────────
{
  const same = [row('p35', '烬九州第三十五期'), row('s2', '烬九州第二季')];
  check('「期」与「季」同组，组内按季号升序',
    titles(groupSeriesItems(same, new Map())),
    ['烬九州第二季', '烬九州第三十五期']);

  // 区分力说明：若「部」被并进「季」组，组内按季号升序会输出 [第二季, 第三部]；
  // 各自成组时块序跟着落位走，仍是输入顺序 [第三部, 第二季]。
  const volume = [row('v3', '某剧第三部'), row('s2', '某剧第二季')];
  check('「部」是另一套编号，不与「季」并组（各自成块，块序按落位）',
    titles(groupSeriesItems(volume, new Map())),
    ['某剧第三部', '某剧第二季']);
  check('反过来也各自成块', titles(groupSeriesItems([...volume].reverse(), new Map())),
    ['某剧第二季', '某剧第三部']);
}

// ── (f) 空数组 / 全无归属 → 原样返回（同一个引用）───────────────────
{
  const empty: Row[] = [];
  check('空数组原样返回', groupSeriesItems(empty, new Map()) === empty, true);

  const plain = [row('o1', '别的剧'), row('o2', '又来一部')];
  check('全无归属原样返回', groupSeriesItems(plain, new Map()) === plain, true);
}

// ── (g) 稳定排序：同 season 两条保持原相对顺序 ─────────────────────
{
  const items = [
    row('dupA', '烬九州：第二季'),
    row('dupB', '烬九州第二季'),
  ];
  check('同 season 两条保持原顺序', titles(groupSeriesItems(items, new Map())),
    ['烬九州：第二季', '烬九州第二季']);
  check('换过来也仍按原顺序（不翻转）', titles(groupSeriesItems([...items].reverse(), new Map())),
    ['烬九州第二季', '烬九州：第二季']);
  check('两条都保留（不去重）', groupSeriesItems(items, new Map()).length, 2);
}

// ── (h) mergeMembership：并入且不覆盖已有归属 ──────────────────────
{
  const base = membershipOf([['X', '烬九州', 1]]);
  const merged = mergeMembership(base, membershipOf([
    ['Y', '烬九州', 2],
    ['X', '错误的系列', 9],
  ]));
  check('新材料并入', merged.get('Y'), { baseKey: '烬九州', season: 2 });
  check('已有归属不被覆盖（原地合并，返回同一引用）', merged === base, true);
  check('已有归属的内容不变', merged.get('X'), { baseKey: '烬九州', season: 1 });
  check('条目数', merged.size, 2);
}

// ── (i) 真实数据：《烬九州》App 搜索 13 条 + 季表聚合后的最终顺序 ────
{
  /** 季表 1..35 的中文数字标题（index=1 是裸名 —— 站方真实写法）。 */
  const cn = ['一', '二', '三', '四', '五', '六', '七', '八', '九', '十'];
  const cnNumber = (n: number): string => {
    if (n <= 10) return cn[n - 1];
    if (n < 20) return `十${cn[n - 11]}`;
    const tens = Math.floor(n / 10);
    const ones = n % 10;
    return `${cn[tens - 1]}十${ones === 0 ? '' : cn[ones - 1]}`;
  };

  const BODY = '7647789981687106622';
  // 首屏 13 条（接口返回顺序，lead 实测）
  const feed: Row[] = [
    row(BODY, '烬九州'),                       // 裸名 = 季表第 1 季
    row('7652205899469048894', '烬九州第三季'),
    row('7651961603817999384', '烬九州第二季'),
    row('7655548765004123160', '悍卒！燃烬九州'),
    row('7654433962474884158', '烬九州AI真人版'),
    row('7657786086797610008', '烬九州第四季'),
    row('7658590108685175832', '烬九州：第二季'),
    row('7658214394211339288', '烬九州：征尘录第一季'),
    row('7654579310786776089', '烬九州'),      // 同名另一部（98 集）
    row('7659763666417355800', '烬九州，风起悍卒'),
    row('7648478038362967065', '烬九州'),      // 同名另一部（74 集）
    row('7671966197985856536', '持械入宋：第一季'),
    row('7694939439503510590', '持械入宋：第九季'),
  ];

  // 季表聚合补进来的季（35 条里去掉首屏已有的那些 id）
  const existing = new Set(feed.map(entry => entry.id));
  const seasonIds: Record<number, string> = {
    1: BODY,
    2: '7651961603817999384',
    3: '7652205899469048894',
    4: '7657786086797610008',
  };
  const extra: Row[] = [];
  const membership = membershipOf([[BODY, '烬九州', 1]]);
  for (let season = 1; season <= 35; season += 1) {
    const id = seasonIds[season] ?? `jjz-${season}`;
    membership.set(id, { baseKey: '烬九州', season });
    if (existing.has(id)) continue;
    const unit = season >= 32 ? '期' : '季';
    extra.push(row(id, season === 1 ? '烬九州' : `烬九州第${cnNumber(season)}${unit}`));
  }

  const merged = [...feed, ...extra.filter(entry => !existing.has(entry.id))];
  const grouped = groupSeriesItems(merged, membership);
  const shown = titles(grouped);

  check('总条数 = 36 条组内（35 季 + 同 base 同 season 的另一部）+ 5 部无关剧 + 征尘录 1 + 持械入宋 2',
    grouped.length, 44);
  check('前 12 个 title', shown.slice(0, 12), [
    '烬九州',
    '烬九州第二季',
    '烬九州：第二季',
    '烬九州第三季',
    '烬九州第四季',
    '烬九州第五季',
    '烬九州第六季',
    '烬九州第七季',
    '烬九州第八季',
    '烬九州第九季',
    '烬九州第十季',
    '烬九州第十一季',
  ]);
  check('第 1 季（裸名）在组首、没有飘到块外', shown[0], '烬九州');
  // 同 season 的两条（第二季 / ：第二季）都留在组里、按原相对顺序挨着，不去重。
  check('同 base 同 season 的两条都保留', shown.slice(1, 3), ['烬九州第二季', '烬九州：第二季']);
  check('第 32 期仍在组内（紧接第 31 季之后）', shown.slice(31, 36), [
    '烬九州第三十一季',
    '烬九州第三十二期',
    '烬九州第三十三期',
    '烬九州第三十四期',
    '烬九州第三十五期',
  ]);
  check('组后依次是无关剧（相对顺序与原列表一致）', shown.slice(36), [
    '悍卒！燃烬九州',
    '烬九州AI真人版',
    '烬九州：征尘录第一季',
    '烬九州',
    '烬九州，风起悍卒',
    '烬九州',
    '持械入宋：第一季',
    '持械入宋：第九季',
  ]);
  // 三张同名裸名里只有本体进了组：另外两张仍在原位，且没被季表覆盖。
  check('两张同名的另一部「烬九州」没进组', grouped
    .slice(39, 42)
    .filter(entry => entry.title === '烬九州').length, 2);
}

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exitCode = 1;
