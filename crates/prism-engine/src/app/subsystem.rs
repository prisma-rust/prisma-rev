//! Subsystem trait, ScheduleLabel, System, AppBuilder

use std::any::{Any, TypeId};
use std::collections::HashMap;

use prism_ecs::World;

// ---------------------------------------------------------------------------
// ScheduleLabel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScheduleLabel {
    Startup,
    FixedUpdate,
    Update,
    LateUpdate,
    Render,
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

pub trait System: Send {
    fn run(&mut self, world: &mut World);
}

impl<F> System for F
where
    F: FnMut(&mut World) + Send + 'static,
{
    fn run(&mut self, world: &mut World) {
        (self)(world)
    }
}

// ---------------------------------------------------------------------------
// Subsystem trait
// ---------------------------------------------------------------------------

/// 引擎功能模块（或用户可复用模块）的统一接口。
///
/// 完整的生命周期：
/// 1. `build()` — 注册系统/资源
/// 2. `on_startup()` — App 启动后
/// 3. `on_suspend()` / `on_resume()` — 平台生命周期（由应用层转发）
/// 4. `on_shutdown()` — 关闭
pub trait Subsystem: Send + 'static {
    /// 配置阶段：注册 Schedule 系统、插入资源。
    fn build(&self, app: &mut AppBuilder);

    /// 启动时调用（由应用层编排）。
    fn on_startup(&mut self) {}

    /// 平台挂起。
    fn on_suspend(&mut self) {}

    /// 平台恢复。
    fn on_resume(&mut self) {}

    /// 关闭前清理。
    fn on_shutdown(&mut self) {}
}

// ---------------------------------------------------------------------------
// AppBuilder
// ---------------------------------------------------------------------------

/// 配置阶段的构建器。所有 `add_subsystem` 和 `add_systems` 在此阶段完成。
pub struct AppBuilder {
    pub(crate) world: World,
    pub(crate) systems: Vec<(ScheduleLabel, Box<dyn System>)>,
    pub(crate) startup_systems: Vec<Box<dyn System>>,
    pub(crate) resources: HashMap<TypeId, Box<dyn Any + Send>>,
    pub(crate) subsystems: Vec<Box<dyn Subsystem>>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            world: World::new(),
            systems: Vec::new(),
            startup_systems: Vec::new(),
            resources: HashMap::new(),
            subsystems: Vec::new(),
        }
    }

    /// 添加 Subsystem（引擎或用户）。
    pub fn add_subsystem(&mut self, subsystem: impl Subsystem + 'static) -> &mut Self {
        subsystem.build(self);
        self.subsystems.push(Box::new(subsystem));
        self
    }

    /// 在指定阶段添加系统函数。
    pub fn add_systems(
        &mut self,
        label: ScheduleLabel,
        system: impl System + 'static,
    ) -> &mut Self {
        self.systems.push((label, Box::new(system)));
        self
    }

    /// 在 Startup 阶段添加一个系统。
    pub fn add_startup_system(&mut self, system: impl System + 'static) -> &mut Self {
        self.startup_systems.push(Box::new(system));
        self
    }

    /// 插入一个运行时资源。
    pub fn insert_resource<T: Send + 'static>(&mut self, resource: T) -> &mut Self {
        self.resources.insert(TypeId::of::<T>(), Box::new(resource));
        self
    }

    /// 获取运行时资源的可变引用。
    pub fn get_resource_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.resources
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut::<T>())
    }

    /// 构建并运行 App（暂未接入）。
    pub fn run(self) {
        log::info!("AppBuilder::run() — Subsystem模式尚未接入，框架待集成");
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// DefaultSubsystems
// ---------------------------------------------------------------------------

/// 引擎内置的默认子系统集合。
pub struct DefaultSubsystems;

impl Subsystem for DefaultSubsystems {
    fn build(&self, _app: &mut AppBuilder) {
        // 逐步提取: WinitSubsystem / RenderSubsystem / EditorSubsystem
        // 当前迁移阶段使用 LegacyApp 维持运行
    }
}
