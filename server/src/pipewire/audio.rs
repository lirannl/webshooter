use anyhow::{Result, anyhow};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use pipewire as pw;
use pipewire::proxy::ProxyT;
use shared::server_datagram::{AudioFormat, ServerDatagram};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One captured (already-encoded) audio chunk, ready to be wrapped in a
/// [`ServerDatagram::AudioFrame`] and forwarded to the client.
pub struct AudioPacket {
    pub channels: u8,
    pub rate: u32,
    pub format: AudioFormat,
    pub data: Vec<u8>,
}

/// Owns the application audio sink (a PipeWire connection kept alive on a
/// background thread) and the GStreamer capture/encode pipeline. Dropping it
/// tears everything down — the sink is created with `object.linger=false`, so
/// it is removed from the graph as soon as the owning connection closes.
pub struct AudioSink {
    cancel: CancellationToken,
    thread: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    pipeline: gst::Pipeline,
    /// Handle to the sink's PipeWire main loop, used to wake/quit it from
    /// `Drop` (which runs on a different thread than the one driving it).
    mainloop_ptr: MainLoopPtr,
}

/// Wrapper around the raw PipeWire main-loop pointer. The pointer is only ever
/// passed to the thread-safe `pw_main_loop_quit`, so it is safe to send between
/// threads (the raw pointer type itself is `!Send`).
struct MainLoopPtr(*mut pw::sys::pw_main_loop);
unsafe impl Send for MainLoopPtr {}

