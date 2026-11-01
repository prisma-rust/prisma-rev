//! 场景系统——负责场景加载、切换与「当前场景」状态。
//!
//! # 职责
//!
//! [`SceneManager`] 是引擎的场景子系统：它拥有「当前场景名」，并统一负责
//! 从 `assets/scenes.json` manifest 加载场景到 ECS 世界。场景**内容**只按
//! [`prism_asset::core::AssetId`] 从资源包（`.pak`）加载：`scenes.json` 的
//! `path` 只是路径清单（`.pak.meta.json`）中的查找键，先解析为 `AssetId`，
//! 再由 [`ResourceManager`] 读取 RSCN 字节——不做任何磁盘文件回退。
//! 早期版本由 `Engine` 直接扫描 manifest 加载场景，现收拢到本系统
//! （`Engine::init_scene` 只做转发）——这是 计划「场景加载应该由场景系统
//! 负责」的落地。
//!
//! # 存档
//!
//! 存档（存/读档）**不是**引擎/场景系统的职责——`Engine` 不再在初始化时
//! 自动读取 `scene_state.json`，只通过 `Engine::save_scene_state` /
//! `Engine::load_scene_state` 暴露 [`crate::scene_state`] 原语，由用户项目
//! 决定何时调用。

use std::path::PathBuf;

use prism_asset::core::AssetId;
use prism_asset::runtime::{ResourceManager, SceneAsset};
use prism_ecs::{Entity, World};

use super::component_registry::ComponentRegistry;
use super::loader::{SceneInstance, SceneLoader, SceneSource};

/// 场景系统：拥有当前场景状态并负责场景加载。
pub struct SceneManager {
    /// 当前已加载场景名（manifest 中 `scenes[].name`）。
    current_scene_name: Option<String>,
    /// 当前场景在资源包内的路径（例如 `scenes/default.rscn`），
    /// 用于把作者相对路径解析成包内路径。
    current_scene_path: Option<String>,
    environment_bytes: Option<Vec<u8>>,
    /// 场景代数：每次成功切换 / 卸载都递增。项目模块靠它判断缓存的实体句柄
    /// 是否已经失效（`prism-project-api` 的 `scene_generation`）。
    generation: u64,
}

/// 当前场景环境资源的中性读取接口。
pub trait EnvironmentProvider {
    fn load_environment(&mut self, asset_path: &str) -> Option<Vec<u8>>;
}

/// 渲染/资源阶段使用的只读 ECS 场景视图。
pub struct SceneReadView<'a> {
    world: &'a World,
}

impl<'a> SceneReadView<'a> {
    pub fn new(world: &'a World) -> Self {
        Self { world }
    }

    pub fn environment_asset_path(&self) -> Option<String> {
        // Skybox is the authoritative source. A scene with a disabled Skybox
        // deliberately opts out of IBL, so do not fall back to the legacy
        // EnvironmentLighting component in that case.
        let mut has_skybox = false;
        for (_, skybox) in self.world.query::<super::components::Skybox>() {
            has_skybox = true;
            if skybox.enabled && !skybox.hdr_path.is_empty() {
                return Some(skybox.hdr_path.clone());
            }
        }
        if has_skybox {
            return None;
        }

        // Legacy compatibility for scenes authored before Skybox owned the
        // environment map.
        self.world
            .query::<super::components::EnvironmentLighting>()
            .next()
            .and_then(|(_, env)| env.env_map.clone())
    }

    /// Per-frame skybox parameters (rotation/intensity) from the first enabled
    /// Skybox component.
    pub fn skybox_settings(&self) -> Option<prism_render::SkyboxSettings> {
        self.world
            .query::<super::components::Skybox>()
            .find(|(_, skybox)| skybox.enabled)
            .map(|(_, skybox)| prism_render::SkyboxSettings {
                rotation: skybox.rotation_degrees.to_radians(),
                intensity: skybox.intensity.max(0.0),
            })
    }

