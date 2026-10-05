use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::Float32Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use web_sys::{
    AnalyserNode, AudioBuffer, AudioContext, AudioData, AudioDataCopyToOptions, AudioDecoder,
    AudioDecoderConfig, AudioNode, AudioSampleFormat, EncodedAudioChunk, EncodedAudioChunkInit,
    EncodedAudioChunkType, GainNode, HtmlButtonElement, HtmlDivElement, HtmlSpanElement,
};

/// How far ahead (seconds) audio frames are scheduled relative to the running
/// play-head, to absorb jitter in datagram arrival.
const SCHEDULE_AHEAD: f64 = 0.05;

/// Per-player playback state shared with the decoder output callback.
struct PlaybackState {
    ctx: AudioContext,
    /// The analyser every decoded buffer is routed through on its way to the
    /// speakers. It is a pass-through tap: it forwards what it measures, so
    /// this costs nothing audible. It feeds the client-side visualiser — the
    /// server sends the same Opus frames whether or not it is here.
    analyser: AnalyserNode,
    /// Optional gain node for volume control. Created lazily when set_volume
    /// is first called, inserted between analyser and destination.
    gain: std::cell::OnceCell<GainNode>,
    /// Client-side, server-oblivious signs that the player is alive: written
    /// by the playback paths, read by the status badge.
    activity: Rc<RefCell<AudioActivity>>,
    next_time: f64,
    /// Set while we've told the server the AudioContext is `Running`, so it can
    /// create the PipeWire sink and start forwarding Opus. Cleared if the
    /// context ever suspends again, so each return to Running re-notifies the
    /// server rather than being a permanent once-only latch.
    audio_ready: Cell<bool>,
    /// End time (AudioContext clock) of the last buffer we scheduled, used to
    /// detect gaps/overlaps in the playback timeline (a cause of choppiness).
    last_end: Cell<f64>,
}

// ---------------------------------------------------------------------------
// Client-side audio status badge + activity feed
// ---------------------------------------------------------------------------

/// What the client believes about the audio player right now. Purely
/// client-side: derived from the AudioContext state and recent play-out, and
/// the server both plays no part in it and is never told about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioPhase {
    /// No player exists in this browser (no AudioContext / Opus AudioDecoder).
    Unavailable,
    /// The context exists but is suspended — typically waiting for the first
    /// user gesture before the page is allowed to play audio at all.
    Suspended,
    /// The context is running but no decoded audio has reached play-out
    /// recently.
    Waiting,
    /// The context is running and frames are being played right now.
    Live,
}

/// The client-side facts the status badge is derived from. Shared between the
/// audio player (writer) and the badge (reader) through one `Rc`, so the two
/// always agree.
#[derive(Default)]
pub(crate) struct AudioActivity {
    /// Whether the AudioContext last reported `Running`.
    running: Cell<bool>,
    /// `performance.now()` of the most recent played frame; `0.0` if none yet.
    last_frame_at: Cell<f64>,
}

/// How often the badge re-derives its state from the activity feed.
const BADGE_REPAINT_MS: u32 = 250;
/// A player counts as "live" while frames have reached play-out within this
/// window.
const BADGE_ACTIVE_WINDOW_MS: f64 = 3000.0;

/// The state shared between the element handle and its periodic repainter.
pub struct BadgeShared {
    feed: Option<Rc<RefCell<AudioActivity>>>,
    pub phase: AudioPhase,
    dot: HtmlDivElement,
    label: HtmlSpanElement,
}

/// What phase the feed currently implies.
fn derive_phase(shared: &Rc<RefCell<BadgeShared>>) -> AudioPhase {
    match &shared.borrow().feed {
        None => AudioPhase::Unavailable,
        Some(feed) => {
            let feed = feed.borrow();
            if !feed.running.get() {
                AudioPhase::Suspended
            } else if now_ms() - feed.last_frame_at.get() <= BADGE_ACTIVE_WINDOW_MS {
                AudioPhase::Live
            } else {
                AudioPhase::Waiting
            }
        }
    }
}

