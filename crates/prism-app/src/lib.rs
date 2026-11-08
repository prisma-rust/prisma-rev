//! PrismaRev 平台应用层
//!
//! 拥有事件循环、窗口和帧编排——连接平台输入、引擎（ECS/逻辑）和
//! 渲染器（Vulkan）的胶水层。
//!
//! ## 窗口后端（feature）
//!
//! - `winit-backend`（默认）：winit 事件循环 + `ApplicationHandler`。
//! - `win32-native`（默认关，仅 Windows）：Win32 原生窗口 + 自己的消息循环
//!   （[`App::run_win32`]）。启用它必须关掉 `winit-backend`，否则本 crate
//!   里仍会有 winit 代码参与编译（体积省不下来）。两个都开时以 winit 为准。
//!
//! 两个后端共用同一份引擎/渲染逻辑（[`App::tick_frame`] / [`App::shutdown`]），
//! 差别只在「窗口怎么来、事件怎么派发」。
//!
//! ## 用户项目接入（完全 ECS 体验）
//!
//! 引擎初始化、事件循环、渲染全部由本层包办；用户项目通过
//! [`app`] 拿到 [`App`]，用 builder 方法注册自己的 ECS 内容后
//! 启动事件循环：
//!
//! ```no_run
//! use prism_app::app;
//!
//! fn main() {
//!     let mut app = app();
//!     app.insert_resource(MyState::default());
//!     app.add_system("my::system", |world, dt| { /* ... */ });
//!     app.run();
//! }
//! # struct MyState;
//! # impl Default for MyState { fn default() -> Self { MyState } }
//! ```
//!
//! - [`App::add_system`] / [`App::insert_resource`]：注册 ECS 系统与资源；
//! - [`App::engine_mut`]：完全 ECS 访问（world / schedule / timer）；
//! - [`App::run`]：桌面入口（自建 EventLoop / Win32 消息循环）；
//! - [`App::run_on`]：Android 入口（外部 winit EventLoop，仅 `winit-backend`）。
//!
//! UI 用引擎自研 ECS 组件 UI（`Node` / `Style` / `Text`），随主渲染
//! 通道每帧绘制。
//!
//! ## 中性扩展点（调试宿主 / 未来宿主）
//!
//! 本 crate 不依赖任何具体 UI 框架。外部宿主可
//! 通过 [`App::with_frame_hook`] 注入 [`FrameHook`]：
//!
//! - [`FrameHook::on_window_event`]：抢先消费窗口事件（返回 `true` 表示吃掉）；
//! - [`FrameHook::on_tick`]：每帧访问 world / 渲染设置 / 统计，并可经
//!   [`RenderShared::send_overlay_message`] 向渲染线程投递**类型擦除**的
//!   overlay 消息；
//! - [`FrameHook::overlay`]：提供渲染线程侧的
//!   `prism_render::external_overlay::SwapchainOverlay` 工厂。
//!
//! 加 UI 功能时**不要**把具体 UI 框架依赖加回本 crate——扩展
//! hook 接口即可。
//!
//! ## 架构（多线程）
//!
//! ```text
//! 主线程                       渲染线程
//!   ────────────                        ────────────
//!   about_to_wait:                       loop:
//!     engine.fixed_update × N              wait_for_packet()
//!     engine.update                        begin_frame()
//!     engine.late_update                   execute()
//!     audio.update                         present()    ← 垂直同步在此
//!     extract_frame_packet ──packet──►   │
//!     frame_hook.on_tick ──overlay_msg──►│
//!
//! resumed(): 主线程创建窗口（快速，~数毫秒）→ 生成 渲染 线程（内部异步
//!   构建 GraphRenderer，~数百毫秒，不阻塞窗口事件）→ 立即返回
//! suspended(): stop 渲染 线程（销毁渲染器、释放 Vulkan 表面）
//! ```
//!
//! 启动期资产解析经 asset_requests/asset_results 通道完成：主线程 CPU 段
//! 准备上传请求，渲染线程 GPU 段上传并回传句柄。

