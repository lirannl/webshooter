#![feature(extend_one, cfg_eval, const_default, const_trait_impl)]

mod auth;
#[cfg(target_os = "linux")]
mod compositor_discovery;
mod config;
mod config_watch;
mod cert_watch;
mod error;
mod extensions;
mod frontend;
mod ipc;
mod keyboard;
mod logging;
#[cfg(target_os = "linux")]
mod pipewire;
mod wt;
use anyhow::Result;
use futures_util::TryFutureExt;
use config::Config;
use error::WebshooterError;
use ipc::setup_ipc;
use logging::log;
use salvo::conn::rustls::RustlsConfig;
use salvo::conn::{QuinnListener, TcpListener};
use salvo::routing::Router;
use salvo::{Listener, Server};
use std::{
    env,
    error::Error,
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
    sync::LazyLock,
};
use tokio::fs;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    auth::{check_auth, check_identity, get_challenge, login, negotiate_wt},
    compositor_discovery::{
        ensure_wayland_display, ensure_xdg_current_desktop, ensure_xdg_runtime_dir,
    },
    config::CONFIG_DIR,
    config_watch::watch_config,
    frontend::index as serve_frontend,
};

pub static APP_CONFIG: LazyLock<Mutex<Option<Config>>> = Default::default();
pub static RESET_TRIGGER: LazyLock<Mutex<Option<mpsc::Sender<()>>>> = Default::default();

pub fn reset_app() {
    if let Some(trigger) = RESET_TRIGGER.blocking_lock().as_ref() {
        let _ = tokio::runtime::Handle::current().block_on(trigger.send(()));
    }
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        ensure_xdg_runtime_dir()?;
        ensure_wayland_display()?;
        ensure_xdg_current_desktop()?;

        // On Linux a desktop environment is required to choose the capture path;
        // if we still couldn't determine one, refuse to launch.
        if env::var_os("XDG_CURRENT_DESKTOP").is_none() {
            eprintln!(
                "error: XDG_CURRENT_DESKTOP could not be determined. A graphical \
             desktop session is required to capture (set XDG_CURRENT_DESKTOP, \
             e.g. KDE/GNOME/sway, or log in graphically)."
            );
            std::process::exit(1);
        }
    }

    let _ = CONFIG_DIR.set(setup_config_dir().await?);
    setup_config(CONFIG_DIR.get().unwrap()).await?;

    let (tx, mut rx) = mpsc::channel::<()>(1);
    RESET_TRIGGER.lock().await.replace(tx);

    loop {
        let config = get_config().await;

        setup_ipc(config.clone()).await?;

        #[cfg(target_os = "linux")]
        crate::pipewire::setup_pipewire().await;

        setup_ssl_certificates(&config).await?;

        println!(
            "Listening for connections on https://{}:{}",
            config.http_config.host, config.http_config.port
        );

        // Hot-reloadable TLS config shared by the TCP (HTTP/1.1 + h2) and QUIC
        // (HTTP/3 + WebTransport) listeners. Both bind the same port.
        let (cert_tx_tcp, cert_rx_tcp) = mpsc::unbounded_channel::<RustlsConfig>();
        let (cert_tx_quic, cert_rx_quic) = mpsc::unbounded_channel::<RustlsConfig>();
        let rustls_config = cert_watch::build_rustls_config(&config.http_config.ssl_conf)?;
        let _ = cert_tx_tcp.send(rustls_config.clone());
        let _ = cert_tx_quic.send(rustls_config);

        let tcp_listener = TcpListener::new((
            config.http_config.host,
            config.http_config.port,
        ))
        .rustls(UnboundedReceiverStream::new(cert_rx_tcp));

        let quinn_listener = QuinnListener::new(
            UnboundedReceiverStream::new(cert_rx_quic),
            (config.http_config.host, config.http_config.port),
        );

        let acceptor = quinn_listener.join(tcp_listener).bind().await;

        // Reload certificates from disk whenever they change.
        let _ = cert_watch::spawn_cert_watcher(
            config.http_config.ssl_conf.clone(),
            vec![cert_tx_tcp, cert_tx_quic],
        );

        watch_config(&config.path).await;

        let router = Router::new()
            .push(Router::with_path("/check_identity").goal(check_identity))
            .push(Router::with_path("/check_auth").goal(check_auth))
            .push(Router::with_path("/challenge").goal(get_challenge))
            .push(Router::with_path("/negotiate_wt").goal(negotiate_wt))
            .push(Router::with_path("/login").goal(login))
            .push(
                Router::with_path("/{*path}")
                    .hoop(wt::connect)
                    .goal(serve_frontend),
            );

        let handle_0 = tokio::spawn(Server::new(acceptor).try_serve(router).or_else(async |err| {
            log(err);
            reset_app();
            Ok::<_, anyhow::Error>(())
        }));

        // Wait for a reset signal
        rx.recv().await;
        handle_0.abort();
    }
}

async fn setup_ssl_certificates(config: &Config) -> Result<()> {
    let ssl_conf = &config.http_config.ssl_conf;
    if !ssl_conf.key.exists() {
        anyhow::bail!(
            "SSL key not found at {:?}. Generate or copy a certificate before starting.",
            ssl_conf.key
        );
    }
    if !ssl_conf.certificate.exists() {
        anyhow::bail!(
            "SSL certificate not found at {:?}. Generate or copy a certificate before starting.",
            ssl_conf.certificate
        );
    }
    Ok(())
}

async fn setup_config(config_dir: &Path) -> Result<()> {
    let config_file_name = std::fs::read_dir(config_dir)?.find_map(|p| {
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
            let mut config: Config = serde_json::from_str(&config)
                .or_else(|_| toml::from_str(&config))
                .map_err(|err| WebshooterError::InvalidConfig(config_path.clone(), err.into()))?;
            config.path = config_path.to_owned();
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