/// Write `phase` to the DOM and record it as the badge's current phase.
pub fn paint_badge(shared: &Rc<RefCell<BadgeShared>>, phase: AudioPhase) {
    let mut s = shared.borrow_mut();
    s.phase = phase;
    let (color, text) = match phase {
        AudioPhase::Unavailable => ("#e5484d", "audio unavailable"),
        AudioPhase::Suspended => ("#9e9e9e", "audio off — click to enable"),
        AudioPhase::Waiting => ("#e5a13a", "audio: waiting for stream"),
        AudioPhase::Live => ("#3fb950", "audio: live"),
    };
    s.dot.style().set_property("background", color).ok();
    s.label.set_text_content(Some(text));
}

/// Repaint the badge from the shared state, skipping the DOM when the phase
/// has not changed.
fn repaint_badge(shared: &Rc<RefCell<BadgeShared>>) {
    let phase = derive_phase(shared);
    if shared.borrow().phase == phase {
        return;
    }
    paint_badge(shared, phase);
}

/// An inline status element that tells this user, entirely on the client
/// side, whether this session's audio player is doing anything. It is created
/// only on the /audio page (a video session keeps the screen for the remote
/// desktop). The server is not involved and cannot influence it. The element
/// lives in the DOM and its repaint timer in a forgotten closure, so the
/// builders below hand back only the shared state a sibling element needs —
/// there is no handle to keep alive.

/// An inline status element fed by a live player's activity, repainted as that
/// activity changes, appended to `parent`. Returns the shared state so a
/// sibling element can drive it.
pub fn status_element_with_activity(
    parent: &HtmlDivElement,
    activity: Rc<RefCell<AudioActivity>>,
) -> Rc<RefCell<BadgeShared>> {
    let (dot, label) = build_status_element(parent);
    let shared = Rc::new(RefCell::new(BadgeShared {
        feed: Some(activity),
        phase: AudioPhase::Suspended,
        dot,
        label,
    }));
    start_status_repaint(shared.clone());
    shared
}

/// An inline status element for a session whose player could not be created.
/// It never changes once painted. Appended to `parent`.
pub fn status_element_unavailable(parent: &HtmlDivElement) {
    let (dot, label) = build_status_element(parent);
    let shared = Rc::new(RefCell::new(BadgeShared {
        feed: None,
        phase: AudioPhase::Unavailable,
        dot,
        label,
    }));
    start_status_repaint(shared);
}

/// Build the dot + label row and append it to `parent`.
fn build_status_element(parent: &HtmlDivElement) -> (HtmlDivElement, HtmlSpanElement) {
    let document = web_sys::window().unwrap().document().unwrap();
    let container = document
        .create_element("div")
        .unwrap()
        .dyn_into::<HtmlDivElement>()
        .unwrap();
    container.style().set_css_text(
        "display:flex;align-items:center;gap:8px;width:100%;box-sizing:border-box;\
         padding:10px 12px;border-radius:8px;background:#1a1a1a;border:1px solid #333;\
         color:rgba(255,255,255,.9);font:14px/1.4 Inter,system-ui,sans-serif;user-select:none;",
    );
    let dot = document
        .create_element("div")
        .unwrap()
        .dyn_into::<HtmlDivElement>()
        .unwrap();
    dot.style()
        .set_css_text("width:10px;height:10px;border-radius:50%;flex:none;");
    let label = document
        .create_element("span")
        .unwrap()
        .dyn_into::<HtmlSpanElement>()
        .unwrap();
    label.style().set_css_text("white-space:nowrap;");
    container.append_child(&dot).unwrap();
    container.append_child(&label).unwrap();
    parent.append_child(&container).unwrap();
    (dot, label)
}

