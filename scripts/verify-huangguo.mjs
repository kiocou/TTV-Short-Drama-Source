import fs from 'node:fs';
import path from 'node:path';
import {
  createSevenClickCounter,
  SEVEN_CLICK_THRESHOLD,
  SEVEN_CLICK_WINDOW_MS,
} from '../src/services/sevenClick.ts';

/**
 * 黄果短剧专区验收。
 *
 * 锁三类东西：
 *  1. **入口手势**：七次触发、间隔超时重算、触发后复位。
 *  2. **隔离性**：这个源是隐藏频道 —— 不进导航栏、不进内置源注册表、
 *     不进全局搜索合并。任何一条被破坏，专区就会漏进首页。
 *  3. **动画铁律**：常驻视图层不许碰 opacity；淡入淡出只能在按需挂载的
 *     Portal 里，且必须 cancel()。这条是项目踩过两次的坑
 *     （卡片永久空白、NVIDIA VSR 失效）。
 *
 * 另外锁两条 Rust 侧的契约：`classify_stream` 必须在 `proxied_url` 之前，
 * 以及映射层不许凭空造 playableEpisodes / rating。
 */

const ROOT = path.resolve(import.meta.dirname, '..');
const read = relative => fs.readFileSync(path.join(ROOT, relative), 'utf8');

function walk(dir, out = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full, out);
    else if (/\.(ts|tsx)$/.test(entry.name)) out.push(full);
  }
  return out;
}

const checks = [];
const add = (label, ok) => checks.push([label, ok]);

/* ─────────── 1. 入口手势 ─────────── */

add('默认阈值是 7', SEVEN_CLICK_THRESHOLD === 7);

{
  const counter = createSevenClickCounter();
  let triggered = false;
  for (let index = 0; index < 7; index += 1) {
    triggered = counter.tap(1000 + index * 100);
    if (index < 6) add(`第 ${index + 1} 次不触发`, triggered === false);
  }
  add('第 7 次触发', triggered === true);
  add('触发后计数复位', counter.count() === 0);
}

{
  const counter = createSevenClickCounter();
  for (let index = 0; index < 6; index += 1) counter.tap(1000 + index * 100);
  add('六次后计数是 6', counter.count() === 6);
  // 间隔超过窗口 → 从 1 重新开始，而不是继续累加到 7。
  const fired = counter.tap(1000 + 5 * 100 + SEVEN_CLICK_WINDOW_MS + 1);
  add('间隔超时后不触发', fired === false);
  add('间隔超时后计数回到 1', counter.count() === 1);
}

{
  const counter = createSevenClickCounter();
  counter.tap(5000);
  counter.tap(5100);
  // 系统时钟被改到过去：当作重新开始，不能卡在永不超时的状态。
  counter.tap(100);
  add('时间倒流后计数回到 1', counter.count() === 1);
  counter.reset();
  add('reset 清零', counter.count() === 0);
}

{
  const counter = createSevenClickCounter({ threshold: 3, windowMs: 200 });
  add('自定义阈值第一次不触发', counter.tap(0) === false);
  add('自定义阈值第二次不触发', counter.tap(100) === false);
  add('自定义阈值第三次触发', counter.tap(200) === true);
}

/* ─────────── 2. 隔离性 ─────────── */

const rail = read('src/components/layout/NavigationRail.tsx');
add('导航栏 navItems 里没有 huangguo', !/id:\s*'huangguo'/.test(rail));

const registry = read('src-tauri/src/source_registry.rs');
add('内置源注册表里没有 huangguo', !/huangguo/.test(registry));

