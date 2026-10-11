//! Configuration: built-in defaults, then a TOML file, then environment variables, then
//! command-line flags, each overriding the one before.
//!
//! API keys come only from the file or the environment, never flags, since flags show up in
//! the process list. An unknown key in the file, or a value that does not make sense, stops
//! the server before it binds anything.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::{Deserialize, Serialize};

/// The smallest key accepted, so a key cannot be guessed by trying them all.
pub const MIN_KEY_LEN: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: Server,
    pub storage: Storage,
    pub auth: Auth,
    pub log: Log,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// An IP address, loopback unless set otherwise.
    pub host: IpAddr,
    pub port: u16,
    pub max_body_bytes: usize,
    /// Host names to accept while no keys are set, beyond IP addresses and `localhost`, such
    /// as the machine's tailnet name.
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Storage {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Auth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_key: Option<String>,
    /// Allows listening beyond loopback with no keys set.
    pub allow_anonymous: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Log {
    /// A filter such as `info` or `nocturnum_api=debug,info`.
    pub level: String,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 6333,
            max_body_bytes: nocturnum_api::DEFAULT_MAX_BODY_BYTES,
            allowed_hosts: Vec::new(),
        }
    }
}

impl Default for Storage {
    fn default() -> Self {
        Storage {
            path: PathBuf::from("./data"),
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Log {
            level: "info".into(),
        }
    }
}

/// Every table and key the config file knows, the only words a parse error may quote.
const FIELDS: [&str; 13] = [
    "server",
    "storage",
    "auth",
    "log",
    "host",
    "port",
    "max_body_bytes",
    "allowed_hosts",
    "path",
    "admin_key",
    "read_only_key",
    "allow_anonymous",
    "level",
];

/// A parse error's message with every value left out: the parser quotes what it could not
/// read, so only the text before the first quote is kept, followed by the known field names
/// the message suggests.
fn scrub(message: &str) -> String {
    let head = message
        .split(['`', '"'])
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', ':', ','])
        .to_owned();
    let named: Vec<String> = FIELDS
        .iter()
        .filter(|f| message.contains(&format!("`{f}`")))
        .map(|f| format!("`{f}`"))
        .collect();
    if named.is_empty() {
        head
    } else {
        format!("{head}, expected one of {}", named.join(", "))
    }
}

/// nocturnum, a vector database.
#[derive(Debug, Default, Parser)]
#[command(version, about)]
pub struct Cli {
    /// A TOML config file.
    #[arg(long, env = "NOCTURNUM_CONFIG")]
    pub config: Option<PathBuf>,
    /// The IP address to listen on.
    #[arg(long, env = "NOCTURNUM_HOST")]
    pub host: Option<IpAddr>,
    #[arg(long, env = "NOCTURNUM_PORT")]
    pub port: Option<u16>,
    /// The data directory.
    #[arg(long, env = "NOCTURNUM_DATA")]
    pub data: Option<PathBuf>,
    /// Allows listening beyond loopback with no API keys set, `--allow-anonymous=false`
    /// overrides a `true` in the file.
    #[arg(
        long,
        env = "NOCTURNUM_ALLOW_ANONYMOUS",
        num_args = 0..=1,
        default_missing_value = "true",
        value_name = "BOOL"
    )]
    pub allow_anonymous: Option<bool>,
    /// Prints the effective config, with keys hidden, and exits.
    #[arg(long)]
    pub print_config: bool,
}

/// Where the keys come from besides the file, read from the environment by [`Env::read`].
#[derive(Debug, Default)]
pub struct Env {
    pub admin_key: Option<String>,
    pub read_only_key: Option<String>,
    pub log: Option<String>,
}

impl Env {
    pub fn read() -> Env {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        Env {
            admin_key: var("NOCTURNUM_ADMIN_KEY"),
            read_only_key: var("NOCTURNUM_READ_ONLY_KEY"),
            log: var("NOCTURNUM_LOG"),
        }
    }
}

impl Config {
    /// Builds the effective config and checks it.
    pub fn load(cli: &Cli, env: Env) -> Result<Config, String> {
        let mut config = match &cli.config {
            Some(path) => Config::from_file(path)?,
            None => Config::default(),
        };
        if env.admin_key.is_some() {
            config.auth.admin_key = env.admin_key;
        }
        if env.read_only_key.is_some() {
            config.auth.read_only_key = env.read_only_key;
        }
        if let Some(level) = env.log {
            config.log.level = level;
        }
        if let Some(host) = cli.host {
            config.server.host = host;
        }
        if let Some(port) = cli.port {
            config.server.port = port;
        }
        if let Some(data) = &cli.data {
            config.storage.path = data.clone();
        }
        if let Some(allow) = cli.allow_anonymous {
            config.auth.allow_anonymous = allow;
        }
        config.check()?;
        Ok(config)
    }

