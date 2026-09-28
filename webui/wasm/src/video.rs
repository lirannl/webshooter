use crate::audio::AudioPlayer;
use crate::{PendingStreams, with_wt};
use shared::client_datagram::ClientDatagram;
use shared::codec::Codec;
use shared::fragment::{FragmentFrame, PushOutcome};
use shared::frame_gate::{FrameAction, FrameGate, HeldFrame};
use shared::server_datagram::ServerDatagram;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, EncodedVideoChunk, HtmlCanvasElement, KeyboardEvent, VideoFrame,
};

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

/// The page's display: the canvas encoded frames are drawn on, and the
/// fullscreen intent waiting for the next user gesture.
///
/// A type rather than two loose arguments because it exists only when the
/// session has a display at all. An audio-only page builds none of it, and
/// every step that needs a display takes `Option<&Display>` — so "no display"
/// is a value the type system carries rather than a `None` each call site has
/// to remember to check.
pub struct Display {
    pub canvas: HtmlCanvasElement,
    pub pending_fullscreen: Rc<Cell<bool>>,
}

pub fn setup_canvas() -> HtmlCanvasElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas = document
        .create_element("canvas")
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();
    canvas.style().set_css_text(
        "position:fixed;inset:0;width:100%;height:100%;background:#000;cursor:pointer;outline:none;",
    );
    document.body().unwrap().append_child(&canvas).unwrap();
    canvas
}

