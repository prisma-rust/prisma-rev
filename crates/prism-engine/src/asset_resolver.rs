//! GPU 资源解析器——从 `.pak` 资源包按需加载+缓存网格/材质/纹理资源。
//!
//! 拥有 [`ResourceManager`]（运行时资源数据库）与三个类型化缓存（确保同一网格/材质/纹理
//! 不会重复上传），并提供 [`GpuAssetResolver::prepare_requests`] 方法查询 ECS 世界中
//! 待处理的 [`MeshRenderer`] 实体，生成 GPU 上传请求。
//!
//! ## 异步模型（与渲染线程解耦）
//!
//! 资源解析被拆成两段，因为 `&mut World`（主线程）与 `&mut GraphRenderer`（渲染线程）
//! 不能同处一线程：
//! - **CPU 段（主线程）**：[`prepare_requests`] 查询待解析实体、加载 `.pak`、解交织顶点/
//!   纹理像素，产出纯数据的 [`AssetResolveRequest`]。不触碰渲染器。
//! - **GPU 段（渲染线程）**：`GraphRenderer::apply_asset_requests` 执行上传并回传句柄。
//! - 主线程再消费结果，把句柄写回 `MeshRef`/`MaterialRef`。
//!
//! 这样启动期渲染器在渲染线程异步构建时，主线程仍可处理窗口事件（关闭/移动/缩放）。

use std::collections::{HashMap, HashSet};

use prism_asset::core::AssetId;
use prism_asset::formats::{
    RTEX_FORMAT_BC1, RTEX_FORMAT_BC3, RTEX_FORMAT_BC5, RTEX_FORMAT_BC6H, RTEX_FORMAT_RGBA8,
};
use prism_asset::runtime::{MaterialAsset, MeshAsset, ResourceManager, TextureAsset};
use prism_ecs::Entity;
use prism_ecs::World;
use prism_render::asset_bridge::{AssetRelease, AssetResolveRequest, MaterialResolveData};
use prism_render::managers::{MeshUploadInput, TextureFormat, TextureUploadInput};

use crate::scene::components::{
    MaterialRef, MeshRef, MeshRenderer, SpriteRef, SpriteRenderer, Tilemap,
};
use crate::scene::runtime_components::{TilemapRef, UiImageRef};
use crate::scene::systems::render::MeshBounds;

/// 把 RTEX 头的 `format` byte 与槽位颜色空间映射成渲染层 [`TextureFormat`]。
///
/// RTEX 不编码颜色空间：同一个 RGBA8 payload 可能是 sRGB（albedo / emissive）
/// 也可能是线性（normal / MR / AO），由调用方按槽位传入 `srgb`。压缩格式的
/// 通道语义固定（BC5 是线性双通道法线，BC6H 是 HDR 线性）。未支持的格式返回
/// `None`，调用方回退洋红。
fn texture_format_for(rtex_format: u8, srgb: bool) -> Option<TextureFormat> {
    match rtex_format {
        RTEX_FORMAT_RGBA8 if srgb => Some(TextureFormat::Rgba8Srgb),
        RTEX_FORMAT_RGBA8 => Some(TextureFormat::Rgba8),
        // 块压缩格式的颜色空间同样由槽位决定（albedo/emissive = sRGB，
        // normal/MR/AO = 线性），与 RGBA8 一致（#519）。
        RTEX_FORMAT_BC1 if srgb => Some(TextureFormat::Bc1Srgb),
        RTEX_FORMAT_BC1 => Some(TextureFormat::Bc1Unorm),
        RTEX_FORMAT_BC3 if srgb => Some(TextureFormat::Bc3Srgb),
        RTEX_FORMAT_BC3 => Some(TextureFormat::Bc3Unorm),
        RTEX_FORMAT_BC5 => Some(TextureFormat::Bc5Unorm),
        RTEX_FORMAT_BC6H => Some(TextureFormat::Bc6HUfloat),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// GpuAssetResolver
// ---------------------------------------------------------------------------

/// 资源槽位，用作「失败缓存」的键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AssetSlot {
    Mesh,
    Material,
    Sprite,
    /// 瓦片地图的 tileset（#529）。
    Tileset,
    /// UI `Style::image` 背景图（#591）。
    UiImage,
}

/// 一个实体本帧要解析的资源（只包含仍然缺失的那几半）。
struct PendingRequest {
    entity: Entity,
    mesh_path: Option<String>,
    material_path: Option<String>,
    /// 精灵贴图 / 瓦片地图的 tileset / UI 背景图：三者都只需要一张贴图，
    /// 一个实体同一帧只会投递其中一个。
    sprite_path: Option<String>,
    tileset: bool,
    /// `sprite_path` 是 UI `Style::image`（#591）。
    ui_image: bool,
}

/// On-demand 资源 loader + GPU uploader + cache.
///
/// 每帧调用 [`prepare_requests`] 收集待处理实体并产出上传请求（CPU 段）；
/// GPU 上传由渲染线程异步完成。
pub struct GpuAssetResolver {
    pub resource_manager: ResourceManager,
    /// 已投递上传请求的实体（去重：避免每帧重复 enqueue，直到结果写回 World）。
    enqueued: HashSet<Entity>,
    /// 已尝试且失败的槽位：`(实体, 槽位) -> 失败时的路径`。
    ///
    /// 路径不在清单里、解码失败这类错误不会「再试一次就好」，每帧重试只会反复
    /// 读 `.pak` 并刷日志。缓存以路径为值：路径被改写（换素材、热重载换包）时
    /// 键不再匹配，会自动重新尝试。
    failed: HashMap<(Entity, AssetSlot), String>,
    /// 最近一次解析的精灵路径。项目只改 `sprite_path`，这里发现不一致就重新解析，
    /// 不再要求调用方手动把 `SpriteRef::generation` 清零。
    sprite_paths: HashMap<Entity, String>,
    /// 最近一次解析的 tileset 路径（#529），语义同 [`Self::sprite_paths`]。
    tileset_paths: HashMap<Entity, String>,
    /// 最近一次解析的 UI 背景图路径（`Style::image`，#591）。
    ui_image_paths: HashMap<Entity, String>,
    /// 已经交给渲染器的工程字体路径（#583）。
    ///
    /// 字体是整份文件注册进 UI 图集，不像贴图那样每实体一份；同一个路径只读一次。
    fonts_loaded: HashSet<String>,
    /// 读过但失败的字体路径：不再每帧重试（与 [`Self::failed`] 同理）。
    fonts_failed: HashSet<String>,
    /// 已经写到组件里的 GPU 槽位 / 句柄，按 `(实体, 槽位)` 记账（#495）。
    ///
    /// 组件被销毁、路径被改写、`generation` 归零（热重载）时据此回收显存；
    /// 以前这些资源只会被新上传覆盖，旧图像 / 网格永远留在 GPU 上。
    resolved: HashMap<(Entity, AssetSlot), AssetRelease>,
    /// 本实体的贴图请求属于哪个槽位（`Sprite` / `Tileset` / `UiImage`）。
    ///
    /// 结果回传时只有一个 `texture_slot`，必须知道它对应哪个组件才能记账。
    pending_texture_slot: HashMap<Entity, AssetSlot>,
    /// 待宿主转发给渲染器的释放请求。
    releases: Vec<AssetRelease>,
    /// 已投递、但句柄还没回写的网格局部包围盒（`entity → AABB`）。
    ///
    /// CPU 段已经在手里有顶点位置，所以包围盒在上传请求发出前就算好；句柄回来
    /// （[`Self::apply_results`]）之后连同句柄一起写进 [`MeshBounds`]，渲染提取
    /// 就能按主相机剔除（#491）。
    pending_bounds: HashMap<Entity, prism_render::culling::Aabb>,
}

/// 引用断裂只报一次（每个路径），并且是 `error` 而不是 `warn`。
///
/// 路径不在资源包里意味着文档里的引用已经断了（文件被删 / 改名 / 拼错）。
/// 后果是物体整个不出现或贴图变白，不该被日志级别悄悄降级；但同一个断引用每个
/// 实体都要解析一次，每帧都刷一行会把 Console 淹掉，所以按路径去重（#467）。
fn report_missing_reference(path: &str, kind: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static REPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut reported) = reported.lock() else {
        return;
    };
    if !reported.insert(path.to_string()) {
        return;
    }
    log::error!(
        "broken asset reference: {kind} '{path}' is not in the resource package          (deleted, renamed, or misspelt?)"
    );
}

impl GpuAssetResolver {
    pub fn new() -> Self {
        Self {
            resource_manager: ResourceManager::new(),
            enqueued: HashSet::new(),
            failed: HashMap::new(),
            sprite_paths: HashMap::new(),
            tileset_paths: HashMap::new(),
            ui_image_paths: HashMap::new(),
            fonts_loaded: HashSet::new(),
            fonts_failed: HashSet::new(),
            resolved: HashMap::new(),
            pending_texture_slot: HashMap::new(),
            releases: Vec::new(),
            pending_bounds: HashMap::new(),
        }
    }

