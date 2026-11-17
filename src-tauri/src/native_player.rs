// native_player.rs —— 原生播放器（mpv + minterpolate 补帧）的进程管理与 IPC 客户端。
//
// 为什么要有这一个模块（而不是继续用 WebView2 播）：
//   * 补帧引擎捕获不到 WebView2（DXGI/WGC 全黑、GDI 崩溃，见 main.rs 的补帧注释）；
//   * 原生播放器直吃红果直链（CENC 解密 + d3d11va 硬解），整条 H.264 转码链路被消除。
//
// 协议（对应 src-tauri/native-player/IpcPipe.cs）：
//   主程序 → 播放器：一行一条 JSON 命令 {cmd:"load"|"pause"|"seek"|...}
//   播放器 → 主程序：{event:"progress"|"ended"|"closed"|"next"|"prev"|"error", ...}
//
// 联播扩展（docs/native-player-redesign.md §3）：
//   * 队列由**本模块**持有（进程内单例 `queue_slot`），播放器只从队列里取下一集；
//   * 主程序 → 播放器新增 `queue` / `queueClear` / `autoplay`，`load` 新增
//     `totalEps` / `index` / `autoplay` / `queue` 字段；
//   * 播放器 → 主程序新增 `advance` / `requestNext` / `autoplayChanged` / `tracks`。
//   * 这些事件里只有 `requestNext` 需要主程序"做事"。它按**两段**处理
//     （docs/native-player-redesign.md §5.4，第二轮 v3 修的就是这段）：
//       ① **Rust 自己解析**：按 index 在队列里找到这一项、取它的 `vid`，走
//          `hongguo_bridge::fetch_stream_payload` 拿直链；成功就**直接下发
//          `load`** 并 emit `native-player-advance`。这一段没有任何前端参与，
//          所以前端没挂桥（用户已经退出播放页）时联播照样能一集集走完 ——
//          改造前链路是 `C# → Rust emit → React 解析 → queuePush → C#`，
//          前端一掉线就断在中间，用户看到的是"第一集播完就停住"。
//       ② 只有"队列里没有这一项 / 没有可解析的 vid / 解析失败"才回落到前端：
//          emit `native-player-need-next` → 前端解析直链 →
//          `native_player_queue_push(item)` 推回来 → 本模块把 item 当 load 下发；
//          8 秒没有回执就 emit `native-player-queue-timeout`，不自己重试
//          （重试会在"前端根本没在解析"时无限空转，且用户看到的是一次都不报错的黑屏）。
//
//   ⚠️ 两条铁律（违反任何一条都会把联播变成"看起来在工作、实际卡住"）：
//     1. **事件读取线程里绝不 await**。它是同步阻塞读行的
//        （`while let Some(Ok(line)) = lines.next()`），await 一次就把后面排队的
//        `progress` 心跳一起按住 —— HUD 的位置/缓存/相位全部冻住，8 秒超时判定
//        也跟着漂。解析一律 `tauri::async_runtime::spawn` 出去。
//     2. **同一个 index 不重复发起解析**。红果对每台匿名设备只回一次播放模型
//        （AGENTS.md §4），两个并发请求会互相踢掉、两个都失败，而且白烧设备配额。
//        去重见 `pending_resolve` / `resolve_in_flight`。
//
//   管道名由主程序生成（\\.\pipe\<name>），播放器是服务端。
//
// 生命周期：同一时刻只保留一个播放器窗口。再次 open 时如果进程还活着，
// 只下发 load 换集；进程没了才重新 spawn。
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// 独立 MPV 播放器总闸（与前端 `services/nativePlayerRollout.ts` 对齐）。
///
/// 新版纯 Rust 播放器（`src-tauri/ttv-player/`）从零重构期间保持 `false`，
/// 统一拦截旧 C# NativePlayer 的启动、队列与命令下发；重构验收通过后再翻为 `true`。
pub const NATIVE_PLAYER_AVAILABLE: bool = true;
pub const NATIVE_PLAYER_GRAY_REASON: &str = "新版独立播放器重构中，暂不可用";

/// 灰度期总闸校验（给 Tauri 命令边界与启动入口统一调用）。
pub fn ensure_native_player_available() -> Result<(), String> {
    if NATIVE_PLAYER_AVAILABLE {
        Ok(())
    } else {
        Err(NATIVE_PLAYER_GRAY_REASON.to_string())
    }
}

/// 播放器回传的状态（命令层读取；前端也可以轮询）。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    pub alive: bool,
    pub position: f64,
    pub duration: f64,
    pub episode: u32,
    pub title: String,
    pub series_id: String,
    pub episode_id: String,

    // ── 阶段 0：内核状态字段（docs/C-播放内核适配面盘点.md §4）──
    // 迁移到统一播放内核之前，先让原生侧能回答"已缓存到哪、现在是什么阶段、
    // 链路是否判死"。这些都由播放器从 mpv 的真实属性推出并通过 progress 心跳
    // 回传，不是占位值；拿不到数据时如实为 -1 / "idle" / "none"。
    /// 已缓存到的秒数（对应 HTML 的 `buffered.end(last)`）；-1 = 尚无数据。
    pub cached_until: f64,
    /// idle / loading / ready / buffering / failed。
    pub phase: String,
    /// none / network / decode / source / autoplay。
    pub failure: String,
    /// 上一次会话的结局：`none`（还在跑 / 从未启动）/ `closed`（用户关窗）/
    /// `died`（进程没了）/ `replaced`（被新的 open 顶掉）/ `failed`（管道没连上）。
    ///
    /// 为什么要有它：`alive:false` 只说"窗口不在了"，而"用户主动关窗"与"进程被杀"
    /// 需要完全不同的处理 —— 前者不该把内嵌播放器重新播起来（用户可能就是想停下），
    /// 后者必须回退，否则两边都没画面、也没有任何提示。
    pub exit: String,
    pub video_width: i32,
    pub video_height: i32,
    /// 画质档位：源侧只有单一路径，如实为 "auto"。
    pub quality: String,
    /// 补帧**请求值**（0 = 自动匹配屏幕原生刷新率；2..=5 = 倍率档位）。
    pub fg_mult: u32,
    /// `out_fps / 源帧率` 的整数倍（实测派生）；只反映 Present 节奏，不证明有新帧。
    pub fg_effective_mult: u32,

    // ── 补帧能力契约（Clean-Room GPU 光流引擎）──
    /// 补帧引擎模式名：`off` / `gpu-optical-flow` / `gpu-panel-bypass` / `gpu-interp-inactive`。
    pub fg_engine: String,
    /// 用户选择的补帧内容模式：`auto` / `drama` / `comic` / `anime`。
    pub fg_mode: String,
    /// 当前实际生效的内容自适应档位：`drama` / `comic` / `anime`。
    pub fg_mode_effective: String,
    /// 显示同步 / Waitable SwapChain 是否已激活。
    pub display_sync: bool,
    /// 实测显示器刷新率（由播放器的 Present 节奏测出，不是驱动声明值）；0 = 尚未测到。
    pub display_hz: i32,
    /// Present 次数/秒（**不是**独特新帧数：重复上屏同一张图也会让它上涨）；0 = 尚未统计。
    pub out_fps: f64,
    /// Present 率 / 源帧率（两位小数，默认 1）。
    ///
    /// ⚠️ 它是**节奏比**，不是"新画面倍率"：`2.67` 只说明每秒上屏次数是源帧率的
    /// 2.67 倍，其中可能全是同一张源帧的重复呈现。判断画面里有没有新插值内容要看
    /// 帧内容与源帧率/输出节奏，这个数不构成证据。
    pub out_ratio: f64,
    /// 播放器当前是否暂停。
    ///
    /// 暂停期间 `out_ratio` 保留最后一次有效值，而 `out_fps` 可能只是维持性重画
    /// （keep-open 下仍会按节奏 Present 同一帧）—— 消费方**不要**据此报警。
    pub paused: bool,

    // ── 联播（docs/native-player-redesign.md §3）──
    // 这 7 个字段全是**新增**、Default 里有安全值：老前端读不到也不会坏，
    // 新前端按 `nativeStatus` 的类型拿到即可。语义上以播放器回传为准
    // （progress 心跳带 autoplay/index/queueLen），Rust 只在没有播放器时给默认值。
    /// 联播开关当前值。缺省 false = 改造前的行为（播完停住）。
    pub autoplay: bool,
    /// 当前这一集在联播队列里的下标（0 基；无队列时 0）。
    pub index: u32,
    /// 队列长度（Rust 侧持有的 pending 集数）。
    pub queue_len: u32,
    /// 音轨列表（`tracks` 事件回填，id=-1 表示"关闭"这一项不存在）。
    pub audio_tracks: Vec<TrackOption>,
    /// 字幕列表。
    pub sub_tracks: Vec<TrackOption>,
    /// 当前音轨 id，-1 = 关闭/未知。
    pub audio_id: i32,
    /// 当前字幕 id，-1 = 关闭/未知。
    pub sub_id: i32,
    /// 仅后端落库与 `history-updated` 广播使用的剧集元数据（不改动前端状态契约）。
    #[serde(skip)]
    pub series_cover: String,
    #[serde(skip)]
    pub total_episodes: u32,
    #[serde(skip)]
    pub channel: String,
    #[serde(skip)]
    pub history_type: String,
}

/// 音轨 / 字幕的一条选项。
///
/// `id` 与 mpv `track-list` 的 id 一致（-1 = 关闭），`label` 直接用播放器给的
/// 显示名。字段名不加 serde rename：`id`/`label` 在 camelCase 下本来就是它自己，
/// 前端 `NativePlayerTrack` 与之一一对应。
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TrackOption {
    pub id: i32,
    pub label: String,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            alive: false,
            position: 0.0,
            duration: 0.0,
            episode: 0,
            title: String::new(),
            series_id: String::new(),
            episode_id: String::new(),
            cached_until: -1.0,
            phase: "idle".into(),
            failure: "none".into(),
            exit: "none".into(),
            video_width: 0,
            video_height: 0,
            quality: "auto".into(),
            fg_mult: 0,
            fg_effective_mult: 1,
            // 补帧能力：缺省即"没开补帧"，老播放器不报这些字段时也不会显示成在跑。
            fg_engine: "off".into(),
            fg_mode: "auto".into(),
            fg_mode_effective: "drama".into(),
            display_sync: false,
            display_hz: 0,
            out_fps: 0.0,
            // 比率默认 1（= 与源同节奏），暂停默认否：旧播放器不报这两项时不会显示成异常。
            out_ratio: 1.0,
            paused: false,
            // 联播默认值：关（= 改造前行为），无队列，无轨道信息。
            autoplay: false,
            index: 0,
            queue_len: 0,
            audio_tracks: Vec::new(),
            sub_tracks: Vec::new(),
            audio_id: -1,
            sub_id: -1,
            series_cover: String::new(),
            total_episodes: 0,
            channel: String::new(),
            history_type: String::new(),
        }
    }
}

/// 事件线程与命令层共享的退出标记。
///
/// 为什么两样东西合在一个结构里：`closed` 决定"还活着吗"，`exit` 决定"为什么没了"。
/// 只有前者，前端就只能知道"窗口不在了"，于是**用户主动点 X** 与**进程被杀**
/// 无法区分 —— 前者不该自动把内嵌播放器重新播起来（用户可能就是想停下来），
/// 后者必须回退，否则两边都没画面。
#[derive(Default)]
struct SessionFlags {
    closed: AtomicBool,
    /// `closed` 的成因：`closed`（用户关窗）/ `died`（进程没了）/ `failed`（管道没连上）。
    exit: Mutex<Option<String>>,
}

impl SessionFlags {
    fn mark(&self, kind: &str) {
        if let Ok(mut guard) = self.exit.lock() {
            if guard.is_none() {
                *guard = Some(kind.to_owned());
            }
        }
        self.closed.store(true, Ordering::Release);
    }

    fn kind(&self) -> Option<String> {
        self.exit.lock().ok().and_then(|g| g.clone())
    }
}

struct Session {
    /// 会话序号。每次 spawn 递增，用来判断"这条事件线程还是不是当前会话的"。
    ///
    /// 为什么需要：`closed` / `advance` / `requestNext` 三个分支会改**全局**的队列
    /// 状态。旧播放器被替换后，它那条事件线程可能还活着几百毫秒（管道 EOF 有延迟），
    /// 期间一条迟到的 `advance` 会把新会话的队列游标推走 —— 表现是"换剧后第一集
    /// 播完直接跳到别的剧的下一集"。有了序号，旧线程一发现自己的会话已不是当前
    /// 会话就退出，一个全局状态都不碰。
    id: u64,
    child: Child,
    /// 管道写端：事件线程连上后填充（命令层通过它下发控制）。
    stdin: Arc<Mutex<Option<File>>>,
    state: Arc<Mutex<PlayerState>>,
    pipe: String,
    flags: Arc<SessionFlags>,
}

impl Session {
    /// 这条会话此刻是否还活着。
    ///
    /// 三条都要满足：事件线程没报 `closed`、子进程还在、没有被新的 spawn 顶掉。
    /// **绝不能无条件返回 true** —— 历史实现的 `status()` 就是无条件置 true，
    /// 于是播放器被杀之后前端一直以为它在播，既不回退也不报错。
    fn alive_now(&mut self) -> bool {
        if self.flags.closed.load(Ordering::Acquire) {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            // 已退出（拿到退出码）或查询失败：都不算活。
            Ok(Some(_)) | Err(_) => false,
        }
    }
}

fn slot() -> &'static Mutex<Option<Session>> {
    static S: OnceLock<Mutex<Option<Session>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn open_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &GATE
}

/// 会话序号发生器。0 保留给"没有会话"，所以从 1 开始。
fn next_session_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 这条会话还是不是槽位里的当前会话。
///
/// 事件线程用它做两件事的闸门：
///   * 旧线程不再改任何**全局**状态（队列游标、pending 回执）；
///   * 也不再把进度写回自己的 state —— 那一份已经不是前端在看的了。
fn is_current_session(id: u64) -> bool {
    slot().lock().ok().and_then(|g| g.as_ref().map(|s| s.id)) == Some(id)
}

/// 最后一次会话的快照（播到哪、哪一集、标题…）+ 它属于哪个会话号。
///
/// 为什么留着：窗口关掉之后 `native_player_status` 仍然要能回答"刚才播到哪一集"，
/// 否则前端在退出那一刻拿到的全是默认值（位置 0、集号 0），落历史会写成"看了 0 秒"。
/// 只用于**只读历史**，`alive` 永远是 false。
///
/// ## 为什么 id 与快照放在同一个锁里（2026-11-10 第三轮）
///
/// 旧实现是 `AtomicU64.fetch_max(id)` 判通过、**再**单独加锁写快照 —— 检查与写之间
/// 有窗口：旧会话判过之后被调度器挂起，新会话写完快照，旧会话醒来照样覆盖。
/// 用户已经切到第 5 集，历史却被写回第 2 集。现在比较与写在同一把锁内完成。
fn last_state() -> &'static Mutex<Option<(u64, PlayerState)>> {
    static LAST: OnceLock<Mutex<Option<(u64, PlayerState)>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// 只有"不比已记录的旧"的会话才允许覆盖历史快照。
fn record_last_if_newer(session_id: u64, snapshot: &PlayerState) {
    if let Ok(mut guard) = last_state().lock() {
        let newer = guard.as_ref().is_none_or(|(id, _)| session_id >= *id);
        if newer {
            *guard = Some((session_id, snapshot.clone()));
        }
    }
}

/// 取历史快照（不带会话号，只读）。
fn last_snapshot() -> Option<PlayerState> {
    last_state()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|(_, st)| st.clone()))
}

/// `progress` 里补帧能力字段的归一化（纯函数，便于逐条钉住边界）。
///
/// 三条规则，各自对应一种"看起来正常、实际是假数据"的形态：
///   1. **引擎名白名单**。播放器只报 `off` / `mpv-display-sync` /
///      `mpv-interp-inactive`；别把 `2X`/`3X` 这类档位名当引擎名搬进来 ——
///      那是请求值，不是任何版本里能直接执行的参数，搬进契约只会让 UI 假装"补帧生效"。
///   2. **拒非有限/负数**。`outFps`/`displayHz` 是播放器侧统计值，一条
///      `NaN`/`Infinity` 或负数就足以让界面显示 `-1fps`；范围上限 1000 是
///      "明显不可能是帧率/刷新率"的兜底。
///   3. **缺字段保持上一次的值**。老播放器不发这几个字段，把它们清零等于让界面
///      从"补帧在跑"掉回"已关闭"并周期性闪烁。
fn apply_fg_capability(st: &mut PlayerState, v: &Value) {
    if let Some(engine) = v.get("fgEngine").and_then(Value::as_str) {
        let normalized = engine.trim();
        if matches!(
            normalized,
            "off" | "gpu-optical-flow" | "gpu-panel-bypass" | "gpu-interp-inactive"
        ) {
            st.fg_engine = normalized.to_owned();
        }
    }
    if let Some(mode) = v.get("fgMode").and_then(Value::as_str) {
        let normalized = mode.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "auto" | "drama" | "comic" | "anime") {
            st.fg_mode = normalized;
        }
    }
    if let Some(eff) = v.get("fgModeEffective").and_then(Value::as_str) {
        let normalized = eff.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "drama" | "comic" | "anime") {
            st.fg_mode_effective = normalized;
        }
    }
    if let Some(sync) = v.get("displaySync").and_then(Value::as_bool) {
        st.display_sync = sync;
    }
    if let Some(hz) = v.get("displayHz").and_then(Value::as_f64) {
        if hz.is_finite() && (0.0..=1000.0).contains(&hz) {
            st.display_hz = hz.round() as i32;
        }
    }
    if let Some(fps) = v.get("outFps").and_then(Value::as_f64) {
        if fps.is_finite() && (0.0..=1000.0).contains(&fps) {
            st.out_fps = fps;
        }
    }
    // 节奏比与暂停：同样走"拒异常 + 缺字段保持"。暂停时保留上一次的有效比率。
    if let Some(ratio) = v.get("outRatio").and_then(Value::as_f64) {
        if ratio.is_finite() && (0.0..=1000.0).contains(&ratio) {
            st.out_ratio = ratio;
        }
    }
    if let Some(paused) = v.get("paused").and_then(Value::as_bool) {
        st.paused = paused;
    }
}

/// 一次 `load` 之后必须归零的"新鲜度"字段。
///
/// 复用窗口换集时最容易漏的就是这几个：`episode_id` 变了而 `phase` 还是上一集的
/// `ready`，前端"确认窗口可播"会当场误判成功（空窗口算成功，用户看到黑屏）。
/// 归零后由播放器的心跳按新加载回填，不需要新协议、也不需要加延时。
///
/// ## 已知边界（不要声称"ready 必然属于新集"）
///
/// 归零只能保证**不会把上一集遗留的 ready 直接当成新集的**。`progress` 心跳里没有
/// 加载代次，如果播放器在 `load` 真正生效之前又发了一次携带旧状态的 `progress`
/// （`hasFramePair` 仍是上一集留下的），这里就会被打回 `ready`。要彻底闭合需要
/// C# 侧在 `progress` 里带上加载代次（或 `loaded` 序号）并由前端比对；本轮不改 C#，
/// 所以只如实记录这条风险。
fn reset_state_for_load(st: &mut PlayerState) {
    st.phase = "loading".into();
    st.failure = "none".into();
    st.exit = "none".into();
    st.cached_until = -1.0;
    st.position = 0.0;
    st.video_width = 0;
    st.video_height = 0;
}

/// 把快照收进历史并返回它，同时给会话打上"已结束"。
///
/// `fallback` 是"没有事件说明成因"时的判定（`status()` 发现进程没了 = `died`，
/// `open()` 顶掉旧会话 = `replaced`）。
fn retire(mut session: Session, fallback: &str) {
    let kind = session.flags.kind().unwrap_or_else(|| fallback.to_owned());
    if let Ok(st) = session.state.lock() {
        let mut snapshot = st.clone();
        snapshot.alive = false;
        snapshot.exit = kind;
        // 比较与写在同一把锁内（见 `last_state` 的注释）。
        record_last_if_newer(session.id, &snapshot);
    }
    session.flags.mark(fallback);
    // 进程已经没了时 kill 是空操作；还活着（管道 EOF 但进程滞留）才真的收掉。
    let _ = session.child.kill();
    let _ = session.child.wait();
}

// ══════════════ 联播队列 ══════════════
//
// 为什么队列放在 Rust 进程内，而不是只交给播放器：
//   1. `advance` / `requestNext` 都由播放器产生，但"下一集是什么、能不能播"取决于
//      直链解析（红果 CENC 那条链路在本进程里），播放器没有解析能力；
//   2. `native_player_status` 要能回答"队列还剩几集"，只有持有队列的一方能答；
//   3. 播放器窗口可能被杀/重启，队列留在主进程里，重启后 `open` 仍能把队列续上。
// 队列项就是 `load` 的字段集合（`{url,key,title,ep,total,vw,vh,start,index}`），
// 所以"从队列取下一集"和"下发 load"是同一份数据，不需要两套结构。

/// 契约里的默认倒计时（秒）。0 表示无等待无缝连播；§3.3 `--autoplay-delay` 可覆盖。
const DEFAULT_AUTOPLAY_DELAY: f64 = 0.0;

/// `requestNext` 的等待预算（毫秒）。§3.3 写死的 8 秒，抽成常量便于测试引用。
const NEXT_REQUEST_TIMEOUT_MS: u64 = 8_000;

/// 一次尚未回执的 `requestNext`。
///
/// `token` 是"这一轮"的编号：8 秒后定时线程先比对 token，只有当前 pending 还是
/// 自己那一轮才报超时。否则会出现"前端其实已经推回来了，1 秒后却又冒出一条
/// 超时事件"的假警报（换集失败后再试一次尤其容易踩到）。
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingNext {
    token: u64,
    index: u32,
    ep: u32,
}

/// 一次正在进行的"下一集下发"（自己解析，或队列项自带直链时直接下发）。
///
/// `seq` 与 `PendingNext::token` 同源：解析是异步的（`tauri::async_runtime::spawn`），
/// 回来时队列可能已经被换掉（换剧 / 用户跳集 / 前端抢先推了回执）。回来时对不上
/// 就丢弃结果 —— 否则一次迟到的解析会把用户刚换的那一集顶掉，表现是"点了第 5 集、
/// 播的却是第 2 集"。
///
/// `index` 是去重键：同一个队列下标只允许一次下发在飞。红果按匿名设备计播放次数
/// （AGENTS.md §4：每台设备只回一次播放模型），并发两个解析请求会互相踢掉、两个都
/// 失败，而且白烧设备配额。
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingResolve {
    index: u32,
    seq: u64,
}

