// SPDX-License-Identifier: EUPL-1.2
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
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
    /// Declared in managed.json: its name, address, username, tokens and
    /// automatic connection come from there on every start.
    #[serde(default)]
    pub managed: bool,
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
            managed: false,
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
    /// From an invite (`?fp=sha256:…`): the server certificate's SHA-256, pinned
    /// before the first connection.
    pub certificate_sha256: Option<String>,
    /// From an invite (`?policy=nuschel`): the server must speak the Nuschel
    /// protocol, from the very first connection.
    pub require_nuschel: bool,
}
impl std::fmt::Debug for MumbleUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MumbleUrl")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("channel", &self.channel)
            .field("certificate_sha256", &self.certificate_sha256)
            .field("require_nuschel", &self.require_nuschel)
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
        let rest = rest.split('#').next().unwrap_or_default();
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut certificate_sha256 = None;
        let mut require_nuschel = false;
        for (name, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
            match name {
                "fp" => {
                    let hex = value
                        .strip_prefix("sha256:")
                        .filter(|hex| hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit()))
                        .ok_or("the fp= parameter must be sha256: and 64 hex digits")?;
                    certificate_sha256 = Some(hex.to_ascii_lowercase());
                }
                "policy" => {
                    require_nuschel = value.split('+').any(|part| part == "nuschel");
                }
                _ => {}
            }
        }
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
            certificate_sha256,
            require_nuschel,
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
    /// Tell others in a conversation that you are typing (Nuschel servers).
    pub send_typing: bool,
    /// magic-wormhole rendezvous server for offering files (`ws://`).
    pub wormhole_rendezvous: String,
    /// magic-wormhole transit relay (`tcp://host:port`).
    pub wormhole_relay: String,
    /// Theme, fonts, text size and zoom.
    pub appearance: AppearanceSettings,
    /// Whether a recovery key is offered when the Nuschel identity is created
    /// (docs/e2ee.md §10, question 1: offered and skippable by default).
    pub recovery_key: nuschel_ipc::crypto::RecoveryPolicy,
    /// End-to-end encrypted direct messages (experimental): upload key packages
    /// and create end-to-end groups for direct conversations on `mls/1` servers.
    /// Off until the client sends and shows them; once a conversation has a group
    /// the server takes no plaintext there.
    pub e2ee_direct_messages: bool,
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
    /// Show LaTeX formulas in messages rendered (docs/math.md).
    pub render_math: bool,
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
            render_math: true,
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
            send_typing: true,
            wormhole_rendezvous: nuschel_core::transfer::PUBLIC_RENDEZVOUS.into(),
            wormhole_relay: nuschel_core::transfer::PUBLIC_RELAY.into(),
            appearance: AppearanceSettings::default(),
            recovery_key: Default::default(),
            e2ee_direct_messages: false,
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

/// What a system configuration (Home Manager, an administrator) sets in
/// managed.json: settings that win over settings.json on every start, and
/// bookmarks. Nuschel never writes it.
#[derive(Clone, Debug, Default)]
pub struct Managed {
    settings: Map<String, Value>,
    servers: Vec<ServerBookmark>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedFile {
    #[serde(default)]
    settings: Map<String, Value>,
    #[serde(default)]
    servers: Vec<ManagedServer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedServer {
    id: String,
    #[serde(default)]
    name: String,
    host: String,
    #[serde(default = "mumble_port")]
    port: u16,
    username: String,
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default = "connect_on_startup")]
    auto_connect: bool,
    #[serde(default)]
    certificate_sha256: Option<String>,
}

fn mumble_port() -> u16 {
    64738
}

