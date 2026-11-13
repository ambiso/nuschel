// SPDX-License-Identifier: EUPL-1.2
//! One server as a client sees it: the connection's status, the people and
//! channels, permissions, what the Nuschl extension negotiated, direct
//! conversations, and the rules that follow from them (who may edit or
//! delete a message, where typing is said, which direct messages are kept by
//! the server).
use crate::{
    HistorySource, SessionSecurity,
    conversation::ConversationState,
    model::{
        Acl, Ban, ContextAction, RegisteredUser, ServerLimits, TextTarget, TransportStats, User,
        UserStats, permission as perm,
    },
    timeline::{Change, Message, WaitingChanges},
};
use nuschl_core::bookmark::ServerBookmark;
use nuschl_protocol::nuschl::v1;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
}

/// The server's certificate needs the user's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificatePrompt {
    pub sha256: String,
    pub changed: bool,
    pub reason: String,
}

/// What the media worker reports about the active audio route.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioInfo {
    pub inputs: Vec<nuschl_core::audio::AudioDevice>,
    pub outputs: Vec<nuschl_core::audio::AudioDevice>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub echo: nuschl_core::audio::EchoSource,
    /// A noise suppressor upstream of the microphone (Nuschl's own is off).
    pub noise: Option<String>,
    /// The output Nuschl's canceller records (`None`: its own playback only).
    pub reference: Option<String>,
    pub erle_db: Option<f32>,
    pub residual_echo: Option<f32>,
    pub health: AudioHealth,
}

/// The media worker's adaptive controller: what it measured and what it did
/// with Nuschl's own playback buffer and transmit gate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioHealth {
    pub clipping: bool,
    pub echo_guard: bool,
    pub double_talk: f32,
    pub underruns: u32,
    pub playback_ms: u32,
}

/// The smallest playback buffer the controller uses.
const NORMAL_PLAYBACK_MS: u32 = 40;

impl AudioHealth {
    /// (short label, advice) for each condition the user may want to act on.
    pub fn issues(&self) -> Vec<(&'static str, String)> {
        let mut issues = Vec::new();
        if self.clipping {
            issues.push((
                "Microphone is clipping",
                "Your microphone reaches its maximum level before Nuschl receives it, which distorts \
                 your voice. Lower the microphone's input level in the system sound settings."
                    .to_string(),
            ));
        }
        if self.echo_guard {
            issues.push((
                "Echo detected",
                "Some of what you hear keeps reaching your microphone. While others talk, Nuschl now \
                 needs clearer speech before it transmits. Headphones avoid echo entirely."
                    .to_string(),
            ));
        }
        if self.playback_ms > NORMAL_PLAYBACK_MS {
            issues.push((
                "Playback stuttered",
                format!(
                    "Audio output ran dry, so Nuschl now buffers {} ms of playback. It returns to \
                     normal after a minute without stutters.",
                    self.playback_ms
                ),
            ));
        }
        issues
    }
}

/// The last durably scoped history scan, distinct from live message maxima
/// and from the timeline's detached-search cursor.
#[derive(Clone, Debug)]
pub(crate) struct HistoryProgress {
    pub conversation: [u8; 16],
    pub through: i64,
    pub needed: bool,
    pub ready_epoch: Option<u64>,
    pub request: Option<crate::HistoryTicket>,
    pub response_end: Option<i64>,
    pub target_head: Option<i64>,
    pub batch_start: i64,
    pub resume_at_ms: Option<u64>,
    pub pages: u8,
    pub started_ms: Option<u64>,
    pub paused: bool,
}
impl HistoryProgress {
    pub fn new(conversation: [u8; 16], through: i64, epoch: u64) -> Self {
        Self {
            conversation,
            through,
            needed: false,
            ready_epoch: Some(epoch),
            request: None,
            response_end: None,
            target_head: None,
            batch_start: through,
            resume_at_ms: None,
            pages: 0,
            started_ms: None,
            paused: false,
        }
    }
    pub fn disconnected(&mut self) {
        self.needed = true;
        self.ready_epoch = None;
        self.request = None;
        self.response_end = None;
        self.target_head = None;
        self.batch_start = self.through;
        self.resume_at_ms = None;
        self.pages = 0;
        self.started_ms = None;
        self.paused = false;
    }
}

