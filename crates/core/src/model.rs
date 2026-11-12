// SPDX-License-Identifier: EUPL-1.2
//! Session values: what a Mumble or Nuschl session reports about a server
//! (channels, users, activity, administration), as plain data every client
//! shares (the native session, the SDK, and a browser build). Raw Mumble HTML
//! (comments, descriptions) is carried as untrusted text; clients render it
//! only through the content worker.
use serde::Serialize;
use std::collections::BTreeSet;

/// Mumble permission bits (`ChanACL::Perm`).
pub mod permission {
    pub const WRITE: u32 = 0x1;
    pub const TRAVERSE: u32 = 0x2;
    pub const ENTER: u32 = 0x4;
    pub const SPEAK: u32 = 0x8;
    pub const MUTE_DEAFEN: u32 = 0x10;
    pub const MOVE: u32 = 0x20;
    pub const MAKE_CHANNEL: u32 = 0x40;
    pub const LINK_CHANNEL: u32 = 0x80;
    pub const WHISPER: u32 = 0x100;
    pub const TEXT_MESSAGE: u32 = 0x200;
    pub const MAKE_TEMP_CHANNEL: u32 = 0x400;
    pub const LISTEN: u32 = 0x800;
    pub const KICK: u32 = 0x1_0000;
    pub const BAN: u32 = 0x2_0000;
    pub const REGISTER: u32 = 0x4_0000;
    pub const SELF_REGISTER: u32 = 0x8_0000;
    pub const RESET_USER_CONTENT: u32 = 0x10_0000;
    pub const CACHED: u32 = 0x800_0000;
    /// Names for the ACL editor, in Mumble's display order.
    pub const NAMED: &[(u32, &str)] = &[
        (WRITE, "Write ACL"),
        (TRAVERSE, "Traverse"),
        (ENTER, "Enter"),
        (SPEAK, "Speak"),
        (MUTE_DEAFEN, "Mute/deafen"),
        (MOVE, "Move"),
        (MAKE_CHANNEL, "Make channel"),
        (LINK_CHANNEL, "Link channel"),
        (WHISPER, "Whisper"),
        (TEXT_MESSAGE, "Text message"),
        (MAKE_TEMP_CHANNEL, "Make temporary"),
        (LISTEN, "Listen"),
        (KICK, "Kick"),
        (BAN, "Ban"),
        (REGISTER, "Register user"),
        (SELF_REGISTER, "Register self"),
        (RESET_USER_CONTENT, "Reset user content"),
    ];
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Channel {
    /// Process-local lifetime; changes after removal/recreation or reconnect.
    pub instance: u64,
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    /// Untrusted Mumble HTML; empty until loaded when only a hash was sent.
    pub description: String,
    pub description_hash: Option<String>,
    pub position: i32,
    pub links: BTreeSet<u32>,
    pub temporary: bool,
    pub max_users: u32,
    pub enter_restricted: bool,
    pub can_enter: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct User {
    /// A process-local presence token, never reused across reconnects. A
    /// certificate hash identifies the same person after a new presence.
    pub instance: u64,
    pub session: u32,
    pub name: String,
    /// Registered users have a server-side identifier.
    pub user_id: Option<u32>,
    pub channel: u32,
    pub certificate_hash: Option<String>,
    pub muted: bool,
    pub deafened: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub suppressed: bool,
    pub priority_speaker: bool,
    pub recording: bool,
    /// Untrusted Mumble HTML.
    pub comment: Option<String>,
    pub comment_hash: Option<String>,
    /// Encoded avatar image (PNG/JPEG or legacy raw); decode only in a worker.
    #[serde(skip)]
    pub texture: Option<Vec<u8>>,
    pub texture_hash: Option<String>,
    pub listening: BTreeSet<u32>,
}

/// A failed private send. Clear it when that recipient becomes available again.
pub const RECIPIENT_UNAVAILABLE_NOTICE: &str =
    "This recipient is no longer available. Your message was not sent.";

impl User {
    /// Bytes of server-provided comment and avatar held for this user.
    pub fn blob_bytes(&self) -> usize {
        self.comment.as_ref().map_or(0, String::len) + self.texture.as_ref().map_or(0, Vec::len)
    }
    pub fn set_comment_hash(&mut self, hash: &[u8]) {
        let hash = hex(hash);
        if self.comment_hash.as_ref() != Some(&hash) {
            self.comment = None;
        }
        self.comment_hash = Some(hash);
    }
    pub fn set_texture_hash(&mut self, hash: &[u8]) {
        let hash = hex(hash);
        if self.texture_hash.as_ref() != Some(&hash) {
            self.texture = None;
        }
        self.texture_hash = Some(hash);
    }
}

impl Channel {
    pub fn set_description_hash(&mut self, hash: &[u8]) {
        let hash = hex(hash);
        if self.description_hash.as_ref() != Some(&hash) {
            self.description.clear();
        }
        self.description_hash = Some(hash);
    }
}

/// Native enqueue authority for automatic metadata work. The SDK supplies
/// original interest lifetimes, never just reusable numeric wire IDs. Browser
/// platforms keep their existing source-ticket checks; this is not a wire type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataAuthority {
    pub generation: u64,
    pub own: (u32, u64),
    pub users: Vec<(u32, u64)>,
    pub channels: Vec<(u32, u64)>,
}

/// Why the server refused a login (`Reject.RejectType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectKind {
    Other,
    WrongVersion,
    InvalidUsername,
    WrongUserPassword,
    WrongServerPassword,
    UsernameInUse,
    ServerFull,
    NoCertificate,
    AuthenticatorFail,
}

