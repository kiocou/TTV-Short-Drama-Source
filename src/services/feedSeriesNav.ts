/**
 * 「换一部推荐剧」的异步编排（补页 + 导航），决策与副作用分离。
 *
 * 抽出来的唯一理由是**可测**：补页是 `await`，而"连续两次补页失败、第三次成功"
 * 这类行为在 React 组件里只能靠真机点，抽到这里就能用假 loader 直接跑断言
 * （见 `scripts/verify-feed-series-nav.mjs`）。
 *
 * 四条不变量：
 * 1. **绝不重播当前剧**：只有拿到另一个下标才 `openIndex`，失败路径一律不开播；
 * 2. **欠页标记只在"还欠着"与"确实没页了"两种结论下写**，其余路径一个字段都不碰；
 * 3. **await 之后先验代数、再验位置**：换频道（`isCurrent()` 为假）时整段续体作废，
 *    连"下标恰好相同"（新旧频道都停在第 0 部）也不许读写 —— 否则旧续体会清掉新
 *    频道的欠页标记，或者对着新频道喊"列表到底了"；
 * 4. 位置被用户改过（同代内换剧）同样放弃，不把用户偷切回去。
 */

import { feedSeriesIndex, type FeedSeriesDirection } from './feedNavigation.ts';

export interface FeedSeriesNavHooks {
  /** 当前剧下标（每次调用都读最新值，不能在 `await` 前缓存后再用）。 */
  readIndex(): number;
  readItemCount(): number;
  readHasMore(): boolean;
  /** 本次导航是否仍属于当前代数（换频道 / 换代后为假）。 */
  isCurrent(): boolean;
  /** 补一页；失败时由它自己记录错误，本函数只看结果。 */
  appendPage(): Promise<void>;
  openIndex(index: number): Promise<void>;
  /** 写「还欠一页」标记。 */
  setPendingPage(pending: boolean): void;
  /** 队列真的到底（没有更多页），给一条轻提示。 */
  onExhausted(): void;
  /** 补页事务收尾：清掉上一次遗留的分页错误。 */
  onPageSettled(): void;
}

export async function runSeriesNav(
  direction: FeedSeriesDirection,
  hooks: FeedSeriesNavHooks,
): Promise<void> {
  if (!hooks.isCurrent()) return;

  const indexBefore = hooks.readIndex();
  let target = feedSeriesIndex(indexBefore, hooks.readItemCount(), direction);

  if (target === null && direction === 'next' && hooks.readHasMore()) {
    await hooks.appendPage();
    // 代数变了（换频道）：这段续体已经无效，任何读写都不能发生。
    if (!hooks.isCurrent()) return;
    if (hooks.readIndex() !== indexBefore) return; // 同代内用户换了剧
    target = feedSeriesIndex(hooks.readIndex(), hooks.readItemCount(), direction);
    if (target !== null) {
      hooks.setPendingPage(false);
      await hooks.openIndex(target);
      return;
    }
    // 仍然没有下一部：还有页就继续欠着（下次重试接着补），没页了就彻底收尾。
    if (hooks.readHasMore()) {
      hooks.setPendingPage(true);
      return;
    }
    hooks.setPendingPage(false);
    hooks.onPageSettled();
    hooks.onExhausted();
    return;
  }

  if (target === null) {
    if (direction === 'next' && !hooks.readHasMore()) {
      hooks.setPendingPage(false);
      hooks.onExhausted();
    }
    return;
  }

  hooks.setPendingPage(false);
  await hooks.openIndex(target);
}