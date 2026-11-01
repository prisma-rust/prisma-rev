//! 现代场景系统的 ECS 组件
//!
//! | 类别       | 组件 |
//! |-----------|------------|
//! | 层次结构  | `Parent`, `Child` |
//! | 变换     | `LocalTransform`, `LocalToWorld`, `TransformDirty` |
//! | 渲染     | `MeshRenderer`, `SpriteRenderer`, `Tilemap`, `Active` |
//! | 动画     | `SpriteAnimation`（2D 帧）, `KeyframeAnimation`（曲线） |
//! | 粒子     | `ParticleSystem`（世界空间 3D CPU 粒子）、`ParticleEmitter2D`（世界 XY 平面 2D 粒子） |
//! | 光源 | `Light`（形状）, `DirectionalLight`, `PointLight`, `SpotLight`（旧）, `Light2D`（2D 精灵光照） |
//! | 相机     | `Camera`, `FlyCameraController` |
//! | 天空盒   | `Skybox` |
//! | 音频     | `AudioSource`（场景内播放）, `AudioListener`（听者） |
//! | 地形     | `Terrain`（高度场 → 运行时生成网格） |
//! | 蒙皮     | `SkeletonRoot`（骨架）, `SkinnedMeshRenderer`（蒙皮网格） |
//! | 物理     | `Collider`（形状 + 查询层）, `Rigidbody`（Static/Kinematic/Dynamic） |
//! | 调试     | `Gizmo` |
//! | 场景     | `SceneMember` |
//! | 标识     | `Name` |

use prism_ecs::Entity;
use prism_reflect::{FieldType, Reflect, ReflectError, ReflectType, Value};
use serde::{Deserialize, Serialize};

use crate::render_enums::{
    correlated_color_temperature_to_rgb, BlendMode, BlendOp, CameraClearFlags, CullMode,
    LightRenderMode, LightShadows, LightType, RenderQueue, ShadowCastingMode,
};

// ---------------------------------------------------------------------------
// SceneAssetId
// ---------------------------------------------------------------------------

/// 场景 schema 使用的 64 位资源标识符。
///
/// 与 `prism_asset::core::AssetId` 的二进制表示一致，但保留为本地类型，
/// 使 `prism-scene` 不依赖运行时 `.pak` 代码。engine 在资产解析边界做转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SceneAssetId(pub u64);

impl ReflectType for SceneAssetId {
    fn field_type() -> FieldType {
        FieldType::U64
    }

    fn to_value(&self) -> Value {
        Value::U64(self.0)
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        Ok(Self(value.as_u64()?))
    }
}

impl SceneAssetId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// Hierarchy
// ---------------------------------------------------------------------------

/// 指向父实体的引用。对应 数据导向 `.Transforms.Parent`（`.0` ≡ `.Value`）。
///
/// 没有 `Parent` 组件的实体是场景层次中的**根节点**。请用
/// [`HierarchyHelper`](super::helpers::HierarchyHelper) 修改父子关系 —— 不要直接改 `Child`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parent(pub Entity);

impl Parent {
    /// DOTS `Parent.Value`：父实体。
    pub const fn value(&self) -> Entity {
        self.0
    }
}

/// 派生的子实体列表。对应 数据导向 `.Transforms.Child`（缓冲区）。
///
/// 由 [`HierarchyHelper::reparent`] 保持同步；**不要**手工修改。
/// 没有该组件的实体没有子节点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Child(pub Vec<Entity>);

impl Child {
    /// 子实体切片。
    pub fn value(&self) -> &[Entity] {
        &self.0
    }

    /// 子实体列表（可变）。
    pub fn value_mut(&mut self) -> &mut Vec<Entity> {
        &mut self.0
    }
}

// ---------------------------------------------------------------------------
// 变换
// ---------------------------------------------------------------------------

/// 相对父实体的局部变换（根节点则为世界空间）。
///
/// 对应 数据导向 `.Transforms.LocalTransform`
/// （`position` / `rotation` / `scale` ≡ DOTS 的 `Position` / `Rotation` / `Scale`）。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::LocalTransform")]
#[serde(default)]
pub struct LocalTransform {
    pub position: glam::Vec3,
    /// 四元数 Identity = `glam::Quat::IDENTITY`.
    pub rotation: glam::Quat,
    pub scale: glam::Vec3,
}

impl Default for LocalTransform {
    fn default() -> Self {
        Self {
            position: glam::Vec3::ZERO,
            rotation: glam::Quat::IDENTITY,
            scale: glam::Vec3::ONE,
        }
    }
}

impl LocalTransform {
    /// 构建 a 模型 矩阵 `T × R × S`.
    pub fn to_model_matrix(&self) -> glam::Mat4 {
        glam::Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.position)
    }
}

/// 由层次系统（`HierarchySystem`）计算的世界空间变换矩阵，每帧更新。
///
/// 对应 数据导向 `.Transforms.LocalToWorld`（`.0` ≡ DOTS 的 `.Value`）。
#[derive(Debug, Clone, Copy)]
pub struct LocalToWorld(pub glam::Mat4);

impl LocalToWorld {
    /// DOTS `LocalToWorld.Value`：世界变换矩阵。
    pub const fn value(&self) -> glam::Mat4 {
        self.0
    }
}

/// Optional dirty marker for subtree-optimised recompute future use).
#[derive(Debug, Clone, Copy, Default)]
pub struct TransformDirty(pub bool);

// ---------------------------------------------------------------------------
// 渲染 references
// ---------------------------------------------------------------------------

/// 激活 状态 — whether the 实体 participates in 渲染
///
/// Defaults to `true`. 集合 to `false` to hide an 实体 without despawning it
/// (the 实体 and its components remain in the 世界
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Active")]
pub struct Active(pub bool);

impl Default for Active {
    fn default() -> Self {
        Self(true)
    }
}

/// 跨场景保留标记（Unity 的 `DontDestroyOnLoad` 等价物）。
///
/// 场景切换（`Engine::unload_scene` 或 `LoadSceneMode::Single` 加载）时，
/// 带这个标记的实体**以及它们的整棵子树**不会被卸载 —— 玩家管理器、音频源、
/// 转场遮罩这类东西需要活过换场。只在运行时添加，不是可挂载组件，
/// 因此不会出现在 `.scene.json` 里（见 `json::DERIVED_COMPONENTS`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Persistent;

/// 派生：自身 [`Active`] 与所有祖先 `Active` 的与（Unity 的
/// `activeInHierarchy`）。
///
/// 由 `prism_engine::scene::systems::hierarchy::active_hierarchy_system` 每帧重写，
/// 与 [`LocalToWorld`] 同类的运行时派生数据：**不进 `.scene.json`、不进组件注册表**。
/// 作者 / 检查器写的永远是 [`Active`]（Unity 的 `activeSelf`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveInHierarchy(pub bool);

impl Default for ActiveInHierarchy {
    fn default() -> Self {
        Self(true)
    }
}

// ---------------------------------------------------------------------------
// Authoring-time helpers
// ---------------------------------------------------------------------------

/// Authoring-time bundle marker for a renderable 实体
///
/// Inserted by [`SceneLoader`](crate::scene::loader::SceneLoader) during
/// scene 生成 when an 实体 carries a 网格 path; the 材质 path is optional
/// (a mesh-only renderer still draws, using the 渲染器 default 材质).
/// This 分量 does **not** participate in 渲染 queries — the actual
/// 渲染 data lives on [`MeshRef`] and [`MaterialRef`] — but provides a
/// convenient handle for 检查器 editing and future hot-reload.
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::MeshRenderer")]
#[serde(default)]
pub struct MeshRenderer {
    pub mesh_path: String,
    /// 单材质路径（兼容旧场景；`material_paths` 为空时使用）。
    pub material_path: String,
    /// 每个 submesh 一个材质路径（#520）。
    ///
    /// 长度通常等于网格的 submesh 数；为空时回退到 [`Self::material_path`]。
    /// 长度不足时按 submesh 的源材质索引（glTF `material.index()`）取，
    /// 再退到 `material_path`。空字符串表示该 submesh 使用默认材质。
    pub material_paths: Vec<String>,
    /// 阴影投射模式 = `Renderer.shadowCastingMode`。
    ///
    /// 决定该物体是否写阴影贴图、是否出现在相机主通道（`ShadowsOnly` 只投影）。
    pub shadow_casting_mode: ShadowCastingMode,
    /// 剔除模式 = 材质/渲染器的 `CullMode`（着色器属性 `_Cull`）。
    ///
    /// 把它放在 `Material` 上；PrismaRev 的材质目前只有 PBR 参数，
    /// 所以先落在渲染器上，语义仍是「这一个绘制用哪一面」。
    pub cull_mode: CullMode,
    /// 渲染队列 = `Material.renderQueue`（`int`，不是枚举）。
    ///
    /// 决定绘制顺序：小于 [`RenderQueue::GEOMETRY_LAST`] 的不透明物体先画，
    /// 之后才是（透明）队列，透明队列内部按距离从远到近。
    pub render_queue: RenderQueue,
    /// 是否写深度 = `_ZWrite`。透明物体通常关掉。
    pub z_write: bool,
    /// 源混合因子 = `_SrcBlend`。
    pub src_blend: BlendMode,
    /// 目标混合因子 = `_DstBlend`。
    pub dst_blend: BlendMode,
    /// 混合运算 = `_BlendOp`。
    pub blend_op: BlendOp,
}

impl Default for MeshRenderer {
    fn default() -> Self {
        Self {
            mesh_path: String::new(),
            material_path: String::new(),
            material_paths: Vec::new(),
            shadow_casting_mode: ShadowCastingMode::default(),
            cull_mode: CullMode::default(),
            render_queue: RenderQueue::default(),
            z_write: true,
            // 引擎的不透明默认值：One / Zero / Add（见 `blends`）。
            src_blend: BlendMode::One,
            dst_blend: BlendMode::Zero,
            blend_op: BlendOp::Add,
        }
    }
}

impl MeshRenderer {
    pub fn has_mesh(&self) -> bool {
        !self.mesh_path.is_empty()
    }
    pub fn has_material(&self) -> bool {
        !self.material_path.is_empty() || self.material_paths.iter().any(|path| !path.is_empty())
    }

