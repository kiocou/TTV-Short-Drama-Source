import React, {
  createContext, useCallback, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState,
} from 'react';
import { createPortal } from 'react-dom';
import { useAppStore } from '../../stores/useAppStore';

/**
 * 黄果短剧专区的进出闸门。
 *
 * ## 为什么 opacity 只能活在这里
 *
 * 项目的视图是**常驻 DOM + display 切换**（App.tsx 每个视图一行）。元素在
 * `display:none` 的祖先里创建时，Chromium 的动画会永远卡在 0% 帧，而 0% 的
 * `opacity:0` 会被当真 —— 那一层就永久透明，只能靠悬停重绘才现形。
 * 所以视图层的入场一律只动 transform（`animate-fluent-page-in`），
 * 淡入淡出全部收进这个**按需挂载、跑完即卸载**的 Portal 遮罩里。
 *
 * ## 时序
 *
 * 遮罩淡入 → 完全不透明时切换底层视图 → 淡出露出新页面。
 * 切换点落在 55%，此时遮罩仍然全黑，用户看不到那一次 display 变化；
 * 新页面在遮罩下完成自己那条纯 transform 入场。
 *
 * ## 动画走 element.animate 而不是 CSS animation
 *
 * 与启动动画同一条规矩：`fill:'both'` + 结束时 `cancel()`。CSS 的
 * `forwards` 会把 transform/filter 永久钉在元素上，项目已因此让 NVIDIA VSR
 * 失效过一次。这里虽然跑完就卸载、残留不会留下来，但仍然沿用同一套写法，
 * 免得下一个人照抄成常驻版本。
 */

/** 遮罩总时长（毫秒）。进入比退出略长：进入要让用户看清「换了个地方」。 */
const ENTER_MS = 720;
const EXIT_MS = 560;
/** 底层视图切换点（占总时长的比例）。必须落在遮罩完全不透明的那段区间里。 */
const SWITCH_AT = 0.55;

type Transition = 'enter' | 'exit';

interface GateApi {
  /** 请求进入专区。已在专区内或正在过渡时忽略。 */
  enter: () => void;
  /** 请求退出专区，回到进入前的那个视图。 */
  exit: () => void;
  /** 当前是否正在过渡（供入口做视觉反馈用）。 */
  transitioning: boolean;
}

const GateContext = createContext<GateApi | null>(null);

export function useHuangguoGate(): GateApi {
  const context = useContext(GateContext);
  if (!context) throw new Error('useHuangguoGate 必须在 HuangguoGateProvider 内使用。');
  return context;
}

function prefersReducedMotion(): boolean {
  return typeof window !== 'undefined'
    && window.matchMedia?.('(prefers-reduced-motion: reduce)').matches === true;
}

