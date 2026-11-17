import { getVideoAspectRatio, nextQueueIndex } from '../src/services/recommendationPlayer.ts';

let pass = 0;
let fail = 0;

function check(name, actual, expected) {
  const ok = Object.is(actual, expected);
  if (ok) pass += 1;
  else fail += 1;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ` expected=${expected} actual=${actual}`}`);
}

// queue 的真实契约是 `{ id: string }[]`（见 recommendationPlayer.ts 的签名与详情页
// 「猜你喜欢」的调用点），所以用例必须传对象，裸字符串会走 findIndex=-1 的分支。
const Q = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];
check('next recommendation advances one item', nextQueueIndex(Q, 'a', 1), 1);
check('previous recommendation moves one item', nextQueueIndex(Q, 'c', -1), 1);
check('single-item recommendation stays selected', nextQueueIndex([{ id: 'a' }], 'a', 1), 0);
check('missing current item starts at first item', nextQueueIndex([{ id: 'a' }, { id: 'b' }], 'missing', 1), 0);
check('portrait media ratio is preserved', getVideoAspectRatio(360, 640), 360 / 640);
check('landscape media ratio is preserved', getVideoAspectRatio(1920, 1080), 16 / 9);
check('zero-size media has no ratio', getVideoAspectRatio(0, 1080), null);
check('non-finite media has no ratio', getVideoAspectRatio(Infinity, 720), null);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail === 0 ? 0 : 1);
