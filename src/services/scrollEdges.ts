import { useEffect, useState } from 'react';
import type { RefObject } from 'react';

/**
 * 横滚轨道两端箭头的**唯一判定口径**。
 *
 * ## 它修的是什么
 *
 * 首页货架（`HomeShelf`）与题材条（`CategoryBar`）都用「还有内容被藏在那边吗」
 * 决定箭头显不显示。原来的判据写死在两处，且都是同一个式子：
 *
 * ```js
 * const left  = rail.scrollLeft > 2;
 * const right = rail.scrollWidth - rail.clientWidth - rail.scrollLeft > 2;
 * ```
 *
 * 这个 2px 的容差**没把轨道自己的左右内边距算进去**，于是「到最左了左箭头还亮着」
 * ——用户报的正是这个。原因是 `scroll-snap-type: x mandatory` 的吸附点按元素的
 * border box 对齐滚动窗口的起始边，而轨道带了 `px-1`（4px）之类的内边距：
 * 第一张卡在文档流里距内容起点 4px，吸附后 `scrollLeft` 停在 **4** 而不是 0，
 * `4 > 2` 成立，箭头就永远当成"左边还有内容"。
 *
 * 所以容差必须是 `该侧内边距 + 平滑冗余`，而不是一个拍脑袋的 2。
 * 两端各用自己那一侧的内边距：轨道常做成 `px-1` 甚至左右不对称。
 *
 * ## 为什么抽成共用
 *
 * 货架与题材条是同一件事的两种排布，判定必须一致，否则同一个手势在两处
 * 表现不同。判定口径只有这一份，改行为改这里。
 *
 * ## 为什么不用 IntersectionObserver 看首/末元素
 *
 * 也想过：观察第一张与最后一张卡，谁被完全挡住谁就说明那头还有内容。问题是
 * "被挡住一部分"与"完全滚过去"是两回事，卡宽参差时判定树更绕；而且换数据
 * 就要换观察目标，得在每次 items 变化后重绑。直接量三个数字更省也更准。
 */

/** 一端是否还有内容被藏起来。 */
export interface ScrollEdges {
  /** 起始方向（LTR = 左侧）。 */
  start: boolean;
  /** 结束方向（LTR = 右侧）。 */
  end: boolean;
}

/** 两端都没有被藏起来的内容。导出它是为了让调用方能拿到稳定引用、少触发一次渲染。 */
export const NO_SCROLL_EDGES: ScrollEdges = { start: false, end: false };

/**
 * 平滑冗余（px）。
 *
 * 触控板横滑与 `scrollBy({behavior:'smooth'})` 都会留下亚像素残值，吸附回弹也会
 * 在落点附近抖动一两像素。取 2 是把"精确贴边"和"贴边但差 1px"归成同一态；
 * 再大就会把真的溢出（第一张卡已滚掉 90%）误判成贴边，箭头该出现却不出现。
 */
const SMOOTHING_SLACK = 2;

/** 只要溢出量不到这个数，就当成"不需要横滚"：两端都不该有箭头。 */
const MIN_OVERFLOW = SMOOTHING_SLACK;

function toPx(raw: string): number {
  const value = Number.parseFloat(raw);
  return Number.isFinite(value) ? value : 0;
}

/**
 * 读一次该轨道的两端容差。
 *
 * `getComputedStyle` 会触发样式重算，所以只在内边距可能变化的时候读（挂载、
 * 尺寸变化），不放进每帧的滚动回调里。返回的两端分开存，因为轨道常见
 * `px-1`、`pl-4 pr-0` 这类不对称写法。
 */
export function readEdgeTolerance(el: HTMLElement): { start: number; end: number } {
  const style = window.getComputedStyle(el);
  return {
    start: toPx(style.paddingLeft) + SMOOTHING_SLACK,
    end: toPx(style.paddingRight) + SMOOTHING_SLACK,
  };
}

/**
 * 量一次两端状态。纯函数，不读 state、不排帧 —— 调用方决定什么时候量。
 *
 * `tolerance` 传 `readEdgeTolerance` 的结果；不传就按 0 内边距 + 平滑冗余算，
 * 只在轨道确实无内边距时才该省略。
 */