#[cfg(any(target_os = "android", test))]
mod android_assets;
mod app;
#[cfg(feature = "audio")]
mod audio_decode_runner;
mod bench_report;
mod debug_hud;
mod hook;
mod io_runner;
pub mod logging;
#[cfg(feature = "physics")]
mod physics_runner;
/// Main-thread physics driver. Exposed so headless scenario tests can step the
/// same runtime the app uses without creating a window.
#[cfg(feature = "physics")]
pub use physics_runner::PhysicsRuntime;
/// Kinematic character moves (#560 P2): write requests into
/// `CharacterMoveRequests`, read the corrected moves back from
/// `CharacterMoveResults` after the step.
#[cfg(feature = "physics")]
pub use physics_runner::{
    CharacterMoveInput, CharacterMoveRequest, CharacterMoveRequests, CharacterMoveResult,
    CharacterMoveResults, CharacterMoveSettings,
};
#[cfg(feature = "audio")]
pub mod one_shot;
mod render_graph_report;
mod render_runner;
mod render_shared;
#[cfg(feature = "audio")]
pub mod scene_audio;
mod screenshot;

use prism_engine::config::AppConfig;

#[cfg(all(feature = "win32-native", not(windows)))]
compile_error!("prism-app: `win32-native` 只在 Windows 目标上有效");

#[cfg(not(any(feature = "winit-backend", all(windows, feature = "win32-native"))))]
compile_error!(
    "prism-app: 至少启用一个窗口后端（`winit-backend`，或 Windows 上的 `win32-native`）"
);

pub use app::{App, Subsystem};
pub use debug_hud::DebugHud;
pub use hook::FrameHook;
pub use render_shared::{FrameImage, RenderShared, RenderStats};

/// 用户项目 Android 入口所需的平台句柄；具体窗口库类型由 app 层封装。
#[cfg(all(target_os = "android", feature = "winit-backend"))]
pub use winit::platform::android::activity::AndroidApp;

/// 把 APK assets 复制到内部存储并切换 cwd。
///
/// [`run_on_android`] 会在构造 [`App`] 前自动调用；若用户项目需要更早访问
/// APK 资源，也可自行调用。
#[cfg(target_os = "android")]
pub use android_assets::init_android_assets;

/// 从应用资源目录读取配置；文件系统访问属于应用层，解析逻辑由 engine
/// 的 `AppConfig::from_ini` 提供。
///
/// 优先 `assets/settings.ini`；不存在时回退读旧的 `assets/settings.toml`
/// （并提示迁移）。路径经 `prism_engine::asset_paths` 解析（cwd / 可执行文件
/// 目录 / 父目录 / `PRISMREV_ASSET_ROOT`），因此从任意目录启动 exe 都能读到配置。
pub fn load_config() -> AppConfig {
    for (name, legacy) in [("settings.ini", false), ("settings.toml", true)] {
        let Some(path) = prism_engine::asset_paths::asset_path(name) else {
            return AppConfig::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if legacy {
                    log::warn!(
                        "{} is deprecated: run `prism-build-pipeline build-scenes` to migrate \
                         it to settings.ini",
                        path.display()
                    );
                }
                return AppConfig::from_ini(&text);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                log::warn!("{name} ({}): {error} - using defaults", path.display());
                return AppConfig::default();
            }
        }
    }
    AppConfig::default()
}

/// 创建应用并完成全部引擎初始化（配置加载、场景、运行时 init）。
///
/// 用户项目在此之后注册 ECS 内容（`add_system` / `insert_resource` /
/// `engine_mut`），然后 [`App::run`]（桌面）或 [`App::run_on`]（Android）。
/// 编辑器等宿主可用 [`App::with_frame_hook`] 注入 [`FrameHook`]。
pub fn app() -> App {
    App::with_config(load_config())
}

// ===========================================================================
// Android entry helper
// ===========================================================================

