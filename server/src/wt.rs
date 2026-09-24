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
use shared::client_datagram::{ClientDatagram, is_input_byte};
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

/// Maximum rate at which one receive pump will accept *input* datagrams once
/// its burst allowance is spent, per second. Real input (mouse, keyboard,
/// touch, gamepad) never approaches this rate, even from a 1000 Hz gaming
/// mouse plus gamepad simultaneously. It exists to bound the CPU the pump
/// spends parsing and broadcasting a deliberate flood, so a hostile client
/// cannot starve the session's other tasks (frame forwarding, control,
/// capture) into a visible freeze.
const INPUT_FLOOD_RATE_PER_SEC: u32 = 4000;
/// Instantaneous burst of input datagrams allowed through before the rate
/// limit applies, so genuine micro-bursts (a fast flick, a quick key rollover)
/// are forwarded untouched.
const INPUT_FLOOD_BURST: u32 = 32;

/// A token bucket bounding the rate at which *input* datagrams are accepted
/// from the network. One token is consumed per input datagram; tokens refill
/// at [`INPUT_FLOOD_RATE_PER_SEC`] and accumulate up to [`INPUT_FLOOD_BURST`].
/// Per-pump state: each session's `broadcast_datagrams` task owns its own.
struct InputFloodGuard {
    tokens: f64,
    last_refill: std::time::Instant,
}

impl InputFloodGuard {
    fn new() -> Self {
        Self {
            tokens: INPUT_FLOOD_BURST as f64,
            last_refill: std::time::Instant::now(),
        }
    }

    /// Take a token if one is available (refilling first). Returns whether the
    /// caller may accept the next input datagram.
    fn allow_input(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * INPUT_FLOOD_RATE_PER_SEC as f64)
            .min(INPUT_FLOOD_BURST as f64);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub async fn setup_wt(config: Config, identity: Identity) -> Result<()> {
    let server_config = ServerConfig::builder()
        .with_bind_default(config.port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_mins(5)))
        .build();

    let server = Endpoint::server(server_config)?;

    let max_log_level = config.log_level;

