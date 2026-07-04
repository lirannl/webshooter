use crate::{
    extensions::CancellationTokenExt, logging::log, pipewire::touch::touch_task,
    portal_auth::{accept_dialog, PortalAuthKb},
};
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
use libc;
use shared::client_datagram::ClientDatagram;
use std::{
    os::fd::IntoRawFd,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    spawn,
    sync::{broadcast::Receiver, mpsc, Mutex as AsyncMutex},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;

// Virtual keyboard created and authorised once at application startup.
static KB: OnceLock<AsyncMutex<Option<PortalAuthKb>>> = OnceLock::new();

// Portal restore token: returned by start(), fed back to select_devices /
// select_sources on the next capture so the portal skips the dialog.
static RESTORE_TOKEN: Mutex<Option<String>> = Mutex::new(None);

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

/// Initialise the portal-authorisation keyboard and grant portal permission
/// once at application startup.  Must be called before any `capture()`.
#[cfg(target_os = "linux")]
pub async fn init_portal_auth() {
    let mut kb = PortalAuthKb::new("Webshooter Portal Authorisation");
    if kb.is_some() {
        let cancel = CancellationToken::new();
        grant_permission_once(&mut kb, &cancel).await;
    }
    let _ = KB.set(AsyncMutex::new(kb));
}

#[cfg(not(target_os = "linux"))]
pub async fn init_portal_auth() {
    let _ = KB.set(AsyncMutex::new(None));
}

/// Open the XDG screencast portal, build a GStreamer encode pipeline, and
/// start streaming encoded AV1 frames into the returned channel.
pub async fn capture(
    mut client_rx: Receiver<ClientDatagram>,
) -> Result<(mpsc::Receiver<EncodedFrame>, CaptureHandle)> {
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(8);
    let cancel = CancellationToken::new();

    let task = spawn({
        let cancel = cancel.clone();
        async move {
            while !cancel.is_cancelled() {
                if let Err(e) =
                    single_capture(&mut client_rx, frame_tx.clone(), &cancel).await
                {
                    log(format!("Capture error: {:#?}", e));
                }
            }
        }
    });

    Ok((frame_rx, CaptureHandle { cancel, task }))
}

/// Do the full portal permission flow once (select_devices, select_sources,
/// start) using the keyboard to auto-accept any dialogs.  The portal should
/// cache the grant so future sessions skip the dialogs.
async fn grant_permission_once(
    kb: &mut Option<PortalAuthKb>,
    cancel: &CancellationToken,
) {
    let remote_desktop = match cancel.r(RemoteDesktop::new()).await {
        Ok(rd) => Arc::new(rd),
        Err(e) => {
            log(format!("grant_permission: RemoteDesktop::new failed: {e:#}"));
            return;
        }
    };
    let screencast = match cancel.r(Screencast::new()).await {
        Ok(sc) => sc,
        Err(e) => {
            log(format!("grant_permission: Screencast::new failed: {e:#}"));
            return;
        }
    };
    let session = match cancel
        .r(remote_desktop.create_session(CreateSessionOptions::default()))
        .await
    {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log(format!("grant_permission: create_session failed: {e:#}"));
            return;
        }
    };

    let source_type = SourceType::Monitor;
    if let Err(e) = accept_dialog(kb, cancel.r(remote_desktop.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(Some(BitFlags::from(DeviceType::Touchscreen)))
            .set_persist_mode(PersistMode::ExplicitlyRevoked),
    )))
    .await
    {
        log(format!("grant_permission: select_devices failed: {e:#}"));
    }
    if let Err(e) = accept_dialog(kb, cancel.r(screencast.select_sources(
        &session,
        SelectSourcesOptions::default()
            .set_multiple(true)
            .set_sources(Some(BitFlags::from(source_type)))
            .set_cursor_mode(CursorMode::Embedded),
    )))
    .await
    {
        log(format!("grant_permission: select_sources failed: {e:#}"));
    }
    if let Ok(req) = accept_dialog(kb, cancel.r(remote_desktop.start(
        &session,
        None,
        StartOptions::default(),
    )))
    .await
        && let Ok(started) = req.response()
    {
        if let Some(token) = started.restore_token() {
            *RESTORE_TOKEN.lock().unwrap() = Some(token.to_owned());
            println!("[video] grant_permission: stored restore_token");
        }
    }
    let _ = session.close().await;
}

async fn single_capture(
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

        // Swap the previous restore_token into the portal calls so the portal
        // can skip the selection dialogs.  After start() a fresh token is
        // stored for the next iteration.
        let prev_token = RESTORE_TOKEN.lock().unwrap().take();

        let mut kb_guard = KB.get().expect("init_portal_auth must be called first").lock().await;
        let kb = &mut *kb_guard;
        accept_dialog(kb, cancel.r(remote_desktop.select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(Some(BitFlags::from(DeviceType::Touchscreen)))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(prev_token.as_deref()),
        )))
        .await?;

        accept_dialog(kb, cancel.r(screencast.select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_multiple(true)
                .set_sources(Some(BitFlags::from(source_type)))
                .set_cursor_mode(CursorMode::Embedded)
                .set_restore_token(prev_token.as_deref()),
        )))
        .await?;

        // Starting the RemoteDesktop session also starts the screen cast that
        // shares it, returning the selected devices and streams together.
        let request = accept_dialog(
            kb,
            cancel.r(remote_desktop.start(&session, None, StartOptions::default())),
        )
        .await?;
        drop(kb_guard);
        let started = request.response()?;

        // Store the new restore_token for the next capture iteration.
        if let Some(token) = started.restore_token() {
            *RESTORE_TOKEN.lock().unwrap() = Some(token.to_owned());
        }

        let stream = started
            .streams()
            .iter()
            .rfind(sized_stream(&width, &height))
            .ok_or(anyhow!("no stream"))?;
        let node_id = stream.pipe_wire_node_id();
        // Offset for translating stream-relative touch coordinates into the
        // compositor coordinate space that libei expects.
        let stream_pos = stream.position().unwrap_or((0, 0));
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

        // Watch the bus for EOS / errors from any element.
        // Detect GPU context loss (AMD GPU hard recovery) and signal for fast restart.
        let (pipeline_restart, mut pipeline_restart_watcher) = tokio::sync::watch::channel(());
        let pipeline_restart = Arc::new(pipeline_restart);
        {
            let bus = pipeline.bus().ok_or(anyhow!("no pipeline bus"))?;
            let pipeline_ref = pipeline.clone();
            let pipeline_restart = pipeline_restart.clone();
            tokio::task::spawn_blocking(move || {
                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    match msg.view() {
                        gst::MessageView::Eos(_) | gst::MessageView::Error(_) => {
                            if let gst::MessageView::Error(err) = msg.view() {
                                let err_str = format!(
                                    "{} — {}",
                                    err.error(),
                                    err.debug().unwrap_or_default()
                                );
                                // Detect AMD GPU context loss / hard recovery
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
        let eis_fd = cancel
            .r(remote_desktop.connect_to_eis(&session, ConnectToEISOptions::default()))
            .await?;

        // Replay client touch events onto the captured stream. Touch datagrams
        // arrive over the same broadcast channel as resize events, so we take
        // an independent receiver and run this alongside the resize wait below.
        // The task is tied to this session and is aborted on teardown.
        let touch_task = touch_task(eis_fd, stream_pos, client_rx, cancel);

        // Wait for the next resize (or cancellation or GPU loss) before tearing down.
        // virtual_monitor must stay alive here — dropping it kills krfb.
        next_size = loop {
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
