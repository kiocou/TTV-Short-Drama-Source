// DOM 集成用例跑者：自带静态服务器 + 隔离 profile 的无头 Chrome + CDP 驱动。
//
// 运行：node src/services/danmaku.dom-it/run.mjs
//
// 边界：
//  * 只用本机已装的 Chrome/Edge，**不加依赖**；
//  * 独立 `--user-data-dir`（`.tmp-danmaku-dom/profile`）、独立 127.0.0.1 端口
//    （`listen(0)` 由系统分配）——不碰用户浏览器、不抢用户端口；
//  * 只结束自己启动的那个浏览器进程，结束后删临时目录；
//  * 无真实网络、无账号、无视频。取数由 `fakeIpc.ts` 提供。

import { spawn, spawnSync } from 'node:child_process';
import { createServer } from 'node:http';
import { existsSync, readFileSync, rmSync, mkdirSync, createReadStream, statSync } from 'node:fs';
import path from 'node:path';
import { buildBundle, outDir, repoRoot } from './build.mjs';

/** 本次运行唯一的 profile 目录（带 pid + 时间戳），绝不复用上一次的。 */
const profileDir = path.join(outDir, `profile-${process.pid}-${Date.now()}`);

/** 只在 target/danmaku-verify 之内删东西；越界直接报错而不是删。 */
async function safeRemove(dir, { retries = 12, delayMs = 250 } = {}) {
  const resolved = path.resolve(dir);
  const root = path.resolve(outDir);
  if (resolved !== root && !resolved.startsWith(root + path.sep)) {
    throw new Error('拒绝清理 target 之外的路径: ' + resolved);
  }
  for (let i = 0; i < retries; i += 1) {
    try {
      rmSync(resolved, { recursive: true, force: true });
      return true;
    } catch (error) {
      if (i === retries - 1) {
        console.log('WARN 临时目录未能删除（浏览器可能还占着文件）: ' + (error?.code ?? error));
        return false;
      }
      await new Promise(r => setTimeout(r, delayMs));
    }
  }
  return false;
}

function waitExit(child, ms) {
  return new Promise(resolve => {
    if (child.exitCode != null) return resolve(true);
    const timer = setTimeout(() => resolve(false), ms);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

let pass = 0;
let fail = 0;
function check(name, ok, detail = '') {
  if (ok) pass += 1;
  else fail += 1;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : '  ' + detail));
}
const near = (actual, expected, tol) =>
  typeof actual === 'number' && Math.abs(actual - expected) <= tol;

function findBrowser() {
  const candidates = [
    process.env.PROGRAMFILES + '\\Google\\Chrome\\Application\\chrome.exe',
    (process.env['PROGRAMFILES(X86)'] ?? '') + '\\Google\\Chrome\\Application\\chrome.exe',
    process.env['PROGRAMFILES(X86)'] + '\\Microsoft\\Edge\\Application\\msedge.exe',
    process.env.PROGRAMFILES + '\\Microsoft\\Edge\\Application\\msedge.exe',
    (process.env.LOCALAPPDATA ?? '') + '\\Google\\Chrome\\Application\\chrome.exe',
  ];
  return candidates.find(p => p && existsSync(p)) ?? null;
}

function serve(root) {
  const types = { '.js': 'text/javascript', '.html': 'text/html' };
  const server = createServer((req, res) => {
    const rel = decodeURIComponent((req.url ?? '/').split('?')[0]);
    const file = path.join(root, rel === '/' ? 'index.html' : rel);
    if (!file.startsWith(root) || !existsSync(file) || !statSync(file).isFile()) {
      res.writeHead(404);
      res.end('not found');
      return;
    }
    res.writeHead(200, { 'content-type': types[path.extname(file)] ?? 'application/octet-stream' });
    createReadStream(file).pipe(res);
  });
  return new Promise(resolve => {
    server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port }));
  });
}

async function waitFile(file, ms) {
  const started = Date.now();
  for (;;) {
    if (existsSync(file)) return true;
    if (Date.now() - started > ms) return false;
    await new Promise(r => setTimeout(r, 100));
  }
}

