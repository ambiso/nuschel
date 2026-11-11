// SPDX-License-Identifier: EUPL-1.2
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachedChannel {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub position: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerBookmark {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub certificate_sha256: Option<String>,
    #[serde(default)]
    pub cached_channels: Vec<CachedChannel>,
    /// Access tokens grant ACL group membership; sent with every login.
    #[serde(default)]
    pub tokens: Vec<String>,
    /// Connect when Nuschel starts, self-muted and deafened.
    #[serde(default = "connect_on_startup")]
    pub auto_connect: bool,
    /// The server website's icon (its favicon), decoded and shrunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ServerIcon>,
    /// When the icon was last looked for (milliseconds since the epoch).
    #[serde(default)]
    pub icon_checked_ms: i64,
    /// The channel you were last in; rejoined when Nuschel connects again.
    #[serde(default)]
    pub last_channel: Option<u32>,
}

/// A small RGBA image (base64), as decoded by the sandboxed content worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIcon {
    pub width: u32,
    pub height: u32,
    pub rgba: String,
}

fn connect_on_startup() -> bool {
    true
}

impl ServerBookmark {
    pub fn new(name: &str, host: &str, port: u16, username: &str) -> Result<Self, String> {
        let host = host.trim().trim_start_matches('[').trim_end_matches(']');
        if host.is_empty()
            || host.len() > 253
            || host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "/@\\".contains(c))
            || port == 0
        {
            return Err("Enter a hostname or IP address and a port from 1 to 65535.".into());
        }
        if username.trim().is_empty()
            || username.len() > 128
            || username.chars().any(char::is_control)
        {
            return Err("Enter a username of at most 128 bytes without control characters.".into());
        }
        if name.len() > 128 || name.chars().any(char::is_control) {
            return Err(
                "The server name must be at most 128 bytes without control characters.".into(),
            );
        }
        Ok(Self {
            id: Uuid::new_v4().to_string(),
            name: if name.trim().is_empty() {
                host.into()
            } else {
                name.trim().into()
            },
            host: host.to_ascii_lowercase(),
            port,
            username: username.trim().into(),
            certificate_sha256: None,
            cached_channels: Vec::new(),
            tokens: Vec::new(),
            auto_connect: true,
            icon: None,
            icon_checked_ms: 0,
            last_channel: None,
        })
    }

    /// Certificate-bound history identity. A different certificate cannot inherit
    /// another server's local history just by reusing a display name or endpoint.
    pub fn history_identity(&self, peer_sha256: &str) -> String {
        fingerprint(format!("{}:{}:{}", self.host, self.port, peer_sha256).as_bytes())
    }
}

/// A `mumble://` link as produced by Mumble's "Copy URL" and web server lists.
#[derive(Clone, PartialEq, Eq)]
pub struct MumbleUrl {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    /// Kept only in memory for the connection attempt; never saved.
    pub password: Option<String>,
    /// Channel names from the root, e.g. `/Lobby/Games` → `["Lobby", "Games"]`.
    pub channel: Vec<String>,
}
impl std::fmt::Debug for MumbleUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MumbleUrl")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("channel", &self.channel)
            .finish()
    }
}
impl MumbleUrl {
    pub fn parse(url: &str) -> Result<Self, String> {
        let invalid =
            || "expected mumble://[user[:password]@]host[:port][/channel/path]".to_string();
        if url.len() > 2048 {
            return Err(invalid());
        }
        let rest = url
            .strip_prefix("mumble://")
            .or_else(|| url.strip_prefix("MUMBLE://"))
            .ok_or_else(invalid)?;
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, endpoint) = match authority.rsplit_once('@') {
            Some((user, endpoint)) => (Some(user), endpoint),
            None => (None, authority),
        };
        let (username, password) = match userinfo {
            Some(info) => {
                let (user, password) = info.split_once(':').unwrap_or((info, ""));
                (
                    Some(percent_decode(user)?).filter(|user| !user.is_empty()),
                    Some(percent_decode(password)?).filter(|password| !password.is_empty()),
                )
            }
            None => (None, None),
        };
        let (host, port) = if let Some(bracketed) = endpoint.strip_prefix('[') {
            let (host, tail) = bracketed.split_once(']').ok_or_else(invalid)?;
            (host, tail.strip_prefix(':'))
        } else {
            match endpoint.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (endpoint, None),
            }
        };
        let port = match port {
            Some(port) => port.parse::<u16>().map_err(|_| invalid())?,
            None => 64738,
        };
        let host = percent_decode(host)?;
        // Reuse bookmark validation for host and username rules.
        ServerBookmark::new("", &host, port, username.as_deref().unwrap_or("user"))?;
        let channel = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(percent_decode)
            .collect::<Result<Vec<_>, _>>()?;
        if channel.len() > 64 || channel.iter().any(|name| name.len() > 512) {
            return Err(invalid());
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            username,
            password,
            channel,
        })
    }
}

