//! PrismaRev 用户游戏项目。
//!
//! 两种构建形态共用同一份 gameplay 代码：
//!
//! - **player**（默认）：桌面二进制 `prisma` + Android `libprisma.so`，
//!   直接链接 `prism-app` / `prism-engine`。
//! - **preview-module**：编辑器加载的动态模块，只依赖跨 ABI 的
//!   `prism-game-api` / `prism-project-api`，**不链接引擎**。
//!
//! 项目自己的类型（`intro::IntroState`）始终留在本 crate 里：引擎和编辑器
//! 只通过 opaque 句柄与通用组件 schema 与项目交互。

pub mod intro;

// ===========================================================================
// 静态 Player
// ===========================================================================

/// 构造应用：引擎从 `assets/scenes.pak` 的路径清单加载 intro 场景实体
/// （由 `assets/scenes/intro.scene.json` cook 而来），本项目只注册
/// `intro::tick_system` 驱动动画。
#[cfg(feature = "player-core")]
pub fn build_app() -> prism_app::App {
    // 音频子系统（`prism-app/audio`，默认关）不在这里启用：本项目没有 AudioSource，
    // 开了也只是建一个空引擎 + 解码线程。要放声音时同时打开 feature 与这一行。
    let mut app = prism_app::app().with_subsystem(prism_app::Subsystem::Render);
    app.add_system("intro::tick", intro::tick_system);
    app
}

// ===========================================================================
// 编辑器加载的 preview 模块
// ===========================================================================

#[cfg(feature = "preview-module")]
mod preview {
    use prism_game_api::GameWorld;

    use crate::intro::{self, IntroState};

    /// 项目模块持有的全部状态。它**不会**出现在宿主世界里：宿主只拿到一个
    /// `*mut c_void`。
    struct PreviewState {
        intro: Option<IntroState>,
    }

    fn new_state() -> PreviewState {
        PreviewState { intro: None }
    }

    fn tick_state(state: &mut PreviewState, world: &mut GameWorld<'_>, dt: f32) {
        intro::tick(&mut state.intro, world, dt);
    }

    prism_game_api::export_project! {
        state = PreviewState,
        init = new_state,
        tick = tick_state,
        log_name = "prisma",
    }
}

// ===========================================================================
// Android JNI entry point
// ===========================================================================

/// GameActivity 加载本库后调用的入口。
///
/// GameActivity's native glue resolves this symbol with `dlsym`, so it must
/// keep C linkage and an unmangled name; a plain Rust `fn` is not exported.
#[cfg(all(target_os = "android", feature = "player-core"))]
#[no_mangle]
pub extern "C" fn android_main(android_app: prism_app::AndroidApp) {
    // `run_on_android` stages APK assets and the launch config before calling
    // `build_app`, so the engine can use the same `assets/...` paths as desktop.
    prism_app::run_on_android(build_app, android_app).expect("fatal application error");
}
