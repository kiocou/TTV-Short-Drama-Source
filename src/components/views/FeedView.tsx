import React, { useCallback, useEffect, useRef } from 'react';
import { feedGestureDirection, feedWheelDirection } from '../../services/feedSource';
import {
  feedEpisodeStep,
  feedNavAllowed,
  type FeedSeriesDirection,
} from '../../services/feedNavigation';
import { isTauriEnvironment } from '../../services/ipc';
import { enterFullscreen, leaveFullscreen } from '../../services/windowFx';
import { useAppStore } from '../../stores/useAppStore';
import { useFeedStore } from '../../stores/useFeedStore';
import { usePlaybackStore } from '../../stores/usePlaybackStore';
import { FeedControls } from '../player/FeedControls';
import { FeedPlaybackBar } from '../player/FeedPlaybackBar';
import { FeedSlide } from '../player/FeedSlide';
import { FeedStatusBanner, deriveFeedPhase } from '../player/FeedStatusBanner';

/**
 * 「短剧推荐」整屏页。
 *
 * ## 这次改版针对的现象
 *
 * 用户实机截图：**整屏黑 + 一句「缓冲中...」+ `00:00 / 00:00`**，页面上没有任何
 * 「现在在做什么 / 还要不要等 / 是不是已经卡死」的信息。同一个「缓冲中」文案同时
 * 覆盖了两件性质完全不同的事：
 *
 * - 首次解析整集（向内容源申请播放地址，实测约 7 秒）；
 * - 媒体真的在积累缓冲（源就绪之后）。
 *
 * 于是「等一下就好」和「可能是网络问题」在屏幕上长得一模一样。
 *
 * ## 三处结构性改动
 *
 * 1. **状态区唯一化**：旧版是「播放器内部 overlay（缓冲中）」+「FeedView 的整屏
 *    错误遮罩」两套并存，叠在一起时下半截文字被遮罩吃掉。现在 `FeedStatusBanner`
 *    是这一页唯一的状态出口，`deriveFeedPhase` 用严格优先级选出要显示哪一个。
 * 2. **封面占位**：黑屏期用 `currentItem.cover` 铺一张半透明封面。之所以在这里做
 *    而不是交给 `VideoSurface`：`FeedSlide` 要等 `feed.detail` 到位才渲染，而黑屏
 *    最严重的那一段（解析整集）恰恰没有 detail，只能由本页用列表项的 cover 兜住。
 *    `VideoSurface` 属于播放器层，不在本次作用域内。
 * 3. **`busy` 收窄**：原来 `isLoadingMore`（后台补推荐页）也会把右侧四颗按钮一起
 *    禁用。后台翻页与「用户此刻能不能翻集」无关，收窄到 `isLoading || isOpening`。
 *
 * ## 不动的部分
 *
 * - 550ms 节流阈值（现在由 `feedNavigation.feedNavAllowed` 判定，状态放在 ref 里）、
 *   `isInteractiveTarget` 白名单、`FeedCompletionBridge` 对 `usePlaybackStore` 的订阅。
 * - `trigger(deltaX, deltaY)` 早在第二次改版就被重构成 `fire(direction)`：方向交给
 *   调用方给。手势的物理方向（上滑 = `deltaY < 0`）与滚轮的滚动量（向下 =
 *   `deltaY > 0`）恰好相反，混在同一个 delta 公式里必然有一路写反 —— 第四次改版
 *   拆成 `feedGestureDirection`（拖滑）与 `feedWheelDirection`（滚轮）两个纯函数，
 *   键鼠仍然共用同一个 `fire`，节流只有一处。
 * - 频道切换与顶栏布局。
 *
 * ## 导航分工（2026-11-10 第四次改版：四路输入统一为「换一部」）
 *
 * 用户原话：「上下方向键要切不同的短剧、不要切不同的集」「每次进入另一部推荐剧
 * 从第 1 集 0 秒开始，不操作时继续本剧第 2、3 集」。于是分工重排：
 *
 * | 输入 | 动作 | 覆盖的人 |
 * | --- | --- | --- |
 * | `ArrowUp` / `ArrowDown` | 上一部 / 下一部推荐剧 | 键盘 |
 * | 滚轮向上 / 向下 | 上一部 / 下一部 | 鼠标 |
 * | 手指/鼠标上滑 / 下滑 | 下一部 / 上一部 | 触屏、触控板 |
 * | 「详细」面板底部两颗按钮 | 本剧的上/下一集 | 屏幕阅读器（手势不可达） |
 *
 * **没有任何输入换集**：本剧第 2、3 集只由"不操作时自动连播"推进
 * （`usePlaybackStore.handleEnded` → `playNextEpisode`，见 `verify:feed-playback`）。
 * 手动换集的兜底入口留在「详细」面板里，且只在本剧内。
 *
 * ## 为什么状态不在这里直接订阅 `usePlaybackStore`
 *
 * 状态按约定只能从 `useFeedStore` 的 `isLoading / isOpening / error / isCompleted`
 * 推导。`usePlaybackStore` 由 A 线独占改造，跨域订阅会让两条线的状态语义互相牵制。
 * 「媒体缓冲」这一档由 `FeedStageOverlay` 单独订阅后传入，主体仍然不订整表。
 *
 * ## 2026-11-10 第三次改版：这一页只留**一套**控制层
 *
 * 用户原话：「右上角那个位置，还有按钮全部放在一起太乱、太杂了；左下角文字区域
 * 太多了…播放控制栏重新设计一套兼容方案，并且对我这个短剧推荐区域的播放器，
 * 不要复用原来的格式，只保留原来的风格就行」。
 *
 * 截图现场是三处叠加：右上角一行挤着「频道 tab + 原生音放图标 + 原生 mpv 播放中·
 * 联播开徽标」；左下角剧名 + 长简介 + 又一遍长简介；底部主播放器的 11 个控件。
 * 分别处理：
 *
 * | 区块 | 现在的样子 |
 * | --- | --- |
 * | 右上 | **只有**频道托盘（短剧 / 漫剧 / AI 剧）。播放器状态文字一律不在这里 —— 原生入口与 mpv 徽标属于 `PlayerControls`，它已在 feed 视图下整块不渲染 |
 * | 左下 | 剧名 + `第 N 集 · 共 M 集` + 一行「简介」入口，正文默认**完全不渲染**，点开走浮层（见 `FeedSlide`） |
 * | 底部 | `FeedPlaybackBar` 自绘的极简条：播放/暂停、进度、时间码、下一集、全屏 |
 *
 * 这条底部条**不**复用 `PlayerControls` 的布局格式，只沿用它的视觉语汇
 * （白/玻璃 + fluent 阴影 + `rounded-full` 托盘）。静音不在条里 —— 右侧竖排
 * 已经有「静音 / 追剧 / 详情」三颗，两处都放就是两个入口抢同一件事。
 */

