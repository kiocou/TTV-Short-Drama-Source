import {
  episodesBadgeText,
  isEpisodeLocked,
} from '../src/services/episodeAvailability.ts';

/**
 * 集数可播边界验收。
 *
 * 锁住的是 2026-11-09 用公开详情页实测出来的三个数字：`episode_cnt=102`、
 * `vid_list.len()=102`、`accessible_episode_cnt=5`。曾经的实现把后两个混读，
 * 详情页因此把 102 集全画成可点，而播放页对未开放集仍回 200（失败静默）。
 */

const ep = (n) => ({ episodeNumber: n });

const checks = [
  // ---- 边界判定 ----
  ['前 N 集可播', !isEpisodeLocked(ep(1), 5) && !isEpisodeLocked(ep(5), 5)],
  ['超出边界即锁', isEpisodeLocked(ep(6), 5) && isEpisodeLocked(ep(102), 5)],
  ['边界为 0 时全集皆锁', isEpisodeLocked(ep(1), 0)],
  ['边界缺失按可播处理（不拿缺席的值去猜）',
    !isEpisodeLocked(ep(102), undefined) && !isEpisodeLocked(ep(102), null)],
  ['非法边界（NaN / Infinity）不倒向锁死', !isEpisodeLocked(ep(102), NaN)],
  ['边界大于集数时等于不锁', !isEpisodeLocked(ep(80), 999)],

  // ---- 角标文案 ----
  ['边界小于集数时以边界为准，站方文案被覆盖',
    episodesBadgeText({ episodesCount: 102, playableEpisodes: 5, rightText: '全102集' })
      === '开放前 5 集 · 共 102 集'],
  ['边界与集数相等时退回站方文案',
    episodesBadgeText({ episodesCount: 12, playableEpisodes: 12, rightText: '更新至12集' })
      === '更新至12集'],
  ['无边界时用站方文案',
    episodesBadgeText({ episodesCount: 12, rightText: '更新至12集' }) === '更新至12集'],
  ['无边界也无站方文案时退回集数',
    episodesBadgeText({ episodesCount: 24 }) === '共 24 集全'],
  ['动漫单集形态优先',
    episodesBadgeText({ episodesCount: 0, animeSingle: true }) === '单集/已完结'],
  ['动漫单集形态不受边界影响',
    episodesBadgeText({ episodesCount: 0, playableEpisodes: 0, animeSingle: true })
      === '单集/已完结'],
];

let failed = 0;
for (const [label, ok] of checks) {
  if (ok) {
    console.log(`PASS ${label}`);
  } else {
    failed += 1;
    console.error(`FAIL ${label}`);
  }
}
console.log(`\n${checks.length - failed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);