impl Managed {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        let file: ManagedFile = serde_json::from_slice(bytes)?;
        // Only settings Nuschel knows, with values of the right types.
        ClientSettings::deserialize(&Value::Object(file.settings.clone())).map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidData, format!("settings: {error}"))
        })?;
        let mut ids = BTreeSet::new();
        let servers = file
            .servers
            .into_iter()
            .map(|declared| {
                let mut server = ServerBookmark::new(
                    &declared.name,
                    &declared.host,
                    declared.port,
                    &declared.username,
                )
                .map_err(io::Error::other)?;
                server.id = declared.id;
                server.tokens = declared.tokens;
                server.auto_connect = declared.auto_connect;
                server.certificate_sha256 = declared
                    .certificate_sha256
                    .map(|pin| pin.to_ascii_lowercase());
                server.managed = true;
                check_bookmark(&server)?;
                if !ids.insert(server.id.clone()) {
                    return Err(io::Error::other("two servers have the same id"));
                }
                Ok(server)
            })
            .collect::<io::Result<Vec<_>>>()?;
        if servers.len() > MAX_SERVERS {
            return Err(io::Error::other("more than 100 servers"));
        }
        Ok(Self {
            settings: file.settings,
            servers,
        })
    }

    /// `own` (settings.json, already checked) with the managed settings over it.
    fn settings_over(&self, own: Map<String, Value>) -> (ClientSettings, ManagedSettings) {
        let settings = ClientSettings::deserialize(&Value::Object(own.clone()))
            .unwrap_or_default()
            .sanitized();
        let managed = ManagedSettings::new(self.settings.clone(), own);
        (managed.apply(settings), managed)
    }
}

/// The settings managed.json sets, and settings.json's own values beneath them.
#[derive(Clone, Debug, Default)]
pub struct ManagedSettings {
    layer: Map<String, Value>,
    own: Map<String, Value>,
    /// Paths to the values `layer` sets, like `["audio", "transmit"]`.
    keys: Vec<Vec<String>>,
}

impl ManagedSettings {
    fn new(layer: Map<String, Value>, own: Map<String, Value>) -> Self {
        fn leaves(map: &Map<String, Value>, path: &mut Vec<String>, keys: &mut Vec<Vec<String>>) {
            for (key, value) in map {
                path.push(key.clone());
                match value {
                    Value::Object(inner) => leaves(inner, path, keys),
                    _ => keys.push(path.clone()),
                }
                path.pop();
            }
        }
        let mut keys = Vec::new();
        leaves(&layer, &mut Vec::new(), &mut keys);
        Self { layer, own, keys }
    }

    /// Whether managed.json sets `key` (dotted, like `audio.transmit`) or part of it.
    pub fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|managed| {
            managed
                .iter()
                .zip(key.split('.'))
                .all(|(managed, key)| managed == key)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// `settings` with the managed values back in place, whatever changed them.
    pub fn apply(&self, settings: ClientSettings) -> ClientSettings {
        if self.keys.is_empty() {
            return settings;
        }
        let Ok(Value::Object(mut map)) = serde_json::to_value(&settings) else {
            return settings;
        };
        merge(&mut map, &self.layer);
        ClientSettings::deserialize(&Value::Object(map)).map_or(settings, ClientSettings::sanitized)
    }

    /// What settings.json keeps of `settings`: under the managed keys, its own
    /// values or nothing.
    fn unmanaged(&self, settings: &ClientSettings) -> serde_json::Result<Value> {
        let mut value = serde_json::to_value(settings)?;
        if let Value::Object(map) = &mut value {
            for key in &self.keys {
                put(map, key, lookup(&self.own, key).cloned());
            }
        }
        Ok(value)
    }
}

/// Objects merge key by key; any other value of `over` replaces `base`'s.
fn merge(base: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(Value::Object(base)), Value::Object(over)) => merge(base, over),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn lookup<'a>(map: &'a Map<String, Value>, path: &[String]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let value = map.get(first)?;
    match rest {
        [] => Some(value),
        rest => lookup(value.as_object()?, rest),
    }
}

/// Set the value at `path`, or remove it with `None`.
fn put(map: &mut Map<String, Value>, path: &[String], value: Option<Value>) {
    match (path, value) {
        ([key], Some(value)) => {
            map.insert(key.clone(), value);
        }
        ([key], None) => {
            map.remove(key);
        }
        ([first, rest @ ..], value) => {
            if let Some(Value::Object(inner)) = map.get_mut(first) {
                put(inner, rest, value);
            }
        }
        ([], _) => {}
    }
}

