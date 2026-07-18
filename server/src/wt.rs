use crate::auth::OnetimeToken;
use crate::config::Bytes64;
use crate::error::WebshooterError;
use crate::logging::log;
use crate::pipewire::video;
use bytes::Bytes;
use data_encoding::BASE64URL;
use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, handler};
use shared::client_datagram::ClientDatagram;
use shared::server_datagram;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex, mpsc::Receiver};
use tokio::time;

use salvo::proto::webtransport::server::WebTransportSession;
use salvo_http3::ext::Protocol as H3Protocol;


type Conn = salvo_http3::quinn::Connection;
type WtSession = WebTransportSession<Conn, Bytes>;

// The client sends a KeepAlive datagram every 50 ms. 500 ms gives ~10
// missed keepalives before we consider the peer gone — enough headroom
// for jitter while still detecting a refresh within half a second.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_millis(500);

// WebTransport rides the same QUIC connection as HTTP/3, so there is no
// separate MTU probe. 1200 is the conservative QUIC datagram ceiling; the
// original `wtransport` server used `max_datagram_size().unwrap_or(1200)`.
const MAX_DATAGRAM_SIZE: usize = 1200;

// Only one active capture session is allowed; a new connect aborts the previous.
static ACTIVE: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::const_new(None);

#[handler]
pub async fn connect(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    flow: &mut FlowCtrl,
) {
    // Only a WebTransport CONNECT reaches us here; anything else (ordinary
    // HTTP requests for static assets) falls through to the frontend. We
    // detect it without accepting the session yet, because `web_transport_mut()`
    // consumes the CONNECT stream by sending the 200 response. This mirrors
    // salvo's private `is_wt_connect`, which keys off the `:protocol` pseudo-
    // header (a `Protocol` extension), not a regular header.
    let is_wt_connect = req.method() == salvo::http::Method::CONNECT
        && req
            .extensions()
            .get::<H3Protocol>()
            .map(|p| *p == H3Protocol::WEB_TRANSPORT)
            .unwrap_or(false);
    if !is_wt_connect {
        flow.call_next(req, depot, res).await;
        return;
    }

    // Authenticate the one-time token passed as a query parameter *before*
    // accepting the WebTransport session. If we accepted first and then
    // returned 403 on a bad token, salvo would try to write a second response
    // on the same stream, which the client sees as a QUIC protocol error.
    // Validating first lets salvo send a single clean 403 instead.
    let token = req
        .queries()
        .get("token")
        .and_then(|t| BASE64URL.decode(t.as_bytes()).ok())
        .and_then(|bytes| OnetimeToken::try_from(Bytes64(bytes)).ok());
    let valid = match token {
        Some(t) => t.check().await,
        None => false,
    };
    if !valid {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(WebshooterError::NotAuthorized.to_string());
        return;
    }

    // Token is good — now accept the WebTransport session, which sends the
    // successful CONNECT response and hands us the session handle.
    let _ = req.web_transport_mut().await;
    let Some(session) = req.extensions().get::<Arc<WtSession>>().cloned() else {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(WebshooterError::NotAuthorized.to_string());
        return;
    };

    // Tear down any previous session before starting a new one.
    if let Some(prev) = ACTIVE.lock().await.take() {
        prev.abort();
    }

    // Run the session to completion *inside* this handler. salvo's
    // `process_web_transport` (which runs immediately after the handler
    // returns) must take unique ownership of the `Arc<WebTransportSession>`
    // to restore the underlying QUIC connection. If we spawned `run_session`
    // and returned, the cloned `Arc` we hold here would keep the reference
    // count above one, `take_unique_arc_extension` would fail, and salvo would
    // tear the whole QUIC connection down — the browser surfaces this as a
    // "QUIC protocol error". Awaiting here drops our `Arc` clone before
    // return, leaving the framework's copy uniquely owned. A WebTransport
    // CONNECT legitimately owns its stream for the session's lifetime, so
    // blocking the handler until disconnect is the correct model.
    let handle = tokio::spawn(run_session(session));
    *ACTIVE.lock().await = Some(handle);
    if let Some(handle) = ACTIVE.lock().await.take() {
        let _ = handle.await;
    }
}