pub fn send_initial_resize(canvas: &HtmlCanvasElement) -> Result<(), JsError> {
    let window = web_sys::window().ok_or(JsError::new("Window not found"))?;
    let w = canvas.offset_width() as f64;
    let h = canvas.offset_height() as f64;
    crate::send_datagram(ClientDatagram::ResizeDisplay {
        index: 0,
        width: (w * window.device_pixel_ratio()) as u16,
        height: (h * window.device_pixel_ratio()) as u16,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Resize prompt
// ---------------------------------------------------------------------------

pub fn setup_resize_prompt(canvas: &HtmlCanvasElement) -> Rc<Cell<bool>> {
    let window = web_sys::window().unwrap();
    let performance = window.performance().unwrap();

    // Debounced resize sender: 2s cooldown after initial send.
    // Not started as None because send_initial_resize already fired before
    // the ResizeObserver was attached — its first callback would duplicate.
    let last_sent = RefCell::new(Some(performance.now()));

    let send_resize = move || -> Result<(), JsError> {
        let now = performance.now();
        let should_send = {
            let mut last = last_sent.borrow_mut();
            if let Some(last_time) = *last {
                if now - last_time >= 2000.0 {
                    *last = Some(now);
                    true
                } else {
                    false
                }
            } else {
                *last = Some(now);
                true
            }
        };
        if should_send {
            send_initial_resize(canvas)?;
        }
        Ok(())
    };

    let resize_cb = Closure::wrap(Box::new(move || {
        send_resize().unwrap_or_else(|err| log::error!("{err:#?}"));
    }) as Box<dyn FnMut()>);

    let ro = web_sys::ResizeObserver::new(resize_cb.as_ref().unchecked_ref::<js_sys::Function>())
        .unwrap();
    ro.observe(canvas);
    resize_cb.forget();

    let fullscreen_cb = Closure::wrap(Box::new(move || {
        let window = web_sys::window().unwrap();
        let document = window.document().unwrap();
        let w = std::cmp::max(
            document
                .document_element()
                .map(|e| e.client_width())
                .unwrap_or(0) as u16,
            window
                .inner_width()
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u16,
        );
        let h = std::cmp::max(
            document
                .document_element()
                .map(|e| e.client_height())
                .unwrap_or(0) as u16,
            window
                .inner_height()
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u16,
        );
        let msg = ClientDatagram::ResizeDisplay {
            index: 0,
            width: w * window.device_pixel_ratio() as u16,
            height: h * window.device_pixel_ratio() as u16,
        };
        crate::send_datagram(msg);
    }) as Box<dyn FnMut()>);
    let _ = canvas.add_event_listener_with_callback(
        "fullscreenchange",
        fullscreen_cb.as_ref().unchecked_ref::<js_sys::Function>(),
    );
    fullscreen_cb.forget();

    // requestFullscreen() can only be called from within a user gesture, but
    // the ToggleFullscreen datagram arrives asynchronously over the network.
    // Stash the intent here and apply it on the next pointerdown (a real
    // gesture). Exiting fullscreen is not gesture-restricted and is handled
    // directly in the render loop.
    let pending_fullscreen = Rc::new(Cell::new(false));
    {
        let pending_fullscreen = pending_fullscreen.clone();
        let cb = Closure::wrap(Box::new(move || {
            if pending_fullscreen.get() {
                pending_fullscreen.set(false);
                let _ = canvas.request_fullscreen();
            }
        }) as Box<dyn FnMut()>);
        let _ = canvas.add_event_listener_with_callback(
            "pointerdown",
            cb.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        cb.forget();
    }
    // keydown is also a user gesture, so keyboard-only input flushes the queue.
    {
        let pending_fullscreen = pending_fullscreen.clone();
        let window = web_sys::window().unwrap();
        let canvas = canvas.clone();
        let cb = Closure::wrap(Box::new(move |_: KeyboardEvent| {
            if pending_fullscreen.get() {
                pending_fullscreen.set(false);
                let _ = canvas.request_fullscreen();
            }
        }) as Box<dyn FnMut(KeyboardEvent)>);
        let _ = window.add_event_listener_with_callback(
            "keydown",
            cb.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        cb.forget();
    }
    pending_fullscreen
}

/// True when the page is running as an installed PWA in standalone display
/// mode. In that case browsers permit `requestFullscreen()` without a fresh
/// user gesture, so we can honour a fullscreen toggle immediately.
fn is_installed_pwa(window: &web_sys::Window) -> bool {
    window
        .match_media("(display-mode: standalone)")
        .ok()
        .flatten()
        .map(|m| m.matches())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Codec capability probing
// ---------------------------------------------------------------------------

pub fn send_decoder_capabilities() -> Result<(), JsError> {
    let decoders = probe_codecs();
    crate::send_datagram(ClientDatagram::DecoderCapabilities { decoders });
    log::info!("sent decoder capabilities");
    Ok(())
}

/// Picture Loss Indication: tell the server its prediction chain is broken.
///
/// Over a stream, and that is load-bearing rather than a nicety. The server
/// never sends a keyframe on its own initiative, so this is the *only* thing
/// that will produce one: a request lost in transit leaves the client holding
/// frames until the session restarts, with nothing on screen to show for it.
/// Datagrams can be dropped without either end noticing, which is exactly the
/// failure this cannot tolerate.
///
/// The client decides how many keyframes it needs, by only asking when the gate
/// has actually found a gap, and never asking again while a request is
/// outstanding. A lossy link therefore asks again as soon as the previous
/// keyframe lands — which is the useful behaviour, since the new keyframe is
/// what the next loss will be measured against — while a clean link stops
/// asking entirely.
async fn send_request_keyframe() {
    if !crate::send_reliable(ClientDatagram::RequestKeyframe).await {
        // Nothing can be done about it from here: the transport is gone, and the
        // session is about to end anyway. Say so, because the freeze it causes
        // would otherwise be silent.
        log::error!("keyframe request could not be sent; the display is stuck");
    }
}

/// Ask the server to resend the fragments the gate says are missing.
///
/// The gate reports the *frame ids* that never assembled. For each of those the
/// client's own pending map holds the frame with its empty slots, so the request
/// can name the exact fragments — which is the difference between resending a
/// few datagrams and asking for a keyframe.
///
/// Returns whether anything was actually requested, so the caller can fall back
/// to a keyframe when the answer is "nothing": a gap the client cannot name a
/// fragment for is a gap only a keyframe can close.
async fn send_request_repair(
    pending: &Rc<RefCell<HashMap<u16, PendingFrame>>>,
    missing: &[u16],
) -> bool {
    let frames: Vec<(u16, Vec<u16>)> = {
        let map = pending.borrow();
        missing
            .iter()
            .filter_map(|id| {
                map.get(id).map(|entry| (*id, entry.fragments.missing()))
            })
            // A frame the client cannot name a fragment for contributes nothing,
            // and a request naming it would be a request the server cannot answer.
            .filter(|(_, indices)| !indices.is_empty())
            .collect()
    };
    if frames.is_empty() {
        return false;
    }
    log::debug!("repair: asking for {} fragment(s) of {} frame(s)",
        frames.iter().map(|(_, i)| i.len()).sum::<usize>(),
        frames.len());
    if !crate::send_reliable(ClientDatagram::ResendDeltas { frames }).await {
        // The request is lost, which is the case the escalation deadline exists
        // for: the gap is still open and the client will ask for a keyframe when
        // it passes. Say so, because a silent failure here looks identical to a
        // server that ignored the request.
        log::error!("repair request could not be sent; the gap will escalate");
    }
    true
}

fn probe_codecs() -> Vec<Codec> {
    // VideoDecoder.isConfigSupported is async and may not be available in
    // all contexts.  Use MediaSource.isTypeSupported as a sync fallback.
    let mut supported = Vec::new();
    for codec in &Codec::ALL {
        let codec_str = codec.web_codec_string();
        let mime = format!("video/mp4; codecs=\"{codec_str}\"");
        if web_sys::MediaSource::is_type_supported(&mime) {
            supported.push(*codec);
        }
    }
    // Fallback: if MediaSource is not available or returned nothing, try
    // VideoDecoder.isConfigSupported (async) — but for simplicity, just
    // always include AV1 and H.264 as a safe fallback.
    if supported.is_empty() {
        supported.push(Codec::Av1);
        supported.push(Codec::H264);
    }
    log::info!("probed codecs: {supported:?}");
    supported
}

// ---------------------------------------------------------------------------
// Frame reassembly
// ---------------------------------------------------------------------------

struct PendingFrame {
    fragments: FragmentFrame,
}

impl PendingFrame {
    fn new(num_frags: usize) -> Self {
        Self {
            fragments: FragmentFrame::new(num_frags),
        }
    }
}

/// Accumulate one fragment of a delta, yielding the frame once it is whole.
///
/// `None` means this fragment did not complete a frame, and in all three cases
/// the right answer is the same: wait for the rest. The rest are still in
/// flight, this one repeats something already held, or its index is impossible.
fn assemble_delta(
    pending: &Rc<RefCell<HashMap<u16, PendingFrame>>>,
    frame_id: u16,
    frag_idx: u16,
    num_frags: u16,
    payload: Vec<u8>,
) -> Option<Vec<u8>> {
    let mut map = pending.borrow_mut();

    // Drop fragments for frames we have already fully moved past. On a lossy
    // link a fragment can arrive late (after its frame was already displayed);
    // re-decoding a stale frame would feed the decoder garbage. Whether a frame
    // is stale is decided later, by the gate, because it depends on whether the
    // chain is waiting to be rebuilt — a keyframe that overtook its own deltas
    // is behind, not stale.
    let entry = map
        .entry(frame_id)
        .or_insert_with(|| PendingFrame::new(num_frags as usize));

    // A fragment's declared fragment count must match the entry we are
    // accumulating. A mismatch means a stale entry from a previous use of this
    // frame_id (the u16 counter wrapped) collided with a new frame. Restart it
    // cleanly instead of indexing out of bounds and aborting the entire stream.
    if num_frags as usize != entry.fragments.num_frags() {
        *entry = PendingFrame::new(num_frags as usize);
    }

    match entry.fragments.push(frag_idx as usize, payload) {
        PushOutcome::OutOfRange => {
            // Impossible fragment index (corrupt/truncated datagram). Drop the
            // whole frame rather than panic on out-of-bounds access.
            map.remove(&frame_id);
            None
        }
        PushOutcome::Duplicate => None,
        // Not all fragments arrived yet. A lost fragment here simply means this
        // frame is skipped — frozen until the next keyframe — rather than
        // crashing the stream.
        PushOutcome::Incomplete => None,
        PushOutcome::Complete(assembled) => Some(assembled),
    }
}

/// The state a frame passes through on its way to the screen.
///
/// Bundled because the same four things are needed at every step, and because
/// `gate` is the only one of them that is mutated — passing them together
/// makes it obvious at each call site which of them this step may change.
struct FramePath {
    decoder: web_sys::VideoDecoder,
    /// The output/error callbacks the decoder was configured with. The decoder
    /// holds JS references to them and can call them on every decoded frame,
    /// so they have to stay alive for exactly as long as the decoder does — a
    /// `Closure` frees its JS function when dropped, and dropping these with
    /// the synchronous builder that created them would silently turn every
    /// decoded frame into a no-op (the canvas stays black while the session
    /// otherwise runs perfectly). Owning them here ties their lifetime to the
    /// path's, which outlives every frame.
    _output: Closure<dyn FnMut(VideoFrame)>,
    _error: Closure<dyn FnMut(JsValue)>,
    /// The codec the decoder is configured for, which is not the codec of the
    /// frame being decoded: the decoder has to be reconfigured first.
    current_codec: Rc<RefCell<Option<Codec>>>,
    /// Ordering and loss policy; owns the prediction chain. See
    /// `shared::frame_gate`.
    gate: FrameGate,
    /// Deltas still arriving a fragment at a time.
    pending: Rc<RefCell<HashMap<u16, PendingFrame>>>,
}

/// Hand a whole frame to the ordering policy, and decode whatever it releases.
///
/// Returns whether anything was decoded. It is not when the gate drops the frame
/// as stale, or holds it pending a gap that has not closed yet.
async fn present(
    path: &mut FramePath,
    frame_id: u16,
    is_keyframe: bool,
    codec: Codec,
    assembled: Vec<u8>,
    repair_deadline: &mut Option<f64>,
) -> bool {
    // The frames the gate releases once the sequence is whole again; empty for
    // every outcome except a keyframe landing or a repair closing a gap.
    let mut replay: Vec<HeldFrame> = Vec::new();
    // Whether this frame itself is decodable. It is not when the gate holds it.
    let mut decode = false;
    // The ids a gap named, when this frame is the first one behind it. Kept
    // rather than acted on inside the block below, because answering a gap means
    // reading the pending map — which the cleanup there is still borrowing.
    let mut gap: Option<Vec<u16>> = None;
    {
        let mut map = path.pending.borrow_mut();

        match path.gate.admit(frame_id, is_keyframe) {
            FrameAction::Drop => return false,
            FrameAction::Hold { request } => {
                if request {
                    // The missing ids are the whole diagnosis. A gap costs a
                    // resend or a keyframe, and a keyframe is the most expensive
                    // thing on the wire, so it is worth being able to say whether
                    // these were never sent, lost, or dropped after arriving.
                    log::debug!("gap after {frame_id}: missing {:?}", path.gate.missing());
                    gap = Some(path.gate.missing().to_vec());
                }
            }
            FrameAction::Resync { held } => {
                replay = held;
                decode = true;
            }
            FrameAction::Decode => decode = true,
        }

        // Every frame we have moved past is finished with; its leftovers
        // (fragments of a delta that will never complete) would otherwise sit
        // in the map forever. Compared in circular order so the u16 wraparound
        // at frame_id 65535 is handled correctly.
        //
        // Two kinds of frame behind us are the exception, and both are here for
        // the same reason: their partial fragments are the only thing that can
        // complete them. A frame the gap named is waiting to have its missing
        // fragments requested; a frame already named in a request is waiting for
        // them to come back. Dropping either turns a repairable gap into an
        // unrepairable one — and the first of them is destroyed by this cleanup
        // before the request that would name it has even been built.
        let gap_ids: &[u16] = gap.as_deref().unwrap_or(&[]);
        let keys: Vec<u16> = map.keys().copied().collect();
        for id in keys {
            if frame_id.wrapping_sub(id) < 0x8000
                && !path.gate.is_awaiting(id)
                && !gap_ids.contains(&id)
            {
                map.remove(&id);
            }
        }
    }

    // Answered outside the block above, because it reads the pending map and the
    // cleanup that just ran is what makes that read meaningful.
    if let Some(missing) = gap {
        // Prefer a resend. It names the exact fragments, so it costs a few
        // datagrams where a keyframe costs tens of packets, and it puts the lost
        // frame back where it belongs — so the frames between the loss and a
        // keyframe stay decodable instead of being discarded.
        if send_request_repair(&path.pending, &missing).await {
            path.gate.expect_repair(&missing);
            *repair_deadline = Some(now_ms() + REPAIR_TIMEOUT_MS);
        } else {
            // Nothing to name a fragment for: the frames were never sent, or
            // their fragments are already gone. Only a keyframe closes this.
            send_request_keyframe().await;
        }
    }

    // The two are mutually exclusive by construction — a frame the gate decodes
    // is not one it holds — and written as branches so each moves `assembled` on
    // exactly one path. Holding a frame can be what closes the gap, so the run it
    // releases comes back here rather than from `admit`.
    if decode {
        configure_decoder(&path.decoder, &path.current_codec, codec);
        decode_frame(&path.decoder, frame_id, &assembled, is_keyframe);
    } else {
        replay = path.gate.hold(frame_id, codec, assembled);
    }
    // The frames that were waiting for exactly this point in the sequence, in
    // frame order. They are decoded only after the frame they are predicted
    // from, which is why a keyframe's own decode comes first.
    let released = replay.len();
    for frame in replay {
        configure_decoder(&path.decoder, &path.current_codec, frame.codec);
        decode_frame(&path.decoder, frame.frame_id, &frame.payload, false);
    }
    decode || released > 0
}

/// The monotonic clock in milliseconds, for the repair deadline.
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

/// How long a client waits for a resend before asking for a keyframe instead.
///
/// The resend needs two round trips — one for the request, one for the fragments —
/// so this has to be at least that, and at the 28 ms round trip this project was
/// measured on, 100 ms is about three and a half. It is still well short of the
/// keyframe path it falls back to, which is a request round trip plus an encode
/// plus a transfer of tens of packets.
///
/// The trade is deliberate and worth stating: a repair that is itself lost costs
/// this much latency before the fallback begins, where asking for a keyframe
/// immediately would not. It is accepted because the common case — a repair that
/// arrives — is both faster and far cheaper, and because the alternative pays the
/// keyframe cost on every single gap rather than on the rare lost repair.
const REPAIR_TIMEOUT_MS: f64 = 100.0;

/// The next message from the server, in the order the server sent it.
///
/// A keyframe and the delta frames the server sent *after* it are not ordered
/// against each other — they travel on different transports, read concurrently —
/// yet those deltas are inter-predicted *from* the keyframe, so decoding one
/// first would leave every frame after it applying to the wrong reference. A
/// stream therefore always wins, including over a datagram that landed while its
/// read was outstanding: such a datagram belongs behind the keyframe and is
/// parked in `deferred` until the stream has been drained.
///
/// That covers every datagram that arrives once the keyframe's stream is
/// visible. One can still overtake it — a delta datagram leaves the server
/// while a multi-megabyte keyframe is still being written — and the frame ids
/// are what catch it: [`shared::frame_gate::FrameGate`] spots the gap, holds the
/// deltas, and releases them once the keyframe that overtook them is decoded.
///
/// Returns `Ok(None)` for a message that could not be parsed, which the caller
/// skips, and `Err` once the transport is gone and the render loop must stop.
///
/// `drained` reports whether any server stream has been drained yet, so the
/// first one crossing can be reported: the split transport is only working if a
/// keyframe arrives here, and nothing else in the pipeline distinguishes "the
/// server never sent one" from "it was sent and never read".
async fn next_message(
    streams: &PendingStreams,
    deferred: &mut Option<ServerDatagram>,
    drained: &Cell<bool>,
) -> Result<Option<ServerDatagram>, JsValue> {
    loop {
        // Taken in its own statement: the borrow must be released before the
        // drain below awaits, or the acceptor task cannot queue the next
        // stream while this one is being read.
        let next_stream = streams.borrow_mut().pop_front();
        if let Some(stream) = next_stream {
            // Drained here rather than by the acceptor so the ordering above
            // holds: the deltas racing to overtake this keyframe stay unread
            // until it has been decoded.
            let bytes = match crate::read_to_end(stream).await {
                Ok(bytes) => bytes,
                Err(err) => {
                    // A stream that dies mid-transfer takes its keyframe with
                    // it; the next one, or a loss, resynchronises us.
                    log::warn!("server stream read failed: {err:?}");
                    continue;
                }
            };
            if !drained.replace(true) {
                log::info!("first server stream drained ({} bytes)", bytes.len());
            } else {
                log::debug!("server stream drained ({} bytes)", bytes.len());
            }
            if let Ok(msg) = ServerDatagram::from_bytes(&bytes) {
                return Ok(Some(msg));
            }
            log::warn!("unparsable server stream, ignored");
            continue;
        }
        if let Some(msg) = deferred.take() {
            return Ok(Some(msg));
        }
        let data = read_datagram().await?;
        let msg = ServerDatagram::from_bytes(&data);
        if !streams.borrow().is_empty() {
            // A keyframe this datagram is predicted from was accepted while the
            // read was outstanding, so it has to be decoded first.
            *deferred = msg.ok();
            continue;
        }
        return Ok(msg.ok());
    }
}

/// Read the next datagram off the WebTransport (unreliable datagram) stream
/// as raw bytes. An `Err` means the stream ended or failed and the render
/// loop must stop.
async fn read_datagram() -> Result<Vec<u8>, JsValue> {
    let promise = with_wt(|gwt| gwt.reader.read());
    let result = JsFuture::from(promise).await;

    match result {
        Ok(val) => {
            if val.is_undefined() || val.is_null() {
                log::info!("render_loop: stream ended (null/undefined)");
                return Err(JsValue::from_str("stream ended"));
            }
            let done = js_sys::Reflect::get(&val, &"done".into())
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if done {
                log::info!("render_loop: stream done");
                return Err(JsValue::from_str("stream done"));
            }
            let value = match js_sys::Reflect::get(&val, &"value".into()) {
                Ok(v) if !v.is_undefined() && !v.is_null() => v,
                _ => {
                    log::warn!("render_loop: missing value");
                    return Err(JsValue::from_str("missing value"));
                }
            };
            let arr = js_sys::Uint8Array::new(&value);
            let mut buf = vec![0u8; arr.length() as usize];
            arr.copy_to(&mut buf);
            Ok(buf)
        }
        Err(e) => {
            log::error!("render_loop: read error: {e:?}");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Point the decoder at `codec`, reconfiguring only when the codec actually
/// changed: `configure` drops every chunk already queued, so calling it for
/// every frame would itself throw frames away.
fn configure_decoder(
    decoder: &web_sys::VideoDecoder,
    current: &RefCell<Option<Codec>>,
    codec: Codec,
) {
    let mut current = current.borrow_mut();
    if *current == Some(codec) {
        return;
    }
    log::info!("codec changed: {current:?} -> {codec:?}");
    let config = js_sys::Object::new();
    js_sys::Reflect::set(&config, &"codec".into(), &codec.web_codec_string().into()).ok();
    js_sys::Reflect::set(&config, &"optimizeForLatency".into(), &JsValue::TRUE).ok();
    decoder.configure(config.unchecked_ref::<web_sys::VideoDecoderConfig>());
    *current = Some(codec);
}

/// Hand one assembled frame to the decoder, typed as a key or a delta.
fn decode_frame(decoder: &web_sys::VideoDecoder, frame_id: u16, payload: &[u8], is_keyframe: bool) {
    let chunk_init = js_sys::Object::new();
    let chunk_type = if is_keyframe { "key" } else { "delta" };
    js_sys::Reflect::set(&chunk_init, &"type".into(), &JsValue::from_str(chunk_type)).ok();
    js_sys::Reflect::set(
        &chunk_init,
        &"timestamp".into(),
        &JsValue::from_f64((frame_id as u64 * 1000) as f64),
    )
    .ok();
    let data_arr = js_sys::Uint8Array::from(payload);
    js_sys::Reflect::set(&chunk_init, &"data".into(), &data_arr).ok();

    match EncodedVideoChunk::new(chunk_init.unchecked_ref::<web_sys::EncodedVideoChunkInit>()) {
        Ok(chunk) => decoder.decode(&chunk),
        Err(e) => log::error!("EncodedVideoChunk creation failed: {e:?}"),
    }
}

// ---------------------------------------------------------------------------
// Render loop
// ---------------------------------------------------------------------------

/// Everything an encoded frame passes through on its way to the screen: the
/// canvas, the decoder, and the ordering policy that owns the prediction
/// chain.
///
/// Split out of [`render_loop`] because it is the half of the loop a session
/// with no display does not have. A `VideoDecoder` is the expensive part, and
/// a decoder with nothing to decode is a hardware context held open for the
/// life of the session.
fn frame_path(canvas: &HtmlCanvasElement) -> Result<FramePath, JsValue> {
    let ctx = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|v| v.dyn_into::<CanvasRenderingContext2d>().ok())
        .expect("no 2d context");

    // VideoDecoder callbacks
    let output_cb = {
        let ctx = ctx.clone();
        let canvas = canvas.clone();
        Closure::wrap(Box::new(move |frame: VideoFrame| {
            if canvas.width() != frame.display_width() || canvas.height() != frame.display_height()
            {
                canvas.set_width(frame.display_width());
                canvas.set_height(frame.display_height());
            }
            let _ = ctx.draw_image_with_video_frame(&frame, 0.0, 0.0);
            frame.close();
        }) as Box<dyn FnMut(VideoFrame)>)
    };

    let error_cb = Closure::wrap(Box::new(move |err: JsValue| {
        web_sys::console::error_1(&format!("VideoDecoder error: {:?}", err).into());
    }) as Box<dyn FnMut(JsValue)>);

    // Build init dict via Reflect.set for compatibility.
    let init = js_sys::Object::new();
    js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref().unchecked_ref()).ok();
    js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref().unchecked_ref()).ok();

    let decoder = web_sys::VideoDecoder::new(init.unchecked_ref::<web_sys::VideoDecoderInit>())
        .map_err(|_| web_sys::console::error_1(&"Failed to create VideoDecoder".into()))
        .ok();

    let decoder = match decoder {
        Some(d) => d,
        None => {
            log::error!("VideoDecoder creation failed");
            return Err(JsValue::from_str("VideoDecoder creation failed"));
        }
    };

    Ok(FramePath {
        decoder,
        // Owned here so the JS references the decoder holds outlive this
        // builder call; see the fields' docs.
        _output: output_cb,
        _error: error_cb,
        // The decoder is configured on the first frame, or whenever the codec
        // changes.
        current_codec: Rc::new(RefCell::new(None)),
        gate: FrameGate::default(),
        pending: Rc::new(RefCell::new(HashMap::new())),
    })
}

pub async fn render_loop(
    display: Option<&Display>,
    release_flag: Rc<Cell<bool>>,
    streams: PendingStreams,
) -> Result<(), JsValue> {
    // The decode path belongs to the display, so an audio-only session has
    // none. Every kind of session runs this loop regardless, because it is also
    // what carries the audio and what returns when the transport dies. A video
    // frame arriving without a decode path is one the server had no reason to
    // send -- its capture is gated on the same signal this session withheld --
    // so it is dropped rather than guessed at.
    let mut path: Option<FramePath> = match display {
        Some(display) => Some(frame_path(&display.canvas)?),
        None => None,
    };

    // Audio player: decodes the Opus frames we receive and plays them through
    // the (user-gesture-resumed) AudioContext. `None` if the browser lacks an
    // Opus AudioDecoder, in which case AudioFrames are ignored.
    let audio = AudioPlayer::new();
    if audio.is_none() {
        log::error!("audio: AudioPlayer::new() returned None — no Opus AudioDecoder / AudioContext");
        // On the /audio page, tell this user, client-side, why there is no
        // sound: without a player there is no activity feed to derive a state
        // from, so the badge is fixed at "unavailable".
        if display.is_none() {
            crate::audio::StatusBadge::unavailable();
        }
    }

    // The visualiser and its status badge are /audio-page decorations: a video
    // session keeps the whole screen for the remote desktop. Both bind to the
    // player's analyser / activity feed, and neither creates nor sends
    // anything the server could see.
    let _audio_page_ui = match (&audio, display) {
        (Some(player), None) => {
            crate::audio::StatusBadge::with_activity(player.activity());
            Some(crate::visualiser::Visualiser::new(&player.analyser()))
        }
        _ => None,
    };

    // A datagram held back because a keyframe the server opened was still
    // being drained; see `next_message`.
    let mut deferred: Option<ServerDatagram> = None;
    // Whether a server stream has ever been drained; see `next_message`.
    let drained = Cell::new(false);
    // When the outstanding repair was requested, and `None` when there is none.
    //
    // Checked on every message rather than on a timer, because the loop only
    // runs when the server is sending: a deadline that is never reached is
    // reached exactly when the next frame arrives, and a server that has gone
    // quiet has no gaps left to escalate. See `REPAIR_TIMEOUT_MS`.
    let mut repair_deadline: Option<f64> = None;

    // How many messages to work through before yielding to the event loop. When
    // the server is sending faster than we can decode, every read resolves from
    // the queue without suspending, and the loop would run for as long as the
    // backlog lasts without the keepalive timer ever getting a turn. See
    // `crate::yield_to_event_loop`.
    const MESSAGES_PER_YIELD: u32 = 32;
    let mut since_yield: u32 = 0;

    loop {
        if since_yield >= MESSAGES_PER_YIELD {
            since_yield = 0;
            crate::yield_to_event_loop().await;
        }
        // A keyframe only ever arrives on its own stream, as one whole message
        // that is a distinct message type, so which frame of a split delta this
        // completes is decided by the variant rather than by a flag.
        let Some(msg) = next_message(&streams, &mut deferred, &drained).await? else {
            continue;
        };
        since_yield += 1;

        // A repair that has not come back by now is not coming back: the request
        // was lost, or the resend was, or the frame is one the server no longer
        // holds. Asking for a keyframe is the only thing that closes a gap a
        // resend cannot, and waiting longer would cost more than the keyframe
        // would have.
        if let Some(deadline) = repair_deadline
            && now_ms() >= deadline
        {
            repair_deadline = None;
            if path
                .as_ref()
                .is_some_and(|path| path.gate.awaiting_repair())
            {
                log::debug!("repair deadline passed; asking for a keyframe");
                send_request_keyframe().await;
            }
        }

        match msg {
            ServerDatagram::AudioFrame {
                frame_id,
                frag_idx,
                num_frags,
                channels,
                rate,
                format: _,
                payload,
            } => {
                if let Some(a) = &audio {
                    a.push(frame_id, frag_idx, num_frags, channels, rate, payload);
                }
                continue;
            }
            ServerDatagram::LogLevel { level } => {
                crate::log::apply_server_level(level);
                continue;
            }
            ServerDatagram::ReleaseMouse => {
                release_flag.set(true);
                continue;
            }
            ServerDatagram::ToggleFullscreen => {
                // Broadcast to every session, including one with no display,
                // where there is nothing to make fullscreen.
                let Some(display) = display else { continue };
                let window = web_sys::window().unwrap();
                let document = window.document().unwrap();
                if document.fullscreen_element().is_some() {
                    // Exiting fullscreen is allowed without a user gesture.
                    let _ = document.exit_fullscreen();
                } else if is_installed_pwa(&window) {
                    // Installed PWAs (standalone display mode) are granted
                    // fullscreen without a fresh user gesture — do it now.
                    let _ = display.canvas.request_fullscreen();
                } else {
                    // In-browser tabs require a gesture; defer to the next pointerdown.
                    display.pending_fullscreen.set(true);
                }
                continue;
            }
            ServerDatagram::Throttle { interval_ms } => {
                crate::throttle::set_throttle(interval_ms);
                continue;
            }
            ServerDatagram::VideoKeyFrame {
                frame_id,
                codec,
                payload,
            } => {
                let Some(path) = path.as_mut() else {
                    log::warn!("video keyframe on a session with no display, dropped");
                    continue;
                };
                // Already the whole frame. The variant has no fragment fields, so
                // there is nothing to reassemble and no way for half of one to
                // go missing.
                present(
                    path,
                    frame_id,
                    /* is_keyframe = */ true,
                    codec,
                    payload,
                    &mut repair_deadline,
                )
                .await;
            }
            ServerDatagram::VideoDelta {
                frame_id,
                frag_idx,
                num_frags,
                codec,
                payload,
            } => {
                let Some(path) = path.as_mut() else {
                    log::warn!("video delta on a session with no display, dropped");
                    continue;
                };
                // Split across as many datagrams as it needed; whole only once
                // the last fragment lands.
                let Some(assembled) =
                    assemble_delta(&path.pending, frame_id, frag_idx, num_frags, payload)
                else {
                    continue;
                };
                present(
                    path,
                    frame_id,
                    /* is_keyframe = */ false,
                    codec,
                    assembled,
                    &mut repair_deadline,
                )
                .await;
            }
        }
    }
}
