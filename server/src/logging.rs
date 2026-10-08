//! Logging setup built on the standard [`log`] facade.
//!
//! The server installs a simple stderr logger; all modules emit through the
//! `log` macros (`log::info!`, `log::error!`, ...).

use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

use log::{LevelFilter, Log, Metadata, Record};

/// Process start, so each line can carry a relative timestamp.
///
/// Relative rather than wall-clock because `std` has no calendar formatter and
/// the wall clock arrives anyway — from the journal, from the file's mtime —
/// while what a redirect-to-file log never has is *spacing*: without it, any
/// question of the form "did X happen before Y, and how far apart" can only be
/// answered by line-number heuristics.
static START: OnceLock<Instant> = OnceLock::new();

/// Stderr logger writing one leveled line per record.
pub struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f64();
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = writeln!(
            lock,
            "[{:>5} +{:>9.3} {}] {}",
            record.level(),
            elapsed,
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
