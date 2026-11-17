// 横滚轨道两端箭头显隐的回归锁。
//
// ## 守的是什么
//
// 用户报：「到顶部了还是不消失」——首页货架每一栏的左右两颗翻页箭头，滚到
// 最左端后左箭头还亮着。根因是判据写的 `scrollLeft > 2`，而轨道带 `px-1`
// （4px 内边距）：`snap-start` 的吸附点落在首张卡的 border box 上，滚动停住的
// 合法位置是 `scrollLeft = 4` 而不是 0，`4 > 2` 恒成立。
//
// 所以判定口径改成「该侧内边距 + 平滑冗余」，两端各用自己那一侧。这个脚本把
// 那套口径铺成矩阵验证，包括吸附落点、末卡吸附、无溢出、无内边距轨道。
//
// 运行：npm run verify:scroll-edges

import { measureScrollEdges, readEdgeTolerance } from '../src/services/scrollEdges.ts';

let pass = 0;
let fail = 0;

function check(name, ok, detail = '') {
  if (ok) {
    pass += 1;
    console.log(`PASS ${name}`);
  } else {
    fail += 1;
    console.log(`FAIL ${name}${detail ? ` :: ${detail}` : ''}`);
  }
}

/** 假的滚动容器：只提供判定用到的三个数。 */
function rail({ scrollLeft, scrollWidth, clientWidth }) {
  return { scrollLeft, scrollWidth, clientWidth };
}

/** 假的内边距：两端各给一个值。 */
const padding = (left, right) => {
  globalThis.window = {
    getComputedStyle: () => ({ paddingLeft: `${left}px`, paddingRight: `${right}px` }),
  };
  return { start: left + 2, end: right + 2 };
};

const same = (a, b) => a.start === b.start && a.end === b.end;
const show = e => `{start:${e.start}, end:${e.end}}`;

/* ---------------- 1. 货架：px-1（4px），吸附停在 4 ---------------- */

const shelfTolerance = padding(4, 4);

check(
  '货架吸附到首卡（scrollLeft=4）时左箭头收起',
  same(measureScrollEdges(rail({ scrollLeft: 4, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance), {
    start: false,
    end: true,
  }),
  show(measureScrollEdges(rail({ scrollLeft: 4, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance)),
);

check(
  '货架滚到首卡之前（scrollLeft=0）左箭头也收起',
  same(measureScrollEdges(rail({ scrollLeft: 0, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance), {
    start: false,
    end: true,
  }),
);

check(
  '货架滚到中段时两端都展开',
  same(measureScrollEdges(rail({ scrollLeft: 900, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance), {
    start: true,
    end: true,
  }),
);

check(
  '货架滚到最后一张（scrollLeft=max）时右箭头收起',
  same(measureScrollEdges(rail({ scrollLeft: 1500, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance), {
    start: true,
    end: false,
  }),
);

check(
  '货架末卡吸附（少 4px）右箭头仍收起',
  same(measureScrollEdges(rail({ scrollLeft: 1496, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance), {
    start: true,
    end: false,
  }),
);

check(
  '亚像素残值（scrollLeft=5.5）不把左箭头重新点亮',
  same(measureScrollEdges(rail({ scrollLeft: 5.5, scrollWidth: 2400, clientWidth: 900 }), shelfTolerance), {
    start: false,
    end: true,
  }),
);

/* ---------------- 2. 题材条：无内边距，两端都该按真实滚动量判 ---------------- */

const barTolerance = padding(0, 0);

check(
  '题材条贴左（scrollLeft=0）左箭头收起',
  same(measureScrollEdges(rail({ scrollLeft: 0, scrollWidth: 1800, clientWidth: 900 }), barTolerance), {
    start: false,
    end: true,
  }),
);

check(
  '题材条滚出一段后左箭头展开',
  same(measureScrollEdges(rail({ scrollLeft: 120, scrollWidth: 1800, clientWidth: 900 }), barTolerance), {
    start: true,
    end: true,
  }),
);

check(
  '题材条贴右（scrollLeft=max）右箭头收起',
  same(measureScrollEdges(rail({ scrollLeft: 900, scrollWidth: 1800, clientWidth: 900 }), barTolerance), {
    start: true,
    end: false,
  }),
);

/* ---------------- 3. 不需要横滚时，两端都不出现 ---------------- */

check(
  '内容不溢出时两端都不出现',
  same(measureScrollEdges(rail({ scrollLeft: 0, scrollWidth: 860, clientWidth: 900 }), shelfTolerance), {
    start: false,
    end: false,
  }),
);

check(
  '1px 亚像素溢出不算溢出（否则箭头会闪）',
  same(measureScrollEdges(rail({ scrollLeft: 0, scrollWidth: 901, clientWidth: 900 }), shelfTolerance), {
    start: false,
    end: false,
  }),
);

/* ---------------- 4. 容差来源必须读得到内边距 ---------------- */

globalThis.window = {
  getComputedStyle: () => ({ paddingLeft: '16px', paddingRight: '0px' }),
};
const asymmetric = readEdgeTolerance({});
check(
  '不对称内边距（pl-4 pr-0）两端各算各的',
  same({ start: asymmetric.start === 18, end: asymmetric.end === 2 }, { start: true, end: true }),
  `实际 {start:${asymmetric.start}, end:${asymmetric.end}}`,
);

check('容器为空时返回 null（调用方据此不清状态）', measureScrollEdges(null) === null);

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exit(1);