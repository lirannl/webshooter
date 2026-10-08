use shared::client_datagram::ClientDatagram;
use shared::codec::Codec;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    CanvasRenderingContext2d, EncodedVideoChunk, HtmlCanvasElement, HtmlDivElement, KeyboardEvent,
    MediaQueryList, VideoFrame,
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

/// The canvas's size in device pixels — what the server builds its virtual
/// monitor to match — or `None` while it has none.
///
/// The `f64 as u16` casts saturate rather than wrap, which a multiplication
/// done in `u16` would not: a wide display at a high device pixel ratio clamps
/// to `u16::MAX` instead of asking the server for a 0×0 monitor.
///
/// A canvas that is not laid out — detached, or not yet painted — measures
/// 0×0, and that is not a display the server could honour: it would build a
/// 0×0 monitor and then find no stream at that size. So zero is reported as
/// no size at all, and the next real observation reports the truth. That also
/// covers a stale canvas: the previous session's handlers stay attached to a
/// canvas that has since been detached, and a fullscreen exit delivers its
/// event to that canvas, where it must not put a 0×0 monitor on the wire of
/// whichever session is current now.
fn display_size(canvas: &HtmlCanvasElement) -> Result<Option<(u16, u16)>, JsError> {
    let window = web_sys::window().ok_or(JsError::new("Window not found"))?;
    let dpr = window.device_pixel_ratio();
    let width = (f64::from(canvas.offset_width()) * dpr) as u16;
    let height = (f64::from(canvas.offset_height()) * dpr) as u16;
    Ok((width > 0 && height > 0).then_some((width, height)))
}

/// Report the canvas's current size to the server, and return what was sent —
/// or `None` if the canvas has no size to report yet.
///
/// Sent once, when the display is built and before anything is observing the
/// canvas: this is the datagram the server waits for before it creates a
/// virtual monitor at all, so a session that never sends it has no display and
/// no input path. When there is no size to send, the caller's next observation
/// reports it instead — which is how a display recovers from a first measurement
/// taken before the page was laid out.
///
/// Reliably, on a unidirectional stream: a lost size leaves the server capturing
/// at the old one, and nothing retries it, because a client whose canvas has
/// not changed since has no further observation to make. A failure is reported
/// as an error so the caller can leave the display unsent rather than believing
/// the server has a size it never received.
///
/// The returned size seeds [`DisplayResize`], which is what lets the observer's
/// initial callback — reporting the size just sent — be recognised as nothing
/// new rather than as a resize.
pub async fn send_initial_resize(
    canvas: &HtmlCanvasElement,
) -> Result<Option<(u16, u16)>, JsError> {
    let Some((width, height)) = display_size(canvas)? else {
        return Ok(None);
    };
    let sent = crate::send_reliable(ClientDatagram::DisplayParameters {
        index: 0,
        width,
        height,
    })
    .await;
    if !sent {
        return Err(JsError::new("DisplayParameters could not be sent"));
    }
    log::info!("canvas sized {width}x{height}");
    Ok(Some((width, height)))
}

// ---------------------------------------------------------------------------
// Resize reporting
// ---------------------------------------------------------------------------

/// How long the canvas has to hold one size before it is reported, in ms.
///
/// `DisplayParameters` is the most expensive message the client can send:
/// receiving one makes the server close its portal session and rebuild the
/// whole capture pipeline, encoder included, and a new encoder's first frame
/// owes the client a keyframe it then has to ask for. A window drag, an
/// address-bar collapse or a rotation therefore has to arrive as one message
/// carrying the size the user actually ended up with — not one per
/// intermediate size.
const RESIZE_SETTLE_MS: f64 = 200.0;

thread_local! {
    /// The display being watched. One canvas exists per session, so the settle
    /// timer can reach the sender without every callback having to carry it.
    static RESIZE: RefCell<Option<Rc<DisplayResize>>> = const { RefCell::new(None) };
    /// The settle timer's callback, held here rather than forgotten on each arm
    /// so the one `Closure` is reused for the session.
    static SETTLE_CB: Closure<dyn FnMut()> =
        Closure::wrap(Box::new(settle_expired) as Box<dyn FnMut()>);
}

type Size = (u16, u16);

