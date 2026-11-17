import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { usePlaybackStore } from '../../stores/usePlaybackStore';
import { useSettingsStore } from '../../stores/useSettingsStore';
import { ipcService, isTauriEnvironment } from '../../services/ipc';
import { useAppStore } from '../../stores/useAppStore';
import {
  NATIVE_OPEN_FAILED_NOTICE,
  planPlaybackRoute,
} from '../../services/playbackRouting';
import {
  releaseHeldNativeAddress,
  releaseNativeAddress,
  resolveNativeAddress,
} from '../../services/nativeAddress';
import { PlayerHud } from './PlayerHud';
import type { DanmakuSettings } from './PlayerHud';
import { Zap } from 'lucide-react';
import { readTheme } from '../../services/theme';
import { NATIVE_PLAYER_AVAILABLE } from '../../services/nativePlayerRollout';
import {
  interpolationPresetFor,
  readSavedNativeFgMode,
  readSavedNativeFgMult,
  saveNativeFgPreferences,
} from '../../services/preferredPlayback';
import { createSerializedPoll, runNativeHandoffPoll } from '../../services/nativeExitPlan';
import {
  NATIVE_STATUS_POLL_MS,
  buildInitialNativeQueue,
  registerNativeNavigateHost,
  releaseNativeNavigateHost,
  isNativeNavigateHostActive,
  startNativeNavigateBridge,
  type NativeNavigateHost,
} from '../../services/nativeNavigate';

/**
 * 短剧 / 漫剧播放器的控制层适配器。
 *
 * 外观已经全部搬到 `PlayerHud`（与动漫专区共用同一套晶体材质），这里只剩两件事：
 * 从 `usePlaybackStore` 取状态、把用户操作映射回 store 的方法。加新样式请改
 * PlayerHud——两个播放器的观感从此只有一处来源。
 */

/**
 * 「原生播放」入口开关（mpv + LSFG 补帧那套独立播放器）。
 *
 * ## 恢复记录（2026-11-10）
 *
 * 2026-11-01 曾按需求**暂时搁置**这个入口（当时原生播放器还没重设计完），
 * 现在按 `docs/native-player-redesign.md` 恢复：控制坞、联播队列、导航桥都已就位，
 * 入口重新打开。
 *
 * ## 样式坑（恢复时必须遵守，别重犯）
 *
 * 上一版按钮复用了 `ttv-lock crystal-surface`。`ttv-lock` 是给**正方形图标按钮**
 * （右侧居中的锁）写的：`width: 42px; height: 42px; border-radius: 50%`。
 * 塞进「原生播放」四个字之后宽度被压成 42px，中文被迫竖排折行，再贴上窗口右上角，
 * 看起来就是一块被裁掉的白方块 —— 这就是它当时"显得难看"的直接原因。
 *
 * 所以这里给入口**独立类名 `ttv-native-entry`**（样式见下方 JSX 的内联 style）
 * 并且：`whiteSpace: nowrap` 保证横排、`padding: 6px 12px` 给文字留横向空间、
 * `fontSize: 12px` 与 HUD 其它次要按钮同号、离右上角留 12px 内边距（不贴边裁切）。
 *
 * 为什么不把样式写进 CSS：`src/styles/**` 不在本次施工边界内（见
 * `docs/native-player-redesign.md` §5 的 react-hud 行），所以 `ttv-native-entry`
 * 只作为**语义钩子**存在，真正生效的规则是内联 style。将来若要收进 CSS，
 * 直接把内联搬过去即可，类名不用改。
 */
const NATIVE_PLAYER_ENTRY_ENABLED = true;

/**
 * 「原生播放」按钮的样式。
 *
 * 单独抽成常量而不是散在 JSX 里：这个按钮踩过一次"宽度被类名压死"的坑，
 * 尺寸相关的四个值（width/height 不设死、whiteSpace、padding、fontSize）
 * 必须放在一起看得见。
 */
