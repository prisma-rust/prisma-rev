//! EchoWell user project.
//!
//! The engine owns the generic ECS, fixed-step scheduling, 2D sprite/tilemap
//! rendering, particles, lighting, input and audio services. This project owns
//! the platformer rules, room layout, ability flags and HUD.

#![deny(warnings)]

pub mod game;

#[cfg(feature = "player")]
pub fn build_app() -> prism_app::App {
    let mut app = prism_app::app()
        .with_subsystem(prism_app::Subsystem::Render)
        .with_subsystem(prism_app::Subsystem::Audio);
    app.add_fixed_game_system::<game::platformer::PlatformerState, _>(
        "echowell::platformer_fixed",
        platformer_fixed_system,
    );
    app.add_game_system::<game::platformer::PlatformerState, _>(
        "echowell::platformer_frame",
        platformer_frame_system,
    );
    app
}

#[cfg(feature = "player")]
fn platformer_fixed_system(
    state: &mut Option<game::platformer::PlatformerState>,
    world: &mut prism_game_api::GameWorld<'_>,
    dt: f32,
) {
    game::platformer::fixed_tick(state, world, dt);
}

#[cfg(feature = "player")]
fn platformer_frame_system(
    state: &mut Option<game::platformer::PlatformerState>,
    world: &mut prism_game_api::GameWorld<'_>,
    dt: f32,
) {
    game::platformer::frame_tick(state, world, dt);
}

#[cfg(feature = "preview-module")]
mod preview {
    use std::sync::OnceLock;

    use prism_game_api::GameWorld;
    use prism_project_api::{guard, HostApi, ProjectApi, ABI_VERSION, INVALID_ARGUMENT, SUCCESS};

    use crate::game;

    struct PreviewState {
        platformer: Option<game::platformer::PlatformerState>,
        scene_generation: Option<u64>,
    }

    fn registry() -> &'static prism_scene::ComponentRegistry {
        static REGISTRY: OnceLock<prism_scene::ComponentRegistry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            let mut registry = prism_scene::ComponentRegistry::new();
            prism_scene::register_builtin_components(&mut registry);
            registry
        })
    }

    pub unsafe extern "C" fn get_api(out: *mut ProjectApi) -> i32 {
        if out.is_null() {
            return INVALID_ARGUMENT;
        }
        unsafe {
            *out = ProjectApi {
                abi_version: ABI_VERSION,
                schema_fingerprint: registry().schema_fingerprint(),
                create: Some(create),
                tick: Some(tick),
                fixed_update: Some(fixed_update),
                shutdown: Some(shutdown),
            };
        }
        SUCCESS
    }

    unsafe extern "C" fn create(host: *const HostApi, out: *mut *mut std::ffi::c_void) -> i32 {
        guard(|| {
            if host.is_null() || out.is_null() {
                return INVALID_ARGUMENT;
            }
            let host = unsafe { &*host };
            if host.abi_version != ABI_VERSION
                || host.schema_fingerprint != registry().schema_fingerprint()
            {
                return INVALID_ARGUMENT;
            }
            let state = Box::new(PreviewState {
                platformer: None,
                scene_generation: None,
            });
            unsafe { *out = Box::into_raw(state).cast() };
            prism_game_api::install_host_logger(host);
            prism_game_api::install_host_panic_hook();
            log::info!("echowell: preview module started");
            SUCCESS
        })
    }

    unsafe extern "C" fn tick(
        host_api: *const HostApi,
        state: *mut std::ffi::c_void,
        dt: f32,
    ) -> i32 {
        guard(|| unsafe {
            let Some(state) = state.cast::<PreviewState>().as_mut() else {
                return INVALID_ARGUMENT;
            };
            if host_api.is_null() {
                return INVALID_ARGUMENT;
            }
            let host = &*host_api;
            if state.scene_generation != Some(host.scene_generation) {
                state.platformer = None;
                state.scene_generation = Some(host.scene_generation);
            }
            let mut world = GameWorld::plugin(host, registry().type_registry());
            game::platformer::frame_tick(&mut state.platformer, &mut world, dt);
            SUCCESS
        })
    }

    unsafe extern "C" fn fixed_update(
        host_api: *const HostApi,
        state: *mut std::ffi::c_void,
        dt: f32,
    ) -> i32 {
        fixed_tick(host_api, state, dt)
    }

    unsafe extern "C" fn fixed_tick(
        host_api: *const HostApi,
        state: *mut std::ffi::c_void,
        dt: f32,
    ) -> i32 {
        guard(|| unsafe {
            let Some(state) = state.cast::<PreviewState>().as_mut() else {
                return INVALID_ARGUMENT;
            };
            if host_api.is_null() {
                return INVALID_ARGUMENT;
            }
            let host = &*host_api;
            if state.scene_generation != Some(host.scene_generation) {
                state.platformer = None;
                state.scene_generation = Some(host.scene_generation);
            }
            let mut world = GameWorld::plugin(host, registry().type_registry());
            game::platformer::fixed_tick(&mut state.platformer, &mut world, dt);
            SUCCESS
        })
    }

    unsafe extern "C" fn shutdown(state: *mut std::ffi::c_void) {
        if !state.is_null() {
            drop(unsafe { Box::from_raw(state.cast::<PreviewState>()) });
        }
    }
}

#[cfg(feature = "preview-module")]
#[no_mangle]
pub unsafe extern "C" fn prism_project_get_api(out: *mut prism_project_api::ProjectApi) -> i32 {
    unsafe { preview::get_api(out) }
}

#[cfg(all(target_os = "android", feature = "player"))]
#[no_mangle]
pub extern "C" fn android_main(android_app: prism_app::AndroidApp) {
    prism_app::run_on_android(build_app, android_app).expect("fatal application error");
}
