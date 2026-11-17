// 用 esbuild 把 DOM 集成用例打成一个浏览器可直接加载的 IIFE。
// 唯一的模块替换：`services/ipc` → `fakeIpc.ts`（被测源码一字不改）。
import { build } from 'esbuild';
import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
export const repoRoot = path.resolve(here, '../../..');
export const outDir = path.join(repoRoot, 'src-tauri', 'target', 'danmaku-verify');

export async function buildBundle() {
  mkdirSync(outDir, { recursive: true });
  await build({
    entryPoints: [path.join(here, 'entry.tsx')],
    outfile: path.join(outDir, 'entry.js'),
    bundle: true,
    format: 'iife',
    platform: 'browser',
    target: ['chrome110'],
    jsx: 'automatic',
    legalComments: 'none',
    define: { 'process.env.NODE_ENV': '"production"' },
    plugins: [
      {
        name: 'stub-ipc',
        setup(b) {
          b.onResolve({ filter: /services\/ipc(\.ts)?$/ }, args => {
            const base = path.basename(args.path);
            if (base !== 'ipc' && base !== 'ipc.ts') return null;
            return { path: path.join(here, 'fakeIpc.ts') };
          });
        },
      },
    ],
  });
  writeFileSync(
    path.join(outDir, 'index.html'),
    '<!doctype html><html><head><meta charset="utf-8"><title>danmaku dom it</title>' +
      '<style>html,body{margin:0;background:#111}</style></head>' +
      '<body><div id="root"></div><script src="./entry.js"></script></body></html>',
    'utf8',
  );
  return outDir;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const dir = await buildBundle();
  console.log('bundle 完成：' + dir);
}