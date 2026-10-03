use crate::keyboard::Keyboard;
use crate::pipewire::portal_auth::{
    PortalToken, accept_dialog, get_portal_token, load_persisted_portal_token, set_portal_token,
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
    sync::{broadcast::Receiver, broadcast::error::RecvError, mpsc},
    task::JoinHandle,
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
pub async fn capture(
    mut client_rx: Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<Codec>>>>,
    display_name: String,
    cancel: CancellationToken,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
) -> Result<(mpsc::Receiver<EncodedFrame>, JoinHandle<()>)> {
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(8);

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

    let task = spawn({
        let cancel = cancel.clone();
        let decoder_caps = decoder_caps.clone();
        let portal_token = portal_token.clone();
        let server_msg_tx = server_msg_tx.clone();
        async move {
            while !cancel.is_cancelled() {
                if let Err(e) = single_capture(
                    &mut client_rx,
                    frame_tx.clone(),
                    server_msg_tx.clone(),
                    &cancel,
                    &mut next_size,
                    &remote_desktop,
                    &screencast,
                    &decoder_caps,
                    &display_name,
                    &portal_token,
                )
                .await
                {
                    log::error!("Capture error: {:#?}", e);
                }
            }
        }
    });

    Ok((frame_rx, task))
}

async fn single_capture(
    client_rx: &mut Receiver<ClientDatagram>,
    frame_tx: mpsc::Sender<EncodedFrame>,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
    cancel: &CancellationToken,
    last_dims: &mut Option<(u16, u16, u8)>,
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
                        Ok(ClientDatagram::ResizeDisplay { width, height, index }) => {
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

        let stream = started
            .streams()
            .iter()
            .rfind(sized_stream(&width, &height))
            .ok_or(anyhow!("no stream at {width}x{height} from portal start"))?;
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
        // The link this runs on measures about 7 Mbit/s, and a keyframe is
        // 25-30x a delta. At 7 Mbit/s of video there is no headroom for one:
        // a 67 KB keyframe is a 77 ms freeze, and the deltas produced behind
        // it push the total past what the link carries, so the keyframe itself
        // is what loses the frames that ask for the next one. The ceiling is
        // set below the link's capacity so a keyframe fits inside the space
        // the deltas leave, which is the only thing that breaks the loop.
        let bitrate = 4000;
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc fd={raw_fd} path={node_id} \
         ! videoconvert \
         ! {encoder} name=enc rate-control=vbr bitrate={bitrate} target-percentage=75 \
         ! appsink name=sink sync=false",
            encoder = codec.gst_encoder_element(),
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;

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

        let (pipeline_restart, mut pipeline_restart_watcher) = tokio::sync::watch::channel(());
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
                                    let _ = pipeline_restart.send(());
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
                    log::warn!("GPU context lost, restarting capture pipeline");
                    break Some((width, height, index));
                },
                msg = client_rx.recv() => match msg {
                    Ok(ClientDatagram::ResizeDisplay { width, height, .. }) => {
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

fn sized_stream(width: &u16, height: &u16) -> impl Fn(&&Stream) -> bool {
    |stream| {
        if let Some((target_width, target_height)) = stream.size() {
            let w_equals = *width == target_width.unsigned_abs() as u16;
            let h_equals = *height == target_height.unsigned_abs() as u16;
            w_equals && h_equals
        } else {
            false
        }
    }
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
