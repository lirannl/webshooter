use crate::config::Config;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use shared::server_datagram::ServerDatagram;
use std::{
    collections::HashMap,
    env, fmt::Display, io::ErrorKind, path::PathBuf, process::exit, str::FromStr,
    sync::{Arc, LazyLock},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, stdin};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum IPCMessage {
    Exit,
    Authorise(Option<usize>),
    Deauthorise(Option<usize>),
    ReleaseMouse,
    FullscreenToggle(Option<usize>),
}

impl IPCMessage {
    pub fn parse_args(args: impl Iterator<Item = impl Display>) -> Result<IPCMessage> {
        let args = args
            .map(|arg| arg.to_string().to_lowercase())
            .collect::<Vec<_>>();
        match args.iter().map(|arg| &**arg).collect::<Vec<&str>>()[..] {
            ["exit"] => Ok(IPCMessage::Exit),
            ["authorise"] => Ok(IPCMessage::Authorise(None)),
            ["authorise", n] if let Ok(n) = n.parse() => Ok(IPCMessage::Authorise(Some(n))),
            ["deauthorise"] => Ok(IPCMessage::Deauthorise(None)),
            ["deauthorise", n] if let Ok(n) = n.parse() => Ok(IPCMessage::Deauthorise(Some(n))),
            ["release_mouse"] => Ok(IPCMessage::ReleaseMouse),
            ["mouse_release"] => Ok(IPCMessage::ReleaseMouse),
            ["fullscreen"] => Ok(IPCMessage::FullscreenToggle(None)),
            ["fullscreen", n] if let Ok(n) = n.parse() => Ok(IPCMessage::FullscreenToggle(Some(n))),
            ["fullscreen_toggle"] => Ok(IPCMessage::FullscreenToggle(None)),
            ["fullscreen_toggle", n] if let Ok(n) = n.parse() => {
                Ok(IPCMessage::FullscreenToggle(Some(n)))
            }
            ["toggle_fullscreen"] => Ok(IPCMessage::FullscreenToggle(None)),
            ["toggle_fullscreen", n] if let Ok(n) = n.parse() => {
                Ok(IPCMessage::FullscreenToggle(Some(n)))
            }
            _ => bail!(
                "Webshooter supports the following commands while running:
    authorise
    deauthorise
    release_mouse
    fullscreen
    exit"
            ),
        }
    }
}

#[cfg(target_os = "linux")]
use tokio::net::UnixStream;

pub enum IPCConnection {
    StdOut,
    #[cfg(target_os = "linux")]
    Unix(UnixStream),
}

