// 执行生产预热回调，禁止黄果/动漫/guo进入红果签名或预转；不联网。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';
import { isHongguoSeriesId } from '../src/services/preferredPlaybackLogic.ts';

const callbackTexts = [];
for (const file of ['DetailView.tsx', 'ExploreView.tsx']) {
  const source = ts.createSourceFile(file, readFileSync(new URL(`../src/components/views/${file}`, import.meta.url), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const visit = node => {
    if (ts.isCallExpression(node) && node.expression.getText(source) === 'useEffect') {
      const callback = node.arguments[0]?.getText(source);
      if (callback && /prefetchStream\(|\.prewarmStreamNative\(/.test(callback)) callbackTexts.push({ file, callback });
    }
    if (ts.isVariableDeclaration(node) && node.name.getText(source) === 'schedulePrewarm') {
      callbackTexts.push({ file, callback: node.initializer.getText(source) });
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
}

test('详情首开、悬停、HLS预转与首页续播四个入口均按来源隔离', () => {
  assert.equal(callbackTexts.length, 4);
  for (const [seriesId, type] of [['hg:6875', 'drama'], ['guo:42', 'drama'], ['dmghg:42', 'anime'], ['12345678', 'anime'], ['7512345678', 'drama']]) {
    const allowed = seriesId === '7512345678';
    const detail = { id: seriesId, type, episodes: [{ id: '123456789012', episodeNumber: 1 }] };
    for (const { file, callback } of callbackTexts) {
      let requested = 0;
      const forbidden = () => {
        assert.ok(allowed, `${file} 不得为 ${seriesId} 触发红果预热`);
        requested += 1;
        return Promise.resolve();
      };
      const context = {
        detail, continueWatching: { seriesId, episodeId: '123456789012', channel: type },
        currentView: 'explore', currentViewRef: { current: file === 'DetailView.tsx' ? 'detail' : 'explore' },
        playingEpisodeIdRef: { current: null }, prewarmedStreamRef: { current: new Set() }, warmResumeRef: { current: null },
        hoverTimerRef: { current: null }, prewarmEpisodeRef: { current: forbidden }, prewarmEpisode: forbidden,
        isHongguoSeriesId, contentTypeFor: () => 1, NEXT_EPISODE_PREWARM_DELAY_MS: 9000,
        ipcService: { playback: { prefetchStream: forbidden, prewarmStreamNative: forbidden }, series: { getDetail: async () => detail } },
        window: { setTimeout: callback => { callback(); return 1; }, clearTimeout: () => {} },
        setTimeout: callback => { callback(); return 1; }, clearTimeout: () => {},
      };
      const js = ts.transpileModule(`(${callback})`, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
      runInNewContext(js, context)('123456789012');
      assert.equal(requested > 0, allowed, `${file} 应保留红果正常预热`);
    }
  }
});