struct QueueState {
    /// 队列项**完整原样**保留前端下发的 JSON（不是裁剪过的结构）。
    ///
    /// 这一点是联播兜底的前提：`vid` / `episodeId` / `title` / `ep` 全在项里，
    /// 裁剪成"load 的字段集合"就把 `vid` 丢了 —— Rust 也就再也解析不了下一集，
    /// 只能回去求前端（那正是 §5.4 要消除的那一跳）。
    items: Vec<Value>,
    index: usize,
    autoplay: bool,
    autoplay_delay: f64,
    pending: Option<PendingNext>,
    /// 在飞的那一次"Rust 自己解析"（同 index 去重 + 过期结果丢弃）。
    pending_resolve: Option<PendingResolve>,
    /// 单调递增的请求编号（wrapping 防溢出）。
    token: u64,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            index: 0,
            autoplay: false,
            autoplay_delay: DEFAULT_AUTOPLAY_DELAY,
            pending: None,
            pending_resolve: None,
            token: 0,
        }
    }
}

fn queue_slot() -> &'static Mutex<QueueState> {
    static Q: OnceLock<Mutex<QueueState>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(QueueState::default()))
}

/// 播放器自己联播换集后，把 Rust 侧索引推到下一格。
///
/// 返回 `None` 表示"已经没有下一格"——此时索引保持原值（停在最后一集），
/// 交给 `requestNext` / `ended{final}` 那条路去决定要不要向前端要新的一集。
fn advance_index(current: usize, len: usize) -> Option<usize> {
    if current + 1 < len {
        Some(current + 1)
    } else {
        None
    }
}

/// 把外部传进来的索引夹进队列范围（空队列恒为 0）。
///
/// 前端算错索引是最常见的脏输入（用户跳集/换剧时 episodes 变了而 queue 没换），
/// 夹取比报错好：报错会让整个 open 失败，用户看到的是"原生播放起不来"。
fn clamp_index(index: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        index.min(len - 1)
    }
}

/// `requestNext` 是否已经超时。纯函数：这是联播里唯一"什么都没发生但用户要等 8 秒"
/// 的路径，必须可测——写成 `Instant` 比较就没法在单测里跑。
fn timed_out(elapsed_ms: u64, budget_ms: u64) -> bool {
    elapsed_ms >= budget_ms
}

/// 从队列项里取"这一集的身份"。
///
/// §3.1 的 `items` 只规定 `{url,key,title,ep,total,vw,vh,start,index}`，**没有
/// episodeId**；所以联播换集后历史会落到旧 episode_id 上（已知缺口）。前端若在
/// 队列项里多带 `episodeId`（或 `vid`），这里就优先采用——多一个字段就能让历史
/// 精确到集，不需要改协议版本。
fn item_episode_id(item: &Value) -> Option<String> {
    for key in ["episodeId", "episode_id", "vid"] {
        if let Some(s) = item.get(key).and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_owned());
            }
        }
    }
    None
}

/// 构造 `queue` 命令（§3.1）。
///
/// `autoplayDelay` 只在调用方给了具体值时才带：老播放器忽略未知字段没问题，
/// 但"每次队列更新都顺手改一遍倒计时"会让用户在播放器里设的值莫名被重置。
fn item_series_id(item: &Value) -> &str {
    pick_str(item, "seriesId", "series_id").trim()
}

fn apply_queue_identity(st: &mut PlayerState, item: &Value) {
    let series_id = item_series_id(item);
    if !series_id.is_empty() && series_id != st.series_id {
        return;
    }
    if let Some(id) = item_episode_id(item) {
        st.episode_id = id;
    }
}

fn queue_command(items: &[Value], index: usize, autoplay: bool, delay: Option<f64>) -> Value {
    let mut cmd = json!({
        "cmd": "queue",
        "items": items,
        "index": index,
        "autoplay": autoplay,
    });
    if let (Some(d), Some(obj)) = (delay, cmd.as_object_mut()) {
        obj.insert("autoplayDelay".into(), json!(d));
    }
    cmd
}

// ══════════════ 联播兜底：Rust 自己解析下一集（§5.4）══════════════
//
// 这一节全是**纯函数**：事件线程只做决策（"这一集我自己能不能接"），
// 网络与下发都在 `spawn_next_resolve` 里。拆成纯函数的理由是这些判定正是
// 联播最容易静默出错的地方（串集、重复打解析、把上一轮的迟到结果顶上去），
// 进程/管道那层在单测里造不出来，判定层可以。

/// 红果 vid 的形态：**纯数字**、至少 6 位。
///
/// 为什么不复用 `extract_vid`：那个函数会从任意字符串里"挑最长数字段"
/// （`guo:751234567890` → `751234567890`），对 `open` 是对的（前端给的就是
/// 各种包装过的 id），但对联播兜底是**危险**的：`guo:` 是红果云端链路
/// （duanju_core.dll）的 id，拿它去问原生 `multi_video_model` 只会白烧一次
/// 匿名设备的播放次数（AGENTS.md §4）。这里宁可判死（→ 回落 need-next），
/// 也不猜一个可能错的 id 去解析。
fn numeric_vid(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() >= 6 && raw.chars().all(|c| c.is_ascii_digit()) {
        Some(raw.to_owned())
    } else {
        None
    }
}

/// 队列项里的红果 vid（`vid` 优先，其次 `episodeId`）。
///
/// 先看**剧集来源**，再校验分集数字形态。非红果源完全可能使用 1、123456
/// 或更长的纯数字分集 id；长度只能说明它像 vid，不能说明它属于红果。
/// 没有 seriesId 的历史调用保持旧行为；open 会给新队列补齐当前 seriesId。
fn item_vid(item: &Value) -> Option<String> {
    if pick_str(item, "contentKind", "content_kind") == "anime" {
        return None;
    }
    let source = item
        .get("seriesId")
        .or_else(|| item.get("series_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if source.is_some_and(|id| numeric_vid(id).is_none()) {
        return None;
    }
    for key in ["vid", "episodeId", "episode_id"] {
        let Some(raw) = item.get(key) else { continue };
        let text = match raw {
            Value::String(s) => s.clone(),
            // 前端 TS 里是 string，但 JSON 数字也接受：一条队列项可能来自
            // 手写配置/调试脚本，多认一种形态不会让谁猜错集。
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if let Some(vid) = numeric_vid(&text) {
            return Some(vid);
        }
    }
    None
}

/// 把前端解析好的一集合并回整剧队列，返回它在队列里的**位置**。
/// 位置供 Rust 游标使用；项内声明的 index 继续对应前端 episodes[] 下标。
fn upsert_queue_item(items: &mut Vec<Value>, item: Value) -> usize {
    let declared_index = item.get("index").and_then(Value::as_u64);
    if let Some(index) = declared_index {
        if let Some(existing) = items
            .iter_mut()
            .find(|candidate| candidate.get("index").and_then(Value::as_u64) == Some(index))
        {
            *existing = item;
        } else {
            items.push(item);
        }
        items.sort_by_key(|candidate| {
            candidate
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX)
        });
        return items
            .iter()
            .position(|candidate| candidate.get("index").and_then(Value::as_u64) == Some(index))
            .unwrap_or(0);
    }
    items.push(item);
    items.len() - 1
}

/// `requestNext` 指向的队列项。
///
/// 优先按项内声明的 `index` 匹配，而不是拿它当数组下标：播放器发的 `index` 是
/// "剧集序号"（`Program.cs`：`index = queue.CurrentIndex + 1`）。队列项的数组位置
/// 与剧集序号不是同一概念：从前端回填一集时只 upsert 这一项，不能拿数组下标
/// 代替项内声明的 index。
/// 只有队列项**完全没有** `index` 字段时才退回数组下标（那是老前端的形状，
/// §3.1 里 `index` 本来就在字段集里，所以这条回退基本不会走到）。
fn find_next_item(items: &[Value], index: u32) -> Option<&Value> {
    let declared = items
        .iter()
        .any(|it| it.get("index").and_then(Value::as_u64).is_some());
    if declared {
        items
            .iter()
            .find(|it| it.get("index").and_then(Value::as_u64) == Some(index as u64))
    } else {
        items.get(index as usize)
    }
}

/// Rust 为什么接不了这一集（单独成枚举是为了让日志说清"差在哪"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextGap {
    /// 队列里没有 `index` 这一项（前端没下发 / 这部剧已经播到最后一个已知集）。
    NoItem,
    /// 有这一项，但项里没有能交给红果原生链路的 vid（动漫源、`guo:` 云端 id）。
    NoVid,
    /// 解析回来了，但 payload 里没有可用直链。
    NoStream,
    /// 解析本身失败（网络 / 设备拒绝 / 签名）。
    ResolveFailed,
}

impl NextGap {
    /// 日志用的短标签（`{:?}` 也够，但中文一行更好读日志）。
    fn label(self) -> &'static str {
        match self {
            NextGap::NoItem => "队列里没有这一项",
            NextGap::NoVid => "这一项没有可解析的 vid",
            NextGap::NoStream => "解析结果没有可用直链",
            NextGap::ResolveFailed => "原生解析失败",
        }
    }
}

fn item_huangguo_episode(item: &Value) -> Option<(String, String)> {
    let series_id = item_series_id(item);
    if !crate::huangguo::mapping::is_huangguo_id(series_id) {
        return None;
    }
    let episode_id = item_episode_id(item)?;
    Some((series_id.to_string(), episode_id))
}

/// `requestNext` 的处置方案。
#[derive(Debug, PartialEq)]
enum NextPlan {
    /// 队列项自带直链：不用碰网络，直接下发（少见，但不该当成错误）。
    Direct(Value),
    /// 只有 vid：交给异步解析。
    Resolve { item: Value, vid: String },
    /// 黄果分集：由 Rust 走 `HuangguoProvider` 解析直链并预热 HLS，无需往返前端。
    ResolveHuangguo {
        item: Value,
        series_id: String,
        episode_id: String,
    },
    /// 自己接不了：回落前端。
    Missing(NextGap),
}

/// 判定 `requestNext{index}` 能不能由 Rust 自己接住。
fn plan_next(items: &[Value], index: u32) -> NextPlan {
    let Some(item) = find_next_item(items, index) else {
        return NextPlan::Missing(NextGap::NoItem);
    };
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if !url.is_empty() {
        return NextPlan::Direct(item.clone());
    }
    if let Some((series_id, episode_id)) = item_huangguo_episode(item) {
        return NextPlan::ResolveHuangguo {
            item: item.clone(),
            series_id,
            episode_id,
        };
    }
    match item_vid(item) {
        Some(vid) => NextPlan::Resolve {
            item: item.clone(),
            vid,
        },
        None => NextPlan::Missing(NextGap::NoVid),
    }
}

/// 同一 index 的解析是否已经在飞（去重）。
fn resolve_in_flight(pending: Option<&PendingResolve>, index: u32) -> bool {
    matches!(pending, Some(p) if p.index == index)
}

/// 一轮解析的结果还要不要用。
///
/// 两个条件都必须成立，少一个都会出可见故障：
///   * `in_flight` 还是自己这一轮 —— 否则队列已经被换掉（换剧 / 用户跳集），
///     这条结果属于上一部剧，下发就是"点了第 5 集播第 2 集"；
///   * `pending` 还是自己这一轮 —— 否则播放器已经不等了（前端抢先推了回执，
///     或播放器自己换集成功了），再下发等于把两集同时塞给它。
fn resolve_result_wanted(
    pending: Option<PendingNext>,
    in_flight: Option<PendingResolve>,
    seq: u64,
    index: u32,
) -> bool {
    let is_current = matches!(in_flight, Some(p) if p.seq == seq && p.index == index);
    let still_waiting = matches!(pending, Some(p) if p.token == seq && p.index == index);
    is_current && still_waiting
}

/// 用"队列项 + 解析 payload"拼一条 `load` 命令（**不带** `cmd`，由下发口补）。
///
/// 字段口径对齐 `open()` 里的 load（同一个播放器、同一份 payload），差别只有
/// 三处，都是联播才有的语义：
///   * `start` 恒 0：联播不继承上一集的播放位置；
///   * `total` 用播放模型的 duration_ms —— 队列项里前端填的 `total` 是**总集数**
///     （`nativeNavigate.ts::buildNativeQueueEntry`），当秒数用会让控制坞显示
///     "00:03 / 01:40" 这种瞎数字；
///   * `index` 是这一集在剧集里的序号，播放器靠它推进自己的队列游标。
///
/// 刻意**不写** `queue`：裸 load 在"正在等 requestNext"时会被播放器当成回执
/// 直接播（`Program.cs` 的 `wasAwaiting`），而带上 queue 会顺手 `Reset` 它自己
/// 的队列 —— 我们这条路的队列收敛由本模块负责，两边同时改必然打乱游标。
/// 也不写 `totalEps`：播放器在 open 时已经有真值，这里传个猜的反而更糟。
fn build_next_load(
    item: &Value,
    payload: &Value,
    index: u32,
    ep: u32,
    volume: u32,
) -> Option<Value> {
    let url = payload
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if url.is_empty() {
        return None;
    }
    let key = payload
        .get("content_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let width = payload.get("width").and_then(Value::as_i64).unwrap_or(1080);
    let height = payload
        .get("height")
        .and_then(Value::as_i64)
        .unwrap_or(1920);
    let (vw, vh) = pipeline_size(width, height);
    let duration = payload
        .get("duration_ms")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        / 1000.0;
    let title = item.get("title").and_then(Value::as_str).unwrap_or("");
    // 集号以 `requestNext` 给的为准（播放器自己算的就是"当前 + 1"），
    // 队列项里的 `ep` 只是备用：某些前端形状里它可能是 0。
    let ep = if ep > 0 {
        ep
    } else {
        item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32
    };

    let mut load = json!({
        "url": url,
        "key": key,
        "start": 0.0,
        "title": title,
        "ep": ep,
        "total": duration,
        "vw": vw,
        "vh": vh,
        "volume": volume,
        "index": index,
    });
    if let Some(obj) = load.as_object_mut() {
        let sid = item_series_id(item);
        if !sid.is_empty() {
            obj.insert("seriesId".into(), json!(sid));
        }
        if let Some(eid) = item_episode_id(item) {
            obj.insert("episodeId".into(), json!(eid));
        }
        if let Some(vid) = item_vid(item) {
            obj.insert("vid".into(), json!(vid));
        }
    }
    Some(load)
}

// ══════════════ 音量镜像 ══════════════
//
// 为什么要记这一份：联播换集的 `load` 必须带 `volume`，而播放器对缺省 `volume`
// 的解释是 100（`LoadRequest.volume = 100`）—— 不记就会"用户把音量调到 40%，
// 每换一集跳回 100%"。播放器的 progress 心跳不带音量，`PlayerState` 里也没有
// 这个字段，所以只能在所有命令的唯一出口（`send`）顺手记下来。

/// 最后一次下发的音量（0–130，与播放器的 `CmdVolume` 同一量程）。
static LAST_VOLUME: AtomicU32 = AtomicU32::new(100);

/// 当前音量（没有任何下发过时是 100，等于播放器的默认值）。
fn last_volume() -> u32 {
    LAST_VOLUME.load(Ordering::Relaxed).min(130)
}

/// 从下发的命令里顺手记音量：`{cmd:"volume",v:40}` / `{cmd:"load",volume:40}`。
fn observe_volume(cmd: &Value) {
    let value = match cmd.get("cmd").and_then(Value::as_str) {
        Some("volume") => cmd.get("v"),
        Some("load") => cmd.get("volume"),
        _ => None,
    };
    if let Some(v) = value.and_then(Value::as_f64) {
        LAST_VOLUME.store(v.round().clamp(0.0, 130.0) as u32, Ordering::Relaxed);
    }
}

/// 改当前会话的 `PlayerState`（没有播放器时静默跳过）。
///
/// 锁顺序固定为 slot → state（和 `open` / `status` 一致），不要反过来嵌套。
fn update_state(f: impl FnOnce(&mut PlayerState)) {
    let guard = slot().lock().unwrap();
    if let Some(s) = guard.as_ref() {
        if let Ok(mut st) = s.state.lock() {
            f(&mut st);
        }
    }
}

/// 给主窗口发一条事件。窗口不在（例如启动早期）就退化成全局 emit，
/// 不让一条通知因为窗口名对不上而丢掉。
pub(crate) fn emit_main<R: Runtime>(app: &AppHandle<R>, event: &str, payload: Value) {
    match app.get_webview_window("main") {
        Some(win) => {
            let _ = win.emit(event, payload);
        }
        None => {
            let _ = app.emit(event, payload);
        }
    }
}

/// 把联播开关落库到设置库的 `settings.autoNext`。
///
/// 为什么不新增设置字段：WebView2 链路早就有 `autoNext`，两条链路各存一个开关，
/// 必然出现"原生里关掉、回网页又自动下一集"这种自相矛盾的状态。
/// 为什么读-改-写：`settings_save` 是整条记录写入，这里只想改一个字段，
/// 其余字段必须原样带回去（漏字段会连带把清晰度/源列表等恢复成默认）。
fn persist_autoplay<R: Runtime>(app: &AppHandle<R>, on: bool) {
    let state = app.state::<crate::AppState>();
    match state.database.settings_get() {
        Ok(mut settings) => {
            if settings.auto_next == on {
                return; // 已经是这个值，别为一次点击多写一次磁盘
            }
            settings.auto_next = on;
            if let Err(e) = state.database.settings_save(&settings) {
                crate::trace::log(format!("[原生播放器] 联播开关落库失败：{e}"));
            }
        }
        Err(e) => crate::trace::log(format!("[原生播放器] 读设置失败，联播开关未落库：{e}")),
    }
}

fn read_sanitized_settings<R: Runtime>(app: &AppHandle<R>) -> crate::models::UserSettings {
    let mut settings = app
        .try_state::<crate::AppState>()
        .and_then(|st| st.database.settings_get().ok())
        .unwrap_or_else(|| crate::models::UserSettings {
            vsr_enabled: crate::vsr_is_enabled(),
            ..Default::default()
        });
    crate::sanitize_danmaku_settings(&mut settings);
    if !NATIVE_PLAYER_AVAILABLE {
        settings.native_player_enabled = false;
        settings.preferred_player = "webview".into();
    }
    settings
}

fn settings_sync_fields(settings: &crate::models::UserSettings) -> Value {
    json!({
        "danmakuEnabled": settings.danmaku_enabled,
        "danmakuArea": settings.danmaku_area,
        "danmakuOpacity": settings.danmaku_opacity,
        "danmakuFontScale": settings.danmaku_font_scale,
        "danmakuSpeed": settings.danmaku_speed,
        "danmakuFont": settings.danmaku_font,
        "danmakuColor": settings.danmaku_color,
        "danmakuBold": settings.danmaku_bold,
        "danmakuStroke": settings.danmaku_stroke,
        "vsrEnabled": settings.vsr_enabled,
    })
}

fn inject_settings_fields(
    obj: &mut serde_json::Map<String, Value>,
    settings: &crate::models::UserSettings,
) {
    if let Value::Object(map) = settings_sync_fields(settings) {
        for (k, v) in map {
            obj.insert(k, v);
        }
    }
}

fn apply_danmaku_settings_event(settings: &mut crate::models::UserSettings, ev: &Value) {
    if let Some(b) = ev.get("danmakuEnabled").and_then(Value::as_bool) {
        settings.danmaku_enabled = b;
    }
    if let Some(v) = ev.get("danmakuArea").and_then(Value::as_f64) {
        settings.danmaku_area = v;
    }
    if let Some(v) = ev.get("danmakuOpacity").and_then(Value::as_f64) {
        settings.danmaku_opacity = v;
    }
    if let Some(v) = ev.get("danmakuFontScale").and_then(Value::as_f64) {
        settings.danmaku_font_scale = v;
    }
    if let Some(v) = ev.get("danmakuSpeed").and_then(Value::as_i64) {
        settings.danmaku_speed = v;
    }
    if let Some(s) = ev.get("danmakuFont").and_then(Value::as_str) {
        settings.danmaku_font = s.to_string();
    }
    if let Some(s) = ev.get("danmakuColor").and_then(Value::as_str) {
        settings.danmaku_color = s.to_string();
    }
    if let Some(b) = ev.get("danmakuBold").and_then(Value::as_bool) {
        settings.danmaku_bold = b;
    }
    if let Some(s) = ev.get("danmakuStroke").and_then(Value::as_str) {
        settings.danmaku_stroke = s.to_string();
    }
}

/// 主程序设置保存后同步给运行中的原生播放器（异步写管道，不阻塞 settings_save）。
pub fn sync_settings(settings: &crate::models::UserSettings) {
    let has_session = slot()
        .lock()
        .map(|mut g| g.as_mut().is_some_and(|s| s.alive_now()))
        .unwrap_or(false);
    if !has_session {
        return;
    }
    let mut clean = settings.clone();
    crate::sanitize_danmaku_settings(&mut clean);
    let mut cmd = settings_sync_fields(&clean);
    if let Some(obj) = cmd.as_object_mut() {
        obj.insert("cmd".into(), json!("settingsSync"));
    }
    std::thread::spawn(move || {
        let _ = send(cmd);
    });
}

fn dispatch_direct_load<R: Runtime>(app: AppHandle<R>, item: Value, index: u32, ep: u32, seq: u64) {
    std::thread::spawn(move || {
        if !claim_resolve(seq, index) {
            return;
        }
        let mut load = item.clone();
        if let Some(obj) = load.as_object_mut() {
            obj.insert("index".into(), json!(index));
            if ep > 0 {
                obj.insert("ep".into(), json!(ep));
            }
            if !obj.contains_key("start") {
                obj.insert("start".into(), json!(0.0));
            }
            if !obj.contains_key("volume") {
                obj.insert("volume".into(), json!(last_volume()));
            }
        }
        deliver_next(&app, load, index, ep, item);
    });
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedDanmakuQuery {
    norm_series: String,
    norm_video: String,
    echo_series: String,
    echo_episode: String,
    start_ms: u64,
    end_ms: u64,
}

fn strip_hongguo_id_prefix(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("guo:")
        .or_else(|| trimmed.strip_prefix("hongguo:"))
        .unwrap_or(trimmed)
        .trim()
}

fn is_valid_danmaku_numeric_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 25 && id.chars().all(|c| c.is_ascii_digit())
}

fn prepare_danmaku_query(v: &Value) -> Result<PreparedDanmakuQuery, Value> {
    let series_raw = pick_str(v, "seriesId", "series_id").trim().to_owned();
    let episode_raw = pick_str(v, "episodeId", "episode_id").trim().to_owned();
    let vid_raw = pick_str(v, "vid", "vid").trim().to_owned();
    let start_ms = v.get("startMs").and_then(Value::as_u64).unwrap_or(0);
    let duration_ms = v
        .get("durationMs")
        .and_then(Value::as_u64)
        .unwrap_or(30_000)
        .clamp(1_000, 120_000);
    let end_ms = start_ms.saturating_add(duration_ms);

    let echo_episode = if !episode_raw.is_empty() {
        episode_raw.clone()
    } else {
        vid_raw.clone()
    };
    let raw_video = if !vid_raw.is_empty() {
        vid_raw.as_str()
    } else {
        episode_raw.as_str()
    };
    let norm_series = strip_hongguo_id_prefix(&series_raw).to_owned();
    let norm_video = strip_hongguo_id_prefix(raw_video).to_owned();

    if !is_valid_danmaku_numeric_id(&norm_series) || !is_valid_danmaku_numeric_id(&norm_video) {
        return Err(build_danmaku_data_reply(
            &series_raw,
            &echo_episode,
            start_ms,
            end_ms,
            Err("当前内容源不支持弹幕".into()),
        ));
    }

    Ok(PreparedDanmakuQuery {
        norm_series,
        norm_video,
        echo_series: series_raw,
        echo_episode,
        start_ms,
        end_ms,
    })
}

fn build_danmaku_data_reply(
    series_id: &str,
    episode_id: &str,
    start_ms: u64,
    fallback_next_ms: u64,
    res: Result<Value, String>,
) -> Value {
    match res {
        Ok(data) => {
            let next_ms = data
                .get("nextMs")
                .and_then(Value::as_u64)
                .unwrap_or(fallback_next_ms);
            let items = data.get("items").cloned().unwrap_or_else(|| json!([]));
            json!({
                "cmd": "danmakuData",
                "seriesId": series_id,
                "episodeId": episode_id,
                "startMs": start_ms,
                "nextMs": next_ms,
                "items": items,
            })
        }
        Err(err) => json!({
            "cmd": "danmakuData",
            "seriesId": series_id,
            "episodeId": episode_id,
            "startMs": start_ms,
            "nextMs": fallback_next_ms,
            "items": [],
            "error": err,
        }),
    }
}

/// 找播放器可执行文件：发布态严格优先随包资源，开发态取最新的本地构建或 staged 产物。
fn find_player_exe<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    let mut bundled: Vec<PathBuf> = Vec::new();
    if let Ok(res) = app.path().resource_dir() {
        bundled.push(res.join("ttv-player").join("ttv-player.exe"));
        bundled.push(res.join("ttv-player.exe"));
    }
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            bundled.push(
                dir.join("resources")
                    .join("ttv-player")
                    .join("ttv-player.exe"),
            );
            bundled.push(dir.join("ttv-player.exe"));
        }
    }
    if !cfg!(debug_assertions) {
        if let Some(p) = bundled.into_iter().find(|p| p.is_file()) {
            crate::trace::log(format!("[原生播放器] 选用 {}", p.display()));
            return Some(p);
        }
        bundled = Vec::new();
    }

    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cands: Vec<PathBuf> = Vec::new();
    cands.extend(bundled);
    cands.push(
        here.join("resources")
            .join("ttv-player")
            .join("ttv-player.exe"),
    );
    cands.push(
        here.join("ttv-player")
            .join("target")
            .join("release")
            .join("ttv-player.exe"),
    );
    cands.push(
        here.join("ttv-player")
            .join("target")
            .join("debug")
            .join("ttv-player.exe"),
    );
    cands.push(
        here.join("target")
            .join("release")
            .join("resources")
            .join("ttv-player")
            .join("ttv-player.exe"),
    );
    cands.push(
        here.join("target")
            .join("debug")
            .join("resources")
            .join("ttv-player")
            .join("ttv-player.exe"),
    );

    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for p in cands {
        if !p.is_file() {
            continue;
        }
        let mt = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &best {
            None => best = Some((p, mt)),
            Some((_, bmt)) if mt >= *bmt => best = Some((p, mt)),
            _ => {}
        }
    }
    if let Some((p, _)) = best {
        crate::trace::log(format!("[原生播放器] 选用 {}", p.display()));
        Some(p)
    } else {
        None
    }
}

