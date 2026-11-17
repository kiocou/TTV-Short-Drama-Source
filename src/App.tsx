import React, { Suspense, lazy, useEffect, useRef, useState } from 'react';
import { AppProvider, useAppStore } from './stores/useAppStore';
import { CatalogProvider } from './stores/useCatalogStore';
import { PlaybackProvider, usePlaybackActions } from './stores/usePlaybackStore';
import { AnimePlayerProvider, useAnimePlayerActions } from './stores/useAnimePlayerStore';
import { HistoryProvider } from './stores/useHistoryStore';
import { FavoritesProvider } from './stores/useFavoritesStore';
import { SettingsProvider } from './stores/useSettingsStore';
import { AgentWatchProvider } from './stores/useAgentWatchStore';
import { leaveFullscreen } from './services/windowFx';
import { isTauriEnvironment } from './services/ipc';

import { TitleBar } from './components/layout/TitleBar';
import { NavigationRail } from './components/layout/NavigationRail';
import { ToastContainer } from './components/layout/ToastContainer';

import { ExploreView } from './components/views/ExploreView';
import { VideoSurface } from './components/player/VideoSurface';
import { HuangguoGateProvider } from './components/huangguo/HuangguoGate';
import { PipReturnBridge } from './components/player/PipReturnBridge';
import { AgentCallCard } from './components/player/AgentCallCard';
import { LaunchAnimation } from './components/layout/LaunchAnimation';

// 非首屏视图按首次访问懒加载并常驻保留（访问后不卸载），消减冷启动 JS 解析与无关挂载开销。
const AnimeView = lazy(() => import('./components/views/AnimeView').then(m => ({ default: m.AnimeView })));
const DetailView = lazy(() => import('./components/views/DetailView').then(m => ({ default: m.DetailView })));
const HistoryView = lazy(() => import('./components/views/HistoryView').then(m => ({ default: m.HistoryView })));
const FavoritesView = lazy(() => import('./components/views/FavoritesView').then(m => ({ default: m.FavoritesView })));
const SettingsView = lazy(() => import('./components/views/SettingsView').then(m => ({ default: m.SettingsView })));
const SearchView = lazy(() => import('./components/views/SearchView').then(m => ({ default: m.SearchView })));
const ShelfMoreView = lazy(() => import('./components/views/ShelfMoreView').then(m => ({ default: m.ShelfMoreView })));
const SpotlightView = lazy(() => import('./components/views/SpotlightView').then(m => ({ default: m.SpotlightView })));
const HuangguoView = lazy(() => import('./components/views/HuangguoView').then(m => ({ default: m.HuangguoView })));
const FeedView = lazy(() => import('./components/views/FeedView').then(m => ({ default: m.FeedView })));
const AnimeVideoSurface = lazy(() => import('./components/player/AnimeVideoSurface').then(m => ({ default: m.AnimeVideoSurface })));

