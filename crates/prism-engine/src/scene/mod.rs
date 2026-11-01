//! Modern scene 系统 — ECS components, loading, hierarchy, and systems.
//!
//! 场景加载（manifest → world）由场景系统 [`SceneManager`] 统一负责；
//! `Engine::init_scene` 委托给 [`SceneManager::load_first_from_manifest`]。
//!
//! See `docs/plans/2026-07-25-modern-scene-system-design.md`.

pub mod component_registry {
    //! Compatibility re-export of [`prism_scene::registry`].
    pub use prism_scene::registry::*;
}
pub mod runtime_components;
pub mod components {
    //! Scene-schema components (from `prism-scene`) plus runtime GPU refs.
    pub use super::runtime_components::{
        MaterialRef, MaterialSlots, MeshRef, MeshSubmesh, MeshSubmeshes, SceneEntityId, SpriteRef,
    };
    pub use prism_scene::components::*;
}
pub mod helpers {
    //! Compatibility re-export of [`prism_scene::helpers`].
    pub use prism_scene::helpers::*;
}
pub mod hot_reload;
pub mod loader;
pub mod scene_system;

pub mod systems;

pub use component_registry::{register_builtin_components, ComponentRegistry};
pub use loader::SceneInstance;
pub use scene_system::{
    EnvironmentProvider, SceneManager, SceneManifest, SceneManifestEntry, SceneReadView,
};

/// Scene hierarchy 适配器 for the 编辑器 检查器。
///
/// Roots: entities with [`LocalTransform`] or [`Name`] but no [`Parent`].
/// Child: via [`Child`] 分量。
///
/// 外部工具 / 编辑器可为同一结构提供只读视图——引擎本身不依赖编辑器。
pub struct SceneHierarchy;
