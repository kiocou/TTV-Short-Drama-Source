// 验证 hls.js 的**回环重试退避**没有退化成秒级。
//
// 运行：npm run verify:attach
//
// ## 这份用例守的是哪次故障（2026-11-08）
//
// 用户报告：「每次打开视频都要加载一段时间」。
//
// 实测（src-tauri/.app-data/ttv-playback.log，会话 114）：
//
//   410.747  [vsr] 本地服务：下发清单 会话=114 字节=264   ← header 版，零分片
//   413.749  [vsr] 本地服务：下发清单 会话=114 字节=1045  ← 3 秒后才有内容
//   416.746  首片迟到，又等一轮
//   416.834  整集 HLS 出画 耗时=7227ms
//
// 这 3 秒不是转码慢，而是**播放器在等自己设定的退避定时器**：本地服务对
// "零分片的清单"回 503（正确语义，因为空清单会让 hls.js 判 LEVEL_EMPTY_ERROR），
// 而 `maxRetryDelayMs = 1500` 让第二次重试干等 1.5 秒。回环请求的真实往返
// 是亚毫秒级 —— 秒级退避在这里纯属白等。
//
// ## 为什么这个脚本读源码而不是 import 模块
//
// `hlsAttach.ts` 静态依赖 `./playbackTrace`（无扩展名）并在函数体内
// 动态 import hls.js，Node 的 ESM 解析器既解析不了无扩展名导入、也不该为
// 一份配置校验去加载 595KB 的播放内核。项目的其它 verify 脚本都只 import
// 纯逻辑模块（`playbackStart` / `seekPlan` …），这里是**配置常量**，直接
// 对源码做结构化断言更合适，也更能守住"数字没有写回字面量"这件事。

import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('../src/services/hlsAttach.ts', import.meta.url), 'utf8');

let pass = 0;
let fail = 0;

function check(name, ok, detail) {
  if (ok) pass += 1;
  else fail += 1;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok || !detail ? '' : '  ' + detail));
}

/** 取出 `export const X = <number>;` 的数值。 */
function constNumber(name) {
  const match = source.match(new RegExp(`export const ${name}\\s*=\\s*(\\d+)`));
  return match ? Number(match[1]) : null;
}

const cap = constNumber('LOOPBACK_RETRY_CAP_MS');

// ── 1. 常量本身必须是亚秒级 ──────────────────────────────────────────
check(
  `LOOPBACK_RETRY_CAP_MS 已定义且为数值（当前 ${cap}）`,
  Number.isFinite(cap),
  '常量缺失：四条 policy 的退避上限失去了唯一的来源',
);

check(
  `回环重试上限 < 1000ms（当前 ${cap}ms）`,
  cap !== null && cap < 1000,
  '上限一旦到秒级，"多给几次重试"就变成"多等好几秒"',
);

check(
  `回环重试上限 >= 200ms（当前 ${cap}ms）`,
  cap !== null && cap >= 200,
  '低于 200ms 会让分片/清单请求在并发连接排队时反复空转',
);

// ── 2. 四条 policy 必须都走这个常量 ──────────────────────────────────
//
// `attachSource` 里有 manifest / playlist / frag 三组、共六个 retry 配置。
// 只要有一处把数字写回字面量，改常量就对它无效 —— 那正是"注释写着 300ms、
// 实际还在等 1.5 秒"的复现路径。
const capUsages = source.match(/maxRetryDelayMs: LOOPBACK_RETRY_CAP_MS(\s*\+\s*100)?/g) ?? [];
check(
  `所有 retry 上限都引用常量（当前 ${capUsages.length} 处，期望 6）`,
  capUsages.length === 6,
  '有 retry 配置把上限写回了字面量，改常量不会影响它',
);

const hardcodedCaps = source.match(/maxRetryDelayMs: \d+/g) ?? [];
check(
  '没有残留的硬编码退避上限',
  hardcodedCaps.length === 0,
  `发现硬编码：${hardcodedCaps.join(', ')}`,
);

// ── 3. 总等待窗口仍要覆盖"首片还没落地" ──────────────────────────────
//
// 压掉退避上限之后，**次数**不能跟着缩 —— 那是用来覆盖 ffmpeg 起进程到
// prime 出首片的 1~4 秒窗口的。按最保守的"每次都只等 retryDelayMs"估算。
function retriesOf(key) {
  const match = source.match(new RegExp(`${key}: (\\d+)`));
  return match ? Number(match[1]) : null;
}

function baseDelayOf(maxNumRetry) {
  const match = source.match(
    new RegExp(`maxNumRetry: ${maxNumRetry}, retryDelayMs: (\\d+)`),
  );
  return match ? Number(match[1]) : null;
}

const manifestRetries = retriesOf('manifestLoadingMaxRetry');
const manifestBase = manifestRetries === null ? null : baseDelayOf(manifestRetries);
check(
  `清单重试窗口 >= 4s（次数 ${manifestRetries} × 基准 ${manifestBase}ms）`,
  manifestRetries !== null && manifestBase !== null && manifestRetries * manifestBase >= 4000,
  '窗口短于 4 秒时，ffmpeg 还没 prime 出首片就会被判死',
);

const fragRetries = retriesOf('fragLoadingMaxRetry');
const fragBase = fragRetries === null ? null : baseDelayOf(fragRetries);
check(
  `分片重试窗口 >= 6s（次数 ${fragRetries} × 基准 ${fragBase}ms）`,
  fragRetries !== null && fragBase !== null && fragRetries * fragBase >= 6000,
  '窗口短于 6 秒时，转码稍有抖动就会触发 fatal',
);

// ── 4. 废弃键与 policy 的次数必须一致 ────────────────────────────────
//
// hls.js 的 `mergeConfig` 会把废弃键映射进 `*LoadPolicy.default.errorRetry`，
// 但**退避策略不在映射范围内**。两处一旦分叉，实际生效的次数取决于哪边最后
// 被写入，完全不可预测。
const manifestPolicyRetries = source.match(
  /manifestLoadPolicy:[\s\S]*?errorRetry: \{ maxNumRetry: (\d+)/,
);
check(
  `manifest 废弃键与 policy 次数一致（${manifestRetries} vs ${manifestPolicyRetries?.[1]}）`,
  manifestPolicyRetries !== null && Number(manifestPolicyRetries[1]) === manifestRetries,
  '两处次数分叉会让实际重试次数取决于写入顺序',
);

const fragPolicyRetries = source.match(
  /fragLoadPolicy:[\s\S]*?errorRetry: \{ maxNumRetry: (\d+)/,
);
check(
  `frag 废弃键与 policy 次数一致（${fragRetries} vs ${fragPolicyRetries?.[1]}）`,
  fragPolicyRetries !== null && Number(fragPolicyRetries[1]) === fragRetries,
  '两处次数分叉会让实际重试次数取决于写入顺序',
);

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exit(1);
