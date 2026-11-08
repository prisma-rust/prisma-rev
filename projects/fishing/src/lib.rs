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

    app.add_game_system::<game::world::LakeWorld, _>("game::world", game::world::tick)
        // `fishing::tick` reads the boat transform, so the boat must run first. The
        // preview module calls the same ticks in this order; keep the two in sync.
        .add_game_system::<game::boat::BoatState, _>("game::boat", game::boat::tick)
        .add_game_system::<game::fishing::FishingState, _>("game::fishing", game::fishing::tick);
    app
}

// ===========================================================================
// 编辑器加载的 preview 模块
// ===========================================================================

#[cfg(feature = "preview-module")]
mod preview {
    use prism_game_api::GameWorld;

    use crate::game;

    /// Every piece of gameplay state the module owns. It never reaches the host
    /// world: the host only holds a `*mut c_void`. The player form hands the same
    /// three types to `add_game_system`, so both forms tick identical state.
    struct PreviewState {
        world: Option<game::world::LakeWorld>,
        fishing: Option<game::fishing::FishingState>,
        boat: Option<game::boat::BoatState>,
    }

    fn new_state() -> PreviewState {
        PreviewState {
            world: None,
            fishing: None,
            boat: None,
        }
    }

    fn tick_state(state: &mut PreviewState, world: &mut GameWorld<'_>, dt: f32) {
        let first_tick = state.world.is_none();
        game::world::tick(&mut state.world, world, dt);
        game::boat::tick(&mut state.boat, world, dt);
        game::fishing::tick(&mut state.fishing, world, dt);
        if first_tick {
            // 第一次 tick 会把 `data/lake_layers.json` 里的图层全部 spawn 出来：
            // 这条日志是「工程代码真的跑起来了」最直接的证据。
            let layers = state
                .world
                .as_ref()
                .map(|lake| lake.layer_count())
                .unwrap_or(0);
            log::info!("fishing: lake world rebuilt, {layers} layers");
        }
    }

    prism_game_api::export_project! {
        state = PreviewState,
        init = new_state,
        tick = tick_state,
        log_name = "fishing",
    }
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
