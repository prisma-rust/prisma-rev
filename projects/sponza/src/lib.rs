//! Sponza 用户项目：使用引擎公开接口启动实时路径追踪并集成 RTXGI 调参面板。

#[cfg(feature = "player")]
pub mod tuning_ui;
#[cfg(feature = "player")]
pub use tuning_ui::SponzaTuningHook;

// ===========================================================================
// 玩家形态 (Standalone Player)
// ===========================================================================

#[cfg(feature = "player")]
pub fn build_app() -> prism_app::App {
    use prism_engine::render_settings::{GiMode, RenderMode, RenderSettings};

    prism_app::app()
        .with_subsystem(prism_app::Subsystem::Render)
        .with_render_settings(|settings: &mut RenderSettings| {
            settings.render_mode = RenderMode::PathTrace;
            settings.gi_mode = GiMode::Sharc;
            settings.pt_max_bounces = 4;
            settings.pt_max_iterations = 0;
            settings.pt_ray_max_distance = 1000.0;
        })
        .with_frame_hook(SponzaTuningHook::default())
}

// ===========================================================================
// 编辑器预览模块 (Editor Play Dynamic Module)
// ===========================================================================

#[cfg(feature = "preview-module")]
mod preview {
    use prism_game_api::GameWorld;

    struct PreviewState;

    fn new_state() -> PreviewState {
        PreviewState
    }

    fn tick_state(_state: &mut PreviewState, _world: &mut GameWorld<'_>, _dt: f32) {}

    prism_game_api::export_project! {
        state = PreviewState,
        init = new_state,
        tick = tick_state,
        log_name = "sponza",
    }
}

// ===========================================================================
// Android JNI 入口
// ===========================================================================

#[cfg(all(target_os = "android", feature = "player"))]
#[no_mangle]
pub extern "C" fn android_main(android_app: prism_app::AndroidApp) {
    prism_app::run_on_android(build_app, android_app).expect("fatal Sponza application error");
}
