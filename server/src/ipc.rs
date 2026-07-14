use crate::config::Config;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{env, fmt::Display, io::ErrorKind, path::PathBuf, process::exit, str::FromStr};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, stdin};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum IPCMessage {
    Exit,
    Authorise(Option<usize>),
    Deauthorise(Option<usize>),
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
            _ => bail!(
                "Webshooter supports the following commands while running:
    authorise
    deauthorise
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
                };
                if let Some(message) = response {
                    conn.write(message.as_bytes()).await?;
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
                        .map(|(n, s)| format!("{n}: {}", format_id(s)))
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
    let key = crate::config::Bytes64(id);
    if config.authorised_keys.remove(&key) {
        crate::update_config(config).await?;
        conn.write(&format!("Deauthorised {short}")).await?;
    } else {
        conn.write(&format!("Key {short} not found in authorised_keys")).await?;
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
        message => {
            ipc_send(message, conn)?;
        }
    }
    Ok(())
}