/// What the debouncer wants its caller to do about a size.
#[derive(Debug, PartialEq, Eq)]
enum ResizeEffect {
    /// Nothing: the size is already sent, or already waiting to be.
    Idle,
    /// (Re)arm the settle timer — a new size is waiting for it to expire.
    Arm,
    /// Forget the outstanding size and cancel the timer.
    Cancel,
    /// Put `size` on the wire. Should the send fail, `restore` becomes the
    /// last-known size again, so a later observation of `size` can retry it.
    Send { size: Size, restore: Option<Size> },
}

/// The decision half of the debounced resize sender: what size, if any, should
/// go on the wire, and when.
///
/// Deliberately free of `web_sys`, timers and transport, because every rule
/// here is one that can be wrong in a way nothing else would notice: a
/// dropped size leaves the server capturing at the old one with no way to hear
/// about the current one, and it fails *silently*. The tests in this module
/// drive it directly, and [`DisplayResize`] is the thin shell that carries the
/// decisions out — measuring the canvas, arming a timer, and opening a stream.
#[derive(Debug, Default)]
struct ResizeDebounce {
    /// Size last put on the wire, or believed to be on it.
    sent: Option<Size>,
    /// Size seen but not yet sent; `None` when nothing is outstanding.
    pending: Option<Size>,
    /// A send is on the wire. At most one at a time, which is what makes the
    /// order the server sees them in the order they were decided: every send
    /// is its own unidirectional stream, and the server accepts those in the
    /// order they were opened, but only if two are never opened at once.
    in_flight: bool,
    /// The size to send once the in-flight one lands.
    queued: Option<Size>,
}

impl ResizeDebounce {
    /// `initial` is what [`send_initial_resize`] already reported, or `None` if
    /// it reported nothing — so the observer's first callback is not mistaken
    /// for a resize the server has not heard about.
    fn new(initial: Option<Size>) -> Self {
        Self {
            sent: initial,
            ..Default::default()
        }
    }

    /// Note a new size, to be sent once it has held still for
    /// [`RESIZE_SETTLE_MS`] — or dropped if the server already has it.
    ///
    /// Every observation re-arms the timer, so a burst of them collapses into
    /// one send of the *last* size in the burst. That trailing send is the
    /// point: a cooldown that merely dropped in-window observations, with
    /// nothing scheduled behind them, silently lost the end of every window
    /// drag and every rotation a second after page load. `ResizeObserver` only
    /// fires on change, so a dropped observation was never retried — the
    /// server simply kept the stale size until the next unrelated change.
    fn observe(&mut self, size: Size) -> ResizeEffect {
        if self.pending == Some(size) {
            // Already waiting to be sent. A repeated observation of the same
            // size must not restart the settle window, or a box that jitters
            // would never be reported at all.
            return ResizeEffect::Idle;
        }
        if self.sent == Some(size) {
            // Back to a size the server already has: nothing to send, and
            // whatever was waiting is now obsolete. `Cancel` only when there
            // was something to cancel, so a caller can tell "dropped what you
            // had" from "there was never anything here".
            return if self.pending.take().is_some() {
                ResizeEffect::Cancel
            } else {
                ResizeEffect::Idle
            };
        }
        self.pending = Some(size);
        ResizeEffect::Arm
    }

    /// The settle window expired.
    fn flush(&mut self) -> ResizeEffect {
        match self.pending.take() {
            Some(size) if self.sent != Some(size) => self.begin_send(size),
            _ => ResizeEffect::Idle,
        }
    }

    /// A send finished, successfully if `ok`. Reports what, if anything, should
    /// go out next, draining a size that was waiting for this one to land.
    fn completed(&mut self, restore: Option<Size>, ok: bool) -> ResizeEffect {
        self.in_flight = false;
        if !ok {
            // The server does not have this size after all, so put back what it
            // last did: a later observation of the failed size must be free to
            // send it again.
            self.sent = restore;
        }
        match self.queued.take() {
            // Coalesced: the queued size is the one just sent, or is already
            // on the wire, so a burst that spans a send costs one stream.
            Some(queued) if self.sent != Some(queued) => self.begin_send(queued),
            _ => ResizeEffect::Idle,
        }
    }