/**
 * 滚轮 / 指针 / 触摸的排除名单。
 *
 * 「推荐页顶部现在是真标题栏了」（第四次改版把 `TitleBar` 装回 feed 视图），
 * 那 40px 属于**窗口 chrome**：滚轮在它上面不该换剧、拖动更不该被当成上滑手势。
 * 用 `[data-launch-part="titlebar"]` 标记（`TitleBar` 的 `<header>` 自带）而不是
 * 另给它一个 id —— 事件是挂在 window 上的，`closest` 顺着祖先链就能命中。
 */
const isInteractiveTarget = (target: EventTarget | null): boolean => (
  target instanceof Element
  && Boolean(target.closest(
    'button, input, select, textarea, [role="slider"], .crystal-flyout, [data-feed-no-swipe], [data-launch-part="titlebar"]',
  ))
);

/**
 * 方向键的排除名单。
 *
 * 与 `isInteractiveTarget` 有意**不共用**：那个名单里有 `button`，是为了不让
 * 「点按钮」被当成滑动；但方向键的语义是「换一部」，焦点停在按钮上按上下键
 * 仍然应该换剧（用户可能刚 Tab 到静音按钮）。真正必须让位的是会自己消费方向键
 * 的元素：文本输入、可编辑区、下拉框，以及弹层（面板里滚轮/方向键属于面板）。
 *
 * 标题栏**不在**排除名单里：feed 下它的搜索框已隐藏（`TitleBar` 内部判断），
 * 剩下的是品牌入口与三颗窗口按钮，焦点停在那里按 ↑/↓ 换剧是合理行为。
 */