/// 关掉播放器（幂等）。应用退出、用户关窗口、切回内嵌播放都走这里。
///
/// ## 退出语义：这里是 `closed`，不是 `died`
///
/// 主动关闭必须留下"用户/应用自己关的"这条成因，否则下一次 `status()` 会把上一次
/// 异常留下的 `died` 当成这次的结果 —— 前端据此误判"窗口崩了"而自作主张把画面播回来，
/// 而用户其实只是切回了内嵌播放器。
///
/// ## 回收放在锁外
///
/// `quit` + 120ms + `kill/wait` 是阻塞的：持着 `slot()` 做这些，会让同一时刻的
/// `native_player_status`（2 秒一次的前端轮询）排队甚至超时。所以先在锁内 `take()`，
/// 出锁再收。
pub fn stop() {
    let taken = {
        let mut guard = slot().lock().unwrap();
        guard.take()
    };
    let Some(session) = taken else { return };
    if let Ok(mut w) = session.stdin.lock() {
        if let Some(w) = w.as_mut() {
            let _ = writeln!(w, "{}", json!({"cmd":"quit"}));
            let _ = w.flush();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    // 先落成因再回收：`retire` 会用 flags 里的成因写进历史快照。
    session.flags.mark("closed");
    retire(session, "closed");
}

/// 当前状态（无播放器时 alive=false）。
/// 把「应用此刻实际生效的主题」推给已经打开的播放器窗口。
///
/// ## 为什么需要一条独立命令，而不是塞进 `open`
///
/// 主题会变，集不变：用户在设置页把浅色切成暗色时，播放器窗口正开着（甚至可能
/// 全屏在播）。走 `open` 会触发一次换集（停流、重建、重新解析），代价与风险都
/// 不对等。这里只发一条 `theme`，播放器侧走 `BeginInvoke` → `ApplySystemTheme()`
/// → `PlayerBar.ApplyTheme(bool)`，**不动任何尺寸与播放状态**（契约 §2 的要求）。
///
/// 幂等且容错：没有播放器时不报错（`Ok(())`）—— 主题同步是"尽力而为"的旁路，
/// 它的失败绝不该让设置页弹错误。返回 `bool` 只是为了给调用方一个可打点的信号。
///
/// `mode` 三值：`light` / `dark` / `auto`（后者交回系统，等价于改造前行为）。
pub fn apply_theme(mode: &str) -> bool {
    let normalized = match mode.trim().to_lowercase().as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "auto",
    };
    send(json!({ "cmd": "theme", "mode": normalized })).is_ok()
}

// 旧实现是 `let mut st = s.state...; st.alive = true;` —— 无条件判活。于是播放器
// 被杀/崩溃之后前端仍读到 `alive: true`，守门员继续把内嵌 `<video>` 按停，
// 自动回退永远不触发，用户面对的是"两边都没有画面、也没有任何提示"。
//
// 现在的语义：
//   * `alive` 由**子进程真实状态 + closed 标记**共同决定（`Session::alive_now`）；
//   * 一旦判定不活，顺手把会话摘掉（避免下一次 `open` 复用死进程）并推进历史快照；
//   * 没有会话时返回**上次的快照**（alive=false），而不是全默认值 —— 前端在退出
//     那一刻落历史仍然要拿到"播到 97 秒、第 3 集"。
//
// 摘会话必须带会话号校验：探测与摘除之间可能刚好有新的 `open` 建了新会话，
// 只按"槽位非空"去 take 会把新会话一起扔掉（表现为刚开的窗口立刻变哑巴）。
pub fn status() -> PlayerState {
    // 第一段：持锁只做三件不阻塞的事 —— 判活、克隆快照、取成因。
    //
    // 成因必须在**这一次**查询里就对齐：旧实现先克隆（exit 恒为 none），
    // 再在 `retire()` 里算出 closed/died 写进历史，最后返回的还是克隆前那份 ——
    // 于是用户点 X 之后**第一次** status 得到 `alive:false, exit:"none"`，
    // 前端按"没有成因"回退、当场把画面播回来；直到第二次轮询才看到 closed。
    let probe = {
        let mut guard = slot().lock().unwrap();
        match guard.as_mut() {
            Some(s) => {
                let alive = s.alive_now();
                let mut snapshot = s.state.lock().map(|g| g.clone()).unwrap_or_default();
                let kind = s.flags.kind().unwrap_or_else(|| {
                    if alive {
                        "none".to_owned()
                    } else {
                        "died".to_owned()
                    }
                });
                snapshot.alive = alive;
                snapshot.exit = kind.clone();
                Some((s.id, alive, snapshot, kind))
            }
            None => None,
        }
    };

    match probe {
        None => {
            // 没有会话：交回历史快照（alive 恒 false），不是全默认值。
            let mut st = last_snapshot().unwrap_or_default();
            st.alive = false;
            st
        }
        Some((id, alive, snapshot, kind)) => {
            if !alive {
                // 第二段：**先出锁再回收**。`retire` 里有 kill/wait（阻塞），
                // 持着 slot 锁做它会把同一时刻的前端轮询、`open`、`send` 全部排队。
                // `take` 仍要带会话号校验：探测与摘除之间可能已有新会话。
                let dead = {
                    let mut guard = slot().lock().unwrap();
                    if guard.as_ref().map(|s| s.id) == Some(id) {
                        guard.take()
                    } else {
                        None
                    }
                };
                if let Some(dead) = dead {
                    // 用**本次查询算出的**成因回收，保证再有并发的 status 看到的
                    // 历史快照与第一段返回的一致。
                    retire(dead, &kind);
                }
            }
            snapshot
        }
    }
}

/// 给播放器发一条命令（JSON 行）。
///
/// ⚠️ 两个刻意的设计（都是踩过的坑）：
///   1. **不持有全局 slot 锁去写管道**。管道写是阻塞调用，一旦播放器端读端停了，
///      持锁写会让 `status`/`open` 全部跟着卡死（实测：一次 volume 命令把整个
///      控制面锁住，前端只能重启）。
///   2. **写操作带超时**：放到独立线程里写，2 秒没写完就返回错误，让调用方
///      拿到明确失败而不是无限等待。
pub fn send(cmd: Value) -> Result<(), String> {
    // 音量镜像：联播换集下发的 load 需要带上用户当前音量，否则播放器按
    // 缺省 100 处理（"每换一集音量跳回 100%"）。记在唯一出口最省事。
    observe_volume(&cmd);

    let is_load = cmd.get("cmd").and_then(Value::as_str) == Some("load");
    let (session_id, stdin) = {
        let guard = slot().lock().unwrap();
        let Some(s) = guard.as_ref() else {
            return Err("播放器没有在运行".into());
        };
        if is_load {
            let source = item_series_id(&cmd);
            if let Ok(st) = s.state.lock() {
                if !source.is_empty() && !st.series_id.is_empty() && source != st.series_id {
                    return Err("load 属于已经离开的剧集".into());
                }
            }
        }
        (s.id, s.stdin.clone())
    }; // 全局锁在这里释放：后面的阻塞写不再影响其他命令

    let line = format!(
        "{cmd}
"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut w = stdin.lock().unwrap();
        if is_load {
            let source = item_series_id(&cmd);
            let valid = {
                let guard = slot().lock().unwrap();
                guard.as_ref().is_some_and(|session| {
                    if session.id != session_id {
                        return false;
                    }
                    session.state.lock().ok().is_some_and(|state| {
                        source.is_empty() || state.series_id.is_empty() || state.series_id == source
                    })
                })
            };
            if !valid {
                let _ = tx.send(Err("load 属于已经离开的剧集".into()));
                return;
            }
            // 先让回执能看到新身份，再真正写 load；写锁覆盖这次校验与更新，
            // 防止排队的旧 load 越过新剧状态。锁顺序固定为 stdin → slot → state。
            {
                let guard = slot().lock().unwrap();
                if let Some(session) = guard.as_ref().filter(|session| session.id == session_id) {
                    if let Ok(mut st) = session.state.lock() {
                        reset_state_for_load(&mut st);
                        apply_queue_identity(&mut st, &cmd);
                        st.position = pick_f64(&cmd, "start", "start", 0.0);
                        st.duration = pick_f64(&cmd, "total", "total", 0.0);
                        if let Some(ep) = cmd.get("ep").and_then(Value::as_u64) {
                            st.episode = ep as u32;
                        }
                        if let Some(index) = cmd.get("index").and_then(Value::as_u64) {
                            st.index = index as u32;
                        }
                    }
                }
            }
            if let Some(index) = cmd.get("index").and_then(Value::as_u64) {
                let mut q = queue_slot().lock().unwrap();
                if let Some(position) = q.items.iter().position(|item| {
                    item.get("index").and_then(Value::as_u64) == Some(index)
                        && (source.is_empty() || item_series_id(item) == source)
                }) {
                    q.index = position;
                }
            }
        }
        let Some(w) = w.as_mut() else {
            let _ = tx.send(Err("播放器管道未就绪（刚启动，稍后再试）".to_string()));
            return;
        };
        let r = (|| -> Result<(), String> {
            w.write_all(line.as_bytes())
                .map_err(|e| format!("写入播放器失败：{e}"))?;
            w.flush().map_err(|e| format!("刷新播放器管道失败：{e}"))
        })();
        let _ = tx.send(r);
    });
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(r) => r,
        Err(_) => Err("播放器无响应（管道写超时）".into()),
    }
}

/// 补帧管线尺寸：长边固定 960（约 540p~720p 档，GPU 预算与画质折中）。
/// 横屏 1920×1080 → 960×540；竖屏 1080×1920 → 540×960。
/// 读字符串字段：前端（TS）传 camelCase，Rust 习惯 snake_case —— 两种都认，
/// 免得一处拼错就让 startSeconds 静默失效（实测踩过：从 0 播而不是续播点）。
fn pick_str<'a>(req: &'a Value, camel: &str, snake: &str) -> &'a str {
    req.get(camel)
        .or_else(|| req.get(snake))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn pick_f64(req: &Value, camel: &str, snake: &str, default: f64) -> f64 {
    req.get(camel)
        .or_else(|| req.get(snake))
        .and_then(Value::as_f64)
        .unwrap_or(default)
}

fn pick_u64(req: &Value, camel: &str, snake: &str, default: u64) -> u64 {
    req.get(camel)
        .or_else(|| req.get(snake))
        .and_then(Value::as_u64)
        .unwrap_or(default)
}

fn pick_u64_opt(req: &Value, camel: &str, snake: &str) -> Option<u64> {
    req.get(camel)
        .or_else(|| req.get(snake))
        .and_then(Value::as_u64)
}

fn pick_bool_opt(req: &Value, camel: &str, snake: &str) -> Option<bool> {
    req.get(camel)
        .or_else(|| req.get(snake))
        .and_then(Value::as_bool)
}

/// 取出红果 vid：整段都是数字就原样用；否则取最长的一段不少于 6 位的数字。
fn extract_vid(raw: &str) -> String {
    let raw = raw.trim();
    if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
        return raw.to_owned();
    }
    let mut best = String::new();
    let mut current = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
            if current.len() > best.len() {
                best = current.clone();
            }
        } else {
            current.clear();
        }
    }
    if best.len() >= 6 {
        best
    } else {
        String::new()
    }
}

fn normalize_fg_mode(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "drama" => "drama",
        "comic" => "comic",
        "anime" => "anime",
        _ => "auto",
    }
}

fn effective_fg_mode_for(mode: &str, content_kind: &str) -> &'static str {
    match normalize_fg_mode(mode) {
        "drama" => "drama",
        "comic" => "comic",
        "anime" => "anime",
        _ => match content_kind.trim().to_ascii_lowercase().as_str() {
            "comic" => "comic",
            "anime" => "anime",
            _ => "drama",
        },
    }
}

fn normalize_fg_mult(raw: u64) -> u64 {
    if raw == 0 {
        0
    } else {
        raw.clamp(2, 5)
    }
}

/// 按内容类型给出补帧默认值（前端也会传一份，这里是后端兜底）。
/// Clean-Room GPU 光流默认全内容启用且档位为 0（自动匹配屏幕原生刷新率）；
/// 漫剧自动进入分镜零形变保护模式（Panel-Safe Bypass）。
fn interpolation_preset(kind: &str) -> (bool, u64) {
    match kind {
        "comic" => (true, 0),
        "anime" => (true, 0),
        _ => (true, 0),
    }
}

/// 显式选档 > 播放器当前实际档位 > 内容初始预设。
/// 支持 `0`（自动匹配显示器刷新率）与 `2..=5`（倍率档位）。
fn select_fg_mult(requested: Option<u64>, actual: Option<u32>, preset: u64) -> u64 {
    normalize_fg_mult(
        requested
            .or_else(|| actual.map(u64::from))
            .unwrap_or(preset),
    )
}

fn pipeline_size(w: i64, h: i64) -> (i64, i64) {
    let (w, h) = (w.max(64) as f64, h.max(64) as f64);
    if w >= h {
        let vw = 960.0f64;
        let vh = (vw * h / w / 2.0).round() * 2.0;
        (vw as i64, vh.max(2.0) as i64)
    } else {
        let vh = 960.0f64;
        let vw = (vh * w / h / 2.0).round() * 2.0;
        (vw.max(2.0) as i64, vh as i64)
    }
}

