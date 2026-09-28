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
    collections::VecDeque,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    spawn,
    sync::{broadcast, mpsc, mpsc::Receiver, watch},
    time::{self},
};
use tokio::sync::broadcast::error::RecvError;
use wtransport::{Connection, Endpoint, Identity, ServerConfig, endpoint::IncomingSession};

/// How long the server waits for a client datagram before deciding the peer is
/// gone. The client sends a keepalive every 50 ms, so this is roughly sixty
/// missed keepalives.
///
/// Keepalives are unreliable datagrams, and the link they travel over is not
/// necessarily a LAN. A mobile one silences the client for longer than any
/// LAN-scale timeout tolerates, for reasons that have nothing to do with the
/// client being alive: a radio handover, an uplink queue that filled under
/// congestion, or a burst of correlated loss. "Ten missed keepalives" was never
/// ten independent chances — loss arrives in runs, and on a bad link a run
/// long enough to matter is the normal case rather than a tail.
///
/// A false positive here costs the user their whole session, and it buys very
/// little: a peer that really is gone is detected by QUIC's own idle timeout
/// regardless, and the WebTransport session dies with it either way. So this
/// only has to be generous enough not to mistake a slow link for a dead one.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(3);

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
        // task (not raced) waits for the client's AudioContext; the driver owns
        // capture negotiation and the run loop.
        let mut datagrams = broadcast_datagrams(connection.clone(), client_tx.clone());
        let mut unistreams = broadcast_unistreams(connection.clone(), client_tx.clone());
        let mut client_events = client_events_task(client_rx.resubscribe(), decoder_caps.clone());
        // Not a supervisor arm — see `audio_ready_task`.
        audio_ready_task(
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
            // Name the arm that ended the session. Which task finished first is
            // the whole diagnosis of an unexpected disconnect — "the client went
            // away" and "the server gave up on the client" look identical from
            // the outside otherwise. Every arm here ends only because something
            // genuinely failed, so the name is trustworthy; the audio arm is
            // absent for exactly that reason (see `audio_ready_task`).
            let ended = tokio::select! {
                _ = &mut datagrams => "datagram pump",
                _ = &mut unistreams => "client unistream pump",
                _ = &mut client_events => "client event pump",
                _ = &mut driver => "driver",
            };
            log::info!("session {client_id} ended: {ended} finished first");
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

    // Taken before `capture` moves the original: the resend pump answers repair
    // requests, which are a client-to-server message like any other, so it needs
    // its own view of the bus rather than a borrow of the one capture owns.
    let resend_rx = client_rx.resubscribe();

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
        // Shared with the forwarder, which fills it, and with the resend task,
        // which reads it. A repair request can only be answered while the frame
        // is still here, so this is the one piece of state that has to be visible
        // to both sides of the request.
        let delta_ring = Arc::new(Mutex::new(DeltaRing::new()));
        let mut frame_forwarder =
            frame_forwarder(frame_rx, control_rx, connection.clone(), delta_ring.clone());
        let mut resends = resend_task(resend_rx, connection.clone(), delta_ring);
        tokio::select! {
            _ = cancel.cancelled() => { log::info!("Disconnect requested"); }
            _ = &mut frame_forwarder => { log::info!("capture pipeline stopped"); }
            _ = &mut resends => { log::info!("resend pump ended"); }
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
                // `Lagged` only means this consumer fell behind the bus, which
                // is expected whenever a capture path is parked on a portal
                // dialog: the dropped records are superseded by the next state
                // message, so it must not end the session. `Closed` means the
                // connection's broadcaster is gone; recv() then returns
                // immediately, so without this break the task would spin at
                // 100% of a core forever (one leaked task per session).
                Err(RecvError::Lagged(skipped)) => {
                    log::debug!("client event pump lagged, skipped {skipped} records");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Wait for the client's AudioReady signal (with its channel/rate caps), then
/// create the PipeWire audio sink and forward packets to the client's control
/// channel.
///
/// Deliberately *not* one of the session supervisor's race arms. It is a one-shot
/// initialisation: create the sink, hand the forwarder its own spawned task, then
/// park until the session token is cancelled. Parking on that token means the task
/// ends precisely *because* the session ended, so as a race arm it became ready in
/// the same instant as the driver whose teardown cancelled it — and `select!`,
/// which polls in a randomised order, named one of the two at random. That made the
/// "which arm ended the session" line actively misleading: a teardown cascade
/// reports the audio arm even when the transport was what died.
///
/// Not racing it loses nothing: `run_session` always cancels the token on its way
/// out, so the task cannot outlive the session, and a missing or failing audio
/// device must degrade the session rather than end it.
fn audio_ready_task(
    mut audio_rx: broadcast::Receiver<ClientDatagram>,
    session_name: String,
    control_tx: mpsc::Sender<ServerDatagram>,
    audio_cancel: CancellationToken,
    audio_sink: Arc<Mutex<Option<AudioSink>>>,
) {
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
                    // Falling behind the bus is not a reason to give up on the
                    // client's audio; the next AudioReady still arrives.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
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
                // Park until the session winds down; see why this task is not
                // one of the supervisor's arms.
                audio_cancel.cancelled().await;
            }
            Err(e) => {
                println!("[audio] audio sink unavailable: {e:#}");
                // Same contract as the success arm: a silent session is a
                // degraded one, not a broken one.
                audio_cancel.cancelled().await;
            }
        }
    });
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
        let mut received: u64 = 0;
        loop {
            match time::timeout(KEEPALIVE_TIMEOUT, connection.receive_datagram()).await {
                Ok(Ok(datagram)) => {
                    received += 1;
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
                // Timed out or connection error — peer is gone. The two are
                // different failures: silence means the client stopped talking
                // (or was blocked), an error means the transport itself died.
                Ok(Err(err)) => {
                    log::info!(
                        "datagram pump ended after {received} datagrams: transport error: {err}"
                    );
                    break;
                }
                Err(_) => {
                    log::info!(
                        "datagram pump ended after {received} datagrams: \
                         no datagram from the client for {KEEPALIVE_TIMEOUT:?}"
                    );
                    break;
                }
            }
        }
    })
}

/// One encoded frame waiting to go out on its own unidirectional stream.
///
/// The bytes are already a complete [`ServerDatagram::VideoKeyFrame`]; `Arc`
/// lets the forwarder hand them over to the sending task without copying a
/// multi-megabyte keyframe a second time.
type KeyframeMessage = Arc<Vec<u8>>;

