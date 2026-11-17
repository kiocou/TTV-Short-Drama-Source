import React, { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  DANMAKU_LIFETIME_MS,
  DANMAKU_SPEED_LIFETIME_MS,
  DANMAKU_WINDOW_MS,
  DanmakuFlight,
  DanmakuLoader,
  createDanmakuTickState,
  nextDanmakuTick,
  planDanmaku,
} from '../../services/danmaku';
import type { DanmakuTickState } from '../../services/danmaku';
import { applyEmotes } from '../../services/danmakuEmoji';
import { ipcService } from '../../services/ipc';
import { tracePlayback } from '../../services/playbackTrace';

/**
 * 弹幕渲染层。
 *
 * ## 为什么不用 React 每帧重渲染
 *
 * 24 条弹幕 × 60fps = 每秒 1440 次状态更新，React 的重渲染与 diff 会把主线程
 * 占满，画面本身就开始掉帧。这里的做法是：**只有「这一页有哪些弹幕」是状态**，
 * 每一帧的位置直接写 DOM 的 `transform`（走合成器，不触发布局）。
 *
 * ## 为什么位置由 `video.currentTime` 算，而不是用 CSS 动画
 *
 * CSS 动画一旦启动就和媒体时钟脱钩：暂停时弹幕继续飞、拖动进度条时位置对不上、
 * 倍速播放时速度不跟随。用 `currentTime` 逐帧计算，这三种情况自动正确 ——
 * 暂停即静止、拖动即跳变到正确位置、倍速即同速。
 *
 * ## 轨道与密度
 *
 * 全部交给 `planDanmaku`（移植自真果鉴），本组件只负责把结果摆到屏幕上。
 * 轨道数不再写死：由「显示区域」设置（层高占比）与行高推导。
 *
 * ## 观感设置（2026-11-07 弹幕设置面板）
 *
 * 显示区域 / 不透明度 / 字号 / 速度 / 字体 / 粗体 / 描边全部来自 `DanmakuSettings`，
 * 由 `VideoSurface` / `AnimeVideoSurface` 从设置仓传入。速度档映射见
 * `DANMAKU_SPEED_LIFETIME_MS`；轨道判定与逐帧定位用**同一个**时长，慢/快档才不会
 * 「轨道算的是一套、飞行速度又是一套」。
 */

interface DanmakuOverlayProps {
  /**
   * 指向当前 `<video>` 的 ref。
   *
   * 收 ref 而不是元素本身：`VideoSurface` 首次渲染时元素还不存在，传元素值会
   * 让组件永远拿到 `null`（后续渲染不再变化，闭包里也读不到新值）。ref 对象
   * 本身是稳定的，逐帧读 `.current` 就能拿到实时元素。
   */
  videoRef: React.RefObject<HTMLVideoElement | null>;
  seriesId: string;
  episodeId: string;
  enabled: boolean;
  /**
   * 当前播放位置（毫秒）。
   *
   * **只用于「新建加载器的那一刻立刻发首窗请求」**，不参与逐帧定位（定位一律读
   * `video.currentTime`，暂停/拖动/倍速才自动正确）。用播放器状态而不是元素时钟，
   * 是因为换集瞬间元素上还留着上一集的播放头 —— 实测（`ttv-playback.log`，会话 103）
   * 首窗请求就是这样发到 60000ms 窗口去的，而正确的 0 窗口晚了 522ms。
   */
  positionMs: number;
  /**
   * 装载阶段的起播点（毫秒）；`null` = 已回归媒体时钟。
   *
   * 只影响**选哪一页**（`ensure` / `pageFor`），不参与逐帧定位——飞行位置永远由
   * `video.currentTime` 决定，所以不会提前放出未来的时间戳。取值判据在
   * `VideoSurface` 用 `danmakuLoadingStartMs` 算好再传进来。
   */
  openStartMs?: number | null;
  /** 弹幕观感设置（显示区域/不透明度/字号/速度/字体/粗体/描边）。 */
  appearance: DanmakuSettings;
}

/** `UserSettings` 里弹幕观感字段的子集——渲染层只需要这一把。 */
export interface DanmakuSettings {
  danmakuArea: number;
  danmakuOpacity: number;
  danmakuFontScale: number;
  danmakuSpeed: number;
  danmakuFont: string;
  danmakuColor: string;
  danmakuBold: boolean;
  danmakuStroke: 'heavy' | 'outline' | 'shadow';
}

/** 基准字号（= 字号缩放 100% 时的值）。 */
const BASE_FONT_SIZE = 20;
const TOP_PADDING = 10;

/** 速度档（0 慢 / 1 适中 / 2 快）→ 飞行时长。越界回落适中。 */
function lifetimeOf(speed: number): number {
  return DANMAKU_SPEED_LIFETIME_MS[speed] ?? DANMAKU_LIFETIME_MS;
}

