use crate::config::Config;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use shared::server_datagram::ServerDatagram;
use std::{
    borrow::Borrow,
    collections::HashSet,
    env,
    fmt::Display,
    hash::{Hash, Hasher},
    io::ErrorKind,
    path::PathBuf,
    process::exit,
    str::FromStr,
    sync::{
        Arc, LazyLock, RwLock,
        atomic::Ordering,
    },
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
        let mut args = args
            .map(|arg| arg.to_string().to_lowercase())
            .collect::<Vec<_>>();

        // Normalise historical command aliases to their canonical spelling so
        // the match below only needs one arm per command.
        if let Some(cmd) = args.first_mut() {
            match cmd.as_str() {
                "toggle_fullscreen" | "fullscreen_toggle" => *cmd = "fullscreen".to_string(),
                "mouse_release" => *cmd = "release_mouse".to_string(),
                _ => {}
            }
        }

        match args.as_slice() {
            [cmd] => match cmd.as_str() {
                "exit" => Ok(IPCMessage::Exit),
                "authorise" => Ok(IPCMessage::Authorise(None)),
                "deauthorise" => Ok(IPCMessage::Deauthorise(None)),
                "release_mouse" => Ok(IPCMessage::ReleaseMouse),
                "fullscreen" => Ok(IPCMessage::FullscreenToggle(None)),
                _ => Err(usage()),
            },
            [cmd, n] => match cmd.as_str() {
                "authorise" => n
                    .parse()
                    .map(|n| IPCMessage::Authorise(Some(n)))
                    .map_err(|_| usage()),
                "deauthorise" => n
                    .parse()
                    .map(|n| IPCMessage::Deauthorise(Some(n)))
                    .map_err(|_| usage()),
                "fullscreen" => n
                    .parse()
                    .map(|n| IPCMessage::FullscreenToggle(Some(n)))
                    .map_err(|_| usage()),
                _ => Err(usage()),
            },
            _ => Err(usage()),
        }
    }
}

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "Webshooter supports the following commands while running:
    authorise
    deauthorise
    release_mouse
    fullscreen
    exit"
    )
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
// The set owns membership; its lock is never held while touching a session's
// state or stopping its tasks. Lookups clone one Arc under a shared read lock.
// A separate watch counter notifies the tray only when membership changes.
// Removal matches on identity — the exact `Arc` — never the number alone, so
// an id freed by a closed session can be handed out again without a late
// teardown disconnecting whoever holds that id now.
// ---------------------------------------------------------------------------

pub type ClientId = u64;

/// The identity and all work belonging to one connected client. The transport
/// task is the ancestor of platform-independent futures. Linux-only work lives
/// in the conditionally compiled fields below, not in the transport handle.
pub struct Session {
    pub id: ClientId,
    pub display_name: String,
    control_tx: tokio::sync::mpsc::Sender<ServerDatagram>,
    disconnect: CancellationToken,
    connection: Arc<wtransport::Connection>,
    transport: std::sync::Mutex<Option<JoinHandle<()>>>,
    #[cfg(target_os = "linux")]
    pub video: std::sync::Mutex<Option<JoinHandle<()>>>,
    #[cfg(target_os = "linux")]
    pub audio: std::sync::Mutex<Option<JoinHandle<()>>>,
    #[cfg(target_os = "linux")]
    pub audio_sink: Arc<std::sync::Mutex<Option<crate::pipewire::audio::AudioSink>>>,
    #[cfg(target_os = "linux")]
    cleanup: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// The runtime this session's tasks run on, captured at creation so removal
    /// can dispose those tasks from any thread (e.g. a tray callback) without
    /// needing to be inside a runtime itself.
    #[cfg(target_os = "linux")]
    runtime: tokio::runtime::Handle,
}

