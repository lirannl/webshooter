//! Logging setup built on the standard [`log`] facade.
//!
//! The server installs a simple stderr logger; all modules emit through the
//! `log` macros (`log::info!`, `log::error!`, ...).

use std::io::Write;

use log::{LevelFilter, Log, Metadata, Record};

/// Stderr logger writing one leveled line per record.
pub struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = writeln!(
            lock,
            "[{:>5} {}] {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Install the global logger.
///
/// Starts at a conservative default (`Debug` under the `debug` feature,
/// otherwise `Info`); [`set_level`] applies the configured value once the
/// global configuration is loaded.
pub fn init() {
    let _ = log::set_boxed_logger(Box::new(Logger));
    if cfg!(feature = "debug") {
        log::set_max_level(LevelFilter::Debug);
    } else {
        log::set_max_level(LevelFilter::Info);
    }
}

/// Apply the configured global maximum level.
pub fn set_level(level: LevelFilter) {
    log::set_max_level(level);
}
