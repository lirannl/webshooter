use crate::{
    auth::OnetimeToken,
    config::{Bytes64, Config},
    error::WebshooterError,
    logging::log,
    video,
};
use anyhow::Result;
use std::{str::FromStr, sync::Arc, time::Duration};
use wtransport::{Connection, Endpoint, Identity, ServerConfig, VarInt, endpoint::IncomingSession};

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// Server-to-client datagram.
///
/// Wire layout:
///   VideoFrame: [0x00][frame_id:u16 BE][frag_idx:u16 BE][num_frags:u16 BE][flags:u8][payload…]
///     flags bit 0 = keyframe (set only on the first fragment of a keyframe)
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

impl ServerDatagram<'_> {
    /// Fixed header overhead: 1 discriminant + 2 frame_id + 2 frag_idx + 2 num_frags + 1 flags.
    const HEADER: usize = 8;

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
                buf.push(0u8);
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

// ---------------------------------------------------------------------------
// Server entry-point
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

async fn webtransport_auth(session: IncomingSession) -> Result<Connection> {
    let request = session.await?;
    let token = request
        .path()
        .split_once('?')
        .map(|(_, params)| params.split('&'))
        .and_then(|params| {
            params
                .filter_map(|param| param.split_once('='))
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

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

pub async fn handle_wt_connection(connection: Connection) -> Result<()> {
    let connection = Arc::new(connection);

    // Wait for the client's "ready" datagram before starting the capture.
    if connection.receive_datagram().await.is_err() {
        return Ok(());
    }

    let (mut frame_rx, _capture_handle) = video::start_capture().await?;

    let payload_size = connection
        .max_datagram_size()
        .unwrap_or(1200)
        .saturating_sub(ServerDatagram::HEADER)
        .max(1);

    let mut frame_id: u16 = 0;

    loop {
        tokio::select! {
            // Stop if the QUIC connection is closed by the peer.
            _ = connection.closed() => {
                log("WebTransport connection closed by peer");
                break;
            }
            // Forward each encoded frame as one or more datagrams.
            maybe_frame = frame_rx.recv() => {
                let Some(frame) = maybe_frame else {
                    // Pipeline stopped (EOS or error) — close gracefully.
                    log("capture pipeline stopped");
                    break;
                };

                let num_frags = frame.data.len().div_ceil(payload_size) as u16;
                let mut send_ok = true;
                for (idx, chunk) in frame.data.chunks(payload_size).enumerate() {
                    let dgram = ServerDatagram::VideoFrame {
                        frame_id,
                        frag_idx: idx as u16,
                        num_frags,
                        is_keyframe: frame.is_keyframe && idx == 0,
                        payload: chunk,
                    }
                    .to_bytes();
                    if connection.send_datagram(&dgram).is_err() {
                        log("send_datagram failed: connection closed");
                        send_ok = false;
                        break;
                    }
                }
                if !send_ok {
                    break;
                }
                frame_id = frame_id.wrapping_add(1);
            }
        }
    }

    connection.close(VarInt::from_u32(0), b"done");
    // _capture_handle is dropped here, stopping the pipeline and closing the portal session.
    Ok(())
}