#[derive(Clone, Debug)]
pub struct ServerState {
    pub id: String,
    pub name: String,
    pub bookmark: ServerBookmark,
    pub history_source: HistorySource,
    pub security: SessionSecurity,
    /// An end-to-end server (`e2ee/1`, docs/e2ee.md §10): every conversation is an
    /// MLS group. Without it there is no end-to-end encryption, and the extension's
    /// extras are labelled "not end-to-end encrypted".
    pub mls: bool,
    pub encryption: BTreeMap<[u8; 16], crate::conversation::EncryptionState>,
    /// Channels in display order, then direct conversations.
    pub channels: Vec<ConversationState>,
    pub status: Status,
    /// The connection attempt events belong to; older ones' are dropped.
    pub generation: u64,
    /// History requests of a retired transport cannot update its successor.
    pub(crate) page_epoch: u64,
    pub(crate) history_progress: BTreeMap<u32, HistoryProgress>,
    pub peer_sha256: Option<String>,
    pub own_session: Option<u32>,
    pub users: BTreeMap<u32, User>,
    /// Channels this connection has seen (cached ones not among them go).
    pub seen_channels: BTreeSet<u32>,
    /// Our latest local microphone/privacy choice, initialized on connect.
    /// Server observations stay in `users` and cannot undo a newer choice.
    pub muted: bool,
    /// Our latest local playback choice. Always implies `muted`.
    pub deafened: bool,
    pub notice: Option<String>,
    pub(crate) notice_expires: Option<(String, u64)>,
    notice_revision: u64,
    send_refusal: Option<u64>,
    pub audio_status: String,
    /// Current connection's original HTML, used to validate worker renderings;
    /// UIs display its document, never this source or worker storage markers.
    pub welcome: Option<String>,
    pub certificate: Option<CertificatePrompt>,
    /// The server answered weaker than it did before (the sticky policy).
    pub downgrade: Option<String>,
    /// Acknowledgements (or refusals, `None`) of our messages that arrived
    /// before the messages themselves were shown.
    pub acked: BTreeMap<[u8; 16], Option<u64>>,
    /// What the Nuschl extension negotiated on this connection (empty on
    /// stock Mumble servers) and the server's policy.
    pub capabilities: Vec<String>,
    pub policy: Option<v1::Policy>,
    /// Our direct conversations on a Nuschl server, by id.
    pub directs: BTreeMap<[u8; 16], v1::Direct>,
    /// Edits and deletions of messages not in a timeline yet.
    pub waiting_changes: WaitingChanges,
    pub talking: BTreeSet<u32>,
    pub transport: Option<TransportStats>,
    pub audio: AudioInfo,
    pub permissions: BTreeMap<u32, u32>,
    /// Explicit UI requests only: channel lifetime, retry time, attempts.
    pub(crate) permission_queries: BTreeMap<u32, (u64, u64, u8)>,
    pub limits: Option<ServerLimits>,
    /// None until this connection actually receives CodecVersion.
    pub opus_available: Option<bool>,
    pub context_actions: Vec<ContextAction>,
    /// A server recommendation the user can act on (e.g. push-to-talk).
    pub suggestion: Option<String>,
    pub stats: BTreeMap<u32, UserStats>,
    /// Users heard speaking with CELT/Speex, which Nuschl cannot play.
    pub legacy_voice: BTreeSet<u32>,
    pub registered_users: Option<Vec<RegisteredUser>>,
    pub bans: Option<Vec<Ban>>,
    pub acl: Option<Acl>,
    /// Only correlated responses can initialize/validate an administrative draft.
    pub acl_reply: Option<nuschl_core::admin::AclReply>,
    pub account_reply: Option<nuschl_core::admin::AccountReply>,
    pub user_names: BTreeMap<u32, String>,
    pub(crate) acl_name_requests: BTreeSet<u32>,
    pub(crate) acl_name_budget: usize,
    pub reconnecting: Option<String>,
    /// Sent and total messages of the text waiting for the server's rate limit.
    pub outbox: Option<(usize, usize)>,
    /// Avatar texture hashes already asked for (or being decoded), and those
    /// decoded (a UI keeps the images).
    pub avatar_requests: BTreeSet<String>,
    pub avatars: BTreeSet<String>,
    /// Active whisper/shout target label while one is selected.
    pub whisper: Option<String>,
    /// Channel path from a `mumble://` link, joined once the tree has loaded.
    pub pending_channel: Vec<String>,
    /// A share link's token, resolved once connected (`share/1`).
    pub share_token: Option<String>,
    /// Branches opened or closed by hand this session, by channel id.
    pub expansion: BTreeMap<u32, bool>,
    /// Channel updates arrived since `channels` was last sorted.
    pub unsorted: bool,
}

impl ServerState {
    /// E2EE-only sessions need SFrame before voice can be enabled. They must
    /// never fall back to ordinary Mumble Opus because chat negotiated MLS.
    pub fn has_mumble_voice(&self) -> bool {
        !self
            .capabilities
            .iter()
            .any(|capability| capability == "e2ee/1")
    }

