/**
 * 用户在设置页选择的播放内核。
 *
 * - `webview`：应用内 HTML 播放器（短剧/漫剧走 VideoSurface，动漫走 AnimeVideoSurface）
 * - `mpv`：独立原生窗口（mpv 零拷贝硬解 + Clean-Room GPU 光流补帧引擎）
 *
 * 缺字段 / 非法值一律回落到 `webview`，避免旧设置记录升级后突然弹出外部窗口。
 */
export type PreferredPlayer = 'webview' | 'mpv';

/** 补帧内容自适应模式（auto = 按剧集类型自动选择）。 */
export type NativeFgMode = 'auto' | 'drama' | 'comic' | 'anime';

/** 补帧开关预设按内容类型区分；倍率由用户偏好或原生播放器当前状态决定。 */
export interface InterpolationPreset {
  /** 是否打开 Clean-Room GPU 光流插帧。 */
  fg: boolean;
}

const NATIVE_FG_MULT_STORAGE_KEY = 'ttv_native_fg_mult';
const NATIVE_FG_MODE_STORAGE_KEY = 'ttv_native_fg_mode';

/** 规范化补帧倍率：0（自动匹配屏幕原生刷新率）或 2..5。 */
export function normalizeNativeFgMult(value: unknown): number | undefined {
  const raw = typeof value === 'string' ? value.trim() : value;
  if (raw === '' || raw === null || raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isInteger(n)) return undefined;
  if (n === 0 || (n >= 2 && n <= 5)) return n;
  return undefined;
}

/** 规范化补帧内容模式。 */
export function normalizeNativeFgMode(value: unknown): NativeFgMode | undefined {
  if (typeof value !== 'string') return undefined;
  const norm = value.trim().toLowerCase();
  if (norm === 'auto' || norm === 'drama' || norm === 'comic' || norm === 'anime') {
    return norm;
  }
  return undefined;
}

/** 读取用户持久化的原生补帧倍率偏好；未设置或非法时返回 undefined，不覆盖播放器状态。 */
export function readSavedNativeFgMult(): number | undefined {
  try {
    if (typeof localStorage === 'undefined' || typeof localStorage.getItem !== 'function') {
      return undefined;
    }
    return normalizeNativeFgMult(localStorage.getItem(NATIVE_FG_MULT_STORAGE_KEY));
  } catch {
    return undefined;
  }
}

/** 读取用户持久化的原生补帧内容模式偏好；未设置或非法时返回 undefined。 */
export function readSavedNativeFgMode(): NativeFgMode | undefined {
  try {
    if (typeof localStorage === 'undefined' || typeof localStorage.getItem !== 'function') {
      return undefined;
    }
    return normalizeNativeFgMode(localStorage.getItem(NATIVE_FG_MODE_STORAGE_KEY));
  } catch {
    return undefined;
  }
}

/** 将用户在原生播放器内选定的倍率与内容模式同步持久化到 localStorage。 */
export function saveNativeFgPreferences(fgMult?: unknown, fgMode?: unknown): void {
  try {
    if (typeof localStorage === 'undefined' || typeof localStorage.setItem !== 'function') return;
    const normMult = normalizeNativeFgMult(fgMult);
    if (normMult !== undefined) {
      localStorage.setItem(NATIVE_FG_MULT_STORAGE_KEY, String(normMult));
    }
    const normMode = normalizeNativeFgMode(fgMode);
    if (normMode !== undefined) {
      localStorage.setItem(NATIVE_FG_MODE_STORAGE_KEY, normMode);
    }
  } catch {
    /* localStorage 不可用时静默忽略 */
  }
}

export function normalizePreferredPlayer(value: unknown): PreferredPlayer {
  return value === 'mpv' ? 'mpv' : 'webview';
}

/** 红果剧 id 是纯数字；其它来源即使分集为数字，也不能交给红果解析。 */
export function isHongguoSeriesId(seriesId: string): boolean {
  return /^\d{4,25}$/.test(seriesId.trim());
}

/**
 * 从分集 id 里取出红果 vid。
 *
 * 详情解析把 `episode.id` 写成源站 vid（纯数字）。历史上也出现过带前缀的写法，
 * 这里取出最长的一段不少于 6 位的数字，避免后端 `native_player_open` 因
 * 「vid 不是纯数字」直接拒绝，表现就是点了播放却没有任何窗口弹出。
 */
export function extractHongguoVid(episodeId: string | undefined | null, seriesId?: string): string | null {
  if (seriesId !== undefined && !isHongguoSeriesId(seriesId)) return null;
  const raw = (episodeId ?? '').trim();
  if (!raw) return null;
  if (/^\d+$/.test(raw)) return raw;
  let best = '';
  let current = '';
  for (const ch of raw) {
    if (ch >= '0' && ch <= '9') {
      current += ch;
      if (current.length > best.length) best = current;
    } else {
      current = '';
    }
  }
  return best.length >= 6 ? best : null;
}

/**
 * 按内容类型给出补帧预设。
 *
 * - 短剧 / AI 剧 / 其它真人：默认打开 GPU 光流插帧（字幕保护 + 切镜保护）。
 * - 漫剧：默认打开，但 GPU 管线启用分镜零形变保护（Panel-Safe Bypass）：静帧位精确直通、翻页瞬间硬切、平移平滑。
 * - 动漫：默认打开，启用 3 槽独特帧去重环形缓存解包一拍二/一拍三，具体档位（自动或 2X–5X）由用户决定。
 */
export function interpolationPresetFor(kind: string | undefined | null): InterpolationPreset {
  switch (kind) {
    case 'comic':
      return { fg: true };
    case 'anime':
      return { fg: true };
    default:
      return { fg: true };
  }
}

export function inferContentKind(input: {
  type?: string | null;
  channel?: string | null;
  seriesId?: string | null;
}): string {
  const seriesId = input.seriesId ?? '';
  if (input.type === 'anime' || input.channel === 'anime' || seriesId.startsWith('dmghg:') || seriesId.startsWith('bfzy:')) {
    return 'anime';
  }
  if (input.type === 'comic' || input.channel === 'comic') return 'comic';
  if (input.type === 'ai' || input.channel === 'ai') return 'ai';
  if (input.type === 'adult' || input.channel === 'adult') return 'adult';
  return 'drama';
}
