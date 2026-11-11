// SPDX-License-Identifier: EUPL-1.2
//! Saved servers: where a server is, who we are there, the certificate pin
//! and the channel tree cached for offline browsing. Plain data every client
//! shares; native platforms keep them in `servers.json` (`nuschel-session`).
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// A new saved server with a random id (the `uuid` feature: native
    /// platforms; a browser build makes its ids another way).
    #[cfg(feature = "uuid")]
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
            id: uuid::Uuid::new_v4().to_string(),
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

/// Lowercase hex SHA-256.
pub fn fingerprint(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
