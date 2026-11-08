//! Cozy Fishing 库目标。
//!
//! 两种形态（`Cargo.toml` 的 `[features]`）：
//!
//! - **player**（默认）：桌面端由 `src/main.rs` 调用 [`build_app`]；Android 端由
//!   `android_main` 导出 JNI 入口（产出 `libfishing.so`）。这一形态链接引擎。
//! - **preview-module**：编辑器 Play 时加载的动态模块。只依赖 `prism-scene` /
//!   `prism-game-api`（plugin 后端），**不链接引擎**，跨 C ABI 读写宿主世界。
//!
//! 玩法代码（`game::*`）两种形态共用：`tick(&mut state, &mut GameWorld, dt)`
//! 里拿到的 `GameWorld` 在宿主内是零拷贝的借用，在模块里是 ABI 调用。

pub mod game;

// ===========================================================================
// 玩家形态
// ===========================================================================

/// 构造应用：引擎从 `assets/scenes.pak` 加载湖面场景，本项目注册玩法 system。
#[cfg(feature = "player")]
pub fn build_app() -> prism_app::App {
    // 音频子系统（`prism-app/audio`，feature 默认关，工程在 `[features] player`
    // 里打开）：一次性音效（上钩水花）走 `GameWorld::play_sound`，需要混音器。
    let mut app = prism_app::app()
        .with_subsystem(prism_app::Subsystem::Render)
        .with_subsystem(prism_app::Subsystem::Audio);

    app.add_system("game::world", world_system);
    app.add_system("game::fishing", fishing_system)
        .add_system("game::boat", boat_system);
    app
}

#[cfg(feature = "player")]
fn world_system(world: &mut prism_engine::ecs::World, dt: f32) {
    let mut state = world.remove_resource::<game::world::LakeWorld>();
    {
        let mut api = prism_game_api::GameWorld::host(world);
        game::world::tick(&mut state, &mut api, dt);
    }
    if let Some(state) = state {
        world.insert_resource(state);
    }
}

#[cfg(feature = "player")]
fn fishing_system(world: &mut prism_engine::ecs::World, dt: f32) {
    let mut state = world.remove_resource::<game::fishing::FishingState>();
    {
        let mut api = prism_game_api::GameWorld::host(world);
        game::fishing::tick(&mut state, &mut api, dt);
    }
    if let Some(state) = state {
        world.insert_resource(state);
    }
}

/// `boat::tick` 的 Player 适配器。
///
/// `BoatState` 是项目私有类型：这里临时放进 ECS resource，交给与插件形态
/// 相同的 `tick`，再放回去。引擎只认自己的通用 resource 机制。
#[cfg(feature = "player")]
fn boat_system(world: &mut prism_engine::ecs::World, dt: f32) {
    let mut state = world.remove_resource::<game::boat::BoatState>();
    {
        let mut api = prism_game_api::GameWorld::host(world);
        game::boat::tick(&mut state, &mut api, dt);
    }
    if let Some(state) = state {
        world.insert_resource(state);
    }
}

// ===========================================================================
// 编辑器加载的 preview 模块
// ===========================================================================

#[cfg(feature = "preview-module")]
mod preview {
    use prism_game_api::GameWorld;
    use prism_project_api::{guard, HostApi, ProjectApi, ABI_VERSION, INVALID_ARGUMENT, SUCCESS};

    use crate::game;

    /// 模块持有的全部状态。它**不会**出现在宿主世界里：宿主只拿到一个
    /// `*mut c_void`。三种玩法状态与 Player 形态里放在 ECS resource 中的是同一批
    /// 类型，只是插件形态下由模块自己持有。
    struct PreviewState {
        world: Option<game::world::LakeWorld>,
        fishing: Option<game::fishing::FishingState>,
        boat: Option<game::boat::BoatState>,
        scene_generation: Option<u64>,
    }

    pub unsafe extern "C" fn get_api(out: *mut ProjectApi) -> i32 {
        if out.is_null() {
            return INVALID_ARGUMENT;
        }
        unsafe {
            *out = ProjectApi {
                abi_version: ABI_VERSION,
                schema_fingerprint: prism_scene::builtin_registry().schema_fingerprint(),
                create: Some(create),
                tick: Some(tick),
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
                || host.schema_fingerprint != prism_scene::builtin_registry().schema_fingerprint()
            {
                return INVALID_ARGUMENT;
            }
            let state = Box::new(PreviewState {
                world: None,
                fishing: None,
                boat: None,
                scene_generation: None,
            });
            unsafe { *out = Box::into_raw(state).cast() };
            // 模块是独立 dylib，自带一份 `log`：不接上宿主的回调，工程里的
            // `log::info!` / `log::error!` 会静默丢掉。这一步之后它们进编辑器 Console。
            prism_game_api::install_host_logger(host);
            prism_game_api::install_host_panic_hook();
            log::info!("fishing: preview module started");
            SUCCESS
        })
    }

    unsafe extern "C" fn tick(
        host_api: *const HostApi,
        state: *mut std::ffi::c_void,
        dt: f32,
    ) -> i32 {
        // panic 不能跨 `extern "C"`：工程里的一个 `unwrap()` 会把编辑器一起
        // abort 掉。`guard` 就地捕获并返回失败码，宿主继续跑。
        guard(|| unsafe {
            let Some(state) = state.cast::<PreviewState>().as_mut() else {
                return INVALID_ARGUMENT;
            };
            if host_api.is_null() {
                return INVALID_ARGUMENT;
            }
            let host = &*host_api;

            if state.scene_generation != Some(host.scene_generation) {
                state.world = None;
                state.fishing = None;
                state.boat = None;
                state.scene_generation = Some(host.scene_generation);
            }

            let mut world =
                GameWorld::plugin(host, prism_scene::builtin_registry().type_registry());
            let first_tick = state.world.is_none();
            game::world::tick(&mut state.world, &mut world, dt);
            game::boat::tick(&mut state.boat, &mut world, dt);
            game::fishing::tick(&mut state.fishing, &mut world, dt);
            if first_tick {
                // 第一次 tick 会把 `lake_layers.json` 里的图层全部 spawn 出来：
                // 这条日志是「工程代码真的跑起来了」最直接的证据。
                let layers = state
                    .world
                    .as_ref()
                    .map(|lake| lake.layer_count())
                    .unwrap_or(0);
                log::info!("fishing: lake world rebuilt, {layers} layers");
            }
            SUCCESS
        })
    }

    unsafe extern "C" fn shutdown(state: *mut std::ffi::c_void) {
        if !state.is_null() {
            drop(unsafe { Box::from_raw(state.cast::<PreviewState>()) });
        }
    }
}

/// 编辑器解析的动态模块入口。
#[cfg(feature = "preview-module")]
#[no_mangle]
pub unsafe extern "C" fn prism_project_get_api(out: *mut prism_project_api::ProjectApi) -> i32 {
    unsafe { preview::get_api(out) }
}

// ===========================================================================
// Android JNI entry point
// ===========================================================================

/// GameActivity 加载本库后调用的入口。
///
/// GameActivity's native glue resolves this symbol via `dlsym`, so it must keep
/// C linkage and an unmangled name; a plain Rust `fn` is not exported.
#[cfg(all(target_os = "android", feature = "player"))]
#[no_mangle]
pub extern "C" fn android_main(android_app: prism_app::AndroidApp) {
    prism_app::run_on_android(build_app, android_app).expect("fatal application error");
}