/// Saved bookmarks with the declared ones over them. A declared bookmark keeps
/// what Nuschel saved about it (an accepted certificate, the channel tree, the
/// icon, the last channel) while its address stays the same; one no longer
/// declared is dropped.
fn merge_servers(
    mut saved: Vec<ServerBookmark>,
    declared: &[ServerBookmark],
) -> Vec<ServerBookmark> {
    saved.retain(|server| !server.managed || declared.iter().any(|known| known.id == server.id));
    for server in declared {
        let mut server = server.clone();
        match saved.iter_mut().find(|known| known.id == server.id) {
            Some(known) => {
                if known.host == server.host && known.port == server.port {
                    server.certificate_sha256 = server
                        .certificate_sha256
                        .or_else(|| known.certificate_sha256.take());
                    server.cached_channels = std::mem::take(&mut known.cached_channels);
                    server.icon = known.icon.take();
                    server.icon_checked_ms = known.icon_checked_ms;
                    server.last_channel = known.last_channel;
                }
                *known = server;
            }
            None => saved.push(server),
        }
    }
    saved
}

/// What servers.json and managed.json must hold for each bookmark.
fn check_bookmark(server: &ServerBookmark) -> io::Result<()> {
    ServerBookmark::new(&server.name, &server.host, server.port, &server.username)
        .map_err(io::Error::other)?;
    if server.tokens.len() > 64 || server.tokens.iter().any(|token| token.len() > 256) {
        return Err(io::Error::other("too many or oversized access tokens"));
    }
    if Uuid::parse_str(&server.id).is_err()
        || server
            .certificate_sha256
            .as_ref()
            .is_some_and(|pin| pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(io::Error::other(
            "invalid saved server identity or certificate fingerprint",
        ));
    }
    Ok(())
}

/// At most 100 servers of your own, and 100 managed ones.
fn too_many(servers: &[ServerBookmark]) -> bool {
    let managed = servers.iter().filter(|server| server.managed).count();
    managed > MAX_SERVERS || servers.len() - managed > MAX_SERVERS
}

/// The first `nuschel/managed.json` in `$XDG_CONFIG_DIRS` (`/etc/xdg` by default).
fn system_managed() -> Option<PathBuf> {
    if !cfg!(all(unix, not(target_os = "macos"))) {
        return None;
    }
    let directories = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|directories| !directories.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    std::env::split_paths(&directories)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("nuschel").join(MANAGED_FILE))
        .find(|path| path.exists())
}

#[derive(Clone)]
pub struct Profile {
    pub config_dir: PathBuf,
    /// The local history folder, the storage worker's only writable directory.
    /// It never holds configuration, the identity or other credentials.
    pub data_dir: PathBuf,
    /// System-wide managed configuration, under the configuration folder's own.
    system_managed: Option<PathBuf>,
    /// Files that could be neither read nor renamed: never written over.
    kept: Arc<Mutex<BTreeSet<&'static str>>>,
}

const HISTORY_FILE: &str = "history.sqlite3";
const SETTINGS_FILE: &str = "settings.json";
const SERVERS_FILE: &str = "servers.json";
/// Written by Home Manager or an administrator; Nuschel only reads it.
const MANAGED_FILE: &str = "managed.json";
const MAX_SETTINGS_FILE: u64 = 1024 * 1024;
const MAX_SERVERS: usize = 100;
const MAX_SERVERS_FILE: usize = 8 * 1024 * 1024;
const MAX_CACHED_CHANNELS: usize = 4096;
const MAX_CHANNEL_NAME: usize = 512;

/// Everything Nuschel reads at start (see [`Profile::load`]).
pub struct Loaded {
    pub settings: ClientSettings,
    pub managed: ManagedSettings,
    pub servers: Vec<ServerBookmark>,
    /// What went wrong, in sentences to show.
    pub problems: Vec<String>,
}

