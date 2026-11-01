//! [`App`] — 平台应用层，拥有事件循环。
//!
//! 窗口后端由 feature 决定（见 crate 文档）：`winit-backend` 走
//! [`ApplicationHandler`]，`win32-native` 走 [`App::run_win32`] 的
//! Win32 消息循环。两者共用同一份引擎/渲染/关闭逻辑
//! （[`App::tick_frame`] / [`App::shutdown`]），差别只在窗口与事件来源。
//!
//! # 架构（渲染线程 = 后台线程）
//!
//! ```text
//! 主线程 (窗口事件)               渲染线程
//!   ──────────────────────────         ──────────────
//! about_to_wait: 循环
//!     engine.fixed_update × N             take_packet()
//!     engine.update                       begin_frame()
//!     engine.late_update                  execute(packet)
//!     audio.update                        present()
//!     extract_frame_packet ──packet──►
//!     frame_hook.on_tick ──overlay_msg──►
//! ```
//!
//! 渲染线程独立于窗口事件运行。垂直同步仅阻塞渲染线程——主线程继续执行。
//!
//! **初始化**（窗口在主线程快速创建，渲染器在渲染线程异步构建）：
//! ```text
//!   App::new:
//!     Engine::empty → pre_init → init_core → init_config → init_resources
//!     → init_scene → runtime_initialize
//!   [resumed]:
//!     PlatformContext::create_window（主线程，~数毫秒）→ 计算扩展名/场景光照
//!     → 生成渲染线程（窗口启动即返回，事件不被阻塞）
//!   [渲染线程]:
//!     GraphRenderer::new（~数百毫秒，不阻塞主线程）→ warmup → set_environment
//!     → 进入帧循环；资源解析经 asset_requests/asset_results 通道异步完成
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(feature = "audio")]
use prism_audio::AudioEngine;
use prism_engine::asset_resolver::GpuAssetResolver;
use prism_engine::config::AppConfig;
use prism_engine::input::InputManager;
use prism_engine::render_settings::RenderSettings;
use prism_engine::render_system::extract_frame_packet_into_with_shadow;
use prism_engine::Engine;
use prism_platform::{
    required_vulkan_extensions, Key, Modifiers, PlatformContext, PlatformEvent, PlatformWindow,
    WindowConfig,
};
#[cfg(feature = "winit-backend")]
use winit::application::ApplicationHandler;
#[cfg(feature = "winit-backend")]
use winit::event::{DeviceEvent, WindowEvent};
#[cfg(feature = "winit-backend")]
use winit::event_loop::{ActiveEventLoop, ControlFlow};
#[cfg(feature = "winit-backend")]
use winit::window::WindowId;

use crate::debug_hud::DebugHud;
use crate::hook::FrameHook;
use crate::render_runner::{render_thread_main, OverlayFactory, RenderTarget};
use crate::render_shared::{FrameImage, RenderShared, RenderStats};

// ===========================================================================
// Subsystem 枚举
// ===========================================================================

/// 引擎子系统，用户项目通过 [`App::with_subsystem`] 显式声明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subsystem {
    /// 渲染子系统（窗口 + Vulkan + 渲染线程）。
    Render,
    /// 音频引擎（AudioEngine + 音频解码线程）。
    Audio,
    /// 物理模拟（Rapier 后台线程）。
    Physics,
    /// 网络子系统
    Network,
    /// 场景系统
    Scene,
    /// 资源系统
    Asset,
    AI,
    UI,
    Animation,
}

impl Subsystem {
    fn bit(self) -> u32 {
        match self {
            Subsystem::Render => 1 << 0,
            Subsystem::Audio => 1 << 1,
            Subsystem::Physics => 1 << 2,
            Subsystem::Network => 1 << 3,
            Subsystem::Scene => 1 << 4,
            Subsystem::Asset => 1 << 5,
            Subsystem::AI => 1 << 6,
            Subsystem::UI => 1 << 7,
            Subsystem::Animation => 1 << 8,
        }
    }
}

// ===========================================================================
// App
// ===========================================================================

/// Application shell：拥有引擎、窗口与渲染线程。
pub struct App {
    // ---------- 启动配置 ----------
    config: AppConfig,

    /// 已启用的子系统位掩码。
    subsystems: u32,

    // ---------- 引擎（主线程） ----------
    engine: Option<Engine>,
    asset_resolver: GpuAssetResolver,

    // ---------- 渲染线程 ----------
    render_shared: Option<Arc<RenderShared>>,
    render_running: Option<Arc<AtomicBool>>,
    render_thread: Option<JoinHandle<()>>,

    // ---------- 窗口上下文 ----------
    /// 主线程持有的窗口（`Arc` 与渲染线程共享）。`None` 表示尚未创建。
    platform: Option<PlatformContext>,

    // ---------- 帧钩子（编辑器等宿主注入） ----------
    frame_hook: Option<Box<dyn FrameHook>>,
    render_settings: RenderSettings,
    /// 运行时诊断 HUD（F3；无 UI 框架依赖）。
    debug_hud: DebugHud,

    // ---------- 每帧状态 ----------
    display_aspect: f32,
    /// 开发期资产热重载（#495）：`PRISMREV_HOT_RELOAD` 打开时轮询 `.pak`。
    ///
    /// 发布版默认关（不建线程、不做任何 stat）；打开后重建 `.pak` 不用退出
    /// 再进游戏。旧 GPU 句柄不会立即释放（渲染器还没有按句柄回收的接口），
    /// 所以只适合开发期反复迭代。
    pak_watcher: Option<prism_engine::scene::hot_reload::HotReloadWatcher>,
    surface_rotation: glam::Mat4,

    // ---------- 输入（主线程） ----------
    input: InputManager,

    // ---------- 音频（主线程，`audio` feature） ----------
    #[cfg(feature = "audio")]
    audio: Option<AudioEngine>,
    #[cfg(feature = "audio")]
    audio_decode_thread: Option<JoinHandle<()>>,
    #[cfg(feature = "audio")]
    audio_decode_tx: Option<flume::Sender<crate::audio_decode_runner::DecodeRequest>>,
    #[cfg(feature = "audio")]
    audio_decode_rx: Option<flume::Receiver<crate::audio_decode_runner::DecodeResult>>,
    /// 场景音频驱动：把 ECS 的 `AudioSource` 接到混音器（#508）。
    #[cfg(feature = "audio")]
    scene_audio: crate::scene_audio::SceneAudio,
    /// `SceneAudio` 声部 id → 真实播放句柄。
    #[cfg(feature = "audio")]
    audio_voices: crate::scene_audio::VoiceHandles,

    // ---------- 物理（主线程 + 独立步进线程，`physics` feature） ----------
    /// 每个固定步把 ECS 里的 `Rigidbody` / `Collider` 送进 rapier，并把结果写回
    /// `LocalTransform`（#522）。`None` = 未启用 / 构建没有 physics feature。
    #[cfg(feature = "physics")]
    physics: Option<crate::physics_runner::PhysicsRuntime>,

    // ---------- 窗口大小调整 ----------
    needs_resize: bool,

    /// 宿主（编辑器）模式下的离屏渲染尺寸。
    ///
    /// `Some` 表示本实例没有 winit 窗口：`tick_sim` 用它写 `ScreenSize`/宽高比，
    /// 渲染线程跑 `RenderTarget::Offscreen`，帧经 [`RenderShared::take_frame`] 读回。
    host_extent: Option<(u32, u32)>,
    /// Optional native window surface supplied by an embedding host.
    host_surface: Option<(prism_platform::SendWindowHandles, Vec<String>)>,
    host_native_surface: bool,

    /// 最近一次修饰键快照（winit 的 `ModifiersChanged` / Win32 的 `GetKeyState`）。
    ///
    /// 键盘事件本身不携带修饰键，PBR 分量调试快捷键
    /// （Shift+1..5 = 分量 bit 10..14）只能从这里判断 Shift 是否按下，
    /// 否则那 5 个 bit 永远不可达。
    modifiers: Modifiers,

    // ---------- lifecycle ----------
    fatal_error: Option<String>,
    /// 后台挂起标志：`suspended()` 置位、`resumed()` 复位。置位期间
    /// `about_to_wait` 跳过 `tick_sim`，防止后台空转烧电（T4）。
    suspended: bool,

    // ---------- timing ----------
    last_frame: Option<Instant>,
    /// Fixed-step accumulator in nanoseconds. Keeping this as an integer
    /// avoids float drift and makes simulation time independent of event rate.
    fixed_accumulator: Duration,
}

/// 把场景里写的资产路径归一化成 pak 内的相对路径。
///
/// 场景作者常写 `assets/` 前缀或相对路径（`../x.hdr`），而 pak 的键是
/// 去掉 `assets/` 且已经解析过 `.`/`..` 的相对路径；不归一化就会查不到。
fn normalize_asset_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let stripped = unified.strip_prefix("assets/").unwrap_or(unified.as_str());
    let mut parts: Vec<&str> = Vec::new();
    for segment in stripped.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

impl App {
    /// 无参构造 = 默认配置的完整引擎（无演示内容）。
    pub fn new() -> Self {
        Self::with_config(crate::load_config())
    }

    /// 用指定配置创建应用，并跑完所有引擎初始化阶段。
    ///
    /// 用户项目在此之后通过 [`Self::engine_mut`] / [`Self::add_system`] /
    /// [`Self::insert_resource`] 注册自己的 ECS 内容，最后调用
    /// [`Self::run`]（桌面）或 [`Self::run_on`]（Android）启动事件循环。
    pub fn with_config(config: AppConfig) -> Self {
        Self::build(config, true)
    }

