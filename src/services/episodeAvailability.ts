import type { EpisodeItem } from '../types/series';

/**
 * 集数可播边界：把「这部剧有多少集」和「现在能播到第几集」分成两件事。
 *
 * ## 为什么需要这一层（2026-11-09 实测）
 *
 * 红果公开详情页对同一部剧给三个数字：
 *
 * | 字段 | 实测值 | 含义 |
 * | --- | --- | --- |
 * | `episode_cnt` | 102 | 这部剧共多少集 |
 * | `vid_list.len()` | 102 | 全集分集 vid 数组（全集，不是可播集） |
 * | `accessible_episode_cnt` | 5 | 现在开放几集 |
 *
 * 前两个是「有多少」，第三个才是「能看多少」。此前把 `vid_list` 直接铺成可播集，
 * 于是详情页把 102 集全画成可点。而未开放集的播放页**仍然返回 200 + 一份完整
 * HTML**（实测 134,662 字节，与第 1 集的 134,573 相差无几），只是里面没有播放
 * 数据——用户点下去什么都不发生，失败是静默的。
 *
 * 所以判定必须前置到 UI：不可播的格子显式置灰并说明理由，而不是让点击落进
 * 那条静默路径。
 *
 * ## 缺失语义
 *
 * `playableEpisodes` 为 `undefined` 表示**这一路源没有给出这个信号**（App 通道
 * 的 episodes 本身就是服务端可播集、动漫源是完整 MP4 列表）。此时一律按可播
 * 处理：拿缺席的值去猜，猜错的代价比不猜更大。
 */
export function isEpisodeLocked(
  episode: Pick<EpisodeItem, 'episodeNumber'>,
  playableEpisodes: number | undefined,
): boolean {
  if (playableEpisodes === undefined || playableEpisodes === null) return false;
  if (!Number.isFinite(playableEpisodes)) return false;
  return episode.episodeNumber > playableEpisodes;
}

/**
 * 集数角标的文案：可播边界优先于站方全集口径。
 *
 * 为什么要覆盖 `rightText`：站方给的 `episode_right_text` 实测是「全102集」，
 * 而当时只开放 5 集。直接拿它当角标，用户看到的就是一个 102 集的承诺。
 * 只有两者相等（或边界未知）时才退回站方文案 —— 那时它才是准确的。
 */
export function episodesBadgeText(input: {
  episodesCount: number;
  playableEpisodes?: number;
  /** 站方给的角标文案（`episode_right_text`），如「更新至 12 集」。 */
  rightText?: string;
  /** 动漫单集形态：集数为 0 时显示「单集/已完结」。 */
  animeSingle?: boolean;
}): string {
  const { episodesCount, playableEpisodes, rightText, animeSingle } = input;
  if (animeSingle) return '单集/已完结';
  if (
    playableEpisodes !== undefined &&
    Number.isFinite(playableEpisodes) &&
    playableEpisodes < episodesCount
  ) {
    return `开放前 ${playableEpisodes} 集 · 共 ${episodesCount} 集`;
  }
  if (rightText) return rightText;
  return `共 ${episodesCount} 集全`;
}