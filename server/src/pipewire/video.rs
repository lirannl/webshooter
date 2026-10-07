use crate::keyboard::Keyboard;
use crate::pipewire::bitrate::{LinkPressure, next_bitrate};
use crate::pipewire::portal_auth::{
    PortalToken, SESSION_LOCKED, accept_dialog, get_portal_token, load_persisted_portal_token,
    set_portal_token,
};
use crate::{extensions::CancellationTokenExt, pipewire::eis::eis_task};
use anyhow::{Result, anyhow};
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType, Stream,
    },
};
use ashpd::enumflags2::BitFlags;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use libc;
use shared::client_datagram::ClientDatagram;
use shared::codec::{Codec, select_codec};
use shared::server_datagram::ServerDatagram;
use std::{
    os::fd::IntoRawFd,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    spawn,
    sync::{broadcast::Receiver, broadcast::error::RecvError, mpsc, watch},
    time::sleep,
};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Virtual monitor (KWin)
// ---------------------------------------------------------------------------

/// Make `user_name` safe to use as a monitor/PipeWire node identifier by
/// replacing every non-alphanumeric character with `_`.
///
/// This is applied to the `name` parameter before it is rendered into a name
/// template, not to the rendered result: the literal text of a template is its
/// author's, and a config that produces something PipeWire rejects is a
/// mistake worth seeing rather than one worth silently rewriting.
pub fn sanitise_name(user_name: &str) -> String {
    user_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

fn is_kwin() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|d| d.to_ascii_lowercase().contains("kde"))
        .unwrap_or(false)
}

enum VirtualMonitor {
    ChildProcess(Child),
    Portal,
}

