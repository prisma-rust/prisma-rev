//! DOTS-style systems and system groups.
//!
//! Aligns with `ISystem` / `SystemBase` / `ComponentSystemGroup`:
//!
//! - [`ISystem`]: full lifecycle hooks (Rust snake_case; the DOTS name is in
//!   parentheses): [`on_create`](ISystem::on_create) (`OnCreate`),
//!   [`on_start_running`](ISystem::on_start_running) (`OnStartRunning`),
//!   [`on_stop_running`](ISystem::on_stop_running) (`OnStopRunning`),
//!   [`on_update`](ISystem::on_update) (`OnUpdate`),
//!   [`on_destroy`](ISystem::on_destroy) (`OnDestroy`).
//! - [`SystemBase`]: wrapper for closure-style systems (the managed base-class
//!   role in DOTS `SystemBase`).
//! - [`SystemGroup`] / [`ComponentSystemGroup`]: the three top-level DOTS
//!   groups (Initialization / Simulation / Presentation).
//!
//! This is an **additive alignment layer**: it does not change the engine's
//! existing [`crate::World`] or scheduling; it only makes the DOTS system shape
//! explicit for new code to adopt.

use crate::World;

/// System group name constants (aligned with the DOTS top-level groups).
pub const INITIALIZATION_SYSTEM_GROUP: &str = "InitializationSystemGroup";
/// See [`INITIALIZATION_SYSTEM_GROUP`].
pub const SIMULATION_SYSTEM_GROUP: &str = "SimulationSystemGroup";
/// See [`INITIALIZATION_SYSTEM_GROUP`].
pub const PRESENTATION_SYSTEM_GROUP: &str = "PresentationSystemGroup";

/// DOTS `.Entities.ISystem`-shaped system interface.
///
/// Only [`on_update`](ISystem::on_update) is required; the other lifecycle
/// hooks default to no-ops.
pub trait ISystem: Send + Sync + 'static {
    /// Called when the system is first instantiated (DOTS: `OnCreate`).
    fn on_create(&mut self, _world: &mut World) {}

    /// Called when the system starts running or resumes from stopped (DOTS:
    /// `OnStartRunning`).
    fn on_start_running(&mut self, _world: &mut World) {}

    /// Called just before the system stops running (DOTS: `OnStopRunning`).
    fn on_stop_running(&mut self, _world: &mut World) {}

    /// Runs every update (DOTS: `OnUpdate`).
    fn on_update(&mut self, world: &mut World);

    /// Called when the system is destroyed (DOTS: `OnDestroy`).
    fn on_destroy(&mut self, _world: &mut World) {}
}

/// Use an ordinary closure as a system (a lightweight alternative to DOTS
/// `SystemBase`).
impl<F> ISystem for F
where
    F: FnMut(&mut World) + Send + Sync + 'static,
{
    fn on_update(&mut self, world: &mut World) {
        (self)(world)
    }
}

/// Named wrapper for a closure-style system (aligns with DOTS `SystemBase`:
/// managed, no hand-written trait impl).
pub struct SystemBase<F>
where
    F: FnMut(&mut World) + Send + Sync + 'static,
{
    func: F,
}

