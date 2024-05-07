#![feature(
    io_error_more,
    if_let_guard,
    async_closure,
    let_chains,
    async_fn_traits,
    extend_one
)]
mod error;
mod frontend;
mod ipc;
mod session;
use anyhow::Result;
use bytes::{Buf, Bytes};
use error::WebshooterError;
use frontend::serve_frontend;
use futures_util::TryFutureExt;
use h3::{quic::BidiStream, server::RequestStream};
use http::Request;
use ipc::setup_ipc;
use rcgen::generate_simple_self_signed;
use rustls::{Certificate, PrivateKey};
use session::{get_challenge, login};
use warp::{
    filters::{
        any::any,
        host::Authority,
        path::{self, FullPath},
    },
    reject::reject,
    reply::{reply, with_header, with_status},
    Filter,
};
use webshooter_shared::Config;
//use session::login;
use std::{
    env,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use tokio::{
    fs::{self, read_to_string},
    spawn,
    sync::Mutex,
};
//use warp::Filter;

lazy_static::lazy_static! {
    pub static ref APP_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
}

#[tokio::main]
pub async fn main() {
    main_result()
        .await
        .unwrap_or_else(|err| eprintln!("{err:#?}"));
}

pub async fn main_result() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let args = args.iter().map(|x| x.as_str()).collect::<Vec<_>>();
    let config_dir = match args.as_slice() {
        [_, path] => PathBuf::from_str(path),
        [_, "-c", path] => PathBuf::from_str(path),
        [_, "--config", path] => PathBuf::from_str(path),
        [_, "--config-path", path] => PathBuf::from_str(path),
        _ => {
            #[cfg(target_os = "linux")]
            let config_dir = format!("{}/.config/webshooter", env::var("HOME")?);
            #[cfg(target_os = "windows")]
            let config_dir = format!("{}\\AppData\\Roaming\\webshooter", env::var("HOME")?);
            PathBuf::from_str(&config_dir)
        }
    }?;

    let config_dir = match fs::read_dir(&config_dir).await {
        Ok(_) => Ok(config_dir),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(&config_dir).await?;
            Ok(config_dir)
        }
        Err(err) if err.kind() == ErrorKind::NotADirectory => Ok(config_dir
            .parent()
            .ok_or(WebshooterError::InvalidConfigPath(format!(
                "{config_dir:?}"
            )))?
            .to_path_buf()),
        Err(err) => Err(err),
    }?;

    {
        let config_file_name = std::fs::read_dir(&config_dir)?.find_map(|p| {
            let name = p.ok()?.file_name().to_str()?.to_string();
            if name.starts_with("config") {
                Some(name)
            } else {
                None
            }
        });
        if let Some(path) = config_file_name {
            let path = config_dir.join(&path);
            let config = fs::read_to_string(&path).await?;
            if config.trim() == "" {
                update_config(&path, Config::initialise_at(&path)?).await?;
            } else {
                let config: Config = serde_json::from_str(&config)
                    .or_else(|_| toml::from_str(&config))
                    .map_err(|err| WebshooterError::InvalidConfig(path.clone(), err.into()))?;
                *APP_CONFIG.lock().await = Some(config);
            }
        } else {
            update_config(
                &config_dir.join("config.json"),
                Config::initialise_at(&config_dir)?,
            )
            .await?;
        }
    }

    let config = get_config().await;
    let config_clone = config.clone();
    spawn(async move { http3_upgrade(config_clone).await });
    let config_clone = config.clone();
    spawn(async move { setup_ipc(config_clone) });

    if !config.http_config.certificate.exists()
        || !config.http_config.key.exists()
        || !config.http_config.pubkey.exists()
    {
        eprintln!("HTTPS certificate/key/pubkey not found. Generating self-signed certificate...");

        let subject_alt_names = vec!["localhost".to_string()];

        #[cfg(target_os = "linux")]
        let subject_alt_names = [
            vec![format!("{}.local", read_to_string("/etc/hostname").await?)],
            subject_alt_names,
        ]
        .concat();
        let cert = generate_simple_self_signed(subject_alt_names)?;
        tokio::fs::write(&config.http_config.certificate, cert.serialize_pem()?).await?;
        tokio::fs::write(
            &config.http_config.pubkey,
            cert.get_key_pair().public_key_der(),
        )
        .await?;
        tokio::fs::write(&config.http_config.key, cert.serialize_private_key_der()).await?;
    }
    let certificate = rustls::Certificate(tokio::fs::read(config.http_config.certificate).await?);
    let key = PrivateKey(tokio::fs::read(config.http_config.key).await?);

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)?;
    server_crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));

    let endpoint = quinn::Endpoint::server(
        server_config,
        SocketAddr::from_str(&config.http_config.addr.to_string())?,
    )?;

    while let Some(new_conn) = endpoint.accept().await {
        tokio::spawn(async move {
            match new_conn.await {
                Ok(conn) => {
                    let mut h3_conn = h3::server::builder()
                        .enable_webtransport(true)
                        .enable_datagram(true)
                        .enable_connect(true)
                        .max_webtransport_sessions(1)
                        .send_grease(true)
                        .build(h3_quinn::Connection::new(conn))
                        .await
                        .unwrap();

                    tokio::spawn(async move {
                        match h3_conn.accept().await {
                            Ok(Some((req, stream))) => {
                                handler(req, stream).await.unwrap_or_else(|err| {
                                    eprintln!("Request not supported by Webshooter: {err:#?}")
                                });
                            }
                            Ok(None) => eprintln!("No request"),
                            Err(err) => eprintln!("Failed to setup http3: {err:#?}"),
                        }
                    });
                }
                Err(err) => {
                    eprintln!("accepting connection failed: {:?}", err);
                }
            }
        });
    }
    /*let login = login();

    let frontend = setup_frontend();

    warp::serve(login.or(frontend))
        .run(SocketAddr::from_str(&config.http_config.addr.to_string())?)
        .await;*/

    Ok(())
}