    /// 收集场景里用到的工程自带字体并读出字节（#583）。
    ///
    /// 返回**本次新读出来**的 `(资产路径, 字体字节)`；调用方把它交给
    /// `GraphRenderer::register_ui_font`。已经交过的路径不会重复返回，读失败
    /// 的路径也不再每帧重试（与网格 / 贴图的失败缓存同一策略）。
    ///
    /// 字体走 `.pak` 的路径清单（`AssetType::Font`），所以打包产物和编辑器里
    /// 看到的是同一份文件。
    pub fn prepare_ui_fonts(&mut self, world: &World) -> Vec<(String, Vec<u8>)> {
        let mut paths: Vec<String> = Vec::new();
        for (_, text) in world.query::<crate::ui::components::Text>() {
            let path = text.font.trim();
            if path.is_empty()
                || self.fonts_loaded.contains(path)
                || self.fonts_failed.contains(path)
                || paths.iter().any(|known| known == path)
            {
                continue;
            }
            paths.push(path.to_string());
        }

        let mut out = Vec::new();
        for path in paths {
            let Some(id) = self.resource_manager.id_by_path(&path) else {
                report_missing_reference(&path, "font");
                self.fonts_failed.insert(path);
                continue;
            };
            match self.resource_manager.load_with_deps_raw(id) {
                Ok(bytes) if !bytes.is_empty() => {
                    log::info!("ui fonts: loaded {} ({} bytes)", path, bytes.len());
                    self.fonts_loaded.insert(path.clone());
                    out.push((path, bytes));
                }
                Ok(_) => {
                    log::warn!("ui fonts: '{path}' is empty — ignoring it");
                    self.fonts_failed.insert(path);
                }
                Err(error) => {
                    log::warn!("ui fonts: load '{path}' failed: {error}");
                    self.fonts_failed.insert(path);
                }
            }
        }
        out
    }

    /// 记下「这个实体的这个槽位现在用的是哪个 GPU 资源」。
    ///
    /// 覆盖旧值时如果资源换了（重新上传到新槽位），旧的那份立即排进释放队列
    /// —— 组件在同一帧被改写，不会出现「还在采样已释放槽位」的中间帧。
    fn record_resolved(&mut self, entity: Entity, slot: AssetSlot, value: AssetRelease) {
        if value.is_empty() {
            return;
        }
        if let Some(previous) = self.resolved.insert((entity, slot), value) {
            if previous != value {
                self.releases.push(previous);
            }
        }
    }

    /// 这一帧里该实体的这个槽位是否已经不再需要 GPU 资源。
    ///
    /// 判定标准只有两条，都是「确定不会再画」：承载它的组件没了，或
    /// `generation == 0`（`invalidate_*` 刚把它标脏，马上会重新上传）。
    fn slot_is_stale(&self, world: &World, entity: Entity, slot: AssetSlot) -> bool {
        use crate::scene::runtime_components::{TilemapRef, UiImageRef};
        match slot {
            AssetSlot::Mesh => {
                world.get::<MeshRenderer>(entity).is_none()
                    || world
                        .get::<MeshRef>(entity)
                        .map(|r| r.generation == 0)
                        .unwrap_or(true)
            }
            AssetSlot::Material => {
                world.get::<MeshRenderer>(entity).is_none()
                    || world
                        .get::<MaterialRef>(entity)
                        .map(|r| r.generation == 0)
                        .unwrap_or(true)
            }
            AssetSlot::Sprite => {
                world.get::<SpriteRenderer>(entity).is_none()
                    || world
                        .get::<SpriteRef>(entity)
                        .map(|r| r.generation == 0)
                        .unwrap_or(true)
            }
            AssetSlot::Tileset => {
                world.get::<Tilemap>(entity).is_none()
                    || world
                        .get::<TilemapRef>(entity)
                        .map(|r| r.generation == 0)
                        .unwrap_or(true)
            }
            AssetSlot::UiImage => {
                world
                    .get::<crate::ui::components::Style>(entity)
                    .map(|style| style.image.is_empty())
                    .unwrap_or(true)
                    || world
                        .get::<UiImageRef>(entity)
                        .map(|r| r.generation == 0)
                        .unwrap_or(true)
            }
        }
    }

    /// 把已经失效的 GPU 资源排进释放队列（由 [`Self::take_releases`] 交给宿主）。
    fn release_stale_assets(&mut self, world: &World) {
        let stale: Vec<(Entity, AssetSlot)> = self
            .resolved
            .keys()
            .filter(|(entity, slot)| self.slot_is_stale(world, *entity, *slot))
            .copied()
            .collect();
        for key in stale {
            if let Some(release) = self.resolved.remove(&key) {
                self.releases.push(release);
            }
        }
        // 实体被销毁后，还没回传的「贴图属于哪个槽位」的登记也没用了。
        self.pending_texture_slot.retain(|entity, _| {
            self.resolved.keys().any(|(e, _)| e == entity)
                || world.get::<SpriteRenderer>(*entity).is_some()
                || world.get::<Tilemap>(*entity).is_some()
                || world.get::<crate::ui::components::Style>(*entity).is_some()
        });
    }

    /// 换场 / 卸载后立刻丢弃已经不存在的实体的缓存（#482）。
    ///
    /// 等价于 `prepare_requests` 开头的清理，但可以显式调用，让换场当帧就把
    /// 旧世界的槽位交回渲染器、并清掉 `enqueued` / 失败缓存里对不上的键。
    pub fn forget_missing_entities(&mut self, world: &World) {
        self.release_stale_assets(world);
        self.enqueued.retain(|entity| {
            world.get::<MeshRenderer>(*entity).is_some()
                || world.get::<SpriteRenderer>(*entity).is_some()
                || world.get::<Tilemap>(*entity).is_some()
                || world
                    .get::<crate::ui::components::Style>(*entity)
                    .is_some_and(|style| !style.image.is_empty())
        });
        self.failed.retain(|(entity, _), _| {
            world.get::<MeshRenderer>(*entity).is_some()
                || world.get::<SpriteRenderer>(*entity).is_some()
                || world.get::<Tilemap>(*entity).is_some()
                || world
                    .get::<crate::ui::components::Style>(*entity)
                    .is_some_and(|style| !style.image.is_empty())
        });
    }

    /// 取走待释放的 GPU 资源；宿主把它们转给渲染器（`GraphRenderer::release_assets`）。
    ///
    /// 在 `prepare_requests` 之后调用：失效的槽位在这一帧内就被回收，重新上传
    /// 的请求同时入队，所以热重载不会每次都多占一份显存。
    pub fn take_releases(&mut self) -> Vec<AssetRelease> {
        std::mem::take(&mut self.releases)
    }

    /// 该实体的这个槽位是否已经因同一个路径失败过。
    fn failed_before(&self, entity: Entity, slot: AssetSlot, path: &str) -> bool {
        self.failed
            .get(&(entity, slot))
            .is_some_and(|failed_path| failed_path == path)
    }

    /// 记住一个加载失败：路径不在清单 / 解码失败都不会因为下一帧再试而变好，
    /// 缓存后同一个路径只尝试一次（路径被改写时自动重新尝试）。
    fn record_failure(&mut self, entity: Entity, slot: AssetSlot, path: &str) {
        log::warn!(
            "asset_resolver: {slot:?} '{path}' failed to load; \
             not retrying until the path changes"
        );
        self.failed.insert((entity, slot), path.to_string());
    }

    // -----------------------------------------------------------------------
    // 包 loading
    // -----------------------------------------------------------------------

    /// Attempt to 加载 the `.pak` 资源 包 and its path manifest.
    ///
    /// Both files are optional — when absent (no CLI 构建 run yet) the
    /// engine continues with only procedural geometry.
    ///
    /// Paths are resolved through [`crate::asset_paths`] instead of the process
    /// cwd, so launching the executable from any directory still finds the
    /// package that sits next to it.
    pub fn load_resource_package(&mut self) {
        let Some(asset_dir) = crate::asset_paths::asset_dir() else {
            log::warn!("resource package unavailable: no `assets/` directory found");
            return;
        };
        // #530：挂载 `assets/` 下**所有** `.pak`，而不是写死 `scenes.pak`。
        // 构建可以按内容分包（场景 / 模型贴图 / 后续的流式包），运行时不需要
        // 知道分了多少个包。
        let paks = crate::asset_paths::pak_files_in(&asset_dir);
        if paks.is_empty() {
            log::warn!(
                "resource package unavailable: no `.pak` in {}; \
                 scene assets cannot load (run `build-scenes`)",
                asset_dir.display()
            );
            return;
        }

        let mut loaded = 0usize;
        for pak_path in paks {
            if let Err(e) = self.resource_manager.load_package(&pak_path) {
                log::warn!(
                    "resource package {} could not be loaded: {e}; \
                     assets from it will be missing",
                    pak_path.display()
                );
                continue;
            }
            loaded += 1;

            // 每个包旁边有 `<name>.pak.meta.json`（路径清单）时一并加载。
            let manifest_path = pak_path.with_extension("pak.meta.json");
            if manifest_path.is_file() {
                if let Err(e) = self.resource_manager.load_path_manifest(&manifest_path) {
                    log::warn!(
                        "failed to load path manifest {}: {e} \
                         (asset resolution by path won't work for this package)",
                        manifest_path.display()
                    );
                }
            } else {
                log::warn!(
                    "resource package {} has no path manifest ({})",
                    pak_path.display(),
                    manifest_path.display()
                );
            }
        }

        log::info!(
            "resource packages loaded from {}: {loaded} package(s), {} assets registered",
            asset_dir.display(),
            self.resource_manager.asset_count(),
        );
    }

