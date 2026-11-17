#!/usr/bin/env node
/**
 * `verify:feed-playback` —— 推荐页（feed）播放链路的**静态契约验收**。
 *
 * 为什么是静态的：这条链路的故障只在"窗口真的在放视频 + 用户真的在点"时出现
 * （WebView2 的媒体事件、派发顺序、原生窗口的进程生命周期），CI 里跑不起来。
 * 但它坏掉的方式**永远是那几种结构改动**，所以用几毫秒的源码门禁钉住即可：
 *
 *   1. 推荐页不许再挂主播放器的控制层（`VideoSurface` 的渲染点）。
 *      改回去 = 11 个主播放器控件 + 「原生播放」按钮 + 「原生 mpv 播放中·联播 开」
 *      徽标一起回到推荐页，用户原话「按钮全部放在一起太乱、太杂了」。
 *   2. 推荐页单击画面**不许**切换播放/暂停。
 *      改回去 = 「点一次，它就自动关闭一次」：每点一下都把上一段播放按停，
 *      点得越快播放越碎（日志：13 条 playing / 2.6 秒，位置只走 0.31s）。
 *   3. 原生播放入口在 feed 下必须有守卫（`handleOpenNative` 的第二道闸）。
 *   4. 打开原生播放器后暂停本窗口，判据必须是**元素真值**而不是 `isPlaying`。
 *      用 state 判断会反向操作：state 说在播、元素其实 paused → 反而播起来，
 *      两套音频同时出声。
 *   5. 连播（本集播完自动进下一集）的调用点仍在：
 *      `handleEnded` → `playNextEpisodeRef` → `openEpisode` → `playBounded`
 *      → `retryMutedAutoplay`。少任何一环，用户就得手动点下一集。
 *
 * 退出码：任何一条 ✗ FAIL → 1；只有 ⚠ WARN 不影响退出码。
 *
 * 运行：node --experimental-strip-types scripts/verify-feed-playback.mjs
 *      （package.json 里对应 `npm run verify:feed-playback`，由 Lead 统一注册）
 */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');

const results = [];
let failed = 0;