    /// 第 `submesh` 个 submesh 应该用哪个材质路径（#520）。
    ///
    /// 解析顺序：
    /// 1. `material_paths` 与 submesh 数一一对应时，直接用同下标；
    /// 2. 否则如果知道源文件里的材质索引（glTF），按材质索引取；
    /// 3. 再退到同下标（短列表时最后一个之外的项由调用方决定默认）；
    /// 4. 最后回退到单数字段 [`Self::material_path`]。
    pub fn material_path_for_submesh(
        &self,
        submesh: usize,
        submesh_count: usize,
        material_index: Option<u32>,
    ) -> Option<&str> {
        if !self.material_paths.is_empty() {
            if self.material_paths.len() == submesh_count {
                if let Some(path) = self
                    .material_paths
                    .get(submesh)
                    .filter(|path| !path.is_empty())
                {
                    return Some(path.as_str());
                }
            }
            if let Some(index) = material_index {
                if let Some(path) = self
                    .material_paths
                    .get(index as usize)
                    .filter(|path| !path.is_empty())
                {
                    return Some(path.as_str());
                }
            }
            if let Some(path) = self
                .material_paths
                .get(submesh)
                .filter(|path| !path.is_empty())
            {
                return Some(path.as_str());
            }
        }

        (!self.material_path.is_empty()).then_some(self.material_path.as_str())
    }
    /// 是否真的需要开启混合：用 `One` / `Zero` 表示「不混合」，
    /// 所以默认值代表不透明。
    pub fn blends(&self) -> bool {
        self.src_blend != BlendMode::One
            || self.dst_blend != BlendMode::Zero
            || self.blend_op != BlendOp::Add
    }
}

// ---------------------------------------------------------------------------
// 2D 精灵
// ---------------------------------------------------------------------------

/// 2D 精灵渲染器 = `SpriteRenderer`（渲染子集）。
///
/// 只承载**作者态**字段：贴图路径、精灵表帧、排序层。GPU 纹理句柄在运行时由
/// `SpriteRef`（`prism-engine::scene::runtime_components`）持有——和
/// `MeshRenderer` → `MeshRef` 的关系完全一致。
///
/// 2D 约定（见 `docs/plans/2026-10-27-cozy-fishing-design.md`）：
/// 相机在 **+Z 看向 −Z**，`z` 越大越靠近相机；世界尺寸 =
/// 纹理像素尺寸 / [`SpriteRenderer::pixels_per_unit`]。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::SpriteRenderer")]
#[serde(default)]
pub struct SpriteRenderer {
    /// 贴图资源路径 = `SpriteRenderer.sprite`。空字符串 = 不绘制。
    pub sprite_path: String,
    /// 精灵表单帧像素尺寸；`[0, 0]` 表示整张纹理就是一帧。
    pub frame_size: [u32; 2],
    /// 图集每行的列数；`0` = 由「纹理宽度 / 单帧宽度」推导（#584）。
    ///
    /// 多行图集里一行通常是一个动作 / 一个角色。
    pub sheet_columns: u32,
    /// 图集行号（0 = 第一行）；超出范围时夹到最后一行（#584）。
    pub sheet_row: u32,
    /// 当前帧索引（在本行内从左到右；超过列数才继续到下一行）。
    pub frame_index: u32,
    /// 每世界单位像素数 = `Sprite.pixelsPerUnit`。
    pub pixels_per_unit: f32,
    /// 轴心（`0..1`，**左下角**为原点）= `Sprite.pivot` 归一化形式。
    /// `[0.5, 0.5]` = 居中，`[0.5, 0.0]` = 底边居中，`[0.5, 1.0]` = 顶边居中。
    pub pivot: [f32; 2],
    /// 顶点色乘算（线性 RGBA）= `SpriteRenderer.color`。
    pub color: [f32; 4],
    /// 水平翻转 = `SpriteRenderer.flipX`。
    pub flip_x: bool,
    /// 垂直翻转 = `SpriteRenderer.flipY`。
    pub flip_y: bool,
    /// 排序层，数值越大越靠前 = `sortingLayerName` 的数值形式。
    pub sorting_layer: i32,
    /// 同层内顺序 = `sortingOrder`。
    pub order_in_layer: i32,
    /// 源混合因子（默认 `SrcAlpha`，标准 alpha 混合，#610）。
    pub src_blend: BlendMode,
    /// 目标混合因子（默认 `OneMinusSrcAlpha`）。
    pub dst_blend: BlendMode,
    /// 混合运算（默认 `Add`）；加色辉光用 `SrcAlpha` / `One`。
    pub blend_op: BlendOp,
    /// 是否用 [`SpriteRenderer::clip_rect`] 裁剪这个精灵（#582）。
    ///
    /// 裁剪在片元着色器里按**世界坐标**逐像素判断，所以不影响一次 draw call，
    /// 同一张水线矩形可以被任意多个精灵共享（倒影、水面以下的整组装饰）。
    /// 多个精灵用同一个矩形 = Unity `SpriteMask` 的矩形模式。
    pub clip_enabled: bool,
    /// 世界空间裁剪矩形 `[x0, y0, x1, y1]`（`x0 <= x1`、`y0 <= y1` 不是必须的，
    /// 引擎会自己排序）；只有 `clip_enabled` 为真时生效。
    ///
    /// 启用一个零面积矩形会把整个精灵裁掉——先填好矩形再打开开关。
    pub clip_rect: [f32; 4],
    /// 是否被 [`Light2D`] 照亮（#611）。
    ///
    /// 默认 `false`：2D 精灵保持引入光照之前的样子（`贴图 × color`）。
    /// 打开后片元按世界位置累加场景里的 `Light2D`，再加上环境光——
    /// 没有任何 `Light2D` 时仍然按 unlit 处理，所以只改这一个开关不会让画面变黑。
    pub receive_lights: bool,
}

impl Default for SpriteRenderer {
    fn default() -> Self {
        Self {
            sprite_path: String::new(),
            frame_size: [0, 0],
            sheet_columns: 0,
            sheet_row: 0,
            frame_index: 0,
            pixels_per_unit: 100.0,
            pivot: [0.5, 0.5],
            color: [1.0, 1.0, 1.0, 1.0],
            flip_x: false,
            flip_y: false,
            sorting_layer: 0,
            order_in_layer: 0,
            src_blend: BlendMode::SrcAlpha,
            dst_blend: BlendMode::OneMinusSrcAlpha,
            blend_op: BlendOp::Add,
            clip_enabled: false,
            clip_rect: [0.0; 4],
            receive_lights: false,
        }
    }
}

impl SpriteRenderer {
    /// 是否有可绘制的贴图。
    pub fn has_sprite(&self) -> bool {
        !self.sprite_path.is_empty()
    }

    /// 单帧像素尺寸；未配置精灵表时由调用方回落到纹理尺寸。
    pub fn frame_size_or(&self, texture_size: [u32; 2]) -> [u32; 2] {
        if self.frame_size[0] > 0 && self.frame_size[1] > 0 {
            self.frame_size
        } else {
            texture_size
        }
    }

    /// 图集列数：显式 `sheet_columns` 优先，否则按纹理宽度 / 单帧宽度推导。
    pub fn sheet_columns_or(&self, texture_size: [u32; 2]) -> u32 {
        if self.sheet_columns > 0 {
            return self.sheet_columns.max(1);
        }
        let frame = self.frame_size_or(texture_size);
        (texture_size[0] / frame[0].max(1)).max(1)
    }

    /// 图集行数（至少 1）。
    pub fn sheet_rows(&self, texture_size: [u32; 2]) -> u32 {
        let frame = self.frame_size_or(texture_size);
        (texture_size[1] / frame[1].max(1)).max(1)
    }

    /// 单帧在世界空间的尺寸（单位/世界）。`pixels_per_unit` 非法时回落到 100。
    pub fn world_size(&self, texture_size: [u32; 2]) -> [f32; 2] {
        let ppu = if self.pixels_per_unit.is_finite() && self.pixels_per_unit > 0.0 {
            self.pixels_per_unit
        } else {
            100.0
        };
        let frame = self.frame_size_or(texture_size);
        [frame[0] as f32 / ppu, frame[1] as f32 / ppu]
    }
}

/// 2D 瓦片地图 = `Tilemap`（#529）。
///
/// 一个实体承载整张网格：引擎每帧把**非空**格子展开成 `SpriteDrawItem`
/// （共用 `SpritePass` 的实例缓冲与 bindless 纹理表），所以几百格仍然只有
/// 一次 draw call，而不是几百个实体各带一份 `SpriteRenderer`。
///
/// 约定：
/// - `tiles` 是**行优先**的扁平数组，`0` = 空格（与 Tiled 的 GID 0 一致），
///   `n > 0` = tileset 里第 `n - 1` 块（从左到右、从上到下）；
/// - `columns == 0` 表示所有瓦片排成一行；
/// - 第 0 行在**上方**（世界 `+Y` 方向是屏幕上方），所以行号越大世界 `y` 越小；
/// - 网格原点 = 实体世界位置（父级变换、旋转、缩放都会生效）。
///
/// 与 `SpriteRenderer` 共用图集：`tile_size` 是 tileset 里单块瓦片的像素尺寸，
/// 实际列数由「纹理宽度 / `tile_size.x`」在运行时推导（和精灵表一致）。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Tilemap")]
#[serde(default)]
pub struct Tilemap {
    /// tileset 贴图路径；空 = 不绘制。
    pub tileset_path: String,
    /// tileset 里单块瓦片的像素尺寸；`[0, 0]` = 整张图一块。
    pub tile_size: [u32; 2],
    /// 每格的世界尺寸。
    pub cell_size: [f32; 2],
    /// 网格列数；`0` = 所有瓦片排成一行。
    pub columns: u32,
    /// 行优先的瓦片索引；`0` = 空格，`n > 0` = 第 `n - 1` 块。
    pub tiles: Vec<u32>,
    /// 顶点色乘算（线性 RGBA）。
    pub color: [f32; 4],
    /// 排序层，数值越大越靠前（与 `SpriteRenderer` 同一套排序键）。
    pub sorting_layer: i32,
    /// 同层内顺序。
    pub order_in_layer: i32,
}