    /// 宿主（编辑器）模式：**不加载 manifest 场景、不创建 winit 窗口**。
    ///
    /// 编辑器持有 engine 实例，场景由 [`Self::load_scene_json_file`] 从当前
    /// 编辑中的 `.scene.json` 载入，渲染走离屏目标，帧由宿主取走。
    pub fn hosted(config: AppConfig) -> Self {
        Self::build(config, false)
    }

    /// Hosted engine backed by a host-owned native window surface.
    ///
    /// The host window must outlive the App and its render thread.
    pub fn hosted_surface(
        config: AppConfig,
        handles: prism_platform::SendWindowHandles,
        extensions: Vec<String>,
    ) -> Self {
        let mut app = Self::build(config, false);
        app.host_surface = Some((handles, extensions));
        app
    }

    fn build(config: AppConfig, load_scene: bool) -> Self {
        let mut engine = Engine::empty();

        // Phase 1 – Subsystem registration

        // 资源 loading：ResourceManager 由应用层持有并注入引擎。
        let mut asset_resolver = GpuAssetResolver::new();
        asset_resolver.load_resource_package();

        if load_scene {
            // 场景 loading — 启动配置可指定场景名（编辑器/启动器预览用）；
            // 未指定时沿用「manifest 第一个可解析场景」的默认行为。
            let launch = prism_engine::launch_config::LaunchConfig::from_env();
            engine.init_scene_named(
                &mut asset_resolver.resource_manager,
                launch.scene.as_deref(),
            );
        }

        // 存档目录跟随工程 `[app] company/name`（AppConfig 已从 settings 读好）。
        // 以前 `scene_state_path()` 用默认配置，所有项目共用一个目录（#484）。
        if let Some(dir) = prism_engine::config::app_data_dir_for(&config) {
            engine.set_save_directory(dir);
        }

        // 运行时 startup callbacks
        engine.runtime_initialize();

        // 项目级渲染默认值（`[render]` 段）。
        let mut render_settings = RenderSettings::default();
        config.render.apply(&mut render_settings);

        Self {
            config,
            subsystems: 0,
            engine: Some(engine),
            asset_resolver,
            render_shared: None,
            render_running: None,
            render_thread: None,
            platform: None,
            frame_hook: None,
            render_settings,
            debug_hud: DebugHud::default(),
            display_aspect: 16.0 / 9.0,
            pak_watcher: spawn_pak_watcher(),
            surface_rotation: glam::Mat4::IDENTITY,
            input: InputManager::new(),
            #[cfg(feature = "audio")]
            audio: None,
            #[cfg(feature = "audio")]
            audio_decode_thread: None,
            #[cfg(feature = "audio")]
            audio_decode_tx: None,
            #[cfg(feature = "audio")]
            audio_decode_rx: None,
            #[cfg(feature = "audio")]
            scene_audio: crate::scene_audio::SceneAudio::new(),
            #[cfg(feature = "audio")]
            audio_voices: crate::scene_audio::VoiceHandles::default(),
            #[cfg(feature = "physics")]
            physics: None,
            needs_resize: false,
            host_extent: None,
            host_surface: None,
            host_native_surface: false,
            modifiers: Modifiers::empty(),
            fatal_error: None,
            suspended: false,
            last_frame: None,
            fixed_accumulator: Duration::ZERO,
        }
    }

    // -----------------------------------------------------------------------
    // ECS 接入（用户项目）
    // -----------------------------------------------------------------------

    /// 完全 ECS 访问——world / schedule / timer 等入口都通过 `Engine`。
    pub fn engine_mut(&mut self) -> &mut Engine {
        self.engine.as_mut().expect("engine alive")
    }

    /// 设置一条音频总线的增益（#508）；`Master` 乘在其它所有总线上。
    ///
    /// 没有 `audio` feature 时是 no-op，方便项目代码无条件调用。
    pub fn set_audio_bus_volume(
        &mut self,
        bus: prism_engine::scene::components::AudioBus,
        volume: f32,
    ) -> &mut Self {
        #[cfg(feature = "audio")]
        self.scene_audio.set_bus_volume(bus, volume);
        #[cfg(not(feature = "audio"))]
        let _ = (bus, volume);
        self
    }

    /// 读取一条音频总线自己的增益（不含 `Master`）；无 `audio` feature 时为 1。
    pub fn audio_bus_volume(&self, bus: prism_engine::scene::components::AudioBus) -> f32 {
        #[cfg(feature = "audio")]
        {
            self.scene_audio.bus_volume(bus)
        }
        #[cfg(not(feature = "audio"))]
        {
            let _ = bus;
            1.0
        }
    }

    /// 注册一个 ECS system（等价于 `engine_mut().schedule_mut().add_system`）。
    ///
    /// 引擎默认调度（UI 基础设施）在构造时已并入，这里注册的系统
    /// 在其之后按注册顺序每帧运行。
    pub fn add_system<F>(&mut self, label: &str, f: F) -> &mut Self
    where
        F: FnMut(&mut prism_engine::ecs::World, f32) + 'static,
    {
        self.engine_mut().schedule_mut().add_system(label, f);
        self
    }

