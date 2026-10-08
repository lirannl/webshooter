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
    pipewire::bitrate::LinkPressure,
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
use tokio::sync::watch;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
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

/// Input refusals tolerated inside [`INPUT_FLOOD_KICK_WINDOW`] before the
/// session is dropped instead of merely throttled.
///
/// [`InputFloodGuard`] admits [`INPUT_FLOOD_RATE_PER_SEC`] datagrams a second,
/// so a refusal can only be produced by a client sending *more* than that. The
/// client caps itself at [`shared::throttle::INPUT_MIN_INTERVAL_MS`] — 250
/// datagrams a second — and so never accumulates a single one. Reaching this
/// many refusals inside one window means attempting at least 5,000 input
/// datagrams a second, twenty times the client's own cap: no burst arrives
/// there by accident, while a client that ignores the cap is gone within a
/// second of starting instead of holding a core for as long as it pleases.
const INPUT_FLOOD_KICK_REFUSALS: u32 = 1_000;

/// The window [`INPUT_FLOOD_KICK_REFUSALS`] is counted over. A second is long
/// enough that refusals from an honest client — which the client cap makes
/// impossible in the first place — would have to arrive at the flood's own rate
/// to matter, and short enough that a flooder is gone within a second of
/// starting rather than holding a core for as long as it cares to.
const INPUT_FLOOD_KICK_WINDOW: Duration = Duration::from_secs(1);

/// How long the pump stays open after queueing the disconnect notice. The
/// notice rides `control_sender`, a *peer* future in `run_session`'s
/// `select!`: it is only polled while this future is pending, so breaking out
/// immediately would tear the session down with the notice still sitting in
/// the channel and leave the user with a silent disconnect. One sleep is
/// enough to drain it, and a quarter second is invisible on the way out — even
/// under the load that caused the kick.
const KICK_NOTICE_DRAIN: Duration = Duration::from_millis(250);

/// Tokio detaches a JoinHandle on drop. These transport children must instead
/// stop whenever the inline future that owns them is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl AbortOnDrop {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(handle)
    }
}

impl std::future::Future for AbortOnDrop {
    type Output = Result<(), tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

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

/// Windowed refusal counter: the difference between "a burst the bucket could
/// not cover" and "a client that is flooding", and so the thing that decides
/// when a session is dropped rather than merely throttled.
///
/// [`InputFloodGuard`] answers "may this one datagram through"; this answers
/// "is this client still trying to flood" — and only over a window, because
/// one bad second must not condemn a client whose next ten are quiet. A
/// throttled client never records anything here at all: it is capped well
/// below the guard's refill rate, which is what makes a non-zero count
/// unambiguous.
struct FloodKick {
    refusals: u32,
    window_start: std::time::Instant,
}

impl FloodKick {
    fn new() -> Self {
        Self {
            refusals: 0,
            window_start: std::time::Instant::now(),
        }
    }