/// 在 Android 上启动应用（供用户项目的 `android_main` 调用）。
///
/// 用户项目传入一个构造 [`App`] 的闭包，而不是已经构造好的 `App`：
/// 本函数会先把 APK assets 复制到内部存储、切换 cwd、注入启动配置，
/// 然后才调用闭包。这样闭包里的 [`app`] 能按与桌面完全相同的
/// `assets/...` 路径加载 `scenes.pak`、`settings.ini` 和环境贴图。
///
/// 用户项目的 cdylib 导出 `#[no_mangle] fn android_main(...)`，在此调用
/// 本函数，JNI 样板（android_logger、crash handler、AssetManager 拷贝、
/// EventLoop 构建）由本函数包办：
///
/// ```no_run
/// use prism_app::{app, run_on_android};
///
/// fn build_app() -> prism_app::App {
///     let mut app = app();
///     // ...注册 ECS 内容（add_system / insert_resource）...
///     app
/// }
///
/// #[cfg(target_os = "android")]
/// #[no_mangle]
/// fn android_main(android_app: prism_app::AndroidApp) {
///     run_on_android(build_app, android_app).expect("fatal application error");
/// }
/// ```
///
/// 与桌面 [`App::run`] 等价，但使用外部传入的 winit EventLoop。
#[cfg(all(target_os = "android", feature = "winit-backend"))]
pub fn run_on_android<F>(
    build_app: F,
    android_app: winit::platform::android::activity::AndroidApp,
) -> anyhow::Result<()>
where
    F: FnOnce() -> App,
{
    // The engine is filesystem-based; stage the APK asset tree before App
    // construction so `assets/scenes.pak` etc. resolve exactly like desktop.
    // This also injects the hub's launch config, which carries `log_level`.
    let data_dir = android_assets::init_android_assets(&android_app)?;
    inject_launch_config(&data_dir);

    // android_logger reads its level once at init; apply the launch override
    // first (default: Debug, matching the historical behavior).
    let level = prism_engine::launch_config::LaunchConfig::from_env()
        .log_level_filter()
        .unwrap_or(log::LevelFilter::Debug);
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(level)
            .with_tag("PrismaRev"),
    );

    log::info!("PrismaRev Android starting");

    prism_engine::crash_dialog::register_android_app(&android_app);
    struct AndroidAppGuard;
    impl Drop for AndroidAppGuard {
        fn drop(&mut self) {
            prism_engine::crash_dialog::unregister_android_app();
        }
    }
    let _app_guard = AndroidAppGuard;

    let app = build_app();
    let event_loop = prism_platform::build_android_event_loop(android_app)?;

    app.run_on(event_loop)
}

/// Bridge the hub's `filesDir/launch_config.json` into the same env var used
/// by desktop. On re-entry, always read the latest file (or clear the env
/// var if the file was removed) so stale launch configs do not persist across runs.
#[cfg(any(target_os = "android", test))]
fn inject_launch_config(data_dir: &std::path::Path) {
    let path = data_dir.join("launch_config.json");
    match std::fs::read_to_string(&path) {
        Ok(json) => std::env::set_var(prism_engine::launch_config::ENV_KEY, json),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::env::remove_var(prism_engine::launch_config::ENV_KEY);
        }
        Err(error) => log::warn!("read {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod launch_config_reentrancy_tests {
    use super::*;

    #[test]
    fn inject_launch_config_updates_and_cleans_up_on_reentry() {
        let temp_dir = std::env::temp_dir().join(format!(
            "prism_test_launch_config_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let cfg_path = temp_dir.join("launch_config.json");

        // Initial launch with config A
        std::fs::write(&cfg_path, r#"{"scene":"intro","log_level":"debug"}"#).unwrap();
        inject_launch_config(&temp_dir);
        assert_eq!(
            std::env::var(prism_engine::launch_config::ENV_KEY).as_deref(),
            Ok(r#"{"scene":"intro","log_level":"debug"}"#)
        );

        // Re-entry with updated config B
        std::fs::write(&cfg_path, r#"{"scene":"gameplay","log_level":"warn"}"#).unwrap();
        inject_launch_config(&temp_dir);
        assert_eq!(
            std::env::var(prism_engine::launch_config::ENV_KEY).as_deref(),
            Ok(r#"{"scene":"gameplay","log_level":"warn"}"#)
        );

        // Re-entry when launch config is deleted
        std::fs::remove_file(&cfg_path).unwrap();
        inject_launch_config(&temp_dir);
        assert!(std::env::var(prism_engine::launch_config::ENV_KEY).is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