impl RejectKind {
    pub fn from_wire(value: Option<i32>) -> Self {
        match value {
            Some(1) => Self::WrongVersion,
            Some(2) => Self::InvalidUsername,
            Some(3) => Self::WrongUserPassword,
            Some(4) => Self::WrongServerPassword,
            Some(5) => Self::UsernameInUse,
            Some(6) => Self::ServerFull,
            Some(7) => Self::NoCertificate,
            Some(8) => Self::AuthenticatorFail,
            _ => Self::Other,
        }
    }
    /// A retry with a password or another name may succeed.
    pub fn needs_password(self) -> bool {
        matches!(self, Self::WrongUserPassword | Self::WrongServerPassword)
    }
    pub fn needs_username(self) -> bool {
        matches!(self, Self::InvalidUsername | Self::UsernameInUse)
    }
    pub fn describe(self) -> &'static str {
        match self {
            Self::Other => "The server refused the connection.",
            Self::WrongVersion => "This server requires a different Mumble version.",
            Self::InvalidUsername => "The server does not accept this username.",
            Self::WrongUserPassword => "Wrong password for this registered username.",
            Self::WrongServerPassword => "This server requires a password.",
            Self::UsernameInUse => "Someone is already connected with this username.",
            Self::ServerFull => "The server is full.",
            Self::NoCertificate => "The server requires a client certificate.",
            Self::AuthenticatorFail => "The server's authenticator failed.",
        }
    }
}

/// A one-line event for the conversation log (joins, moves, moderation…).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Activity {
    /// Channels the line is relevant to; empty means server-wide.
    pub channels: Vec<u32>,
    pub text: String,
    pub observed_at_ms: i64,
    /// Where `text` names people, in order, so a client can show its own
    /// names for them. The first is the person the line is about.
    pub names: Vec<NameSpan>,
    /// Someone connected, disconnected or changed name.
    pub presence: Option<Presence>,
}

