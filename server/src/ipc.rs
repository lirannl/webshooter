use crate::config::Config;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use shared::server_datagram::ServerDatagram;
use std::{
    collections::HashMap,
    env, fmt::Display, io::ErrorKind, path::PathBuf, process::exit, str::FromStr,
    sync::{Arc, LazyLock, Mutex},
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
// Every live WebTransport client is represented by a `Session` stored in a
// single map keyed by `ClientId`.  A `Session` owns everything needed to tear
// the connection down: the control channel, a `CancellationToken` for a
// forced-disconnect, the `Connection` itself, and the `JoinHandle`s of the
// tasks that drive it.  Dropping a `Session` — which happens automatically
// when it is removed from the map — cancels its tasks and closes the
// connection, so removing a client from the registry cleanly disconnects it.
//
// Client ids are globally unique across all connected sessions: each new
// session gets the lowest id not currently in use, so no two live sessions
// ever share a map key (and thus never overwrite each other).
// ---------------------------------------------------------------------------

pub type ClientId = u64;

pub struct Session {
    pub id: ClientId,
    pub display_name: String,
    control_tx: tokio::sync::mpsc::Sender<ServerDatagram>,
    disconnect: CancellationToken,
    connection: Arc<wtransport::Connection>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Session {
    /// A clone of the control channel used to route `ServerDatagram`s to this
    /// client (e.g. fullscreen toggles, mouse release).
    pub fn control_tx(&self) -> tokio::sync::mpsc::Sender<ServerDatagram> {
        self.control_tx.clone()
    }

    /// A clone of the token that, when cancelled, tears this session down.
    pub fn disconnect(&self) -> CancellationToken {
        self.disconnect.clone()
    }

    /// Track a task that belongs to this session so it is cancelled on drop.
    pub fn add_task(&self, handle: JoinHandle<()>) {
        self.tasks.lock().unwrap().push(handle);
    }

    /// Route a `ServerDatagram` into this session's control channel.
    pub fn send(&self, msg: ServerDatagram) {
        let _ = self.control_tx.try_send(msg);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Cancel every task owned by this session (capture, forwarder, …).
        self.disconnect.cancel();
        // Close the connection so transport-level tasks also wind down.
        let _ = self
            .connection
            .close(wtransport::VarInt::from_u32(0), b"done");
    }
}

static REGISTRY: LazyLock<Mutex<HashMap<ClientId, Arc<Session>>>> =
    LazyLock::new(Default::default);

/// Bumped every time a session is registered or removed so subscribers (e.g.
/// the desktop tray) know the connected-client list has changed and can
/// re-render their menu.
static REGISTRY_VERSION: LazyLock<watch::Sender<u64>> =
    LazyLock::new(|| watch::channel(0).0);

/// Subscribe to notifications about connected-client changes.  The value is a
/// monotonically increasing revision; it changes whenever the registry does.
pub fn subscribe_registry_changes() -> watch::Receiver<u64> {
    REGISTRY_VERSION.subscribe()
}

fn notify_registry_changed() {
    let _ = REGISTRY_VERSION.send_if_modified(|rev| {
        *rev += 1;
        true
    });
}

/// Register a new session and assign it the lowest free id.
///
/// Ids must be globally unique across *all* sessions: the registry is keyed by
/// the bare `ClientId`, so if two different users both got `id == 1` the second
/// `insert` would overwrite the first session and hide it from the tray/menu
/// (while its background tasks kept running, keeping its audio sink alive).
pub fn register_session(
    display_name: String,
    control_tx: tokio::sync::mpsc::Sender<ServerDatagram>,
    disconnect: CancellationToken,
    connection: Arc<wtransport::Connection>,
) -> Arc<Session> {
    let mut registry = REGISTRY.lock().unwrap();
    let mut id = 1u64;
    while registry.contains_key(&id) {
        id += 1;
    }
    let session = Arc::new(Session {
        id,
        display_name,
        control_tx,
        disconnect,
        connection,
        tasks: Mutex::new(Vec::new()),
    });
    registry.insert(id, Arc::clone(&session));
    notify_registry_changed();
    session
}

/// Remove a session from the registry.  Dropping the last `Arc<Session>`
/// cancels its tasks and closes the connection.
pub fn remove_session(id: ClientId) {
    REGISTRY.lock().unwrap().remove(&id);
    notify_registry_changed();
}

/// Snapshot of currently connected clients for UI (tray menu) rendering.
pub fn list_clients() -> Vec<(ClientId, String)> {
    REGISTRY
        .lock()
        .unwrap()
        .iter()
        .map(|(id, s)| (*id, s.display_name.clone()))
        .collect()
}

/// Broadcast a control datagram to every connected client.
pub fn send_client_control(msg: ServerDatagram) {
    let registry = REGISTRY.lock().unwrap();
    for session in registry.values() {
        session.send(msg.clone());
    }
}

/// Route a control datagram to a single client.
pub fn send_client_control_to(id: ClientId, msg: ServerDatagram) {
    let registry = REGISTRY.lock().unwrap();
    if let Some(session) = registry.get(&id) {
        session.send(msg);
    }
}

/// Force a client to disconnect by cancelling its token and removing it from
/// the registry.  Removing the entry drops the registry's `Arc<Session>`; the
/// remaining `Arc` (held by the connection task) is released once the token
/// cancellation winds the session down, which triggers `Session::drop` and
/// closes the connection.  A disconnected session is therefore always removed
/// from the collection — there is no way to disconnect while leaving a stale
/// entry behind.
pub fn disconnect_client(id: ClientId) {
    let token = REGISTRY.lock().unwrap().get(&id).map(|s| s.disconnect());
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
