//! 主逻辑线程与渲染线程之间的跨线程共享状态
//!
//! # 设计
//!
//! [`FramePacket`] 每帧都会被覆盖：
//! - **主线程**在渲染线程读取前写入最新数据。
//! - **渲染线程**每轮迭代读取最新数据并处理。
//! - 无阻塞——渲染线程始终使用最新的可用数据。
//!
//! 外部叠加层（宿主 UI 等）的 CPU 帧数据经 [`overlay_messages`]
//! 传递：主线程把类型擦除的闭包（[`OverlayMessage`]）入队，渲染线程
//! 在每帧开始时取出并应用到叠加层。
//!
//! `running` 是一个 [`AtomicBool`]，主线程将其设为 `false` 以通知渲染线程退出。
//!
//! [`RenderStats`] 流向相反方向（渲染线程 → 主线程），
//! `pt_reset_requested` 同理（主线程 → 渲染线程）。

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use prism_engine::ecs::Entity;
use prism_engine::render_system::FramePacket;
use prism_render::asset_bridge::{AssetResolveRequest, AssetResolveResult};
use prism_render::external_overlay::OverlayMessage;
use prism_render::gpu_timer::GpuTimings;
use prism_render::GraphRenderer;

/// 进程启动基准时刻，仅用于启动期诊断打印。
static START_EPOCH: OnceLock<Instant> = OnceLock::new();

