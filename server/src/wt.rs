use crate::{
    auth::{OnetimeToken, UserId, user_from_id},
    config::{Bytes64, Config, NameTemplate},
    error::WebshooterError,
    get_config, ipc,
    moq::{
        self, AppSide,
        publish::{AudioFrame, Publisher, VideoFrame},
        transport::WtSession,
    },
    pipewire::audio::{AudioPacket, AudioSink, start_audio_sink},
    pipewire::video,
};
use anyhow::Result;
use log::LevelFilter;
use shared::client_datagram::{ClientDatagram, is_input_byte};
use shared::server_datagram::ServerDatagram;
use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::{
    spawn,
    sync::{broadcast, mpsc},
    time::{self},
};
use tokio_util::sync::CancellationToken;
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
/// Per-pump state: each session's `client_pump` task owns its own.
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
        self.tokens =
            (self.tokens + elapsed * INPUT_FLOOD_RATE_PER_SEC as f64).min(INPUT_FLOOD_BURST as f64);
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
        let decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>> =
            Arc::new(Mutex::new(None));
        let audio_sink: Arc<Mutex<Option<AudioSink>>> = Arc::new(Mutex::new(None));
        // The sink's volume setting, reported by the sink node itself. The
        // channel is session-scoped but the producer does not exist until the
        // client's AudioContext starts, so `monitor_host_volume` has to be
        // armed before there is anything to report.
        let (volume_tx, volume_rx) = mpsc::channel::<u8>(8);
        // Encoded audio, from the capture pipeline to the MoQ publisher.
        //
        // Created here, before the producer exists, for the same reason as the
        // volume channel: the audio task only starts once the client says its
        // AudioContext is up, and by then the media pump that drains this must
        // already be running. `audio_ready_task` gets the sender and hands over
        // the packets when the sink comes up.
        let (session_audio_tx, session_audio_rx) = mpsc::channel::<AudioPacket>(256);

        // Render this session's resource names once, up front. A template that
        // fails here was already validated when the config was parsed, so this
        // is the defensive path — and it is handled per session rather than
        // with `?`, because one unrenderable name must not take the whole
        // WebTransport endpoint down with it.
        let config = get_config().await;
        let (virtual_display, virtual_speaker) =
            match render_session_names(&config, &display_name, client_id) {
                Ok(names) => names,
                Err(err) => {
                    log::error!("dropping session {client_id}: {err:#}");
                    continue;
                }
            };

        // MoQ is attached to the connection *before* anything reads from it: the
        // mux pump this spawns becomes the session's only reader, and both halves
        // of what it finds — MoQ's and webshooter's — come out of it. Nothing else
        // may read from the connection again.
        //
        // Sending needs no such rule. The mux is one-way because only MoQ writes
        // MoQ bytes and only webshooter writes webshooter's, so webshooter's own
        // outgoing streams and datagrams stay unambiguous — they lead with a
        // discriminant the client routes on, and the peer does the routing.
        let (moq, app_side) = moq::attach(connection.clone());
        let audio_tx = mpsc::Sender::clone(&session_audio_tx);
        let mut client_pump = client_pump(app_side, client_tx.clone());
        let mut client_events = client_events_task(client_rx.resubscribe(), decoder_caps.clone());
        // Forward the host's volume setting to the client. Not a supervisor arm:
        // it ends only when the session does.
        tokio::spawn({
            let wt = connection.clone();
            let cancel = disconnect.clone();
            async move { monitor_host_volume(wt, volume_rx, cancel).await }
        });
        // Not a supervisor arm — see `audio_ready_task`.
        audio_ready_task(
            client_rx.resubscribe(),
            virtual_speaker.clone(),
            audio_tx,
            disconnect.clone(),
            audio_sink.clone(),
            volume_tx,
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
                virtual_display,
                control_tx,
                control_rx,
                connection,
                moq,
                session_audio_rx,
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
                _ = &mut client_pump => "client pump",
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

/// This session's two rendered resource names: the virtual display and the
/// virtual speaker.
///
/// Both are resolved against the registry *before* this session is registered,
/// so each reflects exactly the sessions it has to stay distinct from.
fn render_session_names(
    config: &Config,
    user_name: &str,
    client_id: ipc::ClientId,
) -> Result<(String, String)> {
    let is_primary = ipc::lowest_client_id().is_none_or(|lowest| client_id <= lowest);
    let render = |template: &NameTemplate| {
        template.render(
            &name_for(template, user_name, client_id, is_primary),
            client_id,
        )
    };
    Ok((
        render(&config.virtual_display_name)?,
        render(&config.virtual_speaker_name)?,
    ))
}

/// The `name` parameter for one session: the sanitised user name, with the
/// client id appended only when it has to be unique.
///
/// A template that spells `{{ client_id }}` out is naming itself, so `name` is
/// left as the plain user name. Otherwise only a session that does not hold the
/// lowest id needs the id to stay distinct from the sessions already live.
/// Ids are reserved exclusively by [`ipc::next_client_id`] and this is decided
/// once, at session start, so no two live sessions can land on the same name.
fn name_for(
    template: &NameTemplate,
    user_name: &str,
    client_id: ipc::ClientId,
    is_primary: bool,
) -> String {
    let user_name = video::sanitise_name(user_name);
    if template.references_client_id() || is_primary {
        user_name
    } else {
        format!("{user_name}-{client_id}")
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
    virtual_display: String,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
    control_rx: mpsc::Receiver<ServerDatagram>,
    connection: Arc<Connection>,
    moq: WtSession,
    audio_rx: mpsc::Receiver<AudioPacket>,
    cancel: CancellationToken,
    audio_sink: Arc<Mutex<Option<AudioSink>>>,
    client_rx: broadcast::Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>>,
    max_log_level: LevelFilter,
) -> Result<()> {
    // Tell the client how verbose we are so it stops generating records we
    // would discard anyway. Best effort: a session that has already gone is a
    // session whose next message cannot be delivered either, and the pumps below
    // report its end.
    let notice = ServerDatagram::LogLevel {
        level: max_log_level,
    }
    .to_bytes();
    if let Err(err) = connection.send_datagram(&notice) {
        log::debug!("could not send the log level notice: {err:#?}");
    }

    // Own the application audio sink at the *session* level (not inside the
    // video capture), so video context resets / display resizes never disturb
    // it.  It is created lazily by session startup's audio task once the
    // client's AudioContext starts, named by the `virtual_speaker_name`
    // template (e.g. `alice-webshooter`), and torn down when the session ends
    // via the session's disconnect token.

    // Encoded video, from the capture pipeline to the MoQ publisher. Created here
    // rather than inside `capture` so the media pump — which owns the publisher
    // and must be running before anything can be appended to a track — is started
    // once, in front of capture, instead of being built around whatever receiver
    // capture happens to hand back.
    //
    // Eight slots is deliberate: the publisher copies each frame into a group
    // immediately, so the only thing a longer wait buys is memory pinned on
    // encoder output, and a frame that has been waiting that long is a frame the
    // client could not use anyway.
    let (frame_tx, frame_rx) = mpsc::channel::<video::EncodedFrame>(8);
    let mut media = media_pump(
        moq,
        server_msg_tx.clone(),
        connection.clone(),
        frame_rx,
        audio_rx,
        cancel.clone(),
    );
    let mut control_sender = control_sender(control_rx, connection.clone());

    // Race start_capture against session cancellation so a
    // refresh/disconnect while waiting for the initial resize doesn't leave a
    // zombie capture; a peer closure reaches this via the supervisor (a pump
    // ends, which cancels the session token).  A capture error is logged
    // rather than bailed: every exit still flows through the teardown below.
    let mut capture = tokio::spawn(video::capture(
        client_rx,
        decoder_caps.clone(),
        virtual_display,
        cancel.clone(),
        server_msg_tx,
        frame_tx,
    ));
    tokio::select! {
        _ = cancel.cancelled() => { log::info!("Disconnect requested"); }
        _ = &mut media => { log::info!("media pipeline stopped"); }
        _ = &mut control_sender => { log::info!("control sender stopped"); }
        started = &mut capture => {
            match started {
                Ok(Ok(())) => log::info!("capture pipeline stopped"),
                Ok(Err(err)) => log::error!("capture failed: {err:#?}"),
                Err(err) => log::error!("capture task panicked: {err}"),
            }
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
/// create the PipeWire audio sink and hand its packets to the media pump.
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
    media_tx: mpsc::Sender<AudioPacket>,
    audio_cancel: CancellationToken,
    audio_sink: Arc<Mutex<Option<AudioSink>>>,
    volume_tx: mpsc::Sender<u8>,
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
        match start_audio_sink(
            audio_cancel.clone(),
            session_name,
            channels,
            rate,
            volume_tx,
        )
        .await
        {
            Ok((sink, mut rx)) => {
                println!("[audio] client ready — created PipeWire audio sink");
                // Packets go to the media pump as they are: the encoder already
                // produced a self-contained Opus packet, so there is nothing to
                // frame, split or stamp on this side any more — MoQ's group is the
                // only framing left. Forwarding rather than sharing the sender is
                // what keeps `start_audio_sink` independent of where media ends up.
                spawn(async move {
                    while let Some(packet) = rx.recv().await {
                        if media_tx.send(packet).await.is_err() {
                            // The media pump is gone, so is the session.
                            return;
                        }
                    }
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

/// Forward everything the client sends that is *ours* to the message bus.
///
/// The mux pump ([`moq`]) owns the connection's reads and routes each message by
/// its first byte; this is the other end of the webshooter half of that split.
/// Both carriers are drained here rather than in two tasks, because one task
/// draining both is what makes "the mux pump is the session's only reader" a
/// property of the code rather than a convention: there is exactly one place that
/// touches the app side of the mux.
///
/// Unidirectional streams are read to their end and parsed as one message. A
/// stream carries a whole message by construction — the end of the stream *is*
/// the delimiter — which is why the messages that must not be lost travel this
/// way rather than as datagrams, and why no framing is needed on top.
fn client_pump(
    app: AppSide,
    client_tx: broadcast::Sender<ClientDatagram>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
        // Input datagrams are flood-limited before the full parse + broadcast;
        // control datagrams (keepalive, resize, keyframe, decoder caps, …)
        // are always parsed and forwarded.
        let mut flood = InputFloodGuard::new();
        let mut received: u64 = 0;
        // Armed before the first message and rearmed by each one. A silent client
        // ends the session; see `KEEPALIVE_TIMEOUT` for why this is generous
        // rather than tight.
        // Pinned because `Sleep` is `!Unpin` and `select!` re-polls it by
        // reference; re-arming it goes through `as_mut().reset(..)`.
        let quiet = tokio::time::sleep(KEEPALIVE_TIMEOUT);
        tokio::pin!(quiet);
        let mut quiet = quiet.as_mut();
        loop {
            tokio::select! {
                biased;
                _ = &mut quiet => {
                    log::info!(
                        "client pump ended after {received} datagrams: \
                         nothing from the client for {KEEPALIVE_TIMEOUT:?}"
                    );
                    break;
                }
                datagram = app.recv_datagram() => {
                    // `None` is the mux pump saying the connection is finished.
                    // It is not a slow connection — that is `Pending`, and it never
                    // arrives here.
                    let Some(datagram) = datagram else { break };
                    received += 1;
                    quiet.as_mut().reset(tokio::time::Instant::now() + KEEPALIVE_TIMEOUT);
                    // Cheap byte pre-filter: skip floods without paying for the
                    // full `from_bytes` parse. Extra datagrams simply linger in
                    // the (bounded) QUIC receive buffer until dropped.
                    if datagram.first().is_some_and(|&b| is_input_byte(b)) && !flood.allow_input() {
                        continue;
                    }
                    if let Ok(datagram) = ClientDatagram::from_bytes(&datagram) {
                        let _ = client_tx.send(datagram);
                    }
                }
                stream = app.recv_unistream() => {
                    let Some(stream) = stream else { break };
                    quiet.as_mut().reset(tokio::time::Instant::now() + KEEPALIVE_TIMEOUT);
                    // The mux pump has already read the stream's first byte — that
                    // is how it decided the stream was ours — and handed it back, so
                    // the message parses as if nothing had touched it.
                    //
                    // Bounded because the read is the one place this task awaits
                    // something the peer controls: the pump only guarantees a *first*
                    // byte, so a client that sends one and then stops would otherwise
                    // wedge the whole session's input behind it.
                    match time::timeout(KEEPALIVE_TIMEOUT, stream.read_to_end()).await {
                        Ok(Ok(bytes)) => {
                            if let Ok(datagram) = ClientDatagram::from_bytes(&bytes) {
                                let _ = client_tx.send(datagram);
                            }
                        }
                        Ok(Err(err)) => log::debug!("client unistream failed: {err}"),
                        Err(_) => log::debug!("client unistream sent a byte and then nothing"),
                    }
                }
            }
        }
        log::info!("client pump ended after {received} datagrams");
    })
}

/// Put control messages on the wire.
///
/// Control messages stay on their own datagrams and their own streams. They used
/// to share the video forwarder's send loop, which made the throttle that protects
/// the session apply to the messages the session needs to keep working — the
/// opposite of what a throttle is for. Giving them a task of their own means a
/// saturated media path cannot delay a `Throttle`, and it means a media path that
/// dies entirely takes nothing else with it.
fn control_sender(
    mut control_rx: mpsc::Receiver<ServerDatagram>,
    connection: Arc<Connection>,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
        while let Some(msg) = control_rx.recv().await {
            let bytes = msg.to_bytes();
            // A datagram is the right carrier here and not merely the convenient
            // one: every control message is small, latency-sensitive, and worthless
            // if it arrives late. The reliable alternative is reserved for
            // `AudioLevel`, which is not.
            if connection.send_datagram(&bytes).is_err() {
                log::warn!("send_datagram (control) failed: connection closed");
                break;
            }
        }
    })
}

/// Drain encoded video and audio into the MoQ publisher.
///
/// One task owns the [`Publisher`], so the track and group state behind it needs
/// no lock and the two streams of media are interleaved by the publisher rather
/// than by whoever won a race for it. Nothing here writes to the transport: the
/// publisher hands frames to the model and moq-net decides what that costs on the
/// wire, including dropping whatever has fallen behind the live edge.
fn media_pump(
    moq: WtSession,
    control_tx: mpsc::Sender<ServerDatagram>,
    connection: Arc<Connection>,
    mut frame_rx: mpsc::Receiver<video::EncodedFrame>,
    mut audio_rx: mpsc::Receiver<AudioPacket>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn(async move {
        // The handshake waits for the client's SETUP, which it sends as soon as the
        // session is up. It is raced against cancellation because it is the one
        // await in this task that waits on the peer rather than on the pipeline: a
        // peer that connects and never says anything must not hold the task (and
        // with it the media pumps behind it) open until QUIC's idle timeout.
        let mut publisher = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                log::info!("media pipeline stopped before the MoQ handshake");
                return;
            }
            started = Publisher::start(moq, control_tx) => match started {
                Ok(publisher) => publisher,
                Err(err) => {
                    log::warn!("MoQ handshake failed, no media will flow: {err:#}");
                    return;
                }
            },
        };

        let mut link = LinkReport::default();
        let mut report = tokio::time::interval(Duration::from_secs(5));
        report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(report);
        loop {
            tokio::select! {
                biased;
                // Ahead of the media arms deliberately: with `biased` the first ready
                // arm wins, and while frames are always available this one would
                // never be polled at all — exactly when the report matters most.
                _ = report.tick() => report_link(&connection, &mut link),
                frame = frame_rx.recv() => {
                    // `None` is the capture pipeline ending, which ends the session:
                    // there is no video left to publish.
                    let Some(frame) = frame else { break };
                    // The buffer is mapped rather than copied: the publisher takes
                    // the bytes synchronously, so the mapping never outlives this
                    // arm and never pins encoder memory across an await.
                    let Ok(mapped) = frame.data.map_readable() else {
                        // A frame that cannot be read is dropped, and the group
                        // keeps going: cutting it here would make the *next* group
                        // start on a delta, which is the one thing a group must
                        // never do. The GOP ends at its next keyframe either way,
                        // and that keyframe is what the client decodes from.
                        log::debug!("encoded frame unreadable, cutting the video group");
                        continue;
                    };
                    if let Err(err) = publisher.push_video(VideoFrame {
                        payload: mapped.as_slice(),
                        codec: frame.codec,
                        is_keyframe: frame.is_keyframe,
                    }) {
                        // A track that has rejected a frame cannot be appended to
                        // again without producing a stream the client cannot decode,
                        // so this ends the media path rather than the frames' worth.
                        log::warn!("MoQ video track failed: {err:#}");
                        break;
                    }
                }
                packet = audio_rx.recv() => {
                    // The audio capture is created only once the client's
                    // AudioContext reports itself, and it may never be, so this
                    // receiver simply never yields. That is not an error.
                    let Some(packet) = packet else { break };
                    if packet.data.is_empty() {
                        continue;
                    }
                    if let Err(err) = publisher.push_audio(AudioFrame { payload: &packet.data }) {
                        log::warn!("MoQ audio track failed: {err:#}");
                        break;
                    }
                }
            }
        }
        log::info!("media pipeline stopped");
    })
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
    let congestion = path.congestion_events.wrapping_sub(last.last_congestion);
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

/// Send an AudioLevel message on a dedicated unidirectional stream.
async fn send_audio_level(wt: &Connection, level: u8) -> Result<()> {
    let bytes = ServerDatagram::AudioLevel { level }.to_bytes();
    let mut stream = wt.open_uni().await?.await?;
    stream.write_all(&bytes).await?;
    stream.finish().await?;
    Ok(())
}

/// Forward the host's volume setting for this session's virtual sink to the
/// client as [`ServerDatagram::AudioLevel`] on a reliable stream.
///
/// `levels` is fed by a PipeWire listener on the sink node, so it starts
/// reporting the sink's actual value as soon as the sink exists and then every
/// time the user changes it. Deduplicating here rather than at the source means
/// a value repeated by PipeWire (a re-enumeration, an unrelated param change)
/// does not become a stream per repetition.
async fn monitor_host_volume(
    wt: Arc<Connection>,
    mut levels: mpsc::Receiver<u8>,
    cancel: CancellationToken,
) {
    let mut last_level: Option<u8> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            level = levels.recv() => {
                // A closed channel means the sink is gone, which ends the
                // session that owned it; there is nothing left to report.
                let Some(level) = level else { break };
                if last_level == Some(level) {
                    continue;
                }
                last_level = Some(level);
                if let Err(e) = send_audio_level(&wt, level).await {
                    log::debug!("audio: failed to send AudioLevel: {e}");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    /// Two sessions of one user must not both be named `alice-webshooter`: they
    /// would collide on a single PipeWire sink and make the `.monitor` source the
    /// capture opens ambiguous. The *first* session keeps the plain name — that
    /// is the one the user's own volume control is pointed at — and only a second
    /// one is disambiguated. A template that spells `{{ client_id }}` out never
    /// needs disambiguating, because it is the thing doing the naming.
    #[test]
    fn only_a_non_primary_session_has_the_client_id_folded_into_its_name() {
        let plain = NameTemplate::parse("{{ name }}-webshooter").unwrap();
        assert_eq!(
            name_for(&plain, "alice", 1, /* is_primary = */ true),
            "alice"
        );
        assert_eq!(
            name_for(&plain, "alice", 2, /* is_primary = */ false),
            "alice-2"
        );

        let self_naming = NameTemplate::parse("{{ client_id }}-{{ name }}").unwrap();
        for is_primary in [true, false] {
            assert_eq!(
                name_for(&self_naming, "alice", 3, is_primary),
                "alice",
                "a template that names itself is already unique"
            );
        }

        // A user name that is not a safe identifier is sanitised before it is
        // rendered, so the template's literal text is the author's business.
        assert_eq!(name_for(&plain, "Alice O'Brien", 1, true), "Alice_O_Brien");
    }
}
