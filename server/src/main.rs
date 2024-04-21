#![feature(io_error_more, if_let_guard)]
mod error;
mod frontend;
use anyhow::Result;
use error::WebshooterError;
use frontend::Assets;
use std::{
    env,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio::{fs, sync::Mutex};
use warp::Filter;
use warp_reverse_proxy::reverse_proxy_filter;
use webshooter_shared::Config;

lazy_static::lazy_static! {
    pub static ref APP_CONFIG: Mutex<Config> = Mutex::new(Default::default());
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
                update_config(&path, Config::default()).await?;
            } else {
                let config: Config = serde_json::from_str(&config)
                    .or_else(|_| toml::from_str(&config))
                    .map_err(|err| WebshooterError::InvalidConfig(path.clone(), err.into()))?;
                // if config.port == 0 {
                //     config.port = if config.ssl_creds.is_some() { 443 } else { 80 };
                //     update_config(&path, config).await?;
                // } else {
                *APP_CONFIG.lock().await = config;
                // }
            }
        } else {
            update_config(&config_dir.join("config.json"), Config::default()).await?;
        }
    }

    let config = get_config().await;

    let login = warp::path!("login").map(|| format!("Hello, name!"));

    let frontend = warp_embed::embed(&Assets);

    #[cfg(debug_assertions)]
    let frontend =
        reverse_proxy_filter("".to_string(), "http://localhost:5173".to_string()).or(frontend);

        warp::serve(login.or(frontend))
        .run(SocketAddr::from_str(&config.http_config.addr.to_string())?)
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
    *APP_CONFIG.lock().await = config;
    Ok(())
}

pub async fn get_config() -> Config {
    APP_CONFIG.lock().await.clone()
}