    /// A saved server, offline, with its cached channels to browse.
    pub fn new(bookmark: ServerBookmark) -> Self {
        let mut channels = bookmark
            .cached_channels
            .iter()
            .map(|channel| ConversationState {
                topic: "Saved channel history".into(),
                ..ConversationState::channel(
                    channel.id,
                    channel.parent,
                    channel.name.clone(),
                    channel.position,
                )
            })
            .collect::<Vec<_>>();
        if !channels.iter().any(|channel| channel.id == 0) {
            channels.insert(0, ConversationState::root());
        }
        Self {
            id: bookmark.id.clone(),
            name: bookmark.name.clone(),
            bookmark,
            history_source: HistorySource::LocalObserved,
            security: SessionSecurity::Unknown,
            mls: false,
            encryption: BTreeMap::new(),
            channels,
            status: Status::Disconnected,
            generation: 0,
            page_epoch: 0,
            history_progress: BTreeMap::new(),
            peer_sha256: None,
            own_session: None,
            users: BTreeMap::new(),
            seen_channels: BTreeSet::new(),
            muted: true,
            deafened: false,
            notice: None,
            notice_expires: None,
            notice_revision: 0,
            send_refusal: None,
            audio_status: "Audio disconnected".into(),
            welcome: None,
            certificate: None,
            downgrade: None,
            acked: BTreeMap::new(),
            capabilities: Vec::new(),
            policy: None,
            directs: BTreeMap::new(),
            waiting_changes: WaitingChanges::default(),
            talking: BTreeSet::new(),
            transport: None,
            audio: AudioInfo::default(),
            permissions: BTreeMap::new(),
            permission_queries: BTreeMap::new(),
            limits: None,
            opus_available: None,
            context_actions: Vec::new(),
            suggestion: None,
            stats: BTreeMap::new(),
            legacy_voice: BTreeSet::new(),
            registered_users: None,
            bans: None,
            acl: None,
            acl_reply: None,
            account_reply: None,
            user_names: BTreeMap::new(),
            acl_name_requests: BTreeSet::new(),
            acl_name_budget: 1024,
            reconnecting: None,
            outbox: None,
            avatar_requests: BTreeSet::new(),
            avatars: BTreeSet::new(),
            whisper: None,
            pending_channel: Vec::new(),
            share_token: None,
            expansion: BTreeMap::new(),
            unsorted: false,
        }
    }

    pub(crate) fn clear_acl_cache(&mut self) {
        self.permission_queries.clear();
        self.acl = None;
        self.acl_reply = None;
        self.account_reply = None;
        self.user_names.clear();
        self.acl_name_requests.clear();
        self.acl_name_budget = 1024;
    }

    /// Deduplicate explicit permission reads across menu invocations. No frame
    /// timer/retry loop: at most two attempts per unknown channel/cache epoch.
    /// The adapter must still send under its original connection/target lease.
    pub fn begin_permission_query(&mut self, channel: u32, now_ms: u64) -> bool {
        if self.status != Status::Connected || self.permissions.contains_key(&channel) {
            return false;
        }
        let Some(instance) = self
            .channels
            .iter()
            .find(|entry| entry.target == TextTarget::Channel(channel))
            .map(|entry| entry.instance)
        else {
            return false;
        };
        if instance == 0 {
            return false;
        }
        let query = self
            .permission_queries
            .entry(channel)
            .or_insert((instance, 0, 0));
        if query.0 != instance {
            *query = (instance, 0, 0);
        }
        if query.2 >= 2 || now_ms < query.1 {
            return false;
        }
        query.2 += 1;
        query.1 = now_ms.saturating_add(2000);
        true
    }

    pub fn status_label(&self) -> String {
        match self.status {
            Status::Disconnected => "Disconnected".into(),
            Status::Connecting => "Connecting…".into(),
            Status::Connected => match &self.transport {
                Some(stats) if stats.udp_active => {
                    format!("Connected · UDP · {:.0} ms", stats.udp.average_ms)
                }
                Some(stats) if stats.tcp.packets > 0 => format!(
                    "Connected · TCP{} · {:.0} ms",
                    if stats.forced_tcp { " (forced)" } else { "" },
                    stats.tcp.average_ms
                ),
                _ => "Connected".into(),
            },
        }
    }

    /// Effective permission bits for a channel, if the server has told us.
    pub fn may(&self, channel: u32, bits: u32) -> bool {
        self.permissions
            .get(&channel)
            .is_some_and(|permissions| permissions & bits == bits)
    }

    /// Like `may`, but optimistic while permissions are still unknown.
    pub fn may_try(&self, channel: u32, bits: u32) -> bool {
        self.permissions
            .get(&channel)
            .is_none_or(|permissions| permissions & bits == bits)
    }

    /// The conversation of channel `id`.
    pub fn channel_view(&mut self, id: u32) -> Option<&mut ConversationState> {
        self.channels
            .iter_mut()
            .find(|channel| channel.target == TextTarget::Channel(id))
    }

    /// The channel our own session is in.
    pub fn voice_channel(&self) -> Option<u32> {
        self.own_session
            .and_then(|session| self.users.get(&session))
            .map(|user| user.channel)
    }

