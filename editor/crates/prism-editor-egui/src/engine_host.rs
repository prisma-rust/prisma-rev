//! 编辑器内嵌的引擎实例（**零拷贝**宿主）。
//!
//! 与 `prism-app` 的 hosted 模式不同，这里**不建渲染线程**：
//! 编辑器在 egui 的绘制线程上同步驱动 `Engine` + `GraphRenderer`，
//! 渲染结果留在 `GraphRenderer` 自己的离屏图像里，随后把 `VkImageView`
//! 交给 egui 采样（[`crate::viewport`]）。因此：
//!
//! - 只有一条队列、一个线程 → 不需要跨线程 semaphore / fence；
//! - 没有 `vkCmdCopyImageToBuffer`，也没有 CPU 像素回读。
//!
//! 资产上传同样同步完成：`GpuAssetResolver::prepare_requests` →（渲染器）
//! `apply_asset_requests` → `apply_results` 写回 ECS，原本分属两个线程的
//! 两段在这里串起来。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use glam::{Mat4, Quat, Vec3};
use prism_engine::asset_resolver::GpuAssetResolver;
use prism_engine::ecs::{Entity, World};
use prism_engine::input::InputManager;
use prism_engine::render_settings::RenderSettings;
use prism_engine::render_system::{extract_frame_packet_into_with_shadow, FramePacket};
use prism_engine::scene::components::{Camera, LocalToWorld, LocalTransform};
use prism_engine::Engine;
use prism_render::descriptor::FrameUBOData;
use prism_render::{FrameInput, GraphRenderer, VulkanContext};

use crate::diagnostics::Diagnostic;
use crate::project_module::{ProjectEvent, ProjectModuleDriver, ProjectModuleStatus};

/// 视口种类：Scene view 用编辑器轨道相机，Game view 用文档相机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Scene,
    Game,
}

#[allow(dead_code)]
impl ViewKind {
    pub fn icon(self) -> &'static str {
        match self {
            ViewKind::Scene => "🎬",
            ViewKind::Game => "🎮",
        }
    }
}

/// 一个视口背后的引擎实例。
pub struct EngineHost {
    kind: ViewKind,
    /// 用户项目模块（只有 Game view 会挂）。
    ///
    /// **必须声明在 `engine` 之前**：项目 session 的回调指针指向宿主世界，
    /// 卸载顺序必须是「先停项目代码，再拆引擎」。
    project: Option<ProjectModuleDriver>,
    /// 场景重建代数：项目模块用它判断实体句柄是否失效。
    scene_generation: u64,
    engine: Engine,
    asset_resolver: GpuAssetResolver,
    renderer: GraphRenderer,
    input: InputManager,
    packet: FramePacket,
    settings: RenderSettings,
    /// `.scene.json` 文档顺序的实体表（= 层级 / 检查器下标）。
    entities: Vec<Entity>,
    editor_camera: Option<Entity>,
    width: u32,
    height: u32,
    /// Scene view 的编辑器相机是否用场景天空盒当背景（引擎默认开）。
    show_skybox: bool,
    /// Scene view 当前是否是 2D（正交）视图（#641）。由
    /// [`Self::place_editor_camera`] 每帧同步，用来关掉侧着看的 XZ 网格。
    view_2d: bool,
    /// 下一次出帧后把离屏图像读回写到这个路径（编辑器截图）。
    capture_path: Option<PathBuf>,
    simulate: bool,
    paused: bool,
    step_pending: bool,
    accumulator: Duration,
    last_tick: Instant,
    scene_loaded: bool,
    /// 已经卷积进渲染器的环境贴图路径（`None` = 没有 IBL）。
    ///
    /// 改个名字也要重跑 IBL 卷积是整场重载里最贵的部分（#435），所以只有
    /// 场景声明的环境贴图换了才重新 `set_environment`。
    env_path: Option<String>,
    /// `assets/scenes.pak` 的修改时间：只有 pak 本身重建了才重新读它。
    pak_mtime: Option<std::time::SystemTime>,
    /// 上一次检查资产变更的时间（500ms 轮询节流，见 [`EngineHost::poll_asset_changes`]）。
    asset_check_at: Instant,
}

