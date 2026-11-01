//! GPU-side 资源 managers.
//!
//! Each 管理器 wraps a slotmap-typed handle 表 and an explicit
//! `destroy(ctx)` lifecycle. 放置 is a no-op that only `debug_assert!`s
//! the 管理器 is 空 — the real 释放 path runs through the explicit
//! 方法 matching the 契约 the rest of `prism-render` follows.
//!
//! These managers consume *local* 输入 structs (defined 下一个 to them) so
//! the 渲染 crate stays decoupled from the 资源 管线 The engine 层
//! converts 资源 data into these inputs at the seam.

pub mod material_manager;
pub mod mesh_manager;
pub mod texture_manager;

pub use material_manager::{
    GpuMaterial, MaterialHandle, MaterialUploadInput, RenderMaterialManager, MATERIAL_SSBO_MAX,
};
pub use mesh_manager::{
    MeshHandle, MeshUploadInput, RenderMeshManager, SkinAttributes, UploadedMesh,
};
pub use texture_manager::{
    AssetTextureHandle, RenderTextureManager, TextureFormat, TextureUploadInput, UploadedTexture,
};