/// Paint the element's first state, then repaint it on a timer for as long as
/// the page lives.
fn start_status_repaint(shared: Rc<RefCell<BadgeShared>>) {
    // Paint the initial state unconditionally: the very first frame must
    // not skip because the sentinel phase happens to equal the derived one.
    paint_badge(&shared, derive_phase(&shared));
    let tick = Closure::wrap(Box::new(move || repaint_badge(&shared)) as Box<dyn FnMut()>);
    if let Some(win) = web_sys::window() {
        let _ = win.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            BADGE_REPAINT_MS as i32,
        );
    }
    tick.forget();
}

/// Append a "Start Audio" button to `parent`. Clicking it resumes `ctx` and
/// hides the button.
///
/// `status` is the sibling status element; a click moves it to "waiting" so the
/// status agrees with the request immediately rather than sitting on "audio
/// off" until the context's `statechange` lands. The click itself is what
/// notifies the server: it is `resume` firing `statechange`, which is what runs
/// [`handle_audio_state`] — only the client knows when a gesture happened, so
/// only the client can originate `AudioReady`.
pub fn audio_start_button(
    parent: &HtmlDivElement,
    ctx: &web_sys::AudioContext,
    status: Rc<RefCell<BadgeShared>>,
) {
    {
        let document = web_sys::window().unwrap().document().unwrap();
        // A `<button>` is an `HtmlButtonElement`, and `dyn_into` on a different
        // concrete element type fails: casting it to `HtmlDivElement` traps and
        // takes the whole render loop with it.
        let button = document
            .create_element("button")
            .unwrap()
            .dyn_into::<HtmlButtonElement>()
            .unwrap();
        button.style().set_css_text(
            "width:100%;box-sizing:border-box;padding:12px 24px;border-radius:8px;\
             border:1px solid #646cff;background:#1a1a1a;color:#646cff;\
             font:16px/1.4 Inter,system-ui,sans-serif;cursor:pointer;\
             transition:background 0.2s,border-color 0.2s;",
        );
        button.set_text_content(Some("Start Audio"));
        button.set_attribute("type", "button").ok();

        let ctx_clone = ctx.clone();
        let button_clone = button.clone();
        let cb = Closure::wrap(Box::new(move || {
            let _ = ctx_clone.resume();
            let _ = button_clone.style().set_property("display", "none");
            if status.borrow().phase == AudioPhase::Suspended {
                paint_badge(&status, AudioPhase::Waiting);
            }
        }) as Box<dyn FnMut()>);
        let _ = button.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        cb.forget();

        parent.append_child(&button).unwrap();
    }
}

/// Monotonic milliseconds since navigation start, used for the client-side
/// "is audio being played right now" window.
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

/// Deliberately not `Clone`. Two clones would share the context but each hold
/// its own `configured` / `next_sample` cells, so the two halves would disagree
/// about what has been decoded — a silent, hard-to-trace fault. Share one player
/// through an `Rc` instead.
pub struct AudioPlayer {
    decoder: AudioDecoder,
    state: Rc<RefCell<PlaybackState>>,
    configured: Cell<bool>,
    /// Running count of decoded samples, used to derive strictly increasing,
    /// accurate chunk timestamps (one Opus packet may hold several 20 ms
    /// frames, so `frame_id * 20_000` would be wrong).
    next_sample: Cell<u64>,
    /// Throttle for diagnostic logging.
    dbg_count: Cell<u32>,
}