/// 编辑器地面网格只在 Scene 视图出现。
///
/// Game 视图代表独立构建跑起来的样子：网格是编辑器的辅助对象，不属于游戏画面
/// （#454）。这里再挡一层，避免「设置通过别的路径漏进来」时又画上去。
fn editor_grid_visible(
    kind: ViewKind,
    settings: &prism_engine::render_settings::RenderSettings,
    view_2d: bool,
) -> bool {
    // 2D 视图里 XZ 网格是侧着的一条线（XY 网格见 #602），先不画。
    kind == ViewKind::Scene && settings.show_grid && !view_2d
}

impl EngineHost {
    pub fn new(
        kind: ViewKind,
        context: Arc<VulkanContext>,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let mut engine = Engine::empty();
        engine.runtime_initialize();

        let mut asset_resolver = GpuAssetResolver::new();
        asset_resolver.load_resource_package();

        let width = width.max(1);
        let height = height.max(1);
        let renderer = GraphRenderer::offscreen_new_with_context(context, width, height, None)
            .context("create offscreen renderer")?;

        Ok(Self {
            kind,
            project: None,
            scene_generation: 0,
            engine,
            asset_resolver,
            renderer,
            input: InputManager::new(),
            packet: FramePacket::default(),
            settings: RenderSettings::default(),
            entities: Vec::new(),
            editor_camera: None,
            width,
            height,
            show_skybox: true,
            view_2d: false,
            capture_path: None,
            simulate: false,
            paused: false,
            step_pending: false,
            accumulator: Duration::ZERO,
            last_tick: Instant::now(),
            scene_loaded: false,
            env_path: None,
            pak_mtime: resource_package_mtime(),
            asset_check_at: Instant::now(),
        })
    }

    /// 给 Game view 挂上用户项目模块的驱动器（Scene view 不跑项目代码）。
    pub fn attach_project_module(&mut self, root: &Path) {
        if self.kind != ViewKind::Game || self.project.is_some() {
            return;
        }
        self.project = Some(ProjectModuleDriver::new(root));
    }

    pub fn project_module_status(&self) -> Option<ProjectModuleStatus> {
        self.project.as_ref().map(ProjectModuleDriver::status)
    }

    /// Problems 面板的明细（state 里只放条数，避免状态栏每帧克隆诊断）。
    pub fn project_diagnostics(&self) -> &[Diagnostic] {
        self.project
            .as_ref()
            .map(ProjectModuleDriver::diagnostics)
            .unwrap_or_default()
    }

    /// 每帧轮询：收编译结果 / 按需编译。不阻塞 UI 线程。
    pub fn poll_project_module(&mut self, playing: bool) {
        if let Some(project) = self.project.as_mut() {
            project.poll(playing);
        }
    }

    pub fn drain_project_events(&mut self) -> Vec<ProjectEvent> {
        self.project
            .as_mut()
            .map(ProjectModuleDriver::drain_events)
            .unwrap_or_default()
    }

    pub fn refresh_project_module(&mut self) {
        if let Some(project) = self.project.as_mut() {
            project.request_refresh();
        }
    }

    pub fn set_project_auto_refresh(&mut self, enabled: bool) {
        if let Some(project) = self.project.as_mut() {
            project.set_auto_refresh(enabled);
        }
    }

    pub fn project_auto_refresh(&self) -> bool {
        self.project
            .as_ref()
            .map(ProjectModuleDriver::auto_refresh)
            .unwrap_or(true)
    }

    pub fn kind(&self) -> ViewKind {
        self.kind
    }

    pub fn is_loaded(&self) -> bool {
        self.scene_loaded
    }

    pub fn input_mut(&mut self) -> &mut InputManager {
        &mut self.input
    }

    /// 文档下标对应的 cooked mesh 局部 AABB（Scene 视图精确拾取用，#600）。
    ///
    /// 场景尚未完成资产上传时返回 `None`，调用方退回单位立方体。
    pub fn mesh_bounds(&self, index: usize) -> Option<([f32; 3], [f32; 3])> {
        let entity = *self.entities.get(index)?;
        let world = self.engine.world();
        let mesh = world.get::<prism_engine::scene::runtime_components::MeshRef>(entity)?;
        let bounds = world
            .get_resource::<prism_engine::scene::systems::render::MeshBounds>()?
            .get(mesh.render_handle)?;
        Some((bounds.min, bounds.max))
    }

