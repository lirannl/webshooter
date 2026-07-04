use anyhow::{Result, anyhow};
use data_encoding::BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};
use std::{
    collections::HashSet,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Deref,
    path::{Path, PathBuf},
    str::FromStr,
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

impl Into<SocketAddr> for &HttpConfig {
    fn into(self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    pub authorised_keys: HashSet<Bytes64>,
    #[serde(flatten)]
    pub http_config: HttpConfig,
    #[serde(default)]
    pub auth_timeout: Option<u64>,
    #[serde(default = "default_settings::permitted_domains")]
    pub webtransport_permitted_domains: Vec<String>,
}

mod default_settings {
    pub fn permitted_domains() -> Vec<String> {
        ["localhost", "127.0.0.1"].map(str::to_string).to_vec()
    }
}

fn default_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

impl Config {
    pub fn initialise_at(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or(anyhow!("The config path must be a file"))?;
        Ok(Self {
            path: path.to_owned(),
            version: default_version(),
            http_config: HttpConfig {
                host: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                port: 443,
                ssl_conf: SslConfig {
                    key: parent.join("key.pem"),
                    certificate: parent.join("cert.pem"),
                },
            },
            webtransport_permitted_domains: default_settings::permitted_domains(),
            authorised_keys: Default::default(),
            auth_timeout: Default::default(),
        })
    }
}

/// Bytes in base64
#[derive(Clone, Debug, Hash, Eq)]
pub struct Bytes64<B: Deref<Target = [u8]> = Vec<u8>>(pub B);

impl<B: Deref<Target = [u8]>, B2: Deref<Target = [u8]>> PartialEq<Bytes64<B2>> for Bytes64<B> {
    fn eq(&self, other: &Bytes64<B2>) -> bool {
        self.0.iter().zip(other.0.iter()).all(|(b1, b2)| *b1 == *b2)
    }
}

impl<B: Deref<Target = [u8]>> Deref for Bytes64<B> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<B: Deref<Target = [u8]>> Into<Vec<u8>> for Bytes64<B> {
    fn into(self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl FromStr for Bytes64<Vec<u8>> {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // let s = s
        //     .chars()
        //     .filter(|c| {
        //         let blacklist = &['='];
        //         !blacklist.contains(c)
        //     })
        //     .collect::<String>();
        let vec = BASE64.decode(s.as_bytes());
        Ok(Bytes64(vec?))
    }
}

impl<B: Deref<Target = [u8]>> Serialize for Bytes64<B> {
    fn serialize<S>(&self, serialiser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str = BASE64.encode(self);
        serialiser.serialize_str(&str)
    }
}

impl<B: Deref<Target = [u8]>> Display for Bytes64<B> {
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
        let bytes =
            Bytes64::from_str(&str).map_err(|err| serde::de::Error::custom(err.to_string()))?;
        Ok(bytes.into())
    }
}
