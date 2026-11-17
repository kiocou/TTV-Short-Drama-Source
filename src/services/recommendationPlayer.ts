export function nextQueueIndex<T extends { id: string }>(
  queue: readonly T[],
  currentId: string | null,
  step: number,
): number {
  if (queue.length === 0) return -1;
  const current = queue.findIndex(item => item.id === currentId);
  if (current < 0) return 0;
  return Math.max(0, Math.min(queue.length - 1, current + Math.sign(step)));
}

export function getVideoAspectRatio(width: number, height: number): number | null {
  if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) return null;
  return width / height;
}

export function fitVideoSize(width: number, height: number, ratio: number): { width: number; height: number } | null {
  if (![width, height, ratio].every(Number.isFinite) || width <= 0 || height <= 0 || ratio <= 0) return null;
  const fittedWidth = Math.min(width, height * ratio);
  return { width: fittedWidth, height: fittedWidth / ratio };
}