    /// 精灵实际纹理尺寸（好让 Scene 拾取在 `frame_size = [0,0]` 时也能知道
    /// 可见范围，而不是退回单位立方体，#600）。
    pub fn sprite_texture_size(&self, index: usize) -> Option<[f32; 2]> {
        let entity = *self.entities.get(index)?;
        let sprite = self
            .engine
            .world()
            .get::<prism_engine::scene::runtime_components::SpriteRef>(entity)?;
        Some([sprite.size[0] as f32, sprite.size[1] as f32])
    }

    pub fn world_mut(&mut self) -> &mut World {
        self.engine.world_mut()
    }

    pub fn world(&self) -> &World {
        self.engine.world()
    }

    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// 离屏渲染目标尺寸（Game view 的「绘图区像素」，与面板逻辑像素不同）。
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn renderer(&self) -> &GraphRenderer {
        &self.renderer
    }

    /// 把当前渲染图导出成 `target/prism-render-graph.json`（分析面板读它）。
    ///
    /// 编辑器以前只会**读**这个文件，写端在 `prism-app` 里（游戏进程），所以面板
    /// 永远是空的（#451）。现在视口自己写：图和设置都在手上，不需要环境变量。
    pub fn write_render_graph_report(
        &self,
        path: &std::path::Path,
        frame: u64,
    ) -> anyhow::Result<()> {
        let report =
            prism_render::render_graph_report::RenderGraphReport::from_snapshot_with_timings(
                &self.renderer.graph().snapshot(),
                self.settings.to_report_settings(),
                frame,
                Some(self.renderer.gpu_timings()),
            )
            .with_stats(self.renderer.render_stats())
            .with_draw_calls(self.renderer.draw_call_report(
                &self.packet.draw_items,
                &self.packet.sprite_items,
                &self.packet.particle_items,
            ));
        report.write_json(path)?;
        Ok(())
    }

    pub fn settings_mut(&mut self) -> &mut RenderSettings {
        &mut self.settings
    }

    /// 局部光源预算 `(kept, total)`：实际画出来的 / 场景里启用的。
    ///
    /// `total > kept` 说明有灯被前向渲染的 `LIGHT_MAX` 挤掉了（#468），
    /// 视口角标与检查器据此提示。
    pub fn local_light_stats(&self) -> (usize, usize) {
        (
            self.packet.scene.local_lights_kept,
            self.packet.scene.local_lights_total,
        )
    }

    /// 被 `LIGHT_MAX` 挤掉的局部光实体（`.scene.json` 的 id）。
    pub fn dropped_light_ids(&self) -> &[u32] {
        &self.packet.scene.dropped_light_ids
    }

    /// 从 `.scene.json` 重建世界（与 Tauri / GPUI 编辑器同一条路径）。
    pub fn load_scene(&mut self, path: &Path) -> Result<usize, String> {
        // 旧世界的实体句柄全部失效：项目模块按这个代数丢弃缓存的句柄。
        self.scene_generation = self.scene_generation.wrapping_add(1).max(1);
        // 旧世界的实体句柄全部失效，helper 实体要重新创建。
        self.editor_camera = None;
        self.engine = Engine::empty();
        // 资产解析器（含已解的 `.pak` 与已上传的句柄）跨重载复用：以前每次
        // 编辑都 `GpuAssetResolver::new()` + 重读 pak（#435）。只有 pak 文件
        // 本身重建了才需要重读清单。
        let mtime = resource_package_mtime();
        if mtime != self.pak_mtime {
            self.asset_resolver.load_resource_package();
            self.pak_mtime = mtime;
        }
        let entities = self.engine.load_scene_json_file(path)?;
        self.engine.runtime_initialize();
        self.engine
            .world_mut()
            .insert_resource(prism_engine::ui::ScreenSize::new(self.width, self.height));

        self.refresh_scene_environment();

        let count = entities.len();
        self.entities = entities;
        self.scene_loaded = true;

        // 加载器只保证根实体的 `LocalToWorld`；立刻传播一次，让相机 / 高亮
        // 在第一帧就落在正确位置。
        prism_engine::scene::systems::hierarchy::hierarchy_system(self.engine.world_mut());
        prism_engine::scene::systems::hierarchy::active_hierarchy_system(self.engine.world_mut());

        self.ensure_helper_entities();
        self.accumulator = Duration::ZERO;
        self.last_tick = Instant::now();
        Ok(count)
    }