impl AudioPlayer {
    pub fn new() -> Option<AudioPlayer> {
        let ctx = web_sys::AudioContext::new().ok()?;
        log::debug!(
            "audio: AudioContext created, state={:?} sampleRate={}",
            ctx.state(),
            ctx.sample_rate()
        );

        // Route every decoded buffer through one analyser on its way to the
        // speakers. The AnalyserNode is a pass-through in the Web Audio graph —
        // it forwards what it measures, so this costs nothing audible — and it
        // is the tap the client-side visualiser draws from. The server neither
        // needs to know about it nor is told about it.
        let analyser = ctx.create_analyser().ok()?;
        analyser.set_fft_size(1024);
        analyser.set_smoothing_time_constant(0.8);
        let destination = ctx.destination();
        let _ = analyser.connect_with_audio_node(&destination);

        // Client-side activity feed shared with every playback path below. The
        // /audio-page status badge is attached by the render loop, not here —
        // a video session has no badge.
        let activity = Rc::new(RefCell::new(AudioActivity::default()));

        let state = Rc::new(RefCell::new(PlaybackState {
            ctx: ctx.clone(),
            analyser: analyser.clone(),
            gain: std::cell::OnceCell::new(),
            activity,
            next_time: 0.0,
            audio_ready: Cell::new(false),
            last_end: Cell::new(0.0),
        }));

        let st = state.clone();
        let output_cb = Closure::wrap(Box::new(move |data: AudioData| {
            play_audio_data(&st, data);
        }) as Box<dyn FnMut(AudioData)>);
        let error_cb = Closure::wrap(Box::new(move |err: JsValue| {
            web_sys::console::error_1(&format!("AudioDecoder error: {:?}", err).into());
            log::error!("audio: AudioDecoder error: {err:?}");
        }) as Box<dyn FnMut(JsValue)>);

        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref().unchecked_ref()).ok();
        js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref().unchecked_ref()).ok();
        let decoder =
            web_sys::AudioDecoder::new(init.unchecked_ref::<web_sys::AudioDecoderInit>()).ok()?;

        // The JS-side callbacks must outlive the decoder; never drop them.
        output_cb.forget();
        error_cb.forget();

        // Browsers start the AudioContext suspended until user interaction (or
        // until playback is permitted by the environment). We deliberately do
        // NOT detect why the context is allowed to run (PWA exemption, a kiosk
        // launched with `--autoplay-policy=no-user-gesture-required`, a
        // permissive webview, or a user gesture) — reason-agnostic by design.
        // The only thing that matters is whether it is Running, because that is
        // what tells the server to create the PipeWire sink and stream Opus.
        //
        // `statechange` is the central hook: it fires on every transition, so we
        // auto-resume on pauses (blur/minimise — a permissive environment
        // honours it immediately, a restrictive web page keeps it pending until
        // a gesture) and re-notify the server on every return to Running (not
        // strictly once-only). If the context is Running from the outset we
        // notify immediately as well.
        fn handle_audio_state(state: &Rc<RefCell<PlaybackState>>) {
            let st = state.borrow();
            // Keep the client-side status badge in lockstep with the context.
            match st.ctx.state() {
                web_sys::AudioContextState::Running => {
                    st.activity.borrow().running.set(true);
                    if !st.audio_ready.get() {
                        let channels = st.ctx.destination().channel_count() as u8;
                        let rate = st.ctx.sample_rate() as u32;
                        st.audio_ready.set(true);
                        crate::send_datagram(shared::client_datagram::ClientDatagram::AudioReady {
                            channels,
                            rate,
                        });
                        log::info!(
                            "audio: AudioContext Running — sent AudioReady (channels={channels}, rate={rate}) to server"
                        );
                    }
                }
                web_sys::AudioContextState::Suspended => {
                    st.activity.borrow().running.set(false);
                    // Auto-resume on any suspension (e.g. tab blur). Permissive
                    // environments resume immediately; restrictive web pages
                    // keep the promise pending until the first gesture, which
                    // is expected. Clear the latch so a later Running notifies
                    // the server again.
                    st.audio_ready.set(false);
                    let _ = st.ctx.resume();
                }
                _ => {}
            }
        }

        // Drive `handle_audio_state` off `statechange`, which is the only path
        // that can transition the context: the "Start Audio" button calls
        // `resume()` from an explicit user gesture, and that fires the event.
        // Deliberately no up-front `resume()` here — an audio-only session must
        // not begin capturing until this user asks for it, and a context created
        // outside a gesture is left suspended.
        let ready_state = state.clone();
        let ready_cb =
            Closure::wrap(Box::new(move || handle_audio_state(&ready_state)) as Box<dyn FnMut()>);
        ctx.set_onstatechange(Some(ready_cb.as_ref().unchecked_ref()));
        ready_cb.forget();

        // An environment that hands out an already-running context needs no
        // resume to get there, so report that honestly rather than pretending
        // the session is still waiting.
        if ctx.state() == web_sys::AudioContextState::Running {
            handle_audio_state(&state);
        }

        log::info!("audio: AudioPlayer created");
        Some(AudioPlayer {
            decoder,
            state,
            configured: Cell::new(false),
            next_sample: Cell::new(0),
            dbg_count: Cell::new(0),
        })
    }

    /// The analyser node this player routes all playback through, for the
    /// client-side visualiser. A player that exists always has one: it is
    /// created in [`AudioPlayer::new`], and its absence there means no player.
    pub fn analyser(&self) -> AnalyserNode {
        self.state.borrow().analyser.clone()
    }

    /// The client-side activity feed the status badge (on the /audio page)
    /// derives its state from.
    pub fn activity(&self) -> Rc<RefCell<AudioActivity>> {
        self.state.borrow().activity.clone()
    }

    /// Get the AudioContext for manual resume from an explicit user gesture.
    pub fn audio_context(&self) -> web_sys::AudioContext {
        self.state.borrow().ctx.clone()
    }

    /// Set the output volume (0-255, representing 0-100%).
    /// Uses the Web Audio API gain node to adjust the volume.
    pub fn set_volume(&self, level: u8) {
        let state = self.state.borrow();
        let gain = state.gain.get_or_init(|| {
            let gain = state.ctx.create_gain().unwrap();
            gain.gain().set_value(1.0);
            // Insert gain node between analyser and destination
            let _ = state.analyser.disconnect();
            let _ = state
                .analyser
                .connect_with_audio_node(gain.unchecked_ref::<AudioNode>());
            let _ = gain.connect_with_audio_node(&state.ctx.destination());
            gain
        });
        let volume = (level as f32 / 255.0).clamp(0.0, 1.0);
        gain.gain().set_value(volume);
    }

    /// Point the decoder at Opus, once, from the shape the context already
    /// declared to the server in `AudioReady` — the same channel count and rate
    /// the sink was created with, read from the same place the notice was built
    /// from. An `AudioDecoder` holds one configuration for its lifetime, so this
    /// is not something to renegotiate per packet.
    fn configure_from_context(&self) {
        let ctx = &self.state.borrow().ctx;
        let channels = ctx.destination().channel_count() as u8;
        let rate = ctx.sample_rate() as u32;
        let head = opus_identification_header(channels, rate);
        let mut config = AudioDecoderConfig::new("opus", channels as u32, rate);
        let arr = js_sys::Uint8Array::from(&head[..]);
        config.description(&arr);
        self.decoder.configure(&config);
    }

    /// Feed one captured Opus packet to the decoder.
    ///
    /// A packet is complete in itself — it arrives whole, on a MoQ group stream
    /// that reassembles nothing — so the fragment table and the reassembler that
    /// used to sit in front of this are gone, and each packet is keyed to the
    /// play-head by its own length rather than by a frame id.
    pub fn push(&self, payload: Vec<u8>) {
        if !self.configured.get() {
            self.configure_from_context();
            self.configured.set(true);
        }

        let ctx = &self.state.borrow().ctx;
        let rate = ctx.sample_rate();

        // One Opus packet can contain several 20 ms frames, so derive the
        // chunk timestamp from the *actual* number of samples carried by this
        // packet (parsed from its TOC header) rather than assuming 20 ms.
        let samples = opus_packet_samples(&payload, rate as u32) as u64;
        let ts = self.next_sample.get() * 1_000_000 / rate as u64;
        self.next_sample.set(self.next_sample.get() + samples);

        let count = self.dbg_count.get();
        self.dbg_count.set(count + 1);

        let arr = js_sys::Uint8Array::from(&payload[..]);
        let init = EncodedAudioChunkInit::new(
            arr.unchecked_ref::<js_sys::Object>(),
            ts as f64,
            EncodedAudioChunkType::Key,
        );
        match EncodedAudioChunk::new(&init) {
            Ok(chunk) => self.decoder.decode(&chunk),
            Err(e) => log::error!("audio: EncodedAudioChunk::new failed: {e:?}"),
        }
    }
}

