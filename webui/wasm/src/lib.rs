mod audio;
mod gamepad;
mod input;
mod log;
mod throttle;
mod video;

use js_sys::Uint8Array;
use shared::client_datagram::ClientDatagram;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HtmlDivElement, ReadableStream, ReadableStreamDefaultReader, WebTransport,
    WebTransportDatagramDuplexStream, WebTransportOptions, WritableStream,
    WritableStreamDefaultWriter,
};

use crate::log::init as init_log;

// ---------------------------------------------------------------------------
// Global WebTransport handle
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct GlobalWt {
    pub writer: WritableStreamDefaultWriter,
    pub reader: ReadableStreamDefaultReader,
    pub wt: WebTransport,
}

thread_local! {
    static GLOBAL_WT: RefCell<Option<GlobalWt>> = const { RefCell::new(None) };
}

/// Try to forward an error message to the server over the WebTransport
/// error channel, tagged with its severity. Returns `false` when the
/// transport is not initialised (e.g. before connection setup or after it
/// was torn down).
pub(crate) fn try_send_error(level: ::log::Level, msg: &str) -> bool {
    let buf = crate::log::encode_error(level, msg);
    GLOBAL_WT.with(|cell| match cell.borrow_mut().as_mut() {
        Some(gwt) => {
            let _ = gwt.writer.write_with_chunk(buf.as_ref());
            true
        }
        None => false,
    })
}

pub(crate) fn with_wt<F, R>(f: F) -> R
where
    F: FnOnce(&mut GlobalWt) -> R,
{
    GLOBAL_WT.with(|cell| {
        let mut opt = cell.borrow_mut();
        f(opt.as_mut().expect("WebTransport not initialised"))
    })
}

/// Send an arbitrary [`ClientDatagram`] to the server over the WebTransport
/// datagram stream.
pub(crate) fn send_datagram(d: shared::client_datagram::ClientDatagram) {
    let bytes = d.to_bytes();
    let buf = Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
}

