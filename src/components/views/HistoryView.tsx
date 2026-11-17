import React, { useEffect, useMemo, useRef, useState } from 'react';
import { useHistoryStore } from '../../stores/useHistoryStore';
import { usePlaybackActions } from '../../stores/usePlaybackStore';
import { useAnimePlayerActions } from '../../stores/useAnimePlayerStore';
import { useAppStore } from '../../stores/useAppStore';
import { useSettingsStore } from '../../stores/useSettingsStore';
import { tryLaunchNativePlayer } from '../../services/preferredPlayback';
import { NATIVE_PLAYER_AVAILABLE } from '../../services/nativePlayerRollout';
import {
  NATIVE_OPEN_FAILED_NOTICE,
  NATIVE_SKIPPED_NOTICE,
  planPlaybackRoute,
} from '../../services/playbackRouting';
import {
  classifyHistoryModule,
  historyKey,
  historyRegionKey,
  historyRegionLabel,
  isHuangguoHistory,
  normalizeHistoryType,
  type HistoryModule,
  type HistoryRegion,
} from '../../services/historyTypes';
import type { HistoryClearScope, HistoryType } from '../../types/history';
import { MicaCard } from '../common/MicaCard';
import { BackToTop } from '../common/BackToTop';
import { FluentButton } from '../common/FluentButton';
import { CoverImage } from '../common/CoverImage';
import { coverResolver, coverRawUsable } from '../../services/ipc';
import {
  History,
  Trash2,
  Play,
  Clock,
  CheckCircle2,
  AlertTriangle,
  Search,
  X,
} from 'lucide-react';

type ModuleFilter = 'all' | Exclude<HistoryModule, 'huangguo'>;
type RegionFilter = 'all' | 'drama' | 'comic' | 'ai' | 'adult';

const MODULE_TABS: Array<{ value: ModuleFilter; label: string }> = [
  { value: 'all', label: '全部' },
  { value: 'featured', label: '短剧精选' },
  { value: 'recommendation', label: '短剧推荐' },
  { value: 'anime', label: '动漫专区' },
];

const MODULE_BADGE_CLASS: Record<Exclude<HistoryModule, 'huangguo'>, string> = {
  featured: 'bg-blue-50 text-blue-600 border-blue-200/60',
  recommendation: 'bg-amber-50 text-amber-700 border-amber-200/60',
  anime: 'bg-purple-50 text-purple-600 border-purple-200/60',
};

