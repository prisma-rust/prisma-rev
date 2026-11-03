//! CPU-side asset data types for the engine's procedural asset pipeline.
//!
//! These are the **raw** mesh/material data that sits on the CPU until the
//! renderer uploads them to the GPU (via `RenderMeshManager` /
//! `RenderMaterialManager`).  The handles pointing here live in
//! [`MeshRenderer`](crate::ecs::components::MeshRenderer).

// ===========================================================================
// MeshAsset
// ===========================================================================

/// CPU-side mesh data (positions, normals, UVs, indices).
///
/// Designed to be cheap to clone if needed; the procedural factory
/// functions produce this once and the asset manager owns it.
#[derive(Debug, Clone)]
pub struct MeshAsset {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

impl MeshAsset {
    /// Create a mesh asset from raw vertex data.
    pub fn new(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        uvs: Vec<[f32; 2]>,
        indices: Vec<u32>,
    ) -> Self {
        Self {
            positions,
            normals,
            uvs,
            indices,
        }
    }
}

// ===========================================================================
// MaterialAsset
// ===========================================================================

/// CPU-side material parameters.
#[derive(Debug, Clone)]
pub struct MaterialAsset {
    /// Linear RGB base colour.
    pub base_color: [f32; 3],
    /// Metallic factor [0, 1].
    pub metallic: f32,
    /// Roughness factor [0, 1].
    pub roughness: f32,
}

impl MaterialAsset {
    pub fn new(base_color: [f32; 3], metallic: f32, roughness: f32) -> Self {
        Self {
            base_color,
            metallic,
            roughness,
        }
    }
}

impl Default for MaterialAsset {
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8],
            metallic: 0.0,
            roughness: 0.5,
        }
    }
}
