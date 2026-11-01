//! 类型化资源包装器，包含每个烘焙格式的 `impl Resource`。
//!
//! 这里的每个类型都是烘焙资源（RTEX、RMES、RMAT、SPIR-V、RSCN）的运行时表示。
//! `ResourceManager::load::<T>(id)` / `get::<T>(handle)` 使用这些 impl
//! 将原始 `.pak` 字节反序列化为结构化数据。
//!
//! 解码器位于共享的 [`crate::formats`] 模块中，运行时因此不依赖 cooker。
//! RSCN 场景格式由引擎的 `SceneLoader` 惰性解析，
//! 因此 `SceneAsset` 仅持有原始字节。

use crate::core::{AssetId, AssetType};
use crate::formats::{
    decode_rmat, decode_rmes, decode_rtex, RmatInfo, RmesInfo, RtexInfo, MATERIAL_SCALAR_COUNT,
};
use crate::formats_skinned::{
    decode_ranim, decode_rskel, decode_rskin, AnimClipInfo, SkeletonInfo, SkinnedMeshInfo,
};
use crate::runtime::{Asset, RuntimeError};

// ---------------------------------------------------------------------------
// 纹理
// ---------------------------------------------------------------------------

/// Cooked 纹理 资源 (RTEX 格式
///
/// Holds the decoded [`RtexInfo`] 宽度 高度 mip 链 格式 byte).
/// The renderer's `RenderTextureManager` consumes mip-0 for the RGBA8 path;
/// BC-compressed formats are 已支持 by the 运行时 upload path.
#[derive(Debug, Clone)]
pub struct TextureAsset {
    pub info: RtexInfo,
    /// The cooked bytes this asset was decoded from, kept so [`Asset::into_bytes`]
    /// can hand back exactly what the `.pak` stored. Decoding copies the mip
    /// chain into `info`, so re-serialising from `info` would need the cooker's
    /// writer in the runtime layer.
    raw: Vec<u8>,
}

impl Asset for TextureAsset {
    fn asset_type() -> AssetType {
        AssetType::Texture
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_rtex(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Texture,
            reason: "invalid RTEX data (bad magic / truncated)".into(),
        })?;
        Ok(Self {
            info,
            raw: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw
    }
}

// ---------------------------------------------------------------------------
// 网格
// ---------------------------------------------------------------------------

/// Cooked 网格 资源 (RMES 格式
///
/// Holds the decoded [`RmesInfo`] (vertex/index counts + raw interleaved
/// 顶点 字节 + raw u32 索引 字节 The renderer's `RenderMeshManager`
/// de-interleaves this into split arrays (`positions`, `normals`, `uvs`,
/// `tangents`, `indices`) at upload 时间
#[derive(Debug, Clone)]
pub struct MeshAsset {
    pub info: RmesInfo,
    /// The cooked bytes this asset was decoded from. Kept verbatim because
    /// [`RmesInfo`] does not model the three sub-buffer offsets in the RMES
    /// header, so anything rebuilt from `info` would not be byte-identical.
    raw: Vec<u8>,
}

impl Asset for MeshAsset {
    fn asset_type() -> AssetType {
        AssetType::Mesh
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_rmes(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Mesh,
            reason: "invalid RMES data (bad magic / truncated)".into(),
        })?;
        Ok(Self {
            info,
            raw: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw
    }
}

// ---------------------------------------------------------------------------
// 材质
// ---------------------------------------------------------------------------

/// Cooked 材质 资源 (RMAT 格式
///
/// Holds the decoded [`RmatInfo`] (18 标量 floats + 5 纹理 `AssetId`
/// slots). The 渲染器 resolves the 纹理 `AssetId`s to bindless SRV slots
/// by loading each `TextureAsset` dependency.
#[derive(Debug, Clone)]
pub struct MaterialAsset {
    pub info: RmatInfo,
    /// The cooked bytes this asset was decoded from, so [`Asset::into_bytes`]
    /// round-trips the original RMAT blob (including any trailing payload the
    /// decoder ignores).
    raw: Vec<u8>,
}

impl MaterialAsset {
    /// Convenience accessor for the 18 标量 floats.
    pub fn scalars(&self) -> &[f32; MATERIAL_SCALAR_COUNT] {
        &self.info.scalars
    }

