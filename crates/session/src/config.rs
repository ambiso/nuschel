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
    /// Push-to-talk key in GPUI keystroke notation, e.g. "f13" or "ctrl-space".
    pub push_to_talk_key: Option<String>,
}
impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            audio: Default::default(),
            force_tcp: false,
            auto_reconnect: true,
            user_volumes: Default::default(),
            local_mutes: Default::default(),
            push_to_talk_key: None,
        }
    }
}
impl ClientSettings {
    pub fn sanitized(mut self) -> Self {
        self.audio = self.audio.sanitized();
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
        if self
            .push_to_talk_key
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > 64)
        {
            self.push_to_talk_key = None;
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
}

#[derive(Clone)]
pub struct Profile {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Profile {
    pub fn open(override_dir: Option<PathBuf>) -> io::Result<Self> {
        let profile = if let Some(root) = override_dir {
            Self {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
            }
        } else {
            let paths = directories::ProjectDirs::from("org", "Nuschel", "Nuschel")
                .ok_or_else(|| io::Error::other("could not find the application data directory"))?;
            Self {
                config_dir: paths.config_dir().to_owned(),
                data_dir: paths.data_local_dir().to_owned(),
            }
        };
        for directory in [&profile.config_dir, &profile.data_dir] {
            fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(profile)
    }

    pub fn load_servers(&self) -> io::Result<Vec<ServerBookmark>> {
        let path = self.config_dir.join("servers.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let servers: Vec<ServerBookmark> =
            serde_json::from_slice(&read_bounded(&path, 8 * 1024 * 1024)?)?;
        if servers.len() > 100 {
            return Err(io::Error::other("too many saved servers"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for server in &servers {
            ServerBookmark::new(&server.name, &server.host, server.port, &server.username)
                .map_err(io::Error::other)?;
            if server.cached_channels.len() > 4096
                || server
                    .cached_channels
                    .iter()
                    .any(|channel| channel.name.len() > 512)
            {
                return Err(io::Error::other("saved channel cache exceeds its limit"));
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

    pub fn save_servers(&self, servers: &[ServerBookmark]) -> io::Result<()> {
        if servers.len() > 100 {
            return Err(io::Error::other("at most 100 servers may be saved"));
        }
        let bytes = serde_json::to_vec_pretty(servers)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(io::Error::other("server metadata exceeds 8 MiB"));
        }
        atomic_private_write(&self.config_dir.join("servers.json"), &bytes)
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
        self.data_dir.join("history.sqlite3")
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
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["nuschel-client.invalid".into()])?;
        let identity = ClientIdentity {
            certificate: cert.der().to_vec(),
            private_key: signing_key.serialize_der(),
        };
        atomic_private_write(&path, &serde_json::to_vec(&identity)?)?;
        Ok(identity)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
}

pub fn fingerprint(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    fn settings_round_trip_and_are_sanitized() {
        let root = std::env::temp_dir().join(format!("nuschel-settings-{}", Uuid::new_v4()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        assert_eq!(profile.load_settings().unwrap(), ClientSettings::default());
        let mut settings = ClientSettings::default();
        settings.user_volumes.insert("abc".into(), 9.);
        settings.local_mutes.insert("def".into());
        profile.save_settings(&settings).unwrap();
        let loaded = profile.load_settings().unwrap();
        assert_eq!(loaded.volume_for(Some("abc")), 4.);
        assert!(loaded.locally_muted(Some("def")));
        assert_eq!(loaded.volume_for(None), 1.);
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
}
