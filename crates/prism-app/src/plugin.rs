//! Application plugin architecture (#1165).
//!
//! Provides an extensible plugin system allowing user projects to customize,
//! extend, or replace core engine subsystems (physics, audio, rendering passes,
//! gameplay systems) via modular, decoupled plugins.

use std::any::TypeId;
use std::collections::HashSet;

use crate::App;

/// Modular plugin interface for configuring and extending the engine.
pub trait AppPlugin: 'static {
    /// Human-readable name of the plugin for diagnostics and logging.
    fn name(&self) -> &'static str;

    /// Configures the app by registering systems, resources, or hooks.
    fn build(&self, app: &mut App);

    /// Unique identifier for plugin deduplication or replacement.
    fn type_id(&self) -> TypeId {
        TypeId::of::<Self>()
    }
}

/// Bundles multiple plugins together.
pub trait PluginGroup {
    /// Registers each plugin in the group into the given app.
    fn build(self, app: &mut App);
}

/// Registry maintaining pending plugins and disabled plugin types.
#[derive(Default)]
pub struct PluginRegistry {
    plugins: Vec<Box<dyn AppPlugin>>,
    disabled: HashSet<TypeId>,
    built: HashSet<TypeId>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a plugin to the pending list.
    pub fn add<P: AppPlugin>(&mut self, plugin: P) {
        self.plugins.push(Box::new(plugin));
    }

    /// Marks a plugin type as disabled so its `build` method is skipped.
    pub fn disable<P: AppPlugin>(&mut self) {
        self.disabled.insert(TypeId::of::<P>());
    }

    /// Returns whether the specified plugin type is currently enabled.
    pub fn is_enabled<P: AppPlugin>(&self) -> bool {
        !self.disabled.contains(&TypeId::of::<P>())
    }

    /// Drains and executes all enabled pending plugins on the app.
    pub(crate) fn apply_pending(app: &mut App) {
        loop {
            let pending = std::mem::take(&mut app.plugin_registry.plugins);
            if pending.is_empty() {
                break;
            }
            for plugin in pending {
                let tid = plugin.type_id();
                if app.plugin_registry.disabled.contains(&tid) {
                    log::info!("Plugin {} is disabled; skipping build", plugin.name());
                    continue;
                }
                if app.plugin_registry.built.contains(&tid) {
                    log::debug!("Plugin {} already built; skipping duplicate", plugin.name());
                    continue;
                }
                app.plugin_registry.built.insert(tid);
                log::debug!("Building plugin: {}", plugin.name());
                plugin.build(app);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Standard Default Plugins
// ---------------------------------------------------------------------------

/// Default plugin for rendering and window management.
pub struct DefaultRenderPlugin;

impl AppPlugin for DefaultRenderPlugin {
    fn name(&self) -> &'static str {
        "DefaultRenderPlugin"
    }

    fn build(&self, app: &mut App) {
        app.enable_subsystem(crate::Subsystem::Render);
    }
}

/// Default plugin for 3D physics integration (Rapier3D).
pub struct DefaultPhysicsPlugin;

impl AppPlugin for DefaultPhysicsPlugin {
    fn name(&self) -> &'static str {
        "DefaultPhysicsPlugin"
    }

    fn build(&self, app: &mut App) {
        app.enable_subsystem(crate::Subsystem::Physics);
    }
}

/// Default plugin for audio engine integration.
pub struct DefaultAudioPlugin;

impl AppPlugin for DefaultAudioPlugin {
    fn name(&self) -> &'static str {
        "DefaultAudioPlugin"
    }

    fn build(&self, app: &mut App) {
        app.enable_subsystem(crate::Subsystem::Audio);
    }
}

/// Bundle of standard default plugins (Render, Physics, Audio).
pub struct DefaultPlugins;

impl PluginGroup for DefaultPlugins {
    fn build(self, app: &mut App) {
        app.add_plugin(DefaultRenderPlugin);
        app.add_plugin(DefaultPhysicsPlugin);
        app.add_plugin(DefaultAudioPlugin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct TestResource(u32);

    struct CustomPlugin;
    impl AppPlugin for CustomPlugin {
        fn name(&self) -> &'static str {
            "CustomPlugin"
        }
        fn build(&self, app: &mut App) {
            app.insert_resource(TestResource(42));
        }
    }

    struct DisabledPlugin;
    impl AppPlugin for DisabledPlugin {
        fn name(&self) -> &'static str {
            "DisabledPlugin"
        }
        fn build(&self, app: &mut App) {
            app.insert_resource(TestResource(999));
        }
    }

    #[test]
    fn test_custom_plugin_registration() {
        let mut app = App::hosted(prism_engine::config::AppConfig::default());
        app.add_plugin(CustomPlugin);
        app.build_plugins();

        let world = app.engine_mut().world_mut();
        let res = world.get_resource::<TestResource>();
        assert_eq!(res, Some(&TestResource(42)));
    }

    #[test]
    fn test_disable_plugin() {
        let mut app = App::hosted(prism_engine::config::AppConfig::default());
        app.disable_plugin::<DisabledPlugin>();
        app.add_plugin(DisabledPlugin);
        app.add_plugin(CustomPlugin);
        app.build_plugins();

        let world = app.engine_mut().world_mut();
        let res = world.get_resource::<TestResource>();
        assert_eq!(res, Some(&TestResource(42)));
    }

    #[test]
    fn test_duplicate_plugin_deduplication() {
        let mut app = App::hosted(prism_engine::config::AppConfig::default());
        app.add_plugin(CustomPlugin);
        app.add_plugin(CustomPlugin);
        app.build_plugins();

        let world = app.engine_mut().world_mut();
        assert_eq!(world.get_resource::<TestResource>(), Some(&TestResource(42)));
    }

    struct GroupA;
    impl PluginGroup for GroupA {
        fn build(self, app: &mut App) {
            app.add_plugin(CustomPlugin);
        }
    }

    #[test]
    fn test_plugin_group() {
        let mut app = App::hosted(prism_engine::config::AppConfig::default());
        app.add_plugins(GroupA);
        app.build_plugins();

        let world = app.engine_mut().world_mut();
        assert_eq!(world.get_resource::<TestResource>(), Some(&TestResource(42)));
    }
}
