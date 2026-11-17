#!/usr/bin/env node
// verify-nordic-theme.mjs —— Crystal HUD 主题切换与单窗口取代旧 WinForms 前端的静态验收。
// 只读源码，不启动播放器、不访问偏好文件、不写任何项目文件。
// 运行：node scripts/verify-nordic-theme.mjs
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
let passed = 0;
const failures = [];

function read(relative) {
  const path = join(root, relative);
  assert.ok(existsSync(path), `缺少验收输入：${relative}`);
  return readFileSync(path, 'utf8');
}

function check(name, condition, detail = '') {
  try {
    assert.ok(condition, detail || '静态契约不成立');
    passed++;
  } catch (error) {
    failures.push(`${name}：${error.message}`);
  }
}

/** 去掉注释与字符串，避免注释中的旧方案误导静态断言。 */
function executable(source) {
  let result = '';
  let state = 'code';
  for (let i = 0; i < source.length; i++) {
    const c = source[i];
    const next = source[i + 1] ?? '';
    if (state === 'line') {
      if (c === '\n') { result += c; state = 'code'; } else result += ' ';
      continue;
    }
    if (state === 'block') {
      if (c === '*' && next === '/') { result += '  '; i++; state = 'code'; }
      else result += c === '\n' ? c : ' ';
      continue;
    }
    if (state === 'string') {
      if (c === '\\') { result += '  '; i++; continue; }
      result += c === '\n' ? c : ' ';
      if (c === '"') state = 'code';
      continue;
    }
    if (state === 'verbatim') {
      if (c === '"' && next === '"') { result += '  '; i++; continue; }
      result += c === '\n' ? c : ' ';
      if (c === '"') state = 'code';
      continue;
    }
    if (state === 'char') {
      if (c === '\\') { result += '  '; i++; continue; }
      result += c === '\n' ? c : ' ';
      if (c === "'") state = 'code';
      continue;
    }
    if (c === '@' && next === '"') { result += '  '; i++; state = 'verbatim'; continue; }
    if (c === '$' && next === '"') { result += '  '; i++; state = 'string'; continue; }
    if (c === '$' && next === '@' && source[i + 2] === '"') { result += '   '; i += 2; state = 'verbatim'; continue; }
    if (c === '@' && next === '$' && source[i + 2] === '"') { result += '   '; i += 2; state = 'verbatim'; continue; }
    if (c === '/' && next === '/') { result += '  '; i++; state = 'line'; continue; }
    if (c === '/' && next === '*') { result += '  '; i++; state = 'block'; continue; }
    if (c === '"') { result += ' '; state = 'string'; continue; }
    if (c === "'") { result += ' '; state = 'char'; continue; }
    result += c;
  }
  return result;
}

/** 抽取花括号体。 */
function braced(source, marker) {
  const start = source.indexOf(marker);
  if (start < 0) return '';
  const open = source.indexOf('{', start);
  if (open < 0) return '';
  let depth = 0;
  for (let i = open; i < source.length; i++) {
    if (source[i] === '{') depth++;
    if (source[i] === '}') {
      depth--;
      if (depth === 0) return source.slice(open + 1, i);
    }
  }
  return '';
}

const program = read('src-tauri/native-player/Program.cs');
const dcomp = read('src-tauri/native-player/DCompHost.cs');
const hudHtml = read('src-tauri/native-player/WebHud/player-hud.html');
const actions = read('src-tauri/native-player/PlayerActions.cs');
const controls = read('src-tauri/native-player/ControlsServer.cs');

// ── 1. 旧 WinForms 主题/控制栏前端彻底退场 ──
for (const legacy of ['PlayerThemes.cs', 'BarTheme.cs', 'PlayerBar.cs', 'TitleBar.cs', 'EpisodeSidebar.cs', 'VideoOverlay.cs', 'BarIcons.cs', 'ChromeSurface.cs']) {
  check(`旧 WinForms 前端已移除：${legacy}`, !existsSync(join(root, 'src-tauri/native-player', legacy)));
}

// ── 2. Crystal HUD（Layout 07）外观、主题与交互契约 ──
check('Crystal HUD 支持影院暗红默认样式与 fluent/nordic 晶璃浅色变量覆盖',
  hudHtml.includes('--fluent-blue: #e02a45;')
  && hudHtml.includes('body[data-style="fluent"], body[data-style="nordic"]'),
  'player-hud.html 缺少默认暗红或 fluent/nordic CSS 变量声明');
