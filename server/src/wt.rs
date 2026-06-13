use crate::{
    auth::OnetimeToken,
    client_datagram::ClientDatagram,
    config::{Bytes64, Config},
    error::WebshooterError,
    logging::log,
    pipewire::video,
    server_datagram,
};
use anyhow::Result;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    spawn,
    sync::{broadcast, mpsc::Receiver},
    time::{self},
};
use wtransport::{Connection, Endpoint, Identity, ServerConfig, VarInt, endpoint::IncomingSession};

// The client sends a KeepAlive datagram every 50 ms. 500 ms gives ~10
// missed keepalives before we consider the peer gone — enough headroom
// for jitter while still detecting a refresh within half a second.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_millis(500);

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.http_config.port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    let mut active: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        let session = server.accept().await;

        // A new client arrived — tear down any existing session immediately
        // rather than waiting for the QUIC idle timeout to fire.
        if let Some(prev) = active.take() {
            prev.abort();
        }

        active = Some(tokio::spawn(async move {
            match webtransport_auth(session).await {
                Ok(connection) => handle_wt_connection(connection)
                    .await
                    .unwrap_or_else(|err| log(err)),
                Err(err) => log(err),
            }
        }));
    }
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
    let _connection = Arc::new(connection);

    let (_broadcaster, _client_rx) = broadcast::channel(256);
    let mut datagrams = broadcast_datagrams(_connection.clone(), _broadcaster.clone());
    let mut unistreams = broadcast_unistreams(_connection.clone(), _broadcaster.clone());

    let mut capture_handle: Option<video::CaptureHandle> = None;

    loop {
        // Race start_capture against connection closure so a refresh/disconnect
        // while waiting for the initial resize doesn't leave a zombie capture.
        let (frame_rx, handle) = tokio::select! {
            r = video::capture(_client_rx.resubscribe()) => r?,
            _ = &mut datagrams  => { log("Datagrams closed");              break; }
            _ = &mut unistreams => { log("Unidirectional streams closed");  break; }
            _ = _connection.closed() => { log("WebTransport connection closed by peer"); break; }
        };
        capture_handle = Some(handle);
        let frame_forwarder = frame_forwarder(frame_rx, _connection.clone());

        let mut client_rx = _client_rx.resubscribe();
        let keyboard_receiver = spawn(async move {
            loop {
                match client_rx.recv().await {
                    Ok(ClientDatagram::Keyboard { keycode, modifiers }) => {
                        log(format!("keycode: {keycode}, modifiers: {modifiers:?}"));
                    }
                    _ => {}
                }
            }
        });

        tokio::select! {
            _ = datagrams => { log("Datagrams closed"); break; }
            _ = unistreams => { log("Unidirectional streams closed");  break; }
            _ = keyboard_receiver => { break; }
            _ = frame_forwarder => { log("capture pipeline stopped");  break; }
            _ = _connection.closed() => { log("WebTransport connection closed by peer"); break; }
        }
    }

    if let Some(h) = capture_handle {
        h.close().await;
    }
    _connection.close(VarInt::from_u32(0), b"done");
    Ok(())
}

fn broadcast_unistreams(
    connection_clone: Arc<Connection>,
    broadcaster_clone: broadcast::Sender<ClientDatagram>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
        while let Ok(mut stream) = connection_clone.accept_uni().await {
            let mut vec = Vec::new();
            if stream.read_to_end(&mut vec).await.is_ok()
                && let Ok(datagram) = ClientDatagram::from_bytes(&vec)
            {
                let _ = broadcaster_clone.send(datagram);
            }
        }
    })
}

fn broadcast_datagrams(
    connection: Arc<Connection>,
    broadcaster: broadcast::Sender<ClientDatagram>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
        loop {
            match time::timeout(KEEPALIVE_TIMEOUT, connection.receive_datagram()).await {
                Ok(Ok(datagram)) => {
                    if let Ok(datagram) = ClientDatagram::from_bytes(&datagram) {
                        let _ = broadcaster.send(datagram);
                    }
                }
                // Timed out or connection error — peer is gone.
                Ok(Err(_)) | Err(_) => break,
            }
        }
    })
}

fn frame_forwarder(
    mut frame_rx: Receiver<video::EncodedFrame>,
    wt: Arc<Connection>,
) -> tokio::task::JoinHandle<()> {
    let payload_size = wt
        .max_datagram_size()
        .unwrap_or(1200)
        .saturating_sub(server_datagram::ServerDatagram::HEADER)
        .max(1);
    let mut frame_id: u16 = 0;
    spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            let num_frags = frame.data.len().div_ceil(payload_size) as u16;
            let mut send_ok = true;
            for (idx, chunk) in frame.data.chunks(payload_size).enumerate() {
                let dgram = server_datagram::ServerDatagram::VideoFrame {
                    frame_id,
                    frag_idx: idx as u16,
                    num_frags,
                    is_keyframe: frame.is_keyframe && idx == 0,
                    payload: chunk,
                }
                .to_bytes();
                if wt.send_datagram(&dgram).is_err() {
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
    })
}