    /// Claim the wire for `size`, or queue it behind the send in flight.
    fn begin_send(&mut self, size: Size) -> ResizeEffect {
        if self.in_flight {
            self.queued = Some(size);
            return ResizeEffect::Idle;
        }
        self.in_flight = true;
        let restore = self.sent.replace(size);
        ResizeEffect::Send { size, restore }
    }
}

/// The debounced `DisplayParameters` sender for one display: the
/// [`ResizeDebounce`] decisions plus everything needed to carry them out.
///
/// Size changes reach it from three places — the canvas box changing, the
/// orientation flipping, the fullscreen box changing — and all three land in
/// [`DisplayResize::observe`], so a rotation that also resizes the canvas
/// costs one datagram rather than two.
struct DisplayResize {
    debounce: RefCell<ResizeDebounce>,
    /// The outstanding settle timer, so a fresh observation cancels it instead
    /// of racing it.
    timer: Cell<Option<i32>>,
    /// The orientation media query, held for the session so the listener
    /// attached to it stays attached.
    orientation: Option<MediaQueryList>,
}

impl DisplayResize {
    fn new(initial: Option<Size>, orientation: Option<MediaQueryList>) -> Self {
        Self {
            debounce: RefCell::new(ResizeDebounce::new(initial)),
            timer: Cell::new(None),
            orientation,
        }
    }

    /// Measure the canvas and feed the result to the debouncer, carrying out
    /// whatever it decides.
    fn observe(self: &Rc<Self>, canvas: &HtmlCanvasElement) {
        let size = match display_size(canvas) {
            Ok(Some(size)) => size,
            // No size to report, so nothing to schedule. A canvas that only now
            // has a size will be observed as changed.
            Ok(None) => return,
            Err(err) => {
                log::error!("cannot measure canvas: {err:?}");
                return;
            }
        };
        self.act(self.debounce.borrow_mut().observe(size));
    }

    /// The settle window expired.
    fn flush(self: &Rc<Self>) {
        self.timer.set(None);
        self.act(self.debounce.borrow_mut().flush());
    }

    /// Perform a decision. The borrow is always released first, because
    /// [`ResizeEffect::Send`] re-enters this type when the send completes.
    fn act(self: &Rc<Self>, effect: ResizeEffect) {
        match effect {
            ResizeEffect::Idle => {}
            ResizeEffect::Arm => self.arm(),
            ResizeEffect::Cancel => self.clear_timer(),
            ResizeEffect::Send { size, restore } => self.send(size, restore),
        }
    }

    /// Arm the settle timer, replacing any already armed.
    fn arm(&self) {
        self.clear_timer();
        let cb = SETTLE_CB.with(|cb| cb.as_ref().unchecked_ref::<js_sys::Function>().clone());
        let Some(window) = web_sys::window() else {
            log::error!("no window: resize settle timer not armed");
            return;
        };
        match window
            .set_timeout_with_callback_and_timeout_and_arguments_0(&cb, RESIZE_SETTLE_MS as i32)
        {
            Ok(id) => self.timer.set(Some(id)),
            Err(err) => log::error!("resize settle timer not armed: {err:?}"),
        }
    }

    fn clear_timer(&self) {
        if let Some(id) = self.timer.take()
            && let Some(window) = web_sys::window()
        {
            window.clear_timeout_with_handle(id);
        }
    }

    /// Put `size` on a fresh unidirectional stream, reporting back to the
    /// debouncer when it lands.
    fn send(self: &Rc<Self>, size: Size, restore: Option<Size>) {
        let resize = self.clone();
        spawn_local(async move {
            let (width, height) = size;
            let ok = crate::send_reliable(ClientDatagram::DisplayParameters {
                index: 0,
                width,
                height,
            })
            .await;
            if ok {
                log::info!("canvas resized: reported {width}x{height}");
            } else {
                log::error!("resize {width}x{height} could not be sent");
            }
            let next = resize.debounce.borrow_mut().completed(restore, ok);
            resize.act(next);
        });
    }
}

fn settle_expired() {
    // The sender is cloned out of the thread-local first so its borrow is
    // released before `flush` runs.
    let resize = RESIZE.with(|cell| cell.borrow().clone());
    if let Some(resize) = resize {
        resize.flush();
    }
}