impl VirtualMonitor {
    fn spawn(width: u16, height: u16, name: String) -> Result<Self> {
        if is_kwin() {
            use std::os::unix::process::CommandExt;
            let mut cmd = Command::new("krfb-virtualmonitor");
            cmd.args([
                "--resolution",
                &format!("{width}x{height}"),
                "--name",
                &name,
                "--password",
                "x",
                "--port",
                "-1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGTERM as libc::c_ulong,
                        0,
                        0,
                        0,
                    );
                    Ok(())
                });
            }
            match cmd.spawn() {
                Ok(child) => Ok(Self::ChildProcess(child)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::Portal),
                Err(e) => Err(e.into()),
            }
        } else {
            Ok(VirtualMonitor::Portal)
        }
    }
}

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        if let Self::ChildProcess(child) = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct EncodedFrame {
    pub data: gst::Buffer,
    pub is_keyframe: bool,
    pub codec: Codec,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create the virtual keyboard at startup.
#[cfg(target_os = "linux")]
/// Open the XDG screencast portal, build a GStreamer encode pipeline, and
/// start streaming encoded frames into the returned channel.
///
/// `display_name` is this capture's already-rendered virtual display name (see
/// [`crate::config::NameTemplate`]); it uniquely identifies the monitor, so
/// multiple simultaneous clients don't collide on the same node.
///
/// `frame_tx` is the session's media channel, created by the caller rather than
/// here so the MoQ publisher draining it is already running before the first
/// frame can arrive. The channel outlives any single pipeline: a resize rebuilds
/// the encoder underneath it, and frames that gap are the ones a decoder would
/// throw away regardless.
/// The configured bitrate ceiling, in kbit/s.
///
/// Also the starting value: the governor only lowers it while the path is
/// congested and walks it back up to exactly this, never past it. See
/// [`bitrate`] for the policy.
const BITRATE_CEILING_KBPS: u32 = 4000;

/// The capture pipeline.
///
/// `videoconvert` is not optional decoration. Every encoder here is VA-API and
/// accepts only NV12 or P010 — never RGB — while a compositor's screencast may
/// hand over whatever it likes; on this one it is BGRA, which costs a software
/// conversion of ~587 MB/s on the single streaming thread (there is no `queue`,
/// so it costs throughput headroom, which is what becomes latency under load).
///
/// The converter's *input* is deliberately left unconstrained. Constraining it to
/// NV12 in the hope of making the conversion a no-op looks tempting and is a trap:
/// it assumes a particular compositor can be asked for a particular format, and
/// when it cannot the pipeline dies with `not-negotiated` at PLAYING — which is
/// where caps are actually resolved, long after `gst::parse::launch` has
/// cheerfully returned a pipeline. An unconstrained converter is the portable
/// choice: it accepts whatever arrives and emits the one format the encoder needs.
///
/// GPU conversion (`vaconvert`) would move the work off the CPU where it exists,
/// and would be worth using if installed.
fn build_pipeline(raw_fd: i32, node_id: u32, codec: Codec) -> Option<gst::Pipeline> {
    gst::parse::launch(&format!(
        "pipewiresrc name=src fd={raw_fd} path={node_id} \
     ! videoconvert name=convert \
     ! {encoder} name=enc rate-control=vbr bitrate={bitrate} target-percentage=75 \
     ! appsink name=sink sync=false",
        encoder = codec.gst_encoder_element(),
        bitrate = BITRATE_CEILING_KBPS,
    ))
    .ok()
    .and_then(|pipeline| pipeline.downcast::<gst::Pipeline>().ok())
}

/// Re-target the encoder when the path cannot carry the configured bitrate.
///
/// The bitrate is changed as a property on the running encoder rather than by
/// rebuilding the pipeline: a rebuild would cost a fresh portal session, a fresh
/// encoder and a keyframe the client has to ask for, which is a far worse
/// response to a congested link than a slower picture.
///
/// Sampling is driven by the path report that already runs for logging, rather
/// than by a second reader of the QUIC counters: one place decides what the
/// link looks like, and the encoder follows from that decision.
async fn govern_bitrate(
    encoder: gst::Element,
    mut pressure_rx: watch::Receiver<LinkPressure>,
    cancel: CancellationToken,
) {
    let mut current = BITRATE_CEILING_KBPS;
    loop {
        let pressure = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            // A closed sender means the path sampler is gone, which means no
            // more evidence either way; the ceiling is the safe thing to sit at.
            changed = pressure_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                *pressure_rx.borrow_and_update()
            }
        };
        let next = next_bitrate(current, BITRATE_CEILING_KBPS, pressure);
        if next == current {
            continue;
        }
        let previous = current;
        current = next;
        // `set_property` rather than a property-notify or a reconfigure: the
        // encoders re-target their rate control on this, which is the whole
        // point, and a failure here is not worth tearing the pipeline down for.
        // The property is read back rather than trusted, because the encoders
        // silently ignore a bitrate they cannot reach — and a governor that
        // believes it adapted when it did not is worse than none.
        encoder.set_property("bitrate", current);
        let applied: u32 = encoder.property("bitrate");
        if applied != current {
            log::warn!(
                "encoder refused bitrate {current} (still {applied}); the \
                 governor will keep trying"
            );
            current = applied;
            continue;
        }
        log::info!(
            "bitrate {previous} -> {applied} kbit/s ({pressure:?}, ceiling \
             {BITRATE_CEILING_KBPS})"
        );
    }
}

/// What format each stage of the pipeline actually negotiated, once per capture.
///
/// This exists to answer one question with evidence rather than plausibility:
/// all four encoders are VA-API and accept only NV12 or P010, and a screencast
/// stream normally *is* NV12 — so `videoconvert` may be a full CPU pass per
/// frame that changes nothing. It sits on the single streaming thread (there is
/// no `queue` in this pipeline), so its cost is not buffering but throughput
/// headroom, which shows up as latency under load.
///
/// It cannot be answered from the elements' own caps: `pipewiresrc` advertises
/// `ANY`, so the format only exists once the stream is running. Hence reading it
/// off the running pipeline.
fn log_negotiated_formats(pipeline: &gst::Pipeline) {
    let stages = [
        ("src", "src"),
        ("convert-in", "sink"),
        ("convert-out", "src"),
        ("enc", "sink"),
    ];
    let mut seen = Vec::new();
    for (label, pad_name) in stages {
        let caps = match label {
            "convert-in" => pipeline
                .by_name("convert")
                .and_then(|e| e.static_pad("sink"))
                .and_then(|p| p.current_caps()),
            "convert-out" => pipeline
                .by_name("convert")
                .and_then(|e| e.static_pad("src"))
                .and_then(|p| p.current_caps()),
            name => pipeline
                .by_name(name)
                .and_then(|e| e.static_pad(pad_name))
                .and_then(|p| p.current_caps()),
        };
        let caps = match caps {
            Some(caps) => format!("{caps}"),
            None => "unlinked".to_string(),
        };
        seen.push(format!("{label}={caps}"));
    }
    log::info!("pipeline formats: {}", seen.join(" | "));
}