impl Drop for AudioSink {
    fn drop(&mut self) {
        // Stop the GStreamer pipeline and wake the sink's main loop so the
        // background thread returns and its PipeWire connection (and the
        // application-owned sink node) is torn down.
        let _ = self.pipeline.set_state(gst::State::Null);
        unsafe {
            pw::sys::pw_main_loop_quit(self.mainloop_ptr.0);
        }
        self.cancel.cancel();
        // Wait for the sink's background thread to actually return so its
        // PipeWire connection (and the `object.linger=false` sink node) is
        // removed from the graph.  Without this, the thread is merely detached
        // and the node can linger past the session that owned it.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Defaults applied only when the client reports no channel/rate caps.
const DEFAULT_CHANNELS: u8 = 2;
const DEFAULT_RATE: u32 = 48000;

/// Map a channel count to a PipeWire channel-map string. A proper named map
/// (e.g. `FL,FR`) gives a real stereo sink instead of `aux0/aux1`; 1 channel is
/// mono. Falls back to `auxN` for counts beyond the common layouts.
fn channel_map(channels: u8) -> String {
    match channels {
        1 => "MONO".into(),
        2 => "FL,FR".into(),
        3 => "FL,FR,FC".into(),
        4 => "FL,FR,RL,RR".into(),
        5 => "FL,FR,FC,RL,RR".into(),
        6 => "FL,FR,FC,LFE,RL,RR".into(),
        7 => "FL,FR,FC,LFE,SL,SR,BC".into(),
        8 => "FL,FR,FC,LFE,RL,RR,SL,SR".into(),
        n => (0..n).map(|i| format!("AUX{i}")).collect::<Vec<_>>().join(","),
    }
}

/// Create an application-owned PipeWire audio sink, then capture and Opus-
/// encode everything played into it. Returns the sink handle (for
/// lifetime/teardown) and a receiver of encoded audio chunks.
///
/// `name` uniquely identifies this sink (typically the session's base name,
/// e.g. `webshooter-alice-1`); it is used to derive the PipeWire node name and
/// the PulseAudio monitor source so each connected session owns a distinct sink.
pub async fn start_audio_sink(
    cancel: CancellationToken,
    name: String,
    channels: u8,
    rate: u32,
) -> Result<(AudioSink, mpsc::Receiver<AudioPacket>)> {
    // Clamp to sane bounds and apply defaults when the client reports nothing.
    let channels = if channels == 0 { DEFAULT_CHANNELS } else { channels.min(8) };
    let rate = if rate == 0 { DEFAULT_RATE } else { rate };

    // The sink gets its own child token. Tearing the sink down on drop must not
    // cancel the surrounding session — only this audio subtree.
    let audio_cancel = cancel.child_token();
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>(256);
    let (id_tx, id_rx) = tokio::sync::oneshot::channel::<(u32, MainLoopPtr)>();

    // The background thread is told to quit by watching the *parent* `cancel`
    // token (not the child one): if this call bails out on cancellation before
    // an `AudioSink` is ever constructed, there is no handle to wake the loop,
    // so the thread would otherwise leak its PipeWire connection (and the
    // `object.linger=false` sink node) forever.
    let cancel_thread = cancel.clone();
    let name_thread = name.clone();
    let channels_thread = channels;
    let thread = std::thread::spawn(move || {
        if let Err(e) = sink_thread(id_tx, cancel_thread, name_thread, channels_thread) {
            eprintln!("[audio] sink error: {e:#}");
        }
    });

    // Wait for the sink node to be created (or cancellation).
    let (_sink_id, mainloop_ptr) = tokio::select! {
        _ = cancel.cancelled() => {
            // Cancelled before the sink was ready.  The sink thread watches the
            // same parent token and will quit its loop (removing the node) on
            // its own; reap the thread on a helper so returning doesn't block
            // the tokio worker, and leave it a moment to wind down.
            std::thread::spawn(move || { let _ = thread.join(); });
            return Err(anyhow!("audio cancelled before sink was ready"));
        }
        id = id_rx => id.map_err(|_| anyhow!("audio sink thread terminated"))?,
    };

    gst::init()?;
    let monitor_source = format!("{name}-audio-sink.monitor");
    // Capture the monitored audio at the client's negotiated rate and channel
    // count, converting to S16LE for `opusenc` (Opus's native integer input
    // format). `audioconvert ! audioresample` normalise whatever the monitor
    // produces (typically F32LE, PulseAudio's native format) into that caps.
    let caps = format!("audio/x-raw,format=S16LE,rate={rate},channels={channels}");
    let pipeline = gst::parse::launch(&format!(
        "pulsesrc device={monitor_source} client-name={name} \
         ! audioconvert ! audioresample \
         ! capsfilter caps=\"{caps}\" \
         ! opusenc bitrate=128000 \
         ! appsink name=sink sync=false",
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("audio pipeline is not a pipeline"))?;

    let appsink = pipeline
        .by_name("sink")
        .ok_or(anyhow!("audio pipeline has no appsink"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("audio sink element is not an appsink"))?;

    let tx = audio_tx.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                if tx
                    .try_send(AudioPacket {
                        channels,
                        rate,
                        format: AudioFormat::Opus,
                        data: map.to_vec(),
                    })
                    .is_err()
                {
                    return Err(gst::FlowError::Error);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).map_err(|e| {
        let _ = pipeline.set_state(gst::State::Null);
        anyhow!("audio pipeline failed to start: {e}")
    })?;

    Ok((
        AudioSink {
            cancel: audio_cancel,
            thread: Some(thread),
            pipeline,
            mainloop_ptr,
        },
        audio_rx,
    ))
}

/// Background thread: create the application-owned sink on its own PipeWire
/// connection and keep that connection alive until cancelled. Sends the sink's
/// node id and main-loop handle back so `Drop` can wake the loop. The mixed
/// audio we stream is captured by the GStreamer pipeline from the sink's
/// PulseAudio `.monitor` source (`<name>-audio-sink.monitor`), which
/// PipeWire's PulseAudio emulation exposes for every sink.
fn sink_thread(
    id_tx: tokio::sync::oneshot::Sender<(u32, MainLoopPtr)>,
    cancel: CancellationToken,
    name: String,
    channels: u8,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let mainloop_ptr = mainloop.as_raw_ptr();

    // `object.linger=false` means the node is removed when this client
    // disconnects — i.e. it is owned by this application. The channel map (e.g.
    // `FL,FR` for stereo) gives a real sink instead of the default `aux0/aux1`,
    // and is matched to the client's reported channel count.
    let sink_name = format!("{name}-audio-sink");
    let sink_props = pw::properties::properties! {
        *pw::keys::FACTORY_NAME => "support.null-audio-sink",
        *pw::keys::NODE_NAME => sink_name,
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::AUDIO_CHANNELS => channel_map(channels),
        *pw::keys::OBJECT_LINGER => "false",
    };
    let _sink = core
        .create_object::<pw::node::Node>("adapter", &sink_props)
        .map_err(|e| anyhow!("failed to create audio sink: {e}"))?;
    let sink_id = _sink.upcast_ref().id();

    // Give PipeWire/PulseAudio a moment to register the sink's `.monitor`
    // source before the GStreamer capture pipeline tries to open it by name.
    std::thread::sleep(Duration::from_millis(400));
    let _ = id_tx.send((sink_id, MainLoopPtr(mainloop_ptr)));

    // Keep the connection alive until cancellation.
    let cancel_timer = cancel.clone();
    let quit_ptr = MainLoopPtr(mainloop_ptr);
    let _timer = mainloop.loop_().add_timer(move |_| {
        if cancel_timer.is_cancelled() {
            unsafe {
                pw::sys::pw_main_loop_quit(quit_ptr.0);
            }
        }
    });
    let _ = _timer.update_timer(Some(Duration::ZERO), Some(Duration::from_millis(200)));

    mainloop.run();

    // `_sink` and `core` are dropped here, removing the sink from the graph.
    Ok(())
}

/// Drain encoded audio chunks and forward them to the client as fragmented
/// [`ServerDatagram::AudioFrame`]s through the server→client message channel.
pub async fn forward_audio(
    mut rx: mpsc::Receiver<AudioPacket>,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
) {
    let mut frame_id: u16 = 0;
    let budget = shared::server_datagram::MAX_AUDIO_DATAGRAM_PAYLOAD;
    while let Some(pkt) = rx.recv().await {
        if pkt.data.is_empty() {
            continue;
        }
        let num_frags = pkt.data.len().div_ceil(budget).max(1) as u16;
        for (i, chunk) in pkt.data.chunks(budget).enumerate() {
            let dgram = ServerDatagram::AudioFrame {
                frame_id,
                frag_idx: i as u16,
                num_frags,
                channels: pkt.channels,
                rate: pkt.rate,
                format: pkt.format,
                payload: chunk.to_vec(),
            };
            if server_msg_tx.send(dgram).await.is_err() {
                return;
            }
        }
        frame_id = frame_id.wrapping_add(1);
    }
}
