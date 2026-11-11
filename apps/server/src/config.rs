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

/// The oldest TLS version the server accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum TlsVersion {
    #[default]
    #[serde(rename = "1.2")]
    Tls12,
    #[serde(rename = "1.3")]
    Tls13,
}

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

/// `history.retention`: `"forever"`, `"none"` or a number of days.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum RetentionSetting {
    Days(u32),
    Word(RetentionWord),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionWord {
    Forever,
    /// Not archived: live delivery only.
    None,
}

/// Persistent history for Nuschel clients (docs/extension.md §4–§9).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct History {
    pub guests: GuestHistory,
    /// How long conversations keep messages unless a channel sets its own:
    /// `"forever"` (the default, user decision), `"none"` (not archived) or
    /// days. See [`History::default_retention`].
    pub retention: Option<RetentionSetting>,
    /// The older spelling: days, 0 keeps messages forever.
    pub retention_days: u32,
    /// The most any conversation may keep, in days, whatever its channel says
    /// (0: no maximum).
    pub max_retention_days: u32,
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
    /// History pages per minute, and their bytes, per network (an IPv4 /24 or
    /// IPv6 /56) for unregistered users and per account for registered ones
    /// (page_limits.rs). Exceeding them fails the single request with a
    /// `retry_after_ms`.
    pub network_pages_per_minute: u32,
    pub network_page_bytes_per_minute: u32,
    /// Networks (CIDR, such as `10.0.0.0/8`) exempt from the per-network limits.
    pub unlimited_networks: Vec<String>,
    /// Nuschel client versions ("1.0.0"; docs/extension.md, "Minimum
    /// versions"). Below `recommended_client` clients suggest an update; below
    /// `minimum_client` they are refused, from `minimum_client_from` on if it
    /// is set (until then admitted and shown the date). Self-reported and
    /// advisory; stock Mumble clients are never affected.
    pub minimum_client: Option<String>,
    pub recommended_client: Option<String>,
    /// A date (`2026-12-01`, quoted or not; midnight UTC).
    pub minimum_client_from: Option<DateSetting>,
}

/// A calendar date in the configuration: a TOML local date or a string.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DateSetting {
    Text(String),
    Toml(toml::value::Datetime),
}

impl DateSetting {
    /// Midnight UTC of the date, in Unix seconds.
    pub fn unix(&self) -> Option<i64> {
        let text = match self {
            Self::Text(text) => text.clone(),
            Self::Toml(datetime) => datetime.to_string(),
        };
        let mut parts = text.trim().splitn(3, '-');
        let mut part = |digits: usize| {
            parts
                .next()
                .filter(|part| part.len() == digits && part.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|part| part.parse::<i64>().ok())
        };
        let (year, month, day) = (part(4)?, part(2)?, part(2)?);
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days_in_month = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month[month as usize - 1] {
            return None;
        }
        // Days from the civil date (Howard Hinnant's algorithm).
        let (y, m) = if month <= 2 {
            (year - 1, month + 9)
        } else {
            (year, month - 3)
        };
        let era = y.div_euclid(400);
        let year_of_era = y - era * 400;
        let day_of_year = (153 * m + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        Some((era * 146_097 + day_of_era - 719_468) * 86_400)
    }
}

impl Default for History {
    fn default() -> Self {
        Self {
            guests: GuestHistory::None,
            retention: None,
            retention_days: 0,
            max_retention_days: 0,
            edit_limit: 20,
            moderators_delete: true,
            archive_stock_direct_messages: false,
            notify_stock_clients: true,
            pages_per_minute: 120,
            page_bytes_per_minute: 16 * 1024 * 1024,
            network_pages_per_minute: 600,
            network_page_bytes_per_minute: 64 * 1024 * 1024,
            unlimited_networks: Vec::new(),
            minimum_client: None,
            recommended_client: None,
            minimum_client_from: None,
        }
    }
}

impl History {
    /// The server's default retention (docs/extension.md §6), within
    /// `max_retention_days`.
    pub fn default_retention(&self) -> nuschel_ipc::server::Retention {
        use nuschel_ipc::server::Retention;
        let days = |days: u32| match days {
            0 => Retention::Forever,
            days => Retention::Days(days.min(nuschel_ipc::server::MAX_RETENTION_DAYS)),
        };
        let configured = match self.retention {
            Some(RetentionSetting::Word(RetentionWord::Forever)) => Retention::Forever,
            Some(RetentionSetting::Word(RetentionWord::None)) => Retention::None,
            Some(RetentionSetting::Days(value)) => days(value),
            None => days(self.retention_days),
        };
        configured.min(self.maximum_retention())
    }

