/**
 * `verify:feed-season-e2e` —— 「短剧推荐」换剧/换集的**真机 e2e**（CDP 直连）。
 *
 * ## 契约已更换
 *
 * 旧版本断言"末集播完自动进下一季第 1 集"并去 `history_list` 找下一季的记录。
 * 该行为已被撤掉：推荐页末集播完要**停留**在完成态，不换季也不换部。整份脚本
 * 因此重写为下面的新契约。
 *
 * | # | 契约 | 怎么读 |
 * | --- | --- | --- |
 * | 1 | 进入推荐页 → 当前剧是**第 1 集** | 信息层 `p` 以「第 1 集」开头 |
 * | 2 | `ArrowDown` → 换成**另一部**剧，且是**第 1 集 0 秒** | 标题变化 + 集号回到第 1 集 + `video.currentTime` 很小 |
 * | 3 | `ArrowUp` → 回**上一部**，同样从第 1 集 0 秒 | 标题回到上一部 + 集号回到第 1 集 |
 * | 4 | 末集按「下一集」是 **no-op**（不换季也不换部） | 一遍 `下一集` 后标题与集号都不变 |
 * | 5 | 末集播完出现**完成态**横幅，且**没有**跳到别的剧 | `[data-feed-status="completed"]` 出现，标题不变 |
 *
 * 契约 5 依赖"推荐页强制连播"（`services/autoNext.ts`）：**不需要**先把设置里的
 * `autoNext` 打开，也不写回设置。若实例当前是关闭态，脚本会把这一点打印出来，
 * 因为那正是最值得验证的情形。
 *
 * ## 前提（会以 assert 失败的形式喊出来）
 *
 * - 实例带 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9225 --remote-allow-origins=*"`；
 * - 推荐流里能找到一部**集数很少**（≤ 3）的剧：契约 4/5 必须真的走到末集，而推荐页
 *   没有"跳到末集"的入口（选集抽屉属于 `PlayerControls`，feed 下不渲染）。
 *
 * ## 诚实边界
 *
 * 需要运行中的应用实例，CI 与无 GUI 环境跑不了。确定性门禁是
 * `verify:feed-series-nav`（纯函数 + 源码契约），两者不能互相替代。
 *
 * 运行：`npm run verify:feed-season-e2e`（可用 `TTV_CDP_PORT` 改端口）
 */

import assert from 'node:assert/strict';
import { setTimeout as delay } from 'node:timers/promises';

const port = Number(process.env.TTV_CDP_PORT ?? 9225);
const baseUrl = `http://127.0.0.1:${port}`;
const targets = await (await fetch(`${baseUrl}/json/list`)).json();
const page = targets.find(target => target.type === 'page' && target.url.startsWith('http://tauri.localhost/'));
assert.ok(page, `No Tauri WebView found on CDP port ${port}`);

const socket = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true });
  socket.addEventListener('error', reject, { once: true });
});

let nextId = 0;
const pending = new Map();
socket.addEventListener('message', event => {
  const message = JSON.parse(event.data);
  const resolve = pending.get(message.id);
  if (resolve) {
    pending.delete(message.id);
    resolve(message);
  }
});

function call(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, resolve);
    socket.send(JSON.stringify({ id, method, params }));
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`CDP call timed out: ${method}`));
    }, 30_000);
    pending.set(id, message => {
      clearTimeout(timer);
      resolve(message);
    });
  });
}

async function evaluate(expression) {
  const response = await call('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (response.result?.exceptionDetails) {
    throw new Error(response.result.exceptionDetails.exception?.description ?? response.result.exceptionDetails.text);
  }
  return response.result?.result?.value;
}

async function pressKey(key) {
  const vk = key === 'ArrowDown' ? 40 : 38;
  const base = { key, code: key, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
  await call('Input.dispatchKeyEvent', { type: 'keyDown', ...base });
  await call('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
}

async function clickPlaybackPrompt() {
  const point = await evaluate(`(()=>{const button=[...document.querySelectorAll('button')].find(button=>button.textContent.trim()==='点击播放');if(!button)return null;const rect=button.getBoundingClientRect();return rect.width&&rect.height?{x:rect.left+rect.width/2,y:rect.top+rect.height/2}:null})()`);
  if (!point) return false;
  await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: point.x, y: point.y });
  await call('Input.dispatchMouseEvent', { type: 'mousePressed', x: point.x, y: point.y, button: 'left', clickCount: 1 });
  await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x: point.x, y: point.y, button: 'left', clickCount: 1 });
  return true;
}

async function waitFor(expression, label, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const result = await evaluate(expression);
    if (result) return result;
    await delay(250);
  }
  throw new Error(`Timed out waiting for ${label}`);
}

