use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{env, fmt::Display, io::ErrorKind, path::PathBuf, process::exit, str::FromStr};
use tokio::io::{stdin, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use webshooter_shared::Config;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum IPCMessage {
    Exit,
    Authorise(Option<usize>),
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
            _ => bail!(
                "Webshooter supports the following commands while running:
    authorise
    exit"
            ),
        }
    }
}

#[cfg(target_family = "unix")]
use tokio::net::UnixStream;

pub enum IPCConnection {
    StdOut,
    #[cfg(target_family = "unix")]
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
            #[cfg(target_family = "unix")]
            Self::Unix(writer) => writer.write_all(str.as_bytes()).await,
        }
    }
}

mod ipc_funcs {
    use anyhow::Result;
    use async_channel::bounded;
    use lazy_static::lazy_static;
    use tokio::sync::Mutex;

    use crate::{ipc::IPCMessage, WebshooterError};

    use super::IPCConnection;

    lazy_static! {
        static ref _IPC: Mutex<
            Option<(
                async_channel::Sender<Option<(IPCMessage, IPCConnection)>>,
                async_channel::Receiver<Option<(IPCMessage, IPCConnection)>>,
            )>,
        > = Mutex::new(None);
    }

    pub async fn ipc_init() -> () {
        let (tx, rx) = bounded(1);
        tx.send(None)
            .await
            .expect("Webshooter failed to initialise IPC");
        *_IPC.lock().await = Some((tx, rx));
    }

    pub async fn ipc_recv() -> Result<(IPCMessage, IPCConnection)> {
        let lock = _IPC.lock().await;
        let ipc = lock.clone();
        drop(lock);
        loop {
            match ipc.as_ref().unwrap().1.recv().await? {
                None => {}
                Some(recv) => break Ok(recv),
            }
        }
    }

    // Always attempt to block the channel after
    pub fn ipc_send(message: IPCMessage, connection: IPCConnection) -> Result<()> {
        let sender = _IPC
            .try_lock()?
            .clone()
            .ok_or(WebshooterError::IPCNotAvailable)?
            .0;
        sender.try_send(Some((message, connection)))?;
        let _ = sender.try_send(None);
        Ok(())
    }
}
pub use ipc_funcs::{ipc_recv, ipc_send};

#[cfg(target_family = "unix")]
pub async fn setup_ipc(_config: Config) -> Result<()> {
    let target = env::var("XDG_RUNTIME_DIR")?;
    let target = PathBuf::from_str(&target)?.join(format!(
        "webshooter_{}.sock",
        include_str!("../../ipc_id.txt")
    ));
    use std::process::exit;
    use tokio::{
        fs::remove_file,
        net::{UnixListener, UnixStream},
    };

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

                let mut buf = Vec::new();
                conn.read_buf(&mut buf).await?;
                let message = serde_json::from_slice(&mut buf)?;

                ipc_handler(message, IPCConnection::Unix(conn)).await
            })()
            .await
            .unwrap_or_else(|err| eprintln!("IPC failure:\n{err:#?}"));
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

async fn ipc_handler(message: IPCMessage, mut conn: IPCConnection) -> Result<()> {
    match message {
        IPCMessage::Exit => {
            conn.write("Webshooter shutting down").await?;
            exit(0)
        }
        message => {
            ipc_send(message, conn)?;
        }
    }
    Ok(())
}