const NATIVE_ENTRY_STYLE: React.CSSProperties = {
  position: 'absolute',
  // 与窗口右上角留 12px 内边距：贴边会被圆角/窗口边框裁掉一截。
  top: 12,
  right: 12,
  zIndex: 40,
  // 不用 flex 容器的宽度约束，改成 inline-flex + nowrap：
  // 宽度由「图标 + 四个字 + padding」自己撑开，永远不会被压成方块。
  display: 'inline-flex',
  alignItems: 'center',
  gap: 6,
  padding: '6px 12px',
  whiteSpace: 'nowrap',
  fontSize: 12,
  lineHeight: 1.2,
  borderRadius: 10,
  color: '#E6EDF5',
  fontWeight: 500,
  cursor: 'pointer',
  // 深色播放器底上的一层薄晶体：同 crystal-surface 的观感，但由这里兜底，
  // 保证即使 crystal.css 的变量在别的宿主里没定义也不会变成一块白板。
  backgroundColor: 'rgba(20, 26, 34, 0.72)',
  border: '1px solid rgba(148, 163, 184, 0.28)',
  boxShadow: '0 8px 24px -10px rgba(0, 0, 0, 0.65)',
  backdropFilter: 'blur(10px) saturate(160%)',
  WebkitBackdropFilter: 'blur(10px) saturate(160%)',
};

interface PlayerControlsProps {
  isVisible: boolean;
  isFullscreen: boolean;
  /** 控制器是否被用户主动收起（锁定）。 */
  isLocked: boolean;
  onToggleLock: () => void;
  onToggleFullscreen: () => void;
  onUserActivity: () => void;
  /** 指针移动统一入口（带合成事件过滤），容器层已按真实位移过滤后传入。 */
  onPointerMove?: (e: React.MouseEvent) => void;
  /** 长按方向键加速时实际生效的速率（未加速为 null），仅用于展示。 */
  effectivePlaybackRate?: number | null;
  /** 短剧推荐有独立的标题 / 返回层。 */
  feedMode?: boolean;
  onFloatingUiChange?: (hasFloatingUi: boolean) => void;
  onLockHoverChange?: (hovered: boolean) => void;
}