impl Default for Tilemap {
    fn default() -> Self {
        Self {
            tileset_path: String::new(),
            tile_size: [16, 16],
            cell_size: [1.0, 1.0],
            columns: 0,
            tiles: Vec::new(),
            color: [1.0, 1.0, 1.0, 1.0],
            sorting_layer: 0,
            order_in_layer: 0,
        }
    }
}

impl Tilemap {
    /// 是否有可绘制的内容。
    pub fn has_tiles(&self) -> bool {
        !self.tileset_path.is_empty()
            && self.tiles.iter().any(|tile| *tile != 0)
            && self.cell_size[0].is_finite()
            && self.cell_size[0] > 0.0
            && self.cell_size[1].is_finite()
            && self.cell_size[1] > 0.0
    }

    /// 网格规模 `(columns, rows)`；空 `tiles` 时返回 `(1, 0)`。
    pub fn grid(&self) -> (u32, u32) {
        let columns = if self.columns > 0 {
            self.columns as usize
        } else {
            self.tiles.len().max(1)
        };
        let rows = self.tiles.len().div_ceil(columns.max(1));
        (columns.max(1) as u32, rows as u32)
    }

    /// 单块瓦片在 tileset 里的像素尺寸（`tile_size` 非法时退化为整张纹理）。
    pub fn tile_size_or(&self, texture_size: [u32; 2]) -> [u32; 2] {
        if self.tile_size[0] > 0 && self.tile_size[1] > 0 {
            self.tile_size
        } else {
            [texture_size[0].max(1), texture_size[1].max(1)]
        }
    }

    /// tileset 的列数 / 行数。
    pub fn tileset_grid(&self, texture_size: [u32; 2]) -> (u32, u32) {
        let tile = self.tile_size_or(texture_size);
        (
            (texture_size[0] / tile[0].max(1)).max(1),
            (texture_size[1] / tile[1].max(1)).max(1),
        )
    }

    /// 第 `index`（0 起）块瓦片在 tileset 里的 UV 矩形 `[u0, v0, u1, v1]`。
    pub fn tile_uv(&self, index: u32, texture_size: [u32; 2]) -> [f32; 4] {
        let tile = self.tile_size_or(texture_size);
        let (columns, rows) = self.tileset_grid(texture_size);
        let slot = index.min(columns.saturating_mul(rows).saturating_sub(1));
        let column = slot % columns;
        let row = slot / columns;
        let (width, height) = (texture_size[0].max(1) as f32, texture_size[1].max(1) as f32);
        let (tile_w, tile_h) = (tile[0].max(1) as f32, tile[1].max(1) as f32);
        let u0 = column as f32 * tile_w / width;
        let v0 = row as f32 * tile_h / height;
        [
            u0,
            v0,
            ((column as f32 + 1.0) * tile_w / width).min(1.0),
            ((row as f32 + 1.0) * tile_h / height).min(1.0),
        ]
    }

    /// 第 `row / column` 格中心的局部坐标（左下角为原点、`+Y` 向上）。
    pub fn cell_center(&self, column: u32, row: u32, rows: u32) -> [f32; 2] {
        let (cell_w, cell_h) = (self.cell_size[0], self.cell_size[1]);
        [
            (column as f32 + 0.5) * cell_w,
            (rows as f32 - row as f32 - 0.5) * cell_h,
        ]
    }
}

/// 2D 精灵帧动画 = `Animator` + `AnimationClip` 的最小替身。
///
/// 只描述「怎么播」；当前帧写回 [`SpriteRenderer::frame_index`]，由引擎的
/// `sprite_animation_system` 每帧推进（播放进度不进场景，见该系统的说明）。
///
/// 精灵表约定：单帧像素尺寸由 [`SpriteRenderer::frame_size`] 给出；一行内
/// 从左到右播放，行由 [`SpriteRenderer::sheet_row`] 选择（多行图集，#584）。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::SpriteAnimation")]
#[serde(default)]
pub struct SpriteAnimation {
    /// 总帧数；`0` = 由本行的列数推导（#584）。
    pub frames: u32,
    /// 本行内的起始帧（`0` = 从第一列开始）；用于同一行里切多个动作。
    pub frame_start: u32,
    /// 每秒帧数。
    pub fps: f32,
    /// 是否循环；`false` 时停在最后一帧。
    pub looped: bool,
    /// 是否播放（暂停时保持当前帧）。
    pub playing: bool,
    /// 相位偏移（秒），用于让多个实体不同步。
    pub phase: f32,
}

impl Default for SpriteAnimation {
    fn default() -> Self {
        Self {
            frames: 0,
            frame_start: 0,
            fps: 12.0,
            looped: true,
            playing: true,
            phase: 0.0,
        }
    }
}

/// 关键帧动画剪辑：驱动**自身** `Transform` 的 position / euler / scale。
///
/// 与 [`SpriteAnimation`]（2D 精灵帧，换 `frame_index`）并列，这里是曲线动画。
/// 播放头不放在组件里（那样会随 `.scene.json` 序列化），而是引擎侧的运行时
/// 组件 `ClipPlayhead`，见
/// `prism_engine::scene::systems::animation::keyframe_animation_system`。
///
/// 三条轨道互相独立：没有关键帧的通道保持作者写进 `transform` 的值，
/// 因此「只动画 position」不会把旋转 / 缩放清零。
///
/// 旋转的关键帧是 XYZ 欧拉角（**度**），与 [`DirectionalLight::euler_xyz`]
/// 同一约定；端点保持（不外插），所以单次播放结束时停在最后一帧。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::KeyframeAnimation")]
#[serde(default)]
pub struct KeyframeAnimation {
    /// 剪辑名（日志 / 编辑器显示用，不影响播放）。
    pub name: String,
    /// 剪辑时长（秒）；`0` = 由三条轨道里最后一个关键帧推导。
    ///
    /// 比最后一帧更长时，末帧之后保持最后一帧的值。
    pub duration: f32,
    /// 播放倍率；`0` 等于暂停，负数倒放。
    pub speed: f32,
    /// 是否循环；`false` 时播到 `duration` 停在末帧。
    pub looped: bool,
    /// 是否播放（暂停时保持当前姿势）。
    pub playing: bool,
    /// 起始时间偏移（秒）。
    pub phase: f32,
    /// 位置轨道。
    pub position: Vec<crate::animation::Vec3Key>,
    /// 旋转轨道（XYZ 欧拉角，度）。
    pub euler_xyz: Vec<crate::animation::Vec3Key>,
    /// 缩放轨道。
    pub scale: Vec<crate::animation::Vec3Key>,
    /// 剪辑事件（#490）：播放头穿过事件时间时写进 `AnimationEventQueue`。
    pub events: Vec<crate::animation::AnimationEvent>,
}

impl Default for KeyframeAnimation {
    fn default() -> Self {
        Self {
            name: "clip".into(),
            duration: 0.0,
            speed: 1.0,
            looped: true,
            playing: true,
            phase: 0.0,
            position: Vec::new(),
            euler_xyz: Vec::new(),
            scale: Vec::new(),
            events: Vec::new(),
        }
    }
}

impl KeyframeAnimation {
    /// 有效时长：`duration > 0` 用它，否则取三条轨道 / 事件里最晚的时间。
    ///
    /// 只有事件的剪辑也必须有长度，否则播放头不会前进、事件永远不触发。
    pub fn clip_duration(&self) -> f32 {
        if self.duration > 0.0 && self.duration.is_finite() {
            return self.duration;
        }
        let last_event = self
            .events
            .iter()
            .map(|event| event.time)
            .filter(|time| time.is_finite())
            .fold(0.0f32, f32::max);
        crate::animation::max_key_time(&self.position)
            .max(crate::animation::max_key_time(&self.euler_xyz))
            .max(crate::animation::max_key_time(&self.scale))
            .max(last_event)
    }

    /// 三条轨道与事件是否都为空（空剪辑不驱动任何通道，也不占播放头）。
    pub fn is_empty(&self) -> bool {
        self.position.is_empty()
            && self.euler_xyz.is_empty()
            && self.scale.is_empty()
            && self.events.is_empty()
    }

    /// 采样 `local_time`（已按循环 / 端点处理过的时间）。
    pub fn sample(&self, local_time: f32) -> crate::animation::ClipSample {
        crate::animation::ClipSample {
            position: crate::animation::sample_vec3(&self.position, local_time),
            euler_xyz: crate::animation::sample_vec3(&self.euler_xyz, local_time),
            scale: crate::animation::sample_vec3(&self.scale, local_time),
        }
    }
}

/// 折线拐点的连接方式（#604）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LineJoin {
    /// 斜接：外角延伸到两条边相交处；超过 `miter_limit` 时退化成
    /// [`Bevel`](Self::Bevel)，避免尖角拉出很长的刺。
    #[default]
    Miter,
    /// 斜切：两段各自在拐点结束，用三角形补上缺口。
    Bevel,
}

impl ReflectType for LineJoin {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Miter", "Bevel"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Miter => 0,
            Self::Bevel => 1,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Miter),
            1 => Ok(Self::Bevel),
            other => Err(ReflectError::BadData(format!(
                "invalid LineJoin discriminant {other}"
            ))),
        }
    }
}

/// 折线端点的收尾方式（#604）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LineCap {
    /// 平头：线段正好在端点结束。
    #[default]
    Butt,
    /// 方头：沿线段方向多延伸半个线宽。
    Square,
    /// 圆头：以端点为圆心的半圆。
    Round,
}

impl ReflectType for LineCap {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Butt", "Square", "Round"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Butt => 0,
            Self::Square => 1,
            Self::Round => 2,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Butt),
            1 => Ok(Self::Square),
            2 => Ok(Self::Round),
            other => Err(ReflectError::BadData(format!(
                "invalid LineCap discriminant {other}"
            ))),
        }
    }
}