    /// 从内存资源加载 `.pak` 及其路径 manifest。
    pub fn load_resource_package_bytes(
        &mut self,
        pak_bytes: &[u8],
        path_manifest_json: Option<&str>,
    ) -> anyhow::Result<()> {
        self.resource_manager.load_package_bytes(pak_bytes)?;
        if let Some(manifest) = path_manifest_json {
            self.resource_manager
                .load_path_manifest_from_str(manifest)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 主线程：CPU 段——收集待解析实体并产出上传请求
    // -----------------------------------------------------------------------

    /// 查询 ECS 世界中待处理的 [`MeshRenderer`] 实体，加载 `.pak` + 解交织，
    /// 产出与 `World` 完全解耦的 [`AssetResolveRequest`]（纯数据，Send）。
    ///
    /// 不触碰 `GraphRenderer`——GPU 上传由渲染线程的 `apply_asset_requests` 完成。
    /// 返回的请求由调用方通过 [`prism_app::RenderShared`] 通道交给渲染线程。
    ///
    /// 返回数量即本帧新投递的实体数。已 enqueue 但尚未写回结果的实体会被去重跳过。
    ///
    /// 每个实体只携带「仍然缺失」的那几半：网格已解析、材质还缺时不会再加载/上传
    /// 网格（旧实现会把整份请求重发一遍，导致每帧注册新的 GPU 资源）。加载失败的
    /// 槽位会记入失败缓存（[`GpuAssetResolver::failed`]），同一坏路径只尝试一次，
    /// 直到路径被改写。
    pub fn prepare_requests(&mut self, world: &World) -> Vec<(Entity, AssetResolveRequest)> {
        // 先回收这一帧已经失效的 GPU 资源（#495）：实体没了、组件被删、或
        // `generation` 归零（热重载）时把槽位交还给渲染器，再重新上传。
        self.release_stale_assets(world);
        // 运行时生成的网格（地形，#517）：路径以 `__proc/` 开头，直接从世界里的
        // 表取，不经过 `.pak`。
        let procedural = world.get_resource::<crate::scene::systems::terrain::ProceduralMeshes>();
        // 收集待处理实体（首个），避免同时借用世界 & self。
        let pending: Vec<PendingRequest> = {
            // 失败缓存只对仍然存在的实体有意义；实体被删除后顺手回收。
            self.sprite_paths
                .retain(|entity, _| world.get::<SpriteRenderer>(*entity).is_some());
            // 实体在上传途中被删除时，包围盒也不该留在表里。
            self.pending_bounds
                .retain(|entity, _| world.get::<MeshRef>(*entity).is_some());
            self.failed.retain(|(entity, _), _| {
                world.get::<MeshRenderer>(*entity).is_some()
                    || world.get::<SpriteRenderer>(*entity).is_some()
                    || world.get::<Tilemap>(*entity).is_some()
                    || world
                        .get::<crate::ui::components::Style>(*entity)
                        .is_some_and(|style| !style.image.is_empty())
            });
            self.tileset_paths
                .retain(|entity, _| world.get::<Tilemap>(*entity).is_some());
            self.ui_image_paths.retain(|entity, _| {
                world
                    .get::<crate::ui::components::Style>(*entity)
                    .is_some_and(|style| !style.image.is_empty())
            });

            let mut out = Vec::new();
            for (entity, mr) in world.query::<MeshRenderer>() {
                // 只有「有路径」且「句柄还没解析」才算待处理：
                // - 路径为空 = 作者没给资源，没有东西可加载（例如无材质的网格，
                //   由渲染器回落到默认材质槽）；
                // - 运行时组件缺失 = 结果无处写回，投递出去只会每帧重复 enqueue；
                // - 失败缓存命中 = 同一个坏路径不再重试。
                let wants_mesh = !mr.mesh_path.is_empty()
                    && world
                        .get::<MeshRef>(entity)
                        .is_some_and(|r| r.generation == 0)
                    && !self.failed_before(entity, AssetSlot::Mesh, &mr.mesh_path);
                let wants_material = !mr.material_path.is_empty()
                    && world
                        .get::<MaterialRef>(entity)
                        .is_some_and(|r| r.generation == 0)
                    && !self.failed_before(entity, AssetSlot::Material, &mr.material_path);
                if (wants_mesh || wants_material) && !self.enqueued.contains(&entity) {
                    out.push(PendingRequest {
                        entity,
                        mesh_path: wants_mesh.then(|| mr.mesh_path.clone()),
                        material_path: wants_material.then(|| mr.material_path.clone()),
                        sprite_path: None,
                        tileset: false,
                        ui_image: false,
                    });
                    self.enqueued.insert(entity);
                }
            }
            // 2D 精灵：只有贴图一条资源，`SpriteRef.generation == 0` 表示待解析。
            for (entity, sprite) in world.query::<SpriteRenderer>() {
                if !sprite.has_sprite() {
                    continue;
                }
                let unresolved = world
                    .get::<SpriteRef>(entity)
                    .map(|r| r.generation == 0)
                    .unwrap_or(true);
                // 没有 `SpriteRef`（运行期新生成的精灵）或路径被改写都算待解析。
                let path_changed = self
                    .sprite_paths
                    .get(&entity)
                    .is_some_and(|last| last != &sprite.sprite_path);
                if (unresolved || path_changed)
                    && !self.failed_before(entity, AssetSlot::Sprite, &sprite.sprite_path)
                    && !self.enqueued.contains(&entity)
                {
                    out.push(PendingRequest {
                        entity,
                        mesh_path: None,
                        material_path: None,
                        sprite_path: Some(sprite.sprite_path.clone()),
                        tileset: false,
                        ui_image: false,
                    });
                    self.enqueued.insert(entity);
                    self.sprite_paths.insert(entity, sprite.sprite_path.clone());
                }
            }
            // 瓦片地图：同样只有 tileset 一张贴图（#529）。
            for (entity, tilemap) in world.query::<Tilemap>() {
                if tilemap.tileset_path.is_empty() {
                    continue;
                }
                let unresolved = world
                    .get::<TilemapRef>(entity)
                    .map(|r| r.generation == 0)
                    .unwrap_or(true);
                let path_changed = self
                    .tileset_paths
                    .get(&entity)
                    .is_some_and(|last| last != &tilemap.tileset_path);
                if (unresolved || path_changed)
                    && !self.failed_before(entity, AssetSlot::Tileset, &tilemap.tileset_path)
                    && !self.enqueued.contains(&entity)
                {
                    out.push(PendingRequest {
                        entity,
                        mesh_path: None,
                        material_path: None,
                        sprite_path: Some(tilemap.tileset_path.clone()),
                        tileset: true,
                        ui_image: false,
                    });
                    self.enqueued.insert(entity);
                    self.tileset_paths
                        .insert(entity, tilemap.tileset_path.clone());
                }
            }
            // UI 背景图（#591）：和精灵一样只需要一张贴图，结果写进 `UiImageRef`。
            for (entity, style) in world.query::<crate::ui::components::Style>() {
                if style.image.is_empty() {
                    continue;
                }
                let unresolved = world
                    .get::<UiImageRef>(entity)
                    .map(|r| r.generation == 0)
                    .unwrap_or(true);
                let path_changed = self
                    .ui_image_paths
                    .get(&entity)
                    .is_some_and(|last| last != &style.image);
                if (unresolved || path_changed)
                    && !self.failed_before(entity, AssetSlot::UiImage, &style.image)
                    && !self.enqueued.contains(&entity)
                {
                    out.push(PendingRequest {
                        entity,
                        mesh_path: None,
                        material_path: None,
                        sprite_path: Some(style.image.clone()),
                        tileset: false,
                        ui_image: true,
                    });
                    self.enqueued.insert(entity);
                    self.ui_image_paths.insert(entity, style.image.clone());
                }
            }
            out
        };

        if pending.is_empty() {
            return Vec::new();
        }

        let mut reqs = Vec::with_capacity(pending.len());
        for item in pending {
            let mesh = match item.mesh_path.as_deref() {
                Some(path) => {
                    let loaded = procedural
                        .and_then(|meshes| meshes.get(path))
                        .or_else(|| self.load_mesh_inputs(path));
                    if loaded.is_none() {
                        self.record_failure(item.entity, AssetSlot::Mesh, path);
                    }
                    loaded
                }
                None => None,
            };
            let material = match item.material_path.as_deref() {
                Some(path) => {
                    let loaded = self.load_material_data(path);
                    if loaded.is_none() {
                        self.record_failure(item.entity, AssetSlot::Material, path);
                    }
                    loaded
                }
                None => None,
            };
            let texture = match item.sprite_path.as_deref() {
                Some(path) => {
                    let loaded = self.load_texture_by_path(path);
                    if loaded.is_none() {
                        let slot = if item.tileset {
                            AssetSlot::Tileset
                        } else if item.ui_image {
                            AssetSlot::UiImage
                        } else {
                            AssetSlot::Sprite
                        };
                        self.record_failure(item.entity, slot, path);
                    }
                    loaded
                }
                None => None,
            };
            if texture.is_some() {
                let slot = if item.tileset {
                    AssetSlot::Tileset
                } else if item.ui_image {
                    AssetSlot::UiImage
                } else {
                    AssetSlot::Sprite
                };
                self.pending_texture_slot.insert(item.entity, slot);
            }
            // 包围盒在请求发出前算好：GPU 上传后句柄才回来，而 CPU 段此刻
            // 已经解出顶点位置了（#491）。
            if let Some(bounds) = mesh.as_ref().and_then(MeshUploadInput::bounds) {
                self.pending_bounds.insert(item.entity, bounds);
            }
            reqs.push((
                item.entity,
                AssetResolveRequest {
                    mesh,
                    material,
                    texture,
                },
            ));
        }

        log::info!(
            "prepare_requests: staged {} entit(y/ies) for async GPU upload",
            reqs.len()
        );
        reqs
    }

    /// 结果写回后调用：解除该实体的 enqueue 去重标记，并写入 GPU 句柄。
    ///
    /// 必须在主线程调用（持有 `&mut World`）。
    pub fn apply_results(
        &mut self,
        world: &mut World,
        results: &[(Entity, prism_render::asset_bridge::AssetResolveResult)],
    ) {
        for (entity, result) in results {
            if let Some(mr) = world.get_mut::<MeshRef>(*entity) {
                if let Some(h) = result.mesh_handle {
                    mr.render_handle = h;
                    mr.generation = 1;
                }
            }
            if let Some(handle) = result.mesh_handle {
                self.record_resolved(
                    *entity,
                    AssetSlot::Mesh,
                    AssetRelease {
                        mesh_handle: Some(handle),
                        ..AssetRelease::default()
                    },
                );
            }
            if let Some(bounds) = self.pending_bounds.remove(entity) {
                if let Some(handle) = result.mesh_handle {
                    if world.get_resource::<MeshBounds>().is_none() {
                        world.insert_resource(MeshBounds::default());
                    }
                    if let Some(registry) = world.get_resource_mut::<MeshBounds>() {
                        registry.insert(handle, bounds);
                    }
                }
            }
            if let Some(mr) = world.get_mut::<MaterialRef>(*entity) {
                if let Some(slot) = result.material_slot {
                    mr.material_slot = slot;
                    mr.generation = 1;
                }
            }
            if let Some(slot) = result.material_slot {
                self.record_resolved(
                    *entity,
                    AssetSlot::Material,
                    AssetRelease {
                        material_slot: Some(slot),
                        ..AssetRelease::default()
                    },
                );
            }
            if let (Some(slot), Some(size)) = (result.texture_slot, result.texture_size) {
                let texture_slot = self
                    .pending_texture_slot
                    .remove(entity)
                    .unwrap_or(AssetSlot::Sprite);
                self.record_resolved(
                    *entity,
                    texture_slot,
                    AssetRelease {
                        texture_slot: Some(slot),
                        ..AssetRelease::default()
                    },
                );
                let mut sprite =
                    world
                        .get::<SpriteRef>(*entity)
                        .cloned()
                        .unwrap_or_else(|| SpriteRef {
                            asset_id: crate::scene::components::SceneAssetId::generate(),
                            texture_slot: 0,
                            size: [0, 0],
                            generation: 0,
                        });
                sprite.texture_slot = slot;
                sprite.size = size;
                sprite.generation = 1;
                world.insert(*entity, sprite);

                // 瓦片地图的 tileset 走同一个上传结果（#529）。
                if world.get::<Tilemap>(*entity).is_some() {
                    let tilemap = world
                        .get::<TilemapRef>(*entity)
                        .cloned()
                        .unwrap_or_else(|| TilemapRef {
                            asset_id: crate::scene::components::SceneAssetId::generate(),
                            texture_slot: 0,
                            size: [0, 0],
                            generation: 0,
                        });
                    world.insert(
                        *entity,
                        TilemapRef {
                            texture_slot: slot,
                            size,
                            generation: 1,
                            ..tilemap
                        },
                    );
                }
                // UI 背景图走同一个上传结果（#591）。
                if world
                    .get::<crate::ui::components::Style>(*entity)
                    .is_some_and(|style| !style.image.is_empty())
                {
                    let image = world
                        .get::<UiImageRef>(*entity)
                        .cloned()
                        .unwrap_or_else(|| UiImageRef {
                            asset_id: crate::scene::components::SceneAssetId::generate(),
                            texture_slot: 0,
                            size: [0, 0],
                            generation: 0,
                        });
                    world.insert(
                        *entity,
                        UiImageRef {
                            texture_slot: slot,
                            size,
                            generation: 1,
                            ..image
                        },
                    );
                }
            }
            self.enqueued.remove(entity);
        }
    }

    // -----------------------------------------------------------------------
    // 热重载：让已解析的资产重新走一遍
    // -----------------------------------------------------------------------

    /// 让**所有** `MeshRenderer` / `SpriteRenderer` 的解析结果失效（`.pak` 重建后调用）。
    ///
    /// 只清实体这一侧的状态（`MeshRef` / `MaterialRef` / `SpriteRef` 的
    /// `generation`、失败缓存、排队标记），**不动** `ResourceManager`：包里的字节由
    /// [`ResourceManager::on_pak_changed`] 换掉，新字节在下一帧
    /// [`Self::prepare_requests`] 里重新读出来，渲染线程给它们新的句柄。
    ///
    /// 返回**重新排队**的实体数。旧 GPU 句柄暂时留在渲染器里（没有从 ECS 反向释放的接口），
    /// 频繁热重载会让纹理/材质槽增长 —— 已知遗留，见 #495 的后续条目。
    pub fn invalidate_assets(&mut self, world: &mut World) -> usize {
        let mut entities: Vec<Entity> = Vec::new();
        for (entity, _) in world.query::<MeshRenderer>() {
            entities.push(entity);
        }
        for (entity, _) in world.query::<SpriteRenderer>() {
            if !entities.contains(&entity) {
                entities.push(entity);
            }
        }
        for (entity, _) in world.query::<Tilemap>() {
            if !entities.contains(&entity) {
                entities.push(entity);
            }
        }
        // UI 背景图（#591）也走同一条贴图上传路径，热重载时必须一起失效。
        for (entity, _) in world.query::<crate::scene::runtime_components::UiImageRef>() {
            if !entities.contains(&entity) {
                entities.push(entity);
            }
        }
        let count = entities.len();
        for entity in entities {
            self.reset_entity_refs(world, entity);
        }
        count
    }

    /// 只让引用了 `path` 的实体重新解析（换了一张贴图 / 一个网格）。
    ///
    /// `path` 与作者文档里的写法一致（`mesh_path` / `material_path` /
    /// `sprite_path`）。返回引用了该路径的实体数；没有实体引用它时返回 0。
    pub fn invalidate_path(&mut self, world: &mut World, path: &str) -> usize {
        let mut entities: Vec<Entity> = Vec::new();
        for (entity, renderer) in world.query::<MeshRenderer>() {
            if renderer.mesh_path == path || renderer.material_path == path {
                entities.push(entity);
            }
        }
        for (entity, sprite) in world.query::<SpriteRenderer>() {
            if sprite.sprite_path == path && !entities.contains(&entity) {
                entities.push(entity);
            }
        }
        for (entity, tilemap) in world.query::<Tilemap>() {
            if tilemap.tileset_path == path && !entities.contains(&entity) {
                entities.push(entity);
            }
        }
        for (entity, style) in world.query::<crate::ui::components::Style>() {
            if style.image == path && !entities.contains(&entity) {
                entities.push(entity);
            }
        }
        let count = entities.len();
        for entity in entities {
            self.reset_entity_refs(world, entity);
        }
        count
    }

    /// 把实体上已解析的句柄打回 `generation == 0`，并清掉会挡住重新解析的缓存。
    ///
    /// 返回 `true` 表示确实做了修改（本来就是 `0` 的实体不算）。
    fn reset_entity_refs(&mut self, world: &mut World, entity: Entity) -> bool {
        let mut changed = false;
        if let Some(mesh) = world.get_mut::<MeshRef>(entity) {
            if mesh.generation != 0 {
                mesh.generation = 0;
                changed = true;
            }
        }
        if let Some(material) = world.get_mut::<MaterialRef>(entity) {
            if material.generation != 0 {
                material.generation = 0;
                changed = true;
            }
        }
        if let Some(sprite) = world.get_mut::<SpriteRef>(entity) {
            if sprite.generation != 0 {
                sprite.generation = 0;
                changed = true;
            }
        }
        if let Some(tileset) = world.get_mut::<TilemapRef>(entity) {
            if tileset.generation != 0 {
                tileset.generation = 0;
                changed = true;
            }
        }
        if let Some(image) = world.get_mut::<UiImageRef>(entity) {
            if image.generation != 0 {
                image.generation = 0;
                changed = true;
            }
        }
        // 失败缓存（同一个坏路径不再重试）与排队标记都会挡住重新解析。
        self.enqueued.remove(&entity);
        self.pending_bounds.remove(&entity);
        self.tileset_paths.remove(&entity);
        self.ui_image_paths.remove(&entity);
        self.failed.retain(|(owner, _), _| *owner != entity);
        changed
    }

    // -----------------------------------------------------------------------
    // 内部 CPU 加载 helpers（纯数据，无渲染器依赖）
    // -----------------------------------------------------------------------

    /// CPU 段：加载网格资源 + 解交织顶点 data → `MeshUploadInput`。
    fn load_mesh_inputs(&mut self, path: &str) -> Option<MeshUploadInput> {
        // 内置基础网格（编辑器 3D Object 预设）：不读 `.pak`、不需要 cook。
        if prism_render::primitives::is_builtin(path) {
            return prism_render::primitives::builtin(path);
        }

        let id = self.resource_manager.id_by_path(path).or_else(|| {
            report_missing_reference(path, "mesh");
            None
        })?;

        let handle = self
            .resource_manager
            .load_with_deps::<MeshAsset>(id)
            .map_err(|e| log::warn!("load_mesh_inputs: load '{path}' failed: {e}"))
            .ok()?;
        let mesh = self
            .resource_manager
            .get(handle)
            .map_err(|e| log::warn!("load_mesh_inputs: get '{path}' failed: {e}"))
            .ok()?;

        // RMES 的顶点区是分段布局（见 `deinterleave_rmes`），不是每顶点交错。
        deinterleave_rmes(path, &mesh.info)
    }

    /// CPU 段：加载材质资源 → 18 个 PBR 标量 + 5 张纹理像素
    /// （顺序：albedo, normal, metallic_roughness, emissive, occlusion）。
    fn load_material_data(&mut self, path: &str) -> Option<MaterialResolveData> {
        let id = self.resource_manager.id_by_path(path).or_else(|| {
            report_missing_reference(path, "material");
            None
        })?;

        let handle = self
            .resource_manager
            .load_with_deps::<MaterialAsset>(id)
            .map_err(|e| log::warn!("load_material_data: load '{path}' failed: {e}"))
            .ok()?;
        let mat = self
            .resource_manager
            .get(handle)
            .map_err(|e| log::warn!("load_material_data: get '{path}' failed: {e}"))
            .ok()?;

        let s = mat.scalars();
        let scalars: [f32; 18] = match s.len() {
            n if n >= 18 => {
                let mut a = [0f32; 18];
                a.copy_from_slice(&s[..18]);
                a
            }
            n => {
                log::warn!("load_material_data: expected >=18 scalars, got {n} for '{path}'");
                let mut a = [0f32; 18];
                a[..n.min(18)].copy_from_slice(&s[..n.min(18)]);
                a
            }
        };

        let tex_ids = mat.texture_ids();
        // 颜色贴图是 sRGB 编码：让硬件在采样时解码。法线/MR/AO 存的是线性
        // 数据，按 sRGB 解码会把 0.5 压成 ≈0.214（法线 Z 变负、光照全错）。
        let albedo = self.load_texture_data(tex_ids[0], true);
        let normal = self.load_texture_data(tex_ids[1], false);
        let mr = self.load_texture_data(tex_ids[2], false);
        let emissive = self.load_texture_data(tex_ids[3], true);
        let occlusion = self.load_texture_data(tex_ids[4], false);
        let textures = [albedo, normal, mr, emissive, occlusion];

        Some(MaterialResolveData { scalars, textures })
    }

    /// CPU 段：按路径加载单张纹理（精灵贴图）→ `TextureUploadInput`。
    fn load_texture_by_path(&mut self, path: &str) -> Option<TextureUploadInput> {
        let id = self.resource_manager.id_by_path(path).or_else(|| {
            report_missing_reference(path, "texture");
            None
        })?;
        self.load_texture_data(Some(id), true)
    }

    /// CPU 段：加载单张纹理依赖 → `TextureUploadInput` 像素（或品红回退）。
    ///
    /// `srgb` 是**槽位**的颜色空间（RTEX 头不区分 sRGB/线性）：albedo、emissive、
    /// 精灵贴图为 `true`，normal、metallic_roughness、occlusion 为 `false`。
    fn load_texture_data(
        &mut self,
        tex_id_opt: Option<AssetId>,
        srgb: bool,
    ) -> Option<TextureUploadInput> {
        let tex_id = tex_id_opt?;
        let tex_handle = self
            .resource_manager
            .load_with_deps::<TextureAsset>(tex_id)
            .map_err(|e| log::warn!("load_texture_data: load {tex_id} failed: {e}"))
            .ok()?;
        let tex = self
            .resource_manager
            .get(tex_handle)
            .map_err(|e| log::warn!("load_texture_data: get {tex_id} failed: {e}"))
            .ok()?;

        let mip0 = tex.info.mip_data.first().cloned().unwrap_or_default();
        let magenta = || TextureUploadInput {
            width: 1,
            height: 1,
            format: TextureFormat::Rgba8,
            mip_levels: 1,
            pixels: vec![255, 0, 255, 255],
        };

        let Some(format) = texture_format_for(tex.info.format, srgb) else {
            log::warn!(
                "load_texture_data: texture {tex_id} uses unsupported RTEX format {}; \
                 using magenta fallback",
                tex.info.format
            );
            return Some(magenta());
        };

        let input = if mip0.is_empty() {
            log::warn!("load_texture_data: texture {tex_id} has no mip 0; using magenta fallback");
            magenta()
        } else {
            let expected = format.compressed_byte_len(tex.info.width, tex.info.height);
            if mip0.len() != expected {
                log::warn!(
                    "load_texture_data: texture {tex_id} mip0 size {} != {}x{} {format:?} ({}); \
                     using magenta fallback",
                    mip0.len(),
                    tex.info.width,
                    tex.info.height,
                    expected
                );
                magenta()
            } else {
                TextureUploadInput {
                    width: tex.info.width,
                    height: tex.info.height,
                    format,
                    mip_levels: 0,
                    pixels: mip0,
                }
            }
        };

        Some(input)
    }
}

impl Default for GpuAssetResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 RMES 的分段顶点区解成 split arrays。
///
/// RMES 的顶点区**不是**每顶点交错，而是三段连续存放：
/// `[positions × vert_count][normals × vert_count][uv0 × vert_count]`。
/// 布局来自写入端 —— `prism-cooker` 的 `GltfImporter::write_intermediate`
/// 按这个顺序产出 RMXI，`MeshCooker::write_rmes` 原样复制，并按同样的分段
/// 写出 `pos_off / nrm_off / uv0_off`。
///
/// `stride_bytes` 只表示「每顶点合计多少字节」，拿它当 interleaved 行宽会把
/// 第二个顶点之后的 positions 读进 normals / uv 的内存里。
fn deinterleave_rmes(path: &str, info: &prism_asset::formats::RmesInfo) -> Option<MeshUploadInput> {
    let stride = info.stride_bytes as usize;
    if stride == 0 || stride % 4 != 0 {
        log::warn!("load_mesh_inputs: bad stride {stride} for '{path}'");
        return None;
    }
    let float_stride = stride / 4;
    let vert_count = info.vert_count as usize;
    let uv_floats = if info.uv_channels >= 1 { 2 } else { 0 };

    // 两种合法组合：每顶点 3 + uv 个 float（无 normals），或 6 + uv 个（有 normals）。
    let has_normals = if float_stride == 3 + uv_floats {
        false
    } else if float_stride == 6 + uv_floats {
        true
    } else {
        log::warn!(
            "load_mesh_inputs: stride mismatch for '{path}' \
             (got {float_stride} floats, uv_channels {})",
            info.uv_channels
        );
        return None;
    };

    if info.vertex_data.len() < vert_count * stride {
        log::warn!("load_mesh_inputs: vertex buffer truncated for '{path}'");
        return None;
    }
    if info.index_data.len() < info.idx_count as usize * 4 {
        log::warn!("load_mesh_inputs: index buffer truncated for '{path}'");
        return None;
    }

    // 段首（单位：float）。
    let nrm_base = vert_count * 3;
    let uv_base = nrm_base + if has_normals { vert_count * 3 } else { 0 };

    let read = |float_index: usize| -> f32 {
        let off = float_index * 4;
        f32::from_le_bytes(info.vertex_data[off..off + 4].try_into().unwrap())
    };

    let mut positions = Vec::with_capacity(vert_count);
    let mut normals = Vec::with_capacity(vert_count);
    let mut uvs = Vec::with_capacity(vert_count);
    let mut tangents = Vec::with_capacity(vert_count);

    for v in 0..vert_count {
        let p = v * 3;
        positions.push([read(p), read(p + 1), read(p + 2)]);
        if has_normals {
            let n = nrm_base + v * 3;
            normals.push([read(n), read(n + 1), read(n + 2)]);
        } else {
            // 没有法线的资源没有可靠的着色法线；给一个固定方向而不是 0 向量，
            // 避免光照直接退化成 NaN / 全黑。
            normals.push([0.0, 0.0, 1.0]);
        }
        if uv_floats == 2 {
            let t = uv_base + v * 2;
            uvs.push([read(t), read(t + 1)]);
        } else {
            uvs.push([0.0, 0.0]);
        }
        tangents.push([1.0, 0.0, 0.0, 1.0]); // default tangent
    }

    let mut indices = Vec::with_capacity(info.idx_count as usize);
    for i in 0..info.idx_count as usize {
        let off = i * 4;
        indices.push(u32::from_le_bytes(
            info.index_data[off..off + 4].try_into().unwrap(),
        ));
    }

    Some(MeshUploadInput {
        positions,
        normals,
        colors: vec![],
        uvs,
        tangents,
        indices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::components::{LocalToWorld, SceneAssetId};
    use prism_render::asset_bridge::AssetResolveResult;
    use prism_render::managers::MeshHandle;

    /// 造一个只含指定 `(path, type, bytes)` 的 `.pak` + 路径清单（#583 测试用）。
    fn tiny_pak(entries: &[(&str, prism_asset::core::AssetType, &[u8])]) -> (Vec<u8>, String) {
        use prism_asset::core::id::AssetIdGenerator;
        use prism_asset::package::PackageBuilder;

        let mut builder = PackageBuilder::new();
        let mut ids = AssetIdGenerator::new(1, 0);
        let mut manifest = Vec::new();
        for (path, asset_type, bytes) in entries {
            let id = ids.next();
            builder.add_asset(id, *asset_type, bytes.to_vec(), &[]);
            manifest.push(format!(
                "{{\"id\":\"0x{:016x}\",\"path\":\"{path}\"}}",
                id.into_raw()
            ));
        }
        let pak = builder.build().expect("build pak");
        let manifest = format!("{{\"assets\":[{}]}}", manifest.join(","));
        (pak, manifest)
    }

    #[test]
    fn project_fonts_are_loaded_once_by_path() {
        // #583：`Text.font` 指向的字体从 `.pak` 读出字节，交给渲染器注册。
        let (pak, manifest) = tiny_pak(&[(
            "fonts/hud.ttf",
            prism_asset::core::AssetType::Font,
            b"not-a-real-ttf",
        )]);
        let mut resolver = GpuAssetResolver::new();
        resolver
            .load_resource_package_bytes(&pak, Some(&manifest))
            .expect("load pak");

        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            crate::ui::components::Text {
                content: "HUD".into(),
                font: "fonts/hud.ttf".into(),
                ..Default::default()
            },
        );

        let fonts = resolver.prepare_ui_fonts(&world);
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].0, "fonts/hud.ttf");
        assert_eq!(fonts[0].1, b"not-a-real-ttf");

        // 同一个路径不再重复读；没有 font 的文本不产生请求。
        assert!(resolver.prepare_ui_fonts(&world).is_empty());
        world
            .get_mut::<crate::ui::components::Text>(entity)
            .unwrap()
            .font = "fonts/nope.ttf".into();
        assert!(
            resolver.prepare_ui_fonts(&world).is_empty(),
            "缺失的字体只报一次，不返回字节"
        );
        assert!(resolver.prepare_ui_fonts(&world).is_empty());
    }

    /// #495：句柄记账 / 释放。返回一个已经「上传完成」的精灵实体。
    fn resolved_sprite(world: &mut World, resolver: &mut GpuAssetResolver, slot: u32) -> Entity {
        let entity = world.spawn();
        world.insert(
            entity,
            SpriteRenderer {
                sprite_path: "sprites/boat.png".to_string(),
                ..Default::default()
            },
        );
        resolver.apply_results(
            world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(slot),
                    texture_size: Some([16, 16]),
                },
            )],
        );
        entity
    }

    #[test]
    fn a_good_upload_does_not_queue_a_release() {
        let mut world = World::new();
        let mut resolver = GpuAssetResolver::new();
        resolved_sprite(&mut world, &mut resolver, 9);
        assert!(resolver.take_releases().is_empty());
    }

    #[test]
    fn re_uploading_to_a_new_slot_releases_the_previous_one() {
        // 路径被改写 → 新上传拿到新槽位，旧槽位必须还回去，否则每次换图漏一张。
        let mut world = World::new();
        let mut resolver = GpuAssetResolver::new();
        let entity = resolved_sprite(&mut world, &mut resolver, 9);
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(21),
                    texture_size: Some([16, 16]),
                },
            )],
        );
        let releases = resolver.take_releases();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].texture_slot, Some(9));
        // 同一个槽位重复回传（结果重放）不算失效。
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(21),
                    texture_size: Some([16, 16]),
                },
            )],
        );
        assert!(resolver.take_releases().is_empty());
    }

    #[test]
    fn despawning_the_entity_releases_its_gpu_handles() {
        let mut world = World::new();
        let mut resolver = GpuAssetResolver::new();
        let entity = resolved_sprite(&mut world, &mut resolver, 5);
        world.despawn(entity);

        // `prepare_requests` 开头会回收失效资源。
        resolver.prepare_requests(&world);
        let releases = resolver.take_releases();
        assert_eq!(releases.len(), 1, "{releases:?}");
        assert_eq!(releases[0].texture_slot, Some(5));
        // 只释放一次。
        resolver.prepare_requests(&world);
        assert!(resolver.take_releases().is_empty());
    }

    #[test]
    fn hot_reload_invalidation_releases_every_handle() {
        // 热重载会把 generation 清零；旧图像 / 网格必须这一帧就还回去。
        let mut world = World::new();
        let mut resolver = GpuAssetResolver::new();
        let sprite = resolved_sprite(&mut world, &mut resolver, 7);
        let mesh_entity = spawn_renderer(&mut world, "models/cube.gltf", "materials/cube.mat.json");
        let mesh_handle = prism_render::managers::MeshHandle::default();
        resolver.apply_results(
            &mut world,
            &[(
                mesh_entity,
                AssetResolveResult {
                    mesh_handle: Some(mesh_handle),
                    material_slot: Some(4),
                    texture_slot: None,
                    texture_size: None,
                },
            )],
        );
        assert!(resolver.take_releases().is_empty());

        resolver.invalidate_assets(&mut world);
        resolver.prepare_requests(&world);
        let mut releases = resolver.take_releases();
        releases.sort_by_key(|r| {
            (
                r.texture_slot.unwrap_or(u32::MAX),
                r.material_slot.unwrap_or(u32::MAX),
            )
        });
        assert_eq!(releases.len(), 3, "{releases:?}");
        assert!(releases.iter().any(|r| r.texture_slot == Some(7)));
        assert!(releases.iter().any(|r| r.material_slot == Some(4)));
        assert!(releases.iter().any(|r| r.mesh_handle == Some(mesh_handle)));
        // 精灵实体本身还在，重新上传时会重新记账。
        assert!(world.get::<SpriteRef>(sprite).is_some());
    }

    fn spawn_renderer(world: &mut World, mesh_path: &str, material_path: &str) -> Entity {
        let entity = world.spawn();
        world.insert(
            entity,
            MeshRenderer {
                mesh_path: mesh_path.to_string(),
                material_path: material_path.to_string(),
                ..Default::default()
            },
        );
        world.insert(
            entity,
            MeshRef {
                asset_id: SceneAssetId::generate(),
                render_handle: MeshHandle::default(),
                generation: 0,
            },
        );
        world.insert(entity, LocalToWorld(glam::Mat4::IDENTITY));
        entity
    }

    /// 运行时材质句柄：`generation == 0` 表示材质还没解析。
    fn insert_material_ref(world: &mut World, entity: Entity) {
        world.insert(
            entity,
            MaterialRef {
                asset_id: SceneAssetId::generate(),
                material_slot: 0,
                generation: 0,
            },
        );
    }

    /// 精灵只需要 `SpriteRenderer`：`SpriteRef` 由解析结果补上，项目不碰引擎内部状态。
    #[test]
    fn sprite_without_ref_is_resolved_and_ref_is_created() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            SpriteRenderer {
                sprite_path: "sprites/boat.png".to_string(),
                ..Default::default()
            },
        );
        let mut resolver = GpuAssetResolver::new();

        let first = resolver.prepare_requests(&world);
        // 资源包为空，所以贴图加载会失败；关键是「没有 `SpriteRef` 也会入队」。
        assert_eq!(first.len(), 1, "没有 SpriteRef 的精灵也要解析一次");

        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(7),
                    texture_size: Some([16, 16]),
                },
            )],
        );
        let sprite = world
            .get::<SpriteRef>(entity)
            .expect("解析后应补上 SpriteRef");
        assert_eq!(sprite.texture_slot, 7);
        assert_eq!(sprite.generation, 1);

        // 已解析且路径没变：不再重复投递。
        assert!(resolver.prepare_requests(&world).is_empty());
    }

    #[test]
    fn tilemap_tileset_is_staged_once_and_written_back() {
        // #529：瓦片地图走和精灵同一条贴图解析路径，但句柄写进 `TilemapRef`。
        use crate::scene::components::Tilemap;
        use crate::scene::runtime_components::TilemapRef;

        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            Tilemap {
                tileset_path: "sprites/tiles.png".to_string(),
                tiles: vec![1, 0, 2],
                ..Tilemap::default()
            },
        );
        let mut resolver = GpuAssetResolver::new();

        let first = resolver.prepare_requests(&world);
        assert_eq!(first.len(), 1, "没有 TilemapRef 也要解析一次");
        // 资源包为空 → 失败缓存，不会每帧重试。
        assert!(resolver.prepare_requests(&world).is_empty());

        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(3),
                    texture_size: Some([64, 64]),
                },
            )],
        );
        let tileset = world
            .get::<TilemapRef>(entity)
            .expect("解析结果应写进 TilemapRef");
        assert_eq!(tileset.texture_slot, 3);
        assert_eq!(tileset.size, [64, 64]);
        assert_eq!(tileset.generation, 1);
    }

    /// #591：`Style::image` 和精灵一样走贴图解析，结果写进 `UiImageRef`。
    #[test]
    fn ui_background_images_are_staged_and_written_back() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            crate::ui::components::Style {
                image: "textures/icon.png".to_string(),
                background: [1.0; 4],
                ..Default::default()
            },
        );
        let mut resolver = GpuAssetResolver::new();

        let first = resolver.prepare_requests(&world);
        assert_eq!(first.len(), 1, "没有 UiImageRef 也要解析一次");
        // 包是空的 → 记失败缓存，不会每帧重试。
        assert!(resolver.prepare_requests(&world).is_empty());

        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(5),
                    texture_size: Some([32, 32]),
                },
            )],
        );
        let image = world
            .get::<UiImageRef>(entity)
            .expect("解析结果应写进 UiImageRef");
        assert_eq!(image.texture_slot, 5);
        assert_eq!(image.size, [32, 32]);
        assert_eq!(image.generation, 1);
    }

    /// 改 `sprite_path` 就够：不再需要手动把 `generation` 清零。
    #[test]
    fn sprite_path_change_re_resolves_without_touching_generation() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            SpriteRenderer {
                sprite_path: "sprites/bg@day.png".to_string(),
                ..Default::default()
            },
        );
        let mut resolver = GpuAssetResolver::new();
        assert_eq!(resolver.prepare_requests(&world).len(), 1);
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: Some(1),
                    texture_size: Some([8, 8]),
                },
            )],
        );
        assert_eq!(world.get::<SpriteRef>(entity).unwrap().generation, 1);

        world.get_mut::<SpriteRenderer>(entity).unwrap().sprite_path =
            "sprites/bg@night.png".to_string();
        let requests = resolver.prepare_requests(&world);
        assert_eq!(requests.len(), 1, "路径变化必须重新解析");
        assert_eq!(
            world.get::<SpriteRef>(entity).unwrap().generation,
            1,
            "宿主不需要（也不应该）手动清零 generation"
        );
    }

    /// 无材质网格：材质路径为空时没有东西可解析，不能每帧重复投递
    /// （否则每次都会重新上传网格）。
    #[test]
    fn mesh_only_renderer_is_enqueued_once() {
        let mut world = World::new();
        let entity = spawn_renderer(&mut world, "models/box.gltf", "");
        let mut resolver = GpuAssetResolver::new();

        let first = resolver.prepare_requests(&world);
        assert_eq!(first.len(), 1, "网格路径仍需解析一次");
        assert!(
            first[0].1.material.is_none(),
            "空材质路径不应产生材质解析请求"
        );

        // 模拟渲染线程回传：网格句柄已解析，材质结果为空。
        world.get_mut::<MeshRef>(entity).unwrap().generation = 1;
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: None,
                    texture_size: None,
                },
            )],
        );

        assert!(
            resolver.prepare_requests(&world).is_empty(),
            "解析完成后不应再投递"
        );
    }

    /// 运行时组件缺失时结果无处写回，投递出去只会每帧重复 enqueue。
    #[test]
    fn renderer_without_runtime_refs_is_not_enqueued() {
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            MeshRenderer {
                mesh_path: "models/box.gltf".to_string(),
                material_path: "materials/box.mat".to_string(),
                ..Default::default()
            },
        );
        let mut resolver = GpuAssetResolver::new();

        assert!(resolver.prepare_requests(&world).is_empty());
        assert!(resolver.prepare_requests(&world).is_empty());
    }

    /// 半解析：网格上传成功、材质路径坏掉时，不能因为材质还在 `generation == 0`
    /// 就把已经解析好的网格每帧重新上传（显存 / bindless 槽泄漏）。
    #[test]
    fn failed_material_is_latched_and_the_resolved_mesh_is_not_reuploaded() {
        let mut world = World::new();
        let entity = spawn_renderer(&mut world, "models/box.gltf", "materials/missing.mat");
        insert_material_ref(&mut world, entity);
        let mut resolver = GpuAssetResolver::new();

        let first = resolver.prepare_requests(&world);
        assert_eq!(first.len(), 1, "两个槽位都还没解析，需要投递一次");

        // 渲染线程回传：网格上传成功，材质没有句柄。
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: Some(MeshHandle::default()),
                    material_slot: None,
                    texture_slot: None,
                    texture_size: None,
                },
            )],
        );
        assert_eq!(world.get::<MeshRef>(entity).unwrap().generation, 1);

        assert!(
            resolver.prepare_requests(&world).is_empty(),
            "失败的材质路径已被缓存，已解析的网格也不应重新投递"
        );
        assert!(
            resolver.prepare_requests(&world).is_empty(),
            "缓存必须跨帧生效（旧实现每帧都会重新请求）"
        );
    }

    /// 失败缓存以路径为值：作者换掉坏路径（或热重载换包）后必须重新尝试，
    /// 且只重试那半边。
    #[test]
    fn changing_a_failed_path_retries_only_that_half() {
        let mut world = World::new();
        let entity = spawn_renderer(&mut world, "models/box.gltf", "materials/missing.mat");
        insert_material_ref(&mut world, entity);
        let mut resolver = GpuAssetResolver::new();

        assert_eq!(resolver.prepare_requests(&world).len(), 1);
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                AssetResolveResult {
                    mesh_handle: Some(MeshHandle::default()),
                    material_slot: None,
                    texture_slot: None,
                    texture_size: None,
                },
            )],
        );

        world.get_mut::<MeshRenderer>(entity).unwrap().material_path =
            "materials/fixed.mat".to_string();
        let retry = resolver.prepare_requests(&world);
        assert_eq!(retry.len(), 1, "路径变化后应重新尝试");
        assert!(
            retry[0].1.mesh.is_none(),
            "网格已解析，重试时不应再次加载/上传它"
        );
    }

    /// 按 RMXI / RMES 的真实布局构造顶点区：分段，不是交错。
    fn planar_mesh(
        positions: &[[f32; 3]],
        normals: &[[f32; 3]],
        uvs: &[[f32; 2]],
        indices: &[u32],
    ) -> prism_asset::formats::RmesInfo {
        let mut vertex_data = Vec::new();
        for p in positions {
            for c in p {
                vertex_data.extend_from_slice(&c.to_le_bytes());
            }
        }
        for n in normals {
            for c in n {
                vertex_data.extend_from_slice(&c.to_le_bytes());
            }
        }
        for t in uvs {
            for c in t {
                vertex_data.extend_from_slice(&c.to_le_bytes());
            }
        }
        let float_stride =
            3 + if normals.is_empty() { 0 } else { 3 } + if uvs.is_empty() { 0 } else { 2 };
        prism_asset::formats::RmesInfo {
            vert_count: positions.len() as u32,
            idx_count: indices.len() as u32,
            uv_channels: if uvs.is_empty() { 0 } else { 1 },
            stride_bytes: (float_stride * 4) as u32,
            vertex_data,
            index_data: indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
        }
    }

    #[test]
    fn planar_rmes_splits_into_channels_instead_of_rows() {
        // 两个顶点的数值互不相同：任何「把分段当交错」的读法都会串道。
        let info = planar_mesh(
            &[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]],
            &[[0.0, 0.0, 1.0], [0.0, 1.0, 0.0]],
            &[[0.25, 0.5], [0.75, 1.0]],
            &[0, 1, 0],
        );

        let mesh = deinterleave_rmes("test/planar.rmes", &info).expect("valid RMES must decode");

        assert_eq!(mesh.positions, vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        assert_eq!(mesh.normals, vec![[0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]);
        assert_eq!(mesh.uvs, vec![[0.25, 0.5], [0.75, 1.0]]);
        assert_eq!(mesh.indices, vec![0, 1, 0]);
    }

    #[test]
    fn planar_rmes_without_normals_keeps_positions_and_uvs() {
        // 无 normals 时 stride 是 3 + 2 个 float，uv 段紧跟 positions 段。
        let info = planar_mesh(
            &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[],
            &[[0.1, 0.2], [0.3, 0.4]],
            &[0, 1, 1],
        );

        let mesh = deinterleave_rmes("test/uv-only.rmes", &info).expect("valid RMES must decode");

        assert_eq!(mesh.positions, vec![[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        assert_eq!(mesh.normals, vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0]]);
        assert_eq!(mesh.uvs, vec![[0.1, 0.2], [0.3, 0.4]]);
    }

    #[test]
    fn planar_rmes_positions_only_falls_back_to_a_fixed_normal() {
        let info = planar_mesh(&[[7.0, 8.0, 9.0], [10.0, 11.0, 12.0]], &[], &[], &[0, 1, 0]);

        let mesh = deinterleave_rmes("test/pos-only.rmes", &info).expect("valid RMES must decode");

        assert_eq!(mesh.positions, vec![[7.0, 8.0, 9.0], [10.0, 11.0, 12.0]]);
        assert_eq!(mesh.normals, vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0]]);
        assert_eq!(mesh.uvs, vec![[0.0, 0.0], [0.0, 0.0]]);
    }

    #[test]
    fn rtex_rgba8_follows_the_slot_colour_space() {
        assert_eq!(
            texture_format_for(RTEX_FORMAT_RGBA8, true),
            Some(TextureFormat::Rgba8Srgb)
        );
        assert_eq!(
            texture_format_for(RTEX_FORMAT_RGBA8, false),
            Some(TextureFormat::Rgba8),
            "linear slots must not be sRGB-decoded"
        );
    }

    #[test]
    fn rtex_compressed_formats_keep_linear_semantics() {
        assert_eq!(
            texture_format_for(RTEX_FORMAT_BC5, true),
            Some(TextureFormat::Bc5Unorm),
            "BC5 is a linear 2-channel normal map regardless of the slot"
        );
        assert_eq!(
            texture_format_for(RTEX_FORMAT_BC6H, false),
            Some(TextureFormat::Bc6HUfloat)
        );
    }

    #[test]
    fn bc1_and_bc3_follow_the_slot_colour_space() {
        // 颜色贴图要 sRGB 采样，法线 / MR 这类要保持线性（#519）。
        assert_eq!(
            texture_format_for(RTEX_FORMAT_BC1, true),
            Some(TextureFormat::Bc1Srgb)
        );
        assert_eq!(
            texture_format_for(RTEX_FORMAT_BC1, false),
            Some(TextureFormat::Bc1Unorm)
        );
        assert_eq!(
            texture_format_for(RTEX_FORMAT_BC3, true),
            Some(TextureFormat::Bc3Srgb)
        );
        assert_eq!(
            texture_format_for(RTEX_FORMAT_BC3, false),
            Some(TextureFormat::Bc3Unorm)
        );
    }

    #[test]
    fn unsupported_rtex_formats_fall_back() {
        assert_eq!(texture_format_for(99, false), None);
        assert_eq!(texture_format_for(99, true), None);
    }
}