    loop {
        let incoming = server.accept().await;

        let (user_id, connection) = match webtransport_auth(incoming).await {
            Ok(pair) => pair,
            Err(err) => {
                log::error!("{err:#?}");
                continue;
            }
        };
        let connection = Arc::new(connection);

        // Resolve the display name and reserve the client id.  The id comes
        // from the registry *before* anything is constructed: capture, audio
        // and resource names all need it.
        let display_name = user_from_id(&user_id)
            .await
            .map(|user| user.display_name)
            .unwrap_or_default();
        let client_id = ipc::next_client_id();

        // Session-scoped channels and state, handed to the tasks below.
        let (control_tx, control_rx) = mpsc::channel::<ServerDatagram>(8);
        let disconnect = CancellationToken::new();
        let (client_tx, client_rx) = broadcast::channel::<ClientDatagram>(256);
        let decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>> = Arc::new(Mutex::new(None));
        let audio_sink: Arc<Mutex<Option<AudioSink>>> = Arc::new(Mutex::new(None));
        let session_name = video::virtual_monitor_name(&display_name, client_id);

        // Every long-lived task of this session is created up-front as a
        // local; the supervisor below takes ownership of them all, so a
        // registered session is always fully built.  The pumps forward the
        // client's transport in and end the moment the peer is gone; the audio
        // task waits for the client's AudioContext; the driver owns capture
        // negotiation and the run loop.
        let mut datagrams = broadcast_datagrams(connection.clone(), client_tx.clone());
        let mut unistreams = broadcast_unistreams(connection.clone(), client_tx.clone());
        let mut client_events = client_events_task(client_rx.resubscribe(), decoder_caps.clone());
        let mut audio = audio_ready_task(
            client_rx.resubscribe(),
            session_name,
            control_tx.clone(),
            disconnect.clone(),
            audio_sink.clone(),
        );

        // Capture negotiation, the frame forwarder and the run loop run in a
        // driver task.  The driver owns the transport and closes it, drops the
        // audio sink and deregisters as its *last* acts.  Pre-clone the values
        // `register_session` also needs: spawning the driver moves the
        // originals into its future.
        let registered_name = display_name.clone();
        let registered_control = control_tx.clone();
        let registered_disconnect = disconnect.clone();
        let driver_token = disconnect.clone();
        let mut driver = tokio::spawn(async move {
            if let Err(err) = run_session(
                client_id,
                display_name,
                control_tx,
                control_rx,
                connection,
                driver_token,
                audio_sink,
                client_rx,
                decoder_caps,
                max_log_level,
            )
            .await
            {
                log::error!("{err:#?}");
            }
        });

        // One supervisor per session: a race over every task that must keep
        // running.  Whichever ends first has ended the session, so the
        // supervisor cancels the session token — waking the driver into its
        // teardown — and deregisters.  The removal is idempotent (the driver
        // removes itself too), so it also covers a driver handle completing
        // without a clean teardown.  `Session` stores only this handle: the
        // session lives exactly as long as its supervisor.
        let supervisor = tokio::spawn(async move {
            tokio::select! {
                _ = &mut datagrams => {}
                _ = &mut unistreams => {}
                _ = &mut client_events => {}
                _ = &mut audio => {}
                _ = &mut driver => {}
            }
            disconnect.cancel();
            ipc::remove_session(client_id);
        });

        // Only once the session struct exists (holding the supervisor) and is
        // registered do we loop back to accept a new connection.
        ipc::register_session(
            client_id,
            registered_name,
            registered_control,
            registered_disconnect,
            supervisor,
        );
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

// Each argument is a distinct session resource with its own owner; the
// alternative to this many arguments is attributing state someone else owns.
#[allow(clippy::too_many_arguments)]
pub async fn run_session(
    client_id: ipc::ClientId,
    display_name: String,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
    control_rx: mpsc::Receiver<ServerDatagram>,
    connection: Arc<Connection>,
    cancel: CancellationToken,
    audio_sink: Arc<Mutex<Option<AudioSink>>>,
    client_rx: broadcast::Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>>,
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

    // Own the application audio sink at the *session* level (not inside the
    // video capture), so video context resets / display resizes never disturb
    // it.  It is created lazily by session startup's audio task once the
    // client's AudioContext starts, named per this session
    // (`webshooter-<user>-<id>-audio-sink`), and torn down when the session
    // ends via the session's disconnect token.

    // Race start_capture against session cancellation so a
    // refresh/disconnect while waiting for the initial resize doesn't leave a
    // zombie capture; a peer closure reaches this via the supervisor (a pump
    // ends, which cancels the session token).  A capture error is logged
    // rather than bailed: every exit still flows through the teardown below.
    let started = tokio::select! {
        r = video::capture(
            client_rx,
            decoder_caps.clone(),
            display_name,
            client_id,
            cancel.clone(),
            server_msg_tx,
        ) => r.map(Some),
        _ = cancel.cancelled() => { log::info!("Disconnect requested"); Ok(None) }
    };
    let started = match started {
        Ok(started) => started,
        Err(err) => {
            log::error!("capture failed: {err:#?}");
            None
        }
    };
    if let Some((frame_rx, _capture_task)) = started {
        let mut frame_forwarder = frame_forwarder(frame_rx, control_rx, connection.clone());
        tokio::select! {
            _ = cancel.cancelled() => { log::info!("Disconnect requested"); }
            _ = &mut frame_forwarder => { log::info!("capture pipeline stopped"); }
        }
    }

    // The single teardown every exit path funnels through: tell the whole
    // session to wind down, close the transport so the pumps error out, drop
    // the application-owned audio sink (unregistering its PipeWire node rather
    // than letting it outlive the session), and only then deregister.  The
    // registry's drop of `Session` is a tripwire, not the mechanism — the
    // cancellation happens first, so `Session::drop` can assert it instead of
    // remediating from the destructor.
    cancel.cancel();
    connection.close(wtransport::VarInt::from_u32(0), b"done");
    *audio_sink.lock().unwrap() = None;
    ipc::remove_session(client_id);
    Ok(())
}

/// Subscribe to the client-message bus: forward client log records into the
/// server log and stash decode caps for the capture pipeline.
fn client_events_task(
    mut client_rx: broadcast::Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
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
                // broadcaster is dropped; recv() then returns Err immediately,
                // so without this break the task would spin at 100% of a core
                // forever (one leaked task per session).
                Err(_) => break,
            }
        }
    })
}