    /// One gizmo instance per enabled `Gizmo` entity, using its world
    /// transform multiplied by the component's additional scale.
    pub fn gizmo_instances(&self) -> Vec<prism_render::GizmoInstance> {
        let mut out = Vec::new();
        self.gizmo_instances_into(&mut out);
        out
    }

    /// [`Self::gizmo_instances`] into a caller-owned buffer (frame-packet reuse).
    pub fn gizmo_instances_into(&self, out: &mut Vec<prism_render::GizmoInstance>) {
        out.clear();
        out.extend(
            self.world
                .query::<super::components::Gizmo>()
                .filter(|(_, gizmo)| gizmo.enabled)
                .filter_map(|(entity, gizmo)| {
                    let transform = self.world.get::<super::components::LocalToWorld>(entity)?;
                    let model = transform.0
                        * glam::Mat4::from_scale(glam::Vec3::splat(gizmo.scale.max(0.0)));
                    Some(prism_render::GizmoInstance {
                        model: model.to_cols_array_2d(),
                        mode: match gizmo.mode {
                            super::components::GizmoMode::Translate => 0,
                            super::components::GizmoMode::Rotate => 1,
                            super::components::GizmoMode::Scale => 2,
                        },
                    })
                }),
        );
    }

    /// 提取 2D 绘制列表（精灵 + 折线，已按 `sorting_layer` →
    /// `order_in_layer` → 世界 `z` → 实体 id 排序，先画远、后画近）。
    pub fn sprite_items(&self) -> Vec<prism_render::SpriteDrawItem> {
        super::systems::render::scene_2d_system(self.world)
    }

    /// [`Self::sprite_items`] into a caller-owned buffer (frame-packet reuse).
    pub fn sprite_items_into(&self, out: &mut Vec<prism_render::SpriteDrawItem>) {
        super::systems::render::scene_2d_system_into(self.world, out)
    }

    /// [`Self::sprite_items_into`] 加主相机视锥（瓦片地图逐格裁剪，#529）。
    pub fn sprite_items_with_frustum_into(
        &self,
        frustum: Option<&prism_render::culling::Frustum>,
        out: &mut Vec<prism_render::SpriteDrawItem>,
    ) {
        super::systems::render::scene_2d_system_into_with_frustum(self.world, frustum, out)
    }

    /// 提取 `Light2D`（#611），已按贡献修剪到 `SPRITE_LIGHT_MAX`。
    pub fn sprite_lights_into(&self, out: &mut Vec<prism_render::SpriteLight>) {
        out.clear();
        out.extend(super::systems::render::sprite_light_entries(self.world));
    }

    /// 提取 3D 粒子 billboard（已按到 `eye` 的距离**远到近**排序）。
    pub fn particle_items_into(
        &self,
        eye: glam::Vec3,
        out: &mut Vec<prism_render::ParticleDrawItem>,
    ) {
        super::systems::particles::scene_particles_into(self.world, eye, out)
    }

    /// 提取渲染所需的扁平绘制列表，不暴露可变 ECS 访问。
    ///
    /// 委托给 [`crate::scene::systems::render::scene_render_system`]：这里曾经是
    /// 一份复制粘贴的实现，两处查询条件（尤其 `MaterialRef` 是否必需）会各自漂移。
    pub fn draw_items(&self) -> Vec<prism_render::DrawItem> {
        crate::scene::systems::render::scene_render_system(self.world)
    }

    /// [`Self::draw_items`] into a caller-owned buffer (frame-packet reuse).
    pub fn draw_items_into(&self, out: &mut Vec<prism_render::DrawItem>) {
        crate::scene::systems::render::scene_render_system_into(self.world, out)
    }

