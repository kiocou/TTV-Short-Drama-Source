// 「短剧推荐」页信息层、状态区与底部控制条的确定性验收。
//
// 为什么单独跑：这里锁的三类东西都是**结构契约**——三级文案怎么拼、三个状态谁的
// 优先级高、底部条到底有哪几颗按钮。全写进 JSX 就只能开窗口、切网络、等七秒解析
// 再肉眼比对，而且是概率性复现；抽成纯函数 + 一次服务端渲染就能确定性地钉死。
//
// 三个必须绕开的技术坑：
// 1. 组件是 `.tsx`，Node 的 `--experimental-strip-types` 只认 `.ts`，遇到 JSX
//    直接 ERR_UNKNOWN_FILE_EXTENSION。所以先 esbuild 打成 ESM 再 import。
// 2. 打包产物必须落在**项目内**（`node_modules/.ttv-verify-feed-view/`）：
//    bundle 里 `react` / `react/jsx-runtime` / `lucide-react` 是 external，
//    Node 要沿父目录链找 node_modules，放到系统临时目录会 ERR_MODULE_NOT_FOUND。
// 3. 组件若直接 import `stores/usePlaybackStore`，bundle 会去打包那份 6000 行的
//    真 store（连带 Tauri IPC）。所以用 esbuild 插件把那两条 hook 换成桩：
//    「有没有偷偷订阅整表」这件事因此会以**渲染失败**的形式暴露，而不是靠人读 diff。
//
// 运行：node --experimental-strip-types scripts/verify-feed-view.mjs
//      （package.json 里对应 `npm run verify:feed-view`，由 Lead 统一注册）

import { build } from 'esbuild';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

const outDir = path.resolve('node_modules', '.ttv-verify-feed-view');
await rm(outDir, { recursive: true, force: true });
await mkdir(outDir, { recursive: true });

const EXTERNAL = ['react', 'react/jsx-runtime', 'react/jsx-dev-runtime', 'react-dom', 'react-dom/server', 'lucide-react'];

await build({
  entryPoints: [
    path.resolve('src/components/player/FeedSlide.tsx'),
    path.resolve('src/components/player/FeedStatusBanner.tsx'),
  ],
  outdir: outDir,
  bundle: true,
  format: 'esm',
  platform: 'node',
  jsx: 'automatic',
  // external 而不是打进来：react 被打进 bundle 会破坏 hooks 的单一实例，
  // lucide-react 打进来只是白白变慢。esbuild 这个版本只接受字符串，不收正则。
  external: EXTERNAL,
  outExtension: { '.js': '.mjs' },
  logLevel: 'warning',
});

const {
  FeedSlide,
  feedEpisodeLine,
  feedEpisodeTitleFragment,
  playableBoundaryText,
  FEED_EPISODE_TITLE_MAX_CHARS,
} = await import(pathToFileURL(path.join(outDir, 'FeedSlide.mjs')).href);
const { FeedStatusBanner, deriveFeedPhase, feedStatusText, feedStatusHint, feedStatusTone } = await import(
  pathToFileURL(path.join(outDir, 'FeedStatusBanner.mjs')).href
);

const slideHtml = (props) => renderToStaticMarkup(createElement(FeedSlide, props));
const bannerHtml = (props) => renderToStaticMarkup(createElement(FeedStatusBanner, props));

const series = (overrides = {}) => ({
  id: 'series-1',
  title: '闪婚后夫人又美又飒',
  cover: 'https://example.invalid/cover.jpg',
  type: 'drama',
  episodesCount: 102,
  tags: ['先婚后爱', '爽剧'],
  origin: '红果推荐',
  brief: '简介占位。',
  ...overrides,
});

const LONG_BRIEF =
  '女主被家族逼着嫁给传闻中暴戾残废的陌生人，婚礼当天才发现新郎是本市最年轻的集团掌权人。'
  + '一场各取所需的契约婚姻，在日复一日的试探与反试探里慢慢变了味道。';

const checks = [];
const check = (label, ok, detail) => checks.push({ label, ok: Boolean(ok), detail });
const checkEqual = (label, actual, expected) => check(
  label,
  Object.is(actual, expected),
  Object.is(actual, expected) ? undefined : `实际=${JSON.stringify(actual)} 期望=${JSON.stringify(expected)}`,
);

// ---------------------------------------------------------------- 三级信息层
//
// 2026-11-10 第三次改版后，信息层是「剧名 / 二级行 / 一行简介入口」三条。
// 断言跟着改成新结构：简介正文**默认一条都不出现**，这是用户说的
// 「左下角文字区域太多了」的正面回应。

const fullHtml = slideHtml({
  series: series({ brief: LONG_BRIEF }),
  episodeNumber: 3,
  episodeTitle: '第 3 集 偶遇',
  playableEpisodes: 5,
});

check('一级：剧名渲染成 h2（页面 h1 归「短剧推荐」，剧名不能顶级）', fullHtml.includes('<h2'));
check('一级：本组件内不出现 h1', !fullHtml.includes('<h1'));
check('一级：剧名文案完整', fullHtml.includes('闪婚后夫人又美又飒'));
check('一级：剧名是粗体大字', /<h2[^>]*class="[^"]*text-\[19px\][^"]*font-bold/.test(fullHtml));

check('二级：含「第 3 集」', fullHtml.includes('第 3 集'));
check('二级：含总集数「共 102 集」', fullHtml.includes('共 102 集'));
check('二级：集标题前缀被剥离，不出现「第 3 集 第 3 集」', !fullHtml.includes('第 3 集 第 3 集'));
check('二级：可用边界时给出「开放前 5 集」', fullHtml.includes('开放前 5 集'));
check('二级：字号压到 text-xs，不与剧名同级', /<p[^>]*class="mt-1 text-xs[^"]*"/.test(fullHtml));

check('三级：简介正文默认完全隐藏（画面里不含任何长文本）', !fullHtml.includes(LONG_BRIEF));
check('三级：只留一行「简介」入口', fullHtml.includes('data-feed-brief-entry'));
check('三级：入口默认收起（aria-expanded="false"）', fullHtml.includes('aria-expanded="false"'));
check('三级：入口收起时不渲染浮层容器', !fullHtml.includes('简介全文'));
check('三级：入口可点击（pointer-events-auto，不被父层 pointer-events-none 吃掉）',
  /<button[^>]*class="[^"]*pointer-events-auto/.test(fullHtml));