function readSource(relativePath) {
  try {
    return readFileSync(resolve(ROOT, relativePath), 'utf8');
  } catch {
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

/**
 * 剥掉 `//` 与 `/* ... *\/` 注释，保留代码本身。
 *
 * 为什么必须剥：本项目的关键决策都写在**注释里**，而注释里会直接引用要防的旧
 * 写法（例如「旧实现对单击无条件在 220ms 后 togglePlay()」）。不剥注释做位置比较，
 * 命中的永远是注释而不是代码 —— 这条脚本第一次跑就是这么假红的。
 * 字符串 / 模板字面量按原样跳过，避免把 URL 里的 `//` 当成注释。
 *
 * @param {string} source
 * @returns {string}
 */
function stripComments(source) {
  let out = '';
  let i = 0;
  let inSingle = false;
  let inDouble = false;
  let inTemplate = false;
  while (i < source.length) {
    const ch = source[i];
    const next = source[i + 1];
    if (!inSingle && !inDouble && !inTemplate) {
      if (ch === '/' && next === '/') {
        while (i < source.length && source[i] !== '\n') i += 1;
        continue;
      }
      if (ch === '/' && next === '*') {
        i += 2;
        while (i < source.length && !(source[i] === '*' && source[i + 1] === '/')) i += 1;
        i += 2;
        continue;
      }
      if (ch === "'") inSingle = true;
      else if (ch === '"') inDouble = true;
      else if (ch === '`') inTemplate = true;
    } else if (inSingle && ch === "'") inSingle = false;
    else if (inDouble && ch === '"') inDouble = false;
    else if (inTemplate && ch === '`') inTemplate = false;
    out += ch;
    i += 1;
  }
  return out;
}

/**
 * 截取一个具名函数/箭头函数的函数体（从签名行到配平的 `}`）。
 *
 * 只做"够用"的括号配平：跳过字符串与模板字面量里的花括号。项目里的这几个函数
 * 都是两三百行的直线代码，没有正则字面量里的 `{}`，够用且比引 parser 便宜。
 *
 * @param {string} source
 * @param {string} signature 例如 `const handleVideoSurfaceClick = () => {`
 * @returns {string | null}
 */
function extractFunctionBody(source, signature) {
  const start = source.indexOf(signature);
  if (start < 0) return null;
  const openBrace = source.indexOf('{', start + signature.length - 1);
  if (openBrace < 0) return null;
  let depth = 0;
  let inSingle = false;
  let inDouble = false;
  let inTemplate = false;
  let escaped = false;
  for (let i = openBrace; i < source.length; i += 1) {
    const ch = source[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (ch === '\\') {
      escaped = true;
      continue;
    }
    if (inSingle) {
      if (ch === "'") inSingle = false;
      continue;
    }
    if (inDouble) {
      if (ch === '"') inDouble = false;
      continue;
    }
    if (inTemplate) {
      if (ch === '`') inTemplate = false;
      continue;
    }
    if (ch === "'") { inSingle = true; continue; }
    if (ch === '"') { inDouble = true; continue; }
    if (ch === '`') { inTemplate = true; continue; }
    if (ch === '{') depth += 1;
    else if (ch === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(openBrace, i + 1);
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// 1. VideoSurface：feed 下不渲染 PlayerControls
// ---------------------------------------------------------------------------

const surface = readSource('src/components/player/VideoSurface.tsx');
if (!surface) {
  record('fail', '读不到 src/components/player/VideoSurface.tsx');
} else {
  /*
   * 条件必须写在**渲染点**上：`{currentView !== 'feed' && (<PlayerControls ... />)}`。
   * 用 props 透传（`feedMode`）或让 PlayerControls 内部 `return null` 都不合格——
   * 前者要改组件接口，后者会让组件"被渲染但不挂 DOM"，它的定时器/轮询照跑。
   */
  check(
    'VideoSurface 用 currentView !== \'feed\' 作为 PlayerControls 的渲染条件',
    /\{\s*currentView\s*!==\s*'feed'\s*&&\s*\(\s*\n?\s*<PlayerControls\b/.test(surface),
    '没找到 `{currentView !== \'feed\' && (<PlayerControls ...` —— 推荐页又会挂上主播放器控制条',
  );

  const controlsMounts = surface.match(/<PlayerControls\b/g) ?? [];
  check(
    'PlayerControls 只有一个挂载点（不会出现第二条无条件渲染路径）',
    controlsMounts.length === 1,
    `找到 ${controlsMounts.length} 处 <PlayerControls —— 多出来的那处绕过了 feed 判断`,
  );

  check(
    '没有退回 `feedMode={currentView === \'feed\'}` 的旧写法',
    !/feedMode=\{\s*currentView\s*===\s*'feed'\s*\}/.test(surface),
    '旧写法意味着 PlayerControls 在 feed 下照样渲染，只是少个标题',
  );
}

// ---------------------------------------------------------------------------
// 2. VideoSurface：feed 单击不切换播放/暂停
// ---------------------------------------------------------------------------

if (surface) {
  const surfaceCode = stripComments(surface);
  const clickBody = extractFunctionBody(surfaceCode, 'const handleVideoSurfaceClick = () => {');
  if (!clickBody) {
    record('fail', '抽不到 handleVideoSurfaceClick 的函数体（签名被改名或重构成别的形式）');
  } else {
    const feedGuardAt = clickBody.indexOf("currentView === 'feed'");
    const firstToggleAt = clickBody.indexOf('togglePlay()');
    check(
      'handleVideoSurfaceClick 有 feed 分支',
      feedGuardAt >= 0,
      '没有 `currentView === \'feed\'` 分支 —— 单击画面会直接走到 togglePlay',
    );
    check(
      'feed 分支出现在第一次 togglePlay() 之前（不该先切换再判断）',
      feedGuardAt >= 0 && (firstToggleAt < 0 || feedGuardAt < firstToggleAt),
      'feed 判断在 togglePlay 之后 —— 单击仍然先暂停了画面',
    );
    check(
      'feed 分支必须以 return 结束（不许继续走主播放器的单击语义）',
      feedGuardAt >= 0 && /currentView\s*===\s*'feed'[\s\S]{0,4000}?return;/.test(clickBody.slice(Math.max(0, feedGuardAt - 200))),
      'feed 分支里没有 return —— 判断形同虚设',
    );

    /*
     * feed 分支里不许直接调 togglePlay。截取"feed 判断 → 该分支的 return"之间那段，
     * 确认它只做 UI 活动唤醒（handleUserActivity）与双击全屏（toggleFullscreen）。
     */
    if (feedGuardAt >= 0) {
      const afterGuard = clickBody.slice(feedGuardAt);
      const guardReturnAt = afterGuard.indexOf('return;');
      const branch = guardReturnAt >= 0 ? afterGuard.slice(0, guardReturnAt) : '';
      check(
        'feed 分支内不出现 togglePlay()',
        branch.length > 0 && !/togglePlay\(\)/.test(branch),
        'feed 分支里仍然调用了 togglePlay —— 「点一次就关一次」会原样复现',
      );
      check(
        'feed 分支保留双击全屏（导航语义不变）',
        branch.includes('toggleFullscreen()'),
        'feed 里双击全屏被一起删掉了 —— 那是导航语义，不该随播放语义一起改',
      );
      check(
        'feed 单击仍会唤醒控制层（handleUserActivity）',
        branch.includes('handleUserActivity'),
        '单击什么都不做会让信息层/控制层在 feed 里再也唤不醒',
      );
    }

    check(
      '主播放器（player）路径的单击暂停仍在',
      clickBody.slice(Math.max(0, firstToggleAt - 400)).includes('togglePlay()'),
      '把单击暂停整体删掉了 —— 主播放器的手感不该被 feed 的修复波及',
    );
  }
}

// ---------------------------------------------------------------------------
// 3 & 4. PlayerControls：原生入口的 feed 守卫 + 元素真值判据
// ---------------------------------------------------------------------------

const controls = readSource('src/components/player/PlayerControls.tsx');
if (!controls) {
  record('fail', '读不到 src/components/player/PlayerControls.tsx');
} else {
  const controlsCode = stripComments(controls);
  const openNativeBody = extractFunctionBody(controlsCode, 'const handleOpenNative = async () => {');
  if (!openNativeBody) {
    record('fail', '抽不到 handleOpenNative 的函数体（签名被改名或改成 useCallback）');
  } else {
    const guardAt = openNativeBody.indexOf("currentView === 'feed'");
    const nativeOpenAt = openNativeBody.indexOf('nativeOpen(');
    check(
      'handleOpenNative 有 feed 守卫',
      guardAt >= 0 && /currentView\s*===\s*'feed'\s*\)\s*return;|currentView\s*===\s*'feed'[\s\S]{0,200}?return;/.test(openNativeBody),
      '推荐页误触原生入口会起一个 mpv 窗口并暂停本窗口 —— 这正是「点一次自动关闭一次」',
    );
    check(
      'feed 守卫在 nativeOpen 之前（守卫不能放在调用之后）',
      guardAt >= 0 && nativeOpenAt >= 0 && guardAt < nativeOpenAt,
      '守卫写在了 nativeOpen 之后，窗口已经起来了才 return',
    );
    check(
      '暂停本窗口的判据是元素真值（!hostVideo.paused），不是 isPlaying',
      openNativeBody.includes('!hostVideo.paused'),
      '回到 `if (isPlaying) togglePlay()` 会出现反向操作：state 与元素不同步时反而播起来，两套音频',
    );
    check(
      'handleOpenNative 里不再出现 `if (isPlaying) togglePlay()`',
      !/if\s*\(\s*isPlaying\s*\)\s*togglePlay\(\)/.test(openNativeBody),
      '旧写法回来了 —— isPlaying 是事件镜像，与 video.paused 最多差一拍',
    );
  }
}

// ---------------------------------------------------------------------------
// 5. 连播（连续播）的调用链
// ---------------------------------------------------------------------------

const store = readSource('src/stores/usePlaybackStore.tsx');
if (!store) {
  record('fail', '读不到 src/stores/usePlaybackStore.tsx');
} else {
  const storeCode = stripComments(store);
  const endedBody = extractFunctionBody(storeCode, 'const handleEnded = () => {');
  if (!endedBody) {
    record('fail', '抽不到 handleEnded 的函数体');
  } else {
    check(
      'handleEnded 仍然通过 playNextEpisodeRef 进下一集（连播唯一触发点）',
      endedBody.includes('playNextEpisodeRef.current()'),
      '自动播完不再进下一集 —— 用户就得手动点，正是「要连续点击才能连续播放」',
    );
    check(
      'handleEnded 仍受连播开关约束（ctxEnded.autoNext）',
      endedBody.includes('ctxEnded.autoNext'),
      '关掉连播设置也会自动跳集',
    );
    check(
      'handleEnded 仍走去重闩锁 tryClaimAutoAdvance（防一次播完连跳多集）',
      endedBody.includes('tryClaimAutoAdvance('),
      '去重闩锁被绕过 —— 一次 ended 会连跳 2~3 集',
    );
  }

  check(
    '播放器有 AUTO_ADVANCE_SETTLE_MS 结算期（装载后 2 秒内的跳集触发一律作废）',
    /const AUTO_ADVANCE_SETTLE_MS\s*=\s*\d+/.test(storeCode),
    '结算期常量被删 —— 装载瞬间的旧源残留事件会立刻触发跳集',
  );

  const autoplayRetries = storeCode.match(/retryMutedAutoplay\(/g) ?? [];
  check(
    '换了集仍会走 retryMutedAutoplay（静音重试，绕开无手势 autoplay 拦截）',
    autoplayRetries.length >= 3,
    `只有 ${autoplayRetries.length} 处 retryMutedAutoplay —— 主链路/整集链路/兜底链某一条丢了自动播放`,
  );

  check(
    '起播仍有界（playBounded 仍然包住 video.play()）',
    storeCode.includes('export function playBounded('),
    'playBounded 被删/改名 —— play() 挂死时界面会永远停在旧帧、连错误卡都不出',
  );

  check(
    'seek 落位后按"是否正在播"恢复（resumeAfterSeek 传 shouldPlay）',
    /resumeAfterSeek\(video,\s*plan\.target,\s*shouldPlay\)/.test(storeCode),
    'seek 之后的恢复被删 —— 拖完进度条画面会停在 waiting',
  );
}

// ---------------------------------------------------------------------------
// 6. feed 侧连续播的入口（只读断言：另一条线独占这些文件）
// ---------------------------------------------------------------------------

const feedStore = readSource('src/stores/useFeedStore.ts');
if (!feedStore) {
  record('warn', '读不到 src/stores/useFeedStore.ts（跳过 feed 连续播入口断言）');
} else {
  check(
    'feed 换集仍带 detailOverride（省一次详情往返，切集不会先卡 1 秒空白）',
    /openEpisodeRef\.current\(item\.id,\s*episode\.id,\s*position,\s*undefined,\s*nextDetail\)/.test(feedStore),
    'startEpisode 没把已经拿到的详情带进 openEpisode —— 每次切集都要重新取详情',
  );
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

const icon = { pass: '✓', warn: '⚠', fail: '✗' };
for (const item of results) {
  const line = `${icon[item.kind]} ${item.title}`;
  if (item.kind === 'pass') console.log(line);
  else console.log(`${line}\n    ${item.detail}`);
}
const passed = results.filter(item => item.kind === 'pass').length;
const warned = results.filter(item => item.kind === 'warn').length;
console.log('');
console.log(`verify:feed-playback —— ${passed} 通过 / ${failed} 失败 / ${warned} 警告`);
process.exit(failed > 0 ? 1 : 0);