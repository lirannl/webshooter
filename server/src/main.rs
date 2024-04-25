#![feature(
    io_error_more,
    if_let_guard,
    async_closure,
    let_chains,
    async_fn_traits
)]
mod error;
mod frontend;
mod session;
use anyhow::Result;
use bytes::Bytes;
use error::WebshooterError;
use h3::{quic::BidiStream, server::RequestStream};
use http::{Request, StatusCode};
use rustls::{Certificate, PrivateKey};
use session::{get_challenge, login};
use webshooter_shared::Config;
//use session::login;
use std::{
    env,
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use tokio::{fs, sync::Mutex};
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

    let certificate = Certificate(std::fs::read(config.http_config.certificate)?);
    let key = PrivateKey(std::fs::read(config.http_config.key)?);

    let mut tls_config = rustls::ServerConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)?;

    tls_config.max_early_data_size = u32::MAX;
    tls_config.alpn_protocols = vec![b"h3".into()];

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let endpoint = quinn::Endpoint::server(
        server_config,
        std::net::SocketAddr::from_str(&config.http_config.addr.to_string())?,
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

    let response = match req.uri().path() {
        "login/challenge" => get_challenge(pubkey.to_vec()).await,
        "login" => login(pubkey).await,
        _ => {
            // Reverse proxy into dev server in development, otherwise, serve embedded
            todo!()
        }
    };
    Ok(())
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
