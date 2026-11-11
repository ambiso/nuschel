// SPDX-License-Identifier: EUPL-1.2
//! The TOML configuration file. Names follow murmur.ini where an equivalent
//! exists (in snake_case); defaults are stock Murmur's. Secrets (the server
//! password, the database URL, private keys) are read from files so that a
//! configuration file can live in a world-readable place such as the Nix store.
use regex::Regex;
use serde::Deserialize;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// Stock Mumble clients are admitted; there is no end-to-end encryption.
    #[default]
    Compatible,
    /// End-to-end encrypted conversations; refuses stock clients. Not
    /// implemented yet: the server refuses to start in this mode.
    E2ee,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Autoban {
    /// Connection attempts per address within `timeframe` before a temporary
    /// ban; 0 disables it.
    pub attempts: u32,
    pub timeframe: u64,
    pub duration: u64,
    /// Count successful logins too (Murmur's `autobanSuccessfulConnections`).
    pub count_successful: bool,
}

impl Default for Autoban {
    fn default() -> Self {
        Self {
            attempts: 10,
            timeframe: 120,
            duration: 300,
            count_successful: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PasswordHashing {
    pub memory_kib: u32,
    pub passes: u32,
}

impl Default for PasswordHashing {
    fn default() -> Self {
        let cost = crate::password::Cost::default();
        Self {
            memory_kib: cost.memory_kib,
            passes: cost.passes,
        }
    }
}

/// Who may read history without an account (docs/extension.md §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestHistory {
    /// Unregistered users read no history; they see live messages.
    #[default]
    None,
    /// The same rules as for registered users.
    Full,
}

/// Persistent history for Nuschel clients (docs/extension.md §4–§9).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct History {
    pub guests: GuestHistory,
    /// Days messages are kept; 0 keeps them forever.
    pub retention_days: u32,
    /// How often one message may be edited.
    pub edit_limit: u32,
    /// Users with Write on a channel may delete other people's messages there.
    pub moderators_delete: bool,
    /// Store direct messages from stock clients, who cannot know they are kept.
    pub archive_stock_direct_messages: bool,
    /// Tell stock clients that text is kept: in the welcome text and when
    /// they enter a channel.
    pub notify_stock_clients: bool,
    /// History pages per minute per connection, and their bytes.
    pub pages_per_minute: u32,
    pub page_bytes_per_minute: u32,
    /// Advisory client versions announced to Nuschel clients ("1.0.0").
    pub minimum_client: Option<String>,
    pub recommended_client: Option<String>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            guests: GuestHistory::None,
            retention_days: 0,
            edit_limit: 20,
            moderators_delete: true,
            archive_stock_direct_messages: false,
            notify_stock_clients: true,
            pages_per_minute: 120,
            page_bytes_per_minute: 16 * 1024 * 1024,
            minimum_client: None,
            recommended_client: None,
        }
    }
}

/// Who may fetch an account's key-transparency history besides its owner
/// (docs/e2ee.md §5.7: a history reveals every past key and when it changed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KtHistoryAccess {
    /// People the account talks to: a direct conversation with it, or the same
    /// channel right now.
    #[default]
    Contacts,
    /// Every connection that may look the account up.
    Members,
}

/// Key transparency (docs/e2ee.md §5): the server's verifiable key directory.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kt {
    /// Run the directory and offer `kt/1` to Nuschel clients.
    pub enabled: bool,
    /// The head-signing key, which is the server's identity key: a file with 32
    /// random bytes or 64 hex digits. Without it one is generated once in
    /// `state_directory` (`kt-signing.key`). Clients pin it: keep it.
    pub signing_key_file: Option<PathBuf>,
    /// The VRF secret, in the same format (`kt-vrf.key` when generated). It hides
    /// labels and can never change for the life of the log.
    pub vrf_secret_file: Option<PathBuf>,
    /// Seconds between epochs while bindings are waiting.
    pub publish_interval: u64,
    /// Seconds after which the current epoch is signed again while nothing
    /// changes, so that a fresh head always exists.
    pub refresh_interval: u64,
    /// Per connection and minute.
    pub lookups_per_minute: u32,
    /// Histories, audits and heads, per connection and minute.
    pub histories_per_minute: u32,
    pub publishes_per_minute: u32,
    pub history: KtHistoryAccess,
}

