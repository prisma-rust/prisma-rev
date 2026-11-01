//! Runtime-only render handles attached to scene entities.
//!
//! These are intentionally **not** part of the scene schema: cooked scenes
//! carry authoring components (`MeshRenderer`, transforms, lights, …), while
//! the engine resolves those into GPU-side handles at load time.

use prism_render::managers::MeshHandle;
use prism_scene::SceneAssetId;

/// GPU mesh reference — resolved from an asset at spawn time.
#[derive(Debug, Clone, Copy)]
pub struct MeshRef {
    /// Stable asset id (hot-reload / debugging).
    pub asset_id: SceneAssetId,
    /// GPU-side handle.
    pub render_handle: MeshHandle,
    /// Asset generation when resolved; bumped on hot-reload.
    pub generation: u32,
}

/// GPU material slot reference.
#[derive(Debug, Clone, Copy)]
pub struct MaterialRef {
    /// Stable asset id.
    pub asset_id: SceneAssetId,
    /// SSBO slot from the render material manager.
    pub material_slot: u32,
    /// Generation at resolve time.
    pub generation: u32,
}

/// 一个网格里的 submesh 索引区间（#520）。
///
/// 由 `.pak` 里的 RMES v2 submesh 表还原。运行时由渲染提取系统按这些区间
/// 把一个实体拆成多个 [`DrawItem`](prism_render::DrawItem)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshSubmesh {
    pub index_start: u32,
    pub index_count: u32,
    /// 源文件（glTF）的材质索引；只用于把 `material_paths` 映射到 submesh。
    pub material_index: Option<u32>,
}

/// 运行时：网格的 submesh 表。作者组件 `MeshRenderer` 不保存它，
/// 因为区间属于被 cook 的网格资产，而不是场景里的某个实例。
#[derive(Debug, Clone, Default)]
pub struct MeshSubmeshes {
    pub submeshes: Vec<MeshSubmesh>,
}

/// 运行时：每个 submesh 解析后的材质 SSBO 槽（`None` = 默认材质）。
///
/// `MaterialRef::material_slot` 仍然保存第一个材质，兼容只读这个字段的旧代码；
/// 多材质绘制读取这里的逐 submesh 槽位。
#[derive(Debug, Clone, Default)]
pub struct MaterialSlots {
    pub slots: Vec<Option<u32>>,
}

/// GPU texture reference for a 2D [`SpriteRenderer`](prism_scene::components::SpriteRenderer).
///
/// Same role as [`MeshRef`] for meshes: the scene only stores the authoring
/// texture path, while the resolved bindless texture handle lives here.
#[derive(Debug, Clone, Copy)]
pub struct SpriteRef {
    /// Stable asset id (hot-reload / debugging).
    pub asset_id: SceneAssetId,
    /// Bindless SRV slot of the resolved texture (what the sprite pass reads).
    pub texture_slot: u32,
    /// Pixel size of the uploaded texture, filled in after upload.
    pub size: [u32; 2],
    /// `0` = not resolved yet; bumped on hot-reload.
    pub generation: u32,
}

/// GPU texture reference for a 2D [`Tilemap`](prism_scene::components::Tilemap) tileset.
///
/// Same role as [`SpriteRef`]: the scene stores `tileset_path`, the resolver uploads
/// it and writes the bindless slot here (#529).
#[derive(Debug, Clone, Copy)]
pub struct TilemapRef {
    /// Stable asset id (hot-reload / debugging).
    pub asset_id: SceneAssetId,
    /// Bindless SRV slot of the tileset texture.
    pub texture_slot: u32,
    /// Pixel size of the uploaded tileset.
    pub size: [u32; 2],
    /// `0` = not resolved yet; bumped on hot-reload.
    pub generation: u32,
}

/// GPU texture reference for a UI [`Style::image`](prism_scene::ui::Style) background.
///
/// Same role as [`SpriteRef`]: the scene stores the authoring path, the resolver
/// uploads it and writes the bindless slot here (#591).
#[derive(Debug, Clone, Copy)]
pub struct UiImageRef {
    /// Stable asset id (hot-reload / debugging).
    pub asset_id: SceneAssetId,
    /// Bindless SRV slot of the uploaded image.
    pub texture_slot: u32,
    /// Pixel size of the uploaded image.
    pub size: [u32; 2],
    /// `0` = not resolved yet; bumped on hot-reload.
    pub generation: u32,
}

/// `.scene.json` v3+ 里每个实体的稳定 id。
///
/// ECS [`Entity`](prism_ecs::Entity) 句柄每次加载都会重新分配，跨会话（存档恢复、
/// 编辑器同步）匹配同一实体只能靠作者文档里的 id。加载器在 spawn 时写入这个标记；
/// 它不参与渲染，也不进 `.scene.json`（作者文档里已经有 `id` 了）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SceneEntityId(pub u32);