    /// Scene view 的编辑器相机 + 背景策略。
    ///
    /// 引擎的 Scene view 默认把场景天空盒当背景（`SceneViewState.showSkybox`），
    /// 所以这里的编辑器相机默认是 `Skybox` 而不是纯色。
    fn editor_clear_flags(&self) -> prism_scene::CameraClearFlags {
        if self.show_skybox {
            prism_scene::CameraClearFlags::Skybox
        } else {
            prism_scene::CameraClearFlags::SolidColor
        }
    }

    /// 切换 Scene view 背景（天空盒 / 纯色），立即作用到编辑器相机。
    pub fn set_show_skybox(&mut self, on: bool) {
        if self.show_skybox == on {
            return;
        }
        self.show_skybox = on;
        let Some(entity) = self.editor_camera else {
            return;
        };
        let flags = self.editor_clear_flags();
        if let Some(camera) = self.engine.world_mut().get_mut::<Camera>(entity) {
            camera.clear_flags = flags;
        }
    }

    /// 编辑器相机 + 选中标记（只在 Scene view 创建）。
    fn ensure_helper_entities(&mut self) {
        if self.kind == ViewKind::Scene {
            if self.editor_camera.is_none() {
                let camera = Camera {
                    depth: 1000.0,
                    far: 5000.0,
                    aspect: self.width as f32 / self.height.max(1) as f32,
                    clear_flags: self.editor_clear_flags(),
                    background_color: [0.11, 0.12, 0.14, 1.0],
                    ..Camera::default()
                };
                let world = self.engine.world_mut();
                let entity = world.spawn();
                world.insert(entity, camera);
                world.insert(entity, LocalTransform::default());
                world.insert(entity, LocalToWorld(Mat4::IDENTITY));
                self.editor_camera = Some(entity);
            }
        }
    }

    /// 把轨道相机写进编辑器相机实体（Scene view 每帧调用）。
    ///
    /// 与 Tauri 编辑器一致：`LocalTransform` 与 `LocalToWorld` 同时写，
    /// 因为 extract 只读 `LocalToWorld`，而相机实体没有父级、不参与传播。
    pub fn place_editor_camera(&mut self, orbit: &prism_editor_core::workspace::OrbitCamera) {
        let Some(entity) = self.editor_camera else {
            return;
        };
        let (yaw, pitch, distance) = (orbit.yaw, orbit.pitch, orbit.distance);
        self.view_2d = orbit.orthographic;
        let target = Vec3::from_array(orbit.target);
        // Keep the rendered camera on the same orbit as `OrbitCamera::eye()`:
        // yaw is world-Y, while positive pitch raises the eye toward +Y.
        let rotation = Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-pitch);
        let eye = target + rotation * Vec3::new(0.0, 0.0, distance.max(0.05));
        let forward = (target - eye).normalize_or_zero();
        let look = if forward.length_squared() > 0.0 {
            Quat::from_rotation_arc(Vec3::NEG_Z, forward)
        } else {
            Quat::IDENTITY
        };