    /// Convenience accessor for the 5 texture-slot `AssetId`s.
    pub fn texture_ids(&self) -> &[Option<AssetId>; 5] {
        &self.info.texture_ids
    }
}

impl Asset for MaterialAsset {
    fn asset_type() -> AssetType {
        AssetType::Material
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_rmat(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Material,
            reason: "invalid RMAT data (bad magic / truncated)".into(),
        })?;
        Ok(Self {
            info,
            raw: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw
    }
}

// ---------------------------------------------------------------------------
// 着色器
// ---------------------------------------------------------------------------

/// Cooked 着色器 资源 (raw SPIR-V bytecode).
///
/// The cooked data is the SPIR-V itself (no 包装器 the 运行时 validates
/// the SPIR-V magic and hands the 字节 to `vkCreateShaderModule`.
#[derive(Debug, Clone)]
pub struct ShaderAsset {
    /// Raw SPIR-V bytecode, little-endian 机 native on x86/ARM).
    pub spirv: Vec<u8>,
}

/// SPIR-V magic number (little-endian 第一个 word).
const SPIRV_MAGIC_LE: u32 = 0x0723_0203;

impl Asset for ShaderAsset {
    fn asset_type() -> AssetType {
        AssetType::Shader
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        if data.len() < 4 {
            return Err(RuntimeError::DeserializeFailed {
                asset_type: AssetType::Shader,
                reason: "SPIR-V too short".into(),
            });
        }
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if magic != SPIRV_MAGIC_LE {
            return Err(RuntimeError::DeserializeFailed {
                asset_type: AssetType::Shader,
                reason: format!(
                    "not valid SPIR-V (magic={:#010x}, expected {:#010x})",
                    magic, SPIRV_MAGIC_LE
                ),
            });
        }
        Ok(Self {
            spirv: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.spirv
    }
}

// ---------------------------------------------------------------------------
// Scene
// ---------------------------------------------------------------------------

/// Cooked scene 资源 (RSCN 格式 raw 字节
///
/// The 运行时 holds the RSCN 字节 verbatim; the engine's `SceneLoader`
/// parses them into `ParsedEntity` records and spawns ECS entities. Keeping
/// the parse out of the 运行时 preserves the "no engine types in the
/// 运行时 boundary.
#[derive(Debug, Clone)]
pub struct SceneAsset {
    /// Raw RSCN 字节 (cooked `SceneCooker` 输出
    pub bytes: Vec<u8>,
}

impl Asset for SceneAsset {
    fn asset_type() -> AssetType {
        AssetType::Scene
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        // 光源 验证 RSCN magic check.
        if data.len() < 5 || &data[..4] != b"RSCN" {
            return Err(RuntimeError::DeserializeFailed {
                asset_type: AssetType::Scene,
                reason: "invalid RSCN data (bad magic / truncated)".into(),
            });
        }
        Ok(Self {
            bytes: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
#[path = "typed_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// 骨骼 / 蒙皮 / 动画剪辑（#560 P0）
// ---------------------------------------------------------------------------

/// Cooked 骨骼（RSKL）。解码后只剩纯数据：运行时由 `Animator` / 蒙皮着色器消费。
#[derive(Debug, Clone)]
pub struct SkeletonAsset {
    pub info: SkeletonInfo,
    raw: Vec<u8>,
}

impl Asset for SkeletonAsset {
    fn asset_type() -> AssetType {
        AssetType::Skeleton
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_rskel(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Skeleton,
            reason: "invalid RSKL data (bad magic / truncated)".into(),
        })?;
        Ok(Self {
            info,
            raw: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw
    }
}

/// Cooked 蒙皮网格（RSKN）：每顶点 4 骨影响 + submesh + bind pose。
#[derive(Debug, Clone)]
pub struct SkinnedMeshAsset {
    pub info: SkinnedMeshInfo,
    raw: Vec<u8>,
}

impl Asset for SkinnedMeshAsset {
    fn asset_type() -> AssetType {
        AssetType::SkinnedMesh
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_rskin(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::SkinnedMesh,
            reason: "invalid RSKN data (bad magic / truncated)".into(),
        })?;
        Ok(Self {
            info,
            raw: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw
    }
}

/// Cooked 动画剪辑（RANI）。
#[derive(Debug, Clone)]
pub struct AnimClipAsset {
    pub info: AnimClipInfo,
    raw: Vec<u8>,
}

impl Asset for AnimClipAsset {
    fn asset_type() -> AssetType {
        AssetType::AnimClip
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_ranim(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::AnimClip,
            reason: "invalid RANI data (bad magic / truncated)".into(),
        })?;
        Ok(Self {
            info,
            raw: data.to_vec(),
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.raw
    }
}
