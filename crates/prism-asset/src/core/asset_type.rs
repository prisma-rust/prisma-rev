//! 资源类型分类。

use serde::{Deserialize, Serialize};

/// 资源数据格式的高级分类。
///
/// 判别值以 `u32` 形式存储在二进制 .pak 格式中，
/// 因此运行时无需借助字符串表即可分派到正确的加载器。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssetType {
    /// 原始二进制数据块——运行时将其加载为 `Vec<u8>`。
    Binary = 0,
    /// GPU 纹理/图像
    Texture = 1,
    /// Geometric mesh (vertex + index data).
    Mesh = 2,
    /// GPU material parameter block + shader bindings.
    Material = 3,
    /// Shader source or compiled bytecode.
    Shader = 4,
    /// Prefab — a reusable entity template.
    Prefab = 5,
    /// Complete scene graph.
    Scene = 6,
    /// Audio clip (PCM, Ogg, etc.).
    Audio = 7,
    /// 字体文件（ttf / otf / ttc），原样进包，运行时交给 UI 文本光栅器（#583）。
    Font = 8,
    /// 骨骼层级 + reference pose + inverse bind（`.rskel`，#560）。
    Skeleton = 9,
    /// 带 4 骨权重与 submesh 的蒙皮网格（`.rskin`，#560）。
    SkinnedMesh = 10,
    /// 骨骼动画剪辑（`.ranim`，#560）。
    AnimClip = 11,
    /// 动画状态机（`.rasm`，#560 P1）。
    StateMachine = 12,
    /// 动画图（`.rag`，#560 P1）。
    AnimGraph = 13,
    /// Fallback for unrecognised formats.
    Unknown = 0xFF,
}

impl AssetType {
    /// Return `true` if this type is known (not `Unknown`).
    pub fn is_known(self) -> bool {
        self != AssetType::Unknown
    }

    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            AssetType::Binary => "binary",
            AssetType::Texture => "texture",
            AssetType::Mesh => "mesh",
            AssetType::Material => "material",
            AssetType::Shader => "shader",
            AssetType::Prefab => "prefab",
            AssetType::Scene => "scene",
            AssetType::Audio => "audio",
            AssetType::Font => "font",
            AssetType::Skeleton => "skeleton",
            AssetType::SkinnedMesh => "skinned-mesh",
            AssetType::AnimClip => "anim-clip",
            AssetType::StateMachine => "state-machine",
            AssetType::AnimGraph => "anim-graph",
            AssetType::Unknown => "unknown",
        }
    }

    /// Build an `AssetType` from its raw `u32` discriminant.
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => AssetType::Binary,
            1 => AssetType::Texture,
            2 => AssetType::Mesh,
            3 => AssetType::Material,
            4 => AssetType::Shader,
            5 => AssetType::Prefab,
            6 => AssetType::Scene,
            7 => AssetType::Audio,
            8 => AssetType::Font,
            9 => AssetType::Skeleton,
            10 => AssetType::SkinnedMesh,
            11 => AssetType::AnimClip,
            12 => AssetType::StateMachine,
            13 => AssetType::AnimGraph,
            _ => AssetType::Unknown,
        }
    }

    /// Get the raw `u32` discriminant.
    pub fn to_u32(self) -> u32 {
        self as u32
    }

    /// Infer asset type from a file extension (lowercase, without leading dot).
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            // Binary
            "bin" | "bytes" => AssetType::Binary,
            // 已 cook 的运行时格式
            "rscn" => AssetType::Scene,
            "rtex" => AssetType::Texture,
            "rmes" => AssetType::Mesh,
            "rmat" => AssetType::Material,
            "rskel" => AssetType::Skeleton,
            "rskin" => AssetType::SkinnedMesh,
            "ranim" => AssetType::AnimClip,
            "rasm" => AssetType::StateMachine,
            "rag" => AssetType::AnimGraph,
            // Textures / images
            "png" | "jpg" | "jpeg" | "tga" | "bmp" | "hdr" | "exr" | "ktx" | "ktx2" | "dds" => {
                AssetType::Texture
            }
            // Mesh / geometry
            "gltf" | "glb" | "fbx" | "obj" | "stl" | "ply" => AssetType::Mesh,
            // Material
            "mat" | "material" => AssetType::Material,
            // Shader
            "slang" | "hlsl" | "glsl" | "spv" | "wgsl" => AssetType::Shader,
            // Prefab
            "prefab" => AssetType::Prefab,
            // Scene
            "scene" => AssetType::Scene,
            // Audio
            "wav" | "mp3" | "ogg" | "flac" | "aac" | "m4a" => AssetType::Audio,
            // 字体（UI 文本用；原样进包，不需要 cook）
            "ttf" | "otf" | "ttc" => AssetType::Font,
            _ => AssetType::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// AssetRef – editor-side reference to another asset
// ---------------------------------------------------------------------------

/// A lightweight editor-side reference to another asset.
///
/// Unlike `Handle<T>`, this is serializable and contains only the stable
/// identity, never a runtime slot index. Use this in JSON metadata, importer
/// dependency lists, and prefab asset references.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRef {
    pub id: crate::core::AssetId,
    pub asset_type: AssetType,
}

impl AssetRef {
    /// Create a new reference.
    pub fn new(id: crate::core::AssetId, asset_type: AssetType) -> Self {
        Self { id, asset_type }
    }
}

#[cfg(test)]
#[path = "asset_type_tests.rs"]
mod tests;
