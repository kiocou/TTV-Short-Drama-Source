// 本地 IPC 桩验证取消只作用于本次订阅；不联网、不启动媒体进程。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';

globalThis.window = { __TAURI_INTERNALS__: {} };
globalThis.localStorage = { getItem: () => 'dark' };
const calls = [];
const pending = [];
globalThis.__cancelInvoke = (command, args) => {
  calls.push({ command, args });
  if (command === 'short_drama_app_resolve' || command === 'short_drama_app_open_stream') {
    return new Promise(resolve => pending.push(resolve));
  }
  return Promise.resolve();
};
const bundled = await build({
  entryPoints: [fileURLToPath(new URL('../src/services/ipc.ts', import.meta.url))],
  bundle: true, platform: 'node', format: 'esm', write: false,
  plugins: [{ name: '取消台架', setup(builder) {
    builder.onResolve({ filter: /^@tauri-apps\/api\/core$/ }, () => ({ path: 'ipc', namespace: 'fixture' }));
    builder.onLoad({ filter: /.*/, namespace: 'fixture' }, () => ({ contents: 'export const invoke = (...args) => globalThis.__cancelInvoke(...args);' }));
  } }],
});
const { ipcService } = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString('base64')}`);
const flush = () => new Promise(resolve => setImmediate(resolve));

test('取消单个同键请求，只发该requestId；另一个订阅仍可完成', async () => {
  const first = new AbortController();
  const second = new AbortController();
  const a = ipcService.playback.resolveNative('7512345678', '7512345679', 1, 'auto', first.signal);
  const b = ipcService.playback.resolveNative('7512345678', '7512345679', 1, 'auto', second.signal);
  const resultA = a.then(() => 'resolved', error => error.name);
  await flush();
  const requests = calls.filter(call => call.command === 'short_drama_app_resolve');
  assert.ok(requests[0].args.input.requestId, '需传递本次订阅requestId');
  assert.notEqual(requests[0].args.input.requestId, requests[1].args.input.requestId);
  first.abort();
  await flush();
  assert.equal(await resultA, 'AbortError');
  const cancels = calls.filter(call => call.command === 'short_drama_app_cancel_request');
  assert.deepEqual(cancels.map(call => call.args.requestId), [requests[0].args.input.requestId]);
  pending[1]({ playUrl: 'fixture.mp4', cached: true, sizeBytes: 12 });
  assert.equal((await b).playUrl, 'fixture.mp4');
  pending[0]({ playUrl: 'late.mp4' });
});

test('已经取消的signal不会再发媒体解析IPC', async () => {
  calls.length = 0;
  const controller = new AbortController();
  controller.abort();
  const attempt = ipcService.playback.resolveNative('7512345678', '7512345679', 1, 'auto', controller.signal);
  void attempt.catch(() => {});
  await flush();
  assert.equal(calls.some(call => call.command === 'short_drama_app_resolve'), false);
  await assert.rejects(attempt, { name: 'AbortError' });
});

test('流式开播也携带订阅id并在会话取消时撤销', async () => {
  calls.length = 0;
  const controller = new AbortController();
  const attempt = ipcService.playback.openStreamNative('7512345678', '7512345679', 105, 1, controller.signal);
  const result = attempt.then(() => 'resolved', error => error.name);
  await flush();
  const request = calls.find(call => call.command === 'short_drama_app_open_stream');
  assert.ok(request.args.input.requestId);
  controller.abort();
  await flush();
  assert.equal(await result, 'AbortError');
  assert.ok(calls.some(call => call.command === 'short_drama_app_cancel_request' && call.args.requestId === request.args.input.requestId));
  pending.at(-1)({ playUrl: 'late.m3u8' });
});

test('实际store换集替换订阅，退出取消当前订阅', async () => {
  const source = ts.createSourceFile('store.tsx', readFileSync(new URL('../src/stores/usePlaybackStore.tsx', import.meta.url), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const declarations = new Map();
  const visit = node => {
    if (ts.isVariableDeclaration(node)) declarations.set(node.name.getText(source), node.initializer);
    ts.forEachChild(node, visit);
  };
  visit(source);
  const stop = new Error('台架在媒体操作之前截停');
  const original = new AbortController();
  const ref = { current: original };
  const context = {
    mediaRequestAbortRef: ref, activeSessionRef: { current: 105 }, AbortController,
    ipcService: { playback: { command: async () => {} } },
    stopTailEndGuard: () => {}, resetStreamingWindow: () => {}, resetStreamingSource: () => {},
    dismissPip: () => { throw stop; }, drainPreparedPool: () => { throw stop; }, useCallback: fn => fn,
  };
  const execute = name => runInNewContext(ts.transpileModule(`(${declarations.get(name).getText(source)})`, {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText, context);
  await assert.rejects(execute('runOpenEpisode')('7512345678', '7512345679'), error => error === stop);
  assert.equal(original.signal.aborted, true, '切集必须取消旧订阅');
  assert.notEqual(ref.current, original);
  assert.equal(ref.current.signal.aborted, false, '新会话使用新订阅');
  assert.throws(() => execute('stopPlayback')(), error => error === stop);
  assert.equal(ref.current.signal.aborted, true, '退出必须取消当前订阅');
});
