// SPDX-License-Identifier: EUPL-1.2
//! What a platform does for the client: connections, history, rendering
//! and sound. The native driver (`native::Driver`) is the implementation on
//! Linux, Windows and macOS; a browser build would provide another. Every
//! method returns at once: slow work answers later as an
//! [`Input`](crate::input::Input). Failures that can be told at once (not
//! connected, a queue full) are returned.
use crate::{HistoryTicket, model::TextTarget, timeline::Page};
use nuschl_core::{SearchCursor, audio::Sound, bookmark::ServerBookmark};

/// A request to one server's connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Move our own session to a channel.
    Join(u32),
    /// Text to a channel, a session or a direct conversation. A direct
    /// message names the session instance it was meant for, so a reused
    /// session number never receives it.
    SendText {
        target: TextTarget,
        body: String,
        expected_peer: Option<u64>,
    },
    /// Establish or refresh the selected encrypted conversation.
    OpenEncrypted {
        conversation: [u8; 16],
        channel: bool,
        peer: Option<u32>,
    },
    /// No plaintext fallback is permitted for this request.
    SendEncrypted {
        conversation: [u8; 16],
        text: String,
    },
    /// Mumble rich text as-is (inline images), one message per entry.
    SendHtml {
        target: TextTarget,
        html: Vec<String>,
        expected_peer: Option<u64>,
    },
    SetSelf {
        muted: bool,
        deafened: bool,
    },
    RequestPermissions(u32),
    RequestBlobs {
        textures: Vec<u32>,
        comments: Vec<u32>,
        descriptions: Vec<u32>,
    },
    QueryUserNames(Vec<u32>),
    /// Register our certificate under our current name.
    RegisterSelf,
    /// A channel read up to `seq` (Nuschl servers; synced to the account's
    /// other devices).
    MarkRead {
        channel: u32,
        seq: u64,
    },
    /// A direct conversation read up to `seq`.
    ReadMarker {
        conversation: [u8; 16],
        seq: u64,
    },
    /// We are typing in this conversation (`typing/1`).
    Typing {
        conversation: [u8; 16],
    },
    /// Open (or find) the direct conversation with registered user `user_id`.
    OpenDirect {
        request_id: u64,
        user_id: u32,
    },
    /// Our access tokens (Mumble's `#token` groups), all of them.
    SetTokens(Vec<String>),
    /// A share link to `channel` (`share/1`): answered as a `ShareLink`.
    CreateShareLink {
        request_id: u64,
        channel: u32,
        expires_at: u64,
        access_token: String,
    },
    /// What a share link's token opens.
    ResolveShareLink {
        request_id: u64,
        token: [u8; 16],
    },
    /// Plugin data to these receivers, each a session and its presence
    /// instance, answered by its admission
    /// ([`ServerEvent::PluginDataAdmission`](crate::input::ServerEvent)). Only
    /// through [`Platform::request_metadata`], whose authority names the same
    /// presences ([`Client::send_plugin_data`](crate::Client::send_plugin_data)).
    PluginData {
        request: u64,
        receivers: Vec<(u32, u64)>,
        payload: nuschl_protocol::plugin::Payload,
    },
}

/// Where a conversation of a Nuschl server is kept in local history: the
/// history identity of the server and the conversation's storage key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey {
    pub identity: String,
    pub channel: String,
}

/// An edit or deletion we ask for, until the server answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRequest {
    pub server: String,
    pub conversation: [u8; 16],
    pub seq: u64,
    /// The new text of an edit; `None` deletes.
    pub content: Option<String>,
    pub cache: Option<CacheKey>,
    /// A deletion by someone who may moderate there: the server records it
    /// as a moderator's (also for their own messages).
    pub moderator: bool,
}

/// Untrusted Mumble HTML rendered by the content worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderKey {
    /// Transient server welcome; an empty source/document revokes its UI cache.
    Welcome,
    Description(u32),
    Comment(u32),
}

