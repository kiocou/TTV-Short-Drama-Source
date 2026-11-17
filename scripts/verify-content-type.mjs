import fs from 'node:fs';
import path from 'node:path';
import { contentTypeFor } from '../src/services/contentType.ts';

/**
 * 内容类型口径验收。
 *
 * 锁两件事：
 *  1. **实现正确**：`comic → 1004`，其余（drama / ai / anime / adult / 未知）→ 1。
 *  2. **没有再散开**：`src/` 下除 `services/contentType.ts` 之外，不允许再出现
 *     裸三元 `=== 'comic' ? 1004`。
 *
 * 第 2 条是重点。这个三元原先散在 17 处，加 AI 剧时**一处都没改**——因为三元
 * 天然有 else，AI 剧静默落进真人剧口径，编译器和类型检查都不会提醒。
 * 收拢成 `contentTypeFor()` 之后，如果谁又手写一份，这条会立刻红。
 */

const ROOT = path.resolve(import.meta.dirname, '..');
const ALLOWED = path.join(ROOT, 'src', 'services', 'contentType.ts');
const NEEDLE = "=== 'comic' ? 1004";

function walk(dir, out = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name === 'dist') continue;
      walk(full, out);
    } else if (/\.(ts|tsx)$/.test(entry.name)) {
      out.push(full);
    }
  }
  return out;
}

const checks = [
  // ---- 实现正确 ----
  ['comic → 1004', contentTypeFor('comic') === 1004],
  ['drama → 1', contentTypeFor('drama') === 1],
  ['ai → 1（现状；原生化先，worker 回退时才用到）', contentTypeFor('ai') === 1],
  ['anime → 1', contentTypeFor('anime') === 1],
  ['adult → 1', contentTypeFor('adult') === 1],
  ['未知频道 → 1', contentTypeFor('nope') === 1],
  ['undefined → 1', contentTypeFor(undefined) === 1],
  ['null → 1', contentTypeFor(null) === 1],
];

// ---- 没有再散开 ----
const offenders = [];
for (const file of walk(path.join(ROOT, 'src'))) {
  if (path.resolve(file) === ALLOWED) continue;
  const text = fs.readFileSync(file, 'utf8');
  if (text.includes(NEEDLE)) {
    offenders.push(path.relative(ROOT, file));
  }
}
checks.push([
  `没有裸三元散落在 contentType.ts 之外（违规文件：${offenders.length ? offenders.join(', ') : '无'}）`,
  offenders.length === 0,
]);

let failed = 0;
for (const [label, ok] of checks) {
  if (ok) {
    console.log(`PASS ${label}`);
  } else {
    failed += 1;
    console.error(`FAIL ${label}`);
  }
}
console.log(`\n${checks.length - failed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);