export const PlayerControls: React.FC<PlayerControlsProps> = ({
  isVisible,
  isFullscreen,
  isLocked,
  onToggleLock,
  onToggleFullscreen,
  onUserActivity,
  onPointerMove,
  effectivePlaybackRate = null,
  feedMode = false,
  onFloatingUiChange,
  onLockHoverChange,
}) => {
  const {
    currentSeries,
    currentEpisode,
    isPlaying,
    position,
    duration,
    buffered,
    volume,
    isMuted,
    playbackRate,
    currentQuality,
    togglePlay,
    seek,
    seekRelative,
    setVolume,
    toggleMute,
    setPlaybackRate,
    setQuality,
    vsrEnabled,
    setVsrEnabled,
    danmakuEnabled,
    setDanmakuEnabled,
    playNextEpisode,
    playPrevEpisode,
    openEpisode,
    toggleSideDrawer,
    availableQualities,
    enterPip,
  } = usePlaybackStore();

  // 补帧入口已在发布前移除（独立窗口方案不成熟：需要另起 D3D11 窗口、
  // 与主窗口两套音频/进度，且调试徽标漏在界面上）。后端 `frame_interp_set`
  // 命令保留但不再被前端调用。
  const { goBack, currentView, navigateTo, showToast } = useAppStore();

  /**
   * 弹幕观感设置（显示区域/不透明度/字号/速度/字体/粗体/描边）。
   *
   * 设置真相在 `useSettingsStore`，「弹幕设置」面板写它；`PlayerHud` 是纯呈现层
   * （文件头写明的边界：不读任何 store），所以在这里读好、以 props 喂下去。
   * 改动即时生效：滑条每动一格都走 `updateSettings`（面板当下重渲染 +
   * 异步落库），不需要"确认"按钮。
   */
  const { settings, updateSettings } = useSettingsStore();
  const danmakuAppearance: DanmakuSettings = {
    danmakuArea: settings.danmakuArea,
    danmakuOpacity: settings.danmakuOpacity,
    danmakuFontScale: settings.danmakuFontScale,
    danmakuSpeed: settings.danmakuSpeed,
    danmakuFont: settings.danmakuFont,
    danmakuColor: settings.danmakuColor,
    danmakuBold: settings.danmakuBold,
    danmakuStroke: settings.danmakuStroke,
  };

  // -------------------------------------------------------------------------
  // 原生播放器（mpv）状态与联播
  // -------------------------------------------------------------------------

  /** 原生播放器窗口是否在播。2 秒轮询一次 `native_player_status`。 */
  const [nativeAlive, setNativeAlive] = useState(false);
  /**
   * 本次"把画面交给原生窗口"的交接凭据（`handleOpenNative` 成功后写入）。
   *
   * ## 为什么需要它（2026-11-10 修「两种入口都播不了」）
   *
   * `handleOpenNative` 成功那一刻会按停本页 `<video>`（防两路声音）。而
   * `nativeOpen` 的返回值只说明**进程 spawn 成功**，不说明窗口活着 —— 播放器
   * 完全可能在几百毫秒后消失，主进程这边收不到任何异常。那时本页已按停、
   * 原生窗口已死，界面上既没画面也没提示。
   *
   * 凭据里三个字段各管一件事：
   *   * `episodeId` —— 交接的是**哪一集**。轮询回来时若当前集已经变了，这一轮
   *     结果与当前画面无关，绝不能把新一集按旧凭据播回来（防串集）。
   *   * `at` —— 交接时刻。轮询是 2 秒一拍且**可能在交接前就发出去了**，
   *     那种"过期回执"必须丢弃，否则会拿旧会话的 `alive:false` 误判"刚开的窗口死了"。
   *   * `sawAlive` —— 是否见过它真的活过一次。只有活过又消失才谈得上"异常退出"；
   *     从没见过活，说明它根本没起来。
   */
  const nativeHandoffRef = useRef<{
    episodeId: string;
    at: number;
    sawAlive: boolean;
  } | null>(null);
  /** 原生播放器侧的联播开关（用于徽标文案；真相在播放器与 settings.autoNext）。 */
  const [nativeAutoplay, setNativeAutoplay] = useState(true);
  /**
   * 原生窗口靠联播推进到了第几集（`null` = 没有待同步）。
   *
   * 只记集号、不立刻开播：见 `NativeNavigateHost.noteAdvance` 的说明 ——
   * 前端此刻看不到自己的画面，为它多跑一次解析只会和原生播放抢编码单元。
   * 用户关掉原生窗口（`syncAfterNativeExit`）时才一次性对齐。
   */
  const pendingNativeEpisodeRef = useRef<number | null>(null);

  /**
   * 最新快照。
   *
   * 导航桥（`nativeNavigate.ts`）是纯模块、只注册一次，事件到达时读不到渲染作用域
   * 里的变量；所有"跨回合要给 bridge 看"的值都从这里取最新的一份。用 effect 写入
   * （而不是渲染期赋值）：渲染期写 ref 会在并发渲染下读到未提交的值。
   */
  const latestRef = useRef<{
    series: typeof currentSeries;
    episode: typeof currentEpisode;
    isPlaying: boolean;
    volume: number;
    position: number;
    openEpisode: typeof openEpisode;
    togglePlay: () => void;
    updateSettings: typeof updateSettings;
  } | null>(null);

  useEffect(() => {
    latestRef.current = {
      series: currentSeries,
      episode: currentEpisode,
      isPlaying,
      volume,
      position,
      openEpisode,
      togglePlay,
      updateSettings,
    };
  }, [currentSeries, currentEpisode, isPlaying, volume, position, openEpisode, togglePlay, updateSettings]);

  /**
   * 原生播放器的状态轮询。
   *
   * 为什么轮询而不是订阅事件：`native_player_status` 是既有的单点查询（命令层
   * 直接读 Rust 里的 `PlayerState`），播放器的 progress 心跳**没有**单独 emit 给
   * 前端；为一个徽标再接一条事件流，等于让整条播放链路多一个必须同步的契约。
   * 2 秒一次、只读三个字段，开销可以忽略。
   *
   * 非 Tauri 环境直接跳过：浏览器里没有原生播放器，`nativeStatus()` 只会返回
   * `alive: false` 的假数据，轮询毫无意义。
   */
  useEffect(() => {
    if (!isTauriEnvironment()) return;
    /**
     * 串行 + 可取消的一次轮询。
     *
     * `setInterval` 只保证按时发起，不保证上一拍已经回来；IPC 稍慢就会叠出两三拍
     * 在飞，乱序回执会把结论互相覆盖。`createSerializedPoll` 的做法是**丢弃**多余
     * 的一拍（2 秒后还有下一拍，丢一拍无损失），并在卸载后既不再发起、也让在飞的
     * 那一拍在产生副作用前（读状态之后、真正 play 之前）被 `isValid()` 拦住 ——
     * 否则离开播放页之后，迟到的 `died` 会去播别的页面的那块 `<video>`。
     */
    const runner = createSerializedPoll(async (isValid) => {
      const { decision, status } = await runNativeHandoffPoll({
        readStatus: async () => {
          try {
            const status = await ipcService.playback.nativeStatus();
            if (!isNativeNavigateHostActive(navigateHost)) {
              nativeHandoffRef.current = null;
              pendingNativeEpisodeRef.current = null;
            }
            return status;
          } catch {
            return null;
          }
        },
        now: () => Date.now(),
        currentEpisodeId: () => latestRef.current?.episode?.id ?? null,
        hasPendingAdvance: () => pendingNativeEpisodeRef.current !== null,
        syncAfterNativeExit: () => navigateHost.syncAfterNativeExit(),
        resumeInline: async () => {
          const host = document.querySelector<HTMLVideoElement>('video');
          if (!host || !host.currentSrc) return false;
          await host.play();
          return true;
        },
        log: (message) => {
          void ipcService.diagnostics.uiLog(message);
        },
        setAlive: (alive) => setNativeAlive(alive),
        isValid,
      }, nativeHandoffRef);
      if (!isValid()) return;
      // 联播开关与 alive 同源：过期回执同样不许拿它覆盖当前值。
      if (decision !== 'stale' && status) {
        setNativeAutoplay(status.autoplay !== false);
        if (status.alive) {
          saveNativeFgPreferences(status.fgMult, status.fgMode);
        }
      }
    });
    void runner.tick().catch(() => undefined);
    const timer = window.setInterval(() => {
      void runner.tick().catch(() => undefined);
    }, NATIVE_STATUS_POLL_MS);
    return () => {
      runner.cancel();
      window.clearInterval(timer);
    };
  }, []);

  /**
   * 「原生在播 → 内嵌必须安静」的守门员。
   *
   * 原生播放器接管画面后，本页的 `<video>` 还在。它一旦出声就是两路音频叠加
   * （用户最容易注意到的缺陷）。这里做成**状态驱动**而不是命令式暂停：
   * 无论是 `openEpisode` 内部把播放拉起来、还是别的地方调了 `play()`，
   * 只要原生窗口活着且内嵌在播，就立刻按停。
   */
  useEffect(() => {
    if (!nativeAlive || !isPlaying) return;
    togglePlay();
  }, [nativeAlive, isPlaying, togglePlay]);

  /**
   * 导航桥的宿主句柄。
   *
   * 只创建一次（依赖数组为空），内部一律读 `latestRef`，所以剧集切换不会让桥
   * 重新注册 —— 重新注册会留下短暂的"没有监听器"窗口，正好落在换集那一刻。
   */
  const navigateHost = useMemo<NativeNavigateHost>(() => ({
    getEpisodes: () => latestRef.current?.series?.episodes ?? [],
    getCurrentIndex: () => {
      const snapshot = latestRef.current;
      if (!snapshot?.series || !snapshot.episode) return -1;
      const pending = pendingNativeEpisodeRef.current;
      if (pending !== null) return snapshot.series.episodes.findIndex(item => item.episodeNumber === pending);
      return snapshot.series.episodes.findIndex(item => item.id === snapshot.episode?.id);
    },
    getSeriesId: () => latestRef.current?.series?.id ?? null,
    getSeriesTitle: () => latestRef.current?.series?.title ?? '',
    getSeriesType: () => latestRef.current?.series?.type ?? null,
    getVolume: () => latestRef.current?.volume ?? 0.8,
    getPosition: () => latestRef.current?.position ?? 0,
    /**
     * 切到第 index 集。
     *
     * 只记录目标集；原生已接管播放，内嵌源等关窗时再一次性同步。
     */
    switchToEpisode: (index: number, startSeconds: number) => {
      const snapshot = latestRef.current;
      if (!snapshot?.series) return;
      const target = snapshot.series.episodes[index];
      if (!target) return;
      pendingNativeEpisodeRef.current = target.episodeNumber;
    },
    ensureEmbeddedPaused: () => {
      const snapshot = latestRef.current;
      if (snapshot?.isPlaying) snapshot.togglePlay();
    },
    noteAdvance: (ep: number) => {
      pendingNativeEpisodeRef.current = ep;
    },
    /**
     * 关掉原生窗口时把内嵌对齐到"原生最后看到的那一集"。
     *
     * 为什么必须在这里做：原生接管期间内嵌是暂停的，`currentEpisode` 还停在旧那集；
     * 用户关掉原生窗口后如果直接 play()，看到的就是从头几集开始播 —— 那比停住更糟。
     */
    syncAfterNativeExit: () => {
      const snapshot = latestRef.current;
      const pending = pendingNativeEpisodeRef.current;
      pendingNativeEpisodeRef.current = null;
      if (!snapshot?.series) return;
      if (pending !== null) {
        const target = snapshot.series.episodes.find(item => item.episodeNumber === pending);
        if (target) {
          // openEpisode 自己会起播；这里不能再 togglePlay，否则刚起播就被按停。
          void snapshot.openEpisode(snapshot.series.id, target.id, 0);
          return;
        }
      }
      if (!snapshot.isPlaying) snapshot.togglePlay();
    },
    setAutoNext: (on: boolean) => {
      latestRef.current?.updateSettings({ autoNext: on });
    },
    log: (message: string) => {
      void ipcService.diagnostics.uiLog(message);
    },
  }), []);

  /**
   * 挂载导航桥。
   *
   * 宿主就是这里（`PlayerControls` 在播放页常驻），而不是 App 壳层：播放页之外
   * 没有剧集上下文，bridge 拿到事件也只能丢弃。`startNativeNavigateBridge()`
   * 内部是单例 + 引用计数，重复挂载不会重复 `listen`。
   */
  useEffect(() => {
    if (!isTauriEnvironment()) return;
    registerNativeNavigateHost(navigateHost);
    const stop = startNativeNavigateBridge();
    return () => {
      stop();
      releaseNativeNavigateHost(navigateHost);
    };
  }, [navigateHost]);

  /**
   * 关闭原生播放器，把画面交回内嵌播放。
   *
   * 徽标点一下就回到「本页播放」：先 `nativeClose`（幂等），再把内嵌对齐到
   * "原生最后看到的那一集"并恢复播放 —— 否则用户会对着一个静止画面（或者更糟，
   * 从旧的那一集重新开始），以为关掉原生播放器就播不了了。
   */
  const handleExitNative = useCallback(() => {
    void ipcService.playback.nativeClose()
      .catch(() => undefined)
      .finally(() => {
        // 先落 alive=false：守门员 effect 才不会把接下来恢复的播放立刻按停。
        setNativeAlive(false);
        // 随窗口存活的地址会话（`guo:`）到这里才收：早一步就把画面掐了。
        void releaseHeldNativeAddress();
        navigateHost.syncAfterNativeExit();
      });
  }, [navigateHost]);

  /**
   * 进入画中画。
   *
   * 交接成功后**主窗口不离开播放器视图**，而是整窗隐藏（由 `PipReturnBridge`
   * 在收到 `pip://shown` 时执行）。这样"同一时刻只有一个窗口"：小窗先出画，
   * 主窗口再退场。
   *
   * 旧实现交接成功后立刻 `goBack()` 回主界面 —— 那正是空档的来源：播放界面
   * 先消失、主界面露出来，小窗还在加载，过一会儿才出现。
   *
   * 交接失败（后端拒绝、非桌面环境）则原地不动：把用户扔到一个空页面上更糟。
   */
  const handleEnterPip = async () => {
    await enterPip();
  };

  /**
   * 用原生播放器（mpv + LSFG 补帧）打开当前这一集，并**带上联播队列**。
   *
   * 为什么需要这条入口：WebView2 里的 `<video>` 只能播 H.264，红果 HEVC+CENC 源
   * 必须先整集转码（首帧 3~6 秒），而且补帧引擎根本捕获不到 WebView2。
   * 原生播放器直吃直链、自带补帧，首帧能进 1 秒内。
   *
   * 为什么必须带队列：不带队列时原生播放器播完这一集就停住，用户得回主窗口再点
   * 一集 —— 那不叫"播放器"，叫"单集播放器"。队列按 `docs/native-player-redesign.md`
   * §3.1 的 `queue.items[]` 构造，当前集也在里面（用 `index` 指明从哪一项开始）。
   *
   * 打开后**暂停本窗口的播放**：否则两套音频会同时出声（守门员 effect 还会兜底）。
   */
  const handleOpenNative = async () => {
    if (!NATIVE_PLAYER_AVAILABLE) return;
    /**
     * 推荐页（feed）里**绝不**打开原生 mpv 窗口 —— 第二道闸。
     *
     * 第一道闸在 `VideoSurface` 的渲染点（`currentView !== 'feed' && <PlayerControls/>`），
     * 推荐页根本不会挂载本组件。这里再判一次，是因为这条路径的实际后果比"多一个
     * 图标"严重得多：
     *   - `nativeOpen` 会在另一个窗口里起一套 mpv + 独立音频；
     *   - 紧随其后的 `togglePlay()` 会把本窗口的 `<video>` 按停（见下方注释）。
     * 用户看到的正是「点一次，它就自动关闭一次」：画面不是崩了，是被这条路径主动
     * 暂停了。日志证据 `ttv-playback.log` 里同一次会话既有 `[原生播放器] 已启动`
     * 又有紧随的 `[ui] 媒体 playing/paused` 交替。
     *
     * 推荐页是"刷着看"的形态，原生窗口（独立窗口 + 联播队列 + 补帧）属于主播放器
     * 的能力，不该在滑动流里被误触。要提升到原生播放，用户进详情页/主播放器再点。
     */
    if (currentView === 'feed') return;
    const ep = currentEpisode;
    if (!ep) return;
    const series = currentSeries;
    /**
     * 手动入口也要问一次落点：这一集若没有可交给 MPV 的地址（换集后 vid 变了、
     * 历史里是没前缀的纯数字动漫集号等），`nativeOpen` 会直接失败，而旧实现
     * 只写一行日志 —— 用户看到的是「点了原生按钮，什么都没发生」。
     */
    const route = planPlaybackRoute({
      preferredPlayer: settings.preferredPlayer,
      nativeAvailable: NATIVE_PLAYER_AVAILABLE,
      seriesId: series?.id ?? '',
      episodeId: ep.id,
      type: series?.type ?? null,
      surface: 'player',
    });
    if (route.kernel === 'inline') {
      if (route.notice) showToast(route.notice, 'warning');
      return;
    }
    const episodes = series?.episodes ?? [];
    const currentIndex = series ? episodes.findIndex(item => item.id === ep.id) : -1;
    /** 初始队列保留整剧元数据，包含当前集之前的集；不等待下一集解析。 */
    const queue = series && currentIndex >= 0
      ? buildInitialNativeQueue({
        seriesId: series.id,
        seriesTitle: series.title,
        seriesType: series.type ?? null,
        episodes,
        currentIndex,
        startSeconds: position,
      })
      : [];

    /**
     * `queue` / `index` / `autoplay` / `autoplayDelay` / `totalEpisodes` 都是
     * §3.1 的联播扩展字段（`ipc.ts::nativeOpen` 入参）。全部可选：不传时 Rust 写出的
     * load 与改造前逐字一致（单集播放、播完停住），所以这里传错也不会破坏老行为。
     */
    const savedFgMult = readSavedNativeFgMult();
    const savedFgMode = readSavedNativeFgMode();
    const fgPreset = interpolationPresetFor(series?.type);

    const openInput = {
      vid: ep.id,
      seriesId: series?.id,
      episodeId: ep.id,
      contentKind: series?.type,
      channel: series?.type,
      seriesCover: series?.cover,
      title: series?.title ?? '',
      episode: ep.episodeNumber,
      startSeconds: position,
      volume: Math.round(volume * 100),
      fg: fgPreset.fg,
      ...(savedFgMult !== undefined ? { fgMult: savedFgMult } : {}),
      ...(savedFgMode !== undefined ? { fgMode: savedFgMode } : {}),
      queue,
      /**
       * 队列里"当前集"的序号（= 它在剧集列表里的 0 基下标）：播放器换集时取
       * index + 1，所以这个值必须与 `buildInitialNativeQueue` 里当前集的 index 一致，
       * 否则播完会把当前集自己再播一遍。
       */
      index: Math.max(0, currentIndex),
      /** 联播默认跟随设置里的连播开关，0 秒表示播完无缝直切下一集（提前预加载）。 */
      autoplay: settings.autoNext,
      autoplayDelay: 0,
      totalEpisodes: episodes.length,
      /**
       * 应用主题（`light` / `dark`）：与 `preferredPlayback.tryLaunchNativePlayer`
       * 同一条口径。播放器是独立进程，读不到本页 localStorage —— 不带这一项时
       * 控制栏会按注册表里的**系统**偏好渲染，与主界面配色不一致。
       */
      theme: readTheme(),
    } satisfies Parameters<typeof ipcService.playback.nativeOpen>[0];

    /**
     * 手动入口也要先取地址（同 `tryLaunchNativePlayer`，见 services/nativeAddress.ts）：
     * 这里只处理"当前正在内置播放的那一集"，来源可能是 `guo:` / `hg:` —— 它们
     * 没有红果 vid，直接 `nativeOpen` 会让 Rust 拿着 `vid` 空转。
     */
    const address = await resolveNativeAddress({
      seriesId: series?.id ?? '',
      episodeId: ep.id,
      type: series?.type ?? null,
    });
    if (address) (openInput as { url?: string }).url = address.url;

    let handedOff = false;
    try {
      await ipcService.playback.nativeOpen(openInput);
      handedOff = true;
      registerNativeNavigateHost(navigateHost, true);
      pendingNativeEpisodeRef.current = null;
      // 交接凭据：从这一刻起本页画面是**我们主动让出去**的。轮询据此区分
      // "用户关窗 / 应用换窗 / 异常消失"（见 nativeHandoffRef 的注释）。
      nativeHandoffRef.current = {
        episodeId: ep.id,
        at: Date.now(),
        sawAlive: false,
      };
      /**
       * 打开成功后暂停本窗口播放（理由见函数头：否则两套音频同时出声）。
       *
       * ## 为什么判据从 `isPlaying` 改成元素真值（2026-11-10）
       *
       * `isPlaying` 是 `play`/`paused` 事件的镜像，与元素的 `video.paused` 之间
       * 最多差一拍（timeupdate 级别，~250ms）。用它当闸门会出两种反向事故：
       *   - state 说在播、元素其实已 paused → `togglePlay()` 读到 paused 后
       *     **把它播起来**，本窗口与原生窗口同时出声（正是要防的那件事）；
       *   - state 说没播、元素其实在播 → 这里直接跳过，同样两套音频。
       * 常驻宿主只有一块 `<video>`（与 `useAgentWatchStore` 取元素的方式一致），
       * 直接读它的 `paused` 才是真值；`togglePlay` 内部也只认 `videoRef`，
       * 两者是同一个元素，所以这里的判断与它随后的动作必然一致。
       */
      const hostVideo = document.querySelector<HTMLVideoElement>('video');
      if (hostVideo && !hostVideo.paused) togglePlay();
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      void ipcService.diagnostics.uiLog(`原生播放器打开失败 ${reason}`);
      // 日志之外还要对用户说一句：这条路径失败时窗口根本不会出现。
      showToast(NATIVE_OPEN_FAILED_NOTICE.replace('。', '') + '：' + reason, 'error');
    } finally {
      // 只用来取地址的会话在这里收掉；`guo:` 只在窗口真的接手后才延寿。
      await releaseNativeAddress(address, { handedOff });
    }
  };

  // 集数徽章文案。
  //
  // 后端在缺少真实分集标题时会把 title 填成"第 N 集"（见 provider.rs 的
  // 详情解析），直接拼接就会显示成"第 1 集 · 第 1 集"。标题与集数基名
  // 等价时不再重复；将来接入真实分集标题时会自动显示成"第 3 集 · 真相"。
  const episodeBadge = (() => {
    if (!currentEpisode) return '第 1 集';
    const base = `第 ${currentEpisode.episodeNumber} 集`;
    const title = (currentEpisode.title || '').trim();
    return title && title !== base ? `${base} · ${title}` : base;
  })();

  return (
    <>
      {/*
        右上角「RTX VSR」常驻徽标已按需求移除（2026-11-08）。
        底部「增强」按钮的 is-on 高亮，加上浮层里的开关状态，已经足够表达链路
        当前是 H.264 分片还是原始直连；多一个常驻角标只是永久遮挡画面右上角。
        scripts/verify-player-accessibility.ts 有「移除右上角 RTX VSR 常驻徽标」断言，
        恢复这个徽标会直接把那条验收打红。
      */}

      {/*
        原生播放器入口（右上角）。
        类名 `ttv-native-entry` 是独立钩子，**不要**改回 `ttv-lock`（见文件顶部
        NATIVE_PLAYER_ENTRY_ENABLED 的注释：ttv-lock 是 42×42 的正方形图标按钮，
        塞文字会竖排折行成白方块）。
      */}
      {NATIVE_PLAYER_ENTRY_ENABLED && (
        <button
          type="button"
          onClick={(event) => {
            event.stopPropagation();
            void handleOpenNative();
          }}
          title="用原生播放器播放（mpv 硬解 + 补帧 + 联播队列，首帧更快）"
          aria-label="用原生播放器播放"
          className={`ttv-native-entry crystal-surface transition-opacity duration-200 ${
            isVisible && !isLocked ? 'opacity-100' : 'opacity-0 pointer-events-none'
          }`}
          inert={isLocked || !isVisible}
          style={NATIVE_ENTRY_STYLE}
        >
          <Zap className="w-[14px] h-[14px]" style={{ flex: '0 0 auto' }} />
          原生播放
        </button>
      )}
      <PlayerHud
        title={currentSeries?.title || '精彩短剧'}
        hideHeader={feedMode}
        episodeLabel={episodeBadge}
        position={position}
        duration={duration}
        buffered={buffered}
        isPlaying={isPlaying}
        isFullscreen={isFullscreen}
        isVisible={isVisible}
        isLocked={isLocked}
        volume={volume}
        isMuted={isMuted}
        playbackRate={playbackRate}
        effectivePlaybackRate={effectivePlaybackRate}
        currentQuality={currentQuality}
        qualityOptions={availableQualities}
        vsrEnabled={vsrEnabled}
        onToggleVsr={setVsrEnabled}
        danmakuEnabled={danmakuEnabled}
        onToggleDanmaku={setDanmakuEnabled}
        danmakuAppearance={danmakuAppearance}
        onDanmakuAppearanceChange={updateSettings}
        /* 原生播放器徽标：alive 时在 HUD 右上角常驻，点一下关掉原生窗口回到本页播放。 */
        nativeAlive={nativeAlive}
        nativeAutoplay={nativeAutoplay}
        onExitNative={handleExitNative}
        onToggleLock={onToggleLock}
        onToggleFullscreen={onToggleFullscreen}
        onUserActivity={onUserActivity}
        onPointerMove={onPointerMove}
        onTogglePlay={togglePlay}
        onSeek={seek}
        onSeekRelative={seekRelative}
        onVolumeChange={setVolume}
        onToggleMute={toggleMute}
        onPlaybackRate={setPlaybackRate}
        onQualityChange={setQuality}
        onPlayPrev={playPrevEpisode}
        onPlayNext={playNextEpisode}
        onOpenEpisodes={() => toggleSideDrawer()}
        onBack={currentView === 'feed' ? () => navigateTo('explore') : goBack}
        onEnterPip={handleEnterPip}
        onFloatingUiChange={onFloatingUiChange}
        onLockHoverChange={onLockHoverChange}
      />
    </>
  );
};