/// 2D 折线渲染器 = `LineRenderer`（2D 子集）。
///
/// 点是**世界空间**的（等价 `useWorldSpace = true`），引擎把折线三角化成
/// 一条**不重叠**的加宽笔触（拐点 join + 端点 cap）并入精灵通道，因此排序 /
/// 混合与精灵完全一致，半透明折线也不会在拐点叠加两次（#604）。
///
/// 线的颜色走精灵通道的 `texture == u32::MAX`（纯色）分支，不采样贴图。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::LineRenderer")]
#[serde(default)]
pub struct LineRenderer {
    /// 世界空间折线点；少于 2 个点时不绘制。
    pub points: Vec<[f32; 3]>,
    /// 线性 RGBA 颜色。
    pub color: [f32; 4],
    /// 线宽（世界单位）。
    pub width: f32,
    /// 排序层，与 [`SpriteRenderer::sorting_layer`] 同一套数值。
    pub sorting_layer: i32,
    /// 同层内顺序。
    pub order_in_layer: i32,
    /// 拐点连接方式（默认斜接）。
    pub join: LineJoin,
    /// 端点收尾方式（默认平头）。
    pub cap: LineCap,
    /// 斜接长度上限（相对半宽的倍数）；`Miter` 超过它就退化成斜切。
    pub miter_limit: f32,
}

impl Default for LineRenderer {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            color: [1.0, 1.0, 1.0, 1.0],
            width: 0.1,
            sorting_layer: 0,
            order_in_layer: 0,
            join: LineJoin::Miter,
            cap: LineCap::Butt,
            miter_limit: 4.0,
        }
    }
}

impl LineRenderer {
    /// 是否有可绘制的线段。
    pub fn has_line(&self) -> bool {
        self.points.len() >= 2
    }
}

// ---------------------------------------------------------------------------
// Particles
// ---------------------------------------------------------------------------

/// 发射器形状 = `ParticleSystemShape`。
///
/// 只影响「粒子出生点的偏移」和「初速度方向」；Phase 1 没有碰撞/拖尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ParticleShape {
    /// 全部从发射器原点出发，方向随机。
    #[default]
    Point,
    /// 球壳内出生，方向沿径向向外。
    Sphere,
    /// 立方体内出生，方向随机。
    Box,
    /// 锥形（沿发射器本地 **+Y**），`cone_angle_degrees` 是半角。
    Cone,
}

impl ReflectType for ParticleShape {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        // 顺序与声明一致；这些字符串就是 `.scene.json` 里的取值。
        Some(&["Point", "Sphere", "Box", "Cone"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Point => 0,
            Self::Sphere => 1,
            Self::Box => 2,
            Self::Cone => 3,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Point),
            1 => Ok(Self::Sphere),
            2 => Ok(Self::Box),
            3 => Ok(Self::Cone),
            other => Err(ReflectError::BadData(format!(
                "invalid ParticleShape discriminant {other}"
            ))),
        }
    }
}

/// 世界空间 CPU 粒子发射器 = `ParticleSystem`（Phase 1 子集）。
///
/// 只描述**作者参数**；活着的粒子存在运行时组件
/// `prism_engine::scene::systems::particles::ParticleEmitter` 里，不进
/// `.scene.json` / RSCN。每个粒子出生时取一次
/// `start_speed` / `start_size` / `lifetime` 的随机值。
///
/// 颜色有两种写法：`color_stops` 非空时按生命周期**均匀采样多段渐变**（一个停靠点 =
/// 恒定色）；为空时退回 `start_color → end_color` 的两色线性插值。尺寸同理：
/// `size_over_lifetime` 是出生 / 死亡时的倍率（`[1, 0]` 收缩、`[0.5, 2]` 长大），
/// 乘在粒子出生尺寸上。
///
/// 粒子在**世界空间**模拟（发射器移动不会拖走已有的粒子），初速度方向按发射器
/// 当前的旋转换算。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::ParticleSystem")]
#[serde(default)]
pub struct ParticleSystem {
    /// 是否发射/推进；`false` 时已有粒子停在原地（不是清除）。
    pub playing: bool,
    /// 每秒发射数量。
    pub emission_rate: f32,
    /// 每个循环开始时额外一次性发射的数量。
    pub burst: u32,
    /// 发射持续时长（秒）；`0` = 一直发射（忽略 `looping`）。
    pub duration: f32,
    /// `duration > 0` 时是否循环发射。
    pub looping: bool,
    /// 同时存活的粒子上限；`0` = 内置默认（256）。
    pub max_particles: u32,
    /// 生命周期随机区间 `[min, max]`（秒）。
    pub lifetime: [f32; 2],
    /// 初始速度随机区间（世界单位 / 秒）。
    pub start_speed: [f32; 2],
    /// 初始半径随机区间（世界单位）。
    pub start_size: [f32; 2],
    /// 出生时的线性 RGBA 颜色（含 alpha）。
    pub start_color: [f32; 4],
    /// 死亡时的线性 RGBA 颜色；alpha 通常写 0 做淡出。
    pub end_color: [f32; 4],
    /// 多段颜色渐变（线性 RGBA）；非空时**覆盖** `start_color → end_color`。
    ///
    /// 停靠点在生命周期上**均匀分布**（第 0 个 = 出生，最后一个 = 死亡），相邻两点
    /// 之间线性插值；只写一个点等于恒定颜色。空 = 用两色插值（旧行为）。
    /// 检查器的列表编辑器可以直接增删 / 编辑这些 `[f32; 4]`。
    pub color_stops: Vec<[f32; 4]>,
    /// 出生 / 死亡时的尺寸倍率，乘在粒子的出生尺寸上（`[1, 1]` = 不变）。
    ///
    /// `[1, 0]` 收缩到消失，`[0.5, 2]` 逐渐变大；非有限值按 1 处理，结果不会为负。
    pub size_over_lifetime: [f32; 2],
    /// 出生 / 死亡时的**世界空间速度偏移**（单位 / 秒），按寿命线性插值后
    /// 叠加到位移上（不写入粒子速度，因此不会反过来影响重力 / 阻尼）。
    ///
    /// `[[0, 0, 0], [0, 0, 0]]` = 不变。可用来做风、上升气流、横向飘移；
    /// 非有限分量按 0 处理。
    pub velocity_over_lifetime: [[f32; 3]; 2],
    /// 恒定加速度（世界单位 / 秒²），`[0, -9.81, 0]` 是重力。
    pub gravity: [f32; 3],
    /// 速度阻尼（每秒衰减比例，`0` = 无阻尼，`1` = 一秒内速度归零）。
    pub drag: f32,
    /// 是否把粒子推出带 `Collider` 的实体表面（#501）。默认关，保持旧行为。
    pub collision: bool,
    /// 碰撞恢复系数：`0` = 贴住表面，`1` = 完全反弹。越界值会被钳到 `[0, 1]`。
    pub collision_bounce: f32,
    /// 碰撞切向摩擦：`0` = 保留全部切向速度，`1` = 切向速度归零。
    pub collision_friction: f32,
    /// 发射器形状。
    pub shape: ParticleShape,
    /// `Sphere` / `Box` 的半径（世界单位）。
    pub shape_radius: f32,
    /// `Cone` 的半角（度，0..90）。
    pub cone_angle_degrees: f32,
}

impl Default for ParticleSystem {
    fn default() -> Self {
        Self {
            playing: true,
            emission_rate: 20.0,
            burst: 0,
            duration: 0.0,
            looping: true,
            max_particles: 0,
            lifetime: [0.8, 1.4],
            start_speed: [0.6, 1.6],
            start_size: [0.12, 0.28],
            start_color: [1.0, 0.85, 0.35, 1.0],
            end_color: [1.0, 0.25, 0.05, 0.0],
            color_stops: Vec::new(),
            size_over_lifetime: [1.0, 1.0],
            velocity_over_lifetime: [[0.0; 3]; 2],
            gravity: [0.0, 1.2, 0.0],
            drag: 0.6,
            collision: false,
            collision_bounce: 0.3,
            collision_friction: 0.2,
            shape: ParticleShape::Cone,
            shape_radius: 0.15,
            cone_angle_degrees: 20.0,
        }
    }
}

impl ParticleSystem {
    /// 同时存活粒子上限（`max_particles == 0` 时回落到 256）。
    pub fn particle_cap(&self) -> usize {
        if self.max_particles == 0 {
            256
        } else {
            self.max_particles as usize
        }
    }

    /// 发射时长（秒）；`0` / 非有限值表示无限。
    pub fn emission_duration(&self) -> Option<f32> {
        if self.duration.is_finite() && self.duration > 0.0 {
            Some(self.duration)
        } else {
            None
        }
    }

    /// 每秒发射数量（非法值按 0，负值不发射）。
    pub fn rate(&self) -> f32 {
        if self.emission_rate.is_finite() && self.emission_rate > 0.0 {
            self.emission_rate
        } else {
            0.0
        }
    }

    /// 区间取值（非法/倒序时用 `fallback`，并把两端排序）。
    pub fn random_range(range: [f32; 2], t: f32, fallback: f32) -> f32 {
        let (lo, hi) = if range[0].is_finite() && range[1].is_finite() {
            if range[0] <= range[1] {
                (range[0], range[1])
            } else {
                (range[1], range[0])
            }
        } else {
            (fallback, fallback)
        };
        lo + (hi - lo) * t
    }
}

/// 2D 发射器形状（`ParticleEmitter2D`）。
///
/// 只影响粒子出生点在发射器本地 XY 平面里的偏移；初速度方向由
/// `direction` + `spread_degrees` 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ParticleShape2D {
    /// 全部从发射器原点出发。
    #[default]
    Point,
    /// 沿本地 X 轴的一条线段（`shape_size[0]` 是半长）。
    Line,
    /// 本地矩形（`shape_size` 是半宽 / 半高）。
    Box,
    /// 本地圆盘（`shape_radius`）。
    Circle,
}

impl ReflectType for ParticleShape2D {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Point", "Line", "Box", "Circle"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Point => 0,
            Self::Line => 1,
            Self::Box => 2,
            Self::Circle => 3,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Point),
            1 => Ok(Self::Line),
            2 => Ok(Self::Box),
            3 => Ok(Self::Circle),
            other => Err(ReflectError::BadData(format!(
                "invalid ParticleShape2D discriminant {other}"
            ))),
        }
    }
}