    /// [`Self::draw_items_into`] with camera frustum culling (#491).
    ///
    /// `frustum == None` 时等同于 [`Self::draw_items_into`]。
    pub fn draw_items_with_frustum_into(
        &self,
        frustum: Option<&prism_render::culling::Frustum>,
        out: &mut Vec<prism_render::DrawItem>,
    ) {
        crate::scene::systems::render::scene_render_system_into_with_frustum(
            self.world, frustum, out,
        )
    }
}

/// 把场景里的相对资源路径解析成资源包内路径。
///
/// 例：场景 `scenes/default.rscn` + `../valley.hdr` -> `valley.hdr`；
/// `assets/valley.hdr` -> `valley.hdr`。
fn normalize_scene_asset_path(scene_path: Option<&str>, asset_path: &str) -> String {
    let asset_path = asset_path.replace('\\', "/");
    if let Some(rest) = asset_path.strip_prefix("assets/") {
        return rest.to_string();
    }
    if asset_path.starts_with('/') {
        return asset_path;
    }

    let mut parts: Vec<&str> = Vec::new();
    if let Some(dir) = scene_path.and_then(|path| path.rsplit_once('/').map(|(dir, _)| dir)) {
        parts.extend(dir.split('/').filter(|part| !part.is_empty()));
    }
    for part in asset_path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

impl Default for SceneManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SceneManager {
    /// 创建一个空的场景系统（尚无当前场景）。
    pub fn new() -> Self {
        Self {
            current_scene_name: None,
            current_scene_path: None,
            environment_bytes: None,
            generation: 0,
        }
    }

    /// 当前场景名；尚未从 manifest 加载任何场景时为 `None`。
    pub fn current_scene_name(&self) -> Option<&str> {
        self.current_scene_name.as_deref()
    }

    /// 场景代数（从 0 开始，每次切换 / 卸载 +1）。
    ///
    /// 宿主（编辑器 / 玩家）把它传给项目模块，模块据此丢弃缓存的实体句柄：
    /// 换场景之后旧句柄指向的实体已经不存在了。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 递增代数并返回新值。
    pub fn bump_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    /// 接管刚刚 spawn 完的**作者格式**场景（`Engine::load_scene_json_file`）。
    ///
    /// `additive = false`（`LoadSceneMode::Single`）：卸载旧场景实体、清空
    /// current_scene / 环境缓存并递增代数 —— 再加载一次不会让实体翻倍。
    /// `additive = true`：只加载不卸载，`Persistent` 与旧场景都留着。
    ///
    /// 必须在新场景 spawn **成功之后**调用：失败时保持旧场景不动。
    pub fn adopt_author_scene(
        &mut self,
        world: &mut World,
        inst: &SceneInstance,
        additive: bool,
    ) -> usize {
        if additive {
            log::info!(
                "scene: additive load kept {} existing scene entity(ies)",
                world.query::<super::components::SceneMember>().count()
            );
            return 0;
        }
        let unloaded = unload_scene_members(world, Some(inst.scene_id));
        // 作者格式没有 manifest 名字：清掉上一次 manifest 场景的身份，
        // 否则 `current_scene_name()` 会指向一个已经被卸掉的场景。
        self.current_scene_name = None;
        self.current_scene_path = None;
        self.environment_bytes = None;
        self.bump_generation();
        unloaded
    }

    /// 卸载当前场景（`Persistent` 子树除外），返回被卸载的实体数。
    ///
    /// 与 [`Self::adopt_scene`] 的区别：这里不加载任何新场景，用于
    /// 「回主菜单」「重开一局」这类只需要清场的场合。引擎自有实体
    /// （没有 `SceneMember`，例如 `runtime_initialize` 的回退相机）不会被删。
    pub fn unload_scene(&mut self, world: &mut World) -> usize {
        let unloaded = unload_scene_members(world, None);
        let had_scene = self.current_scene_name.is_some();
        self.current_scene_name = None;
        self.current_scene_path = None;
        self.environment_bytes = None;
        self.bump_generation();
        if had_scene || unloaded > 0 {
            log::info!(
                "scene unloaded: {unloaded} entity(ies) removed (generation {})",
                self.generation
            );
        }
        unloaded
    }

    /// 注入当前场景的环境贴图字节。字节由运行时资源服务提供，场景系统
    /// 不负责文件读取或格式解码。
    pub fn set_environment_bytes(&mut self, bytes: Option<Vec<u8>>) {
        self.environment_bytes = bytes;
    }

    /// 从 `assets/scenes.json` manifest 加载**第一个**已注册进资源包的场景到世界。
    ///
    /// 引擎初始化（`Engine::init_scene`）会委托本方法：场景一律按 `AssetId`
    /// 从 [`ResourceManager`] 加载（`scenes.json` 的 `path` → 路径清单 → `AssetId`）。
    /// 返回加载成功的场景名并记为当前场景；无 manifest / 无可用场景时返回 `None`。
    pub fn load_first_from_manifest(
        &mut self,
        rm: &mut ResourceManager,
        world: &mut World,
        registry: &ComponentRegistry,
    ) -> Option<String> {
        self.load_from_manifest(rm, world, registry, None)
    }

    /// 从 `assets/scenes.json` 按名字加载指定场景。
    ///
    /// 清单中找不到该名字或加载失败时，保持「当前场景」不变并返回 `None`。
    pub fn load_named_from_manifest(
        &mut self,
        rm: &mut ResourceManager,
        world: &mut World,
        registry: &ComponentRegistry,
        name: &str,
    ) -> Option<String> {
        self.load_from_manifest(rm, world, registry, Some(name))
    }

    /// 从调用方提供的内存 manifest 加载场景。
    ///
    /// 运行时宿主可以从 `.pak`、Android asset 或网络资源注入清单，避免
    /// 场景系统依赖当前工作目录。文件入口仅作为兼容层保留。
    pub fn load_from_manifest_text(
        &mut self,
        rm: &mut ResourceManager,
        world: &mut World,
        registry: &ComponentRegistry,
        text: &str,
        scene_name: Option<&str>,
    ) -> Option<String> {
        let manifest: SceneManifest = match SceneManifest::parse(text) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("scene manifest parse error: {e}");
                return None;
            }
        };
        self.load_entries(rm, world, registry, &manifest, scene_name)
    }

    /// 解析「当前场景」声明式光照所需的环境贴图字节。
    ///
    /// 取代旧的 [`load_env_bytes_from_manifest`]，后者扫描 manifest 所有 `.rscn`，
    /// 导致 UI 开屏也加载 3D 环境贴图。新逻辑按 `current_scene_name` 定位场景：
    /// - `.rscn`：复用 `read_env_path_from_rscn` 自动派生（向后兼容，default/sponza
    ///   的 IBL 无需改场景即保留）。
    /// - `.scene.json`：从已加载的 ECS 世界查询 `EnvironmentLighting` 组件；
    ///   无该组件或 `env_map = None` → 返回 `None`（开屏等无光照场景不构建 IBL）。
    #[cfg(feature = "legacy-disk-scenes")]
    #[deprecated(
        note = "use current_scene_env_bytes_with_provider; this fallback reads scene files"
    )]
    pub fn current_scene_env_bytes(&self, world: &World) -> Option<Vec<u8>> {
        if let Some(bytes) = &self.environment_bytes {
            return Some(bytes.clone());
        }
        let name = self.current_scene_name.as_ref()?;
        let (manifest_dir, manifest) = find_and_parse_manifest()?;
        let entry = manifest.scenes.iter().find(|e| &e.name == name)?;
        let path = manifest_dir.join(&entry.path);

        if entry.path.ends_with(".rscn") {
            if let Some(hdr_rel) = super::loader::read_env_path_from_rscn(&path) {
                let hdr_path = path
                    .parent()
                    .map(|d| d.join(&hdr_rel))
                    .unwrap_or_else(|| PathBuf::from(&hdr_rel));
                match std::fs::read(&hdr_path) {
                    Ok(bytes) => {
                        log::info!(
                            "scene '{}' environment (rscn): {}",
                            name,
                            hdr_path.display()
                        );
                        return Some(bytes);
                    }
                    Err(e) => log::warn!("env map {} not readable: {e}", hdr_path.display()),
                }
            }
            return None;
        }

        if entry.path.ends_with(".scene.json") {
            for (_, env) in world.query::<super::components::EnvironmentLighting>() {
                if let Some(env_map) = &env.env_map {
                    let hdr_path = path
                        .parent()
                        .map(|d| d.join(env_map))
                        .unwrap_or_else(|| PathBuf::from(env_map));
                    match std::fs::read(&hdr_path) {
                        Ok(bytes) => {
                            log::info!(
                                "scene '{}' environment (component): {}",
                                name,
                                hdr_path.display()
                            );
                            return Some(bytes);
                        }
                        Err(e) => {
                            log::warn!("env map {} not readable: {e}", hdr_path.display())
                            // 组件声明了 env_map 但文件缺失：视为无 IBL。
                        }
                    }
                }
                // 找到 EnvironmentLighting 组件但无 env_map（或文件缺失）→ 不构建 IBL。
                return None;
            }
        }
        None
    }

    /// 使用调用方提供的资源读取器解析环境贴图，不访问文件系统。
    pub fn current_scene_env_bytes_with_provider<P: EnvironmentProvider>(
        &self,
        view: SceneReadView<'_>,
        provider: &mut P,
    ) -> Option<Vec<u8>> {
        if let Some(bytes) = &self.environment_bytes {
            return Some(bytes.clone());
        }
        if let Some(path) = view.environment_asset_path() {
            let normalized = normalize_scene_asset_path(self.current_scene_path.as_deref(), &path);
            return provider.load_environment(&normalized);
        }
        None
    }

    // -----------------------------------------------------------------------
    // 内部：manifest 驱动加载
    // -----------------------------------------------------------------------

    /// 按 manifest 逐个解析场景并加载；`scene_name = None` 表示首个可加载场景。
    fn load_from_manifest(
        &mut self,
        rm: &mut ResourceManager,
        world: &mut World,
        registry: &ComponentRegistry,
        scene_name: Option<&str>,
    ) -> Option<String> {
        let manifest_path = find_manifest_path();
        let manifest_path = match manifest_path {
            Some(p) => p,
            None => {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unknown>".into());
                log::warn!("no assets/scenes.json found (cwd={cwd}); no scene will be loaded");
                return None;
            }
        };
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("failed to read scene manifest {:?}: {e}", manifest_path);
                return None;
            }
        };
        log::info!("scene manifest: {:?} ({} bytes)", manifest_path, text.len());
        let manifest: SceneManifest = match SceneManifest::parse(&text) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("scene manifest parse error: {e}");
                return None;
            }
        };
        log::info!(
            "scene manifest parsed: {} scene(s) listed",
            manifest.scenes.len()
        );
        self.load_entries(rm, world, registry, &manifest, scene_name)
    }

    fn load_entries(
        &mut self,
        rm: &mut ResourceManager,
        world: &mut World,
        registry: &ComponentRegistry,
        manifest: &SceneManifest,
        scene_name: Option<&str>,
    ) -> Option<String> {
        for entry in &manifest.scenes {
            if !entry.enabled {
                continue;
            }
            if let Some(name) = scene_name {
                if &entry.name != name {
                    continue;
                }
            }
            // 场景内容一律按 AssetId 从资源包加载：scenes.json 的 `path` 只是
            // 路径清单（.pak.meta.json）中的查找键，先解析为 AssetId，再由
            // ResourceManager 读取 RSCN 字节——不做磁盘文件回退。
            let Some(id) = rm.id_by_path(&entry.path) else {
                log::warn!(
                    "scene '{}' -> '{}' not registered in resource package; skipping",
                    entry.name,
                    entry.path
                );
                continue;
            };
            match load_scene_by_id(rm, world, id, registry) {
                Ok(inst) => {
                    // 只有新场景已经 spawn 成功才卸载旧场景：失败时保持当前
                    // 场景原样（见 `load_named_from_manifest` 的约定）。
                    let unloaded = self.adopt_scene(world, &entry.name, &entry.path, &inst);
                    log::info!(
                        "scene '{}' loaded: {} entities ({} roots), unloaded {unloaded} entity(ies) from the previous scene",
                        entry.name,
                        inst.all_entities.len(),
                        inst.root_entities.len()
                    );
                    return Some(entry.name.clone());
                }
                Err(e) => {
                    log::warn!("scene '{}' failed to load: {e}", entry.name);
                    continue;
                }
            }
        }
        log::warn!("no resolvable scene in manifest; no scene will be loaded");
        if let Some(name) = scene_name {
            log::warn!("requested scene '{name}' was not found in the manifest");
        }
        None
    }

    /// 接管刚刚 spawn 成功的新场景：卸载旧场景实体、清空环境贴图缓存，
    /// 并把「当前场景」切到新场景。
    ///
    /// 返回被卸载的旧场景实体数。不属于任何场景的实体（例如
    /// `Engine::runtime_initialize` 生成的回退相机）保留——它们由引擎而不是
    /// 场景拥有。
    fn adopt_scene(
        &mut self,
        world: &mut World,
        name: &str,
        path: &str,
        inst: &SceneInstance,
    ) -> usize {
        let unloaded = unload_scene_members(world, Some(inst.scene_id));
        self.current_scene_name = Some(name.to_owned());
        self.current_scene_path = Some(path.to_owned());
        // IBL 按场景解析：切换后不能继续复用上一个场景注入的字节，
        // 否则旧 HDR 会盖住新场景的天空盒。
        self.environment_bytes = None;
        self.bump_generation();
        unloaded
    }
}

