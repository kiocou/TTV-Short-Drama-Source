#!/usr/bin/env node
/**
 * TTV 真实运行时探测（CDP）。
 *
 * ## 为什么需要它
 *
 * 前面所有验证都是在两种"间接环境"里做的：
 *  - Rust 台架：证明接口能通，但证明不了**界面真的这么渲染**；
 *  - 浏览器 + mock：证明界面结构，但数据层是假的。
 *
 * 真实应用（Tauri + WebView2）里两者都有，但过去没有可控的入口：
 * WebView2 不是 Playwright 能接管的 Chromium，UI Automation 在它上面只暴露空
 * `Pane`（实测 `seat_observe` 的树里一个按钮都没有），坐标点击又只能靠截图猜。
 *
 * 真正的入口是 **CDP**：WebView2 支持 `--remote-debugging-port`，用
 * `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 传进去即可。之后用原生 WebSocket
 * 跑 `Runtime.evaluate`，就能读真实 DOM、点真实按钮。
 *
 * ## 用法
 *
 * ```sh
 * # 1. 在 Anode 虚拟桌面里起一个测试实例（隔离 TEMP 与 WebView2 目录）
 * #    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9223 --remote-allow-origins=*"
 * # 2. 探测
 * node tools/ttv-cdp.cjs list               # 列出可点的按钮，用来定位
 * node tools/ttv-cdp.cjs click "AI 剧"       # 按文本点一个按钮
 * node tools/ttv-cdp.cjs text "AI 剧|热度"   # 抓页面里匹配这些词的行
 * node tools/ttv-cdp.cjs eval "document.title"
 * node tools/ttv-cdp.cjs shot out.jpeg       # 让 WebView 自己截一张（含真实渲染）
 * ```
 *
 * 端口用 `CDP_PORT` 覆盖，默认 9223。
 */

const fs = require('node:fs');
const http = require('node:http');
const WebSocket = require('ws');

const PORT = Number(process.env.CDP_PORT || 9223);

function listTargets() {
  return new Promise((resolve, reject) => {
    http
      .get(`http://127.0.0.1:${PORT}/json/list`, (res) => {
        let body = '';
        res.on('data', (chunk) => (body += chunk));
        res.on('end', () => {
          try {
            resolve(JSON.parse(body));
          } catch (error) {
            reject(error);
          }
        });
      })
      .on('error', (error) =>
        reject(new Error(`连不上 CDP ${PORT}：${error.message}（实例是否带 --remote-debugging-port 启动？）`)),
      );
  });
}

async function connect() {
  const targets = await listTargets();
  const page = targets.find((t) => t.type === 'page');
  if (!page) throw new Error('CDP 里没有 page 目标');
  const ws = new WebSocket(page.webSocketDebuggerUrl, { origin: `http://127.0.0.1:${PORT}` });
  let seq = 0;
  const pending = new Map();
  ws.on('message', (raw) => {
    let msg;
    try {
      msg = JSON.parse(raw.toString());
    } catch {
      return;
    }
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  });
  await new Promise((resolve, reject) => {
    ws.once('open', resolve);
    ws.once('error', reject);
  });
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, resolve);
      ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => {
        if (pending.has(id)) {
          pending.delete(id);
          reject(new Error(`${method} 超时`));
        }
      }, 25000);
    });
  await send('Runtime.enable', {});
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (r.result && r.result.exceptionDetails) {
      throw new Error('页面内异常: ' + JSON.stringify(r.result.exceptionDetails.exception || {}));
    }
    return r.result && r.result.result ? r.result.result.value : undefined;
  };
  return { ws, send, evaluate, page };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const [cmd = 'list', ...rest] = process.argv.slice(2);
  const { ws, send, evaluate, page } = await connect();
  console.log('页面:', page.url);

  if (cmd === 'list') {
    const out = await evaluate(`
      (() => {
        const buttons = [...document.querySelectorAll('button')]
          .map((b) => (b.textContent || '').trim())
          .filter((t) => t && t.length <= 24);
        return JSON.stringify([...new Set(buttons)].slice(0, 60), null, 1);
      })()
    `);
    console.log(out);
  } else if (cmd === 'click') {
    const label = rest.join(' ');
    const result = await evaluate(`
      (() => {
        const target = ${JSON.stringify(label)};
        const btn = [...document.querySelectorAll('button')]
          .find((b) => (b.textContent || '').trim() === target);
        if (!btn) return 'NOT_FOUND';
        btn.click();
        return 'CLICKED';
      })()
    `);
    console.log('点击', JSON.stringify(label), '→', result);
    await sleep(2500);
    console.log(
      await evaluate(`
        (() => {
          const text = document.body.innerText;
          return JSON.stringify({
            长度: text.length,
            标题: document.title,
            片段: text.split('\\n').filter((l) => l.trim()).slice(0, 12),
          }, null, 1);
        })()
      `),
    );
  } else if (cmd === 'text') {
    const pattern = rest.join(' ');
    const out = await evaluate(`
      (() => {
        const re = new RegExp(${JSON.stringify(pattern)});
        return JSON.stringify(
          document.body.innerText.split('\\n').filter((l) => re.test(l)).slice(0, 20),
          null, 1
        );
      })()
    `);
    console.log(out);
  } else if (cmd === 'eval') {
    console.log(await evaluate(rest.join(' ')));
  } else if (cmd === 'shot') {
    const file = rest[0] || 'ttv-webview.jpg';
    const r = await send('Page.captureScreenshot', { format: 'jpeg', quality: 80 });
    const data = r.result && r.result.data;
    if (!data) throw new Error('截图失败');
    fs.writeFileSync(file, Buffer.from(data, 'base64'));
    console.log('已写', file, fs.statSync(file).size, '字节');
  } else {
    console.log('未知命令:', cmd);
  }

  ws.close();
  process.exit(0);
})().catch((error) => {
  console.error('失败:', error.message);
  process.exit(1);
});