/// Wait for the client's AudioReady signal (with its channel/rate caps), then
/// create the PipeWire audio sink and forward packets to the client's control
/// channel.
fn audio_ready_task(
    mut audio_rx: broadcast::Receiver<ClientDatagram>,
    session_name: String,
    control_tx: mpsc::Sender<ServerDatagram>,
    audio_cancel: CancellationToken,
    audio_sink: Arc<Mutex<Option<AudioSink>>>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
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
                // This task is one of the supervisor's race arms, so its
                // handle completing is taken to mean the session is over.  The
                // audio stage is a one-shot initialisation (create the sink,
                // hand the forwarder its own spawned task), so once the sink
                // exists the task must stay alive until cancelled rather than
                // returning and tripping the supervisor.
                audio_cancel.cancelled().await;
            }
            Err(e) => println!("[audio] audio sink unavailable: {e:#}"),
        }
    })
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
        // Input datagrams are flood-limited before the full parse + broadcast;
        // control datagrams (keepalive, resize, keyframe, decoder caps, …)
        // are always parsed and forwarded.
        let mut flood = InputFloodGuard::new();
        loop {
            match time::timeout(KEEPALIVE_TIMEOUT, connection.receive_datagram()).await {
                Ok(Ok(datagram)) => {
                    // Cheap byte pre-filter: skip floods without paying for the
                    // full `from_bytes` parse. Extra datagrams simply linger in
                    // the (bounded) QUIC receive buffer until dropped.
                    if datagram.first().is_some_and(|&b| is_input_byte(b))
                        && !flood.allow_input()
                    {
                        continue;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn flood_guard_spends_its_burst_then_refuses() {
        let mut guard = InputFloodGuard::new();
        for _ in 0..INPUT_FLOOD_BURST {
            assert!(guard.allow_input(), "burst tokens should be spendable");
        }
        // No time has elapsed: the bucket is empty, further input is refused.
        assert!(!guard.allow_input());
    }

    #[test]
    fn flood_guard_refills_to_the_burst_cap() {
        let mut guard = InputFloodGuard::new();
        for _ in 0..INPUT_FLOOD_BURST {
            assert!(guard.allow_input());
        }
        assert!(!guard.allow_input());
        // Pretend a full second passed: 4000 tokens refill, capped at the burst.
        guard.last_refill -= Duration::from_secs(1);
        let mut allowed = 0;
        while guard.allow_input() {
            allowed += 1;
        }
        assert_eq!(allowed, INPUT_FLOOD_BURST);
    }

    /// Sustained throughput tracks the refill rate, never the attempt rate:
    /// attempting to spend faster than rate/2 over half a second yields
    /// ~rate/2 accepted datagrams (plus whatever the drained burst still held
    /// and a little wall-clock refill).
    #[test]
    fn flood_guard_limits_sustained_rate_over_time() {
        let mut guard = InputFloodGuard::new();
        for _ in 0..INPUT_FLOOD_BURST {
            assert!(guard.allow_input());
        }
        // Simulate half a second in 0.25 ms steps, trying to spend on *every*
        // step (far more than the refill provides).
        let mut allowed = 0u32;
        for _ in 0..2000 {
            guard.last_refill -= Duration::from_micros(250);
            if guard.allow_input() {
                allowed += 1;
            }
        }
        let expected = INPUT_FLOOD_RATE_PER_SEC / 2; // 2000 tokens over 500 ms
        assert!(
            allowed <= expected + INPUT_FLOOD_BURST + 16,
            "must cap sustained input near the refill rate: allowed {allowed}, expected ~{expected}"
        );
        assert!(
            allowed >= expected - 8,
            "must track the refill rate under load: allowed {allowed}, expected ~{expected}"
        );
    }
}
