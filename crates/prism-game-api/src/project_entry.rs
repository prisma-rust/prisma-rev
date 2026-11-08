//! The project-module entry boilerplate, generated once per project.
//!
//! Every preview module has to export the same shape: `prism_project_get_api`,
//! `create` / `tick` / `shutdown`, ABI + schema validation, the host log/panic
//! bridge and the scene-generation reset. That is the integration contract, not
//! gameplay, so projects only supply their state type plus `init` / `tick`.

/// Generate the whole preview-module entry point for a project.
///
/// Invoke it inside a module that also defines the functions you pass in; the
/// macro exports the `prism_project_get_api` symbol for that module.
///
/// ```ignore
/// mod preview {
///     use prism_game_api::GameWorld;
///
///     struct State { /* … */ }
///     fn init() -> State { State { /* … */ } }
///     fn tick(state: &mut State, world: &mut GameWorld<'_>, dt: f32) {
///         // gameplay
///     }
///
///     prism_game_api::export_project! {
///         state = State,
///         init = init,
///         tick = tick,
///         log_name = "my-game",
///     }
/// }
/// ```
///
/// `init` is called once at startup and again whenever the host reports a new
/// scene generation, because entity handles from the previous scene are gone.
/// Pass `shutdown = path` when the state needs to tear something down before it
/// is dropped.
#[cfg(feature = "plugin")]
#[macro_export]
macro_rules! export_project {
    (
        state = $state:ty,
        init = $init:path,
        tick = $tick:path,
        log_name = $log_name:literal
        $(, shutdown = $shutdown:path)?
        $(,)?
    ) => {
        struct __PrismProjectRuntime {
            state: Option<$state>,
            scene_generation: Option<u64>,
        }

        impl __PrismProjectRuntime {
            fn new() -> Self {
                Self {
                    state: None,
                    scene_generation: None,
                }
            }

            /// Rebuilds the project state when the host loaded a new scene.
            fn sync_scene(&mut self, generation: u64) {
                if self.scene_generation != Some(generation) {
                    self.state = None;
                    self.scene_generation = Some(generation);
                }
                self.state.get_or_insert_with($init);
            }
        }

        #[no_mangle]
        pub unsafe extern "C" fn prism_project_get_api(
            out: *mut $crate::__project_api::ProjectApi,
        ) -> i32 {
            // SAFETY: the host hands us a valid, writable `ProjectApi`.
            $crate::export_project!(@get_api out)
        }

        /// Internal: writes the function table; kept callable for tests.
        pub unsafe extern "C" fn get_api(out: *mut $crate::__project_api::ProjectApi) -> i32 {
            $crate::export_project!(@get_api out)
        }

        #[allow(clippy::macro_metavars_in_unsafe)]
        unsafe extern "C" fn create(
            host: *const $crate::__project_api::HostApi,
            out: *mut *mut core::ffi::c_void,
        ) -> i32 {
            $crate::__project_api::guard(|| unsafe {
                if host.is_null() || out.is_null() {
                    return $crate::__project_api::INVALID_ARGUMENT;
                }
                let host = &*host;
                if host.abi_version != $crate::__project_api::ABI_VERSION
                    || host.schema_fingerprint != $crate::builtin_schema_fingerprint()
                {
                    return $crate::__project_api::INVALID_ARGUMENT;
                }
                *out = Box::into_raw(Box::new(__PrismProjectRuntime::new())).cast();
                // The module is its own dylib with its own `log` instance and
                // panic hook; without these the host console sees nothing.
                $crate::install_host_logger(host);
                $crate::install_host_panic_hook();
                log::info!("{}: preview module started", $log_name);
                $crate::__project_api::SUCCESS
            })
        }

        #[allow(clippy::macro_metavars_in_unsafe)]
        unsafe extern "C" fn tick(
            host_api: *const $crate::__project_api::HostApi,
            state: *mut core::ffi::c_void,
            dt: f32,
        ) -> i32 {
            $crate::__project_api::guard(|| unsafe {
                let Some(runtime) = state.cast::<__PrismProjectRuntime>().as_mut() else {
                    return $crate::__project_api::INVALID_ARGUMENT;
                };
                if host_api.is_null() {
                    return $crate::__project_api::INVALID_ARGUMENT;
                }
                let host = &*host_api;
                runtime.sync_scene(host.scene_generation);
                let mut world =
                    $crate::GameWorld::plugin(host, $crate::builtin_registry().type_registry());
                let state = runtime
                    .state
                    .as_mut()
                    .expect("sync_scene always fills the project state");
                $tick(state, &mut world, dt);
                $crate::__project_api::SUCCESS
            })
        }

        unsafe extern "C" fn shutdown(state: *mut core::ffi::c_void) {
            if state.is_null() {
                return;
            }
            #[allow(unused_mut)]
            let mut runtime = unsafe { Box::from_raw(state.cast::<__PrismProjectRuntime>()) };
            $(
                if let Some(state) = runtime.state.as_mut() {
                    $shutdown(state);
                }
            )?
            drop(runtime);
        }
    };

    (@get_api $out:expr) => {{
        let out = $out;
        if out.is_null() {
            return $crate::__project_api::INVALID_ARGUMENT;
        }
        unsafe {
            *out = $crate::__project_api::ProjectApi {
                abi_version: $crate::__project_api::ABI_VERSION,
                schema_fingerprint: $crate::builtin_schema_fingerprint(),
                create: Some(create),
                tick: Some(tick),
                shutdown: Some(shutdown),
            };
        }
        $crate::__project_api::SUCCESS
    }};
}

#[cfg(all(test, feature = "plugin"))]
mod tests {
    use crate::__project_api::{ProjectApi, SUCCESS};

    struct State {
        ticks: u32,
    }

    fn init() -> State {
        State { ticks: 0 }
    }

    fn tick_state(state: &mut State, _world: &mut crate::GameWorld<'_>, _dt: f32) {
        state.ticks += 1;
    }

    crate::export_project! {
        state = State,
        init = init,
        tick = tick_state,
        log_name = "entry-test",
    }

    #[test]
    fn generated_table_reports_the_builtin_schema() {
        let mut api = std::mem::MaybeUninit::<ProjectApi>::uninit();
        let code = unsafe { get_api(api.as_mut_ptr()) };
        assert_eq!(code, SUCCESS);
        let api = unsafe { api.assume_init() };
        assert_eq!(api.abi_version, crate::__project_api::ABI_VERSION);
        assert_eq!(api.schema_fingerprint, crate::builtin_schema_fingerprint());
        assert!(api.create.is_some());
        assert!(api.tick.is_some());
        assert!(api.shutdown.is_some());
    }
}