    fn from_file(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        // The error's own text quotes the offending line, which could hold a key, so only its
        // position, a known field name and a message with quoted values blanked are shown.
        toml::from_str(&text).map_err(|e| {
            let at = e.span().map_or(0, |s| s.start).min(text.len());
            let before = &text[..at];
            let line = before.matches('\n').count() + 1;
            let start = before.rfind('\n').map_or(0, |i| i + 1);
            let column = before.len() - start + 1;
            let key = text[start..]
                .lines()
                .next()
                .and_then(|l| l.split_once('='))
                .map(|(k, _)| k.trim())
                .filter(|k| FIELDS.contains(k))
                .map(|k| format!(" in `{k}`"))
                .unwrap_or_default();
            format!(
                "{}: line {line}, column {column}{key}: {}",
                path.display(),
                scrub(e.message())
            )
        })
    }

    pub fn check(&self) -> Result<(), String> {
        let auth = &self.auth;
        for (name, key) in [
            ("auth.admin_key", &auth.admin_key),
            ("auth.read_only_key", &auth.read_only_key),
        ] {
            if let Some(k) = key {
                if k.chars().count() < MIN_KEY_LEN {
                    return Err(format!("{name} must be at least {MIN_KEY_LEN} characters"));
                }
                // Header values are read as visible ASCII, so any other key could never match.
                if !k.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err(format!(
                        "{name} must be visible ASCII characters, without spaces"
                    ));
                }
            }
        }
        if auth.read_only_key.is_some() && auth.admin_key.is_none() {
            return Err("auth.read_only_key needs auth.admin_key set too".into());
        }
        if auth.read_only_key.is_some() && auth.read_only_key == auth.admin_key {
            return Err("auth.read_only_key must differ from auth.admin_key".into());
        }
        let anonymous = auth.admin_key.is_none();
        if anonymous && !self.server.host.is_loopback() && !auth.allow_anonymous {
            return Err(format!(
                "refusing to listen on {} with no API keys set, set auth.admin_key, or \
                 auth.allow_anonymous = true to allow it",
                self.server.host
            ));
        }
        if !(1024..=1 << 30).contains(&self.server.max_body_bytes) {
            return Err("server.max_body_bytes must be between 1 KiB and 1 GiB".into());
        }
        if self
            .server
            .allowed_hosts
            .iter()
            .any(|h| h.trim().is_empty())
        {
            return Err("server.allowed_hosts must not hold an empty name".into());
        }
        if self.storage.path.as_os_str().is_empty() {
            return Err("storage.path must not be empty".into());
        }
        tracing_subscriber::EnvFilter::try_new(&self.log.level)
            .map_err(|e| format!("log.level {:?}: {e}", self.log.level))?;
        Ok(())
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(self.server.host, self.server.port)
    }

    /// The config as TOML with keys hidden, for the startup log.
    pub fn redacted(&self) -> String {
        let mut c = self.clone();
        let hide = |k: &mut Option<String>| {
            if k.is_some() {
                *k = Some("(set)".into());
            }
        };
        hide(&mut c.auth.admin_key);
        hide(&mut c.auth.read_only_key);
        toml::to_string(&c).unwrap_or_else(|e| format!("(cannot print the config: {e})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> (tempfile::TempDir, Cli) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nocturnum.toml");
        std::fs::write(&path, text).unwrap();
        let cli = Cli {
            config: Some(path),
            ..Cli::default()
        };
        (dir, cli)
    }

    #[test]
    fn defaults_listen_on_loopback() {
        let c = Config::load(&Cli::default(), Env::default()).unwrap();
        assert_eq!(c.addr(), "127.0.0.1:6333".parse().unwrap());
        assert_eq!(c.storage.path, PathBuf::from("./data"));
    }

    #[test]
    fn later_layers_win() {
        let (_d, mut cli) = file(
            "[server]\nport = 7000\nhost = \"127.0.0.2\"\n[auth]\nadmin_key = \"from-the-file-0123\"\n",
        );
        cli.port = Some(7001);
        let env = Env {
            admin_key: Some("from-the-env-01234".into()),
            ..Env::default()
        };
        let c = Config::load(&cli, env).unwrap();
        assert_eq!(c.server.port, 7001);
        assert_eq!(c.server.host, "127.0.0.2".parse::<IpAddr>().unwrap());
        assert_eq!(c.auth.admin_key.as_deref(), Some("from-the-env-01234"));
    }

    #[test]
    fn refuses_bad_configs() {
        let refused = |text: &str| {
            let (_d, cli) = file(text);
            Config::load(&cli, Env::default()).unwrap_err()
        };
        assert!(refused("[server]\nprot = 1\n").contains("unknown field"));
        assert!(refused("[server]\nhost = \"0.0.0.0\"\n").contains("no API keys"));
        assert!(refused("[server]\nhost = \"my-server\"\n").contains("host"));
        assert!(refused("[auth]\nadmin_key = \"short\"\n").contains("at least"));
        assert!(refused("[auth]\nread_only_key = \"long-enough-key-1\"\n").contains("admin_key"));
        assert!(
            refused(
                "[auth]\nadmin_key = \"long-enough-key-1\"\nread_only_key = \"long-enough-key-1\"\n"
            )
            .contains("differ")
        );
        assert!(refused("[server]\nmax_body_bytes = 10\n").contains("max_body_bytes"));
        assert!(refused("[auth]\nadmin_key = \"abcdefghijklmnop\u{e9}\"\n").contains("ASCII"));
        assert!(refused("[auth]\nadmin_key = \"abcdefgh ijklmnop\"\n").contains("ASCII"));
        assert!(refused("[log]\nlevel = \"=bad=\"\n").contains("log.level"));
    }

    #[test]
    fn public_binds_need_keys_or_an_explicit_choice() {
        let (_d, cli) =
            file("[server]\nhost = \"0.0.0.0\"\n[auth]\nadmin_key = \"long-enough-key-1\"\n");
        assert!(Config::load(&cli, Env::default()).is_ok());
        let (_d, cli) = file("[server]\nhost = \"0.0.0.0\"\n[auth]\nallow_anonymous = true\n");
        assert!(Config::load(&cli, Env::default()).is_ok());
    }

    #[test]
    fn anonymous_access_can_be_switched_off_again() {
        let (_d, mut cli) = file("[server]\nhost = \"0.0.0.0\"\n[auth]\nallow_anonymous = true\n");
        cli.allow_anonymous = Some(false);
        assert!(Config::load(&cli, Env::default()).is_err());
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().allow_anonymous;
        assert_eq!(parse(&["nocturnum"]), None);
        assert_eq!(parse(&["nocturnum", "--allow-anonymous"]), Some(true));
        assert_eq!(
            parse(&["nocturnum", "--allow-anonymous=false"]),
            Some(false)
        );
    }

    #[test]
    fn parse_errors_never_quote_keys() {
        let error = |text: &str| {
            let (_d, cli) = file(text);
            let e = Config::load(&cli, Env::default()).unwrap_err();
            assert!(!e.contains("sentinel"), "{e}");
            assert!(!e.contains("11111111"), "{e}");
            e
        };
        let e = error("[auth]\nadmin_key = \"sentinel-in-the-file-0123\",\n");
        assert!(e.contains("line 2") && e.contains("`admin_key`"), "{e}");
        error("[auth]\nadmin_key = \"\"\"\nsentinel-value-0123=tail\n");
        error("[auth]\nadmin_key = 1111111111111111\n");
        error("[auth]\nallow_anonymous = \"sentinel-value-0123\"\n");
        error("[auth]\nsentinel-value-0123 = 1\n");
        error("[auth]\nallow_anonymous = \"prefix\\\"sentinel-value-0123\"\n");
        error("[auth]\n\"a`sentinel-value-0123`b\" = 1\n");
        let e = error("[server]\nprot = 1\n");
        assert!(e.contains("unknown field") && e.contains("`host`"), "{e}");
    }

    #[test]
    fn scrub_keeps_only_field_names() {
        assert_eq!(
            scrub("invalid type: string \"a\\\"b`c\", expected a boolean"),
            "invalid type: string"
        );
        assert_eq!(
            scrub("unknown field `pr\"ot`, expected one of `host`, `port`"),
            "unknown field, expected one of `host`, `port`"
        );
    }

    #[test]
    fn redacted_hides_keys() {
        let mut c = Config::default();
        c.auth.admin_key = Some("a-very-secret-key-1".into());
        let text = c.redacted();
        assert!(!text.contains("a-very-secret-key-1"));
        assert!(text.contains("admin_key = \"(set)\""));
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.server, c.server);
    }
}