fn percent_decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or("invalid percent-encoding in mumble:// URL")?;
            out.push(hex);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(out).map_err(|_| "mumble:// URL is not valid UTF-8")?;
    if decoded.chars().any(char::is_control) {
        return Err("mumble:// URL contains control characters".into());
    }
    Ok(decoded)
}

/// What a keyboard shortcut does. Holds act while the key is down; toggles
/// act once per press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutAction {
    PushToTalk,
    ToggleMute,
    ToggleDeafen,
    PushToMute,
    /// Ctrl+K: the switcher (in Nuschel only).
    Find,
    /// Ctrl+F: search this conversation (in Nuschel only).
    Search,
    /// Ctrl+Shift+F: search every server (in Nuschel only).
    SearchAll,
}
impl ShortcutAction {
    pub const ALL: [Self; 7] = [
        Self::PushToTalk,
        Self::ToggleMute,
        Self::ToggleDeafen,
        Self::PushToMute,
        Self::Find,
        Self::Search,
        Self::SearchAll,
    ];
    /// Voice actions work system-wide; the others only while Nuschel is in
    /// front, and are never registered with the desktop (they would take the
    /// key from every other application).
    pub fn system_wide(self) -> bool {
        matches!(
            self,
            Self::PushToTalk | Self::ToggleMute | Self::ToggleDeafen | Self::PushToMute
        )
    }
    /// The stable name, as in the settings file and system shortcut registrations.
    pub fn id(self) -> &'static str {
        match self {
            Self::PushToTalk => "push-to-talk",
            Self::ToggleMute => "toggle-mute",
            Self::ToggleDeafen => "toggle-deafen",
            Self::PushToMute => "push-to-mute",
            Self::Find => "find",
            Self::Search => "search",
            Self::SearchAll => "search-all",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }
}

/// Local preferences. Per-user adjustments are keyed by certificate hash, as in
/// Mumble, so they follow a person rather than a reusable display name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
    pub audio: nuschel_core::audio::AudioSettings,
    /// Keep voice in the TLS tunnel (Mumble's "Force TCP mode").
    pub force_tcp: bool,
    /// Reconnect automatically after an unexpected disconnect.
    pub auto_reconnect: bool,
    pub user_volumes: std::collections::BTreeMap<String, f32>,
    pub local_mutes: std::collections::BTreeSet<String>,
    /// Keyboard shortcuts in GPUI keystroke notation, e.g. "f13" or "ctrl-shift-m".
    pub shortcuts: std::collections::BTreeMap<ShortcutAction, String>,
    /// The push-to-talk key of older versions, read once into `shortcuts`.
    #[serde(skip_serializing)]
    push_to_talk_key: Option<String>,
    /// Enter sends a message and Shift+Enter starts a new line; otherwise
    /// Enter starts a new line and Ctrl/Cmd+Enter sends.
    pub enter_sends: bool,
    /// Width of the channel list in logical pixels.
    pub sidebar_width: f32,
    /// Look for signed updates (Windows and macOS builds).
    pub check_updates: bool,
    /// Desktop notifications for direct messages, your voice channel and mentions.
    pub notifications: bool,
    /// Show each server's website icon, fetched over HTTPS from its address.
    pub server_icons: bool,
    /// Sound effects for your channel, mute and deafen, push-to-talk, messages
    /// and the connection.
    pub sound_effects: bool,
    /// Their volume, from 0 to 1.
    pub sound_volume: f32,
    /// Friends by certificate hash, with the name they had when added.
    pub friends: std::collections::BTreeMap<String, String>,
    /// Names shown instead of a user's own, by certificate hash; only for you.
    pub nicknames: std::collections::BTreeMap<String, String>,
    /// A desktop notification when a friend connects.
    pub friend_notifications: bool,
    /// magic-wormhole rendezvous server for offering files (`ws://`).
    pub wormhole_rendezvous: String,
    /// magic-wormhole transit relay (`tcp://host:port`).
    pub wormhole_relay: String,
    /// Theme, fonts, text size and zoom.
    pub appearance: AppearanceSettings,
}

/// Light or dark, or as the desktop prefers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

/// How Nuschel looks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceSettings {
    pub theme: ThemeChoice,
    /// The interface's font family; `None` is the system's.
    pub ui_font: Option<String>,
    /// The family for code; `None` is the system's monospace font.
    pub mono_font: Option<String>,
    /// Message text in logical pixels.
    pub text_size: f32,
    /// Everything larger or smaller, 1 being 100 % (Ctrl+= and Ctrl+-).
    pub zoom: f32,
}