/** 遮罩本体。只在过渡期间挂载。 */
const UnlockOverlay: React.FC<{ kind: Transition; duration: number }> = ({ kind, duration }) => {
  const rootRef = useRef<HTMLDivElement>(null);
  const emblemRef = useRef<HTMLDivElement>(null);
  const labelRef = useRef<HTMLDivElement>(null);

  useLayoutEffect(() => {
    const running: Animation[] = [];
    const spring = 'cubic-bezier(0.16, 1, 0.3, 1)';
    // 0 → 45% 淡入，45%~62% 保持全不透明（视图就在这段里切），62% → 100% 淡出。
    const fade = [
      { opacity: 0 },
      { opacity: 1, offset: 0.45 },
      { opacity: 1, offset: 0.62 },
      { opacity: 0 },
    ];
    if (rootRef.current) {
      running.push(rootRef.current.animate(fade, { duration, easing: 'linear', fill: 'both' }));
    }
    if (emblemRef.current) {
      const scale = kind === 'enter' ? [0.82, 1, 1, 1.08] : [0.9, 1, 1, 0.94];
      running.push(emblemRef.current.animate(
        [
          { transform: `scale(${scale[0]})`, opacity: 0 },
          { transform: `scale(${scale[1]})`, opacity: 1, offset: 0.45 },
          { transform: `scale(${scale[2]})`, opacity: 1, offset: 0.62 },
          { transform: `scale(${scale[3]})`, opacity: 0 },
        ],
        { duration, easing: spring, fill: 'both' },
      ));
    }
    if (labelRef.current) {
      running.push(labelRef.current.animate(
        [
          { transform: 'translateY(12px)', opacity: 0 },
          { transform: 'translateY(0)', opacity: 1, offset: 0.5 },
          { transform: 'translateY(0)', opacity: 1, offset: 0.62 },
          { transform: 'translateY(-6px)', opacity: 0 },
        ],
        { duration, easing: spring, fill: 'both' },
      ));
    }
    return () => running.forEach(animation => animation.cancel());
  }, [kind, duration]);

  return createPortal(
    <div
      ref={rootRef}
      aria-hidden="true"
      className="fixed inset-0 z-[80] flex flex-col items-center justify-center gap-5 pointer-events-auto"
      style={{ background: 'radial-gradient(120% 90% at 50% 42%, #1b1206 0%, #0a0a0c 62%, #050506 100%)' }}
    >
      <div
        ref={emblemRef}
        className="w-[86px] h-[86px] rounded-[26px] flex items-center justify-center"
        style={{
          background: 'linear-gradient(150deg, #fbbf24 0%, #f59e0b 48%, #b45309 100%)',
          boxShadow: '0 18px 48px -12px rgba(245,158,11,.55), inset 0 1px 0 rgba(255,255,255,.35)',
        }}
      >
        <svg width="44" height="44" viewBox="0 0 24 24" fill="none" stroke="#1b1206" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <path d="M12 3.2c2.6 0 4.6 1.9 4.6 4.4 0 1.2-.5 2.3-1.3 3.1 2.4 1 4 3.2 4 5.8 0 3.5-3.2 6.3-7.3 6.3S4.7 20 4.7 16.5c0-2.6 1.6-4.8 4-5.8-.8-.8-1.3-1.9-1.3-3.1 0-2.5 2-4.4 4.6-4.4Z" />
          <path d="M12 3.2V1.6" />
        </svg>
      </div>
      <div ref={labelRef} className="flex flex-col items-center gap-1.5">
        <div className="text-[19px] font-semibold tracking-[.14em] text-amber-50">
          {kind === 'enter' ? '黄果短剧专区' : '正在返回'}
        </div>
        <div className="text-[11.5px] tracking-[.28em] text-amber-200/55 uppercase">
          {kind === 'enter' ? 'Huangguo Zone' : 'Leaving'}
        </div>
      </div>
    </div>,
    document.body,
  );
};

export const HuangguoGateProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const { navigateTo, goBack, currentView } = useAppStore();
  const [transition, setTransition] = useState<Transition | null>(null);
  const timers = useRef<number[]>([]);
  const busy = useRef(false);
  const viewRef = useRef(currentView);
  viewRef.current = currentView;

  const clearTimers = useCallback(() => {
    timers.current.forEach(id => window.clearTimeout(id));
    timers.current = [];
  }, []);

  const run = useCallback((kind: Transition) => {
    if (busy.current) return;
    if (kind === 'enter' && viewRef.current === 'huangguo') return;
    if (kind === 'exit' && viewRef.current !== 'huangguo') return;
    const move = () => (kind === 'enter' ? navigateTo('huangguo') : goBack());
    // 降低动效偏好：直接切换，不挂遮罩。功能可达性不受动效设置影响。
    if (prefersReducedMotion()) {
      move();
      return;
    }
    busy.current = true;
    setTransition(kind);
    const duration = kind === 'enter' ? ENTER_MS : EXIT_MS;
    timers.current.push(window.setTimeout(move, Math.round(duration * SWITCH_AT)));
    // +40ms 收尾余量：让淡出真的跑完再卸载，否则最后一帧会被硬切掉。
    timers.current.push(window.setTimeout(() => {
      setTransition(null);
      busy.current = false;
    }, duration + 40));
  }, [navigateTo, goBack]);

  // 卸载时清干净：标题栏在全屏切换时会重建，绝不能留下悬挂回调。
  useEffect(() => clearTimers, [clearTimers]);

  const api = useMemo<GateApi>(() => ({
    enter: () => run('enter'),
    exit: () => run('exit'),
    transitioning: transition !== null,
  }), [run, transition]);

  return (
    <GateContext.Provider value={api}>
      {children}
      {transition && (
        <UnlockOverlay kind={transition} duration={transition === 'enter' ? ENTER_MS : EXIT_MS} />
      )}
    </GateContext.Provider>
  );
};