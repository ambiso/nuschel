// SPDX-License-Identifier: EUPL-1.2
//! What a platform reports to the client: connection lifecycle, what each
//! server says, rendered messages and history pages, audio. The client
//! applies these in order ([`Client::handle`](crate::client::Client::handle)).
use crate::{
    HistoryTicket,
    model::{
        Acl, Activity, Ban, Channel, ContextAction, RegisteredUser, RejectKind, ServerLimits,
        TextTarget, TransportStats, User, UserStats,
    },
    platform::RenderKey,
    server::AudioHealth,
    timeline::{Deletion, IncomingMessage, Page},
};
use nuschel_core::{
    MessageDocument, SearchPage,
    audio::{AudioDevice, EchoSource},
};
use nuschel_protocol::nuschel::v1;
use std::collections::BTreeMap;

/// Something a platform reports. Those of one connection carry its
/// generation; the client drops what an older connection reports.
// Inputs move once, from the platform to the client: boxing the large ones
// (a message, a user) would allocate for each of them instead.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Input {
    /// What a server said on this connection.
    Server {
        server: String,
        generation: u64,
        event: ServerEvent,
    },
    /// The server's certificate needs the user's decision.
    Certificate {
        server: String,
        generation: u64,
        sha256: String,
        changed: bool,
        reason: String,
    },
    Disconnected {
        server: String,
        generation: u64,
        reason: String,
        /// Set when the server refused the login, so a UI can ask for a
        /// password or another name.
        reject: Option<RejectKind>,
    },
    /// The connection dropped; a new attempt starts after `delay_secs`.
    Reconnecting {
        server: String,
        generation: u64,
        attempt: u32,
        delay_secs: u64,
        reason: String,
    },
    /// We were not connected between these times; history may miss messages.
    Gap {
        server: String,
        generation: u64,
        from_ms: i64,
        to_ms: i64,
    },
    /// The server offers less than it did before (the sticky policy).
    Downgrade {
        server: String,
        generation: u64,
        reason: String,
    },
    /// A message, rendered and kept in local history.
    Message {
        server: String,
        generation: u64,
        target: TextTarget,
        storage_key: String,
        peer: Option<User>,
        message: IncomingMessage,
    },
    /// Our text left for the server.
    Sent {
        server: String,
        generation: u64,
        target: TextTarget,
        body: String,
    },
    /// A failed encrypted send keeps the composer's text and releases its pending state.
    SendFailed {
        server: String,
        generation: u64,
        reason: String,
    },
    /// A page read backwards; `before` is None for the latest.
    History {
        server: String,
        ticket: HistoryTicket,
        before: Option<i64>,
        messages: Vec<IncomingMessage>,
        next_before: Option<i64>,
    },
    /// A page read around or after a message.
    HistoryWindow {
        server: String,
        ticket: HistoryTicket,
        page: Page,
        messages: Vec<IncomingMessage>,
        next_before: Option<i64>,
        next_after: Option<i64>,
    },
    /// A server history page could not be loaded.
    HistoryFailed {
        server: String,
        ticket: HistoryTicket,
        reason: String,
    },
    /// Local history failed.
    HistoryError(String),
    /// History writes work again after an earlier failure.
    HistoryRecovered,
    /// A message on a Nuschel server was edited (by its author, perhaps us).
    Edited {
        server: String,
        conversation: [u8; 16],
        seq: u64,
        source: String,
        document: MessageDocument,
        edited_ms: i64,
    },
    /// A message on a Nuschel server became a tombstone.
    Deleted {
        server: String,
        conversation: [u8; 16],
        seq: u64,
        deletion: Deletion,
    },
    /// The server refused our edit or deletion.
    ChangeFailed {
        server: String,
        delete: bool,
        reason: String,
    },
    /// Results for search `request`.
    SearchResults {
        request: u64,
        result: Result<SearchPage, String>,
    },
    /// Inert rendering of untrusted Mumble HTML (descriptions, comments).
    Rendered {
        server: String,
        generation: u64,
        key: RenderKey,
        source: String,
        document: MessageDocument,
    },
    /// A decoded avatar for a texture hash.
    Avatar {
        server: String,
        generation: u64,
        hash: String,
        document: MessageDocument,
    },
    /// Voice activity of a session (ours too).
    Talking {
        server: String,
        generation: u64,
        session: u32,
        talking: bool,
    },
    AudioStatus {
        server: String,
        generation: u64,
        message: String,
    },
    AudioDevices {
        server: String,
        generation: u64,
        inputs: Vec<AudioDevice>,
        outputs: Vec<AudioDevice>,
        input: Option<String>,
        output: Option<String>,
        echo: EchoSource,
        noise: Option<String>,
        reference: Option<String>,
    },
    Echo {
        server: String,
        generation: u64,
        erle_db: Option<f32>,
        residual_likelihood: Option<f32>,
    },
    /// The media worker's adaptive controller changed its view of the graph.
    AudioHealth {
        server: String,
        generation: u64,
        health: AudioHealth,
    },
}

/// What a server said (the session's events the client models).
#[derive(Clone, Debug)]
pub enum ServerEvent {
    /// An authenticated encryption failure is an activity gap, never stored text.
    EncryptionGap {
        conversation: [u8; 16],
        reason: String,
        at_ms: i64,
    },
    Encryption {
        conversation: [u8; 16],
        state: crate::conversation::EncryptionState,
    },
    /// The server's certificate fingerprint on this connection.
    PeerCertificate(String),
    /// The Nuschel extension's negotiation (only with Nuschel servers).
    Negotiated {
        capabilities: Vec<String>,
        policy: Option<v1::Policy>,
    },
    /// A channel's conversation on a Nuschel server.
    ChannelInfo {
        channel: u32,
        conversation: Option<[u8; 16]>,
    },
    /// The server stored our message as `seq`.
    ChatAcked {
        key: [u8; 16],
        seq: u64,
    },
    /// The server refused our message.
    ChatFailed {
        key: [u8; 16],
        reason: String,
    },
    /// A channel's newest seq and our read marker.
    ConversationState {
        channel: u32,
        head: u64,
        read: u64,
    },
    Typing(v1::Typing),
    DirectList(Vec<v1::Direct>),
    DirectOpened(v1::DirectOpened),
    /// A share link created or resolved (`share/1`).
    ShareLink(v1::ShareLink),
    /// An extension request of ours failed.
    ExtensionError(v1::Error),
    /// Read on our account's other device.
    ReadMarker(v1::ReadMarker),
    Connected {
        session: u32,
    },
    Channel(Channel),
    ChannelRemoved(u32),
    User(User),
    UserRemoved(u32),
    Permissions {
        channel: u32,
        permissions: u32,
        flush: bool,
    },
    Activity(Activity),
    Limits(ServerLimits),
    Stats(UserStats),
    Suggest {
        push_to_talk: Option<bool>,
        minimum_version: Option<String>,
    },
    ContextActions(Vec<ContextAction>),
    RegisteredUsers(Vec<RegisteredUser>),
    Bans(Vec<Ban>),
    Acl(Acl),
    UserNames(BTreeMap<u32, String>),
    /// Someone speaks with a codec Nuschel cannot play (seen at `at_ms`).
    UnsupportedCodec {
        session: u32,
        at_ms: i64,
    },
    Outbox {
        sent: usize,
        total: usize,
    },
    Notice(String),
    PermissionDenied(String),
    Welcome(String),
    Transport(TransportStats),
}