    /// Record one refused input datagram at `now`. Returns `true` when enough
    /// have accumulated inside one window to call the session a flood.
    fn record(&mut self, now: std::time::Instant) -> bool {
        if now.duration_since(self.window_start) >= INPUT_FLOOD_KICK_WINDOW {
            // The window expired before the threshold: this refusal starts a
            // fresh count rather than carrying an old one forward forever.
            self.window_start = now;
            self.refusals = 0;
        }
        self.refusals += 1;
        self.refusals >= INPUT_FLOOD_KICK_REFUSALS
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

        let (control_tx, control_rx) = mpsc::channel::<ServerDatagram>(8);
        // Render names before registering; an invalid template must not take
        // the whole WebTransport endpoint down with one session.
        let config = get_config().await;
        let (virtual_display, virtual_speaker) =
            match render_session_names(&config, &display_name, client_id) {
                Ok(names) => names,
                Err(err) => {
                    log::error!("dropping session {client_id}: {err:#}");
                    continue;
                }
            };

        let session = ipc::Session::new(
            client_id,
            display_name,
            control_tx.clone(),
            connection.clone(),
        );
        // The transport cannot finish (or call remove_session) until it has been
        // installed in the registry. No awaits between registration and release.
        let (start_tx, start_rx) = oneshot::channel();
        let worker = session.clone();
        session.set_transport(tokio::spawn(async move {
            if start_rx.await.is_ok() {
                run_session(
                    worker,
                    connection,
                    virtual_display,
                    virtual_speaker,
                    control_tx,
                    control_rx,
                    max_log_level,
                )
                .await;
            }
        }));
        ipc::register_session(session);
        let _ = start_tx.send(());
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

async fn run_session(
    session: Arc<ipc::Session>,
    connection: Arc<Connection>,
    virtual_display: String,
    virtual_speaker: String,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
    control_rx: mpsc::Receiver<ServerDatagram>,
    max_log_level: LevelFilter,
) {
    let client_id = session.id;
    let cancel = session.token();
    // The mux is the only reader of the transport. Its spawned task and the
    // MoQ driver below are owned by this ancestor, never detached on teardown.
    let (moq, app, mux) = moq::attach(connection.clone());
    let mut mux = AbortOnDrop::new(mux);
    let (client_tx, client_rx) = broadcast::channel::<ClientDatagram>(256);
    let decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>> = Arc::new(Mutex::new(None));
    let (volume_tx, volume_rx) = mpsc::channel::<u8>(8);
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>(256);

    let notice = ServerDatagram::LogLevel {
        level: max_log_level,
    }
    .to_bytes();
    if let Err(err) = connection.send_datagram(&notice) {
        log::debug!("could not send the log level notice: {err:#?}");
    }

    // The media and control pumps and the event consumer are inline futures: a
    // single platform-agnostic transport task polls and owns them all.
    let (frame_tx, frame_rx) = mpsc::channel::<video::EncodedFrame>(8);
    let (pressure_tx, pressure_rx) = watch::channel(LinkPressure::Clear);
    let media = media_pump(
        moq,
        server_msg_tx.clone(),
        connection.clone(),
        pressure_tx,
        frame_rx,
        audio_rx,
        cancel.clone(),
    );
    let control = control_sender(control_rx, connection.clone());
    let input = client_pump(app, client_tx.clone(), server_msg_tx.clone());
    let events = client_events_task(client_rx.resubscribe(), decoder_caps.clone());
    tokio::pin!(media, control, input, events);

    // PipeWire audio and capture are Linux-only tasks. Their handles live on the
    // session in `#[cfg]` fields, never inside the platform-agnostic transport
    // task above. Audio is optional: ending it degrades the session, never ends
    // it, so it is not one of the race arms below.
    session.set_audio(tokio::spawn(audio_ready_task(
        client_rx.resubscribe(),
        virtual_speaker,
        audio_tx,
        cancel.clone(),
        session.audio_sink.clone(),
        volume_tx,
        volume_rx,
        connection.clone(),
    )));
    // The oneshot reports capture completion without moving its handle out of
    // the session.
    let (video_done_tx, mut video_done_rx) = oneshot::channel();
    session.set_video(tokio::spawn(async move {
        let result = video::capture(
            client_rx,
            decoder_caps,
            virtual_display,
            client_id,
            pressure_rx,
            cancel.clone(),
            server_msg_tx,
            frame_tx,
        )
        .await;
        let _ = video_done_tx.send(result);
    }));
    let cancel = session.token();
    // Fair selection matters: an input flood must not starve media or control
    // merely because the input future is listed earlier here.
    let ended = tokio::select! {
        _ = cancel.cancelled() => "disconnect requested",
        outcome = &mut mux => {
            if let Err(err) = outcome { log::warn!("mux pump failed: {err}"); }
            "mux pump"
        }
        _ = &mut input => "client pump",
        _ = &mut events => "client event pump",
        _ = &mut media => "media pipeline",
        _ = &mut control => "control sender",
        outcome = &mut video_done_rx => {
            match outcome {
                Ok(Ok(())) => log::info!("capture pipeline stopped"),
                Ok(Err(err)) => log::error!("capture failed: {err:#?}"),
                Err(err) => log::error!("capture task ended without a result: {err}"),
            }
            "capture pipeline"
        }
    };
    let ended = if cancel.is_cancelled() {
        "disconnect requested"
    } else {
        ended
    };
    log::info!("session {client_id} ended: {ended}");
    // Cancel before removal so every future (and the MoQ driver it owns) winds
    // down. Registry removal closes the connection and tears the stored Linux
    // tasks down, including this transport task. Nothing is awaited after it.
    cancel.cancel();
    drop(mux);
    // By identity, not by `client_id`: this task may be finishing long after
    // its id was freed and handed to a new session, and that session must not
    // be torn down by this one's remains.
    ipc::remove_session(&session);
}

/// Subscribe to the client-message bus: forward client log records into the
/// server log and stash decode caps for the capture pipeline.
async fn client_events_task(
    mut client_rx: broadcast::Receiver<ClientDatagram>,
    decoder_caps: Arc<Mutex<Option<Vec<shared::codec::Codec>>>>,
) {
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
}

/// Audio is optional and Linux-only: failing to start it degrades the session
/// but does not disconnect video. One task covers the whole audio subtree —
/// waiting for `AudioReady`, creating the sink, and forwarding both packets and
/// volume updates — so no second packet-forwarding or volume-monitor task is
/// needed. The session owns this task's handle, not the transport runner.
#[allow(clippy::too_many_arguments)]
async fn audio_ready_task(
    mut audio_rx: broadcast::Receiver<ClientDatagram>,
    session_name: String,
    media_tx: mpsc::Sender<AudioPacket>,
    cancel: CancellationToken,
    audio_sink: Arc<Mutex<Option<AudioSink>>>,
    volume_tx: mpsc::Sender<u8>,
    mut levels: mpsc::Receiver<u8>,
    connection: Arc<Connection>,
) {
    let ready = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            msg = audio_rx.recv() => match msg {
                Ok(ClientDatagram::AudioReady { channels, rate }) => break (channels, rate),
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    };
    // `start_audio_sink` watches the token while it waits for PipeWire. If
    // registration is revoked mid-start, its thread sees the same cancellation.
    let (sink, mut packets) =
        match start_audio_sink(cancel.clone(), session_name, ready.0, ready.1, volume_tx).await {
            Ok(started) => started,
            Err(err) => {
                log::warn!("audio sink unavailable: {err:#}");
                cancel.cancelled().await;
                return;
            }
        };
    *audio_sink.lock().unwrap() = Some(sink);
    let mut last_level = None;
    let mut packets_open = true;
    let mut levels_open = true;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            packet = packets.recv(), if packets_open => match packet {
                Some(packet) => {
                    if media_tx.send(packet).await.is_err() { break; }
                }
                None => packets_open = false,
            },
            level = levels.recv(), if levels_open => match level {
                Some(level) if last_level != Some(level) => {
                    last_level = Some(level);
                    if let Err(err) = send_audio_level(&connection, level).await {
                        log::debug!("audio: failed to send AudioLevel: {err}");
                        levels_open = false;
                    }
                }
                Some(_) => {},
                None => levels_open = false,
            },
        }
        // A closed encoder or monitor does not close the video session.
        if !packets_open && !levels_open {
            break;
        }
    }
    // Never return early just because audio failed: avoid racing cancellation
    // with a second, misleading transport-end reason.
    cancel.cancelled().await;
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
async fn client_pump(
    app: AppSide,
    client_tx: broadcast::Sender<ClientDatagram>,
    server_msg_tx: mpsc::Sender<ServerDatagram>,
) {
    // Input datagrams are flood-limited before the full parse + broadcast;
    // control datagrams (keepalive, resize, keyframe, decoder caps, …)
    // are always parsed and forwarded.
    let mut flood = InputFloodGuard::new();
    // Refusing forever is not a solution: a client that keeps flooding keeps
    // costing the pump the wake-up and the byte test, and the session is one
    // the user is not getting back anyway. Sustained refusals end it instead.
    let mut kick = FloodKick::new();
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
                // the (bounded) QUIC receive buffer until dropped — up to the
                // point where dropping them is not enough.
                if datagram.first().is_some_and(|&b| is_input_byte(b)) && !flood.allow_input() {
                    if kick.record(std::time::Instant::now()) {
                        log::warn!(
                            "dropping session: input flood, {INPUT_FLOOD_KICK_REFUSALS} \
                             datagrams refused in {INPUT_FLOOD_KICK_WINDOW:?} (the pump \
                             admits {INPUT_FLOOD_RATE_PER_SEC}/s)"
                        );
                        // Tell the client why before the session goes; see
                        // KICK_NOTICE_DRAIN for why this sleeps instead of
                        // breaking straight out. `try_send` rather than
                        // `send().await`: a full channel is the notice's
                        // problem, not the teardown's, and awaiting here could
                        // hold the pump open on a peer that is no longer
                        // draining.
                        let _ = server_msg_tx.try_send(ServerDatagram::Error {
                            level: log::Level::Warn,
                            message: format!(
                                "input flood: this session was closed because the client \
                                 sent input faster than the server's {INPUT_FLOOD_RATE_PER_SEC} \
                                 datagrams per second limit"
                            ),
                        });
                        time::sleep(KICK_NOTICE_DRAIN).await;
                        break;
                    }
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
}

/// Put control messages on the wire.
///
/// Control messages stay on their own datagrams and their own streams. They used
/// to share the video forwarder's send loop, which made backpressure on the
/// media path apply to the messages the session needs to keep working — the
/// opposite of what throttling input is for. Giving them a separate inline
/// future means a saturated media path cannot delay a control message; neither
/// future performs a blocking send on the other's channel.
async fn control_sender(
    mut control_rx: mpsc::Receiver<ServerDatagram>,
    connection: Arc<Connection>,
) {
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
}

/// Drain encoded video and audio into the MoQ publisher.
///
/// One task owns the [`Publisher`], so the track and group state behind it needs
/// no lock and the two streams of media are interleaved by the publisher rather
/// than by whoever won a race for it. Nothing here writes to the transport: the
/// publisher hands frames to the model and moq-net decides what that costs on the
/// wire, including dropping whatever has fallen behind the live edge.
async fn media_pump(
    moq: WtSession,
    control_tx: mpsc::Sender<ServerDatagram>,
    connection: Arc<Connection>,
    pressure: watch::Sender<LinkPressure>,
    mut frame_rx: mpsc::Receiver<video::EncodedFrame>,
    mut audio_rx: mpsc::Receiver<AudioPacket>,
    cancel: CancellationToken,
) {
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

    // The driver is polled as a peer of the frame queues. A transport
    // failure ends media immediately; dropping this future aborts the driver.
    let mut driver = AbortOnDrop::new(
        publisher
            .take_driver()
            .expect("Publisher::start supplies a driver"),
    );
    let mut link = LinkReport::default();
    let mut report = tokio::time::interval(Duration::from_secs(5));
    report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(report);
    loop {
        tokio::select! {
            outcome = &mut driver => {
                if let Err(err) = outcome { log::warn!("MoQ driver failed: {err}"); }
                break;
            }
            // Randomized selection gives audio and link reports a turn even
            // when the video queue has frames ready continuously.
            _ = report.tick() => report_link(&connection, &mut link, &pressure),
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
fn report_link(wt: &Connection, last: &mut LinkReport, pressure: &watch::Sender<LinkPressure>) {
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
    // The same verdict that decides whether to log also drives the encoder's
    // bitrate. One reader of the path counters, one verdict: two samplers would
    // be free to disagree about whether the link is congested.
    let _ = pressure.send(if degraded {
        LinkPressure::Congested
    } else {
        LinkPressure::Clear
    });
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
        let task = tokio::spawn(client_events_task(rx, decoder_caps.clone()));

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

    /// The contract the two halves of the input policy make with each other.
    /// The client caps itself at [`shared::throttle::INPUT_MIN_INTERVAL_MS`]
    /// and the guard admits [`INPUT_FLOOD_RATE_PER_SEC`], so the honest case is
    /// twenty times below the line at which a datagram is refused — and a
    /// refusal is the only thing [`FloodKick`] ever counts. This is what makes
    /// "the client is capped, so its own flood attempt cannot kick it off" a
    /// property of the code rather than of timing.
    #[test]
    fn a_client_at_the_shared_rate_is_never_refused() {
        let mut guard = InputFloodGuard::new();
        // Five simulated seconds of the fastest honest client there is: one
        // datagram every floor interval, which is exactly what the client
        // throttle allows.
        let interval = Duration::from_millis(u64::from(
            shared::throttle::INPUT_MIN_INTERVAL_MS,
        ));
        let mut refusals = 0;
        for _ in 0..5_000 / u64::from(shared::throttle::INPUT_MIN_INTERVAL_MS) {
            guard.last_refill -= interval;
            if !guard.allow_input() {
                refusals += 1;
            }
        }
        assert_eq!(
            refusals, 0,
            "a throttled client must never see a datagram refused"
        );
    }

    /// A burst short of the threshold inside one window is not a flood: the
    /// session stays up.
    #[test]
    fn refusals_below_the_threshold_are_tolerated() {
        let mut kick = FloodKick::new();
        let start = kick.window_start;
        for i in 0..INPUT_FLOOD_KICK_REFUSALS - 1 {
            let now = start + Duration::from_millis(u64::from(i));
            assert!(
                !kick.record(now),
                "refusal {i} must not end the session on its own"
            );
        }
    }

    /// A sustained flood does end it, and it does so inside the first window:
    /// the flood is cut off at the threshold rather than at some later point.
    #[test]
    fn a_sustained_flood_ends_the_session() {
        let mut kick = FloodKick::new();
        let start = kick.window_start;
        let mut fired = false;
        for i in 0..INPUT_FLOOD_KICK_REFUSALS {
            fired = kick.record(start + Duration::from_millis(u64::from(i)));
        }
        assert!(
            fired,
            "a whole window of refusals must be treated as a flood"
        );
    }

    /// The count lives in a window, not forever: a client that refuses a
    /// thousand datagrams and then goes quiet starts over, so the session is
    /// not one refusal away from being dropped for the rest of its life.
    #[test]
    fn refusals_do_not_accumulate_across_windows() {
        let mut kick = FloodKick::new();
        let start = kick.window_start;
        for i in 0..INPUT_FLOOD_KICK_REFUSALS - 1 {
            let _ = kick.record(start + Duration::from_millis(u64::from(i)));
        }
        let later = start + INPUT_FLOOD_KICK_WINDOW + Duration::from_secs(5);
        assert!(
            !kick.record(later),
            "an expired window must not inherit its predecessor's count"
        );
        assert_eq!(
            kick.refusals, 1,
            "the refusal that opened the new window is its only member"
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
