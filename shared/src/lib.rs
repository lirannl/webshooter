use anyhow::{anyhow, Result};
use openidconnect::{AuthUrl, ClientId, TokenUrl};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::{
    net::{IpAddr, Ipv4Addr},
    ops::Deref,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SslConfig {
    pub key: PathBuf,
    pub certificate: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpConfig {
    pub host: IpAddr,
    pub port: u16,
    #[serde(flatten)]
    pub ssl_conf: SslConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcData {
    pub client_id: ClientId,
    pub auth_url: AuthUrl,
    pub token_url: TokenUrl,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    version: String,
    pub authorised_keys: Vec<BytesLowercase>,
    #[serde(flatten)]
    pub http_config: HttpConfig,
    pub oidc: Option<OidcData>,
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
                host: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                port: 443,
                ssl_conf: SslConfig {
                    key: parent.join("server.key"),
                    certificate: parent.join("server.crt"),
                },
            },
            authorised_keys: Vec::default(),
            oidc: None,
        })
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize, Eq)]
pub struct BytesLowercase(#[serde_as(as = "serde_with::base64::Base64")] Vec<u8>);

impl PartialEq<BytesLowercase> for BytesLowercase {
    fn eq(&self, other: &BytesLowercase) -> bool {
        self.0 == other.0
    }
}

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

impl AsRef<[u8]> for BytesLowercase {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