pub const TEXT_SIZE: std::ops::RangeInclusive<f32> = 12.0..=24.0;
pub const ZOOM: std::ops::RangeInclusive<f32> = 0.5..=3.0;
impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            ui_font: None,
            mono_font: None,
            text_size: 15.,
            zoom: 1.,
        }
    }
}
impl AppearanceSettings {
    pub fn sanitized(mut self) -> Self {
        let family =
            |name: Option<String>| name.filter(|name| !name.trim().is_empty() && name.len() <= 256);
        self.ui_font = family(self.ui_font);
        self.mono_font = family(self.mono_font);
        let clamp = |value: f32, range: std::ops::RangeInclusive<f32>, default: f32| {
            if value.is_finite() {
                value.clamp(*range.start(), *range.end())
            } else {
                default
            }
        };
        self.text_size = clamp(self.text_size, TEXT_SIZE, 15.);
        self.zoom = clamp(self.zoom, ZOOM, 1.);
        self
    }
}

pub const SIDEBAR_WIDTH: std::ops::RangeInclusive<f32> = 200.0..=560.0;
impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            audio: Default::default(),
            force_tcp: false,
            auto_reconnect: true,
            user_volumes: Default::default(),
            local_mutes: Default::default(),
            shortcuts: Default::default(),
            push_to_talk_key: None,
            enter_sends: true,
            sidebar_width: 248.,
            check_updates: true,
            notifications: true,
            server_icons: true,
            sound_effects: true,
            sound_volume: 0.7,
            friends: Default::default(),
            nicknames: Default::default(),
            friend_notifications: false,
            wormhole_rendezvous: nuschel_core::transfer::PUBLIC_RENDEZVOUS.into(),
            wormhole_relay: nuschel_core::transfer::PUBLIC_RELAY.into(),
            appearance: AppearanceSettings::default(),
        }
    }
}
impl ClientSettings {
    pub fn sanitized(mut self) -> Self {
        self.audio = self.audio.sanitized();
        self.appearance = self.appearance.sanitized();
        self.user_volumes
            .retain(|hash, volume| hash.len() <= 128 && volume.is_finite());
        for volume in self.user_volumes.values_mut() {
            *volume = volume.clamp(0., 4.);
        }
        self.local_mutes.retain(|hash| hash.len() <= 128);
        while self.user_volumes.len() > 4096 {
            self.user_volumes.pop_first();
        }
        while self.local_mutes.len() > 4096 {
            self.local_mutes.pop_first();
        }
        for names in [&mut self.friends, &mut self.nicknames] {
            names.retain(|hash, name| hash.len() <= 128 && clean_name(name) == *name);
            while names.len() > MAX_PEOPLE {
                names.pop_first();
            }
        }
        if let Some(key) = self.push_to_talk_key.take() {
            self.shortcuts
                .entry(ShortcutAction::PushToTalk)
                .or_insert(key);
        }
        self.sound_volume = if self.sound_volume.is_finite() {
            self.sound_volume.clamp(0., 1.)
        } else {
            0.7
        };
        self.shortcuts
            .retain(|_, key| !key.is_empty() && key.len() <= 64);
        self.sidebar_width = if self.sidebar_width.is_finite() {
            self.sidebar_width
                .clamp(*SIDEBAR_WIDTH.start(), *SIDEBAR_WIDTH.end())
        } else {
            248.
        };
        if !nuschel_core::transfer::valid_url(&self.wormhole_rendezvous) {
            self.wormhole_rendezvous = nuschel_core::transfer::PUBLIC_RENDEZVOUS.into();
        }
        if !self.wormhole_relay.starts_with("tcp://") || self.wormhole_relay.len() > 256 {
            self.wormhole_relay = nuschel_core::transfer::PUBLIC_RELAY.into();
        }
        self
    }
    pub fn volume_for(&self, hash: Option<&str>) -> f32 {
        hash.and_then(|hash| self.user_volumes.get(hash))
            .copied()
            .unwrap_or(1.)
    }
    pub fn locally_muted(&self, hash: Option<&str>) -> bool {
        hash.is_some_and(|hash| self.local_mutes.contains(hash))
    }
    pub fn nickname(&self, hash: Option<&str>) -> Option<&str> {
        hash.and_then(|hash| self.nicknames.get(hash))
            .map(String::as_str)
    }
    /// The name a friend had when added.
    pub fn friend(&self, hash: Option<&str>) -> Option<&str> {
        hash.and_then(|hash| self.friends.get(hash))
            .map(String::as_str)
    }
    /// Befriend, remembering `name`; false when the friends list is full.
    pub fn add_friend(&mut self, hash: &str, name: &str) -> bool {
        insert_name(&mut self.friends, hash, name)
    }
    /// Set a local nickname; an empty one removes it. False when the list is full.
    pub fn set_nickname(&mut self, hash: &str, nickname: &str) -> bool {
        if clean_name(nickname).is_empty() {
            self.nicknames.remove(hash);
            return true;
        }
        insert_name(&mut self.nicknames, hash, nickname)
    }
}

/// Friends and nicknames kept, each; at most this many of the longest names
/// keep settings.json well within the 1 MiB it is read with.
pub const MAX_PEOPLE: usize = 1024;
/// Longest friend name or nickname, in characters.
pub const MAX_PERSON_NAME: usize = 64;