fn play_audio_data(state: &Rc<RefCell<PlaybackState>>, data: AudioData) {
    static ENTERED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let e = ENTERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if e < 3 {
        log::debug!("audio: output callback fired (#{e})");
    }
    let channels = data.number_of_channels();
    let frames = data.number_of_frames();
    let sample_rate = data.sample_rate();
    if channels == 0 || frames == 0 {
        log::debug!("audio: empty AudioData dropped (channels={channels} frames={frames})");
        data.close();
        return;
    }

    // The AudioContext is suspended until a user gesture. Scheduling into a
    // suspended context freezes our playhead far ahead of the (stopped) clock,
    // which makes audio silent / hugely delayed once it resumes. Drop the
    // buffer instead; we re-anchor to the real clock on the first Running frame.
    if state.borrow().ctx.state() == web_sys::AudioContextState::Suspended {
        data.close();
        return;
    }

    let ctx = state.borrow().ctx.clone();
    let buffer: AudioBuffer = match ctx.create_buffer(channels, frames, sample_rate) {
        Ok(b) => b,
        Err(e) => {
            log::error!("audio: create_buffer failed: {e:?}");
            data.close();
            return;
        }
    };

    for ch in 0..channels {
        let f32arr = Float32Array::new_with_length(frames);
        let mut opts = AudioDataCopyToOptions::new(ch);
        opts.format(AudioSampleFormat::F32Planar);
        opts.frame_count(frames);
        data.copy_to_with_buffer_source(f32arr.unchecked_ref::<js_sys::Object>(), &opts);
        let vec = f32arr.to_vec();
        let peak = vec.iter().fold(0.0_f32, |m, &s| m.max(s.abs()));
        if ch == 0 && frames > 0 {
            // First buffer only of each PCM stream: confirms audio is arriving
            // at sane levels without logging every buffer.
            static PEAK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = PEAK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 3 {
                log::debug!("audio: first pcm buffer peak={peak:.4} frames={frames} rate={sample_rate}");
            }
        }
        let _ = buffer.copy_to_channel(&vec, ch as i32);
    }
    data.close();

    // Feed the client-side badge's notion of "audio is actually playing".
    state.borrow().activity.borrow().last_frame_at.set(now_ms());

    let duration = frames as f64 / sample_rate as f64;
    let when = {
        let mut st = state.borrow_mut();
        let now = st.ctx.current_time();
        // Keep a continuous playhead. If we've fallen behind real time (a stall
        // or lost frames), jump it forward to `now` so we don't schedule into
        // the past. Crucially, do NOT pull it backward when we're *ahead* of
        // `now` — doing so overlaps already-scheduled buffers and makes the
        // audio choppy/garbled during the bursts that happen when datagrams
        // arrive coalesced.
        if st.next_time < now {
            st.next_time = now;
        } else if st.next_time > now + 1.0 {
            // Pathological excess buffering: resync instead of growing forever.
            st.next_time = now + 0.1;
        }
        let w = st.next_time + SCHEDULE_AHEAD;
        st.next_time += duration;
        w
    };

    if frames > 0 {
        // One-time-ish diagnostic: report the first few scheduled buffers.
        static FIRST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = FIRST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 3 {
            log::debug!(
                "audio: play frames={frames} rate={sample_rate} dur={duration:.3}s when={when:.3}s now={:.3} ctx={:?}",
                ctx.current_time(),
                ctx.state()
            );
        }
        if ctx.state() == web_sys::AudioContextState::Suspended {
            static WARN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if WARN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
                log::warn!(
                    "audio: scheduling while AudioContext is SUSPENDED — no sound until resumed by a gesture",
                );
            }
        }
    }

    if let Ok(src) = ctx.create_buffer_source() {
        src.set_buffer(Some(&buffer));
        // Into the analyser, which passes the signal through to the speakers —
        // it is simultaneously the tap the visualiser draws from.
        let analyser = state.borrow().analyser.clone();
        let _ = src.connect_with_audio_node(analyser.unchecked_ref::<AudioNode>());
        let _ = src.start_with_when(when);
    } else {
        log::error!("audio: create_buffer_source failed");
    }

    // Report timeline discontinuities (gaps/overlaps) that cause choppiness.
    {
        let st = state.borrow_mut();
        let prev_end = st.last_end.get();
        st.last_end.set(when + duration);
        if prev_end > 0.0 {
            let delta = when - prev_end; // >0 gap, <0 overlap
            let anomaly = if delta > duration * 1.5 {
                Some(format!("GAP {:.1}ms", (delta - duration) * 1000.0))
            } else if delta < -0.001 {
                Some(format!("OVERLAP {:.1}ms", (-delta) * 1000.0))
            } else {
                None
            };
            if let Some(a) = anomaly {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Throttle: log every anomaly but cap the burst.
                if n < 200 {
                    log::debug!(
                        "audio: timeline {a} (when={when:.3} prev_end={prev_end:.3} dur={duration:.3})"
                    );
                }
            }
        }
    }
}

