import React, { useCallback, useEffect, useId, useRef, useState } from 'react';
import { ChevronDown } from 'lucide-react';
import type { SeriesItem } from '../../types/catalog';

/**
 * 推荐页的左下信息层。
 *
 * ## 2026-11-10 第三次改版：信息层从「卡片式简介墙」压成三条事实
 *
 * 用户原话：「左下角文字区域太多了，影响视觉空间了，和体验了」。
 *
 * 第二版的教训是**层级分清了、总量没减**：剧名 + 二级行 + 2 行简介 + 标签，
 * 叠起来仍有近 150px 高，而且简介正文默认就占着画面。这一版据此做了三件事：
 *
 * 1. **简介默认完全隐藏**，只留一行「简介」入口；点开在浮层里看全文。
 * 2. **标签从信息层撤走** —— 它在「详细」面板里本来就有，这里再列一遍只是
 *    把左下角重新撑宽。
 * 3. **二级行不再承载叙事文本**（见下），这是「同一段简介出现两遍」的真因。
 *
 * ## 「简介重复」的根因：二级行把长简介当成了集标题
 *
 * 用户截图里那句话是「第2集·乞丐李傲本是皇族太子…弹指镇压。…·共 87 集」——
 * 前 12 个字像集标题，后面全是剧情简介。也就是说 **数据源给的 `episodeTitle`
 * 本身就是整段简介**。旧版 `feedEpisodeLine` 把它原样拼进二级行，紧接着
 * `description` 又原样渲染一次，视觉上就是同一段文字出现两遍。
 *
 * 修法不是「在后端清洗」也不是「只渲染一遍」，而是**给二级行定职责**：
 * 它只陈述结构化事实（第几集 / 共几集 / 开放到第几集）。集标题只有在
 * 「像标题」的时候才并入 —— 见 `feedEpisodeTitleFragment` 的两条判据。
 *
 * ## 标题层级
 *
 * 页面里有且只有 `FeedView` 的「短剧推荐」是 `h1`，所以剧名必须是 `h2`。
 * 旧版把剧名写成 `h1`，和页面标题撞级 —— 屏幕阅读器按标题跳转时会出现两个
 * 一级标题，用户无法用「跳到大标题」区分「页面」和「当前剧」。
 *
 * ## 可播边界为什么单独抽成函数
 *
 * 红果对未开放集的播放页回 200 + 完整 HTML（失败是静默的，见
 * `services/episodeAvailability.ts` 文件头），所以「现在只开放到第 K 集」必须
 * 前置到文案里。而这个判断有三个坑：边界缺失（`undefined`）不能当「全部开放」
 * 也**不能**显示「开放前」字样；集数为 0 时没有参照物；边界 ≥ 集数等于全开放。
 * 写成 JSX 内联三元就再也测不到了，所以抽成 `playableBoundaryText`。
 */

export interface FeedEpisodeLineInput {
  episodeNumber: number;
  episodeTitle: string;
  episodesCount: number;
  playableEpisodes?: number;
}

/**
 * 可播边界提示。`null` = 这一路源没给信号 / 已经全开放 / 没有集数参照，
 * 此时**什么都不显示**，绝不拿缺席的值去猜。
 */
export function playableBoundaryText(input: Pick<FeedEpisodeLineInput, 'episodesCount' | 'playableEpisodes'>): string | null {
  const { episodesCount, playableEpisodes } = input;
  if (playableEpisodes === undefined || playableEpisodes === null) return null;
  if (!Number.isFinite(playableEpisodes)) return null;
  const limit = Math.floor(playableEpisodes);
  // 与 `episodesBadgeText` 同一条口径：只有「边界 < 总集数」时才值得说，
  // 相等就是全开放，多一句「开放前 102 集」只会让人以为还有未开放的。
  if (episodesCount <= 0 || limit >= episodesCount) return null;
  return `开放前 ${limit} 集`;
}

/**
 * 集标题的可并入上限（字符数）。
 *
 * 12 个字够放下「偶遇」「替嫁新娘」「她回来了」这类正常的集标题；
 * 超过这个长度基本可以断定源把整段简介塞进了 `title`。
 */
export const FEED_EPISODE_TITLE_MAX_CHARS = 12;

/** 句读标点：真正的集标题不会带这些，叙事文本几乎必然带。 */
const NARRATIVE_PUNCTUATION = /[。！？，；、…]/;

/**
 * 从 `episodeTitle` 里取「像标题」的那一段；不像就返回 `null`（不并入二级行）。
 *
 * 两条判据（任一不满足即整段丢弃）：
 * 1. 长度 ≤ `FEED_EPISODE_TITLE_MAX_CHARS`；
 * 2. 不含句读标点。
 *
 * 为什么是**丢弃**而不是「截前 12 字 + …」：截断同样长的叙事文本会在二级行里
 * 留下「女主被家族逼着嫁给…」这种半截话，它既不成立也恰好是简介的开头，
 * 用户仍会读成「简介又出现了一遍」。二级行的价值在于事实，不在叙事；
 * 需要正文的地方已经由「简介」入口和「详细」面板各给了一次。
 */
export function feedEpisodeTitleFragment(episodeTitle: string): string | null {
  // 详情里的 `title` 往往自带「第 3 集」前缀，直接拼会得到「第 3 集 第 3 集 偶遇」。
  const tail = episodeTitle.replace(/^第\s*\d+\s*集\s*[·：:—-]?\s*/, '').trim();
  if (!tail) return null;
  if ([...tail].length > FEED_EPISODE_TITLE_MAX_CHARS) return null;
  if (NARRATIVE_PUNCTUATION.test(tail)) return null;
  return tail;
}

