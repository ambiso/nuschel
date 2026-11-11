// SPDX-License-Identifier: EUPL-1.2
//! Server state between `nuscheld` and its storage worker: channels with their
//! ACLs and groups, links, registered users, blobs and bans. The server keeps
//! the authoritative state in memory and writes through; the worker owns the
//! database (SQLite or PostgreSQL) and its credential.
use serde::{Deserialize, Serialize};

/// Registered users per `LoadUsers` page, so a large server never needs one
/// oversized frame.
pub const USER_PAGE: u32 = 1000;

/// Who may read a channel's history from before they could see it (phase 2,
/// docs/extension.md); stored now so the schema does not have to change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryVisibility {
    /// Members read the retained history, including messages from before they joined.
    #[default]
    All,
    SinceJoining,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAcl {
    pub apply_here: bool,
    pub apply_subs: bool,
    pub user_id: Option<u32>,
    pub group: Option<String>,
    pub grant: u32,
    pub deny: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredGroup {
    pub name: String,
    pub inherit: bool,
    pub inheritable: bool,
    pub add: Vec<u32>,
    pub remove: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredChannel {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub description: String,
    pub position: i32,
    pub max_users: u32,
    pub inherit_acl: bool,
    /// 128 random bits as 32 lowercase hex digits, assigned once and never
    /// reused, even when a channel id is (docs/extension.md, "Identifiers").
    pub conversation_id: String,
    pub history_visibility: HistoryVisibility,
    /// In priority order.
    pub acls: Vec<StoredAcl>,
    pub groups: Vec<StoredGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredUser {
    pub id: u32,
    pub name: String,
    /// An argon2id PHC string.
    pub password_hash: Option<String>,
    /// SHA-1 of the client certificate, lowercase hex (Mumble's user identity).
    pub certificate_hash: Option<String>,
    pub last_channel: Option<u32>,
    /// Unix seconds.
    pub last_active: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredBan {
    /// An IP address; IPv4 bans are stored as IPv4-mapped IPv6 with the mask
    /// counted on 128 bits, as Mumble does.
    pub address: Option<String>,
    pub mask: u8,
    pub name: String,
    pub hash: String,
    pub reason: String,
    /// Unix seconds.
    pub start: i64,
    /// Seconds; 0 means permanent.
    pub duration: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSnapshot {
    pub channels: Vec<StoredChannel>,
    pub links: Vec<(u32, u32)>,
    pub bans: Vec<StoredBan>,
    /// Registration ids below this were handed out once and are never reused.
    pub next_user_id: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerStorageRequest {
    /// Channels, links and bans; registered users come in pages.
    Load,
    LoadUsers {
        /// Return users with ids above this one.
        after: Option<u32>,
    },
    SaveChannel {
        channel: StoredChannel,
    },
    DeleteChannel {
        id: u32,
    },
    Link {
        a: u32,
        b: u32,
    },
    Unlink {
        a: u32,
        b: u32,
    },
    SaveUser {
        user: StoredUser,
    },
    DeleteUser {
        id: u32,
    },
    SetComment {
        id: u32,
        comment: String,
    },
    /// Base64-encoded image bytes; empty clears it.
    SetTexture {
        id: u32,
        texture: String,
    },
    UserBlobs {
        id: u32,
    },
    SaveBans {
        bans: Vec<StoredBan>,
    },
    SetNextUserId {
        id: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ServerStorageResponse {
    Snapshot(ServerSnapshot),
    Users(Vec<StoredUser>),
    /// Comment text and base64-encoded texture.
    Blobs {
        comment: String,
        texture: String,
    },
    Done,
    Rejected(String),
}