/// 2D 世界空间粒子发射器（#594）。
///
/// 和 3D 的 [`ParticleSystem`] 是同一套思路：这里只描述**作者参数**，活着的
/// 粒子存在运行时组件
/// `prism_engine::scene::systems::particles2d::ParticleEmitter2DState` 里，
/// 不进 `.scene.json` / RSCN。粒子在**世界 XY 平面**积分（发射器移动不会
/// 拖走已飞出的粒子），初速度方向按发射器当前的 Z 轴旋转换算。
///
/// 绘制走 2D 精灵通道（`SpriteDrawItem`）：每个粒子是一个纯色四边形，
/// 和精灵一起按 `sorting_layer → order_in_layer → z` 排序，因此水花可以夹在
/// 船和水面之间，而不需要单独的渲染 pass。
///
/// 常见的雨 / 雪 / 水花不需要新的着色器，只是参数不同：
///
/// ```json
/// "prism_engine::scene::ParticleEmitter2D": {
///   "emission_rate": 240, "lifetime": [1.2, 1.8],
///   "start_speed": [9.0, 12.0], "start_size": [0.04, 0.06],
///   "direction": [0, -1], "spread_degrees": 6,
///   "shape": "Box", "shape_size": [12, 0.5],
///   "gravity": [0, -2.0], "drag": 0.0,
///   "start_color": [0.75, 0.85, 1.0, 0.65],
///   "end_color": [0.75, 0.85, 1.0, 0.25]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::ParticleEmitter2D")]
#[serde(default)]
pub struct ParticleEmitter2D {
    /// 是否发射/推进；`false` 时已有粒子停在原地（不是清除）。
    pub playing: bool,
    /// 每秒发射数量。
    pub emission_rate: f32,
    /// 每个发射循环开始时额外一次性发射的数量。
    pub burst: u32,
    /// 发射持续时长（秒）；`0` = 一直发射（忽略 `looping`）。
    pub duration: f32,
    /// `duration > 0` 时是否循环发射。
    pub looping: bool,
    /// 同时存活的粒子上限；`0` = 内置默认（256）。
    pub max_particles: u32,
    /// 生命周期随机区间 `[min, max]`（秒）。
    pub lifetime: [f32; 2],
    /// 初始速度随机区间（世界单位 / 秒）。
    pub start_speed: [f32; 2],
    /// 初始边长随机区间（世界单位；四边形是正方形）。
    pub start_size: [f32; 2],
    /// 出生时的 RGBA 颜色（含 alpha）。
    pub start_color: [f32; 4],
    /// 死亡时的 RGBA 颜色；alpha 通常写 0 做淡出。
    pub end_color: [f32; 4],
    /// 恒定加速度（世界单位 / 秒²），`[0, -9.8]` 是重力。
    pub gravity: [f32; 2],
    /// 速度阻尼（每秒衰减比例，`0` = 无阻尼，`1` = 一秒内速度归零）。
    pub drag: f32,
    /// 出生点形状。
    pub shape: ParticleShape2D,
    /// `Line` / `Box` 的半尺寸（世界单位）；`Line` 只用 `[0]`。
    pub shape_size: [f32; 2],
    /// `Circle` 的半径（世界单位）。
    pub shape_radius: f32,
    /// 初速度基准方向（本地空间，不必归一化）；零向量时用 `[0, -1]`。
    pub direction: [f32; 2],
    /// 相对 `direction` 的随机半角（度，0 = 全部沿同一方向）。
    pub spread_degrees: f32,
    /// 与其它 2D 绘制项一起排序的层。
    pub sorting_layer: i32,
    /// 同层内的顺序。
    pub order_in_layer: i32,
}

impl Default for ParticleEmitter2D {
    fn default() -> Self {
        Self {
            playing: true,
            emission_rate: 20.0,
            burst: 0,
            duration: 0.0,
            looping: true,
            max_particles: 0,
            lifetime: [0.6, 1.2],
            start_speed: [1.0, 2.5],
            start_size: [0.05, 0.12],
            start_color: [1.0, 0.92, 0.7, 1.0],
            end_color: [1.0, 0.55, 0.2, 0.0],
            gravity: [0.0, -3.0],
            drag: 0.4,
            shape: ParticleShape2D::Point,
            shape_size: [0.5, 0.5],
            shape_radius: 0.25,
            direction: [0.0, -1.0],
            spread_degrees: 15.0,
            sorting_layer: 0,
            order_in_layer: 0,
        }
    }
}

impl ParticleEmitter2D {
    /// 同时存活粒子上限（`max_particles == 0` 时回落到 256）。
    pub fn particle_cap(&self) -> usize {
        if self.max_particles == 0 {
            256
        } else {
            self.max_particles as usize
        }
    }

    /// 发射时长（秒）；`0` / 非有限值表示无限。
    pub fn emission_duration(&self) -> Option<f32> {
        if self.duration.is_finite() && self.duration > 0.0 {
            Some(self.duration)
        } else {
            None
        }
    }

    /// 每秒发射数量（非法值按 0，负值不发射）。
    pub fn rate(&self) -> f32 {
        if self.emission_rate.is_finite() && self.emission_rate > 0.0 {
            self.emission_rate
        } else {
            0.0
        }
    }

    /// 归一化的基准方向；零向量 / 非有限值时回落到「向下」。
    pub fn direction_or_down(&self) -> [f32; 2] {
        let length =
            (self.direction[0] * self.direction[0] + self.direction[1] * self.direction[1]).sqrt();
        if length.is_finite() && length > 1e-6 {
            [self.direction[0] / length, self.direction[1] / length]
        } else {
            [0.0, -1.0]
        }
    }
}

// ---------------------------------------------------------------------------
// Lighting
// ---------------------------------------------------------------------------

/// 2D 光源类型（#611）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Light2DKind {
    /// 均匀照亮整幅画面（夜晚的底色）。`intensity` 直接乘到颜色上。
    Ambient,
    /// 圆形衰减：半径内按 `(1 - d / radius)^2` 衰减。
    #[default]
    Point,
    /// 平行光：没有位置 / 衰减，方向上不做阴影——2D 精灵没有法线，
    /// 只能整体乘一个颜色，用来做「夕阳把画面染成橙色」。
    Directional,
}

impl ReflectType for Light2DKind {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Ambient", "Point", "Directional"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Ambient => 0,
            Self::Point => 1,
            Self::Directional => 2,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Ambient),
            1 => Ok(Self::Point),
            2 => Ok(Self::Directional),
            other => Err(ReflectError::BadData(format!(
                "invalid Light2DKind discriminant {other}"
            ))),
        }
    }
}

/// 2D 光源（#611）：照亮打开了 [`SpriteRenderer::receive_lights`] 的精灵。
///
/// 与 3D 的 [`PointLight`] / [`DirectionalLight`] 是**两套单位**：3D 用坎德拉
/// / lux，2D 这里 `intensity = 1.0` 就是「原色」，方便作者按画面调。
///
/// 2D 精灵没有法线，所以点光只做径向衰减（不做兰伯特），方向光对整个精灵
/// 乘一个颜色。软阴影 / 光斑贴图留到后续（#611 Phase 2）。
///
/// 用法（夜晚灯笼）：
///
/// ```json
/// "prism_engine::scene::Light2D": {
///   "kind": "Point", "color": [1.0, 0.75, 0.45], "intensity": 1.2, "radius": 6.0
/// }
/// ```
///
/// 再放一盏 `kind = "Ambient"`, `color = [0.18, 0.2, 0.32]`, `intensity = 1.0`
/// 做夜色底子；没有环境光时，半径外的精灵会变黑（这是 2D 光照的正常结果）。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Light2D")]
#[serde(default)]
pub struct Light2D {
    /// 光源类型（默认点光）。
    pub kind: Light2DKind,
    /// 线性 RGB 颜色（`intensity = 1` 时乘到精灵上的比例）。
    pub color: [f32; 3],
    /// 强度倍数；`0` = 关灯，负值按 0 处理。
    pub intensity: f32,
    /// 点光半径（世界单位）；`<= 0` / 非有限值时点光退化成「全画面」，
    /// 便于调试时先看到效果。
    pub radius: f32,
    /// 方向光的照射方向（世界 XY，归一化前）。2D 没有法线，这个值只用于
    /// 后续的方向性衰减（Phase 2），当前不影响结果。
    pub direction: [f32; 2],
    /// 是否参与光照。
    pub enabled: bool,
}

impl Default for Light2D {
    fn default() -> Self {
        Self {
            kind: Light2DKind::Point,
            color: [1.0, 1.0, 1.0],
            intensity: 1.0,
            radius: 5.0,
            direction: [0.0, -1.0],
            enabled: true,
        }
    }
}

impl Light2D {
    /// 是否会给 `receive_lights` 的精灵增加亮度（关闭 / 强度非正 → `false`）。
    pub fn contributes(&self) -> bool {
        self.enabled
            && self.intensity.is_finite()
            && self.intensity > 0.0
            && self.color.iter().any(|channel| *channel > 0.0)
    }

    /// 有效的强度（非有限 / 负值按 0）。
    pub fn strength(&self) -> f32 {
        if self.intensity.is_finite() && self.intensity > 0.0 {
            self.intensity
        } else {
            0.0
        }
    }

    /// 有效的半径；`0` 表示「不衰减 / 全画面」。
    pub fn falloff_radius(&self) -> f32 {
        if self.radius.is_finite() && self.radius > 0.0 {
            self.radius
        } else {
            0.0
        }
    }
}

/// Directional 无限 光源
///
/// Orientation is stored as XYZ Euler angles 角度 so it round-trips
/// cleanly through `scene_state.json`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::DirectionalLight")]
#[serde(default)]
pub struct DirectionalLight {
    /// XYZ Euler angles 角度 音高 (X), yaw (Y), roll (Z).
    pub euler_xyz: glam::Vec3,
    /// 线性 RGB 颜色 typically `[0, 1]`.
    pub color: glam::Vec3,
    /// Illuminance in **lux** 物理 unit).
    pub intensity: f32,
    /// IBL ambient factor.
    pub ambient: f32,
}

impl Default for DirectionalLight {
    fn default() -> Self {
        Self {
            euler_xyz: glam::Vec3::new(45.0, -45.0, 0.0),
            color: glam::Vec3::ONE,
            intensity: 100_000.0,
            ambient: 1.0,
        }
    }
}

/// Point 光源
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::PointLight")]
#[serde(default)]
pub struct PointLight {
    /// 线性 RGB 颜色
    pub color: glam::Vec3,
    /// Luminous intensity in **candela**.
    pub intensity: f32,
    /// Attenuation 半径 世界 units).
    pub range: f32,
}