impl IPCConnection {
    pub async fn write(&mut self, str: &str) -> std::io::Result<()> {
        match self {
            Self::StdOut => {
                for line in str.lines() {
                    println!("{line}");
                }
                Ok(())
            }
            #[cfg(target_os = "linux")]
            Self::Unix(writer) => writer.write_all(str.as_bytes()).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Connected-client session registry
//
// Every live WebTransport client is represented by a `Session`.  The live set
// is the current value of a `watch` channel: readers take a brief read lock
// (`borrow`), writers mutate in place through `send_if_modified`, which runs
// the mutation under the channel's write lock and pokes subscribers in the
// same step.  Connecting and disconnecting are rare, so serialising the whole
// registry for the duration of a mutation is the right trade: this code is
// about concurrency, not parallelism.
//
// Subscribers (the desktop tray) are *poked*, not given data: the watch
// carries the registry itself, and a notification means "the registry changed
// — re-read it".  Pokes are emitted as part of the store, and each read takes
// a fresh owned snapshot, so a wake can never observe a pre-change registry.
// Rapid changes coalesce into a single wake, which is harmless: the next
// render reflects the latest snapshot, not a per-event delta.
//
// A `Session` owns everything needed to tear the connection down: the control
// channel, a `CancellationToken` for a forced-disconnect, and the `JoinHandle`
// of the supervisor that races every task driving the session (the driver
// itself holds the transport).  Teardown is initiated by async cancellation —
// a task finishing, a forced disconnect, or the transport dying — and the
// driver closes the transport, drops the audio sink and deregisters as its
// *last* act (the supervisor's deregistration is idempotent with it).
// `Session::drop` therefore never remediates: it asserts that the token was
// already cancelled, so removing a client from the registry can only ever be
// the final step of a wind-down it was already told to do.
//
// Client ids are globally unique across all connected sessions: each new
// session gets the lowest id not currently in use, so no two live sessions
// ever share a map key (and thus never overwrite each other).
// ---------------------------------------------------------------------------

pub type ClientId = u64;

/// A connected client, plus the supervisor racing the tasks driving its
/// session.
///
/// Session startup creates every long-lived task up-front — `datagrams` /
/// `unistreams` feed the client's transport in, `client_events` and `audio`
/// react to messages on the broadcast bus, `driver` owns capture negotiation,
/// the frame forwarder and the run loop — and hands them to a supervisor that
/// races them all.  The first task to end has ended the session: the
/// supervisor cancels the session token and deregisters.  A registered session
/// is therefore always complete, and `Session` holds exactly one task: the
/// supervisor.
pub struct Session {
    pub id: ClientId,
    pub display_name: String,
    control_tx: tokio::sync::mpsc::Sender<ServerDatagram>,
    disconnect: CancellationToken,
    // Held so the supervisor lives exactly as long as this session; teardown
    // is driven by the disconnect token, so the supervisor is never joined or
    // aborted from here.
    #[expect(dead_code)]
    supervisor: JoinHandle<()>,
}

impl Session {
    /// A clone of the token that, when cancelled, tears this session down.
    pub fn disconnect(&self) -> CancellationToken {
        self.disconnect.clone()
    }

    /// Route a `ServerDatagram` into this session's control channel.
    pub fn send(&self, msg: ServerDatagram) {
        let _ = self.control_tx.try_send(msg);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A tripwire, not a net: teardown is driven by explicit async
        // cancellation (the driver's run loop) and the token is cancelled
        // before this ever runs.  An un-cancelled drop here is a bug — an
        // authorisation path skipped its teardown — so make it loud instead of
        // silently remediating by cancelling from the destructor.
        if !self.disconnect.is_cancelled() {
            log::error!("session {} dropped without the cancel token cancelled", self.id);
            debug_assert!(
                self.disconnect.is_cancelled(),
                "session {} dropped without async cancellation",
                self.id
            );
        }
    }
}

/// The connected-client registry.  The state is the watch channel's current
/// value: writers mutate it in place via `send_if_modified` and readers take
/// momentary `borrow()` read locks.  A single object therefore provides the
/// snapshot, serialises mutations, and notifies subscribers in one place.
type RegistrySnapshot = HashMap<ClientId, Arc<Session>>;
static REGISTRY: LazyLock<watch::Sender<RegistrySnapshot>> =
    LazyLock::new(|| watch::Sender::new(HashMap::new()));

/// Subscribe to connected-client change notifications.  The value carried is
/// the registry itself; the current state is always re-read from it afterwards
/// via [`list_clients`].
pub fn subscribe_registry_changes() -> watch::Receiver<RegistrySnapshot> {
    REGISTRY.subscribe()
}

/// Mutate the registry: the mutation runs under the watch's write lock and the
/// change notification is sent in the same step, so a wake can never observe a
/// pre-change registry.
fn mutate_registry(mutate: impl FnOnce(&mut RegistrySnapshot)) {
    REGISTRY.send_if_modified(|registry| {
        mutate(registry);
        true
    });
}

/// Reserve the lowest id not currently in use for a session still being
/// constructed.  Only the accept loop allocates ids, so between reservation
/// and [`register_session`] the id cannot be taken again — removals only ever
/// *free* ids, and only registration occupies them.
pub fn next_client_id() -> ClientId {
    let registry = REGISTRY.borrow();
    let mut id = 1u64;
    while registry.contains_key(&id) {
        id += 1;
    }
    id
}

/// Register a fully-constructed session.
///
/// The id must come from [`next_client_id`]: the lowest id not currently in
/// use, and because only the accept loop registers, no other session can be
/// allocated that id in between.  The session (including its supervisor,
/// racing every task that must keep running) is created before registration
/// and inserted atomically, so the moment the registry holds it, the client is
/// live and addressable.
pub fn register_session(
    id: ClientId,
    display_name: String,
    control_tx: tokio::sync::mpsc::Sender<ServerDatagram>,
    disconnect: CancellationToken,
    supervisor: JoinHandle<()>,
) {
    let session = Arc::new(Session {
        id,
        display_name,
        control_tx,
        disconnect,
        supervisor,
    });
    REGISTRY.send_if_modified(|registry| {
        debug_assert!(!registry.contains_key(&id), "client id {id} already in use");
        registry.insert(id, Arc::clone(&session));
        true
    });
}

/// Remove a session from the registry.  Callers must already have cancelled the
/// session's token and torn the connection down; this is the *last* step of a
/// wind-down.  `Session::drop` asserts that cancellation happened.
pub fn remove_session(id: ClientId) {
    mutate_registry(|registry| {
        registry.remove(&id);
    });
}

/// Snapshot of currently connected clients for UI (tray menu) rendering.
pub fn list_clients() -> Vec<(ClientId, String)> {
    REGISTRY
        .borrow()
        .values()
        .map(|s| (s.id, s.display_name.clone()))
        .collect()
}

/// Broadcast a control datagram to every connected client.
pub fn send_client_control(msg: ServerDatagram) {
    let registry = REGISTRY.borrow();
    for session in registry.values() {
        session.send(msg.clone());
    }
}

/// Route a control datagram to a single client.
pub fn send_client_control_to(id: ClientId, msg: ServerDatagram) {
    let registry = REGISTRY.borrow();
    if let Some(session) = registry.get(&id) {
        session.send(msg);
    }
}

/// Force a client to disconnect by cancelling its token and removing it from
/// the registry.  The registry is the only `Arc<Session>` holder: removing the
/// entry drops it, and `Session::drop` asserts the token is already cancelled.
/// The driver task wakes on the cancellation and finishes the wind-down
/// (transport close, audio-sink teardown); its own `remove_session()` call is
/// then a no-op.  A disconnected session is therefore always removed from the
/// collection — there is no way to disconnect while leaving a stale entry
/// behind.
pub fn disconnect_client(id: ClientId) {
    let token = REGISTRY.borrow().get(&id).map(|s| s.disconnect());
    if let Some(token) = token {
        token.cancel();
    }
    remove_session(id);
}

mod ipc_funcs {
    use std::sync::OnceLock;

    use anyhow::Result;
    use async_channel::bounded;

    use crate::{WebshooterError, ipc::IPCMessage};

    use super::IPCConnection;

    static IPC: OnceLock<(
        async_channel::Sender<Option<(IPCMessage, IPCConnection)>>,
        async_channel::Receiver<Option<(IPCMessage, IPCConnection)>>,
    )> = Default::default();

    pub async fn ipc_init() -> () {
        let (tx, rx) = bounded(1);
        tx.send(None)
            .await
            .expect("Webshooter failed to initialise IPC");
        let _ = IPC.set((tx, rx));
    }

    pub async fn ipc_recv() -> Result<(IPCMessage, IPCConnection)> {
        loop {
            match IPC
                .get()
                .ok_or(WebshooterError::IPCNotAvailable)?
                .1
                .recv()
                .await?
            {
                None => {}
                Some(recv) => break Ok(recv),
            }
        }
    }

    // Always attempt to block the channel after
    pub fn ipc_send(message: IPCMessage, connection: IPCConnection) -> Result<()> {
        let sender = &IPC.get().ok_or(WebshooterError::IPCNotAvailable)?.0;
        sender.try_send(Some((message, connection)))?;
        let _ = sender.try_send(None);
        Ok(())
    }
}
pub use ipc_funcs::{ipc_recv, ipc_send};

pub const IPC_ID: &str = include_str!("../../ipc_id.txt");

#[cfg(target_os = "linux")]
pub async fn setup_ipc(_config: Config) -> Result<()> {
    let target = env::var("XDG_RUNTIME_DIR")?;
    let target = PathBuf::from_str(&target)?.join(format!("webshooter_{IPC_ID}.sock",));
    use std::process::exit;
    use tokio::{
        fs::remove_file,
        net::{UnixListener, UnixStream},
    };

    use crate::auth::get_challenged_sessions;

    let my_uid = unsafe { libc::getuid() };

    let listener = match UnixListener::bind(&target) {
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            let connection = UnixStream::connect(&target).await;
            if let Err(err) = &connection
                && err.kind() == ErrorKind::ConnectionRefused
            {
                remove_file(&target).await?;
                UnixListener::bind(&target)
            } else {
                let mut connection = connection?;
                let ipcmessage =
                    IPCMessage::parse_args(env::args().skip(1)).unwrap_or_else(|err| {
                        eprintln!("{err:?}");
                        exit(1)
                    });
                connection.try_write(&serde_json::to_vec(&ipcmessage)?)?;
                connection.flush().await?;
                let mut str = String::new();
                connection.read_to_string(&mut str).await?;
                println!("{str}");
                exit(0)
            }
        }
        listener => listener,
    }?;

    ipc_funcs::ipc_init().await;
    stdio_setup();
    tokio::spawn(async move {
        loop {
            let listener = &listener;
            (async || {
                let (mut conn, _) = listener.accept().await?;

                // Verify the connecting process runs as the same user
                {
                    use std::os::unix::io::AsRawFd;
                    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                    let rc = unsafe {
                        libc::getsockopt(
                            conn.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_PEERCRED,
                            &mut cred as *mut _ as *mut libc::c_void,
                            &mut len,
                        )
                    };
                    if rc != 0 || cred.uid != my_uid {
                        let _ = conn.write(b"Rejected: not same user").await;
                        return Ok(());
                    }
                }

                let mut buf = Vec::new();
                conn.read_buf(&mut buf).await?;
                let message = serde_json::from_slice(&mut buf)?;

                let response = match message {
                    IPCMessage::Authorise(_) => {
                        if get_challenged_sessions().await.is_empty() {
                            Some("No challenged sessions")
                        } else {
                            None
                        }
                    }
                    IPCMessage::Deauthorise(_) => None,
                    IPCMessage::Exit => {
                        let _ = conn.write(b"Bye!").await;
                        exit(0)
                    }
                    IPCMessage::ReleaseMouse => None,
                    IPCMessage::FullscreenToggle(_) => None,
                };
                if let Some(message) = response {
                    conn.write_all(message.as_bytes()).await?;
                } else {
                    ipc_handler(message, IPCConnection::Unix(conn)).await?;
                }
                Ok(())
            })()
            .await
            .unwrap_or_else(|err: anyhow::Error| eprintln!("IPC failure:\n{err:#?}"));
        }
    });
    Ok(())
}

fn stdio_setup() {
    tokio::spawn(async move {
        let mut stdin = BufReader::new(stdin()).lines();
        while let Some(line) = stdin.next_line().await? {
            match IPCMessage::parse_args(line.split(' ')) {
                Ok(message) => {
                    ipc_handler(message, IPCConnection::StdOut).await?;
                }
                Err(err) => {
                    for line in format!("{err:#?}").lines() {
                        println!("{line}");
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    });
}

fn format_id(id: &[u8]) -> String {
    use data_encoding::BASE64;
    if id.len() >= 32 {
        BASE64.encode(&id[24..32]).trim_matches('=').to_string()
    } else {
        BASE64.encode(id).trim_matches('=').to_string()
    }
}

pub async fn deauthorise(index: Option<usize>, mut conn: IPCConnection) -> Result<()> {
    let sessions = crate::auth::get_challenged_sessions().await;
    let id = match sessions.len() {
        0 => {
            conn.write("No challenged sessions").await?;
            return Ok(());
        }
        1 => sessions.into_iter().next().unwrap(),
        _ => {
            if let Some(n) = index {
                sessions
                    .into_iter()
                    .nth(n)
                    .ok_or_else(|| anyhow::anyhow!("Invalid index"))?
            } else {
                conn.write(&format!(
                    "Please select a session:\n{}",
                    sessions
                        .iter()
                        .enumerate()
                        .map(|(n, s)| format!("{n}: {}", format_id(&s)))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
                .await?;
                return Ok(());
            }
        }
    };
    let short = format_id(&id);
    let mut config = crate::get_config().await;
    if let Some(doomed) = config.users.extract_if(|user| id == *user).last() {
        crate::update_config(config).await?;
        conn.write(&format!("Deauthorised \"{}\"", doomed.display_name))
            .await?;
    } else {
        conn.write(&format!("No user matching key {short}")).await?;
    }
    Ok(())
}

async fn ipc_handler(message: IPCMessage, mut conn: IPCConnection) -> Result<()> {
    match message {
        IPCMessage::Exit => {
            conn.write("Webshooter shutting down").await?;
            exit(0)
        }
        IPCMessage::Deauthorise(idx) => {
            deauthorise(idx, conn).await?;
        }
        IPCMessage::FullscreenToggle(_) => {
            send_client_control(ServerDatagram::ToggleFullscreen);
            conn.write("Fullscreen toggled").await?;
        }
        IPCMessage::ReleaseMouse => {
            send_client_control(ServerDatagram::ReleaseMouse);
            conn.write("Mouse released").await?;
        }
        IPCMessage::Authorise(_) => {
            // Routed to the authorisation flow.
            ipc_send(message, conn)?;
        }
    }
    Ok(())
}