/// Watch `canvas` for size changes and report them, and return the flag
/// carrying a fullscreen intent from the render loop to the next user gesture,
/// which is the earliest point it can actually be applied at.
///
/// `initial` is what has already been reported for this display, so an
/// observation of it is not a resize.
pub fn setup_display_events(
    canvas: &HtmlCanvasElement,
    initial: Option<(u16, u16)>,
) -> Rc<Cell<bool>> {
    let window = web_sys::window().unwrap();

    // A rotation is reported through a media query as well as through the
    // canvas box. Usually the box does move with the orientation and the
    // `ResizeObserver` below has it covered; the media query is the
    // authoritative signal for the change, and it also fires when the box
    // stays put — a foldable unfolding, or a browser that resizes only the
    // visual viewport. Both paths feed one debounce, so a rotation that does
    // resize the canvas still costs a single datagram.
    let orientation = window.match_media("(orientation: portrait)").ok().flatten();
    let resize = Rc::new(DisplayResize::new(initial, orientation));
    RESIZE.with(|cell| *cell.borrow_mut() = Some(resize.clone()));

    let ro_cb = Closure::wrap(Box::new({
        let resize = resize.clone();
        let canvas = canvas.clone();
        move || resize.observe(&canvas)
    }) as Box<dyn FnMut()>);
    let ro = web_sys::ResizeObserver::new(ro_cb.as_ref().unchecked_ref::<js_sys::Function>())
        .expect("ResizeObserver exists wherever WebTransport does");
    ro.observe(canvas);
    ro_cb.forget();
    // Leaked deliberately. An observation is only kept for as long as its
    // observer is, and the renderer keeps an observer alive on account of the
    // element it observes rather than on account of the script that created
    // it — an implementation detail, not the contract. Holding the handle
    // makes the lifetime ours to reason about instead.
    //
    // `std::mem::forget` and not a `forget()` method: `ResizeObserver` extends
    // `js_sys::Object`, so wasm-bindgen models it as a borrowed handle, which
    // it does not generate `forget()` for.
    std::mem::forget(ro);

    if let Some(orientation) = resize.orientation.as_ref() {
        let orientation_cb = Closure::wrap(Box::new({
            let resize = resize.clone();
            let canvas = canvas.clone();
            move || resize.observe(&canvas)
        }) as Box<dyn FnMut()>);
        let _ = orientation.add_event_listener_with_callback(
            "change",
            orientation_cb.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        orientation_cb.forget();
    }

    // Entering and leaving fullscreen both resize the canvas, and
    // `fullscreenchange` is delivered to the canvas because the canvas is the
    // fullscreen element. The size is read from the canvas rather than from
    // `innerWidth`: the two agree in fullscreen, and one number the browser
    // recomputes is one fewer thing to keep correct.
    let fullscreen_cb = Closure::wrap(Box::new({
        let resize = resize.clone();
        let canvas = canvas.clone();
        move || resize.observe(&canvas)
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
    // Decoder outputs so far, counted from the output callback: the far end of
    // the pipeline, set beside `decode_queue_size()` in `run_video_track`'s
    // periodic report. WebCodecs keeps every frame it is handed in its queue,
    // so this pair is what separates "the server sent slowly" from "the
    // decoder fell behind" — the difference grows nowhere else.
    let outputs = Rc::new(Cell::new(0u32));
    let output_cb = {
        let ctx = ctx.clone();
        let canvas = canvas.clone();
        let outputs = Rc::clone(&outputs);
        Closure::wrap(Box::new(move |frame: VideoFrame| {
            outputs.set(outputs.get().wrapping_add(1));
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
        outputs,
        _output: output_cb,
        _error: error_cb,
        current_codec: Rc::new(RefCell::new(None)),
    })
}

/// The decoder, its output callback and the last configuration.
pub(crate) struct FramePath {
    decoder: web_sys::VideoDecoder,
    /// Frames the decoder has produced, counted by the output callback.
    outputs: Rc<Cell<u32>>,
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

    // How many frames the decoder may hold before this loop stops feeding it.
    //
    // Six is a little over one frame at the server's 60 fps cap — enough to
    // absorb a scheduling hiccup without turning it into visible stutter, not
    // enough to accumulate into perceptible delay. WebCodecs itself has no
    // bound: it will queue whatever it is handed for as long as it takes to
    // decode it, and the wait is paid entirely in latency.
    const MAX_DECODE_QUEUE: u32 = 6;

    // Receipt/decode accounting, reported every few seconds so it lands beside
    // the server's own `frame rate:` lines in the same log. Three numbers with
    // three distinct owners: `arrived` is what the transport delivered (if it
    // lags the server's encoded count, the queue is between here and the wire),
    // `fed` is what passed the keyframe gate into WebCodecs, and `decoded` is
    // what came back out. WebCodecs holds every fed frame until it decodes it,
    // so a `decode queue` that only climbs while a drag lasts *is* the growing
    // latency — its slope is the rate the delay accumulates.
    let mut arrived = 0u32;
    let mut fed = 0u32;
    let mut outputs_mark = path.outputs.get();
    let mut last_report = js_sys::Date::now();

    loop {
        match subscribed.recv_group().await {
            Ok(Some(mut group)) => loop {
                match group.read_frame().await {
                    Ok(Some(frame)) => {
                        arrived += 1;
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
                        fed += 1;

                        // The latency bound, enforced where it can be: once
                        // the decoder holds more than MAX_DECODE_QUEUE frames,
                        // everything queued inside it is already stale by the
                        // time it would be shown, and waiting only makes that
                        // worse. Raising the gate drops every frame up to the
                        // next keyframe — a GOP of quality traded for a
                        // picture that stays current — and the keyframe is
                        // asked for here, so the wait is one round trip.
                        if !awaiting_keyframe
                            && path.decoder.decode_queue_size() > MAX_DECODE_QUEUE
                        {
                            awaiting_keyframe = true;
                            crate::send_reliable(ClientDatagram::RequestKeyframe).await;
                        }

                        let now = js_sys::Date::now();
                        if now - last_report >= 5_000.0 {
                            let decoded = path.outputs.get().saturating_sub(outputs_mark);
                            log::info!(
                                "video stats: arrived {arrived} fed {fed} decoded {decoded} \
                                 in {:.1}s, decode queue {}",
                                (now - last_report) / 1_000.0,
                                path.decoder.decode_queue_size(),
                            );
                            arrived = 0;
                            fed = 0;
                            outputs_mark = path.outputs.get();
                            last_report = now;
                        }
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

#[cfg(test)]
mod tests {
    use super::{ResizeDebounce, ResizeEffect, Size};

    const A: Size = (1920, 1080);
    const B: Size = (1280, 720);
    const C: Size = (800, 600);

    /// The size an effect asks to be sent, or `None` for anything else.
    fn sending(effect: &ResizeEffect) -> Option<Size> {
        match effect {
            ResizeEffect::Send { size, .. } => Some(*size),
            _ => None,
        }
    }

    /// The regression this whole sender exists for: a resize inside the settle
    /// window of the previous one must still reach the server, and a burst must
    /// arrive as its *last* size.
    ///
    /// The bug it replaces dropped in-window observations with nothing
    /// scheduled behind them, so a rotation a second after page load was never
    /// sent at all.
    #[test]
    fn a_burst_collapses_to_its_last_size_and_that_size_is_sent() {
        let mut d = ResizeDebounce::new(Some(A));
        // A drag, observed faster than the settle window.
        for size in [B, C, (1024, 768), (1152, 648)] {
            assert_eq!(d.observe(size), ResizeEffect::Arm, "{size:?} must re-arm");
        }
        // Only one size ever reaches the wire, and it is the one the user ended
        // up with — not the first of the burst.
        assert_eq!(sending(&d.flush()), Some((1152, 648)));
    }

    /// A size that is already on the wire is not news. This is also what makes
    /// the `ResizeObserver`'s initial callback free: it reports the size
    /// `send_initial_resize` just sent.
    #[test]
    fn an_unchanged_size_is_not_resent() {
        let mut d = ResizeDebounce::new(Some(A));
        assert_eq!(d.observe(A), ResizeEffect::Idle);
        assert_eq!(d.flush(), ResizeEffect::Idle);
    }

    /// The same size observed again while it waits must not restart the settle
    /// window — otherwise a box that jitters would never be reported.
    #[test]
    fn a_repeated_pending_size_does_not_re_arm() {
        let mut d = ResizeDebounce::new(Some(A));
        assert_eq!(d.observe(B), ResizeEffect::Arm);
        assert_eq!(d.observe(B), ResizeEffect::Idle);
        assert_eq!(d.observe(B), ResizeEffect::Idle);
        assert_eq!(sending(&d.flush()), Some(B));
    }

    /// Returning to a size the server already has cancels the size that was
    /// waiting: the user is back where they started, so there is nothing to
    /// report.
    #[test]
    fn returning_to_the_sent_size_cancels_the_pending_one() {
        let mut d = ResizeDebounce::new(Some(A));
        assert_eq!(d.observe(B), ResizeEffect::Arm);
        assert_eq!(d.observe(A), ResizeEffect::Cancel);
        assert_eq!(d.flush(), ResizeEffect::Idle);
    }

    /// Two sends must never be open at once. The server accepts unidirectional
    /// streams in the order they were opened, so a second concurrent stream
    /// could put a stale size on the wire *after* the one that replaced it.
    #[test]
    fn a_second_send_waits_for_the_first_instead_of_racing_it() {
        let mut d = ResizeDebounce::new(None);
        assert_eq!(d.observe(A), ResizeEffect::Arm);
        let ResizeEffect::Send { size, restore } = d.flush() else {
            panic!("the first send must go out, not wait");
        };
        assert_eq!(size, A);
        // With A in flight, observing and flushing B must not open a stream.
        assert_eq!(d.observe(B), ResizeEffect::Arm);
        assert_eq!(d.flush(), ResizeEffect::Idle);
        // B goes out only once A has landed, and in that order.
        assert_eq!(sending(&d.completed(restore, true)), Some(B));
    }

    /// Ordering is not enough — the server must not be told to rebuild twice for
    /// one burst, and must not be rebuilt for a size it already has.
    #[test]
    fn a_burst_spanning_a_send_costs_one_stream() {
        let mut d = ResizeDebounce::new(None);
        assert_eq!(d.observe(A), ResizeEffect::Arm);
        let ResizeEffect::Send { restore, .. } = d.flush() else {
            panic!("the first send must go out");
        };
        // The canvas is back to A, the size just sent, while that send is still
        // in flight: coalesced, not a second stream.
        assert_eq!(d.observe(A), ResizeEffect::Idle);
        assert_eq!(d.flush(), ResizeEffect::Idle);
        assert_eq!(d.completed(restore, true), ResizeEffect::Idle);
    }

    /// A failed send must leave the debouncer willing to report that size again.
    /// Otherwise one dropped stream is permanent, which is the whole failure
    /// mode the reliable transport is there to prevent — and if it happens
    /// anyway, the next observation has to be able to fix it.
    #[test]
    fn a_failed_send_is_retried_rather_than_believed() {
        let mut d = ResizeDebounce::new(None);
        assert_eq!(d.observe(A), ResizeEffect::Arm);
        let ResizeEffect::Send { restore, .. } = d.flush() else {
            panic!("the first send must go out");
        };
        // A was never delivered, so the last size the server has is the one
        // before it — nothing, here.
        assert_eq!(d.completed(restore, false), ResizeEffect::Idle);
        // Observing the same size must now be news again.
        assert_eq!(d.observe(A), ResizeEffect::Arm);
        assert_eq!(sending(&d.flush()), Some(A));
    }

    /// A send that fails while another size is queued must not lose the queued
    /// one, and the two must not reorder.
    #[test]
    fn a_queued_size_survives_a_failed_send() {
        let mut d = ResizeDebounce::new(None);
        assert_eq!(d.observe(A), ResizeEffect::Arm);
        let ResizeEffect::Send { restore, .. } = d.flush() else {
            panic!("the first send must go out");
        };
        assert_eq!(d.observe(B), ResizeEffect::Arm);
        assert_eq!(d.flush(), ResizeEffect::Idle);
        // A failed, B still goes — and B is now the size to believe.
        assert_eq!(sending(&d.completed(restore, false)), Some(B));
        assert_eq!(d.observe(A), ResizeEffect::Arm);
    }

    /// A display that never sent its initial size has told the server nothing,
    /// so every observation is news until one lands.
    #[test]
    fn with_nothing_sent_the_first_observation_is_a_resize() {
        let mut d = ResizeDebounce::new(None);
        assert_eq!(d.observe(A), ResizeEffect::Arm);
        assert_eq!(sending(&d.flush()), Some(A));
        assert_eq!(d.observe(A), ResizeEffect::Idle);
    }
}
