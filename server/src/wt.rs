use crate::{
    auth::{OnetimeToken, UserId, user_from_id},
    config::{Bytes64, Config},
    error::WebshooterError,
    ipc,
    pipewire::audio::{AudioSink, forward_audio, start_audio_sink},
    pipewire::video,
};
use tokio_util::sync::CancellationToken;
use anyhow::Result;
use log::LevelFilter;
use shared::client_datagram::ClientDatagram;
use shared::server_datagram;
use shared::server_datagram::ServerDatagram;
use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    spawn,
    sync::{broadcast, mpsc, mpsc::Receiver},
    time::{self},
};
use wtransport::{Connection, Endpoint, Identity, ServerConfig, endpoint::IncomingSession};

// The client sends
//  a KeepAlive datagram every 50 ms. 500 ms gives ~10
// missed keepalives before we consider the peer gone — enough headroom
// for jitter while still detecting a refresh within half a second.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_millis(500);

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    let max_log_level = config.log_level;

    loop {
        let session = server.accept().await;

        let (user_id, connection) = match webtransport_auth(session).await {
            Ok(pair) => pair,
            Err(err) => {
                log::error!("{err:#?}");
                continue;
            }
        };
        let connection = Arc::new(connection);

        // Resolve the display name and register the session into the collection
        // *before* looping back to accept the next connection, so a session is
        // always addressable the moment another one is taken.  The capture and
        // forwarding work is then spawned and runs concurrently.
        let display_name = user_from_id(&user_id)
            .await
            .map(|user| user.display_name)
            .unwrap_or_default();
        let (control_tx, control_rx) = mpsc::channel::<ServerDatagram>(8);
        let disconnect = CancellationToken::new();
        let session = ipc::register_session(
            display_name,
            control_tx,
            disconnect,
            connection.clone(),
        );
        let client_id = session.id;

        tokio::spawn(async move {
            if let Err(err) =
                run_session(session, connection, client_id, control_rx, max_log_level).await
            {
                log::error!("{err:#?}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

async fn webtransport_auth(session: IncomingSession) -> Result<(UserId, Connection)> {
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
    if let Some(user_id) = OnetimeToken::try_from(token)?.check().await {
        let connection = request.accept().await?;
        Ok((user_id, connection))
    } else {
        request.forbidden().await;
        Err(WebshooterError::NotAuthorized.into())
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

pub async fn run_session(
    session: Arc<ipc::Session>,
    connection: Arc<Connection>,
    client_id: ipc::ClientId,
    control_rx: mpsc::Receiver<ServerDatagram>,
    max_log_level: LevelFilter,
) -> Result<()> {
    // Tell the client how verbose we are so it stops generating records we
    // would discard anyway.
    let _ = connection.send_datagram(
        &shared::server_datagram::ServerDatagram::LogLevel {
            level: max_log_level,
        }
        .to_bytes(),
    );

    let (_broadcaster, _client_rx) = broadcast::channel(256);
    let mut datagrams = broadcast_datagrams(connection.clone(), _broadcaster.clone());
    let mut unistreams = broadcast_unistreams(connection.clone(), _broadcaster.clone());

    let decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>> = Arc::new(Mutex::new(None));
    {
        let mut client_rx = _client_rx.resubscribe();
        let decoder_caps = decoder_caps.clone();
        session.add_task(spawn(async move {
            loop {
                match client_rx.recv().await {
                    Ok(ClientDatagram::Error { level, message }) => {
                        log::log!(target: "webshooter::client", level, "{message}");
                    }
                    Ok(ClientDatagram::DecoderCapabilities { decoders }) => {
                        *decoder_caps.lock().unwrap() = Some(decoders);
                    }
                    Ok(_) => {}
                    // The broadcast channel is closed once the connection's
                    // broadcaster is dropped; recv() then returns Err
                    // immediately, so without this break the task would spin
                    // at 100% of a core forever (one leaked task per session).
                    Err(_) => break,
                }
            }
        }));
    }

    // Own the application audio sink at the *session* level (not inside the
    // video capture), so video context resets / display resizes never disturb
    // it.  It is created lazily once the client's AudioContext starts, named
    // per this session (`webshooter-<user>-<id>-audio-sink`), and torn down
    // when the session ends via the session's disconnect token.
    let session_name = video::virtual_monitor_name(&session.display_name, client_id);
    let audio_sink: Arc<Mutex<Option<AudioSink>>> = Arc::new(Mutex::new(None));
    {
        let mut audio_rx = _client_rx.resubscribe();
        let audio_sink = audio_sink.clone();
        let control_tx = session.control_tx();
        let session_name = session_name.clone();
        let audio_cancel = session.disconnect();
        session.add_task(spawn(async move {
            // Wait for the client's AudioReady signal (with its channel/rate
            // caps), or for cancellation.
            let recv = async {
                loop {
                    match audio_rx.recv().await {
                        Ok(ClientDatagram::AudioReady { channels, rate }) => {
                            return Some((channels, rate));
                        }
                        Ok(_) => continue,
                        Err(_) => return None,
                    }
                }
            };
            let cancel_fut = audio_cancel.cancelled();
            tokio::pin!(recv);
            tokio::pin!(cancel_fut);
            let ready = tokio::select! {
                r = &mut recv => r,
                _ = &mut cancel_fut => None,
            };
            let Some((channels, rate)) = ready else {
                return;
            };
            if audio_cancel.is_cancelled() {
                return;
            }
            match start_audio_sink(audio_cancel.clone(), session_name, channels, rate).await {
                Ok((sink, rx)) => {
                    println!("[audio] client ready — created PipeWire audio sink");
                    spawn(async move {
                        forward_audio(rx, control_tx).await;
                    });
                    *audio_sink.lock().unwrap() = Some(sink);
                }
                Err(e) => println!("[audio] audio sink unavailable: {e:#}"),
            }
        }));
    }

    // Race start_capture against connection closure so a refresh/disconnect
    // while waiting for the initial resize doesn't leave a zombie capture.
    let cancel = session.disconnect();
    let started = tokio::select! {
        r = video::capture(
            _client_rx.resubscribe(),
            decoder_caps.clone(),
            session.display_name.clone(),
            client_id,
            cancel.clone(),
            session.control_tx(),
        ) => r.map(Some)?,
        _ = cancel.cancelled() => { log::info!("Disconnect requested"); None }
        _ = &mut datagrams  => { log::info!("Datagrams closed");              None }
        _ = &mut unistreams => { log::info!("Unidirectional streams closed");  None }
        _ = connection.closed() => { log::info!("WebTransport connection closed by peer"); None }
    };
    let Some((frame_rx, capture_task)) = started else {
        // Connection went away before a capture could start; removing the entry
        // drops the registry's Arc and tears the session down.  Drop any audio
        // sink that was being created so its PipeWire node doesn't outlive the
        // session.
        *audio_sink.lock().unwrap() = None;
        ipc::remove_session(client_id);
        return Ok(());
    };
    session.add_task(capture_task);
    let mut frame_forwarder = frame_forwarder(frame_rx, control_rx, connection.clone());

    tokio::select! {
        _ = cancel.cancelled() => { log::info!("Disconnect requested"); }
        _ = &mut datagrams => { log::info!("Datagrams closed"); }
        _ = &mut unistreams => { log::info!("Unidirectional streams closed");  }
        _ = &mut frame_forwarder => { log::info!("capture pipeline stopped");  }
        _ = connection.closed() => { log::info!("WebTransport connection closed by peer"); }
    }

    session.add_task(datagrams);
    session.add_task(unistreams);
    session.add_task(frame_forwarder);
    // Tear down the application-owned audio sink before deregistering the
    // session: dropping it stops the capture pipeline and joins the sink's
    // background thread so the `object.linger=false` PipeWire node is removed,
    // rather than persisting after the session that owned it has ended.
    *audio_sink.lock().unwrap() = None;
    ipc::remove_session(client_id);
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
    mut server_msg_rx: mpsc::Receiver<shared::server_datagram::ServerDatagram>,
    wt: Arc<Connection>,
) -> tokio::task::JoinHandle<()> {
    let payload_size = wt
        .max_datagram_size()
        .unwrap_or(1200)
        .saturating_sub(server_datagram::ServerDatagram::header_size())
        .max(1);
    let mut frame_id: u16 = 0;
    spawn(async move {
        loop {
            tokio::select! {
                biased;
                frame = frame_rx.recv() => {
                    let Some(frame) = frame else { break };
                    let mapped = match frame.data.map_readable() {
                        Ok(m) => m,
                        Err(_) => {
                            frame_id = frame_id.wrapping_add(1);
                            continue;
                        }
                    };
                    let data = mapped.as_slice();
                    let num_frags = data.len().div_ceil(payload_size) as u16;
                    let mut send_ok = true;
                    for (idx, chunk) in data.chunks(payload_size).enumerate() {
                        let dgram = server_datagram::ServerDatagram::video_frame_to_bytes(
                            frame_id,
                            idx as u16,
                            num_frags,
                            frame.is_keyframe && idx == 0,
                            frame.codec,
                            chunk,
                        );
                        if wt.send_datagram(&dgram).is_err() {
                            log::warn!("send_datagram failed: connection closed");
                            send_ok = false;
                            break;
                        }
                    }
                    if !send_ok {
                        break;
                    }
                    frame_id = frame_id.wrapping_add(1);
                }
                msg = server_msg_rx.recv() => {
                    let Some(dgram) = msg else { break };
                    let bytes = dgram.to_bytes();
                    if wt.send_datagram(&bytes).is_err() {
                        log::warn!("send_datagram (control) failed: connection closed");
                        break;
                    }
                }
            }
        }
    })
}