impl Default for Kt {
    fn default() -> Self {
        Self {
            enabled: true,
            signing_key_file: None,
            vrf_secret_file: None,
            publish_interval: 10,
            refresh_interval: 3600,
            lookups_per_minute: 120,
            histories_per_minute: 60,
            publishes_per_minute: 6,
            history: KtHistoryAccess::Contacts,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub security: Security,
    /// Addresses for both TCP and UDP. IPv6 sockets are IPv6-only, so `::` and
    /// `0.0.0.0` can be bound together.
    pub bind: Vec<IpAddr>,
    /// TCP and UDP port; 0 picks a free one (tests).
    pub port: u16,
    /// Generated certificates and the default SQLite database live here.
    pub state_directory: PathBuf,
    /// SQLite database; relative paths are inside `state_directory`.
    pub database: PathBuf,
    /// A file holding a `sqlite:` or `postgresql://` URL; it is only read by
    /// the storage worker. Replaces `database` when set.
    pub database_url_file: Option<PathBuf>,
    /// PEM certificate chain and private key. Without them a self-signed
    /// certificate is generated once and kept in `state_directory`.
    pub certificate: Option<PathBuf>,
    pub private_key: Option<PathBuf>,
    /// The storage worker executable; defaults to `nuschel-storage-worker`
    /// next to `nuscheld`.
    pub storage_worker: Option<PathBuf>,

    /// The root channel's name (murmur.ini `registerName`).
    pub registered_name: String,
    pub welcome_text: String,
    pub welcome_text_file: Option<PathBuf>,
    /// The server password for unregistered users. Prefer `password_file`.
    pub password: String,
    pub password_file: Option<PathBuf>,
    /// Applied at startup: the SuperUser's password is set to this file's
    /// contents (an existing different password is replaced).
    pub superuser_password_file: Option<PathBuf>,

    pub max_users: u32,
    pub max_users_per_channel: u32,
    /// Bits per second per user, including overhead (murmur.ini `bandwidth`).
    pub max_bandwidth: u32,
    /// Seconds without any message before a client is dropped.
    pub timeout: u64,
    /// Seconds for TLS and authentication to complete.
    pub handshake_timeout: u64,
    pub text_message_length: u32,
    pub image_message_length: u32,
    pub allow_html: bool,
    pub default_channel: u32,
    pub remember_channel: bool,
    /// Seconds; 0 remembers the last channel forever.
    pub remember_channel_duration: u64,
    /// Answer unencrypted pings from server browsers.
    pub allow_ping: bool,
    /// Stock clients' server lists send 12-byte pings and expect 24-byte
    /// answers. Off by default: an answer is never larger than its request, so
    /// the server cannot amplify spoofed traffic. On: answers up to twice the
    /// request (rate-limited per address and globally), and stock clients show
    /// the user count and ping.
    pub allow_ping_amplification: bool,
    pub certificate_required: bool,
    pub send_version: bool,
    pub allow_recording: bool,
    pub channel_nesting_limit: u32,
    pub channel_count_limit: u32,
    pub username_pattern: String,
    pub channel_name_pattern: String,
    /// Control messages per second and burst (murmur.ini `messagelimit`,
    /// `messageburst`). Excess messages are dropped, as Murmur does.
    pub message_limit: u32,
    pub message_burst: u32,
    pub plugin_message_limit: u32,
    pub plugin_message_burst: u32,
    /// `None` is unlimited (Murmur's -1).
    pub listeners_per_channel: Option<u32>,
    pub listeners_per_user: Option<u32>,
    pub broadcast_listener_volume_adjustments: bool,
    pub suggest_version: Option<String>,
    pub suggest_positional: Option<bool>,
    pub suggest_push_to_talk: Option<bool>,

    /// Simultaneous connections, authenticated or not, from one address.
    pub connections_per_address: u32,
    /// Connections that have not authenticated yet, server-wide.
    pub max_pending_connections: u32,
    /// Bytes queued for one client before it is disconnected as too slow.
    pub max_queued_bytes: usize,
    pub autoban: Autoban,
    pub password_hashing: PasswordHashing,
    pub history: History,
    pub kt: Kt,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            security: Security::Compatible,
            bind: vec![
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            ],
            port: 64738,
            state_directory: PathBuf::from("."),
            database: PathBuf::from("nuscheld.sqlite3"),
            database_url_file: None,
            certificate: None,
            private_key: None,
            storage_worker: None,
            registered_name: String::new(),
            welcome_text: String::new(),
            welcome_text_file: None,
            password: String::new(),
            password_file: None,
            superuser_password_file: None,
            max_users: 1000,
            max_users_per_channel: 0,
            max_bandwidth: 558_000,
            timeout: 30,
            handshake_timeout: 10,
            text_message_length: 5000,
            image_message_length: 1_048_576,
            allow_html: true,
            default_channel: 0,
            remember_channel: true,
            remember_channel_duration: 0,
            allow_ping: true,
            allow_ping_amplification: false,
            certificate_required: false,
            send_version: true,
            allow_recording: true,
            channel_nesting_limit: 10,
            channel_count_limit: 1000,
            username_pattern: r"[ -=\w\[\]\{\}\(\)\@\|\.]+".into(),
            channel_name_pattern: r"[ -=\w\#\[\]\{\}\(\)\@\|]+".into(),
            message_limit: 1,
            message_burst: 5,
            plugin_message_limit: 4,
            plugin_message_burst: 15,
            listeners_per_channel: None,
            listeners_per_user: None,
            broadcast_listener_volume_adjustments: false,
            suggest_version: None,
            suggest_positional: None,
            suggest_push_to_talk: None,
            connections_per_address: 16,
            max_pending_connections: 256,
            max_queued_bytes: 8 * 1024 * 1024,
            autoban: Autoban::default(),
            password_hashing: PasswordHashing::default(),
            history: History::default(),
            kt: Kt::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid {name}: {source}")]
    Pattern {
        name: &'static str,
        source: regex::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// What the configuration resolves to at startup: compiled patterns and the
/// contents of referenced files.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: Config,
    pub username: Regex,
    pub channel_name: Regex,
    pub welcome_text: String,
    pub password: String,
    pub suggest_version: Option<u64>,
    pub minimum_client: Option<u64>,
    pub recommended_client: Option<u64>,
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })
}

/// "1.5.0" in the 1.5 version format.
pub fn parse_version(text: &str) -> Option<u64> {
    let mut parts = text.trim().split('.').map(|part| part.parse::<u16>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    parts
        .next()
        .is_none()
        .then_some((u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16))
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::from_toml(&read(path)?)
    }

    pub fn database_path(&self) -> PathBuf {
        self.state_directory.join(&self.database)
    }

    /// Validate and resolve. Fails rather than starting with surprising behaviour.
    pub fn settings(self) -> Result<Settings, ConfigError> {
        if self.security == Security::E2ee {
            return Err(ConfigError::Invalid(
                "security = \"e2ee\" is not implemented yet; this server would have no \
                 end-to-end encryption. Use security = \"compatible\" until it is."
                    .into(),
            ));
        }
        if self.bind.is_empty() {
            return Err(ConfigError::Invalid(
                "bind needs at least one address".into(),
            ));
        }
        if self.certificate.is_some() != self.private_key.is_some() {
            return Err(ConfigError::Invalid(
                "certificate and private_key must be set together".into(),
            ));
        }
        if self.timeout == 0 || self.handshake_timeout == 0 {
            return Err(ConfigError::Invalid("timeouts must be nonzero".into()));
        }
        if self.max_users == 0 || self.connections_per_address == 0 {
            return Err(ConfigError::Invalid(
                "user and connection limits must be nonzero".into(),
            ));
        }
        let kt = &self.kt;
        if kt.publish_interval == 0 || kt.refresh_interval < kt.publish_interval {
            return Err(ConfigError::Invalid(
                "kt.publish_interval must be nonzero and at most kt.refresh_interval".into(),
            ));
        }
        for (name, path) in [
            ("kt.signing_key_file", &kt.signing_key_file),
            ("kt.vrf_secret_file", &kt.vrf_secret_file),
        ] {
            if path
                .as_ref()
                .is_some_and(|path| path.starts_with("/nix/store"))
            {
                return Err(ConfigError::Invalid(format!(
                    "{name} is in the Nix store, which is world-readable; pass it as a credential"
                )));
            }
        }
        if self.max_queued_bytes < 64 * 1024 {
            return Err(ConfigError::Invalid(
                "max_queued_bytes must be at least 64 KiB".into(),
            ));
        }
        let anchored = |name, pattern: &str| {
            Regex::new(&format!("^(?:{pattern})$"))
                .map_err(|source| ConfigError::Pattern { name, source })
        };
        let username = anchored("username_pattern", &self.username_pattern)?;
        let channel_name = anchored("channel_name_pattern", &self.channel_name_pattern)?;
        let welcome_text = match (&self.welcome_text_file, self.welcome_text.is_empty()) {
            (Some(path), true) => read(path)?,
            _ => self.welcome_text.clone(),
        };
        let password = match &self.password_file {
            Some(path) => read(path)?.trim_end_matches(['\r', '\n']).to_owned(),
            None => self.password.clone(),
        };
        let version = |name: &str, text: &Option<String>| match text {
            Some(text) => parse_version(text).map(Some).ok_or_else(|| {
                ConfigError::Invalid(format!("{name} {text:?} is not a version like 1.5.0"))
            }),
            None => Ok(None),
        };
        let suggest_version = version("suggest_version", &self.suggest_version)?;
        let minimum_client = version("history.minimum_client", &self.history.minimum_client)?;
        let recommended_client = version(
            "history.recommended_client",
            &self.history.recommended_client,
        )?;
        Ok(Settings {
            config: self,
            username,
            channel_name,
            welcome_text,
            password,
            suggest_version,
            minimum_client,
            recommended_client,
        })
    }
}

/// The example shipped as `nuscheld.example.toml`, also used by tests.
pub const EXAMPLE: &str = include_str!("../nuscheld.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_with_murmurs_defaults() {
        let settings = Config::from_toml(EXAMPLE).unwrap().settings().unwrap();
        assert_eq!(settings.config.security, Security::Compatible);
        assert_eq!(settings.config.port, 64738);
        assert!(settings.username.is_match("Alice (away) [x]"));
        assert!(settings.username.is_match("Ünïcødé_name"));
        // Murmur's class `[ -=...]` is a range from space to '=', so '/' is allowed.
        assert!(settings.username.is_match("a/b"));
        assert!(!settings.username.is_match("bad>name"));
        assert!(!settings.username.is_match("tab\tname"));
        assert!(!settings.username.is_match(""));
        assert!(settings.channel_name.is_match("Lobby #1"));
        assert!(!settings.channel_name.is_match("a\nb"));
    }

