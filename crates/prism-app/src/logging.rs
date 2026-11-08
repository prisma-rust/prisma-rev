//! Player-side logging bootstrap.
//!
//! The logger must be live **before** [`crate::App::build`] mounts the asset
//! packages and loads the startup scene: every `warn!` / `error!` emitted there
//! (broken package, missing path manifest, stale cook stamp) would otherwise be
//! dropped, which is how a stale `.pak` could render the wrong thing silently
//! (#1013).
//!
//! Hosts that own the process logger keep that ownership. The egui editor wraps
//! `env_logger` in its own tee logger, and Android installs `android_logger`
//! inside [`crate::run_on_android`] before the `App` is built; both call neither
//! of the entry points below, and `try_init` is a no-op when a logger already
//! exists.

use std::sync::atomic::{AtomicBool, Ordering};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Whether a player entry point has run the logging bootstrap in this process.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Relaxed)
}

/// Copy the launch config's `log_level` into `RUST_LOG`.
///
/// Must run before the logger reads the environment: `env_logger` and
/// `android_logger` parse their filter exactly once, at initialization.
pub(crate) fn apply_launch_log_level() {
    let launch = prism_engine::launch_config::LaunchConfig::from_env();
    if let Some(level) = &launch.log_level {
        std::env::set_var("RUST_LOG", level);
    }
}

/// Install the player logger as early as possible. Idempotent.
///
/// Called from [`crate::App::with_config`] (the player entry) and the desktop
/// event-loop entry points, so an `App` built through any of them has a logger
/// before it mounts anything.
pub(crate) fn init_player_logging() {
    apply_launch_log_level();
    // Android installs `android_logger` in `run_on_android` before the App is
    // built; `env_logger` there would only fight it.
    #[cfg(not(target_os = "android"))]
    let _ = env_logger::try_init();
    INITIALIZED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_log_level_is_copied_to_rust_log() {
        let previous = std::env::var("RUST_LOG").ok();
        std::env::set_var(
            prism_engine::launch_config::ENV_KEY,
            r#"{"log_level":"trace"}"#,
        );
        apply_launch_log_level();
        assert_eq!(std::env::var("RUST_LOG").as_deref(), Ok("trace"));

        std::env::remove_var(prism_engine::launch_config::ENV_KEY);
        match previous {
            Some(value) => std::env::set_var("RUST_LOG", value),
            None => std::env::remove_var("RUST_LOG"),
        }
    }
}