/// 自进程启动起经过的毫秒数（单调）。
pub fn startup_ms() -> u64 {
    START_EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 类型擦除的渲染器命令（主线程→渲染线程）：渲染线程每帧取出并应用到
/// `&mut GraphRenderer`（如 `set_environment` 在场景切换时按需重建 IBL）。
pub type RendererMessage = Box<dyn FnOnce(&mut GraphRenderer) + Send>;

// ---------------------------------------------------------------------------
// FrameImage — 渲染线程 → 宿主（编辑器）
// ---------------------------------------------------------------------------

/// 一帧离屏渲染结果（RGBA8，行优先，左上原点）。
///
/// 只有无窗口（`GraphRenderer::offscreen_new`）模式会产生它：宿主请求一帧后，
/// 渲染线程把 PostPass 的 tonemap 结果读回 CPU 并放在 [`RenderShared::frame`]。
#[derive(Clone, Debug)]
pub struct FrameImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

// ---------------------------------------------------------------------------
// RenderStats — 渲染线程 → 主线程
// ---------------------------------------------------------------------------

/// 渲染线程产生的每帧渲染指标，由主线程消费以显示编辑器 HUD。
#[derive(Clone, Debug, Default)]
pub struct RenderStats {
    /// 从 begin_frame 到 present 的总时间（毫秒）
    pub frame_time_ms: f32,
    /// 平滑后的每秒帧数
    pub fps: f32,
    /// 当前路径追踪采样数（None = 无 PT 通道）
    pub pt_frame_count: Option<u32>,
    /// 上一帧的 per-pass GPU 计时（None = 设备不支持时间戳）
    pub gpu_timings: Option<GpuTimings>,
}

// ---------------------------------------------------------------------------
// RenderShared
// ---------------------------------------------------------------------------

/// 主线程（写入者）与渲染线程（读取者）之间的共享状态。
pub struct RenderShared {
    /// 由主线程设为 `false` 以通知渲染线程退出。
    pub running: Arc<AtomicBool>,
    /// 来自游戏循环的最新帧数据包（主线程→渲染线程）
    pub packet: Mutex<Option<FramePacket>>,
    /// Wakes the renderer when a new packet or renderer-side command arrives.
    pub packet_ready: Condvar,
    /// 渲染线程用完归还的空包（渲染线程→主线程），主线程下一帧
    /// `clear()` + 重填，复用里面的 `Vec` 容量（issue #66）。
    pub spare_packet: Mutex<Option<FramePacket>>,
    /// 投递给外部叠加层的类型擦除消息队列（主线程→渲染线程）
    pub overlay_messages: Mutex<Vec<OverlayMessage>>,
    /// 投递给渲染器本身的类型擦除消息队列（主线程→渲染线程）。
    /// 用于跨线程调用 [`GraphRenderer`] 方法（如 `set_environment` 在
    /// 场景切换时按需重建 IBL），避免在渲染线程持有所有权时从主线程
    /// 直接借用 `&mut GraphRenderer`。
    pub renderer_messages: Mutex<Vec<RendererMessage>>,
    /// 来自渲染线程的最新渲染统计（渲染→主线程）。
    pub render_stats: Mutex<RenderStats>,
    /// 集合 `true` by main 线程 to request PT accumulation reset.
    pub pt_reset_requested: AtomicBool,
    /// 资产上传请求队列（主线程 CPU 段 → 渲染线程 GPU 段）。
    /// 主线程 `prepare_requests` 生成的纯数据请求在这里入队；
    /// 渲染线程每帧取走并交给 `GraphRenderer::apply_asset_requests`。
    pub asset_requests: Mutex<Vec<(Entity, AssetResolveRequest)>>,
    /// 资产上传结果队列（渲染线程 GPU 段 → 主线程 CPU 段）。
    /// 渲染线程完成上传后回传 `(Entity, AssetResolveResult)`；
    /// 主线程每帧取走并写回 `MeshRef`/`MaterialRef`。
    pub asset_results: Mutex<Vec<(Entity, AssetResolveResult)>>,
    /// 启动期计时标记（主线程 + 渲染线程共享，仅用于启动诊断打印）。
    pub startup_marks: Mutex<HashMap<&'static str, u64>>,
    /// 渲染线程无法继续时记录的致命错误（初始化失败等），由主线程取出并展示。
    pub render_failure: Mutex<Option<String>>,
    /// 宿主（编辑器）请求下一帧读回（主线程→渲染线程，一次性）。
    pub frame_requested: AtomicBool,
    /// 最近一次读回的离屏帧（渲染线程→主线程）。
    pub frame: Mutex<Option<FrameImage>>,
}

impl RenderShared {
    /// 创建 a new shared 状态 with `running = true`.
    pub fn new() -> (Arc<Self>, Arc<AtomicBool>) {
        let running = Arc::new(AtomicBool::new(true));
        let shared = Arc::new(Self {
            running: running.clone(),
            packet: Mutex::new(None),
            packet_ready: Condvar::new(),
            spare_packet: Mutex::new(None),
            overlay_messages: Mutex::new(Vec::new()),
            renderer_messages: Mutex::new(Vec::new()),
            render_stats: Mutex::new(RenderStats::default()),
            pt_reset_requested: AtomicBool::new(false),
            asset_requests: Mutex::new(Vec::new()),
            asset_results: Mutex::new(Vec::new()),
            startup_marks: Mutex::new(HashMap::new()),
            render_failure: Mutex::new(None),
            frame_requested: AtomicBool::new(false),
            frame: Mutex::new(None),
        });
        (shared, running)
    }

    // -------------------------------------------------------------------
    // Packet (main → 渲染
    // -------------------------------------------------------------------

    /// Submit the latest 帧 packet (main 线程 → 渲染 线程
    pub fn send_packet(&self, packet: FramePacket) {
        *self.packet.lock().unwrap() = Some(packet);
        self.packet_ready.notify_one();
    }

    /// 入队一条叠加层消息（主线程 → 渲染线程）。
    ///
    /// 消息是类型擦除的闭包：渲染线程在每帧开始时取出并应用到
    /// 外部叠加层（如"这是新的宿主 UI 帧"）。
    pub fn send_overlay_message(&self, msg: OverlayMessage) {
        self.overlay_messages.lock().unwrap().push(msg);
    }

    /// Take the latest 帧 packet 渲染 线程
    pub fn take_packet(&self) -> Option<FramePacket> {
        self.packet.lock().unwrap().take()
    }

    /// 取回渲染线程归还的空包（主线程）。
    ///
    /// 没有可复用的包时返回 `None`，调用方用 [`FramePacket::default`] 兜底。
    pub fn take_spare_packet(&self) -> Option<FramePacket> {
        self.spare_packet.lock().unwrap().take()
    }

    /// 渲染线程用完帧数据包后归还，供下一帧 `clear()` + 重填复用（issue #66）。
    ///
    /// 只保留最新一个：主线程没取走时覆盖旧的，避免无界堆积。
    pub fn recycle_packet(&self, packet: FramePacket) {
        *self.spare_packet.lock().unwrap() = Some(packet);
    }

    /// 取走所有排队中的叠加层消息（渲染线程）。
    pub fn take_overlay_messages(&self) -> Vec<OverlayMessage> {
        std::mem::take(&mut *self.overlay_messages.lock().unwrap())
    }

    // -------------------------------------------------------------------
    // 离屏帧读回（宿主模式）
    // -------------------------------------------------------------------

    /// 请求渲染线程在下一帧把 tonemap 结果读回 CPU（主线程 → 渲染线程）。
    ///
    /// 一次性标志：渲染线程在**有可用帧**时消费；宿主每 tick 重新请求即可，
    /// 掉帧时自然降频，不会堆积请求。
    pub fn request_frame(&self) {
        self.frame_requested.store(true, Ordering::Relaxed);
        self.packet_ready.notify_one();
    }

    /// Wake the render thread without requesting a CPU readback.
    pub fn wake_renderer(&self) {
        self.packet_ready.notify_one();
    }

    /// 取走最近读回的帧（主线程）；没有新帧时返回 `None`。
    pub fn take_frame(&self) -> Option<FrameImage> {
        self.frame.lock().unwrap().take()
    }

    // -------------------------------------------------------------------
    // Renderer messages (main → 渲染线程)
    // -------------------------------------------------------------------

    /// 入队一条渲染器命令（主线程 → 渲染线程）。渲染线程在每帧开始时取出并
    /// 应用到 `&mut GraphRenderer`。用于跨线程触发 `set_environment` 等。
    pub fn send_renderer_message(&self, msg: RendererMessage) {
        self.renderer_messages.lock().unwrap().push(msg);
        self.packet_ready.notify_one();
    }

    /// 取走所有排队中的渲染器命令（渲染线程）。
    pub fn take_renderer_messages(&self) -> Vec<RendererMessage> {
        std::mem::take(&mut *self.renderer_messages.lock().unwrap())
    }

    // -------------------------------------------------------------------
    // 资产解析（主线程 CPU 段 ↔ 渲染线程 GPU 段）
    // -------------------------------------------------------------------

    /// 入队一批资产上传请求（主线程 → 渲染线程）。
    pub fn enqueue_asset_requests(&self, reqs: Vec<(Entity, AssetResolveRequest)>) {
        if reqs.is_empty() {
            return;
        }
        self.asset_requests.lock().unwrap().extend(reqs);
    }

    /// 取走所有排队中的资产上传请求（渲染线程）。
    pub fn take_asset_requests(&self) -> Vec<(Entity, AssetResolveRequest)> {
        std::mem::take(&mut *self.asset_requests.lock().unwrap())
    }

    /// 回传一批资产上传结果（渲染线程 → 主线程）。
    pub fn push_asset_results(&self, results: Vec<(Entity, AssetResolveResult)>) {
        if results.is_empty() {
            return;
        }
        self.asset_results.lock().unwrap().extend(results);
    }

    /// 取走所有排队中的资产上传结果（主线程）。
    pub fn take_asset_results(&self) -> Vec<(Entity, AssetResolveResult)> {
        std::mem::take(&mut *self.asset_results.lock().unwrap())
    }

    // -------------------------------------------------------------------
    // 启动计时（主线程 + 渲染线程共享）
    // -------------------------------------------------------------------

    /// 记录一个启动里程碑（相对进程启动时刻的毫秒数）。
    pub fn mark(&self, key: &'static str) {
        self.set_mark(key, startup_ms());
    }

    /// 写入一个指定毫秒数的启动里程碑（主线程一次性写入跨线程已知时刻）。
    pub fn set_mark(&self, key: &'static str, ms: u64) {
        self.startup_marks.lock().unwrap().insert(key, ms);
    }

    /// 首帧呈现后打印启动汇总。各里程碑在 `mark` 时收集，此处计算相对差值：
    /// `event_loop_free`（resumed 返回，证明事件循环已解锁）、`window`
    /// （建窗耗时）、`renderer`（渲染线程建渲染器耗时）、`warmup`（管线预热）、
    /// `first_frame`（首帧呈现总耗时）。
    pub fn print_startup_report(&self) {
        let m = self.startup_marks.lock().unwrap();
        let get = |k: &str| m.get(k).copied();
        let resumed = match get("resumed_entry") {
            Some(v) => v,
            None => return,
        };
        let spawned = get("render_thread_spawned").unwrap_or(resumed);
        let window_ms = get("window_built").map(|v| v.saturating_sub(resumed));
        let renderer_ms = get("renderer_built").map(|v| v.saturating_sub(spawned));
        let warmup_ms = get("warmup_done")
            .zip(get("renderer_built"))
            .map(|(w, r)| w.saturating_sub(r));
        let first_frame_ms = get("first_frame").map(|v| v.saturating_sub(resumed));
        let event_loop_free = spawned.saturating_sub(resumed);

        log::info!(
            "[startup] event_loop_free={}ms window={:?}ms renderer={:?}ms warmup={:?}ms first_frame={:?}ms",
            event_loop_free,
            window_ms,
            renderer_ms,
            warmup_ms,
            first_frame_ms,
        );
    }

    // -------------------------------------------------------------------
    // RenderStats 渲染 → main)
    // -------------------------------------------------------------------

    /// 写入 latest 渲染 stats 渲染 线程
    pub fn set_render_stats(&self, stats: RenderStats) {
        *self.render_stats.lock().unwrap() = stats;
    }

    /// 读取 latest 渲染 stats (main 线程
    pub fn read_render_stats(&self) -> RenderStats {
        self.render_stats.lock().unwrap().clone()
    }

    // -------------------------------------------------------------------
    // PT reset (main → 渲染
    // -------------------------------------------------------------------

    /// Request PT accumulation reset (main 线程
    pub fn request_pt_reset(&self) {
        self.pt_reset_requested.store(true, Ordering::Relaxed);
    }

    /// Take and 清空 PT reset flag 渲染 线程
    pub fn take_pt_reset(&self) -> bool {
        self.pt_reset_requested.swap(false, Ordering::Relaxed)
    }

    // -------------------------------------------------------------------
    // Render-thread failure (渲染 → main)
    // -------------------------------------------------------------------

    /// Record a fatal render-thread error so the main thread can surface it.
    pub fn fail_render(&self, message: String) {
        *self.render_failure.lock().unwrap() = Some(message);
    }

    /// Take the recorded render-thread error, if any.
    pub fn take_render_failure(&self) -> Option<String> {
        self.render_failure.lock().unwrap().take()
    }

    /// 只读当前错误（不消费），供宿主轮询展示。
    pub fn read_render_failure(&self) -> Option<String> {
        self.render_failure.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_failure_is_reported_once() {
        let (shared, _running) = RenderShared::new();
        assert!(shared.take_render_failure().is_none());

        shared.fail_render("boom".to_string());
        assert_eq!(shared.take_render_failure().as_deref(), Some("boom"));
        assert!(shared.take_render_failure().is_none());
    }
}