impl<F> SystemBase<F>
where
    F: FnMut(&mut World) + Send + Sync + 'static,
{
    /// Constructs a system from a closure.
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F> ISystem for SystemBase<F>
where
    F: FnMut(&mut World) + Send + Sync + 'static,
{
    fn on_update(&mut self, world: &mut World) {
        (self.func)(world)
    }
}

/// One subsystem slot inside a group; records whether its lifecycle hooks have
/// fired.
struct Slot {
    system: Box<dyn ISystem>,
    /// `on_create` has fired (filled in on the first `&mut World`).
    created: bool,
    /// `on_start_running` has fired and `on_stop_running` has not.
    started: bool,
}

/// DOTS `ComponentSystemGroup`-shaped system group: a container that updates
/// its subsystems in registration order.
pub struct SystemGroup {
    name: String,
    systems: Vec<Slot>,
    running: bool,
}

impl SystemGroup {
    /// Named system group.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            systems: Vec::new(),
            running: false,
        }
    }

    /// `InitializationSystemGroup` (DOTS top-level group).
    pub fn initialization() -> Self {
        Self::new(INITIALIZATION_SYSTEM_GROUP)
    }

    /// `SimulationSystemGroup` (DOTS top-level group).
    pub fn simulation() -> Self {
        Self::new(SIMULATION_SYSTEM_GROUP)
    }

    /// `PresentationSystemGroup` (DOTS top-level group).
    pub fn presentation() -> Self {
        Self::new(PRESENTATION_SYSTEM_GROUP)
    }

    /// Group name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Appends a subsystem (DOTS: `AddSystemToUpdateList`).
    ///
    /// There is no `&mut World` at registration time, so `on_create` is
    /// deferred until this group's first `update` (the earliest point a world
    /// is available); if the group is already running, the new system also
    /// receives `on_start_running` on the next `update`.
    pub fn add_system(&mut self, system: impl ISystem) -> &mut Self {
        self.systems.push(Slot {
            system: Box::new(system),
            created: false,
            started: false,
        });
        self
    }

    /// Number of subsystems.
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Whether there are no subsystems.
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    /// Whether the group is running (has fired `on_start_running`).
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Updates the whole group: every subsystem in order, maintaining the
    /// start/stop-running state transitions (DOTS: `ComponentSystemGroup.Update`).
    pub fn update(&mut self, world: &mut World) {
        // 1. Fill in `on_create` on the first world access
        //    (DOTS: OnCreate precedes OnStartRunning).
        for slot in &mut self.systems {
            if !slot.created {
                slot.system.on_create(world);
                slot.created = true;
            }
        }

        // 2. Start running: all systems when the group starts; only newly
        //    added ones when already running. (An empty group must still flip
        //    `running`, or `is_running()` would stay false forever.)
        for slot in &mut self.systems {
            if !slot.started {
                slot.system.on_start_running(world);
                slot.started = true;
            }
        }
        self.running = true;

        // 3. Update in registration order.
        for slot in &mut self.systems {
            slot.system.on_update(world);
        }
    }

    /// Stops the group (fires `on_stop_running`).
    pub fn stop_running(&mut self, world: &mut World) {
        if self.running {
            for slot in &mut self.systems {
                if slot.started {
                    slot.system.on_stop_running(world);
                    slot.started = false;
                }
            }
            self.running = false;
        }
    }

    /// Destroys every subsystem in the group (fires `on_destroy`).
    pub fn destroy(&mut self, world: &mut World) {
        self.stop_running(world);
        for slot in &mut self.systems {
            slot.system.on_destroy(world);
        }
        self.systems.clear();
    }
}

/// Alias for [`SystemGroup`] under the DOTS name (`ComponentSystemGroup`).
pub type ComponentSystemGroup = SystemGroup;