function formatDuration(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}分${s}秒`;
}

function formatRelativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  const hours = Math.floor(diff / (3600 * 1000));
  if (hours < 1) return '刚刚观看';
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  return `${days} 天前`;
}

export const HistoryView: React.FC = () => {
  const { records, isLoading, loadHistory, removeRecord, clearHistory } = useHistoryStore();
  // 只订阅低频动作专线：整表订阅会让历史列表跟着播放进度重渲染。
  const { openEpisode } = usePlaybackActions();
  const { settings, updateSettings } = useSettingsStore();
  const { open: openAnimeEpisode } = useAnimePlayerActions();
  const { currentView, navigateTo, openFeed, showToast } = useAppStore();

  const [showClearConfirm, setShowClearConfirm] = useState(false);
  const [filterKeyword, setFilterKeyword] = useState('');
  const [activeModule, setActiveModule] = useState<ModuleFilter>('all');
  const [activeRegion, setActiveRegion] = useState<RegionFilter>('all');

  // 每次进入历史页都重新拉取一次（视图常驻 DOM，只切 hidden）。
  useEffect(() => {
    if (currentView === 'history') void loadHistory();
  }, [currentView, loadHistory]);

  // 黄果模块有独立专属的观看历史入口，不混入主历史页。
  const mainRecords = useMemo(
    () => records.filter(item => !isHuangguoHistory(item)),
    [records],
  );

  const moduleCounts = useMemo(() => {
    const counts: Record<ModuleFilter, number> = {
      all: mainRecords.length,
      featured: 0,
      recommendation: 0,
      anime: 0,
    };
    for (const item of mainRecords) {
      const mod = classifyHistoryModule(item);
      if (mod !== 'huangguo') counts[mod] += 1;
    }
    return counts;
  }, [mainRecords]);

  const moduleRecords = useMemo(() => {
    if (activeModule === 'all') return mainRecords;
    return mainRecords.filter(item => classifyHistoryModule(item) === activeModule);
  }, [mainRecords, activeModule]);

  const regionTabs = useMemo<Array<{ value: RegionFilter; label: string; count: number }>>(() => {
    if (activeModule !== 'featured' && activeModule !== 'recommendation') return [];
    const counts: Record<Exclude<RegionFilter, 'all'>, number> = {
      drama: 0,
      comic: 0,
      ai: 0,
      adult: 0,
    };
    for (const item of moduleRecords) {
      const region = historyRegionKey(item) as HistoryRegion;
      if (region === 'drama' || region === 'comic' || region === 'ai' || region === 'adult') {
        counts[region] += 1;
      }
    }
    const base: Array<{ value: RegionFilter; label: string; count: number }> = [
      { value: 'all', label: '全部区域', count: moduleRecords.length },
      { value: 'drama', label: activeModule === 'featured' ? '短剧专区' : '短剧', count: counts.drama },
      { value: 'comic', label: activeModule === 'featured' ? '漫剧次元' : '漫剧', count: counts.comic },
      { value: 'ai', label: 'AI 剧', count: counts.ai },
    ];
    if (counts.adult > 0) {
      base.push({ value: 'adult', label: '神秘小窝', count: counts.adult });
    }
    return base;
  }, [activeModule, moduleRecords]);

  const selectModule = (next: ModuleFilter) => {
    setActiveModule(next);
    setActiveRegion('all');
  };

  const keyword = filterKeyword.trim().toLowerCase();
  const visibleRecords = useMemo(() => {
    return moduleRecords.filter(item => {
      if (
        (activeModule === 'featured' || activeModule === 'recommendation')
        && activeRegion !== 'all'
        && historyRegionKey(item) !== activeRegion
      ) {
        return false;
      }
      if (keyword && !item.title.toLowerCase().includes(keyword)) {
        return false;
      }
      return true;
    });
  }, [moduleRecords, activeModule, activeRegion, keyword]);

  const handleResume = (
    seriesId: string,
    episodeId: string,
    position: number,
    channel?: string | null,
    title?: string,
    episodeNumber?: number,
    historyType?: HistoryType,
  ) => {
    void (async () => {
      /** 落点判定统一口径（见 `services/playbackRouting.ts`）。 */
      const route = planPlaybackRoute({
        preferredPlayer: settings.preferredPlayer,
        nativeAvailable: NATIVE_PLAYER_AVAILABLE,
        seriesId,
        episodeId,
        channel,
        surface: 'history',
      });
      if (route.kernel === 'native') {
        try {
          const launched = await tryLaunchNativePlayer({
            preferredPlayer: settings.preferredPlayer,
            autoNext: settings.autoNext,
            setAutoNext: on => { void updateSettings({ autoNext: on }); },
            surface: 'history',
            seriesId,
            episodeId,
            startSeconds: position,
            title,
            episodeNumber,
            channel,
            historyType,
          });
          if (launched) return;
          // 计划说能交、实际没交出去：必须说出来，不能悄悄换成内置播放器。
          showToast(NATIVE_SKIPPED_NOTICE, 'warning');
        } catch (error) {
          console.warn('MPV 播放器打开失败，回退网页播放', error);
          showToast(NATIVE_OPEN_FAILED_NOTICE, 'error');
        }
      } else if (route.notice) {
        showToast(route.notice, 'warning');
      }
      // 动漫走独立播放器：dmghg 的 id 带 `dmghg:` 前缀，而暴风兜底源的是纯数字 id，
      // 所以前缀与 channel 两个判据都要看（历史记录里 channel 已落库）。
      if (channel === 'anime' || seriesId.startsWith('dmghg:') || seriesId.startsWith('bfzy:')) {
        void openAnimeEpisode(seriesId, episodeId, position);
        return;
      }
      navigateTo('player', seriesId);
      openEpisode(seriesId, episodeId, position, undefined, undefined, historyType);
    })();
  };

  const clearScope: HistoryClearScope = activeModule === 'all' ? 'non_huangguo' : activeModule;
  const clearTargetLabel = activeModule === 'all'
    ? '观看历史'
    : `「${MODULE_TABS.find(tab => tab.value === activeModule)?.label ?? ''}」观看历史`;

  const handleClearAll = async () => {
    await clearHistory(clearScope);
    setShowClearConfirm(false);
    showToast(`${clearTargetLabel}已清空`, 'info');
  };

  /** 回到顶部要自己拿得到滚动容器。与发现页 / 动漫专区同一套交互。 */
  const scrollRef = useRef<HTMLDivElement | null>(null);

  const emptyAction = (() => {
    if (activeModule === 'recommendation') {
      return { label: '前往短剧推荐', run: () => openFeed() };
    }
    if (activeModule === 'anime') {
      return { label: '前往动漫专区', run: () => navigateTo('anime') };
    }
    return { label: '前往短剧精选', run: () => navigateTo('explore') };
  })();

  return (
    <div ref={scrollRef} className="flex-1 h-full overflow-y-auto p-8 max-w-5xl mx-auto flex flex-col gap-6 select-none">
      {/* 头部：标题与清空按钮 */}
      <div className="flex items-center justify-between pb-4 border-b border-black/[0.05]">
        <div>
          <h1 className="text-xl font-bold text-slate-900 flex items-center gap-2">
            <History className="w-5 h-5 text-blue-600" />
            <span>观看历史记录</span>
          </h1>
          <p className="text-xs text-slate-400 mt-1">
            按模块与区域分开记录，支持一键断点续播与独立清理
          </p>
        </div>

        {mainRecords.length > 0 && (
          <div className="flex items-center gap-3">
            {/* 即时搜索：按剧名筛选历史记录 */}
            <div className="relative w-52">
              <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-slate-400 pointer-events-none" />
              <input
                type="text"
                value={filterKeyword}
                onChange={(e) => setFilterKeyword(e.target.value)}
                placeholder="搜索看过的剧..."
                aria-label="搜索历史记录"
                className="ttv-input w-full h-8 pl-8 pr-7 text-xs bg-white rounded-lg border border-slate-200/80 shadow-xs"
              />
              {filterKeyword && (
                <button
                  type="button"
                  onClick={() => setFilterKeyword('')}
                  aria-label="清除搜索"
                  className="absolute right-2 top-1/2 -translate-y-1/2 text-slate-400 hover:text-slate-600 p-0.5 cursor-pointer"
                >
                  <X className="w-3.5 h-3.5" />
                </button>
              )}
            </div>

            {moduleRecords.length > 0 && (
              <div className="p-1 bg-slate-100/90 rounded-xl border border-slate-200/70 shadow-inner inline-flex">
                <FluentButton
                  variant="danger"
                  size="sm"
                  icon={<Trash2 className="w-3.5 h-3.5" />}
                  onClick={() => setShowClearConfirm(true)}
                  className="shadow-sm"
                >
                  {activeModule === 'all' ? '清空历史' : '清空本区'}
                </FluentButton>
              </div>
            )}
          </div>
        )}
      </div>

      {/* 模块切换与区域细分 */}
      {mainRecords.length > 0 && (
        <div className="flex flex-col gap-2.5">
          <div className="flex flex-wrap items-center gap-1 self-start rounded-xl border border-slate-200/70 bg-slate-100/90 p-1 shadow-inner">
            {MODULE_TABS.map(tab => {
              const active = activeModule === tab.value;
              const count = moduleCounts[tab.value];
              return (
                <button
                  key={tab.value}
                  type="button"
                  onClick={() => selectModule(tab.value)}
                  className={`flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-xs transition-all duration-150 cursor-pointer ${
                    active
                      ? 'fluent-convex-tab font-bold text-blue-600'
                      : 'font-medium text-slate-600 hover:bg-white/60 hover:text-slate-900'
                  }`}
                >
                  <span>{tab.label}</span>
                  <span
                    className={`rounded-md px-1.5 py-0.5 text-[10px] font-bold ${
                      active ? 'bg-blue-50 text-blue-600' : 'bg-slate-200/70 text-slate-500'
                    }`}
                  >
                    {count}
                  </span>
                </button>
              );
            })}
          </div>

          {regionTabs.length > 0 && (
            <div className="ttv-chips">
              {regionTabs.map(region => (
                <button
                  key={region.value}
                  type="button"
                  onClick={() => setActiveRegion(region.value)}
                  className={`ttv-chip${activeRegion === region.value ? ' is-active' : ''}`}
                >
                  {region.label}
                  {region.count > 0 ? ` (${region.count})` : ''}
                </button>
              ))}
            </div>
          )}
        </div>
      )}

      {/* 历史记录列表 */}
      {isLoading && mainRecords.length === 0 ? (
        <div className="flex flex-col gap-3" aria-busy="true">
          {Array.from({ length: 4 }).map((_, index) => (
            <MicaCard key={index} className="p-4 flex items-center gap-4">
              <div className="h-[88px] w-16 shrink-0 rounded-xl shimmer-loading" />
              <div className="flex min-w-0 flex-1 flex-col gap-2">
                <div className="h-4 w-1/3 rounded shimmer-loading" />
                <div className="h-3 w-2/5 rounded shimmer-loading" />
                <div className="h-3 w-1/4 rounded shimmer-loading" />
              </div>
            </MicaCard>
          ))}
        </div>
      ) : mainRecords.length === 0 ? (
        <div className="py-24 flex flex-col items-center justify-center text-center gap-3">
          <img src="/app-icon.png" alt="" className="w-14 h-14 object-contain opacity-60" draggable={false} />
          <p className="text-sm font-semibold text-slate-700">暂无观看历史</p>
          <p className="text-xs text-slate-400">在发现页寻找心仪短剧开启追剧体验吧</p>
          <div className="p-1 bg-slate-100/90 rounded-xl border border-slate-200/70 shadow-inner inline-flex mt-2">
            <FluentButton
              variant="primary"
              size="sm"
              onClick={() => navigateTo('explore')}
              className="shadow-sm"
            >
              前往短剧精选
            </FluentButton>
          </div>
        </div>
      ) : visibleRecords.length === 0 ? (
        <div className="py-24 flex flex-col items-center justify-center text-center gap-3">
          <img src="/app-icon.png" alt="" className="w-14 h-14 object-contain opacity-60" draggable={false} />
          <p className="text-sm font-semibold text-slate-700">
            {keyword
              ? `没有匹配「${filterKeyword.trim()}」的历史记录`
              : `暂无${MODULE_TABS.find(tab => tab.value === activeModule)?.label ?? ''}观看历史`}
          </p>
          <p className="text-xs text-slate-400">
            {keyword ? '换个关键词试试，或清除筛选查看全部' : '观看过的内容会自动按所属模块与区域归档'}
          </p>
          {!keyword && activeRegion === 'all' && (
            <div className="p-1 bg-slate-100/90 rounded-xl border border-slate-200/70 shadow-inner inline-flex mt-2">
              <FluentButton
                variant="primary"
                size="sm"
                onClick={emptyAction.run}
                className="shadow-sm"
              >
                {emptyAction.label}
              </FluentButton>
            </div>
          )}
        </div>
      ) : (
        <div className="flex flex-col gap-3">
          {visibleRecords.map(item => {
            const mod = classifyHistoryModule(item) as Exclude<HistoryModule, 'huangguo'>;
            const regionLabel = historyRegionLabel(item);
            const itemHistoryType = normalizeHistoryType(item.historyType);
            return (
              <MicaCard
                key={historyKey(item)}
                hoverable
                className="p-4 flex items-center justify-between gap-4 animate-fluent-card-in group"
              >
                <div className="flex items-center gap-4 min-w-0">
                  {/* 封面缩略图 3:4 = 64×88 */}
                  <div
                    onClick={() => handleResume(item.seriesId, item.episodeId, item.positionSeconds, item.channel, item.title, item.episodeNumber, itemHistoryType)}
                    className="relative w-16 h-[88px] rounded-xl overflow-hidden shadow-sm flex-shrink-0 cursor-pointer group-hover:scale-105 transition-transform duration-300"
                  >
                    <CoverImage
                      src={item.seriesCover}
                      title={item.title}
                      fallbackChar="剧"
                      placeholderTextClassName="text-base"
                      loading="eager"
                      resolveSrc={coverResolver(item.seriesId, item.seriesCover)}
                      rawUsable={coverRawUsable(item.seriesId)}
                    />
                    <div className="absolute inset-0 bg-black/20 group-hover:bg-black/40 flex items-center justify-center transition-colors">
                      <Play className="w-5 h-5 text-white fill-current opacity-80 group-hover:opacity-100 transform group-hover:scale-110 transition-transform" />
                    </div>
                  </div>

                  {/* 详细文字信息 */}
                  <div className="flex flex-col gap-1.5 min-w-0">
                    <div className="flex items-center gap-2 flex-wrap">
                      <h3
                        onClick={() => navigateTo('detail', item.seriesId)}
                        className="text-sm font-bold text-slate-900 truncate hover:text-blue-600 cursor-pointer transition-colors"
                      >
                        {item.title}
                      </h3>
                      <span className={`text-[10px] font-bold px-1.5 py-0.5 rounded border ${MODULE_BADGE_CLASS[mod]}`}>
                        {regionLabel}
                      </span>
                      {item.isFinished ? (
                        <span className="text-[10px] font-bold px-1.5 py-0.5 rounded bg-emerald-50 text-emerald-600 border border-emerald-200/60 flex items-center gap-1">
                          <CheckCircle2 className="w-2.5 h-2.5" /> 已看完
                        </span>
                      ) : (
                        <span className="text-[10px] font-bold px-1.5 py-0.5 rounded bg-blue-50 text-blue-600 border border-blue-200/60">
                          看到第 {item.episodeNumber} 集
                        </span>
                      )}
                    </div>

                    <div className="flex items-center gap-3 text-xs text-slate-400">
                      <span>
                        进度：
                        {item.durationSeconds > 0
                          ? `${formatDuration(item.positionSeconds)} / ${formatDuration(item.durationSeconds)}`
                          : item.positionSeconds > 0
                            ? `已看 ${formatDuration(item.positionSeconds)}`
                            : '尚未开始播放'}
                      </span>
                      <span>·</span>
                      <span className="flex items-center gap-1">
                        <Clock className="w-3 h-3" />
                        {formatRelativeTime(item.updatedAt)}
                      </span>
                    </div>

                    {/* 进度条 */}
                    <div className="w-48 h-1.5 bg-slate-200/70 rounded-full overflow-hidden mt-0.5">
                      <div
                        className="h-full bg-blue-600 rounded-full"
                        style={{ width: `${item.progressPercent}%` }}
                      />
                    </div>
                  </div>
                </div>

                {/* 右侧行动点 (嵌入式操作底座) */}
                <div className="p-1 bg-slate-100/80 rounded-xl border border-slate-200/60 shadow-inner flex items-center gap-1.5 flex-shrink-0">
                  <FluentButton
                    variant="primary"
                    size="sm"
                    icon={<Play className="w-3 h-3 fill-current" />}
                    onClick={() => handleResume(item.seriesId, item.episodeId, item.positionSeconds, item.channel, item.title, item.episodeNumber, itemHistoryType)}
                    className="shadow-sm"
                  >
                    继续播放
                  </FluentButton>

                  <button
                    onClick={() => removeRecord(item.seriesId, itemHistoryType)}
                    className="p-1.5 rounded-lg text-slate-400 hover:text-rose-600 hover:bg-white/80 transition-colors cursor-pointer"
                    title="移除此条记录"
                  >
                    <Trash2 className="w-4 h-4" />
                  </button>
                </div>
              </MicaCard>
            );
          })}
        </div>
      )}

      {/* 清空历史二次确认弹窗 */}
      {showClearConfirm && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-sm animate-fade-in">
          <div className="w-full max-w-sm rounded-3xl bg-white/95 backdrop-blur-2xl border border-white shadow-fluent-hud p-5 flex flex-col gap-4 animate-fluent-scale-in">
            <div className="flex items-center gap-3 text-amber-600">
              <div className="w-9 h-9 rounded-xl bg-amber-50 flex items-center justify-center border border-amber-200/60">
                <AlertTriangle className="w-5 h-5" />
              </div>
              <h3 className="text-sm font-bold text-slate-800">确认清空{clearTargetLabel}？</h3>
            </div>
            <p className="text-xs text-slate-500 leading-relaxed">
              此操作将永久移除当前范围的观看进度和断点记录，无法撤销。
            </p>
            <div className="flex items-center justify-end gap-2 pt-2">
              <FluentButton
                variant="secondary"
                size="sm"
                onClick={() => setShowClearConfirm(false)}
              >
                取消
              </FluentButton>
              <FluentButton
                variant="danger"
                size="sm"
                onClick={handleClearAll}
              >
                确认清空
              </FluentButton>
            </div>
          </div>
        </div>
      )}

      <BackToTop targetRef={scrollRef} />
    </div>
  );
};