/// 加载 environment 映射表字节 from the **第一个** scene in `scenes.json`.
///
/// Legacy 手动入口——已被 [`SceneManager::current_scene_env_bytes`] 取代
/// （后者只解析**当前**场景，避免 UI 开屏也加载 3D 环境贴图）。
#[cfg(feature = "legacy-disk-scenes")]
#[deprecated(note = "load environment data through EnvironmentProvider")]
pub fn load_env_bytes_from_manifest() -> Option<Vec<u8>> {
    let (manifest_dir, manifest) = find_and_parse_manifest()?;
    for entry in &manifest.scenes {
        let path = manifest_dir.join(&entry.path);
        if !path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("rscn"))
            .unwrap_or(false)
            || !path.exists()
        {
            continue;
        }
        if let Some(hdr_rel) = super::loader::read_env_path_from_rscn(&path) {
            let hdr_path = path
                .parent()
                .map(|d| d.join(&hdr_rel))
                .unwrap_or_else(|| PathBuf::from(&hdr_rel));
            match std::fs::read(&hdr_path) {
                Ok(bytes) => {
                    log::info!("loaded environment map from scene: {}", hdr_path.display());
                    return Some(bytes);
                }
                Err(e) => log::warn!("env map HDR {} not readable: {e}", hdr_path.display()),
            }
        }
    }
    log::info!("no environment map in scene manifest; using procedural fallback");
    None
}

