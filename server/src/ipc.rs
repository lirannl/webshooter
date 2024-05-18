use crate::session::SESSIONS;
use anyhow::{bail, Result};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::{env, fmt::Display, io::ErrorKind, path::PathBuf, str::FromStr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::watch,
};
use webshooter_shared::Config;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum IPCMessage {
    Exit,
    Authorise,
    AuthoriseN(u32),
}

impl IPCMessage {
    pub fn parse_args(args: impl Iterator<Item = impl Display>) -> Result<IPCMessage> {
        let args = args
            .map(|arg| arg.to_string().to_lowercase())
            .collect::<Vec<_>>();
        match args.iter().map(|arg| &**arg).collect::<Vec<&str>>()[..] {
            ["exit"] => Ok(IPCMessage::Exit),
            ["authorise"] => Ok(IPCMessage::Authorise),
            ["authorise", n] if let Ok(n) = n.parse::<u32>() => Ok(IPCMessage::AuthoriseN(n)),
            _ => bail!(
                "Webshooter supports the following commands while running:
    authorise
    exit"
            ),
        }
    }
}

lazy_static! {
    static ref _IPC: (watch::Sender<IPCMessage>, watch::Receiver<IPCMessage>,) =
        watch::channel(IPCMessage::Exit);
}
pub fn ipc_receiver() -> watch::Receiver<IPCMessage> {
    _IPC.1.clone()
}

pub fn send_ipc(msg: IPCMessage) {
    let _ = _IPC.0.send(msg);
}

// IPC will be implemented for each platform separately
#[cfg(target_os = "linux")]
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
        spawn,
    };

    use crate::session::Session;
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
                connection
                    .write_all(&serde_json::to_vec(&ipcmessage)?)
                    .await?;
                connection.flush().await?;
                let mut str = String::new();
                connection.read_to_string(&mut str).await?;
                println!("{str}");
                exit(0)
            }
        }
        listener => listener,
    }?;
    spawn(async move {
        async move {
            loop {
                let (mut conn, _) = listener.accept().await?;
                let handling = async {
                    let mut buf = Vec::new();
                    if conn.read_buf(&mut buf).await? > 0 {
                        let message: IPCMessage = serde_json::from_slice(&mut buf)?;

                        match message {
                            IPCMessage::Exit => {
                                conn.write_all("Webshooter shutting down".as_bytes())
                                    .await?;
                                exit(0)
                            }
                            IPCMessage::Authorise => {
                                let sessions = SESSIONS.lock().await;
                                let sessions = sessions.len();
                                if sessions == 1 {
                                    send_ipc(IPCMessage::AuthoriseN(0))
                                } else {
                                    let sessions = &SESSIONS
                                        .lock()
                                        .await;
                                    let mut sessions = sessions
                                        .iter().filter_map(|(id,session)| match session {
                                            Session::Challenged(_) => Some(id.clone()),
                                            _ => None
                                        }).collect::<Vec<_>>();
                                    sessions.sort();
                                    let sessions = sessions.into_iter()
                                        .enumerate()
                                        .map(|(idx, id)| format!("  {idx}: {id:x?}"))
                                        .collect::<Vec<_>>();
                                    let _ = conn.write_all((
                                            "Multiple sessions connected, please pick the relevant session:".to_owned() + 
                                            &sessions.join("\n")
                                        ).as_bytes()).await;
                                }
                            }
                            message => send_ipc(message),
                        };
                    }
                    conn.shutdown().await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(err) = handling {
                    let _ = conn.write_all(err.to_string().as_bytes()).await;
                    let _ = conn.shutdown().await;
                }
            }
            #[allow(unreachable_code)]
            Ok(())
        }
        .await
        .unwrap_or_else(|err: anyhow::Error| {
            eprintln!("{err:#?}");
            exit(1)
        })
    });
    Ok(())
}