async fn run_session(session: Arc<WtSession>) {
    let (broadcaster, _client_rx) = broadcast::channel(256);
    let mut datagrams = tokio::spawn(broadcast_datagrams(session.clone(), broadcaster.clone()));
    let mut unistreams =
        tokio::spawn(broadcast_unistreams(session.clone(), broadcaster.clone()));

    let decoder_caps: Arc<std::sync::Mutex<Option<Vec<shared::codec::Codec>>>> =
        Arc::new(std::sync::Mutex::new(None));
    {
        let mut client_rx = _client_rx.resubscribe();
        let decoder_caps = decoder_caps.clone();
        tokio::spawn(async move {
            loop {
                match client_rx.recv().await {
                    Ok(ClientDatagram::Error { message }) => log(&message),
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
        });
    }

    let mut capture_handle: Option<video::CaptureHandle> = None;
    let mut frame_forwarder_handle: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        // Race start_capture against connection closure so a refresh/disconnect
        // while waiting for the initial resize doesn't leave a zombie capture.
        let result = tokio::select! {
            r = video::capture(_client_rx.resubscribe(), decoder_caps.clone()) => r,
            _ = &mut datagrams  => { log("Datagrams closed");              break; }
            _ = &mut unistreams => { log("Unidirectional streams closed");  break; }
        };
        let (frame_rx, server_msg_rx, handle) = match result {
            Ok(v) => v,
            Err(err) => {
                log(err);
                break;
            }
        };
        capture_handle = Some(handle);
        frame_forwarder_handle =
            Some(frame_forwarder(frame_rx, server_msg_rx, session.clone(), broadcaster.clone()));

        tokio::select! {
            _ = &mut datagrams => { log("Datagrams closed"); break; }
            _ = &mut unistreams => { log("Unidirectional streams closed");  break; }
            _ = frame_forwarder_handle.as_mut().unwrap() => { log("capture pipeline stopped");  break; }
        }
    }

    if let Some(h) = capture_handle {
        h.close().await;
    }

    // Release every clone of the `Arc<WebTransportSession>` before returning.
    // salvo's `process_web_transport` (which runs right after this handler
    // returns) needs unique ownership of the session `Arc`; any surviving
    // clone here would keep the reference count above one and make salvo tear
    // the QUIC connection down (browser sees a "QUIC protocol error"). Abort
    // the reader/forwarder tasks and await them so all `session` clones are
    // dropped while we still own them.
    datagrams.abort();
    unistreams.abort();
    if let Some(ref h) = frame_forwarder_handle {
        h.abort();
    }
    let _ = datagrams.await;
    let _ = unistreams.await;
    if let Some(h) = frame_forwarder_handle {
        let _ = h.await;
    }
}

fn broadcast_unistreams(
    connection: Arc<WtSession>,
    broadcaster: broadcast::Sender<ClientDatagram>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(Some((_, mut stream))) = connection.accept_uni().await {
            let mut vec = Vec::new();
            if tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut vec)
                .await
                .is_ok()
                && let Ok(datagram) = ClientDatagram::from_bytes(&vec)
            {
                let _ = broadcaster.send(datagram);
            }
        }
    })
}

fn broadcast_datagrams(
    connection: Arc<WtSession>,
    broadcaster: broadcast::Sender<ClientDatagram>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = connection.datagram_reader();
        loop {
            match time::timeout(KEEPALIVE_TIMEOUT, reader.read_datagram()).await {
                Ok(Ok(datagram)) => {
                    let bytes = datagram.into_payload();
                    if let Ok(datagram) = ClientDatagram::from_bytes(&bytes) {
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
    wt: Arc<WtSession>,
    _broadcaster: broadcast::Sender<ClientDatagram>,
) -> tokio::task::JoinHandle<()> {
    let payload_size = MAX_DATAGRAM_SIZE
        .saturating_sub(server_datagram::ServerDatagram::header_size())
        .max(1);
    let mut frame_id: u16 = 0;
    let mut sender = wt.datagram_sender();
    tokio::spawn(async move {
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
                        if sender.send_datagram(Bytes::from(dgram)).is_err() {
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
                msg = server_msg_rx.recv() => {
                    let Some(dgram) = msg else { break };
                    let bytes = dgram.to_bytes();
                    if sender.send_datagram(Bytes::from(bytes)).is_err() {
                        log("send_datagram (control) failed: connection closed");
                        break;
                    }
                }
            }
        }
    })
}