impl Session {
    pub fn new(
        id: ClientId,
        display_name: String,
        control_tx: tokio::sync::mpsc::Sender<ServerDatagram>,
        connection: Arc<wtransport::Connection>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            display_name,
            control_tx,
            disconnect: CancellationToken::new(),
            connection,
            transport: std::sync::Mutex::new(None),
            #[cfg(target_os = "linux")]
            video: std::sync::Mutex::new(None),
            #[cfg(target_os = "linux")]
            audio: std::sync::Mutex::new(None),
            #[cfg(target_os = "linux")]
            audio_sink: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(target_os = "linux")]
            cleanup: std::sync::Mutex::new(None),
            #[cfg(target_os = "linux")]
            runtime: tokio::runtime::Handle::current(),
        })
    }

    pub fn token(&self) -> CancellationToken {
        self.disconnect.clone()
    }

    pub fn set_transport(&self, handle: JoinHandle<()>) {
        let mut task = self.transport.lock().unwrap();
        if self.disconnect.is_cancelled() {
            handle.abort();
        } else {
            assert!(task.is_none(), "transport task already running");
            *task = Some(handle);
        }
    }

    #[cfg(target_os = "linux")]
    pub fn set_video(&self, handle: JoinHandle<()>) {
        let mut task = self.video.lock().unwrap();
        if self.disconnect.is_cancelled() {
            handle.abort();
        } else {
            assert!(task.is_none(), "video task already running");
            *task = Some(handle);
        }
    }

    #[cfg(target_os = "linux")]
    pub fn set_audio(&self, handle: JoinHandle<()>) {
        let mut task = self.audio.lock().unwrap();
        if self.disconnect.is_cancelled() {
            handle.abort();
        } else {
            assert!(task.is_none(), "audio task already running");
            *task = Some(handle);
        }
    }

    /// Route a control message without holding the registry lock.
    pub fn send(&self, msg: ServerDatagram) {
        if !self.disconnect.is_cancelled() {
            let _ = self.control_tx.try_send(msg);
        }
    }

    fn stop(self: Arc<Self>) {
        self.disconnect.cancel();
        self.connection
            .close(wtransport::VarInt::from_u32(0), b"done");
        if let Some(task) = self.transport.lock().unwrap().take() {
            task.abort();
        }
        #[cfg(target_os = "linux")]
        {
            // A capture must close its portal grant and stop its GStreamer
            // pipeline. Aborting it immediately would skip that async cleanup.
            // Keep the cleanup task on this session, never in the registry lock.
            let video = self.video.lock().unwrap().take();
            let audio = self.audio.lock().unwrap().take();
            let owner = Arc::clone(&self);
            let reservation = PendingCleanup;
            let cleanup = self.runtime.spawn(async move {
                // Captured before spawning, so even an unpolled aborted task
                // releases the resource-name reservation.
                let _reservation = reservation;
                async fn finish(task: Option<JoinHandle<()>>) {
                    if let Some(mut task) = task
                        && tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                            .await
                            .is_err()
                    {
                        task.abort();
                        let _ = task.await;
                    }
                }
                tokio::join!(finish(video), finish(audio));
                let sink = owner.audio_sink.lock().unwrap().take();
                if let Some(sink) = sink {
                    let _ = tokio::task::spawn_blocking(move || drop(sink)).await;
                }
            });
            *self.cleanup.lock().unwrap() = Some(cleanup);
        }
    }
}

/// The hash key is the immutable ID, never a mutable field or a task handle.
struct SessionEntry(Arc<Session>);

impl Borrow<ClientId> for SessionEntry {
    fn borrow(&self) -> &ClientId {
        &self.0.id
    }
}

impl PartialEq for SessionEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}
impl Eq for SessionEntry {}
impl Hash for SessionEntry {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.0.id.hash(hasher);
    }
}

static REGISTRY: LazyLock<RwLock<HashSet<SessionEntry>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));
static CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));
#[cfg(target_os = "linux")]
static TEARING_DOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "linux")]
struct PendingCleanup;

#[cfg(target_os = "linux")]
impl Drop for PendingCleanup {
    fn drop(&mut self) {
        TEARING_DOWN.fetch_sub(1, Ordering::Release);
    }
}

pub fn subscribe_registry_changes() -> watch::Receiver<u64> {
    CHANGES.subscribe()
}

/// The lowest id no live session holds — `1` for a lone client, past the live
/// set otherwise — so a client that reconnects finds its own id, and its own
/// tray label, waiting for it instead of starting one higher every time.
///
/// Recycling is sound only because [`remove_session`] decides on identity: a
/// stale teardown naming a recycled id finds no matching `Arc` in the
/// registry and is a no-op. That is the guarantee the monotonic counter used
/// to buy, bought without giving up id reuse.
///
/// Only the accept loop calls this, and it registers the session without
/// accepting another connection in between, so two sessions cannot scan their
/// way to the same id.
pub fn next_client_id() -> ClientId {
    let registry = REGISTRY.read().unwrap();
    lowest_free_id(|id| registry.contains(&id))
}

/// The least positive id `in_use` reports as taken. Split out so the
/// allocation rule is testable without a live [`Session`] to put in the
/// registry.
fn lowest_free_id(in_use: impl Fn(ClientId) -> bool) -> ClientId {
    let mut id = 1;
    while in_use(id) {
        id = id.checked_add(1).expect("client ID space exhausted");
    }
    id
}

pub fn register_session(session: Arc<Session>) {
    let mut registry = REGISTRY.write().unwrap();
    if session.disconnect.is_cancelled() {
        return;
    }
    assert!(
        registry.insert(SessionEntry(session)),
        "client id already in use"
    );
    CHANGES.send_modify(|revision| *revision = revision.wrapping_add(1));
}

