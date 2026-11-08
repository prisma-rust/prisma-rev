//! Logging must exist before `App::build` mounts asset packages.
//!
//! `App::build` used to mount `assets/*.pak` and load the startup scene before
//! `run` called `env_logger::try_init`, so every "package missing / broken"
//! warning was dropped and a bad package looked like a rendering bug (#1013).

#[test]
fn build_installs_logging_before_mounting_assets() {
    // Nothing has initialized `log` in this test binary yet.
    assert_eq!(
        log::max_level(),
        log::LevelFilter::Off,
        "the test binary must start without a logger"
    );

    // An asset root with no package: `build` has warnings to emit here.
    let root = std::env::temp_dir().join("prismrev-startup-logging");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("assets")).expect("create temp asset root");
    std::env::set_var("PRISMREV_ASSET_ROOT", &root);

    let _app = prism_app::App::hosted(prism_engine::config::AppConfig::default());

    std::env::remove_var("PRISMREV_ASSET_ROOT");
    assert_ne!(
        log::max_level(),
        log::LevelFilter::Off,
        "App::build must install the logger before the package mount warns"
    );
}
