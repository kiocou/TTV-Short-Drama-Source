import {
  dedupeFeedItems,
  detailChannelForFeed,
  discoverChannelForFeed,
  feedDetailFromHongguo,
  feedGestureDirection,
  latestHistoryForFeed,
  mockFeedItems,
  nextPlayableSeason,
  pickResumeEpisode,
  seasonNumberFromTitle,
} from '../src/services/feedSource.ts';

const drama = { id: 'a', title: 'A', cover: '', type: 'drama', episodesCount: 2, tags: [], origin: '' };
const duplicate = { ...drama };
const comic = { ...drama, id: 'b', type: 'comic' };
const episodes = [
  { id: 'a-1', episodeNumber: 1, title: '第1集', durationSeconds: 10 },
  { id: 'a-2', episodeNumber: 2, title: '第2集', durationSeconds: 10 },
];
/** 三集版：用来验证「历史超出可播边界」这一类，两集装不下。 */
const threeEpisodes = [
  ...episodes,
  { id: 'a-3', episodeNumber: 3, title: '第3集', durationSeconds: 10 },
];

const checks = [
  ['drama maps to isolated live channel', discoverChannelForFeed('drama') === 'live'],
  ['comic maps to comic', discoverChannelForFeed('comic') === 'comic'],
  ['detail uses series type rather than discover tab', detailChannelForFeed('drama') === 'drama'
    && detailChannelForFeed('comic') === 'comic'],
  ['mock feed stays in its selected channel', mockFeedItems('comic').every(item => item.type === 'comic')
    && mockFeedItems('drama').every(item => item.type === 'drama')],
  ['queue dedupes and filters', dedupeFeedItems([drama, duplicate, comic], 'drama').length === 1],
  ['unfinished history resumes exact episode', pickResumeEpisode(episodes, {
    seriesId: 'a', episodeId: 'a-2', title: 'A', seriesCover: '', episodeNumber: 2,
    totalEpisodes: 2, positionSeconds: 4, durationSeconds: 10, progressPercent: 40,
    updatedAt: 0, isFinished: false,
  }).index === 1],
  ['resume falls back to episode number when id changes', pickResumeEpisode(episodes, {
    seriesId: 'a', episodeId: 'old-a-2', title: 'A', seriesCover: '', episodeNumber: 2,
    totalEpisodes: 2, positionSeconds: 4, durationSeconds: 10, progressPercent: 40,
    updatedAt: 0, isFinished: false,
  }).index === 1],
  ['finished history advances', pickResumeEpisode(episodes, {
    seriesId: 'a', episodeId: 'a-1', title: 'A', seriesCover: '', episodeNumber: 1,
    totalEpisodes: 2, positionSeconds: 10, durationSeconds: 10, progressPercent: 100,
    updatedAt: 0, isFinished: true,
  }).index === 1],
  ['finished last episode reports finished (末集停留，不再续季)', pickResumeEpisode(episodes, {
    seriesId: 'a', episodeId: 'a-2', title: 'A', seriesCover: '', episodeNumber: 2,
    totalEpisodes: 2, positionSeconds: 10, durationSeconds: 10, progressPercent: 100,
    updatedAt: 0, isFinished: true,
  }).finished],
  // 可播边界：历史记着第 3 集，而源现在只开放 2 集时必须夹回边界内。
  // 不夹的话按老索引打开第 3 集在红果侧是静默失败（播放页 200 但无播放数据）。
  ['resume clamps into the playable window', (() => {
    const r = pickResumeEpisode(threeEpisodes, {
      seriesId: 'a', episodeId: 'a-3', title: 'A', seriesCover: '', episodeNumber: 3,
      totalEpisodes: 3, positionSeconds: 4, durationSeconds: 10, progressPercent: 40,
      updatedAt: 0, isFinished: false,
    }, 2);
    return r.index === 1 && r.position === 0 && r.clamped === true;
  })()],
  ['resume inside the window is untouched', (() => {
    const r = pickResumeEpisode(threeEpisodes, {
      seriesId: 'a', episodeId: 'a-2', title: 'A', seriesCover: '', episodeNumber: 2,
      totalEpisodes: 3, positionSeconds: 4, durationSeconds: 10, progressPercent: 40,
      updatedAt: 0, isFinished: false,
    }, 2);
    return r.index === 1 && r.position === 4 && r.clamped === false;
  })()],
  ['resume without a boundary behaves as before',
    pickResumeEpisode(threeEpisodes, {
      seriesId: 'a', episodeId: 'a-3', title: 'A', seriesCover: '', episodeNumber: 3,
      totalEpisodes: 3, positionSeconds: 4, durationSeconds: 10, progressPercent: 40,
      updatedAt: 0, isFinished: false,
    }).index === 2],
  ['boundary of zero still yields a playable index', (() => {
    const r = pickResumeEpisode(threeEpisodes, {
      seriesId: 'a', episodeId: 'a-2', title: 'A', seriesCover: '', episodeNumber: 2,
      totalEpisodes: 3, positionSeconds: 4, durationSeconds: 10, progressPercent: 40,
      updatedAt: 0, isFinished: false,
    }, 0);
    return r.index === 0 && r.clamped === true;
  })()],
  ['latest history excludes another channel', latestHistoryForFeed([
    { seriesId: 'a', episodeId: 'a-1', title: 'A', seriesCover: '', episodeNumber: 1, totalEpisodes: 2,
      positionSeconds: 4, durationSeconds: 10, progressPercent: 40, updatedAt: 10, isFinished: false, channel: 'drama' },
    { seriesId: 'b', episodeId: 'b-1', title: 'B', seriesCover: '', episodeNumber: 1, totalEpisodes: 2,
      positionSeconds: 4, durationSeconds: 10, progressPercent: 40, updatedAt: 20, isFinished: false, channel: 'comic' },
  ], 'drama')?.seriesId === 'a'],
  ['legacy history without channel is only a drama fallback', latestHistoryForFeed([
    { seriesId: 'legacy', episodeId: 'legacy-1', title: 'Legacy', seriesCover: '', episodeNumber: 1,
      totalEpisodes: 2, positionSeconds: 4, durationSeconds: 10, progressPercent: 40, updatedAt: 10, isFinished: false },
    { seriesId: 'anime_1', episodeId: 'anime-1', title: 'Anime', seriesCover: '', episodeNumber: 1,
      totalEpisodes: 2, positionSeconds: 4, durationSeconds: 10, progressPercent: 40, updatedAt: 20, isFinished: false },
  ], 'drama')?.seriesId === 'legacy'
    && latestHistoryForFeed([
      { seriesId: 'legacy', episodeId: 'legacy-1', title: 'Legacy', seriesCover: '', episodeNumber: 1,
        totalEpisodes: 2, positionSeconds: 4, durationSeconds: 10, progressPercent: 40, updatedAt: 10, isFinished: false },
    ], 'comic') === undefined],
  ['season parser handles Chinese labels', seasonNumberFromTitle('示例第三十五期') === 35],
  ['next playable season skips unavailable entries', nextPlayableSeason([
    { seriesId: 'a', title: '示例第1季', cover: '', seasonIndex: 1, episodeCount: 10, disablePlay: false },
    { seriesId: 'b', title: '示例第2季', cover: '', seasonIndex: 2, episodeCount: 0, disablePlay: true },
    { seriesId: 'c', title: '示例第3季', cover: '', seasonIndex: 3, episodeCount: 8, disablePlay: false },
  ], 1)?.seriesId === 'c'],
  ['vertical gesture advances and retreats', feedGestureDirection(0, -80) === 'next'
    && feedGestureDirection(0, 80) === 'previous'],
  ['diagonal and short gestures are ignored', feedGestureDirection(24, -20) === null
    && feedGestureDirection(0, -20) === null],
  ['Hongguo detail maps playable episode fields', (() => {
    const result = feedDetailFromHongguo({
      series: drama,
      status: 1,
      payType: 0,
      ageGate: 0,
      isMultiSeason: true,
      rightText: '',
      playCount: 0,
      followCount: 0,
      diggCount: 0,
      createdAt: 0,
      episodes: [{ id: 'a-1', seriesId: 'a', number: 1, title: '第1集', durationSeconds: 10, cover: '' }],
    });
    return result.id === 'a' && result.episodes[0].episodeNumber === 1
      && result.episodes[0].seriesId === 'a' && result.episodes[0].durationSeconds === 10;
  })()],
];

const failed = checks.filter(([, passed]) => !passed);
for (const [name, passed] of checks) console.log(`${passed ? 'PASS' : 'FAIL'} ${name}`);
if (failed.length) process.exit(1);
console.log(`${checks.length} passed, 0 failed`);
