use shared::client_datagram::ClientDatagram;
use shared::codec::Codec;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{
    CanvasRenderingContext2d, EncodedVideoChunk, HtmlCanvasElement, HtmlDivElement, KeyboardEvent,
    VideoFrame,
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
#[derive(Clone)]
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
    crate::send_datagram(ClientDatagram::DisplayParameters {
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
        let msg = ClientDatagram::DisplayParameters {
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
pub(crate) fn is_installed_pwa(window: &web_sys::Window) -> bool {
    window
        .match_media("(display-mode: standalone)")
        .ok()
        .flatten()
        .map(|m| m.matches())
        .unwrap_or(false)
}

/// The card the /audio page is built in: the status indicator and start button
/// on top, the visualiser below, all laid out in one column so nothing is
/// pinned over anything else.
pub(crate) fn create_audio_container() -> HtmlDivElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let container = document
        .create_element("div")
        .unwrap()
        .dyn_into::<HtmlDivElement>()
        .unwrap();
    container.style().set_css_text(
        "position:fixed;top:50%;left:50%;transform:translate(-50%,-50%);\
         width:min(560px,calc(100vw - 32px));box-sizing:border-box;padding:20px;\
         display:flex;flex-direction:column;align-items:stretch;gap:14px;\
         background:#16161b;border:1px solid #33333f;border-radius:14px;\
         box-shadow:0 18px 48px rgba(0,0,0,.55);z-index:1000;pointer-events:auto;",
    );
    document.body().unwrap().append_child(&container).unwrap();
    container
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

fn probe_codecs() -> Vec<Codec> {
    let mut supported = Vec::new();
    for codec in &Codec::ALL {
        let codec_str = codec.web_codec_string();
        let mime = format!("video/mp4; codecs=\"{codec_str}\"");
        if web_sys::MediaSource::is_type_supported(&mime) {
            supported.push(*codec);
        }
    }
    if supported.is_empty() {
        supported.push(Codec::Av1);
        supported.push(Codec::H264);
    }
    log::info!("probed codecs: {supported:?}");
    supported
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

/// Whether a published frame is a keyframe, from the byte the publisher prefixed.
///
/// The publisher tags every frame rather than relying on its position in a group,
/// because a subscription can resolve mid-group. `None` for a frame with no tag at
/// all, which is not a frame this decoder can be given.
fn frame_tag(payload: &[u8]) -> Option<bool> {
    payload.first().map(|tag| *tag != 0)
}

/// Hand one frame to the decoder: the encoded bytes after the tag byte, with the
/// chunk type the tag named.
///
/// The tag is read once, by [`frame_tag`], and passed in — a frame whose type the
/// caller has not established is not decoded on a guess.
fn decode_frame(
    decoder: &web_sys::VideoDecoder,
    tagged: &[u8],
    is_keyframe: bool,
    timestamp_us: f64,
) {
    let Some((_tag, payload)) = tagged.split_first() else {
        log::warn!("video frame has no tag byte, dropped");
        return;
    };
    let chunk_init = js_sys::Object::new();
    let chunk_type = if is_keyframe { "key" } else { "delta" };
    js_sys::Reflect::set(&chunk_init, &"type".into(), &JsValue::from_str(chunk_type)).ok();
    js_sys::Reflect::set(
        &chunk_init,
        &"timestamp".into(),
        &JsValue::from_f64(timestamp_us),
    )
    .ok();
    let data_arr = js_sys::Uint8Array::from(payload);
    js_sys::Reflect::set(&chunk_init, &"data".into(), &data_arr).ok();

    match EncodedVideoChunk::new(chunk_init.unchecked_ref::<web_sys::EncodedVideoChunkInit>()) {
        Ok(chunk) => decoder.decode(&chunk),
        Err(e) => log::error!("EncodedVideoChunk creation failed: {e:?}"),
    }
}

/// Everything an encoded frame passes through on its way to the screen: the
/// canvas, the decoder, and the current codec — the client-chosen configuration
/// that codec change must re-arm.
///
/// Split out of [`start_video`] because it is the half of the subscriber a
/// session with no display does not have. A `VideoDecoder` is the expensive
/// part, and a decoder with nothing to decode is a hardware context held open
/// for the life of the session.
pub(crate) fn frame_path(canvas: &HtmlCanvasElement) -> Result<FramePath, JsValue> {
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
        _output: output_cb,
        _error: error_cb,
        current_codec: Rc::new(RefCell::new(None)),
    })
}

/// The decoder, its output callback and the last configuration.
pub(crate) struct FramePath {
    decoder: web_sys::VideoDecoder,
    /// Lifetime anchor: the decoder holds JS refs to these Closure callbacks.
    _output: Closure<dyn FnMut(VideoFrame)>,
    _error: Closure<dyn FnMut(JsValue)>,
    /// The codec the decoder was last configured for, so a genuine codec change
    /// is a one-line reconfigure and every subsequent frame takes the
    /// no-op path.
    current_codec: Rc<RefCell<Option<Codec>>>,
}

/// Decode one video track from its live edge until the track ends.
///
/// The frames arrive in delivery order and whole, which is what the gate, the
/// fragment reassembler and the resend protocol used to reconstruct: one moq-lite
/// group is one GOP on one stream, so the decoder is fed a keyframe first and
/// then that GOP's deltas, in order, and a lost group is a lost group rather than
/// a hole in someone else's frame.
///
/// Returns when the track ends, which is how a codec change arrives: the server
/// finishes the old track and creates a new one under a new name, and the
/// `VideoTrack` message that announced it starts a fresh task with a fresh
/// decoder. The old decoder goes with this task, so the canvas stops being written
/// by it even if its last group is still draining.
pub(crate) async fn run_video_track(
    consumer: &moq_net::broadcast::Consumer,
    codec: Codec,
    path: FramePath,
) {
    let track_name = shared::track_names::video_track(codec);
    let track = match consumer.track(&track_name) {
        Ok(track) => track,
        Err(err) => {
            log::warn!("video track {track_name} not in the broadcast: {err:#?}");
            return;
        }
    };
    let mut subscribed = match track
        .subscribe(moq_net::track::Subscription::default())
        .await
    {
        Ok(subscribed) => subscribed,
        Err(err) => {
            log::warn!("video subscribe to {track_name} failed: {err:#?}");
            return;
        }
    };

    // Ask for a keyframe rather than waiting for the encoder's own schedule: the
    // subscription lands wherever the track happens to be, and if that is
    // mid-GOP the first frames cannot be decoded until a keyframe cuts the next
    // group.
    crate::send_reliable(ClientDatagram::RequestKeyframe).await;

    // Everything before the first keyframe is dropped. A group is a GOP, so a
    // subscription that resolves inside one may start with deltas that reference a
    // frame this decoder never saw; the keyframe that opens the group after them
    // is where decoding can start.
    let mut awaiting_keyframe = true;

    loop {
        match subscribed.recv_group().await {
            Ok(Some(mut group)) => loop {
                match group.read_frame().await {
                    Ok(Some(frame)) => {
                        let Some(is_keyframe) = frame_tag(&frame.payload) else {
                            continue;
                        };
                        if awaiting_keyframe {
                            if !is_keyframe {
                                continue;
                            }
                            awaiting_keyframe = false;
                        }
                        configure_decoder(&path.decoder, &path.current_codec, codec);
                        let timestamp_us = (frame.timestamp.as_millis() as f64) * 1000.0;
                        decode_frame(&path.decoder, &frame.payload[..], is_keyframe, timestamp_us);
                    }
                    // A group ends at its own boundary and a failed one is
                    // dropped whole: the next group is a fresh stream, and it
                    // starts with a keyframe, so carrying on costs at most this
                    // GOP and never leaves the decoder without a base frame.
                    Ok(None) => break,
                    Err(err) => {
                        log::warn!("video group stream error: {err:#?}");
                        break;
                    }
                }
            },
            Ok(None) => return,
            Err(err) => {
                log::warn!("video track {track_name} terminated: {err:#?}");
                return;
            }
        }
    }
}

// `FramePath` needs a non-async `Drop` handle: kept as such because its
// contents need the JS blobs to outlive the decoder binding carefully.
