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
    decode_ranim2d, decode_ratl, decode_rmat, decode_rmes, decode_rtex, decode_rtilemap,
    Ranim2DInfo, RatlInfo, RmatInfo, RmesInfo, RtexInfo, RtilemapInfo, MATERIAL_SCALAR_COUNT,
};
use crate::formats_anim_graph::AnimGraphInfo;
use crate::formats_skinned::{
    decode_ranim, decode_rskel, decode_rskin, AnimClipInfo, SkeletonInfo, SkinnedMeshInfo,
};
use crate::runtime::{Asset, RuntimeError};

// ---------------------------------------------------------------------------
// Texture
// ---------------------------------------------------------------------------

/// Cooked texture asset (RTEX format).
///
/// Holds the decoded [`RtexInfo`] (width, height, mip chain, format byte).
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
// Mesh
// ---------------------------------------------------------------------------

/// Cooked mesh asset (RMES format).
///
/// Holds the decoded [`RmesInfo`] (vertex/index counts + raw interleaved
/// vertex bytes + raw u32 index bytes). The renderer's `RenderMeshManager`
/// de-interleaves this into split arrays (`positions`, `normals`, `uvs`,
/// `tangents`, `indices`) at upload time.
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
// Material
// ---------------------------------------------------------------------------

/// Cooked material asset (RMAT format).
///
/// Holds the decoded [`RmatInfo`] (18 scalar floats + 5 texture `AssetId`
/// slots). The renderer resolves the texture `AssetId`s to bindless SRV slots
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
    /// Convenience accessor for the 18 scalar floats.
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
// Shader
// ---------------------------------------------------------------------------

/// Cooked shader asset (raw SPIR-V bytecode).
///
/// The cooked data is the SPIR-V itself (no wrapper); the runtime validates
/// the SPIR-V magic and hands the bytes to `vkCreateShaderModule`.
#[derive(Debug, Clone)]
pub struct ShaderAsset {
    /// Raw SPIR-V bytecode, little-endian (machine native on x86/ARM).
    pub spirv: Vec<u8>,
}

/// SPIR-V magic number (little-endian first word).
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

/// Cooked scene asset (RSCN format, raw bytes).
///
/// The runtime holds the RSCN bytes verbatim; the engine's `SceneLoader`
/// parses them into `ParsedEntity` records and spawns ECS entities. Keeping
/// the parse out of the runtime preserves the "no engine types in the
/// runtime" boundary.
#[derive(Debug, Clone)]
pub struct SceneAsset {
    /// Raw RSCN bytes (cooked `SceneCooker` output).
    pub bytes: Vec<u8>,
}

impl Asset for SceneAsset {
    fn asset_type() -> AssetType {
        AssetType::Scene
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        // Light validation: RSCN magic check.
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

/// 解码后的 `.rasm`：状态 / 转移 / 条件（#560 P1）。
#[derive(Debug, Clone)]
pub struct StateMachineAsset {
    pub info: crate::formats_state_machine::StateMachineInfo,
    raw: Vec<u8>,
}

impl Asset for StateMachineAsset {
    fn asset_type() -> AssetType {
        AssetType::StateMachine
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = crate::formats_state_machine::decode_rasm(data).ok_or_else(|| {
            RuntimeError::DeserializeFailed {
                asset_type: AssetType::StateMachine,
                reason: "invalid RASM data (bad magic / truncated)".into(),
            }
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

/// 解码后的 `.rag`：动画图参数 / 节点 / 连线（#560 P1）。
#[derive(Debug, Clone)]
pub struct AnimGraphAsset {
    pub info: AnimGraphInfo,
    raw: Vec<u8>,
}

impl Asset for AnimGraphAsset {
    fn asset_type() -> AssetType {
        AssetType::AnimGraph
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = crate::formats_anim_graph::decode_rag(data).ok_or_else(|| {
            RuntimeError::DeserializeFailed {
                asset_type: AssetType::AnimGraph,
                reason: "invalid RAGP data (bad magic / truncated)".into(),
            }
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

// ---------------------------------------------------------------------------
// Atlas
// ---------------------------------------------------------------------------

/// Cooked sprite atlas metadata asset (.ratlas format).
#[derive(Debug, Clone)]
pub struct AtlasAsset {
    pub info: RatlInfo,
    raw: Vec<u8>,
}

impl Asset for AtlasAsset {
    fn asset_type() -> AssetType {
        AssetType::Atlas
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_ratl(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Atlas,
            reason: "invalid RATL data (bad magic / truncated)".into(),
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
// 2D Animation (.ranim2d)
// ---------------------------------------------------------------------------

/// Cooked 2D sprite animation asset (.ranim2d format).
#[derive(Debug, Clone)]
pub struct Anim2DAsset {
    pub info: Ranim2DInfo,
    raw: Vec<u8>,
}

impl Asset for Anim2DAsset {
    fn asset_type() -> AssetType {
        AssetType::Anim2D
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_ranim2d(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Anim2D,
            reason: "invalid RA2D data (bad magic / truncated)".into(),
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
// Tilemap (.rtilemap)
// ---------------------------------------------------------------------------

/// Cooked tilemap level asset (.rtilemap format).
#[derive(Debug, Clone)]
pub struct TilemapAsset {
    pub info: RtilemapInfo,
    raw: Vec<u8>,
}

impl Asset for TilemapAsset {
    fn asset_type() -> AssetType {
        AssetType::Tilemap
    }

    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError> {
        let info = decode_rtilemap(data).ok_or_else(|| RuntimeError::DeserializeFailed {
            asset_type: AssetType::Tilemap,
            reason: "invalid RTMG data (bad magic / truncated)".into(),
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
