use crate::keyboard::Keyboard;
use crate::pipewire::portal_auth::{accept_dialog, get_portal_token, set_portal_token};
use crate::{extensions::CancellationTokenExt, logging::log, pipewire::eis::eis_task};
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
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    spawn,
    sync::{broadcast::Receiver, mpsc},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Virtual monitor (KWin)
// ---------------------------------------------------------------------------

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
    fn spawn(width: u16, height: u16, index: u8) -> Result<Self> {
        if is_kwin() {
            use std::os::unix::process::CommandExt;
            let mut cmd = Command::new("krfb-virtualmonitor");
            cmd.args([
                "--resolution",
                &format!("{width}x{height}"),
                "--name",
                &format!("webshooter-{}", index),
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

pub struct CaptureHandle {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl CaptureHandle {
    pub async fn close(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Create the virtual keyboard at startup.
#[cfg(target_os = "linux")]
/// Open the XDG screencast portal, build a GStreamer encode pipeline, and
/// start streaming encoded frames into the returned channel.
pub async fn capture(
    mut client_rx: Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<Codec>>>>,
) -> Result<(mpsc::Receiver<EncodedFrame>, mpsc::Receiver<ServerDatagram>, CaptureHandle)> {
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(8);
    let (server_msg_tx, server_msg_rx) = mpsc::channel::<ServerDatagram>(8);
    let cancel = CancellationToken::new();

    // Forward IPC client-control commands (fullscreen toggle, mouse release)
    // to the connected client.
    if let Some(mut client_control_rx) = crate::ipc::subscribe_client_control() {
        let server_msg_tx = server_msg_tx.clone();
        spawn(async move {
            while let Ok(dgram) = client_control_rx.recv().await {
                if server_msg_tx.send(dgram).await.is_err() {
                    break;
                }
            }
        });
    }

    let remote_desktop = cancel.r(RemoteDesktop::new()).await?;
    let screencast = cancel.r(Screencast::new()).await?;
    // Persists across single_capture calls so a pipeline failure retry
    // reuses the same dimensions instead of hanging for another
    // ResizeDisplay (which was consumed on the first call).
    let mut next_size: Option<(u16, u16, u8)> = None;

    let task = spawn({
        let cancel = cancel.clone();
        let decoder_caps = decoder_caps.clone();
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
                )
                .await
                {
                    log(format!("Capture error: {:#?}", e));
                }
            }
        }
    });

    Ok((frame_rx, server_msg_rx, CaptureHandle { cancel, task }))
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
                        Ok(ClientDatagram::ResizeDisplay { width, height, index }) => break (width, height, index),
                        Ok(_) => continue,
                        Err(_) => return Ok(()),
                    },
                }
            },
        };
        // Stash so a pipeline-failure retry reuses the same dimensions
        // instead of hanging for another ResizeDisplay.
        last_dims.replace((width, height, index));

        let virtual_monitor = VirtualMonitor::spawn(width, height, index)?;

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
            .set_restore_token(get_portal_token().await.as_deref())
            .set_devices(Some(BitFlags::from(
                DeviceType::Touchscreen | DeviceType::Pointer | DeviceType::Keyboard,
            )))
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
            set_portal_token(token.to_owned()).await;
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
        let bitrate = 7000;
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc fd={raw_fd} path={node_id} \
         ! videoconvert \
         ! {encoder} name=enc rate-control=vbr bitrate={bitrate} target-percentage=75 \
         ! appsink name=sink sync=false",
            encoder = codec.gst_encoder_element(),
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;

        // Force a keyframe on demand (PLI) or on a fixed interval so that a
        // single dropped frame cannot poison the inter-frame prediction chain
        // indefinitely. Packet loss turns into a bounded clean resync rather
        // than progressive corruption.
        let encoder = pipeline
            .by_name("enc")
            .ok_or(anyhow!("no encoder element"))?;
        {
            let mut keyframe_rx = client_rx.resubscribe();
            let encoder = encoder.clone();
            let cancel = cancel.clone();
            spawn(async move {
                // Bound corruption duration even if no PLI arrives.
                let mut interval = tokio::time::interval(Duration::from_secs(2));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        msg = keyframe_rx.recv() => match msg {
                            Ok(ClientDatagram::RequestKeyframe) => force_keyframe(&encoder),
                            Ok(_) => continue,
                            Err(_) => break,
                        },
                        _ = interval.tick() => force_keyframe(&encoder),
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
                    if let Some(meta) = buffer.meta::<gst_video::VideoRegionOfInterestMeta>() {
                        if meta.roi_type() == "cursor" {
                            let (x, y, _w, _h) = meta.rect();
                            let _ = cursor_tx.try_send((x as i32, y as i32));
                        }
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
                                    log(format!("GPU context loss detected: {err_str}"));
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

        let touch_task = eis_task(eis_fd, stream_pos, client_rx, &server_msg_tx, cursor_rx, cancel);

        // Wait for the next resize (or cancellation or GPU loss) before tearing
        // down.  virtual_monitor must stay alive here — dropping it kills krfb.
        *last_dims = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break None,
                _ = pipeline_restart_watcher.changed() => {
                    log("GPU context lost, restarting capture pipeline");
                    break Some((width, height, index));
                },
                msg = client_rx.recv() => match msg {
                    Ok(ClientDatagram::ResizeDisplay { width, height, .. }) =>
                        break Some((width, height, index)),
                    Ok(ClientDatagram::Keyboard { keycode: _, modifiers: _ }) => {
                        // Handled by the EIS input task in touch.rs
                    }
                    Ok(_) => continue,
                    Err(_) => break None,
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
/// encoder recognises. This is how the server answers a client's PLI and how
/// it enforces a periodic keyframe interval to bound corruption from packet
/// loss — no bitstream-specific logic required.
fn force_keyframe(encoder: &gst::Element) {
    let structure = gst::Structure::new_empty("GstForceKeyUnit");
    let event = gst::event::CustomDownstream::new(structure);
    let _ = encoder.send_event(event);
}
