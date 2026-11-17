#!/usr/bin/env node
/**
 * `verify:native-navigate` —— 原生播放入口 + 联播导航桥的**静态契约验收**。
 *
 * 为什么是静态的：这条链路的三个参与方（React / Rust / C#）是并行施工的
 * （`docs/native-player-redesign.md` §5），而真实链路要起一个 Tauri 窗口 +
 * 一个 mpv 进程 + 一部真剧才能跑通。CI 与日常自检需要一个几毫秒就能跑、
 * 不依赖任何运行时环境的门禁 —— 它只证明"契约还在"，不证明"链路能跑"。
 *
 * 它盯的是四件最容易在重构里被悄悄改坏的事：
 *   1. 「原生播放」入口没被再次关掉（`NATIVE_PLAYER_ENTRY_ENABLED = true`），
 *      并且按钮用的是独立类名 `ttv-native-entry` 而不是会把文字竖排压死的
 *      `ttv-lock`（见 `PlayerControls.tsx` 顶部那段历史注释）。
 *   2. 联播导航桥的 **5 个事件名**一个都没少 —— 事件名是跨语言硬契约，
 *      Rust 侧 `emit` 的名字改一个字，前端就再也收不到（而且不会报错）。
 *   3. `ipc.ts` 是否已经暴露 `nativeQueue` / `nativeQueuePush`
 *      （由 rust-bridge 在 task-2 并行补）。缺失只标 ⚠ 不算失败：
 *      这一项的所有权不在本任务，前端已按契约写好调用并在运行时探测。
 *   4. `preferredPlayback.ts` 的 `extractHongguoVid` 仍被导出 ——
 *      它是"分集 id → 红果 vid"的唯一口径，队列构造与换集下发都靠它；
 *      它一旦被内联或改名，vid 提取就会分叉成两份。
 *
 * 退出码：任何一条 **✗ FAIL** → 1；只有 ⚠ WARN 不影响退出码。
 */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');

/** 联播导航桥必须包含的 5 个事件名（与 Rust 侧 emit 逐字一致）。 */
const REQUIRED_EVENTS = [
  'native-player-navigate',
  'native-player-advance',
  'native-player-need-next',
  'native-player-queue-timeout',
  'native-player-autoplay-changed',
];

const results = [];
let failed = 0;

function readSource(relativePath) {
  const absolute = resolve(ROOT, relativePath);
  try {
    return readFileSync(absolute, 'utf8');
  } catch (error) {
    return null;
  }
}

/**
 * @param {'pass'|'warn'|'fail'} kind
 * @param {string} title
 * @param {string} detail
 */
function record(kind, title, detail = '') {
  results.push({ kind, title, detail });
  if (kind === 'fail') failed += 1;
}

function check(title, condition, detail) {
  if (condition) record('pass', title);
  else record('fail', title, detail);
}

// ---------------------------------------------------------------------------
// 1. 播放页入口（PlayerControls.tsx）
// ---------------------------------------------------------------------------

