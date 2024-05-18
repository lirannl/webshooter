use anyhow::{anyhow, Result};
use data_encoding::BASE64;
use openidconnect::{AuthUrl, ClientId, TokenUrl};
use serde::{de::Visitor, Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt::Display,
    net::{IpAddr, Ipv4Addr},
    ops::Deref,
    path::{Path, PathBuf},
    str::FromStr, time::Duration,
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

fn default_session_ttl() -> Duration {
    Duration::from_secs(86400)
}

fn is_default_session_ttl(duration: &Duration) -> bool {
    Duration::from_secs(86400) == *duration
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    version: String,
    pub authorised_keys: Vec<Bytes64>,
    #[serde(flatten)]
    pub http_config: HttpConfig,
    pub oidc: Option<OidcData>,
    pub auth_timeout: Option<u64>,
    #[serde(default = "default_session_ttl", skip_serializing_if = "is_default_session_ttl")]
    pub session_ttl: Duration,
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
            session_ttl: default_session_ttl(),
            authorised_keys: Default::default(),
            auth_timeout: Default::default(),
            oidc: Default::default(),
        })
    }
}

/// Bytes in base64
#[derive(Clone, Debug, Eq)]
pub struct Bytes64(Vec<u8>);

impl PartialEq<Bytes64> for Bytes64 {
    fn eq(&self, other: &Bytes64) -> bool {
        self.0 == other.0
    }
}

impl Deref for Bytes64 {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Into<Vec<u8>> for Bytes64 {
    fn into(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for Bytes64 {
    fn from(value: Vec<u8>) -> Self {
        Bytes64(value)
    }
}

impl FromStr for Bytes64 {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(serde_json::from_str(s)?)
    }
}

impl Serialize for Bytes64 {
    fn serialize<S>(&self, serialiser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str = BASE64.encode(self);
        serialiser.serialize_str(&str)
    }
}

impl Display for Bytes64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&BASE64.encode(&self.0))
    }
}

struct StringVisitor;
impl<'de> Visitor<'de> for StringVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(v.to_string())
    }
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(v)
    }
    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(v.to_string())
    }
}

impl<'de> Deserialize<'de> for Bytes64 {
    fn deserialize<D>(deserialiser: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str = deserialiser.deserialize_string(StringVisitor {})?;
        let bytes = BASE64
            .decode(str.as_bytes())
            .map_err(|err| serde::de::Error::custom(err.to_string()))?;
        Ok(bytes.into())
    }
}