/// A name on one line, trimmed and bounded.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_PERSON_NAME)
        .collect::<String>()
        .trim()
        .to_string()
}

fn insert_name(
    names: &mut std::collections::BTreeMap<String, String>,
    hash: &str,
    name: &str,
) -> bool {
    let name = clean_name(name);
    if hash.is_empty() || hash.len() > 128 || name.is_empty() {
        return false;
    }
    if names.len() >= MAX_PEOPLE && !names.contains_key(hash) {
        return false;
    }
    names.insert(hash.to_string(), name);
    true
}

#[derive(Clone)]
pub struct Profile {
    pub config_dir: PathBuf,
    /// The local history folder, the storage worker's only writable directory.
    /// It never holds configuration, the identity or other credentials.
    pub data_dir: PathBuf,
}

const HISTORY_FILE: &str = "history.sqlite3";
const MAX_SERVERS: usize = 100;
const MAX_SERVERS_FILE: usize = 8 * 1024 * 1024;
const MAX_CACHED_CHANNELS: usize = 4096;
const MAX_CHANNEL_NAME: usize = 512;

impl Profile {
    pub fn open(override_dir: Option<PathBuf>) -> io::Result<Self> {
        if let Some(root) = override_dir {
            return Self::at(root.join("config"), root.join("data"));
        }
        let paths = directories::ProjectDirs::from("org", "Nuschel", "Nuschel")
            .ok_or_else(|| io::Error::other("could not find the application data directory"))?;
        Self::at(paths.config_dir().into(), paths.data_local_dir().into())
    }

    /// macOS keeps configuration and data in one folder (Application Support),
    /// and XDG variables can nest them elsewhere. The storage worker may write
    /// anywhere in its directory, so history then gets a subfolder of its own.
    fn at(config_dir: PathBuf, data_dir: PathBuf) -> io::Result<Self> {
        let profile = Self {
            data_dir: if config_dir.starts_with(&data_dir) {
                data_dir.join("history")
            } else {
                data_dir.clone()
            },
            config_dir,
        };
        for directory in [&profile.config_dir, &profile.data_dir] {
            fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
        }
        migrate_history(&data_dir, &profile.data_dir)?;
        Ok(profile)
    }