    /// 插入一个 ECS 资源（等价于 `engine_mut().world_mut().insert_resource`）。
    pub fn insert_resource<R: 'static + Send + Sync>(&mut self, resource: R) -> &mut Self {
        self.engine_mut().world_mut().insert_resource(resource);
        self
    }

    /// 修改应用级渲染设置；用户项目可在启动时选择 Raster 或 PathTrace，
    /// 不需要依赖编辑器或直接访问渲染器。
    pub fn render_settings_mut(&mut self) -> &mut RenderSettings {
        &mut self.render_settings
    }

    /// 以闭包配置应用级渲染设置。
    pub fn with_render_settings(mut self, configure: impl FnOnce(&mut RenderSettings)) -> Self {
        configure(&mut self.render_settings);
        self
    }

    /// 注入帧钩子（编辑器等宿主）。钩子在渲染线程启动前被询问外部
    /// 叠加层工厂，之后每帧收到 tick 与窗口事件转发。
    pub fn with_frame_hook(mut self, hook: impl FrameHook + 'static) -> Self {
        self.frame_hook = Some(Box::new(hook));
        self
    }

    /// 显式声明需要启用某个引擎子系统。
    ///
    /// 不调用此方法则对应子系统不创建（无窗口、无音频、无物理等）。
    pub fn with_subsystem(mut self, subsystem: Subsystem) -> Self {
        let bit = subsystem.bit();
        if self.subsystems & bit != 0 {
            return self; // 已启用，跳过
        }
        self.subsystems |= bit;

        match subsystem {
            Subsystem::Render => {
                // 渲染在 resumed() 时条件创建
            }
            Subsystem::Physics => {
                #[cfg(feature = "physics")]
                {
                    // #522：物理线程每个固定步积分 `Rigidbody`，结果写回
                    // `LocalTransform`；`Collider` 同时继续服务射线查询。
                    self.physics = Some(crate::physics_runner::PhysicsRuntime::spawn());
                }
                #[cfg(not(feature = "physics"))]
                log::warn!(
                    "physics subsystem requested but this build has no `physics` feature — \
                     colliders remain query-only; rebuild with `--features physics` to step bodies"
                );
            }
            Subsystem::Audio => {
                #[cfg(feature = "audio")]
                {
                    self.audio = AudioEngine::new(prism_audio::AudioConfig::default()).ok();
                    if self.audio.is_none() {
                        log::warn!("audio subsystem requested but failed to initialize");
                    }
                    self.start_audio_decode_thread();
                }
                #[cfg(not(feature = "audio"))]
                log::warn!(
                    "audio subsystem requested but this build has no `audio` feature — \
                     rebuild with `--features audio` to enable sound"
                );
            }
            Subsystem::Network | Subsystem::AI => {
                log::warn!(
                    "{:?} subsystem requested — not implemented yet, ignoring",
                    subsystem
                )
            }
            Subsystem::Scene | Subsystem::Asset | Subsystem::UI | Subsystem::Animation => {
                // 场景/资产/UI/动画本来就是常开的引擎能力，没有单独的开关要拨。
                log::debug!("{:?} subsystem is always on — nothing to start", subsystem)
            }
        }
        self
    }

    /// 检查是否启用了指定子系统。
    pub fn has_subsystem(&self, subsystem: Subsystem) -> bool {
        self.subsystems & subsystem.bit() != 0
    }

    // -----------------------------------------------------------------------
    // 宿主（编辑器）模式
    // -----------------------------------------------------------------------

    /// 当前渲染表面的逻辑尺寸（像素）。
    ///
    /// 宿主模式用 [`Self::host_extent`]，游戏模式取窗口客户区尺寸。
    fn surface_size(&self) -> (u32, u32) {
        if let Some(size) = self.host_extent {
            return size;
        }
        self.platform
            .as_ref()
            .map(|platform| platform.window().inner_size())
            .unwrap_or((1920, 1080))
    }

    /// 以**离屏目标**启动渲染线程（编辑器视口）。
    ///
    /// 与 [`Self::run`]/[`Self::run_on`] 互斥：宿主模式不创建 winit 窗口，
    /// 由宿主按自己的节奏调用 [`Self::host_tick`] 并取走
    /// [`Self::take_hosted_frame`] 返回的像素。
    pub fn start_hosted(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        self.host_extent = Some((width, height));
        self.display_aspect = width as f32 / height as f32;
        let scene_env = self.engine.as_ref().and_then(|engine| {
            engine.current_scene_env_bytes_with_provider(&mut self.asset_resolver.resource_manager)
        });
        let target = match self.host_surface.take() {
            Some((handles, extensions)) => {
                self.host_native_surface = true;
                RenderTarget::NativeSurface {
                    handles,
                    extensions,
                    vsync: false,
                }
            }
            None => RenderTarget::Offscreen { width, height },
        };
        self.spawn_render_thread_with(target, scene_env, None);
    }

    /// 调整离屏渲染尺寸（面板 resize）；下一帧起生效。
    pub fn host_resize(&mut self, width: u32, height: u32) {
        let size = (width.max(1), height.max(1));
        if self.host_extent == Some(size) {
            return;
        }
        self.host_extent = Some(size);
        self.display_aspect = size.0 as f32 / size.1 as f32;
        if let Some(shared) = self.render_shared.as_ref() {
            let (w, h) = size;
            shared.send_renderer_message(Box::new(move |renderer| {
                let result = if renderer.is_headless() {
                    renderer.resize_offscreen(w, h)
                } else {
                    renderer.recreate_swapchain()
                };
                if let Err(e) = result {
                    log::warn!("host resize ({w}x{h}) failed: {e:#}");
                }
            }));
        }
    }

    /// 宿主驱动的一帧：模拟（可选）+ extract + 提交渲染 + 请求回读。
    ///
    /// `simulate = false` 即编辑器「编辑态」：只传播层级，不跑游戏 system。
    pub fn host_tick(&mut self, simulate: bool) {
        self.tick_sim(simulate);
        if let Some(shared) = self.render_shared.as_ref() {
            if self.host_native_surface {
                shared.wake_renderer();
            } else {
                shared.request_frame();
            }
        }
    }

    /// 取走渲染线程最近读回的一帧（RGBA8）。
    pub fn take_hosted_frame(&self) -> Option<FrameImage> {
        self.render_shared
            .as_ref()
            .and_then(|shared| shared.take_frame())
    }

    /// 渲染线程的致命错误（初始化失败 / 线程退出）。宿主模式用它给面板报错。
    pub fn render_failure(&self) -> Option<String> {
        self.render_shared
            .as_ref()
            .and_then(|shared| shared.read_render_failure())
    }

    /// 渲染线程的实时统计（FPS / 帧时间）。
    pub fn render_stats(&self) -> Option<RenderStats> {
        self.render_shared
            .as_ref()
            .map(|shared| shared.read_render_stats())
    }

    /// 输入句柄：宿主把鼠标/键盘事件写进同一个 [`InputManager`]。
    pub fn input_manager_mut(&mut self) -> &mut InputManager {
        &mut self.input
    }

    /// 停止宿主渲染线程并收尾（音频 / 引擎 pre/post shutdown）。
    pub fn shutdown_hosted(&mut self) {
        if let Some(watcher) = self.pak_watcher.as_mut() {
            watcher.stop();
        }
        self.pak_watcher = None;
        #[cfg(feature = "audio")]
        self.stop_audio_decode_thread();
        self.stop_render_thread();
        if let Some(engine) = self.engine.as_mut() {
            engine.pre_shutdown();
        }
        #[cfg(feature = "audio")]
        self.audio.take();
        if let Some(engine) = self.engine.as_mut() {
            engine.post_shutdown();
        }
    }

    /// 与渲染线程做一次同步：等它处理完**已经排队**的资产上传，然后丢弃结果。
    ///
    /// 重新加载场景会把世界整体换掉，而 `prism-ecs` 的实体 id 从 0 重新分配；
    /// 旧场景在途的上传结果一旦晚到，就会挂到新场景同 id 的实体上（错误网格）。
    /// 渲染线程每帧先处理 `asset_requests`、再处理 `renderer_messages`，所以
    /// 一条消息返回即代表之前的请求都已应用到结果队列。
    fn sync_render_thread(&self) {
        let Some(shared) = self.render_shared.as_ref() else {
            return;
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        shared.send_renderer_message(Box::new(move |_| {
            let _ = done_tx.send(());
        }));
        if done_rx.recv_timeout(Duration::from_millis(500)).is_err() {
            log::warn!("scene reload: render thread sync timed out");
        }
        let _ = shared.take_asset_results();
    }

    /// 从松散的 `.scene.json` 加载 / 切换场景（宿主 / 编辑器）。
    ///
    /// **复用同一个 `Engine`**（#482）：`Engine::load_scene_json_file` 是 `Single`
    /// 模式——先加载新场景、成功后才卸载旧场景，`Persistent` 实体（等价
    /// `DontDestroyOnLoad`）连同子树活过换场；注册过的 system / schedule 也不会
    /// 被冲掉。
    ///
    /// 旧场景的 GPU 句柄由资源解析器回收：被卸载的实体不再出现在世界里，
    /// `prepare_requests` 会把它们的纹理 / 网格 / 材质槽交回渲染器（#495）。
    /// 只有第一次加载才新建 engine 与资源解析器（`.pak` 也只挂一次）。
    ///
    /// 返回**文档顺序**的实体表（与 `.scene.json` 的 `entities` 下标一一对应）。
    pub fn load_scene_json_file(
        &mut self,
        path: &Path,
    ) -> Result<Vec<prism_engine::ecs::Entity>, String> {
        self.sync_render_thread();
        if self.engine.is_none() {
            self.engine = Some(Engine::empty());
            self.asset_resolver = GpuAssetResolver::new();
            self.asset_resolver.load_resource_package();
            self.pak_watcher = spawn_pak_watcher();
        }
        let engine = self.engine.as_mut().expect("engine present");
        let entities = engine.load_scene_json_file(path)?;
        engine.runtime_initialize();

        self.last_frame = None;
        self.fixed_accumulator = Duration::ZERO;
        self.reset_persistent_play_state();
        self.apply_scene_environment();
        Ok(entities)
    }

    /// #509：按需挂载 `assets/optional/` 下的分包（`optional_*.pak`）。
    ///
    /// 启动时的自动挂载只扫描 `assets/` 顶层，所以分包里的资源不占常驻内存。
    /// 进入需要它们的关卡 / 玩法时调用；已经挂过的包会跳过，重复调用安全。
    /// 挂载后之前因为「路径不在包里」失败过的实体会重新解析（否则失败缓存会
    /// 挡住它们）。返回**本次新挂载**的包数。
    pub fn load_optional_packages(&mut self) -> usize {
        let count = self.asset_resolver.load_optional_packages();
        if count > 0 {
            if let Some(engine) = self.engine.as_mut() {
                self.asset_resolver
                    .retry_failed_after_package_change(engine.world_mut());
            }
        }
        count
    }

    /// 换场后清掉只对「上一场」有意义的播放状态。
    ///
    /// 资源解析器按 `(实体, 槽位)` 记账（#495）；旧实体已经被卸载，这里让
    /// 缓存立刻丢弃它们的条目，而不是等下一次 `prepare_requests` 才清理。
    fn reset_persistent_play_state(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        self.asset_resolver.forget_missing_entities(engine.world());
    }

    /// 把场景 `Skybox` 指向的 HDR 交给渲染线程重建 IBL。
    ///
    /// manifest 路径下 IBL 来自 cook 进 `.rscn` 的环境字节；松散 `.scene.json`
    /// 没有 manifest 条目，所以由宿主按组件里的 `hdr_path` 现取现送。
    fn apply_scene_environment(&mut self) {
        let Some(shared) = self.render_shared.clone() else {
            return;
        };
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let Some(path) =
            prism_engine::scene::SceneReadView::new(engine.world()).environment_asset_path()
        else {
            return;
        };
        let normalized = normalize_asset_path(&path);
        let Some(id) = self
            .asset_resolver
            .resource_manager
            .id_by_path(normalized.as_str())
        else {
            log::warn!("hosted: environment `{path}` not found in package");
            return;
        };
        let bytes = match self.asset_resolver.resource_manager.load_with_deps_raw(id) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("hosted: load environment `{path}` failed: {e}");
                return;
            }
        };
        shared.send_renderer_message(Box::new(move |renderer| {
            if let Err(e) = renderer.set_environment(Some(bytes)) {
                log::warn!("hosted: set_environment failed: {e:#}");
            }
        }));
    }

    // -----------------------------------------------------------------------
    // 启动配置
    // -----------------------------------------------------------------------

    /// 解析启动配置，把日志级别写入 `RUST_LOG`。
    ///
    /// **必须在 logger 初始化之前调用**：`env_logger` / `android_logger`
    /// 只在初始化时读取一次过滤器。桌面入口在 [`Self::run`] 里先调用本函数，
    /// Android 入口在 `run_on_android` 里先读取同一份配置。
    fn apply_launch_config(&self) {
        let launch = prism_engine::launch_config::LaunchConfig::from_env();
        if let Some(level) = &launch.log_level {
            std::env::set_var("RUST_LOG", level);
        }
    }

    /// 构造这次启动要用的窗口参数（两个后端共用）。
    fn window_config(&self) -> WindowConfig {
        WindowConfig {
            title: self.config.window.title.clone(),
            width: self.config.window.width,
            height: self.config.window.height,
            min_width: self.config.window.min_width,
            min_height: self.config.window.min_height,
            max_width: self.config.window.max_width,
            max_height: self.config.window.max_height,
            position_x: self.config.window.position_x,
            position_y: self.config.window.position_y,
            resizable: self.config.window.resizable,
            fullscreen: self.config.window.fullscreen,
            maximized: self.config.window.maximized,
            visible: self.config.window.visible,
            decorations: self.config.window.decorations,
            vsync: self.config.window.vsync,
        }
    }

    /// 启动事件循环（桌面入口，自建 winit EventLoop）。
    ///
    /// 内部构造 EventLoop 并交给 [`Self::run_on`]；致命错误直接 panic。
    #[cfg(feature = "winit-backend")]
    pub fn run(self) {
        // 日志级别必须在 try_init 之前写入，否则 env_logger 已经读完 RUST_LOG。
        self.apply_launch_config();
        // 日志兜底（try_init 幂等，已初始化时静默失败）。
        let _ = env_logger::try_init();
        let event_loop = winit::event_loop::EventLoop::new().expect("failed to create event loop");
        self.run_on(event_loop).expect("fatal application error");
    }

    /// 在已构建的 winit EventLoop 上运行（Android 入口用）。
    ///
    /// 启动前解析启动配置（`PRISMREV_LAUNCH_CONFIG` env）并设置日志级别。
    ///
    /// Android 的 `android_main` 应在调用本函数 **之前** 自行注入
    /// `PRISMREV_LAUNCH_CONFIG` env（从 files 目录读取）。
    #[cfg(feature = "winit-backend")]
    pub fn run_on(mut self, event_loop: winit::event_loop::EventLoop<()>) -> anyhow::Result<()> {
        self.apply_launch_config();
        event_loop.run_app(&mut self)?;
        Ok(())
    }

    /// 启动应用（桌面入口，Win32 原生窗口 + 自己的消息循环）。
    ///
    /// 与 winit 入口等价——窗口、渲染线程、模拟循环、关闭顺序完全一致，
    /// 只是窗口与消息来自 `prism-platform` 的 `win32-native` 后端。
    #[cfg(all(windows, feature = "win32-native", not(feature = "winit-backend")))]
    pub fn run(self) {
        self.apply_launch_config();
        let _ = env_logger::try_init();
        self.run_win32().expect("fatal application error");
    }

    /// 恢复时重启随挂起释放的子系统（音频流 / 解码线程）。
    fn resume_subsystems(&mut self) {
        #[cfg(feature = "audio")]
        if self.has_subsystem(Subsystem::Audio) {
            if let Some(audio) = self.audio.as_mut() {
                audio.resume_stream();
            }
            // `suspended()` shuts the decode thread down; restart it so
            // playback queued after resume is decoded again.
            self.start_audio_decode_thread();
        }
    }

    /// 每帧主线程工作：渲染线程存活检查 → 模拟 → 宿主钩子。
    ///
    /// 返回 `Some(message)` 表示渲染线程已经死亡（初始化失败 / panic），
    /// 调用方应弹出致命错误并关闭窗口，而不是空转一个白窗口。
    fn tick_frame(&mut self) -> Option<String> {
        // The render thread exclusively owns the Vulkan context. If it exited
        // (renderer init failure or panic), stop instead of spinning a blank
        // window at 100% CPU.
        if let Some(handle) = self.render_thread.as_ref() {
            if handle.is_finished() {
                return Some(
                    self.render_shared
                        .as_ref()
                        .and_then(|shared| shared.take_render_failure())
                        .unwrap_or_else(|| {
                            "render thread exited unexpectedly (see log)".to_string()
                        }),
                );
            }
        }

        self.tick_sim(true);

        // 帧钩子（宿主 UI 等）— 主线程。全部用字段级借用（不整体借 self）。
        let window = self.platform.as_ref().map(PlatformContext::window_arc);
        if let (Some(hook), Some(shared), Some(engine), Some(window)) = (
            self.frame_hook.as_mut(),
            self.render_shared.as_ref(),
            self.engine.as_mut(),
            window,
        ) {
            let stats = shared.read_render_stats();
            hook.on_tick(
                &window,
                engine.world_mut(),
                &mut self.render_settings,
                &stats,
                shared,
            );
        }

        None
    }

    /// 关闭顺序（两个后端共用）——顺序很重要。
    fn shutdown(&mut self) {
        // 1. 停音频解码线程
        #[cfg(feature = "audio")]
        self.stop_audio_decode_thread();
        // 2. 停渲染线程
        self.stop_render_thread();
        // 4. 引擎预关闭
        if let Some(ref mut engine) = self.engine {
            engine.pre_shutdown();
        }
        // 5. 丢弃音频引擎（在平台/引擎之前停止音频流）
        #[cfg(feature = "audio")]
        self.audio.take();
        // 6. 丢弃窗口（渲染器归渲染线程所有，已随渲染线程停止而销毁）
        self.platform = None;
        // 7. 引擎后关闭
        if let Some(ref mut engine) = self.engine {
            engine.post_shutdown();
        }
    }

    /// 取出致命错误文案（关闭窗口前弹的对话框内容）。
    fn take_fatal_message(&mut self) -> String {
        self.fatal_error
            .take()
            .unwrap_or_else(|| "An unknown fatal error occurred.".to_string())
    }

    /// Win32 原生窗口的主循环（`win32-native` 构建的 `run_app` 等价物）。
    ///
    /// 消息循环用 `PeekMessageW` 泵 + `MsgWaitForMultipleObjectsEx` 让出
    /// CPU（原因见 `prism_platform::win32` 的模块文档）：主线程必须在没有
    /// 窗口消息时继续推进模拟，阻塞式 `GetMessageW` 会让静止画面冻住。
    #[cfg(all(windows, feature = "win32-native", not(feature = "winit-backend")))]
    fn run_win32(mut self) -> anyhow::Result<()> {
        // 复位挂起标志 + 恢复子系统（对应 winit 的 `resumed`）。
        self.suspended = false;
        self.resume_subsystems();

        if !self.has_subsystem(Subsystem::Render) {
            log::info!("render subsystem disabled — headless mode (no window, no GPU)");
            return Ok(());
        }

        let resumed_entry = crate::render_shared::startup_ms();
        let window_config = self.window_config();
        let platform = match PlatformContext::create_window_native(&window_config) {
            Ok(platform) => platform,
            Err(error) => {
                log::error!("failed to create native window: {error:#}");
                return Err(error);
            }
        };
        let window_built = crate::render_shared::startup_ms();

        self.platform = Some(platform);
        // 渲染线程异步构建渲染器 + 启动帧循环（重量级初始化不阻塞主线程）。
        self.spawn_render_thread();

        let spawned = crate::render_shared::startup_ms();
        if let Some(shared) = self.render_shared.as_ref() {
            shared.set_mark("resumed_entry", resumed_entry);
            shared.set_mark("window_built", window_built);
            shared.set_mark("render_thread_spawned", spawned);
        }

        let window = match self.platform.as_ref() {
            Some(platform) => platform.window_arc(),
            None => return Ok(()),
        };

        let mut exit = false;
        while !exit {
            // 1. 消息泵：把 WM_* 变成中立事件（含用户点关闭按钮）。
            let events = window.poll_events();
            for event in events {
                if self.fatal_error.is_some() {
                    exit = true;
                    break;
                }

                // 先把事件发给帧钩子（宿主 UI 等）；被消费则跳过快捷键。
                let consumed = match self.frame_hook.as_mut() {
                    Some(hook) => hook.on_platform_event(&window, event),
                    None => false,
                };

                // 路由到 InputManager；`false` = 用户关闭窗口。
                if !self.handle_platform_event(&window, event) {
                    exit = true;
                    break;
                }

                if consumed {
                    continue;
                }

                if let PlatformEvent::Key { key, pressed } = event {
                    if pressed && self.handle_shortcut(key) {
                        continue;
                    }
                }
            }

            if exit || window.close_requested() {
                break;
            }

            // 2. 模拟 + 提取 + 提交渲染（与 winit 的 `about_to_wait` 同一份）。
            if let Some(message) = self.tick_frame() {
                log::error!("{message}");
                self.fatal_error = Some(message);
                break;
            }

            // 3. 让出 CPU：有消息立即返回，否则最多等 1 ms。
            window.wait_events(1);
        }

        self.shutdown();

        if self.fatal_error.is_some() {
            let message = self.take_fatal_message();
            let _choice =
                prism_engine::crash_dialog::show_crash_dialog("PrismaRev - Fatal Error", &message);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 渲染线程生命周期
    // -----------------------------------------------------------------------

    /// 在**渲染线程**异步构建 [`GraphRenderer`] 并启动帧循环。
    ///
    /// 主线程只持有已创建的平台窗口。本函数计算场景光照、
    /// Vulkan 扩展名、叠加层工厂，创建共享状态，然后 spawn 渲染线程——
    /// 渲染线程内部才构建渲染器（~数百毫秒），因此 `resumed` 调用后能立即
    /// 返回，窗口事件（关闭/移动/缩放）全程不被阻塞。
    fn spawn_render_thread(&mut self) {
        // 原始句柄必须在**窗口创建线程**取好再跨线程传递（winit 的
        // `window_handle()` 在其他线程会失败）；原始 HWND 是进程级值，
        // 跨线程用于创建 Vulkan 表面是安全的（见 `SendWindowHandles`）。
        let (handles, extensions) = match self.platform.as_ref().map(PlatformContext::window) {
            Some(window) => (
                prism_platform::raw_window_handles(window).expect("get raw window handles"),
                // Vulkan 实例扩展名（依赖窗口 display handle，便宜）。
                required_vulkan_extensions(window),
            ),
            None => {
                log::error!("spawn_render_thread: window not created");
                return;
            }
        };

        // 场景声明式光照：纯 CPU 计算，无需渲染器（None → 廉价空 IBL）。
        let scene_env = self.engine.as_ref().and_then(|engine| {
            engine.current_scene_env_bytes_with_provider(&mut self.asset_resolver.resource_manager)
        });

        // 外部叠加层工厂（宿主 UI 等）——仅提供工厂，渲染线程建好
        // 渲染器后调用一次产出叠加层（GPU 资源 record 时懒创建）。
        let overlay_factory: Option<OverlayFactory> =
            self.frame_hook.as_ref().and_then(|h| h.overlay());

        let vsync = self.config.window.vsync;
        self.spawn_render_thread_with(
            RenderTarget::Surface {
                handles,
                extensions,
                vsync,
            },
            scene_env,
            overlay_factory,
        );
    }

    /// 启动渲染线程——目标（交换链 / 离屏图像）由调用方决定。
    fn spawn_render_thread_with(
        &mut self,
        target: RenderTarget,
        scene_env: Option<Vec<u8>>,
        overlay_factory: Option<OverlayFactory>,
    ) {
        let (shared, running) = RenderShared::new();
        let shared_clone = shared.clone();
        let thread = std::thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                render_thread_main(shared_clone, target, scene_env, true, overlay_factory)
            })
            .expect("failed to spawn render thread");

        self.render_shared = Some(shared);
        self.render_running = Some(running);
        self.render_thread = Some(thread);
    }

    fn stop_render_thread(&mut self) {
        // 通知渲染线程停止。
        if let Some(ref running) = self.render_running {
            running.store(false, Ordering::Relaxed);
        }
        if let Some(shared) = self.render_shared.as_ref() {
            shared.packet_ready.notify_all();
        }

        // 等待线程结束
        if let Some(handle) = self.render_thread.take() {
            if handle.join().is_err() {
                log::error!("render thread panicked");
            }
        }
        self.render_running = None;
        self.render_shared = None;
    }

    // -----------------------------------------------------------------------
    // 音频解码线程生命周期（`audio` feature）
    // -----------------------------------------------------------------------

    #[cfg(feature = "audio")]
    fn start_audio_decode_thread(&mut self) {
        if self.audio_decode_thread.is_some() {
            return;
        }
        let (tx, rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::bounded(8);

        let thread = std::thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || crate::audio_decode_runner::audio_decode_thread_main(rx, result_tx))
            .expect("failed to spawn audio decode thread");

        self.audio_decode_tx = Some(tx);
        self.audio_decode_rx = Some(result_rx);
        self.audio_decode_thread = Some(thread);
    }

    #[cfg(feature = "audio")]
    fn stop_audio_decode_thread(&mut self) {
        if let Some(tx) = self.audio_decode_tx.take() {
            let _ = tx.send(crate::audio_decode_runner::DecodeRequest::Shutdown);
        }
        if let Some(handle) = self.audio_decode_thread.take() {
            let _ = handle.join();
        }
    }

    // -----------------------------------------------------------------------
    // 平台上下文（窗口 + 渲染器）生命周期
    // -----------------------------------------------------------------------
    // 注意：窗口由主线程的 `PlatformContext::create_window` 创建（快速），
    // 渲染器由 `spawn_render_thread` → 渲染线程内部构建（异步、不阻塞事件）。

    // -----------------------------------------------------------------------
    // 游戏循环（主线程）
    // -----------------------------------------------------------------------

    /// 单步模拟 + extract + 提交渲染。
    ///
    /// `simulate = false` 是编辑器「编辑态」：只传播层级后直接 extract，不跑
    /// 任何游戏 system（相机控制器 / UI / 用户逻辑都不该在点 Play 之前动场景）。
    fn tick_sim(&mut self, simulate: bool) {
        // Poll mode may call this at any event rate. Time is measured once from
        // the monotonic clock; input events must never determine simulation speed.
        let now = Instant::now();
        let elapsed = self
            .last_frame
            .map(|previous| now.saturating_duration_since(previous))
            .unwrap_or(Duration::ZERO)
            .min(Duration::from_millis(100));
        self.fixed_accumulator += elapsed;
        let dt = elapsed.as_secs_f32();
        // Advance the frame clock HERE, at the point `elapsed` was measured:
        // the heavy per-frame work (extract + render submit) happens later in
        // this same call, so stamping the clock at the end of `about_to_wait`
        // made `dt` measure only the idle gap between iterations (≈0.5 µs).
        // That froze time-driven simulation and made its speed depend on how
        // often winit woke the loop (i.e. on mouse movement).
        self.last_frame = Some(now);

        // --- 引擎模拟（独立借用 self.engine，块结束即释放） ---
        // 尺寸要在借用 engine 之前取：`surface_size()` 借整个 `&self`。
        let (surface_w, surface_h) = self.surface_size();
        {
            let Some(engine) = self.engine.as_mut() else {
                return;
            };

            // --- 同步屏幕尺寸（ECS UI 布局依赖 ScreenSize resource） ---
            if surface_w > 0 && surface_h > 0 {
                engine
                    .world_mut()
                    .insert_resource(prism_engine::ui::ScreenSize::new(surface_w, surface_h));
            }

            // --- 输入：帧开始，清空瞬时状态 ---
            self.input.begin_frame();

            if simulate {
                // --- Fixed timestep: integer nanosecond clock, capped catch-up ---
                const FIXED_STEP: Duration = Duration::from_nanos(16_666_667);
                let mut fixed_steps = 0;
                while self.fixed_accumulator >= FIXED_STEP && fixed_steps < 8 {
                    // 物理走固定步（#522）：结果在下面的 `engine.update()` 里由
                    // `hierarchy_system` 传播成 `LocalToWorld`，渲染当帧就能看到。
                    #[cfg(feature = "physics")]
                    if let Some(physics) = self.physics.as_mut() {
                        physics.step(engine.world_mut());
                    }
                    engine.fixed_update(FIXED_STEP.as_secs_f32(), &self.input);
                    self.fixed_accumulator -= FIXED_STEP;
                    fixed_steps += 1;
                }

                // --- 可变时间步长更新 ---
                engine.update(dt, &self.input);

                // --- 延迟更新 ---
                engine.late_update();
            } else {
                // 编辑态：不消费 fixed accumulator，避免切到 Play 时补跑一大段。
                self.fixed_accumulator = Duration::ZERO;
                prism_engine::scene::systems::hierarchy::hierarchy_system(engine.world_mut());
                prism_engine::scene::systems::hierarchy::active_hierarchy_system(
                    engine.world_mut(),
                );
            }

            // --- 音频更新（`audio` feature） ---
            #[cfg(feature = "audio")]
            {
                if let Some(audio) = self.audio.as_mut() {
                    audio.update();
                }

                // 处理音频解码结果：交给场景音频缓存（不再自动播放，
                // 播放由 `AudioSource` 组件决定）。
                if let Some(rx) = self.audio_decode_rx.as_ref() {
                    while let Ok(result) = rx.try_recv() {
                        if let crate::audio_decode_runner::DecodeResult::Error { message, .. } =
                            &result
                        {
                            log::warn!("Audio decode error: {message}");
                        }
                        self.scene_audio.receive(result);
                    }
                }

                // 场景音频：ECS `AudioSource` → 混音器（#508）。
                if let Some(audio) = self.audio.as_mut() {
                    let mut sink = (audio, &mut self.audio_voices);
                    self.scene_audio.update(
                        engine.world(),
                        &mut sink,
                        self.audio_decode_tx.as_ref(),
                    );
                }
            }
        } // engine 借用在此结束

        // --- 开发期热重载（#495）：重建 `.pak` 后不用退出再进 ---
        self.poll_asset_changes();

        // --- 资产解析：主线程 CPU 段准备请求 → 通道 → 渲染线程 GPU 段 ---
        self.pump_asset_requests();

        // --- 提取帧数据包 → 发送到渲染线程（仅渲染启用时） ---
        if let (Some(shared), Some(engine)) = (self.render_shared.as_ref(), self.engine.as_mut()) {
            // 复用渲染线程上一帧归还的数据包（`Vec` 容量随之保留，#66）。
            let mut packet = shared.take_spare_packet().unwrap_or_default();
            // #613：内部分辨率生效时相机按**内部**宽高比投影，后处理再整数
            // 放大到窗口；否则 2D 像素会被拉成不等宽的矩形。
            let aspect = self.render_settings.render_aspect(self.display_aspect);
            extract_frame_packet_into_with_shadow(
                engine.world_mut(),
                aspect,
                &self.surface_rotation,
                self.render_settings.shadow_distance,
                &mut packet,
            );
            // 渲染设置随帧数据包送到渲染线程（快捷键 / 项目配置的修改在此生效）。
            packet.settings = self.render_settings.clone();

            // 运行时诊断 HUD：追加到 ECS UI 叠加（随后统一由 ui_overlay 绘制）。
            if self.debug_hud.show {
                let (width, height) = self.surface_size();
                let (width, height) = (width as f32, height as f32);
                let stats = shared.read_render_stats();
                self.debug_hud.append(
                    &mut packet.ui_overlay,
                    &stats,
                    &self.render_settings,
                    packet.scene.has_camera,
                    width,
                    height,
                );
            }

            shared.send_packet(packet);
        }
    }

    /// 每帧驱动跨线程资产解析：
    /// 1. **CPU 段**（主线程）：扫描待解析 `MeshRenderer` 实体，加载 `.pak`
    ///    + 解交织顶点/纹理像素，产出纯数据上传请求并入队（不碰渲染器）。
    /// 2. **结果写回**（主线程）：取走渲染线程 GPU 段回传的句柄，写入
    ///    `MeshRef`/`MaterialRef`（`generation = 1`）。
    ///
    /// 渲染器在渲染线程异步构建，故此函数可在首帧前安全地每帧调用——
    /// 请求会暂存在通道中直到渲染器就绪。
    fn pump_asset_requests(&mut self) {
        let (Some(shared), Some(engine)) = (self.render_shared.as_ref(), self.engine.as_mut())
        else {
            return;
        };

        // 工程自带字体（#583）：主线程读字节，渲染线程把它注册进 UI 图集。
        let fonts = self.asset_resolver.prepare_ui_fonts(engine.world());
        for (name, bytes) in fonts {
            shared.send_renderer_message(Box::new(move |renderer| {
                renderer.register_ui_font(name, bytes);
            }));
        }

        // CPU 段：扫描待解析实体并产出上传请求（纯数据，不碰渲染器）。
        let reqs = self.asset_resolver.prepare_requests(engine.world_mut());
        if !reqs.is_empty() {
            shared.enqueue_asset_requests(reqs);
        }

        // 失效的 GPU 资源（实体销毁 / 热重载 / 路径改写）在渲染线程真正释放（#495）。
        let releases = self.asset_resolver.take_releases();
        if !releases.is_empty() {
            shared.send_renderer_message(Box::new(move |renderer| {
                renderer.release_assets(&releases);
            }));
        }

        // GPU 段结果写回：渲染线程回传句柄，主线程写入组件。
        let results = shared.take_asset_results();
        if !results.is_empty() {
            self.asset_resolver
                .apply_results(engine.world_mut(), &results);
        }
    }

    /// 开发期资产热重载：`.pak` 重建后换掉 reader 并让实体重新解析（#495）。
    ///
    /// 只在 [`spawn_pak_watcher`] 建了轮询线程时生效（`PRISMREV_HOT_RELOAD`
    /// 打开）。和编辑器一样分三步：`on_pak_changed` 换文件字节 → 重载路径清单
    /// → `invalidate_assets` 把实体打回待解析，下一帧 `pump_asset_requests`
    /// 会重新上传。
    ///
    /// 已知代价：渲染器没有「按句柄释放」的接口，被替换的旧纹理 / 网格槽会一直
    /// 占用，所以这条路径只用于开发期。
    fn poll_asset_changes(&mut self) {
        let Some(watcher) = self.pak_watcher.as_ref() else {
            return;
        };
        // 先把事件收干净再改状态：`receiver()` 借的是 watcher，处理事件要借 self。
        let mut changed: Vec<std::path::PathBuf> = Vec::new();
        while let Ok(event) = watcher.receiver().try_recv() {
            match event {
                prism_engine::scene::hot_reload::HotReloadEvent::PakModified(path) => {
                    if !changed.contains(&path) {
                        changed.push(path);
                    }
                }
                prism_engine::scene::hot_reload::HotReloadEvent::WatchError(error) => {
                    log::warn!("hot reload: {error}");
                }
            }
        }
        if changed.is_empty() {
            return;
        }

        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        let mut reloaded = 0usize;
        for pak in &changed {
            if let Err(error) = self.asset_resolver.resource_manager.on_pak_changed(pak) {
                log::warn!("hot reload: reopening {} failed: {error}", pak.display());
                continue;
            }
            let manifest = pak.with_extension("pak.meta.json");
            if manifest.is_file() {
                if let Err(error) = self
                    .asset_resolver
                    .resource_manager
                    .load_path_manifest(&manifest)
                {
                    log::warn!(
                        "hot reload: path manifest {} failed: {error}",
                        manifest.display()
                    );
                }
            }
            reloaded += 1;
        }
        if reloaded == 0 {
            return;
        }
        let count = self.asset_resolver.invalidate_assets(engine.world_mut());
        log::info!(
            "hot reload: {count} entity/entities re-resolved after {reloaded} package(s) were rebuilt"
        );
    }

    // -----------------------------------------------------------------------
    // PBR 调试 helpers
    // -----------------------------------------------------------------------

    /// 把一个中立平台事件路由到 [`InputManager`] 与窗口状态。
    ///
    /// 返回 `false` 表示应用应退出（用户关闭窗口）。两个窗口后端共用这一份，
    /// 所以 winit 与 Win32 的输入语义不会各写一遍再慢慢漂移。
    fn handle_platform_event(&mut self, window: &PlatformWindow, event: PlatformEvent) -> bool {
        use prism_engine::input::{ElementState, MouseButton};

        match event {
            PlatformEvent::CloseRequested => return false,
            PlatformEvent::Focused(focused) => {
                self.input.focus_return_click = !focused;
                if !focused && self.input.pointer_locked {
                    prism_platform::release_pointer(window);
                    self.input.set_locked(false);
                }
            }
            PlatformEvent::CursorMoved { x, y } => {
                apply_window_cursor_move(&mut self.input, x, y);
            }
            // 锁定态下的原始设备增量（winit: `DeviceEvent::MouseMotion`，
            // Win32: 回中式鼠标捕获的偏移）。
            PlatformEvent::MouseMotion { dx, dy } => {
                apply_device_mouse_motion(&mut self.input, dx, dy);
            }
            PlatformEvent::MouseWheel { delta } => self.input.handle_scroll(delta),
            PlatformEvent::Touch { id, x, y, phase } => {
                // 触摸不走鼠标按钮模拟：多点触控必须保留触点 id 与 phase，
                // 项目模块经 `InputSnapshot::touches`（宿主填表，最多
                // `InputSnapshot::MAX_TOUCHES` 点）自行解释，而不是被压成左键。
                self.input
                    .handle_touch(id, engine_touch_phase(phase), [x, y]);
            }
            PlatformEvent::MouseButton { button, pressed } => {
                let button = match button {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Back,
                    4 => MouseButton::Forward,
                    other => MouseButton::Other(other),
                };
                if pressed && button == MouseButton::Left && !self.input.pointer_locked {
                    prism_platform::grab_pointer(window);
                    self.input.set_locked(true);
                }
                self.input.handle_mouse_button(
                    button,
                    if pressed {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    },
                );
            }
            PlatformEvent::Key { key, pressed } => {
                // `Other(u32::MAX)` = 平台没给出物理键（winit 的 `Unidentified`）。
                // 旧实现直接丢弃这类事件，这里保持一致。
                if key == Key::Other(u32::MAX) {
                    return true;
                }
                let key = engine_key_from_native(key);
                if should_release_pointer_lock(key, pressed, self.input.pointer_locked) {
                    prism_platform::release_pointer(window);
                    self.input.set_locked(false);
                }
                self.input.handle_keyboard(
                    key,
                    if pressed {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    },
                );
            }
            PlatformEvent::Modifiers(modifiers) => self.modifiers = modifiers,
            // 尺寸变化同时更新宽高比（渲染线程从平台上下文读真实尺寸）。
            PlatformEvent::Resized { width, height } => {
                self.needs_resize = true;
                if width > 0 && height > 0 {
                    self.display_aspect = width as f32 / height as f32;
                }
            }
            // 尚未建模的事件：保持显式列出，避免 `_ => {}` 静默吞掉新变体
            // （`PlatformEvent::Touch` 就是这样丢了整条触摸链路）。
            PlatformEvent::Other => {}
        }

        true
    }

    /// 应用级键盘快捷键（F3 / Tab / T）+ PBR 分量隔离。
    ///
    /// 只在按键**按下**时调用；返回 `true` 表示这个键已被吃掉。
    fn handle_shortcut(&mut self, key: Key) -> bool {
        // 修饰键不在 `Key` 事件里，Shift 取最近一次 `Modifiers` 快照。
        let shift = self.modifiers.shift;

        if let Some(bit) = Self::pbr_debug_key_to_bit(key, shift) {
            self.render_settings.debug_flags = if self.render_settings.debug_flags == (1u32 << bit)
            {
                0
            } else {
                1u32 << bit
            };
            log::info!(
                "PBR isolate = {} (flags=0x{:x})",
                self.pbr_flag_label(),
                self.render_settings.debug_flags
            );
            return true;
        }

        match key {
            Key::F3 => {
                self.debug_hud.show = !self.debug_hud.show;
                log::info!(
                    "runtime HUD = {}",
                    if self.debug_hud.show { "on" } else { "off" }
                );
            }
            Key::Tab => {
                self.render_settings.debug_rt = (self.render_settings.debug_rt + 1) % 3;
                let name = match self.render_settings.debug_rt {
                    0 => "normal (HDR tonemap)",
                    1 => "depth (linearized)",
                    2 => "normal (view-space)",
                    _ => "?",
                };
                log::info!("debug RT = {} ({})", self.render_settings.debug_rt, name);
            }
            Key::KeyT => {
                // 0 = Reinhard, 1 = ACES, 2 = passthrough（2D / 像素画）。
                self.render_settings.tonemap_mode = (self.render_settings.tonemap_mode + 1) % 3;
                log::info!("tonemap mode = {}", self.render_settings.tonemap_mode);
            }
            _ => return false,
        }
        true
    }

    fn pbr_flag_label(&self) -> String {
        prism_engine::render_settings::pbr_flag_label(self.render_settings.debug_flags).to_string()
    }

    fn pbr_debug_key_to_bit(key: Key, shift: bool) -> Option<u32> {
        Some(match (key, shift) {
            (Key::Digit1, false) => 0,
            (Key::Digit2, false) => 1,
            (Key::Digit3, false) => 2,
            (Key::Digit4, false) => 3,
            (Key::Digit5, false) => 4,
            (Key::Digit6, false) => 5,
            (Key::Digit7, false) => 6,
            (Key::Digit8, false) => 7,
            (Key::Digit9, false) => 8,
            (Key::Digit0, false) => 9,
            (Key::Digit1, true) => 10,
            (Key::Digit2, true) => 11,
            (Key::Digit3, true) => 12,
            (Key::Digit4, true) => 13,
            (Key::Digit5, true) => 14,
            _ => return None,
        })
    }

    #[cfg(feature = "winit-backend")]
    fn show_fatal_dialog(&mut self, event_loop: &ActiveEventLoop) {
        let message = self.take_fatal_message();
        let _choice =
            prism_engine::crash_dialog::show_crash_dialog("PrismaRev - Fatal Error", &message);
        event_loop.exit();
    }
}

