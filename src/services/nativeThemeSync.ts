/**
 * 应用主题 ⇄ 原生播放器窗口的跨进程同步。
 *
 * ## 为什么需要它（2026-11-10）
 *
 * 原生播放器（`NativePlayer.exe`）是**独立进程**，它读不到 WebView2 的
 * `localStorage['ttv:theme']`，改造前只能读注册表里的系统偏好
 * （`AppsUseLightTheme`）。于是用户在应用里选了浅色、系统却是深色时，
 * 控制栏是三行浅色、主界面是白蓝 —— 两种配色同屏。
 *
 * 契约（`docs/native-player-redesign.md` §2）要求「原生窗口跟随**应用**主题」，
 * 所以同步必须覆盖三个时刻，缺一个就会出现"某条路径下窗口不跟"：
 *   1. **启动那一刻**：`nativeOpen` 带 `theme` 字段 → Rust 写 `--theme` 启动参数
 *      （落在 `preferredPlayback.ts` 与 `PlayerControls.tsx` 两个入口）；
 *   2. **应用主题变更**：用户在设置页改 → `saveTheme` → `applyTheme` → 本文件推送；
 *   3. **系统配色变化**：选了「跟随系统」时 `watchSystemTheme` 会重新
 *      `applyTheme`，同样落到这里。
 *
 * 三者最终都汇到 `applyTheme()` 上，所以订阅点只有 `subscribeTheme` 一处。
 *
 * ## 为什么不做"主题改了就把播放器重启一次"
 *
 * `nativeOpen` 的复用分支会走换集（停流、重建 D3D 输出、重新解析），代价与风险
 * 都不对等 —— 用户只是换了配色，不该打断正在播的那一集。走 `theme` 命令只改
 * `PlayerBar` 的色板指针与窗口背景，**不碰任何尺寸与播放状态**。
 *
 * ## 失败处理
 *
 * 主题同步是**旁路**：没有播放器、管道刚启动没就绪、老后端没有这条命令 ——
 * 一律静默。它绝不能把设置页变成错误卡，也绝不能阻塞首帧。
 */
import { ipcService } from './ipc';
import { subscribeTheme } from './theme';
import type { ThemeMode } from './theme';

/**
 * 开始同步。**整个进程只挂一次**（重复调用是空操作，理由同 `watchSystemTheme`）。
 *
 * 返回解除函数只给测试用：生产入口不需要卸载，主题是全应用级的。
 */
let started = false;

export function startNativeThemeSync(): () => void {
  if (started) return () => undefined;
  started = true;
  const unsubscribe = subscribeTheme((mode: ThemeMode) => {
    // `nativeTheme` 自己吞掉所有失败并返回 false，这里不需要 catch。
    // 打点只在**失败**时落一条：成功路径每切一次主题就多一行日志，没有诊断价值。
    void ipcService.playback.nativeTheme(mode).then((ok) => {
      if (ok) return;
      void ipcService.diagnostics.uiLog(
        `原生播放器主题同步跳过（没有运行中的窗口）mode=${mode}`,
      );
    });
  });
  return () => {
    started = false;
    unsubscribe();
  };
}