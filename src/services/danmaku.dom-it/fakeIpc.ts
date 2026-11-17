/**
 * 集成用例的假 `ipcService`：只在 DOM 集成用例的 bundle 里替换 `services/ipc`。
 *
 * 为什么不直接 mock `ipcService`：`DanmakuOverlay` 是**静态** import 它，浏览器里
 * 只能靠打包期把模块换掉（`build.mjs` 的 onResolve 插件）。这样被测的是**同一份**
 * `DanmakuOverlay` / `danmaku.ts` 源码，只有取数这一端被替换。
 *
 * 数据全部由用例在页面里写进 `window.__danmakuFake`；没有真实网络、没有账号。
 */

export interface FakeItem {
  id: string;
  text: string;
  timeMs: number;
}

export interface FakeEpisode {
  items: FakeItem[];
  /** 本集每次取页的人为延迟（毫秒）。 */
  latencyMs: number;
}

export interface FakeCall {
  seriesId: string;
  episodeId: string;
  startMs: number;
  durationMs: number;
  at: number;
}

export interface FakeCfg {
  episodes: Record<string, FakeEpisode>;
  calls: FakeCall[];
}

function cfg(): FakeCfg {
  const g = globalThis as unknown as { __danmakuFake?: FakeCfg };
  if (!g.__danmakuFake) g.__danmakuFake = { episodes: {}, calls: [] };
  return g.__danmakuFake;
}

function wait(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}

export const ipcService = {
  playback: {
    async danmaku(
      seriesId: string,
      vid: string,
      startMs: number,
      durationMs: number,
    ): Promise<{
      episodeId: string;
      startMs: number;
      nextMs: number;
      total: number;
      items: FakeItem[];
    }> {
      const state = cfg();
      state.calls.push({ seriesId, episodeId: vid, startMs, durationMs, at: performance.now() });
      const episode = state.episodes[vid] ?? { items: [], latencyMs: 0 };
      await wait(episode.latencyMs ?? 0);
      const items = (episode.items ?? []).filter(
        item => item.timeMs >= startMs && item.timeMs < startMs + 30_000,
      );
      return { episodeId: vid, startMs, nextMs: startMs + 30_000, total: items.length, items };
    },
  },
};