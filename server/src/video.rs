use crate::{
    config::CaptureSource, extensions::CancellationTokenExt, get_config, input::ClientDatagram,
    logging::log, update_config,
};
use anyhow::{Result, anyhow};
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType, Stream,
    },
};
use ashpd::enumflags2::BitFlags;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use libc;
use std::{
    collections::HashSet,
    os::fd::IntoRawFd,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    spawn,
    sync::{
        broadcast::{self, Receiver},
        mpsc,
    },
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
            // Ask the kernel to send SIGTERM to this child if the parent dies,
            // so krfb-virtualmonitor is cleaned up even on an abrupt exit.
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
    pub data: Vec<u8>,
    pub is_keyframe: bool,
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

/// Open the XDG screencast portal, build a GStreamer encode pipeline, and
/// start streaming encoded AV1 frames into the returned channel.
pub async fn start_capture(
    mut client_rx: Receiver<ClientDatagram>,
) -> Result<(mpsc::Receiver<EncodedFrame>, CaptureHandle)> {
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(8);
    let cancel = CancellationToken::new();

    let task = spawn({
        let cancel = cancel.clone();
        async move {
            while !cancel.is_cancelled() {
                if let Err(e) = capture(&mut client_rx, frame_tx.clone(), &cancel).await {
                    log(format!("Capture error: {:#?}", e));
                }
            }
        }
    });

    Ok((frame_rx, CaptureHandle { cancel, task }))
}

async fn capture(
    client_rx: &mut Receiver<ClientDatagram>,
    frame_tx: mpsc::Sender<EncodedFrame>,
    cancel: &CancellationToken,
) -> Result<()> {
    // On the first iteration we wait for the initial ResizeDisplay.
    // On resize, the new dimensions come from the previous iteration's
    // end-of-loop wait, so we skip the inner wait.
    let mut next_size: Option<(u16, u16, u8)> = None;

    loop {
        // --- Portal / screencast setup ------------------------------------------
        // Drive both screen capture and touch emulation from a single
        // RemoteDesktop session so emulated touch events land on the captured
        // stream. RemoteDesktop implements HasScreencastSession, which lets the
        // ScreenCast portal operate on the same session. The session is shared
        // (Arc) with the touch task spawned further down.
        let remote_desktop = Arc::new(cancel.r(RemoteDesktop::new()).await?);
        let screencast = cancel.r(Screencast::new()).await?;
        let session = Arc::new(
            cancel
                .r(remote_desktop.create_session(CreateSessionOptions::default()))
                .await?,
        );

        // Use dimensions carried over from the previous resize, or wait for
        // the first ResizeDisplay from the client.
        let (width, height, index) = match next_size.take() {
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

        let virtual_monitor = VirtualMonitor::spawn(width, height, index)?;

        if let VirtualMonitor::ChildProcess(_) = virtual_monitor {
            cancel
                .run_until_cancelled(sleep(Duration::from_millis(500)))
                .await;
        }

        // When krfb-virtualmonitor owns the display it appears as a regular
        // Monitor to the compositor. Only request a portal Virtual source when
        // we don't have our own virtual monitor.
        let source_type = match &virtual_monitor {
            VirtualMonitor::ChildProcess(_) => SourceType::Monitor,
            VirtualMonitor::Portal => SourceType::Virtual,
        };
        let restore_token = get_config()
            .await
            .capture_sources
            .pop()
            .map(|s| s.session_token);

        cancel
            .r(screencast.select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_multiple(true)
                    .set_sources(Some(BitFlags::from(source_type)))
                    .set_cursor_mode(CursorMode::Embedded),
            ))
            .await?;

        // Request a touchscreen device so we can replay client touch events
        // onto the captured stream. Must be called before select_sources.
        // persist_mode=2 (ExplicitlyRevoked) causes the portal to return a
        // restore_token in the Start response, skipping the picker next time.
        cancel
            .r(remote_desktop.select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(Some(BitFlags::from(DeviceType::Touchscreen)))
                    .set_restore_token(restore_token.as_deref())
                    .set_persist_mode(PersistMode::ExplicitlyRevoked),
            ))
            .await?;

        // Starting the RemoteDesktop session also starts the screen cast that
        // shares it, returning the selected devices and streams together.
        let started = cancel
            .r(remote_desktop.start(&session, None, StartOptions::default()))
            .await?
            .response()?;

        // Persist the new restore token so future sessions skip the picker.
        if let Some(token) = started.restore_token() {
            let mut config = get_config().await;
            config.capture_sources = vec![CaptureSource {
                session_token: token.to_string(),
            }];
            update_config(config).await?;
        }

        let stream = started
            .streams()
            .iter()
            .rfind(sized_stream(&width, &height))
            .ok_or(anyhow!("no stream"))?;
        let node_id = stream.pipe_wire_node_id();
        let fd = cancel
            .r(screencast.open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default()))
            .await?;
        let raw_fd = fd.into_raw_fd();

        // --- GStreamer pipeline --------------------------------------------------

        gst::init()?;
        let bitrate = 7000;
        let pipeline = gst::parse::launch(&format!(
            "pipewiresrc fd={raw_fd} path={node_id} \
         ! videoconvert \
         ! vaav1enc rate-control=vbr bitrate={bitrate} target-percentage=75 \
         ! appsink name=sink sync=false"
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;

        let appsink = pipeline
            .by_name("sink")
            .ok_or(anyhow!("no sink element"))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("not an appsink"))?;

        // Push encoded frames into the channel from the appsink callback.
        // The closure holds the only sender; when the pipeline stops the
        // appsink is torn down, the sender is dropped, and the channel closes.
        let tx = frame_tx.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let frame = EncodedFrame {
                        data: map.to_vec(),
                        is_keyframe,
                    };
                    // try_send so the sync callback never blocks; drop the frame if
                    // the consumer is too slow rather than stalling the pipeline.
                    if tx.try_send(frame).is_err() {
                        return Err(gst::FlowError::Error);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Watch the bus for EOS / errors from any element (e.g. pipewiresrc
        // posting an error when the cast is stopped externally).
        // iter_timed blocks, so run it on the blocking thread pool.
        // When we break out the task ends and its clone of pipeline is dropped,
        // which eventually causes the appsink sender to be dropped too.
        {
            let bus = pipeline.bus().ok_or(anyhow!("no pipeline bus"))?;
            let pipeline_ref = pipeline.clone();
            tokio::task::spawn_blocking(move || {
                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    match msg.view() {
                        gst::MessageView::Eos(_) | gst::MessageView::Error(_) => {
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

        // Replay client touch events onto the captured stream. Touch datagrams
        // arrive over the same broadcast channel as resize events, so we take
        // an independent receiver and run this alongside the resize wait below.
        // The task is tied to this session and is aborted on teardown.
        let touch_task = spawn({
            let remote_desktop = remote_desktop.clone();
            let session = session.clone();
            let cancel = cancel.clone();
            let mut touch_rx = client_rx.resubscribe();
            async move {
                // Slots that are currently pressed. The first event for a slot
                // is a touch-down; subsequent events are motion, as the portal
                // expects.
                let mut active_slots: HashSet<u8> = Default::default();
                loop {
                    let msg = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        msg = touch_rx.recv() => msg,
                        _ = sleep(Duration::from_secs(1)) => Ok(ClientDatagram::TouchscreenRelease { index: None })
                    };
                    match msg {
                        Ok(ClientDatagram::Touchscreen { x, y, index }) => {
                            let (x, y) = (x as f64, y as f64);
                            let slot = index as u32;
                            let result = if active_slots.insert(index) {
                                remote_desktop
                                    .notify_touch_down(
                                        &session,
                                        node_id,
                                        slot,
                                        x,
                                        y,
                                        Default::default(),
                                    )
                                    .await
                            } else {
                                remote_desktop
                                    .notify_touch_motion(
                                        &session,
                                        node_id,
                                        slot,
                                        x,
                                        y,
                                        Default::default(),
                                    )
                                    .await
                            };
                            if let Err(e) = result {
                                log(format!("touch down/motion failed: {e}"));
                            }
                        }
                        Ok(ClientDatagram::TouchscreenRelease { index }) => {
                            let indices: Vec<u8> = if let Some(index) = index {
                                if active_slots.remove(&index) {
                                    vec![index]
                                } else {
                                    vec![]
                                }
                            } else {
                                active_slots.drain().collect()
                            };
                            for index in indices {
                                if let Err(e) = remote_desktop
                                    .notify_touch_up(&session, index as u32, Default::default())
                                    .await
                                {
                                    log(format!("touch up failed: {e}"));
                                }
                            }
                        }
                        // Unrelated datagrams (resize, keyboard, keepalive).
                        Ok(_) => continue,
                        // Dropped messages under load — keep going.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        // Sender gone — nothing left to emulate.
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });

        // Wait for the next resize (or cancellation) before tearing down.
        // virtual_monitor must stay alive here — dropping it kills krfb.
        next_size = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break None,
                msg = client_rx.recv() => match msg {
                    Ok(ClientDatagram::ResizeDisplay { width, height, .. }) =>
                        break Some((width, height, index)),
                    Ok(_) => continue,
                    Err(_) => break None,
                },
            }
        };

        // Tear down the current pipeline and session before starting the next
        // one (or exiting if cancelled). The touch task borrows this session, so
        // stop it first.
        touch_task.abort();
        tokio::task::spawn_blocking({
            let pipeline = pipeline.clone();
            move || {
                let _ = pipeline.set_state(gst::State::Null);
            }
        })
        .await?;
        let _ = session.close().await;
        // virtual_monitor dropped here — kills the old krfb process.

        if next_size.is_none() {
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