        let world = self.engine.world_mut();
        world.insert(
            entity,
            LocalTransform {
                position: eye,
                rotation: look,
                scale: Vec3::ONE,
            },
        );
        world.insert(
            entity,
            LocalToWorld(Mat4::from_rotation_translation(look, eye)),
        );
        if let Some(camera) = world.get_mut::<Camera>(entity) {
            camera.aspect = self.width as f32 / self.height.max(1) as f32;
            // 2D 视图：正交半高直接复用 `distance`，滚轮缩放的手感不变（#641）。
            camera.orthographic = orbit.orthographic;
            if orbit.orthographic {
                camera.orthographic_size = orbit.distance.max(0.05);
            }
            // 编辑器的「View…」设置（#416）：FOV 与裁剪面同时作用于渲染与拾取，
            // 否则屏幕上的 gizmo / 点选会和画面对不上。
            camera.fov_y_degrees = orbit.fov_y_degrees.clamp(10.0, 170.0);
            camera.near = orbit.near;
            camera.far = orbit.far;
        }
    }

    pub fn set_simulate(&mut self, simulate: bool) {
        if self.simulate != simulate {
            self.accumulator = Duration::ZERO;
            self.last_tick = Instant::now();
            self.paused = false;
            self.step_pending = false;
            if !simulate {
                // 停止 Play 时丢弃按键 / 鼠标的瞬时状态：否则下一次 Play
                // 会继承上一次的「刚按下」。
                self.input = InputManager::new();
            }
        }
        self.simulate = simulate;
    }

    pub fn is_simulating(&self) -> bool {
        self.simulate
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn step(&mut self) {
        self.paused = true;
        self.step_pending = true;
    }

    pub fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        self.width = width;
        self.height = height;
        self.renderer.resize_offscreen(width, height)?;
        if self.kind == ViewKind::Scene {
            // 尺寸变化后编辑器相机的 aspect 要跟上。
            if let Some(entity) = self.editor_camera {
                if let Some(camera) = self.engine.world_mut().get_mut::<Camera>(entity) {
                    camera.aspect = width as f32 / height as f32;
                }
            }
        }
        Ok(())
    }

    /// 场景声明式光照：只有声明的环境贴图真的换了才重新卷积 IBL。
    fn refresh_scene_environment(&mut self) {
        let env_path =
            prism_engine::scene::SceneReadView::new(&self.engine.world()).environment_asset_path();
        if env_path == self.env_path {
            return;
        }
        let env_bytes = self
            .engine
            .current_scene_env_bytes_with_provider(&mut self.asset_resolver.resource_manager);
        if let Err(error) = self.renderer.set_environment(env_bytes) {
            log::warn!("set scene environment failed: {error:#}");
        }
        self.env_path = env_path;
    }

    /// 执行项目代码请求的换场（#482）。在 tick 的引擎 update 之后调用。
    ///
    /// 复用同一个 `Engine`（`Single` 会保留 `Persistent` 子树），并推进
    /// `scene_generation`，让项目模块丢掉旧世界的实体句柄；旧资产的 GPU 槽位
    /// 由解析器记账后交回渲染器回收（#495）。新场景加载失败时保留旧场景。
    fn apply_scene_request(&mut self, request: prism_runtime_host::SceneRequest) {
        match request {
            prism_runtime_host::SceneRequest::Load { path, additive } => {
                let mode = if additive {
                    prism_engine::engine::LoadSceneMode::Additive
                } else {
                    prism_engine::engine::LoadSceneMode::Single
                };
                match self
                    .engine
                    .load_scene_json_file_with_mode(Path::new(&path), mode)
                {
                    Ok(entities) => {
                        self.scene_generation = self.scene_generation.wrapping_add(1).max(1);
                        log::info!(
                            "scene switched to {path} ({} entities, additive={additive})",
                            entities.len()
                        );
                    }
                    Err(error) => {
                        log::error!(
                            "scene switch to {path} failed: {error}; keeping the current scene"
                        );
                        return;
                    }
                }
            }
            prism_runtime_host::SceneRequest::Unload => {
                let count = self.engine.unload_scene();
                self.scene_generation = self.scene_generation.wrapping_add(1).max(1);
                log::info!("scene unloaded ({count} entities)");
            }
        }
        self.engine.runtime_initialize();
        self.asset_resolver
            .forget_missing_entities(self.engine.world_mut());
        self.refresh_scene_environment();
        self.pump_assets();
    }

    /// 推进模拟（Play）或只传播层级（编辑态），并把资产上传同步做完。
    pub fn tick(&mut self) {
        // ECS UI 布局依赖 `ScreenSize`；离屏宿主没有 winit 窗口，自己写。
        self.engine
            .world_mut()
            .insert_resource(prism_engine::ui::ScreenSize::new(self.width, self.height));
        let now = Instant::now();
        let elapsed = now
            .saturating_duration_since(self.last_tick)
            .min(Duration::from_millis(100));
        self.last_tick = now;

        let mut run_fixed = 0u32;
        if self.simulate && !self.paused {
            self.accumulator += elapsed;
            const FIXED_STEP: Duration = Duration::from_nanos(16_666_667);
            while self.accumulator >= FIXED_STEP && run_fixed < 8 {
                self.engine
                    .fixed_update(FIXED_STEP.as_secs_f32(), &self.input);
                self.accumulator -= FIXED_STEP;
                run_fixed += 1;
            }
        }

        if self.simulate && (!self.paused || self.step_pending) {
            let dt = if self.step_pending {
                xstep_dt()
            } else {
                elapsed.as_secs_f32()
            };
            self.step_pending = false;
            // 用户项目的 gameplay 代码先于引擎 system 执行：它写入的组件
            // 和 UI 四边形在同一帧被 hierarchy / ui::render 看到。
            let mut scene_request = None;
            if let Some(project) = self.project.as_mut() {
                project.tick(
                    self.engine.world_mut(),
                    &self.input,
                    dt,
                    self.scene_generation,
                    [self.width as f32, self.height as f32],
                );
                scene_request = project.take_scene_request();
            }
            // 项目代码请求的换场在世界借用结束后执行（#482）。
            if let Some(request) = scene_request {
                self.apply_scene_request(request);
            }
            self.engine.update(dt, &self.input);
            self.engine.late_update();
        } else {
            // 编辑态（或暂停）：不跑游戏 system，只传播层级。
            self.accumulator = Duration::ZERO;
            prism_engine::scene::systems::hierarchy::hierarchy_system(self.engine.world_mut());
            prism_engine::scene::systems::hierarchy::active_hierarchy_system(
                self.engine.world_mut(),
            );
        }

        self.pump_assets();
        self.input.begin_frame();
    }

    /// 轮询 `assets/scenes.pak`：重建过就换掉 reader、把已上传的资产打回待解析（#495）。
    ///
    /// 每 500ms 最多检查一次（与 `prism_asset` 的 watcher 同节奏）。返回重新解析的
    /// 实体数；0 表示这一帧什么都没变。上传在这里同步走完（和 [`Self::pump_assets`]
    /// 同一条路径），所以 Play 中的下一帧就能看到新贴图 / 新网格，不用退出再进。
    ///
    /// 只处理 `.pak`：运行时只从包里读资产，单独改一张松散贴图不会进到内存里。
    pub fn poll_asset_changes(&mut self) -> usize {
        const INTERVAL: Duration = Duration::from_millis(500);
        if self.asset_check_at.elapsed() < INTERVAL {
            return 0;
        }
        self.asset_check_at = Instant::now();

        let Some(pak) = prism_engine::asset_paths::asset_path("scenes.pak") else {
            return 0;
        };
        let mtime = std::fs::metadata(&pak)
            .and_then(|meta| meta.modified())
            .ok();
        if mtime == self.pak_mtime {
            return 0;
        }
        // 打开失败（pak 正在被写）就等下一轮，别把 reader 换成半截文件。
        if let Err(error) = self.asset_resolver.resource_manager.on_pak_changed(&pak) {
            log::warn!(
                "hot reload: reopening {} failed: {error}; will retry",
                pak.display()
            );
            return 0;
        }
        self.pak_mtime = mtime;
        if let Some(manifest) = prism_engine::asset_paths::asset_path("scenes.pak.meta.json") {
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
        let count = self
            .asset_resolver
            .invalidate_assets(self.engine.world_mut());
        self.pump_assets();
        log::info!(
            "hot reload: {count} entity/entities re-resolved after {} was rebuilt",
            pak.display()
        );
        count
    }

    /// 资产解析：CPU 段 → GPU 上传 → 结果写回 ECS，全部同步。
    fn pump_assets(&mut self) {
        // 工程自带字体（#583）：读 `.pak` 里的字节交给 UI 图集。只在新路径出现时
        // 读一次，和贴图/网格的失败缓存同一策略。
        let fonts = self.asset_resolver.prepare_ui_fonts(self.engine.world());
        for (name, bytes) in fonts {
            self.renderer.register_ui_font(name, bytes);
        }

        let requests = self
            .asset_resolver
            .prepare_requests(self.engine.world_mut());
        // 失效的 GPU 资源同步释放（编辑器没有渲染线程，#495）。
        let releases = self.asset_resolver.take_releases();
        if !releases.is_empty() {
            self.renderer.release_assets(&releases);
        }
        if !requests.is_empty() {
            let (entities, stripped): (Vec<Entity>, Vec<_>) = requests.into_iter().unzip();
            let results = self.renderer.apply_asset_requests(stripped);
            let paired: Vec<(Entity, _)> = entities.into_iter().zip(results).collect();
            self.asset_resolver
                .apply_results(self.engine.world_mut(), &paired);
        }
    }

    /// 渲染一帧到离屏图像，并把图像转成可采样的
    /// `SHADER_READ_ONLY_OPTIMAL`。返回 `(image_view, extent)`。
    pub fn render(&mut self) -> anyhow::Result<Option<(ash::vk::ImageView, ash::vk::Extent2D)>> {
        // 编辑态不跑 `tick()`（它只在 Play 时被调用），但出帧同样需要「模拟步末尾」
        // 那两步：层级传播 + 资产上传。否则 `LocalToWorld` 停在加载时的值，
        // `SpriteRef::generation == 0` 又会让 2D 渲染系统把所有精灵过滤掉 ——
        // 编辑态的 Scene / Game 视图就只剩清屏色（#579）。
        if !self.simulate {
            prism_engine::scene::systems::hierarchy::hierarchy_system(self.engine.world_mut());
            prism_engine::scene::systems::hierarchy::active_hierarchy_system(
                self.engine.world_mut(),
            );
        }
        self.pump_assets();
        // #613：内部渲染分辨率与游戏进程走同一条路径，编辑器 Game 视图看到的
        // 像素比例就是真机的比例。
        let aspect = self
            .settings
            .render_aspect(self.width as f32 / self.height.max(1) as f32);
        extract_frame_packet_into_with_shadow(
            self.engine.world_mut(),
            aspect,
            &Mat4::IDENTITY,
            self.settings.shadow_distance,
            &mut self.packet,
        );
        self.packet.settings = self.settings.clone();

        let extent = self.renderer.extent();
        let scene = &self.packet.scene;
        let frame_data = FrameUBOData {
            view_proj: scene.view_proj.to_cols_array_2d(),
            camera_position: [
                scene.eye.x,
                scene.eye.y,
                scene.eye.z,
                scene.lights.len() as f32,
            ],
            light_direction: scene.light_direction.to_array(),
            light_color: scene.light_color.to_array(),
            view: scene.view.to_cols_array_2d(),
            light_view_proj: scene.light_view_proj.to_cols_array_2d(),
            tonemap_mode: self.settings.tonemap_mode,
            viewport_size: [extent.width as f32, extent.height as f32],
            exposure: scene.exposure,
            shadow_strength: scene.shadow_strength,
            _pad2: [0.0; 3],
        };

        let input = FrameInput {
            draw_items: &self.packet.draw_items,
            gizmos: &self.packet.gizmos,
            sprite_items: &self.packet.sprite_items,
            sprite_lights: &self.packet.sprite_lights,
            particle_items: &self.packet.particle_items,
            skybox: self.packet.skybox,
            frame_data: &frame_data,
            light_view_proj: scene.light_view_proj.to_cols_array_2d(),
            inv_projection: scene.inv_projection.to_cols_array_2d(),
            debug_flags: self.settings.debug_flags,
            tonemap_mode: self.settings.tonemap_mode,
            aa_mode: self.settings.aa_mode,
            debug_rt: self.settings.debug_rt,
            grade: prism_render::PostGrade {
                contrast: self.settings.contrast,
                saturation: self.settings.saturation,
                vignette: self.settings.vignette,
            },
            proj22: scene.proj22,
            proj32: scene.proj32,
            lights: &scene.lights,
            render_mode: self.settings.render_mode,
            pt_max_bounces: self.settings.pt_max_bounces,
            pt_ray_max_distance: self.settings.pt_ray_max_distance,
            pt_max_iterations: self.settings.pt_max_iterations,
            exposure: scene.exposure,
            pt_lights: &scene.pt_lights,
            // 编辑器只在「有变化」时重绘，因此每次渲染都视为新的累积起点。
            pt_accum_dirty: true,
            has_camera: scene.has_camera,
            camera_orthographic: scene.camera_orthographic,
            clear_color: scene.clear_color,
            shadows_enabled: scene.shadows_enabled,
            ui_overlay: Some(&self.packet.ui_overlay),
            show_grid: editor_grid_visible(self.kind, &self.settings, self.view_2d),
        };

        self.renderer.set_internal_resolution(
            self.settings.internal_resolution,
            self.settings.internal_scale_mode,
        );
        let Some(ctx) = self.renderer.begin_frame()? else {
            return Ok(None);
        };
        let result = self
            .renderer
            .prepare(&ctx, &input)
            .and_then(|_| self.renderer.execute(&ctx, &input))
            .and_then(|_| self.renderer.present(&ctx))
            .and_then(|_| self.renderer.end_frame(&ctx, false));
        if let Err(error) = result {
            return Err(error);
        }

        // 截图要读离屏图像当前布局（帧结束是 `COLOR_ATTACHMENT_OPTIMAL`），
        // 所以必须在转采样布局**之前**做。
        if let Some(path) = self.capture_path.take() {
            match self.capture_offscreen(&path) {
                Ok(path) => log::info!("viewport capture: {}", path.display()),
                Err(error) => log::warn!("viewport capture failed: {error:#}"),
            }
        }

        // COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL：egui 的采样描述符要求
        // 后者，且这一步在同一个队列上完成，不需要额外 semaphore。
        self.renderer.offscreen_present_for_sampling()?;

        Ok(self
            .renderer
            .offscreen_image_view()
            .map(|view| (view, extent)))
    }

    /// 请求**下一次出帧**时把离屏图像读回并写成 PNG（编辑器截图用）。
    pub fn request_capture(&mut self, path: PathBuf) {
        self.capture_path = Some(path);
    }

    /// 离屏图像 → host-visible 缓冲区 → RGBA8 → PNG。
    fn capture_offscreen(&mut self, path: &Path) -> anyhow::Result<PathBuf> {
        let extent = self
            .renderer
            .offscreen_extent()
            .context("no offscreen target to capture")?;
        self.renderer.copy_offscreen_to_readback()?;
        let pixels = self.renderer.readback_pixels()?;
        prism_render::screenshot::save_rgba_png(path, extent.width, extent.height, &pixels, None)
    }

    pub fn shutdown(&mut self) {
        // 项目 session 必须先于引擎销毁：模块回调借用了宿主世界。
        self.project = None;
        self.engine.pre_shutdown();
        self.engine.post_shutdown();
    }
}