const AppContent: React.FC = () => {
  const { currentView, selectedSeriesId, isFullscreen, setIsFullscreen } = useAppStore();
  const [mountedViews, setMountedViews] = useState<Set<string>>(() => new Set(['explore']));
  const currentViewRef = useRef(currentView);
  currentViewRef.current = currentView;
  useEffect(() => {
    setMountedViews(prev => (prev.has(currentView) ? prev : new Set([...prev, currentView])));
  }, [currentView]);
  // 发现页首屏稳定后在后台静默预载推荐页 JS 与首屏列表/详情/预签名，缩短首次切入等待。
  useEffect(() => {
    if (!isTauriEnvironment()) return;
    const timer = window.setTimeout(() => {
      if (currentViewRef.current !== 'explore') return;
      void import('./components/views/FeedView');
      void import('./stores/useFeedStore').then(({ warmFeedEntry }) => warmFeedEntry({ channel: 'drama' }));
    }, 2200);
    return () => window.clearTimeout(timer);
  }, []);
  /**
   * 只订阅 stopPlayback，**不要** usePlaybackStore()。
   *
   * PlaybackContext 的 value 是内联对象（见 usePlaybackStore 的 Provider），
   * 而 store 里的 position 每次 timeupdate 都变（约 4 次/秒）。订阅整个
   * context 就意味着 AppContent 每秒重渲染 4 次 —— 它是**所有视图的父节点**，
   * 于是发现页的上百张卡片、历史、收藏、设置页每一帧都在跟着 reconcile。
   * 用户感知就是「切页面 / 进出播放器都发涩」。
   * store 早就为此提供了 usePlaybackSelector（只读单个字段），此前一处都没用上。
   */
  const { stopPlayback } = usePlaybackActions();
  const { isOpen: isAnimePlayerOpen } = useAnimePlayerActions();

  const isFeed = currentView === 'feed';
  const isPlayer = currentView === 'player' || isFeed;

  /**
   * 「详情页」这一个视图层的重挂载闸门。
   *
   * ## 为什么不给每个视图层挂 key（这曾是切页卡顿的直接来源）
   *
   * 旧实现给每一层写的是 `key={currentView === 'x' ? 'view-x' : undefined}`。
   * 只有**当前激活**的那层拿到字符串 key，其余 7 层都是 `undefined`。React 的
   * key 一旦变化就销毁重建：从 A 切到 B 时，B 层 `undefined → 'view-b'` 要重挂，
   * A 层 `'view-a' → undefined` **同样**要重挂 —— 一次点击换来整棵视图树的
   * 卸掉再建（含卡片列表、封面 IntersectionObserver、分页游标），主线程被占满，
   * 用户看到的就是"切换页面卡一下"。而且这跟"视图常驻 DOM"的初衷正好相反：
   * 每一层的 useEffect（拉数据、订阅）都被反复重跑。
   *
   * 真正需要重挂的只有一种情况：**换了一部剧**。详情页带着上一个剧的
   * `detail` / `selectedSeriesId` 时切到另一部，必须重新挂载才能拿到干净状态
   * （旧实现靠 `view-detail-${selectedSeriesId}` 达到这个效果，这是**唯一**
   * 值得保留的部分）。换频道、换搜索词、进「更多」页都已有各自的复位逻辑，
   * 不需要靠 key 重建。
   *
   * 这个 key 绝不能写在最外层主工作区上：那里住着播放器宿主，重挂会让 store 里
   * 只绑定一次的媒体事件监听器与 `<video>` 元素脱钩（详见主工作区那段注释的
   * 实机现象）。它只挂在详情页那一层。
   *
   * 同理，它也**不能跟视图走**。旧写法是
   * `currentView === 'detail' ? (selectedSeriesId ?? '') : ''`：detail 时
   * key=剧 id、player 时 key=''，于是「详情页 → 播放器」这一次跳转必然换 key，
   * 播放器宿主跟着被卸载重建。改成只跟**剧**走，进出播放器就不再重挂。
   */
  const workspaceKey = selectedSeriesId ?? '';

  // This is a focused desktop player rather than a browser surface. Prevent the
  // WebView's generic context menu so right-click never exposes browser actions.
  useEffect(() => {
    const preventContextMenu = (event: MouseEvent) => event.preventDefault();
    document.addEventListener('contextmenu', preventContextMenu);
    return () => document.removeEventListener('contextmenu', preventContextMenu);
  }, []);

  // 播放器宿主常驻 DOM，离开时仅被 display:none 隐藏，video 不会自动停。
  // 不显式停止就会出现"回到主界面但声音还在播"（含后台连播倒计时自动开播）。
  //
  // ⚠️ `isAnimePlayerOpen` 必须列进来，且判定是「或」而不是只看视图。
  // 从短剧播放器直接进动漫播放时（详情页/发现页/历史页三条入口都是这个走法），
  // `currentView` 全程停在 `'player'`，`isPlayer` 自始至终是 true —— 旧写法的
  // `if (!isPlayer)` 因此**一次都不会触发**，短剧那条链路（卡死看门狗、连播倒计时、
  // 预解池、在途 resolveNative）全部跨这次「离开」继续存活：倒计时到点会把一部
  // 用户根本看不见的短剧强行开播并出声。动漫侧的 haltCurrent 停的是它自己那块
  // video，不会碰短剧这块。这是不变量 1「同一时刻只有一路在播」的实质缺口。
  useEffect(() => {
    if (!isPlayer || isAnimePlayerOpen) stopPlayback();
  }, [isPlayer, isAnimePlayerOpen, stopPlayback]);

  // 非播放视图下必须退出全屏。
  // 否则用户在全屏播放时返回详情页/发现页，窗口仍停在全屏，整个程序看起来
  // 被"卡"在全屏状态（实测现象：返回详情页后程序仍全屏，底部还留一条黑边——
  // 那是被隐藏的播放器容器）。
  // 走 windowFx.leaveFullscreen：它同时负责还原"进全屏前是否最大化"，
  // 避免退出后窗口比用户预期更小。
  useEffect(() => {
    if (isPlayer || !isFullscreen) return;
    let cancelled = false;
    void (async () => {
      await leaveFullscreen();
      if (!cancelled) setIsFullscreen(false);
    })();
    return () => {
      cancelled = true;
    };
  }, [isPlayer, isFullscreen, setIsFullscreen]);

  return (
    <div
      className="w-full h-full flex flex-col mica-backdrop select-none overflow-hidden"
      onContextMenu={(event) => event.preventDefault()}
    >
      {/*
        全屏时隐藏标题栏，让播放器真正占满整个窗口。
        原生窗口全屏已经让窗口铺满屏幕，此时唯一还挡着画面的就是这条 40px
        标题栏——不隐藏它，用户就会觉得"全屏了但视频没放大"。

        ⚠️ 2026-11-10：条件里**不再**排除 `currentView === 'feed'`。
        旧写法是 `!isFullscreen && currentView !== 'feed'`，代价是从侧栏进
        「短剧推荐」之后整个窗口**没有标题栏**：既没有拖动区，也没有
        最小化 / 最大化 / 关闭三颗按钮，用户只能靠系统热键或别的窗口操作。
        那一行是本页"顶部窗口控制栏缺失"的唯一原因。

        推荐页仍然需要"返回入口与频道"：返回键（左上）与频道托盘（右上）住
        在 `FeedView` 里，**全屏时也照样可用**（全屏时本栏被隐藏，正好没有
        两条控制带叠在一起）。`TitleBar` 内部只按 `currentView === 'feed'`
        换肤色、收搜索框，不复制任何页面级控件——见它自己的注释。
      */}
      {!isFullscreen && <TitleBar />}

      {/* 主工作区：持久化常驻渲染，杜绝切换时的卸载闪屏与白屏。

          **这一层刻意不给 key**：它内部住着播放器宿主（`<VideoSurface/>` 里的
          那块 `<video>`）与动漫宿主。store 的媒体事件监听器只在首次挂载时绑定
          一次（见 usePlaybackStore 的「监听器只绑定一次，永不重绑」），捕获的是
          **那一刻**的 video 元素；一旦 React 因为 key 变化把这块 `<video>` 卸载
          重建，新元素在播、监听器却还挂在被丢弃的旧元素上，
          `setPosition` / `setDuration` 再也不会被调用 —— 表现就是画面明明在走，
          进度条却永远停在 00:00 / 00:00 而且拖不动（用户实机报告的现象）。
          同一原因还会让连播失效（`ended` 绑在旧元素）与「下载中」遮罩不消失
          （`playing` 上的 clearResolveOverlay 绑在旧元素）。

          换剧要重挂的是**视图树**，不是播放器宿主，所以 key 挂到了下面包着
          NavigationRail + main 的那一层。 */}
      <div className={`flex-1 w-full flex overflow-hidden relative ${isFeed && !isFullscreen ? 'bg-[#161823]' : ''}`}>
        <div className={currentView === 'player' || isFullscreen ? 'hidden' : 'flex shrink-0'}>
          <NavigationRail />
        </div>
        <div className={`flex-1 min-w-0 h-full relative flex overflow-hidden ${isFeed && !isFullscreen ? 'p-2.5 pl-0' : ''}`}>
        {/* 播放器宿主常驻 DOM。不要给包含 video 的祖先加 opacity/transform 入场动画：
            fluentScaleIn 的 forwards 会在结束后保留合成状态；实测即使 HUD 已去模糊，
            仍无法触发 NVIDIA VSR。与动漫宿主一样直接显示，控制器自身的动效不受影响。 */}
        {/*
          短剧/漫剧播放器。动漫播放期间一并隐藏：动漫走的是另一块 `<video>`
          （AnimeVideoSurface），两块媒体元素同时活跃会出现"两个声音"、
          MSE 互相抢占等难以排查的状态。
        */}
        <div
          className={
            isPlayer && !isAnimePlayerOpen
              ? isFeed && !isFullscreen
                ? 'relative w-full h-full rounded-2xl overflow-hidden bg-black ring-1 ring-white/[0.08] pointer-events-auto z-30'
                : 'w-full h-full absolute inset-0 z-30 block pointer-events-auto'
              : 'w-full h-full absolute inset-0 z-30 hidden pointer-events-none -z-10'
          }
        >
          <VideoSurface />
          {mountedViews.has('feed') && (
            <Suspense fallback={null}>
              <FeedView />
            </Suspense>
          )}
        </div>

        {/*
          动漫专区专用播放器：**按需挂载**。
          动漫源的挂载方式在 hls.js(MSE) 与原生 src 之间来回切，元素上容易留下
          残留状态；用"退出即销毁"代替"复用常驻元素并小心清理"，少一类事故。
        */}
        {isAnimePlayerOpen && (
          <div className="w-full h-full absolute inset-0 z-40">
            <Suspense fallback={null}>
              <AnimeVideoSurface />
            </Suspense>
          </div>
        )}

        {/* 业务主画板：常驻 DOM，视图切换平滑带动画。 */}
        <div
          className={`flex-1 w-full h-full flex overflow-hidden ${
            isPlayer ? 'hidden' : 'flex'
          }`}
        >
          <main 
            style={{ contain: 'layout paint' }}
            data-launch-part="content"
            className="ttv-workspace-panel flex-1 min-w-0 my-4 mr-4 overflow-hidden relative bg-white/50"
          >
            <div
              className={`h-full w-full ${currentView === 'explore' ? 'block animate-fluent-page-in' : 'hidden'}`}
            >
              <ExploreView />
            </div>
            {mountedViews.has('anime') && (
              <div
                className={`h-full w-full ${currentView === 'anime' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <AnimeView />
                </Suspense>
              </div>
            )}
            {/*
              详情页是唯一需要 key 的视图层，换剧才重挂。
            */}
            {mountedViews.has('detail') && (
              <div
                key={workspaceKey}
                className={`h-full w-full ${currentView === 'detail' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <DetailView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('history') && (
              <div
                className={`h-full w-full ${currentView === 'history' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <HistoryView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('favorites') && (
              <div
                className={`h-full w-full ${currentView === 'favorites' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <FavoritesView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('settings') && (
              <div
                className={`h-full w-full ${currentView === 'settings' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <SettingsView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('search') && (
              <div
                className={`h-full w-full ${currentView === 'search' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <SearchView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('shelf') && (
              <div
                className={`h-full w-full ${currentView === 'shelf' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <ShelfMoreView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('spotlight') && (
              <div
                className={`h-full w-full ${currentView === 'spotlight' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <SpotlightView />
                </Suspense>
              </div>
            )}
            {mountedViews.has('huangguo') && (
              <div
                className={`h-full w-full ${currentView === 'huangguo' ? 'block animate-fluent-page-in' : 'hidden'}`}
              >
                <Suspense fallback={null}>
                  <HuangguoView />
                </Suspense>
              </div>
            )}
          </main>
        </div>
        </div>
      </div>

      {/* 全局 Toast 通知容器 */}

      {/* 画中画小窗回流：小窗关闭时接回播放/落历史（不渲染任何界面） */}
      <PipReturnBridge />

      {/*
        智能体联动：呼叫卡（播放中收到事件时浮出）。
        收起后的常驻卡片**不在这里**——它是独立的置顶窗口（agent_dock.rs），
        必须能在用户切到智能体工具之后仍然浮在桌面上。
      */}
      <AgentCallCard />
      <ToastContainer />

      {/*
        启动进入动画（方案 05「轨道汇聚」）。放在最后 = 盖在 TitleBar 与更新弹窗之上。
        它自己管生命周期：动画跑完（或点击跳过）后 unmount，不留任何常驻 DOM 与内联样式。
        **不要**把它改成常驻 + display:none —— 那样会在"隐藏祖先里创建动画"，
        命中项目里已经记录过的坑（动画永久卡在 0% 帧）。
      */}
      <LaunchAnimation />
    </div>
  );
};

export const App: React.FC = () => {
  return (
    <AppProvider>
      <SettingsProvider>
        <CatalogProvider>
          <PlaybackProvider>
            <HistoryProvider>
              <FavoritesProvider>
                <AnimePlayerProvider>
                  <AgentWatchProvider>
                    {/* 闸门要在 AppProvider 内（它用 navigateTo/goBack），
                        又要包住 TitleBar 与 HuangguoView 两端。 */}
                    <HuangguoGateProvider>
                      <AppContent />
                    </HuangguoGateProvider>
                  </AgentWatchProvider>
                </AnimePlayerProvider>
              </FavoritesProvider>
            </HistoryProvider>
          </PlaybackProvider>
        </CatalogProvider>
      </SettingsProvider>
    </AppProvider>
  );
};

export default App;