class Cdp {
  constructor(url) {
    this.ws = new WebSocket(url);
    this.id = 0;
    this.pending = new Map();
    this.events = new Map();
    this.ready = new Promise((resolve, reject) => {
      this.ws.addEventListener('open', () => resolve());
      this.ws.addEventListener('error', e => reject(new Error('ws error: ' + e?.message)));
    });
    this.ws.addEventListener('message', ev => {
      const msg = JSON.parse(ev.data);
      if (msg.id != null && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(msg.error.message));
        else resolve(msg.result);
        return;
      }
      const waiters = this.events.get(msg.method);
      if (waiters) {
        this.events.delete(msg.method);
        waiters.forEach(w => w(msg.params));
      }
    });
  }

  send(method, params = {}) {
    this.id += 1;
    const id = this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }

  once(method, ms = 15000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('等待事件超时: ' + method)), ms);
      const wrap = p => {
        clearTimeout(timer);
        resolve(p);
      };
      this.events.set(method, [wrap]);
    });
  }

  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
      userGesture: true,
    });
    if (result.exceptionDetails) {
      throw new Error('页面异常: ' + JSON.stringify(result.exceptionDetails.exception ?? result.exceptionDetails));
    }
    return result.result?.value;
  }

  close() {
    try {
      this.ws.close();
    } catch {
      // 忽略关闭异常。
    }
  }
}

const EPISODES = `{
  e1: { latencyMs: 120, items: [
    { id: 'c1', text: 'C1', timeMs: 500 },
    { id: 'c2', text: 'C2', timeMs: 1500 },
    { id: 'c3', text: 'C3', timeMs: 2500 } ] },
  e2: { latencyMs: 900, items: [
    { id: 'c1', text: 'E2-a', timeMs: 1000 },
    { id: 'c2', text: 'E2-b', timeMs: 2000 },
    { id: 'c3', text: 'E2-c', timeMs: 2900 } ] },
  e3: { latencyMs: 100, items: [
    { id: 'z1', text: 'Z1', timeMs: 1000 },
    { id: 'z2', text: 'Z2', timeMs: 2400 } ] },
  e4: { latencyMs: 60, items: [
    { id: 'x0', text: 'X0', timeMs: 29000 },
    { id: 'x1', text: 'X1', timeMs: 31000 },
    { id: 'x2', text: 'X2', timeMs: 32500 } ] },
  e5: { latencyMs: 80, items: [
    { id: 'r1', text: 'R1', timeMs: 50000 },
    { id: 'r2', text: 'R2', timeMs: 50800 },
    { id: 'r3', text: 'R3', timeMs: 59000 } ] },
  e6: { latencyMs: 80, items: [
    { id: 'n1', text: 'N1', timeMs: 500 },
    { id: 'n2', text: 'N2', timeMs: 1200 } ] },
  e7: { latencyMs: 80, items: [
    { id: 'p1', text: 'P1', timeMs: 500 },
    { id: 'p2', text: 'P2', timeMs: 1400 } ] }
}`;

