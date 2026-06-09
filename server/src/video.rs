use crate::{
    config::{CaptureSource, CaptureType},
    get_config,
    input::ClientDatagram,
    update_config,
};
use anyhow::{Result, anyhow};
use ashpd::desktop::{
    CreateSessionOptions,
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions,
    },
};
use ashpd::enumflags2::BitFlags;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use libc;
use rand::{RngExt, rngs::ThreadRng};
use std::{
    os::fd::IntoRawFd,
    process::{Child, Command, Stdio},
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
    fn spawn(width: u16, height: u16) -> Result<Self> {
        if is_kwin() {
            use std::os::unix::process::CommandExt;
            let mut cmd = Command::new("krfb-virtualmonitor");
            cmd.args([
                "--resolution",
                &format!("{width}x{height}"),
                "--name",
                "webshooter",
                "--password",
                "x",
                "--port",
                &ThreadRng::default().random::<u16>().to_string(),
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
                let _ = capture(&mut client_rx, frame_tx.clone(), &cancel).await;
            }
        }
    });

    Ok((frame_rx, CaptureHandle { cancel, task }))
}

/// Await `$fut`, but return `Ok(())` immediately if `$cancel` fires first.
macro_rules! or_cancel {
    ($cancel:expr, $fut:expr) => {
        tokio::select! {
            biased;
            _ = $cancel.cancelled() => return Ok(()),
            result = $fut => result,
        }
    };
}

async fn capture(
    client_rx: &mut Receiver<ClientDatagram>,
    frame_tx: mpsc::Sender<EncodedFrame>,
    cancel: &CancellationToken,
) -> Result<()> {
    // On the first iteration we wait for the initial ResizeDisplay.
    // On resize, the new dimensions come from the previous iteration's
    // end-of-loop wait, so we skip the inner wait.
    let mut next_size: Option<(u16, u16)> = None;

    loop {
        // --- Portal / screencast setup ------------------------------------------
        let screencast = or_cancel!(cancel, Screencast::new())?;
        let session = or_cancel!(
            cancel,
            screencast.create_session(CreateSessionOptions::default())
        )?;

        let source_token = get_config()
            .await
            .capture_sources
            .pop()
            .map(|s| s.session_token);

        // Use dimensions carried over from the previous resize, or wait for
        // the first ResizeDisplay from the client.
        let (width, height) = match next_size.take() {
            Some(size) => size,
            None => loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(()),
                    msg = client_rx.recv() => match msg {
                        Ok(ClientDatagram::ResizeDisplay { width, height, .. }) => break (width, height),
                        Ok(_) => continue,
                        Err(_) => return Ok(()),
                    },
                }
            },
        };

        let virtual_monitor = VirtualMonitor::spawn(width, height)?;

        if let VirtualMonitor::ChildProcess(_) = virtual_monitor {
            or_cancel!(cancel, sleep(Duration::from_millis(500)));
        }

        // When krfb-virtualmonitor owns the display it appears as a regular
        // Monitor to the compositor. Only request a portal Virtual source when
        // we don't have our own virtual monitor.
        let source_type = match &virtual_monitor {
            VirtualMonitor::ChildProcess(_) => SourceType::Monitor,
            VirtualMonitor::Portal => SourceType::Virtual,
        };
        or_cancel!(
            cancel,
            screencast.select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_sources(Some(BitFlags::from(source_type)))
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_restore_token(source_token.as_deref())
                    .set_persist_mode(Some(ashpd::desktop::PersistMode::ExplicitlyRevoked)),
            )
        )?;

        let capture = or_cancel!(
            cancel,
            screencast.start(&session, None, StartCastOptions::default())
        )?
        .response()?;

        // Persist the new restore token so future sessions skip the picker.
        if let Some(token) = capture.restore_token() {
            let mut config = get_config().await;
            config.capture_sources = vec![CaptureSource {
                session_token: token.to_string(),
            }];
            update_config(config).await?;
        }

        let stream = capture.streams().first().ok_or(anyhow!("no stream"))?;
        let fd = or_cancel!(
            cancel,
            screencast.open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        )?;
        let node_id = stream.pipe_wire_node_id();
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

        // Wait for the next resize (or cancellation) before tearing down.
        // virtual_monitor must stay alive here — dropping it kills krfb.
        next_size = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break None,
                msg = client_rx.recv() => match msg {
                    Ok(ClientDatagram::ResizeDisplay { width, height, .. }) =>
                        break Some((width, height)),
                    Ok(_) => continue,
                    Err(_) => break None,
                },
            }
        };

        // Tear down the current pipeline and session before starting the next
        // one (or exiting if cancelled).
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
