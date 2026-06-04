use crate::{
    auth::OnetimeToken,
    config::{Bytes64, CaptureType, Config},
    error::WebshooterError,
    get_config,
    logging::log,
    update_config,
};
use anyhow::{Result, anyhow};
use ashpd::{
    desktop::{
        CreateSessionOptions,
        screencast::{
            CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
            StartCastOptions,
        },
    },
    enumflags2::BitFlags,
};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::{os::fd::IntoRawFd, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use wtransport::{Connection, Endpoint, Identity, ServerConfig, VarInt, endpoint::IncomingSession};

/// Server-to-client datagram. The first byte on the wire is the discriminant (u8, up to 256 variants).
///
/// Wire layout per variant:
///   VideoFrame: [0x00][frame_id:u16][frag_idx:u16][num_frags:u16][flags:u8][payload...]
///     flags bit 0 = keyframe (set only on the first fragment of a keyframe frame)
#[repr(u8)]
enum ServerDatagram<'a> {
    VideoFrame {
        frame_id: u16,
        frag_idx: u16,
        num_frags: u16,
        is_keyframe: bool,
        payload: &'a [u8],
    } = 0,
}

impl<'a> ServerDatagram<'a> {
    /// Total header bytes shared by all variants (discriminant only; variant fields are additional).
    const HEADER: usize = 8; // 1 type + 2 frame_id + 2 frag_idx + 2 num_frags + 1 flags

    fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::VideoFrame {
                frame_id,
                frag_idx,
                num_frags,
                is_keyframe,
                payload,
            } => {
                let mut buf = Vec::with_capacity(Self::HEADER + payload.len());
                buf.push(0u8); // discriminant
                buf.extend_from_slice(&frame_id.to_be_bytes());
                buf.extend_from_slice(&frag_idx.to_be_bytes());
                buf.extend_from_slice(&num_frags.to_be_bytes());
                buf.push(*is_keyframe as u8);
                buf.extend_from_slice(payload);
                buf
            }
        }
    }
}

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    for _id in 0.. {
        let session = server.accept().await;
        tokio::spawn(async move {
            let connection = webtransport_auth(session).await?;
            handle_wt_connection(connection)
                .await
                .unwrap_or_else(|err| log(err));
            Ok::<_, anyhow::Error>(())
        });
    }
    Ok(())
}

async fn webtransport_auth(session: IncomingSession) -> Result<Connection> {
    let request = session.await?;
    let token = request
        .path()
        .split_once("?")
        .map(|(_, params)| params.split("&"))
        .and_then(|params| {
            params
                .filter_map(|param| param.split_once("="))
                .find_map(|(k, v)| if k == "token" { Some(v) } else { None })
        })
        .ok_or(WebshooterError::NoAuthentication)?;
    let token = Bytes64::from_str(token)?;
    if OnetimeToken::try_from(token)?.check().await {
        let connection = request.accept().await?;
        Ok(connection)
    } else {
        request.forbidden().await;
        Err(WebshooterError::NotAuthorized.into())
    }
}

pub async fn handle_wt_connection(connection: Connection) -> Result<()> {
    let connection = Arc::new(connection);

    async move {
        if let Ok(_) = connection.receive_datagram().await {
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

            // Shutdown channel: signals from either the connection-close watcher
            // or from a send_datagram failure in the appsink callback.
            let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<String>(1);

            // Watcher task: fires when the QUIC connection is closed by the peer.
            {
                let conn = connection.clone();
                let tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    let err = conn.closed().await;
                    let _ = tx.send(format!("connection closed: {err:#?}")).await;
                });
            }

            let payload_size = connection
                .max_datagram_size()
                .unwrap_or(1200)
                .saturating_sub(ServerDatagram::HEADER)
                .max(1);
            let frame_counter = Arc::new(AtomicU16::new(0));
            let conn = connection.clone();
            let frame_id_ref = frame_counter.clone();

            // Watchdog: if no frame arrives for 5 s the cast was silently stopped
            // (pipewiresrc blocks without posting a bus message).
            let frame_alive = Arc::new(AtomicBool::new(true));
            {
                let flag = frame_alive.clone();
                let tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    let mut misses = 0u32;
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        if flag.swap(false, Ordering::Relaxed) {
                            misses = 0;
                        } else {
                            misses += 1;
                            if misses >= 5 {
                                let _ = tx.send("watchdog: no frames for 5 s".to_string()).await;
                                break;
                            }
                        }
                    }
                });
            }

            let bus_tx = shutdown_tx.clone();
            // shutdown_tx is moved into the new_sample closure; when the pipeline is
            // dropped the sender is dropped too, which closes the channel.
            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        frame_alive.store(true, Ordering::Relaxed);
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let data: &[u8] = &map;
                        let frame_id = frame_id_ref.fetch_add(1, Ordering::Relaxed);
                        let num_frags = data.len().div_ceil(payload_size) as u16;
                        for (idx, chunk) in data.chunks(payload_size).enumerate() {
                            let dgram = ServerDatagram::VideoFrame {
                                frame_id,
                                frag_idx: idx as u16,
                                num_frags,
                                is_keyframe: is_keyframe && idx == 0,
                                payload: chunk,
                            }
                            .to_bytes();
                            if conn.send_datagram(&dgram).is_err() {
                                let _ = shutdown_tx.try_send(
                                    "send_datagram failed: connection closed".to_string(),
                                );
                                return Err(gst::FlowError::Error);
                            }
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

            // Keep the screencast proxy alive — dropping it tears down the portal session.
            let _screencast = screencast;

            pipeline.set_state(gst::State::Playing).map_err(|e| {
                // Pull any detailed GStreamer error off the bus before cleaning up.
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

            // Watch the pipeline bus for EOS or errors from any element
            // (e.g. pipewiresrc posting an error when the cast is stopped).
            // iter_timed is a blocking call, so run it on the blocking thread pool.
            // It exits naturally when the pipeline reaches Null and flushes the bus.
            {
                let bus = pipeline.bus().ok_or(anyhow!("no pipeline bus"))?;
                let tx = bus_tx;
                tokio::task::spawn_blocking(move || {
                    for msg in bus.iter_timed(gst::ClockTime::NONE) {
                        match msg.view() {
                            gst::MessageView::Eos(_) => {
                                let _ = tx.try_send("pipeline EOS".to_string());
                                break;
                            }
                            gst::MessageView::Error(e) => {
                                let _ = tx.try_send(format!(
                                    "pipeline error: {} — {}",
                                    e.error(),
                                    e.debug().unwrap_or_default(),
                                ));
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }

            let reason = shutdown_rx
                .recv()
                .await
                .unwrap_or_else(|| "shutdown channel closed".to_string());
            log(&reason);

            // set_state(Null) blocks until all elements have stopped; run it off
            // the async executor to avoid stalling other tasks.
            let pipeline_stop = pipeline.clone();
            tokio::task::spawn_blocking(move || {
                let _ = pipeline_stop.set_state(gst::State::Null);
            })
            .await?;
            connection.close(VarInt::from_u32(0), b"Aborted");
            session.close().await?;
        }
        Ok(())
    }
    .await
}