#[cfg(test)]
mod mesh_bounds_tests {
    use super::*;
    use crate::scene::systems::render::MeshBounds;
    use prism_render::managers::MeshHandle;
    use prism_scene::SceneAssetId;

    #[test]
    fn the_mesh_bounds_land_in_the_registry_with_the_handle() {
        // 内置立方体不需要 `.pak`，但走的是同一条 prepare → upload → 写回路径。
        let mut world = World::new();
        let entity = world.spawn();
        world.insert(
            entity,
            MeshRenderer {
                mesh_path: "__builtin/box".into(),
                ..Default::default()
            },
        );
        world.insert(
            entity,
            MeshRef {
                asset_id: SceneAssetId::generate(),
                render_handle: MeshHandle::default(),
                generation: 0,
            },
        );

        let mut resolver = GpuAssetResolver::new();
        let requests = resolver.prepare_requests(&world);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].1.mesh.is_some());

        let handle = MeshHandle::default();
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                prism_render::asset_bridge::AssetResolveResult {
                    mesh_handle: Some(handle),
                    material_slot: None,
                    texture_slot: None,
                    texture_size: None,
                },
            )],
        );

        let registry = world
            .get_resource::<MeshBounds>()
            .expect("bounds registry must be created on the first upload");
        let bounds = registry.get(handle).expect("box bounds must be registered");
        // 单位立方体（边长 1，居中在原点）。
        assert!((bounds.min[0] + 0.5).abs() < 1e-5);
        assert!((bounds.max[1] - 0.5).abs() < 1e-5);

        // 再投递一次不会重复：句柄已写回（`generation = 1`）。
        assert!(resolver.prepare_requests(&world).is_empty());
    }
}