const blocksFeedKeys = (target: EventTarget | null): boolean => {
  if (target instanceof HTMLElement && target.isContentEditable) return true;
  if (!(target instanceof Element)) return false;
  return Boolean(target.closest(
    'input, textarea, select, [contenteditable]:not([contenteditable="false"]), .crystal-flyout, [data-feed-no-swipe]',
  ));
};

const FeedCompletionBridge: React.FC<{ onFinished: (sessionId: number) => void }> = ({ onFinished }) => {
  const { finishedAll, sessionId } = usePlaybackStore();
  const handledSession = useRef<number | null>(null);

  useEffect(() => {
    if (!finishedAll) {
      handledSession.current = null;
      return;
    }
    if (handledSession.current === sessionId) return;
    handledSession.current = sessionId;
    onFinished(sessionId);
  }, [finishedAll, onFinished, sessionId]);

  return null;
};

/**
 * 播放阶段叠加层：黑屏期封面 + 状态横幅。
 *
 * ## 为什么单独成一个组件（2026-11-10）
 *
 * 上一版把 `isBuffering` 直接订在 `FeedView` 里，代价是整棵推荐页跟着播放进度
 * 重渲染——`usePlaybackStore()` 返回整表，`position` 每秒变 4 次。`FeedSlide`
 * 与 `FeedControls` 都没包 `React.memo`，等于每秒重建两棵子树。
 *
 * 把**唯一需要那个高频信号的两块 UI**（封面占位、状态横幅）挪到这里之后：
 * - `FeedView` 主体不订阅 playback，只在 `feed`（低频：加载/解析/错误/收藏）
 *   变化时重渲染；
 * - 这个组件的重渲染范围就是它自己返回的几个节点。
 *
 * 比给两个子组件包 `React.memo` 更彻底：memo 只能挡住 props 未变的子树，
 * 而这里从源头就没有订阅。也不依赖子组件的 props 是否稳定（那种前提一旦被
 * 后来人加一个内联箭头函数就会静默失效）。
 *
 * 为什么不用 `usePlaybackSelector`：它只省解构、不省重渲染（见它的注释）。
 */
const FeedStageOverlay: React.FC<{
  feed: ReturnType<typeof useFeedStore>;
  canSwitchSeries: boolean;
}> = ({ feed, canSwitchSeries }) => {
  const { isBuffering } = usePlaybackStore();

  const phase = deriveFeedPhase({
    error: feed.error,
    isCompleted: feed.isCompleted,
    isOpening: feed.isOpening,
    isLoading: feed.isLoading,
    hasDetail: Boolean(feed.detail),
    isBuffering,
  });

  // 已预热的剧集切换 <280ms 即可出画，短暂等待态不闪烁"正在解析"横幅；错误与完成态立即呈现。
  const isTransientPending = phase === 'opening' || phase === 'loading' || phase === 'buffering';
  const [showPending, setShowPending] = React.useState(false);
  useEffect(() => {
    if (!isTransientPending) {
      setShowPending(false);
      return;
    }
    const timer = window.setTimeout(() => setShowPending(true), 280);
    return () => window.clearTimeout(timer);
  }, [isTransientPending, phase]);

  const visiblePhase = isTransientPending && !showPending ? null : phase;

  return (
    <>
      {visiblePhase && (
        <FeedStatusBanner
          phase={visiblePhase}
          message={visiblePhase === 'error' ? feed.error ?? undefined : undefined}
          canSwitchSeries={canSwitchSeries}
          onSwitchSeries={() => feed.switchSeries('next')}
          onRetry={feed.retry}
          retryLabel={feed.isCompleted ? '重播' : '重试'}
        />
      )}
    </>
  );
};

