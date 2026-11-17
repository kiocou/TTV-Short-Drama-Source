/**
 * DOM 集成用例的页面入口：**真挂载** `DanmakuOverlay`（真 React 渲染器、真 DOM、
 * 真 ResizeObserver / rAF），由 `run.mjs` 通过 CDP 驱动。
 *
 * 只有两处是假的：
 *  - 取数：`services/ipc` 在打包期换成 `fakeIpc.ts`（见 `build.mjs`）；
 *  - 媒体：`videoRef.current` 指向 `{ currentTime, duration }` 普通对象 ——
 *    组件只读这两个字段（`DanmakuOverlay` 里没有任何 `<video>` API）。
 * 以及 canvas 的 `measureText` 被换成**确定性**字宽（按字号/粗体/字体族线性换算），
 * 否则不同机器的字体度量会让「换字体是否重排」这类断言无法复现。
 *
 * 页面只暴露数据与等待原语，PASS/FAIL 判定都在 `run.mjs`。
 */

import { DanmakuLoader } from '../danmaku';
import { createRoot } from 'react-dom/client';
import React from 'react';
import { DanmakuOverlay } from '../../components/player/DanmakuOverlay';
import type { FakeCfg } from './fakeIpc';

/** 与 `fakeIpc.ts` 共享的同一份状态（懒创建，避免模块求值顺序问题）。 */
function fakeState(): FakeCfg {
  const g = globalThis as unknown as { __danmakuFake?: FakeCfg };
  if (!g.__danmakuFake) g.__danmakuFake = { episodes: {}, calls: [] };
  return g.__danmakuFake;
}

// ── 确定性字宽 ────────────────────────────────────────────────────────
// 组件按 `${bold?'bold ':''}${size}px "${font}"…` 设置 ctx.font，这里解析它：
// 字宽 = 字符数 × 10 × 字体系数(A=1,B=2) × 粗体系数(1.5) × 字号/20。
//
// ⚠️ 必须用「字号之后的第一段」判字体：浏览器会把 `ctx.font` **规范化**
// （实测 `20px "A", "X", sans-serif` 读回来时引号被去掉），按引号正则取族名会
// 全部落空、A/B 宽度一模一样。
const FONT_FACTOR: Record<string, number> = { A: 1, B: 2 };