/** 描边类型 → 文本阴影。heavy 是引入设置前的原始观感。 */
function strokeShadow(stroke: DanmakuSettings['danmakuStroke']): string {
  switch (stroke) {
    case 'outline':
      // 细描边：四向 1px 纯黑（经典「描边」观感）。
      return '-1px 0 0 #000, 1px 0 0 #000, 0 -1px 0 #000, 0 1px 0 #000';
    case 'shadow':
      // 45° 投影：右下实影 + 柔和扩散。
      return '2px 2px 0 rgba(0,0,0,.72), 4px 4px 8px rgba(0,0,0,.5)';
    default:
      // 重墨：四向粗黑边 + 底部投影（原实现）。
      return '-1px -1px 0 rgba(0,0,0,.85), 1px -1px 0 rgba(0,0,0,.85), -1px 1px 0 rgba(0,0,0,.85), 1px 1px 0 rgba(0,0,0,.85), 0 2px 4px rgba(0,0,0,.6)';
  }
}

/** 用 canvas 量文本宽度：不进 DOM、不触发回流，比 span.offsetWidth 快一个量级。 */
function createMeasurer(fontSize: number, bold: boolean, font: string): (text: string) => number {
  const canvas = document.createElement('canvas');
  const ctx = canvas.getContext('2d');
  if (!ctx) {
    // 极端环境（无 2d 上下文）退化成按字符数估算：宁可略宽也不能返回 0，
    // 否则所有弹幕宽度都是 0，轨道判定会全部挤在第一条轨道上。
    return (text: string) => text.length * fontSize * 0.6;
  }
  ctx.font = `${bold ? 'bold ' : ''}${fontSize}px "${font}", "Microsoft YaHei", -apple-system, sans-serif`;
  return (text: string) => ctx.measureText(text).width;
}

