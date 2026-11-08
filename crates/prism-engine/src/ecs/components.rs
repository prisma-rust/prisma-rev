//! ECS component type aliases and factory helpers.
//!
//! Re-exports the engine's standard components so consumers can write
//! `use crate::ecs::components::*` instead of
//! `use crate::scene::components::{...}`.
//!
//! # Base components
//!
//! | Alias | Canonical type |
//! |-------|----------------|
//! | [`Transform`] | [`LocalTransform`](crate::scene::components::LocalTransform) |
//! | [`Camera`]   | [`Camera`](crate::scene::components::Camera) |
//! | [`Name`]     | [`Name`](crate::scene::components::Name) |

pub use crate::scene::components::{
    Camera, DirectionalLight, LocalTransform, Name, PointLight, SpotLight,
};

/// Convenience: the engine's standard game-object transform.
pub type Transform = LocalTransform;

// ---------------------------------------------------------------------------
// MeshRenderer
// ---------------------------------------------------------------------------

/// A renderable entity with a mesh and material, referenced by handle.
///
/// This is the high-level authoring component.  At extraction time the
/// render system resolves the handles to GPU resources and produces
/// [`DrawItem`]s.
#[derive(Debug, Clone)]
pub struct MeshRenderer {
    /// CPU-side handle to a [`MeshAsset`](crate::asset::MeshAsset).
    pub mesh: crate::asset::MeshId,
    /// CPU-side handle to a [`MaterialAsset`](crate::asset::MaterialAsset).
    pub material: crate::asset::MaterialId,
}