/// Build an Opus identification header (RFC 7845) used as the decoder
/// description for `codec = "opus"`.
fn opus_identification_header(channels: u8, rate: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(23);
    v.extend_from_slice(b"OpusHead");
    v.push(1); // version
    v.push(channels);
    v.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    v.extend_from_slice(&rate.to_le_bytes()); // input sample rate
    v.extend_from_slice(&0u16.to_le_bytes()); // output gain
    if channels == 2 {
        v.push(1); // channel mapping family (mapping present)
        v.push(1); // stream count
        v.push(1); // coupled stream count
        v.push(0); // channel 0 -> stream 0
        v.push(1); // channel 1 -> stream 0 (coupled)
    } else {
        v.push(0); // channel mapping family 0 (mono / no mapping table)
    }
    v
}

/// Number of samples per Opus frame for the given TOC byte and sample rate.
/// Ported from libopus `opus_packet_get_samples_per_frame`.
fn opus_samples_per_frame(toc: u8, fs: u32) -> u32 {
    if toc & 0x80 != 0 {
        let n = ((toc >> 3) & 0x3) as u32;
        (fs << n) / 400
    } else if (toc & 0x60) == 0x60 {
        if toc & 0x08 != 0 { fs / 50 } else { fs / 100 }
    } else {
        let n = ((toc >> 3) & 0x3) as u32;
        if n == 3 {
            fs * 60 / 1000
        } else {
            (fs << n) / 100
        }
    }
}

/// Number of frames carried in an Opus packet. Ported from libopus
/// `opus_packet_get_nb_frames`.
fn opus_nb_frames(packet: &[u8]) -> Option<u32> {
    if packet.is_empty() {
        return None;
    }
    let mode = (packet[0] & 0x3) as u32;
    let count = if mode == 0 {
        1
    } else if mode != 3 {
        2
    } else {
        if packet.len() < 2 {
            return None;
        }
        (packet[1] & 0x3F) as u32
    };
    Some(count)
}

/// Total number of samples (per channel) in an Opus packet, i.e. libopus
/// `opus_packet_get_nb_samples`. Returns 0 for an invalid/empty packet.
fn opus_packet_samples(packet: &[u8], fs: u32) -> u32 {
    match opus_nb_frames(packet) {
        Some(frames) => frames * opus_samples_per_frame(packet[0], fs),
        None => 0,
    }
}
