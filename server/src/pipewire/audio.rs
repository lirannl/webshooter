use anyhow::{Result, anyhow};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use pipewire as pw;
use pipewire::proxy::ProxyT;
use pw::spa::utils::ChoiceEnum;
use std::ptr::NonNull;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One captured (already-encoded) audio chunk, ready to become a MoQ frame.
///
/// There is nothing on this but the bytes, and that is the point: the capture
/// encodes Opus unconditionally, an Opus packet declares its own channel count in
/// its header, and Opus always decodes at 48 kHz. So the negotiated channel count
/// and rate need not travel with each packet — a subscriber learns everything
/// there is to know from the track name (`audio/opus`) and the bytes themselves.
pub struct AudioPacket {
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
}

/// Covers the interval before an AudioSink exists. In particular, the caller
/// can drop the startup future on session teardown while PipeWire is still
/// registering the sink; dropping a bare JoinHandle would detach that thread.
struct SinkStartup {
    cancel: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SinkStartup {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.cancel.cancel();
        // A future can be dropped by its owner without returning from
        // start_audio_sink. Hand off the join to the blocking pool so we don't
        // stall the async executor; the reaper owns the OS thread to completion.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _ = thread.join();
            });
        } else {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        // Stop the GStreamer pipeline; the thread-local PipeWire timer notices
        // cancellation and quits its loop, releasing the sink node.
        self.cancel.cancel();
        let _ = self.pipeline.set_state(gst::State::Null);
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

/// The SPA property ids a node's `Props` param carries its volume in.
///
/// SPA has no dedicated volume parameter. `channelVolumes`, `volume` and `mute`
/// are properties of the generic `SPA_PARAM_Props` object that every node
/// carries, and it is where the session manager writes when a volume control
/// moves — so this is the only place a sink's volume setting is readable.
const PROP_VOLUME: u32 = pw::spa::sys::SPA_PROP_volume;
const PROP_CHANNEL_VOLUMES: u32 = pw::spa::sys::SPA_PROP_channelVolumes;

/// The float a property holds. A `PropInfo` range wraps its current value in a
/// choice, while the live `Props` object carries it bare; both are the same
/// number to a caller that only wants the value.
fn as_float(value: &pw::spa::pod::Value) -> Option<f32> {
    use pw::spa::pod::{ChoiceValue, Value};
    match value {
        Value::Float(value) => Some(*value),
        Value::Choice(ChoiceValue::Float(choice)) => match choice.1 {
            ChoiceEnum::None(value) => Some(value),
            ChoiceEnum::Range { default, .. }
            | ChoiceEnum::Step { default, .. }
            | ChoiceEnum::Enum { default, .. }
            | ChoiceEnum::Flags { default, .. } => Some(default),
        },
        _ => None,
    }
}

/// The level a node's `Props` param reports, as 0..=255.
///
/// `channelVolumes` is authoritative and the scalar `volume` is only a
/// fallback: on a multi-channel sink the session manager writes the per-channel
/// values and leaves `volume` at its default of 1.0, so reading the scalar
/// alone would report every sink as unattenuated.
///
/// The linear gain is converted back to the scale a volume control is on.
/// PipeWire and PulseAudio both use a cubic slider, so 30% is a linear gain of
/// 0.027; reporting that as "3%" would contradict what the user is looking at.
/// (`AudioLevel` is a *setting*, not a measurement, and the client reads it as
/// a percentage.)
fn level_from_props(props: Option<&pw::spa::pod::Pod>) -> Option<u8> {
    let ptr = NonNull::new(props?.as_raw_ptr())?;
    // SAFETY: `ptr` points at a live `spa_pod` that outlives this call, and a
    // deserialized `Value` owns everything it keeps (strings and byte arrays
    // are copied out), so the result borrows nothing from the pod.
    let value = unsafe { pw::spa::pod::deserialize::PodDeserializer::deserialize_ptr(ptr) }.ok()?;
    let pw::spa::pod::Value::Object(object) = value else {
        return None;
    };

    let linear = match object
        .properties
        .iter()
        .find(|prop| prop.key == PROP_CHANNEL_VOLUMES)
    {
        Some(prop) => {
            use pw::spa::pod::{Value, ValueArray};
            let Value::ValueArray(ValueArray::Float(volumes)) = &prop.value else {
                return None;
            };
            if volumes.is_empty() {
                return None;
            }
            volumes.iter().sum::<f32>() / volumes.len() as f32
        }
        None => as_float(
            &object
                .properties
                .iter()
                .find(|prop| prop.key == PROP_VOLUME)?
                .value,
        )?,
    };

    let setting = linear.clamp(0.0, 1.0).cbrt();
    Some((setting * u8::MAX as f32).round() as u8)
}

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
        n => (0..n)
            .map(|i| format!("AUX{i}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Create an application-owned PipeWire audio sink, then capture and Opus-
/// encode everything played into it. Returns the sink handle (for
/// lifetime/teardown) and a receiver of encoded audio chunks.
///
/// `name` is this session's already-rendered virtual speaker name (see
/// [`crate::config::NameTemplate`]) and becomes the PipeWire node name. It is
/// unique per live session, which is what lets the capture pipeline below
/// address the `.monitor` source by name at all.
///
/// `volume` receives the sink's volume setting (0..=255) as the user changes it
/// on the host, and is sent to whenever the node's current value is known —
/// which includes the value at the moment the sink is created.
pub async fn start_audio_sink(
    cancel: CancellationToken,
    name: String,
    channels: u8,
    rate: u32,
    volume: mpsc::Sender<u8>,
) -> Result<(AudioSink, mpsc::Receiver<AudioPacket>)> {
    // Clamp to sane bounds and apply defaults when the client reports nothing.
    let channels = if channels == 0 {
        DEFAULT_CHANNELS
    } else {
        channels.min(8)
    };
    let rate = if rate == 0 { DEFAULT_RATE } else { rate };

    // The sink gets its own child token. Tearing the sink down on drop must not
    // cancel the surrounding session — only this audio subtree.
    let audio_cancel = cancel.child_token();
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>(256);
    let (id_tx, id_rx) = tokio::sync::oneshot::channel::<u32>();

    // The same child token is cancelled on every startup failure, so the
    // thread can always be reaped even before an AudioSink exists.
    let cancel_thread = audio_cancel.clone();
    let name_thread = name.clone();
    let channels_thread = channels;
    let thread = std::thread::spawn(move || {
        if let Err(e) = sink_thread(id_tx, cancel_thread, name_thread, channels_thread, volume) {
            eprintln!("[audio] sink error: {e:#}");
        }
    });
    let mut startup = SinkStartup {
        cancel: audio_cancel.clone(),
        thread: Some(thread),
    };

    // Keep ownership of the OS thread through *all* startup steps. An error
    // after the sink appears (GStreamer setup included) must close its node.
    let setup: Result<(gst::Pipeline, mpsc::Receiver<AudioPacket>)> = async {
        tokio::select! {
            _ = cancel.cancelled() => return Err(anyhow!("audio cancelled before sink was ready")),
            id = id_rx => { id.map_err(|_| anyhow!("audio sink thread terminated"))?; }
        }

        gst::init()?;
        let monitor_source = format!("{name}.monitor");
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
                    if tx.try_send(AudioPacket { data: map.to_vec() }).is_err() {
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

        Ok((pipeline, audio_rx))
    }
    .await;

    match setup {
        Ok((pipeline, audio_rx)) => Ok((
            AudioSink {
                cancel: audio_cancel,
                thread: startup.thread.take(),
                pipeline,
            },
            audio_rx,
        )),
        Err(error) => {
            audio_cancel.cancel();
            // Reap off the executor: the PipeWire loop needs up to one timer
            // tick to notice cancellation and release the application sink.
            // Taking the handle out of `startup` prevents its drop from
            // spawning a second reaper for the same thread.
            if let Some(thread) = startup.thread.take() {
                tokio::task::spawn_blocking(move || {
                    let _ = thread.join();
                })
                .await?;
            }
            Err(error)
        }
    }
}

/// Background thread: create the application-owned sink on its own PipeWire
/// connection and keep that connection alive until cancelled. Sends the sink's
/// node id back once it has been created. The mixed
/// audio we stream is captured by the GStreamer pipeline from the sink's
/// PulseAudio `.monitor` source (`<name>.monitor`), which PipeWire's PulseAudio
/// emulation exposes for every sink. Volume changes on this sink are forwarded
/// to `volume` from a listener on the sink node itself.
fn sink_thread(
    id_tx: tokio::sync::oneshot::Sender<u32>,
    cancel: CancellationToken,
    name: String,
    channels: u8,
    volume: mpsc::Sender<u8>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let mainloop_ptr = mainloop.as_raw_ptr();

    // `object.linger=false` means the node is removed when this client
    // disconnects — i.e. it is owned by this application. The channel map (e.g.
    // `FL,FR` for stereo) gives a real sink instead of the default `aux0/aux1`,
    // and is matched to the client's reported channel count. `name` is already
    // the full, session-unique sink name.
    let sink_props = pw::properties::properties! {
        *pw::keys::FACTORY_NAME => "support.null-audio-sink",
        *pw::keys::NODE_NAME => name.clone(),
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::AUDIO_CHANNELS => channel_map(channels),
        *pw::keys::OBJECT_LINGER => "false",
    };
    let _sink = core
        .create_object::<pw::node::Node>("adapter", &sink_props)
        .map_err(|e| anyhow!("failed to create audio sink: {e}"))?;
    let sink_id = _sink.upcast_ref().id();

    // Watch this node's own volume. `_sink` is the object the volume lives on,
    // so nothing has to be looked up by name and no second PipeWire connection
    // is needed — the callback already runs on the main loop driving this
    // connection. Subscribing first and then enumerating gets the current value
    // as one initial event, with no window in which a change can be missed.
    _sink.subscribe_params(&[pw::spa::param::ParamType::Props]);
    let _listener = _sink
        .add_listener_local()
        .param(move |_seq, id, _index, _next, props| {
            if id == pw::spa::param::ParamType::Props
                && let Some(level) = level_from_props(props)
            {
                // Never park the PipeWire loop on a full async channel: Drop
                // joins this thread, and the consumer may have stopped reading.
                // Volume is a setting, not a sequence of required samples.
                let _ = volume.try_send(level);
            }
        })
        .register();
    _sink.enum_params(0, Some(pw::spa::param::ParamType::Props), 0, u32::MAX);

    // Give PipeWire/PulseAudio a moment to register the sink's `.monitor`
    // source before the GStreamer capture pipeline tries to open it by name.
    std::thread::sleep(Duration::from_millis(400));
    let _ = id_tx.send(sink_id);

    // Keep the connection alive until cancellation.
    let cancel_timer = cancel.clone();
    // This pointer never leaves the owning OS thread and remains valid until
    // mainloop.run() returns. No other thread may call quit after it is freed.
    let _timer = mainloop.loop_().add_timer(move |_| {
        if cancel_timer.is_cancelled() {
            unsafe {
                pw::sys::pw_main_loop_quit(mainloop_ptr);
            }
        }
    });
    let _ = _timer.update_timer(Some(Duration::ZERO), Some(Duration::from_millis(200)));

    mainloop.run();

    // `_sink` and `core` are dropped here, removing the sink from the graph.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw::spa::pod::builder::Builder;
    use pw::spa::utils::SpaTypes;
    use pw::spa::{param::ParamType, sys as spa_sys};
    use std::mem::MaybeUninit;

    /// Build the `Props` pod a node reports, holding the properties given.
    ///
    /// `libspa`'s builder macro has no arm for an array-valued property, which
    /// is exactly the one this reads, so the builder is driven directly.
    fn props_pod(channel_volumes: &[f32], volume: Option<f32>) -> Vec<u8> {
        let mut data = vec![0u8; 512];
        let len = {
            let mut builder = Builder::new(&mut data);
            let mut frame = MaybeUninit::<spa_sys::spa_pod_frame>::uninit();
            // SAFETY: the frame is initialised by `push_object` below and popped
            // before it goes out of scope, which is the only contract it has.
            unsafe {
                assert_eq!(
                    spa_sys::spa_pod_builder_push_object(
                        builder.as_raw_ptr(),
                        frame.as_mut_ptr(),
                        SpaTypes::ObjectParamProps.0,
                        ParamType::Props.as_raw(),
                    ),
                    0
                );
                if let Some(volume) = volume {
                    spa_sys::spa_pod_builder_prop(builder.as_raw_ptr(), PROP_VOLUME, 0);
                    spa_sys::spa_pod_builder_float(builder.as_raw_ptr(), volume);
                }
                if !channel_volumes.is_empty() {
                    spa_sys::spa_pod_builder_prop(builder.as_raw_ptr(), PROP_CHANNEL_VOLUMES, 0);
                    spa_sys::spa_pod_builder_array(
                        builder.as_raw_ptr(),
                        size_of::<f32>() as u32,
                        SpaTypes::Float.0,
                        channel_volumes.len() as u32,
                        channel_volumes.as_ptr().cast(),
                    );
                }
                builder.pop(frame.assume_init_mut());
            }
            // A pod is an 8-byte header followed by its body; `Pod::from_bytes`
            // needs the whole thing, and the builder's buffer is over-allocated.
            builder.as_raw().size as usize + 8
        };
        data.truncate(len);
        data
    }

    /// Read the level back out of a built pod, which is the whole contract:
    /// `level_from_props` only ever sees a `Props` object like this one.
    fn level_of(channel_volumes: &[f32], volume: Option<f32>) -> Option<u8> {
        let bytes = props_pod(channel_volumes, volume);
        let pod = pw::spa::pod::Pod::from_bytes(&bytes)?;
        level_from_props(Some(pod))
    }

    /// The per-channel values win over the scalar default. The session manager
    /// leaves `volume` at 1.0 once it has written per-channel values, so
    /// preferring it would report every muted sink as unattenuated.
    #[test]
    fn the_per_channel_volumes_are_what_gets_reported() {
        assert_eq!(level_of(&[0.027, 0.027], Some(1.0)), Some(77));
        assert_eq!(level_of(&[0.0, 0.0], Some(1.0)), Some(0));
        // A linear gain of 0.5 is a cubic-slider 79%, not 50% — reading the
        // linear gain straight through would report a third of the setting.
        assert_eq!(level_of(&[0.5, 0.5], None), Some(202));
    }

    /// With no per-channel values the scalar default is all there is, and it is
    /// on the same cubic scale.
    #[test]
    fn the_scalar_volume_is_a_fallback() {
        assert_eq!(level_of(&[], Some(1.0)), Some(255));
        assert_eq!(level_of(&[], Some(0.027)), Some(77));
    }

    /// A pod that is not a props object, or a props object with neither
    /// property, reports nothing rather than a made-up level.
    #[test]
    fn a_level_is_only_reported_when_there_is_one() {
        assert_eq!(level_of(&[], None), None);
        assert_eq!(level_from_props(None), None);
    }

    /// A gain above unity is possible (PipeWire allows up to 10.0) and is
    /// reported as full scale rather than wrapping around into silence.
    #[test]
    fn a_gain_above_unity_clamps_to_full_scale() {
        assert_eq!(level_of(&[4.0, 4.0], None), Some(255));
        assert_eq!(level_of(&[-1.0, -1.0], None), Some(0));
    }
}