    /// Where local history keeps this server's conversations: the configured
    /// CA origin, or its address and explicit certificate pin.
    pub fn history_identity(&self) -> Option<String> {
        if self.bookmark.certificate_sha256.is_none() {
            // Offline cache lookup is not permission to trust a network peer.
            return Some(nuschl_core::bookmark::webpki_history_identity(
                &self.bookmark.host,
                self.bookmark.port,
            ));
        }
        self.peer_sha256
            .as_ref()
            .or(self.bookmark.certificate_sha256.as_ref())
            .map(|pin| self.bookmark.history_identity(pin))
    }

    /// Whether the extension negotiated `capability` on this connection.
    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|have| have == capability)
    }

    /// Channel names from the root to `channel` (empty for the root), if
    /// every channel on the way is known.
    pub fn channel_path(&self, channel: u32) -> Option<Vec<String>> {
        let mut path = Vec::new();
        let mut current = channel;
        while current != 0 {
            let view = self
                .channels
                .iter()
                .find(|view| view.target == TextTarget::Channel(current))?;
            path.push(view.name.clone());
            current = view.parent?;
            if path.len() > 64 {
                return None;
            }
        }
        path.reverse();
        Some(path)
    }

    /// Our registration number, if we are registered.
    pub fn own_user_id(&self) -> Option<u32> {
        self.own_session
            .and_then(|session| self.users.get(&session))
            .and_then(|user| user.user_id)
    }

    /// Whether this conversation's timeline is the server's history (a
    /// Nuschl server's channel or direct conversation, connected), rather
    /// than what this device observed.
    pub fn archived(&self, channel: &ConversationState) -> bool {
        self.history_source == HistorySource::ServerArchive
            && channel.conversation.is_some()
            && !matches!(channel.target, TextTarget::Direct(_))
    }

    /// The conversation, if its messages can be changed now.
    pub fn changeable(&self, channel: &ConversationState) -> Option<[u8; 16]> {
        (!self.mls
            && self.status == Status::Connected
            && self.archived(channel)
            && self.has("history/1"))
        .then_some(channel.conversation)
        .flatten()
    }

    /// Whether we may edit this message: ours, stored, and we are registered.
    pub fn may_edit(&self, channel: &ConversationState, message: &Message) -> bool {
        let own = self.own_user_id();
        self.changeable(channel).is_some()
            && own.is_some()
            && message.ours(own)
            && message.deleted.is_none()
            && message.pending.is_none()
            && message.seq.is_some()
            && message.offer.is_none()
    }

    /// Whether we may delete this message: our own (registered), or as a
    /// moderator where the server allows it.
    pub fn may_delete(&self, channel: &ConversationState, message: &Message) -> bool {
        if self.changeable(channel).is_none()
            || message.deleted.is_some()
            || message.pending.is_some()
            || message.seq.is_none()
        {
            return false;
        }
        let own = self.own_user_id();
        (own.is_some() && message.ours(own)) || self.moderates(channel)
    }

    /// Moderators delete others' messages in channels they hold Write on,
    /// if the server's policy says so.
    pub fn moderates(&self, channel: &ConversationState) -> bool {
        self.policy
            .as_ref()
            .is_some_and(|policy| policy.moderators_delete)
            && matches!(channel.target, TextTarget::Channel(id) if self.may(id, perm::WRITE))
    }

    /// Apply `change` to message `seq` of `conversation`, or hand it back if
    /// that message is not in a timeline. Returns whether it was deleted.
    pub fn change_message(
        &mut self,
        conversation: [u8; 16],
        seq: u64,
        change: Change,
    ) -> Result<bool, Change> {
        match self
            .channels
            .iter_mut()
            .find(|channel| channel.conversation == Some(conversation))
        {
            Some(channel) => channel.timeline.apply_change(seq, change),
            None => Err(change),
        }
    }

    /// Our message was acknowledged as `seq` (or refused): returns whether a
    /// timeline shows it yet.
    pub fn settle_pending(&mut self, key: [u8; 16], seq: Option<u64>) -> bool {
        self.channels
            .iter_mut()
            .any(|channel| channel.timeline.settle(key, seq))
    }

    /// Text still waiting for the rate limit when the connection ends is
    /// lost: what to tell the user.
    pub fn lost_messages(&mut self) -> Option<String> {
        let (sent, total) = self.outbox.take()?;
        Some(match total - sent {
            1 => "1 message waiting to be sent was not sent.".into(),
            lost => format!("{lost} messages waiting to be sent were not sent."),
        })
    }

    /// The conversation to say we type in, if this one gets indicators
    /// (`typing/1`): one the server keeps and we may write in.
    pub fn typing_conversation(&self, channel: &ConversationState) -> Option<[u8; 16]> {
        if !self.has("typing/1") || !self.archived(channel) {
            return None;
        }
        // The server forwards it only from those who may write there.
        if let TextTarget::Channel(id) = channel.target
            && !self.may_try(id, perm::TEXT_MESSAGE)
        {
            return None;
        }
        channel.conversation
    }

    /// Someone (not us, and known here) types in `conversation`. At most 64
    /// people are remembered typing in one conversation.
    pub fn someone_typing(&mut self, conversation: &[u8], session: u32, now_ms: u64) {
        if self.own_session == Some(session) || !self.users.contains_key(&session) {
            return;
        }
        if let Some(channel) = self
            .channels
            .iter_mut()
            .find(|channel| channel.conversation.is_some_and(|id| id == conversation))
            && (channel.typing.len() < 64 || channel.typing.contains_key(&session))
        {
            channel.typing.insert(session, now_ms);
        }
    }

    /// The line under a composer: who is typing in `channel` now, or nothing.
    pub fn typing_line(
        &self,
        channel: &ConversationState,
        names: &(impl crate::names::Names + ?Sized),
        now_ms: u64,
    ) -> Option<String> {
        let mut shown: Vec<String> = channel
            .typing_now(now_ms)
            .filter(|session| Some(*session) != self.own_session)
            .filter_map(|session| self.users.get(&session))
            .map(|user| crate::names::display_name(user, names).to_string())
            .collect();
        shown.sort_by_key(|name| name.to_lowercase());
        match shown.as_slice() {
            [] => None,
            [one] => Some(format!("{one} is typing…")),
            [one, two] => Some(format!("{one} and {two} are typing…")),
            _ => Some("Several people are typing…".into()),
        }
    }

    /// Whether direct messages with `peer` go to a durable conversation:
    /// between two registered accounts on a Nuschl server with history.
    pub fn durable_directs_with(&self, peer: &User) -> bool {
        self.history_source == HistorySource::ServerArchive
            && self.has("history/1")
            && self.own_user_id().is_some()
            // 0 is SuperUser, and also what the server says for a deleted
            // account: never matched to a conversation.
            && peer.user_id.is_some_and(|id| id != 0)
    }

    /// The direct conversation with registered user `user_id`, if known.
    pub fn direct_with(&self, user_id: u32) -> Option<[u8; 16]> {
        self.directs
            .iter()
            .find(|(_, direct)| user_id != 0 && direct.user_id == user_id)
            .map(|(id, _)| *id)
    }

    /// The online session of registered user `user_id`.
    pub fn online(&self, user_id: u32) -> Option<&User> {
        self.users
            .values()
            .find(|user| user_id != 0 && user.user_id == Some(user_id))
    }

    /// A free id for a new direct conversation (counting down from `u32::MAX`).
    fn direct_id(&self) -> u32 {
        let mut id = u32::MAX;
        while self.channels.iter().any(|channel| channel.id == id) {
            id -= 1;
        }
        id
    }

    /// The timeline of durable direct conversation `id`, created if needed
    /// and named `name`; returns its index in `channels`.
    pub fn direct_view(&mut self, id: [u8; 16], name: &str, peer: Option<User>) -> usize {
        let key = direct_conversation_key(&id);
        if let Some(index) = self
            .channels
            .iter()
            .position(|channel| channel.conversation == Some(id) || channel.storage_key == key)
        {
            let channel = &mut self.channels[index];
            channel.conversation = Some(id);
            channel.encryption = self.encryption.get(&id).cloned();
            channel.target = TextTarget::DirectConversation(id);
            if peer.is_some() {
                channel.direct_peer = peer;
            }
            return index;
        }
        let mut channel = ConversationState::direct(
            self.direct_id(),
            name,
            TextTarget::DirectConversation(id),
            key,
        );
        channel.direct_peer = peer;
        channel.conversation = Some(id);
        channel.encryption = self.encryption.get(&id).cloned();
        self.channels.push(channel);
        self.channels.len() - 1
    }

    /// The timeline of direct messages with `peer` over the stock path (a
    /// `TextMessage` to their session), created if needed and named `name`;
    /// returns its id.
    pub fn stock_direct_view(&mut self, peer: &User, name: &str) -> u32 {
        let session = peer.session;
        let key = peer
            .certificate_hash
            .as_ref()
            .map(|hash| format!("direct:{hash}"))
            .unwrap_or_else(|| {
                format!(
                    "direct:pending:{}:{session}:{}",
                    self.generation, peer.instance
                )
            });
        if let Some(channel) = self
            .channels
            .iter_mut()
            .find(|channel| channel.storage_key == key)
        {
            channel.target = TextTarget::Direct(session);
            channel.direct_peer = Some(peer.clone());
            channel.id
        } else {
            let id = self.direct_id();
            let mut channel = ConversationState::direct(id, name, TextTarget::Direct(session), key);
            channel.direct_peer = Some(peer.clone());
            self.channels.push(channel);
            id
        }
    }

    /// Refresh a legacy private conversation from current presence. Only a
    /// certificate identity, or the very same live presence, can retarget it;
    /// names and recycled session/account numbers are not sufficient.
    pub fn refresh_direct(&mut self, position: usize) -> bool {
        let Some(channel) = self.channels.get(position) else {
            return false;
        };
        if !matches!(channel.target, TextTarget::Direct(_)) {
            return false;
        }
        let previous = channel.direct_peer.as_ref();
        let hash = previous
            .and_then(|peer| peer.certificate_hash.as_deref())
            .filter(|hash| !hash.is_empty())
            .or_else(|| {
                channel
                    .storage_key
                    .strip_prefix("direct:")
                    .filter(|key| !key.starts_with("pending:") && !key.is_empty())
            });
        let matches = |peer: &&User| match hash {
            Some(hash) => peer
                .certificate_hash
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(hash)),
            None => previous
                .is_some_and(|old| old.session == peer.session && old.instance == peer.instance),
        };
        let peer = previous
            .and_then(|old| self.users.get(&old.session))
            .filter(matches)
            .or_else(|| self.users.values().find(matches))
            .cloned();
        let Some(peer) = peer else {
            return false;
        };
        let channel = &mut self.channels[position];
        channel.target = TextTarget::Direct(peer.session);
        if channel.storage_key.starts_with("direct:pending:")
            && let Some(hash) = peer
                .certificate_hash
                .as_ref()
                .filter(|hash| !hash.is_empty())
        {
            channel.storage_key = format!("direct:{hash}");
        }
        channel.direct_peer = Some(peer);
        if self.notice.as_deref() == Some(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE) {
            self.dismiss_notice();
        }
        true
    }

    /// Resolve at the moment of sending, including batches whose image worker
    /// took time. Keep the session's final presence-token check in place too.
    pub fn message_target(
        &mut self,
        position: usize,
    ) -> Result<(TextTarget, Option<u64>), &'static str> {
        if position >= self.channels.len() {
            return Err("This conversation is no longer available.");
        }
        if matches!(self.channels[position].target, TextTarget::Direct(_))
            && !self.refresh_direct(position)
        {
            return Err(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE);
        }
        let channel = &self.channels[position];
        Ok((
            channel.target.clone(),
            channel.direct_peer.as_ref().map(|peer| peer.instance),
        ))
    }

    /// Replace a notice, invalidating any previous send-refusal lease even
    /// when the new text is identical. Frontends may read `notice` directly;
    /// production writers use these helpers to preserve its lifetime.
    pub fn set_notice(&mut self, notice: String) {
        self.notice_revision = self.notice_revision.wrapping_add(1);
        self.notice = Some(notice);
        self.notice_expires = None;
        self.send_refusal = None;
    }

    pub(crate) fn refuse_send(&mut self, reason: String, now_ms: u64) -> String {
        self.set_notice(reason.clone());
        self.notice_expires = Some((reason.clone(), now_ms.saturating_add(10_000)));
        self.send_refusal = Some(self.notice_revision);
        reason
    }

    pub(crate) fn send_refusal_lease(&self) -> Option<u64> {
        self.send_refusal
    }

    pub(crate) fn clear_sent_refusal(&mut self, lease: Option<u64>) {
        if lease.is_some() && lease == self.send_refusal && lease == Some(self.notice_revision) {
            self.dismiss_notice();
        }
    }

    pub fn dismiss_notice(&mut self) {
        self.notice_revision = self.notice_revision.wrapping_add(1);
        self.notice = None;
        self.notice_expires = None;
        self.send_refusal = None;
    }

    pub fn has_temporary_notice(&self) -> bool {
        self.notice_expires.is_some()
    }

    /// A stock direct message arrived for a conversation not shown yet
    /// (`storage_key` as local history keeps it): its new timeline's index.
    pub fn received_direct(
        &mut self,
        name: &str,
        target: TextTarget,
        peer: Option<User>,
        storage_key: String,
    ) -> usize {
        let mut channel = ConversationState::direct(self.direct_id(), name, target, storage_key);
        channel.direct_peer = peer;
        self.channels.push(channel);
        self.channels.len() - 1
    }
}

