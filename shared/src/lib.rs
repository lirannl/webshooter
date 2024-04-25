use anyhow::{anyhow, Result};
use rustls::{Certificate, PrivateKey};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Display,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum HttpAddr {
    HostPort(String, u16),
    Address(String),
}

impl Display for HttpAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostPort(host, port) => write!(f, "{host}:{port}"),
            Self::Address(address) => write!(f, "{address}"),
        }
    }
}
impl Default for HttpAddr {
    fn default() -> Self {
        HttpAddr::HostPort("127.0.0.1".to_string(), 443)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpConfig {
    pub addr: HttpAddr,
    pub key: PathBuf,
    pub certificate: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    version: String,
    pub authorised_keys: Vec<String>,
    #[serde(flatten)]
    pub http_config: HttpConfig,
}

impl Config {
    pub fn initialise_at(path: &Path) -> Result<Self> {
        let parent = if path.is_dir() {
            path.to_owned()
        } else {
            path.parent()
                .ok_or(anyhow!("The config path must be a file"))?
                .to_owned()
        };
        Ok(Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            http_config: HttpConfig {
                addr: HttpAddr::default(),
                key: parent.join("webshooter.key"),
                certificate: parent.join("webshooter.crt"),
            },
            authorised_keys: Vec::default(),
        })
    }
}
