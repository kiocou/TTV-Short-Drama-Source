// 运行：node --experimental-strip-types src/services/seek-progress.test.mjs
// 直接执行生产 timeupdate 处理器，回放黄果会话 105 的连续点击，不访问网络或媒体源。
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';
import { flushSeek, requestSeek, resetSeekCoalesce } from './seekPlan.ts';

const source = ts.createSourceFile(
  'usePlaybackStore.tsx',
  readFileSync(new URL('../stores/usePlaybackStore.tsx', import.meta.url), 'utf8'),
  ts.ScriptTarget.Latest,
  true,
  ts.ScriptKind.TSX,
);
let handler;
function visit(node) {
  if (ts.isVariableDeclaration(node) && node.name.getText(source) === 'handleTimeUpdate') {
    handler = node.initializer.getText(source);
  }
  ts.forEachChild(node, visit);
}
visit(source);
assert.ok(handler, '找不到生产 timeupdate 处理器');

const video = { currentTime: 21.49, duration: 292.9, dataset: { sessionId: '105' } };
const seekCoalesceRef = { current: resetSeekCoalesce() };
const seekWaitTargetRef = { current: null };
const activeSessionRef = { current: 105 };
let position = 0;
let savedPosition = 0;
const onTimeUpdate = runInNewContext(`(${handler})`, {
  video,
  seekCoalesceRef,
  seekWaitTargetRef,
  activeSessionRef,
  staleTickNotedRef: { current: new Set() },
  tailProbeRef: { current: null },
  firstTickAt: 0,
  lastTickTraceAt: 1000,
  performance: { now: () => 1000 },
  isStaleElementSession: (element, session) => Number(element.dataset.sessionId) !== session,
  usableDuration: element => element.duration,
  tracePlayback: () => {},
  setPosition: value => { position = value; },
  setDuration: () => {},
  saveProgressThrottledRef: { current: value => { savedPosition = value; } },
});

seekCoalesceRef.current = requestSeek(seekCoalesceRef.current, 20.8, 1000).state;
seekCoalesceRef.current = requestSeek(seekCoalesceRef.current, 32.1, 1628).state;
onTimeUpdate();
assert.equal(position, 32.1, '第二次点击后，旧媒体时钟不能把进度条拉回 21.49 秒');
assert.equal(savedPosition, 32.1, '历史进度须与界面待跳转目标一致');

seekCoalesceRef.current = requestSeek(seekCoalesceRef.current, 40.1, 2518).state;
onTimeUpdate();
assert.equal(position, 40.1, '连续点击期间必须保持最新目标');

seekCoalesceRef.current = requestSeek(seekCoalesceRef.current, 0, 2600).state;
onTimeUpdate();
assert.equal(position, 0, '点击片头的零秒目标不能被当成空值');

seekWaitTargetRef.current = 90;
onTimeUpdate();
assert.equal(position, 90, '等转码时仍保留原有的目标保护');

seekWaitTargetRef.current = null;
const flushed = flushSeek(seekCoalesceRef.current, 3800);
assert.equal(flushed.kind, 'dispatch');
seekCoalesceRef.current = flushed.state;
video.currentTime = 0.2;
onTimeUpdate();
assert.equal(position, 0.2, '落位后读数恢复跟随真实时钟');

activeSessionRef.current = 106;
video.currentTime = 50;
onTimeUpdate();
assert.equal(position, 0.2, '旧会话的迟到事件仍须丢弃');
console.log('通过：连续点击、历史进度、最新目标、零秒、转码等待、落位恢复和旧会话隔离');