check('Crystal HUD 保留透明穿孔宿主能力（DComp 下层播放视频，上层透明 HUD）',
  dcomp.includes('controller.DefaultBackgroundColor = Color.Transparent')
  && hudHtml.includes('syncVideoRect')
  && program.includes('case "videoRect":'),
  '缺少透明底色或 videoRect 视频视口裁剪协议');
check('Crystal HUD 小窗仅适配 16:9 与 9:16 并保留 264px 窄窗布局',
  hudHtml.includes('is-pip-narrow') && !hudHtml.includes('10:16'),
  'player-hud.html 必须包含 is-pip-narrow 且不得再含 10:16 比例分支');
check('Crystal HUD 右侧动作栏、工具弹幕面板、选集抽屉、锁屏与 VSR 齐全',
  ['epSheet', 'dmRequestWindow', 'renderDanmaku', 'EMOTE_MAP', 'data-act="vsr"'].every((k) => hudHtml.includes(k)),
  'player-hud.html 缺少核心交互模块');

// ── 3. Program 主题切换：热切不重启播放/补帧，并安全落盘 ──
const programCode = executable(program);
const applyStyleBody = braced(program, 'internal void ApplyPlayerStyle(string id)');
const applyThemeBody = braced(program, 'void ApplyThemeMode(string? mode)');
check('Program 支持 ResolveInitialStyle 与 fluent/nordic/dark 归一化',
  program.includes('static string ResolveInitialStyle(')
  && applyStyleBody.includes('"nordic"')
  && applyStyleBody.includes('"fluent"')
  && applyStyleBody.includes('prefs.Save()')
  && applyStyleBody.includes('PushHudState()'),
  'ApplyPlayerStyle 应归一外观名、写偏好并推送到 WebHud');
for (const forbidden of ['mpv.LoadFile(', 'InitMpv(', 'SetFg(', 'ApplyLoad(']) {
  check(`Program.ApplyPlayerStyle 不打断播放/补帧 ${forbidden}`,
    !applyStyleBody.includes(forbidden) && !applyThemeBody.includes(forbidden),
    `外观切换意外调用 ${forbidden}`);
}
check('支持 T 快捷键、--style 参数及 HTTP 控制接口切外观',
  program.includes('case Keys.T:')
  && program.includes('case "--style":')
  && controls.includes('case "style":')
  && actions.includes('Style'),
  '外观热切换入口不完整');

// ── 4. 补帧与解码解耦不变量（任何外观模式下恒成立） ──
const initMpvBody = braced(program, 'void InitMpv()');
check('mpv 恒定走零拷贝 audio 主时钟 + auto-safe，且关闭 mpv 内置插值与 CPU 滤镜',
  initMpvBody.includes('mpv.SetOption("video-sync", Program.FgVideoSync(false))')
  && initMpvBody.includes('mpv.SetOption("interpolation", "no")')
  && initMpvBody.includes('mpv.SetOption("vf", "")')
  && programCode.includes('Program.FgTargetFpsFor(srcFps, fgMult, displayHz)'),
  '解码与 GPU 补帧必须彻底解耦');
check('Clean-Room GPU 光流引擎接线保持完整',
  programCode.includes('GpuInterpPipeline')
  && programCode.includes('interp.IngestAndFlow')
  && programCode.includes('interp.Synthesize'),
  'Program.cs 必须使用 GpuInterpPipeline 执行光流计算与亚像素合成');
check('补帧开关非阻塞更新并唤醒渲染线程',
  ['fgEnabled = on', 'fgMult = normMult', 'ResetPace()', 'fboDirty = true', 'wake.Set()']
    .every((needle) => braced(programCode, 'public void SetFg(bool on, int mult)').includes(needle)),
  'SetFg 必须更新倍率/节奏并置 fboDirty/wake.Set');

if (failures.length) {
  console.error(`[verify-nordic-theme] ${passed} 通过，${failures.length} 失败：`);
  failures.forEach((failure, index) => console.error(`${String(index + 1).padStart(2, '0')}. ${failure}`));
  process.exitCode = 1;
} else {
  console.log(`[verify-nordic-theme] 全部 ${passed} 条断言通过。`);
}