// ---------------------------------------------------------------------------
// Manifest helpers（由旧 engine.rs 迁入）——场景系统的内部实现
// ---------------------------------------------------------------------------

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SceneManifestEntry {
    pub name: String,
    pub path: String,
    /// Build Settings 的勾选状态；缺省视为启用（兼容旧 manifest）。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SceneManifest {
    pub scenes: Vec<SceneManifestEntry>,
}

impl SceneManifest {
    /// 解析清单文本：优先 `scenes.json`（`{"scenes":[{name,path,enabled}]}`），
    /// 失败时回退到旧项目的 `scenes.toml`（`[[scenes]]` 数组表）并提示迁移。
    ///
    /// 回退只在 JSON 解析失败时发生，所以新格式解析错误不会被旧解析器
    /// 「猜」成一份空清单——两种格式都失败才返回错误。
    pub fn parse(text: &str) -> Result<Self, String> {
        match serde_json::from_str::<Self>(text) {
            Ok(manifest) => Ok(manifest),
            Err(json_error) => match parse_legacy_toml(text) {
                Ok(manifest) => {
                    log::warn!(
                        "scene manifest is in the legacy TOML format; run \
                         `prism-build-pipeline build-scenes` to migrate it to scenes.json"
                    );
                    Ok(manifest)
                }
                Err(toml_error) => Err(format!(
                    "not scenes.json ({json_error}) and not legacy scenes.toml ({toml_error})"
                )),
            },
        }
    }
}