/// 打开（或复用）播放器播放一集。
///
/// `req`：vid、series_id、episode_id、title、episode、start_seconds、volume、fg、fg_mult
pub async fn open<R: Runtime>(app: AppHandle<R>, req: Value) -> Result<PlayerState, String> {
    if !NATIVE_PLAYER_AVAILABLE {
        return Err(NATIVE_PLAYER_GRAY_REASON.to_string());
    }
    if !cfg!(windows) {
        return Err("独立 MPV 播放器目前仅支持 Windows，请使用内置播放器。".to_string());
    }
    crate::media_enhance::stop_prewarms();
    // 两个点击可能在红果解析里交错；按调用顺序完成，后打开的请求才拥有最终状态。
    let _open_guard = open_gate().lock().await;
    let vid_raw = req
        .get("vid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let vid = extract_vid(&vid_raw);
    let direct_url = pick_str(&req, "url", "playUrl").trim().to_owned();
    let content_kind = pick_str(&req, "contentKind", "content_kind").to_owned();
    let (preset_fg, preset_mult) = interpolation_preset(&content_kind);
    // ── 应用主题（`light` / `dark` / `auto`）──
    //
    // 原生窗口是个**独立进程**，读不到 WebView2 的 `localStorage['ttv:theme']`，
    // 所以它此前只能读注册表里的系统偏好 —— 用户在应用里选了浅色、系统却是深色时，
    // 播放器控制栏与主界面就是两种配色。这里把"应用此刻实际生效的模式"作为协议字段
    // 带下去（前端 `theme.ts` 解析过 `system` 之后的值）。
    //
    // 空值/未知值一律当 `auto`：老调用方不带这个字段，行为与改造前逐字一致。
    let theme_mode = match pick_str(&req, "theme", "theme")
        .trim()
        .to_lowercase()
        .as_str()
    {
        "light" => "light",
        "dark" => "dark",
        _ => "auto",
    }
    .to_owned();

    // 1) 直链：优先用调用方已经解析好的地址（动漫 MP4 / HLS），否则走红果原生解析。
    let (url, key, width, height, duration) = if !direct_url.is_empty() {
        let key = pick_str(&req, "key", "content_key").to_owned();
        let width = pick_u64(&req, "width", "width", 1080) as i64;
        let height = pick_u64(&req, "height", "height", 1920) as i64;
        let duration = pick_f64(&req, "duration", "duration", 0.0);
        (direct_url, key, width, height, duration)
    } else {
        if vid.is_empty() {
            return Err("缺少有效的 vid".into());
        }
        let payload = crate::short_drama_app::ensure_stream_payload_cached(&app, &vid)
            .await
            .map_err(|e| format!("解析播放地址失败：{e}"))?;
        let url = payload
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if url.is_empty() {
            return Err("播放地址为空".into());
        }
        let key = payload
            .get("content_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let width = payload.get("width").and_then(Value::as_i64).unwrap_or(1080);
        let height = payload
            .get("height")
            .and_then(Value::as_i64)
            .unwrap_or(1920);
        let duration = payload
            .get("duration_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            / 1000.0;
        (url, key, width, height, duration)
    };
    let (vw, vh) = pipeline_size(width, height);

    let title = pick_str(&req, "title", "title").to_owned();
    let episode = pick_u64(&req, "episode", "episode", 0) as u32;
    let series_id = pick_str(&req, "seriesId", "series_id").to_owned();
    let episode_id = pick_str(&req, "episodeId", "episode_id").to_owned();
    let start_seconds = pick_f64(&req, "startSeconds", "start_seconds", 0.0).max(0.0);
    let series_cover = {
        let primary = pick_str(&req, "seriesCover", "series_cover").trim();
        if !primary.is_empty() {
            primary.to_owned()
        } else {
            pick_str(&req, "cover", "cover").trim().to_owned()
        }
    };
    let req_channel = pick_str(&req, "channel", "channel").trim().to_owned();
    let resolved_channel = if !req_channel.is_empty() {
        req_channel
    } else if !content_kind.is_empty() {
        content_kind.clone()
    } else {
        String::new()
    };
    let req_history_type = pick_str(&req, "historyType", "history_type")
        .trim()
        .to_owned();
    let resolved_history_type = if req_history_type.is_empty() {
        "featured".to_string()
    } else {
        crate::storage::Database::normalize_history_type(Some(&req_history_type)).to_string()
    };

    // ── 联播队列入参（§3.1）──
    //
    // 向后兼容的硬约束：老调用方（PlayerControls / preferredPlayback）不传这几个
    // 字段，此处的 load JSON 与命令行必须与改造前逐字一致，行为仍然是
    // "单集播放、播完停住"。
    let mut queue_items: Vec<Value> = req
        .get("queue")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 来源身份在入口补齐：老前端可能只在 open 外层带 seriesId，队列项里没有。
    // 一旦落到 Rust 队列，每一项都必须能独立回答“我属于哪个源”。
    for item in &mut queue_items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let has_source = obj
            .get("seriesId")
            .or_else(|| obj.get("series_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty());
        if !has_source && !series_id.is_empty() {
            obj.insert("seriesId".into(), json!(series_id));
        }
        if !obj.contains_key("contentKind") && !obj.contains_key("content_kind") {
            obj.insert("contentKind".into(), json!(content_kind));
        }
    }
    let queue_index = pick_u64_opt(&req, "index", "index")
        .or_else(|| pick_u64_opt(&req, "queueIndex", "queue_index"))
        .map(|n| n as usize)
        .unwrap_or(0);
    let autoplay_req = pick_bool_opt(&req, "autoplay", "autoplay");
    // 只给队列、不给开关 ⇒ 按"要联播"处理：队列本身就是联播意图，前端漏传一个
    // 布尔不该让用户点了联播却一路停住。
    let autoplay = autoplay_req.unwrap_or(!queue_items.is_empty());
    let autoplay_delay = pick_f64(
        &req,
        "autoplayDelay",
        "autoplay_delay",
        DEFAULT_AUTOPLAY_DELAY,
    );
    let total_eps = pick_u64_opt(&req, "totalEpisodes", "total_episodes")
        .or_else(|| pick_u64_opt(&req, "totalEps", "total_eps"));
    let queue_pos = clamp_index(queue_index, queue_items.len());

    let mut load = json!({
        "url": url,
        "key": key,
        "start": pick_f64(&req, "startSeconds", "start_seconds", 0.0),
        "title": title,
        "ep": episode,
        "total": duration,
        "vw": vw,
        "vh": vh,
        "volume": pick_f64(&req, "volume", "volume", 80.0),
        "kind": content_kind,
    });

    let req_fg_mode = pick_str(&req, "fgMode", "fg_mode").trim().to_owned();
    let fg_mode_n = if req_fg_mode.is_empty() {
        let guard = slot().lock().unwrap();
        guard
            .as_ref()
            .and_then(|session| session.state.lock().ok().map(|state| state.fg_mode.clone()))
            .unwrap_or_else(|| "auto".into())
    } else {
        normalize_fg_mode(&req_fg_mode).to_owned()
    };
    let fg_mode_effective_n = effective_fg_mode_for(&fg_mode_n, &content_kind).to_owned();

    let db_settings = read_sanitized_settings(&app);

    // 新增字段只在调用方确实给了时才写进 load：老链路的 load JSON 保持逐字不变，
    // 播放器按旧字段解析得到的行为与改造前完全一致。
    if let Some(obj) = load.as_object_mut() {
        if !series_id.is_empty() {
            obj.insert("seriesId".into(), json!(series_id));
        }
        if !episode_id.is_empty() {
            obj.insert("episodeId".into(), json!(episode_id));
        }
        if !vid.is_empty() {
            obj.insert("vid".into(), json!(vid));
        }
        inject_settings_fields(obj, &db_settings);
        if !req_fg_mode.is_empty() || fg_mode_n != "auto" {
            obj.insert("fgMode".into(), json!(fg_mode_n));
        }
        if let Some(n) = total_eps {
            obj.insert("totalEps".into(), json!(n));
        }
        if autoplay_req.is_some() || !queue_items.is_empty() {
            obj.insert("index".into(), json!(queue_pos));
            obj.insert("autoplay".into(), json!(autoplay));
        }
        if !queue_items.is_empty() {
            // 新建窗口时不存在"先 queue 再 load"的时机（窗口是带着 load 文件起来的），
            // 所以队列必须随 load 一起落地，由 Program.cs 读 `LoadRequest.Queue`
            // （对象形态 {items,index,autoplay}，与 queue 命令同形；只写数组会让
            // 播放器拿不到"当前是第几项"，联播倒计时之后就会从第一集重来）。
            obj.insert(
                "queue".into(),
                json!({
                    "items": queue_items.clone(),
                    "index": queue_pos,
                    "autoplay": autoplay,
                }),
            );
        }
    }

    // 队列状态先落位再下发：`send` 返回后播放器可能立刻吐 `advance`/`requestNext`，
    // 事件线程读到的必须是这一轮的队列，而不是上一部剧的残留。
    {
        let mut q = queue_slot().lock().unwrap();
        q.items = queue_items.clone();
        q.index = queue_pos;
        q.autoplay = autoplay;
        q.autoplay_delay = autoplay_delay;
        q.pending = None; // 新一次 open 作废上一次 requestNext 的待回执
                          // 同时作废在飞的解析：它属于上一部剧/上一集，结果回来时 `claim_resolve`
                          // 会因为 seq 对不上而丢弃，但这里先清掉能让下一次 requestNext 立刻发起
                          // 新一轮（而不是被一个已经无主的 in-flight 挡住）。
        q.pending_resolve = None;
    }

    let fg_on = pick_bool_opt(&req, "fg", "fg").unwrap_or(preset_fg);
    let actual_fg_mult = {
        let guard = slot().lock().unwrap();
        guard
            .as_ref()
            .and_then(|session| session.state.lock().ok().map(|state| state.fg_mult))
    };
    let fg_mult_n = select_fg_mult(
        pick_u64_opt(&req, "fgMult", "fg_mult"),
        actual_fg_mult,
        preset_mult,
    );

    // 2) 活着就复用（只换集），否则重建
    //
    // "活着"必须走 `alive_now()`：只看 `child.try_wait()` 会漏掉"事件线程已经收到
    // `closed`、但进程还在收尾"的那一小段，那种状态下复用会往一个正在关闭的窗口
    // 发 load，表现为"点了下一集什么都没发生"。
    let reuse = {
        let mut guard = slot().lock().unwrap();
        let mut alive = false;
        if let Some(s) = guard.as_mut() {
            if s.alive_now() {
                alive = true;
                if let Ok(mut st) = s.state.lock() {
                    // ⚠️ 先归零"新鲜度"字段，再写新集的元数据。
                    //
                    // 复用窗口时 `episode_id` 换了，但 `phase` 还留着**上一集的 ready**。
                    // 前端启动确认（详情页那条入口）第一次查询就会把这份陈旧 ready
                    // 当成"新集已出画"，于是空窗口被判成功。归零之后要等播放器下一次
                    // `progress` 心跳按新加载的真实状态回填（loading → ready）。
                    reset_state_for_load(&mut st);
                    st.position = start_seconds;
                    st.duration = duration;
                    st.episode = episode;
                    st.title = title.clone();
                    st.series_id = series_id.clone();
                    st.episode_id = episode_id.clone();
                    st.series_cover = series_cover.clone();
                    st.total_episodes = total_eps
                        .map(|n| n as u32)
                        .unwrap_or(queue_items.len() as u32);
                    st.channel = resolved_channel.clone();
                    st.history_type = resolved_history_type.clone();
                    // 复用时队列可能整组换掉（换剧），状态字段必须一起对齐，
                    // 否则前端轮询会看到"新剧 + 旧队列长度"的错配。
                    st.autoplay = autoplay;
                    st.index = queue_pos as u32;
                    st.queue_len = queue_items.len() as u32;
                    st.fg_mult = fg_mult_n as u32;
                    st.fg_mode = fg_mode_n.clone();
                    st.fg_mode_effective = fg_mode_effective_n.clone();
                }
            }
        }
        if !alive {
            // 已退出（或主动关闭）：摘掉骨架 + 收进历史快照。
            // 必须摘，否则下一次 status 仍会把它当成"当前会话"而反复探测。
            if let Some(dead) = guard.take() {
                retire(dead, "replaced");
            }
        }
        alive
    };

    if reuse {
        // 顺序是契约要求的（§3.1）：**先 queue 再 load**。
        // 反过来会留出一个窗口期：load 已生效、队列还是空的，用户如果立刻播完
        // 这一集，播放器会误报 `requestNext`（而队列其实马上就到）。
        if !queue_items.is_empty() {
            send(queue_command(
                &queue_items,
                queue_pos,
                autoplay,
                Some(autoplay_delay),
            ))?;
        }
        let mut cmd = load.clone();
        if let Some(obj) = cmd.as_object_mut() {
            obj.insert("cmd".into(), json!("load"));
        }
        send(cmd)?;
        let _ = send(json!({"cmd":"fg","on": fg_on, "mult": fg_mult_n, "mode": fg_mode_n}));
        // 复用窗口换集：主题跟着应用走（窗口是新一集，但用户没改主题时这一步是幂等的）。
        let _ = send(json!({"cmd":"theme","mode": theme_mode}));
        crate::trace::log(format!(
            "[原生播放器] 复用窗口换集 vid={vid} ep={episode} kind={content_kind} fg={} mult={fg_mult_n} mode={fg_mode_n}",
            if fg_on { "on" } else { "off" }
        ));
        spawn_prefetch_next(&app, &queue_items, queue_pos);
        return Ok(status());
    }

    let exe = find_player_exe(&app)
        .ok_or_else(|| "找不到 ttv-player.exe（请先运行 scripts/ensure-integrity.ps1 或构建 src-tauri/ttv-player）".to_string())?;
    let pipe = format!(
        "ttv-player-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    // 3) load 走文件：URL 带签名很长，命令行传会被外壳吞引号（实测 PowerShell 踩过）
    let load_path = std::env::temp_dir().join(format!("{pipe}.json"));
    std::fs::write(&load_path, serde_json::to_vec(&load).unwrap_or_default())
        .map_err(|e| format!("写 load 文件失败：{e}"))?;

    let fg = if fg_on { "1" } else { "0" };
    let fg_mult = fg_mult_n.to_string();

    let mut launch = Command::new(&exe);
    strip_parent_webview2_env(&mut launch);
    launch
        .arg("--pipe")
        .arg(&pipe)
        .arg("--load-file")
        .arg(&load_path)
        .arg("--fg")
        .arg(fg)
        .arg("--fgmult")
        .arg(&fg_mult)
        .arg("--fgmode")
        .arg(&fg_mode_n)
        .arg("--kind")
        .arg(&content_kind);
    // 启动即带应用主题：不带的后果是"打开那一刻还读注册表"，之后才被 `theme`
    // 命令纠正 —— 用户会看到控制栏先闪一下另一种配色。
    launch
        .arg("--theme")
        .arg(&theme_mode)
        .arg("--vsr")
        .arg(if db_settings.vsr_enabled { "1" } else { "0" });
    // 联播启动参数（§3.3）。只在调用方明确要联播时才追加：老调用方不带
    // queue/autoplay，命令行与改造前逐字一致（播放器端少两个参数也不受影响）。
    if autoplay_req.is_some() || !queue_items.is_empty() {
        launch
            .arg("--autoplay")
            .arg(if autoplay { "1" } else { "0" })
            .arg("--autoplay-delay")
            .arg(format!("{autoplay_delay:.1}"));
    }
    let mut child = launch
        .current_dir(exe.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动播放器失败：{e}"))?;

    let _ = child.stdin.take();
    let state = Arc::new(Mutex::new({
        let mut st = PlayerState {
            alive: true,
            position: start_seconds,
            duration,
            episode,
            title: title.clone(),
            series_id: series_id.clone(),
            episode_id: episode_id.clone(),
            // 联播字段：新窗口继承这一轮的队列，前端轮询立刻能拿到正确长度。
            autoplay,
            index: queue_pos as u32,
            queue_len: queue_items.len() as u32,
            fg_mult: fg_mult_n as u32,
            fg_mode: fg_mode_n,
            fg_mode_effective: fg_mode_effective_n,
            series_cover,
            total_episodes: total_eps
                .map(|n| n as u32)
                .unwrap_or(queue_items.len() as u32),
            channel: resolved_channel,
            history_type: resolved_history_type,
            ..Default::default()
        };
        // 新窗口同样从"加载中"起算：默认的 `idle` 会让前端确认循环多等一拍，
        // 而 `loading` 才是这一刻的真实状态（与复用分支共用同一套归零）。
        reset_state_for_load(&mut st);
        st.position = start_seconds;
        st
    }));

    let session_id = next_session_id();
    let flags = Arc::new(SessionFlags::default());
    {
        let mut guard = slot().lock().unwrap();
        // 极端情况下上一轮在 spawn 与入槽之间被判活（几乎不可能），这里再兜一次：
        // 入槽前把残留会话 retire 掉，保证同一时刻只有一个会话。
        if let Some(old) = guard.take() {
            retire(old, "replaced");
        }
        *guard = Some(Session {
            id: session_id,
            child,
            stdin: Arc::new(Mutex::new(None)),
            state: state.clone(),
            pipe: pipe.clone(),
            flags: flags.clone(),
        });
    }

    spawn_event_reader(
        app.clone(),
        pipe.clone(),
        state,
        load_path,
        session_id,
        flags.clone(),
    );

    crate::trace::log(format!(
        "[原生播放器] 已启动 vid={vid} ep={episode} 管线={vw}x{vh} 管道={pipe} exe={}",
        exe.display()
    ));
    spawn_prefetch_next(&app, &queue_items, queue_pos);
    Ok(status())
}

/// 运行时替换联播队列（契约 §3.1 的 `queue` / `queueClear`）。
///
/// 入参形状与 `queue` 命令同形：`{items:[{url,key,title,ep,total,vw,vh,start,index}],
/// index, autoplay, autoplayDelay}`。
///
/// 两个刻意的设计：
///   1. **空 items 等价于 queueClear**。前端"退出剧集/关联播"时本来就要把
///      index/autoplay 一起归位，拆成两条命令只会多一个"清队列但忘了清 index"的
///      失败模式；契约里的 `queueClear` 语义在这里被吞进同一个入口。
///   2. 队列先写进进程内状态、再发命令。反过来的话，`send` 成功而状态没落位，
///      紧接着到的 `advance` 会按旧队列对齐索引，串到上一部剧的项上。
pub fn queue(input: Value) -> Result<PlayerState, String> {
    let items: Vec<Value> = input
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let queue_index = pick_u64_opt(&input, "index", "index")
        .map(|n| n as usize)
        .unwrap_or(0);
    let autoplay_req = pick_bool_opt(&input, "autoplay", "autoplay");
    let delay = pick_f64(
        &input,
        "autoplayDelay",
        "autoplay_delay",
        DEFAULT_AUTOPLAY_DELAY,
    );

    if items.is_empty() {
        let (autoplay, had_player) = {
            let mut q = queue_slot().lock().unwrap();
            q.items.clear();
            q.index = 0;
            q.pending = None; // 清队列等于"不要下一集了"，待回执一起作废
            q.pending_resolve = None; // 在飞的解析同样无主，结果会被丢弃
            q.autoplay_delay = delay;
            if let Some(on) = autoplay_req {
                q.autoplay = on;
            }
            (q.autoplay, slot().lock().unwrap().is_some())
        };
        if had_player {
            // 播放器在跑才需要下发；没播放器时 Rust 侧清干净就够了，
            // 下一次 open 会带着空队列重新开始。
            send(json!({"cmd":"queueClear"}))?;
        }
        update_state(|st| {
            st.index = 0;
            st.queue_len = 0;
            if autoplay_req.is_some() {
                st.autoplay = autoplay;
            }
        });
        return Ok(status());
    }

    let queue_pos = clamp_index(queue_index, items.len());
    let autoplay = {
        let mut q = queue_slot().lock().unwrap();
        q.items = items.clone();
        q.index = queue_pos;
        q.autoplay_delay = delay;
        q.pending = None;
        // 整组换队列 = 在飞的那次解析已经无主（它算的是旧队列里的下标）。
        q.pending_resolve = None;
        if let Some(on) = autoplay_req {
            q.autoplay = on;
        }
        q.autoplay
    };
    send(queue_command(&items, queue_pos, autoplay, Some(delay)))?;
    update_state(|st| {
        st.index = queue_pos as u32;
        st.queue_len = items.len() as u32;
        st.autoplay = autoplay;
    });
    crate::trace::log(format!(
        "[原生播放器] 队列更新 len={} index={queue_pos} autoplay={autoplay}",
        items.len()
    ));
    Ok(status())
}

/// `requestNext` 的回执：前端解析好下一集直链后推回来（见 `native_player_queue_push`）。
///
/// 完整链路（也是"为什么长这样"的全部答案）：
///   1. 播放器播完一集、队列里没有可播项 → 发 `requestNext{index,ep}`；
///   2. Rust **先自己解析**（§5.4）：队列项带 `vid` 就走
///      `hongguo_bridge::fetch_stream_payload`，成功则直接下发 `load`，根本
///      不会走到这里 —— 见 `spawn_next_resolve`；
///   3. 只有"没有这一项 / 没有 vid / 解析失败"才 emit `native-player-need-next`；
///   4. 前端用 episodes[] 里的下一集 + `extractHongguoVid` 解析出直链，
///      调本函数把一条 queue item（就是 load 的字段集合）推回来；
///   5. 本函数把它当 `load` 直接下发 —— §3.3 说的"收到 load 即视为成功"；
///   6. 8 秒没回执 → emit `native-player-queue-timeout`（播放器侧自己按契约
///      Toast 报错并停住，Rust 只负责通知前端）。
///
/// 为什么回执不塞回 `queue` 命令：`queue` 是"整组替换"，语义上会清掉播放器
/// 正在等的上下文；这里要的是"就这一集，开始播"，用 `load` 才和 §3.3 对齐。
pub fn push_next(mut item: Value) -> Result<PlayerState, String> {
    if !item.is_object() {
        return Err("queue_push 需要一条队列项对象".into());
    }
    let is_prefetch = item
        .as_object_mut()
        .and_then(|o| o.remove("prefetch"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let declared_index = item.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err("queue_push 的队列项缺少 url（前端没解析出直链）".into());
    }
    let ep = item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;

    let queue_len = {
        let mut q = queue_slot().lock().unwrap();
        if let Some(current_source) = q.items.iter().map(item_series_id).find(|id| !id.is_empty()) {
            let incoming_source = item_series_id(&item);
            if !incoming_source.is_empty() && incoming_source != current_source {
                return Err("queue_push 属于已经离开的剧集".into());
            }
            if incoming_source.is_empty() {
                item.as_object_mut()
                    .unwrap()
                    .insert("seriesId".into(), json!(current_source));
            }
        }
        if let Some(existing) = find_next_item(&q.items, declared_index) {
            if let (Some(expected), Some(incoming)) =
                (item_episode_id(existing), item_episode_id(&item))
            {
                if expected != incoming {
                    return Err("queue_push 分集身份与当前队列不一致".into());
                }
            }
            if !item.as_object().unwrap().contains_key("contentKind") {
                let kind = pick_str(existing, "contentKind", "content_kind");
                if !kind.is_empty() {
                    item.as_object_mut()
                        .unwrap()
                        .insert("contentKind".into(), json!(kind));
                }
            }
        }
        if is_prefetch {
            upsert_queue_item(&mut q.items, item.clone());
            q.items.len() as u32
        } else {
            // 回执到了：pending 清空，8 秒定时线程的 token 比对会失败，不再报超时。
            if let Some(p) = q.pending.take() {
                if ep != 0 && p.ep != 0 && p.ep != ep {
                    crate::trace::log(format!(
                        "[原生播放器] 回执 ep={ep} ≠ 请求 ep={}（index={}），按回执下发",
                        p.ep, p.index
                    ));
                }
            }
            // 前端已经把这一集送来了：Rust 侧那次解析（如果有）立刻作废 ——
            // 让它回来再下发一次 load，会把刚播起来的这一集重新装载一遍。
            q.pending_resolve = None;
            // 只更新这一集；其它集仍留在队列供选集栏和下一次 requestNext 使用。
            let position = upsert_queue_item(&mut q.items, item.clone());
            q.index = position;
            q.items.len() as u32
        }
    };

    if is_prefetch {
        let mut push_cmd = item.clone();
        if let Some(obj) = push_cmd.as_object_mut() {
            obj.insert("cmd".into(), json!("queuePush"));
        }
        send(push_cmd)?;
        update_state(|st| {
            st.queue_len = queue_len;
        });
        crate::trace::log(format!(
            "[原生播放器] 下一集 ep={ep}（index={declared_index}）已预推入播放器队列"
        ));
        return Ok(status());
    }

    // item 本身就是 load 的字段集合，补两个播放器必需的缺省值即可直接下发。
    let mut load = item.clone();
    if let Some(obj) = load.as_object_mut() {
        obj.insert("cmd".into(), json!("load"));
        if !obj.contains_key("start") {
            obj.insert("start".into(), json!(0.0));
        }
        if !obj.contains_key("total") {
            obj.insert("total".into(), json!(0.0));
        }
    }
    send(load)?;
    update_state(|st| {
        apply_queue_identity(st, &item);
        st.index = declared_index;
        st.queue_len = queue_len;
        st.episode = ep;
        st.position = 0.0;
    });
    crate::trace::log(format!("[原生播放器] requestNext 回执已下发 ep={ep}"));
    Ok(status())
}

/// 认领这一轮解析的结果：只有"仍是最新那一轮"且"播放器还在等"时才成立。
///
/// 成立时顺手把 `pending` / `pending_resolve` 一起清掉 —— 一轮解析只有一个结局
/// （下发 load 或回落 need-next），清干净才不会让 8 秒超时线程误报"超时"。
/// 不成立时只清掉自己那一格 `pending_resolve`，别人的 pending 一个字都不动
/// （它可能属于更新的一轮）。
fn claim_resolve(seq: u64, index: u32) -> bool {
    let mut q = queue_slot().lock().unwrap();
    if !resolve_result_wanted(q.pending, q.pending_resolve, seq, index) {
        if matches!(q.pending_resolve, Some(p) if p.seq == seq) {
            q.pending_resolve = None;
        }
        return false;
    }
    q.pending_resolve = None;
    q.pending = None;
    true
}

/// 联播兜底的下发口：`load` 写进管道 + 队列收敛 + emit `native-player-advance`。
///
/// 顺序是刻意的：
///   1. 先下发：`load` 一到播放器就开始装载（它在等 requestNext 时会把这条
///      当迟到的那一集直接播，见 `Program.cs` 的 `wasAwaiting`）；
///   2. 再收敛 Rust 侧队列：只 upsert 当前项、保留其它集的元数据，
///      这样后续 requestNext 与选集栏仍能使用整剧队列，观看历史也能落到新集；
///   3. 最后 emit advance：让前端同步"看到第几集"（没挂桥也不影响播放）。
///
/// ⚠️ `send` 是阻塞调用（自带 2s 超时），所以本函数只允许在**非事件线程**里跑：
/// 事件线程被它按住 2 秒，progress 心跳就断 2 秒。
fn deliver_next<R: Runtime>(app: &AppHandle<R>, load: Value, index: u32, ep: u32, item: Value) {
    let mut load = load;
    if let Some(obj) = load.as_object_mut() {
        obj.insert("cmd".into(), json!("load"));
    }
    if let Err(e) = send(load) {
        // 管道写失败 = 播放器已经没了（进程退出/窗口被关）。这时候再回落
        // need-next 只会让前端白解析一次，什么都不做才是对的。
        crate::trace::log(format!("[原生播放器] 联播 load 下发失败：{e}"));
        return;
    }
    let episode_id = item_episode_id(&item);
    let queue_len = {
        let mut q = queue_slot().lock().unwrap();
        let position = upsert_queue_item(&mut q.items, item.clone());
        q.index = position;
        q.pending = None;
        q.pending_resolve = None;
        q.items.len() as u32
    };
    update_state(|st| {
        apply_queue_identity(st, &item);
        st.index = index;
        st.queue_len = queue_len;
        if ep > 0 {
            st.episode = ep;
        }
        st.position = 0.0;
    });
    emit_main(
        app,
        "native-player-advance",
        json!({
            "index": index,
            "ep": ep,
            "seriesId": item_series_id(&item),
            "episodeId": episode_id,
            "queueLen": queue_len,
        }),
    );
}

/// 回落到前端：emit `native-player-need-next` + 起 8 秒超时定时器。
///
/// 两条路会走到这里：队列里没有可用项（Rust 一开始就接不了），以及 Rust 自己
/// 解析失败。两条路都有完整语义：前端解析成功就 `queue_push` 回来；没挂桥/失败
/// 就 8 秒后 emit timeout，播放器侧如实 Toast 并停住 —— 不会出现"永远在转圈"。
fn request_next_from_frontend<R: Runtime>(app: &AppHandle<R>, index: u32, ep: u32, gap: NextGap) {
    let series_id = status().series_id;
    // 重新起一轮 pending：这一轮的 token 是超时线程判断"还要不要报超时"的依据。
    // 不能沿用上一轮 —— 解析失败时它已经被 `claim_resolve` 清掉了。
    let token = {
        let mut q = queue_slot().lock().unwrap();
        q.token = q.token.wrapping_add(1);
        q.pending_resolve = None;
        q.pending = Some(PendingNext {
            token: q.token,
            index,
            ep,
        });
        q.token
    };
    crate::trace::log(format!(
        "[原生播放器] requestNext index={index} ep={ep} 回落到前端（{}），等回执（{NEXT_REQUEST_TIMEOUT_MS}ms）",
        gap.label()
    ));
    emit_main(
        app,
        "native-player-need-next",
        json!({
            "index": index,
            "ep": ep,
            "seriesId": series_id,
            "timeoutMs": NEXT_REQUEST_TIMEOUT_MS,
        }),
    );
    // 超时线程只负责"到点还没回执就报超时"，不碰播放器状态。
    let app_wait = app.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(NEXT_REQUEST_TIMEOUT_MS));
        let elapsed = started.elapsed().as_millis() as u64;
        if !timed_out(elapsed, NEXT_REQUEST_TIMEOUT_MS) {
            return; // 睡眠被系统打断（挂起恢复）：交给下一轮，别误报
        }
        // token 比对：只有 pending 还是自己这一轮才报超时，否则
        // "前端已经推回来了、1 秒后又冒出一条超时"这种假警报会一直存在。
        let still_waiting = {
            let mut q = queue_slot().lock().unwrap();
            if q.pending.map(|p| p.token) == Some(token) {
                q.pending = None;
                true
            } else {
                false
            }
        };
        if still_waiting {
            crate::trace::log(format!(
                "[原生播放器] requestNext 超时 index={index} ep={ep}"
            ));
            emit_main(
                &app_wait,
                "native-player-queue-timeout",
                json!({
                    "index": index,
                    "ep": ep,
                    "seriesId": series_id,
                    "timeoutMs": NEXT_REQUEST_TIMEOUT_MS,
                }),
            );
        }
    });
}

async fn resolve_huangguo_load<R: Runtime>(
    app: &AppHandle<R>,
    item: &Value,
    series_id: &str,
    episode_id: &str,
    index: u32,
    ep: u32,
    volume: u32,
) -> Result<Value, String> {
    let state = app.state::<crate::AppState>();
    let session = state
        .huangguo_provider
        .open_episode(0, series_id, episode_id, "auto", 0.0)
        .await?;
    let ep_num = if ep > 0 {
        ep
    } else {
        item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32
    };
    let title = item.get("title").and_then(Value::as_str).unwrap_or("");
    let total = item.get("total").and_then(Value::as_f64).unwrap_or(0.0);
    let mut load = json!({
        "url": session.url,
        "key": "",
        "start": 0.0,
        "title": title,
        "ep": ep_num,
        "total": total,
        "vw": 1080,
        "vh": 1920,
        "volume": volume,
        "index": index,
        "seriesId": series_id,
        "episodeId": episode_id,
        "kind": "huangguo",
    });
    if let Some(obj) = load.as_object_mut() {
        obj.insert("contentKind".into(), json!("huangguo"));
    }
    Ok(load)
}

fn spawn_next_resolve_huangguo<R: Runtime>(
    app: AppHandle<R>,
    index: u32,
    ep: u32,
    item: Value,
    series_id: String,
    episode_id: String,
    seq: u64,
) {
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let res = resolve_huangguo_load(
            &app,
            &item,
            &series_id,
            &episode_id,
            index,
            ep,
            last_volume(),
        )
        .await;
        let elapsed = started.elapsed().as_millis() as u64;
        if !claim_resolve(seq, index) {
            return;
        }
        match res {
            Ok(load) => {
                crate::trace::log(format!(
                    "[原生播放器] Rust 自己解析黄果成功 ep={ep}（{elapsed}ms），直接下发 load"
                ));
                let app_deliver = app.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    deliver_next(&app_deliver, load, index, ep, item);
                });
            }
            Err(e) => {
                crate::trace::log(format!(
                    "[原生播放器] Rust 黄果解析失败 ep={ep}（{elapsed}ms）：{e}，回落前端"
                ));
                request_next_from_frontend(&app, index, ep, NextGap::ResolveFailed);
            }
        }
    });
}