#[cfg(test)]
mod hot_reload_tests {
    use super::*;
    use prism_scene::SceneAssetId;

    fn spawn_mesh(world: &mut World, path: &str, generation: u32) -> Entity {
        let entity = world.spawn();
        world.insert(
            entity,
            MeshRenderer {
                mesh_path: path.into(),
                ..Default::default()
            },
        );
        world.insert(
            entity,
            MeshRef {
                asset_id: SceneAssetId::generate(),
                render_handle: Default::default(),
                generation,
            },
        );
        entity
    }

    #[test]
    fn invalidating_everything_re_uploads_the_meshes() {
        let mut world = World::new();
        let entity = spawn_mesh(&mut world, "__builtin/box", 1);

        let mut resolver = GpuAssetResolver::new();
        assert!(
            resolver.prepare_requests(&world).is_empty(),
            "a resolved mesh must not be re-uploaded on its own"
        );

        assert_eq!(resolver.invalidate_assets(&mut world), 1);
        assert_eq!(world.get::<MeshRef>(entity).unwrap().generation, 0);
        assert_eq!(
            resolver.prepare_requests(&world).len(),
            1,
            "a hot reload must put the entity back in the upload queue"
        );
    }

    #[test]
    fn invalidating_a_path_only_touches_its_users() {
        let mut world = World::new();
        let boxed = spawn_mesh(&mut world, "__builtin/box", 1);
        let sphere = spawn_mesh(&mut world, "__builtin/sphere", 1);

        let mut resolver = GpuAssetResolver::new();
        assert_eq!(resolver.invalidate_path(&mut world, "__builtin/box"), 1);
        assert_eq!(world.get::<MeshRef>(boxed).unwrap().generation, 0);
        assert_eq!(
            world.get::<MeshRef>(sphere).unwrap().generation,
            1,
            "an unrelated path must keep its resolved handles"
        );
        assert_eq!(
            resolver.invalidate_path(&mut world, "assets/nothing.rmes"),
            0
        );
    }