/// `prism-platform` 的触点 phase → 引擎输入模型的 phase。
///
/// 两者当前一一对应，但分层上必须显式转换：`prism-app` 不把平台类型直接
/// 传进引擎，避免以后任一侧加减 phase 时静默错配。
fn engine_touch_phase(phase: prism_platform::TouchPhase) -> prism_engine::input::TouchPhase {
    use prism_engine::input::TouchPhase;
    match phase {
        prism_platform::TouchPhase::Started => TouchPhase::Started,
        prism_platform::TouchPhase::Moved => TouchPhase::Moved,
        prism_platform::TouchPhase::Ended => TouchPhase::Ended,
        prism_platform::TouchPhase::Cancelled => TouchPhase::Cancelled,
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// ApplicationHandler
// ===========================================================================

#[cfg(feature = "winit-backend")]
impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Drive simulation/extraction continuously. With the default Wait mode
        // winit only re-enters `about_to_wait` after input/window events, which
        // freezes animation when the user is idle.
        event_loop.set_control_flow(ControlFlow::Poll);

        // 复位挂起标志（必须先于任何子系统门控——audio 恢复不依赖 Render）。
        self.suspended = false;
        self.resume_subsystems();

        if !self.has_subsystem(Subsystem::Render) {
            log::info!("render subsystem disabled — headless mode (no window, no GPU)");
            return;
        }

        let resumed_entry = crate::render_shared::startup_ms();

        if self.platform.is_none() {
            // 主线程仅创建窗口（快速，~数毫秒），随后立即返回——窗口事件
            // （关闭/移动/缩放）此刻起即可被 winit 派发，不被任何初始化阻塞。
            let window_config = self.window_config();
            let platform = PlatformContext::create_window(event_loop, &window_config);
            let window_built = crate::render_shared::startup_ms();

            self.platform = Some(platform);

            // 渲染线程异步构建渲染器 + 启动帧循环（重量级初始化不阻塞主线程）。
            self.spawn_render_thread();

            // 记录启动里程碑：resumed 返回时刻 = event_loop_free（事件循环已解锁）。
            let spawned = crate::render_shared::startup_ms();
            if let Some(shared) = self.render_shared.as_ref() {
                shared.set_mark("resumed_entry", resumed_entry);
                shared.set_mark("window_built", window_built);
                shared.set_mark("render_thread_spawned", spawned);
            }
            return;
        }

        // Android suspend 后重建表面：本设计在 suspended 时已停止渲染线程并
        // 丢弃窗口，故此处会重新走上面的「首次恢复」分支。仅作预留日志。
        log::info!("resumed (surface already active)");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }
        if self.fatal_error.is_some() {
            self.show_fatal_dialog(event_loop);
            return;
        }

        let platform_event = prism_platform::to_platform_event(&event);

        // 先将事件转发给帧钩子（宿主 UI 等）；被消费则不再走应用
        // 自身的快捷键处理（输入仍会路由到 InputManager 保持按键状态一致）。
        let consumed = match (self.frame_hook.as_mut(), self.platform.as_ref()) {
            (Some(hook), Some(platform)) => {
                let window = platform.window();
                hook.on_platform_event(window, platform_event)
                    || hook.on_window_event(window, &event)
            }
            _ => false,
        };

        // 路由窗口事件 → InputManager（始终路由，包括快捷键未处理的重复事件）。
        if let Some(window) = self.platform.as_ref().map(PlatformContext::window_arc) {
            if !self.handle_platform_event(&window, platform_event) {
                event_loop.exit();
                return;
            }
        }

        if consumed {
            return;
        }

        // 修饰键状态只随 `ModifiersChanged` 更新（路由已写入），事件本身无需处理。
        if matches!(event, WindowEvent::ModifiersChanged(_)) {
            return;
        }

        // 键盘快捷键（仅按下时触发）。`KeyboardInput` 不带修饰键，
        // Shift 取自 `self.modifiers`。
        if let WindowEvent::KeyboardInput {
            event:
                winit::event::KeyEvent {
                    physical_key,
                    state,
                    ..
                },
            ..
        } = &event
        {
            if *state == winit::event::ElementState::Pressed {
                if let winit::keyboard::PhysicalKey::Code(code) = physical_key {
                    self.handle_shortcut(Key::from_winit(*code));
                }
            }
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        // 在指针锁定模式下，鼠标移动以原始设备增量形式到达。
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            apply_device_mouse_motion(&mut self.input, dx, dy);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if event_loop.exiting() {
            self.shutdown();
            return;
        }

        // 游戏循环 tick（主线程）——后台挂起期间暂停模拟并提前返回（T4）：
        // 不投喂 dt → 模拟冻结 → 后台进程 CPU 归零，且音频解码已随
        // suspended 停止，无队列空转。
        if self.suspended {
            return;
        }

        if let Some(message) = self.tick_frame() {
            log::error!("{message}");
            self.fatal_error = Some(message);
            self.show_fatal_dialog(event_loop);
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        // 后台不再需要逐帧轮询；`resumed()` 会恢复 `Poll`。否则 winit 在
        // Android 后台仍持续空转，CPU 占用保持 100%。
        event_loop.set_control_flow(ControlFlow::Wait);

        // 置位挂起标志（必须先于任何子系统门控——audio 挂起不依赖 Render）。
        self.suspended = true;

        // --- 音频子系统：交还音频设备（T4）。drop cpal 流 → 停止回调线程；
        // Firewheel 图与活动播放节点保留，恢复时以挂起前配置重新注册并续播。 ---
        #[cfg(feature = "audio")]
        if self.has_subsystem(Subsystem::Audio) {
            if let Some(audio) = self.audio.as_mut() {
                audio.suspend_stream();
            }
        }

        if !self.has_subsystem(Subsystem::Render) {
            return;
        }

        // 停止后台线程。
        #[cfg(feature = "audio")]
        self.stop_audio_decode_thread();

        // 停止渲染线程（渲染器归渲染线程所有，停止即释放 Vulkan 表面）。
        self.stop_render_thread();

        self.platform = None;
    }
}

/// `PRISMREV_HOT_RELOAD` 的取值是否表示「打开」（#495）。
///
/// 接受 `1` / `true` / `yes` / `on`（大小写、首尾空白都忽略）；其它值（含 `0`、
/// 空串、未设置）一律算关。发布版默认关，所以运行时既不起线程也不 stat 文件。
fn hot_reload_enabled(raw: Option<&str>) -> bool {
    raw.map(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
    .unwrap_or(false)
}

/// `PRISMREV_HOT_RELOAD=1` 时给 `assets/` 下所有 `.pak` 建一个轮询器（#495）。
///
/// 发布版默认不建：没有线程、没有 stat，行为与之前完全一致。接受 `1` / `true` /
/// `yes` / `on`（大小写无关），其它值（含 `0`、空串）都当关。
fn spawn_pak_watcher() -> Option<prism_engine::scene::hot_reload::HotReloadWatcher> {
    let raw = std::env::var("PRISMREV_HOT_RELOAD").ok();
    if !hot_reload_enabled(raw.as_deref()) {
        return None;
    }
    let paks = prism_engine::asset_paths::pak_files();
    if paks.is_empty() {
        log::warn!("hot reload: no `.pak` under assets/ to watch");
        return None;
    }
    match prism_engine::scene::hot_reload::HotReloadWatcher::watch_files(
        paks.clone(),
        Duration::from_millis(500),
    ) {
        Ok(watcher) => {
            log::info!(
                "hot reload: watching {} package(s) for changes (development only)",
                paks.len()
            );
            Some(watcher)
        }
        Err(error) => {
            log::warn!("hot reload: cannot start the package watcher: {error}");
            None
        }
    }
}

/// Esc 是指针锁定的逃生口：锁定态下它先释放光标，再作为普通按键交给游戏。
fn should_release_pointer_lock(
    key: prism_engine::input::KeyCode,
    pressed: bool,
    locked: bool,
) -> bool {
    locked && pressed && key == prism_engine::input::KeyCode::Escape
}

/// 锁定态下窗口光标位置是被 confinement 夹住的，不能参与累加；此时唯一的
/// 鼠标来源是原始设备增量 [`apply_device_mouse_motion`]。
///
/// 位置本身仍然同步（不产生 delta），解锁后第一帧才不会从旧位置跳一下。
fn apply_window_cursor_move(input: &mut InputManager, x: f64, y: f64) {
    if input.pointer_locked {
        input.set_mouse_position([x, y]);
        return;
    }
    input.handle_mouse_move([x, y]);
}

/// 原始设备增量（`DeviceEvent::MouseMotion`）：锁定态下累加到一个虚拟绝对
/// 位置，使 `InputManager::mouse_delta()` 反映原始增量。
fn apply_device_mouse_motion(input: &mut InputManager, dx: f64, dy: f64) {
    if !input.pointer_locked {
        return;
    }
    let cur = input.mouse_position();
    input.handle_mouse_move([cur[0] + dx, cur[1] + dy]);
}

/// 中立物理键 → 引擎键码。
///
/// 转换留在 `prism-app`（同时依赖 `prism-platform` 与 `prism-engine`），
/// 而不是 `prism-platform`：平台层保持 engine-free，只产出与窗口库无关的
/// [`prism_platform::PlatformEvent`]。未列出的键退化为 `Key::Other` 的原始码，
/// 避免静默丢弃。
fn engine_key_from_native(key: Key) -> prism_engine::input::KeyCode {
    use prism_engine::input::KeyCode as Eng;
    match key {
        Key::KeyW => Eng::KeyW,
        Key::KeyA => Eng::KeyA,
        Key::KeyS => Eng::KeyS,
        Key::KeyD => Eng::KeyD,
        Key::KeyQ => Eng::KeyQ,
        Key::KeyE => Eng::KeyE,
        Key::Space => Eng::Space,
        Key::ShiftLeft => Eng::ShiftLeft,
        Key::ShiftRight => Eng::ShiftRight,
        Key::ControlLeft => Eng::ControlLeft,
        Key::ControlRight => Eng::ControlRight,
        Key::AltLeft => Eng::AltLeft,
        Key::AltRight => Eng::AltRight,
        Key::Escape => Eng::Escape,
        Key::Tab => Eng::Tab,
        Key::Enter => Eng::Enter,
        Key::ArrowUp => Eng::ArrowUp,
        Key::ArrowDown => Eng::ArrowDown,
        Key::ArrowLeft => Eng::ArrowLeft,
        Key::ArrowRight => Eng::ArrowRight,
        Key::Digit0 => Eng::Digit0,
        Key::Digit1 => Eng::Digit1,
        Key::Digit2 => Eng::Digit2,
        Key::Digit3 => Eng::Digit3,
        Key::Digit4 => Eng::Digit4,
        Key::Digit5 => Eng::Digit5,
        Key::Digit6 => Eng::Digit6,
        Key::Digit7 => Eng::Digit7,
        Key::Digit8 => Eng::Digit8,
        Key::Digit9 => Eng::Digit9,
        // 引擎没有字母/T/F1–F12 的专用变体：保留原始虚拟键码，不丢键。
        Key::KeyT
        | Key::F1
        | Key::F2
        | Key::F3
        | Key::F4
        | Key::F5
        | Key::F6
        | Key::F7
        | Key::F8
        | Key::F9
        | Key::F10
        | Key::F11
        | Key::F12
        | Key::Other(_) => Eng::Other(key.to_vk()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_platform::PlatformEvent;

    /// 场景里写的环境贴图路径常见三种写法，都要落到 pak 的键上。
    #[test]
    fn environment_paths_normalize_to_package_keys() {
        assert_eq!(
            normalize_asset_path("../valley_of_desolation_1k.hdr"),
            "valley_of_desolation_1k.hdr"
        );
        assert_eq!(normalize_asset_path("assets/env/sky.hdr"), "env/sky.hdr");
        assert_eq!(normalize_asset_path("assets\\env\\sky.hdr"), "env/sky.hdr");
        assert_eq!(normalize_asset_path("./env/./sky.hdr"), "env/sky.hdr");
        assert_eq!(
            normalize_asset_path("assets/env/../env/sky.hdr"),
            "env/sky.hdr"
        );
    }

    /// `touch_event` 走 winit 的 `WindowEvent`，只有 winit 后端才编得出来。
    #[cfg(feature = "winit-backend")]
    fn touch_event(phase: winit::event::TouchPhase, id: u64) -> WindowEvent {
        WindowEvent::Touch(winit::event::Touch {
            device_id: winit::event::DeviceId::dummy(),
            phase,
            location: winit::dpi::PhysicalPosition::new(40.0, 50.0),
            force: Some(winit::event::Force::Normalized(1.0)),
            id,
        })
    }

    /// 触摸必须经平台快照落到 `InputManager::touches()`，并保留触点 id 与 phase
    /// （回归防护：`handle_platform_event` 的 `_ => {}` 曾把整条链路丢掉）。
    #[cfg(feature = "winit-backend")]
    #[test]
    fn touch_snapshot_reaches_input_manager() {
        let mut input = InputManager::new();
        for (phase, id) in [
            (winit::event::TouchPhase::Started, 3u64),
            (winit::event::TouchPhase::Moved, 3u64),
        ] {
            match prism_platform::to_platform_event(&touch_event(phase, id)) {
                PlatformEvent::Touch { id, x, y, phase } => {
                    input.handle_touch(id, engine_touch_phase(phase), [x, y])
                }
                other => panic!("touch 被转换成了 {other:?}"),
            }
        }

        let touches = input.touches();
        assert_eq!(touches.len(), 2);
        assert_eq!(touches[0].id, 3);
        assert_eq!(touches[0].phase, prism_engine::input::TouchPhase::Started);
        assert_eq!(touches[0].position, [40.0, 50.0]);
        assert_eq!(touches[1].phase, prism_engine::input::TouchPhase::Moved);
    }

    /// 锁定态下窗口光标位置不再参与累加（confinement 会把它夹在窗口内）。
    #[test]
    fn locked_pointer_ignores_window_cursor_positions() {
        let mut input = InputManager::new();
        input.set_locked(true);
        apply_window_cursor_move(&mut input, 120.0, 80.0);
        assert_eq!(input.mouse_delta(), [0.0, 0.0], "窗口光标不该产生增量");

        input.set_locked(false);
        apply_window_cursor_move(&mut input, 132.0, 80.0);
        assert_eq!(input.mouse_delta(), [12.0, 0.0], "解锁后光标位置恢复正常");
    }

    /// 回归防护：锁定态下原始设备增量是唯一来源，不能与窗口光标叠加。
    #[test]
    fn raw_device_motion_is_the_only_delta_while_locked() {
        let mut input = InputManager::new();
        input.set_locked(true);
        // 夹住后的光标位置先到（以前会先累加一次 delta）。
        apply_window_cursor_move(&mut input, 640.0, 360.0);
        apply_device_mouse_motion(&mut input, 3.0, -4.0);
        assert_eq!(input.mouse_delta(), [3.0, -4.0]);
    }

    /// 未锁定时不接受原始增量（否则窗口光标与设备增量会同时生效）。
    #[test]
    fn raw_device_motion_is_ignored_when_unlocked() {
        let mut input = InputManager::new();
        apply_device_mouse_motion(&mut input, 5.0, 5.0);
        assert_eq!(input.mouse_delta(), [0.0, 0.0]);
    }

    /// Esc 是锁定态下的释放入口；其它按键和按键释放都不影响锁定。
    #[test]
    fn escape_releases_the_pointer_lock() {
        use prism_engine::input::KeyCode;
        assert!(should_release_pointer_lock(KeyCode::Escape, true, true));
        assert!(!should_release_pointer_lock(KeyCode::Escape, false, true));
        assert!(!should_release_pointer_lock(KeyCode::KeyW, true, true));
        assert!(!should_release_pointer_lock(KeyCode::Escape, true, false));
    }

    /// 未按 Shift 的 1..9、0 覆盖分量 0..9。
    #[test]
    fn plain_digits_select_the_lower_pbr_components() {
        use prism_platform::Key as K;
        for (code, bit) in [
            (K::Digit1, 0),
            (K::Digit2, 1),
            (K::Digit3, 2),
            (K::Digit4, 3),
            (K::Digit5, 4),
            (K::Digit6, 5),
            (K::Digit7, 6),
            (K::Digit8, 7),
            (K::Digit9, 8),
            (K::Digit0, 9),
        ] {
            assert_eq!(
                App::pbr_debug_key_to_bit(code, false),
                Some(bit),
                "{code:?} without Shift"
            );
        }
    }

    /// 回归防护：Shift 曾经被硬编码成 `false`，分量 10..14 完全不可达。
    #[test]
    fn shift_digits_select_the_upper_pbr_components() {
        use prism_platform::Key as K;
        for (code, bit) in [
            (K::Digit1, 10),
            (K::Digit2, 11),
            (K::Digit3, 12),
            (K::Digit4, 13),
            (K::Digit5, 14),
        ] {
            assert_eq!(
                App::pbr_debug_key_to_bit(code, true),
                Some(bit),
                "{code:?} with Shift"
            );
        }
        // 只有 15 个分量；Shift+6..0 与普通字母都不该抢快捷键。
        for code in [
            K::Digit6,
            K::Digit7,
            K::Digit8,
            K::Digit9,
            K::Digit0,
            K::KeyA,
        ] {
            assert_eq!(App::pbr_debug_key_to_bit(code, true), None, "{code:?}");
        }
    }

    /// 键盘映射是手工表：抽查移动/修饰/功能/数字键，防止平台层改动后
    /// 只有部分键还能到达 `InputManager`。
    #[test]
    fn native_keys_map_to_engine_keys() {
        use prism_engine::input::KeyCode as Eng;
        use prism_platform::Key as K;
        for (key, expected) in [
            (K::KeyW, Eng::KeyW),
            (K::KeyA, Eng::KeyA),
            (K::KeyS, Eng::KeyS),
            (K::KeyD, Eng::KeyD),
            (K::KeyQ, Eng::KeyQ),
            (K::KeyE, Eng::KeyE),
            (K::Space, Eng::Space),
            (K::ShiftLeft, Eng::ShiftLeft),
            (K::ShiftRight, Eng::ShiftRight),
            (K::ControlLeft, Eng::ControlLeft),
            (K::ControlRight, Eng::ControlRight),
            (K::AltLeft, Eng::AltLeft),
            (K::AltRight, Eng::AltRight),
            (K::Escape, Eng::Escape),
            (K::Tab, Eng::Tab),
            (K::Enter, Eng::Enter),
            (K::ArrowUp, Eng::ArrowUp),
            (K::ArrowDown, Eng::ArrowDown),
            (K::ArrowLeft, Eng::ArrowLeft),
            (K::ArrowRight, Eng::ArrowRight),
            (K::Digit0, Eng::Digit0),
            (K::Digit5, Eng::Digit5),
            (K::Digit9, Eng::Digit9),
        ] {
            assert_eq!(engine_key_from_native(key), expected, "{key:?}");
        }
        // 未列出的键不能丢：退化为原始码（winit 判别值 / Win32 VK）。
        assert_eq!(engine_key_from_native(K::Other(0x5A)), Eng::Other(0x5A));
        // 引擎没有 F 键变体，用虚拟键码承载。
        assert_eq!(engine_key_from_native(K::F3), Eng::Other(0x72));
    }
}

#[cfg(test)]
mod hot_reload_tests {
    use super::hot_reload_enabled;

    /// #495：只有显式的「开」才建轮询线程，发布版默认完全不做额外工作。
    #[test]
    fn only_explicit_truthy_values_enable_hot_reload() {
        for value in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(hot_reload_enabled(Some(value)), "{value}");
        }
        for value in ["0", "false", "", "  ", "off", "no", "2", "enabled"] {
            assert!(!hot_reload_enabled(Some(value)), "{value}");
        }
        assert!(!hot_reload_enabled(None), "未设置 = 关");
    }
}