/** 二级信息行：`第 3 集 · 偶遇 · 共 102 集 · 开放前 5 集`。 */
export function feedEpisodeLine(input: FeedEpisodeLineInput): string {
  const parts: string[] = [`第 ${input.episodeNumber} 集`];
  const fragment = feedEpisodeTitleFragment(input.episodeTitle);
  if (fragment) parts.push(fragment);
  if (input.episodesCount > 0) parts.push(`共 ${input.episodesCount} 集`);
  const boundary = playableBoundaryText(input);
  if (boundary) parts.push(boundary);
  return parts.join(' · ');
}

interface FeedSlideProps {
  series: SeriesItem | null;
  episodeNumber: number;
  episodeTitle: string;
  /** `FeedDetail.playableEpisodes`；缺失表示源没给边界信号。 */
  playableEpisodes?: number;
  /** 切剧过渡方向：用于配合抖音风垂直滑入动画。 */
  slideDirection?: 'next' | 'previous';
}

export const FeedSlide: React.FC<FeedSlideProps> = ({
  series,
  episodeNumber,
  episodeTitle,
  playableEpisodes,
  slideDirection = 'next',
}) => {
  const [briefOpen, setBriefOpen] = useState(false);
  const entryRef = useRef<HTMLButtonElement | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const briefId = useId();

  /**
   * 关闭浮层，并把焦点还给「简介」入口。
   *
   * 焦点不能掉进 `body`：浮层是 `pointer-events-auto` 的独立层，Esc 关掉之后
   * 若焦点留在被卸载的节点上，下一位 Tab 会从文档头重新开始，用户得再走一遍。
   */
  const closeBrief = useCallback((restoreFocus = true) => {
    setBriefOpen(false);
    if (restoreFocus) entryRef.current?.focus({ preventScroll: true });
  }, []);

  useEffect(() => {
    if (!briefOpen) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      closeBrief();
    };
    window.addEventListener('keydown', onKeyDown);
    // 浮层可聚焦（tabIndex=-1）：打开后先进去，读屏才会念出区域标题。
    const timer = window.setTimeout(() => scrollRef.current?.focus({ preventScroll: true }), 0);
    return () => {
      window.removeEventListener('keydown', onKeyDown);
      window.clearTimeout(timer);
    };
  }, [briefOpen, closeBrief]);

  if (!series) return null;
  const description = series.brief?.trim() ?? '';
  const hasDescription = description.length > 0;
  const lineText = feedEpisodeLine({
    episodeNumber,
    episodeTitle,
    episodesCount: series.episodesCount,
    playableEpisodes,
  });

  return (
    <section
      // 紧贴底部一体化控制栏（44px 高）上方，保持抖音左下无框悬浮排版。
      className="pointer-events-none absolute inset-x-0 bottom-16 z-20 px-5 sm:px-7 select-none"
      aria-label="当前短剧"
      aria-live="polite"
      aria-atomic="false"
      data-feed-slide={series.id}
    >
      <div className={`relative max-w-[min(28rem,56vw)] ${slideDirection === 'previous' ? 'ttv-feed-slide-prev' : 'ttv-feed-slide-next'}`}>
        {/* 简介浮层：**向上**展开，避免遮住底部播放条。 */}
        {briefOpen && hasDescription && (
          <div
            ref={scrollRef}
            id={briefId}
            role="region"
            aria-label="简介全文"
            tabIndex={-1}
            // 让 `FeedView` 的滚轮/方向键白名单生效：在浮层里滚动不会翻集。
            data-feed-no-swipe
            className="pointer-events-auto absolute inset-x-0 bottom-full z-10 mb-2 max-h-[50vh] overflow-y-auto rounded-2xl border border-white/15 bg-slate-950/90 px-3.5 py-2.5 text-xs leading-5 text-white/90 shadow-fluent backdrop-blur-md animate-fluent-page-in focus:outline-none"
          >
            <p className="whitespace-pre-line">{description}</p>
          </div>
        )}

        <div
          className="text-white [text-shadow:0_2px_10px_rgba(0,0,0,0.85)]"
          data-feed-info-card
        >
          {/* 一级：剧名。`h2` 理由见文件头「标题层级」。 */}
          <h2 className="line-clamp-2 text-[19px] font-bold leading-snug tracking-tight">{series.title}</h2>

          {/* 二级：集号 + 共几集 + 可播边界。 */}
          <p className="mt-1 text-xs font-medium text-white/85" title={lineText}>
            {lineText}
          </p>

          {/* 三级：只有一行紧凑入口，正文默认隐藏在向上浮层中。 */}
          {hasDescription && (
            <button
              ref={entryRef}
              type="button"
              onClick={() => (briefOpen ? closeBrief(false) : setBriefOpen(true))}
              aria-expanded={briefOpen}
              aria-controls={briefOpen ? briefId : undefined}
              className="pointer-events-auto mt-1.5 inline-flex items-center gap-1 rounded-full bg-white/10 px-2.5 py-0.5 text-[11px] font-semibold text-white/90 backdrop-blur-sm transition-colors hover:bg-white/20 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/90"
              data-feed-brief-entry
            >
              简介
              <ChevronDown
                className={`h-3 w-3 transition-transform duration-150 ${briefOpen ? 'rotate-180' : ''}`}
                aria-hidden="true"
              />
            </button>
          )}
        </div>
      </div>
    </section>
  );
};