import {
  classifyHistoryModule,
  distinctHistoryByKey,
  historyKey,
  historyRegionLabel,
  isAnimeHistory,
  isHuangguoHistory,
  matchesHistoryClearScope,
} from '../src/services/historyTypes.ts';

let pass = 0;
let fail = 0;

function check(name, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (ok) pass += 1;
  else fail += 1;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ` expected=${JSON.stringify(expected)} actual=${JSON.stringify(actual)}`}`);
}

check('legacy records map to normal viewing history', historyKey({ seriesId: 's', historyType: undefined }), 'watch:s');
check('same series in two sections has separate keys', historyKey({ seriesId: 's', historyType: 'recommendation' }), 'recommendation:s');
check('dedupe keeps same series from different sections', distinctHistoryByKey([
  { seriesId: 's', historyType: 'watch' },
  { seriesId: 's', historyType: 'recommendation' },
  { seriesId: 's', historyType: 'featured' },
  { seriesId: 's', historyType: 'watch' },
]), ['watch:s', 'recommendation:s', 'featured:s']);

check('huangguo detected by prefix or channel', [
  isHuangguoHistory({ seriesId: 'hg:101', channel: undefined }),
  isHuangguoHistory({ seriesId: 'other', channel: 'huangguo' }),
  isHuangguoHistory({ seriesId: '7701', channel: 'drama' }),
], [true, true, false]);

check('anime detected without leaking huangguo', [
  isAnimeHistory({ seriesId: 'dmghg:1', channel: undefined }),
  isAnimeHistory({ seriesId: 'bfzy:2', channel: undefined }),
  isAnimeHistory({ seriesId: '123', channel: 'anime' }),
  isAnimeHistory({ seriesId: 'hg:1', channel: 'huangguo' }),
], [true, true, true, false]);

check('module classification distinguishes all four modules', [
  classifyHistoryModule({ seriesId: '7701', channel: 'drama', historyType: 'featured' }),
  classifyHistoryModule({ seriesId: '7701', channel: 'comic', historyType: 'recommendation' }),
  classifyHistoryModule({ seriesId: 'dmghg:1', channel: 'anime', historyType: 'featured' }),
  classifyHistoryModule({ seriesId: 'hg:1', channel: 'huangguo', historyType: 'featured' }),
], ['featured', 'recommendation', 'anime', 'huangguo']);

check('region labels distinguish module and area', [
  historyRegionLabel({ seriesId: '7701', channel: 'drama', historyType: 'featured' }),
  historyRegionLabel({ seriesId: '7702', channel: 'comic', historyType: 'featured' }),
  historyRegionLabel({ seriesId: '7703', channel: 'ai', historyType: 'featured' }),
  historyRegionLabel({ seriesId: '7704', channel: 'comic', historyType: 'recommendation' }),
  historyRegionLabel({ seriesId: 'dmghg:9', channel: 'anime', historyType: 'featured' }),
  historyRegionLabel({ seriesId: 'hg:9', channel: 'huangguo', historyType: 'featured' }),
], [
  '短剧精选 · 短剧专区',
  '短剧精选 · 漫剧次元',
  '短剧精选 · AI 剧',
  '短剧推荐 · 漫剧',
  '动漫专区',
  '黄果专区',
]);

check('clear scope isolates huangguo vs main history', [
  matchesHistoryClearScope({ seriesId: 'hg:1', channel: 'huangguo', historyType: 'featured' }, 'non_huangguo'),
  matchesHistoryClearScope({ seriesId: 'hg:1', channel: 'huangguo', historyType: 'featured' }, 'huangguo'),
  matchesHistoryClearScope({ seriesId: '7701', channel: 'drama', historyType: 'recommendation' }, 'recommendation'),
  matchesHistoryClearScope({ seriesId: '7701', channel: 'drama', historyType: 'recommendation' }, 'featured'),
], [false, true, true, false]);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail === 0 ? 0 : 1);