check('三级：入口文案就是「简介」，不再是「展开简介」', fullHtml.includes('简介') && !fullHtml.includes('展开简介'));

// 「有简介就给入口」而不是「够长才给入口」：正文已经不在画面里了，
// 长度阈值失去意义；反而短简介若不给入口就彻底看不到。
const shortHtml = slideHtml({
  series: series({ brief: '一句话简介。' }),
  episodeNumber: 1,
  episodeTitle: '第 1 集',
  playableEpisodes: undefined,
});
check('三级：短简介同样给入口（正文默认隐藏，入口是唯一读法）',
  shortHtml.includes('data-feed-brief-entry') && !shortHtml.includes('一句话简介。'));
check('三级：入口的 aria-controls 只在展开时绑定（收起态无悬空引用）',
  !fullHtml.includes('aria-controls='));

const noBriefHtml = slideHtml({
  series: series({ brief: '   ' }),
  episodeNumber: 1,
  episodeTitle: '第 1 集',
});
check('三级：没有简介时不给空入口', !noBriefHtml.includes('data-feed-brief-entry'));

check('信息层：无外框纯投影（Douyin 风格，不含 bg-black/35 容器）',
  /class="text-white \[text-shadow:[^\]]+\]"/.test(fullHtml) && !fullHtml.includes('bg-black/35'));
check('信息层：宽度上限 max-w-[min(28rem,56vw)]（不横跨半屏）',
  /class="[^"]*max-w-\[min\(28rem,56vw\)\]/.test(fullHtml));
check('信息层：标签已撤出（它在「详细」面板里有一份，这里再列只会撑宽）',
  !fullHtml.includes('先婚后爱') && !fullHtml.includes('爽剧'));

checkEqual('空态：series 为 null 时整块不渲染', slideHtml({ series: null, episodeNumber: 1, episodeTitle: '' }), '');

// -------------------------------------------- 二级行不承载叙事文本（防重复）
//
// 用户截图里「同一段简介出现两遍」的真因：数据源把整段简介放进了
// `episodeTitle`，旧版 `feedEpisodeLine` 原样拼进二级行，紧接着 `description`
// 又渲染一次。这一组断言把「二级行只陈述结构化事实」钉死。

checkEqual('二级行：像标题的集标题并入',
  feedEpisodeLine({ episodeNumber: 3, episodeTitle: '第 3 集 偶遇', episodesCount: 102, playableEpisodes: 5 }),
  '第 3 集 · 偶遇 · 共 102 集 · 开放前 5 集');
checkEqual('二级行：episodeTitle 自带前缀时不重复',
  feedEpisodeLine({ episodeNumber: 3, episodeTitle: '第 3 集', episodesCount: 102 }),
  '第 3 集 · 共 102 集');
checkEqual('二级行：集数未知时只留集号',
  feedEpisodeLine({ episodeNumber: 2, episodeTitle: '第 2 集', episodesCount: 0 }),
  '第 2 集');

const narrativeLine = feedEpisodeLine({ episodeNumber: 2, episodeTitle: LONG_BRIEF, episodesCount: 87 });
checkEqual('二级行：整段简介当集标题时被丢弃（截图里那两遍的第一遍）',
  narrativeLine, '第 2 集 · 共 87 集');
check('二级行：长文本场景下整行不超过 60 字符（不会再撑成一段话）',
  narrativeLine.length <= 60, `实际 ${narrativeLine.length} 字符`);
check('二级行：不含原文连续 30 字片段（与简介正文不可能再"看起来重复"）',
  !narrativeLine.includes(LONG_BRIEF.slice(0, 30)));

checkEqual('集标题片段：正常集标题保留', feedEpisodeTitleFragment('第 3 集 偶遇'), '偶遇');
checkEqual('集标题片段：整段简介返回 null', feedEpisodeTitleFragment(LONG_BRIEF), null);
checkEqual('集标题片段：恰好到上限长度仍保留',
  feedEpisodeTitleFragment('一二三四五六七八九十一二'), '一二三四五六七八九十一二');
checkEqual('集标题片段：超出上限整段丢弃（不截断成半截话）',
  feedEpisodeTitleFragment('一二三四五六七八九十一二三'), null);
checkEqual('集标题片段：短但带句读标点的叙事文本也丢弃',
  feedEpisodeTitleFragment('她，回来了'), null);
checkEqual('集标题片段：空标题返回 null', feedEpisodeTitleFragment('   '), null);
checkEqual('集标题片段：上限就是 12 字', FEED_EPISODE_TITLE_MAX_CHARS, 12);

// ------------------------------------------------------------ 可播边界不猜

const noBoundaryHtml = slideHtml({
  series: series(),
  episodeNumber: 3,
  episodeTitle: '第 3 集',
  playableEpisodes: undefined,
});
check('边界：playableEpisodes 缺失时**不显示**「开放前」', !noBoundaryHtml.includes('开放前'));
checkEqual('边界：缺失返回 null（不拿缺席值猜）',
  playableBoundaryText({ episodesCount: 102, playableEpisodes: undefined }), null);
checkEqual('边界：显式 null 视同缺失', playableBoundaryText({ episodesCount: 102, playableEpisodes: null }), null);
checkEqual('边界：NaN / Infinity 不产生文案',
  playableBoundaryText({ episodesCount: 102, playableEpisodes: NaN }), null);

const allOpenHtml = slideHtml({
  series: series({ episodesCount: 12 }),
  episodeNumber: 12,
  episodeTitle: '第 12 集',
  playableEpisodes: 12,
});
check('边界：边界等于总集数时视为全开放，不提示「开放前」', !allOpenHtml.includes('开放前'));
checkEqual('边界：边界大于总集数时同样不提示',
  playableBoundaryText({ episodesCount: 12, playableEpisodes: 99 }), null);
