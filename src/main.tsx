import { isPipWindow } from './services/pip';
import { isAgentDockWindow } from './services/agentWatch';
import { applyTheme, readTheme, watchSystemTheme } from './services/theme';
import { startNativeThemeSync } from './services/nativeThemeSync';
import './index.css';

// 首次渲染之前就要落到 <html> 上：暗色用户不该先看到一帧浅色界面。
// `readTheme()` 内部会解析「跟随系统」——选了它的话，这里取到的就是系统此刻的值。
applyTheme(readTheme());

// 区分鼠标点击与键盘 Tab 导航：避免鼠标点击播放器按钮（如全屏/锁屏）后残留焦点框，
// 同时保留键盘 Tab 用户的无障碍焦点环。
if (typeof window !== 'undefined' && typeof document !== 'undefined') {
  window.addEventListener(
    'pointerdown',
    () => {
      document.documentElement.classList.remove('kbd-nav');
    },
    { capture: true, passive: true },
  );
  window.addEventListener(
    'keydown',
    (e) => {
      if (e.key === 'Tab' && !e.ctrlKey && !e.altKey && !e.metaKey) {
        document.documentElement.classList.add('kbd-nav');
      }
    },
    { capture: true, passive: true },
  );
  window.addEventListener(
    'click',
    (e) => {
      if (e.detail > 0 && e.target instanceof Element) {
        const btn = e.target.closest('button');
        if (btn instanceof HTMLElement) btn.blur();
      }
    },
    { capture: true, passive: true },
  );
}
// 挂上系统配色监听：用户在 Windows 设置里改应用模式时，"跟随系统"要立刻跟上。
// 必须在首帧之后也一直有效，所以挂在这里（进程级、只挂一次），而不是某个组件里
// ——组件会随视图切换卸载，那时"跟随"就会静默失效。
watchSystemTheme();
// 原生播放器窗口跟随应用主题：订阅"实际生效模式"的变化并推给它。
// 必须早于任何 `nativeOpen` 才能保证"打开那一刻就是对的"（启动参数已带 theme，
// 这条负责之后的切换）。放在这里而不是组件里：组件会随视图卸载，那时同步会静默失效。
startNativeThemeSync();

const rootElement = document.getElementById('root');
if (rootElement) {
  /**
   * 同一个 index.html 承担两个入口：主界面与画中画小窗。分流依据是**窗口标签**
   * （min 前缀的窗口由 Rust 侧 pip.rs 创建），而不是查询参数——生产环境前端走
   * asset 协议，URL 参数的处理规则与 dev server 并不一致，标签才是唯一稳定的依据。
   *
   * ⚠️ 两个入口都用**动态 import**，不能静态 `import App`。
   *
   * 小窗是一块 380×214 的播放面，它只需要 MiniPlayer 那一小撮模块；静态引入 App
   * 会把全部视图、store、服务一并加载并执行（冷启动下是几百毫秒级的解析与执行）。
   * 用户感知就是"点画中画要卡一下才出来"，而画中画恰恰是最需要即时响应的动作。
   * 拆成 `bootApp` / `bootPip` 两个文件后，Rollup 按 chunk 分开，小窗只付自己那份。
   */
  if (isPipWindow()) {
    void import('./bootPip').then(({ mountPip }) => mountPip(rootElement));
  } else if (isAgentDockWindow()) {
    // 智能体悬浮窗：独立置顶窗，同样只加载自己那一小份模块图。
    void import('./bootAgentDock').then(({ mountAgentDock }) => mountAgentDock(rootElement));
  } else {
    void import('./bootApp').then(({ mountApp }) => mountApp(rootElement));
  }
}