    /// `max_retention_days` as a retention (forever when unset).
    pub fn maximum_retention(&self) -> nuschel_ipc::server::Retention {
        match self.max_retention_days {
            0 => nuschel_ipc::server::Retention::Forever,
            days => nuschel_ipc::server::Retention::Days(
                days.min(nuschel_ipc::server::MAX_RETENTION_DAYS),
            ),
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
    /// The names clients reach this server under, `host:port` as in their
    /// bookmarks (`chat.example.org:64738`): each is an origin of the epoch log,
    /// `<host>:<port>/nuschel-kt/v1`, whose checkpoints the witnesses cosign
    /// (docs/witness.md). Without one, nothing is submitted and replies carry no
    /// checkpoint; clients that require witnesses then show "Automatic
    /// verification unavailable".
    pub origins: Vec<String>,
    /// Submit to the built-in witnesses (`nuschel_kt::DEFAULT_WITNESSES`, empty
    /// until the project auditor is deployed).
    pub default_witnesses: bool,
    /// More witnesses to submit to.
    pub witnesses: Vec<KtWitness>,
    /// Seconds between submissions while new epochs wait for cosignatures.
    pub witness_interval: u64,
    /// Seconds after which an unchanged checkpoint is submitted again, so that
    /// its cosignatures stay fresh (clients accept them for a day by default).
    pub witness_refresh_interval: u64,
}

/// A witness `nuscheld` submits its epochs to (`[[kt.witnesses]]`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KtWitness {
    /// Its C2SP verifier key (`nuschel-witness vkey`).
    pub key: String,
    /// Its base URL; submissions go to `<url>/nuschel-kt/v1/add-epochs`.
    pub url: String,
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
            origins: Vec::new(),
            default_witnesses: true,
            witnesses: Vec::new(),
            witness_interval: 60,
            witness_refresh_interval: 3600,
        }
    }
}

/// The MLS delivery service (`mls/1`, docs/e2ee.md §6.3). Offered only with key
/// transparency, which credentials are checked against.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mls {
    pub enabled: bool,
    /// Key-package claims per account and minute.
    pub claims_per_minute: u32,
    /// Key-package uploads per account and minute.
    pub uploads_per_minute: u32,
    /// Member devices per group (docs/e2ee.md §6.7, open question 6).
    pub max_devices: u32,
    /// Welcomes older than this are dropped from the mailbox (the catch-up window,
    /// docs/e2ee.md §6.3).
    pub welcome_days: u32,
}

