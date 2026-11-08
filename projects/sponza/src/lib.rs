//! Sponza 用户项目：使用引擎公开接口启动实时路径追踪。

use prism_engine::render_settings::{RenderMode, RenderSettings};

/// 构造 Sponza 路径追踪应用。
///
/// 场景资产由运行目录下的资源包提供；请先按项目文档构建 Sponza `.pak`。
pub fn build_app() -> prism_app::App {
    prism_app::app()
        .with_subsystem(prism_app::Subsystem::Render)
        .with_render_settings(|settings: &mut RenderSettings| {
            settings.render_mode = RenderMode::PathTrace;
            settings.pt_max_bounces = 4;
            settings.pt_max_iterations = 0;
            settings.pt_ray_max_distance = 1000.0;
        })
}

#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(android_app: prism_app::AndroidApp) {
    prism_app::run_on_android(build_app, android_app).expect("fatal Sponza application error");
}
