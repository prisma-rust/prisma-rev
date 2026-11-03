use crate::types::CubeDef;
use crate::{impl_asset_data, AssetGuid, AssetHandle};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// MaterialDef
// ---------------------------------------------------------------------------

/// PBR material definition — a ScriptableObject-style asset type.
///
/// `MaterialDef` is the **editable source** of a PBR material. It is
/// serialised as JSON/RON in the asset library and cooked into the runtime
/// [`GpuMaterial`] layout by the pipeline.
///
/// # Cross-references
///
/// Texture references use [`AssetHandle<TextureDef>`] — typed, GUID-based
/// handles that survive file renames. The editor resolves these to texture
/// data during cooking.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MaterialDef {
    // --- identity ---
    /// Stable GUID — set once when the asset is first created.
    pub guid: AssetGuid,
    /// Human-readable label (not path-based — survives moves).
    pub label: String,

    // --- PBR parameters ---
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub normal_scale: f32,
    pub occlusion_strength: f32,

    // --- Texture references ---
    pub albedo_map: Option<AssetHandle<TextureDef>>,
    pub normal_map: Option<AssetHandle<TextureDef>>,
    pub metallic_roughness_map: Option<AssetHandle<TextureDef>>,
    pub emissive_map: Option<AssetHandle<TextureDef>>,
    pub occlusion_map: Option<AssetHandle<TextureDef>>,

    // --- IBL reference ---
    /// Cubemap for IBL environment lighting.
    pub env_map: Option<AssetHandle<CubeDef>>,

    // --- Advanced PBR ---
    pub transmission: f32,
    pub ior: f32,
    pub translucency: f32,
    pub anisotropy: f32,
    pub clearcoat: f32,
    pub clearcoat_roughness: f32,
}

impl Default for MaterialDef {
    fn default() -> Self {
        Self {
            guid: AssetGuid::nil(),
            label: String::new(),
            base_color: [1.0, 1.0, 1.0, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            emissive: [0.0; 3],
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            albedo_map: None,
            normal_map: None,
            metallic_roughness_map: None,
            emissive_map: None,
            occlusion_map: None,
            env_map: None,

            transmission: 0.0,
            ior: 1.5,
            translucency: 0.0,
            anisotropy: 0.0,
            clearcoat: 0.0,
            clearcoat_roughness: 0.0,
        }
    }
}

impl_asset_data!(MaterialDef, "material", "PBR Material");

// ---------------------------------------------------------------------------
// TextureDef
// ---------------------------------------------------------------------------

/// 引用 to a 纹理 源 资源 (imported 图像 待烘焙).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TextureDef {
    pub guid: AssetGuid,
    pub label: String,
    /// Source image slot — e.g. "albedo", "normal", "orm".
    pub slot: String,
}

impl_asset_data!(TextureDef, "texture", "Texture Source");
