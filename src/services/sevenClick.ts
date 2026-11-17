/**
 * 黄果短剧专区的入口手势：标题栏品牌区连点七次。
 *
 * 纯逻辑、无 React、无定时器 —— 时间由调用方传入，所以可以直接在
 * `scripts/verify-huangguo.mjs` 里断言，不需要 DOM 或假时钟。
 *
 * ## 窗口语义是「相邻间隔」而不是「总时长」
 *
 * 只要两次点击的间隔不超过 `windowMs`，计数就继续累加；超了就从 1 重新开始。
 * 用总时长窗口的话，用户点到第五下稍作停顿就得全部重来，而这类隐藏入口本来
 * 就没有任何提示，挫败感会直接变成「这功能坏了」。
 *
 * ## 为什么不在这里放 setTimeout
 *
 * 计时器意味着要在组件卸载时清理，而这个计数器挂在标题栏上——标题栏在全屏
 * 切换时会卸载重建。把时间做成入参，卸载就只是丢一个 ref，没有悬挂回调。
 */

export interface SevenClickOptions {
  /** 触发所需的连点次数。默认 7。 */
  threshold?: number;
  /** 相邻两次点击的最大间隔（毫秒）。默认 900。 */
  windowMs?: number;
}

export interface SevenClickCounter {
  /** 记一次点击；达到阈值时返回 true 并自动复位。 */
  tap(now: number): boolean;
  /** 当前累计次数（0 表示尚未开始或已复位）。 */
  count(): number;
  reset(): void;
}

export const SEVEN_CLICK_THRESHOLD = 7;
export const SEVEN_CLICK_WINDOW_MS = 900;

export function createSevenClickCounter(options: SevenClickOptions = {}): SevenClickCounter {
  const threshold = Math.max(2, Math.floor(options.threshold ?? SEVEN_CLICK_THRESHOLD));
  const windowMs = Math.max(1, options.windowMs ?? SEVEN_CLICK_WINDOW_MS);
  let hits = 0;
  let last = Number.NEGATIVE_INFINITY;

  return {
    tap(now: number): boolean {
      // 时间倒流（系统时钟被改）当作重新开始，不要让计数卡在一个永不超时的状态。
      const expired = !Number.isFinite(now) || now < last || now - last > windowMs;
      hits = expired ? 1 : hits + 1;
      last = now;
      if (hits >= threshold) {
        hits = 0;
        last = Number.NEGATIVE_INFINITY;
        return true;
      }
      return false;
    },
    count(): number {
      return hits;
    },
    reset(): void {
      hits = 0;
      last = Number.NEGATIVE_INFINITY;
    },
  };
}