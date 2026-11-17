// 横向轨道（隐藏滚动条）的回归锁。
//
// ## 为什么需要它
//
// `ttv-no-scrollbar` 同时写了 `scrollbar-width: none` 和
// `::-webkit-scrollbar{display:none}`，WebView2 下确实彻底不可见。
// 首页定版（AGENTS.md §6）就是"货架形态平铺横滚"，所以横滚本身是对的；
// **问题只在于"看不出能滑"**——如果一条横滚轨道既没有箭头、也没有 snap 提示，
// 用户永远不会发现右边还有内容。
//
// 计划核对（docs/计划核对-2026-11-08.md §三）里登记了四处：
//   HomeShelf / RankingRail / CategoryBar / ExploreView
// 这条脚本做两件事：
//   1. 清单固定：再出现"隐藏滚动条 + 横滚"的新容器就必须显式登记，不能偷偷加；
//   2. 发现入口：每一处都要有箭头（ArrowLeft/ArrowRight/Chevron*）或 snap 提示。
//
// 运行：npm run verify:horizontal-rails

import { readFileSync, readdirSync, statSync } from 'node:fs';
import path from 'node:path';

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

/** 已登记允许出现"隐藏滚动条 + 横滚"的文件。新增必须在这里显式登记。 */
const REGISTERED = [
  'src/components/common/HomeShelf.tsx',
  'src/components/common/RankingRail.tsx',
  'src/components/common/CategoryBar.tsx',
  'src/components/views/ExploreView.tsx',
];

function walk(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue;
      walk(full, out);
    } else if (/\.(tsx|ts)$/.test(entry.name)) {
      out.push(full);
    }
  }
  return out;
}

const files = walk('src');
const hits = [];

for (const file of files) {
  const rel = file.split(path.sep).join('/');
  const text = readFileSync(file, 'utf8');
  const lines = text.split('\n');
  lines.forEach((line, index) => {
    if (!line.includes('overflow-x-auto')) return;
    // 同一处 class 串里同时出现 ttv-no-scrollbar 才算"隐藏滚动条的横滚"
    const window = lines.slice(Math.max(0, index - 3), index + 3).join('\n');
    if (!window.includes('ttv-no-scrollbar')) return;
    hits.push({ rel, line: index + 1, text: line.trim() });
  });
}

const hitFiles = [...new Set(hits.map(h => h.rel))].sort();
const registered = [...REGISTERED].sort();

check('隐藏滚动条的横滚容器数量未擅自增加', hitFiles.length === registered.length, `实际 ${hitFiles.length} 处：${hitFiles.join(', ')}`);

for (const rel of registered) {
  check(`${rel} 仍是隐藏滚动条的横滚（清单有效）`, hitFiles.includes(rel));
}

for (const hit of hits) {
  const text = readFileSync(hit.rel, 'utf8');
  // 发现入口：箭头按钮或 snap 对齐提示
  const hasArrow = /ArrowLeft|ArrowRight|ChevronLeft|ChevronRight/.test(text);
  const hasSnap = /snap-x|snap-mandatory/.test(text);
  check(
    `${hit.rel}:${hit.line} 有可发现的滚动入口（箭头或 snap）`,
    hasArrow || hasSnap,
    '完全看不出能滑 = 用户永远发现不了右侧内容',
  );
}

// ttv-no-scrollbar 的定义本身必须仍然"彻底不可见"，否则上面两条都在描述幻觉
const css = readFileSync('src/index.css', 'utf8');
const noScrollbarBlock = css.slice(css.indexOf('.ttv-no-scrollbar'));
check('ttv-no-scrollbar 同时禁用标准与 WebKit 滚动条', /scrollbar-width:\s*none/.test(noScrollbarBlock) && /::-webkit-scrollbar/.test(noScrollbarBlock));

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exit(1);
