use anyhow::{Result, anyhow};
use data_encoding::BASE64;
use log::LevelFilter;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ssl_controller::{
    AsyncFilesystemMode, CertKeyPaths, GenerationMethod, SslControllerConfiguration,
};
use std::{
    collections::HashSet,
    fmt::Display,
    hash::Hash,
    iter::zip,
    net::{IpAddr, Ipv4Addr},
    ops::Deref,
    path::{Path, PathBuf},
    str::FromStr,
    sync::OnceLock,
    time::Duration,
};

use crate::auth::User;

pub static CONFIG_DIR: OnceLock<PathBuf> = Default::default();

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    pub users: HashSet<User>,
    pub host: IpAddr,
    pub port: u16,
    #[serde(flatten)]
    pub ssl: SslControllerConfiguration<AsyncFilesystemMode>,
    #[serde(default)]
    pub auth_timeout: Option<u64>,
    #[serde(default)]
    pub rate_limit: Option<u32>,
    /// Maximum verbosity for server logs, announced to clients so they stop
    /// generating records the server would discard anyway.
    #[serde(default = "default_log_level")]
    pub log_level: LevelFilter,
}

fn default_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn default_log_level() -> LevelFilter {
    LevelFilter::Info
}

impl Config {
    pub fn initialise_at(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or(anyhow!("The config path must be a file"))?;
        let host = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        Ok(Self {
            path: path.to_owned(),
            version: default_version(),
            host,
            port: 443,
            ssl: SslControllerConfiguration::new(
                CertKeyPaths {
                    cert: parent.join("cert.pem"),
                    key: parent.join("key.pem"),
                },
                GenerationMethod::SelfSigned {
                    sans: vec!["localhost".to_string(), host.to_string()],
                    ttl: Duration::from_secs(365 * 24 * 3600),
                },
                0.25,
            ),
            users: Default::default(),
            auth_timeout: Default::default(),
            rate_limit: Default::default(),
            log_level: default_log_level(),
        })
    }
}

/// Bytes in base64
#[derive(Clone, Debug, Hash, Eq)]
pub struct Bytes64<B: Deref<Target = [u8]> = Vec<u8>>(pub B);

impl<B: Deref<Target = [u8]>, B2: Deref<Target = [u8]>> PartialEq<Bytes64<B2>> for Bytes64<B> {
    fn eq(&self, other: &Bytes64<B2>) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        zip(self.0.iter(), other.0.iter()).all(|(b1, b2)| *b1 == *b2)
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

impl<'de> Deserialize<'de> for Bytes64 {
    fn deserialize<D>(deserialiser: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str = String::deserialize(deserialiser)?;
        let bytes =
            Bytes64::from_str(&str).map_err(|err| serde::de::Error::custom(err.to_string()))?;
        Ok(bytes.into())
    }
}

/// Parse `contents` as a `Config`. Dispatch on `path`'s extension first so the
/// author's intended format wins (a TOML parser would happily choke on JSON
/// that is also valid YAML and vice versa), then fall back to trying every
/// supported format so hand-written configs still load regardless of name.
///
/// Used by both the startup load (`setup_config`) and hot reload
/// (`watch_respond`) so the two paths can never drift apart.
pub fn parse_config(path: &Path, contents: &str) -> Result<Config> {
    let by_extension = match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("toml") => {
            toml::from_str(contents).map_err(anyhow::Error::from)
        }
        Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") => {
            serde_yaml::from_str(contents).map_err(anyhow::Error::from)
        }
        Some(ext) if ext.eq_ignore_ascii_case("json") => {
            serde_json::from_str(contents).map_err(anyhow::Error::from)
        }
        None => Err(anyhow!("config file \"{}\" has no extension", path.display())),
        Some(ext) => Err(anyhow!(
            "config file \"{}\" has unknown extension .{ext}",
            path.display()
        )),
    };
    by_extension.or_else(|extension_err| {
        toml::from_str(contents)
            .map_err(anyhow::Error::from)
            .or_else(|_| serde_yaml::from_str(contents).map_err(anyhow::Error::from))
            .or_else(|_| serde_json::from_str(contents).map_err(anyhow::Error::from))
            .map_err(|_| {
                anyhow!(
                    "Failed to parse config \"{}\":\n{extension_err:#}",
                    path.display()
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every round-trippable format loads through its extension, unknown or
    /// mismatched extensions still load via the format fallback, and parse
    /// errors name the offending file. (YAML is not round-tripped here:
    /// `serde_yaml` 0.9 cannot deserialize the `ssl_method` untagged enum,
    /// so `config.yaml` is exercised via the fallback path instead.)
    #[test]
    fn parse_config_dispatch_and_fallback() {
        let reference = Config::initialise_at(Path::new("/tmp/webshooter-test/config.json")).unwrap();
        let toml = toml::to_string(&reference).unwrap();
        let json = serde_json::to_string(&reference).unwrap();

        // Extension-first dispatch for the formats that round-trip natively.
        for (name, contents) in [
            ("config.toml", toml.as_str()),
            ("config.json", json.as_str()),
        ] {
            let parsed = parse_config(Path::new(name), contents).unwrap();
            assert_eq!(parsed.host, reference.host, "{name}");
            assert_eq!(parsed.port, reference.port, "{name}");
        }

        // Format fallback: content still loads under a mismatched or unknown
        // extension (JSON under .yaml goes extension → YAML parse fails →
        // fallback → JSON parse succeeds).
        for (name, contents) in [
            ("config.txt", toml.as_str()),
            ("config.yaml", json.as_str()),
            ("config", json.as_str()),
        ] {
            let parsed = parse_config(Path::new(name), contents).unwrap();
            assert_eq!(parsed.host, reference.host, "{name}");
            assert_eq!(parsed.port, reference.port, "{name}");
        }

        // Unknown content is rejected, naming the file.
        let err = parse_config(Path::new("config.toml"), "definitely not a config").unwrap_err();
        assert!(err.to_string().contains("config.toml"));
    }
}