/// The local storage key of a direct conversation on a Nuschl server.
pub fn direct_conversation_key(conversation: &[u8; 16]) -> String {
    let hex: String = conversation
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("conversation:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{Deletion, IncomingMessage};
    use nuschl_core::{ChatMessage, MessageDocument};

    fn message(id: i64, author_id: Option<u32>, outgoing: bool) -> Message {
        let mut rendered = IncomingMessage::plain(
            Some(id),
            ChatMessage {
                server: String::new(),
                channel: String::new(),
                author: "Ada".into(),
                observed_at_ms: id,
                body: "hello".into(),
            },
            MessageDocument { blocks: Vec::new() },
            outgoing,
        );
        rendered.author_id = author_id;
        rendered.seq = Some(id as u64);
        rendered.into()
    }

    pub(crate) fn test_server() -> ServerState {
        ServerState::new(
            ServerBookmark::new("Test", "localhost", 64738, "andi").expect("valid bookmark"),
        )
    }

    fn server(
        registered: bool,
        moderators_delete: bool,
        write: bool,
    ) -> (ServerState, ConversationState) {
        let mut server = test_server();
        server.status = Status::Connected;
        server.history_source = HistorySource::ServerArchive;
        server.capabilities = vec!["history/1".into()];
        server.own_session = Some(1);
        server.users.insert(
            1,
            User {
                session: 1,
                user_id: registered.then_some(7),
                ..Default::default()
            },
        );
        server.policy = Some(v1::Policy {
            moderators_delete,
            ..Default::default()
        });
        server
            .permissions
            .insert(3, if write { perm::WRITE } else { 0 });
        let channel = ConversationState {
            conversation: Some([1; 16]),
            ..ConversationState::channel(3, Some(0), "Three".into(), 0)
        };
        (server, channel)
    }

    #[test]
    fn authors_edit_and_delete_their_own_messages_only_when_registered() {
        let (server, channel) = server(true, false, false);
        let mine = message(5, Some(7), false);
        let sent_here = message(6, None, true);
        let theirs = message(7, Some(8), false);
        assert!(server.may_edit(&channel, &mine) && server.may_delete(&channel, &mine));
        assert!(server.may_edit(&channel, &sent_here));
        assert!(!server.may_edit(&channel, &theirs) && !server.may_delete(&channel, &theirs));
        let (guest, channel) = self::server(false, false, false);
        assert!(!guest.may_edit(&channel, &sent_here) && !guest.may_delete(&channel, &sent_here));
    }

    #[test]
    fn moderators_delete_others_messages_only_where_the_server_allows_it() {
        let theirs = message(7, Some(8), false);
        for (allowed, write, expected) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
        ] {
            let (server, channel) = server(true, allowed, write);
            assert_eq!(server.may_delete(&channel, &theirs), expected);
            assert!(
                !server.may_edit(&channel, &theirs),
                "nobody edits others' messages"
            );
        }
    }

    #[test]
    fn changes_find_their_conversation_or_wait() {
        let (mut server, mut channel) = server(true, false, false);
        channel.timeline.messages.push(message(5, Some(8), false));
        server.channels.push(channel);
        let edit = |text: &str, at| Change::Edited {
            source: text.into(),
            document: MessageDocument { blocks: Vec::new() },
            edited_ms: at,
        };
        assert_eq!(
            server.change_message([1; 16], 5, edit("newer", 20)),
            Ok(false)
        );
        assert!(
            server
                .change_message([1; 16], 6, edit("later", 30))
                .is_err()
        );
        assert!(
            server
                .change_message([2; 16], 5, edit("elsewhere", 30))
                .is_err()
        );
        let deleted = Change::Deleted(Deletion {
            at_ms: 40,
            moderator: true,
        });
        assert_eq!(server.change_message([1; 16], 5, deleted), Ok(true));
    }

    #[test]
    fn nothing_changes_on_stock_servers_offline_or_in_tombstones() {
        let mine = message(5, Some(7), false);
        let (mut stock, channel) = server(true, true, true);
        stock.capabilities.clear();
        stock.history_source = HistorySource::LocalObserved;
        assert!(!stock.may_edit(&channel, &mine) && !stock.may_delete(&channel, &mine));
        let (mut offline, channel) = server(true, true, true);
        offline.status = Status::Disconnected;
        assert!(!offline.may_edit(&channel, &mine));
        let (server, channel) = server(true, true, true);
        let mut gone = message(5, Some(7), false);
        gone.deleted = Some(Deletion {
            at_ms: 1,
            moderator: false,
        });
        assert!(!server.may_edit(&channel, &gone) && !server.may_delete(&channel, &gone));
        let mut pending = message(5, None, true);
        pending.pending = Some([1; 16]);
        assert!(!server.may_edit(&channel, &pending));
    }

    #[test]
    fn typing_is_said_only_where_the_server_keeps_the_conversation_and_we_may_write() {
        let (mut server, channel) = server(true, false, false);
        assert_eq!(server.typing_conversation(&channel), None, "no typing/1");
        server.capabilities.push("typing/1".into());
        server.permissions.insert(3, perm::TEXT_MESSAGE);
        assert_eq!(server.typing_conversation(&channel), Some([1; 16]));
        server.permissions.insert(3, perm::WRITE);
        assert_eq!(
            server.typing_conversation(&channel),
            None,
            "no Text message"
        );
        server.channels.push(channel);
        server.users.insert(
            2,
            User {
                session: 2,
                ..Default::default()
            },
        );
        server.someone_typing(&[1; 16], 2, 100);
        server.someone_typing(&[1; 16], 1, 100);
        server.someone_typing(&[1; 16], 9, 100);
        let typing = &server.channels.last().unwrap().typing;
        assert_eq!(typing.keys().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn the_typing_line_names_one_or_two_people_and_counts_more() {
        use crate::{conversation::TYPING_SHOWN_MS, names::Preferences};
        let mut server = test_server();
        server.own_session = Some(1);
        for (session, name) in [(1, "Me"), (2, "Bob"), (3, "ada"), (4, "Cy")] {
            server.users.insert(
                session,
                User {
                    session,
                    name: name.into(),
                    ..Default::default()
                },
            );
        }
        let names = Preferences::default();
        let now = 100_000;
        let mut channel = ConversationState::default();
        assert_eq!(server.typing_line(&channel, &names, now), None);
        // Our own session never shows, and an old indicator is over.
        channel.typing.insert(1, now);
        channel.typing.insert(4, now - TYPING_SHOWN_MS - 1);
        assert_eq!(server.typing_line(&channel, &names, now), None);
        channel.typing.insert(2, now);
        assert_eq!(
            server.typing_line(&channel, &names, now).as_deref(),
            Some("Bob is typing…")
        );
        channel.typing.insert(3, now);
        assert_eq!(
            server.typing_line(&channel, &names, now).as_deref(),
            Some("ada and Bob are typing…")
        );
        channel.typing.insert(4, now);
        assert_eq!(
            server.typing_line(&channel, &names, now).as_deref(),
            Some("Several people are typing…")
        );
    }

    #[test]
    fn direct_conversations_are_found_by_id_or_storage_key() {
        let (mut server, _) = server(true, false, false);
        let peer = User {
            session: 4,
            user_id: Some(9),
            certificate_hash: Some("abc".into()),
            ..Default::default()
        };
        assert!(server.durable_directs_with(&peer));
        let first = server.direct_view([5; 16], "Bob", Some(peer.clone()));
        assert_eq!(server.channels[first].name, "@ Bob");
        assert_eq!(server.channels[first].id, u32::MAX);
        assert_eq!(server.direct_view([5; 16], "Robert", None), first);
        let stock = server.stock_direct_view(&peer, "Bob");
        assert_eq!(stock, u32::MAX - 1);
        assert_eq!(server.stock_direct_view(&peer, "Bob"), stock);
        let channel = server.channels.iter().find(|channel| channel.id == stock);
        assert_eq!(
            channel.map(|channel| channel.storage_key.as_str()),
            Some("direct:abc")
        );
    }

    #[test]
    fn a_private_conversation_follows_the_same_certificate_but_not_a_reused_session() {
        let (mut server, _) = server(false, false, false);
        let old = User {
            session: 4,
            instance: 10,
            certificate_hash: Some("abc".into()),
            ..Default::default()
        };
        let id = server.stock_direct_view(&old, "Bob");
        let position = server.channels.iter().position(|c| c.id == id).unwrap();
        // Someone else takes the old session number, even using Bob's name.
        server.users.insert(
            4,
            User {
                session: 4,
                instance: 11,
                name: "Bob".into(),
                certificate_hash: Some("other".into()),
                ..Default::default()
            },
        );
        assert!(server.message_target(position).is_err());
        // Bob is still online under a different session; sending finds him.
        server.users.insert(
            9,
            User {
                session: 9,
                instance: 12,
                certificate_hash: Some("abc".into()),
                ..Default::default()
            },
        );
        server.set_notice(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE.into());
        assert_eq!(
            server.message_target(position),
            Ok((TextTarget::Direct(9), Some(12)))
        );
        assert!(server.notice.is_none());
        assert_eq!(server.channels[position].id, id);
        assert_eq!(server.channels[position].storage_key, "direct:abc");
    }

    #[test]
    fn an_anonymous_private_recipient_must_keep_the_same_presence() {
        let (mut server, _) = server(false, false, false);
        let old = User {
            session: 4,
            instance: 10,
            name: "Bob".into(),
            ..Default::default()
        };
        let id = server.stock_direct_view(&old, "Bob");
        let position = server.channels.iter().position(|c| c.id == id).unwrap();
        server.users.insert(4, old.clone());
        assert_eq!(
            server.message_target(position),
            Ok((TextTarget::Direct(4), Some(10)))
        );
        server.users.insert(
            4,
            User {
                instance: 11,
                ..old.clone()
            },
        );
        assert!(server.message_target(position).is_err());
        // Learning the certificate of the original live presence promotes its
        // history key, without making the user open another conversation.
        server.users.insert(
            4,
            User {
                certificate_hash: Some("abc".into()),
                ..old
            },
        );
        assert_eq!(
            server.message_target(position),
            Ok((TextTarget::Direct(4), Some(10)))
        );
        assert_eq!(server.channels[position].storage_key, "direct:abc");
    }
}
