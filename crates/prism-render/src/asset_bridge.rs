//! 主线程(CPU) ↔ 渲染线程(GPU) 资产解析请求/结果桥接类型。
//!
//! 这些类型是纯数据（`Send`），通过 [`prism_app::RenderShared`] 的通道在两侧传递：
//! - 主线程准备上传输入（CPU：加载 `.pak` + 解交织顶点/纹理），不碰 `GraphRenderer`；
//! - 渲染线程（`GraphRenderer::apply_asset_requests`）执行 GPU 上传并返回句柄。
//!
//! 这样资产解析可以异步进行，而不要求 `&mut World` 与 `&mut GraphRenderer`
//! 同时存在于同一线程——启动期渲染器在渲染线程构建时，主线程仍可处理窗口事件。

use crate::managers::{MeshHandle, MeshUploadInput, TextureUploadInput};

/// 材质解析所需的 CPU 侧数据：18 个 PBR 标量 + 5 张纹理像素
/// （固定顺序：albedo, normal, metallic_roughness, emissive, occlusion）。
/// 渲染线程负责上传纹理取 bindless 槽，再据此组装 `MaterialUploadInput`。
pub struct MaterialResolveData {
    pub scalars: [f32; 18],
    pub textures: [Option<TextureUploadInput>; 5],
}

/// 单个实体的 GPU 上传请求（纯数据，Send）。
pub struct AssetResolveRequest {
    pub mesh: Option<MeshUploadInput>,
    pub material: Option<MaterialResolveData>,
    /// 2D 精灵贴图（`SpriteRenderer.sprite_path` 解析结果）。
    pub texture: Option<TextureUploadInput>,
}

/// 一 decided 释放的 GPU 资源（纯数据，Send）。
///
/// 主线程不知道渲染器的内部句柄，只知道组件里存的槽位 / 句柄值，所以回传的
/// 释放请求也用同一组值；渲染线程按值反查并销毁（#495）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssetRelease {
    /// bindless SRV 槽位。
    pub texture_slot: Option<u32>,
    /// 网格句柄。
    pub mesh_handle: Option<MeshHandle>,
    /// 材质 SSBO 槽位。
    pub material_slot: Option<u32>,
}

impl AssetRelease {
    /// 是否包含任何需要释放的资源。
    pub fn is_empty(&self) -> bool {
        self.texture_slot.is_none() && self.mesh_handle.is_none() && self.material_slot.is_none()
    }
}

/// 渲染线程完成上传后回传的句柄（纯数据，Send）。
pub struct AssetResolveResult {
    pub mesh_handle: Option<MeshHandle>,
    pub material_slot: Option<u32>,
    /// 精灵贴图的 bindless SRV 槽位。
    pub texture_slot: Option<u32>,
    /// 精灵贴图的像素尺寸 `[width, height]`（与 `texture_slot` 同时有效）。
    pub texture_size: Option<[u32; 2]>,
}
