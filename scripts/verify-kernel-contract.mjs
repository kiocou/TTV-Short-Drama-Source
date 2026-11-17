// 播放内核契约的确定性验证（docs/C-播放内核适配面盘点.md §3 / §4 阶段 0-1）。
//
// 为什么单独跑：内核契约的关键不是"能编译"，而是**映射语义对**。
// `<video>` 的 buffered / readyState 换成语义等价物时最容易悄悄改掉判定阈值，
// 而那种错在真机上是"起播明明成功却反复重载"或"卡死被判成正常"。
// 这里用伪 video 对象把每条阈值钉死，不需要浏览器、不需要 Tauri。
//
// 运行：npm run verify:kernel-contract

import {
  bufferedEndSeconds,
  failureFromMediaError,
  phaseFromVideo,
  seekableEndSeconds,
  snapshotFromVideo,
} from '../src/services/playbackKernel.ts';

let pass = 0;
let fail = 0;

function check(name, actual, expected) {
  const ok = Object.is(actual, expected);
  if (ok) pass += 1;
  else fail += 1;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ` expected=${expected} actual=${actual}`}`);
}

/** 造一个伪 video：默认是"正在正常播放"的状态。 */
function makeVideo(overrides = {}) {
  const buffered = overrides.buffered ?? {
    length: 1,
    end: () => 42,
  };
  const seekable = overrides.seekable ?? {
    length: 1,
    end: () => 100,
  };
  return {
    currentTime: 10,
    duration: 100,
    paused: false,
    playbackRate: 1,
    volume: 0.8,
    muted: false,
    readyState: 4,
    videoWidth: 1080,
    videoHeight: 1920,
    error: null,
    ...overrides,
    buffered,
    seekable,
  };
}

// ── 缓冲/可跳转上界 ──────────────────────────────────────────────
check('buffered 末端取最后一段', bufferedEndSeconds(makeVideo()), 42);
check(
  'buffered 多段时取最后一段',
  bufferedEndSeconds(
    makeVideo({
      buffered: { length: 3, end: (i) => [5, 20, 33][i] ?? -1 },
    }),
  ),
  33,
);
check('buffered 为空返回 -1', bufferedEndSeconds(makeVideo({ buffered: { length: 0, end: () => 0 } })), -1);
check(
  'buffered 抛错时兜底 -1（未就绪元素）',
  bufferedEndSeconds(
    makeVideo({
      buffered: {
        length: 1,
        end: () => {
          throw new Error('InvalidStateError');
        },
      },
    }),
  ),
  -1,
);
check('null video 的 buffered 为 -1', bufferedEndSeconds(null), -1);
check('seekable 末端取最后一段', seekableEndSeconds(makeVideo()), 100);
check('seekable 为空返回 -1', seekableEndSeconds(makeVideo({ seekable: { length: 0, end: () => 0 } })), -1);

// ── 阶段判定：阈值必须与原 store 一致 ────────────────────────────
check('null → idle', phaseFromVideo(null), 'idle');
check('有 error → failed', phaseFromVideo(makeVideo({ error: { code: 3 } })), 'failed');
check('readyState 0 → loading', phaseFromVideo(makeVideo({ readyState: 0 })), 'loading');
check('readyState 1 → loading', phaseFromVideo(makeVideo({ readyState: 1 })), 'loading');
check('readyState 2 → buffering', phaseFromVideo(makeVideo({ readyState: 2 })), 'buffering');
check('readyState 3 缓冲充足 → ready', phaseFromVideo(makeVideo({ readyState: 3 })), 'ready');
check('readyState 4 缓冲充足 → ready', phaseFromVideo(makeVideo({ readyState: 4 })), 'ready');
check(
  'readyState 4 但缓冲末端落后播放位置 → buffering',
  phaseFromVideo(
    makeVideo({
      readyState: 4,
      currentTime: 50,
      buffered: { length: 1, end: () => 20 },
    }),
  ),
  'buffering',
);
check(
  'readyState 4 缓冲刚好追上（差值 <0.25s）→ ready',
  phaseFromVideo(
    makeVideo({
      readyState: 4,
      currentTime: 50,
      buffered: { length: 1, end: () => 49.9 },
    }),
  ),
  'ready',
);

// ── 失败分类：三类必须分开（不能混成一句"播放失败"）──────────────
check('无 error → none', failureFromMediaError(null), 'none');
check('MediaError 2 → network', failureFromMediaError({ code: 2 }), 'network');
check('MediaError 3 → decode', failureFromMediaError({ code: 3 }), 'decode');
check('MediaError 4 → source', failureFromMediaError({ code: 4 }), 'source');
check('未知 code 归入 decode', failureFromMediaError({ code: 99 }), 'decode');

// ── 快照：字段映射 ─────────────────────────────────────────────
const snap = snapshotFromVideo(makeVideo());
check('snapshot.position', snap.position, 10);
check('snapshot.duration', snap.duration, 100);
check('snapshot.paused', snap.paused, false);
check('snapshot.rate', snap.rate, 1);
check('snapshot.volume', snap.volume, 0.8);
check('snapshot.muted', snap.muted, false);
check('snapshot.cachedUntil', snap.cachedUntil, 42);
check('snapshot.seekableUntil', snap.seekableUntil, 100);
check('snapshot.phase', snap.phase, 'ready');
check('snapshot.failure', snap.failure, 'none');
check('snapshot.video.width', snap.video?.width, 1080);
check('snapshot.video.height', snap.video?.height, 1920);

const emptySnap = snapshotFromVideo(null);
check('无 video 时 phase=idle', emptySnap.phase, 'idle');
check('无 video 时 cachedUntil=-1', emptySnap.cachedUntil, -1);
check('无 video 时 video=null', emptySnap.video, null);
check('无 video 时 paused=true', emptySnap.paused, true);

// 竖屏短剧 + 非有限值时不能把 NaN 漏进 UI
const brokenSnap = snapshotFromVideo(
  makeVideo({ currentTime: NaN, duration: Infinity, playbackRate: 0, videoWidth: 0, videoHeight: 0 }),
);
check('NaN 位置归零', brokenSnap.position, 0);
check('Infinity 时长归零', brokenSnap.duration, 0);
check('非法倍率回落 1', brokenSnap.rate, 1);
check('零尺寸不产出 video', brokenSnap.video, null);

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) process.exit(1);
