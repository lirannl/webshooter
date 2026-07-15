use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use js_sys::Float32Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::log::log;
use web_sys::{
    AudioBuffer, AudioContext, AudioData, AudioDataCopyToOptions, AudioDecoder, AudioDecoderConfig,
    AudioSampleFormat, EncodedAudioChunk, EncodedAudioChunkInit, EncodedAudioChunkType,
};

/// How far ahead (seconds) audio frames are scheduled relative to the running
/// play-head, to absorb jitter in datagram arrival.
const SCHEDULE_AHEAD: f64 = 0.05;

/// One pending (possibly fragmented) audio frame awaiting reassembly.
struct PendingAudio {
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
}

/// Per-player playback state shared with the decoder output callback.
struct PlaybackState {
    ctx: AudioContext,
    next_time: f64,
    /// Set once we've told the server the AudioContext is `Running`, so it can
    /// create the PipeWire sink and start forwarding Opus. Sent only once.
    audio_ready: Cell<bool>,
    /// Highest sample magnitude seen since the last periodic report.
    peak_max: Cell<f32>,
    /// Number of decoded frames since the last periodic report.
    frame_count: Cell<u32>,
    /// End time (AudioContext clock) of the last buffer we scheduled, used to
    /// detect gaps/overlaps in the playback timeline (a cause of choppiness).
    last_end: Cell<f64>,
}

pub struct AudioPlayer {
    decoder: AudioDecoder,
    pending: RefCell<HashMap<u16, PendingAudio>>,
    #[allow(dead_code)]
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
        log(format!(
            "audio: AudioContext created, state={:?} sampleRate={}",
            ctx.state(),
            ctx.sample_rate()
        ));

        let state = Rc::new(RefCell::new(PlaybackState {
            ctx: ctx.clone(),
            next_time: 0.0,
            audio_ready: Cell::new(false),
            peak_max: Cell::new(0.0),
            frame_count: Cell::new(0),
            last_end: Cell::new(0.0),
        }));

        // Periodically report received audio levels so we can tell whether the
        // server is actually streaming real audio, independent of any silence
        // at capture start (the host player doesn't autoplay and the user
        // can't start it the instant the sink appears).
        {
            let report_state = state.clone();
            let report_cb = Closure::wrap(Box::new(move || {
                let (peak, frames, ctx_state) = {
                    let st = report_state.borrow();
                    (st.peak_max.get(), st.frame_count.get(), st.ctx.state())
                };
                log(format!(
                    "audio: levels report peak={peak:.4} frames_in_window={frames} ctx={ctx_state:?}"
                ));
                report_state.borrow().peak_max.set(0.0);
                report_state.borrow().frame_count.set(0);
            }) as Box<dyn FnMut()>);
            if let Some(win) = web_sys::window() {
                let _ = win.set_interval_with_callback_and_timeout_and_arguments_0(
                    report_cb.as_ref().unchecked_ref(),
                    10_000,
                );
            }
            report_cb.forget();
        }

