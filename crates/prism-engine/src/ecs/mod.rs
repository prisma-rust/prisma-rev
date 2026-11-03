//! ECS module — engine data layer.
//!
//! Provides the engine-level `World` (prism_ecs), a `Schedule` system
//! dispatcher, and re-exports of the base component types that every engine
//! consumer needs: `Transform`, `Camera`, `Entity`.
//!
//! ## Quick start
//!
//! ```ignore
//! use crate::ecs::*;
//!
//! let mut world = World::new();
//! let e = world.spawn();
//! world.insert(e, Transform::default());
//! world.insert(e, Camera::default());
//! ```

// Re-export the ECS core so consumers only need `crate::ecs::*`.
pub use prism_ecs::{Component, Entity, World};

pub mod components;
pub mod schedule;

/// Base position/rotation/scale component.
///
/// Thin wrapper around [`crate::scene::components::LocalTransform`] so it can
/// be referenced from the unified `ecs::components` namespace.
pub type Transform = crate::scene::components::LocalTransform;

/// Perspective camera parameters component.
pub type Camera = crate::scene::components::Camera;