/// 单步模拟使用的固定步长。
fn xstep_dt() -> f32 {
    1.0 / 60.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_kind_metadata_is_stable() {
        assert_eq!(ViewKind::Scene.icon(), "🎬");
        assert_eq!(ViewKind::Game.icon(), "🎮");
        assert_ne!(ViewKind::Scene, ViewKind::Game);
    }

    /// #454：网格是 Scene 视图的辅助对象，Game 视图永远不画。
    #[test]
    fn the_editor_grid_never_reaches_the_game_view() {
        let mut settings = prism_engine::render_settings::RenderSettings::default();
        settings.show_grid = true;
        assert!(editor_grid_visible(ViewKind::Scene, &settings, false));
        assert!(!editor_grid_visible(ViewKind::Game, &settings, false));

        settings.show_grid = false;
        assert!(!editor_grid_visible(ViewKind::Scene, &settings, false));
        assert!(!editor_grid_visible(ViewKind::Game, &settings, false));
    }

    /// #641：2D 视图里 XZ 网格是侧着的一条线，必须关掉。
    #[test]
    fn the_grid_is_hidden_in_the_2d_view() {
        let mut settings = prism_engine::render_settings::RenderSettings::default();
        settings.show_grid = true;
        assert!(!editor_grid_visible(ViewKind::Scene, &settings, true));
    }

    #[test]
    fn simulation_step_uses_fixed_sixtieth_second() {
        assert!((xstep_dt() - 1.0 / 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn editor_camera_rotation_matches_orbit_camera_eye_convention() {
        let yaw = 0.0;
        let pitch = 0.0;
        let distance = 12.0;
        let rotation = Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-pitch);
        let eye_offset = rotation * Vec3::new(0.0, 0.0, distance);
        let (sin_yaw, cos_yaw) = yaw.sin_cos();
        let (sin_pitch, cos_pitch) = pitch.sin_cos();
        let expected = Vec3::new(
            distance * cos_pitch * sin_yaw,
            distance * sin_pitch,
            distance * cos_pitch * cos_yaw,
        );
        assert!((eye_offset - expected).length() < 1e-5);
    }
}

/// `assets/scenes.pak` 的修改时间；没有 pak 或读不到元数据时为 `None`。
fn resource_package_mtime() -> Option<std::time::SystemTime> {
    let path = prism_engine::asset_paths::asset_path("scenes.pak")?;
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}