impl Profile {
    /// With `override_dir` (tests, separate profiles) the system-wide managed
    /// configuration is not read either.
    pub fn open(override_dir: Option<PathBuf>) -> io::Result<Self> {
        if let Some(root) = override_dir {
            return Self::at(root.join("config"), root.join("data"));
        }
        let paths = directories::ProjectDirs::from("org", "Nuschel", "Nuschel")
            .ok_or_else(|| io::Error::other("could not find the application data directory"))?;
        let mut profile = Self::at(paths.config_dir().into(), paths.data_local_dir().into())?;
        profile.system_managed = system_managed();
        Ok(profile)
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
            system_managed: None,
            kept: Default::default(),
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

    /// Settings, managed configuration and servers. A settings.json or
    /// servers.json that cannot be read is renamed to `<name>.invalid` (or
    /// `.invalid.N`) and Nuschel goes on without it; an invalid managed.json
    /// is ignored and left alone. Either way the problem is reported.
    pub fn load(&self) -> Loaded {
        let mut problems = Vec::new();
        let managed = match self.load_managed() {
            Ok(managed) => Some(managed),
            Err(error) => {
                problems.push(format!(
                    "The system configuration is invalid, so Nuschel ignored it: {error}."
                ));
                None
            }
        };
        let declared = managed.clone().unwrap_or_default();
        let (settings, managed_settings) = self.load_settings(&declared).unwrap_or_else(|error| {
            problems.push(self.set_aside(SETTINGS_FILE, &error, "default settings"));
            declared.settings_over(Map::new())
        });
        let servers = self.load_servers(managed.as_ref()).unwrap_or_else(|error| {
            problems.push(self.set_aside(SERVERS_FILE, &error, "no saved servers"));
            merge_servers(Vec::new(), &declared.servers)
        });
        Loaded {
            settings,
            managed: managed_settings,
            servers,
            problems,
        }
    }

    /// managed.json, over the first `nuschel/managed.json` in
    /// `$XDG_CONFIG_DIRS`. Missing files declare nothing.
    pub fn load_managed(&self) -> io::Result<Managed> {
        let mut managed = Managed::default();
        let own = self.config_dir.join(MANAGED_FILE);
        for path in self.system_managed.iter().chain([&own]) {
            let file = match read_bounded(path, MAX_SETTINGS_FILE) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                bytes => bytes.and_then(|bytes| Managed::parse(&bytes)),
            }
            .map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", path.display()))
            })?;
            merge(&mut managed.settings, &file.settings);
            for server in file.servers {
                match managed
                    .servers
                    .iter_mut()
                    .find(|known| known.id == server.id)
                {
                    Some(known) => *known = server,
                    None => managed.servers.push(server),
                }
            }
        }
        if managed.servers.len() > MAX_SERVERS {
            return Err(io::Error::other("more than 100 managed servers"));
        }
        Ok(managed)
    }

    /// settings.json with the managed settings over it, and which those are.
    pub fn load_settings(
        &self,
        managed: &Managed,
    ) -> io::Result<(ClientSettings, ManagedSettings)> {
        let own = match read_bounded(&self.config_dir.join(SETTINGS_FILE), MAX_SETTINGS_FILE) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
            bytes => {
                let own: Map<String, Value> = serde_json::from_slice(&bytes?)?;
                ClientSettings::deserialize(&Value::Object(own.clone()))?;
                own
            }
        };
        Ok(managed.settings_over(own))
    }

    /// servers.json, with the bookmarks `managed` declares over it. Without
    /// `managed` (an invalid managed.json) the saved ones stay as they are.
    pub fn load_servers(&self, managed: Option<&Managed>) -> io::Result<Vec<ServerBookmark>> {
        let bytes = match read_bounded(&self.config_dir.join(SERVERS_FILE), MAX_SERVERS_FILE as u64)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => b"[]".to_vec(),
            bytes => bytes?,
        };
        let mut servers: Vec<ServerBookmark> = serde_json::from_slice(&bytes)?;
        if too_many(&servers) {
            return Err(io::Error::other("too many saved servers"));
        }
        let mut ids = BTreeSet::new();
        for server in &mut servers {
            check_bookmark(server)?;
            if !ids.insert(&server.id) {
                return Err(io::Error::other("two saved servers have the same id"));
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
        }
        Ok(match managed {
            Some(managed) => merge_servers(servers, &managed.servers),
            None => servers,
        })
    }

    /// The cached channel trees are the only part a server controls, and JSON
    /// escapes a control character to six bytes, so 4,096 such names overflow
    /// the file. The caches are only an offline convenience: they are trimmed
    /// to what loading accepts, and the largest are dropped until the file
    /// fits, so a server can never stop bookmarks or certificate pins from
    /// being saved.
    pub fn save_servers(&self, servers: &[ServerBookmark]) -> io::Result<()> {
        if too_many(servers) {
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
                return self.write(SERVERS_FILE, &bytes);
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

    /// Managed keys keep settings.json's own values (or stay out of it), so
    /// the file holds only what the user chose.
    pub fn save_settings(
        &self,
        settings: &ClientSettings,
        managed: &ManagedSettings,
    ) -> io::Result<()> {
        self.write(
            SETTINGS_FILE,
            &serde_json::to_vec_pretty(&managed.unmanaged(settings)?)?,
        )
    }

    fn write(&self, name: &'static str, bytes: &[u8]) -> io::Result<()> {
        if self
            .kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(name)
        {
            return Err(io::Error::other(format!(
                "{name} could not be read, so Nuschel does not overwrite it"
            )));
        }
        atomic_private_write(&self.config_dir.join(name), bytes)
    }

    /// Rename a file that could not be read to `<name>.invalid` (`.invalid.N`
    /// when taken), so that saving never overwrites it, and say so.
    fn set_aside(&self, name: &'static str, error: &io::Error, instead: &str) -> String {
        let renamed = (0..1000)
            .map(|n| match n {
                0 => format!("{name}.invalid"),
                n => format!("{name}.invalid.{n}"),
            })
            .find(|candidate| fs::symlink_metadata(self.config_dir.join(candidate)).is_err())
            .ok_or_else(|| io::Error::other("too many earlier copies"))
            .and_then(|target| {
                fs::rename(self.config_dir.join(name), self.config_dir.join(&target))?;
                Ok(target)
            });
        match renamed {
            Ok(target) => format!(
                "{name} could not be read ({error}). It was renamed to {target}, and Nuschel started with {instead}."
            ),
            Err(rename) => {
                self.kept
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name);
                format!(
                    "{name} could not be read ({error}) or renamed ({rename}). Nuschel started with {instead} and will not save over it."
                )
            }
        }
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

pub(crate) fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("configuration exceeds its size limit"));
    }
    Ok(bytes)
}