impl Default for Mls {
    fn default() -> Self {
        Self {
            enabled: true,
            claims_per_minute: 30,
            uploads_per_minute: 10,
            max_devices: nuschel_e2ee::MAX_GROUP_DEVICES as u32,
            welcome_days: nuschel_e2ee::INACTIVE_DEVICE_DAYS as u32,
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
    /// Temporary channels one user (account or certificate) may have at once;
    /// 0 is unlimited. Past it, creating another is refused with a reason.
    pub temporary_channels_per_user: u32,
    /// Voice datagrams per second all whisper and shout targets together may
    /// fan out to (a second's worth may come at once); 0 is unlimited. Past
    /// it, a whispered packet is dropped for all its receivers and the
    /// speaker is told, at most every ten seconds. Channel voice is not counted.
    pub whisper_fanout_per_second: u64,
    /// Seconds between the statistics lines in the log (connections, logins,
    /// messages, voice in and out); 0 turns them off.
    pub statistics_interval: u64,
    /// "1.2" (the default; Mumble clients built against OpenSSL before
    /// 1.1.1 speak nothing newer) or "1.3", which keeps client certificates
    /// off the wire in the clear.
    pub minimum_tls_version: TlsVersion,
    /// Certificate authorities whose client certificates are "strong"
    /// (Murmur's `bVerified`: the ACL group `strong`, `UserStats`): `"webpki"`
    /// (Mozilla's roots, built in), `"system"` (the system's bundle) or PEM
    /// files (murmur.ini's `sslCA`). Empty, the default: nobody is strong.
    pub strong_certificate_authorities: Vec<String>,
    /// Logs show a keyed hash of each client address instead of the address
    /// (murmur.ini `obfuscate`); the key is random per start.
    pub obfuscate: bool,
    pub autoban: Autoban,
    pub password_hashing: PasswordHashing,
    pub history: History,
    pub kt: Kt,
    pub mls: Mls,
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
            temporary_channels_per_user: 16,
            whisper_fanout_per_second: 500_000,
            statistics_interval: 300,
            minimum_tls_version: TlsVersion::Tls12,
            strong_certificate_authorities: Vec::new(),
            obfuscate: false,
            autoban: Autoban::default(),
            password_hashing: PasswordHashing::default(),
            history: History::default(),
            kt: Kt::default(),
            mls: Mls::default(),
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
    /// When clients below `minimum_client` start to be refused (Unix
    /// seconds); `None` with a minimum: at once.
    pub minimum_client_from: Option<i64>,
    /// How client addresses appear in logs (`obfuscate`).
    pub addresses: crate::obfuscate::Addresses,
    /// What makes a client certificate strong (`strong_certificate_authorities`).
    pub authorities: crate::tls::Authorities,
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
        if kt.witness_interval == 0 || kt.witness_refresh_interval < kt.witness_interval {
            return Err(ConfigError::Invalid(
                "kt.witness_interval must be nonzero and at most kt.witness_refresh_interval"
                    .into(),
            ));
        }
        for origin in &kt.origins {
            let valid = origin
                .rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
                && *origin == origin.to_ascii_lowercase()
                && nuschel_kt::witness::origin(origin).is_some();
            if !valid {
                return Err(ConfigError::Invalid(format!(
                    "kt.origins: {origin:?} is not a lower-case host:port"
                )));
            }
        }
        for witness in &kt.witnesses {
            if let Err(error) = nuschel_kt::witness::note::CosignerKey::from_vkey(&witness.key) {
                return Err(ConfigError::Invalid(format!(
                    "kt.witnesses: the key {:?} is not an ML-DSA-44 verifier key: {error}",
                    witness.key.chars().take(40).collect::<String>()
                )));
            }
            if !(witness.url.starts_with("https://") || witness.url.starts_with("http://")) {
                return Err(ConfigError::Invalid(format!(
                    "kt.witnesses: {:?} is not an http(s) URL",
                    witness.url
                )));
            }
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
        let history = &self.history;
        if history.retention.is_some() && history.retention_days != 0 {
            return Err(ConfigError::Invalid(
                "set history.retention or history.retention_days, not both".into(),
            ));
        }
        if history.retention == Some(RetentionSetting::Days(0)) {
            return Err(ConfigError::Invalid(
                "history.retention = 0 is ambiguous; use \"forever\" or \"none\"".into(),
            ));
        }
        if let Some(network) = history
            .unlimited_networks
            .iter()
            .find(|network| crate::page_limits::parse_cidr(network).is_none())
        {
            return Err(ConfigError::Invalid(format!(
                "history.unlimited_networks: {network:?} is not a network like 10.0.0.0/8"
            )));
        }
        let mls = &self.mls;
        if mls.claims_per_minute == 0
            || mls.uploads_per_minute == 0
            || mls.welcome_days == 0
            || mls.max_devices == 0
            || mls.max_devices as usize > nuschel_e2ee::MAX_GROUP_DEVICES
        {
            return Err(ConfigError::Invalid(format!(
                "mls limits must be nonzero, and mls.max_devices at most {}",
                nuschel_e2ee::MAX_GROUP_DEVICES
            )));
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
        let minimum_client_from = match &self.history.minimum_client_from {
            Some(date) => Some(date.unix().ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "history.minimum_client_from {date:?} is not a date like 2026-12-01"
                ))
            })?),
            None => None,
        };
        if minimum_client_from.is_some() && minimum_client.is_none() {
            return Err(ConfigError::Invalid(
                "history.minimum_client_from needs history.minimum_client".into(),
            ));
        }
        if let (Some(minimum), Some(recommended)) = (minimum_client, recommended_client)
            && recommended < minimum
        {
            return Err(ConfigError::Invalid(
                "history.recommended_client is below history.minimum_client".into(),
            ));
        }
        let obfuscate = self.obfuscate;
        let authorities = crate::tls::Authorities::load(&self.strong_certificate_authorities)
            .map_err(|error| {
                ConfigError::Invalid(format!("strong_certificate_authorities: {error}"))
            })?;
        Ok(Settings {
            config: self,
            username,
            channel_name,
            welcome_text,
            password,
            suggest_version,
            minimum_client,
            recommended_client,
            minimum_client_from,
            addresses: crate::obfuscate::Addresses::new(obfuscate),
            authorities,
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
        let mls = Config::default().mls;
        assert!(mls.enabled);
        assert_eq!((mls.max_devices, mls.welcome_days), (1000, 90));
        for bad in ["[mls]\nmax_devices = 1001", "[mls]\nclaims_per_minute = 0"] {
            assert!(Config::from_toml(bad).unwrap().settings().is_err(), "{bad}");
        }
        assert!(
            !Config::from_toml("[mls]\nenabled = false")
                .unwrap()
                .mls
                .enabled
        );
        let members = Config::from_toml("[kt]\nhistory = \"members\"").unwrap();
        assert_eq!(members.kt.history, KtHistoryAccess::Members);
    }

    #[test]
    fn retention_is_forever_by_default_and_explicit_otherwise() {
        use nuschel_ipc::server::Retention;
        let retention = |text: &str| {
            Config::from_toml(text)
                .map_err(|error| error.to_string())
                .and_then(|config| config.settings().map_err(|error| error.to_string()))
                .map(|settings| settings.config.history.default_retention())
        };
        assert_eq!(retention(""), Ok(Retention::Forever));
        assert_eq!(
            retention("[history]\nretention = \"forever\""),
            Ok(Retention::Forever)
        );
        assert_eq!(
            retention("[history]\nretention = \"none\""),
            Ok(Retention::None)
        );
        assert_eq!(
            retention("[history]\nretention = 30"),
            Ok(Retention::Days(30))
        );
        assert_eq!(
            retention("[history]\nretention_days = 7"),
            Ok(Retention::Days(7))
        );
        // The maximum caps the default.
        assert_eq!(
            retention("[history]\nmax_retention_days = 90"),
            Ok(Retention::Days(90))
        );
        assert_eq!(
            retention("[history]\nretention = \"none\"\nmax_retention_days = 90"),
            Ok(Retention::None)
        );
        assert!(retention("[history]\nretention = 0").is_err());
        assert!(retention("[history]\nretention = \"never\"").is_err());
        assert!(retention("[history]\nretention = 3\nretention_days = 3").is_err());
    }

    #[test]
    fn minimum_client_dates() {
        let settings = |extra: &str| {
            Config::from_toml(&format!("[history]\n{extra}"))
                .unwrap()
                .settings()
        };
        let from = |extra: &str| settings(extra).unwrap().minimum_client_from;
        assert_eq!(
            from("minimum_client = \"0.3\"\nminimum_client_from = \"1970-01-02\""),
            Some(86_400)
        );
        // A TOML date works unquoted; 2026-12-01 is day 20,788.
        assert_eq!(
            from("minimum_client = \"0.3\"\nminimum_client_from = 2026-12-01"),
            Some(20_788 * 86_400)
        );
        assert_eq!(
            from("minimum_client = \"0.3\"\nminimum_client_from = \"2000-02-29\""),
            Some(951_782_400)
        );
        assert_eq!(from("minimum_client = \"0.3\""), None, "in force at once");
        for bad in [
            "minimum_client = \"0.3\"\nminimum_client_from = \"2026-02-29\"",
            "minimum_client = \"0.3\"\nminimum_client_from = \"2026-12-01T10:00:00Z\"",
            "minimum_client = \"0.3\"\nminimum_client_from = 2026-12-01T10:00:00Z",
            "minimum_client_from = \"2026-12-01\"",
            "minimum_client = \"0.3\"\nrecommended_client = \"0.2\"",
        ] {
            assert!(settings(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.5.0"), Some(0x0001_0005_0000_0000));
        assert_eq!(parse_version("1.4"), Some(0x0001_0004_0000_0000));
        assert_eq!(parse_version("1.x"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }
}