/// Send a [`ClientDatagram`] *reliably*, over a freshly opened unidirectional
/// stream rather than a datagram. Datagrams are best-effort and can be silently
/// dropped in transit; streams are ordered and delivered. Use this for state
/// transitions whose loss would leave the server with a stale virtual device —
/// most importantly `GamepadDisconnect`, which must always tear down the host's
/// virtual controller. Returns `false` when the transport is gone or the send
/// failed (the datagram path is unaffected by a failure here).
pub(crate) async fn send_reliable(d: shared::client_datagram::ClientDatagram) -> bool {
    let bytes = d.to_bytes();
    let buf = Uint8Array::from(&bytes[..]);
    let wt = match GLOBAL_WT.with(|cell| cell.borrow().as_ref().map(|g| g.wt.clone())) {
        Some(wt) => wt,
        None => return false,
    };
    let stream_promise = wt.create_unidirectional_stream();
    let stream: WritableStream = match JsFuture::from(stream_promise).await {
        Ok(v) => match v.dyn_into() {
            Ok(s) => s,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let writer = match stream.get_writer() {
        Ok(w) => w,
        Err(_) => return false,
    };
    if JsFuture::from(writer.write_with_chunk(&JsValue::from(buf))).await.is_err() {
        return false;
    }
    JsFuture::from(writer.close()).await.is_ok()
}

// ---------------------------------------------------------------------------
// Server-initiated streams
// ---------------------------------------------------------------------------

/// Streams the server has opened, waiting for the render loop to drain them.
pub(crate) type PendingStreams = Rc<RefCell<VecDeque<ReadableStream>>>;

/// How much of a server stream to drain before yielding to the event loop.
/// Small enough that the yield's own latency is negligible next to the drain,
/// large enough that a multi-megabyte keyframe does not pay for dozens of them.
const DRAIN_YIELD_BYTES: usize = 256 * 1024;

/// Read a named field off a JS object, treating missing and null fields alike.
fn field(obj: &JsValue, name: &str) -> Option<JsValue> {
    js_sys::Reflect::get(obj, &JsValue::from_str(name))
        .ok()
        .filter(|value| !value.is_undefined() && !value.is_null())
}

/// Whether a `ReadableStreamDefaultReader` result reports end-of-stream.
fn at_end(result: &JsValue) -> bool {
    field(result, "done")
        .and_then(|done| done.as_bool())
        .unwrap_or(false)
}

/// `getReader()` is typed as a bare `Object` by `web-sys`, so the reader has to
/// be downcast before its `read()` is reachable.
fn stream_reader(stream: ReadableStream) -> Result<ReadableStreamDefaultReader, JsValue> {
    stream
        .get_reader()
        .dyn_into()
        .map_err(|_| JsValue::from_str("getReader() did not return a ReadableStreamDefaultReader"))
}

/// Hand control back to the browser's event loop, letting *tasks* run.
///
/// Every `await` in the receive path is a microtask, and a chain of awaits over
/// already-buffered reads resolves without ever yielding to the macrotask queue.
/// Draining a multi-megabyte keyframe is such a chain, and so is a render loop
/// working through a backlog of datagrams faster than the decoder consumes them
/// — which is exactly what a congested link produces.
///
/// The keepalive that tells the server we are still here is a `setInterval`,
/// i.e. a task. A client that is busy receiving therefore stops sending it, and
/// the server cannot distinguish that from a client that has gone away. Yielding
/// on a task source keeps the liveness signal independent of how much video is
/// in flight.
///
/// The `setTimeout` in the executor is what makes this a task; awaiting an
/// already-resolved promise would resume in a microtask, which is the very queue
/// the caller cannot leave. The browser clamps a zero-delay timeout to 4 ms once
/// timers start nesting, which is why the call sites yield on a byte or message
/// budget rather than every iteration.
pub(crate) async fn yield_to_event_loop() {
    let make_timer = js_sys::Function::new_no_args(
        "return new Promise((resolve) => setTimeout(resolve, 0))",
    );
    if let Ok(timer) = make_timer.call0(&JsValue::UNDEFINED) {
        let _ = JsFuture::from(timer.unchecked_into::<js_sys::Promise>()).await;
    }
}

/// Drain a stream into one buffer, concatenating its chunks. The end of the
/// stream is what delimits a message, so a server stream needs no length
/// prefix and the very same [`shared::server_datagram::ServerDatagram`] bytes
/// work on either transport.
///
/// A keyframe is megabytes, so the drain yields to the event loop every
/// [`DRAIN_YIELD_BYTES`]; see [`yield_to_event_loop`] for why that matters.
pub(crate) async fn read_to_end(stream: ReadableStream) -> Result<Vec<u8>, JsValue> {
    let reader = stream_reader(stream)?;
    let mut buf = Vec::new();
    let mut since_yield = 0usize;
    loop {
        let result = JsFuture::from(reader.read()).await?;
        if at_end(&result) {
            return Ok(buf);
        }
        let chunk = field(&result, "value")
            .ok_or_else(|| JsValue::from_str("stream chunk has no value"))?;
        let chunk = Uint8Array::new(&chunk).to_vec();
        since_yield += chunk.len();
        buf.extend_from_slice(&chunk);
        if since_yield >= DRAIN_YIELD_BYTES {
            since_yield = 0;
            yield_to_event_loop().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Deferred reporting
// ---------------------------------------------------------------------------

/// Where a message that could not be delivered over a dying transport is
/// parked for the next session.
const DEFERRED_REPORT_KEY: &str = "webshooter.deferred-report";

/// Stash a message for the *next* session's server log.
///
/// The WebTransport session is the only live channel to the server, so anything
/// worth reporting at the moment it dies has nowhere to go: the record is
/// written to a writer that is already closing. `sessionStorage` is same-origin
/// and survives the reload, so the next connect can carry it instead of the
/// detail being lost. Best-effort throughout — a browser with storage disabled
/// simply drops it, and the console mirror is all that is left.
fn defer_report(msg: &str) {
    let stored = web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .is_some_and(|storage| storage.set_item(DEFERRED_REPORT_KEY, msg).is_ok());
    if !stored {
        web_sys::console::warn_1(
            &"(previous session's close reason could not be deferred)".into(),
        );
    }
}

/// Report whatever [`defer_report`] parked, now that the transport can carry it.
fn flush_deferred_report() {
    let parked = web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .and_then(|storage| {
            let previous = storage.get_item(DEFERRED_REPORT_KEY).ok().flatten()?;
            storage.remove_item(DEFERRED_REPORT_KEY).ok()?;
            Some(previous)
        });
    if let Some(parked) = parked {
        ::log::error!("from the previous session: {parked}");
    }
}

/// Start accepting the server's unidirectional streams, returning the queue
/// they land in.
///
/// Each stream carries exactly one [`shared::server_datagram::ServerDatagram`].
/// Keyframes are far too large to fragment across datagrams — a single lost
/// fragment discards the whole frame, and the keyframe is precisely the frame a
/// client cannot decode without — so the server sends them here, where the
/// transport retransmits them instead of dropping them.
///
/// Only the *opening* of a stream is handled here; the contents are deliberately
/// left unread for the render loop, which is what keeps a keyframe ahead of the
/// delta frames the server sent after it. Draining a megabyte of keyframe on
/// this task would publish it only once it had landed, by which time those
/// deltas would already have been decoded without the reference frame they are
/// predicted from. Ordering that survives the two transports racing each other
/// is the render loop's job — see [`video::next_message`].
fn accept_server_streams(wt: &WebTransport) -> Result<PendingStreams, JsValue> {
    let streams = stream_reader(wt.incoming_unidirectional_streams())?;

    let pending: PendingStreams = Rc::new(RefCell::new(VecDeque::new()));
    let sink = pending.clone();
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            let next = JsFuture::from(streams.read()).await;
            let next = match next {
                Ok(next) => next,
                Err(err) => {
                    ::log::info!("server stream acceptor stopped: {err:?}");
                    break;
                }
            };
            // The stream list only ends when the session itself does.
            if at_end(&next) {
                ::log::info!("server stream list closed");
                break;
            }
            let Some(stream) = field(&next, "value").and_then(|value| value.dyn_into().ok()) else {
                ::log::warn!("server stream list yielded a non-stream entry");
                break;
            };
            sink.borrow_mut().push_back(stream);
        }
    });
    Ok(pending)
}

fn show_connection_lost() -> Option<()> {
    let document = web_sys::window()?.document()?;
    let body = document.body()?;

    // Remove existing canvas
    if let Some(old_canvas) = document.query_selector("canvas").ok().flatten() {
        let _ = old_canvas.remove();
    }
    // Create connection lost overlay
    let div = document
        .create_element("div")
        .ok()
        .and_then(|d| d.dyn_into::<HtmlDivElement>().ok())?;
    let _ = div.set_attribute("style", "position:fixed;inset:0;background:rgba(0,0,0,0.9);color:white;display:flex;flex-direction:column;align-items:center;justify-content:center;font-family:sans-serif;z-index:9999;");
    div.set_inner_html("<h2 style='margin:0 0 1rem;'>Connection lost</h2><p style='margin:0;color:#aaa;'>The server disconnected. Please refresh the page to reconnect.</p>");
    let _ = body.append_child(&div);

    Some(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub async fn start() -> Result<(), JsValue> {
    init_log();
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let href = location.href()?;

    // 1. Negotiate — fetch token + server certificate hash.
    let negotiate_resp: web_sys::Response = JsFuture::from(window.fetch_with_str("negotiate_wt"))
        .await?
        .dyn_into()?;
    let token = negotiate_resp
        .headers()
        .get("token")?
        .ok_or_else(|| JsValue::from("no token header"))?;
    let cert_hash_buf = JsFuture::from(negotiate_resp.array_buffer()?).await?;
    let cert_hash_arr = Uint8Array::new(&cert_hash_buf);
    let cert_hash_js: JsValue = cert_hash_arr.buffer().into();

    // 2. Build WebTransportOptions with serverCertificateHashes.
    let opts = WebTransportOptions::new();
    js_sys::Reflect::set(&opts, &"requireUnreliable".into(), &JsValue::TRUE)?;
    let hash_entry = js_sys::Object::new();
    js_sys::Reflect::set(&hash_entry, &"algorithm".into(), &"sha-256".into())?;
    js_sys::Reflect::set(&hash_entry, &"value".into(), &cert_hash_js)?;
    let hashes = js_sys::Array::new();
    hashes.push(&hash_entry);
    js_sys::Reflect::set(&opts, &"serverCertificateHashes".into(), &hashes)?;

    let url = format!("{}?token={}", href, token);
    let wt = WebTransport::new_with_options(&url, &opts)?;

    // Report *why* the transport died. The browser is the only party that knows:
    // it can tell a network change, a failed handshake and an aborted session
    // apart, and it puts the reason in the close event. Nothing else in the
    // pipeline can — the server sees an already-dead QUIC connection with no
    // close reason recorded, and reports that as its own ambiguous fallback. With
    // no listener here the teardown is silent from the client's side, and a
    // session lost on a marginal link is indistinguishable from one the user
    // simply closed.
    let closed = Closure::wrap(Box::new(move |event: web_sys::CloseEvent| {
        let reason = event.reason();
        let report = format!(
            "WebTransport closed: code={} clean={} reason={reason:?}",
            event.code(),
            event.was_clean(),
        );
        ::log::error!("{report}");
        // This runs *because* the session ended, so the record above is written
        // to a writer that is already closing and will most likely never reach
        // the server. Park it so the next connect delivers it instead.
        defer_report(&report);
    }) as Box<dyn FnMut(web_sys::CloseEvent)>);
    // Set reflectively: web-sys models `WebTransport` as extending `Object`
    // rather than `EventTarget`, so there is no typed `add_event_listener` for
    // it, but `onclose` is a real IDL attribute and assigning it is equivalent.
    js_sys::Reflect::set(&wt, &JsValue::from_str("onclose"), closed.as_ref())
        .map_err(|err| ::log::warn!("could not observe WebTransport close: {err:?}"))
        .ok();
    closed.forget();

    // 3. Wait for ready.
    JsFuture::from(wt.ready()).await?;

    // 4. Open datagram streams.
    let datagrams: WebTransportDatagramDuplexStream = wt.datagrams();
    let writer = datagrams.writable().get_writer().unwrap();
    let reader: ReadableStreamDefaultReader = datagrams
        .readable()
        .get_reader()
        .dyn_into()
        .expect("get_reader() did not return a ReadableStreamDefaultReader");

    // 5. Store in global handle.
    GLOBAL_WT.with(|cell| {
        *cell.borrow_mut() = Some(GlobalWt {
            writer,
            reader,
            wt: wt.clone(),
        });
    });

    // 6. Start accepting the server's unidirectional streams. Keyframes come
    // this way, so the acceptor has to be running before the render loop asks
    // for its first message.
    let streams = accept_server_streams(&wt)?;

    // Anything the last session could not report over its own dying transport,
    // now that this one can carry it.
    flush_deferred_report();

    // Transport ready — from here we can send errors via WT
    let result = async {
        // 7. Keepalive every 50 ms.
        let keepalive = Closure::wrap(Box::new(|| {
            send_datagram(ClientDatagram::KeepAlive);
        }) as Box<dyn FnMut()>);
        let keepalive_id = window.set_interval_with_callback_and_timeout_and_arguments_0(
            keepalive.as_ref().unchecked_ref::<js_sys::Function>(),
            50,
        )?;
        keepalive.forget();

        // 8. Advertise decoder capabilities.
        video::send_decoder_capabilities()
        .unwrap_or_else(|err| ::log::warn!("decoder capabilities not sent: {err:#?}"));

        // 9. Canvas + video
        let canvas = video::setup_canvas();
        video::send_initial_resize(&canvas)
        .unwrap_or_else(|err| ::log::warn!("initial resize not sent: {err:#?}"));
        let pending_fullscreen = video::setup_resize_prompt(&canvas);

        // 10. Render loop
        let release_flag = Rc::new(Cell::new(false));
        let render_loop = video::render_loop(
            &canvas,
            release_flag.clone(),
            pending_fullscreen.clone(),
            streams,
        );

        // 11. Input handlers
        input::setup_keyboard(&canvas);
        input::setup_touch(&canvas);
        gamepad::setup_gamepad();
        input::setup_mouse(&canvas, release_flag);

        // 12. Wait for render loop to finish (signals connection closed).
        if let Err(e) = render_loop.await {
            ::log::error!("render_loop error: {e:?}");
            show_connection_lost();
        }

        window.clear_interval_with_handle(keepalive_id);
        Ok::<(), JsValue>(())
    }
    .await;

    if let Err(e) = result {
        ::log::error!("start error after transport ready: {e:?}");
        show_connection_lost();
    }
    Ok(())
}
