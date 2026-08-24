//! Single-byte wire encoding for log levels.
//!
//! Used by the client `Error` datagram (record severities) and the server
//! `LogLevel` datagram (maximum-verbosity announcements).

use anyhow::{Result, bail};
use log::{Level, LevelFilter};

/// Encode a maximum-level filter: 0 = Off, 1 = Error … 5 = Trace.
pub fn filter_to_byte(filter: LevelFilter) -> u8 {
    match filter {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

/// Decode a filter byte written by [`filter_to_byte`].
pub fn filter_from_byte(byte: u8) -> Result<LevelFilter> {
    Ok(match byte {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        5 => LevelFilter::Trace,
        b => bail!("invalid log level byte: {b}"),
    })
}

/// Encode a record severity. Never produces 0, since `Off` is not a severity.
pub fn level_to_byte(level: Level) -> u8 {
    match level {
        Level::Error => 1,
        Level::Warn => 2,
        Level::Info => 3,
        Level::Debug => 4,
        Level::Trace => 5,
    }
}

/// Decode a severity byte written by [`level_to_byte`].
pub fn level_from_byte(byte: u8) -> Result<Level> {
    Ok(match byte {
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        4 => Level::Debug,
        5 => Level::Trace,
        b => bail!("invalid log level byte: {b}"),
    })
}
