import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = join(import.meta.dirname, '..');
const read = (path: string) => readFileSync(join(root, path), 'utf8');

const progress = read('src/components/player/ProgressBar.tsx');
const hud = read('src/components/player/PlayerHud.tsx');
const drawer = read('src/components/player/EpisodeDrawer.tsx');
const video = read('src/components/player/VideoSurface.tsx');
const anime = read('src/components/player/AnimeVideoSurface.tsx');
const controls = read('src/components/player/PlayerControls.tsx');
const mini = read('src/components/player/MiniPlayer.tsx');
const diagnostics = read('src/components/player/DiagnosticsModal.tsx');
const boost = read('src/components/player/BoostToast.tsx');
const crystal = read('src/styles/crystal.css');

const checks: Array<[string, boolean]> = [
  ['进度条可聚焦', /role="slider"/.test(progress) && /tabIndex=\{duration > 0 \? 0 : -1\}/.test(progress)],
  ['进度条处理方向键和 Home/End', /ArrowLeft/.test(progress) && /ArrowRight/.test(progress) && /Home/.test(progress) && /End/.test(progress)],
  ['进度条键盘事件阻止全局快捷键冒泡', /e\.stopPropagation\(\)/.test(progress)],
  ['清晰度浮层关闭时 inert', /ttv-flyout-fixed[\s\S]*inert=\{!showQualityMenu/.test(hud)],
  ['倍速浮层关闭时 inert', /ttv-flyout-fixed[\s\S]*inert=\{!showSpeedMenu/.test(hud)],
  ['音量浮层关闭时 inert', /crystal-volume-bubble[\s\S]*inert=\{!open\}/.test(hud)],
  ['短剧选集支持 Escape 关闭', /e\.key === 'Escape'[\s\S]*isSideDrawerOpen[\s\S]*toggleSideDrawer\(false\)/.test(video)],
  ['短剧选集关闭时 inert', /ttv-drawer-overlay[\s\S]*inert=\{!isOpen\}/.test(drawer)],
  ['动漫选集关闭时 inert', /showEpisodes[\s\S]*inert=\{!showEpisodes\}/.test(anime)],
  ['选集搜索占位符使用省略号', !/placeholder="[^"]*\.\.\./.test(drawer)],
  ['选集遮罩使用更实的底色', /background:\s*rgba\(0,\s*0,\s*0,\s*0\.72\)/.test(crystal)],
  ['错误态为控制坞预留底部空间', /player-error-overlay/.test(video) && /padding-bottom:\s*112px/.test(crystal)],
  ['多档清晰度显示自动档与当前选择', /sourceQualities\.length > 1[\s\S]*value === 'auto'/.test(hud) && /aria-pressed=\{isActive\}/.test(hud)],
  ['清晰度选项保持统一宽度', /\.crystal-menu-item\s*\{[\s\S]*?width:\s*100%/.test(crystal)],
  ['移除右上角 RTX VSR 常驻徽标', !/title="视频增强已开启：正在播放 RTX VSR 超分链路/.test(controls)],
  ['VSR 开启禁用毛玻璃，关闭时恢复', /ttv-video-stage\.ttv-vsr-on \.crystal-surface/.test(crystal) && /vsrEnabled \? ' ttv-vsr-on'/.test(video)],
  ['短剧与动漫提示卡复用控制台玻璃材质', /crystal-surface/.test(video) && /crystal-surface/.test(anime) && /crystal-surface/.test(boost)],
  ['小窗和诊断卡片复用控制台玻璃材质', /crystal-surface/.test(mini) && /crystal-surface/.test(diagnostics)],
  ['VSR 开启时禁用播放器提示卡背景模糊', /ttv-video-stage\.ttv-vsr-on \.crystal-surface/.test(crystal) && /handoff\?\.vsrEnabled !== false/.test(mini)],
  ['控制按钮具有回弹动效与减弱动效适配', /--curve-spring/.test(crystal) && /\.btn-play-hero:active/.test(crystal) && /prefers-reduced-motion: reduce/.test(crystal)],
];

let failed = 0;
for (const [name, ok] of checks) {
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}`);
  if (!ok) failed += 1;
}

console.log(`\n${checks.length - failed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