        let st = state.clone();
        let output_cb = Closure::wrap(Box::new(move |data: AudioData| {
            play_audio_data(&st, data);
        }) as Box<dyn FnMut(AudioData)>);
        let error_cb = Closure::wrap(Box::new(move |err: JsValue| {
            web_sys::console::error_1(&format!("AudioDecoder error: {:?}", err).into());
            log(format!("audio: AudioDecoder error: {err:?}"));
        }) as Box<dyn FnMut(JsValue)>);

        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref().unchecked_ref()).ok();
        js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref().unchecked_ref()).ok();
        let decoder =
            web_sys::AudioDecoder::new(init.unchecked_ref::<web_sys::AudioDecoderInit>()).ok()?;

        // The JS-side callbacks must outlive the decoder; never drop them.
        output_cb.forget();
        error_cb.forget();

        // Browsers start the AudioContext suspended until a user gesture.
        let resume_ctx = ctx.clone();
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            let resume_cb = Closure::wrap(Box::new(move || {
                let _ = resume_ctx.resume();
            }) as Box<dyn FnMut()>);
            for ev in ["pointerdown", "keydown", "click", "touchstart"] {
                let _ =
                    doc.add_event_listener_with_callback(ev, resume_cb.as_ref().unchecked_ref());
            }
            resume_cb.forget();
        }

        // Tell the server (exactly once) as soon as the AudioContext is
        // actually `Running`. This is what lets the server create the PipeWire
        // sink and start streaming Opus — we must NOT wait for the first
        // decoded frame, because the server won't send any until it has heard
        // from us (chicken-and-egg). `statechange` fires when `resume()`
        // completes the transition from Suspended -> Running.
        let ready_state = state.clone();
        let ready_cb = Closure::wrap(Box::new(move || {
            if ready_state.borrow().ctx.state() == web_sys::AudioContextState::Running
                && !ready_state.borrow().audio_ready.get()
            {
                ready_state.borrow().audio_ready.set(true);
                crate::send_datagram(shared::client_datagram::ClientDatagram::AudioReady);
                log("audio: AudioContext Running — sent AudioReady to server");
            }
        }) as Box<dyn FnMut()>);
        ctx.set_onstatechange(Some(ready_cb.as_ref().unchecked_ref()));
        ready_cb.forget();

        // In case it is already running (e.g. autoplay allowed).
        if ctx.state() == web_sys::AudioContextState::Running && !state.borrow().audio_ready.get() {
            state.borrow().audio_ready.set(true);
            crate::send_datagram(shared::client_datagram::ClientDatagram::AudioReady);
            log("audio: AudioContext Running — sent AudioReady to server");
        }

        log("audio: AudioPlayer created");
        Some(AudioPlayer {
            decoder,
            pending: RefCell::new(HashMap::new()),
            state,
            configured: Cell::new(false),
            next_sample: Cell::new(0),
            dbg_count: Cell::new(0),
        })
    }

    fn configure(&self, channels: u8, rate: u32) {
        let head = opus_identification_header(channels, rate);
        let mut config = AudioDecoderConfig::new("opus", channels as u32, rate);
        let arr = js_sys::Uint8Array::from(&head[..]);
        config.description(&arr);
        self.decoder.configure(&config);
    }

    pub fn push(
        &self,
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        channels: u8,
        rate: u32,
        payload: Vec<u8>,
    ) {
        if !self.configured.get() {
            self.configure(channels, rate);
            self.configured.set(true);
        }

        let mut map = self.pending.borrow_mut();
        let entry = map.entry(frame_id).or_insert_with(|| PendingAudio {
            fragments: vec![None; num_frags as usize],
            received: 0,
        });

        let fi = frag_idx as usize;
        if fi >= entry.fragments.len() || entry.fragments[fi].is_some() {
            return;
        }
        entry.fragments[fi] = Some(payload);
        entry.received += 1;

        if entry.received < entry.fragments.len() {
            return;
        }

        let mut assembled = Vec::with_capacity(entry.fragments.len() * 256);
        for frag in entry.fragments.drain(..) {
            if let Some(d) = frag {
                assembled.extend_from_slice(&d);
            }
        }
        map.remove(&frame_id);
        drop(map);

        // One Opus packet can contain several 20 ms frames, so derive the
        // chunk timestamp from the *actual* number of samples carried by this
        // packet (parsed from its TOC header) rather than assuming 20 ms.
        let rate = rate as u64;
        let samples = opus_packet_samples(&assembled, rate as u32) as u64;
        let ts = self.next_sample.get() * 1_000_000 / rate;
        self.next_sample.set(self.next_sample.get() + samples);

        let count = self.dbg_count.get();
        self.dbg_count.set(count + 1);

        let arr = js_sys::Uint8Array::from(&assembled[..]);
        let init = EncodedAudioChunkInit::new(
            arr.unchecked_ref::<js_sys::Object>(),
            ts as f64,
            EncodedAudioChunkType::Key,
        );
        match EncodedAudioChunk::new(&init) {
            Ok(chunk) => self.decoder.decode(&chunk),
            Err(e) => log(format!("audio: EncodedAudioChunk::new failed: {e:?}")),
        }
    }
}

fn play_audio_data(state: &Rc<RefCell<PlaybackState>>, data: AudioData) {
    static ENTERED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let e = ENTERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if e < 3 {
        log(format!("audio: output callback fired (#{e})"));
    }
    let channels = data.number_of_channels();
    let frames = data.number_of_frames();
    let sample_rate = data.sample_rate();
    if channels == 0 || frames == 0 {
        log(format!(
            "audio: empty AudioData dropped (channels={channels} frames={frames})"
        ));
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
            log(format!("audio: create_buffer failed: {e:?}"));
            data.close();
            return;
        }
    };

    let mut frame_peak = 0.0_f32;
    for ch in 0..channels {
        let f32arr = Float32Array::new_with_length(frames);
        let mut opts = AudioDataCopyToOptions::new(ch);
        opts.format(AudioSampleFormat::F32Planar);
        opts.frame_count(frames);
        data.copy_to_with_buffer_source(f32arr.unchecked_ref::<js_sys::Object>(), &opts);
        let vec = f32arr.to_vec();
        let peak = vec.iter().fold(0.0_f32, |m, &s| m.max(s.abs()));
        frame_peak = frame_peak.max(peak);
        if ch == 0 && frames > 0 {
            static PEAK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = PEAK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 3 {
                log(format!(
                    "audio: pcm ch0 peak={peak:.4} frames={frames} rate={sample_rate}"
                ));
            }
        }
        let _ = buffer.copy_to_channel(&vec, ch as i32);
    }
    data.close();

    // Feed the periodic levels reporter.
    {
        let st = state.borrow_mut();
        if frame_peak > st.peak_max.get() {
            st.peak_max.set(frame_peak);
        }
        st.frame_count.set(st.frame_count.get() + 1);
    }

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
            log(format!(
                "audio: play frames={frames} rate={sample_rate} dur={duration:.3}s when={when:.3}s now={:.3} ctx={:?}",
                ctx.current_time(),
                ctx.state()
            ));
        }
        if ctx.state() == web_sys::AudioContextState::Suspended {
            static WARN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if WARN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
                log(
                    "audio: WARNING scheduling while AudioContext is SUSPENDED — no sound until resumed by a gesture",
                );
            }
        }
    }

    if let Ok(src) = ctx.create_buffer_source() {
        src.set_buffer(Some(&buffer));
        let _ = src.connect_with_audio_node(&ctx.destination());
        let _ = src.start_with_when(when);
    } else {
        log("audio: create_buffer_source failed");
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
                    log(format!(
                        "audio: timeline {a} (when={when:.3} prev_end={prev_end:.3} dur={duration:.3})"
                    ));
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