impl std::fmt::Debug for SystemGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemGroup")
            .field("name", &self.name)
            .field("systems", &self.systems.len())
            .field("running", &self.running)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn group_runs_systems_in_order_and_emits_hooks() {
        let mut world = World::new();
        let mut group = SystemGroup::simulation();
        assert_eq!(group.name(), SIMULATION_SYSTEM_GROUP);

        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        group.add_system(move |_w: &mut World| {
            c1.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!group.is_running());

        group.update(&mut world);
        group.update(&mut world);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert!(group.is_running());

        group.stop_running(&mut world);
        assert!(!group.is_running());
    }

    #[test]
    fn lifecycle_trait_defaults_are_noops() {
        struct Marker(*const AtomicUsize);
        // SAFETY: single-threaded test, pointer stays valid for the test body.
        unsafe impl Send for Marker {}
        unsafe impl Sync for Marker {}
        impl ISystem for Marker {
            fn on_update(&mut self, _world: &mut World) {
                unsafe { (*self.0).fetch_add(1, Ordering::SeqCst) };
            }
        }
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let mut world = World::new();
        let mut group = SystemGroup::presentation();
        group.add_system(Marker(&HITS));
        group.update(&mut world);
        assert_eq!(HITS.load(Ordering::SeqCst), 1);
    }

    /// Records lifecycle hooks in order into a shared log, for asserting call
    /// counts and ordering.
    struct Recorder {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        tag: &'static str,
    }

    impl Recorder {
        fn new(log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, tag: &'static str) -> Self {
            Self {
                log: log.clone(),
                tag,
            }
        }

        fn push(&self, hook: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{hook}", self.tag));
        }
    }

    impl ISystem for Recorder {
        fn on_create(&mut self, _world: &mut World) {
            self.push("create");
        }
        fn on_start_running(&mut self, _world: &mut World) {
            self.push("start");
        }
        fn on_stop_running(&mut self, _world: &mut World) {
            self.push("stop");
        }
        fn on_update(&mut self, _world: &mut World) {
            self.push("update");
        }
        fn on_destroy(&mut self, _world: &mut World) {
            self.push("destroy");
        }
    }

    #[test]
    fn create_and_start_run_once_before_the_first_updates() {
        let mut world = World::new();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut group = SystemGroup::simulation();
        group.add_system(Recorder::new(&log, "a"));

        group.update(&mut world);
        group.update(&mut world);

        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["a:create", "a:start", "a:update", "a:update"]
        );
    }

    #[test]
    fn system_added_while_running_gets_create_and_start_on_the_next_update() {
        let mut world = World::new();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut group = SystemGroup::simulation();
        group.add_system(Recorder::new(&log, "a"));
        group.update(&mut world);

        // A system hot-added while running has received no hooks before.
        group.add_system(Recorder::new(&log, "b"));
        group.update(&mut world);

        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["a:create", "a:start", "a:update", "b:create", "b:start", "a:update", "b:update"]
        );
        assert!(group.is_running());
    }

    #[test]
    fn stopping_does_not_touch_systems_that_never_started() {
        let mut world = World::new();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut group = SystemGroup::simulation();
        group.add_system(Recorder::new(&log, "a"));
        group.update(&mut world);

        group.add_system(Recorder::new(&log, "b"));
        group.stop_running(&mut world);
        // `a` was running, so it does receive stop; `b` never started, so it
        // should not.
        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["a:create", "a:start", "a:update", "a:stop"]
        );
        assert!(!group.is_running());
        group.update(&mut world);
        assert_eq!(
            log.lock().unwrap().as_slice(),
            [
                "a:create", "a:start", "a:update", "a:stop", "b:create", "a:start", "b:start",
                "a:update", "b:update"
            ]
        );
    }

    #[test]
    fn empty_group_still_transitions_to_running() {
        let mut world = World::new();
        let mut group = SystemGroup::initialization();
        group.update(&mut world);
        assert!(group.is_running());
        group.stop_running(&mut world);
        assert!(!group.is_running());
    }

    #[test]
    fn destroy_stops_then_destroys_every_system() {
        let mut world = World::new();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut group = SystemGroup::presentation();
        group.add_system(Recorder::new(&log, "a"));
        group.update(&mut world);
        group.destroy(&mut world);

        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["a:create", "a:start", "a:update", "a:stop", "a:destroy"]
        );
        assert!(group.is_empty());
        assert!(!group.is_running());
    }

    #[test]
    fn system_base_wraps_closure() {
        let mut world = World::new();
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let s = seen.clone();
        let mut base = SystemBase::new(move |_w: &mut World| {
            s.fetch_add(1, Ordering::SeqCst);
        });
        base.on_update(&mut world);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }
}
