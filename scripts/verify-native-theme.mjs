/**
 * 主题订阅（跨进程同步的唯一入口）的行为验收。
 *
 * 为什么值得单独一条脚本：原生播放器窗口是**独立进程**，它读不到本页
 * `localStorage['ttv:theme']`，只能靠 `theme.ts` 的订阅把"实际生效模式"推过去。
 * 这条链路一旦漏掉某个入口（首帧、设置页、跟随系统），失败形态是"某个路径下
 * 控制栏配色和主界面不一致"——类型检查与构建都发现不了，只能在行为上钉住。
 *
 * 这里覆盖四件事：
 *   1. 启动主题：`applyTheme(readTheme())` 之后订阅者能立刻拿到当前值；
 *   2. 手动 light/dark：`saveTheme()` 之后 `data-theme` 与推送值同时变；
 *   3. 跟随系统：系统真的变（matchMedia 事件）时推送跟着变，且**只在跟随系统时**；
 *   4. 解除订阅：取消之后不再收到任何回调；订阅者抛错不能影响主题落盘。
 *
 * 运行：`node scripts/verify-native-theme.mjs`
 */
let failures = 0;
const fail = (message) => {
  failures += 1;
  console.error(`FAIL ${message}`);
};
const ok = (condition, message) => {
  if (!condition) fail(message);
};

// ── 最小 DOM / localStorage / matchMedia 桩（theme.ts 只用这三样）──────────
const store = new Map();
globalThis.localStorage = {
  getItem: (key) => (store.has(key) ? store.get(key) : null),
  setItem: (key, value) => store.set(key, String(value)),
  removeItem: (key) => store.delete(key),
};

let systemDark = false;
const mediaListeners = [];
globalThis.window = {
  matchMedia: (query) => ({
    get matches() {
      return query.includes('dark') ? systemDark : !systemDark;
    },
    addEventListener: (_type, handler) => mediaListeners.push(handler),
  }),
};
globalThis.document = { documentElement: { dataset: {} } };

/** 模拟"用户在 Windows 设置里改了应用模式"。 */
const flipSystemTheme = (dark) => {
  systemDark = dark;
  for (const handler of [...mediaListeners]) handler({ matches: dark });
};

const { applyTheme, readTheme, readThemePreference, saveTheme, subscribeTheme, watchSystemTheme } =
  await import('../src/services/theme.ts');

// ── 1. 启动：首帧应用 + 订阅立刻回灌当前值 ────────────────────────────────
applyTheme(readTheme());
const seen = [];
const unsubscribe = subscribeTheme((mode) => seen.push(mode));
ok(seen.length === 1 && seen[0] === 'light', `启动后订阅者应立刻收到 light，实际 ${JSON.stringify(seen)}`);
ok(document.documentElement.dataset.theme === 'light', 'data-theme 应为 light');

// ── 2. 手动切换 light / dark ────────────────────────────────────────────────
saveTheme('dark');
ok(document.documentElement.dataset.theme === 'dark', 'saveTheme(dark) 应落到 data-theme');
ok(seen.at(-1) === 'dark', `订阅者应收到 dark，实际收到 ${JSON.stringify(seen.at(-1))}`);

saveTheme('light');
ok(seen.at(-1) === 'light', '订阅者应收到 light');
ok(readThemePreference() === 'light', '偏好应记住 light');

// ── 3. 跟随系统：只有 preference=system 时才响应系统变化 ────────────────────
watchSystemTheme();
ok(mediaListeners.length === 1, 'watchSystemTheme 应只挂一个监听器');

// 当前偏好是 light（固定）：系统变暗**不应**影响推送。
const before = seen.length;
flipSystemTheme(true);
ok(seen.length === before, '固定 light 时系统变暗不应触发推送');
ok(document.documentElement.dataset.theme === 'light', '固定 light 时不应被系统改掉');

// 切到跟随系统：应立刻按系统此刻的值落一次（此时系统是暗）。
saveTheme('system');
ok(document.documentElement.dataset.theme === 'dark', '跟随系统且系统为暗 → data-theme=dark');
const afterSystemSelect = seen.length;

// 系统真的变回亮：推送必须跟着变。
flipSystemTheme(false);
ok(seen.length === afterSystemSelect + 1, '系统变化应触发一次推送');
ok(seen.at(-1) === 'light', `系统变亮应推送 light，实际 ${JSON.stringify(seen.at(-1))}`);

// 切回固定暗色后再动系统：不应再推送。
saveTheme('dark');
const afterFixed = seen.length;
flipSystemTheme(true);
ok(seen.length === afterFixed, '切回固定暗色后系统变化不应再推送');

// ── 4. 解除订阅 / 订阅者抛错 ────────────────────────────────────────────────
unsubscribe();
const beforeUnsub = seen.length;
saveTheme('light');
ok(seen.length === beforeUnsub, '解除订阅后不应再收到推送');

// 抛错的订阅者不能把主题落盘搞坏（跨进程同步失败不该阻断界面）。
subscribeTheme(() => {
  throw new Error('boom');
});
saveTheme('dark');
ok(document.documentElement.dataset.theme === 'dark', '订阅者抛错时 data-theme 仍应落到 dark');

// 订阅一个新监听器：应立刻回灌当前生效值（晚挂载的组件不会看到"没有主题"）。
let replayed = null;
subscribeTheme((mode) => {
  replayed = mode;
});
ok(replayed === 'dark', `晚订阅应立刻收到当前值 dark，实际 ${JSON.stringify(replayed)}`);

if (failures > 0) {
  console.error(`verify-native-theme: ${failures} 条失败`);
  process.exit(1);
}
console.log('verify-native-theme: ok');