impl Default for PointLight {
    fn default() -> Self {
        Self {
            color: glam::Vec3::ONE,
            intensity: 100.0,
            range: 12.0,
        }
    }
}

/// Spot 光源
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::SpotLight")]
#[serde(default)]
pub struct SpotLight {
    pub color: glam::Vec3,
    pub intensity: f32,
    pub range: f32,
    /// Inner cone half-angle 弧度
    pub inner_cone_angle: f32,
    /// Outer cone half-angle 弧度
    pub outer_cone_angle: f32,
}

impl Default for SpotLight {
    fn default() -> Self {
        Self {
            color: glam::Vec3::ONE,
            intensity: 100.0,
            range: 20.0,
            inner_cone_angle: 0.436, // ~25°
            outer_cone_angle: 0.873, // ~50°
        }
    }
}

/// 统一光源组件。
///
/// 只有一个 `Light` 组件，用 [`LightType`] 区分方向光 / 点光 / 聚光；光源的
/// **方向由实体变换决定**（里是 `light.transform.forward`），位置由
/// `LocalToWorld` 决定。本组件就是那个形状。
///
/// 旧的 [`DirectionalLight`] / [`PointLight`] / [`SpotLight`] 仍然可用（渲染器两者
/// 都收，见 `prism-engine::scene::systems::lights`），所以老场景不需要迁移；
/// 新场景建议只用本组件。
///
/// 单位与旧组件一致，采用**物理量**（方向光 lux、点光/聚光 candela），而不是
/// 默认的归一化 `intensity = 1`：渲染器内部按 `LUX_TO_RADIANCE_SCALE` 折算，改
/// `intensity` 的观感与旧组件相同。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Light")]
#[serde(default)]
pub struct Light {
    /// `Behaviour.enabled`：为 `false` 时渲染器跳过该光源。
    pub enabled: bool,
    /// 光源类型 = `Light.type`。
    pub light_type: LightType,
    /// 线性 RGB 颜色 = `Light.color`。
    pub color: glam::Vec3,
    /// 强度（方向光 lux / 点光与聚光 candela）= `Light.intensity`。
    pub intensity: f32,
    /// 衰减半径（世界单位）= `Light.range`（方向光忽略）。
    pub range: f32,
    /// 聚光**全锥角**（度）= `Light.spotAngle`。
    pub spot_angle: f32,
    /// 内锥**全锥角**（度）= `Light.innerSpotAngle`；小于 `spot_angle`。
    pub inner_spot_angle: f32,
    /// 阴影模式 = `Light.shadows`。
    pub shadows: LightShadows,
    /// 阴影强度 = `Light.shadowStrength`（`0..=1`）。
    pub shadow_strength: f32,
    /// 色温（开尔文）= `Light.colorTemperature`。
    pub color_temperature: f32,
    /// 是否让色温参与光源颜色 = `Light.useColorTemperature`。
    pub use_color_temperature: bool,
    /// 渲染模式 = `Light.renderMode`。
    pub render_mode: LightRenderMode,
}

impl Default for Light {
    fn default() -> Self {
        Self {
            enabled: true,
            // 引擎的 `new Light()` 默认是点光源。
            light_type: LightType::Point,
            color: glam::Vec3::ONE,
            intensity: 100.0,
            range: 10.0,
            spot_angle: 30.0,
            inner_spot_angle: 21.8,
            shadows: LightShadows::None,
            shadow_strength: 1.0,
            color_temperature: 6570.0,
            use_color_temperature: false,
            render_mode: LightRenderMode::Auto,
        }
    }
}

impl Light {
    /// 最终光源颜色 = `color`（× 色温滤色片）。
    ///
    /// 对应引擎的 `ExtractColorTemperature` + `ApplyColorTemperature`：
    /// 只有 `use_color_temperature` 为真时才乘色温，且不归一化。
    pub fn effective_color(&self) -> glam::Vec3 {
        if self.use_color_temperature {
            self.color * correlated_color_temperature_to_rgb(self.color_temperature)
        } else {
            self.color
        }
    }

    /// 聚光的内 / 外**半锥角**（弧度），供 GPU 与着色器使用。
    ///
    /// 引擎的 `spotAngle` / `innerSpotAngle` 都是全锥角（度）；这里除以 2 转成
    /// 半角，并把内角裁到 `[0, 外角]`，避免非法配置产生负的余弦区间。
    pub fn cone_half_angles_radians(&self) -> (f32, f32) {
        let outer = (self.spot_angle.max(0.0) * 0.5).to_radians();
        let inner = (self.inner_spot_angle.max(0.0) * 0.5).to_radians();
        (inner.min(outer), outer)
    }

    /// 是否投阴影（启用 + `shadows != None`）。
    pub fn is_shadow_caster(&self) -> bool {
        self.enabled && self.shadows.casts_shadows()
    }
}

/// 场景环境光照（IBL）声明式组件。
///
/// 引擎采用「场景声明式光照」：只有当场景显式携带此组件且 `env_map` 为 `Some` 时，
/// 渲染器才在场景加载时构建 IBL 预卷积（CPU 卷积，约数 ms）。开屏等无 3D 光照
/// 需求的场景不写此组件（或 `env_map = None`）→ 渲染器以廉价中性空 IBL 启动，
/// 秒开、不加载 3D 环境贴图。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::EnvironmentLighting")]
#[serde(default)]
pub struct EnvironmentLighting {
    /// 环境贴图（等距柱状 HDR）相对场景文件的路径。`None` 或缺失 = 不构建 IBL。
    pub env_map: Option<String>,
}

/// 场景全局光照（GI probe 音量）声明式组件。
///
/// 仅当 `enabled = true` 且存在烘焙好的体积数据时，渲染器才上传真实 GI probe
/// 音量；否则保留免费的 synthetic sky 预留（仅让 set5 描述符 合法）。无 3D 光照
/// 需求的场景（如开屏）不写此组件。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::GiConfig")]
#[serde(default)]
pub struct GiConfig {
    /// 是否启用真实 GI（替换 synthetic sky 预留）。
    pub enabled: bool,
    /// 烘焙 GI 体积数据相对场景文件的路径（可选）。
    pub volume: Option<String>,
}

// ---------------------------------------------------------------------------
// 相机
// ---------------------------------------------------------------------------

/// 透视 相机 parameters (data 分量
///
/// Holds the editor-editable 投影 + exposure fields. The 运行时 视图
/// 矩阵 is computed each 帧 by [`super::systems::camera`] from this
/// 分量 plus a sibling [`FlyCameraController`] (or future controller)
/// and the entity's [`LocalToWorld`].
///
/// 宽高比 is a 运行时 cache written by the app on 调整大小 it is exposed in
/// 检查器视为只读。启用标志控制渲染器是否拾取
/// this 相机
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Camera")]
#[serde(default)]
pub struct Camera {
    pub fov_y_degrees: f32,
    pub near: f32,
    pub far: f32,
    /// Exposure multiplier applied to the final 高动态范围 颜色 before tonemapping.
    /// 默认 1.0 = no scaling; range [0, 5] via 检查器 滑动条
    pub exposure: f32,
    /// 当前 宽高比 比率 宽度 / 高度 Written by the app on 调整大小 /
    /// orientation change; the 投影 矩阵 is derived from it each 帧
    #[reflect(read_only)]
    pub aspect: f32,
    /// When `false` the 相机 is skipped during scene 集合 (the
    /// 渲染器 falls 后 to the 下一个 available 相机
    pub enabled: bool,
    /// 清屏方式 = `Camera.clearFlags`（[`CameraClearFlags`]）。
    pub clear_flags: CameraClearFlags,
    /// 纯色背景 = `Camera.backgroundColor`（线性 RGBA，`SolidColor` 时生效）。
    pub background_color: [f32; 4],
    /// 正交投影 = `Camera.orthographic`。
    pub orthographic: bool,
    /// 正交视口半高（世界单位）= `Camera.orthographicSize`。
    pub orthographic_size: f32,
    /// 渲染顺序 = `Camera.depth`；数值大的后绘制，`main_camera` 取最大者。
    pub depth: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            fov_y_degrees: 60.0,
            near: 0.1,
            far: 1000.0,
            exposure: 1.0,
            aspect: 16.0 / 9.0,
            enabled: true,
            clear_flags: CameraClearFlags::Skybox,
            background_color: [0.5, 0.5, 0.5, 1.0],
            orthographic: false,
            orthographic_size: 5.0,
            depth: 0.0,
        }
    }
}

impl Camera {
    /// 投影矩阵（列主序）= `Camera.projectionMatrix`。
    ///
    /// 右手系、相机看向 −Z、Vulkan y-flip（`p[1][1] = -inv_tan`）、
    /// 深度范围 `[0, 1]`。`aspect` 由调用方给出（运行时来自 framebuffer，
    /// 编辑器里来自面板），非有限或非正时退化为 1.0。
    ///
    /// `orthographic` 为真时用 [`Camera::orthographic_size`]（半高，世界单位）。
    pub fn projection_matrix(&self, aspect: f32) -> glam::Mat4 {
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let near = self.near;
        let far = self.far;
        if self.orthographic {
            let half_h = self.orthographic_size.max(1e-4);
            let half_w = half_h * aspect;
            glam::Mat4::from_cols(
                glam::vec4(1.0 / half_w, 0.0, 0.0, 0.0),
                glam::vec4(0.0, -1.0 / half_h, 0.0, 0.0),
                glam::vec4(0.0, 0.0, 1.0 / (near - far), 0.0),
                glam::vec4(0.0, 0.0, near / (near - far), 1.0),
            )
        } else {
            let inv_tan = 1.0 / (self.fov_y_degrees.to_radians() * 0.5).tan();
            glam::Mat4::from_cols(
                glam::vec4(inv_tan / aspect, 0.0, 0.0, 0.0),
                glam::vec4(0.0, -inv_tan, 0.0, 0.0),
                glam::vec4(0.0, 0.0, far / (near - far), -1.0),
                glam::vec4(0.0, 0.0, near * far / (near - far), 0.0),
            )
        }
    }

