//! Buffer log records emitted before the process logger is installed.
//!
//! `App::build` mounts the resource packages and loads the startup scene, and
//! every warning from that phase used to be dropped: `env_logger::try_init`
//! only ran later, inside `App::run`. Install this logger at the start of
//! `build`, then replay the buffer once the event loop starts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use log::{Level, Log, Metadata, Record};

/// Startup produces a handful of lines; the cap only guards against runaway
/// logging in a path that loops before the event loop exists.
const CAPACITY: usize = 1024;

struct BufferedRecord {
    level: Level,
    target: String,
    message: String,
    module_path: Option<String>,
    file: Option<String>,
    line: Option<u32>,
}

impl BufferedRecord {
    fn new(record: &Record<'_>) -> Self {
        Self {
            level: record.level(),
            target: record.target().to_string(),
            message: record.args().to_string(),
            module_path: record.module_path().map(str::to_string),
            file: record.file().map(str::to_string),
            line: record.line(),
        }
    }

    fn log_to(&self, logger: &dyn Log) {
        logger.log(
            &Record::builder()
                .level(self.level)
                .target(&self.target)
                .args(format_args!("{}", self.message))
                .module_path(self.module_path.as_deref())
                .file(self.file.as_deref())
                .line(self.line)
                .build(),
        );
    }
}

struct StartupLogger {
    inner: env_logger::Logger,
    ready: AtomicBool,
    pending: Mutex<Vec<BufferedRecord>>,
}

impl StartupLogger {
    fn new() -> Self {
        Self {
            inner: env_logger::Builder::from_env(env_logger::Env::default()).build(),
            ready: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Replay what was buffered and let later records go straight through.
    fn replay(&self) {
        self.ready.store(true, Ordering::SeqCst);
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *pending)
        };
        for record in pending {
            record.log_to(&self.inner);
        }
    }
}

impl Log for StartupLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        if self.ready.load(Ordering::Relaxed) {
            self.inner.log(record);
            return;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.len() < CAPACITY {
            pending.push(BufferedRecord::new(record));
        }
    }

    fn flush(&self) {}
}

static LOGGER: OnceLock<&'static StartupLogger> = OnceLock::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install the buffering logger if the process does not have one yet.
///
/// Android and hosted hosts initialize their logger before constructing the
/// app; `set_logger` then fails and we leave their setup untouched.
pub fn install() {
    if INSTALLED.load(Ordering::SeqCst) {
        return;
    }
    let logger = *LOGGER.get_or_init(|| Box::leak(Box::new(StartupLogger::new())));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(logger.inner.filter());
        INSTALLED.store(true, Ordering::SeqCst);
    }
}

/// Replay buffered startup records; no-op when another logger owns the process.
pub fn replay() {
    if !INSTALLED.load(Ordering::SeqCst) {
        return;
    }
    if let Some(logger) = LOGGER.get() {
        logger.replay();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_record_round_trips_level_target_and_message() {
        let metadata = Metadata::builder()
            .level(Level::Warn)
            .target("prism_app::startup_log")
            .build();
        let message = "resource package scenes.pak is stale";
        let buffered = BufferedRecord::new(
            &Record::builder()
                .metadata(metadata)
                .args(format_args!("{message}"))
                .file(Some("app.rs"))
                .line(Some(42))
                .build(),
        );

        assert_eq!(buffered.level, Level::Warn);
        assert_eq!(buffered.target, "prism_app::startup_log");
        assert_eq!(buffered.message, "resource package scenes.pak is stale");
        assert_eq!(buffered.file.as_deref(), Some("app.rs"));
        assert_eq!(buffered.line, Some(42));
    }
}