/// 旧格式 `scenes.toml` 的最小读取器。
///
/// 只认清单用到的 TOML 子集：`[[scenes]]` 数组表 + `name` / `path` / `enabled`
/// 三个键 + 整行注释。它不是通用 TOML 解析器——存在的唯一理由是让旧项目
/// 在原文件上继续可加载，直到 `build-scenes` 把它们迁移成 `scenes.json`。
fn parse_legacy_toml(text: &str) -> Result<SceneManifest, String> {
    let mut scenes: Vec<SceneManifestEntry> = Vec::new();
    let mut current: Option<SceneManifestEntry> = None;
    let mut saw_table = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[scenes]]" {
            if let Some(entry) = current.take() {
                scenes.push(entry);
            }
            current = Some(SceneManifestEntry {
                name: String::new(),
                path: String::new(),
                enabled: true,
            });
            saw_table = true;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // 别的表（理论上不该出现）：结束当前条目，后续键忽略。
            if let Some(entry) = current.take() {
                scenes.push(entry);
            }
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = crate::config::ini::parse_value(value);
        match key.trim() {
            "name" => entry.name = value,
            "path" => entry.path = value,
            "enabled" => {
                entry.enabled = match value.trim().to_ascii_lowercase().as_str() {
                    "false" | "0" | "no" | "off" => false,
                    _ => true,
                }
            }
            _ => {}
        }
    }
    if let Some(entry) = current.take() {
        scenes.push(entry);
    }

    if !saw_table {
        return Err("no [[scenes]] table".into());
    }
    Ok(SceneManifest { scenes })
}

