//! Logging built on the standard [`log`] facade.
//!
//! Records are forwarded to the server over the WebTransport error channel,
//! tagged with their severity; before the transport is up (or after it goes
//! away) they fall back to the browser console so nothing is silently lost.
//! The browser console always receives a local mirror as well, since
//! server-side logs are not visible in the client.
//!
//! Until the server announces its configured maximum level ([`apply_server_level`],
//! sent on every connection) records are capped at `Info`, so a chatty client
//! cannot spam either console with diagnostics nobody asked for.
//!
//! The JS-exported [`log`](js_log) accepts any structured value, not just
//! strings: non-string values are serialized faithfully via serde. Callers
//! choose the level.

use js_sys::{Error, Uint8Array};
use log::{Level, LevelFilter, Log, Metadata, Record};
use shared::client_datagram::ClientDatagram;
use wasm_bindgen::prelude::*;

/// Logger forwarding records to the server over the WebTransport error
/// channel.
struct ServerLogger;

impl Log for ServerLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let msg = record.args().to_string();
        if crate::try_send_error(record.level(), &msg) {
            mirror(record.level(), &msg);
        } else {
            web_sys::console::warn_1(
                &format!(
                    "[{} {}] {msg} (transport unavailable, not sent)",
                    record.level(),
                    record.target()
                )
                .into(),
            );
        }
    }

    fn flush(&self) {}
}

/// Mirror a delivered record into the browser console at matching severity.
fn mirror(level: Level, msg: &str) {
    let formatted = format!("[{level}] {msg}").into();
    match level {
        Level::Error => web_sys::console::error_1(&formatted),
        Level::Warn => web_sys::console::warn_1(&formatted),
        _ => web_sys::console::log_1(&formatted),
    }
}

/// Install the global logger. Later calls are no-ops.
///
/// The level starts at `Info`; the server raises or lowers it once the
/// connection is up.
pub fn init() {
    let _ = log::set_boxed_logger(Box::new(ServerLogger));
    log::set_max_level(LevelFilter::Info);
}

/// Apply the maximum level announced by the server.
pub(crate) fn apply_server_level(filter: LevelFilter) {
    if filter == log::max_level() {
        return;
    }
    log::set_max_level(filter);
    log::debug!("server set maximum log level: {filter}");
}

/// Log any `Serialize` value as JSON at `level`.
///
/// The server transport carries text datagrams, so structure is preserved
/// by embedding a JSON document in the message rather than flattening the
/// value into a `Debug` string.
pub fn log_serialized<T: serde::Serialize + ?Sized>(level: Level, value: &T) {
    match serde_json::to_string(value) {
        Ok(json) => log::log!(level, "{json}"),
        Err(err) => log::error!("failed to serialize log value: {err}"),
    }
}

/// Parse a JS-supplied level name; unknown or missing means `Info`.
fn parse_level(level: Option<&str>) -> Level {
    match level.map(str::to_ascii_lowercase).as_deref() {
        Some("error") => Level::Error,
        Some("warn" | "warning") => Level::Warn,
        Some("debug") => Level::Debug,
        Some("trace") => Level::Trace,
        _ => Level::Info,
    }
}

#[wasm_bindgen(js_name = "log", skip_typescript)]
pub fn js_log(val: &JsValue, level: Option<String>) {
    let level = parse_level(level.as_deref());
    if let Some(msg) = val.as_string() {
        log::log!(level, "{msg}");
    } else if val.is_instance_of::<Error>() {
        let err: Error = Error::unchecked_from_js(val.clone());
        log::log!(level, "{}", err.to_string());
    } else if let Ok(value) = serde_wasm_bindgen::from_value::<serde_json::Value>(val.clone()) {
        // Any other structured value — serialize it faithfully via serde.
        log::log!(level, "{value}");
    } else {
        log::warn!("unloggable value: {val:?}");
    }
}

#[wasm_bindgen(typescript_custom_section)]
const TS_LOG: &'static str = r#"
export function log(
    val: string | Error | object | null,
    level?: "trace" | "debug" | "info" | "warn" | "error",
): void;
"#;

/// Encode an error datagram carrying `msg` at severity `level`.
pub(crate) fn encode_error(level: Level, msg: &str) -> Uint8Array {
    let bytes = ClientDatagram::Error {
        level,
        message: msg.to_string(),
    }
    .to_bytes();
    Uint8Array::from(&bytes[..])
}