checkEqual('边界：边界小于总集数才提示',
  playableBoundaryText({ episodesCount: 102, playableEpisodes: 5 }), '开放前 5 集');
checkEqual('边界：总集数为 0（无参照）时不提示',
  playableBoundaryText({ episodesCount: 0, playableEpisodes: 5 }), null);

// -------------------------------------------------------------- 三种状态文案

checkEqual('状态文案：opening = 正在解析整集…', feedStatusText('opening'), '正在解析整集…');
checkEqual('状态文案：buffering = 缓冲中…', feedStatusText('buffering'), '缓冲中…');
checkEqual('状态文案：loading = 正在加载推荐…', feedStatusText('loading'), '正在加载推荐…');
checkEqual('状态文案：completed = 当前短剧已播放完成', feedStatusText('completed'), '当前短剧已播放完成');
checkEqual('状态文案：error 有兜底句（真实原因由 message 传入）', feedStatusText('error'), '出错了');
check('状态文案：全部用全角省略号，不用三个半角点',
  !['opening', 'buffering', 'loading', 'completed', 'error']
    .some((phase) => feedStatusText(phase).includes('...')));
check('状态提示：opening 解释「为什么慢」（源站往返）',
  (feedStatusHint('opening') || '').includes('播放地址'));
check('状态提示：buffering 与 opening 说明不同的事',
  feedStatusHint('opening') !== feedStatusHint('buffering'));

const openingHtml = bannerHtml({ phase: 'opening' });
check('状态渲染：opening 显示解析文案', openingHtml.includes('正在解析整集…'));
check('状态渲染：opening 带 spinner', openingHtml.includes('animate-spin'));
check('状态渲染：opening 是 role="status" + aria-live="polite"',
  openingHtml.includes('role="status"') && openingHtml.includes('aria-live="polite"'));
check('状态渲染：状态区带 data 标记便于定位（data-feed-status）',
  openingHtml.includes('data-feed-status="opening"'));

const bufferingHtml = bannerHtml({ phase: 'buffering', progressText: '已播 1:12' });
check('状态渲染：buffering 显示「缓冲中…」', bufferingHtml.includes('缓冲中…'));
check('状态渲染：buffering 能带已播进度', bufferingHtml.includes('已播 1:12'));

// 真实用法：`FeedView` 恒传 `onRetry`（`feed.retry`）与 `onSwitchSeries`；
// `canSwitchSeries` 才决定「换一部」出不出现。这里两个用例分开摆，
// 因为「没有下一部」与「没有重试」是两件事——后者绝不该发生。
const errorHtml = bannerHtml({
  phase: 'error',
  message: '这一集暂时没有可用的播放地址。',
  canSwitchSeries: false,
  onSwitchSeries: () => {},
  onRetry: () => {},
});
check('状态渲染：error 显示真实原因而不是兜底句', errorHtml.includes('这一集暂时没有可用的播放地址。'));
check('状态渲染：error 用 alert 打断朗读', errorHtml.includes('role="alert"') && errorHtml.includes('aria-live="assertive"'));
check('状态渲染：error 给「重试」出口', errorHtml.includes('重试'));
check('状态渲染：无更多推荐时不显示「换一部」', !errorHtml.includes('换一部'));
check('状态渲染：error 走 rose 语义色', feedStatusTone('error').includes('rose'));

const errorSwitchHtml = bannerHtml({
  phase: 'error',
  message: '推荐内容加载失败。',
  canSwitchSeries: true,
  onSwitchSeries: () => {},
});
check('状态渲染：有下一部时给「换一部」出口', errorSwitchHtml.includes('换一部'));

const completedHtml = bannerHtml({ phase: 'completed', onRetry: () => {} });
check('状态渲染：completed 显示完成文案', completedHtml.includes('当前短剧已播放完成'));
check('状态渲染：completed 的重试按钮叫「重播」', completedHtml.includes('重播'));
check('状态渲染：completed 走 emerald 语义色（肯定态，不是错误）',
  feedStatusTone('completed').includes('emerald'));
check('状态渲染：加载中的三个阶段不染警告色',
  ['loading', 'opening', 'buffering'].every((phase) => !/rose|emerald/.test(feedStatusTone(phase))));

// -------------------------------------------------------------- 状态优先级

const base = { error: null, isCompleted: false, isOpening: false, isLoading: false, hasDetail: false, isBuffering: false };
checkEqual('优先级：error 盖住一切', deriveFeedPhase({ ...base, error: '断链', isCompleted: true, isOpening: true, isLoading: true, hasDetail: true, isBuffering: true }), 'error');
checkEqual('优先级：completed 需要 detail 才算终态', deriveFeedPhase({ ...base, isCompleted: true, hasDetail: true }), 'completed');
checkEqual('优先级：没有 detail 的 completed 只是中间态，不喊「已播放完成」', deriveFeedPhase({ ...base, isCompleted: true }), null);
checkEqual('优先级：opening 压过 loading（解析整集比「加载中」更具体）', deriveFeedPhase({ ...base, isOpening: true, isLoading: true, hasDetail: true }), 'opening');
checkEqual('优先级：只有 loading 时报 loading', deriveFeedPhase({ ...base, isLoading: true }), 'loading');
checkEqual('优先级：buffering 单列时才报缓冲', deriveFeedPhase({ ...base, isBuffering: true, hasDetail: true }), 'buffering');
checkEqual('优先级：opening 与 buffering 并存时先报解析', deriveFeedPhase({ ...base, isOpening: true, isBuffering: true, hasDetail: true }), 'opening');
checkEqual('优先级：全静默时不渲染状态区（正常播放别挂常驻提示）', deriveFeedPhase({ ...base, hasDetail: true }), null);

// ------------------------------------------------- 源码级契约（防这次改造回退）

/**
 * 注释剥离。
 *
 * 这一组契约检查的是**代码结构**，而注释里到处在说反话：「原生音放图标与 mpv
 * 徽标不再出现」「旧版是 bottom-32」「拖时才不调 seek()」——不剥注释的话，
 * 一句解释就把自己的断言判红了。剥掉注释只留代码，断言才在检查"现在真这么写"。
 */