/// 发起一次"Rust 自己解析下一集"（§5.4 的核心）。
///
/// 必须 `spawn` 而不是在当前线程 await：调用方是**事件读取线程**，它是同步阻塞
/// 读行的（`while let Some(Ok(line)) = lines.next()`）。在里面 await 一次，后面
/// 排队的所有 `progress` 心跳都会被压住 —— HUD 的位置/缓存/相位全部冻住，
/// 8 秒超时判定也跟着漂。用户看到的现象是"画面在动、进度条和状态全卡死"。
fn spawn_next_resolve<R: Runtime>(
    app: AppHandle<R>,
    index: u32,
    ep: u32,
    item: Value,
    vid: String,
    seq: u64,
) {
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let payload = crate::short_drama_app::ensure_stream_payload_cached(&app, &vid).await;
        let elapsed = started.elapsed().as_millis() as u64;

        // 结果可能已经过期（换剧 / 前端抢先推了回执 / 播放器自己换了集）。
        // 过期就什么都不做 —— 一次迟到的 load 会把用户正在看的那一集顶掉。
        if !claim_resolve(seq, index) {
            crate::trace::log(format!(
                "[原生播放器] 解析结果作废（index={index} 已不需要，用时 {elapsed}ms）"
            ));
            return;
        }

        match payload {
            Ok(payload) => match build_next_load(&item, &payload, index, ep, last_volume()) {
                Some(load) => {
                    crate::trace::log(format!(
                        "[原生播放器] Rust 自己解析成功 vid={vid} ep={ep}（{elapsed}ms），直接下发 load"
                    ));
                    // 写管道是阻塞调用：丢到阻塞线程池，别占 tokio worker。
                    let app_deliver = app.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        deliver_next(&app_deliver, load, index, ep, item);
                    });
                }
                None => {
                    // payload 里没有直链：这一集等于没解析出来，回落前端。
                    request_next_from_frontend(&app, index, ep, NextGap::NoStream);
                }
            },
            Err(e) => {
                crate::trace::log(format!(
                    "[原生播放器] Rust 自己解析失败 vid={vid}（{elapsed}ms）：{e}，回落前端"
                ));
                request_next_from_frontend(&app, index, ep, NextGap::ResolveFailed);
            }
        }
    });
}

/// 首开/换集后**预解析下一集**直链，写回 Rust 队列槽（不下发 load / queuePush）。
/// 目的：把「播完才解析」的 150~250ms（失败时 8s 等待）挪到观看期间并行完成；
/// 播完时 `plan_next` 看到带直链的项走 Direct，接播零网络等待。
/// 只改队列槽、不写管道，因此与 requestNext 的在飞解析不会造成双重换集。
/// 失败静默 —— requestNext 兜底链路原样保留。
/// 将已就绪的队列项异步通过 `queuePush` 推入独立播放器，保证播完零等待直切。
fn send_queue_push_async(mut item: Value) {
    if let Some(obj) = item.as_object_mut() {
        obj.insert("cmd".into(), json!("queuePush"));
    }
    let _ = std::thread::Builder::new()
        .name("ttv-queue-push".into())
        .spawn(move || {
            let _ = send(item);
        });
}

fn spawn_prefetch_next<R: Runtime>(app: &AppHandle<R>, items: &[Value], pos: usize) {
    let want = (pos + 1) as u64;
    let Some(item) = items
        .iter()
        .find(|it| it.get("index").and_then(Value::as_u64) == Some(want))
        .cloned()
    else {
        return;
    };
    spawn_prefetch_item_for_index(app, item, pos + 1);
}

fn spawn_prefetch_item_for_index<R: Runtime>(app: &AppHandle<R>, item: Value, target_idx: usize) {
    let want = target_idx as u64;
    if !item
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return; // 已有直链，无需预解析
    }
    if let Some((series_id, episode_id)) = item_huangguo_episode(&item) {
        let ep = item.get("ep").and_then(Value::as_u64).unwrap_or(want + 1) as u32;
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let started = std::time::Instant::now();
            let Ok(load) =
                resolve_huangguo_load(&app, &item, &series_id, &episode_id, want as u32, ep, 0)
                    .await
            else {
                return;
            };
            let ms = started.elapsed().as_millis();
            let pushed_item = {
                let mut q = queue_slot().lock().unwrap();
                let Some(slot_item) = q
                    .items
                    .iter_mut()
                    .find(|it| it.get("index").and_then(Value::as_u64) == Some(want))
                else {
                    return;
                };
                if item_episode_id(slot_item).as_deref() != Some(episode_id.as_str())
                    || item_series_id(slot_item) != series_id
                {
                    return;
                }
                if !slot_item
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    return;
                }
                if let Some(obj) = slot_item.as_object_mut() {
                    for k in ["url", "key", "total", "vw", "vh"] {
                        if let Some(v) = load.get(k) {
                            obj.insert(k.into(), v.clone());
                        }
                    }
                }
                let mut combined = slot_item.clone();
                if let Some(obj) = combined.as_object_mut() {
                    obj.entry("index").or_insert_with(|| json!(want));
                    obj.entry("ep").or_insert_with(|| json!(ep));
                }
                combined
            };
            send_queue_push_async(pushed_item);
            crate::trace::log(format!(
                "[原生播放器] 预解析黄果下一集 ep={ep} 已提前推入播放器队列（{ms}ms），无缝接播就绪"
            ));
        });
        return;
    }
    let vid = item
        .get("vid")
        .or_else(|| item.get("episodeId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if vid.is_empty() {
        return;
    }
    let ep = item.get("ep").and_then(Value::as_u64).unwrap_or(want + 1) as u32;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let Ok(payload) = crate::short_drama_app::ensure_stream_payload_cached(&app, &vid).await
        else {
            return;
        };
        let Some(load) = build_next_load(&item, &payload, want as u32, ep, 0) else {
            return;
        };
        let ms = started.elapsed().as_millis();
        let pushed_item = {
            let mut q = queue_slot().lock().unwrap();
            // 换剧防护：槽里同 index 的项必须还是同一个 vid，否则丢弃（属于上一部剧）。
            let Some(slot_item) = q
                .items
                .iter_mut()
                .find(|it| it.get("index").and_then(Value::as_u64) == Some(want))
            else {
                return;
            };
            if item_vid(slot_item).as_deref() != Some(vid.as_str())
                || item_series_id(slot_item) != item_series_id(&item)
            {
                return;
            }
            if !slot_item
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                return; // 已被别的路径解析
            }
            if let Some(obj) = slot_item.as_object_mut() {
                for k in ["url", "key", "total", "vw", "vh"] {
                    if let Some(v) = load.get(k) {
                        obj.insert(k.into(), v.clone());
                    }
                }
            }
            let mut combined = slot_item.clone();
            if let Some(obj) = combined.as_object_mut() {
                obj.entry("index").or_insert_with(|| json!(want));
                obj.entry("ep").or_insert_with(|| json!(ep));
            }
            combined
        };
        send_queue_push_async(pushed_item);
        crate::trace::log(format!(
            "[原生播放器] 预解析下一集 ep={ep} 已提前推入播放器队列（{ms}ms），无缝接播就绪"
        ));
    });
}

/// `requestNext` 的处置方案（事件线程只做决策，动作都交给下面的函数）。
enum ResolveDecision {
    /// 同一 index 已经在解析：这条 requestNext 什么都不用做。
    InFlight,
    /// 队列项自带直链，直接下发。
    Direct { item: Value, seq: u64 },
    /// 交给异步解析。
    Native { item: Value, vid: String, seq: u64 },
    /// 交给黄果异步解析。
    Huangguo {
        item: Value,
        series_id: String,
        episode_id: String,
        seq: u64,
    },
    /// 自己接不了，回落前端。
    Frontend { gap: NextGap },
}