function familyOf(font: string): string {
  const size = /(\d+)px/.exec(font);
  if (!size) return '';
  const rest = font.slice(font.indexOf(size[0]) + size[0].length).trim();
  return (rest.split(',')[0] ?? '').trim().replace(/^["']|["']$/g, '').trim();
}

(CanvasRenderingContext2D.prototype as unknown as { measureText: (t: string) => { width: number } }).measureText =
  function measureText(this: CanvasRenderingContext2D, text: string) {
    const font = this.font || '';
    const size = Number(/(\d+)px/.exec(font)?.[1] ?? 20);
    const bold = /\bbold\b/.test(font) ? 1.5 : 1;
    const factor = (FONT_FACTOR[familyOf(font)] ?? 1) * bold * (size / 20);
    return { width: text.length * 10 * factor };
  };

// ── 受控场景 ──────────────────────────────────────────────────────────

const props = {
  seriesId: 's1',
  episodeId: 'e0', // 启动时先不挂任何集：避免组件在用例写入假数据之前就取数
  enabled: false,
  font: 'A',
  bold: false,
  fontScale: 100,
  speed: 1,
  /** 装载期起播点（毫秒）；null = 用媒体时钟（等价 `danmakuLoadingStartMs` 返回 null）。 */
  openStartMs: null as number | null,
  /**
   * 等价 store 的 `position`（毫秒）。真实 `VideoSurface` 传的就是它：播放中开弹幕
   * 时这里就是当前播放头，换集那批被 store 归零。
   */
  positionMs: 0,
};

const video = { currentTime: 0, duration: 90 };
const videoRef = { current: video };
const stage = { width: 400, height: 328, ref: null as HTMLDivElement | null };
const clock = { on: false, last: 0 };

let root: ReturnType<typeof createRoot> | null = null;

function Scene() {
  return (
    <div
      ref={el => {
        stage.ref = el;
      }}
      style={{
        position: 'relative',
        width: `${stage.width}px`,
        height: `${stage.height}px`,
        overflow: 'hidden',
        background: '#000',
      }}
    >
      <DanmakuOverlay
        videoRef={videoRef as unknown as React.RefObject<HTMLVideoElement | null>}
        seriesId={props.seriesId}
        episodeId={props.episodeId}
        enabled={props.enabled}
        positionMs={props.positionMs}
        openStartMs={props.openStartMs}
        appearance={{
          danmakuArea: 25,
          danmakuOpacity: 100,
          danmakuFontScale: props.fontScale,
          danmakuSpeed: props.speed,
          danmakuFont: props.font,
          danmakuColor: '#FFFFFF',
          danmakuBold: props.bold,
          danmakuStroke: 'heavy',
        }}
      />
    </div>
  );
}

function render(): void {
  root?.render(<Scene />);
}

// 媒体时钟：只在用例要求时推进（每条用例自己决定是"真在播"还是"手动定格"）。
function loop(ts: number): void {
  if (clock.on) {
    if (clock.last === 0) clock.last = ts;
    const dt = Math.min(250, ts - clock.last);
    clock.last = ts;
    video.currentTime += dt / 1000;
  }
  requestAnimationFrame(loop);
}

function layer(): HTMLElement | null {
  return (stage.ref?.firstElementChild as HTMLElement | null) ?? null;
}

interface NodeView {
  text: string;
  opacity: string;
  x: number | null;
  y: number | null;
  fontSize: string;
  fontWeight: string;
}

function nodes(): NodeView[] {
  const container = layer();
  if (!container) return [];
  return Array.from(container.children).map(child => {
    const el = child as HTMLElement;
    const m = /translate3d\(([-\d.]+)px,\s*([-\d.]+)px/.exec(el.style.transform || '');
    return {
      text: el.textContent ?? '',
      opacity: el.style.opacity,
      x: m ? Number(m[1]) : null,
      y: m ? Number(m[2]) : null,
      fontSize: el.style.fontSize,
      fontWeight: el.style.fontWeight,
    };
  });
}

/** 行高与轨道数：与组件同一公式，用于断言「rows 没变」。 */
function rowsHint(): { layerHeight: number; rowHeight: number; rows: number } {
  const container = layer();
  const height = container ? container.getBoundingClientRect().height : 0;
  const fontSize = Math.round(20 * (props.fontScale / 100));
  const rowHeight = Math.max(24, Math.round(fontSize * 1.5));
  const rows = Math.min(12, Math.max(1, Math.floor((height - 10) / rowHeight) || 1));
  return { layerHeight: height, rowHeight, rows };
}

/**
 * 由 transform 反推这条弹幕排程时的 `width`（`x = W - (W + fw) * elapsed/lifetime`）。
 * 这是「换字体后 flight.width 是否真的换了」的直接证据 —— 没重排就拿到旧值。
 */
function flightWidth(text: string): number | null {
  const container = layer();
  if (!container) return null;
  const width = container.getBoundingClientRect().width;
  const item = (fakeState().episodes[props.episodeId]?.items ?? []).find(i => i.text === text);
  if (!item) return null;
  const node = nodes().find(n => n.text === text);
  if (!node || node.x == null) return null;
  const elapsed = video.currentTime * 1000 - item.timeMs;
  const lifetime = 8000;
  const progress = elapsed / lifetime;
  if (!(progress > 0 && progress < 1)) return null;
  return (width - node.x) / progress - width;
}

function frames(count: number): Promise<void> {
  return new Promise(resolve => {
    let left = count;
    const step = (): void => {
      left -= 1;
      if (left <= 0) resolve();
      else requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  });
}

async function waitFor(predicate: () => boolean, ms: number): Promise<{ ok: boolean; waited: number }> {
  const started = performance.now();
  for (;;) {
    if (predicate()) return { ok: true, waited: Math.round(performance.now() - started) };
    if (performance.now() - started > ms) return { ok: false, waited: Math.round(performance.now() - started) };
    await frames(1);
  }
}

const visibleCount = (): number => nodes().filter(n => n.opacity === '1').length;
const visibleTexts = (): string[] => nodes().filter(n => n.opacity === '1').map(n => n.text);

(globalThis as unknown as { __it: unknown }).__it = {
  boot(container: HTMLElement) {
    root = createRoot(container);
    render();
    clock.last = 0;
    requestAnimationFrame(loop);
    return true;
  },
  setFake(next: FakeCfg) {
    const state = fakeState();
    state.episodes = next.episodes ?? state.episodes;
    if (next.calls) state.calls = next.calls;
    return true;
  },
  requests: () => fakeState().calls.slice(),
  resetRequests() {
    fakeState().calls.length = 0;
    return true;
  },
  resetCache() {
    DanmakuLoader.resetSharedCacheForTests();
    return true;
  },
  setProps(patch: Partial<typeof props>) {
    Object.assign(props, patch);
    render();
    return true;
  },
  setStage(width: number, height = 328) {
    stage.width = width;
    stage.height = height;
    render();
    return true;
  },
  setTime(seconds: number) {
    video.currentTime = seconds;
    return video.currentTime;
  },
  clock(on: boolean) {
    clock.on = on;
    clock.last = 0;
    return clock.on;
  },
  async settle(ms = 120) {
    await waitFor(() => false, ms);
    return true;
  },
  snapshot: () => ({
    hasLayer: !!layer(),
    layerWidth: layer() ? Math.round(layer()!.getBoundingClientRect().width) : null,
    /** 粗体挂在层上、由每条弹幕继承（组件就是这么写的）。 */
    layerFontWeight: layer() ? layer()!.style.fontWeight : null,
    layerFontFamily: layer() ? layer()!.style.fontFamily : null,
    nodeCount: nodes().length,
    visible: visibleCount(),
    visibleTexts: visibleTexts(),
    texts: nodes().map(n => n.text),
    nodes: nodes(),
  }),
  rowsHint,
  flightWidth,
  waitVisible: (n: number, ms: number) => waitFor(() => visibleCount() >= n, ms),
  waitNone: (ms: number) => waitFor(() => visibleCount() === 0, ms),
  waitRequests: (n: number, ms: number) => waitFor(() => fakeState().calls.length >= n, ms),
  waitTexts: (pred: string[], ms: number) =>
    waitFor(() => pred.every(t => visibleTexts().includes(t)), ms),
};