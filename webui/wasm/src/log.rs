//! Logging built on the standard [`log`] facade.
//!
//! Records are queued here and posted to the server's `client_logs` route in
//! batches, and mirrored into the browser console at matching severity.
//!
//! # Why HTTP rather than the session
//!
//! A client's log records have to survive the moments that matter most: before
//! its WebTransport session exists, and after it has gone. Both are exactly the
//! windows over which the session's own channel can carry nothing — a client
//! whose session is being established has no channel to report *why* the
//! handshake is slow, and a client whose session has just dropped has no channel
//! to report *why*. Posting over HTTP covers both, needs no session to be alive,
//! and does not stop working because the thing being reported is a transport
//! failure.
//!
//! The route is authenticated by the same cookie as everything else, so records
//! arrive on the same `webshooter::client` target, attributed the same way, as
//! records that came over the transport.
//!
//! Until the server announces its configured maximum level ([`apply_server_level`],
//! sent on every connection) records are capped at `Info`, so a chatty client
//! cannot spam the console with diagnostics nobody asked for.

use std::cell::RefCell;
use std::collections::VecDeque;

use js_sys::Error;
use log::{Level, LevelFilter, Log, Metadata, Record};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Where records go on the server. Relative, like every other call this client
/// makes, so a page served from a mount point keeps its prefix.
const ENDPOINT: &str = "client_logs";

/// How long the flusher waits after an empty queue before sleeping again.
const FLUSH_INTERVAL_MS: i32 = 250;

/// Records held before the oldest are dropped, and the most this client can have
/// in flight at once.
///
/// A bound rather than an unbounded queue because the queue's whole purpose is to
/// be there when the client is failing, and a client failing *hard* can produce
/// records faster than HTTP can drain them. Losing the oldest keeps the newest —
/// which is the end of the story — and losing them is reported rather than silent.
const MAX_QUEUED: usize = 256;

/// The most records one request carries, matching the server's own limit.
const MAX_PER_BATCH: usize = 64;

/// Logger forwarding records to the server, in batches, over HTTP.
struct ServerLogger;

impl Log for ServerLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        queue(record.level(), record.args().to_string());
        mirror(record.level(), &record.args().to_string());
    }

    fn flush(&self) {}
}

/// Mirror a record into the browser console at matching severity.
///
/// Unconditional, because the server's log is not somewhere the user can see: the
/// console is the copy a person debugging a session actually reads.
fn mirror(level: Level, msg: &str) {
    let formatted = format!("[{level}] {msg}").into();
    match level {
        Level::Error => web_sys::console::error_1(&formatted),
        Level::Warn => web_sys::console::warn_1(&formatted),
        _ => web_sys::console::log_1(&formatted),
    }
}

thread_local! {
    static QUEUE: RefCell<VecDeque<(Level, String)>> = const { RefCell::new(VecDeque::new()) };
    /// Set once a flusher is running, so a second record does not start a second
    /// loop. Reset by the loop itself when it ends, so a session that comes back
    /// is flushed too.
    static FLUSHER_RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Hand a record to the queue and make sure something is draining it.
fn queue(level: Level, message: String) {
    let start_flusher = QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        if queue.len() >= MAX_QUEUED {
            queue.pop_front();
            // One line, to the console: the queue is bounded on purpose, and the
            // console is where a person can see that it is happening.
            web_sys::console::warn_1(
                &JsValue::from_str(&format!(
                    "client log queue full ({MAX_QUEUED}); dropping the oldest records"
                )),
            );
        }
        queue.push_back((level, message));
        !FLUSHER_RUNNING.replace(true)
    });
    if start_flusher {
        wasm_bindgen_futures::spawn_local(flush_loop());
    }
}

/// Drain the queue into `client_logs` requests, forever.
///
/// One loop, started on the first record: a queue with no drainer is a queue that
/// eventually loses everything, and the whole point is that these records arrive
/// at their worst moment.
async fn flush_loop() {
    loop {
        let batch = QUEUE.with(|queue| {
            let mut queue = queue.borrow_mut();
            let take = queue.len().min(MAX_PER_BATCH);
            queue.drain(..take).collect::<Vec<_>>()
        });
        if batch.is_empty() {
            FLUSHER_RUNNING.with(|running| running.set(false));
            return;
        }
        if !post(&batch).await {
            // Put them back at the *front*: newer records must not overtake older
            // ones, because a log that reorders itself is a log that lies about
            // what happened when.
            QUEUE.with(|queue| {
                let mut queue = queue.borrow_mut();
                let room = MAX_QUEUED.saturating_sub(queue.len());
                for (level, message) in batch.into_iter().rev().take(room) {
                    queue.push_front((level, message));
                }
            });
            // Nothing to wait for but the server coming back; without this the loop
            // would spin at the flush interval forever on a server that is down.
            sleep(FLUSH_INTERVAL_MS * 4).await;
            continue;
        }
        sleep(FLUSH_INTERVAL_MS).await;
    }
}

/// POST one batch. `false` means it did not land.
async fn post(batch: &[(Level, String)]) -> bool {
    let records: Vec<serde_json::Value> = batch
        .iter()
        .map(|(level, message)| serde_json::json!({ "level": level, "message": message }))
        .collect();
    let body = match serde_json::to_string(&serde_json::json!({ "records": records })) {
        Ok(body) => body,
        Err(err) => {
            report_once(format!("client log not serialised: {err}"));
            // The records were never built, so there is nothing to retry.
            return true;
        }
    };

    let Some(window) = web_sys::window() else {
        return false;
    };
    let headers = web_sys::Headers::new().ok();
    if let Some(headers) = &headers
        && let Err(err) = headers.set("Content-Type", "application/json")
    {
        report_once(format!("client log headers not set: {err:?}"));
        return true;
    }
    let mut init = web_sys::RequestInit::new();
    init.method("POST");
    init.body(Some(&JsValue::from_str(&body)));
    if let Some(headers) = &headers {
        init.headers(headers.as_ref());
    }

    match JsFuture::from(window.fetch_with_str_and_init(ENDPOINT, &init)).await {
        Ok(response) => match response.dyn_into::<web_sys::Response>() {
            Ok(response) => response.ok(),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

thread_local! {
    /// Messages already reported, so a persistent failure reports once instead of
    /// once per batch.
    static REPORTED: RefCell<VecDeque<String>> = const { RefCell::new(VecDeque::new()) };
}

/// Say something about the log pipeline itself, once per distinct message.
fn report_once(message: String) {
    let first_time = REPORTED.with(|reported| {
        let mut reported = reported.borrow_mut();
        if reported.contains(&message) {
            return false;
        }
        if reported.len() >= 16 {
            reported.pop_front();
        }
        reported.push_back(message.clone());
        true
    });
    if first_time {
        web_sys::console::warn_1(&JsValue::from_str(&message));
    }
}

/// Wait `ms` milliseconds.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            let _ = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
                .map(|_| ());
        }
    });
    let _ = JsFuture::from(promise).await;
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