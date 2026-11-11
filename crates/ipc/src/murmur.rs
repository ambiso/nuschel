// SPDX-License-Identifier: EUPL-1.2
//! A Murmur (mumble-server) database, as the storage worker reads it for
//! `nuscheld --import-murmur`. The worker opens the file read-only and decodes
//! only storage formats (blobs to addresses, dates to text, the 1.6 schema's
//! group references to Murmur's group strings); every value is still
//! untrusted, and `nuscheld` validates it with its own rules. The worker
//! streams [`MurmurFrame`]s on stdout, each below the frame limit.
use serde::{Deserialize, Serialize};

/// The largest comment, description, texture or config value the reader
/// passes on; larger values are reported and dropped. Keeps every row, even
/// one of control characters escaped by JSON, below [`crate::MAX_FRAME_BYTES`].
pub const MAX_BLOB_BYTES: usize = 1024 * 1024;
/// Rows read per table; a table cut here is reported.
pub const MAX_ROWS: usize = 200_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurHeader {
    /// "legacy" (`ServerDB.cpp`, Murmur 1.2.4 to 1.5) or "1.6".
    pub layout: String,
    /// The schema version in the `meta` table.
    pub schema_version: i64,
    /// Virtual servers in the file.
    pub server_ids: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurChannel {
    pub id: i64,
    /// `None` for the root channel.
    pub parent: Option<i64>,
    pub name: String,
    pub inherit_acl: bool,
    pub description: Option<String>,
    /// Murmur keeps these as text in `channel_info`.
    pub position: Option<String>,
    pub max_users: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurAcl {
    pub channel: i64,
    pub priority: i64,
    pub user_id: Option<i64>,
    /// Murmur's group string, with its `~ ! # $` prefixes and `sub,a,b,c`.
    pub group: Option<String>,
    pub apply_here: bool,
    pub apply_subs: bool,
    pub grant: i64,
    pub deny: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurGroup {
    pub channel: i64,
    pub name: String,
    pub inherit: bool,
    pub inheritable: bool,
    pub add: Vec<i64>,
    pub remove: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurBan {
    /// An IPv6 address (IPv4 mapped), as text.
    pub address: Option<String>,
    /// Prefix length on 128 bits.
    pub mask: Option<i64>,
    pub name: Option<String>,
    pub hash: Option<String>,
    pub reason: Option<String>,
    /// As stored: an ISO date (legacy) or Unix seconds (1.6), as text.
    pub start: Option<String>,
    pub duration: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurListener {
    pub user_id: i64,
    pub channel: i64,
    pub volume: Option<f64>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurmurUser {
    pub id: i64,
    pub name: String,
    /// Murmur's `pw`: SHA-1 hex (`kdf_iterations` ≤ 0 or absent) or
    /// PBKDF2-HMAC-SHA384 hex.
    pub password: Option<String>,
    /// Hex.
    pub salt: Option<String>,
    pub kdf_iterations: Option<i64>,
    pub last_channel: Option<i64>,
    /// As stored: an SQLite date (legacy) or Unix seconds (1.6), as text.
    pub last_active: Option<String>,
    pub certificate_hash: Option<String>,
    pub comment: Option<String>,
    /// Base64.
    pub texture: Option<String>,
    pub email: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "rows",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MurmurFrame {
    Header(MurmurHeader),
    /// Key and value pairs of the selected server's `config` table.
    Config(Vec<(String, String)>),
    Channels(Vec<MurmurChannel>),
    Acls(Vec<MurmurAcl>),
    Groups(Vec<MurmurGroup>),
    Links(Vec<(i64, i64)>),
    Bans(Vec<MurmurBan>),
    Listeners(Vec<MurmurListener>),
    Users(Vec<MurmurUser>),
    /// What the reader skipped (malformed or oversized values, cut tables);
    /// ids and table names only, never contents.
    Notes(Vec<String>),
    End,
    Failed(String),
}

/// Bytes per frame when rows are split (a single larger row goes alone).
const FRAME_TARGET: usize = 4 * 1024 * 1024;

fn chunks<T: Serialize>(rows: Vec<T>, wrap: fn(Vec<T>) -> MurmurFrame) -> Vec<MurmurFrame> {
    let mut frames = Vec::new();
    let mut batch = Vec::new();
    let mut size = 0;
    for row in rows {
        let row_size = serde_json::to_vec(&row).map_or(0, |bytes| bytes.len());
        if !batch.is_empty() && size + row_size > FRAME_TARGET {
            frames.push(wrap(std::mem::take(&mut batch)));
            size = 0;
        }
        size += row_size;
        batch.push(row);
    }
    if !batch.is_empty() {
        frames.push(wrap(batch));
    }
    frames
}

impl MurmurFrame {
    /// The same rows in frames of about 4 MiB each (none if there are no rows).
    pub fn split(self) -> Vec<MurmurFrame> {
        match self {
            Self::Config(rows) => chunks(rows, Self::Config),
            Self::Channels(rows) => chunks(rows, Self::Channels),
            Self::Acls(rows) => chunks(rows, Self::Acls),
            Self::Groups(rows) => chunks(rows, Self::Groups),
            Self::Links(rows) => chunks(rows, Self::Links),
            Self::Bans(rows) => chunks(rows, Self::Bans),
            Self::Listeners(rows) => chunks(rows, Self::Listeners),
            Self::Users(rows) => chunks(rows, Self::Users),
            Self::Notes(rows) => chunks(rows, Self::Notes),
            other => vec![other],
        }
    }
}

/// Everything read, assembled from the frames.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MurmurDump {
    pub header: MurmurHeader,
    pub config: Vec<(String, String)>,
    pub channels: Vec<MurmurChannel>,
    pub acls: Vec<MurmurAcl>,
    pub groups: Vec<MurmurGroup>,
    pub links: Vec<(i64, i64)>,
    pub bans: Vec<MurmurBan>,
    pub listeners: Vec<MurmurListener>,
    pub users: Vec<MurmurUser>,
    pub notes: Vec<String>,
}

impl MurmurDump {
    /// Pretty JSON (test fixtures).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default() + "\n"
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    /// Add one frame: `Ok(true)` at the end, `Err` if the reader failed.
    pub fn apply(&mut self, frame: MurmurFrame) -> Result<bool, String> {
        match frame {
            MurmurFrame::Header(header) => self.header = header,
            MurmurFrame::Config(rows) => self.config.extend(rows),
            MurmurFrame::Channels(rows) => self.channels.extend(rows),
            MurmurFrame::Acls(rows) => self.acls.extend(rows),
            MurmurFrame::Groups(rows) => self.groups.extend(rows),
            MurmurFrame::Links(rows) => self.links.extend(rows),
            MurmurFrame::Bans(rows) => self.bans.extend(rows),
            MurmurFrame::Listeners(rows) => self.listeners.extend(rows),
            MurmurFrame::Users(rows) => self.users.extend(rows),
            MurmurFrame::Notes(rows) => self.notes.extend(rows),
            MurmurFrame::End => return Ok(true),
            MurmurFrame::Failed(error) => return Err(error),
        }
        Ok(false)
    }
}

/// Base64 for blobs in frames.
pub fn encode_blob(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_blob(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}