const stripComments = (source) => source
  .replace(/\/\*[\s\S]*?\*\//g, '')
  .replace(/^\s*\/\/.*$/gm, '');

/** 读源码并剥注释：下面所有「源码级契约」都在这个视图上跑。 */
const readCode = async (relative) => stripComments(await readFile(path.resolve(relative), 'utf8'));

const feedViewSource = await readCode('src/components/views/FeedView.tsx');
const feedSlideSource = await readCode('src/components/player/FeedSlide.tsx');

check('源码：FeedView 的整屏错误遮罩已删除（bg-black/25 不再出现）', !feedViewSource.includes('bg-black/25'));
check('源码：FeedView 只挂一个状态区', (feedViewSource.match(/<FeedStatusBanner/g) || []).length === 1);
check('源码：FeedView 用 deriveFeedPhase 推导状态', feedViewSource.includes('deriveFeedPhase'));
/**
 * 2026-11-10 第二次改写：`isBuffering` 的订阅从 `FeedView` **下沉**到
 * `FeedStageOverlay`；第三次改版的 `FeedPlaybackBar` 同样把 `position`/`duration`
 * 关在自己里面。这组断言锁的是「主体一处高频订阅都没有」。
 */
// 按位置切分，不能数全文件：`FeedStageOverlay` 与 `FeedCompletionBridge` 都住在
// 同一个文件里，它们的订阅不该算到主体头上。
const feedMainBody = feedViewSource.slice(feedViewSource.indexOf('export const FeedView'));
check('源码：FeedView 主体内没有任何 usePlaybackStore 订阅（高频信号已下沉）',
  !feedMainBody.includes('usePlaybackStore()'));
check('源码：存在 FeedStageOverlay 这一层',
  feedViewSource.includes('const FeedStageOverlay: React.FC<'));
check('源码：FeedStageOverlay 里订阅 isBuffering',
  /const \{ isBuffering \} = usePlaybackStore\(\);/.test(feedViewSource));
check('源码：isBuffering 真的进了 deriveFeedPhase（不是解构了没用）',
  /isBuffering,\s*\n\s*\}\);/.test(feedViewSource));
check('源码：播放器推荐页只展示视频不铺封面占位',
  !feedViewSource.includes('const showCover ='));
check('源码：FeedView 用组件方式挂载 overlay（不是把两块 UI 又写回主体）',
  /<FeedStageOverlay[\s\S]*?feed=\{feed\}[\s\S]*?canSwitchSeries=\{canSwitchSeries\}/.test(feedViewSource));
check('源码：FeedView 已移除瀑布流与视频流/瀑布流切换器',
  !feedViewSource.includes('FeedWaterfall')
  && !feedViewSource.includes('viewMode')
  && !feedViewSource.includes('视频流')
  && !feedViewSource.includes('瀑布流'));
check('源码：FeedView 的节流交给 feedNavigation（阈值不再散落在组件里）',
  feedViewSource.includes('feedNavAllowed('));
check('源码：FeedSlide 的可播边界来自 feed.detail.playableEpisodes',
  feedViewSource.includes('playableEpisodes={feed.detail?.playableEpisodes}'));