async fn handler<T>(req: Request<()>, mut stream: RequestStream<T, Bytes>) -> Result<()>
where
    T: BidiStream<Bytes>,
{
    let pubkey: Arc<[u8]> = req
        .headers()
        .get("pubkey")
        .ok_or(WebshooterError::MissingPubkey)?
        .as_bytes()
        .into();

    let (response, data) = match req.uri().path() {
        "login/challenge" => get_challenge(pubkey.to_vec()).await,
        "login" => login(pubkey).await,
        _ => {
            serve_frontend(
                &req,
                stream
                    .recv_data()
                    .await?
                    .map(|stream| stream.chunk().to_vec()),
            )
            .await
        }
    }?;
    stream.send_response(response).await?;
    stream.send_data(data.into()).await?;
    Ok(stream.finish().await?)
}

async fn update_config(path: &Path, config: Config) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(WebshooterError::InvalidConfigPath(format!("{path:?}")))?;
    let contents = if name.ends_with(".toml") {
        toml::to_string_pretty(&config)?
    } else if name.ends_with(".yaml") || name.ends_with(".yml") {
        serde_yaml::to_string(&config)?
    } else {
        serde_json::to_string_pretty(&config)?
    };
    fs::write(path, &contents).await?;
    *APP_CONFIG.lock().await = Some(config);
    Ok(())
}

pub async fn get_config() -> Config {
    APP_CONFIG.lock().await.clone().unwrap()
}

/* Given http (no tls) - redirect to https */
async fn http3_upgrade(config: Config) -> Result<()> {
    let server = warp::serve(
        any()
            .and(
                warp::filters::host::optional()
                    .and_then(|auth: Option<Authority>| async { auth.ok_or(reject()) }),
            )
            .and(path::full())
            .and_then(|auth: Authority, path: FullPath| async move {
                (async move || {
                    let response = reply();
                    let response =
                        with_status(response, warp::http::StatusCode::TEMPORARY_REDIRECT);
                    let response = with_header(
                        response,
                        "Location",
                        format!("https://{auth}{}", path.as_str()),
                    );

                    Ok(response)
                })()
                .map_err(|_: anyhow::Error| warp::reject())
                .await
            }),
    );
    spawn(server.bind(SocketAddr::from_str(&config.http_config.addr.to_string())?));
    Ok(())
}