const controls = readSource('src/components/player/PlayerControls.tsx');
const queueSource = readSource('src/services/nativeNavigate.ts');
if (!controls) {
  record('fail', '读不到 src/components/player/PlayerControls.tsx');
} else {
  check(
    'PlayerControls 里 NATIVE_PLAYER_ENTRY_ENABLED = true（入口已恢复）',
    /NATIVE_PLAYER_ENTRY_ENABLED\s*=\s*true\b/.test(controls),
    '常量是 false 或已被改名 —— 播放页右上角不会出现「原生播放」按钮',
  );

  check(
    '原生播放入口使用独立类名 ttv-native-entry',
    controls.includes('ttv-native-entry'),
    '找不到 ttv-native-entry —— 按钮可能又复用了 ttv-lock（42×42 正方形，塞文字会竖排折行）',
  );

  check(
    '原生播放入口没有复用 ttv-lock',
    !/className=[^\n]*\bttv-lock\b/.test(controls.replace(/ttv-native-entry/g, '')),
    '入口按钮上出现 ttv-lock，宽度会被压成方块（历史坑，见文件内注释）',
  );

  check(
    '入口按钮带 nowrap + 12px 内边距（横排不折行）',
    /whiteSpace:\s*'nowrap'/.test(controls) && /padding:\s*'6px 12px'/.test(controls),
    'NATIVE_ENTRY_STYLE 里的 whiteSpace/padding 被改动，中文会折行',
  );

  check(
    '入口调用 nativeOpen 时带上联播队列',
    /queue,/.test(controls) && controls.includes('buildInitialNativeQueue'),
    '没有构造 queue 或没传给 nativeOpen —— 原生播放器播完一集就停住',
  );

  /*
   * 整剧队列的三条防回归断言（2026-11-10 修的「播到第 2、3 集就停住」）。
   *
   * 事故形态：队列只下发「当前集占位 + 已解析好的下一集」，于是第 2 集之后 Rust
   * 手里没有 vid —— 它自己解析不了，只能回落"等前端"，日志就是
   * `requestNext index=1 → 8 秒超时 → 停住`。
   *
   * 现在的形状是：每一集都进队列，只带元数据（`url: ''` + `vid` + `index`），
   * 由 Rust `plan_next` 按 vid 现解析。三条断言分别盯住这个形状的三个要素：
   * 空 url（不预解析、零网络成本）、vid（Rust 的解析输入）、整剧遍历（不是只两项）。
   */
  check(
    "整剧队列项带空 url（url: ''，由 Rust 解析而不是前端预解析）",
    /url:\s*''/.test(queueSource) || /url:\s*""/.test(queueSource),
    "找不到 url: '' —— 队列可能又退回「只下发已解析的几项」，"
      + '第 2 集之后 Rust 拿不到 vid，会 8 秒超时停住',
  );

  check(
    '整剧队列项带 vid（extractHongguoVid，Rust plan_next 的唯一解析输入）',
    queueSource.includes('extractHongguoVid(episode.id, input.seriesId)'),
    '队列项没有 vid —— Rust 侧 plan_next 会判 NoVid 并回落前端，等于把联播退回改造前',
  );

  check(
    '队列遍历整部剧（包含当前集之前的所有元数据）',
    /input\.episodes\.map\(/.test(queueSource),
    '找不到整剧遍历 —— 队列可能只塞了当前集和下一集两项（就是"播两集就停"的那版）',
  );

  check(
    '播放页挂载了联播导航桥（startNativeNavigateBridge）',
    controls.includes('startNativeNavigateBridge') && controls.includes('registerNativeNavigateHost'),
    'bridge 没有挂载 —— 原生窗口里的 ⏮/⏭ 与联播推进不会被前端处理',
  );

  /**
   * 守门员 effect 的两种等价写法都认：
   *   `if (!nativeAlive || !isPlaying) return;`（提前返回）
   *   `if (nativeAlive && isPlaying) togglePlay();`（正面条件）
   * 检查的是"同时判了两个条件"，不锁死具体句式 —— 为风格差异把门禁改红没有意义。
   */
  const guardEffect =
    /!\s*nativeAlive\s*\|\|\s*!\s*isPlaying/.test(controls) ||
    /nativeAlive\s*&&\s*isPlaying/.test(controls);
  check(
    '原生在播时前端被按停（守门员 effect）',
    guardEffect,
    '缺少"原生在播则内嵌暂停"的 effect，会出现两路音频叠加',
  );
}

// ---------------------------------------------------------------------------
// 2. 导航桥事件名（nativeNavigate.ts）
// ---------------------------------------------------------------------------

const navigate = readSource('src/services/nativeNavigate.ts');
if (!navigate) {
  record('fail', '读不到 src/services/nativeNavigate.ts');
} else {
  for (const eventName of REQUIRED_EVENTS) {
    check(
      `nativeNavigate.ts 声明事件 ${eventName}`,
      navigate.includes(`'${eventName}'`),
      `事件名缺失 —— Rust 侧 emit("${eventName}") 将没有任何监听者（静默失效）`,
    );
  }

  check(
    'nativeNavigate.ts 导出 startNativeNavigateBridge',
    /export\s+function\s+startNativeNavigateBridge\s*\(/.test(navigate),
    '宿主无法启动 bridge',
  );

  check(
    'nativeNavigate.ts 用 isTauriEnvironment 守卫（Web 环境静默）',
    navigate.includes('isTauriEnvironment'),
    '浏览器里会去 import @tauri-apps/api，控制台一片报错',
  );

  check(
    'nativeNavigate.ts 对 nativeQueuePush 做可选探测（并行契约）',
    navigate.includes('nativeQueuePush'),
    '没有探测 nativeQueuePush —— ipc.ts 尚未合入时会直接抛 TypeError',
  );
}

// ---------------------------------------------------------------------------
// 3. HUD 徽标（PlayerHud.tsx）
// ---------------------------------------------------------------------------

const hud = readSource('src/components/player/PlayerHud.tsx');
if (!hud) {
  record('fail', '读不到 src/components/player/PlayerHud.tsx');
} else {
  check(
    'PlayerHud 有原生播放器徽标',
    hud.includes('原生 mpv 播放中') && hud.includes('nativeAlive'),
    '徽标缺失 —— 点了原生播放后本页只剩静止帧，用户不知道画面在哪',
  );
}

// ---------------------------------------------------------------------------
// 4. ipc.ts 的联播方法（rust-bridge / task-2 提供）
// ---------------------------------------------------------------------------

const ipc = readSource('src/services/ipc.ts');
if (!ipc) {
  record('fail', '读不到 src/services/ipc.ts');
} else {
  const hasQueuePush = /nativeQueuePush\s*[(:]/.test(ipc);
  const hasQueue = /nativeQueue\s*[(:]/.test(ipc);
  if (hasQueuePush && hasQueue) {
    record('pass', 'ipc.ts 已暴露 nativeQueue / nativeQueuePush');
  } else {
    record(
      'warn',
      'ipc.ts 尚未暴露 nativeQueue / nativeQueuePush',
      `缺失：${[hasQueue ? '' : 'nativeQueue', hasQueuePush ? '' : 'nativeQueuePush'].filter(Boolean).join(' / ')}`
        + ' —— 由 rust-bridge 在 task-2 并行补；前端已按契约写好调用并做运行时探测，合入后自动生效',
    );
  }

  const hasQueueField = /queue\??\s*:/.test(ipc) && ipc.includes('NativeQueueItem');
  if (!hasQueueField) {
    record(
      'warn',
      'ipc.ts 的 nativeOpen 入参尚未声明 queue / NativeQueueItem',
      '前端用窄化断言传入契约字段（§3.1），对方合入后断言等价恒等',
    );
  }
}

// ---------------------------------------------------------------------------
// 5. vid 口径没被破坏（preferredPlayback.ts）
// ---------------------------------------------------------------------------

const preferred = readSource('src/services/preferredPlayback.ts');
if (!preferred) {
  record('fail', '读不到 src/services/preferredPlayback.ts');
} else {
  check(
    'preferredPlayback.ts 仍导出 extractHongguoVid',
    preferred.includes('extractHongguoVid'),
    'vid 提取口径丢失 —— 队列项与换集下发会各自写一份数字提取，迟早分叉',
  );
}

const preferredLogic = readSource('src/services/preferredPlaybackLogic.ts');
if (!preferredLogic) {
  record('fail', '读不到 src/services/preferredPlaybackLogic.ts');
} else {
  check(
    'preferredPlaybackLogic.ts 仍导出 extractHongguoVid 实现',
    /export\s+function\s+extractHongguoVid\s*\(/.test(preferredLogic),
    '实现被内联/改名，preferredPlayback.ts 的 re-export 会指向空',
  );
}

// ---------------------------------------------------------------------------
// 6. Rust 侧事件名一致性（只校验已经落地的 navigate）
// ---------------------------------------------------------------------------

const rust = readSource('src-tauri/src/native_player.rs');
if (!rust) {
  record('warn', '读不到 src-tauri/src/native_player.rs（跳过 Rust 侧事件名校验）');
} else {
  check(
    'Rust 侧 emit("native-player-navigate") 与前端一致',
    rust.includes('"native-player-navigate"'),
    'Rust 与前端的事件名分叉 —— 用户在原生窗口点 ⏮/⏭ 前端收不到',
  );
  for (const eventName of REQUIRED_EVENTS.slice(1)) {
    if (!rust.includes(`"${eventName}"`)) {
      record(
        'warn',
        `Rust 侧尚未 emit("${eventName}")`,
        '联播扩展由 rust-bridge 在 task-2 并行实现；前端监听已就位，合入后即生效',
      );
    }
  }
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

const mark = { pass: '✓', warn: '⚠', fail: '✗' };
console.log('[verify:native-navigate] 原生播放入口 + 联播导航桥契约校验');
console.log(`  根目录 ${ROOT}`);
for (const item of results) {
  console.log(`  ${mark[item.kind]} ${item.title}`);
  if (item.detail) console.log(`      ${item.detail}`);
}

const passed = results.filter(item => item.kind === 'pass').length;
const warned = results.filter(item => item.kind === 'warn').length;
console.log('');
console.log(`  通过 ${passed} · 警告 ${warned} · 失败 ${failed}`);

if (failed > 0) {
  console.log('\n[verify:native-navigate] 失败：上面标 ✗ 的断言未满足。');
  process.exit(1);
}

console.log('[verify:native-navigate] 通过（警告项是并行施工中尚未合入的契约，不影响本任务）。');