export const FeedView: React.FC = () => {
  const { currentView, isFullscreen, setIsFullscreen } = useAppStore();
  const feed = useFeedStore();
  const titleRef = useRef<HTMLHeadingElement | null>(null);
  const active = currentView === 'feed';
  const prevIndexRef = useRef(feed.currentIndex);
  const slideDirectionRef = useRef<FeedSeriesDirection>('next');
  if (feed.currentIndex !== prevIndexRef.current) {
    slideDirectionRef.current = feed.currentIndex < prevIndexRef.current ? 'previous' : 'next';
    prevIndexRef.current = feed.currentIndex;
  }
  const slideDirection = slideDirectionRef.current;
  const busy = feed.isLoading || feed.isOpening;
  const canSwitchSeries = feed.currentIndex + 1 < feed.items.length || feed.hasMore;
  const displaySeries = feed.currentItem && feed.detail
    ? { ...feed.currentItem, brief: feed.detail.description, episodesCount: feed.detail.episodesCount }
    : null;
  /** 当前集。集号取自源（可能因季号而不等于 index + 1），信息层与详细面板共用。 */
  const currentEpisode = feed.detail?.episodes[feed.episodeIndex] ?? null;
  const episodeNumber = currentEpisode?.episodeNumber ?? feed.episodeIndex + 1;

  /**
   * 详细面板的数据源：`detail` 优先、列表项兜底。
   *
   * 为什么不复用 `displaySeries`（它要 `currentItem && detail` 同时成立）：详情还在
   * 路上时用户仍想看一眼这部剧是什么——列表项已经有剧名/标签/来源，面板可以先
   * 给出这些，简介留空，而不是让「详细」按钮一直灰着。
   */
  const panelTitle = feed.detail?.title || feed.currentItem?.title || '';
  const panelDescription = feed.detail?.description || feed.currentItem?.brief || '';
  const panelTags = feed.detail?.tags ?? feed.currentItem?.tags ?? [];
  const panelOrigin = feed.detail?.origin || feed.currentItem?.origin || '';

  /**
   * 底部条「下一集」是否可用。
   *
   * 2026-11-10 第四次改版改了口径：旧实现在这里刻意**不**推边界，理由是
   * `feed.nextEpisode` 到末集会转成「换一部」，灰掉就等于掐掉用户唯一入口。
   * 现在末集按「下一集」是 no-op（不换季、不换部），所以边界必须如实反映在
   * 按钮上：末集 / 可播边界处直接禁用，用户不会点了没反应还以为卡住。
   *
   * 判据复用 `feedEpisodeStep`（与 store 同一个纯函数），不在 UI 里另写一份
   * `episodeIndex + 1 < n`。
   */
  const canGoNext = !busy && feedEpisodeStep(
    feed.episodeIndex,
    feed.detail?.episodes.length ?? 0,
    'next',
    feed.detail?.playableEpisodes,
  ) !== null;

  /**
   * 全屏切换。沿用 `VideoSurface` 的结论：**只走原生窗口全屏**，不用
   * `element.requestFullscreen()`。
   *
   * 两套机制各自独立、无法可靠同步（DOM 全屏成功而窗口没铺满 / 窗口还在全屏而
   * DOM 已退出），所以这里查窗口真实状态为准，不乐观写入。非 Tauri（浏览器 Mock）
   * 环境没有原生窗口，退化成 `appStore.isFullscreen` 上的纯状态切换。
   */
  const toggleFullscreen = useCallback(async () => {
    const entering = !isFullscreen;
    if (!isTauriEnvironment()) {
      setIsFullscreen(entering);
      return;
    }
    const actual = entering ? await enterFullscreen() : await leaveFullscreen();
    setIsFullscreen(actual);
  }, [isFullscreen, setIsFullscreen]);

  useEffect(() => {
    if (active) titleRef.current?.focus({ preventScroll: true });
  }, [active]);

  /**
   * 手势/键盘闸门的**时间戳**。
   *
   * 为什么是 `useRef` 而不是 `useEffect` 里的局部变量：第四次改版的语义变了，
   * 依赖里出现了 `feed.switchSeries` 这类 store 回调，任何一次重渲染只要让它
   * 换了身份，effect 就会重订阅、局部变量被重新初始化成 0 —— 550ms 节流失效，
   * 而失效的方向恰好是"连切更快"，用户感知是"按一下换了两部"。ref 的生命周期
   * 跟组件实例走，与重渲染次数无关。
   */
  const lastNavAtRef = useRef(0);

  useEffect(() => {
    if (!active) return;
    const start = { x: 0, y: 0, valid: false };
    /**
     * 手势、滚轮与键盘的**唯一**闸门。
     *
     * 550ms 节流必须只有这一处：如果键盘另算一个计时器，用户「上滑一部 + 顺手按
     * 一下方向键」就会在几十毫秒内触发两次换剧，而 store 只挡得住并发（
     * `openingRef` / `loadingRef`），挡不住先后到达的两次顺序调用。
     *
     * 方向由调用方给出而不是在这里从 delta 推：拖滑的物理方向（上滑 = 下一部，
     * `deltaY < 0`）与滚轮的滚动量（向下 = 下一部，`deltaY > 0`）恰好相反，
     * 混在一个公式里迟早写反。翻译分别由 `feedGestureDirection` 与
     * `feedWheelDirection` 负责。
     *
     * 动作统一是 `feed.switchSeries(direction)` —— 换**另一部剧**，从首集 0 秒
     * 开始。换集只由自动连播与「详细」面板里的兜底按钮负责。
     */
    const fire = (direction: FeedSeriesDirection | null, event?: Event) => {
      if (!direction) return;
      if (!feedNavAllowed(lastNavAtRef.current, Date.now())) return;
      lastNavAtRef.current = Date.now();
      event?.preventDefault();
      feed.switchSeries(direction);
    };
    const onWheel = (event: WheelEvent) => {
      if (isInteractiveTarget(event.target)) return;
      fire(feedWheelDirection(event.deltaX, event.deltaY), event);
    };
    /**
     * 方向键：`ArrowUp` → 上一部，`ArrowDown` → 下一部（**不是**上一集/下一集）。
     *
     * 焦点在输入框 / 文本域 / 可编辑区 / 弹层内时完全不响应（见 `blocksFeedKeys`），
     * 否则用户在详情面板里按上下键滚内容，画面上却在换剧。
     */
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.altKey || event.ctrlKey || event.metaKey) return;
      const direction: FeedSeriesDirection | null =
        event.key === 'ArrowUp' ? 'previous' : event.key === 'ArrowDown' ? 'next' : null;
      if (!direction) return;
      if (blocksFeedKeys(event.target) || blocksFeedKeys(document.activeElement)) return;
      fire(direction, event);
    };
    const onPointerDown = (event: PointerEvent) => {
      if (event.button !== 0 || isInteractiveTarget(event.target)) {
        start.valid = false;
        return;
      }
      start.x = event.clientX;
      start.y = event.clientY;
      start.valid = true;
    };
    const onPointerUp = (event: PointerEvent) => {
      if (!start.valid) return;
      start.valid = false;
      fire(feedGestureDirection(event.clientX - start.x, event.clientY - start.y));
    };
    const onPointerCancel = () => {
      start.valid = false;
    };
    const onTouchStart = (event: TouchEvent) => {
      if (isInteractiveTarget(event.target) || event.touches.length !== 1) {
        start.valid = false;
        return;
      }
      start.x = event.touches[0].clientX;
      start.y = event.touches[0].clientY;
      start.valid = true;
    };
    const onTouchEnd = (event: TouchEvent) => {
      if (!start.valid || event.changedTouches.length === 0) return;
      start.valid = false;
      fire(feedGestureDirection(
        event.changedTouches[0].clientX - start.x,
        event.changedTouches[0].clientY - start.y,
      ));
    };

    window.addEventListener('wheel', onWheel, { passive: false });
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener('pointerdown', onPointerDown);
    window.addEventListener('pointerup', onPointerUp);
    window.addEventListener('pointercancel', onPointerCancel);
    window.addEventListener('touchstart', onTouchStart, { passive: true });
    window.addEventListener('touchend', onTouchEnd, { passive: true });
    return () => {
      window.removeEventListener('wheel', onWheel);
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('pointerdown', onPointerDown);
      window.removeEventListener('pointerup', onPointerUp);
      window.removeEventListener('pointercancel', onPointerCancel);
      window.removeEventListener('touchstart', onTouchStart);
      window.removeEventListener('touchend', onTouchEnd);
    };
  }, [active, feed.switchSeries]);

  return (
    <div
      className={`absolute inset-0 z-50 overflow-hidden select-none ${active ? 'block' : 'hidden'}`}
      style={{ pointerEvents: 'none', touchAction: 'pan-x' }}
      aria-hidden={!active}
      inert={!active}
    >
      {active && <FeedCompletionBridge onFinished={feed.advanceAfterFinished} />}
      <div className="pointer-events-none absolute inset-x-0 top-0 z-10 h-24 bg-gradient-to-b from-black/65 to-transparent" aria-hidden="true" />
      <div className="pointer-events-none absolute inset-x-0 bottom-0 z-10 h-48 bg-gradient-to-t from-black/70 to-transparent" aria-hidden="true" />

      {/*
        封面占位与状态横幅收进 `FeedStageOverlay`：只有它们需要 `isBuffering`
        这个每秒会变的信号，让它们各自订阅，FeedView 主体就不必跟着播放进度重渲染。
      */}
      <FeedStageOverlay
        feed={feed}
        canSwitchSeries={canSwitchSeries}
      />

      <h1 ref={titleRef} tabIndex={-1} className="sr-only">
        短剧推荐
      </h1>
      {/*
        右上角**只有**这一组频道切换。
        用户截图里的「原生音放」图标与「● 原生 mpv 播放中·联播 开」徽标都来自
        `PlayerControls`，它已在 feed 视图下整块不渲染 —— 本页不再补任何播放器
        状态文字：状态由中间的状态横幅说，右上只负责「换哪个频道」。
        托盘是单一容器（`rounded-xl` + `bg-black/40` + `border-white/20`），
        三颗 tab 用同一尺寸（`h-8 min-w-14`），避免第二版那种"一行三组东西"。
      */}
      <div
        className="pointer-events-auto absolute right-4 top-4 z-50 flex items-center gap-1 rounded-xl border border-white/20 bg-black/40 p-1 shadow-lg backdrop-blur-xl"
        role="group"
        aria-label="推荐频道"
        data-feed-no-swipe
        data-feed-channel-tray
      >
        <button
          type="button"
          onClick={() => feed.selectChannel('drama')}
          aria-pressed={feed.channel === 'drama'}
          className={`h-8 min-w-14 rounded-lg px-2 text-xs font-semibold transition-all duration-200 ease-[cubic-bezier(0.22,1,0.36,1)] hover:scale-[1.03] active:scale-95 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 focus-visible:ring-offset-2 focus-visible:ring-offset-black/60 ${feed.channel === 'drama' ? 'bg-white text-slate-900 shadow-sm' : 'text-white hover:bg-white/15'}`}
        >
          短剧
        </button>
        <button
          type="button"
          onClick={() => feed.selectChannel('comic')}
          aria-pressed={feed.channel === 'comic'}
          className={`h-8 min-w-14 rounded-lg px-2 text-xs font-semibold transition-all duration-200 ease-[cubic-bezier(0.22,1,0.36,1)] hover:scale-[1.03] active:scale-95 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 focus-visible:ring-offset-2 focus-visible:ring-offset-black/60 ${feed.channel === 'comic' ? 'bg-white text-slate-900 shadow-sm' : 'text-white hover:bg-white/15'}`}
        >
          漫剧
        </button>
        {/*
          AI 剧频道（2026-11-10 补入口）。
          
          数据侧早就通了——`FeedChannel` 已扩成三值，`feedPageSource('ai')` 走
          `web-catalog`，用 `/rank/hot-ai-drama` 打头再接 `/category/ai-drama`。
          缺的只是这颗按钮：没有它，用户点不进 AI 剧推荐流，那套分页逻辑等于
          白写。
          
          文案取自 `feed.channelCopy.note`（「按站点热度排列，不是个性化推荐」），
          不在这里硬编码——C 线把这段说明做成了 hook 返回值的一部分，就是为了
          避免"AI 剧推荐"被读成个性化算法。
        */}
        <button
          type="button"
          onClick={() => feed.selectChannel('ai')}
          aria-pressed={feed.channel === 'ai'}
          title={feed.channelCopy?.note}
          className={`h-8 min-w-14 rounded-lg px-2 text-xs font-semibold transition-all duration-200 ease-[cubic-bezier(0.22,1,0.36,1)] hover:scale-[1.03] active:scale-95 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90 focus-visible:ring-offset-2 focus-visible:ring-offset-black/60 ${feed.channel === 'ai' ? 'bg-white text-slate-900 shadow-sm' : 'text-white hover:bg-white/15'}`}
        >
          AI 剧
        </button>
      </div>

      <FeedSlide
        // `key` 绑定剧 id：换一部时重建组件，把「简介」浮层收起。
        // 没有这个 key，`useState` 会跟着组件实例留在上一部剧上——用户换到下一部
        // 看到的是一段别人的简介浮层。翻集（同一部）时 key 不变，浮层状态保留，
        // 因为简介本来就是同一条。
        key={displaySeries?.id ?? 'feed-slide-empty'}
        series={displaySeries}
        episodeNumber={episodeNumber}
        episodeTitle={currentEpisode?.title ?? ''}
        playableEpisodes={feed.detail?.playableEpisodes}
        slideDirection={slideDirection}
      />

      {/* 状态区已经随封面一起收进上面的 `FeedStageOverlay`（同一个 `phase`
          推导，只是搬到了会订阅 `isBuffering` 的那一层）。 */}

      {/* 推荐页专用底部条。它自己订阅 `position`/`duration`（高频），
          FeedView 只把动作与低频状态递下去 —— 否则整棵推荐页会跟着播放进度
          每秒重渲染 4 次，上一轮刚把这个订阅下沉掉。 */}
      <FeedPlaybackBar
        canGoNext={canGoNext}
        onNextEpisode={feed.nextEpisode}
        isFullscreen={isFullscreen}
        onToggleFullscreen={toggleFullscreen}
      />

      <FeedControls
        episodeIndex={feed.episodeIndex}
        episodeNumber={episodeNumber}
        episodeCount={feed.detail?.episodes.length ?? 0}
        totalEpisodes={feed.detail?.episodesCount ?? feed.currentItem?.episodesCount ?? 0}
        favoriteMark={feed.favoriteMark}
        busy={busy}
        canSwitchSeries={canSwitchSeries}
        title={panelTitle}
        description={panelDescription}
        tags={panelTags}
        origin={panelOrigin}
        playableEpisodes={feed.detail?.playableEpisodes}
        onPrevious={feed.previousEpisode}
        onNext={feed.nextEpisode}
        onSwitchSeries={() => feed.switchSeries('next')}
        onToggleFavorite={feed.toggleFavorite}
        onClose={feed.close}
      />
    </div>
  );
};