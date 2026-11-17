import {
  extractHongguoVid,
  interpolationPresetFor,
  inferContentKind,
  normalizePreferredPlayer,
} from '../src/services/preferredPlaybackLogic.ts';

const cases = [
  ['1234567890', '1234567890'],
  [' 987654321 ', '987654321'],
  ['prefix-751234567890', '751234567890'],
  ['abc', null],
  ['12', '12'], // already all digits
  ['id:12', null],
];

let failed = 0;
for (const [input, expected] of cases) {
  const got = extractHongguoVid(input);
  if (got !== expected) {
    console.error(`extractHongguoVid(${JSON.stringify(input)}) => ${JSON.stringify(got)}, expected ${JSON.stringify(expected)}`);
    failed += 1;
  }
}

const presets = {
  comic: interpolationPresetFor('comic'),
  anime: interpolationPresetFor('anime'),
  drama: interpolationPresetFor('drama'),
  ai: interpolationPresetFor('ai'),
};
if (presets.comic.fg !== true) {
  console.error('comic preset (Panel-Safe clean-room bypass)', presets.comic);
  failed += 1;
}
if (presets.anime.fg !== true) {
  console.error('anime preset', presets.anime);
  failed += 1;
}
if (presets.drama.fg !== true) {
  console.error('drama preset', presets.drama);
  failed += 1;
}
if (presets.ai.fg !== true) {
  console.error('ai preset', presets.ai);
  failed += 1;
}
for (const [kind, preset] of Object.entries(presets)) {
  if (Object.hasOwn(preset, 'fgMult')) {
    console.error(`${kind} 内容预设不得覆盖播放器实际倍率`, preset);
    failed += 1;
  }
}

if (inferContentKind({ seriesId: 'dmghg:1' }) !== 'anime') {
  console.error('infer anime from prefix failed');
  failed += 1;
}
if (inferContentKind({ channel: 'comic' }) !== 'comic') {
  console.error('infer comic failed');
  failed += 1;
}
if (normalizePreferredPlayer(undefined) !== 'webview' || normalizePreferredPlayer('mpv') !== 'mpv') {
  console.error('normalizePreferredPlayer failed');
  failed += 1;
}

if (failed) {
  console.error(`verify-preferred-player: ${failed} failed`);
  process.exit(1);
}
console.log('verify-preferred-player: ok');
