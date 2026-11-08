//! #1013: the player entry point installs the logger before `App::build` mounts
//! the asset packages.
//!
//! Each integration test file is its own process, so the assertion about a
//! process-global logger is stable here.

use std::path::PathBuf;

fn temp_asset_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("prismrev-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(root.join("assets")).expect("create temp asset root");
    root
}

#[test]
fn player_entry_installs_the_logger_before_mounting_packages() {
    // Precondition: nothing has installed a logger in this process yet.
    assert_eq!(
        log::max_level(),
        log::LevelFilter::Off,
        "the test binary must start without a logger"
    );

    // Keep the app away from the real assets and the user's save directory.
    let root = temp_asset_root("startup-log");
    std::env::set_var("PRISMREV_ASSET_ROOT", &root);
    std::env::set_var(prism_engine::user_data::ENV_OVERRIDE, root.join("user"));

    let app = prism_app::App::with_config(prism_engine::config::AppConfig::default());
    drop(app);

    assert_ne!(
        log::max_level(),
        log::LevelFilter::Off,
        "the logger must be live while packages are mounted, or mount-time warnings are dropped"
    );
    assert!(
        prism_app::logging::is_initialized(),
        "App::with_config must run the logging bootstrap"
    );
}