    #[test]
    fn unknown_keys_and_e2ee_are_refused() {
        assert!(Config::from_toml("prot = 1").is_err());
        let error = Config::from_toml("security = \"e2ee\"")
            .unwrap()
            .settings()
            .unwrap_err();
        assert!(error.to_string().contains("not implemented"));
        assert!(
            Config::from_toml("timeout = 0")
                .unwrap()
                .settings()
                .is_err()
        );
        assert!(
            Config::from_toml("username_pattern = \"(\"")
                .unwrap()
                .settings()
                .is_err()
        );
    }

    #[test]
    fn key_transparency_settings_are_checked() {
        let kt = Config::default().kt;
        assert!(kt.enabled);
        assert_eq!((kt.publish_interval, kt.refresh_interval), (10, 3600));
        assert!(
            Config::from_toml("[kt]\npublish_interval = 0")
                .unwrap()
                .settings()
                .is_err()
        );
        let error = Config::from_toml("[kt]\nvrf_secret_file = \"/nix/store/abc-kt/vrf\"")
            .unwrap()
            .settings()
            .unwrap_err();
        assert!(error.to_string().contains("credential"));
        let members = Config::from_toml("[kt]\nhistory = \"members\"").unwrap();
        assert_eq!(members.kt.history, KtHistoryAccess::Members);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.5.0"), Some(0x0001_0005_0000_0000));
        assert_eq!(parse_version("1.4"), Some(0x0001_0004_0000_0000));
        assert_eq!(parse_version("1.x"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }
}