async function main() {
  const browser = findBrowser();
  if (!browser) {
    console.log('UNRUN 未找到本机 Chrome/Edge：集成用例**未运行**（不是通过）');
    return 2;
  }
  await buildBundle();
  await safeRemove(profileDir);
  mkdirSync(profileDir, { recursive: true });

  const { server, port } = await serve(outDir);
  const child = spawn(
    browser,
    [
      '--headless=new',
      '--disable-gpu',
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-extensions',
      '--disable-background-timer-throttling',
      '--disable-renderer-backgrounding',
      '--disable-backgrounding-occluded-windows',
      '--hide-scrollbars',
      '--mute-audio',
      '--window-size=600,400',
      '--remote-debugging-port=0',
      '--remote-allow-origins=*',
      '--user-data-dir=' + profileDir,
      'about:blank',
    ],
    { stdio: 'ignore' },
  );

  /*
   * 清理只碰**本次**启动的那一个进程（`taskkill` 用本进程 pid + `/T`），
   * 然后等它真的退出，再删本次唯一 profile；其它浏览器实例一概不看、不动。
   */
  const cleanup = async () => {
    try {
      spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
    } catch {
      // 进程可能已自行退出，忽略。
    }
    if (!child.killed) {
      try {
        child.kill();
      } catch {
        // 忽略。
      }
    }
    await waitExit(child, 8000);
    server.close();
  };

  let cdp = null;
  try {
    const portFile = path.join(profileDir, 'DevToolsActivePort');
    if (!(await waitFile(portFile, 15000))) throw new Error('浏览器未写出 DevToolsActivePort');
    const devtoolsPort = Number(readFileSync(portFile, 'utf8').split('\n')[0]);
    const targets = await (
      await fetch('http://127.0.0.1:' + devtoolsPort + '/json/list')
    ).json();
    const page = targets.find(t => t.type === 'page');
    if (!page) throw new Error('没有 page target');

    cdp = new Cdp(page.webSocketDebuggerUrl);
    await cdp.ready;
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    const loaded = cdp.once('Page.loadEventFired');
    await cdp.send('Page.navigate', { url: 'http://127.0.0.1:' + port + '/index.html' });
    await loaded;

    const boot = await cdp.evaluate(`(function(){ return __it.boot(document.getElementById('root')) })()`);
    if (boot !== true) throw new Error('挂载失败');

    // ── 1. 冷启动首窗：请求在提交时刻发出，首条可见由数据/媒体决定 ─────────
    const cold = await cdp.evaluate(`(async function(){
      __it.setFake({ episodes: ${EPISODES} });
      __it.setStage(400, 328);
      __it.resetRequests();
      __it.setProps({ seriesId: 's1', episodeId: 'e1', enabled: true, bold: false, font: 'A', fontScale: 100, speed: 1 });
      const first = await __it.waitRequests(1, 800);
      const atStart = __it.requests();
      await __it.settle(200);
      __it.setTime(0);
      __it.clock(true);
      const waited = await __it.waitVisible(1, 2500);
      __it.clock(false);
      return { first, atStart, requests: __it.requests(), waited, snap: __it.snapshot() };
    })()`);
    check('集成·冷启动：启用即发出首窗请求', cold.first.ok === true, JSON.stringify(cold.first));
    check('集成·冷启动：有弹幕可见', cold.waited.ok === true, JSON.stringify(cold.waited));
    check(
      '集成·冷启动：首窗请求 = 当前集、0 窗口',
      cold.atStart.length === 1 && cold.atStart[0].episodeId === 'e1' && cold.atStart[0].startMs === 0,
      JSON.stringify(cold.atStart),
    );
    check(
      '集成·冷启动：页到达后不重复请求（同一窗口只拉一次）',
      cold.requests.length === 1,
      '实际=' + cold.requests.length,
    );
    check(
      '集成·冷启动：可见内容是这一集的数据（未伪造）',
      cold.snap.visibleTexts.length > 0 && cold.snap.visibleTexts.every(t => ['C1', 'C2', 'C3'].includes(t)),
      JSON.stringify(cold.snap.visibleTexts),
    );

    // ── 2. 粗体变化、rows 不变 → 必须按新字宽重排，且不重新取数 ───────────
    const bold = await cdp.evaluate(`(async function(){
      __it.setProps({ bold: false, font: 'A', fontScale: 100 });
      __it.setTime(2.0);
      await __it.settle(150);
      const before = { w: __it.flightWidth('C1'), rows: __it.rowsHint(), req: __it.requests().length };
      __it.setProps({ bold: true });
      await __it.settle(150);
      const after = { w: __it.flightWidth('C1'), rows: __it.rowsHint(), req: __it.requests().length, snap: __it.snapshot() };
      return { before, after };
    })()`);
    check(
      '集成·粗体：rows/width 未变（前提成立）',
      bold.before.rows.rows === bold.after.rows.rows && bold.before.rows.layerHeight === bold.after.rows.layerHeight,
      JSON.stringify([bold.before.rows, bold.after.rows]),
    );
    check(
      '集成·粗体：排程宽度按新量宽重算（≈1.5 倍）',
      near((bold.after.w - 8) / (bold.before.w - 8), 1.5, 0.12),
      'before=' + bold.before.w + ' after=' + bold.after.w,
    );
    check('集成·粗体：弹幕层字重已切到 700（节点继承）', bold.after.snap.layerFontWeight === '700', '实际=' + bold.after.snap.layerFontWeight);
    check('集成·粗体：没有因此多打一次取数请求', bold.after.req === bold.before.req, `${bold.before.req} → ${bold.after.req}`);

    // ── 3. 字体族变化（rows/width 不变）→ 同理 ────────────────────────────
    const font = await cdp.evaluate(`(async function(){
      __it.setProps({ bold: false, font: 'A', fontScale: 100 });
      __it.setTime(2.0);
      await __it.settle(150);
      const before = { w: __it.flightWidth('C1'), rows: __it.rowsHint() };
      __it.setProps({ font: 'B' });
      await __it.settle(150);
      const after = { w: __it.flightWidth('C1'), rows: __it.rowsHint(), req: __it.requests().length };
      return { before, after };
    })()`);

    check(
      '集成·字体族：rows 未变、排程宽度按新量宽重算（≈2 倍）',
      font.before.rows.rows === font.after.rows.rows && near((font.after.w - 8) / (font.before.w - 8), 2, 0.25),
      'before=' + font.before.w + ' after=' + font.after.w,
    );

    // ── 4. 字号变化但 rows 不变 → 同理 ───────────────────────────────────
    const scale = await cdp.evaluate(`(async function(){
      __it.setProps({ bold: false, font: 'A', fontScale: 100 });
      __it.setTime(2.0);
      await __it.settle(150);
      const before = { w: __it.flightWidth('C1'), rows: __it.rowsHint() };
      __it.setProps({ fontScale: 120 });
      await __it.settle(150);
      const after = { w: __it.flightWidth('C1'), rows: __it.rowsHint() };
      return { before, after };
    })()`);
    check(
      '集成·字号：120% 时 rows 仍相同（前提成立）',
      scale.before.rows.rows === scale.after.rows.rows,
      JSON.stringify([scale.before.rows, scale.after.rows]),
    );
    check(
      '集成·字号：排程宽度按新量宽重算（≈1.2 倍）',
      near((scale.after.w - 8) / (scale.before.w - 8), 1.2, 0.12),
      'before=' + scale.before.w + ' after=' + scale.after.w,
    );

    // ── 5. 换集：慢新页 + 媒体头仍旧 + 相同弹幕 ID → 当次提交丢弃旧排程 ────
    const swap = await cdp.evaluate(`(async function(){
      __it.setProps({ seriesId: 's1', episodeId: 'e1', enabled: true, bold: false, font: 'A', fontScale: 100 });
      __it.clock(false);
      __it.setTime(3.0);
      await __it.settle(200);
      const before = __it.snapshot();
      __it.resetRequests();
      const timeBefore = __it.setTime(3.0);          // 媒体头故意停在旧值
      __it.setProps({ episodeId: 'e2' });
      const cleared = await __it.waitNone(200);
      const mid = __it.snapshot();
      await __it.settle(1100);                        // e2 的页 900ms 才到
      const after = __it.snapshot();
      return { before, cleared, mid, after, requests: __it.requests(), timeBefore, timeAfter: 3.0 };
    })()`);
    check('集成·换集：切换前 e1 确实有可见弹幕（前提成立）', swap.before.visible > 0, JSON.stringify(swap.before.visibleTexts));
    check('集成·换集：切换后 200ms 内旧集弹幕全部不可见', swap.cleared.ok === true, JSON.stringify(swap.cleared));
    check('集成·换集：新页未到时画面上没有任何旧节点', swap.mid.nodeCount === 0, JSON.stringify(swap.mid.texts));
    check(
      '集成·换集：新页到达后只出现新集内容（相同 id 未与旧集融合）',
      swap.after.visible > 0 && swap.after.visibleTexts.every(t => t.startsWith('E2-')),
      JSON.stringify(swap.after.visibleTexts),
    );
    check(
      '集成·换集：取数请求属于新集',
      swap.requests.length >= 1 && swap.requests.every(r => r.episodeId === 'e2'),
      JSON.stringify(swap.requests.map(r => r.episodeId)),
    );

    // ── 6. 关开：关掉立刻无层、打开重新取数并出弹幕 ──────────────────────
    const toggle = await cdp.evaluate(`(async function(){
      __it.setProps({ enabled: false });
      await __it.settle(120);
      const off = __it.snapshot();
      __it.resetRequests();
      __it.setProps({ enabled: true });
      const req = await __it.waitVisible(1, 100);
      const after = __it.requests();
      return { off, req, visible: req, snap: __it.snapshot(), requests: after };
    })()`);
    check('集成·开关：关掉后弹幕层不渲染', toggle.off.hasLayer === false, JSON.stringify(toggle.off));
    check('集成·开关：重新打开直接显示共享缓存，不再发首窗请求', toggle.req.ok === true && toggle.requests.length === 0, JSON.stringify(toggle));
    check('集成·开关：重新打开后重新出弹幕', toggle.visible.ok === true, JSON.stringify(toggle.visible));
    check(
      '集成·开关：重新打开后内容是当前集（e2）',
      toggle.snap.visibleTexts.every(t => t.startsWith('E2-')),
      JSON.stringify(toggle.snap.visibleTexts),
    );

    // ── 7. 尺寸为 0 时页已到 → 宽度恢复后必须补排程，且不重新取数 ─────────
    const zero = await cdp.evaluate(`(async function(){
      __it.setProps({ episodeId: 'e3', enabled: true, bold: false, font: 'A', fontScale: 100 });
      __it.setStage(0, 328);
      __it.resetRequests();
      __it.setTime(1.0);
      const req = await __it.waitRequests(1, 900);
      await __it.settle(250);
      const trapped = __it.snapshot();
      __it.setStage(400, 328);
      const recovered = await __it.waitVisible(1, 900);
      return { req, trapped, recovered, snap: __it.snapshot(), requests: __it.requests() };
    })()`);
    check('集成·零宽：请求照常发出（不是取数问题）', zero.req.ok === true, JSON.stringify(zero.req));
    check('集成·零宽：宽度为 0 时无法排程（前提成立）', zero.trapped.nodeCount === 0, JSON.stringify(zero.trapped));
    check('集成·零宽：宽度恢复后自动补排程并出弹幕', zero.recovered.ok === true, JSON.stringify(zero.recovered));
    check(
      '集成·零宽：恢复过程没有再打请求（用同一页重排）',
      zero.requests.length === 1,
      'requests=' + zero.requests.length,
    );

    // ── 8. 同一集跨 30s 窗口：预取生效且不丢在飞弹幕 ─────────────────────
    const boundary = await cdp.evaluate(`(async function(){
      __it.setProps({ episodeId: 'e4', enabled: true, bold: false, font: 'A', fontScale: 100 });
      __it.setStage(400, 328);
      __it.resetRequests();
      __it.clock(false);
      __it.setTime(0);
      await __it.settle(200);
      __it.setTime(29.5);
      await __it.settle(400);
      const at29 = __it.snapshot();
      __it.setTime(32.6);
      await __it.settle(400);
      const at32 = __it.snapshot();
      return { at29, at32, requests: __it.requests() };
    })()`);
    check('集成·跨窗口：29.5s 处 X0 可见', boundary.at29.visibleTexts.includes('X0'), JSON.stringify(boundary.at29.visibleTexts));
    check(
      '集成·跨窗口：下一窗口已预取（两个窗口各一次请求）',
      boundary.requests.some(r => r.startMs === 0) && boundary.requests.some(r => r.startMs === 30000),
      JSON.stringify(boundary.requests.map(r => r.startMs)),
    );
    check(
      '集成·跨窗口：32.6s 处跨窗口的旧弹幕仍在飞（未清屏、两代合并）',
      boundary.at32.visibleTexts.includes('X0') && boundary.at32.visibleTexts.includes('X1'),
      JSON.stringify(boundary.at32.visibleTexts),
    );
  // ── 9. 续播 50s：装载期直接拉 30s 窗口，不白拉 0 窗、不提前未来弹幕 ────
    const resume = await cdp.evaluate(`(async function(){
      __it.setProps({ enabled: false });
      await __it.settle(120);
      __it.resetRequests();
      __it.clock(false);
      __it.setTime(0);                                 // 媒体尚未归位（仍是 0）
      __it.setStage(400, 328);
      __it.setProps({ episodeId: 'e5', enabled: true, openStartMs: 50000, bold: false, font: 'A', fontScale: 100 });
      const first = await __it.waitRequests(1, 900);
      // 装载期模拟 store 的 opening→buffering：媒体一直是 0，持续 0.8 秒。
      await __it.settle(800);
      const requests = __it.requests();
      const beforeAlign = __it.snapshot();
      __it.setTime(50.2);                              // 媒体真正归位
      await __it.settle(300);
      const aligned = __it.snapshot();
      return { first, requests, beforeAlign, aligned, afterAlign: __it.requests() };
    })()`);
    check('集成·续播：装载期立刻发出首窗请求', resume.first.ok === true, JSON.stringify(resume.first));
    check(
      '集成·续播：装载期（媒体 0 持续 0.8s，等价 opening→buffering）只拉 30000 窗',
      resume.requests.length === 1 && resume.requests[0].startMs === 30000,
      JSON.stringify(resume.requests),
    );
    check(
      '集成·续播：媒体未归位时不显示未来时间戳',
      resume.beforeAlign.visible === 0,
      JSON.stringify(resume.beforeAlign.visibleTexts),
    );
    check(
      '集成·续播：归位后显示对应弹幕（R1 在 50.0s）',
      resume.aligned.visibleTexts.includes('R1'),
      JSON.stringify(resume.aligned.visibleTexts),
    );
    check(
      '集成·续播：远处的未来弹幕（R3 在 59.0s）没有被提前放出',
      resume.aligned.visibleTexts.includes('R3') === false,
      JSON.stringify(resume.aligned.visibleTexts),
    );
    check(
      '集成·续播：归位后没有补拉 0 窗（请求数仍为 1）',
      resume.afterAlign.length === 1,
      'requests=' + resume.afterAlign.length,
    );

    // ── 10. 起播就绪（openStartMs=null）+ 播放中开关：用当前播放头 ────────
    await cdp.evaluate(`__it.resetCache()`);
    const playing = await cdp.evaluate(`(async function(){
      __it.setProps({ openStartMs: null });            // 等价 uiState 离开 opening
      __it.setTime(62.0);
      __it.setProps({ openStartMs: null, positionMs: 62000, enabled: false });   // store 的 position 跟随播放头
      await __it.settle(200);
      __it.resetRequests();
      __it.setProps({ enabled: true });                // 播放中打开弹幕
      const req = await __it.waitRequests(1, 900);
      return { req, requests: __it.requests() };
    })()`);
    check(
      '集成·播放中开关：请求落在当前播放头所在的 60000ms 窗口',
      playing.req.ok === true && playing.requests[0].startMs === 60000,
      JSON.stringify(playing.requests),
    );

    // ── 11. 同集重开：只有目标变也立刻补发对应窗口 ───────────────────────
    const retarget = await cdp.evaluate(`(async function(){
      __it.resetRequests();
      __it.setProps({ openStartMs: 92000 });           // 同剧同集，只有目标变
      const req = await __it.waitRequests(1, 900);
      await __it.settle(150);
      return { req, requests: __it.requests() };
    })()`);
    check(
      '集成·同集重开：目标一变就补发对应窗口（90000ms），不等 250ms 节流',
      retarget.req.ok === true && retarget.requests.length === 1 && retarget.requests[0].startMs === 90000,
      JSON.stringify(retarget.requests),
    );

    // ── 12. 切新集从 0 开始：不继承上集目标 ─────────────────────────────
    await cdp.evaluate(`__it.resetCache()`);
    const fresh = await cdp.evaluate(`(async function(){
      __it.resetRequests();
      __it.setProps({ enabled: true, openStartMs: 0, episodeId: 'e6', positionMs: 0 });
      __it.setTime(1.5);
      const req = await __it.waitRequests(1, 900);
      await __it.settle(250);
      return { req, requests: __it.requests(), snap: __it.snapshot() };
    })()`);
    check(
      '集成·换新集：只拉 0 窗，不继承上集的 90000 目标',
      fresh.req.ok === true && fresh.requests.length === 1 && fresh.requests[0].startMs === 0,
      JSON.stringify(fresh.requests),
    );
    check(
      '集成·换新集：出的是新集弹幕',
      fresh.snap.visibleTexts.length > 0 && fresh.snap.texts.every(t => t.startsWith('N')),
      JSON.stringify(fresh.snap.texts),
    );
  // ── 13. 片尾目标被 alignPlayhead 夹回 0：目标释放后按媒体 0 拉 0 窗（不死锁）─
    await cdp.evaluate(`__it.resetCache()`);
    const nearEnd = await cdp.evaluate(`(async function(){
      __it.resetRequests();
      // 判据判定「片尾目标会被夹回 0」时 openStartMs 传 null，媒体就是 0。
      __it.setProps({ enabled: true, openStartMs: null, episodeId: 'e7', positionMs: 0 });
      __it.setTime(0.6);
      const req = await __it.waitRequests(1, 900);
      await __it.settle(200);
      return { req, requests: __it.requests(), snap: __it.snapshot() };
    })()`);
    check(
      '集成·片尾夹回 0：目标释放后正常拉 0 窗（不死等 59.5s 那页）',
      nearEnd.req.ok === true && nearEnd.requests.length === 1 && nearEnd.requests[0].startMs === 0,
      JSON.stringify(nearEnd.requests),
    );
    check(
      '集成·片尾夹回 0：正常出弹幕',
      nearEnd.snap.visibleTexts.length > 0,
      JSON.stringify(nearEnd.snap.visibleTexts),
    );
  } finally {
    cdp?.close();
    await cleanup();
    await safeRemove(profileDir);
  }
}

try {
  const code = await main();
  if (code === 2) {
    console.log('\n结果：UNRUN —— 未运行（无浏览器），不算通过');
    process.exitCode = 2;
  } else {
    console.log(`\n结果：${pass} 通过 / ${fail} 失败`);
    if (fail > 0) process.exitCode = 1;
  }
} catch (error) {
  console.log('集成用例失败：' + (error instanceof Error ? error.message : String(error)));
  process.exitCode = 1;
}