    pub fn load_servers(&self) -> io::Result<Vec<ServerBookmark>> {
        let path = self.config_dir.join("servers.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut servers: Vec<ServerBookmark> =
            serde_json::from_slice(&read_bounded(&path, MAX_SERVERS_FILE as u64)?)?;
        if servers.len() > MAX_SERVERS {
            return Err(io::Error::other("too many saved servers"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for server in &mut servers {
            ServerBookmark::new(&server.name, &server.host, server.port, &server.username)
                .map_err(io::Error::other)?;
            if server.tokens.len() > 64 || server.tokens.iter().any(|token| token.len() > 256) {
                return Err(io::Error::other("too many or oversized access tokens"));
            }
            // Only a cache, written by earlier versions without limits: drop it
            // rather than refuse every bookmark.
            if server.cached_channels.len() > MAX_CACHED_CHANNELS
                || server
                    .cached_channels
                    .iter()
                    .any(|channel| channel.name.len() > MAX_CHANNEL_NAME)
            {
                server.cached_channels.clear();
            }
            if Uuid::parse_str(&server.id).is_err()
                || !ids.insert(&server.id)
                || server.certificate_sha256.as_ref().is_some_and(|pin| {
                    pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit())
                })
            {
                return Err(io::Error::other(
                    "invalid saved server identity or certificate fingerprint",
                ));
            }
        }
        Ok(servers)
    }

    /// The cached channel trees are the only part a server controls, and JSON
    /// escapes a control character to six bytes, so 4,096 such names overflow
    /// the file. The caches are only an offline convenience: they are trimmed
    /// to what loading accepts, and the largest are dropped until the file
    /// fits, so a server can never stop bookmarks or certificate pins from
    /// being saved.
    pub fn save_servers(&self, servers: &[ServerBookmark]) -> io::Result<()> {
        if servers.len() > MAX_SERVERS {
            return Err(io::Error::other("at most 100 servers may be saved"));
        }
        let mut servers = servers.to_vec();
        for server in &mut servers {
            let cache = &mut server.cached_channels;
            cache.retain(|channel| channel.name.len() <= MAX_CHANNEL_NAME);
            cache.truncate(MAX_CACHED_CHANNELS);
        }
        loop {
            let bytes = serde_json::to_vec_pretty(&servers)?;
            if bytes.len() <= MAX_SERVERS_FILE {
                return atomic_private_write(&self.config_dir.join("servers.json"), &bytes);
            }
            let size =
                |cache: &Vec<CachedChannel>| serde_json::to_vec(cache).map_or(0, |b| b.len());
            let largest = servers
                .iter_mut()
                .map(|server| &mut server.cached_channels)
                .filter(|cache| !cache.is_empty())
                .max_by_key(|cache| size(cache))
                .ok_or_else(|| io::Error::other("server metadata exceeds 8 MiB"))?;
            largest.clear();
        }
    }

    pub fn load_settings(&self) -> io::Result<ClientSettings> {
        let path = self.config_dir.join("settings.json");
        if !path.exists() {
            return Ok(ClientSettings::default());
        }
        let settings: ClientSettings = serde_json::from_slice(&read_bounded(&path, 1024 * 1024)?)?;
        Ok(settings.sanitized())
    }

    pub fn save_settings(&self, settings: &ClientSettings) -> io::Result<()> {
        atomic_private_write(
            &self.config_dir.join("settings.json"),
            &serde_json::to_vec_pretty(settings)?,
        )
    }

    pub fn history_path(&self) -> PathBuf {
        self.data_dir.join(HISTORY_FILE)
    }

    pub fn identity(&self) -> Result<ClientIdentity, Box<dyn std::error::Error + Send + Sync>> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let identity_lock = options.open(self.config_dir.join("identity.lock"))?;
        identity_lock.lock()?;
        let path = self.config_dir.join("identity.json");
        if path.exists() {
            return Ok(serde_json::from_slice(&read_bounded(&path, 64 * 1024)?)?);
        }
        let identity = ClientIdentity::generate()?;
        atomic_private_write(&path, &serde_json::to_vec(&identity)?)?;
        Ok(identity)
    }
}

/// PKCS#12 files from Mumble's certificate wizard are small; refuse anything large.
pub const MAX_PKCS12_BYTES: usize = 256 * 1024;

impl Profile {
    /// Replace the client identity with one exported by Mumble (or another client).
    /// The previous identity is kept as a timestamped backup.
    pub fn import_identity(&self, pkcs12: &[u8], password: &str) -> Result<ClientIdentity, String> {
        let identity = ClientIdentity::from_pkcs12(pkcs12, password)?;
        let current = self.config_dir.join("identity.json");
        if current.exists() {
            let backup = self.config_dir.join(format!(
                "identity.{}.json.bak",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0)
            ));
            fs::copy(&current, backup).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
        atomic_private_write(&current, &bytes).map_err(|error| error.to_string())?;
        Ok(identity)
    }

    /// Export the identity so Mumble and other clients can log in as the same user.
    pub fn export_identity(&self, password: &str) -> Result<Vec<u8>, String> {
        self.identity()
            .map_err(|error| error.to_string())?
            .to_pkcs12(password)
    }

    /// Write the exported identity to `path`, readable by the owner only. The
    /// password may be empty, so the file is as secret as the key. It is
    /// written beside the target and renamed over it: an existing file never
    /// keeps wider permissions, and a failed export leaves no partial key.
    pub fn export_identity_to(&self, path: &Path, password: &str) -> Result<(), String> {
        let bytes = self.export_identity(password)?;
        atomic_private_write(path, &bytes).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("certificate_sha256", &fingerprint(&self.certificate))
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    /// A self-signed RSA-2048 identity like Mumble's own. Stock Mumble imports
    /// only RSA keys, so this keeps identities portable between the clients.
    pub fn generate() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)?;
        let der = key.to_pkcs8_der()?;
        let signing_key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &rustls::pki_types::PrivatePkcs8KeyDer::from(der.as_bytes()),
            &rcgen::PKCS_RSA_SHA256,
        )?;
        let mut params = rcgen::CertificateParams::new(vec!["nuschel-client.invalid".to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Nuschel user");
        params.not_after = rcgen::date_time_ymd(2046, 1, 1);
        let cert = params.self_signed(&signing_key)?;
        Ok(Self {
            certificate: cert.der().to_vec(),
            private_key: der.as_bytes().to_vec(),
        })
    }

    /// Stock Mumble can import only RSA client keys.
    pub fn is_rsa(&self) -> bool {
        // PKCS#8 AlgorithmIdentifier for rsaEncryption (1.2.840.113549.1.1.1).
        const RSA_OID: &[u8] = &[
            0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
        ];
        self.private_key
            .windows(RSA_OID.len())
            .take(32)
            .any(|window| window == RSA_OID)
    }

    /// SHA-1 of the certificate, the hash Mumble servers show and register.
    pub fn mumble_hash(&self) -> String {
        use sha1::Digest as _;
        sha1::Sha1::digest(&self.certificate)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn from_pkcs12(pkcs12: &[u8], password: &str) -> Result<Self, String> {
        if pkcs12.len() > MAX_PKCS12_BYTES {
            return Err("The certificate file is too large.".into());
        }
        let store = p12_keystore::KeyStore::from_pkcs12(
            pkcs12,
            password,
            p12_keystore::Pkcs12ImportPolicy::Relaxed,
        )
        .map_err(|error| match error {
            p12_keystore::error::Error::MacError(_) | p12_keystore::error::Error::UnpadError => {
                "Wrong password for this certificate file.".to_string()
            }
            other => format!("Could not read the certificate file: {other}"),
        })?;
        let (_, chain) = store
            .private_key_chain()
            .ok_or("The file contains no private key with a certificate.")?;
        let certificate = chain
            .certs()
            .first()
            .ok_or("The file's private key has no certificate.")?
            .as_der()
            .to_vec();
        let identity = Self {
            certificate,
            private_key: chain.key().as_der().to_vec(),
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn to_pkcs12(&self, password: &str) -> Result<Vec<u8>, String> {
        if !self.is_rsa() {
            return Err(
                "This identity uses an ECDSA key, which stock Mumble cannot import. \
                 Identities created by current Nuschel versions use RSA."
                    .into(),
            );
        }
        let key = p12_keystore::PrivateKey::from_der(&self.private_key)
            .map_err(|error| error.to_string())?;
        let certificate = p12_keystore::Certificate::from_der(&self.certificate)
            .map_err(|error| error.to_string())?;
        let chain =
            p12_keystore::PrivateKeyChain::new(self.mumble_hash().into_bytes(), key, [certificate]);
        let mut store = p12_keystore::KeyStore::new();
        store.add_entry(
            "Nuschel",
            p12_keystore::KeyStoreEntry::PrivateKeyChain(chain),
        );
        store
            .writer(password)
            .write()
            .map_err(|error| error.to_string())
    }

    /// Rustls must accept the key for TLS client authentication.
    fn validate(&self) -> Result<(), String> {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                self.private_key.clone(),
            )))
            .map(|_| ())
            .map_err(|_| "This key type is not supported for Mumble logins.".into())
    }
}

pub fn fingerprint(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Move a database that earlier versions kept beside the configuration (macOS)
/// into the history folder. Sidecars move first and the database last, so an
/// interrupted move resumes on the next start. A failure is an error: the
/// worker would otherwise start an empty database and strand the old one.
fn migrate_history(old: &Path, new: &Path) -> io::Result<()> {
    if old == new || new.join(HISTORY_FILE).exists() || !old.join(HISTORY_FILE).exists() {
        return Ok(());
    }
    for suffix in ["-wal", "-shm", ""] {
        let name = format!("{HISTORY_FILE}{suffix}");
        match fs::rename(old.join(&name), new.join(&name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("configuration exceeds its size limit"));
    }
    Ok(bytes)
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_endpoints_and_binds_history_to_certificate() {
        assert!(ServerBookmark::new("", "mumble://example.org", 64738, "me").is_err());
        assert!(ServerBookmark::new("", "localhost", 0, "me").is_err());
        assert!(ServerBookmark::new("", "localhost", 64738, "").is_err());
        let server = ServerBookmark::new("", "[::1]", 64738, "me").unwrap();
        assert_eq!(server.host, "::1");
        assert_ne!(
            server.history_identity("one"),
            server.history_identity("two")
        );
    }
    #[test]
    fn new_identities_are_mumble_compatible_rsa() {
        let identity = ClientIdentity::generate().unwrap();
        assert!(identity.is_rsa());
        identity.validate().unwrap();
        let legacy = ClientIdentity {
            certificate: identity.certificate.clone(),
            private_key: rcgen::KeyPair::generate().unwrap().serialize_der(),
        };
        assert!(!legacy.is_rsa());
        assert!(legacy.to_pkcs12("").is_err());
    }
    #[test]
    fn identities_round_trip_through_pkcs12() {
        let root = std::env::temp_dir().join(format!("nuschel-identity-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        let original = profile.identity().unwrap();
        let exported = profile.export_identity("secret").unwrap();
        assert!(ClientIdentity::from_pkcs12(&exported, "wrong").is_err());
        let other = Profile::open(Some(root.join("other"))).unwrap();
        let imported = other.import_identity(&exported, "secret").unwrap();
        // A written export replaces an existing file with an owner-only one.
        let file = root.join("exported.p12");
        fs::write(&file, b"world-readable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            profile.export_identity_to(&file, "").unwrap();
            let mode = fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        #[cfg(not(unix))]
        profile.export_identity_to(&file, "").unwrap();
        let written = ClientIdentity::from_pkcs12(&fs::read(&file).unwrap(), "").unwrap();
        assert_eq!(written.certificate, original.certificate);
        assert_eq!(imported.certificate, original.certificate);
        assert_eq!(
            other.identity().unwrap().mumble_hash(),
            original.mumble_hash()
        );
        assert!(ClientIdentity::from_pkcs12(b"not a pkcs12 file", "").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn history_never_shares_the_configuration_folder() {
        let root = std::env::temp_dir().join(format!("nuschel-layout-{}", Uuid::new_v4()));
        // Linux and Windows: separate folders stay as they are.
        let separate = Profile::at(root.join("config"), root.join("data")).unwrap();
        assert_eq!(separate.data_dir, root.join("data"));
        // macOS: one folder for both. A database (and its WAL) that an earlier
        // version kept beside the configuration moves into the history folder.
        let shared = root.join("Application Support");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join(HISTORY_FILE), b"database").unwrap();
        fs::write(shared.join(format!("{HISTORY_FILE}-wal")), b"log").unwrap();
        let profile = Profile::at(shared.clone(), shared.clone()).unwrap();
        assert_eq!(profile.data_dir, shared.join("history"));
        assert_eq!(fs::read(profile.history_path()).unwrap(), b"database");
        assert_eq!(
            fs::read(profile.data_dir.join(format!("{HISTORY_FILE}-wal"))).unwrap(),
            b"log"
        );
        assert!(!shared.join(HISTORY_FILE).exists());
        // Configuration nested inside the data folder gets the same treatment.
        let nested = Profile::at(root.join("data/config"), root.join("data")).unwrap();
        assert_eq!(nested.data_dir, root.join("data/history"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_hostile_channel_cache_cannot_block_saving_bookmarks() {
        let root = std::env::temp_dir().join(format!("nuschel-servers-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        let mut hostile = ServerBookmark::new("", "hostile.invalid", 64738, "me").unwrap();
        // Each control character is escaped to six bytes ("\u0001") in JSON.
        hostile.cached_channels = (0..4096)
            .map(|id| CachedChannel {
                id,
                parent: Some(0),
                name: "\u{1}".repeat(512),
                position: 0,
            })
            .collect();
        let mut friendly = ServerBookmark::new("", "friendly.invalid", 64738, "me").unwrap();
        friendly.cached_channels = vec![CachedChannel {
            id: 0,
            parent: None,
            name: "Root\u{7}".into(),
            position: 0,
        }];
        friendly.certificate_sha256 = Some("ab".repeat(32));
        profile.save_servers(&[hostile, friendly]).unwrap();
        let loaded = profile.load_servers().unwrap();
        assert_eq!(loaded.len(), 2);
        // Only the hostile cache was dropped; the other bookmark and its pin survive.
        assert!(loaded[0].cached_channels.is_empty());
        assert_eq!(loaded[1].cached_channels[0].name, "Root\u{7}");
        assert_eq!(loaded[1].certificate_sha256, Some("ab".repeat(32)));
        // A cache beyond the limits from an earlier version no longer refuses
        // every bookmark.
        let mut old = loaded;
        old[1].cached_channels = vec![old[1].cached_channels[0].clone(); 5000];
        atomic_private_write(
            &profile.config_dir.join("servers.json"),
            &serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        assert!(
            profile.load_servers().unwrap()[1]
                .cached_channels
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn settings_round_trip_and_are_sanitized() {
        let root = std::env::temp_dir().join(format!("nuschel-settings-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        assert_eq!(profile.load_settings().unwrap(), ClientSettings::default());
        let mut settings = ClientSettings::default();
        settings.user_volumes.insert("abc".into(), 9.);
        settings.local_mutes.insert("def".into());
        settings.sound_volume = 3.;
        profile.save_settings(&settings).unwrap();
        let loaded = profile.load_settings().unwrap();
        assert_eq!(loaded.volume_for(Some("abc")), 4.);
        assert_eq!(loaded.sound_volume, 1.);
        assert!(loaded.locally_muted(Some("def")));
        assert_eq!(loaded.volume_for(None), 1.);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn friends_and_nicknames_are_bounded() {
        let mut settings = ClientSettings::default();
        assert!(settings.add_friend("aa", "Ada"));
        assert!(settings.set_nickname("aa", "  Countess\n of Lovelace "));
        assert_eq!(settings.friend(Some("aa")), Some("Ada"));
        assert_eq!(settings.nickname(Some("aa")), Some("Countess of Lovelace"));
        assert_eq!(settings.nickname(None), None);
        // Empty clears; no hash, an overlong one or a blank name is refused.
        assert!(settings.set_nickname("aa", " \t"));
        assert_eq!(settings.nickname(Some("aa")), None);
        assert!(!settings.add_friend("", "Ada"));
        assert!(!settings.add_friend(&"f".repeat(129), "Ada"));
        assert!(!settings.add_friend("bb", "\u{7}"));
        assert!(settings.set_nickname("bb", &"n".repeat(500)));
        assert_eq!(
            settings.nickname(Some("bb")).unwrap().len(),
            MAX_PERSON_NAME
        );
        // A full list refuses newcomers but still renames those on it.
        for n in 0..MAX_PEOPLE - 1 {
            assert!(settings.add_friend(&format!("{n:040x}"), "Friend"));
        }
        assert!(!settings.add_friend("cc", "One too many"));
        assert!(settings.add_friend("aa", "Ada Lovelace"));
        // A hand-edited file loses what the menu could not have written.
        settings.friends.insert("dd".into(), "Two\nlines".into());
        settings
            .nicknames
            .insert("x".repeat(200), "Long hash".into());
        settings.nicknames.insert("ee".into(), " padded ".into());
        let settings = settings.sanitized();
        assert_eq!(settings.friends.len(), MAX_PEOPLE);
        assert_eq!(settings.friend(Some("aa")), Some("Ada Lovelace"));
        assert_eq!(settings.friend(Some("dd")), None);
        assert_eq!(settings.nicknames.len(), 1);
        let many = ClientSettings {
            nicknames: (0..MAX_PEOPLE + 10)
                .map(|n| (format!("{n:040x}"), "Nick".to_string()))
                .collect(),
            ..Default::default()
        };
        assert_eq!(many.sanitized().nicknames.len(), MAX_PEOPLE);
    }
    #[test]
    fn settings_from_before_appearance_follow_the_system_at_100_percent() {
        let root = std::env::temp_dir().join(format!("nuschel-appearance-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        std::fs::write(
            profile.config_dir.join("settings.json"),
            r#"{"force_tcp": true, "sidebar_width": 300.0}"#,
        )
        .unwrap();
        let loaded = profile.load_settings().unwrap();
        assert!(loaded.force_tcp);
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        assert_eq!(loaded.appearance.theme, ThemeChoice::System);
        // Out-of-range or partial appearance values are repaired, not rejected.
        std::fs::write(
            profile.config_dir.join("settings.json"),
            r#"{"appearance": {"theme": "light", "zoom": 9.0, "text_size": 2.0, "ui_font": " "}}"#,
        )
        .unwrap();
        let loaded = profile.load_settings().unwrap().appearance;
        assert_eq!(loaded.theme, ThemeChoice::Light);
        assert_eq!((loaded.zoom, loaded.text_size), (3., 12.));
        assert_eq!(loaded.ui_font, None);
        let mut settings = ClientSettings::default();
        settings.appearance.mono_font = Some("Iosevka".into());
        settings.appearance.zoom = 1.25;
        profile.save_settings(&settings).unwrap();
        assert_eq!(profile.load_settings().unwrap(), settings);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shortcuts_load_from_older_settings() {
        let old: ClientSettings =
            serde_json::from_str(r#"{"push_to_talk_key": "f13", "enter_sends": false}"#).unwrap();
        let settings = old.sanitized();
        assert_eq!(
            settings
                .shortcuts
                .get(&ShortcutAction::PushToTalk)
                .map(String::as_str),
            Some("f13")
        );
        assert!(!settings.enter_sends);
        // The old field is not written back, and a saved binding wins over it.
        let saved = serde_json::to_string(&settings).unwrap();
        assert!(!saved.contains("push_to_talk_key"), "{saved}");
        assert!(
            saved.contains(r#""shortcuts":{"push-to-talk":"f13"}"#),
            "{saved}"
        );
        let both: ClientSettings = serde_json::from_str(
            r#"{"push_to_talk_key": "f13", "shortcuts": {"push-to-talk": "f14", "toggle-mute": "ctrl-shift-m", "push-to-mute": ""}}"#,
        )
        .unwrap();
        let both = both.sanitized();
        assert_eq!(
            both.shortcuts.into_iter().collect::<Vec<_>>(),
            [
                (ShortcutAction::PushToTalk, "f14".to_string()),
                (ShortcutAction::ToggleMute, "ctrl-shift-m".to_string()),
            ]
        );
        assert_eq!(
            ClientSettings::default().sanitized(),
            ClientSettings::default()
        );
        for action in ShortcutAction::ALL {
            assert_eq!(ShortcutAction::from_id(action.id()), Some(action));
            assert_eq!(
                serde_json::to_string(&action).unwrap(),
                format!("\"{}\"", action.id())
            );
        }
    }
    #[test]
    fn parses_mumble_urls_like_stock_clients() {
        let url = MumbleUrl::parse(
            "mumble://Ada%20L:s3cret@Voice.Example.org:1234/Lobby/Dev%20Talk?version=1.2.0",
        )
        .unwrap();
        assert_eq!(url.host, "voice.example.org");
        assert_eq!(url.port, 1234);
        assert_eq!(url.username.as_deref(), Some("Ada L"));
        assert_eq!(url.password.as_deref(), Some("s3cret"));
        assert_eq!(url.channel, ["Lobby", "Dev Talk"]);
        assert!(!format!("{url:?}").contains("s3cret"));
        let url = MumbleUrl::parse("mumble://[::1]/").unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.username),
            ("::1", 64738, None)
        );
        for bad in [
            "http://x",
            "mumble://",
            "mumble://host:99999",
            "mumble://a%0ab",
            "mumble://h/%zz",
        ] {
            assert!(MumbleUrl::parse(bad).is_err(), "{bad}");
        }
    }
}