export function measureScrollEdges(
  el: HTMLElement | null,
  tolerance?: { start: number; end: number },
): ScrollEdges | null {
  if (!el) return null;
  const max = el.scrollWidth - el.clientWidth;
  // 没溢出（或只有亚像素抖动）就是两端都贴边，别让箭头因为 1px 残值闪一下。
  if (max <= MIN_OVERFLOW) return NO_SCROLL_EDGES;

  const tol = tolerance ?? { start: SMOOTHING_SLACK, end: SMOOTHING_SLACK };
  const scrolled = el.scrollLeft;
  return {
    start: scrolled > tol.start,
    end: max - scrolled > tol.end,
  };
}

/**
 * 把"量两端状态"接进 React：滚动、尺寸变化、内容变化后自动重测。
 *
 * ## 为什么在滚动之外还挂了 ResizeObserver 和下一帧补测
 *
 * 滚动事件只覆盖"用户正在滚"。剩下三种情况会让箭头停在错误状态，而这三种
 * 恰恰都是用户会看到的：
 *
 *   1. **视图刚从 `display:none` 恢复**（本应用所有页面常驻 DOM、靠 display 切）。
 *      挂载那一刻量到的 `clientWidth` 可能是 0 或旧值，箭头会按错的溢出量亮着；
 *   2. **换数据/换频道后轨道内容变短**（例如题材从 40 个变 12 个）。浏览器会把
 *      越界后的 `scrollLeft` 收敛回合法值，但这个收敛**不一定派发 scroll 事件**，
 *      于是"左边还有内容"停在旧值上；
 *   3. **字体就绪 / 卡片宽度回填**。文字变宽会改 `scrollWidth`，而容器的
 *      border-box 没变，`ResizeObserver` 看不见，只能等 `document.fonts.ready`。
 *
 * `extraDeps` 就是给调用方塞"内容变了"的信号（`items.length`、`categories.length`）。
 * 它被展开进依赖数组，所以调用方必须传**稳定长度**的数组。
 */
export function useScrollEdges(
  ref: RefObject<HTMLElement | null>,
  enabled = true,
  extraDeps: readonly unknown[] = [],
): ScrollEdges {
  const [edges, setEdges] = useState<ScrollEdges>(NO_SCROLL_EDGES);

  useEffect(() => {
    if (!enabled) {
      // 关掉时归零：轨道这一形态根本不存在（例如货架退化成了自适应 Grid），
      // 留着一个 true 会让调用方渲出一颗永远点不出反应的箭头。
      setEdges(prev => (prev.start || prev.end ? NO_SCROLL_EDGES : prev));
      return;
    }
    const el = ref.current;
    if (!el) return;

    let tolerance = readEdgeTolerance(el);
    let frame = 0;

    const apply = (next: ScrollEdges | null) => {
      if (!next) return;
      // 值没变就返回同一个对象：scroll 事件每帧都来，不能让无变化的测量穿透到渲染。
      setEdges(prev => (prev.start === next.start && prev.end === next.end ? prev : next));
    };
    const measure = () => {
      frame = 0;
      apply(measureScrollEdges(el, tolerance));
    };
    const schedule = () => {
      if (frame) return;
      frame = requestAnimationFrame(measure);
    };

    measure();
    // 下一帧再量一次：挂载当帧的 clientWidth 常是旧值（见上面第 1 条）。
    const settle = requestAnimationFrame(measure);

    el.addEventListener('scroll', schedule, { passive: true });
    const observer =
      typeof ResizeObserver === 'undefined'
        ? null
        : new ResizeObserver(() => {
            // 尺寸变了内边距可能跟着变（响应式断点），容差顺带刷新。
            tolerance = readEdgeTolerance(el);
            schedule();
          });
    observer?.observe(el);

    const fonts = typeof document !== 'undefined' ? document.fonts : undefined;
    fonts?.ready?.then(schedule).catch(() => {});

    return () => {
      if (frame) cancelAnimationFrame(frame);
      cancelAnimationFrame(settle);
      el.removeEventListener('scroll', schedule);
      observer?.disconnect();
    };
    // extraDeps 由调用方提供的"内容变了"信号构成，展开进依赖是有意的。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ref, enabled, ...extraDeps]);

  return edges;
}