const invoke = async (command, args = {}) => {
  const encodedArgs = JSON.stringify(args);
  const result = await evaluate(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${encodedArgs}).then(JSON.stringify)`,
  );
  return JSON.parse(result);
};

/** 当前剧的标题（信息层 h1）。 */
const TITLE_EXPR = "(()=>{const s=document.querySelector('[aria-hidden=\"false\"] section[aria-label=\"当前短剧\"]');return s?.querySelector('h1')?.textContent.trim()||''})()";
/** 当前剧的二级行（「第 N 集 · 共 M 集 …」）。 */
const EPISODE_EXPR = "(()=>{const s=document.querySelector('[aria-hidden=\"false\"] section[aria-label=\"当前短剧\"]');return s?.querySelector('p')?.textContent??''})()";
/** 进度条「下一集」按钮（`FeedPlaybackBar` 自绘那一颗）。 */
const NEXT_BUTTON_EXPR = "[...document.querySelectorAll('[aria-hidden=\"false\"] button[aria-label=\"下一集\"]')][0]||null";

const feedVisible = "Boolean(document.querySelector('[aria-hidden=\"false\"] [aria-label=\"推荐频道\"]'))";

async function clickNextEpisode() {
  const clicked = await evaluate(`(()=>{const b=${NEXT_BUTTON_EXPR};if(!b||b.disabled)return false;b.click();return true})()`);
  assert.ok(clicked, '「下一集」按钮不可点（busy 或 detail 未就绪）');
}

async function enterFeed() {
  await evaluate(`(()=>{if(!${feedVisible}){[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='短剧推荐')?.click()}return true})()`);
  await waitFor(feedVisible, 'feed view');
}

/**
 * 等到"第 1 集"且画面从 0 秒附近开始。
 *
 * `currentTime` 只在媒体真的就绪后才可信，所以只在 `readyState >= 2` 时比较；
 * 媒体还没到时只断言集号 —— 这一条是**集号契约**，不冒充起播位置。
 */
async function waitFirstEpisode(label, timeoutMs = 60_000) {
  return waitFor(
    `(()=>{const ep=${EPISODE_EXPR};if(!ep.startsWith('第 1 集'))return null;const v=document.querySelector('video');if(!v)return {ep, position:null};if(v.readyState<2)return {ep, position:null};return {ep, position:v.currentTime}})()`,
    label,
    timeoutMs,
  );
}

try {
  assert.equal(await evaluate("typeof window.__TAURI_INTERNALS__?.invoke"), 'function', 'Tauri IPC is unavailable');
  const settings = await invoke('settings_get');
  // 推荐页强制连播（`services/autoNext.ts`），所以这里**不**要求 autoNext 是开的：
  // 关闭态反而是更有价值的情形。只把当前值记下来，方便回报时对照。
  const userAutoNext = settings.autoNext !== false;

  // 找一个集数很少的剧：契约 4/5 必须真的走到末集。
  const recommendations = await invoke('hongguo_discover', { channel: 'comic', cursor: null });
  const items = (recommendations.items ?? []).filter(item => item?.id && item?.title);
  assert.ok(items.length > 0, 'The live comic recommendation page returned no items');
  const shortSeries = items
    .filter(item => Number(item.episodesCount) > 0 && Number(item.episodesCount) <= 3)
    .sort((left, right) => Number(left.episodesCount) - Number(right.episodesCount))[0] ?? null;
  assert.ok(
    shortSeries,
    'No short-run (<= 3 episodes) comic in the live feed: this check needs to reach the last episode, '
    + 'and the feed has no "jump to last episode" entry. Widen the discovery window or run on a day with short series.',
  );

  await enterFeed();

  // ── 契约 1：进入推荐页即第 1 集 ──────────────────────────────────────────
  const firstEntry = await waitFirstEpisode('first feed episode', 90_000);
  if (firstEntry.position !== null) {
    assert.ok(firstEntry.position < 10, `首集起播点不在开头：${firstEntry.position}s`);
  }

  await evaluate(`(()=>{const g=document.querySelector('[aria-hidden="false"] [aria-label="推荐频道"]');[...g.querySelectorAll('button')].find(b=>b.textContent.trim()==='漫剧')?.click();return true})()`);
  await waitFor(
    "document.querySelector('[aria-hidden=\"false\"] [aria-label=\"推荐频道\"] button[aria-pressed=\"true\"]')?.textContent.trim()==='漫剧'",
    'comic channel',
  );
  await waitFirstEpisode('first comic episode', 120_000);
  const previousTitle = await evaluate(TITLE_EXPR);
  assert.ok(previousTitle, 'The feed shows no series title');

  // ── 契约 2：ArrowDown → 另一部剧，第 1 集 0 秒 ───────────────────────────
  await pressKey('ArrowDown');
  const nextTitle = await waitFor(
    `(()=>{const t=${TITLE_EXPR};return t&&t!==${JSON.stringify(previousTitle)}?t:null})()`,
    'next series after ArrowDown',
    120_000,
  );
  const nextFirst = await waitFirstEpisode('next series first episode', 120_000);
  if (nextFirst.position !== null) {
    assert.ok(nextFirst.position < 10, `换剧后没有从 0 秒起播：${nextFirst.position}s`);
  }

  // ── 契约 3：ArrowUp → 回上一部，同样从第 1 集 0 秒 ───────────────────────
  await pressKey('ArrowUp');
  await waitFor(
    `(()=>{const t=${TITLE_EXPR};return t===${JSON.stringify(previousTitle)}?t:null})()`,
    'previous series after ArrowUp',
    120_000,
  );
  const backFirst = await waitFirstEpisode('previous series first episode', 120_000);
  if (backFirst.position !== null) {
    assert.ok(backFirst.position < 10, `反向回上一部没有从 0 秒起播：${backFirst.position}s`);
  }

  // ── 契约 4：走到末集后「下一集」是 no-op ─────────────────────────────────
  // 先把队列推到"集数很少的那一部"。换一部用键盘（与用户路径一致），
  // 每次换完都等标题真的变了，避免节流期内的按键被吃掉。
  let guard = 0;
  while ((await evaluate(TITLE_EXPR)) !== shortSeries.title && guard < items.length + 4) {
    guard += 1;
    const before = await evaluate(TITLE_EXPR);
    await pressKey('ArrowDown');
    await waitFor(
      `(()=>{const t=${TITLE_EXPR};return t&&t!==${JSON.stringify(before)}?t:null})()`,
      `advance to ${shortSeries.title} (attempt ${guard})`,
      120_000,
    );
  }
  assert.equal(await evaluate(TITLE_EXPR), shortSeries.title, 'The short-run series was not reachable in the feed queue');

  const lastNumber = Number(shortSeries.episodesCount);
  for (let step = 1; step < lastNumber; step += 1) {
    const before = await evaluate(EPISODE_EXPR);
    await clickNextEpisode();
    await waitFor(
      `(()=>{const ep=${EPISODE_EXPR};return ep&&ep!==${JSON.stringify(before)}?ep:null})()`,
      `episode ${step + 1} of ${lastNumber}`,
      120_000,
    );
  }
  const atLast = { title: await evaluate(TITLE_EXPR), episode: await evaluate(EPISODE_EXPR) };
  assert.ok(atLast.episode.startsWith(`第 ${lastNumber} 集`), `not on the last episode: ${atLast.episode}`);

  // 关键一条：末集再按「下一集」，标题与集号都不许变（不跨季、不换部）。
  await clickNextEpisode();
  await delay(3_000);
  assert.equal(await evaluate(TITLE_EXPR), atLast.title, '末集按「下一集」换了剧 —— 又回到自动跨季/换部');
  assert.equal(await evaluate(EPISODE_EXPR), atLast.episode, '末集按「下一集」换了集 —— 越过了集数边界');

  // ── 契约 5：末集播完停留完成态，且没有跳到别的剧 ─────────────────────────
  // 先把画面推到结尾（媒体不在播时手动点一次播放提示）。
  if (await evaluate('document.querySelector(\'video\')?.paused ?? true')) await clickPlaybackPrompt();
  const seeked = await evaluate(`(()=>{const v=document.querySelector('video');if(!v||!Number.isFinite(v.duration)||v.duration<=0)return null;v.currentTime=Math.max(0,v.duration-0.6);return v.duration})()`);
  assert.ok(seeked, '末集时长不可读 —— 无法把画面推到结尾');

  const completed = await waitFor(
    `(()=>{const b=document.querySelector('[aria-hidden="false"] [data-feed-status="completed"]');return b?b.textContent.trim():null})()`,
    'completed banner after the last episode ends',
    180_000,
  );
  assert.ok(completed.includes('已播放完成'), `完成态横幅文案不对：${completed}`);
  assert.equal(await evaluate(TITLE_EXPR), atLast.title, '末集播完后换了剧 —— 用户要求"停留完成状态"');

  console.log(JSON.stringify({
    status: 'PASS',
    series: shortSeries.title,
    episodes: lastNumber,
    userAutoNextSetting: userAutoNext,
    verified: [
      'feed entry starts at episode 1',
      'ArrowDown switches to another series at episode 1',
      'ArrowUp returns to the previous series at episode 1',
      'manual next-episode is a no-op on the last episode',
      'last episode ends into the completed banner without jumping to another series',
    ],
  }, null, 2));
} finally {
  socket.close();
}