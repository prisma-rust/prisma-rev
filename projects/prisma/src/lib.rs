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
    use std::sync::OnceLock;

    use prism_game_api::GameWorld;
    use prism_project_api::{guard, HostApi, ProjectApi, ABI_VERSION, INVALID_ARGUMENT, SUCCESS};

    use crate::intro::{self, IntroState};

    /// 项目模块持有的全部状态。
    ///
    /// 它**不会**出现在宿主世界里：宿主只拿到一个 `*mut c_void`。
    struct PreviewState {
        intro: Option<IntroState>,
        scene_generation: Option<u64>,
    }

    /// 组件 schema：编码组件时生成 RTTIBin 的类型表。只需建一次。
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
                shutdown: Some(shutdown),
            };
        }
        SUCCESS
    }

    unsafe extern "C" fn create(host: *const HostApi, out: *mut *mut std::ffi::c_void) -> i32 {
        guard(|| unsafe {
            if host.is_null() || out.is_null() {
                return INVALID_ARGUMENT;
            }
            let host = &*host;
            if host.abi_version != ABI_VERSION
                || host.schema_fingerprint != registry().schema_fingerprint()
            {
                return INVALID_ARGUMENT;
            }
            let state = Box::new(PreviewState {
                intro: None,
                scene_generation: None,
            });
            *out = Box::into_raw(state).cast();
            // The module is its own dylib with its own `log` instance. Without
            // this bridge, `log::info!` in project code never reaches the host
            // console, and a panic would unwind across `extern "C"`.
            prism_game_api::install_host_logger(host);
            prism_game_api::install_host_panic_hook();
            log::info!("prisma: preview module started");
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

            // 场景重载会让所有实体句柄失效：丢掉绑定的状态，下一帧按 Name 重绑。
            if state.scene_generation != Some(host.scene_generation) {
                state.intro = None;
                state.scene_generation = Some(host.scene_generation);
            }

            let mut world = GameWorld::plugin(host, registry().type_registry());
            intro::tick(&mut state.intro, &mut world, dt);
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
/// GameActivity's native glue resolves this symbol with `dlsym`, so it must
/// keep C linkage and an unmangled name; a plain Rust `fn` is not exported.
#[cfg(all(target_os = "android", feature = "player-core"))]
#[no_mangle]
pub extern "C" fn android_main(android_app: prism_app::AndroidApp) {
    // `run_on_android` stages APK assets and the launch config before calling
    // `build_app`, so the engine can use the same `assets/...` paths as desktop.
    prism_app::run_on_android(build_app, android_app).expect("fatal application error");
}