    /// 世界 → 相机空间 = `Camera.worldToCameraMatrix`（即 view 矩阵）。
    pub fn world_to_camera_matrix(camera_to_world: glam::Mat4) -> glam::Mat4 {
        camera_to_world.inverse()
    }

    /// 相机 → 世界空间 = `Camera.cameraToWorldMatrix`。
    pub fn camera_to_world_matrix(world_to_camera: glam::Mat4) -> glam::Mat4 {
        world_to_camera.inverse()
    }
}

/// Free-fly 相机 输入 controller (data 分量
///
/// Splits the runtime/input-owned fields off the old `FlyCamera` 枚举 variant:
/// `yaw`/`pitch` are written each 帧 by [`super::systems::camera`] from
/// 输入 while `move_speed`/`look_sensitivity` are editor-editable. The
/// 相机 position lives on the sibling [`LocalTransform`] (and its derived
/// [`LocalToWorld`]), so this 分量 carries no position of its own.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::FlyCameraController")]
#[serde(default)]
pub struct FlyCameraController {
    /// Yaw around +Y (rad). 0 = looking 下 -Z (matches `FlyCamera`).
    pub yaw: f32,
    /// 音高 above/below the horizon (rad). 0 = 水平
    pub pitch: f32,
    /// Base 平移 speed 世界 units / 秒 at boost = 1.
    pub move_speed: f32,
    /// 鼠标 look sensitivity (rad per 像素
    pub look_sensitivity: f32,
}

impl Default for FlyCameraController {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            move_speed: 5.0,
            look_sensitivity: 0.005,
        }
    }
}

// ---------------------------------------------------------------------------
// Prefab instances (editor metadata)
// ---------------------------------------------------------------------------

/// **编辑器元数据**：该实体（子树根）是从哪个预制体实例化来的。
///
/// 引擎运行时对它没有任何行为（没有 system 查询它），但它是普通场景组件，
/// 因此会随场景一起 cook/加载 —— 这样编辑器才能做到「反向链接 / 还原 / 应用到预制体 /
/// 同步同源实例」，而不需要额外的 sidecar 文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::PrefabInstance")]
#[serde(default)]
pub struct PrefabInstance {
    /// 预制体路径（**相对项目根**，`/` 分隔），如 `assets/prefabs/door.prefab.json`。
    pub prefab: String,
}

// ---------------------------------------------------------------------------
// Identification
// ---------------------------------------------------------------------------

/// Human-readable 实体 name (data 分量
///
/// Optional: entities without a `Name` are displayed in the 检查器 by their
/// raw id. Populated by the scene loader from the cooked `.rscn` name field;
/// editable from the 检查器 at 运行时 (not persisted to the scene file).
#[derive(Debug, Clone, Default, Reflect)]
#[reflect(name = "prism_engine::scene::Name")]
pub struct Name(pub String);

// ---------------------------------------------------------------------------
// Skybox
// ---------------------------------------------------------------------------

/// Skybox / environment 映射表 分量
///
/// The entity with this component owns the environment map for the scene:
/// its HDR drives both image-based lighting (IBL) and the background sky.
/// There should be at most one enabled skybox entity per scene; the first
/// enabled one wins.
///
/// The map is rendered as an infinitely distant background; the component's
/// rotation/intensity are artist-facing controls rather than a world
/// transform.
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Skybox")]
#[serde(default)]
pub struct Skybox {
    /// Optional stable resource id resolved by the cooker (legacy path).
    pub env_asset: SceneAssetId,
    /// Authoring HDR path, relative to the scene file (or `assets/...`).
    pub hdr_path: String,
    /// When `false` the environment is ignored: no IBL and no sky draw.
    pub enabled: bool,
    /// Background yaw rotation in degrees.
    pub rotation_degrees: f32,
    /// Background brightness multiplier (1 = HDR as-is).
    pub intensity: f32,
}

impl Default for Skybox {
    fn default() -> Self {
        Self {
            env_asset: SceneAssetId::from_raw(0),
            hdr_path: String::new(),
            enabled: true,
            rotation_degrees: 0.0,
            intensity: 1.0,
        }
    }
}

/// World-space XYZ axis gizmo owned by an entity.
///
/// 控制手柄显示模式（平移 / 旋转 / 缩放）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum GizmoMode {
    #[default]
    Translate,
    Rotate,
    Scale,
}

impl prism_reflect::ReflectType for GizmoMode {
    fn field_type() -> prism_reflect::FieldType {
        prism_reflect::FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Translate", "Rotate", "Scale"])
    }

    fn to_value(&self) -> prism_reflect::Value {
        prism_reflect::Value::U64(match self {
            Self::Translate => 0,
            Self::Rotate => 1,
            Self::Scale => 2,
        })
    }

    fn from_value(value: prism_reflect::Value) -> Result<Self, prism_reflect::ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Translate),
            1 => Ok(Self::Rotate),
            2 => Ok(Self::Scale),
            other => Err(prism_reflect::ReflectError::BadData(format!(
                "invalid GizmoMode discriminant {other}"
            ))),
        }
    }
}

/// The gizmo uses the entity's `LocalToWorld` (position/rotation/scale)
/// multiplied by [`Gizmo::scale`], so it can be placed and oriented like any
/// other scene object. No `Gizmo` component means no gizmo is drawn.
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Gizmo")]
#[serde(default)]
pub struct Gizmo {
    pub enabled: bool,
    /// Additional uniform scale applied on top of the entity transform.
    pub scale: f32,
    /// 显示手柄模式（平移箭头、旋转圆环、缩放手柄）。
    pub mode: GizmoMode,
}

impl Default for Gizmo {
    fn default() -> Self {
        Self {
            enabled: true,
            scale: 1.0,
            mode: GizmoMode::Translate,
        }
    }
}

// ---------------------------------------------------------------------------
// 物理
// ---------------------------------------------------------------------------

/// 碰撞体形状 = `ColliderShape`。
///
/// 尺寸都是**本地空间**的（再乘实体缩放）；Phase 1 只做形状/射线查询，
/// 还没有真正的刚体积分（见 #522 的后续步骤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ColliderShape {
    /// 轴对齐盒：`half_extents` 是三个方向上的半尺寸。
    #[default]
    Box,
    /// 球：`radius` 是半径。
    Sphere,
    /// 胶囊：沿本地 **+Y**，`radius` + `half_height`（圆柱段半高）。
    Capsule,
}

impl ReflectType for ColliderShape {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Box", "Sphere", "Capsule"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Box => 0,
            Self::Sphere => 1,
            Self::Capsule => 2,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Box),
            1 => Ok(Self::Sphere),
            2 => Ok(Self::Capsule),
            other => Err(ReflectError::BadData(format!(
                "invalid ColliderShape discriminant {other}"
            ))),
        }
    }
}

/// 刚体类型 = `BodyType`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BodyType {
    /// 不动，只参与碰撞/查询（地板、墙）。
    #[default]
    Static,
    /// 由动画/代码移动，推开动态物体，自己不被推。
    Kinematic,
    /// 受重力与碰撞影响。
    Dynamic,
}

impl ReflectType for BodyType {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Static", "Kinematic", "Dynamic"])
    }

    fn to_value(&self) -> Value {
        Value::U64(match self {
            Self::Static => 0,
            Self::Kinematic => 1,
            Self::Dynamic => 2,
        })
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Static),
            1 => Ok(Self::Kinematic),
            2 => Ok(Self::Dynamic),
            other => Err(ReflectError::BadData(format!(
                "invalid BodyType discriminant {other}"
            ))),
        }
    }
}

/// 碰撞体 = `Collider`。
///
/// 形状由 [`ColliderShape`] 决定，具体尺寸用下面三个字段（彼此独立，按形状取用）：
/// 盒用 `half_extents`，球用 `radius`，胶囊用 `radius` + `half_height`。
/// 所有尺寸都在**本地空间**，世界空间大小再乘实体缩放。
///
/// `#522` 已落地「可创作 + 射线查询 + rapier 刚体积分」：场景/编辑器里能加、
/// 能改，引擎的 `physics::raycast` 按真实形状求交；触发器回调与碰撞网格仍待接线。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Collider")]
#[serde(default)]
pub struct Collider {
    pub shape: ColliderShape,
    /// 形状中心相对实体原点的偏移（本地空间）。
    pub center: [f32; 3],
    /// 盒的三个半尺寸。
    pub half_extents: [f32; 3],
    /// 球 / 胶囊半径。
    pub radius: f32,
    /// 胶囊圆柱段的半高（不含两端半球）。
    pub half_height: f32,
    /// 触发器：参与查询但**不阻挡**刚体（rapier sensor 事件尚未接回调）。
    pub is_trigger: bool,
    pub friction: f32,
    pub restitution: f32,
    /// 查询用的层掩码位；`raycast` 只命中与掩码相与非 0 的碰撞体。
    pub layer: u32,
}

impl Default for Collider {
    fn default() -> Self {
        Self {
            shape: ColliderShape::Box,
            center: [0.0, 0.0, 0.0],
            half_extents: [0.5, 0.5, 0.5],
            radius: 0.5,
            half_height: 0.5,
            is_trigger: false,
            friction: 0.5,
            restitution: 0.0,
            layer: 1,
        }
    }
}

/// 刚体 = `Rigidbody`。
///
/// 真正的积分/求解由 `prism-app` 的 rapier 线程完成（#522），支持父子层级；
/// 有 `Collider` 没有 `Rigidbody` 的实体按静态障碍处理。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Rigidbody")]
#[serde(default)]
pub struct Rigidbody {
    pub body_type: BodyType,
    /// 质量（公斤）；`Dynamic` 才有意义。
    pub mass: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub use_gravity: bool,
}

impl Default for Rigidbody {
    fn default() -> Self {
        Self {
            body_type: BodyType::Dynamic,
            mass: 1.0,
            linear_damping: 0.0,
            angular_damping: 0.05,
            use_gravity: true,
        }
    }
}

// ---------------------------------------------------------------------------
// 蒙皮
// ---------------------------------------------------------------------------