pub async fn capture(
    mut client_rx: Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<Codec>>>>,
    display_name: String,
    client_id: u64,
    pressure_rx: watch::Receiver<LinkPressure>,
    cancel: CancellationToken,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
    frame_tx: mpsc::Sender<EncodedFrame>,
) -> Result<()> {
    // Per-capture portal token, seeded from the startup token so the
    // first `select_devices` dialog is still skipped, but kept private to this
    // capture so concurrent captures can't clobber each other.
    let portal_token: PortalToken = load_persisted_portal_token().await;

    // Control datagrams are routed to this specific client via the session's
    // control channel, so no broadcast subscription is needed here.

    let remote_desktop = cancel.r(RemoteDesktop::new()).await?;
    let screencast = cancel.r(Screencast::new()).await?;
    // Persists across single_capture calls so a pipeline failure retry
    // reuses the same dimensions instead of hanging for another
    // ResizeDisplay (which was consumed on the first call).
    let mut next_size: Option<(u16, u16, u8)> = None;

    while !cancel.is_cancelled() {
        if let Err(e) = single_capture(
            &mut client_rx,
            frame_tx.clone(),
            server_msg_tx.clone(),
            &cancel,
            &mut next_size,
            client_id,
            pressure_rx.clone(),
            &remote_desktop,
            &screencast,
            &decoder_caps,
            &display_name,
            &portal_token,
        )
        .await
        {
            log::error!("Capture error: {:#?}", e);
            // A capture that failed because the session is locked is not a
            // capture that failed. Retrying it immediately would spin on the
            // same unapprovable dialog, so it waits for the unlock instead — but
            // only *after* failing, never as a precondition: with a valid
            // restore token the portal calls show no dialog at all, and a
            // locked session starts those captures perfectly well.
            if !crate::session_lock::state().can_approve()
                && !park_until_unlocked(&server_msg_tx, &cancel).await
            {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Wait for the session to unlock, telling the client once.
///
/// Reached only *after* a capture has failed and the lock state has been read
/// as the reason, so the lock is a diagnosis of a failure rather than a gate in
/// front of one.
///
/// The client is told once, not once per poll: this re-checks every
/// `POLL_INTERVAL`, and re-sending the same refusal on each of those turns one
/// message into a stream of them.
///
/// Returns `false` if the session ended while waiting, which ends the capture
/// rather than starting one nobody is there for.
async fn park_until_unlocked(
    server_msg_tx: &mpsc::Sender<ServerDatagram>,
    cancel: &CancellationToken,
) -> bool {
    log::warn!("capture parked: {SESSION_LOCKED} (waiting for an unlock)");
    if let Err(err) = server_msg_tx
        .send(ServerDatagram::Error {
            level: log::Level::Warn,
            message: SESSION_LOCKED.to_owned(),
        })
        .await
    {
        log::debug!("could not tell the client the session is locked: {err:#}");
        return false;
    }
    crate::session_lock::wait_until_unlocked(cancel).await;
    !cancel.is_cancelled()
}

async fn single_capture(
    client_rx: &mut Receiver<ClientDatagram>,
    frame_tx: mpsc::Sender<EncodedFrame>,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
    cancel: &CancellationToken,
    last_dims: &mut Option<(u16, u16, u8)>,
    client_id: u64,
    pressure_rx: watch::Receiver<LinkPressure>,
    remote_desktop: &RemoteDesktop,
    screencast: &Screencast,
    decoder_caps: &Mutex<Option<Vec<Codec>>>,
    display_name: &str,
    portal_token: &PortalToken,
) -> Result<()> {
    loop {
        // --- Portal session -------------------------------------------------
        // Create a fresh portal session each iteration. Both screencast and
        // remote-desktop (touchscreen) share this one session.  The session
        // is dropped at the bottom of the loop and recreated on the next
        // iteration (which handles both resize and GPU reconnect).

        // Use dimensions carried over from the previous resize or retry, or
        // wait for the first ResizeDisplay from the client.
        let (width, height, index) = match last_dims.take() {
            Some(size) => size,
            None => loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(()),
                    msg = client_rx.recv() => match msg {
                        Ok(ClientDatagram::DisplayParameters { width, height, index }) => {
                            // A resize is not a cheap message: the loop below closes
                            // the portal session and rebuilds the entire pipeline,
                            // encoder included, and a new encoder's first frame owes
                            // the client a keyframe it has to ask for. Every
                            // address-bar collapse or keyboard toggle on a phone
                            // triggers one, so it is worth being able to see.
                            log::info!("resize: rebuilding capture at {width}x{height}");
                            break (width, height, index);
                        }
                        Ok(_) => continue,
                        // Falling behind the bus is expected here, not fatal: the
                        // client keeps its 50 ms keepalive running while the
                        // portal dialog is up, so a slow human decision
                        // overflows the channel on its own. The resize being
                        // waited for has not been lost — it is a state message
                        // the client resends on the next one.
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return Ok(()),
                    },
                }
            },
        };
        // Stash so a pipeline-failure retry reuses the same dimensions
        // instead of hanging for another ResizeDisplay.
        last_dims.replace((width, height, index));

        let virtual_monitor = VirtualMonitor::spawn(width, height, display_name.to_owned())?;

        if let VirtualMonitor::ChildProcess(_) = virtual_monitor {
            cancel
                .run_until_cancelled(sleep(Duration::from_millis(500)))
                .await;
        }

        let source_type = match &virtual_monitor {
            VirtualMonitor::ChildProcess(_) => SourceType::Monitor,
            VirtualMonitor::Portal => SourceType::Virtual,
        };

        // A short-lived keyboard just for auto-confirming portal dialogs.
        // Dropped once the portal session is established.
        let mut portal_kb = Keyboard::new("Webshooter Portal Authorisation");

        let session = cancel
            .r(remote_desktop.create_session(CreateSessionOptions::default()))
            .await?;

        // The restore token from the previous start() lets select_devices
        // restore the same device permissions without showing a dialog.
        let select_dev_opts = SelectDevicesOptions::default()
            .set_restore_token(get_portal_token(portal_token).as_deref())
            .set_devices(Some(
                DeviceType::Touchscreen | DeviceType::Pointer | DeviceType::Keyboard,
            ))
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        accept_dialog(
            &mut portal_kb,
            cancel.r(remote_desktop.select_devices(&session, select_dev_opts)),
        )
        .await?;

        accept_dialog(
            &mut portal_kb,
            cancel.r(screencast.select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_multiple(true)
                    .set_sources(Some(BitFlags::from(source_type)))
                    .set_cursor_mode(CursorMode::Embedded),
            )),
        )
        .await?;

        let request = accept_dialog(
            &mut portal_kb,
            cancel.r(remote_desktop.start(&session, None, StartOptions::default())),
        )
        .await?;
        let started = request.response()?;

        drop(portal_kb);

        // The restore token lets the next capture skip the select_devices
        // dialog.  Source selection and start() always show dialogs.
        let token = started.restore_token();
        if let Some(token) = token {
            set_portal_token(portal_token, token.to_owned());
        }

        let streams = started.streams();
        let stream = pick_stream(streams, width, height).ok_or_else(|| {
            // Naming what *was* offered is the difference between a
            // diagnosable failure and a guessing game: "no stream" alone cannot
            // distinguish a portal that offered nothing from one that offered
            // every size except the right shape.
            let offered = streams
                .iter()
                .map(|s| match s.size() {
                    Some((w, h)) => format!("{w}x{h}"),
                    None => "unsized".to_owned(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "no stream of the right shape for {width}x{height} from portal start; \
                 it offered [{}]",
                if offered.is_empty() {
                    "nothing"
                } else {
                    &offered
                }
            )
        })?;
        if let Some((w, h)) = stream.size() {
            // The portal is allowed to answer with a different resolution than
            // the one asked for (it does, and did: two thirds of it), so this
            // is expected rather than alarming — but it changes what the client
            // is paying for in bitrate, so it is worth being able to see.
            if (w as u16) != width || (h as u16) != height {
                log::info!(
                    "portal streamed at {w}x{h} rather than the requested \
                     {width}x{height}; same shape, different detail"
                );
            }
        }
        let node_id = stream.pipe_wire_node_id();
        let stream_pos = stream.position().unwrap_or((0, 0));

        let pw_fd = cancel
            .r(screencast.open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default()))
            .await?;
        let raw_fd = pw_fd.into_raw_fd();

        // --- GStreamer pipeline --------------------------------------------------

        // Pick the best codec that the client supports.
        let decoders = decoder_caps.lock().unwrap().clone().unwrap_or_default();
        let codec = select_codec(&decoders);
        println!("[video] selected codec: {codec:?} (client decoders: {decoders:?})");

        gst::init()?;
        // The ceiling, and the starting point. `bitrate::next_bitrate` may only
        // move this down while the path is congested and back up to exactly
        // this value — never past it.
        //
        // The link this runs on measures about 7 Mbit/s, and a keyframe is
        // 25-30x a delta. At 7 Mbit/s of video there is no headroom for one:
        // a 67 KB keyframe is a 77 ms freeze, and the deltas produced behind
        // it push the total past what the link carries, so the keyframe itself
        // is what loses the frames that ask for the next one. The ceiling is
        // set below the link's capacity so a keyframe fits inside the space
        // the deltas leave, which is the only thing that breaks the loop.
        let pipeline = build_pipeline(raw_fd, node_id, codec)
            .ok_or(anyhow!("could not build the capture pipeline"))?;

        // Keyframes are produced only when the client asks for one. The client is
        // the only party that knows whether its prediction chain is broken, and a
        // keyframe is the most expensive thing on the link by a wide margin
        // (measured ~72 KB against ~16 KB for a delta), so a schedule the server
        // picks for itself spends the link on frames nobody needs — and a
        // periodic one guarantees the client pays for a burst of them every
        // period, right when the link is already struggling.
        let encoder = pipeline
            .by_name("enc")
            .ok_or(anyhow!("no encoder element"))?;
        {
            let mut keyframe_rx = client_rx.resubscribe();
            let encoder = encoder.clone();
            let cancel = cancel.clone();
            spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        msg = keyframe_rx.recv() => match msg {
                            Ok(ClientDatagram::RequestKeyframe) => force_keyframe(&encoder),
                            Ok(_) => continue,
                            // The request may be in the messages this receiver just
                            // missed, and the client cannot recover on its own: it
                            // is holding frames waiting for a keyframe that nothing
                            // is going to send, so the display stays frozen until
                            // the session restarts. A keyframe nobody asked for is
                            // cheap next to that, so err towards sending one.
                            Err(RecvError::Lagged(_)) => force_keyframe(&encoder),
                            Err(RecvError::Closed) => break,
                        },
                    }
                }
            });
        }

        {
            let cancel = cancel.clone();
            spawn(govern_bitrate(encoder.clone(), pressure_rx.clone(), cancel));
        }

        let appsink = pipeline
            .by_name("sink")
            .ok_or(anyhow!("no sink element"))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("not an appsink"))?;

        let (cursor_tx, cursor_rx) = mpsc::channel::<(i32, i32)>(32);
        let tx = frame_tx.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;

                    // Extract compositor cursor position from
                    // GstVideoRegionOfInterestMeta("cursor") emitted by
                    // pipewiresrc when CursorMode::Embedded is set.
                    if let Some(meta) = buffer.meta::<gst_video::VideoRegionOfInterestMeta>()
                        && meta.roi_type() == "cursor"
                    {
                        let (x, y, _w, _h) = meta.rect();
                        let _ = cursor_tx.try_send((x as i32, y as i32));
                    }

                    let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    let frame = EncodedFrame {
                        data: buffer.to_owned(),
                        is_keyframe,
                        codec,
                    };
                    // Backpressure: if the consumer can't keep up, drop this
                    // frame rather than returning FlowError::Error, which would
                    // tear down the whole GStreamer pipeline and trigger a
                    // restart storm.
                    if tx.try_send(frame).is_err() {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Log what resolution the pipeline *actually* negotiated, next to the
        // size the portal reported.
        //
        // These are not the same number, and which is which decides where input
        // coordinates belong. The portal reports a stream's size in logical
        // units — divided by the output's scale — while the frames on the
        // PipeWire node are physical pixels. On a 150%-scaled desktop that makes
        // the two differ by exactly 1.5, so a client that reports coordinates in
        // frame pixels and a portal that accepts them in the reported units are
        // talking past each other by that factor, and nothing in the error
        // messages says so.
        if let Some(enc) = pipeline.by_name("enc")
            && let Some(pad) = enc.static_pad("src")
        {
            let requested = (width, height);
            let reported = stream.size().map(|(w, h)| (w as u16, h as u16));
            pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _info| {
                // The pad's negotiated caps are what the encoder is really
                // being fed, which is the question being asked here.
                let size = pad.current_caps().and_then(|caps| {
                    let s = caps.structure(0)?;
                    Some((s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?))
                });
                if let Some((w, h)) = size {
                    log::info!(
                        "encoder is fed {w}x{h} (monitor {requested:?}, \
                         portal reported {reported:?})"
                    );
                } else {
                    log::info!(
                        "encoder caps unreadable (monitor {requested:?}, \
                         portal reported {reported:?})"
                    );
                }
                // One look is all that is needed; the negotiated caps do not
                // change without the pipeline being rebuilt.
                gst::PadProbeReturn::Remove
            });
        }

        // Carries *why* a restart was asked for, because the two reasons need
        // different things read back out of the log hours later: a GPU loss is
        // about the encoder, a lost input device is about the compositor
        // replacing its handles.
        let (pipeline_restart, mut pipeline_restart_watcher) =
            tokio::sync::watch::channel(None::<&'static str>);
        let pipeline_restart = Arc::new(pipeline_restart);
        {
            let bus = pipeline.bus().ok_or(anyhow!("no pipeline bus"))?;
            let pipeline_ref = pipeline.clone();
            let pipeline_restart = pipeline_restart.clone();
            let cancel = cancel.clone();
            tokio::task::spawn_blocking(move || {
                // Exit on cancellation as well as EOS/Error, otherwise this
                // task would block forever (iter_timed with no timeout) and
                // keep a strong ref to the pipeline — preventing it from ever
                // being finalized and leaking the VA-API encoder context across
                // sessions.
                loop {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) else {
                        continue;
                    };
                    match msg.view() {
                        gst::MessageView::Eos(_) | gst::MessageView::Error(_) => {
                            if let gst::MessageView::Error(err) = msg.view() {
                                let err_str = format!(
                                    "{} — {}",
                                    err.error(),
                                    err.debug().unwrap_or_default()
                                );
                                if err_str.contains("context") && err_str.contains("lost")
                                    || err_str.contains("hard recovery")
                                    || err_str.contains("context is lost")
                                    || err_str.contains("GPU")
                                    || err_str.contains("vaapi")
                                    || err_str.contains("amf")
                                {
                                    log::error!("GPU context loss detected: {err_str}");
                                    let _ = pipeline_restart.send(Some("GPU context lost"));
                                }
                            }
                            let _ = pipeline_ref.set_state(gst::State::Null);
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }

        pipeline.set_state(gst::State::Playing).map_err(|e| {
            let bus_msg = pipeline
                .bus()
                .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
                .and_then(|msg| {
                    if let gst::MessageView::Error(err) = msg.view() {
                        Some(format!(
                            "{} — {}",
                            err.error(),
                            err.debug().unwrap_or_default()
                        ))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| format!("{e:?}"));
            let _ = pipeline.set_state(gst::State::Null);
            anyhow!("Pipeline failed to enter Playing state: {bus_msg}")
        })?;
        // Once PLAYING, and therefore once caps have actually been negotiated
        // from the live stream rather than from what the elements advertise.
        log_negotiated_formats(&pipeline);

        // Connect to the EIS implementation for touch injection. This
        // replaces the NotifyTouch* portal calls which are a no-op on KDE
        // and many wlroots-based compositors.
        let eis_fd = match cancel
            .r(remote_desktop.connect_to_eis(&session, ConnectToEISOptions::default()))
            .await
        {
            Ok(fd) => fd,
            Err(e) => {
                println!("[video] connect_to_eis: {e:#}, retrying in 500ms");
                sleep(Duration::from_millis(500)).await;
                cancel
                    .r(remote_desktop.connect_to_eis(&session, ConnectToEISOptions::default()))
                    .await?
            }
        };

        let touch_task = eis_task(
            eis_fd,
            stream_pos,
            (width, height),
            client_id,
            pipeline_restart.clone(),
            client_rx,
            &server_msg_tx,
            cursor_rx,
            cancel,
        );

        // Wait for the next resize (or cancellation or GPU loss) before tearing
        // down.  virtual_monitor must stay alive here — dropping it kills krfb.
        *last_dims = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break None,
                _ = pipeline_restart_watcher.changed() => {
                    log::warn!(
                        "{}; restarting capture pipeline",
                        pipeline_restart_watcher
                            .borrow_and_update()
                            .unwrap_or("restart requested")
                    );
                    break Some((width, height, index));
                },
                msg = client_rx.recv() => match msg {
                    Ok(ClientDatagram::DisplayParameters { width, height, .. }) => {
                        log::info!("resize: rebuilding capture at {width}x{height}");
                        break Some((width, height, index));
                    }
                    Ok(ClientDatagram::Keyboard { keycode: _, modifiers: _ }) => {
                        // Handled by the EIS input task in touch.rs
                    }
                    Ok(_) => continue,
                    // A stalled consumer is not a lost session: keep the
                    // pipeline up and wait for the next resize, as above.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break None,
                },
            }
        };

        touch_task.abort();
        tokio::task::spawn_blocking({
            let pipeline = pipeline.clone();
            move || {
                let _ = pipeline.set_state(gst::State::Null);
            }
        })
        .await?;
        // Explicitly close the portal session so the compositor releases
        // the capture/input grants.  Drop alone does not call the D-Bus
        // Session.Close method.
        if let Err(e) = session.close().await {
            println!("[video] failed to close portal session: {e:#}");
        }
        // virtual_monitor, remote_desktop, screencast, pw_fd, eis_fd
        // dropped here.  Next loop iteration creates fresh ones.

        if last_dims.is_none() {
            return Ok(());
        }
    }
}

/// How far a stream's aspect ratio may sit from the requested one and still
/// count as the same picture.
///
/// Two percent separates "the same shape at a different resolution" from "a
/// different shape entirely" by a wide margin: the real case is 1080x2255
/// against 720x1503, whose ratios differ by a thousandth.
///
/// Shared with `touch::input_space`, which asks the same question of a touch
/// region's logical size: both are asking "same monitor, different resolution?",
/// and two tolerances for one idea would be free to disagree about it.
pub(super) const ASPECT_TOLERANCE: f64 = 0.02;

/// Choose which of the sizes the portal offered to actually capture.
///
/// The portal is the authority on what it will stream, and it need not answer
/// with the size that was asked for. KDE's screencast answers a 1080x2255
/// request with a 720x1503 stream — the same picture at two thirds the linear
/// size — so requiring an exact match fails a capture that would have worked,
/// and fails it on every retry, because the portal answers the same way each
/// time. That is a permanent failure, not a transient one.
///
/// An exact match still wins whenever it is offered. Otherwise the closest
/// aspect ratio within [`ASPECT_TOLERANCE`] wins, because the aspect ratio is
/// what decides whether the picture is the right *shape*, while resolution only
/// decides how much detail it carries. Rejecting a correct shape over its
/// resolution throws away a working session to protect a nicety.
///
/// A smaller stream is a lower-quality session rather than a broken one: the
/// pipeline carries no fixed-size capsfilter, so it encodes whatever the source
/// node produces, and the client sizes its canvas from the frames it decodes and
/// lets CSS scale them to the viewport.
///
/// Split out from [`pick_stream`] so the decision is testable against the sizes
/// a portal actually returns, rather than only through a live session.
fn best_stream_index(requested: (u16, u16), offered: &[(i32, i32)]) -> Option<usize> {
    let want = (f64::from(requested.0) / f64::from(requested.1)).abs();

    // An exact match needs no judgement: if the asked-for size is on offer, it
    // is the answer, whatever else was offered alongside it.
    let exact = offered
        .iter()
        .position(|&(w, h)| w == i32::from(requested.0) && h == i32::from(requested.1));
    if exact.is_some() {
        return exact;
    }

    offered
        .iter()
        .enumerate()
        // A zero or negative dimension has no aspect ratio; it is a stream that
        // cannot be compared, not one that matches everything.
        .filter(|(_, (w, h))| *w > 0 && *h > 0)
        .map(|(i, (w, h))| {
            let got = f64::from(*w).abs() / f64::from(*h).abs();
            (i, ((got - want) / want).abs())
        })
        .filter(|(_, relative)| *relative <= ASPECT_TOLERANCE)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The stream to capture from, or `None` if none of the offered sizes is even
/// the right shape.
fn pick_stream(streams: &[Stream], width: u16, height: u16) -> Option<&Stream> {
    // Indexed rather than flattened, so the winning size maps back to its
    // stream even when two streams report identical sizes.
    let candidates: Vec<(usize, (i32, i32))> = streams
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.size().map(|size| (i, size)))
        .collect();
    let sizes: Vec<(i32, i32)> = candidates.iter().map(|(_, size)| *size).collect();
    best_stream_index((width, height), &sizes)
        .and_then(|winner| candidates.get(winner))
        .map(|(i, _)| &streams[*i])
}

/// Ask the encoder to produce a keyframe as soon as possible. We send a
/// standard `GstForceKeyUnit` downstream event, which every GStreamer video
/// encoder recognises. This is the only way a keyframe is produced: the server
/// never schedules one, so a keyframe means some client asked for it.
///
/// The running count is reported because the rate is the whole story on a
/// constrained link — a keyframe costs several times a delta, so keyframes per
/// second is what decides whether the link keeps up. Nothing else in the log
/// shows it: the client asks over a stream that looks like any other, and the
/// frames themselves are indistinguishable once they reach the forwarder.
fn force_keyframe(encoder: &gst::Element) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    log::debug!("keyframe {n} forced on client request");
    let structure = gst::Structure::new_empty("GstForceKeyUnit");
    let event = gst::event::CustomDownstream::new(structure);
    let _ = encoder.send_event(event);
}

#[cfg(test)]
mod stream_selection_tests {
    use super::{ASPECT_TOLERANCE, best_stream_index};

    /// The case that made this a bug rather than a nicety: a client asked for
    /// 1080x2255 and KDE's screencast answered with 720x1503 — the same picture
    /// at two thirds the linear size. Requiring an exact match refused a capture
    /// that worked, on every retry, forever.
    #[test]
    fn a_scaled_but_same_shape_stream_is_accepted() {
        let offered = [(720, 1503)];
        assert_eq!(best_stream_index((1080, 2255), &offered), Some(0));
    }

    /// When the asked-for size *is* offered it wins outright, even alongside a
    /// stream that is nearly the same shape — there is no reason to prefer the
    /// approximation over the thing that was requested.
    #[test]
    fn an_exact_match_beats_a_near_miss() {
        let offered = [(720, 1503), (1080, 2255)];
        assert_eq!(best_stream_index((1080, 2255), &offered), Some(1));
    }

    /// The real machine has one physical monitor as well as the virtual one, so
    /// both can be on offer. A portrait request must not be satisfied by a
    /// landscape monitor that happens to be there.
    #[test]
    fn a_differently_shaped_monitor_is_rejected() {
        // A 4K TV at 1920x1080, offered alongside the virtual monitor.
        let offered = [(1920, 1080), (720, 1503)];
        assert_eq!(best_stream_index((1080, 2255), &offered), Some(1));
        // ...and with only the TV on offer there is nothing usable at all.
        assert_eq!(best_stream_index((1080, 2255), &[(1920, 1080)]), None);
    }

    /// Landscape requests must work as symmetrically as portrait ones.
    #[test]
    fn a_landscape_request_matches_a_landscape_stream() {
        let offered = [(1280, 720)];
        assert_eq!(best_stream_index((1920, 1080), &offered), Some(0));
    }

    /// The tolerance has to be tight enough to reject a wrong shape and loose
    /// enough to accept the real one, which differ by four orders of magnitude.
    #[test]
    fn the_tolerance_separates_shape_from_detail() {
        let want = 1080.0_f64 / 2255.0;
        let got = 720.0_f64 / 1503.0;
        let real = ((got - want) / want).abs();
        let wrong = ((1920.0_f64 / 1080.0 - want) / want).abs();
        assert!(
            real <= ASPECT_TOLERANCE,
            "the real scaled stream must be inside the tolerance ({real})"
        );
        assert!(
            wrong > ASPECT_TOLERANCE,
            "a landscape monitor must be outside it ({wrong})"
        );
    }

    /// A stream with no size cannot be compared to anything, and a zero
    /// dimension has no aspect ratio at all. Neither may match by accident.
    #[test]
    fn unusable_sizes_never_match() {
        assert_eq!(best_stream_index((1080, 2255), &[]), None);
        assert_eq!(best_stream_index((1080, 2255), &[(0, 0)]), None);
        assert_eq!(best_stream_index((1080, 2255), &[(0, 1503)]), None);
        assert_eq!(best_stream_index((1080, 2255), &[(720, 0)]), None);
    }
}
