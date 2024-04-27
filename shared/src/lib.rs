use anyhow::{anyhow, Result};
use bytes::Bytes;
use http::{HeaderMap, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::{
    borrow::Borrow,
    fmt::Display,
    fs,
    ops::Deref,
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
    #[serde(flatten)]
    pub authorised_keys: Vec<BytesLowercase>,
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

#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BytesLowercase(#[serde_as(as = "serde_with::base64::Base64")] Vec<u8>);

impl Deref for BytesLowercase {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Into<Vec<u8>> for BytesLowercase {
    fn into(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for BytesLowercase {
    fn from(value: Vec<u8>) -> Self {
        BytesLowercase(value)
    }
}