/// A user's name in an activity line: bytes `start..end` of its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NameSpan {
    pub start: usize,
    pub end: usize,
    pub certificate_hash: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Connected,
    Disconnected,
    Renamed,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PacketStats {
    pub good: u32,
    pub late: u32,
    pub lost: u32,
    pub resync: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UserStats {
    pub session: u32,
    pub release: Option<String>,
    pub os: Option<String>,
    pub version: Option<String>,
    pub address: Option<String>,
    pub online_seconds: Option<u32>,
    pub idle_seconds: Option<u32>,
    /// Mumble UserStats traffic rate in bytes per second; server limits use bits per second.
    pub bandwidth: Option<u32>,
    pub udp_ping_ms: Option<f32>,
    pub tcp_ping_ms: Option<f32>,
    pub udp_packets: Option<u32>,
    pub tcp_packets: Option<u32>,
    pub from_client: Option<PacketStats>,
    pub from_server: Option<PacketStats>,
    /// Whether the client supports Opus; servers send it only with the
    /// extended statistics (the user themselves, or with Register permission).
    pub opus: Option<bool>,
    pub strong_certificate: bool,
    /// SHA-1 of the leaf certificate, as Mumble shows it.
    pub certificate_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextAction {
    pub action: String,
    pub text: String,
    pub server: bool,
    pub channel: bool,
    pub user: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RegisteredUser {
    pub user_id: u32,
    pub name: Option<String>,
    pub last_seen: Option<String>,
    pub last_channel: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ban {
    pub address: Vec<u8>,
    pub mask: u32,
    pub name: Option<String>,
    pub hash: Option<String>,
    pub reason: Option<String>,
    pub start: Option<String>,
    pub duration: Option<u32>,
}

impl Ban {
    pub fn address_text(&self) -> String {
        match self.address.len() {
            16 if self.address[..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff] => {
                let v4 = &self.address[12..];
                format!(
                    "{}.{}.{}.{}/{}",
                    v4[0],
                    v4[1],
                    v4[2],
                    v4[3],
                    self.mask.saturating_sub(96)
                )
            }
            16 => {
                let octets: [u8; 16] = self.address[..16].try_into().unwrap_or([0; 16]);
                format!("{}/{}", std::net::Ipv6Addr::from(octets), self.mask)
            }
            _ => "invalid address".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AclGroup {
    pub name: String,
    pub inherited: bool,
    pub inherit: bool,
    pub inheritable: bool,
    pub add: Vec<u32>,
    pub remove: Vec<u32>,
    pub inherited_members: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AclEntry {
    pub apply_here: bool,
    pub apply_subs: bool,
    pub inherited: bool,
    pub user_id: Option<u32>,
    pub group: Option<String>,
    pub grant: u32,
    pub deny: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Acl {
    pub channel: u32,
    pub inherit_acls: bool,
    pub groups: Vec<AclGroup>,
    pub entries: Vec<AclEntry>,
}

/// A whisper/shout destination (`VoiceTarget.Target`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum VoiceTargetSpec {
    Users(Vec<u32>),
    Channel {
        channel: u32,
        links: bool,
        children: bool,
        group: Option<String>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct ChannelEdit {
    pub name: Option<String>,
    pub description: Option<String>,
    pub position: Option<i32>,
    pub max_users: Option<u32>,
    pub parent: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserAction {
    Kick,
    Ban,
    Mute(bool),
    Deafen(bool),
    PrioritySpeaker(bool),
    Move(u32),
    Register,
    ResetComment,
    ResetAvatar,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum TextTarget {
    Channel(u32),
    Direct(u32),
    /// A direct conversation on a Nuschl server (`OpenDirect`), by its id:
    /// stored, and reachable while the other account is offline.
    DirectConversation([u8; 16]),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerLimits {
    pub allow_html: bool,
    /// Server limits count UTF-16 code units. Zero means unlimited by the
    /// server; application byte ceilings are separate and always remain finite.
    pub message_length: usize,
    pub image_message_length: usize,
    pub max_users: Option<u32>,
    pub recording_allowed: bool,
    pub max_bandwidth: Option<u32>,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            allow_html: true,
            message_length: 5000,
            image_message_length: 131_072,
            max_users: None,
            recording_allowed: true,
            max_bandwidth: Some(crate::audio::DEFAULT_SERVER_BANDWIDTH),
        }
    }
}

/// Protocol-independent optional fields from one sparse server update.
#[derive(Clone, Copy, Debug, Default)]
pub struct LimitsUpdate {
    pub allow_html: Option<bool>,
    pub message_length: Option<u32>,
    pub image_message_length: Option<u32>,
    pub max_users: Option<u32>,
    pub recording_allowed: Option<bool>,
    pub max_bandwidth: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageLimitExceeded {
    ClientBytes,
    TextCharacters(usize),
    HtmlCharacters(usize),
}

impl std::fmt::Display for MessageLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientBytes => write!(
                f,
                "Message exceeds the {} KiB client limit.",
                crate::MAX_CONTENT_BYTES / 1024
            ),
            Self::TextCharacters(limit) => write!(
                f,
                "Message exceeds this server's {limit}-character text limit."
            ),
            Self::HtmlCharacters(limit) => write!(
                f,
                "Message exceeds this server's {limit}-character total HTML limit."
            ),
        }
    }
}

impl ServerLimits {
    /// Whether this update supplied any limit fields. An explicit zero/false
    /// is preserved, including when it repeats the previous value.
    pub fn apply(&mut self, update: LimitsUpdate) -> bool {
        let supplied = update.allow_html.is_some()
            || update.message_length.is_some()
            || update.image_message_length.is_some()
            || update.max_users.is_some()
            || update.recording_allowed.is_some()
            || update.max_bandwidth.is_some();
        if let Some(value) = update.allow_html {
            self.allow_html = value;
        }
        if let Some(value) = update.message_length {
            self.message_length = value as usize;
        }
        if let Some(value) = update.image_message_length {
            self.image_message_length = value as usize;
        }
        if let Some(value) = update.max_users {
            self.max_users = Some(value);
        }
        if let Some(value) = update.recording_allowed {
            self.recording_allowed = value;
        }
        if let Some(value) = update.max_bandwidth {
            self.max_bandwidth = Some(value);
        }
        supplied
    }

    pub fn bandwidth(&self) -> u32 {
        self.max_bandwidth
            .unwrap_or(crate::audio::DEFAULT_SERVER_BANDWIDTH)
    }

    /// ASCII inline-image markup is prepared against this byte budget. This is
    /// intentionally independent of a server's unlimited (zero) declaration.
    pub fn inline_image_byte_budget(&self) -> usize {
        if self.image_message_length == 0 {
            crate::MAX_CONTENT_BYTES
        } else {
            self.image_message_length.min(crate::MAX_CONTENT_BYTES)
        }
    }

    /// Plain user text after legacy formatting (it contains no inline images).
    /// In HTML mode Murmur always enforces the whole-message image cap too.
    pub fn check_text_wire(&self, wire: &str) -> Result<(), MessageLimitExceeded> {
        let units = Self::wire_units(wire)?;
        self.check_html_units(units)?;
        if self.message_length != 0 && units > self.message_length {
            return Err(MessageLimitExceeded::TextCharacters(self.message_length));
        }
        Ok(())
    }

    /// Only the total envelope budget for rich HTML. Its separate text budget
    /// excludes image src values and needs XML-aware checking; this helper must
    /// not apply min(text,image) to a complete inline-image body.
    pub fn check_html_total(&self, wire: &str) -> Result<(), MessageLimitExceeded> {
        self.check_html_units(Self::wire_units(wire)?)
    }

    fn wire_units(wire: &str) -> Result<usize, MessageLimitExceeded> {
        if wire.len() > crate::MAX_CONTENT_BYTES {
            return Err(MessageLimitExceeded::ClientBytes);
        }
        Ok(wire.encode_utf16().count())
    }

    fn check_html_units(&self, units: usize) -> Result<(), MessageLimitExceeded> {
        if self.allow_html && self.image_message_length != 0 && units > self.image_message_length {
            return Err(MessageLimitExceeded::HtmlCharacters(
                self.image_message_length,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct PingStats {
    pub packets: u32,
    pub average_ms: f32,
    pub variance_ms: f32,
}
impl PingStats {
    pub fn record(&mut self, rtt_ms: f32) {
        if !rtt_ms.is_finite() || rtt_ms < 0. {
            return;
        }
        self.packets = self.packets.saturating_add(1);
        let n = self.packets as f32;
        let delta = rtt_ms - self.average_ms;
        self.average_ms += delta / n;
        // Exponentially weighted variance keeps the value responsive on long calls.
        self.variance_ms = if self.packets == 1 {
            0.
        } else {
            0.9 * self.variance_ms + 0.1 * delta * delta
        };
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct TransportStats {
    pub udp_active: bool,
    pub forced_tcp: bool,
    pub udp: PingStats,
    pub tcp: PingStats,
    /// Server-to-client UDP crypt statistics.
    pub good: u32,
    pub late: u32,
    pub lost: u32,
    pub resync: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_limits_keep_server_zero_false_and_large_values_separate_from_local_bytes() {
        let mut limits = ServerLimits::default();
        assert!(!limits.apply(LimitsUpdate::default()));
        assert!(limits.apply(LimitsUpdate {
            allow_html: Some(false),
            message_length: Some(0),
            image_message_length: Some(u32::MAX),
            max_users: Some(0),
            recording_allowed: Some(false),
            max_bandwidth: Some(0)
        }));
        assert_eq!(limits.message_length, 0);
        assert_eq!(limits.image_message_length, u32::MAX as usize);
        assert_eq!(limits.max_users, Some(0));
        assert_eq!(limits.bandwidth(), 0);
        assert!(!limits.allow_html && !limits.recording_allowed);
        assert_eq!(limits.inline_image_byte_budget(), crate::MAX_CONTENT_BYTES);
        let before = limits.clone();
        assert!(!limits.apply(LimitsUpdate::default()));
        assert_eq!(limits, before);
        limits.apply(LimitsUpdate {
            max_bandwidth: Some(48_000),
            ..Default::default()
        });
        assert_eq!(limits.message_length, 0);
        assert!(!limits.allow_html && !limits.recording_allowed);
        assert_eq!(limits.bandwidth(), 48_000);
    }

    #[test]
    fn text_limits_count_utf16_and_always_enforce_html_total_even_with_unlimited_text() {
        for html in [false, true] {
            let mut limits = ServerLimits {
                allow_html: html,
                message_length: 1,
                image_message_length: 0,
                ..Default::default()
            };
            assert_eq!(limits.check_text_wire("漢"), Ok(()));
            assert_eq!(limits.check_text_wire("é"), Ok(()));
            assert_eq!(
                limits.check_text_wire("🙂"),
                Err(MessageLimitExceeded::TextCharacters(1))
            );
            limits.message_length = 2;
            assert_eq!(limits.check_text_wire("🙂"), Ok(()));
            assert_eq!(limits.check_text_wire("漢字"), Ok(()));
            limits.message_length = 0;
            limits.image_message_length = 2;
            assert_eq!(limits.check_text_wire("abc").is_err(), html);
            assert_eq!(limits.check_text_wire("漢字"), Ok(()));
            limits.image_message_length = 0;
            assert_eq!(
                limits.check_text_wire(&"x".repeat(crate::MAX_CONTENT_BYTES)),
                Ok(())
            );
            assert_eq!(
                limits.check_text_wire(&"x".repeat(crate::MAX_CONTENT_BYTES + 1)),
                Err(MessageLimitExceeded::ClientBytes)
            );
            assert_eq!(
                limits.check_text_wire(&"漢".repeat(crate::MAX_CONTENT_BYTES / 3 + 1)),
                Err(MessageLimitExceeded::ClientBytes)
            );
        }
    }

    #[test]
    fn rich_html_total_never_uses_the_larger_text_limit_or_strips_inline_image_bytes() {
        let limits = ServerLimits {
            message_length: 5000,
            image_message_length: 128,
            ..Default::default()
        };
        assert_eq!(
            limits.check_html_total(&"x".repeat(129)),
            Err(MessageLimitExceeded::HtmlCharacters(128))
        );
        assert_eq!(limits.check_html_total(&"x".repeat(128)), Ok(()));
        let limits = ServerLimits {
            message_length: 50,
            image_message_length: 256,
            ..Default::default()
        };
        let image = format!("<img src="data:image/png;base64,{}"/>","A".repeat(100));
        assert!(image.len() > limits.message_length);
        assert_eq!(
            limits.check_html_total(&image),
            Ok(()),
            "total-only check must not apply the text cap to image data"
        );
    }

    #[test]
    fn formats_mapped_ipv4_and_ipv6_bans() {
        let mut address = vec![0; 10];
        address.extend([0xff, 0xff, 192, 0, 2, 7]);
        let ban = Ban {
            address,
            mask: 120,
            name: None,
            hash: None,
            reason: None,
            start: None,
            duration: None,
        };
        assert_eq!(ban.address_text(), "192.0.2.7/24");
        let ban = Ban {
            address: std::net::Ipv6Addr::LOCALHOST.octets().to_vec(),
            mask: 128,
            ..ban
        };
        assert_eq!(ban.address_text(), "::1/128");
    }
    #[test]
    fn maps_reject_types() {
        assert_eq!(
            RejectKind::from_wire(Some(4)),
            RejectKind::WrongServerPassword
        );
        assert!(RejectKind::from_wire(Some(3)).needs_password());
        assert!(RejectKind::from_wire(Some(5)).needs_username());
        assert_eq!(RejectKind::from_wire(Some(99)), RejectKind::Other);
    }
}