/// 事件读取线程：连管道（重试 10s）→ 逐行解析 → 更新状态 / 落观看历史。
fn spawn_event_reader<R: Runtime>(
    app: AppHandle<R>,
    pipe: String,
    state: Arc<Mutex<PlayerState>>,
    load_path: PathBuf,
    session_id: u64,
    flags: Arc<SessionFlags>,
) {
    std::thread::spawn(move || {
        let path = format!(r"\\.\pipe\{pipe}");
        let mut conn = None;
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(f) => {
                    conn = Some(f);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let Some(write_end) = conn else {
            let _ = std::fs::remove_file(&load_path);
            crate::trace::log("[原生播放器] 管道连接失败（播放器未起来？）");
            if let Ok(mut st) = state.lock() {
                st.alive = false;
            }
            // 连不上 = 这条会话注定起不来：立刻置 closed，让 status() 回收它，
            // 而不是让前端对着一个永远 alive 的幽灵会话轮询。
            flags.mark("failed");
            return;
        };
        // load 文件已经用过了，删掉（里面是签名 URL，不留在临时目录）
        let _ = std::fs::remove_file(&load_path);

        let read_end = match write_end.try_clone() {
            Ok(c) => c,
            Err(e) => {
                crate::trace::log(format!("[原生播放器] 管道复制失败：{e}"));
                if let Ok(mut st) = state.lock() {
                    st.alive = false;
                }
                flags.mark("failed");
                return;
            }
        };
        // 把写端交给命令层
        {
            let mut guard = slot().lock().unwrap();
            if let Some(s) = guard.as_mut() {
                if s.pipe == pipe {
                    if let Ok(mut w) = s.stdin.lock() {
                        *w = Some(write_end);
                    }
                }
            }
        }

        let mut last_history_emit = std::time::Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(std::time::Instant::now);
        let mut last_emitted_episode = String::new();

        let mut lines = BufReader::new(read_end).lines();
        while let Some(Ok(line)) = lines.next() {
            // 会话已被替换（用户换剧/重开了窗口）：这条线程继续读下去就会用
            // 上一部剧的 `advance` / `closed` 去改**新会话**的全局队列。
            // 直接退出，一个全局状态都不碰。
            if !is_current_session(session_id) {
                crate::trace::log("[原生播放器] 旧会话事件线程退出（已被替换）");
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let event = v.get("event").and_then(Value::as_str).unwrap_or("");
            match event {
                "progress" => {
                    let snapshot = {
                        let Ok(mut st) = state.lock() else { continue };
                        apply_progress_fields(&mut st, &v);
                        st.clone()
                    };
                    // Rust 侧队列的 autoplay 跟心跳对齐：`queue` 命令可能在心跳之后
                    // 才被播放器消费，不同步的话下一次 requestNext 回执会按旧开关
                    // 决定推不推下一集。
                    if let Some(b) = v.get("autoplay").and_then(Value::as_bool) {
                        if let Ok(mut q) = queue_slot().lock() {
                            q.autoplay = b;
                        }
                    }
                    // 尚在加载首帧时不要拿 0.0 覆盖续播点；出画或有效进度后再写库并按节流广播。
                    if snapshot.phase != "loading"
                        || snapshot.position > 0.5
                        || snapshot.duration > 0.5
                    {
                        let should_emit = last_emitted_episode != snapshot.episode_id
                            || last_history_emit.elapsed() >= Duration::from_millis(1500);
                        save_history_and_emit(&app, &snapshot, false, should_emit);
                        if should_emit && !snapshot.episode_id.is_empty() {
                            last_emitted_episode = snapshot.episode_id.clone();
                            last_history_emit = std::time::Instant::now();
                        }
                    }
                }
                "loaded" => {
                    // 一集真正开播：集号以播放器为准；历史身份若能对上队列项就一起更新。
                    let ep = v.get("ep").and_then(Value::as_u64).map(|n| n as u32);
                    let item = {
                        let current_series = state.lock().ok().map(|state| state.series_id.clone());
                        let q = queue_slot().lock().unwrap();
                        v.get("index")
                            .and_then(Value::as_u64)
                            .and_then(|index| find_next_item(&q.items, index as u32).cloned())
                            .or_else(|| {
                                let ep = ep?;
                                q.items
                                    .iter()
                                    .find(|item| {
                                        item.get("ep").and_then(Value::as_u64) == Some(ep as u64)
                                            && (item_series_id(item).is_empty()
                                                || Some(item_series_id(item))
                                                    == current_series.as_deref())
                                    })
                                    .cloned()
                            })
                            .or_else(|| q.items.get(q.index).cloned())
                    };
                    let snapshot = {
                        let Ok(mut st) = state.lock() else { continue };
                        if let Some(item) = item.as_ref() {
                            apply_queue_identity(&mut st, item);
                        }
                        if let Some(n) = ep {
                            st.episode = n;
                        }
                        st.clone()
                    };
                    if snapshot.episode_id != last_emitted_episode || snapshot.duration > 0.5 {
                        save_history_and_emit(&app, &snapshot, false, true);
                        if !snapshot.episode_id.is_empty() {
                            last_emitted_episode = snapshot.episode_id.clone();
                            last_history_emit = std::time::Instant::now();
                        }
                    }
                    crate::trace::log(format!("[原生播放器] 开始播放 ep={}", ep.unwrap_or(0)));
                }
                "ended" => {
                    let is_final = v.get("final").and_then(Value::as_bool).unwrap_or(false);
                    let snapshot = {
                        let Ok(mut st) = state.lock() else { continue };
                        if let Some(d) = v.get("dur").and_then(Value::as_f64) {
                            if d > 0.5 {
                                st.duration = d;
                            }
                        }
                        if let Some(p) = v.get("pos").and_then(Value::as_f64) {
                            st.position = p.max(0.0);
                        } else if st.duration > 0.5 {
                            st.position = st.duration;
                        }
                        st.clone()
                    };
                    save_history_and_emit(&app, &snapshot, true, true);
                    if !snapshot.episode_id.is_empty() {
                        last_emitted_episode = snapshot.episode_id.clone();
                        last_history_emit = std::time::Instant::now();
                    }
                    crate::trace::log(format!("[原生播放器] 本集播完 final={is_final}"));
                }
                "closed" => {
                    let snapshot = {
                        let Ok(mut st) = state.lock() else { continue };
                        st.alive = false;
                        // 成因同步写进快照：`status()` 已经会从 flags 取，
                        // 但直接读 state 的路径（诊断/日志）也不该看到一个空的成因。
                        st.exit = "closed".into();
                        if let Some(d) = v.get("dur").and_then(Value::as_f64) {
                            if d > 0.5 {
                                st.duration = d;
                            }
                        }
                        if let Some(p) = v.get("pos").and_then(Value::as_f64) {
                            st.position = p.max(0.0);
                        }
                        if let Some(ep) = v.get("ep").and_then(Value::as_u64) {
                            if ep > 0 {
                                st.episode = ep as u32;
                            }
                        }
                        let snap = st.clone();
                        record_last_if_newer(session_id, &snap);
                        snap
                    };
                    // 明确关闭：先置标记，`status()` 下一次轮询就会把它判成不活并回收，
                    // 前端据此区分"用户自己关的"（有 closed 事件）与"异常消失"（只有 EOF）。
                    flags.mark("closed");
                    // 窗口没了，队列也没有意义：留着会让下一次 open 带着上一部剧的
                    // 残留项，联播第一跳就串集。
                    if let Ok(mut q) = queue_slot().lock() {
                        q.items.clear();
                        q.index = 0;
                        q.pending = None;
                        q.pending_resolve = None;
                    }
                    save_history_and_emit(&app, &snapshot, false, true);
                    emit_closed_event(&app, &snapshot, "closed");
                    crate::trace::log("[原生播放器] 窗口关闭");
                    break;
                }
                "advance" => {
                    // 播放器自己按队列换集了（§3.3）。Rust 要做三件事：
                    // 对齐索引、把状态指到新那一集、把"换了集"告诉前端。
                    let explicit_index = v.get("index").and_then(Value::as_u64).map(|n| n as usize);
                    let from_index = v.get("fromIndex").and_then(Value::as_u64).map(|n| n as u32);
                    let ep = v.get("ep").and_then(Value::as_u64).map(|n| n as u32);
                    let (new_index, queue_len, item) = {
                        let mut q = queue_slot().lock().unwrap();
                        // 已经换集成功，之前那次 requestNext 的 8 秒等待作废。
                        q.pending = None;
                        // 在飞的解析也作废：播放器自己换集成功了，再下发一条 load
                        // 会把刚播起来的那一集重新装载（画面黑一下、位置回 0）。
                        q.pending_resolve = None;
                        match explicit_index {
                            Some(i) => q.index = clamp_index(i, q.items.len()),
                            None => {
                                if let Some(i) = advance_index(q.index, q.items.len()) {
                                    q.index = i;
                                }
                            }
                        }
                        (q.index, q.items.len(), q.items.get(q.index).cloned())
                    };
                    let snapshot = {
                        let Ok(mut st) = state.lock() else { continue };
                        if let Some(item) = item.as_ref() {
                            apply_queue_identity(&mut st, item);
                        }
                        if let Some(n) = ep {
                            st.episode = n;
                        }
                        st.index = new_index as u32;
                        st.queue_len = queue_len as u32;
                        st.position = 0.0;
                        st.clone()
                    };
                    save_history_and_emit(&app, &snapshot, false, true);
                    if !snapshot.episode_id.is_empty() {
                        last_emitted_episode = snapshot.episode_id.clone();
                        last_history_emit = std::time::Instant::now();
                    }
                    crate::trace::log(format!(
                        "[原生播放器] 联播换集 ep={} index={new_index}",
                        snapshot.episode
                    ));
                    emit_main(
                        &app,
                        "native-player-advance",
                        json!({
                            "index": new_index,
                            "ep": snapshot.episode,
                            "seriesId": snapshot.series_id,
                            "fromIndex": from_index,
                            "episodeId": snapshot.episode_id,
                            "queueLen": queue_len,
                        }),
                    );
                }
                "requestNext" => {
                    // 播放器要下一集（§3.3 / §5.4）。**先自己接一手**，接不住才求前端：
                    //
                    //   ① 队列里这一项带 `vid` → 本模块自己解析（spawn，异步），
                    //      成功直接下发 load + emit advance。这一段没有前端参与，
                    //      所以即使 React 桥没挂（用户已经退出播放页）联播也能走完。
                    //   ② 没有这一项 / 没有 vid / 解析失败 → 才 emit
                    //      `native-player-need-next` 给前端，保留 8 秒超时与 token 机制。
                    //
                    // 改造前是**无条件**走 ②（Rust 只发 need-next），链路里硬性夹着
                    // 一次"前端在线 + 前端解析成功"——这就是"第一集播完就停住"的根因。
                    //
                    // ⚠️ 这一段绝不能 await：本循环是同步阻塞读行的，await 一次就把
                    // 后面排队的 progress 心跳全压住（HUD 卡死、超时判定漂移）。
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let ep = v.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;

                    // 决策与状态落位在同一次加锁里完成：`pending_resolve` 必须先于
                    // 异步任务落位，否则任务回来时 `claim_resolve` 找不到自己的槽。
                    let decision = {
                        let mut q = queue_slot().lock().unwrap();
                        if resolve_in_flight(q.pending_resolve.as_ref(), index) {
                            // 同一个 index 已经在解析：不重复发起（红果按匿名设备计
                            // 播放次数，两个并发请求会互相踢掉，见 AGENTS.md §4）。
                            // 也**不动** pending —— 在飞那一轮要靠 token 判断"结果还要
                            // 不要"，换掉 token 会把马上就能用的结果判成过期，
                            // 用户就永远停在"正在加载下一集"。
                            ResolveDecision::InFlight
                        } else {
                            let plan = plan_next(&q.items, index);
                            q.token = q.token.wrapping_add(1);
                            let seq = q.token;
                            q.pending = Some(PendingNext {
                                token: seq,
                                index,
                                ep,
                            });
                            q.pending_resolve = None;
                            match plan {
                                NextPlan::Direct(item) => {
                                    // 现成直链也占这个槽：保证"同一个 index 同时只有
                                    // 一次下发在飞"，否则两条 requestNext 会把同一集
                                    // 下发两遍（画面黑一下、位置回 0）。
                                    q.pending_resolve = Some(PendingResolve { index, seq });
                                    ResolveDecision::Direct { item, seq }
                                }
                                NextPlan::Resolve { item, vid } => {
                                    q.pending_resolve = Some(PendingResolve { index, seq });
                                    ResolveDecision::Native { item, vid, seq }
                                }
                                NextPlan::ResolveHuangguo {
                                    item,
                                    series_id,
                                    episode_id,
                                } => {
                                    q.pending_resolve = Some(PendingResolve { index, seq });
                                    ResolveDecision::Huangguo {
                                        item,
                                        series_id,
                                        episode_id,
                                        seq,
                                    }
                                }
                                NextPlan::Missing(gap) => ResolveDecision::Frontend { gap },
                            }
                        }
                    };

                    match decision {
                        ResolveDecision::InFlight => {
                            crate::trace::log(format!(
                                "[原生播放器] requestNext index={index} ep={ep}：同一 index 的解析还在跑，不重复发起"
                            ));
                        }
                        ResolveDecision::Direct { item, seq } => {
                            // 队列项本来就有直链（少见：播放器没接住它的原因通常是
                            // 游标/去重，不是地址问题）。直接下发，不碰网络。
                            // 本线程不写管道（send 最长阻塞 2s，会按住心跳）：
                            // 交给一个独立线程，`deliver_next` 自己会带上 seq 校验。
                            crate::trace::log(format!(
                                "[原生播放器] requestNext index={index} ep={ep}：队列项自带直链，直接下发"
                            ));
                            dispatch_direct_load(app.clone(), item, index, ep, seq);
                        }
                        ResolveDecision::Native { item, vid, seq } => {
                            crate::trace::log(format!(
                                "[原生播放器] requestNext index={index} ep={ep}：Rust 自己解析 vid={vid}"
                            ));
                            spawn_next_resolve(app.clone(), index, ep, item, vid, seq);
                        }
                        ResolveDecision::Huangguo {
                            item,
                            series_id,
                            episode_id,
                            seq,
                        } => {
                            crate::trace::log(format!(
                                "[原生播放器] requestNext index={index} ep={ep}：Rust 自己解析黄果 {series_id}#{episode_id}"
                            ));
                            spawn_next_resolve_huangguo(
                                app.clone(),
                                index,
                                ep,
                                item,
                                series_id,
                                episode_id,
                                seq,
                            );
                        }
                        ResolveDecision::Frontend { gap } => {
                            request_next_from_frontend(&app, index, ep, gap);
                        }
                    }
                }
                "prefetchNext" => {
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let ep = v.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let plan_opt = {
                        let q = queue_slot().lock().unwrap();
                        if resolve_in_flight(q.pending_resolve.as_ref(), index) {
                            None
                        } else {
                            Some(plan_next(&q.items, index))
                        }
                    };
                    match plan_opt {
                        Some(NextPlan::Direct(item)) => {
                            send_queue_push_async(item);
                        }
                        Some(NextPlan::Resolve { item, vid: _ })
                        | Some(NextPlan::ResolveHuangguo { item, .. }) => {
                            spawn_prefetch_item_for_index(&app, item, index as usize);
                        }
                        Some(NextPlan::Missing(_)) => {
                            let series_id = state
                                .lock()
                                .ok()
                                .map(|s| s.series_id.clone())
                                .unwrap_or_default();
                            emit_main(
                                &app,
                                "native-player-prefetch-next",
                                json!({
                                    "index": index,
                                    "ep": ep,
                                    "seriesId": series_id,
                                }),
                            );
                        }
                        None => {}
                    }
                }
                "autoplayChanged" => {
                    let on = v.get("on").and_then(Value::as_bool).unwrap_or(false);
                    if let Ok(mut st) = state.lock() {
                        st.autoplay = on;
                    }
                    if let Ok(mut q) = queue_slot().lock() {
                        q.autoplay = on;
                    }
                    // 用户在播放器里改了开关 → 主程序负责落库（§3.2）。
                    persist_autoplay(&app, on);
                    emit_main(&app, "native-player-autoplay-changed", json!({ "on": on }));
                }
                "fgChanged" => {
                    let payload = {
                        let Ok(mut st) = state.lock() else { continue };
                        if let Some(m) = v.get("mult").and_then(Value::as_u64) {
                            st.fg_mult = normalize_fg_mult(m) as u32;
                        }
                        apply_fg_capability(&mut st, &v);
                        let fg_on = v
                            .get("fg")
                            .and_then(Value::as_bool)
                            .unwrap_or(st.fg_engine != "off");
                        json!({
                            "fg": fg_on,
                            "mult": st.fg_mult,
                            "fgEngine": st.fg_engine,
                            "fgMode": st.fg_mode,
                            "fgModeEffective": st.fg_mode_effective,
                        })
                    };
                    emit_main(&app, "native-player-fg-changed", payload);
                }
                "tracks" => {
                    // §3.2：音轨/字幕列表变化。空数组是有效值（这一集真的没有字幕），
                    // 所以列表无条件覆盖；只有 id 缺字段时才保持旧值。
                    let read_tracks = |key: &str| -> Vec<TrackOption> {
                        v.get(key)
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|it| {
                                        let id = it.get("id").and_then(Value::as_i64)?;
                                        Some(TrackOption {
                                            id: id as i32,
                                            label: it
                                                .get("label")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let audio = read_tracks("audio");
                    let subs = read_tracks("subs");
                    if let Ok(mut st) = state.lock() {
                        st.audio_tracks = audio;
                        st.sub_tracks = subs;
                        if let Some(n) = v.get("audioId").and_then(Value::as_i64) {
                            st.audio_id = n as i32;
                        }
                        if let Some(n) = v.get("subId").and_then(Value::as_i64) {
                            st.sub_id = n as i32;
                        }
                    }
                }
                "prev" | "next" => {
                    // 播放器的上下集按钮：把意图转给前端（前端持有剧集列表）
                    crate::trace::log(format!("[原生播放器] 用户点了{event}"));
                    if let Some(win) = app.get_webview_window("main") {
                        let series_id = state
                            .lock()
                            .map(|st| st.series_id.clone())
                            .unwrap_or_default();
                        let _ = win.emit(
                            "native-player-navigate",
                            json!({ "dir": event, "seriesId": series_id, "pos": v.get("pos").cloned().unwrap_or(json!(0.0)) }),
                        );
                    }
                }
                "requestDanmaku" => match prepare_danmaku_query(&v) {
                    Ok(query) => {
                        tauri::async_runtime::spawn(async move {
                            let res = crate::hongguo_bridge::fetch_danmaku(
                                &query.norm_video,
                                &query.norm_series,
                                query.start_ms as i64,
                                query.end_ms as i64,
                            )
                            .await;
                            let reply = build_danmaku_data_reply(
                                &query.echo_series,
                                &query.echo_episode,
                                query.start_ms,
                                query.end_ms,
                                res,
                            );
                            tauri::async_runtime::spawn_blocking(move || {
                                let _ = send(reply);
                            });
                        });
                    }
                    Err(reply) => {
                        tauri::async_runtime::spawn_blocking(move || {
                            let _ = send(reply);
                        });
                    }
                },
                "danmakuSettingsChanged" => {
                    if let Some(app_state) = app.try_state::<crate::AppState>() {
                        match app_state.database.settings_get() {
                            Ok(mut settings) => {
                                apply_danmaku_settings_event(&mut settings, &v);
                                crate::sanitize_danmaku_settings(&mut settings);
                                match app_state.database.settings_save(&settings) {
                                    Ok(()) => {
                                        emit_main(
                                            &app,
                                            "native-player-settings-changed",
                                            settings_sync_fields(&settings),
                                        );
                                    }
                                    Err(e) => {
                                        crate::trace::log(format!(
                                            "[原生播放器] 弹幕设置落库失败：{e}"
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                crate::trace::log(format!(
                                    "[原生播放器] 读设置失败，弹幕设置未落库：{e}"
                                ));
                            }
                        }
                    }
                }
                "vsrChanged" => {
                    let on = v.get("on").and_then(Value::as_bool).unwrap_or(false);
                    crate::set_vsr_enabled(on);
                    if let Some(app_state) = app.try_state::<crate::AppState>() {
                        match app_state.database.settings_get() {
                            Ok(mut settings) => {
                                if settings.vsr_enabled != on {
                                    settings.vsr_enabled = on;
                                    if let Err(e) = app_state.database.settings_save(&settings) {
                                        crate::trace::log(format!(
                                            "[原生播放器] VSR 开关落库失败：{e}"
                                        ));
                                    }
                                }
                                emit_main(
                                    &app,
                                    "native-player-settings-changed",
                                    settings_sync_fields(&settings),
                                );
                            }
                            Err(e) => {
                                crate::trace::log(format!(
                                    "[原生播放器] 读设置失败，VSR 开关未落库：{e}"
                                ));
                            }
                        }
                    }
                }
                "pickEpisode" => {
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let req_series = pick_str(&v, "seriesId", "series_id").trim().to_owned();
                    let sid = if !req_series.is_empty() {
                        req_series
                    } else {
                        state
                            .lock()
                            .map(|st| st.series_id.clone())
                            .unwrap_or_default()
                    };
                    let decision = {
                        let mut q = queue_slot().lock().unwrap();
                        let plan = plan_next(&q.items, index);
                        match plan {
                            NextPlan::Direct(item) => {
                                q.token = q.token.wrapping_add(1);
                                let seq = q.token;
                                let ep_val =
                                    item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                                q.pending = Some(PendingNext {
                                    token: seq,
                                    index,
                                    ep: ep_val,
                                });
                                q.pending_resolve = Some(PendingResolve { index, seq });
                                ResolveDecision::Direct { item, seq }
                            }
                            NextPlan::Resolve { item, vid } => {
                                q.token = q.token.wrapping_add(1);
                                let seq = q.token;
                                let ep_val =
                                    item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                                q.pending = Some(PendingNext {
                                    token: seq,
                                    index,
                                    ep: ep_val,
                                });
                                q.pending_resolve = Some(PendingResolve { index, seq });
                                ResolveDecision::Native { item, vid, seq }
                            }
                            NextPlan::ResolveHuangguo {
                                item,
                                series_id,
                                episode_id,
                            } => {
                                q.token = q.token.wrapping_add(1);
                                let seq = q.token;
                                let ep_val =
                                    item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                                q.pending = Some(PendingNext {
                                    token: seq,
                                    index,
                                    ep: ep_val,
                                });
                                q.pending_resolve = Some(PendingResolve { index, seq });
                                ResolveDecision::Huangguo {
                                    item,
                                    series_id,
                                    episode_id,
                                    seq,
                                }
                            }
                            NextPlan::Missing(gap) => ResolveDecision::Frontend { gap },
                        }
                    };
                    match decision {
                        ResolveDecision::Direct { item, seq } => {
                            let ep_val = item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                            dispatch_direct_load(app.clone(), item, index, ep_val, seq);
                        }
                        ResolveDecision::Native { item, vid, seq } => {
                            let app_pick = app.clone();
                            let sid_pick = sid.clone();
                            let ep_val = item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                            tauri::async_runtime::spawn(async move {
                                let payload = crate::short_drama_app::ensure_stream_payload_cached(
                                    &app_pick, &vid,
                                )
                                .await;
                                if !claim_resolve(seq, index) {
                                    return;
                                }
                                if let Ok(payload) = payload {
                                    if let Some(load) = build_next_load(
                                        &item,
                                        &payload,
                                        index,
                                        ep_val,
                                        last_volume(),
                                    ) {
                                        let app_deliver = app_pick.clone();
                                        tauri::async_runtime::spawn_blocking(move || {
                                            deliver_next(&app_deliver, load, index, ep_val, item);
                                        });
                                        return;
                                    }
                                }
                                emit_main(
                                    &app_pick,
                                    "native-player-navigate",
                                    json!({ "dir": "jump", "index": index, "seriesId": sid_pick }),
                                );
                            });
                        }
                        ResolveDecision::Huangguo {
                            item,
                            series_id,
                            episode_id,
                            seq,
                        } => {
                            let app_pick = app.clone();
                            let sid_pick = sid.clone();
                            let ep_val = item.get("ep").and_then(Value::as_u64).unwrap_or(0) as u32;
                            tauri::async_runtime::spawn(async move {
                                let res = resolve_huangguo_load(
                                    &app_pick,
                                    &item,
                                    &series_id,
                                    &episode_id,
                                    index,
                                    ep_val,
                                    last_volume(),
                                )
                                .await;
                                if !claim_resolve(seq, index) {
                                    return;
                                }
                                if let Ok(load) = res {
                                    let app_deliver = app_pick.clone();
                                    tauri::async_runtime::spawn_blocking(move || {
                                        deliver_next(&app_deliver, load, index, ep_val, item);
                                    });
                                    return;
                                }
                                emit_main(
                                    &app_pick,
                                    "native-player-navigate",
                                    json!({ "dir": "jump", "index": index, "seriesId": sid_pick }),
                                );
                            });
                        }
                        ResolveDecision::Frontend { .. } | ResolveDecision::InFlight => {
                            emit_main(
                                &app,
                                "native-player-navigate",
                                json!({ "dir": "jump", "index": index, "seriesId": sid }),
                            );
                        }
                    }
                }
                "home" => {
                    if let Some(win) = app.get_webview_window("main") {
                        let _ = win.unminimize();
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
                "error" => {
                    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("");
                    crate::trace::log(format!("[原生播放器] 错误：{msg}"));
                }
                _ => {}
            }
        }
        let was_closed = flags.kind().as_deref() == Some("closed");
        let eof_snapshot = if let Ok(mut st) = state.lock() {
            st.alive = false;
            // 只有"还没有成因"时才写 died：`closed` 分支已经写过，不能被覆盖。
            if st.exit == "none" || st.exit.is_empty() {
                st.exit = "died".into();
            }
            if !was_closed {
                let snap = st.clone();
                record_last_if_newer(session_id, &snap);
                Some(snap)
            } else {
                None
            }
        } else {
            None
        };
        // EOF 且没有先行收到 closed = 窗口不是被用户关的（被杀/崩溃/会话被顶掉）：
        // 只有这种成因才允许前端自动回退到内嵌播放。
        flags.mark("died");
        if let Some(snap) = eof_snapshot {
            if is_current_session(session_id) {
                save_history_and_emit(&app, &snap, false, true);
                emit_closed_event(&app, &snap, "died");
            }
        }
        crate::trace::log("[原生播放器] 事件流结束");
    });
}

fn apply_progress_fields(st: &mut PlayerState, v: &Value) {
    // 阶段 0：心跳顺带带回内核状态。先读 phase，避免仍在 loading 时被 0 覆盖已加载位置。
    if let Some(s) = v.get("phase").and_then(Value::as_str) {
        st.phase = s.to_string();
    }
    let next_pos = v.get("pos").and_then(Value::as_f64);
    let still_loading =
        st.phase == "loading" && st.position > 0.5 && next_pos.unwrap_or(0.0) <= 0.5;
    if !still_loading {
        if let Some(p) = next_pos {
            st.position = p.max(0.0);
        }
    }
    if let Some(d) = v.get("dur").and_then(Value::as_f64) {
        if d > 0.5 {
            st.duration = d;
        }
    }
    if let Some(c) = v.get("cachedUntil").and_then(Value::as_f64) {
        st.cached_until = c;
    }
    if let Some(s) = v.get("failure").and_then(Value::as_str) {
        st.failure = s.to_string();
    }
    if let Some(n) = v.get("vw").and_then(Value::as_i64) {
        st.video_width = n as i32;
    }
    if let Some(n) = v.get("vh").and_then(Value::as_i64) {
        st.video_height = n as i32;
    }
    if let Some(s) = v.get("quality").and_then(Value::as_str) {
        st.quality = s.to_string();
    }
    if let Some(n) = v.get("mult").and_then(Value::as_u64) {
        st.fg_mult = n as u32;
    }
    if let Some(n) = v.get("outMult").and_then(Value::as_u64) {
        st.fg_effective_mult = n as u32;
    }
    apply_fg_capability(st, v);
    if let Some(b) = v.get("autoplay").and_then(Value::as_bool) {
        st.autoplay = b;
    }
    if let Some(n) = v.get("index").and_then(Value::as_u64) {
        st.index = n as u32;
    }
    if let Some(n) = v.get("queueLen").and_then(Value::as_u64) {
        st.queue_len = n as u32;
    }
}

fn emit_closed_event<R: Runtime>(app: &AppHandle<R>, st: &PlayerState, exit: &str) {
    emit_main(
        app,
        "native-player-closed",
        json!({
            "exit": exit,
            "seriesId": st.series_id,
            "episodeId": st.episode_id,
            "episode": st.episode,
            "position": st.position,
            "duration": st.duration,
        }),
    );
}

/// 落观看历史：与 WebView2 链路共用同一张表，切回去续播位置一致。
fn save_history_and_emit<R: Runtime>(
    app: &AppHandle<R>,
    st: &PlayerState,
    force_finished: bool,
    emit_event: bool,
) {
    if st.series_id.is_empty() || st.episode_id.is_empty() {
        return;
    }
    let state = app.state::<crate::AppState>();
    let item = history_item_with_finished(st, force_finished);
    if let Err(e) = state.database.save_history(&item) {
        crate::trace::log(format!("[原生播放器] 历史保存失败：{e}"));
        return;
    }
    if emit_event {
        let canonical = state
            .database
            .get_history_item(&st.series_id, item.history_type.as_deref())
            .ok()
            .flatten()
            .unwrap_or(item);
        if let Ok(payload) = serde_json::to_value(canonical) {
            emit_main(app, "history-updated", payload);
        }
    }
}

#[cfg(test)]
fn history_item(st: &PlayerState) -> crate::models::WatchHistoryItem {
    history_item_with_finished(st, false)
}

fn history_item_with_finished(
    st: &PlayerState,
    force_finished: bool,
) -> crate::models::WatchHistoryItem {
    let pct = if force_finished {
        100
    } else if st.duration > 0.5 {
        ((st.position / st.duration) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8
    } else {
        0
    };
    let history_type = if st.history_type.trim().is_empty() {
        "featured"
    } else {
        crate::storage::Database::normalize_history_type(Some(&st.history_type))
    };
    crate::models::WatchHistoryItem {
        series_id: st.series_id.clone(),
        episode_id: st.episode_id.clone(),
        title: st.title.clone(),
        series_cover: st.series_cover.clone(),
        episode_number: st.episode,
        total_episodes: st.total_episodes,
        position_seconds: st.position.max(0.0),
        duration_seconds: st.duration.max(0.0),
        progress_percent: pct,
        updated_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        is_finished: force_finished || pct >= 98,
        channel: (!st.channel.trim().is_empty()).then(|| st.channel.trim().to_owned()),
        history_type: Some(history_type.to_string()),
    }
}

#[cfg(test)]
mod playback_identity_tests {
    use super::*;

    #[test]
    fn reused_window_history_uses_current_identity() {
        let current = PlayerState {
            series_id: "hg:new-series".into(),
            episode_id: "8".into(),
            title: "当前剧集".into(),
            episode: 8,
            position: 42.0,
            duration: 100.0,
            ..Default::default()
        };
        let history = history_item(&current);
        assert_eq!(history.series_id, current.series_id);
        assert_eq!(history.episode_id, current.episode_id);
        assert_eq!(history.title, current.title);
        assert_eq!(history.position_seconds, 42.0);
        assert_eq!(history.progress_percent, 42);
    }

    #[test]
    fn advancing_episode_updates_history_without_accepting_old_series_identity() {
        let mut current = PlayerState {
            series_id: "hg:new-series".into(),
            episode_id: "8".into(),
            title: "当前剧集".into(),
            ..Default::default()
        };
        apply_queue_identity(
            &mut current,
            &json!({"seriesId": "hg:old", "episodeId": "2"}),
        );
        assert_eq!(history_item(&current).episode_id, "8");
        apply_queue_identity(
            &mut current,
            &json!({"seriesId": "hg:new-series", "episodeId": "9"}),
        );
        let history = history_item(&current);
        assert_eq!(history.episode_id, "9");
        assert_eq!(history.series_id, "hg:new-series");
    }

    #[test]
    fn history_item_preserves_cover_total_channel_and_finished() {
        let state = PlayerState {
            series_id: "7706".into(),
            episode_id: "ep12".into(),
            title: "剧名".into(),
            series_cover: "https://img.test/p.jpg".into(),
            episode: 12,
            total_episodes: 88,
            position: 89.0,
            duration: 90.0,
            channel: "drama".into(),
            ..Default::default()
        };
        let item = history_item_with_finished(&state, true);
        assert_eq!(item.series_cover, "https://img.test/p.jpg");
        assert_eq!(item.total_episodes, 88);
        assert_eq!(item.channel.as_deref(), Some("drama"));
        assert!(item.is_finished);
        assert_eq!(item.progress_percent, 100);
    }

    #[test]
    fn loading_heartbeat_does_not_zero_out_resumed_position() {
        let mut st = PlayerState {
            position: 45.0,
            duration: 120.0,
            phase: "loading".into(),
            ..Default::default()
        };
        apply_progress_fields(
            &mut st,
            &json!({ "phase": "loading", "pos": 0.0, "dur": 0.0 }),
        );
        assert!((st.position - 45.0).abs() < 1e-6);

        apply_progress_fields(
            &mut st,
            &json!({ "phase": "ready", "pos": 46.2, "dur": 120.0 }),
        );
        assert!((st.position - 46.2).abs() < 1e-6);
    }
}

#[cfg(test)]
mod vid_and_preset_tests {
    use super::{extract_vid, interpolation_preset};

    #[test]
    fn extract_vid_accepts_plain_and_prefixed_ids() {
        assert_eq!(extract_vid("751234567890"), "751234567890");
        assert_eq!(extract_vid("prefix-751234567890"), "751234567890");
        assert_eq!(extract_vid("abc"), "");
        assert_eq!(extract_vid(""), "");
    }

    #[test]
    fn interpolation_preset_matches_content_kinds() {
        assert_eq!(interpolation_preset("comic"), (true, 0));
        assert_eq!(interpolation_preset("anime"), (true, 0));
        assert_eq!(interpolation_preset("drama"), (true, 0));
        assert_eq!(interpolation_preset(""), (true, 0));
    }
}

#[cfg(test)]
mod session_lifecycle_tests {
    //! 会话判活是"播放器到底还在不在"的唯一答案，它错了不会报错，只会静默错：
    //! 前端一直以为窗口还在 → 守门员继续按停内嵌播放器 → 用户两边都没画面。
    //!
    //! 这里**不**去断言 `alive = !closed && try_wait()==None` 这类和实现同构的表达式，
    //! 而是真的 spawn 一个 Windows 进程、让它跑完，看判活结果有没有跟着翻。
    use super::{PlayerState, Session};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    /// 一个"活着约 N 秒"的真实进程。
    ///
    /// 用 `ping` 而不是 `timeout`：后者在没有控制台句柄时会立即失败退出，
    /// 那种"spawn 完就是死的"进程无法证明"活着时判活为 true"。
    fn spawn_alive_for(seconds: u32) -> Child {
        Command::new("ping")
            .args(["-n", &(seconds + 1).to_string(), "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    fn session_with(child: Child, closed: bool) -> Session {
        let flags = Arc::new(super::SessionFlags::default());
        if closed {
            flags.mark("closed");
        }
        Session {
            id: 1,
            child,
            stdin: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(PlayerState::default())),
            pipe: "unit-test".into(),
            flags,
        }
    }

    /// 所有会动全局 `slot()` / `last_state()` 的用例必须串起来。
    ///
    /// `cargo test` 默认多线程：不串行的话，一个用例刚把会话装进槽位、
    /// 另一个用例的 `status()` 就会把它当成自己的（现象是断言随机失败，
    /// 换台机器就可能变绿）。
    static GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn lock_global() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 装一条真实会话进全局槽位，返回它的会话号。
    ///
    /// 顺带清历史快照：`last_state` 里存的是"最大会话号"那份，用例之间不清理的话，
    /// 谁先跑谁就把比较基准抬高，后面的用例会被判成"旧会话"而写不进去 ——
    /// 断言随即随机失败。清理发生在 `GLOBAL_LOCK` 之内，用例仍然串行。
    fn install_session(kind: Option<&str>) -> u64 {
        *super::last_state().lock().unwrap() = None;
        let id = super::next_session_id();
        let flags = Arc::new(super::SessionFlags::default());
        if let Some(kind) = kind {
            flags.mark(kind);
        }
        let state = PlayerState {
            episode: 7,
            position: 42.5,
            ..Default::default()
        };
        *super::slot().lock().unwrap() = Some(Session {
            id,
            child: spawn_alive_for(2),
            stdin: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(state)),
            pipe: "unit-test".into(),
            flags,
        });
        id
    }

    fn clear_slot() {
        let taken = { super::slot().lock().unwrap().take() };
        if let Some(mut s) = taken {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
    }

    /// **第一次** `status()` 就必须带对成因。
    ///
    /// 这是本轮修的那个回归：旧实现在第一次回执里给 `exit: "none"`，
    /// 前端按"没有成因 = 异常"回退，用户点一下 X 反而被强行播回来。
    #[test]
    fn first_status_after_closed_already_reports_closed() {
        let _guard = lock_global();
        install_session(Some("closed"));

        let first = super::status();
        assert!(!first.alive, "closed 之后第一次 status 必须 alive=false");
        assert_eq!(
            first.exit, "closed",
            "第一次 status 就要给出 closed（旧实现给 none，前端据此误回退）"
        );
        assert_eq!(first.position, 42.5, "快照里的位置不能被回收过程丢掉");

        // 第二次读的是历史快照，成因与 alive 必须一致。
        let second = super::status();
        assert!(!second.alive);
        assert_eq!(second.exit, "closed");
        assert_eq!(second.episode, 7);
    }

    /// 没有任何明确关闭事件 → 第一次就要判 `died`。
    #[test]
    fn first_status_after_silent_exit_reports_died() {
        let _guard = lock_global();
        let id = install_session(None);
        // 让进程真的退出去，模拟"被杀/崩溃"（没有 closed 事件）。
        {
            let mut guard = super::slot().lock().unwrap();
            if let Some(s) = guard.as_mut() {
                assert_eq!(s.id, id);
                let _ = s.child.kill();
                let _ = s.child.wait();
            }
        }
        let first = super::status();
        assert!(!first.alive);
        assert_eq!(first.exit, "died", "静默消失的成因必须是 died");

        let second = super::status();
        assert!(!second.alive);
        assert_eq!(second.exit, "died");
    }

    /// 管道没连上（`failed`）同样要在第一次回执里说清楚。
    #[test]
    fn first_status_reports_failed_when_pipe_never_connected() {
        let _guard = lock_global();
        install_session(Some("failed"));
        let first = super::status();
        assert!(!first.alive);
        assert_eq!(first.exit, "failed");
        clear_slot();
    }

    /// 旧会话迟到的回收不允许覆盖新会话的历史快照（防串集/防位置回退）。
    #[test]
    fn stale_retire_does_not_overwrite_newer_snapshot() {
        let _guard = lock_global();
        // 先清历史：比较基准必须由本用例自己建立，否则受执行顺序影响。
        {
            let mut guard = super::last_state().lock().unwrap();
            *guard = None;
        }
        // 先装一条"新"会话并回收，历史记的是它。
        let new_id = {
            let id = super::next_session_id();
            let state = PlayerState {
                episode: 9,
                position: 88.0,
                ..Default::default()
            };
            *super::slot().lock().unwrap() = Some(Session {
                id,
                child: spawn_alive_for(2),
                stdin: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(state)),
                pipe: "new".into(),
                flags: Arc::new(super::SessionFlags::default()),
            });
            id
        };
        let dead = { super::slot().lock().unwrap().take() }.expect("新会话应在槽里");
        super::retire(dead, "died");

        // 再拿一条**更旧**的会话号回收（模拟旧 reader 迟到）。
        let old = Session {
            id: new_id - 1,
            child: spawn_alive_for(2),
            stdin: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(PlayerState::default())),
            pipe: "old".into(),
            flags: Arc::new(super::SessionFlags::default()),
        };
        super::retire(old, "died");

        let st = super::status();
        assert_eq!(st.episode, 9, "旧会话的回收不能把集号冲回默认值");
        assert_eq!(st.position, 88.0, "旧会话的回收不能把位置冲掉");
    }

    /// 事件线程用来判断"我还是不是当前会话"的闸门。
    #[test]
    fn stale_reader_is_not_current_session() {
        let _guard = lock_global();
        let old_id = install_session(None);
        assert!(
            super::is_current_session(old_id),
            "装进槽位的会话应被认作当前"
        );
        // 用一条更新的会话顶掉它（模拟用户换剧/重开窗口）。
        let newer = Session {
            id: super::next_session_id(),
            child: spawn_alive_for(2),
            stdin: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(PlayerState::default())),
            pipe: "newer".into(),
            flags: Arc::new(super::SessionFlags::default()),
        };
        let newer_id = newer.id;
        let displaced = { super::slot().lock().unwrap().replace(newer) };
        if let Some(mut s) = displaced {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
        assert!(
            !super::is_current_session(old_id),
            "被顶掉的会话不能让它的 reader 继续改全局队列"
        );
        assert!(super::is_current_session(newer_id));
        clear_slot();
    }

    /// 进程跑着 → 活；进程退出 → 不活。**不能**是常量 true。
    #[test]
    fn alive_follows_real_process_exit() {
        let mut session = session_with(spawn_alive_for(2), false);
        assert!(session.alive_now(), "刚 spawn 的存活进程必须判活");

        let _ = session.child.wait();
        assert!(
            !session.alive_now(),
            "进程已经退出，status 仍判活 → 前端不会回退，用户两边都没画面"
        );
    }

    /// `closed` 是"窗口明确关闭"的即时信号，优先级高于进程还在。
    ///
    /// 为什么必须有它：播放器 keep-open + 关闭动画期间进程会多活几百毫秒，
    /// 只靠 try_wait 会在这段窗口里继续回 `alive: true`。
    #[test]
    fn closed_flag_wins_over_live_process() {
        let mut session = session_with(spawn_alive_for(2), true);
        assert!(!session.alive_now(), "已经收到 closed 事件的会话不能再判活");
        let _ = session.child.kill();
        let _ = session.child.wait();
    }

    /// 复用窗口换集时，上一集的 `ready` 绝不能留给新集。
    #[test]
    fn reset_state_for_load_clears_stale_readiness() {
        let mut st = PlayerState {
            phase: "ready".into(),
            failure: "none".into(),
            position: 31.0,
            cached_until: 40.0,
            video_width: 1080,
            video_height: 1920,
            ..Default::default()
        };

        super::reset_state_for_load(&mut st);

        assert_eq!(
            st.phase, "loading",
            "旧 ready 留给新集 → 前端确认循环会把空窗口当成已出画"
        );
        assert_eq!(st.failure, "none");
        assert_eq!(st.exit, "none");
        assert_eq!(st.position, 0.0);
        assert_eq!(st.cached_until, -1.0);
        assert_eq!(
            (st.video_width, st.video_height),
            (0, 0),
            "旧尺寸不能留给新集"
        );
    }

    #[test]
    fn manual_load_updates_episode_identity_and_queue_cursor() {
        let _guard = lock_global();
        install_session(None);
        let path =
            std::env::temp_dir().join(format!("native-load-test-{}.json", uuid::Uuid::new_v4()));
        {
            let guard = super::slot().lock().unwrap();
            let session = guard.as_ref().unwrap();
            *session.stdin.lock().unwrap() = Some(std::fs::File::create(&path).unwrap());
            let mut state = session.state.lock().unwrap();
            state.series_id = "hg:series".into();
            state.episode_id = "1".into();
            state.phase = "ready".into();
        }
        {
            let mut q = super::queue_slot().lock().unwrap();
            q.items = vec![
                serde_json::json!({"index": 0, "seriesId": "hg:series", "episodeId": "1"}),
                serde_json::json!({"index": 1, "seriesId": "hg:series", "episodeId": "2"}),
            ];
            q.index = 0;
        }
        let result = super::send(serde_json::json!({
            "cmd": "load", "seriesId": "hg:series", "episodeId": "2",
            "index": 1, "ep": 2, "start": 12.0, "total": 90.0,
            "url": "https://cdn.test/episode-2.mp4",
        }));
        let state = super::status();
        let cursor = super::queue_slot().lock().unwrap().index;
        clear_slot();
        {
            let mut q = super::queue_slot().lock().unwrap();
            q.items.clear();
            q.index = 0;
        }
        let _ = std::fs::remove_file(path);
        assert!(result.is_ok());
        assert_eq!(state.episode_id, "2", "手动换集必须同步历史身份");
        assert_eq!(state.episode, 2);
        assert_eq!(state.position, 12.0);
        assert_eq!(state.phase, "loading");
        assert_eq!(cursor, 1, "loaded事件须读取新集队列位置");
    }

    #[test]
    fn queued_old_series_load_never_overwrites_new_open() {
        let _guard = lock_global();
        install_session(None);
        let path =
            std::env::temp_dir().join(format!("native-stale-load-{}.json", uuid::Uuid::new_v4()));
        {
            let guard = super::slot().lock().unwrap();
            let session = guard.as_ref().unwrap();
            *session.stdin.lock().unwrap() = Some(std::fs::File::create(&path).unwrap());
            let mut state = session.state.lock().unwrap();
            state.series_id = "hg:old".into();
            state.episode_id = "1".into();
            state.phase = "ready".into();
            state.position = 40.0;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let sender = std::thread::spawn(move || {
            tx.send(()).unwrap();
            super::send(serde_json::json!({
                "cmd": "load", "seriesId": "hg:old", "episodeId": "2",
                "index": 1, "ep": 2, "start": 4.0, "url": "https://cdn.test/old.mp4",
            }))
        });
        rx.recv().unwrap();
        let stdin = {
            let guard = super::slot().lock().unwrap();
            guard.as_ref().unwrap().stdin.clone()
        };
        let pipe_guard = stdin.lock().unwrap();
        {
            let guard = super::slot().lock().unwrap();
            let mut state = guard.as_ref().unwrap().state.lock().unwrap();
            state.series_id = "hg:new".into();
            state.episode_id = "9".into();
            state.episode = 9;
            state.position = 88.0;
            state.phase = "loading".into();
        }
        drop(pipe_guard);
        let result = sender.join().unwrap();
        let state = super::status();
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        clear_slot();
        let _ = std::fs::remove_file(path);
        assert!(result.is_err(), "已被新剧替代的 load 必须丢弃");
        assert_eq!(state.series_id, "hg:new");
        assert_eq!(state.episode_id, "9");
        assert_eq!(state.episode, 9);
        assert_eq!(state.position, 88.0);
        assert!(contents.trim().is_empty(), "旧剧 load 不能继续发往播放器");
    }

    /// 历史快照：比较与写同锁 —— 并发下始终是"最大会话号"的那份。
    ///
    /// 旧实现是 `fetch_max` 判通过后再单独加锁写，检查与写之间有窗口，
    /// 旧会话能覆盖新快照（用户已经到第 5 集、历史写回第 2 集）。
    #[test]
    fn concurrent_retire_keeps_newest_snapshot() {
        let _guard = lock_global();
        {
            let mut guard = super::last_state().lock().unwrap();
            *guard = None;
        }
        let mut handles = Vec::new();
        for id in 1u64..=8 {
            handles.push(std::thread::spawn(move || {
                let snap = PlayerState {
                    episode: id as u32,
                    position: id as f64 * 10.0,
                    ..Default::default()
                };
                // 制造真实的交错窗口：判过之后立刻让出 CPU。
                std::thread::yield_now();
                super::record_last_if_newer(id, &snap);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let stored = super::last_state()
            .lock()
            .unwrap()
            .clone()
            .expect("应有快照");
        assert_eq!(stored.0, 8, "历史快照必须属于最大的会话号");
        assert_eq!(stored.1.episode, 8);
        assert_eq!(stored.1.position, 80.0);
    }

    /// 受控顺序：旧会话迟到的新增不得覆盖，更新会话可以。
    #[test]
    fn older_session_cannot_overwrite_newer_snapshot() {
        let _guard = lock_global();
        {
            let mut guard = super::last_state().lock().unwrap();
            *guard = None;
        }
        let five = PlayerState {
            episode: 5,
            ..Default::default()
        };
        super::record_last_if_newer(5, &five);

        let three = PlayerState {
            episode: 3,
            ..Default::default()
        };
        super::record_last_if_newer(3, &three);
        assert_eq!(
            super::last_state()
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .episode,
            5,
            "旧会话不能覆盖新快照"
        );

        let nine = PlayerState {
            episode: 9,
            ..Default::default()
        };
        super::record_last_if_newer(9, &nine);
        assert_eq!(
            super::last_state()
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .episode,
            9
        );
    }

    /// 没有会话时的 status 必须回历史快照且 alive=false（不是全默认值）。
    #[test]
    fn suspend_last_snapshot_is_reported_without_session() {
        let _guard = lock_global();
        clear_slot();
        {
            // 先清历史：不清的话，前面用例留下的快照会让这里的会话号比较失真。
            let mut guard = super::last_state().lock().unwrap();
            *guard = None;
        }
        let snap = PlayerState {
            position: 97.0,
            episode: 3,
            title: "某剧".into(),
            ..Default::default()
        };
        super::record_last_if_newer(super::next_session_id(), &snap);
        let st = super::status();
        assert!(!st.alive, "没有会话时 alive 必须是 false");
        assert_eq!(st.position, 97.0);
        assert_eq!(st.episode, 3);
    }
}

#[cfg(test)]
mod fg_contract_tests {
    //! 补帧能力字段的跨语言契约（C# `progress` → Rust `PlayerState` → 前端
    //! `NativePlayerStatus`）。字段名一旦对不上，失败形态是"界面永远显示未开启"，
    //! 不会有任何报错，所以这里把映射、拒值、缺字段与序列化逐条钉住。
    use super::{apply_fg_capability, PlayerState};
    use serde_json::json;

    #[test]
    fn progress_maps_capability_fields() {
        let mut st = PlayerState::default();
        apply_fg_capability(
            &mut st,
            &json!({
                "fgEngine": "gpu-optical-flow",
                "fgMode": "comic",
                "fgModeEffective": "comic",
                "displaySync": true,
                "displayHz": 180,
                "outFps": 29.7,
                "outRatio": 2.67,
                "paused": false,
            }),
        );
        assert_eq!(st.fg_engine, "gpu-optical-flow");
        assert_eq!(st.fg_mode, "comic");
        assert_eq!(st.fg_mode_effective, "comic");
        assert!(st.display_sync);
        assert_eq!(st.display_hz, 180);
        assert!((st.out_fps - 29.7).abs() < 1e-6);
        assert!((st.out_ratio - 2.67).abs() < 1e-6);
        assert!(!st.paused);
        // fg_mult 不受影响（能否执行与用户档位是两件事）。
        assert_eq!(st.fg_mult, 0);
    }

    /// 暂停时保留最后一次有效比率：`paused=true` 的心跳即使不带 `outRatio`
    /// （或只带 outFps 的维持性重画），比率也不能被清零。同时暂停状态本身要如实更新。
    #[test]
    fn paused_keeps_last_valid_ratio() {
        let mut st = PlayerState {
            out_ratio: 2.67,
            out_fps: 29.7,
            ..Default::default()
        };

        apply_fg_capability(&mut st, &json!({"paused": true, "outFps": 30.0}));
        assert!(st.paused, "暂停状态要如实同步");
        assert!(
            (st.out_ratio - 2.67).abs() < 1e-6,
            "暂停期间不得丢失最后一次有效比率"
        );
        assert!(
            (st.out_fps - 30.0).abs() < 1e-6,
            "outFps 可继续按心跳更新（维持性重画）"
        );

        apply_fg_capability(&mut st, &json!({"paused": false, "outRatio": 1.5}));
        assert!(!st.paused);
        assert!((st.out_ratio - 1.5).abs() < 1e-6, "恢复后按新值更新");
    }

    #[test]
    fn progress_missing_fields_keep_previous_values() {
        let mut st = PlayerState {
            fg_engine: "gpu-interp-inactive".into(),
            fg_mode: "anime".into(),
            fg_mode_effective: "anime".into(),
            display_sync: true,
            display_hz: 144,
            out_fps: 24.0,
            out_ratio: 2.5,
            paused: false,
            ..Default::default()
        };
        // 老播放器的心跳：只有老字段，没有这几个。
        apply_fg_capability(&mut st, &json!({"pos": 12.0, "dur": 40.0}));
        assert_eq!(
            st.fg_engine, "gpu-interp-inactive",
            "缺字段不许清零（否则界面闪回已关闭）"
        );
        assert_eq!(st.fg_mode, "anime");
        assert_eq!(st.fg_mode_effective, "anime");
        assert!(st.display_sync);
        assert_eq!(st.display_hz, 144);
        assert!((st.out_fps - 24.0).abs() < 1e-6);
        assert!(
            (st.out_ratio - 2.5).abs() < 1e-6,
            "缺 outRatio 时保持上一次的值"
        );
        assert!(!st.paused, "缺 paused 时保持上一次的值");
    }

    #[test]
    fn progress_rejects_invalid_numbers_and_unknown_engine() {
        let mut st = PlayerState {
            display_hz: 120,
            out_fps: 30.0,
            out_ratio: 1.8,
            ..Default::default()
        };
        apply_fg_capability(
            &mut st,
            &json!({
                "fgEngine": "2X",          // 档位名不是引擎名
                "fgMode": "bogus",
                "fgModeEffective": "auto", // auto 不是已解析模式
                "displaySync": "yes",      // 类型不对
                "displayHz": -1,           // 负数
                "outFps": -0.5,            // 负数
                "outRatio": -2.0,          // 负数
                "paused": "yes",           // 类型不对
            }),
        );
        assert_eq!(st.fg_engine, "off", "未知引擎名不得写进契约");
        assert_eq!(st.fg_mode, "auto");
        assert_eq!(st.fg_mode_effective, "drama");
        assert!(!st.display_sync, "类型不对不得写成 true");
        assert_eq!(st.display_hz, 120, "负数刷新率必须被拒");
        assert!((st.out_fps - 30.0).abs() < 1e-6, "负数帧率必须被拒");
        assert!((st.out_ratio - 1.8).abs() < 1e-6, "负数比率必须被拒");
        assert!(!st.paused, "类型不对的 paused 不得写进契约");

        // 超范围（明显不可能是帧率/刷新率/比率）同样被拒。
        apply_fg_capability(
            &mut st,
            &json!({"displayHz": 100000, "outFps": 1e9, "outRatio": 1e9}),
        );
        assert_eq!(st.display_hz, 120);
        assert!((st.out_fps - 30.0).abs() < 1e-6);
        assert!((st.out_ratio - 1.8).abs() < 1e-6);
    }

    #[test]
    fn player_state_serializes_capability_as_camel_case() {
        let value = serde_json::to_value(PlayerState::default()).expect("serialize");
        let obj = value.as_object().expect("object");
        // 前端 `NativePlayerStatus` 读的就是这几个键名。
        for key in [
            "fgEngine",
            "fgMode",
            "fgModeEffective",
            "displaySync",
            "displayHz",
            "outFps",
            "outRatio",
            "paused",
            "fgMult",
            "fgEffectiveMult",
        ] {
            assert!(obj.contains_key(key), "序列化缺字段 {key}");
        }
        assert_eq!(obj["fgEngine"], json!("off"));
        assert_eq!(obj["fgMode"], json!("auto"));
        assert_eq!(obj["fgModeEffective"], json!("drama"));
        assert_eq!(obj["displaySync"], json!(false));
        assert_eq!(obj["displayHz"], json!(0));
        assert_eq!(obj["outFps"], json!(0.0));
        assert_eq!(obj["outRatio"], json!(1.0), "默认比率必须是 1，不能是 0");
        assert_eq!(obj["paused"], json!(false));
    }

    #[test]
    fn open_multiplier_prefers_explicit_then_actual_then_preset() {
        assert_eq!(
            super::select_fg_mult(None, Some(4), 0),
            4,
            "重开未带倍率时必须沿用播放器实际的 4X"
        );
        assert_eq!(
            super::select_fg_mult(None, Some(0), 2),
            0,
            "播放器当前在自动匹配档位(0)时必须保留 0"
        );
        assert_eq!(
            super::select_fg_mult(Some(5), Some(4), 0),
            5,
            "用户显式选档优先于旧状态"
        );
        assert_eq!(
            super::select_fg_mult(None, None, 3),
            3,
            "没有旧状态时才使用内容预设"
        );
        assert_eq!(
            super::select_fg_mult(Some(1), None, 0),
            2,
            "非零低倍率夹取到最小 2X"
        );
        assert_eq!(
            super::select_fg_mult(Some(99), None, 0),
            5,
            "显式倍率仍受 2X–5X 上限约束"
        );
    }
}

#[cfg(test)]
mod queue_logic_tests {
    //! 联播里"纯计算"的部分：索引推进、越界夹取、超时判定、命令形状。
    //!
    //! 这些正是联播最容易静默出错的地方（串集、永远不超时、命令少字段），
    //! 所以全部抽成纯函数单独锁住 —— 进程/管道/窗口那层在单测里造不出来。

    use super::{
        advance_index, apply_danmaku_settings_event, build_danmaku_data_reply, build_next_load,
        clamp_index, find_next_item, item_episode_id, item_vid, numeric_vid, plan_next,
        prepare_danmaku_query, queue_command, resolve_in_flight, resolve_result_wanted,
        settings_sync_fields, strip_parent_webview2_env, timed_out, NextGap, NextPlan, PendingNext,
        PendingResolve, QueueState, DEFAULT_AUTOPLAY_DELAY, NEXT_REQUEST_TIMEOUT_MS,
    };
    use serde_json::{json, Value};
    use std::process::Command;

    #[test]
    fn advance_index_only_moves_inside_queue() {
        assert_eq!(advance_index(0, 0), None); // 空队列：没有下一格
        assert_eq!(advance_index(0, 1), None); // 只有一集
        assert_eq!(advance_index(0, 3), Some(1));
        assert_eq!(advance_index(2, 3), None); // 已在最后一格
        assert_eq!(advance_index(9, 3), None); // 脏索引不会绕回第一集
    }

    #[test]
    fn clamp_index_handles_empty_and_overflow() {
        assert_eq!(clamp_index(0, 0), 0);
        assert_eq!(clamp_index(7, 0), 0);
        assert_eq!(clamp_index(0, 5), 0);
        assert_eq!(clamp_index(4, 5), 4);
        assert_eq!(clamp_index(9, 5), 4);
    }

    #[test]
    fn timeout_is_exactly_at_budget() {
        assert!(!timed_out(0, NEXT_REQUEST_TIMEOUT_MS));
        assert!(!timed_out(
            NEXT_REQUEST_TIMEOUT_MS - 1,
            NEXT_REQUEST_TIMEOUT_MS
        ));
        assert!(timed_out(NEXT_REQUEST_TIMEOUT_MS, NEXT_REQUEST_TIMEOUT_MS));
        assert!(timed_out(
            NEXT_REQUEST_TIMEOUT_MS + 5,
            NEXT_REQUEST_TIMEOUT_MS
        ));
        // 契约 §3.3 写死 8 秒：改这里等于改协议，必须有意识
        assert_eq!(NEXT_REQUEST_TIMEOUT_MS, 8_000);
    }

    #[test]
    fn queue_command_matches_contract() {
        let items = vec![json!({"url": "u1", "ep": 3})];
        let cmd = queue_command(&items, 1, true, None);
        assert_eq!(cmd.get("cmd").and_then(|v| v.as_str()), Some("queue"));
        assert_eq!(cmd.get("index").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(cmd.get("autoplay").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            cmd.get("items").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        // 没给倒计时就不带字段：别每次更新队列都顺手把用户设的倒计时重置回默认
        assert!(cmd.get("autoplayDelay").is_none());

        let with_delay = queue_command(&items, 0, false, Some(3.0));
        assert_eq!(
            with_delay.get("autoplayDelay").and_then(|v| v.as_f64()),
            Some(3.0)
        );
    }

    #[test]
    fn item_episode_id_prefers_episode_id_then_vid() {
        assert_eq!(
            item_episode_id(&json!({"episodeId": "7512"})).as_deref(),
            Some("7512")
        );
        assert_eq!(
            item_episode_id(&json!({"vid": " 88 "})).as_deref(),
            Some("88")
        );
        // 空串视为没给，继续看下一个候选
        assert_eq!(
            item_episode_id(&json!({"episodeId": "", "vid": "9"})).as_deref(),
            Some("9")
        );
        assert_eq!(item_episode_id(&json!({"ep": 3})), None);
    }

    #[test]
    fn queue_state_defaults_are_legacy_safe() {
        let q = QueueState::default();
        assert!(q.items.is_empty());
        assert_eq!(q.index, 0);
        assert!(!q.autoplay); // 默认不联播 = 改造前行为
        assert_eq!(q.autoplay_delay, DEFAULT_AUTOPLAY_DELAY);
        assert!(q.pending.is_none());
        assert_eq!(q.token, 0);
    }

    // ── §5.4 联播兜底：Rust 自己解析下一集 ──

    #[test]
    fn queue_state_starts_without_pending_delivery() {
        // 进程刚起来时没有在飞的下发：残留一个会让第一次 requestNext 被
        // "同一 index 已在解析"挡住，用户看到的是第一集播完就再没动静。
        assert!(QueueState::default().pending_resolve.is_none());
    }

    #[test]
    fn numeric_vid_only_accepts_redguo_shaped_ids() {
        assert_eq!(numeric_vid("751234567890").as_deref(), Some("751234567890"));
        assert_eq!(
            numeric_vid("  751234567890  ").as_deref(),
            Some("751234567890")
        );
        // guo: 是红果云端链路（duanju_core.dll）的 id，原生 multi_video_model 不认。
        // 判死（→ 回落前端）比白烧一次匿名设备播放次数好。
        assert_eq!(numeric_vid("guo:751234567890"), None);
        // 少于 6 位：拦住"第 3 集"这类序号混进 vid 位置
        assert_eq!(numeric_vid("1234"), None);
        assert_eq!(numeric_vid(""), None);
        assert_eq!(numeric_vid("abc"), None);
    }

    #[test]
    fn item_vid_prefers_vid_then_episode_id() {
        assert_eq!(
            item_vid(&json!({"vid": "751234567890", "episodeId": "751234567999"})).as_deref(),
            Some("751234567890")
        );
        // 只有 episodeId 时也能用：红果的 episodes[].id 与 vid 同源
        assert_eq!(
            item_vid(&json!({"episodeId": "751234567999"})).as_deref(),
            Some("751234567999")
        );
        // JS 的 number 形态也认（手写配置/调试脚本会这么给）
        assert_eq!(
            item_vid(&json!({"vid": 751234567890u64})).as_deref(),
            Some("751234567890")
        );
        // 非红果源：两种 id 都不是纯数字 → 必须回落前端（Rust 没有它的解析器）
        assert_eq!(
            item_vid(&json!({"vid": "guo:123456789", "episodeId": "anime-42"})),
            None
        );
        assert_eq!(item_vid(&json!({"ep": 3})), None);
    }

    #[test]
    fn non_hongguo_numeric_episode_never_routes_to_hongguo() {
        for (series_id, episode_id) in [
            ("hg:1527", "1"),
            ("hg:1527", "123456"),
            ("dmghg:42", "987654"),
            ("guo:dsd:42", "123456"),
        ] {
            let item = json!({
                "seriesId": series_id,
                "episodeId": episode_id,
                "vid": episode_id,
                "url": "",
            });
            assert_eq!(
                item_vid(&item),
                None,
                "{series_id} 的分集 {episode_id} 必须回落本源，不能按数字形态走红果"
            );
        }
        assert_eq!(
            item_vid(&json!({"seriesId": "751234567890", "episodeId": "751234567999"})).as_deref(),
            Some("751234567999"),
            "红果剧集自己的数字 vid 仍应走原生解析"
        );
        let hg_item = json!({"index": 1, "seriesId": "hg:1527", "episodeId": "2", "url": ""});
        assert!(matches!(
            plan_next(&[hg_item], 1),
            NextPlan::ResolveHuangguo { series_id, episode_id, .. }
            if series_id == "hg:1527" && episode_id == "2"
        ));
    }

    #[test]
    fn numeric_anime_source_never_routes_to_hongguo() {
        let item = json!({
            "seriesId": "123456", "episodeId": "987654", "vid": "987654",
            "contentKind": "anime", "index": 0, "url": "",
        });
        assert_eq!(item_vid(&item), None, "动漫数字ID不能被当作红果来源");
        assert_eq!(plan_next(&[item], 0), NextPlan::Missing(NextGap::NoVid));
    }

    #[test]
    fn push_next_preserves_unrelated_queue_items() {
        let _guard = super::session_lifecycle_tests::lock_global();
        {
            let mut q = super::queue_slot().lock().unwrap();
            q.items = vec![
                json!({"index": 0, "seriesId": "hg:1527", "episodeId": "1", "url": ""}),
                json!({"index": 1, "seriesId": "hg:1527", "episodeId": "2", "url": ""}),
                json!({"index": 2, "seriesId": "hg:1527", "episodeId": "3", "url": ""}),
            ];
            q.index = 0;
            q.pending = None;
            q.pending_resolve = None;
        }

        // 没有活动管道时 send 会失败，但旧实现在此之前已经缩了队列；这里正是要
        // 用真实 push_next 行为抓住这个破坏，而不是只测一个新 helper。
        let _ = super::push_next(json!({
            "index": 1,
            "seriesId": "hg:1527",
            "episodeId": "2",
            "url": "https://cdn.test/huangguo-2.m3u8",
            "ep": 2,
        }));
        let snapshot = { super::queue_slot().lock().unwrap().items.clone() };
        {
            let mut q = super::queue_slot().lock().unwrap();
            q.items.clear();
            q.index = 0;
            q.pending = None;
            q.pending_resolve = None;
        }

        assert_eq!(snapshot.len(), 3, "回填一集不得丢掉整剧队列");
        assert_eq!(snapshot[0].get("url").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            snapshot[1].get("url").and_then(|v| v.as_str()),
            Some("https://cdn.test/huangguo-2.m3u8")
        );
        assert_eq!(
            snapshot[2].get("episodeId").and_then(|v| v.as_str()),
            Some("3")
        );
    }

    #[test]
    fn stale_cross_series_push_does_not_mutate_current_queue() {
        let _guard = super::session_lifecycle_tests::lock_global();
        let original = vec![
            json!({"index": 0, "seriesId": "hg:new", "episodeId": "1", "url": ""}),
            json!({"index": 1, "seriesId": "hg:new", "episodeId": "2", "url": ""}),
        ];
        {
            let mut q = super::queue_slot().lock().unwrap();
            q.items = original.clone();
            q.index = 0;
            q.pending = None;
            q.pending_resolve = None;
        }
        let result = super::push_next(json!({
            "index": 1, "seriesId": "hg:old", "episodeId": "2",
            "url": "https://cdn.test/old-series.m3u8", "ep": 2,
        }));
        let (items, index) = {
            let mut q = super::queue_slot().lock().unwrap();
            let snapshot = (q.items.clone(), q.index);
            q.items.clear();
            q.index = 0;
            snapshot
        };
        assert!(result.is_err());
        assert_eq!(items, original, "旧剧的迟到回执不能覆盖当前剧队列");
        assert_eq!(index, 0, "拒绝旧回执不能推进当前游标");
    }

    #[test]
    fn find_next_item_prefers_declared_index_over_position() {
        let items = vec![
            json!({"index": 7, "url": ""}),
            json!({"index": 8, "url": ""}),
        ];
        assert_eq!(
            find_next_item(&items, 8)
                .and_then(|v| v.get("index"))
                .and_then(|v| v.as_u64()),
            Some(8)
        );
        // 稀疏队列仍按声明序号查找：即使数组里只剩第 7 集，播放器请求 8 时
        // 也只能得到 NoItem，绝不能按数组位置错取第 7 集。
        let shrunk = vec![json!({"index": 7, "url": ""})];
        assert!(find_next_item(&shrunk, 8).is_none());
        // 老前端形状（项里没有 index 字段）才退回数组下标
        let legacy = vec![json!({"url": "u0"}), json!({"url": "u1"})];
        assert_eq!(
            find_next_item(&legacy, 1)
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str()),
            Some("u1")
        );
    }

    #[test]
    fn plan_next_finds_vid_for_the_requested_index() {
        let items = vec![
            json!({"index": 0, "url": "", "vid": "751234567890", "ep": 1}),
            json!({"index": 1, "url": "", "vid": "751234567891", "ep": 2}),
        ];
        match plan_next(&items, 1) {
            NextPlan::Resolve { item, vid } => {
                assert_eq!(vid, "751234567891");
                assert_eq!(item.get("ep").and_then(|v| v.as_u64()), Some(2));
            }
            other => panic!("期望 Resolve，实际 {other:?}"),
        }
    }

    #[test]
    fn plan_next_prefers_ready_url_and_reports_gaps() {
        // 项里已经有直链就不用解析
        let direct = vec![json!({"index": 3, "url": "https://cdn/x.mp4", "vid": "751234567890"})];
        assert!(matches!(plan_next(&direct, 3), NextPlan::Direct(_)));

        // 有项但没有可解析的 vid：回落前端（动漫源 / guo: 云端 id）
        let no_vid = vec![json!({"index": 2, "url": "", "episodeId": "guo:abc"})];
        assert_eq!(plan_next(&no_vid, 2), NextPlan::Missing(NextGap::NoVid));

        // 队列里根本没有这一项（前端没下发 / 这部剧到此为止）
        assert_eq!(plan_next(&no_vid, 9), NextPlan::Missing(NextGap::NoItem));
        assert_eq!(plan_next(&[], 0), NextPlan::Missing(NextGap::NoItem));
    }

    #[test]
    fn resolve_in_flight_only_blocks_the_same_index() {
        let p = PendingResolve { index: 5, seq: 9 };
        assert!(resolve_in_flight(Some(&p), 5));
        // 用户跳到另一集：那是另一次下发，不该被挡住
        assert!(!resolve_in_flight(Some(&p), 6));
        assert!(!resolve_in_flight(None, 5));
    }

    #[test]
    fn resolve_result_wanted_rejects_stale_rounds() {
        let pending = PendingNext {
            token: 4,
            index: 2,
            ep: 3,
        };
        let in_flight = PendingResolve { index: 2, seq: 4 };
        assert!(resolve_result_wanted(Some(pending), Some(in_flight), 4, 2));

        // 队列被换掉（换剧 / 用户跳集）：in_flight 已经不是自己这一轮
        assert!(!resolve_result_wanted(Some(pending), None, 4, 2));
        // 前端抢先推了回执 / 播放器自己换了集：pending 已经被清掉
        assert!(!resolve_result_wanted(None, Some(in_flight), 4, 2));
        // 更新的一轮 requestNext 已经顶掉旧的
        let newer = PendingNext {
            token: 5,
            index: 2,
            ep: 3,
        };
        assert!(!resolve_result_wanted(Some(newer), Some(in_flight), 4, 2));
        // 同一轮的 pending 但 index 对不上（脏负载）：也不认
        let dirty = PendingNext {
            token: 4,
            index: 7,
            ep: 8,
        };
        assert!(!resolve_result_wanted(Some(dirty), Some(in_flight), 4, 2));
    }

    #[test]
    fn build_next_load_maps_payload_without_touching_queue() {
        let item = json!({
            "index": 1,
            "title": "某剧",
            "ep": 2,
            // 前端在队列项里填的 total 是**总集数**，不是时长 —— 见断言 100 != 173
            "total": 100,
            "vid": "751234567891",
            "episodeId": "751234567891",
        });
        let payload = json!({
            "ok": true,
            "url": "https://cdn.example/x.mp4?sig=1",
            "content_key": "aabbcc",
            "width": 1080,
            "height": 1920,
            "duration_ms": 173000,
        });
        let load = build_next_load(&item, &payload, 1, 2, 40).expect("应能构造 load");
        assert_eq!(
            load.get("url").and_then(|v| v.as_str()),
            Some("https://cdn.example/x.mp4?sig=1")
        );
        assert_eq!(load.get("key").and_then(|v| v.as_str()), Some("aabbcc"));
        assert_eq!(load.get("ep").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(load.get("index").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(load.get("volume").and_then(|v| v.as_u64()), Some(40));
        assert_eq!(load.get("start").and_then(|v| v.as_f64()), Some(0.0));
        // total 是**秒**（duration_ms / 1000），不是队列项里的总集数
        assert_eq!(load.get("total").and_then(|v| v.as_f64()), Some(173.0));
        // 竖屏 1080x1920 → 补帧管线 540x960（与 open() 同一口径）
        assert_eq!(load.get("vw").and_then(|v| v.as_i64()), Some(540));
        assert_eq!(load.get("vh").and_then(|v| v.as_i64()), Some(960));
        assert_eq!(load.get("title").and_then(|v| v.as_str()), Some("某剧"));
        // 不带 queue：裸 load 在播放器等 requestNext 时就是回执，带 queue 会顺手
        // Reset 它自己的队列，与本模块的游标打架。cmd 由 deliver_next 补。
        assert!(load.get("queue").is_none());
        assert!(load.get("cmd").is_none());
        assert!(load.get("totalEps").is_none());

        // payload 里没有直链 = 这一集没解析出来 → 调用方据此回落前端
        assert!(build_next_load(&item, &json!({"url": ""}), 1, 2, 40).is_none());
        assert!(build_next_load(&item, &json!({}), 1, 2, 40).is_none());

        // ep 缺省（requestNext 没带）时用队列项里的集号
        let no_ep = build_next_load(&item, &payload, 1, 0, 40).expect("应能构造 load");
        assert_eq!(no_ep.get("ep").and_then(|v| v.as_u64()), Some(2));
    }

    #[test]
    fn danmaku_query_strips_guo_and_computes_window_end() {
        let q = prepare_danmaku_query(&json!({
            "seriesId": "guo:751234567890",
            "episodeId": "guo:751234567891",
            "vid": "751234567892",
            "startMs": 30000,
            "durationMs": 30000
        }))
        .expect("有效红果 id 必须放行");
        assert_eq!(q.norm_series, "751234567890");
        assert_eq!(q.norm_video, "751234567892");
        assert_eq!(q.echo_series, "guo:751234567890");
        assert_eq!(q.echo_episode, "guo:751234567891");
        assert_eq!(q.start_ms, 30_000);
        assert_eq!(q.end_ms, 60_000);

        // durationMs 缺失或越界时按 30_000ms 兜底 / 夹取
        let clamped = prepare_danmaku_query(&json!({
            "seriesId": "751234567890",
            "episodeId": "751234567891",
            "startMs": 1000,
            "durationMs": 999999
        }))
        .expect("缺失 vid 时应回退 episodeId");
        assert_eq!(clamped.norm_video, "751234567891");
        assert_eq!(clamped.end_ms, 121_000);
    }

    #[test]
    fn danmaku_query_rejects_non_hongguo_ids() {
        let err = prepare_danmaku_query(&json!({
            "seriesId": "hg:abc123",
            "episodeId": "1",
            "startMs": 0,
            "durationMs": 30000
        }))
        .expect_err("非红果源必须直接拦截并返回 error");
        assert_eq!(err.get("cmd").and_then(Value::as_str), Some("danmakuData"));
        assert_eq!(
            err.get("seriesId").and_then(Value::as_str),
            Some("hg:abc123")
        );
        assert_eq!(err.get("nextMs").and_then(Value::as_u64), Some(30_000));
        assert!(err.get("error").and_then(Value::as_str).is_some());
    }

    #[test]
    fn danmaku_data_reply_preserves_identity_and_errors() {
        let ok = build_danmaku_data_reply(
            "751",
            "ep-1",
            0,
            30_000,
            Ok(json!({
                "nextMs": 28500,
                "items": [{"id": "1", "text": "nice", "timeMs": 1200}]
            })),
        );
        assert_eq!(ok.get("cmd").and_then(Value::as_str), Some("danmakuData"));
        assert_eq!(ok.get("episodeId").and_then(Value::as_str), Some("ep-1"));
        assert_eq!(ok.get("nextMs").and_then(Value::as_u64), Some(28_500));
        assert!(ok.get("error").is_none());

        let fail = build_danmaku_data_reply("751", "ep-1", 30_000, 60_000, Err("超时".into()));
        assert_eq!(fail.get("nextMs").and_then(Value::as_u64), Some(60_000));
        assert_eq!(fail.get("error").and_then(Value::as_str), Some("超时"));
        assert_eq!(
            fail.get("items").and_then(Value::as_array).map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn danmaku_settings_event_clamps_and_sync_payload_aligns() {
        let mut settings = crate::models::UserSettings::default();
        apply_danmaku_settings_event(
            &mut settings,
            &json!({
                "danmakuEnabled": false,
                "danmakuArea": 5.0,
                "danmakuOpacity": 999.0,
                "danmakuFontScale": 300.0,
                "danmakuSpeed": 9,
                "danmakuFont": "   ",
                "danmakuColor": "#00ffaa",
                "danmakuBold": false,
                "danmakuStroke": "unknown"
            }),
        );
        crate::sanitize_danmaku_settings(&mut settings);
        assert!(!settings.danmaku_enabled);
        assert_eq!(settings.danmaku_area, 25.0);
        assert_eq!(settings.danmaku_opacity, 100.0);
        assert_eq!(settings.danmaku_font_scale, 200.0);
        assert_eq!(settings.danmaku_speed, 2);
        assert_eq!(settings.danmaku_font, "Microsoft YaHei");
        assert_eq!(settings.danmaku_color, "#00FFAA");
        assert!(!settings.danmaku_bold);
        assert_eq!(settings.danmaku_stroke, "heavy");

        let sync = settings_sync_fields(&settings);
        assert_eq!(
            sync.get("danmakuColor").and_then(Value::as_str),
            Some("#00FFAA")
        );
        assert_eq!(sync.get("vsrEnabled").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn build_next_load_preserves_series_episode_and_vid_identity() {
        let item = json!({
            "index": 4,
            "title": "测试短剧",
            "ep": 5,
            "seriesId": "751234567890",
            "episodeId": "ep_5",
            "vid": "751234567895",
        });
        let payload = json!({
            "url": "https://cdn.example/5.mp4",
            "content_key": "k5",
            "width": 1080,
            "height": 1920,
            "duration_ms": 90000,
        });
        let load = build_next_load(&item, &payload, 4, 5, 80).expect("load");
        assert_eq!(
            load.get("seriesId").and_then(Value::as_str),
            Some("751234567890")
        );
        assert_eq!(load.get("episodeId").and_then(Value::as_str), Some("ep_5"));
        assert_eq!(
            load.get("vid").and_then(Value::as_str),
            Some("751234567895")
        );
    }

    #[test]
    fn strip_parent_webview2_env_removes_host_browser_vars() {
        let mut cmd = Command::new("ttv-player.exe");
        strip_parent_webview2_env(&mut cmd);
        let removed: Vec<&str> = cmd
            .get_envs()
            .filter_map(|(k, v)| match v {
                None => k.to_str(),
                Some(_) => None,
            })
            .collect();
        assert!(removed.contains(&"WEBVIEW2_USER_DATA_FOLDER"));
        assert!(removed.contains(&"WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"));
        assert!(removed.contains(&"WEBVIEW2_BROWSER_EXECUTABLE_FOLDER"));
    }

    #[test]
    fn rollout_gate_is_open_in_production() {
        const {
            assert!(super::NATIVE_PLAYER_AVAILABLE);
        }
        assert!(super::ensure_native_player_available().is_ok());
    }

    #[test]
    fn seamless_autoplay_defaults_to_zero_delay() {
        assert_eq!(DEFAULT_AUTOPLAY_DELAY, 0.0);
        let items = vec![json!({"url": "", "ep": 2})];
        let cmd = queue_command(&items, 0, true, Some(DEFAULT_AUTOPLAY_DELAY));
        assert_eq!(cmd.get("autoplay").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(cmd.get("autoplayDelay").and_then(|v| v.as_f64()), Some(0.0));
    }
}

fn strip_parent_webview2_env(launch: &mut Command) {
    // 主进程 Tauri 会设置 WEBVIEW2_USER_DATA_FOLDER / WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS。
    // ttv-player.exe 若继承这组变量，WebView2 会尝试接入主进程正在运行的 browser 实例并因
    // 路径/启动参数冲突抛 COMException 0x8007139F，导致 HUD 彻底黑屏。
    for key in [
        "WEBVIEW2_USER_DATA_FOLDER",
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
    ] {
        launch.env_remove(key);
    }
}