check('源码：FeedSlide 以剧 id 为 key（换部时把简介浮层收起）',
  /key=\{displaySeries\?\.id \?\? /.test(feedViewSource));
check('源码：busy 不含 isLoadingMore（后台补页不该冻结翻集）',
  feedViewSource.includes('const busy = feed.isLoading || feed.isOpening;'));
check('源码：底部条的「下一集」可用性走 feedEpisodeStep（末集/可播边界如实禁用）',
  /const canGoNext = !busy && feedEpisodeStep\(/.test(feedViewSource),
  '自己推 episodeIndex + 1 < n 会在末集把按钮灰掉/亮着但点了没反应');

// ------------------------------------------------- 右上角只留频道切换（新）
//
// 用户：「右上角那个位置，还有按钮全部放在一起太乱、太杂了」。
// 截图里那一行是三组东西（频道 tab + 原生音放 + mpv 徽标）。原生那两样来自
// `PlayerControls`，已在 feed 视图下整块不渲染；这里锁住「本页不再补任何
// 播放器状态文字」，以及「托盘只有一组」。

check('右上：只有一个频道托盘（data-feed-channel-tray 出现一次）',
  (feedViewSource.match(/data-feed-channel-tray/g) || []).length === 1,
  `实际 ${(feedViewSource.match(/data-feed-channel-tray/g) || []).length} 处`);
check('右上：托盘是唯一一处 right-4 top-4 定位（没有第二组东西挤在同一行）',
  (feedViewSource.match(/absolute right-4 top-4/g) || []).length === 1,
  `实际 ${(feedViewSource.match(/absolute right-4 top-4/g) || []).length} 处`);
check('右上：三颗频道按钮各一次（短剧 / 漫剧 / AI 剧）',
  ["selectChannel('drama')", "selectChannel('comic')", "selectChannel('ai')"]
    .every((call) => (feedViewSource.match(new RegExp(call.replace(/[()']/g, '\\$&'), 'g')) || []).length === 1));
check('右上：托盘样式 bg-black/40 + border-white/20 + rounded-xl',
  /data-feed-channel-tray[\s\S]{0,200}/.test(feedViewSource)
  && feedViewSource.includes('rounded-xl border border-white/20 bg-black/40'));
check('右上：不放任何播放器状态文字（原生 / mpv / 联播 / native 一律不出现）',
  !/原生|mpv|联播|native/i.test(feedViewSource));
check('右上：本页不渲染 PlayerControls（控制层只有自绘那一套）',
  !feedViewSource.includes('PlayerControls'));

// ---------------------------------- 底部条：自绘一套，且只有刷着看会点的五样
//
// 用第二个桩构建：这个桩的 `usePlaybackStore` 返回固定 position/duration，
// 于是「底部条内部订阅高频读数」这件事本身也被渲染验证覆盖到。

const barStubFile = path.join(outDir, 'playback-bar-stub.ts');
await writeFile(barStubFile, [
  'export function usePlaybackActions() {',
  '  return { isPlaying: false, togglePlay: () => {}, seek: () => {}, isMuted: false, toggleMute: () => {} };',
  '}',
  'export function usePlaybackStore() {',
  '  return { position: 12.4, duration: 300 };',
  '}',
  '',
].join('\n'), 'utf8');

const barOut = path.join(outDir, 'FeedPlaybackBar.mjs');
await build({
  entryPoints: [path.resolve('src/components/player/FeedPlaybackBar.tsx')],
  outfile: barOut,
  bundle: true,
  format: 'esm',
  platform: 'node',
  jsx: 'automatic',
  external: EXTERNAL,
  plugins: [{
    name: 'playback-bar-stub',
    setup(builder) {
      builder.onResolve({ filter: /usePlaybackStore$/ }, () => ({ path: barStubFile }));
    },
  }],
  logLevel: 'warning',
});

const {
  FeedPlaybackBar,
  feedBarRatio,
  feedBarTargetSeconds,
  feedBarKeyboardTarget,
  FEED_BAR_SKIP_SECONDS,
} = await import(pathToFileURL(barOut).href);

const barHtml = renderToStaticMarkup(createElement(FeedPlaybackBar, {
  canGoNext: true,
  onNextEpisode: () => {},
  isFullscreen: false,
  onToggleFullscreen: () => {},
}));

const barSource = await readCode('src/components/player/FeedPlaybackBar.tsx');
const barButtonCount = (barHtml.match(/<button/g) || []).length;
check('底部条渲染：恰好四颗图标按钮（播放 / 下一集 / 静音 / 全屏）', barButtonCount === 4, `实际 ${barButtonCount} 颗`);
check('底部条渲染：播放按钮在场（初始未播放 → aria-label=播放）', barHtml.includes('aria-label="播放"'));
check('底部条渲染：下一集按钮在场', barHtml.includes('aria-label="下一集"'));
check('底部条渲染：全屏按钮在场', barHtml.includes('aria-label="全屏"'));
check('底部条渲染：容器是 toolbar 语义', barHtml.includes('role="toolbar"') && barHtml.includes('aria-label="播放控制"'));
check('底部条渲染：进度是 role="slider"（键盘可达）', barHtml.includes('role="slider"'));
check('底部条渲染：slider 暴露 aria-valuemin/max/now',
  barHtml.includes('aria-valuemin="0"') && barHtml.includes('aria-valuemax="300"') && barHtml.includes('aria-valuenow="12"'),
  barHtml.match(/aria-valuenow="[^"]*"/)?.[0]);
check('底部条渲染：slider 的 valuetext 就是时间码（当前 / 总长）',
  barHtml.includes('aria-valuetext="00:12 / 05:00"'));
check('底部条渲染：时间码按「当前 / 总长」显示',
  barHtml.includes('>00:12 / 05:00<'));
check('底部条渲染：贴底通栏 Douyin 控制条（inset-x-0 bottom-0 + 深色渐变 + backdrop-blur-md）',
  /class="[^"]*inset-x-0 bottom-0[^"]*from-black\/95[^"]*backdrop-blur-md/.test(barHtml));
check('底部条渲染：图标 20px（h-5 w-5 至少四处）',
  (barHtml.match(/\bh-5 w-5\b/g) || []).length >= 4,
  `实际 ${(barHtml.match(/\bh-5 w-5\b/g) || []).length} 处`);
check('底部条渲染：带 data-feed-no-swipe（条上的滚轮/方向键不翻集）',
  barHtml.includes('data-feed-no-swipe'));
check('底部条渲染：含静音开关（aria-pressed，初始未静音）',
  barHtml.includes('aria-pressed="false"') && barHtml.includes('aria-label="静音"'));
check('底部条渲染：不含「上一集」（上滑 / 方向键 / 详细面板已有入口）',
  !barHtml.includes('上一集'));

check('底部条源码：高频读数在组件内部订阅（usePlaybackStore）',
  barSource.includes('usePlaybackStore()'));
check('底部条源码：动作走低频专线 usePlaybackActions',
  barSource.includes('usePlaybackActions()'));
check('底部条源码：没有倍速 / 弹幕 / 增强 / 选集 / 画中画 / 清晰度',
  !/倍速|弹幕|增强|选集|画中画|清晰度/.test(barSource));
check('底部条源码：也没有对应的写入函数（setPlaybackRate / setDanmakuEnabled / setVsrEnabled / enterPip / setQuality）',
  !/setPlaybackRate|setDanmakuEnabled|setVsrEnabled|enterPip|setQuality/.test(barSource));
check('底部条源码：静音通过 toggleMute / isMuted 接入并随图标切换',
  barSource.includes('toggleMute') && barSource.includes('isMuted')
  && barSource.includes('aria-pressed={isMuted}')
  && barSource.includes('VolumeX') && barSource.includes('Volume2'));
check('底部条源码：拖动用 pointer capture（滑出细槽也能继续拖）',
  barSource.includes('setPointerCapture') && barSource.includes('hasPointerCapture'));
check('底部条源码：整次拖动只落位一次（seek( 出现两次：pointerup + 键盘）',
  (barSource.match(/\bseek\(/g) || []).length === 2,
  `实际 ${(barSource.match(/\bseek\(/g) || []).length} 处`);
const pointerMoveStart = barSource.indexOf('const onPointerMove');
const pointerUpStart = barSource.indexOf('const onPointerUp');
check('底部条源码：pointermove 里没有任何 seek（拖时只更新读数）',
  pointerMoveStart >= 0 && pointerUpStart > pointerMoveStart
  && !barSource.slice(pointerMoveStart, pointerUpStart).includes('seek('));
check('底部条源码：pointerup 里落位（松手才 seek）',
  barSource.slice(pointerUpStart, barSource.indexOf('const onPointerCancel')).includes('seek(target)'));
check('底部条源码：时间码复用 ProgressBar 的 formatTime（格式不另立一套）',
  barSource.includes("from './ProgressBar'") && barSource.includes('formatTime('));

// 纯函数：拖动/键盘的目标位置必须可确定性验收，不能只活在事件回调里。
checkEqual('进度纯函数：步长 5 秒', FEED_BAR_SKIP_SECONDS, 5);
check('进度纯函数：轨道最左 → 0', feedBarRatio(100, { left: 100, width: 400 }) === 0);
check('进度纯函数：轨道中点 → 0.5', feedBarRatio(300, { left: 100, width: 400 }) === 0.5);
check('进度纯函数：轨道最右 → 1', feedBarRatio(500, { left: 100, width: 400 }) === 1);
check('进度纯函数：指针在轨道左侧之外 → 仍为 0（不出负数）', feedBarRatio(20, { left: 100, width: 400 }) === 0);
check('进度纯函数：指针在轨道右侧之外 → 仍为 1（不超片长）', feedBarRatio(900, { left: 100, width: 400 }) === 1);
check('进度纯函数：轨道宽度为 0（未布局/隐藏）→ 0', feedBarRatio(200, { left: 100, width: 0 }) === 0);
check('进度纯函数：NaN 输入不产生 NaN 结果', feedBarRatio(NaN, { left: 0, width: 100 }) === 0);
check('落位纯函数：0.5 × 300s = 150s', feedBarTargetSeconds(0.5, 300) === 150);
check('落位纯函数：比例越界时收口到片尾', feedBarTargetSeconds(2, 300) === 300);
check('落位纯函数：片长未知时返回 0（不产生 NaN 时间）',
  feedBarTargetSeconds(0.5, 0) === 0 && feedBarTargetSeconds(0.5, NaN) === 0);
check('键盘纯函数：ArrowLeft 退 5 秒', feedBarKeyboardTarget(100, 'ArrowLeft', 300) === 95);
check('键盘纯函数：ArrowRight 进 5 秒', feedBarKeyboardTarget(100, 'ArrowRight', 300) === 105);
check('键盘纯函数：起点按 ArrowLeft 不外溢为负', feedBarKeyboardTarget(2, 'ArrowLeft', 300) === 0);
check('键盘纯函数：片尾按 ArrowRight 不外溢超长', feedBarKeyboardTarget(298, 'ArrowRight', 300) === 300);
check('键盘纯函数：Home 回片头', feedBarKeyboardTarget(120, 'Home', 300) === 0);
check('键盘纯函数：End 到片尾', feedBarKeyboardTarget(120, 'End', 300) === 300);
check('键盘纯函数：其它键不归进度条管（返回 null）', feedBarKeyboardTarget(120, 'Tab', 300) === null);
check('键盘纯函数：片长未知时键盘也不落位', feedBarKeyboardTarget(0, 'ArrowRight', 0) === null);

// ------------------------------------- 需求 2：键鼠映射（2026-11-10 第四次改版）
//
// 第四次改版把四路输入统一成「换一部」：方向键/滚轮/上滑都**不再换集**。
// 换集只剩自动连播与「详细」面板里的兜底按钮（见 verify:feed-series-nav）。

check('键盘：ArrowUp 映射到上一部（方向字面量仍是 previous）',
  feedViewSource.includes("event.key === 'ArrowUp' ? 'previous'"));
check('键盘：ArrowDown 映射到下一部', feedViewSource.includes("event.key === 'ArrowDown' ? 'next'"));
check('键盘：窗口级 keydown 监听，且卸载时一并移除',
  feedViewSource.includes("window.addEventListener('keydown', onKeyDown)")
  && feedViewSource.includes("window.removeEventListener('keydown', onKeyDown)"));
const activeGuardAt = feedViewSource.indexOf('if (!active) return;');
const keydownMountAt = feedViewSource.indexOf("window.addEventListener('keydown', onKeyDown)");
check('键盘：监听注册在 active 守卫之后（非活动视图不挂）',
  activeGuardAt >= 0 && keydownMountAt > activeGuardAt,
  `guard@${activeGuardAt} mount@${keydownMountAt}`);
check('键盘：焦点在输入框/文本域/可编辑区/弹层内时不响应',
  feedViewSource.includes('blocksFeedKeys(event.target)')
  && feedViewSource.includes('blocksFeedKeys(document.activeElement)')
  && feedViewSource.includes('[contenteditable]:not([contenteditable="false"])')
  && feedViewSource.includes('.crystal-flyout'));
check('键盘/手势：共用同一个 fire 闸门（不是各算各的）',
  feedViewSource.includes('fire(direction, event)')
  && feedViewSource.includes('fire(feedGestureDirection(')
  && feedViewSource.includes('fire(feedWheelDirection('));
check('键盘/手势：闸门动作是 switchSeries（不再有 nextEpisode/previousEpisode 分支）',
  (() => {
    const at = feedViewSource.indexOf('const fire = (');
    const end = feedViewSource.indexOf("window.addEventListener('wheel'", at);
    const body = at >= 0 && end > at ? feedViewSource.slice(at, end) : '';
    return body.includes('feed.switchSeries(')
      && !/feed\.nextEpisode\(\)|feed\.previousEpisode\(\)/.test(body);
  })(),
  'fire 里仍在翻集 —— 那次改版的语义又退回去了');
check('键盘/手势：方向直接交给 store（不再有归一化包装）',
  feedViewSource.includes('feed.switchSeries(direction)')
  && !feedViewSource.includes('normalizeFeedDirection'));
check('滚轮：用 feedWheelDirection（向下=下一部），不复用拖滑公式',
  /fire\(feedWheelDirection\(event\.deltaX, event\.deltaY\)/.test(feedViewSource));
check('节流：时间戳放在 ref 里（重渲染不会把它重置成 0）',
  feedViewSource.includes('const lastNavAtRef = useRef(0)')
  && feedViewSource.includes('feedNavAllowed(lastNavAtRef.current, Date.now())'));
check('节流：550ms 不再硬编码在组件里（唯一出处是 feedNavigation.FEED_NAV_THROTTLE_MS）',
  !feedViewSource.includes('550'));
check('节流：effect 的依赖里不再有 nextEpisode / previousEpisode',
  feedViewSource.includes('[active, feed.switchSeries]'));
check('旧通道已不存在：const trigger = / lastGestureAt',
  !feedViewSource.includes('const trigger =') && !feedViewSource.includes('lastGestureAt'));
check('交互隔离：标题栏（窗口 chrome）不参与滚轮/拖动换剧',
  feedViewSource.includes('[data-launch-part="titlebar"]'));

// ------------------------------ 需求 1：竖排改语义（2026-11-10 增）

// 竖排是纯 props + 一个低频 hook 驱动的，可以脱离 store 服务端渲染。为了让它在
// 本脚本里可渲染，把 `usePlaybackStore` 模块换成桩：只提供 `usePlaybackActions`，
// 而 `usePlaybackStore` 直接抛错——这样「FeedControls 偷偷订了整表」这件事会以
// 渲染失败的方式暴露，而不是靠人去读 import。
const stubFile = path.join(outDir, 'playback-actions-stub.ts');
await writeFile(stubFile, [
  'export function usePlaybackActions() {',
  '  return { isMuted: false, toggleMute: () => {} };',
  '}',
  'export function usePlaybackStore() { throw new Error("FeedControls 不该订阅整表"); }',
  '',
].join('\n'), 'utf8');

const controlsOut = path.join(outDir, 'FeedControls.mjs');
await build({
  entryPoints: [path.resolve('src/components/player/FeedControls.tsx')],
  outfile: controlsOut,
  bundle: true,
  format: 'esm',
  platform: 'node',
  jsx: 'automatic',
  external: EXTERNAL,
  plugins: [{
    name: 'playback-actions-stub',
    setup(builder) {
      builder.onResolve({ filter: /usePlaybackStore$/ }, () => ({ path: stubFile }));
    },
  }],
  logLevel: 'warning',
});
const { FeedControls } = await import(pathToFileURL(controlsOut).href);

const controlsHtml = renderToStaticMarkup(createElement(FeedControls, {
  episodeIndex: 2,
  episodeNumber: 3,
  episodeCount: 102,
  totalEpisodes: 102,
  favoriteMark: undefined,
  busy: false,
  canSwitchSeries: true,
  title: '闪婚后夫人又美又飒',
  description: '简介占位。',
  tags: ['先婚后爱'],
  origin: '红果推荐',
  playableEpisodes: 5,
  onPrevious: () => {},
  onNext: () => {},
  onSwitchSeries: () => {},
  onToggleFavorite: () => {},
  onClose: () => {},
}));

const buttonCount = (controlsHtml.match(/<button/g) || []).length;
check('竖排渲染：恰好三颗按钮（收藏 / 详细 / 返回）', buttonCount === 3, `实际 ${buttonCount} 颗`);
check('竖排渲染：两颗右侧语义与返回都在', ['追剧', '详细'].every(t => controlsHtml.includes(t)));
check('竖排渲染：上一集 / 下一集 / 换一部已不在竖排',
  ['上一集', '下一集', '换一部'].every(t => !controlsHtml.includes(t)));
check('竖排渲染：不含静音（已并入底部播放控制栏）', !controlsHtml.includes('静音'));
check('竖排渲染：详细按钮默认收起（aria-expanded="false"）',
  controlsHtml.includes('aria-expanded="false"'));
check('竖排渲染：容器有分组语义 role=group + aria-label',
  controlsHtml.includes('role="group"') && controlsHtml.includes('aria-label="播放操作"'));

const feedControlsSource = await readFile(path.resolve('src/components/player/FeedControls.tsx'), 'utf8');

check('竖排：不订阅播放器高频状态或播放动作，零 usePlayback* 耦合',
  !feedControlsSource.includes('usePlaybackStore')
  && !feedControlsSource.includes('usePlaybackActions'));
check('竖排：无显式上一部 / 下一部按钮（仅键盘上下键 / 滚轮 / 拖滑切换剧集）',
  !feedControlsSource.includes('上一部') && !feedControlsSource.includes('下一部'));
check('竖排：恰好两颗 ACTION_CLASS（收藏 / 详细）',
  (feedControlsSource.match(/className=\{ACTION_CLASS\}/g) || []).length === 2,
  `实际 ${(feedControlsSource.match(/className=\{ACTION_CLASS\}/g) || []).length} 处`);
check('竖排：换内容的三个动作全部移进面板（PANEL_ACTION 三处）',
  (feedControlsSource.match(/className=\{PANEL_ACTION\}/g) || []).length === 3,
  `实际 ${(feedControlsSource.match(/className=\{PANEL_ACTION\}/g) || []).length} 处`);
check('竖排：详细按钮有 aria-expanded / aria-controls 且无信息时禁用',
  feedControlsSource.includes('aria-expanded={detailOpen}')
  && feedControlsSource.includes('aria-controls={detailOpen ? panelId : undefined}')
  && feedControlsSource.includes('disabled={!canOpenDetail}'));
check('面板：非模态 dialog，标题用 aria-labelledby 绑定',
  feedControlsSource.includes('role="dialog"')
  && feedControlsSource.includes('aria-modal="false"')
  && feedControlsSource.includes('aria-labelledby={titleId}'));
check('面板：容器与遮罩都带 data-feed-no-swipe（滚轮/方向键不被当成翻集）',
  (feedControlsSource.match(/data-feed-no-swipe/g) || []).length >= 3,
  `实际 ${(feedControlsSource.match(/data-feed-no-swipe/g) || []).length} 处`);
check('面板：信息全部来自 props（不新开请求）',
  !feedControlsSource.includes('ipcService') && !feedControlsSource.includes('fetch('));
check('面板：可播边界复用 playableBoundaryText（口径不另立一套）',
  feedControlsSource.includes('playableBoundaryText'));
check('面板：Esc 关闭并把焦点还给「详细」按钮',
  feedControlsSource.includes("event.key !== 'Escape'")
  && feedControlsSource.includes('detailButtonRef.current?.focus'));

// ------------------------------------------------ 简介浮层（源码级，第三次改版）

check('浮层：简介正文只在浮层里渲染一次（{description} 全文件一次）',
  (feedSlideSource.match(/\{description\}/g) || []).length === 1,
  `实际 ${(feedSlideSource.match(/\{description\}/g) || []).length} 处`);
check('浮层：默认不渲染（briefOpen 初值 false）',
  feedSlideSource.includes('const [briefOpen, setBriefOpen] = useState(false)'));
check('浮层：max-h-[50vh] + overflow-y-auto（全文可读但不吃满屏）',
  feedSlideSource.includes('max-h-[50vh] overflow-y-auto'));
check('浮层：带 data-feed-no-swipe（浮层里滚动不翻集）',
  /role="region"[\s\S]{0,400}data-feed-no-swipe/.test(feedSlideSource));
check('浮层：Esc 关闭并把焦点还给入口',
  feedSlideSource.includes("event.key !== 'Escape'")
  && feedSlideSource.includes('entryRef.current?.focus'));
check('信息层：锚点上移（bottom-16 给贴底通栏底部条让位，不再是 bottom-32）',
  feedSlideSource.includes('bottom-16') && !feedSlideSource.includes('bottom-32'));
check('FeedView：把集标题原样传给二级行（清洗发生在 feedEpisodeTitleFragment，不在调用方）',
  (feedViewSource.match(/episodeTitle=\{currentEpisode\?\.title \?\? ''\}/g) || []).length === 1);

// ------------------------------------ 推荐页顶部窗口控制栏（2026-11-10 第四次改版）
//
// 用户要求「从侧栏进入短剧推荐，补齐缺失的顶部窗口控制栏」，并明确约束：
// 复用 TitleBar 的窗口操作、风格与主项目一致、颜色适配推荐页、保留返回入口与
// 频道但不重叠重复。四条都在下面钉住。

const appSource = await readCode('src/App.tsx');
const titleBarSource = await readCode('src/components/layout/TitleBar.tsx');

check('顶栏：App 在 feed 视图下也渲染 TitleBar（不再有 currentView !== \'feed\' 排除）',
  appSource.includes('{!isFullscreen && <TitleBar />}')
  && !/currentView\s*!==\s*'feed'/.test(appSource),
  '旧条件仍在 —— 从侧栏进推荐页后整窗没有标题栏、没有三颗窗口按钮');
check('顶栏：TitleBar 只有一个挂载点（不是又渲染了一条平行栏）',
  (appSource.match(/<TitleBar\b/g) || []).length === 1);
check('顶栏：全屏时仍然隐藏（播放器要占满窗口）',
  appSource.includes('!isFullscreen &&'));
check('顶栏：feed 形态由 currentView === \'feed\' 判定',
  titleBarSource.includes("const isFeed = currentView === 'feed'"));
check('顶栏：拖动区仍在 header 上（onMouseDown → startDragging）',
  titleBarSource.includes('onMouseDown={handleDragStart}')
  && titleBarSource.includes('startDragging()'));
check('顶栏：feed 下中间空出来的是拖动区（不带 data-window-interactive）',
  titleBarSource.includes("data-window-interactive={currentView === 'player' || isFeed ? undefined : ''}"));
check('顶栏：feed 下不渲染搜索框（播放视图里没有搜索语义，还会吃掉拖动区）',
  titleBarSource.includes('currentView !== \'player\' && !isFeed && ('));
check('顶栏：三颗窗口按钮仍在（minimize / maximize / close）',
  ["'minimize'", "'maximize'", "'close'"].every(key => titleBarSource.includes(`key: ${key}`)));
check('顶栏：最大化图标跟真实状态走（isMaximized 三元）',
  titleBarSource.includes('label: isMaximized ?'));
check('顶栏：颜色适配推荐页（深色一套独立常量，不改尺寸）',
  titleBarSource.includes('WIN_BUTTON_NEUTRAL_DARK') && titleBarSource.includes('WIN_BUTTON_CLOSE_DARK')
  && titleBarSource.includes('isFeed ? WIN_BUTTON_NEUTRAL_DARK : WIN_BUTTON_NEUTRAL_LIGHT'));
check('顶栏：feed 底色是**不透明深色实色**（bg-[#161823]/95 与左侧导航栏一致，叠浅色 Mica 也不会发亮灰）',
  titleBarSource.includes('bg-[#161823]/95 border-white/[0.06] text-white')
  && !titleBarSource.includes('bg-black/55 border-white/10'),
  '半透明黑在浅色系统主题下会与 Mica 底混成一条亮灰横条，与暗色画面割裂');
check('顶栏：feed 形态带 data-titlebar-tone（便于定位/主题回归）',
  titleBarSource.includes('data-titlebar-tone={isFeed ? \'dark\' : \'light\'}'));
check('顶栏：保留返回入口（品牌块 → navigateTo(\'explore\')）',
  // 黄果七击入口占用了品牌块点击之后，返回动作收进了 handleBrandClick：
  // 断言跟着改认这条路径，但**意图不变**——品牌块必须能回到精选页。
  titleBarSource.includes("onClick={() => navigateTo('explore')}")
  || (titleBarSource.includes("handleBrandClick")
    && titleBarSource.includes("navigateTo('explore')")));
check('顶栏：不复制频道托盘（频道只在 FeedView 里，全屏也可用）',
  !titleBarSource.includes('aria-label="推荐频道"') && !titleBarSource.includes('selectChannel'));
check('顶栏：不复制返回按钮（返回键在 FeedView 左上，全屏也可用）',
  !titleBarSource.includes('ArrowLeft')
  && !titleBarSource.includes('aria-label="返回短剧精选"'),
  '标题栏里又出现一颗返回按钮 —— 与 FeedView 左上那颗重复，且全屏时它会被隐藏');
check('顶栏：feed 下不显示版本徽标（把宽度让给画面）',
  titleBarSource.includes('{!isFeed && <VersionBadge />}'));

await rm(outDir, { recursive: true, force: true });

let failed = 0;
for (const { label, ok, detail } of checks) {
  if (ok) {
    console.log(`PASS ${label}`);
  } else {
    failed += 1;
    console.error(`FAIL ${label}${detail ? `\n     ${detail}` : ''}`);
  }
}
console.log(`\n${checks.length - failed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);