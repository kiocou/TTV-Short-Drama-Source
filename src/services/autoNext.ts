/**
 * 「自动连播」的有效值。
 *
 * 用户设置 `settings.autoNext` 是普通播放器的开关：关掉之后，一集播完就停在
 * 结束帧，这是用户对「认真看一部剧」的预期。
 *
 * 推荐页（`feed`）是「刷着看」的形态：用户明确要求"不操作时继续本剧第 2、3 集"，
 * 停留在首集等手动点下一集不算满足。所以这一页**强制**有效连播，且：
 *
 * - 只影响判定，**不写回** `settings.autoNext`（用户关掉的是他自己的偏好，
 *   不该因为进了一次推荐页被改掉）；
 * - 只在 **本剧内** 生效：末集播完仍走 `finishedAll → FeedCompletionBridge`，
 *   停留成完成态，不换季也不换部。
 *
 * 抽成纯函数是为了让 `scripts/verify-feed-series-nav.mjs` 能直接跑边界，
 * 不必开窗口；`usePlaybackStore` 只把它接到事件上下文上。
 */
export function effectiveAutoNext(autoNext: boolean | undefined, currentView: string): boolean {
  if (currentView === 'feed') return true;
  return autoNext !== false;
}