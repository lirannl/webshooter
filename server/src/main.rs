#![feature(
    io_error_more,
    if_let_guard,
    async_closure,
    let_chains,
    async_fn_traits,
    extend_one,
    slice_as_chunks,
    generic_arg_infer,
    duration_constructors
)]

mod auth;
pub mod config;
mod config_watch;
mod error;
mod frontend;
mod ipc;
mod logging;
mod video_serve;
use anyhow::Result;
use auth::negotiate_websocket;
use config::Config;
use error::WebshooterError;
use futures_util::{join, TryFutureExt};
use ipc::setup_ipc;
use logging::log;
use poem::{
    get, handler,
    listener::{Listener, RustlsCertificate, TcpListener},
    post, EndpointExt, IntoResponse, Response, Route, Server,
};
use std::{
    env,
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio::{
    fs,
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
};
use video_serve::setup_wt;
use wtransport::Identity;

use crate::{
    auth::{check_identity, get_challenge, login, Authenticated},
    config_watch::watch_config,
};

lazy_static::lazy_static! {
    pub static ref APP_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
    pub static ref RESET_TRIGGER: Mutex<Option<Sender<()>>> = Mutex::new(None);
}

pub fn reset_app() {
    if let Some(trigger) = RESET_TRIGGER.blocking_lock().as_ref() {
        let _ = tokio::runtime::Handle::current().block_on(trigger.send(()));
    }
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let config_dir = setup_config_dir().await?;
    setup_config(&config_dir).await?;

    let (tx, mut rx) = mpsc::channel::<()>(1);
    RESET_TRIGGER.lock().await.replace(tx);

    loop {
        let config = get_config().await;

        setup_ipc(config.clone()).await?;

        setup_ssl_certificates(&config).await?;

        println!(
            "Listening for connections on https://{}:{}",
            config.http_config.host, config.http_config.port
        );

        let listener = TcpListener::bind(format!(
            "{}:{}",
            config.http_config.host, config.http_config.port
        ))
        .rustls(
            poem::listener::RustlsConfig::new().fallback(
                RustlsCertificate::new()
                    .cert(fs::read(&config.http_config.ssl_conf.certificate).await?)
                    .key(fs::read(&config.http_config.ssl_conf.key).await?),
            ),
        );

        watch_config(&config.path).await;

        let identity = Identity::self_signed(&config.webtransport_permitted_domains)?;
        let app = Route::new()
            .at("/check_identity", get(check_identity))
            .at("/check_auth", get(check_auth))
            .at("/challenge", get(get_challenge))
            .at(
                "/negotiate_websocket",
                get(negotiate_websocket).data(identity.certificate_chain().as_slice()[0].hash()),
            )
            .at("/login", post(login))
            .at("/*", frontend::frontend);

        let handle_0 = tokio::spawn(Server::new(listener).run(app).or_else(async |err| {
            log(err);
            reset_app();
            Ok::<_, anyhow::Error>(())
        }));
        let handle_1 = tokio::spawn(setup_wt(config.clone(), identity).or_else(async |err| {
            log(err);
            reset_app();
            Ok::<_, anyhow::Error>(())
        }));
        // Wait for a reset signal
        rx.recv().await;
        handle_0.abort();
        handle_1.abort();
    }
}

async fn setup_ssl_certificates(config: &Config) -> Result<()> {
    if !config.http_config.ssl_conf.key.exists()
        || !config.http_config.ssl_conf.certificate.exists()
    {
        println!("Ssl certificate not found. Generating...");
        let gen = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            config.http_config.host.to_string(),
        ])?;
        let ssl_conf = &config.http_config.ssl_conf;
        tokio::fs::write(&ssl_conf.certificate, gen.cert.pem()).await?;
        println!(
            "New ssl certificate PEM generated at {:?}",
            ssl_conf.certificate
        );
        tokio::fs::write(&ssl_conf.key, gen.key_pair.serialize_pem()).await?;
        println!("New ssl keys PEM generated at {:?}", ssl_conf.key);
    }
    Ok(())
}

async fn setup_config(config_dir: &Path) -> Result<()> {
    let config_file_name = std::fs::read_dir(&config_dir)?.find_map(|p| {
        let name = p.ok()?.file_name().to_str()?.to_string();
        if name.starts_with("config") {
            Some(name)
        } else {
            None
        }
    });
    if let Some(config_path) = config_file_name {
        let config_path = config_dir.join(&config_path);
        let config = fs::read_to_string(&config_path).await?;
        if config.trim() == "" {
            update_config(Config::initialise_at(&config_path)?).await?;
        } else {
            let config: Config = serde_json::from_str(&config)
                .or_else(|_| {
                    let mut config: Config = toml::from_str(&config)?;
                    config.path = config_path.to_owned();
                    Ok(config)
                })
                .map_err(|err| WebshooterError::InvalidConfig(config_path.clone(), err))?;
            *APP_CONFIG.lock().await = Some(config);
        }
    } else {
        update_config(Config::initialise_at(&config_dir.join("config.json"))?).await?;
    }
    Ok(())
}

pub async fn setup_config_dir() -> Result<PathBuf> {
    let args = std::env::args().collect::<Vec<_>>();
    let args = args.iter().map(|x| x.as_str()).collect::<Vec<_>>();
    let config_dir = match args.as_slice() {
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
    Ok(config_dir)
}

pub async fn update_config(config: Config) -> Result<()> {
    let path = &config.path;
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

#[handler]
fn check_auth(Authenticated { id }: Authenticated) -> impl IntoResponse {
    Response::builder().body(format!("Authenticated as: {id}"))
}