export const DanmakuOverlay: React.FC<DanmakuOverlayProps> = ({
  videoRef,
  seriesId,
  episodeId,
  enabled,
  positionMs,
  openStartMs = null,
  appearance,
}) => {
  const layerRef = useRef<HTMLDivElement | null>(null);
  const nodesRef = useRef<Map<string, HTMLDivElement>>(new Map());
  const flightsRef = useRef<DanmakuFlight[]>([]);
  const widthRef = useRef(0);
  const [flights, setFlights] = useState<DanmakuFlight[]>([]);
  const [layerWidth, setLayerWidth] = useState(0);
  const [layerHeight, setLayerHeight] = useState(0);

  const fontSize = Math.round(BASE_FONT_SIZE * (appearance.danmakuFontScale / 100));
  const rowHeight = Math.max(24, Math.round(fontSize * 1.5));
  const lifetimeMs = lifetimeOf(appearance.danmakuSpeed);
  const measure = useMemo(
    () => createMeasurer(fontSize, appearance.danmakuBold, appearance.danmakuFont),
    [fontSize, appearance.danmakuBold, appearance.danmakuFont],
  );

  /**
   * 轨道数 = 实际层高 ÷ 行高。「显示区域」给的是层高占比（层高本身用百分比
   * 写进样式，随播放器尺寸缩放），量出来的像素高再换算成轨道，1–12 条夹紧。
   */
  const rows = Math.min(12, Math.max(1, Math.floor((layerHeight - TOP_PADDING) / rowHeight) || 1));

  // 加载器只是共享缓存的访问口：换集清空的是画面，成功页和在途请求仍保留。
  // 取数包装里顺带把平台表情码换成 emoji（量宽在替换之后，宽度天然正确）。
  const loaderRef = useRef<DanmakuLoader | null>(null);
  /** 调度状态（上次 ensure 时刻 / 上次排程键）。判据本身在 `nextDanmakuTick`。 */
  const tickerRef = useRef<DanmakuTickState>(createDanmakuTickState());
  /**
   * 起播点镜像。建加载器的 effect 只依赖「剧 + 集」，不能把 `positionMs` 也放进依赖
   * （它每 250ms 变一次，进依赖就等于每 250ms 重建加载器、把页缓存全丢掉），
   * 所以用 ref 传值；写它用独立 effect，保证同一次提交里先写后读。
   */
  const positionMsRef = useRef(0);
  useEffect(() => {
    positionMsRef.current = Number.isFinite(positionMs) && positionMs >= 0 ? positionMs : 0;
  }, [positionMs]);

  /** 装载期起播点镜像（同上：不进建加载器 effect 的依赖，用 ref 传值）。 */
  const openStartMsRef = useRef<number | null>(null);
  useEffect(() => {
    const value = openStartMs != null && Number.isFinite(openStartMs) && openStartMs >= 0 ? openStartMs : null;
    openStartMsRef.current = value;
    // 同一集重开（列表里再点一次）不会重建加载器，但它可能还没有目标窗口那一页：
    // 目标一变就立刻补发一次，不等 250ms 节流，也不让它被媒体里的旧时钟拉回 0 窗。
    const loader = loaderRef.current;
    if (value == null || !loader) return;
    loader.ensure(value, Math.max(value + DANMAKU_WINDOW_MS, DANMAKU_WINDOW_MS));
    tickerRef.current.lastEnsureAt = performance.now();
  }, [openStartMs]);

  /**
   * 内容身份（剧 + 集）一变，**当次提交就**丢掉旧集的排程与画面。
   *
   * 不这么做时：切集瞬间 `flightsRef` 里仍是上一集的弹幕，而 `<video>.currentTime`
   * 还停在旧值（新源没装载），上一集的弹幕会继续画到新页到达 —— 跨集融合。
   * 用 layout effect 是为了在 paint 之前就抹掉，不给旧内容留一帧。同一集跨 30s
   * 窗口边界不走这里（走主循环的合并分支），在飞弹幕照旧飞完。
   */
  useLayoutEffect(() => {
    flightsRef.current = [];
    nodesRef.current.forEach(node => {
      node.style.opacity = '0';
    });
    setFlights([]);
  }, [seriesId, episodeId]);

  useEffect(() => {
    if (!enabled || !seriesId || !episodeId) {
      loaderRef.current?.dispose();
      loaderRef.current = null;
      return;
    }
    const loader = new DanmakuLoader(seriesId, episodeId, (sid, vid, startMs, durationMs) =>
      ipcService.playback.danmaku(sid, vid, startMs, durationMs).then(page => ({
        ...page,
        items: page.items.map(item => ({ ...item, text: applyEmotes(item.text) })),
      })),
    );
    loaderRef.current = loader;
    tickerRef.current = createDanmakuTickState();
    tracePlayback(`弹幕已启用 剧=${seriesId} 集=${episodeId}`);

    /*
     * 提交时刻直接发首窗，不等 rAF：起播期首帧可能被任何原因推迟（解析、落盘、
     * 渲染排队同一时刻发生，逐项未测），而旧实现把首个 ensure 挂在 rAF 上是**确定**
     * 的依赖。装载期用起播目标、否则用当前播放头；只对**当前窗口** ensure
     * （临近窗口末尾时它自带下一页预取，沿用既有行为），不额外请求、不猜内容。
     */
    const startMs = openStartMsRef.current ?? positionMsRef.current;
    loader.ensure(startMs, Math.max(startMs + DANMAKU_WINDOW_MS, DANMAKU_WINDOW_MS));
    tickerRef.current.lastEnsureAt = performance.now();

    return () => {
      loader.dispose();
      loaderRef.current = null;
    };
  }, [enabled, seriesId, episodeId]);

  // 容器宽度/高度：宽度是轨道判定公式里的 `width`，高度决定轨道数，
  // 任一变化都必须重新规划。
  useEffect(() => {
    const layer = layerRef.current;
    if (!layer) return undefined;
    const observer = new ResizeObserver(entries => {
      const rect = entries[0]?.contentRect;
      const width = rect?.width ?? 0;
      const height = rect?.height ?? 0;
      setLayerWidth(current => (Math.abs(current - width) < 1 ? current : width));
      setLayerHeight(current => (Math.abs(current - height) < 1 ? current : height));
    });
    observer.observe(layer);
    return () => observer.disconnect();
  }, [enabled]);

  useEffect(() => {
    widthRef.current = layerWidth;
  }, [layerWidth]);

  // 主循环：每帧读一次缓存页（页到即排程），`ensure` 仍按 250ms 节流；判据全在
  // `nextDanmakuTick`（可单测，见 `danmaku.timing.test.mjs`）。
  useEffect(() => {
    if (!enabled) {
      setFlights([]);
      flightsRef.current = [];
      tickerRef.current = createDanmakuTickState();
      return undefined;
    }
    let raf = 0;

    // 量宽身份：字体/粗体/字号变了 `measure` 就是另一个函数，`planDanmaku` 的
    // 输出随之变化（rows/width 可能一动不动），必须让判据知道它变了。
    const measureKey = `${fontSize}|${appearance.danmakuBold ? 1 : 0}|${appearance.danmakuFont}`;

    const tick = () => {
      raf = requestAnimationFrame(tick);
      const element = videoRef.current;
      if (!element) return;
      const loader = loaderRef.current;
      if (!loader) return;

      const positionMs = (element.currentTime || 0) * 1000;
      // 选页位置：装载期用起播目标（媒体时钟此时可能还是上一集的、也可能已清零），
      // 起播就绪后 `openStartMs` 变 null，自动回到媒体时间。定位不读它。
      const selectionMs = openStartMsRef.current ?? positionMs;
      // 时长兜底（2026-11-07 稳定性）：个别 HLS 流在缓冲瞬间 duration 会读到
      // NaN/Infinity，旧实现就此拒绝拉页（durationMs=0），表现为弹幕莫名消失
      // 一段时间。拿「选页位置 + 一个窗口」顶上，保证 ensure 的门永远开着。
      const durationMs = (Number.isFinite(element.duration) && element.duration > 0
        ? element.duration * 1000
        : selectionMs + DANMAKU_WINDOW_MS);

      const page = loader.pageFor(selectionMs);
      const decision = nextDanmakuTick(tickerRef.current, {
        nowMs: performance.now(),
        positionMs: selectionMs,
        durationMs,
        width: widthRef.current,
        rows,
        lifetimeMs,
        measureKey,
        page,
      });

      if (decision.ensure) loader.ensure(selectionMs, durationMs);

      if (decision.plan && page) {
        const planned = planDanmaku(page.items, {
          width: widthRef.current,
          rows,
          measure,
          lifetimeMs,
        });
        /*
         * 换页不清屏：带上仍在飞的旧弹幕一起显示（2026-11-07 稳定性）。
         * 定位只依赖 `positionMs - timeMs`，每条按自己的时钟飞完再自然隐藏，
         * 按 id 去重防止两页重叠窗口里同一条出现两次。代价是边界后 8s 内两代
         * 各自排轨、理论上可能同轨相叠 —— 比"飞着飞着消失"好得多。
         * 页隙（预取没赶上 / 冷却中）走判据的 `!page` 分支：只作废排程键。
         */
        const alive = flightsRef.current.filter(f => {
          const elapsed = positionMs - f.item.timeMs;
          return (
            elapsed >= 0 &&
            elapsed < lifetimeMs &&
            !planned.some(p => p.item.id === f.item.id)
          );
        });
        const merged = [...alive, ...planned];
        flightsRef.current = merged;
        setFlights(merged);
      }

      // 2) 逐帧定位：只改 transform/opacity，不产生 React 更新。
      const width = widthRef.current;
      if (width <= 0) return;
      for (const flight of flightsRef.current) {
        const node = nodesRef.current.get(flight.item.id);
        if (!node) continue;
        const elapsed = positionMs - flight.item.timeMs;
        if (elapsed < 0 || elapsed > lifetimeMs) {
          if (node.style.opacity !== '0') node.style.opacity = '0';
          continue;
        }
        const progress = elapsed / lifetimeMs;
        const x = width - (width + flight.width) * progress;
        node.style.transform = `translate3d(${x.toFixed(1)}px, ${
          TOP_PADDING + flight.lane * rowHeight
        }px, 0)`;
        if (node.style.opacity !== '1') node.style.opacity = '1';
      }
    };

    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [enabled, videoRef, rows, measure, lifetimeMs, rowHeight]);

  // 清掉已不在列表里的节点引用。
  useEffect(() => {
    const alive = new Set(flights.map(flight => flight.item.id));
    for (const id of [...nodesRef.current.keys()]) {
      if (!alive.has(id)) nodesRef.current.delete(id);
    }
  }, [flights]);

  if (!enabled) return null;

  return (
    <div
      ref={layerRef}
      className="absolute inset-x-0 top-0 pointer-events-none overflow-hidden"
      style={{
        // 显示区域 = 占播放器高度的百分比；透明度挂在整层，字号/字体/描边
        // 由每条弹幕继承。层高变化由 ResizeObserver 换算成轨道数。
        height: `${Math.min(100, Math.max(25, appearance.danmakuArea))}%`,
        opacity: Math.min(1, Math.max(0.1, appearance.danmakuOpacity / 100)),
        fontFamily: `"${appearance.danmakuFont}", "Microsoft YaHei", -apple-system, sans-serif`,
        fontWeight: appearance.danmakuBold ? 700 : 400,
        zIndex: 25,
      }}
      aria-hidden
    >
      {flights.map(flight => (
        <div
          key={flight.item.id}
          ref={node => {
            if (node) nodesRef.current.set(flight.item.id, node);
            else nodesRef.current.delete(flight.item.id);
          }}
          style={{
            position: 'absolute',
            top: 0,
            left: 0,
            whiteSpace: 'nowrap',
            fontSize,
            lineHeight: `${rowHeight}px`,
            // 文字颜色来自设置（色板/取色器）。必须显式声明：继承父层的颜色
            // 不可靠，深色主题下会被带成深灰、视频上直接看不清。
            color: appearance.danmakuColor,
            textShadow: strokeShadow(appearance.danmakuStroke),
            opacity: 0,
            willChange: 'transform',
          }}
        >
          {flight.text}
        </div>
      ))}
    </div>
  );
};

export default DanmakuOverlay;
