use crate::{config::CaptureType, get_config, update_config};
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
use std::{os::fd::IntoRawFd, sync::Arc};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Logical fields of a video frame, before wire packing.
pub struct VideoFrameVariant<'a> {
    pub frame_id: u16,
    pub frag_idx: u16,
    pub num_frags: u16,
    pub is_keyframe: bool,
    pub payload: &'a [u8],
}

impl VideoFrameVariant<'_> {
    pub const fn header_size() -> usize {
        6
    }
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

/// Keeps the screencast portal proxy and GStreamer pipeline alive.
/// Stopping the pipeline when this is dropped is handled by [`CaptureHandle::stop`];
/// call that explicitly so the async session close can be awaited.
pub struct CaptureHandle {
    pipeline: gst::Pipeline,
    // Keep these alive — dropping either tears down the portal session.
    _screencast: Screencast,
    session: ashpd::desktop::Session<Screencast>,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Block the current thread on the async portal session close.
            // We use block_in_place so Tokio can move other tasks off this
            // thread while we wait, avoiding a deadlock on a single-threaded runtime.
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    let _ = self.session.close().await;
                });
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Open the XDG screencast portal, build a GStreamer encode pipeline, and
/// start streaming encoded AV1 frames into the returned channel.
///
/// The channel closes naturally when the pipeline reaches EOS or errors out.
/// Drop (or call `.stop()` on) the returned [`CaptureHandle`] to tear everything down.
pub async fn start_capture() -> Result<(mpsc::Receiver<EncodedFrame>, CaptureHandle)> {
    // --- Portal / screencast setup ------------------------------------------

    let screencast = Screencast::new().await?;
    let session = screencast
        .create_session(CreateSessionOptions::default())
        .await?;

    let virt_source_token = get_config()
        .await
        .capture_sources
        .into_iter()
        .find(|s| s.type_ == CaptureType::Virtual)
        .map(|s| s.session_token);

    let mut bitflags = BitFlags::empty();
    bitflags.insert(SourceType::Virtual);
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_restore_token(virt_source_token.as_deref())
                .set_persist_mode(Some(ashpd::desktop::PersistMode::ExplicitlyRevoked))
                .set_sources(bitflags),
        )
        .await?;

    let capture = screencast
        .start(&session, None, StartCastOptions::default())
        .await?
        .response()?;

    // Persist the new restore token so future sessions skip the picker.
    if let Some(token) = capture.restore_token() {
        let mut config = get_config().await;
        if let Some(source) = config
            .capture_sources
            .iter_mut()
            .find(|s| s.type_ == CaptureType::Virtual)
        {
            source.session_token = token.to_string();
            update_config(config).await?;
        }
    }

    let stream = capture.streams().first().ok_or(anyhow!("no stream"))?;
    let fd = screencast
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await?;
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

    // Bounded channel; if the consumer falls behind, back-pressure is applied.
    let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(8);

    // Push encoded frames into the channel from the appsink callback.
    // The closure holds the only sender; when the pipeline stops the
    // appsink is torn down, the sender is dropped, and the channel closes.
    let tx = Arc::new(frame_tx);
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

    let handle = CaptureHandle {
        pipeline,
        _screencast: screencast,
        session,
    };

    Ok((frame_rx, handle))
}