/// 骨架根（#560）：指向一份 `.rskel`，是这个实体子树里所有
/// [`SkinnedMeshRenderer`] 共用的骨骼 + 参考姿态。
///
/// 骨骼的世界矩阵由 `prism-anim` 在运行时从参考姿态 + 动画剪辑求出来，
/// 场景文件里只存路径，不存任何逐骨骼数据。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::SkeletonRoot")]
#[serde(default)]
#[derive(Default)]
pub struct SkeletonRoot {
    /// `.rskel` 路径（相对 `assets/`）。
    pub skeleton_path: String,
    /// 默认动画剪辑路径（`.ranim`，可空：由代码/编辑器决定播哪条）。
    pub default_clip_path: String,
}

impl SkeletonRoot {
    pub fn has_skeleton(&self) -> bool {
        !self.skeleton_path.trim().is_empty()
    }
}

/// 蒙皮网格渲染器（#560）。
///
/// 与 [`MeshRenderer`] 并列而不是扩展它：`.rskin` 的顶点带着 joints/weights，
/// 上传路径和普通 `.rmes` 不同，GPU 蒙皮（P0-3）也要走单独的管线分支。
/// 材质、阴影、剔除这些「怎么画」的设置照抄 [`MeshRenderer`] 的语义。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::SkinnedMeshRenderer")]
#[serde(default)]
#[derive(Default)]
pub struct SkinnedMeshRenderer {
    /// `.rskin` 路径（相对 `assets/`）。
    pub skinned_mesh_path: String,
    /// 每个 submesh 一个材质路径；为空时用 `material_path`。
    pub material_paths: Vec<String>,
    /// 单材质回退路径。
    pub material_path: String,
    /// 阴影投射模式（与 [`MeshRenderer`] 同义）。
    pub shadow_casting_mode: ShadowCastingMode,
}

impl SkinnedMeshRenderer {
    pub fn has_mesh(&self) -> bool {
        !self.skinned_mesh_path.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// 蒙皮动画
// ---------------------------------------------------------------------------

/// 骨架的播放控制（#560 P0-2）。
///
/// 事件源是 cooked 的 `.ranim` 剪辑；骨架由同一实体上的 [`SkeletonRoot`] 指定。
/// `clip_path` 为空时用 `SkeletonRoot::default_clip_path`。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Animator")]
#[serde(default)]
pub struct Animator {
    /// 动画剪辑路径（`.ranim`，相对 `assets/`）。空 = 用骨架的默认剪辑。
    pub clip_path: String,
    /// 起始时间（秒）；编辑器 scrub 直接写这里。
    pub time: f32,
    /// 播放速度倍数（负值倒放）。
    pub speed: f32,
    /// 循环播放。
    pub looped: bool,
    /// 是否推进时间。
    pub playing: bool,
}

impl Default for Animator {
    fn default() -> Self {
        Self {
            clip_path: String::new(),
            time: 0.0,
            speed: 1.0,
            looped: true,
            playing: true,
        }
    }
}

impl Animator {
    pub fn has_clip(&self) -> bool {
        !self.clip_path.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// 地形
// ---------------------------------------------------------------------------

/// 高度场地形（#517 Phase 1）。
///
/// 只是一个数据组件：运行时按 `heightmap`（f32 little-endian 高度数组）+ 网格参数生成
/// 地形网格，挂到同一实体的 [`MeshRenderer`] 上；材质仍然由 MeshRenderer 决定。
///
/// 高度数组是行优先的 `resolution × resolution` 个 f32（`resolution = 0` 按 32 处理），
/// 世界尺寸是 `size` 米（x / z），`height_scale` 把 `[0, 1]` 的高度放大成米。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::Terrain")]
#[serde(default)]
pub struct Terrain {
    /// `.raw` 高度图路径（相对 `assets/`，与其它资产路径一样解析）。
    pub heightmap: String,
    /// 水平尺寸（米）：`[x, z]`。
    pub size: [f32; 2],
    /// 高度倍数：网格高度 = 高度图数值 × `height_scale`（米）。
    ///
    /// heightmap CLI 写出的 `.raw` 已经是米，所以这里的默认值是 `1.0`；
    /// 归一化到 `[0, 1]` 的高度场可以用它（例如 `100.0`）换算成米。
    pub height_scale: f32,
    /// 每边格子数。`0` = 跟随高度图大小（样本数必须是完全平方数）。
    ///
    /// 非 `0` 时高度数组长度必须正好是 `resolution * resolution`，否则退回平坦地形并告警。
    pub resolution: u32,
}

impl Default for Terrain {
    fn default() -> Self {
        Self {
            heightmap: String::new(),
            size: [100.0, 100.0],
            height_scale: 1.0,
            resolution: 0,
        }
    }
}

impl Terrain {
    /// 每边格子数：`0` 走默认值，再钳到安全范围。
    pub fn cells(&self) -> usize {
        let cells = if self.resolution == 0 {
            32
        } else {
            self.resolution
        };
        cells.clamp(2, 1024) as usize
    }

    pub fn has_heightmap(&self) -> bool {
        !self.heightmap.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// 音频总线 = 分组音量（#508）。
///
/// `Master` 乘在**所有**总线上；其余总线各自再乘一层。项目代码可以在设置菜单里
/// 调 `prism_app::App::set_audio_bus_volume`，不需要重建任何声部。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AudioBus {
    Master,
    Music,
    #[default]
    Sfx,
    Ambient,
    Voice,
}

impl AudioBus {
    pub const ALL: [Self; 5] = [
        Self::Master,
        Self::Music,
        Self::Sfx,
        Self::Ambient,
        Self::Voice,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Master => 0,
            Self::Music => 1,
            Self::Sfx => 2,
            Self::Ambient => 3,
            Self::Voice => 4,
        }
    }
}

impl ReflectType for AudioBus {
    fn field_type() -> FieldType {
        FieldType::U8
    }

    fn enum_variants() -> Option<&'static [&'static str]> {
        Some(&["Master", "Music", "Sfx", "Ambient", "Voice"])
    }

    fn to_value(&self) -> Value {
        Value::U64(self.index() as u64)
    }

    fn from_value(value: Value) -> Result<Self, ReflectError> {
        match value.as_u64()? {
            0 => Ok(Self::Master),
            1 => Ok(Self::Music),
            2 => Ok(Self::Sfx),
            3 => Ok(Self::Ambient),
            4 => Ok(Self::Voice),
            other => Err(ReflectError::BadData(format!(
                "invalid AudioBus discriminant {other}"
            ))),
        }
    }
}

/// 音频播放器 = `AudioSource`。
///
/// 只承载**作者态**参数：片段路径 + 播放/衰减设置。真正的解码与混音在宿主层
/// （`prism-app::scene_audio`）：按路径从 `assets/` 取文件、后台线程解码、
/// `AudioEngine::play`，与 `MeshRenderer` → `MeshRef` 的分层一致 —— 引擎/场景层
/// 不持有任何音频句柄。
///
/// 空间化做**距离衰减 + 等功率声像**：`spatial_blend = 0` 时按 2D 播放（不衰减、
/// 居中），`= 1` 时完全按 [`AudioSource::volume_at`] 衰减并按听者右向量声像。
/// 声像在开始播放时烘焙（混音器只有 `set_volume`）。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::AudioSource")]
#[serde(default)]
pub struct AudioSource {
    /// 音频资源路径，相对 `assets/`（`.ogg` / `.wav` / `.mp3` / `.flac`）。
    /// 空字符串 = 不播放。
    pub sound_path: String,
    /// 音量，`0.0..=1.0`（乘上距离衰减与监听者增益）。
    pub volume: f32,
    /// 循环播放：播完自动从头再来。
    pub looped: bool,
    /// 场景加载后自动播放；关掉后组件只是「装了个片段」，需要代码/编辑器手动触发。
    pub play_on_awake: bool,
    /// 音量分组（#508）；`Master` 总线会再乘在所有分组上。
    pub bus: AudioBus,
    /// 3D 空间化程度：`0.0` = 纯 2D（不衰减），`1.0` = 完全按距离衰减。
    pub spatial_blend: f32,
    /// 开始衰减的近距离（世界单位）。
    pub min_distance: f32,
    /// 衰减到静止的远距离（世界单位）。
    pub max_distance: f32,
}

impl Default for AudioSource {
    fn default() -> Self {
        Self {
            sound_path: String::new(),
            volume: 1.0,
            looped: false,
            play_on_awake: true,
            bus: AudioBus::Sfx,
            spatial_blend: 0.0,
            min_distance: 1.0,
            max_distance: 50.0,
        }
    }
}

impl AudioSource {
    /// 是否配置了片段（空路径 = 静默占位）。
    pub fn has_sound(&self) -> bool {
        !self.sound_path.is_empty()
    }

    /// 给定与监听者的距离时的**实际音量**（已含 `volume` 与 `spatial_blend`）。
    ///
    /// 曲线：`min_distance` 内为 1.0，之后线性衰减，`max_distance` 处为 0，
    /// 再远保持 0。`spatial_blend` 把这条曲线与「2D 的 1.0」线性混合。
    pub fn volume_at(&self, distance: f32) -> f32 {
        let near = self.min_distance.max(0.0);
        let far = self.max_distance.max(near);
        let attenuation = if !distance.is_finite() || distance >= far {
            0.0
        } else if distance <= near || far <= near {
            1.0
        } else {
            1.0 - (distance - near) / (far - near)
        };
        let blend = self.spatial_blend.clamp(0.0, 1.0);
        self.volume.clamp(0.0, 1.0) * (1.0 - blend * (1.0 - attenuation))
    }
}

/// 听者 = `AudioListener`。
///
/// 位置取实体的 `LocalToWorld`；场景里没有听者时宿主退回到「第一个相机实体」，
/// 再没有就用世界原点。多个听者时只认第一个（同 `Skybox` 的约定）。
#[derive(Debug, Clone, Serialize, Deserialize, Reflect)]
#[reflect(name = "prism_engine::scene::AudioListener")]
#[serde(default)]
pub struct AudioListener {
    /// 全局增益，`0.0..=1.0`（乘在每个 `AudioSource` 的音量上）。
    pub gain: f32,
}

impl Default for AudioListener {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

// ---------------------------------------------------------------------------
// Scene management
// ---------------------------------------------------------------------------

/// Marks an 实体 as belonging to a specific scene.
///
/// Used for batch unload and multi-scene bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SceneMember(pub SceneAssetId);

#[cfg(test)]
#[path = "components_tests.rs"]
mod tests;