/// Encode a keyframe as the single message its unidirectional stream carries.
///
/// The end of the stream delimits the message, and
/// [`ServerDatagram::VideoKeyFrame`] has no fragment fields, so a keyframe
/// cannot be split even by mistake.
fn key_frame_message(frame_id: u16, codec: shared::codec::Codec, payload: &[u8]) -> Vec<u8> {
    ServerDatagram::key_frame_to_bytes(frame_id, codec, payload)
}

fn frame_forwarder(
    mut frame_rx: Receiver<video::EncodedFrame>,
    mut server_msg_rx: mpsc::Receiver<shared::server_datagram::ServerDatagram>,
    wt: Arc<Connection>,
    ring: Arc<Mutex<DeltaRing>>,
) -> tokio::task::JoinHandle<()> {
    let payload_size = wt
        .max_datagram_size()
        .unwrap_or(1200)
        .saturating_sub(server_datagram::ServerDatagram::video_header_size())
        .max(1);
    // Keyframes travel on a stream rather than a datagram, and a
    // multi-megabyte write must not stall the control datagrams this task also
    // owns, so they get a task of their own. The channel has a single slot: a
    // keyframe still waiting when a newer one arrives is redundant — only the
    // newest can resynchronise the client — so the newest replaces it.
    let (keyframe_tx, keyframe_rx) = watch::channel::<Option<KeyframeMessage>>(None);
    spawn(keyframe_forwarder(keyframe_rx, wt.clone()));
    let mut pacer = datagram_pacing_bps().map(|bps| {
        log::info!("delta datagrams paced at {bps} bit/s ({}B each)", payload_size);
        Pacer::new(bps, payload_size)
    });
    if pacer.is_none() {
        log::info!("delta datagrams unpaced: a frame's fragments go out back to back");
    }
    // Every delta payload is kept for a bounded time so a repair request can be
    // answered. The copy the paced send already makes is the one that goes in, so
    // this costs no extra memcpy — only the memory the bound allows. The lock is
    // taken per frame rather than held, because a `std::sync::Mutex` guard cannot
    // be held across the `await` points inside the send loop.
    let mut frame_id: u16 = 0;
    let mut frames_seen: u64 = 0;
    let mut frames_sent: u64 = 0;
    let mut link = LinkReport::default();
    let mut profile = SendProfile::default();
    spawn(async move {
        // Fixed-cadence report of whether the link is keeping up. Declared before
        // the loop because a fresh `interval` restarts on every construction.
        let mut report = tokio::time::interval(Duration::from_secs(5));
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(report);
        loop {
            tokio::select! {
                biased;
                // Ahead of the frame arm deliberately: with `biased` the first
                // ready arm wins, and while frames are always available this
                // one would never be polled at all — exactly when the report
                // matters most.
                _ = report.tick() => {
                    // Frames the encoder produced vs. frames actually put on the
                    // wire. The two are reconciled here because the difference is
                    // invisible everywhere else: a hole in the id sequence looks
                    // exactly like a lost frame at the client, which answers by
                    // sending a keyframe that is among the most expensive things
                    // on the link. If this ever differs, the loss is ours.
                    if frames_seen > frames_sent {
                        log::info!(
                            "id holes: {} frames produced but only {} sent in 5s",
                            frames_seen,
                            frames_sent
                        );
                    }
                    if profile.frames > 0 {
                        // Reported unconditionally while a session is running,
                        // unlike the link line below, because this is the baseline
                        // the next report is compared against: a mean rate that
                        // sits at the pacing ceiling says the pacer is the thing
                        // limiting the send, which is the whole point of it.
                        log::info!(
                            "deltas: {} frames, {} fragments, largest burst {}B/{}frags, \
                             mean rate {}Mbit/s",
                            profile.frames,
                            profile.fragments,
                            profile.peak_burst_bytes,
                            profile.peak_frags,
                            profile.mean_rate_bps() / 1_000_000,
                        );
                    }
                    frames_seen = 0;
                    frames_sent = 0;
                    profile.frames = 0;
                    profile.fragments = 0;
                    profile.peak_burst_bytes = 0;
                    profile.peak_frags = 0;
                    profile.total_bytes = 0;
                    profile.total_span = Duration::ZERO;
                    report_link(&wt, &mut link);
                }
                frame = frame_rx.recv() => {
                    let Some(frame) = frame else { break };
                    frames_seen += 1;
                    let mapped = match frame.data.map_readable() {
                        Ok(m) => m,
                        Err(_) => {
                            // This frame's id is consumed and never sent, which
                            // is indistinguishable at the client from a frame
                            // lost in transit — it sees a hole and pays a
                            // keyframe for it. Previously silent.
                            log::debug!("frame {frame_id} unreadable, its id is now a hole");
                            frame_id = frame_id.wrapping_add(1);
                            continue;
                        }
                    };
                    let data = mapped.as_slice();
                    if frame_goes_on_a_stream(frame.is_keyframe) {
                        // A keyframe is the one frame the client cannot decode
                        // without, so it is the one frame that must never be
                        // dropped. Streams are ordered and retransmitted, so it
                        // goes on one of its own, whole.
                        keyframe_tx.send_replace(Some(Arc::new(key_frame_message(
                            frame_id,
                            frame.codec,
                            data,
                        ))));
                        frames_sent += 1;
                    } else {
                        // Deltas go on datagrams, split to fit. A delta lost this
                        // way costs one frame and a resynchronisation, which is a
                        // fair price for keeping a stream per frame off the
                        // connection — at 30-60 fps those streams are a constant
                        // stream-credit and flow-control churn, and the deltas they
                        // carry are individually worthless the moment they are
                        // late.
                        //
                        // The payload is copied out before it is sent because the
                        // paced loop below awaits, and holding a readable mapping
                        // of an encoder buffer across an await would pin that
                        // memory for as long as the frame takes to trickle out.
                        // A few hundred kilobytes is nothing next to a pinned
                        // DMABUF, and the copy is what the frames are going to
                        // need regardless once they outlive the send.
                        let data = data.to_vec();
                        drop(mapped);
                        let num_frags = data.len().div_ceil(payload_size) as u16;
                        // The payload goes into the ring and the send loop gets a
                        // handle to the same allocation, so a frame is copied out of
                        // the encoder exactly once. The guard is dropped at the end
                        // of this block, before the first `await` in the loop below.
                        let payload = {
                            let mut ring = ring.lock().expect("delta ring is session-scoped");
                            ring.insert(DeltaEntry {
                                frame_id,
                                codec: frame.codec,
                                num_frags,
                                payload_size,
                                payload: Arc::new(data),
                            });
                            ring.entries.back().expect("inserted above").payload.clone()
                        };
                        let mut ctx = SendCtx {
                            wt: &wt,
                            payload_size,
                            pacer: &mut pacer,
                            profile: &mut profile,
                        };
                        if send_delta(&mut ctx, frame_id, frame.codec, num_frags, &payload).await {
                            frames_sent += 1;
                        }
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

/// Answer [`ClientDatagram::ResendDeltas`] by putting the named fragments back on
/// the wire.
///
/// This is the cheap half of the answer to a gap, and the reason it is worth
/// having a ring at all: a client short one fragment of a frame can name that
/// fragment, and the cost of sending it again is one datagram against the
/// keyframe — tens of packets — that the same gap would otherwise cost. It also
/// repairs more than a keyframe does, because the frame goes back where it
/// belongs and the frames between the loss and a keyframe stay decodable.
///
/// A request for a frame the ring has already dropped is not an error and is not
/// retried: the client gives every gap a deadline and asks for a keyframe when
/// it passes, so an unanswerable request is simply the case that fallback exists
/// for. It is logged, because a ring that cannot answer is a ring that is too
/// small or a client that is too slow, and both are worth knowing about.
fn resend_task(
    mut client_rx: broadcast::Receiver<ClientDatagram>,
    wt: Arc<Connection>,
    ring: Arc<Mutex<DeltaRing>>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
        loop {
            let msg = match client_rx.recv().await {
                Ok(msg) => msg,
                Err(RecvError::Lagged(skipped)) => {
                    // A repair request in the records just missed is not
                    // recoverable here, but it is not lost either: the client
                    // re-requests on the next gap and escalates on the deadline,
                    // so falling behind degrades a repair rather than the session.
                    log::debug!("resend pump lagged, skipped {skipped} records");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            let ClientDatagram::ResendDeltas { frames } = msg else {
                continue;
            };
            let mut answered = 0usize;
            let mut unknown = 0usize;
            for (frame_id, indices) in &frames {
                let mut ring = ring.lock().expect("delta ring is session-scoped");
                match ring.resend(&wt, *frame_id, indices) {
                    0 => unknown += 1,
                    resent => answered += resent,
                }
            }
            if answered > 0 || unknown > 0 {
                log::info!(
                    "repair: {answered} fragment(s) resent, {unknown} frame(s) no longer held",
                );
            }
        }
    })
}

/// Put each queued keyframe on a unidirectional stream of its own.
///
/// Streams are ordered and retransmitted, so a keyframe is guaranteed to reach
/// the client whole, which is what makes them worth the extra transport
/// bookkeeping: a delta frame is disposable, a keyframe is the one frame that
/// can resynchronise a client which has lost its prediction chain.
///
/// The task ends when the forwarder drops the sender, or as soon as a write
/// fails on the closed connection.
///
async fn keyframe_forwarder(mut rx: watch::Receiver<Option<KeyframeMessage>>, wt: Arc<Connection>) {
    let mut sent = 0usize;
    while rx.changed().await.is_ok() {
        // The slot holds at most one entry and `changed` only wakes on a fresh
        // one, so this is always the newest keyframe.
        let Some(bytes) = rx.borrow_and_update().clone() else {
            continue;
        };
        match send_keyframe(&wt, &bytes).await {
            Ok(()) => {
                sent += 1;
                if sent == 1 {
                    // The first one is reported at info because it is the one
                    // that proves the whole split path works: no line here means
                    // the client never got a reference frame, whatever the
                    // datagram traffic looks like.
                    log::info!("first keyframe sent on its own stream ({} bytes)", bytes.len());
                } else {
                    log::debug!("keyframe sent on its own stream ({} bytes)", bytes.len());
                }
            }
            Err(err) => {
                sent = 0;
                log::warn!("keyframe stream failed: {err:#?}");
            }
        }
    }
}

/// What the last link report counted from, so the next one can report deltas.
#[derive(Default)]
struct LinkReport {
    /// Whether the path baseline has been logged yet.
    primed: bool,
    last_lost: u64,
    last_black_holes: u64,
    last_congestion: u64,
}

/// Report the path itself, once, and again whenever it degrades.
///
/// This is the only way to tell a network loss from a local one, and the two
/// need opposite fixes: network loss means the frames are too big for the path,
/// while send-buffer exhaustion means they are going faster than the socket and
/// no amount of bandwidth would help. `mtu` is the sharpest signal of all on a
/// tunnelled path — if it has been discovered below the peer's advertised
/// maximum, packets that size are being lost somewhere and quinn had to back
/// off.
fn report_link(wt: &Connection, last: &mut LinkReport) {
    // wtransport keeps the underlying quinn connection private but exposes it, and
    // the path counters live only on quinn's side.
    let stats = wt.quic_connection().stats();
    let path = &stats.path;
    let lost = path.lost_packets.wrapping_sub(last.last_lost);
    let black_holes = path
        .black_holes_detected
        .wrapping_sub(last.last_black_holes);
    let congestion = path
        .congestion_events
        .wrapping_sub(last.last_congestion);
    last.last_lost = path.lost_packets;
    last.last_black_holes = path.black_holes_detected;
    last.last_congestion = path.congestion_events;

    let degraded = lost > 0 || black_holes > 0 || congestion > 0;
    if !degraded && last.primed {
        return;
    }
    // A clean path is only worth saying once: repeating it every five seconds
    // would bury the lines that matter when one appears.
    if !degraded {
        log::info!(
            "link clean: mtu={} rtt={:.0}ms cwnd={}B window=5s",
            path.current_mtu,
            path.rtt.as_secs_f64() * 1e3,
            path.cwnd
        );
    } else {
        log::info!(
            "link degraded over 5s: mtu={} rtt={:.0}ms cwnd={}B lost={} black_holes={} \
             congestion_events={}",
            path.current_mtu,
            path.rtt.as_secs_f64() * 1e3,
            path.cwnd,
            lost,
            black_holes,
            congestion
        );
    }
    last.primed = true;
}

/// Whether a datagram can be sent without losing an older queued one.
///
/// quinn guarantees that a send of at most the available space will not evict
/// anything already queued; anything larger drops the oldest datagram to make
/// room. So this is an exact test, not a guess at how full is too full.
fn datagram_fits(dgram: &[u8], buffer_space: usize) -> bool {
    dgram.len() <= buffer_space
}

/// Free space in quinn's outgoing datagram buffer.
///
/// Read through [`Connection::quic_connection`] because wtransport 0.7 exposes
/// `max_datagram_size` but not this, and the send buffer is the only way to know
/// a datagram send will not silently evict an older one.
fn datagram_send_buffer_space(wt: &Connection) -> usize {
    wt.quic_connection().datagram_send_buffer_space()
}

/// The ceiling on how fast datagrams may leave, in bits per second.
///
/// This is the encoder's nominal bitrate, which is the rate the video actually
/// needs; the point is not to send less, it is to stop sending a frame's worth
/// in a single instant. See [`Pacer`] for why the datagram path needs this
/// imposed on it from the outside.
const DEFAULT_PACING_BPS: u64 = 7_000_000;

/// Read the datagram pacing ceiling, in bits per second, from the environment.
///
/// `WEBSHOOTER_DATAGRAM_PACING=off` restores the unpaced send, which is the
/// behaviour this pacing was added to measure against. Any other value is taken
/// as a ceiling in bits per second, so the average can be held below the
/// encoder's nominal rate as well as smoothed above it.
fn datagram_pacing_bps() -> Option<u64> {
    parse_pacing_bps(std::env::var("WEBSHOOTER_DATAGRAM_PACING").ok())
}

/// The parsing half of [`datagram_pacing_bps`], split out so it can be tested
/// without the process-wide environment.
fn parse_pacing_bps(raw: Option<String>) -> Option<u64> {
    let Some(raw) = raw else {
        return Some(DEFAULT_PACING_BPS);
    };
    if raw.eq_ignore_ascii_case("off") {
        return None;
    }
    match raw.parse::<u64>() {
        // Zero is a request to send nothing, which is never what a caller of a
        // pacing ceiling means, so it is read the way it is usually meant.
        Ok(0) => None,
        Ok(bps) => Some(bps),
        Err(_) => {
            log::warn!("WEBSHOOTER_DATAGRAM_PACING={raw:?} is not a bitrate, pacing off");
            None
        }
    }
}

/// Space datagrams out so one frame's fragments leave as a trickle, not a burst.
///
/// The reason this has to exist here rather than being left to the transport is
/// that quinn does not congestion-control datagrams. In
/// `quinn-proto`'s `Connection::poll_transmit` the congestion-window and pacer
/// checks sit behind `if ack_eliciting`, and that flag is computed before any
/// frame is written, from pending *stream* frames, a pending ping, or a pending
/// immediate-ack. A packet built only to carry `DATAGRAM` frames therefore
/// carries `ack_eliciting = false` and skips both checks — it is neither paced
/// nor refused when the window is full.
///
/// The consequence is specific and load-bearing: our keyframes go on streams, so
/// quinn paces and congestion-controls them, while our deltas go on datagrams,
/// so quinn paces and congestion-controls *nothing* about them. A delta path can
/// flood a link far past what the link can carry, with no backoff, and the only
/// limit in its way is the size of quinn's own send buffer — a figure chosen to
/// be larger than any path, not smaller. The `cwnd` in the link report is the
/// congestion window of the *stream* traffic; the deltas that filled the path
/// were never measured against it.
///
/// So the ceiling is imposed here, at the one place that knows how much video
/// there is to send.
struct Pacer {
    /// When the next datagram becomes due.
    next: Instant,
    /// The gap between consecutive datagrams.
    interval: Duration,
}

impl Pacer {
    /// Pace at `bps`, measured over datagrams of `payload_size` bytes.
    fn new(bps: u64, payload_size: usize) -> Self {
        // seconds per datagram, in nanoseconds. The multiply cannot overflow for
        // any datagram that fits in a `u16` MTU, and the divisor is a ceiling in
        // bits per second, so a smaller rate gives a longer interval.
        let nanos = (payload_size as u64)
            .saturating_mul(8)
            .saturating_mul(1_000_000_000)
            / bps;
        Self {
            next: Instant::now(),
            interval: Duration::from_nanos(nanos.max(1)),
        }
    }

    /// Wait for the next datagram's slot, then take the one after it.
    async fn tick(&mut self) {
        let now = Instant::now();
        if self.next > now {
            tokio::time::sleep_until(self.next.into()).await;
            self.next += self.interval;
        } else {
            // Behind schedule, which is what congestion looks like from here.
            // Resynchronise instead of letting the interval run down: catching
            // up by sending the backlog with no delay at all would reproduce
            // exactly the burst this exists to prevent.
            self.next = now + self.interval;
        }
    }
}

/// One delta frame's payload, kept so a lost fragment can be sent again.
struct DeltaEntry {
    frame_id: u16,
    codec: shared::codec::Codec,
    /// How many fragments the frame was split into when it went out.
    num_frags: u16,
    /// The payload size those fragments were cut at, stored so a resend reproduces
    /// the original boundaries exactly rather than whatever the path's MTU happens
    /// to be now.
    payload_size: usize,
    /// Shared rather than owned, because the send loop needs the same bytes and a
    /// `std::sync::Mutex` guard cannot be held across the `await` points inside
    /// it. Cloning an `Arc` is a refcount, not a copy of the payload.
    payload: Arc<Vec<u8>>,
}

/// Recent delta payloads, so a repair request can be answered.
///
/// A client that is short a fragment of a frame knows *which* fragment, and can
/// ask for just that. The server can only honour the request while it still holds
/// the frame, so a delta's payload outlives its send by a bounded amount of time.
///
/// The bound is what makes this cheap rather than a second video buffer: a delta is
/// a few kilobytes, so even a generous allowance is a few hundred kilobytes, which
/// is nothing next to the keyframe a repair replaces — and a request for a frame
/// this has already dropped is a request the client gave up on and escalated past.
struct DeltaRing {
    /// Oldest first.
    entries: VecDeque<DeltaEntry>,
    /// Bytes held, so the bound can be on memory rather than on a frame count that
    /// says nothing about how much a frame cost.
    bytes: usize,
}

/// How much delta payload the ring may hold.
///
/// A repair request is answered within a round trip of being sent, so anything
/// older than that is a frame the client has already stopped waiting for. A second
/// of deltas at this project's bitrate is a few hundred kilobytes; this is several
/// times that, so the count bound below is the one that binds in practice.
const DELTA_RING_BYTES: usize = 512 * 1024;

/// How many frames the ring may hold, as a second bound.
///
/// A run of very large frames could otherwise grow the ring past the point where
/// searching it is free, which matters because a repair request is answered on the
/// client's message path.
const DELTA_RING_FRAMES: usize = 256;

impl DeltaRing {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Keep `payload` so a later request for one of its fragments can be answered.
    fn insert(&mut self, entry: DeltaEntry) {
        self.bytes += entry.payload.len();
        self.entries.push_back(entry);
        // Evict from the front: the oldest frames are the ones a request is least
        // likely to name, and the newest are the ones still worth answering.
        while self.bytes > DELTA_RING_BYTES || self.entries.len() > DELTA_RING_FRAMES {
            let Some(oldest) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= oldest.payload.len();
        }
    }

    /// The datagram that carries fragment `index` of `frame_id`, or `None` if the
    /// frame is not held or the index is not one of its fragments.
    ///
    /// Split out from the send so the reconstruction can be tested without a live
    /// connection: the boundaries have to be exactly the ones the frame was
    /// originally cut at, or the client reassembles a different payload than the
    /// one it was missing.
    fn fragment(&self, frame_id: u16, index: u16) -> Option<Vec<u8>> {
        let entry = self.entries.iter().find(|entry| entry.frame_id == frame_id)?;
        let index = index as usize;
        if index >= entry.num_frags as usize {
            return None;
        }
        let start = index * entry.payload_size;
        let end = ((index + 1) * entry.payload_size).min(entry.payload.len());
        let chunk = entry.payload.get(start..end)?;
        Some(server_datagram::ServerDatagram::delta_to_bytes(
            frame_id,
            index as u16,
            entry.num_frags,
            entry.codec,
            chunk,
        ))
    }

    /// Send the named fragments of `frame_id` again, if the frame is still held.
    ///
    /// Returns how many were actually resent, which is worth logging: a request
    /// the ring cannot answer is a request the client will escalate to a keyframe
    /// for, and that is the case this whole path exists to avoid.
    fn resend(&mut self, wt: &Connection, frame_id: u16, indices: &[u16]) -> usize {
        let mut resent = 0;
        for index in indices {
            // The send is fire-and-forget by design: a repair that is itself lost
            // is not an error, it is the case the client's escalation deadline
            // exists for. Pacing it would be wrong too — a repair is a few
            // datagrams, and delaying them costs the one round trip the whole
            // scheme is trying to save.
            if let Some(dgram) = self.fragment(frame_id, *index)
                && wt.send_datagram(&dgram).is_ok()
            {
                resent += 1;
            }
        }
        resent
    }
}

/// What [`send_delta`] needs, bundled so the call does not take more arguments
/// than a reader can hold in their head at once.
struct SendCtx<'a> {
    wt: &'a Connection,
    payload_size: usize,
    pacer: &'a mut Option<Pacer>,
    profile: &'a mut SendProfile,
}

/// Put one delta frame's fragments on the wire, paced.
///
/// Returns whether the frame was sent. It is not when the connection is gone,
/// which the caller must treat as the end of the session rather than a reason
/// to keep a frame the peer will never see.
async fn send_delta(
    ctx: &mut SendCtx<'_>,
    frame_id: u16,
    codec: shared::codec::Codec,
    num_frags: u16,
    payload: &Arc<Vec<u8>>,
) -> bool {
    let SendCtx {
        wt,
        payload_size,
        pacer,
        profile,
    } = ctx;
    let mut sent_bytes = 0usize;
    let started = Instant::now();
    for (idx, chunk) in payload.chunks(*payload_size).enumerate() {
        let dgram = server_datagram::ServerDatagram::delta_to_bytes(
            frame_id,
            idx as u16,
            num_frags,
            codec,
            chunk,
        );
        // quinn drops the *oldest* queued datagram to make room for one that
        // does not fit, so sending into a full buffer loses a fragment that was
        // never on the wire. Skipping this one instead loses only the frame we
        // were already going to lose, and only while the link is behind.
        if !datagram_fits(&dgram, datagram_send_buffer_space(wt)) {
            log::debug!(
                "delta {frame_id} fragment {idx} skipped: \
                 send buffer holds {} bytes, fragment is {}",
                datagram_send_buffer_space(wt),
                dgram.len()
            );
            continue;
        }
        if let Some(pacer) = pacer.as_mut() {
            pacer.tick().await;
        }
        if wt.send_datagram(&dgram).is_err() {
            log::warn!("send_datagram failed: connection closed");
            return false;
        }
        sent_bytes += dgram.len();
        profile.fragments += 1;
    }
    profile.record(sent_bytes, num_frags, started.elapsed());
    true
}

/// What the datagram path actually did, read back in the five-second report.
///
/// This exists to tell two causes of loss apart, because they are
/// indistinguishable from the client's side and need opposite fixes. Frames lost
/// on the wire want a smaller payload or forward error correction. Frames lost
/// because the path was handed more than it could take, in one instant, want
/// pacing — and no amount of parity or retransmission helps, because the
/// overflow happens on the way out of the socket and the parity shard is
/// dropped alongside the shard it was meant to repair.
#[derive(Default)]
struct SendProfile {
    /// Delta frames whose fragments were handed over.
    frames: u64,
    /// Datagrams sent for those frames.
    fragments: u64,
    /// The largest number of bytes handed over for a single frame.
    peak_burst_bytes: usize,
    /// The most fragments any single frame was split into.
    ///
    /// Reported alongside the byte count because the two say different things: a
    /// frame can be large because it is one wide frame or because it is many
    /// fragments, and it is the fragment count that decides how many chances the
    /// path has to lose part of it.
    peak_frags: u16,
    /// Bytes handed over in total, and the time the send loops took.
    ///
    /// The mean of the two is the rate the path is actually asked to sustain
    /// while sending, which is the number the pacing ceiling is compared against.
    /// A peak rate is deliberately *not* reported: a one-fragment frame has a
    /// send span of microseconds, so a maximum over per-frame rates is a maximum
    /// over noise and reads as a burst that never happened.
    total_bytes: u64,
    total_span: Duration,
}

impl SendProfile {
    /// Fold in one frame's send, given its total bytes, its fragment count, and
    /// how long the loop that sent them took.
    fn record(&mut self, bytes: usize, frags: u16, span: Duration) {
        self.frames += 1;
        self.total_bytes += bytes as u64;
        self.total_span += span;
        self.peak_burst_bytes = self.peak_burst_bytes.max(bytes);
        self.peak_frags = self.peak_frags.max(frags);
    }

    /// The rate the path was asked to sustain while sending, in bits per second.
    fn mean_rate_bps(&self) -> u64 {
        if self.total_span.is_zero() {
            return 0;
        }
        (self.total_bytes as u128 * 8 * 1_000_000_000 / self.total_span.as_nanos().max(1)) as u64
    }
}

/// Whether a frame travels on a reliable stream rather than a datagram.
///
/// Only a keyframe does. It is the one frame the client cannot decode without,
/// so it is the one frame that must never be dropped, and only a stream
/// guarantees that: a datagram is discarded the moment the send buffer is full,
/// and one split across several is lost as soon as any fragment is.
///
/// A delta is deliberately not treated the same way. It is worth one lost frame
/// and a resynchronisation, and keeping a stream per frame off the connection
/// matters more at 30-60 fps — that is steady stream-credit and flow-control
/// churn for data that is worthless the moment it is late.
fn frame_goes_on_a_stream(is_keyframe: bool) -> bool {
    is_keyframe
}

/// Open a unidirectional stream carrying one complete [`ServerDatagram`] and
/// wait for the peer to acknowledge it.
async fn send_keyframe(wt: &Connection, bytes: &[u8]) -> Result<()> {
    // Two awaits: the first reserves the stream, the second completes the
    // WebTransport handshake that makes it usable.
    let mut stream = wt.open_uni().await?.await?;
    stream.write_all(bytes).await?;
    // `finish` completes once the peer has acknowledged everything, so a
    // keyframe is never declared sent before the client can read all of it.
    stream.finish().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A keyframe is the one frame the client cannot decode without, so it is
    /// the one frame that must never be dropped — which is why it is the only
    /// kind of frame that goes on a stream. A delta may be lost and recovered
    /// from, and keeping a stream per frame off the connection matters more at
    /// 30-60 fps than the occasional delta is worth.
    #[test]
    fn only_keyframes_travel_on_a_stream() {
        assert!(frame_goes_on_a_stream(/* is_keyframe = */ true));
        assert!(!frame_goes_on_a_stream(/* is_keyframe = */ false));
    }

    /// A datagram is only safe to send while it fits the space quinn has queued.
    /// One byte over and quinn evicts the *oldest* datagram to make room, losing
    /// a fragment that never reached the wire — self-inflicted loss, with no
    /// congestion event and nothing in the client's gap log to explain it.
    #[test]
    fn a_datagram_is_only_sent_while_it_fits_the_buffer() {
        let roomy = usize::MAX;
        assert!(datagram_fits(&[0; 1191], roomy));
        // Exactly the space available: the boundary case, and the one an
        // off-by-one on the header would break.
        assert!(datagram_fits(&[0; 1191], 1191));
        assert!(!datagram_fits(&[0; 1191], 1190));
        // Nothing queued at all, as on a freshly opened connection.
        assert!(!datagram_fits(&[0; 1191], 0));
        // A datagram is never zero bytes, but the test must not depend on that.
        assert!(datagram_fits(&[], 0));
    }

    /// A keyframe goes out as one whole `ServerDatagram::VideoKeyFrame`. Pin
    /// every part the client switches on: the frame id, the codec, and a
    /// payload far larger than any datagram, so this cannot quietly regress
    /// into the fragmented delta path. The variant carries no fragment fields
    /// at all, so there is nothing here to split it with.
    #[test]
    fn a_keyframe_is_one_whole_server_datagram() {
        let payload = vec![0xAB; 5000];
        let bytes = key_frame_message(0x0201, shared::codec::Codec::Av1, &payload);
        match ServerDatagram::from_bytes(&bytes).expect("keyframe message parses") {
            ServerDatagram::VideoKeyFrame {
                frame_id,
                codec,
                payload: got,
            } => {
                assert_eq!(frame_id, 0x0201);
                assert_eq!(codec, shared::codec::Codec::Av1);
                assert_eq!(got, payload, "the whole keyframe must survive the hop");
            }
            other => panic!("expected a VideoKeyFrame, got {other:?}"),
        }
    }

    /// A delta wide enough for several datagrams is split across them, and every
    /// fragment has to agree on the frame it belongs to, its own place in the
    /// split, and how many places there are — the client reassembles on exactly
    /// those three numbers, and a frame missing one is dropped whole.
    #[test]
    fn a_wide_delta_is_split_across_datagrams_that_reassemble() {
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let payload_size = 1191;
        let num_frags = payload.len().div_ceil(payload_size) as u16;
        assert!(
            num_frags > 1,
            "this test is about a delta wider than one datagram"
        );

        let mut rebuilt = Vec::new();
        for (idx, chunk) in payload.chunks(payload_size).enumerate() {
            let bytes = ServerDatagram::delta_to_bytes(
                0x0402,
                idx as u16,
                num_frags,
                shared::codec::Codec::Av1,
                chunk,
            );
            // Each fragment must fit the datagram it travels in, or the link
            // would have to fragment it again.
            assert!(
                bytes.len() <= 1200,
                "fragment {idx} is {} bytes, over the datagram budget",
                bytes.len()
            );
            match ServerDatagram::from_bytes(&bytes).expect("delta fragment parses") {
                ServerDatagram::VideoDelta {
                    frame_id,
                    frag_idx,
                    num_frags: declared,
                    codec,
                    payload: got,
                } => {
                    assert_eq!(frame_id, 0x0402, "every fragment names its frame");
                    assert_eq!(frag_idx, idx as u16, "fragments are in order");
                    assert_eq!(declared, num_frags, "every fragment agrees on the count");
                    assert_eq!(codec, shared::codec::Codec::Av1);
                    rebuilt.extend_from_slice(&got);
                }
                // The keyframe variant is a distinct message type, so a delta
                // cannot be mistaken for a reference frame — and therefore
                // cannot reset the client's prediction chain.
                other => panic!("expected a VideoDelta, got {other:?}"),
            }
        }
        assert_eq!(rebuilt, payload, "the split must be lossless");
    }

    /// Pacing must hold the *average* rate at the ceiling, not merely slow some
    /// fragments down. A pacer that stalls and then catches up by bursting has
    /// moved the problem rather than fixed it, so the interval is pinned against
    /// the arithmetic a whole frame depends on.
    #[test]
    fn pacing_interval_is_the_ceiling_over_the_datagram_size() {
        // 1192 bytes is the payload a 1200-byte datagram leaves after the delta
        // header, and 7 Mbit/s is the encoder's nominal rate.
        let interval = Pacer::new(7_000_000, 1192).interval;
        // 1192 * 8 / 7_000_000 seconds, which is 1.362285714 ms truncated to
        // whole nanoseconds.
        let exact = Duration::from_nanos(1192 * 8 * 1_000_000_000 / 7_000_000);
        assert_eq!(interval, exact);

        // A whole 14-fragment delta, sent at that interval, must land on the
        // ceiling. Truncating each interval to a whole nanosecond can only make
        // it marginally shorter, so the implied average sits a hair *over* the
        // ceiling — 3.7 bit/s out of seven million here. That is the real bound
        // of integer pacing, and it is asserted as such rather than wished away
        // with a tolerance that would hide a genuine overshoot.
        let frame = 14 * interval;
        let implied_bps = 14.0 * 1192.0 * 8.0 / frame.as_secs_f64();
        let overage = implied_bps - 7_000_000.0;
        assert!(
            overage < 1e-3 * 7_000_000.0,
            "14 fragments in {frame:?} implies {implied_bps} bit/s, \
             over the ceiling by {overage} bit/s"
        );
    }

    /// A rate so low that the arithmetic would round to zero nanoseconds must
    /// still produce a wait, or the ceiling silently becomes no ceiling at all.
    #[test]
    fn pacing_never_rounds_to_an_instant_send() {
        // 1 bit/s over a full 65535-byte datagram is well under one nanosecond
        // per bit, and truncating it to zero would send every fragment at once.
        let pacer = Pacer::new(1, 65_535);
        assert!(pacer.interval > Duration::ZERO, "a ceiling of 1 bit/s gave no wait");
        // A higher rate legitimately gives a shorter one, so the interval must
        // actually be tracking the ceiling rather than being a constant.
        assert!(Pacer::new(7_000_000, 1192).interval < pacer.interval);
    }

    /// The largest burst and the largest fragment count have to be tracked
    /// independently: a frame can be wide because it is one large frame or because
    /// it is many fragments, and the fragment count is what decides how many
    /// chances the path has to lose part of it.
    #[test]
    fn send_profile_keeps_the_largest_burst_and_the_fragment_count_apart() {
        let mut profile = SendProfile::default();
        // A wide frame in few fragments.
        profile.record(60_000, 3, Duration::from_millis(60));
        // A smaller frame in many fragments.
        profile.record(1_000, 40, Duration::from_millis(1));

        assert_eq!(profile.frames, 2);
        assert_eq!(profile.peak_burst_bytes, 60_000, "largest burst is the wide frame");
        assert_eq!(profile.peak_frags, 40, "most fragments is the split frame");
    }

    /// The mean rate is the rate the path is asked to sustain while sending, and
    /// it is the number the pacing ceiling is compared against — so it has to be
    /// the total bytes over the total time, not a maximum over per-frame rates.
    #[test]
    fn the_mean_rate_is_the_total_bytes_over_the_total_time() {
        let mut profile = SendProfile::default();
        // Two frames of 1000 bytes each, each taking 10ms: 2000 bytes in 20ms,
        // which is 800 kbit/s. A per-frame maximum would report the same here, so
        // the two frames are given different spans to tell the formulae apart.
        profile.record(1_000, 1, Duration::from_millis(10));
        profile.record(1_000, 1, Duration::from_millis(30));
        // 2000 bytes in 40ms is 400 kbit/s. A max-of-per-frame-rates would report
        // 800 kbit/s (the faster frame), which is not the rate being sustained.
        assert_eq!(profile.mean_rate_bps(), 400_000);
    }

    /// A frame that took no measurable time must not divide by zero.
    #[test]
    fn send_profile_survives_an_empty_window() {
        let profile = SendProfile::default();
        assert_eq!(profile.mean_rate_bps(), 0, "no frames means no rate");
    }

    /// The environment must not be the only way to turn pacing off, and a typo
    /// must not silently take it off when the caller believed it was on.
    #[test]
    fn pacing_setting_is_read_from_the_environment() {
        assert_eq!(parse_pacing_bps(None), Some(DEFAULT_PACING_BPS), "on by default");
        assert_eq!(parse_pacing_bps(Some("off".into())), None);
        assert_eq!(parse_pacing_bps(Some("OFF".into())), None);
        assert_eq!(parse_pacing_bps(Some("0".into())), None);
        assert_eq!(parse_pacing_bps(Some("2000000".into())), Some(2_000_000));
        // Unparseable is read as off, and is logged, so the log says the ceiling
        // is gone rather than the log staying quiet about it.
        assert_eq!(parse_pacing_bps(Some("fast".into())), None);
    }

    /// A ring entry as the forwarder builds it.
    fn entry(frame_id: u16, payload: &[u8], payload_size: usize) -> DeltaEntry {
        DeltaEntry {
            frame_id,
            codec: shared::codec::Codec::Av1,
            num_frags: payload.len().div_ceil(payload_size) as u16,
            payload_size,
            payload: Arc::new(payload.to_vec()),
        }
    }

    /// A resend has to reproduce the exact datagram the frame was originally cut
    /// into, because the client reassembles by concatenating fragments in index
    /// order and comparing the count against its own record of the frame. A
    /// boundary that has drifted produces a payload that is the right length and
    /// the wrong bytes.
    #[test]
    fn a_resend_reproduces_the_original_fragment_boundaries() {
        // 2500 bytes at 1000 a fragment is three fragments: 1000, 1000, 500.
        let payload: Vec<u8> = (0..2500u32).map(|i| (i % 251) as u8).collect();
        let mut ring = DeltaRing::new();
        ring.insert(entry(7, &payload, 1000));

        let mut rebuilt = Vec::new();
        for index in 0..3 {
            let dgram = ring
                .fragment(7, index)
                .expect("the frame is held and the index is in range");
            match ServerDatagram::from_bytes(&dgram).expect("parses") {
                ServerDatagram::VideoDelta {
                    frame_id,
                    frag_idx,
                    num_frags,
                    payload: chunk,
                    ..
                } => {
                    assert_eq!(frame_id, 7);
                    assert_eq!(frag_idx, index);
                    assert_eq!(num_frags, 3, "the fragment count is unchanged");
                    rebuilt.extend_from_slice(&chunk);
                }
                other => panic!("expected a VideoDelta, got {other:?}"),
            }
        }
        assert_eq!(rebuilt, payload, "the resend reassembles to the original bytes");
    }

    /// The last fragment of a frame is short, and a resend must not pad it or run
    /// off the end of the payload — either would change the reassembled length.
    #[test]
    fn a_resend_of_the_last_fragment_is_the_short_one() {
        let payload: Vec<u8> = (0..2500u32).map(|i| (i % 251) as u8).collect();
        let mut ring = DeltaRing::new();
        ring.insert(entry(7, &payload, 1000));

        let dgram = ring.fragment(7, 2).expect("the last fragment is held");
        match ServerDatagram::from_bytes(&dgram).expect("parses") {
            ServerDatagram::VideoDelta { payload, .. } => {
                assert_eq!(payload.len(), 500, "the tail fragment is 500 bytes, not 1000");
            }
            other => panic!("expected a VideoDelta, got {other:?}"),
        }
    }

    /// A frame the ring has dropped cannot be resent, and that is the normal case
    /// rather than an error: the client escalates to a keyframe on a deadline, so
    /// an unanswerable request is the fallback working as designed.
    #[test]
    fn a_resend_of_an_unheld_frame_is_refused() {
        let mut ring = DeltaRing::new();
        ring.insert(entry(7, &[0; 100], 1000));
        assert!(ring.fragment(8, 0).is_none(), "frame 8 was never held");
        assert!(ring.fragment(7, 5).is_none(), "index 5 is not one of frame 7's fragments");
    }

    /// The ring is bounded, and the bound has to fall on the oldest frames: those
    /// are the ones a request is least likely to name, and a repair is only useful
    /// while the client is still waiting for the frame.
    #[test]
    fn the_ring_evicts_the_oldest_frames_first() {
        let mut ring = DeltaRing::new();
        // 600 entries of 1000 bytes is 600 KB, well past the 512 KB bound, so the
        // bound has to be what stops the growth rather than the frame count.
        for id in 0..600u16 {
            ring.insert(entry(id, &[0; 1000], 1000));
        }
        assert!(
            ring.bytes <= DELTA_RING_BYTES,
            "the byte bound is what stops the growth, not the frame count"
        );
        assert!(ring.fragment(0, 0).is_none(), "the oldest frame is evicted");
        assert!(
            ring.fragment(599, 0).is_some(),
            "the newest frame is still held"
        );
    }

    /// The frame-count bound has to hold too, because a run of large frames could
    /// otherwise grow the ring past the point where searching it is free.
    #[test]
    fn the_ring_is_bounded_by_frame_count_as_well_as_bytes() {
        let mut ring = DeltaRing::new();
        // One byte each, so the byte bound is nowhere near binding.
        for id in 0..DELTA_RING_FRAMES as u16 + 10 {
            ring.insert(entry(id, &[0], 1000));
        }
        assert!(
            ring.entries.len() <= DELTA_RING_FRAMES,
            "the ring holds {} frames",
            ring.entries.len()
        );
        assert!(ring.fragment(0, 0).is_none(), "the oldest frames are evicted");
    }

    /// An unpaced send loop cannot rate-limit anything, and this pins the proof.
    ///
    /// Splitting a realistic delta — 15,636 bytes, the mean measured on a real
    /// session — into its fourteen fragments and serialising each one costs a
    /// couple of microseconds, because that is a memcpy. Paced at the encoder's
    /// nominal rate the same bytes take eighteen milliseconds. So the tight loop
    /// provides no rate limit whatsoever: whatever ceiling the path enforces is
    /// the only ceiling there is, and quinn imposes none on datagrams (see
    /// [`Pacer`]). The bound is a whole millisecond rather than the couple of
    /// microseconds actually observed, because the claim under test is the
    /// orders-of-magnitude gap and a debug build on a loaded machine must not be
    /// able to fail it.
    #[test]
    fn an_unpaced_delta_send_loop_costs_far_too_little_to_be_a_rate_limit() {
        let payload: Vec<u8> = (0..15_636u32).map(|i| (i % 251) as u8).collect();
        let payload_size = 1192usize;
        let num_frags = payload.len().div_ceil(payload_size) as u16;
        assert_eq!(num_frags, 14, "a realistic delta is fourteen fragments");

        let started = Instant::now();
        let mut bytes = 0usize;
        for (idx, chunk) in payload.chunks(payload_size).enumerate() {
            let dgram = ServerDatagram::delta_to_bytes(
                7,
                idx as u16,
                num_frags,
                shared::codec::Codec::Av1,
                chunk,
            );
            bytes += dgram.len();
            std::hint::black_box(&dgram);
        }
        let span = started.elapsed();
        assert!(span < Duration::from_millis(1), "the tight loop took {span:?}");

        // The same bytes at the ceiling, and the ratio that makes the point: the
        // loop is thousands of times faster than the rate it is supposed to be
        // sending at, so it will always overrun anything but an uncongested path.
        let paced = Duration::from_nanos(bytes as u64 * 8 * 1_000_000_000 / DEFAULT_PACING_BPS);
        assert!(
            span * 1000 < paced,
            "unpaced {span:?} versus paced {paced:?}: the loop is not the limit"
        );
    }

    /// A consumer that falls behind the client-message bus must keep going.
    ///
    /// Falling behind is routine, not exceptional: the client sends a keepalive
    /// every 50 ms and any capture path parked on a portal dialog stops
    /// draining the bus for as long as the human takes to click Share, so a
    /// 256-slot channel overflows on its own. Treating that lag as fatal ended
    /// live sessions — this pins that it no longer does, and that a record sent
    /// after the lag is still acted on (the decoder caps are what the capture
    /// pipeline blocks on).
    #[tokio::test]
    async fn a_lagged_client_event_pump_keeps_running() {
        let (tx, rx) = broadcast::channel::<ClientDatagram>(4);
        let decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>> =
            Arc::new(Mutex::new(None));
        // Overflow the bus before the pump has read anything: the receiver's
        // cursor is created here, so every one of these is already lost by the
        // time the task starts.
        for _ in 0..8 {
            let _ = tx.send(ClientDatagram::KeepAlive);
        }
        let task = client_events_task(rx, decoder_caps.clone());

        let _ = tx.send(ClientDatagram::DecoderCapabilities {
            decoders: vec![shared::codec::Codec::Av1],
        });
        time::sleep(Duration::from_millis(50)).await;

        assert!(
            !task.is_finished(),
            "a lagged pump must not end the session it belongs to"
        );
        assert_eq!(
            decoder_caps.lock().unwrap().as_deref(),
            Some([shared::codec::Codec::Av1].as_slice()),
            "the record after the lag must still be acted on"
        );
        task.abort();
    }

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