const main = read('src-tauri/src/main.rs');
// 只取函数体本身：窗口开太大会扫进下一个函数（catalog_list 里本来就有 huangguo）。
const mergeStart = main.indexOf('async fn merge_search_sources');
const mergeBody = main.slice(mergeStart, mergeStart + 10
  + main.slice(mergeStart + 10).search(/\n(#\[tauri::command\]|async fn |fn |\/\/\/ )/));
add('全局搜索合并里没有 huangguo', mergeStart > 0 && !/huangguo/.test(mergeBody));

const app = read('src/App.tsx');
add('App.tsx 挂了 HuangguoView', app.includes('<HuangguoView />'));
add('App.tsx 用 HuangguoGateProvider 包住内容', app.includes('<HuangguoGateProvider>'));

const explore = read('src/components/views/ExploreView.tsx');
add('发现页没有引用 huangguo（频道状态不外泄）', !/huangguo/.test(explore));

const view = read('src/components/views/HuangguoView.tsx');
add('专区自己持有频道状态，不写全局 channel', !/setChannel\(\s*'huangguo'/.test(view) && view.includes('useState'));
add('专区取数以 currentView 为闸门', view.includes("currentView === 'huangguo'"));
add('专区不传 onUnavailable（封面失败保留占位）', !/onUnavailable=/.test(view));

const ipc = read('src/services/ipc.ts');
add('coverResolver 有 hg: 分支', /seriesId\.startsWith\('hg:'\)/.test(ipc));
add('封面命令已接入', ipc.includes("'huangguo_cover'") && ipc.includes("'huangguo_cover_batch'"));

const titleBar = read('src/components/layout/TitleBar.tsx');
add('标题栏接了七击计数器', titleBar.includes('createSevenClickCounter'));
add('品牌区仍然带 data-window-interactive（否则点击被拖动吞掉）',
  /data-window-interactive>\s*\n\s*<div\s*\n\s*onClick=\{handleBrandClick\}/.test(titleBar));

/* ─────────── 3. 动画铁律 ─────────── */

const wrapper = app.match(/currentView === 'huangguo'[^\n]*/)?.[0] ?? '';
add('专区视图层用纯 transform 入场', wrapper.includes('animate-fluent-page-in'));
add('专区视图层没有 opacity 入场类', !/animate-fade|animate-fluent-scale-in|opacity-0/.test(wrapper));

const gate = read('src/components/huangguo/HuangguoGate.tsx');
add('遮罩走 Portal', gate.includes('createPortal'));
add('遮罩用 element.animate', gate.includes('.animate('));
add('遮罩结束时 cancel()（不留 forwards 残留）', gate.includes('animation.cancel()'));
add('遮罩卸载时清定时器', gate.includes('clearTimeout'));
add('尊重 prefers-reduced-motion', gate.includes('prefers-reduced-motion'));
add('视图切换点在遮罩不透光的区间内', /SWITCH_AT = 0\.55/.test(gate) && gate.includes('offset: 0.45') && gate.includes('offset: 0.62'));

// 常驻视图里不许出现带 opacity 的自定义关键帧。
const huangguoFiles = [
  'src/components/views/HuangguoView.tsx',
  'src/components/huangguo/HuangguoGate.tsx',
];
const opacityKeyframeOffenders = huangguoFiles.filter(file => /@keyframes[^}]*opacity/.test(read(file)));
add('新增文件里没有 CSS opacity 关键帧', opacityKeyframeOffenders.length === 0);

/* ─────────── 4. Rust 契约 ─────────── */

const provider = read('src-tauri/src/huangguo_provider.rs');
const classifyAt = provider.indexOf('classify_stream(remote)');
const proxyAt = provider.indexOf('proxied_url(remote)');
add('classify_stream 在 proxied_url 之前', classifyAt > 0 && proxyAt > classifyAt);
add('preview_only 如实给 false（实测整集）', /preview_only: false/.test(provider));
add('不加入 VSR 增强链路', !/enhance_short_drama_session/.test(provider));

const mapping = read('src-tauri/src/huangguo/mapping.rs');
add('playableEpisodes 不凭空造', /playable_episodes: None/.test(mapping));
add('sources / availableQualities 恒空', /available_qualities: Vec::new\(\)/.test(mapping) && /sources: Vec::new\(\)/.test(mapping));
add('rating 缺失给 None', /\(score\.is_finite\(\) && score > 0\.0\)\.then_some\(score\)/.test(mapping));
add('id 前缀是 hg:', /ID_PREFIX: &str = "hg:"/.test(mapping));
add('四个频道全部接入', ['ai-duanju', 'ai-manju', 'ai-huanlian', 'ai-mogai'].every(slug => mapping.includes(slug)));

const media = read('src-tauri/src/huangguo/media.rs');
add('封面缓存键去掉时效签名', /url\.split\('\?'\)/.test(media));
add('AES 有 NIST 向量锁定', media.includes('aes128_matches_the_nist_known_answer'));
add('AES 有真实封面向量锁定', media.includes('real_cover_first_block_decrypts_to_a_jpeg_header'));

// 净室重写：不得引入上游那套自定义协议标识。
const upstreamMarkers = ['hg.localhost', 'LOCAL_ORIGIN', 'huangguo-client'];
const leaked = ['src-tauri/src/huangguo_provider.rs', 'src-tauri/src/huangguo/client.rs', 'src-tauri/src/huangguo/media.rs', 'src-tauri/src/huangguo/mapping.rs']
  .filter(file => upstreamMarkers.some(marker => read(file).includes(marker)));
add(`没有上游实现标识泄漏（${leaked.length ? leaked.join(', ') : '无'}）`, leaked.length === 0);

/* ─────────── 5. 封面偶发空白的回归锁（2026-11-11） ─────────── */

// ① 密文封面不许直连：每个 coverResolver 调用点都必须一并传 rawUsable。
//    散着写必漏，所以这里逐个调用点核对，而不是只看有没有出现过这个词。
const coverCallSites = [];
for (const file of walk(path.join(ROOT, 'src'))) {
  const text = fs.readFileSync(file, 'utf8');
  const pattern = /resolveSrc=\{coverResolver\(([A-Za-z0-9_.]+),[^)]*\)\}/g;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    const tail = text.slice(match.index + match[0].length, match.index + match[0].length + 220);
    coverCallSites.push({
      file: path.relative(ROOT, file),
      id: match[1],
      ok: new RegExp(`rawUsable=\\{coverRawUsable\\(${match[1].replace(/\./g, '\\.')}\\)\\}`).test(tail),
    });
  }
}
const missingRawUsable = coverCallSites.filter(site => !site.ok);
add(`每个 coverResolver 调用点都带 rawUsable（共 ${coverCallSites.length} 处，缺 ${missingRawUsable.length} 处${missingRawUsable.length ? '：' + missingRawUsable.map(s => s.file + '/' + s.id).join(', ') : ''}）`,
  coverCallSites.length > 0 && missingRawUsable.length === 0);

// ipc.ts 有浏览器侧副作用（localStorage / 动态 import Tauri），不在 Node 里直接 import，
// 只核对这个纯函数的实现本身。
add('coverRawUsable 以 hg: 前缀为唯一判据',
  /export function coverRawUsable\(seriesId: string\): boolean \{\s*return !seriesId\.startsWith\('hg:'\);/.test(ipc));

const coverImage = read('src/components/common/CoverImage.tsx');
add('rawUsable=false 时永不回退原地址', /!rawUsable \? ''/.test(coverImage));
add('rawUsable 默认 true（不改其它源行为）', /rawUsable = true/.test(coverImage));

// ② 代理不许在启动期烤死。
const hgClient = read('src-tauri/src/huangguo/client.rs');
add('主客户端只挂系统代理，启动零探测',
  /pub fn build_client[\s\S]{0,220}with_system_proxy/.test(hgClient)
  && !/pub fn build_client[\s\S]{0,220}with_local_proxy_fallback/.test(hgClient));
add('兜底客户端探不到代理时返回 None（不缓存失败）',
  /pub fn fallback_client\(\) -> Option<reqwest::Client>[\s\S]{0,200}detect_local_http_proxy\(\)\?/.test(hgClient));
add('provider 惰性持有兜底客户端', /fallback: tokio::sync::OnceCell<reqwest::Client>/.test(provider));
add('业务请求失败后走兜底重试', /async fn edge\([\s\S]{0,700}fallback_client\(\)\.await/.test(provider));
add('批量封面整批失败后才探一次代理', /let missing: Vec<usize>/.test(provider));
add('封面槽位按入参长度预开（返回值不会变短）', /vec!\[None; total\]/.test(provider));

const update = read('src-tauri/src/update.rs');
add('代理探测失败可重探（不再永久缓存 None）',
  /RETRY_AFTER/.test(update) && /at\.elapsed\(\) < RETRY_AFTER/.test(update));
add('代理探测成功仍永久缓存', /found\.is_some\(\) \|\|/.test(update));

add('HLS 转发也走代理兜底（系统代理关掉时仍能播）',
  /with_local_proxy_fallback\(reqwest::Client::builder\(\)\)/.test(read('src-tauri/src/hls_proxy.rs')));

// ③ 失败必须留下可归因的日志。
add('封面失败落日志且不写时效凭据',
  /\[黄果\] 封面失败/.test(media) && /let stable = url\.split\('\?'\)/.test(media));
add('缓存淘汰已节流（不再每写一张扫全目录）', /fn prune[\s\S]{0,400}RETRY|fn prune[\s\S]{0,400}LAST/.test(media));

/* ─────────── 汇总 ─────────── */

let failed = 0;
for (const [label, ok] of checks) {
  if (ok) console.log(`PASS ${label}`);
  else {
    failed += 1;
    console.error(`FAIL ${label}`);
  }
}
console.log(`\n${checks.length - failed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);