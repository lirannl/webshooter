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
use error::WebshooterError;
use frontend::setup_frontend;
use ipc::setup_ipc;
// use session::login;
use warp::{filters::path::path, reply::json, Filter};
use webshooter_shared::Config;
//use session::login;
use std::{
    env,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio::{fs, spawn, sync::Mutex};

use crate::session::login;
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
    spawn(async move { setup_ipc(config_clone) });

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

    // let login = login();
    // let login = path("oidc_data")
    //     .map(move || json(&config.oidc))
    //     .or(login);

    let frontend = setup_frontend();

    println!(
        "Listening for connections on {}:{}",
        config.http_config.host, config.http_config.port
    );
    warp::serve(frontend)
        .tls()
        .key_path(config.http_config.ssl_conf.key)
        .cert_path(config.http_config.ssl_conf.certificate)
        .run(SocketAddr::new(
            config.http_config.host,
            config.http_config.port,
        ))
        .await;

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
