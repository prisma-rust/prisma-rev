//! ECS system schedule — ordered list of systems dispatched per tick.
//!
//! A [`Schedule`] owns a boxed list of system closures, each receiving
//! `(&mut World, dt)`. Systems run in registration order.
//!
//! # Example
//!
//! ```ignore
//! let mut schedule = Schedule::new();
//! schedule.add_system("rotate_cubes", |world, dt| {
//!     for (_, transform) in world.query_mut::<Transform>() {
//!         transform.rotation[1] += dt * 0.5; // yaw
//!     }
//! });
//! ```

use crate::ecs::World;

/// Type of a system function: `(world, dt_seconds)`.
pub type SystemFn = Box<dyn FnMut(&mut World, f32)>;

/// Ordered list of systems driven by the engine each tick.
pub struct Schedule {
    systems: Vec<(String, SystemFn)>,
}

impl Schedule {
    /// Empty schedule.
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
        }
    }

    /// Append a system with a debug label.
    pub fn add_system<F>(&mut self, label: &str, f: F)
    where
        F: FnMut(&mut World, f32) + 'static,
    {
        self.systems.push((label.to_string(), Box::new(f)));
    }

    /// 是否已注册 label 相同的系统。
    pub fn contains(&self, label: &str) -> bool {
        self.systems.iter().any(|(l, _)| l == label)
    }

    /// 合并另一个 调度：仅追加 label 尚未存在的系统（幂等）。
    ///
    /// `runtime_initialize` 用它把默认系统并入用户已注册的系统之后，
    /// 而不是整体替换——用户在任何时机注册的 system 都不会被冲掉。
    pub fn merge_if_absent(&mut self, other: Schedule) {
        for (label, f) in other.systems {
            if !self.contains(&label) {
                self.systems.push((label, f));
            }
        }
    }

    /// Run all registered systems in order.
    pub fn run(&mut self, world: &mut World, dt: f32) {
        for (_label, sys) in &mut self.systems {
            sys(world, dt);
        }
    }

    /// Clear all registered systems.
    pub fn clear(&mut self) {
        self.systems.clear();
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}