/// 不依赖当前工作目录地定位场景清单：
/// 按 [`crate::asset_paths::search_roots`] 的顺序（`PRISMREV_ASSET_ROOT`、
/// 当前目录及其父目录、可执行文件目录及其父目录），先找
/// `assets/scenes.json`，再回退旧项目的 `assets/scenes.toml`。
///
/// 这样无论从仓库根还是 `projects/prisma/` 子目录启动，
/// 都能正确找到资源清单——之前从用户项目目录启动时，导致
/// 清单找不到、回退到空场景而黑屏。
fn find_manifest_path() -> Option<PathBuf> {
    const NAMES: [&str; 2] = ["scenes.json", "scenes.toml"];
    const DIRS: [&str; 2] = ["assets", "crates/prism-engine/assets"];
    for base in crate::asset_paths::search_roots() {
        for name in NAMES {
            for dir in DIRS {
                let candidate = base.join(dir).join(name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(feature = "legacy-disk-scenes")]
fn find_and_parse_manifest() -> Option<(PathBuf, SceneManifest)> {
    let manifest_path = find_manifest_path()?;
    let manifest_dir = manifest_path.parent()?.to_path_buf();
    let text = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest = SceneManifest::parse(&text).ok()?;
    log::info!(
        "scene manifest: {:?} ({} entries)",
        manifest_path,
        manifest.scenes.len()
    );
    Some((manifest_dir, manifest))
}

fn load_scene_by_id(
    rm: &mut ResourceManager,
    world: &mut World,
    id: AssetId,
    registry: &ComponentRegistry,
) -> Result<SceneInstance, anyhow::Error> {
    use anyhow::Context;
    let handle = rm
        .load_with_deps::<SceneAsset>(id)
        .with_context(|| format!("load scene {id}"))?;
    let asset = rm
        .get::<SceneAsset>(handle)
        .with_context(|| format!("get scene {id}"))?;
    let mut loader = SceneLoader::new();
    loader
        .load_and_spawn(world, SceneSource::RawCooked(asset.bytes.clone()), registry)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// 卸载所有不属于 `keep` 的场景实体，返回被移除的实体数。
///
/// 归属由 [`SceneMember`](super::components::SceneMember) 判定：没有该组件的
/// 实体（引擎自有实体、用户运行时 spawn 的对象）一律保留。
/// 卸载所有属于「非 `keep` 场景」的实体，返回卸载数量。
///
/// `keep = None` 表示卸载**全部**场景实体（`Engine::unload_scene`）。
/// 以下两类实体永远保留：
/// - 没有 `SceneMember` 的实体（引擎自有：回退相机、宿主注入的对象）；
/// - 带 [`super::components::Persistent`] 的实体及其整棵子树（Unity 的
///   `DontDestroyOnLoad`）。
fn unload_scene_members(world: &mut World, keep: Option<super::components::SceneAssetId>) -> usize {
    let candidates: Vec<Entity> = world
        .query::<super::components::SceneMember>()
        .filter(|(_, member)| Some(member.0) != keep)
        .map(|(entity, _)| entity)
        .collect();
    if candidates.is_empty() {
        return 0;
    }

    let keep_set = persistent_subtrees(world);
    let mut unloaded = 0;
    for entity in candidates {
        if keep_set.contains(&entity) {
            continue;
        }
        if world.is_alive(entity) {
            world.despawn(entity);
            unloaded += 1;
        }
    }
    warn_dangling_parents(world);
    unloaded
}

/// 带 `Persistent` 标记的实体及其全部后代（保留清单）。
fn persistent_subtrees(world: &World) -> std::collections::HashSet<Entity> {
    let mut keep = std::collections::HashSet::new();
    let roots: Vec<Entity> = world
        .query::<super::components::Persistent>()
        .map(|(entity, _)| entity)
        .collect();
    if roots.is_empty() {
        return keep;
    }
    let mut stack = roots;
    while let Some(entity) = stack.pop() {
        if !keep.insert(entity) {
            continue;
        }
        if let Some(children) = world.get::<super::components::Child>(entity) {
            stack.extend(children.0.iter().copied());
        }
    }
    keep
}

/// 卸载后检查断掉的父子关系并提示一次。
///
/// 典型情况：父级属于旧场景被删掉，而子级带 `Persistent` 留了下来 ——
/// 层级上它已经没有父节点了，继续按「有父级」处理会算错世界矩阵。
/// 这里只警告，不替用户改结构。
fn warn_dangling_parents(world: &World) {
    let dangling: Vec<Entity> = world
        .query::<super::components::Parent>()
        .filter(|(entity, parent)| world.is_alive(*entity) && !world.is_alive(parent.0))
        .map(|(entity, _)| entity)
        .collect();
    if dangling.is_empty() {
        return;
    }
    log::warn!(
        "{} entity(ies) kept across the scene switch have a dead parent; \
         reparent them or clear the hierarchy (dangling: {:?})",
        dangling.len(),
        dangling
    );
}

#[cfg(test)]
#[path = "scene_system_tests.rs"]
mod tests;
