import type { ChannelType } from '../types/catalog';

/**
 * App API 的**内容类型**口径（`content_type` / `contentType`）。
 *
 * ## 为什么要有这个函数
 *
 * 这个值在项目里散在 17 处，原先每一处都是一份独立的三元：
 * `x === 'comic' ? 1004 : 1`。加 AI 剧频道时，这 17 处**一处都没改**——
 * 因为三元天然有 `else`，AI 剧静默落进 `1`（真人剧口径），编译器不会提醒。
 * 同一语义在多处各写一份，加第三个值的那天必然漏改；收拢成一个函数之后，
 * 下次只需要改这里。
 *
 * ## AI 剧当前落成 1，是有依据的现状而不是猜的
 *
 * 这个值只对 **worker 那条路**有意义：
 *
 * ```python
 * # worker.py::fetch_stream
 * payload = {"biz_param": dict(BIZ_PARAM),
 *            "mixed_video_id_map": {str(content_type): [vid]}}
 * if content_type != 1:
 *     path = VIDEO_MODEL_V2_PATH          # 漫剧走 V2 端点
 * ```
 *
 * 而**原生链路根本不发这个字段**（`client.rs::resolve_playback_once` 发的是
 * `video_id` + `biz_param`），播放又是原生优先、worker 才兜底
 * （`short_drama_app.rs::fetch_stream_payload_with_fallback`）。
 *
 * 所以 AI 剧现在能正常播（台架实测：5 路码流含 1080p、整集 169s、密钥解得开），
 * 这个 `1` 只在"原生解析失败退 worker"那一刻才会被用到——那时它会带错口径。
 *
 * ## 真要修的那天
 *
 * 先用 worker 实测 AI 剧 vid 的真实 content_type（起一次 worker 约 2 秒以上、
 * 还要吃设备配额），再改这里的 `ai` 分支。**不要**在原处改三元——
 * 那样 17 处又会重新散开。
 */
export function contentTypeFor(channel: ChannelType | string | null | undefined): 1 | 1004 {
  return channel === 'comic' ? 1004 : 1;
}