// 运行：node src/services/huangguo-playback-routing.test.mjs
// 执行生产分流条件和预取函数，用拒绝桩保证黄果不会请求红果解析。
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';

const source = ts.createSourceFile('usePlaybackStore.tsx',
  readFileSync(new URL('../stores/usePlaybackStore.tsx', import.meta.url), 'utf8'),
  ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const declarations = new Map();
function visit(node) {
  if (ts.isVariableDeclaration(node)) declarations.set(node.name.getText(source), node.initializer);
  ts.forEachChild(node, visit);
}
visit(source);

function execute(text, context) {
  const js = ts.transpileModule(`(${text})`, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  }).outputText;
  return runInNewContext(js, context);
}

let directBranch;
function findDirectBranch(node) {
  if (!directBranch && ts.isIfStatement(node) && node.expression.getText(source).includes("detail.type === 'anime'")) {
    directBranch = node;
  }
  ts.forEachChild(node, findDirectBranch);
}
findDirectBranch(declarations.get('runOpenEpisode'));
assert.ok(directBranch, '找不到通用媒体地址挂载分支');
const selectsDirectBranch = (seriesId, type) => execute(directBranch.expression.getText(source), {
  seriesId, detail: { type },
});
assert.equal(selectsDirectBranch('hg:6875', 'drama'), true, '黄果应直接打开通用媒体会话');
assert.equal(selectsDirectBranch('dmghg:test', 'anime'), true, '动漫继续走原有直挂链路');
assert.equal(selectsDirectBranch('7703500187427146814', 'drama'), false, '红果仍走自身解析链路');
assert.equal(selectsDirectBranch('guo:test', 'drama'), false, 'guo 保留已有专有链路');
assert.match(directBranch.thenStatement.getText(source), /ipcService\.playback\.open\(/);
assert.doesNotMatch(directBranch.thenStatement.getText(source), /startNativeResolve\(/);

const forbidden = () => assert.fail('黄果不应进入红果预取链路');
const context = {
  useCallback: fn => fn,
  currentSeries: null,
  currentQuality: 'auto',
  vsrEnabled: false,
  vsrEnabledRef: { current: false },
  contentTypeFor: () => 1,
  drainPreparedPool: forbidden,
  prefetchDanmaku: forbidden,
  scheduleNextEpisodePrewarmRef: { current: forbidden },
  streamingInFlight: () => false,
  handlerCtxRef: { current: { currentQuality: 'auto' } },
  prewarmedNextRef: { current: '' },
  PREWARM_RETRIES: 0,
  ipcService: { playback: { resolveNative: forbidden, prewarmStreamNative: forbidden } },
  tracePlayback: () => {},
};
const series = { id: 'hg:6875', type: 'drama', episodes: [{ id: '1' }, { id: '2' }] };
execute(declarations.get('warmAdjacentEpisodes').getText(source), context)(series, '1', 105);
execute(declarations.get('prewarmEpisode').getText(source), context)(series.id, '1', 1);
execute(declarations.get('prewarmNextEpisode').getText(source), context)(series, '1', 1);
console.log('通过：黄果通用直挂、动漫和既有源路由保持、三处红果预取隔离');