    #[test]
    fn invalidation_clears_the_failure_cache() {
        // 一个不存在于任何包里的路径：第一次 prepare 会失败并进失败缓存，
        // 之后不再重试。热重载必须能让它再试一次（新包里可能刚加上它）。
        let mut world = World::new();
        let entity = spawn_mesh(&mut world, "assets/new_mesh.rmes", 0);

        let mut resolver = GpuAssetResolver::new();
        let first = resolver.prepare_requests(&world);
        assert_eq!(first.len(), 1);
        assert!(first[0].1.mesh.is_none(), "the path is not in any package");
        // 上传失败也是「有结果」：清掉排队标记，于是只剩失败缓存挡着重试。
        resolver.apply_results(
            &mut world,
            &[(
                entity,
                prism_render::asset_bridge::AssetResolveResult {
                    mesh_handle: None,
                    material_slot: None,
                    texture_slot: None,
                    texture_size: None,
                },
            )],
        );
        assert!(
            resolver.prepare_requests(&world).is_empty(),
            "a failed path must not be retried until something changes"
        );

        assert_eq!(
            resolver.invalidate_path(&mut world, "assets/new_mesh.rmes"),
            1
        );
        assert_eq!(
            resolver.prepare_requests(&world).len(),
            1,
            "hot reload must clear the failed-path cache"
        );
        assert_eq!(world.get::<MeshRef>(entity).unwrap().generation, 0);
    }
}