pub(crate) fn atomic_private_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
    use nuschel_core::audio::TransmitMode;
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
        let loaded = profile.load_servers(None).unwrap();
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
            profile.load_servers(None).unwrap()[1]
                .cached_channels
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn settings_round_trip_and_are_sanitized() {
        let root = std::env::temp_dir().join(format!("nuschel-settings-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        assert_eq!(
            profile.load_settings(&Managed::default()).unwrap().0,
            ClientSettings::default()
        );
        let mut settings = ClientSettings::default();
        settings.user_volumes.insert("abc".into(), 9.);
        settings.local_mutes.insert("def".into());
        settings.sound_volume = 3.;
        profile
            .save_settings(&settings, &ManagedSettings::default())
            .unwrap();
        let loaded = profile.load_settings(&Managed::default()).unwrap().0;
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
        let loaded = profile.load_settings(&Managed::default()).unwrap().0;
        assert!(loaded.force_tcp);
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        assert_eq!(loaded.appearance.theme, ThemeChoice::System);
        // Out-of-range or partial appearance values are repaired, not rejected.
        std::fs::write(
            profile.config_dir.join("settings.json"),
            r#"{"appearance": {"theme": "light", "zoom": 9.0, "text_size": 2.0, "ui_font": " "}}"#,
        )
        .unwrap();
        let loaded = profile
            .load_settings(&Managed::default())
            .unwrap()
            .0
            .appearance;
        assert_eq!(loaded.theme, ThemeChoice::Light);
        assert_eq!((loaded.zoom, loaded.text_size), (3., 12.));
        assert_eq!(loaded.ui_font, None);
        let mut settings = ClientSettings::default();
        settings.appearance.mono_font = Some("Iosevka".into());
        settings.appearance.zoom = 1.25;
        profile
            .save_settings(&settings, &ManagedSettings::default())
            .unwrap();
        assert_eq!(
            profile.load_settings(&Managed::default()).unwrap().0,
            settings
        );
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
    fn temporary_profile(name: &str) -> (PathBuf, Profile) {
        let root = std::env::temp_dir().join(format!("nuschel-{name}-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        (root, profile)
    }
    fn write(profile: &Profile, name: &str, contents: &str) {
        fs::write(profile.config_dir.join(name), contents).unwrap();
    }
    fn read(profile: &Profile, name: &str) -> String {
        fs::read_to_string(profile.config_dir.join(name)).unwrap()
    }
    #[test]
    fn managed_settings_win_and_stay_out_of_settings_json() {
        let (root, profile) = temporary_profile("managed-settings");
        write(
            &profile,
            "settings.json",
            r#"{"enter_sends": true, "sound_volume": 0.2}"#,
        );
        write(
            &profile,
            "managed.json",
            r#"{"settings": {"enter_sends": false, "audio": {"transmit": "push_to_talk"},
                "sound_volume": 3.0, "shortcuts": {"push-to-talk": "f13"}}}"#,
        );
        let loaded = profile.load();
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        let (settings, managed) = (loaded.settings, loaded.managed);
        assert!(!settings.enter_sends);
        assert_eq!(settings.audio.transmit, TransmitMode::PushToTalk);
        // Managed values are sanitised like any other.
        assert_eq!(settings.sound_volume, 1.);
        for key in [
            "enter_sends",
            "audio.transmit",
            "audio",
            "shortcuts.push-to-talk",
        ] {
            assert!(managed.contains(key), "{key}");
        }
        for key in [
            "force_tcp",
            "audio.vad_sensitivity",
            "shortcuts.toggle-mute",
        ] {
            assert!(!managed.contains(key), "{key}");
        }
        // A change to managed keys does not hold; other changes do.
        let mut changed = settings.clone();
        changed.enter_sends = true;
        changed.audio.transmit = TransmitMode::Continuous;
        changed.force_tcp = true;
        let changed = managed.apply(changed);
        assert!(!changed.enter_sends && changed.force_tcp);
        assert_eq!(changed.audio.transmit, TransmitMode::PushToTalk);
        // settings.json keeps its own values under managed keys, or none.
        profile.save_settings(&changed, &managed).unwrap();
        let saved: Value = serde_json::from_str(&read(&profile, "settings.json")).unwrap();
        assert_eq!(saved["enter_sends"], true);
        assert_eq!(saved["sound_volume"], 0.2);
        assert_eq!(saved["force_tcp"], true);
        assert!(saved["audio"].get("transmit").is_none(), "{saved}");
        assert_eq!(saved["shortcuts"], serde_json::json!({}));
        assert_eq!(profile.load().settings, changed);
        // Without managed.json, the user's own choices return.
        fs::remove_file(profile.config_dir.join("managed.json")).unwrap();
        let loaded = profile.load();
        assert!(loaded.managed.is_empty());
        assert!(loaded.settings.enter_sends && loaded.settings.force_tcp);
        assert_eq!(loaded.settings.audio.transmit, TransmitMode::VoiceActivity);
        assert_eq!(loaded.settings.sound_volume, 0.2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn system_wide_managed_settings_yield_to_the_users_own() {
        let (root, mut profile) = temporary_profile("system-managed");
        let system = root.join("etc/xdg/nuschel/managed.json");
        fs::create_dir_all(system.parent().unwrap()).unwrap();
        let id = |n: u8| format!("00000000-0000-4000-8000-0000000000{n:02}");
        fs::write(
            &system,
            serde_json::json!({
                "settings": {"force_tcp": true, "enter_sends": false},
                "servers": [{"id": id(1), "name": "System", "host": "a.example", "username": "me"},
                            {"id": id(2), "host": "b.example", "username": "me"}]
            })
            .to_string(),
        )
        .unwrap();
        profile.system_managed = Some(system);
        write(
            &profile,
            "managed.json",
            &serde_json::json!({
                "settings": {"enter_sends": true},
                "servers": [{"id": id(1), "name": "Mine", "host": "c.example", "port": 1234,
                             "username": "me", "auto_connect": false}]
            })
            .to_string(),
        );
        let loaded = profile.load();
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert!(loaded.settings.force_tcp && loaded.settings.enter_sends);
        let servers = loaded.servers;
        assert_eq!(servers.len(), 2);
        assert_eq!((servers[0].name.as_str(), servers[0].port), ("Mine", 1234));
        assert_eq!(
            (servers[1].name.as_str(), servers[1].port),
            ("b.example", 64738)
        );
        assert!(!servers[0].auto_connect && servers[1].auto_connect);
        assert!(servers.iter().all(|server| server.managed));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn managed_servers_keep_their_runtime_state_until_no_longer_declared() {
        let (root, profile) = temporary_profile("managed-servers");
        let id = "6f1e1b8e-2b9f-4c57-9a7e-6a0b0d1e2f30";
        let declare = |host: &str, pin: Option<&str>| {
            write(
                &profile,
                "managed.json",
                &serde_json::json!({"servers": [{"id": id, "name": "Team", "host": host,
                    "port": 64738, "username": "alice", "tokens": ["blue"], "auto_connect": true,
                    "certificate_sha256": pin}]})
                .to_string(),
            )
        };
        declare("Voice.Example.org", None);
        let mut own = ServerBookmark::new("Mine", "own.example", 64738, "me").unwrap();
        own.certificate_sha256 = Some("cd".repeat(32));
        profile.save_servers(&[own.clone()]).unwrap();
        let mut servers = profile.load().servers;
        assert_eq!(servers.len(), 2);
        assert!(servers[1].managed && !servers[0].managed);
        let team = &mut servers[1];
        // What Nuschel learns at run time is saved with it, and kept.
        team.certificate_sha256 = Some("ab".repeat(32));
        team.last_channel = Some(7);
        team.cached_channels = vec![CachedChannel {
            id: 0,
            parent: None,
            name: "Root".into(),
            position: 0,
        }];
        // An edit to declared fields does not survive the next start.
        team.name = "Renamed".into();
        team.tokens.clear();
        profile.save_servers(&servers).unwrap();
        let servers = profile.load().servers;
        let team = &servers[1];
        assert_eq!(
            (team.name.as_str(), team.host.as_str()),
            ("Team", "voice.example.org")
        );
        assert_eq!(team.tokens, ["blue"]);
        assert_eq!(team.certificate_sha256, Some("ab".repeat(32)));
        assert_eq!(
            (team.last_channel, team.cached_channels.len()),
            (Some(7), 1)
        );
        assert_eq!(servers[0].certificate_sha256, own.certificate_sha256);
        // A declared certificate wins over an accepted one.
        declare("voice.example.org", Some(&"EF".repeat(32)));
        let servers = profile.load().servers;
        assert_eq!(servers[1].certificate_sha256, Some("ef".repeat(32)));
        assert_eq!(servers[1].last_channel, Some(7));
        // A new address starts afresh.
        profile.save_servers(&servers).unwrap();
        declare("other.example.org", None);
        let servers = profile.load().servers;
        assert_eq!(servers[1].certificate_sha256, None);
        assert!(servers[1].last_channel.is_none() && servers[1].cached_channels.is_empty());
        // No longer declared: gone; your own bookmark stays.
        profile.save_servers(&servers).unwrap();
        write(&profile, "managed.json", "{}");
        let servers = profile.load().servers;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].id, own.id);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn broken_files_are_set_aside_and_never_overwritten() {
        let (root, profile) = temporary_profile("broken");
        let servers = ServerBookmark::new("", "voice.example.org", 64738, "me").unwrap();
        profile.save_servers(&[servers]).unwrap();
        write(&profile, "settings.json", r#"{"enter_sends": false,"#);
        let loaded = profile.load();
        assert_eq!(loaded.settings, ClientSettings::default());
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.problems.len(), 1);
        assert!(
            loaded.problems[0].starts_with("settings.json could not be read (")
                && loaded.problems[0].contains("renamed to settings.json.invalid"),
            "{:?}",
            loaded.problems
        );
        assert!(!profile.config_dir.join("settings.json").exists());
        assert_eq!(
            read(&profile, "settings.json.invalid"),
            r#"{"enter_sends": false,"#
        );
        // Saving writes a new file beside it.
        profile
            .save_settings(&loaded.settings, &loaded.managed)
            .unwrap();
        assert!(profile.load().problems.is_empty());
        // A later broken file does not replace the first one.
        write(&profile, "settings.json", r#"{"unknown": 1}"#);
        write(&profile, "servers.json", "[{}]");
        let loaded = profile.load();
        assert_eq!(loaded.problems.len(), 2, "{:?}", loaded.problems);
        assert!(loaded.servers.is_empty());
        assert!(loaded.problems[1].contains("renamed to servers.json.invalid,"));
        assert_eq!(
            read(&profile, "settings.json.invalid.1"),
            r#"{"unknown": 1}"#
        );
        assert_eq!(
            read(&profile, "settings.json.invalid"),
            r#"{"enter_sends": false,"#
        );
        assert_eq!(read(&profile, "servers.json.invalid"), "[{}]");
        // Invalid values are refused just like invalid JSON.
        let pin = ServerBookmark::new("", "voice.example.org", 64738, "me").unwrap();
        let mut file = serde_json::to_value([pin]).unwrap();
        file[0]["certificate_sha256"] = "not a pin".into();
        write(&profile, "servers.json", &file.to_string());
        let loaded = profile.load();
        assert!(
            loaded.problems[0].contains("servers.json.invalid.1"),
            "{:?}",
            loaded.problems
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_invalid_managed_file_is_ignored_and_left_alone() {
        let (root, profile) = temporary_profile("invalid-managed");
        let id = "6f1e1b8e-2b9f-4c57-9a7e-6a0b0d1e2f30";
        let valid = format!(
            r#"{{"settings": {{"force_tcp": true}}, "servers": [{{"id": "{id}", "host": "voice.example.org", "username": "me"}}]}}"#
        );
        write(&profile, "managed.json", &valid);
        write(&profile, "settings.json", r#"{"enter_sends": false}"#);
        let loaded = profile.load();
        assert!(loaded.settings.force_tcp && loaded.problems.is_empty());
        profile.save_servers(&loaded.servers).unwrap();
        let server = |fields: &str| {
            format!(
                r#"{{"servers": [{{"id": "{id}", "host": "voice.example.org", "username": "me"{fields}}}]}}"#
            )
        };
        for invalid in [
            "{".to_string(),
            r#"{"settings": {}, "extra": 1}"#.into(),
            r#"{"settings": {"no_such_setting": true}}"#.into(),
            r#"{"settings": {"force_tcp": "yes"}}"#.into(),
            r#"{"settings": {"audio": {"transmit": "telepathy"}}}"#.into(),
            server(r#", "cached_channels": []"#),
            server(r#", "port": 0"#),
            server(r#", "certificate_sha256": "abcd""#),
            server(r#", "id": "not-a-uuid""#).replace(&format!(r#""id": "{id}", "#), ""),
            format!(
                r#"{{"servers": [{{"id": "{id}", "host": "a.example", "username": "me"}}, {{"id": "{id}", "host": "b.example", "username": "me"}}]}}"#
            ),
        ] {
            write(&profile, "managed.json", &invalid);
            assert!(profile.load_managed().is_err(), "{invalid}");
            let loaded = profile.load();
            assert_eq!(loaded.problems.len(), 1, "{invalid}");
            assert!(loaded.problems[0].starts_with("The system configuration is invalid"));
            // Nothing managed applies, settings.json does, and the last known
            // managed bookmark stays.
            assert!(loaded.managed.is_empty() && !loaded.settings.force_tcp);
            assert!(!loaded.settings.enter_sends);
            assert_eq!(loaded.servers.len(), 1, "{invalid}");
            assert!(loaded.servers[0].managed);
            profile
                .save_settings(&loaded.settings, &loaded.managed)
                .unwrap();
            assert_eq!(read(&profile, "managed.json"), invalid);
            assert!(!profile.config_dir.join("managed.json.invalid").exists());
        }
        std::fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn invites_carry_the_certificate_pin_and_policy() {
        let fp = "AB".repeat(32);
        let url = MumbleUrl::parse(&format!(
            "mumble://voice.example.org/?fp=sha256:{fp}&policy=nuschel#x"
        ))
        .unwrap();
        assert_eq!(url.certificate_sha256, Some("ab".repeat(32)));
        assert!(url.require_nuschel);
        let plain = MumbleUrl::parse("mumble://voice.example.org/?version=1.2.0").unwrap();
        assert!(!plain.require_nuschel && plain.certificate_sha256.is_none());
        assert!(MumbleUrl::parse("mumble://h/?fp=sha1:00").is_err());
    }
}
