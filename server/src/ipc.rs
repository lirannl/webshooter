use std::{
    env,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Result;
use webshooter_shared::Config;

// IPC will be implemented for each platform separately
#[cfg(target_os = "linux")]
pub async fn setup_ipc(config: Config) -> Result<()> {
    let target = env::var("XDG_RUNTIME_DIR")?;
    let target = PathBuf::from_str(&target)?.join(format!(
        "webshooter_{}.sock",
        include_str!("../../ipc_id.txt")
    ));
    use std::{collections::HashMap, env::args, io::ErrorKind, process::exit};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixSocket,
        spawn,
    };

    use crate::{
        session::{Session, SESSIONS},
        APP_CONFIG,
    };
    let socket = UnixSocket::new_stream()?;
    let bind = socket.bind(&target);
    if let Err(err) = &bind
        && err.kind() == ErrorKind::AddrInUse
    {
        let mut socket = socket.connect(&target).await?;
        socket
            .write_all(env::args().collect::<Vec<_>>().join(" ").as_bytes())
            .await?;
        let mut str = String::new();
        socket.read_to_string(&mut str).await?;
        println!("{str}");
        exit(0)
    } else {
        let socket = socket.listen(2)?;
        loop {
            let (mut conn, _) = socket.accept().await?;
            let handling = async {
                let mut buf = Vec::new();
                conn.read(&mut buf).await?;
                let mut message = String::from_utf8(buf)?;
                let aliases = {
                    let mut aliases = HashMap::new();
                    aliases.insert("quit", "exit");
                    aliases.insert("authorize", "authorise");
                    aliases
                };
                for (source, replacement) in aliases {
                    message = message.replace(source, &replacement);
                }
                let message = message.trim();
                match message {
                    "exit" => exit(0),
                    "authorise" => {
                        let mut sessions = SESSIONS.lock().await;
                        if let [(pubkey, session)] =
                            sessions.iter_mut().collect::<Vec<_>>().as_slice()
                            && let Session::Challenged(challenge) = session
                        {
                            conn.write_all(
                                format!(
                                    "Authorising {}",
                                    challenge
                                        .iter()
                                        .map(|c| format!("{c:02x}"))
                                        .collect::<String>()
                                )
                                .as_bytes(),
                            )
                            .await?;
                            APP_CONFIG
                                .lock()
                                .await
                                .as_mut()
                                .unwrap()
                                .authorised_keys
                                .extend_one((*pubkey).clone().into());
                        } else if sessions.len() == 0 {
                            conn.write_all("No unauthorised sessions".as_bytes())
                                .await?;
                        }
                    }
                    _ => (),
                };
                conn.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(err) = handling {
                let _ = conn.write_all(err.to_string().as_bytes()).await;
                let _ = conn.shutdown().await;
            }
        }
    }
}
