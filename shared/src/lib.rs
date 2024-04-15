use std::fmt::Display;

use serde::{Deserialize, Serialize};

type SslPK = String;
type SslCert = String;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpConfig {
    pub addr: HttpAddr,
    pub ssl_creds: Option<(SslPK, SslCert)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    version: String,
    pub authorised_keys: Vec<String>,
    #[serde(flatten)]
    pub http_config: HttpConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            authorised_keys: Default::default(),
            http_config: HttpConfig {
                addr: HttpAddr::HostPort("127.0.0.1".to_string(), 80),
                ssl_creds: None,
            },
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}
