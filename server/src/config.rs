use anyhow::{Result, anyhow};
use data_encoding::BASE64;
use log::LevelFilter;
use macros::name_template;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ssl_controller::{
    AsyncFilesystemMode, CertKeyPaths, GenerationMethod, SslControllerConfiguration,
};
use std::{
    collections::{BTreeMap, HashSet},
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
    /// How this session's virtual display is named. Must substitute
    /// `{{ name }}`; `{{ client_id }}` is optional and decides whether the
    /// session id is folded into `name`.
    #[serde(default = "default_virtual_device_name")]
    pub virtual_display_name: NameTemplate,
    /// How this session's virtual speaker (the PipeWire sink application audio
    /// is captured from) is named. Same template rules as the display.
    #[serde(default = "default_virtual_device_name")]
    pub virtual_speaker_name: NameTemplate,
}

/// A config loaded from disk, paired with its source path for hot-reload and
/// write-back. The path is not part of the serializable config.
#[derive(Clone, Debug)]
pub struct ConfigWithPath {
    pub config: Config,
    pub path: PathBuf,
}

fn default_virtual_device_name() -> NameTemplate {
    name_template!("{{ name }}-webshooter")
}

fn default_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn default_log_level() -> LevelFilter {
    LevelFilter::Info
}

impl Default for Config {
    /// Create a new default configuration. The `path` parameter is only used
    /// to derive default certificate paths; it is not stored in the config.
    fn default() -> Self {
        let host = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        Self {
            version: default_version(),
            host,
            port: 443,
            ssl: SslControllerConfiguration::new(
                CertKeyPaths {
                    cert: PathBuf::from("cert.pem"),
                    key: PathBuf::from("key.pem"),
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
            virtual_display_name: default_virtual_device_name(),
            virtual_speaker_name: default_virtual_device_name(),
        }
    }
}

/// The key a [`NameTemplate`]'s single compiled template is registered under
/// inside its own engine. It never appears in config or output; it exists only
/// because `upon` addresses templates by name.
const TEMPLATE_KEY: &str = "self";

/// Substituted for `name` when validating a template, then looked for verbatim
/// in the rendered output. Control characters make it something no plausible
/// config file contains, so finding it in the output can only mean the template
/// really substituted that parameter.
const NAME_PROBE: &str = "\u{1}ws-name\u{1}";


/// A configured name template, compiled and validated once when the config is
/// parsed.
///
/// Both parameters are always bound when rendering: `name` and `client_id`.
/// `name` is the session's user-derived name and `client_id` its numeric id.
/// `client_id` is an optional hole — it is always *available* to the template,
/// but whether it is *used* is what decides how `name` is built (see
/// [`NameTemplate::references_client_id`]).
///
/// A template that never substitutes `{{ name }}` is rejected at parse time. A
/// name that silently ignored the session would render the same string for
/// every session — two PipeWire sinks of one name collide, and the `.monitor`
/// source `pulsesrc` opens becomes ambiguous — so this is a load-time error
/// rather than something to discover during a call.
#[derive(Debug)]
pub struct NameTemplate {
    /// The template's own engine. `upon::Template` borrows its source, so a
    /// template and its `String` cannot sit side by side in one struct; the
    /// engine owns an owned copy instead, which also means the template is
    /// compiled exactly once.
    engine: upon::Engine<'static>,
    /// Whether the template references `{{ client_id }}`, resolved once here
    /// rather than re-decided at every render.
    references_client_id: bool,
}

impl NameTemplate {
    /// Compile and validate `source`, or explain why it is unusable.
    pub fn parse(source: &str) -> Result<Self> {
        let mut engine = upon::Engine::new();
        engine.add_template(TEMPLATE_KEY, source.to_owned())?;
        let template = engine.template(TEMPLATE_KEY);

        // Substituting probes for the parameters and finding them in the output
        // is the only portable way to ask "does this template actually use this
        // parameter?"; `upon` has no variable introspection, and a template
        // that merely mentions an unbound name renders empty rather than
        // failing. The two probes have to be distinguishable from each other,
        // or every template looks like it uses both.
        //
        // This is also what catches a name the template does not have: an
        // unbound variable is a render error, not an empty string, so it
        // surfaces here at config load rather than at the first session.
        let probed = render_probe(template).map_err(|err| {
            anyhow!(
                "name template \"{source}\" does not render: {err}\n  \
                 only `name` and `client_id` are available"
            )
        })?;
        if !probed.contains(NAME_PROBE) {
            return Err(anyhow!(
                "name template \"{source}\" does not reference {{{{ name }}}}: \
                 every name template must substitute {{{{ name }}}}"
            ));
        }

        Ok(Self {
            references_client_id: probed.contains(CLIENT_ID_PROBE),
            engine,
        })
    }

    /// The template as written, for serialising the config back out.
    pub fn source(&self) -> &str {
        self.engine.template(TEMPLATE_KEY).source()
    }

    /// Whether the template substitutes `{{ client_id }}`.
    ///
    /// This is what makes `name` unique only when it has to be: a template
    /// that spells the id out is naming itself, so `name` is left as the plain
    /// user name and the template decides where the id goes.
    pub fn references_client_id(&self) -> bool {
        self.references_client_id
    }

    /// Render this template for one session.
    pub fn render(&self, name: &str, client_id: u64) -> Result<String> {
        let ctx = BTreeMap::from([
            ("name".to_owned(), name.to_owned()),
            ("client_id".to_owned(), client_id.to_string()),
        ]);
        // The error carries the source: a template's author is the one who can
        // fix it, and `upon`'s message alone does not say which template.
        self.engine
            .template(TEMPLATE_KEY)
            .render(ctx)
            .to_string()
            .map_err(|err| anyhow!("name template \"{}\": {err}", self.source()))
    }
}

/// Render `template` with each parameter bound to its own probe, so the two are
/// told apart in the output.
fn render_probe(template: upon::TemplateRef<'_>) -> Result<String> {
    let ctx = BTreeMap::from([
        ("name".to_owned(), NAME_PROBE.to_owned()),
        ("client_id".to_owned(), CLIENT_ID_PROBE.to_owned()),
    ]);
    Ok(template.render(ctx).to_string()?)
}

impl Clone for NameTemplate {
    fn clone(&self) -> Self {
        // SAFETY: Since the NameTemplate was built succesfully its source is necessarily valid
        unsafe { Self::parse(self.source()).unwrap_unchecked() }
    }
}

impl FromStr for NameTemplate {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl Serialize for NameTemplate {
    fn serialize<S>(&self, serialiser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialiser.serialize_str(self.source())
    }
}

impl<'de> Deserialize<'de> for NameTemplate {
    fn deserialize<D>(deserialiser: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let source = String::deserialize(deserialiser)?;
        Self::parse(&source).map_err(serde::de::Error::custom)
    }
}

/// The same, for the optional `client_id` hole.
const CLIENT_ID_PROBE: &str = "\u{1}ws-client-id\u{1}";

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

/// The extensions [`parse_config`] dispatches on, in the order
/// [`discover_config`] prefers them when several are side by side.
///
/// `.json5` and `.jsonc` deliberately outrank plain `.json`: those names only
/// ever turn up because someone wrote a config by hand, whereas `config.json`
/// is what this server generates for itself, so a hand-written config must
/// never be silently shadowed by a generated leftover.
pub const CONFIG_EXTENSIONS: &[&str] = &["toml", "yaml", "yml", "json5", "jsonc", "json"];

/// The subset of [`CONFIG_EXTENSIONS`] read by the JSON5 parser. They share one
/// parser because JSON5 is a strict superset of JSON: `.json` stays plain JSON
/// when *we* write it (see `main::update_config`) so other tools can still read
/// it, but anything hand-written may use `//` and `/* */` comments, trailing
/// commas, single-quoted strings and unquoted keys.
const JSON_EXTENSIONS: &[&str] = &["json5", "jsonc", "json"];

fn is_json_extension(ext: &str) -> bool {
    JSON_EXTENSIONS
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known))
}

/// The config file to load out of `dir`, or `None` if there is none.
///
/// An exact `config.<ext>` for an extension in [`CONFIG_EXTENSIONS`] wins
/// outright, in that list's order; failing one, a `config`-prefixed variant
/// (`config.local.json`) still counts, as it always has. Both passes impose an
/// order of their own, because `read_dir`'s would otherwise decide which of two
/// configs silently wins.
///
/// Callers generate a fresh `config.json` when this returns `None`.
pub fn discover_config(dir: &Path) -> Option<PathBuf> {
    let supported = |path: &Path| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                CONFIG_EXTENSIONS
                    .iter()
                    .any(|known| ext.eq_ignore_ascii_case(known))
            })
    };
    let variant = |path: &Path| {
        // Note that a bare `starts_with("config")` also matches `configx.json`.
        // That is what this has always done, and narrowing it to `config.`,
        // `config-` and `config_` is a separate change to make deliberately.
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.len() > "config".len() && name.starts_with("config"))
    };

    CONFIG_EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("config.{ext}")))
        .find(|path| path.is_file())
        .or_else(|| {
            let mut variants = std::fs::read_dir(dir)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| variant(path) && supported(path) && path.is_file())
                .collect::<Vec<_>>();
            variants.sort();
            variants.into_iter().next()
        })
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
        Some(ext) if is_json_extension(ext) => {
            json5::from_str(contents).map_err(anyhow::Error::from)
        }
        None => Err(anyhow!(
            "config file \"{}\" has no extension",
            path.display()
        )),
        Some(ext) => Err(anyhow!(
            "config file \"{}\" has unknown extension .{ext}",
            path.display()
        )),
    };
    by_extension.or_else(|extension_err| {
        (|_| toml::from_str(contents).map_err(anyhow::Error::from))(())
            // JSON5 before YAML: both accept a flow mapping, and `serde_yaml`
            // only fails a JSON5 config on the `//` or `/* */` comments and
            // trailing commas this dialect exists to accept. Serde ignores
            // unknown fields, so such a file would otherwise "parse" as YAML
            // with the comments silently mistaken for keys.
            .or_else(|_| json5::from_str(contents).map_err(anyhow::Error::from))
            .or_else(|_| serde_yaml::from_str(contents).map_err(anyhow::Error::from))
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
    use macros::name_template;

    use super::*;

    /// Every round-trippable format loads through its extension, unknown or
    /// mismatched extensions still load via the format fallback, and parse
    /// errors name the offending file. (YAML is not round-tripped here:
    /// `serde_yaml` 0.9 cannot deserialize the `ssl_method` untagged enum,
    /// so `config.yaml` is exercised via the fallback path instead.)
    #[test]
    fn parse_config_dispatch_and_fallback() {
        let reference = Config::default();
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

    /// Both name options default to `{{ name }}-webshooter` and render that
    /// literal, and survive a config round-trip as the source they were written
    /// as rather than as something re-derived.
    #[test]
    fn name_templates_default_and_round_trip() {
        let default_virtual_device_name = default_virtual_device_name();
        let config = Config::default();
        for template in [&config.virtual_display_name, &config.virtual_speaker_name] {
            assert_eq!(template.source(), default_virtual_device_name.source());
            assert!(!template.references_client_id());
            assert_eq!(template.render("alice", 1).unwrap(), "alice-webshooter");
        }

        let serialised = toml::to_string(&config).unwrap();
        let parsed = parse_config(Path::new("config.toml"), &serialised).unwrap();
        assert_eq!(
            parsed.virtual_display_name.source(),
            default_virtual_device_name.source()
        );
        assert_eq!(
            parsed.virtual_speaker_name.source(),
            default_virtual_device_name.source()
        );
    }

    /// A name template that never substitutes `{{ name }}` is a parse error, not
    /// a name that silently ignores the session — and a template naming some
    /// other variable does not count as referencing `name`.
    #[test]
    fn name_template_without_name_is_a_parse_error() {
        for source in [
            "webshooter",                              // no parameter at all
            "{{ client_id }}-webshooter",              // only the optional hole
            "{% if name %}{{ client_id }}{% endif %}", // name only tested, never emitted
        ] {
            let err = NameTemplate::parse(source)
                .err()
                .unwrap_or_else(|| panic!("{source:?} should not parse"));
            assert!(
                err.to_string().contains("{{ name }}"),
                "error for {source:?} should name the missing parameter: {err}"
            );
        }

        // The minimum that does parse. The test is that `{{ name }}` reaches
        // the output, not that it does so unconditionally — `name` is always
        // bound, so a conditional substitution is still a substitution.
        for source in ["{{ name }}", "{% if name %}{{ name }}{% endif %}"] {
            assert_eq!(
                NameTemplate::parse(source)
                    .unwrap()
                    .render("alice", 7)
                    .unwrap(),
                "alice"
            );
        }
    }

    /// A name the template does not have is a load-time error, and the error
    /// has to say which parameters exist — `upon` itself only reports "not found
    /// in this scope", which leaves the author guessing between the two holes.
    #[test]
    fn an_unknown_parameter_lists_the_ones_that_exist() {
        let err = NameTemplate::parse("{{ user }}-webshooter")
            .unwrap_err()
            .to_string();
        assert!(err.contains("user"), "{err}");
        assert!(
            err.contains("`name` and `client_id` are available"),
            "{err}"
        );
    }

    /// `client_id` is an optional hole, but it is always bound: a template that
    /// spells it out renders it, and is recognised as doing so.
    #[test]
    fn client_id_is_always_bound() {
        let template = name_template!("{{ client_id }}-{{ name }}");
        assert!(template.references_client_id());
        assert_eq!(template.render("alice", 42).unwrap(), "42-alice");
    }

    /// A malformed template is rejected at parse time, before anything can
    /// render it.
    #[test]
    fn malformed_name_template_is_a_parse_error() {
        assert!(NameTemplate::parse("{{ name }").is_err());
        assert!(NameTemplate::parse("{{ }}").is_err());
    }

    /// The JSON dialect is JSON5, so a config may carry the things a JSON
    /// config never could — `//` and `/* */` comments, trailing commas,
    /// single-quoted strings and unquoted keys — and every extension that names
    /// it reads them the same way.
    #[test]
    fn json_dialect_accepts_comments_and_trailing_commas() {
        let json5 = r#"{
            /* webshooter configuration.
               Anything JSON5 allows is fair game here. */
            host: '127.0.0.1',   // a single-quoted string
            port: 8443,
            paths: {
                cert: '/etc/webshooter/ssl.crt',
                key: '/etc/webshooter/ssl.key',
            },
            ssl_method: {
                SelfSigned: {
                    sans: ['localhost'],
                    ttl: { secs: 31536000, nanos: 0 },
                },
            },
            renew_before_ratio: 0.25,
            auth_timeout: 30,
            // Name templates get the same treatment.
            virtual_display_name: '{{ name }}-{{ client_id }}-display',
        }"#;
        for name in JSON_EXTENSIONS.iter().map(|ext| format!("config.{ext}")) {
            let config = parse_config(Path::new(&name), json5).unwrap();
            assert_eq!(
                config.host,
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "{name}"
            );
            assert_eq!(config.port, 8443, "{name}");
            assert_eq!(config.auth_timeout, Some(30), "{name}");
            assert!(config.virtual_display_name.references_client_id(), "{name}");
            assert_eq!(
                config.virtual_display_name.render("alice", 3).unwrap(),
                "alice-3-display",
                "{name}"
            );
        }
    }

    /// Being more permissive when reading must not cost anything when writing:
    /// strict JSON is a subset of JSON5, so it still loads under every JSON
    /// extension. This is the file shape `update_config` writes and the shape
    /// every other JSON tool in the world accepts.
    #[test]
    fn strict_json_still_loads_under_every_json_extension() {
        let reference = Config::default();
        let json = serde_json::to_string(&reference).unwrap();
        for name in JSON_EXTENSIONS.iter().map(|ext| format!("config.{ext}")) {
            let parsed = parse_config(Path::new(&name), &json).unwrap();
            assert_eq!(parsed.host, reference.host, "{name}");
            assert_eq!(parsed.port, reference.port, "{name}");
            assert_eq!(
                parsed.virtual_display_name.source(),
                default_virtual_device_name().source(),
                "{name}"
            );
        }
    }

    /// A permissive dialect is only a superset of JSON syntax, not of JSON
    /// meaning: a typo, a missing field or a broken template is still a load
    /// error, reported against the file the user actually has.
    #[test]
    fn a_broken_json5_config_is_still_an_error() {
        for (name, contents) in [
            ("config.jsonc", "{ host: '127.0.0.1', port: 8443 "), // truncated
            ("config.json5", "{ host: '127.0.0.1', port: nope }"), // not a number
            ("config.json", "not a config at all"),
            ("config.jsonc", "{ host: '127.0.0.1', port: 8443 } trailing"), // junk after
        ] {
            let err = parse_config(Path::new(name), contents)
                .err()
                .unwrap_or_else(|| panic!("{name} should not have parsed {contents:?}"));
            assert!(err.to_string().contains(name), "{err}");
        }
    }

    /// Discovery finds every dialect `parse_config` reads, and when several are
    /// side by side the choice follows [`CONFIG_EXTENSIONS`] rather than
    /// `read_dir` order. A hand-written `.json5` outranks a generated `.json`
    /// for exactly that reason — otherwise the config a user wrote is silently
    /// ignored because an older file happens to sort first.
    #[test]
    fn discovery_ranks_config_files_by_preference() {
        let dir = std::env::temp_dir().join("webshooter-discover-config-test");
        let names = CONFIG_EXTENSIONS
            .iter()
            .map(|ext| format!("config.{ext}"))
            .collect::<Vec<_>>();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(discover_config(&dir), None);

        // Written lowest-preference first, so a `read_dir`-order
        // implementation would pick the last one rather than the first.
        for name in names.iter().rev() {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        let winner = |dir: &Path| {
            discover_config(dir).and_then(|path| {
                path.file_name()
                    .map(|name| name.to_str().unwrap().to_owned())
            })
        };
        assert_eq!(winner(&dir).as_deref(), Some(&*names[0]));

        // Removing the winner hands the decision to the next one down, until
        // nothing is left to load.
        for (index, name) in names.iter().enumerate() {
            std::fs::remove_file(dir.join(name)).unwrap();
            assert_eq!(
                winner(&dir).as_deref(),
                names.get(index + 1).map(String::as_str),
                "after removing {name}"
            );
        }
        assert_eq!(discover_config(&dir), None);

        // A file the parser cannot read is not a config, however close its
        // name looks to one that is.
        for stray in ["config.conf", "config.json5.bak", "webshooter.json"] {
            std::fs::write(dir.join(stray), "{}").unwrap();
        }
        assert_eq!(discover_config(&dir), None);

        // With no exact name present, a `config`-prefixed variant still counts
        // as it always has — and two of them resolve the same way every time
        // rather than by `read_dir` order.
        for variant in ["config-staging.json5", "config-local.json"] {
            std::fs::write(dir.join(variant), "{}").unwrap();
        }
        assert_eq!(winner(&dir).as_deref(), Some("config-local.json"));

        // ...but an exact name takes precedence over any of them.
        std::fs::write(dir.join("config.toml"), "{}").unwrap();
        assert_eq!(winner(&dir).as_deref(), Some("config.toml"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every name [`discover_config`] can return is one [`parse_config`]
    /// dispatches on rather than rejecting outright — the two read the same
    /// list, so neither can quietly fall behind the other.
    #[test]
    fn discovery_only_returns_names_the_parser_knows() {
        for ext in CONFIG_EXTENSIONS {
            let name = format!("config.{ext}");
            let err = parse_config(Path::new(&name), "{}")
                .err()
                .unwrap_or_else(|| panic!("{name} should reject {{}}, not parse it"));
            assert!(
                !err.to_string().contains("unknown extension"),
                "{name} is discoverable but unparseable: {err}"
            );
        }
    }
}