/// Membership changes are exclusive. Cancellation, transport closure and task
/// disposal happen only after the write lock is released.
/// Remove a session by identity, never by its id alone.
///
/// By the time a finished task reaches this call its id may already have been
/// recycled into a brand-new session: deleting by number would disconnect a
/// client that had nothing to do with this teardown. The registry is keyed by
/// id, but the decision here is `Arc::ptr_eq`, so a late or duplicate removal
/// finds nothing and does nothing — which is exactly what makes id reuse in
/// [`next_client_id`] safe.
pub fn remove_session(session: &Arc<Session>) {
    let doomed = {
        let mut registry = REGISTRY.write().unwrap();
        let doomed = registry
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.0, session))
            .map(|entry| SessionEntry(entry.0.clone()));
        let Some(doomed) = doomed else {
            return;
        };
        registry.remove(&doomed);
        // Retain the old resource names while Linux capture winds down.
        #[cfg(target_os = "linux")]
        TEARING_DOWN.fetch_add(1, Ordering::Release);
        CHANGES.send_modify(|revision| *revision = revision.wrapping_add(1));
        doomed
    };
    doomed.0.stop();
}

pub fn lowest_client_id() -> Option<ClientId> {
    let lowest = REGISTRY.read().unwrap().iter().map(|s| s.0.id).min();
    #[cfg(target_os = "linux")]
    if lowest.is_none() && TEARING_DOWN.load(Ordering::Acquire) != 0 {
        // ID zero is never allocated. A new session must use a suffixed name
        // until the old capture has released its PipeWire nodes and monitor.
        return Some(0);
    }
    lowest
}

pub fn list_clients() -> Vec<(ClientId, String)> {
    REGISTRY
        .read()
        .unwrap()
        .iter()
        .map(|s| (s.0.id, s.0.display_name.clone()))
        .collect()
}

fn get_session(id: ClientId) -> Option<Arc<Session>> {
    REGISTRY
        .read()
        .unwrap()
        .get(&id)
        .map(|entry| entry.0.clone())
}

pub fn send_client_control(msg: ServerDatagram) {
    let sessions: Vec<_> = REGISTRY
        .read()
        .unwrap()
        .iter()
        .map(|entry| entry.0.clone())
        .collect();
    for session in sessions {
        session.send(msg.clone());
    }
}

pub fn send_client_control_to(id: ClientId, msg: ServerDatagram) {
    if let Some(session) = get_session(id) {
        session.send(msg);
    }
}

pub fn disconnect_client(id: ClientId) {
    if let Some(session) = get_session(id) {
        remove_session(&session);
    }
}

/// Give Linux capture and audio a chance to release their resources before the
/// runtime is shut down. The interactive removal path remains non-blocking.
pub async fn shutdown_sessions() {
    let sessions: Vec<_> = REGISTRY
        .read()
        .unwrap()
        .iter()
        .map(|entry| entry.0.clone())
        .collect();
    for session in &sessions {
        remove_session(session);
    }
    #[cfg(target_os = "linux")]
    {
        let cleanup: Vec<_> = sessions
            .iter()
            .filter_map(|session| session.cleanup.lock().unwrap().take())
            .collect();
        if tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for task in cleanup {
                let _ = task.await;
            }
        })
        .await
        .is_err()
        {
            log::warn!("timed out waiting for sessions to release Linux resources");
        }
    }
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
    use tokio::{fs::remove_file, net::UnixListener};

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
                conn.write(&crate::auth::session_menu(&sessions)).await?;
                return Ok(());
            }
        }
    };
    let short = crate::auth::format_id(&id);
    let mut config_with_path = crate::get_config_with_path().await;
    let config = &mut config_with_path.config;
    if let Some(doomed) = config.users.extract_if(|user| id == *user).last() {
        crate::update_config(config_with_path).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids rewind to the lowest free slot, so a lone client that reconnects
    /// reclaims its old id — and its old tray label — instead of climbing.
    /// Recycling is sound only because `remove_session` compares identities:
    /// a stale removal naming a recycled id finds no matching `Arc` and is a
    /// no-op, which is the property the previous monotonic counter provided.
    #[test]
    fn client_ids_rewind_to_the_lowest_free() {
        let used: HashSet<ClientId> = [1, 2, 4].into_iter().collect();
        assert_eq!(lowest_free_id(|id| used.contains(&id)), 3);
        let empty: HashSet<ClientId> = HashSet::new();
        assert_eq!(
            lowest_free_id(|id| empty.contains(&id)),
            1,
            "id 0 is reserved as a sentinel"
        );
        let full: HashSet<ClientId> = (1..1000).collect();
        assert_eq!(lowest_free_id(|id| full.contains(&id)), 1000);
    }

    /// While a removed Linux capture is still releasing its resources, the
    /// registry is empty but the unsuffixed (primary) name is still taken, so a
    /// new session must be named as if a lower id were live.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pending_teardown_reserves_the_primary_name() {
        assert_eq!(lowest_client_id(), None);
        TEARING_DOWN.fetch_add(1, Ordering::Release);
        assert_eq!(lowest_client_id(), Some(0));
        TEARING_DOWN.fetch_sub(1, Ordering::Release);
        assert_eq!(lowest_client_id(), None);
    }
}