/// Request ids of the client's own extension requests (pages, edits,
/// deletions, direct conversations); sessions use ids with the top bit.
pub fn request_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub trait Platform {
    /// Connect (self-muted; `deafened` too), replacing any connection to
    /// this server; returns the connection's generation, which its events carry.
    fn connect(
        &self,
        bookmark: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String>;
    fn disconnect(&self, server: &str);
    fn request(&self, server: &str, request: Request) -> Result<(), String>;
    /// Automatic metadata: native implementations bind at enqueue/consume;
    /// source-ticket-only platforms retain their existing rendering guards.
    fn request_metadata(
        &self,
        server: &str,
        authority: nuschl_core::model::MetadataAuthority,
        request: Request,
    ) -> Result<(), String> {
        let _ = authority;
        self.request(server, request)
    }

    /// A page of a conversation's local history (`channel` is its storage key).
    fn local_history(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    ) -> Result<(), String>;
    /// A page of a Nuschl server's history (ids are seqs), kept in local
    /// history under `cache`.
    fn server_history(
        &self,
        server: String,
        conversation: [u8; 16],
        ticket: HistoryTicket,
        cache: Option<CacheKey>,
        page: Page,
    ) -> Result<(), String>;
    /// Retire earlier history pages for this server. This does not cancel
    /// sends, edits, deletions or cryptographic state. Adapters without a
    /// native page registry rely on the ticket checked by the client.
    fn cancel_history(&self, server: &str, epoch: u64) {
        let _ = (server, epoch);
    }
    fn change_message(&self, change: ChangeRequest) -> Result<(), String>;
    /// Render untrusted Mumble HTML in the content worker.
    fn render_html(&self, server: String, generation: u64, key: RenderKey, source: String);
    /// Decode an avatar in the content worker.
    fn decode_avatar(&self, server: String, generation: u64, hash: String, encoded: Vec<u8>);
    /// A sound effect on this server's connection.
    fn play_sound(&self, server: &str, sound: Sound);
    /// A sound effect where no connection plays it (after a disconnect).
    fn chime(&self, sound: Sound);
    /// Search the local history of these server identities, or of one
    /// conversation (storage key) of a single one.
    fn search(
        &self,
        request: u64,
        scope: (Vec<String>, Option<String>),
        query: String,
        after: Option<SearchCursor>,
    ) -> Result<(), String>;
    /// Whether every connection, now and after reconnects, decodes and
    /// reports plugin data. A platform without plugin data ignores it.
    fn accept_plugin_data(&self, accept: bool) {
        let _ = accept;
    }
}

impl<T: Platform + ?Sized> Platform for std::sync::Arc<T> {
    fn connect(
        &self,
        bookmark: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String> {
        (**self).connect(bookmark, password, deafened)
    }
    fn disconnect(&self, server: &str) {
        (**self).disconnect(server)
    }
    fn request(&self, server: &str, request: Request) -> Result<(), String> {
        (**self).request(server, request)
    }
    fn accept_plugin_data(&self, accept: bool) {
        (**self).accept_plugin_data(accept)
    }
    fn request_metadata(
        &self,
        server: &str,
        authority: nuschl_core::model::MetadataAuthority,
        request: Request,
    ) -> Result<(), String> {
        (**self).request_metadata(server, authority, request)
    }
    fn local_history(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    ) -> Result<(), String> {
        (**self).local_history(server, identity, channel, ticket, page)
    }
    fn server_history(
        &self,
        server: String,
        conversation: [u8; 16],
        ticket: HistoryTicket,
        cache: Option<CacheKey>,
        page: Page,
    ) -> Result<(), String> {
        (**self).server_history(server, conversation, ticket, cache, page)
    }
    fn cancel_history(&self, server: &str, epoch: u64) {
        (**self).cancel_history(server, epoch)
    }
    fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        (**self).change_message(change)
    }
    fn render_html(&self, server: String, generation: u64, key: RenderKey, source: String) {
        (**self).render_html(server, generation, key, source)
    }
    fn decode_avatar(&self, server: String, generation: u64, hash: String, encoded: Vec<u8>) {
        (**self).decode_avatar(server, generation, hash, encoded)
    }
    fn play_sound(&self, server: &str, sound: Sound) {
        (**self).play_sound(server, sound)
    }
    fn chime(&self, sound: Sound) {
        (**self).chime(sound)
    }
    fn search(
        &self,
        request: u64,
        scope: (Vec<String>, Option<String>),
        query: String,
        after: Option<SearchCursor>,
    ) -> Result<(), String> {
        (**self).search(request, scope, query, after)
    }
}
