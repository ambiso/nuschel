// SPDX-License-Identifier: EUPL-1.2
//! Control messages from authenticated clients, with stock Murmur's semantics
//! (`Messages.cpp`): the same permission checks, in the same order, with the
//! same `PermissionDenied` types, and the same broadcasts.
use crate::{
    acl::{self, AclEntry, Group, perm},
    authz::Action,
    conversations::{Scope, hex},
    state::{Ban, Channel, Registered, State, WhisperTarget, mapped, now_unix, random},
    text,
};
use bytes::Bytes;
use nuschel_ipc::server::ServerStorageRequest as Storage;
use nuschel_protocol::{ControlFrame, control as proto, kind};
use proto::permission_denied::DenyType;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv6Addr},
    sync::atomic::Ordering,
    time::Instant,
};

/// Entries the ACL editor may store per channel, and their sizes.
const MAX_ACL_ENTRIES: usize = 512;
const MAX_GROUPS: usize = 512;
const MAX_GROUP_MEMBERS: usize = 8192;
const MAX_NAME_BYTES: usize = 512;
const MAX_BANS: usize = 65_536;
const MAX_VOICE_TARGET_ENTRIES: usize = 512;
/// Plugin data limits (Mumble's `PluginMessage::MAX_DATA_LENGTH`, `MAX_DATA_ID_LENGTH`).
const MAX_PLUGIN_DATA: usize = 1000;
const MAX_PLUGIN_DATA_ID: usize = 100;

fn decode<M: prost::Message + Default>(payload: &Bytes) -> Option<M> {
    M::decode(payload.clone()).ok()
}

impl State {
    /// Every control message from an authenticated client passes through here:
    /// the one place that decides which types are accepted after
    /// authentication and applies the flood limit, before any handler runs.
    /// Handlers then ask [`State::authorize`] (authz.rs) for every permission.
    pub fn handle(
        &mut self,
        session: u32,
        frame: ControlFrame,
        out: &mut crate::routing::Datagrams,
    ) {
        let now = Instant::now();
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        if frame.kind != kind::UDP_TUNNEL {
            self.counters
                .control_messages
                .fetch_add(1, Ordering::Relaxed);
            self.counters
                .control_bytes
                .fetch_add(frame.payload.len() as u64 + 6, Ordering::Relaxed);
        }
        if frame.kind != kind::UDP_TUNNEL && frame.kind != kind::PING && user.flood.limited(now) {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Murmur's MSG_SETUP: these count as activity for the idle time.
        if !matches!(
            frame.kind,
            kind::UDP_TUNNEL
                | kind::PING
                | kind::CRYPT_SETUP
                | kind::VOICE_TARGET
                | kind::PERMISSION_QUERY
                | kind::USER_STATS
                | kind::REQUEST_BLOB
                | kind::PLUGIN_DATA_TRANSMISSION
        ) {
            user.voice.bandwidth.idle_since = now;
        }
        match frame.kind {
            kind::UDP_TUNNEL => self.voice_from_tcp(session, &frame.payload, out),
            kind::VERSION => {
                if let Some(message) = decode::<proto::Version>(&frame.payload)
                    && !self.rate_limited(session)
                    && let Some(user) = self.users.get_mut(&session)
                {
                    user.info.apply(&message);
                }
            }
            kind::AUTHENTICATE => {
                if let Some(message) = decode(&frame.payload) {
                    self.update_tokens(session, message);
                }
            }
            kind::PING => {
                if let Some(message) = decode(&frame.payload) {
                    self.ping(session, message);
                }
            }
            kind::CHANNEL_REMOVE => {
                if let Some(message) = decode::<proto::ChannelRemove>(&frame.payload) {
                    self.channel_remove(session, message.channel_id);
                }
            }
            kind::CHANNEL_STATE => {
                if let Some(message) = decode(&frame.payload) {
                    self.channel_state_message(session, message);
                }
            }
            kind::USER_REMOVE => {
                if let Some(message) = decode(&frame.payload) {
                    self.user_remove(session, message);
                }
            }
            kind::USER_STATE => {
                if let Some(message) = decode(&frame.payload) {
                    self.user_state(session, message);
                }
            }
            kind::BAN_LIST => {
                if let Some(message) = decode(&frame.payload) {
                    self.ban_list(session, message);
                }
            }
            kind::TEXT_MESSAGE => {
                if let Some(message) = decode(&frame.payload) {
                    self.text_message(session, message);
                }
            }
            kind::ACL => {
                if let Some(message) = decode(&frame.payload) {
                    self.acl(session, message);
                }
            }
            kind::QUERY_USERS => {
                if let Some(message) = decode(&frame.payload) {
                    self.query_users(session, message);
                }
            }
            kind::CRYPT_SETUP => {
                if let Some(message) = decode(&frame.payload) {
                    self.crypt_setup(session, message);
                }
            }
            kind::USER_LIST => {
                if let Some(message) = decode(&frame.payload) {
                    self.user_list(session, message);
                }
            }
            kind::VOICE_TARGET => {
                if let Some(message) = decode(&frame.payload) {
                    self.voice_target(session, message);
                }
            }
            kind::PERMISSION_QUERY => {
                if let Some(message) = decode::<proto::PermissionQuery>(&frame.payload)
                    && !self.query_limited(session)
                    && let Some(channel) = message.channel_id
                    && self.channels.get(channel).is_some()
                {
                    self.send_permissions(session, channel, true);
                }
            }
            kind::USER_STATS => {
                if let Some(message) = decode(&frame.payload) {
                    self.user_stats(session, message);
                }
            }
            kind::REQUEST_BLOB => {
                if let Some(message) = decode(&frame.payload) {
                    self.request_blob(session, message);
                }
            }
            kind::PLUGIN_DATA_TRANSMISSION => {
                if let Some(message) = decode(&frame.payload) {
                    self.plugin_data(session, message);
                }
            }
            // Server-to-client messages and context actions (no RPC interface).
            kind::REJECT
            | kind::SERVER_SYNC
            | kind::PERMISSION_DENIED
            | kind::CONTEXT_ACTION_MODIFY
            | kind::CONTEXT_ACTION
            | kind::CODEC_VERSION
            | kind::SERVER_CONFIG
            | kind::SUGGEST_CONFIG => {}
            other if kind::NUSCHEL_EXTENSION.contains(&other) => {
                self.extension_frame(session, &frame);
            }
            _ => {
                self.counters.unknown_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.finish();
    }

    /// Murmur's `RATELIMIT`: the `messagelimit`/`messageburst` bucket, which
    /// the extension's sends and edits share.
    pub(crate) fn rate_limited(&mut self, session: u32) -> bool {
        let limited = self
            .users
            .get_mut(&session)
            .is_some_and(|user| user.rate.limited(Instant::now()));
        if limited {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
        }
        limited
    }

    /// A separate bucket for queries that make the server evaluate permissions
    /// or send data back (blobs, permissions, user lookups, statistics).
    fn query_limited(&mut self, session: u32) -> bool {
        let limited = self
            .users
            .get_mut(&session)
            .is_some_and(|user| user.queries.limited(Instant::now()));
        if limited {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
        }
        limited
    }

    /// Deferred work after a handler: temporary channels that became empty go
    /// away once the state change that emptied them has been broadcast.
    pub fn finish(&mut self) {
        while let Some(id) = self.emptied.pop() {
            if self.channels.get(id).is_some_and(|channel| {
                channel.temporary && channel.users.is_empty() && channel.children.is_empty()
            }) {
                self.remove_channel(id, None);
            }
        }
        // Listeners, then subscriptions and access intervals, follow every
        // authorisation change before the lock is released (docs/extension.md
        // §5): a listener that lost Listen no longer makes its channel readable.
        self.revoke_listeners();
        self.reevaluate();
    }

    /// Remember a temporary channel that may have become empty.
    fn left_channel(&mut self, channel: u32) {
        if self
            .channels
            .get(channel)
            .is_some_and(|channel| channel.temporary && channel.users.is_empty())
        {
            self.emptied.push(channel);
        }
    }

    fn update_tokens(&mut self, session: u32, message: proto::Authenticate) {
        if self.query_limited(session) {
            return;
        }
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        if message.tokens.is_empty() && user.tokens.is_empty() {
            return;
        }
        user.tokens = bounded_tokens(&message.tokens);
        self.permissions_changed(Some(session));
        self.authz_changed(Scope::Session(session));
        let states: Vec<proto::ChannelState> = self
            .channels
            .0
            .keys()
            .map(|&channel| proto::ChannelState {
                channel_id: Some(channel),
                can_enter: Some(self.allowed(session, Action::Enter { channel })),
                ..Default::default()
            })
            .collect();
        for state in states {
            self.send(session, kind::CHANNEL_STATE, &state);
        }
    }

    fn ping(&mut self, session: u32, message: proto::Ping) {
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        let voice = &mut user.voice;
        voice.remote = nuschel_protocol::crypt::CryptStats {
            good: message.good(),
            late: message.late(),
            lost: message.lost(),
            resync: message.resync(),
        };
        voice.udp_ping = (
            message.udp_ping_avg(),
            message.udp_ping_var(),
            message.udp_packets(),
        );
        voice.tcp_ping = (
            message.tcp_ping_avg(),
            message.tcp_ping_var(),
            message.tcp_packets(),
        );
        let local = voice.crypt.stats;
        user.link.message(
            kind::PING,
            &proto::Ping {
                timestamp: message.timestamp,
                good: Some(local.good),
                late: Some(local.late),
                lost: Some(local.lost),
                resync: Some(local.resync),
                ..Default::default()
            },
        );
    }

    fn crypt_setup(&mut self, session: u32, message: proto::CryptSetup) {
        if self.query_limited(session) {
            return;
        }
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        match message.client_nonce {
            // The client lost track of our nonce and asks for it.
            None => user.link.message(
                kind::CRYPT_SETUP,
                &proto::CryptSetup {
                    server_nonce: Some(user.voice.crypt.encrypt_iv().to_vec()),
                    ..Default::default()
                },
            ),
            // The answer to our resynchronisation request.
            Some(nonce) => {
                if user.voice.crypt.set_decrypt_iv(&nonce).is_err() {
                    tracing::debug!(session, "ignored an invalid nonce resync");
                }
            }
        }
    }

    // ---- users ----

    /// Murmur's `msgUserState`: every check first, in Murmur's order, then the
    /// changes.
    fn user_state(&mut self, source: u32, mut message: proto::UserState) {
        let target = message.session.unwrap_or(source);
        if !self.users.contains_key(&source) || !self.users.contains_key(&target) {
            return;
        }
        if !self.check(source, Action::ChangeUser { user: target }) {
            return;
        }
        message.session = Some(target);
        message.actor = Some(source);
        if message.name.is_some() {
            self.check(source, Action::RenameUser { user: target });
            return;
        }
        if source == target && self.rate_limited(source) {
            return;
        }
        // Temporary access tokens count for this message only (Murmur's
        // `TemporaryAccessTokenHelper`): permissions are re-evaluated when they
        // are added and again when they are gone, so suppression and the
        // client's permission cache never keep what only a token granted.
        let temporary: Vec<[u8; 32]> = match self.users.get(&source) {
            Some(user) => bounded_tokens(&message.temporary_access_tokens)
                .into_iter()
                .filter(|token| !user.tokens.contains(token))
                .collect(),
            None => return,
        };
        if !temporary.is_empty() {
            if let Some(user) = self.users.get_mut(&source) {
                user.tokens.extend(temporary.iter().cloned());
            }
            self.permissions_changed(Some(source));
        }
        self.user_state_inner(source, target, message);
        if !temporary.is_empty() && self.users.contains_key(&source) {
            if let Some(user) = self.users.get_mut(&source) {
                user.tokens.retain(|token| !temporary.contains(token));
            }
            self.permissions_changed(Some(source));
        }
    }

    fn user_state_inner(&mut self, source: u32, target: u32, mut message: proto::UserState) {
        let (Some(source_user), Some(destination)) =
            (self.users.get(&source), self.users.get(&target))
        else {
            return;
        };
        let destination_channel = destination.channel;
        let source_version_121 = source_user.info.at_least(1, 2, 1);
        let source_version_140 = source_user.info.at_least(1, 4, 0);
        if let Some(channel) = message.channel_id {
            if self.channels.get(channel).is_none() || channel == destination_channel {
                return;
            }
            if !self.check(
                source,
                Action::Move {
                    user: target,
                    to: channel,
                },
            ) {
                return;
            }
            if self.channel_full(channel, Some(source)) {
                self.deny_with(
                    source,
                    DenyType::ChannelFull,
                    (!source_version_121).then_some("Channel is full"),
                    None,
                );
                return;
            }
        }
        let mut listen_add = Vec::new();
        let already = self
            .users
            .get(&source)
            .map_or(0, |user| user.listening.len());
        for &channel in &message.listening_channel_add {
            let Some(listeners) = self
                .channels
                .get(channel)
                .map(|channel| channel.listeners.len())
            else {
                continue;
            };
            if listen_add.contains(&channel) {
                continue;
            }
            if !self.check(
                source,
                Action::Listen {
                    user: target,
                    channel,
                },
            ) {
                continue;
            }
            if self
                .settings
                .config
                .listeners_per_channel
                .is_some_and(|limit| listeners + 1 > limit as usize)
            {
                self.deny_with(
                    source,
                    DenyType::ChannelListenerLimit,
                    (!source_version_140).then_some("No more listeners allowed in this channel"),
                    None,
                );
                continue;
            }
            if self
                .settings
                .config
                .listeners_per_user
                .is_some_and(|limit| already + listen_add.len() + 1 > limit as usize)
            {
                self.deny_with(
                    source,
                    DenyType::UserListenerLimit,
                    (!source_version_140).then_some("No more listeners allowed in this channel"),
                    None,
                );
                continue;
            }
            listen_add.push(channel);
        }
        let speak_state = message.mute.is_some()
            || message.deaf.is_some()
            || message.suppress.is_some()
            || message.priority_speaker.is_some();
        if speak_state
            && !self.check(
                source,
                Action::SpeakState {
                    user: target,
                    suppress: message.suppress == Some(true),
                    muting: message.mute.is_some()
                        || message.deaf.is_some()
                        || message.suppress.is_some(),
                },
            )
        {
            return;
        }
        if let Some(comment) = &message.comment {
            if !self.check(
                source,
                Action::SetComment {
                    user: target,
                    clearing: comment.is_empty(),
                },
            ) {
                return;
            }
            match text::check(comment, self.text_limits()) {
                Err(text::TooLong) => {
                    self.deny_type(source, DenyType::TextTooLong);
                    return;
                }
                Ok(Some(changed)) => message.comment = Some(changed),
                Ok(None) => {}
            }
        }
        if let Some(texture) = &message.texture {
            let limit = self.settings.config.image_message_length as usize;
            if limit > 0 && texture.len() > limit {
                self.deny_type(source, DenyType::TextTooLong);
                return;
            }
            if !self.check(
                source,
                Action::SetTexture {
                    user: target,
                    clearing: texture.is_empty(),
                },
            ) {
                return;
            }
        }
        if message.user_id.is_some() && !self.check(source, Action::Register { user: target }) {
            return;
        }
        if (message.self_deaf.is_some()
            || message.self_mute.is_some()
            || message.plugin_context.is_some()
            || message.plugin_identity.is_some()
            || message.recording.is_some()
            || !message.listening_channel_add.is_empty()
            || !message.listening_channel_remove.is_empty())
            && !self.check(source, Action::SelfState { user: target })
        {
            return;
        }

        // ---- all checks passed: act ----
        let mut broadcast = false;
        let Some(user) = self.users.get_mut(&target) else {
            return;
        };
        if let Some(texture) = &message.texture {
            user.set_texture(Bytes::copy_from_slice(texture));
            if let Some(id) = user.user_id.filter(|id| *id > 0) {
                use base64::Engine as _;
                self.storage.set_texture(
                    id,
                    base64::engine::general_purpose::STANDARD.encode(texture),
                );
            }
            broadcast = true;
        }
        let Some(user) = self.users.get_mut(&target) else {
            return;
        };
        if let Some(self_deaf) = message.self_deaf {
            user.self_deaf = self_deaf;
            if self_deaf {
                message.self_mute = Some(true);
            }
            broadcast = true;
        }
        if let Some(self_mute) = message.self_mute {
            user.self_mute = self_mute;
            if !self_mute {
                message.self_deaf = Some(false);
                user.self_deaf = false;
            }
            broadcast = true;
        }
        if let Some(context) = message.plugin_context.take() {
            user.plugin_context = context.into_iter().take(1024).collect();
        }
        if let Some(identity) = message.plugin_identity.take() {
            user.plugin_identity = identity.chars().take(1024).collect();
        }
        if let Some(comment) = &message.comment {
            user.set_comment(comment.clone());
            if let Some(id) = user.user_id.filter(|id| *id > 0) {
                self.storage.set_comment(id, comment.clone());
            }
            broadcast = true;
        }
        let Some(user) = self.users.get_mut(&target) else {
            return;
        };
        if speak_state {
            if let Some(deaf) = message.deaf {
                user.deaf = deaf;
                if deaf {
                    message.mute = Some(true);
                }
            }
            if let Some(mute) = message.mute {
                user.mute = mute;
                if !mute {
                    message.deaf = Some(false);
                    user.deaf = false;
                }
            }
            if let Some(suppress) = message.suppress {
                user.suppress = suppress;
            }
            if let Some(priority) = message.priority_speaker {
                user.priority_speaker = priority;
            }
            broadcast = true;
            tracing::info!(
                actor = %self.log_name(source),
                target = %self.log_name(target),
                "changed speak state"
            );
        }
        let Some(user) = self.users.get_mut(&target) else {
            return;
        };
        if let Some(recording) = message.recording
            && user.recording != recording
        {
            user.recording = recording;
            if recording && !self.settings.config.allow_recording {
                let reason = "Recording is not allowed on this server";
                self.send(
                    source,
                    kind::USER_REMOVE,
                    &proto::UserRemove {
                        session: source,
                        reason: Some(reason.into()),
                        ..Default::default()
                    },
                );
                self.disconnect(source, true);
                return;
            }
            broadcast = true;
        }
        if let Some(channel) = message.channel_id {
            self.enter_channel(target, channel, &mut message);
            tracing::info!(actor = %self.log_name(source), target = %self.log_name(target), channel, "moved");
            broadcast = true;
        }
        let mut routing_changed = false;
        for &channel in &listen_add {
            self.add_listener(target, channel);
            self.persist_listener(target, channel);
            routing_changed = true;
        }
        message.listening_channel_add = listen_add;
        let mut volume_changed = false;
        message.listening_volume_adjustment.retain(|adjustment| {
            adjustment
                .listening_channel
                .is_some_and(|channel| self.channels.get(channel).is_some())
                && adjustment.volume_adjustment.is_some_and(f32::is_finite)
        });
        message.listening_volume_adjustment.truncate(256);
        for adjustment in &message.listening_volume_adjustment {
            if let (Some(channel), Some(volume)) =
                (adjustment.listening_channel, adjustment.volume_adjustment)
                && let Some(user) = self.users.get_mut(&target)
                && let Some(entry) = user.listening.get_mut(&channel)
            {
                *entry = volume.clamp(0., 64.);
                volume_changed = true;
                self.persist_listener(target, channel);
            }
        }
        let removals: Vec<u32> = message
            .listening_channel_remove
            .iter()
            .copied()
            .filter(|channel| {
                self.users
                    .get(&target)
                    .is_some_and(|user| user.listening.contains_key(channel))
            })
            .collect();
        for &channel in &removals {
            self.forget_listener(target, channel);
            routing_changed = true;
        }
        message.listening_channel_remove = removals;
        let volume_only = !broadcast && volume_changed && !routing_changed;
        broadcast |= routing_changed || volume_changed;
        if routing_changed || volume_changed {
            self.generation += 1;
        }
        let mut registered = false;
        if message.user_id.is_some() {
            match self.register(target) {
                Some(id) => {
                    message.user_id = Some(id);
                    registered = true;
                }
                None => message.user_id = None,
            }
            broadcast = true;
        }
        if broadcast {
            let Some(user) = self.users.get(&target) else {
                return;
            };
            if message.texture.is_some()
                && let Some(hash) = user.texture_hash
            {
                message.texture = None;
                message.texture_hash = Some(hash.to_vec());
            }
            if message.comment.is_some()
                && let Some(hash) = user.comment_hash
            {
                message.comment = None;
                message.comment_hash = Some(hash.to_vec());
            }
            self.send(source, kind::USER_STATE, &message);
            if !self.settings.config.broadcast_listener_volume_adjustments {
                message.listening_volume_adjustment.clear();
            }
            if self.settings.config.broadcast_listener_volume_adjustments || !volume_only {
                self.broadcast(kind::USER_STATE, &message, Some(source));
            }
            if registered {
                self.permissions_changed(Some(target));
            }
        }
    }

    /// Move a user (Murmur's `userEnterChannel`): updates suppression from the
    /// Speak permission and clears priority speaker in `message`.
    pub fn enter_channel(&mut self, session: u32, channel: u32, message: &mut proto::UserState) {
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        let old = user.channel;
        if old == channel || self.channels.get(channel).is_none() {
            return;
        }
        user.channel = channel;
        if let Some(previous) = self.channels.get_mut(old) {
            previous.users.retain(|member| *member != session);
        }
        self.left_channel(old);
        if let Some(next) = self.channels.get_mut(channel) {
            next.users.push(session);
        }
        self.invalidate();
        let may_speak = self.allowed(session, Action::Speak { channel });
        if let Some(user) = self.users.get_mut(&session) {
            if may_speak == user.suppress {
                user.suppress = !may_speak;
                message.suppress = Some(user.suppress);
            }
            if user.priority_speaker {
                user.priority_speaker = false;
                message.priority_speaker = Some(false);
            }
        }
        self.permissions_changed(Some(session));
        self.authz_changed(Scope::Session(session));
        self.remember_channel(session);
        self.send_permissions(session, channel, false);
        if let Some(parent) = self.channels.get(channel).and_then(Channel::parent) {
            self.send_permissions(session, parent, false);
        }
        self.retention_notice(session, channel);
    }

    /// Stock clients cannot know that text is kept and readable by people who
    /// join later; they are told when they enter a channel with history
    /// (docs/extension.md §9).
    pub fn retention_notice(&mut self, session: u32, channel: u32) {
        let history = &self.settings.config.history;
        let Some(id) = self.conversation_of_channel(channel) else {
            return;
        };
        // Channels with history, and those that keep something else than the
        // welcome text said (retention.rs).
        let retention = self.retention(&id);
        if !history.notify_stock_clients
            || (self.conversations.head(&id) == 0 && retention == history.default_retention())
        {
            return;
        }
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        if user.extension.is_some() || user.notified.len() >= 256 || !user.notified.insert(channel)
        {
            return;
        }
        let text = crate::retention::notice(retention, true);
        self.text_to(session, &text);
    }

    /// Registered users' listeners outlive the connection (Murmur's
    /// `channel_listeners`), except in temporary channels.
    pub(crate) fn persist_listener(&self, session: u32, channel: u32) {
        let Some(user) = self.users.get(&session) else {
            return;
        };
        if let (Some(id), Some(volume)) = (user.user_id, user.listening.get(&channel))
            && self
                .channels
                .get(channel)
                .is_some_and(|channel| !channel.temporary)
        {
            self.storage.set_listener(id, channel, *volume);
        }
    }

    pub(crate) fn add_listener(&mut self, session: u32, channel: u32) {
        if let Some(user) = self.users.get_mut(&session) {
            user.listening.entry(channel).or_insert(1.);
        }
        if let Some(channel) = self.channels.get_mut(channel) {
            channel.listeners.insert(session);
        }
        self.authz_changed(Scope::Session(session));
    }

    /// End a listener, and forget it if the user is registered (Murmur's
    /// `channel_listeners`).
    pub(crate) fn forget_listener(&mut self, session: u32, channel: u32) {
        self.remove_listener(session, channel);
        if let Some(id) = self.users.get(&session).and_then(|user| user.user_id) {
            self.storage.remove_listener(id, channel);
        }
    }

    fn remove_listener(&mut self, session: u32, channel: u32) {
        if let Some(user) = self.users.get_mut(&session) {
            user.listening.remove(&channel);
        }
        if let Some(channel) = self.channels.get_mut(channel) {
            channel.listeners.remove(&session);
        }
        self.authz_changed(Scope::Session(session));
    }

    /// Register a connected user (Murmur's `registerUser`).
    fn register(&mut self, session: u32) -> Option<u32> {
        let user = self.users.get(&session)?;
        let name = user.name.clone();
        if name.is_empty() || !self.valid_username(&name) || self.registry.by_name(&name).is_some()
        {
            return None;
        }
        let id = self.registry.allocate_id();
        self.storage.set_next_user_id(self.registry.next_id);
        let account = random();
        self.registry.insert(Registered {
            id,
            account,
            name,
            password_hash: None,
            certificate_hash: user.certificate_hash.clone(),
            last_channel: None,
            last_active: Some(now_unix()),
        });
        let comment = user.comment.clone();
        let texture = user.texture.clone();
        if let Some(user) = self.users.get_mut(&session) {
            user.user_id = Some(id);
            user.account = Some(account);
        }
        self.authz_changed(Scope::Session(session));
        self.persist_registration(id);
        if !comment.is_empty() {
            self.storage.set_comment(id, comment);
        }
        if !texture.is_empty() {
            use base64::Engine as _;
            self.storage.set_texture(
                id,
                base64::engine::general_purpose::STANDARD.encode(&texture),
            );
        }
        let listening: Vec<u32> = self
            .users
            .get(&session)
            .map(|user| user.listening.keys().copied().collect())
            .unwrap_or_default();
        for channel in listening {
            self.persist_listener(session, channel);
        }
        self.remember_channel(session);
        tracing::info!(user = %self.log_name(session), "registered");
        Some(id)
    }

    pub fn valid_username(&self, name: &str) -> bool {
        name.chars().count() <= 512
            && name.len() <= MAX_STORED_NAME
            && name.trim() == name
            && self.settings.username.is_match(name)
    }

    fn user_remove(&mut self, source: u32, mut message: proto::UserRemove) {
        let target = message.session;
        let Some(destination) = self.users.get(&target) else {
            return;
        };
        message.actor = Some(source);
        let ban = message.ban == Some(true);
        let action = if ban {
            Action::Ban { user: target }
        } else {
            Action::Kick { user: target }
        };
        if !self.check(source, action) {
            return;
        }
        if let Some(reason) = &mut message.reason {
            truncate_chars(
                reason,
                self.settings.config.text_message_length.max(256) as usize,
            );
        }
        if ban {
            let mut reason = message.reason.clone().unwrap_or_default();
            truncate_chars(&mut reason, 1000);
            self.bans.push(Ban {
                address: Some(mapped(destination.address.ip())),
                mask: 128,
                name: destination.name.clone(),
                hash: destination.certificate_hash.clone().unwrap_or_default(),
                reason,
                start: now_unix(),
                duration: 0,
            });
            self.persist_bans();
        }
        tracing::info!(
            actor = %self.log_name(source),
            target = %self.log_name(target),
            "{}",
            if ban { "kickbanned" } else { "kicked" }
        );
        self.broadcast(kind::USER_REMOVE, &message, None);
        self.disconnect(target, false);
    }

    /// Drop a user's session: everyone else learns it left (`announce`, unless
    /// they were already told), and the connection flushes and closes.
    pub fn disconnect(&mut self, session: u32, announce: bool) {
        let Some(user) = self.users.remove(&session) else {
            return;
        };
        if let Some((_, address)) = user.voice.udp {
            self.udp_peers.remove(&address);
        }
        if let Some(sessions) = self.hosts.get_mut(&user.address.ip()) {
            sessions.retain(|member| *member != session);
            if sessions.is_empty() {
                self.hosts.remove(&user.address.ip());
            }
        }
        if let Some(channel) = self.channels.get_mut(user.channel) {
            channel.users.retain(|member| *member != session);
        }
        self.left_channel(user.channel);
        for channel in user.listening.keys() {
            if let Some(channel) = self.channels.get_mut(*channel) {
                channel.listeners.remove(&session);
            }
        }
        for other in self.users.values_mut() {
            for target in other.targets.values_mut() {
                target.sessions.retain(|member| *member != session);
            }
        }
        if let Some(extension) = &user.extension {
            for id in &extension.subscriptions {
                if let Some(subscribers) = self.conversations.subscribers.get_mut(id) {
                    subscribers.remove(&session);
                    if subscribers.is_empty() {
                        self.conversations.subscribers.remove(id);
                    }
                }
            }
        }
        // Read markers are kept in memory while any of the account's
        // connections is online.
        if let Some(account) = user.account
            && !self
                .users
                .values()
                .any(|other| other.account == Some(account))
        {
            self.conversations.markers.remove(&account);
        }
        if let Some(id) = user.user_id
            && let Some(registered) = self.registry.users.get_mut(&id)
        {
            registered.last_active = Some(now_unix());
            self.persist_registration(id);
        }
        if announce {
            self.broadcast(
                kind::USER_REMOVE,
                &proto::UserRemove {
                    session,
                    ..Default::default()
                },
                None,
            );
        }
        user.link.close();
        self.invalidate();
        self.finish();
    }

    fn user_list(&mut self, source: u32, message: proto::UserList) {
        if !self.check(source, Action::ManageRegistrations) {
            return;
        }
        if message.users.is_empty() {
            let users = self
                .registry
                .users
                .values()
                .filter(|user| user.id > 0)
                .map(|user| proto::user_list::User {
                    user_id: user.id,
                    name: Some(user.name.clone()),
                    last_seen: user.last_active.map(crate::timefmt::iso8601),
                    last_channel: user.last_channel,
                })
                .collect();
            self.send(source, kind::USER_LIST, &proto::UserList { users });
            return;
        }
        for entry in message.users.into_iter().take(1024) {
            let id = entry.user_id;
            if !self.allowed(source, Action::EditRegistration { user_id: id })
                || !self.registry.users.contains_key(&id)
            {
                continue;
            }
            match entry.name {
                None => self.unregister(id),
                Some(name) => {
                    let name = name.trim().to_owned();
                    if !self.valid_username(&name) {
                        let too_old = !self
                            .users
                            .get(&source)
                            .is_some_and(|user| user.info.at_least(1, 2, 1));
                        let reason = format!("{name} is not a valid username");
                        self.send(
                            source,
                            kind::PERMISSION_DENIED,
                            &proto::PermissionDenied {
                                r#type: Some(DenyType::UserName as i32),
                                reason: too_old.then_some(reason),
                                name: (!too_old).then(|| name.clone()),
                                ..Default::default()
                            },
                        );
                        continue;
                    }
                    if self
                        .registry
                        .by_name(&name)
                        .is_some_and(|other| other.id != id)
                    {
                        continue;
                    }
                    if let Some(mut registered) = self.registry.users.get(&id).cloned() {
                        registered.name = name.clone();
                        self.registry.insert(registered);
                        self.persist_registration(id);
                    }
                    let online = self
                        .users
                        .values()
                        .find(|user| user.user_id == Some(id))
                        .map(|user| user.session);
                    if let Some(session) = online {
                        if let Some(user) = self.users.get_mut(&session) {
                            user.name = name.clone();
                        }
                        self.broadcast(
                            kind::USER_STATE,
                            &proto::UserState {
                                session: Some(session),
                                actor: Some(source),
                                name: Some(name),
                                ..Default::default()
                            },
                            None,
                        );
                    }
                }
            }
        }
    }

    /// Murmur's `unregisterUser`: forget the account and every ACL entry and
    /// group membership naming it.
    pub fn unregister(&mut self, id: u32) {
        if id == 0 {
            return;
        }
        let Some(removed) = self.registry.remove(id) else {
            return;
        };
        self.storage.delete_user(id);
        // The account id is never reused: its direct conversations freeze and
        // its messages show as a deleted user's; markers and intervals go.
        self.conversations.markers.remove(&removed.account);
        for accounts in self.conversations.access.values_mut() {
            accounts.remove(&removed.account);
        }
        self.history.write(Storage::HistoryForgetAccount {
            account: hex(&removed.account),
        });
        let mut changed = Vec::new();
        for channel in self.channels.0.values_mut() {
            let before = (channel.node.acls.len(), channel.node.groups.clone());
            channel.node.acls.retain(|acl| acl.user_id != Some(id));
            for group in channel.node.groups.values_mut() {
                group.add.remove(&id);
                group.remove.remove(&id);
            }
            if before.0 != channel.node.acls.len() || before.1 != channel.node.groups {
                changed.push(channel.id);
            }
        }
        for channel in changed {
            self.persist_channel(channel);
        }
        let online = self
            .users
            .values()
            .find(|user| user.user_id == Some(id))
            .map(|user| user.session);
        if let Some(session) = online {
            if let Some(user) = self.users.get_mut(&session) {
                user.user_id = None;
                user.account = None;
            }
            self.broadcast(
                kind::USER_STATE,
                &proto::UserState {
                    session: Some(session),
                    user_id: Some(u32::MAX),
                    ..Default::default()
                },
                None,
            );
        }
        self.permissions_changed(None);
        self.authz_changed(Scope::All);
    }

    fn query_users(&mut self, source: u32, message: proto::QueryUsers) {
        if self.query_limited(source) || message.ids.len() + message.names.len() > 256 {
            return;
        }
        if !self.allowed(source, Action::QueryUsers) {
            return;
        }
        let mut reply = proto::QueryUsers::default();
        for id in message.ids.into_iter().take(1024) {
            if let Some(user) = self.registry.users.get(&id) {
                reply.ids.push(id);
                reply.names.push(user.name.clone());
            }
        }
        for name in message.names.iter().take(1024) {
            if let Some(user) = self.registry.by_name(name) {
                reply.ids.push(user.id);
                reply.names.push(user.name.clone());
            }
        }
        self.send(source, kind::QUERY_USERS, &reply);
    }

    fn user_stats(&mut self, source: u32, message: proto::UserStats) {
        if self.query_limited(source) {
            return;
        }
        let target = message.session.unwrap_or(source);
        let Some(destination) = self.users.get(&target) else {
            return;
        };
        if !self.check(source, Action::UserStats { user: target }) {
            return;
        }
        let details = self.allowed(source, Action::UserStatsDetails { user: target })
            && !message.stats_only();
        let local = self.allowed(source, Action::UserStatsLocal { user: target });
        let now = Instant::now();
        let voice = &destination.voice;
        let mut reply = proto::UserStats {
            session: Some(target),
            ..Default::default()
        };
        if details {
            reply.certificates = destination.certificates.clone();
            reply.strong_certificate = Some(destination.strong);
            reply.version = Some(destination.info.to_proto());
            reply.celt_versions = destination.celt_versions.clone();
            reply.opus = Some(destination.opus);
            reply.address = Some(mapped(destination.address.ip()).octets().to_vec());
        }
        if local {
            let stats = |stats: nuschel_protocol::crypt::CryptStats| proto::user_stats::Stats {
                good: Some(stats.good),
                late: Some(stats.late),
                lost: Some(stats.lost),
                resync: Some(stats.resync),
            };
            reply.from_client = Some(stats(voice.crypt.stats));
            reply.from_server = Some(stats(voice.remote));
            reply.bandwidth = Some(voice.bandwidth.bandwidth(now));
            reply.idlesecs = Some(voice.bandwidth.idle_seconds(now));
        }
        reply.udp_packets = Some(voice.udp_ping.2);
        reply.tcp_packets = Some(voice.tcp_ping.2);
        reply.udp_ping_avg = Some(voice.udp_ping.0);
        reply.udp_ping_var = Some(voice.udp_ping.1);
        reply.tcp_ping_avg = Some(voice.tcp_ping.0);
        reply.tcp_ping_var = Some(voice.tcp_ping.1);
        reply.onlinesecs = Some(
            now.saturating_duration_since(voice.bandwidth.online_since)
                .as_secs() as u32,
        );
        self.send(source, kind::USER_STATS, &reply);
    }

    fn request_blob(&mut self, source: u32, message: proto::RequestBlob) {
        if self.query_limited(source) {
            return;
        }
        let unique = |ids: Vec<u32>| ids.into_iter().take(64).collect::<BTreeSet<u32>>();
        for channel in unique(message.channel_description) {
            if let Some(channel) = self
                .channels
                .get(channel)
                .filter(|channel| !channel.description.is_empty())
            {
                self.send(
                    source,
                    kind::CHANNEL_STATE,
                    &proto::ChannelState {
                        channel_id: Some(channel.id),
                        description: Some(channel.description.clone()),
                        ..Default::default()
                    },
                );
            }
        }
        for session in unique(message.session_texture) {
            if let Some(user) = self
                .users
                .get(&session)
                .filter(|user| !user.texture.is_empty())
            {
                self.send(
                    source,
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(session),
                        texture: Some(user.texture.to_vec()),
                        ..Default::default()
                    },
                );
            }
        }
        for session in unique(message.session_comment) {
            if let Some(user) = self
                .users
                .get(&session)
                .filter(|user| !user.comment.is_empty())
            {
                self.send(
                    source,
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(session),
                        comment: Some(user.comment.clone()),
                        ..Default::default()
                    },
                );
            }
        }
    }

    fn voice_target(&mut self, source: u32, message: proto::VoiceTarget) {
        if self.query_limited(source) {
            return;
        }
        let Some(id) = message
            .id
            .and_then(|id| u8::try_from(id).ok())
            .filter(|id| (1..31).contains(id))
        else {
            return;
        };
        let mut target = WhisperTarget::default();
        let mut entries = 0;
        for entry in &message.targets {
            for &session in &entry.session {
                entries += 1;
                if entries > MAX_VOICE_TARGET_ENTRIES {
                    break;
                }
                if self.users.contains_key(&session) && !target.sessions.contains(&session) {
                    target.sessions.push(session);
                }
            }
            if let Some(channel) = entry.channel_id
                && self.channels.get(channel).is_some()
            {
                entries += 1;
                let mut group = entry.group.clone().unwrap_or_default();
                truncate_chars(&mut group, MAX_NAME_BYTES);
                let spec = (channel, entry.children(), entry.links(), group);
                if entries <= MAX_VOICE_TARGET_ENTRIES && !target.channels.contains(&spec) {
                    target.channels.push(spec);
                }
            }
        }
        let Some(user) = self.users.get_mut(&source) else {
            return;
        };
        user.target_cache.get_mut().remove(&id);
        if target.sessions.is_empty() && target.channels.is_empty() {
            user.targets.remove(&id);
        } else {
            user.targets.insert(id, target);
        }
    }

    fn plugin_data(&mut self, source: u32, mut message: proto::PluginDataTransmission) {
        let Some(user) = self.users.get_mut(&source) else {
            return;
        };
        if user.plugin_rate.limited(Instant::now()) {
            return;
        }
        let (Some(data), Some(data_id)) = (&message.data, &message.data_id) else {
            return;
        };
        if data.len() > MAX_PLUGIN_DATA || data_id.len() > MAX_PLUGIN_DATA_ID {
            return;
        }
        if message.receiver_sessions.len() > self.users.len() + 20 {
            return;
        }
        message.sender_session = Some(source);
        let receivers: BTreeSet<u32> = std::mem::take(&mut message.receiver_sessions)
            .into_iter()
            .collect();
        let Some(frame) =
            nuschel_protocol::encode_control(kind::PLUGIN_DATA_TRANSMISSION, &message)
        else {
            return;
        };
        for receiver in receivers {
            if let Some(user) = self.users.get(&receiver) {
                user.link.send(frame.clone());
            }
        }
    }

    // ---- bans ----

    fn ban_list(&mut self, source: u32, message: proto::BanList) {
        if !self.check(source, Action::ManageBans) {
            return;
        }
        if message.query() {
            let bans = self
                .bans
                .iter()
                .map(|ban| proto::ban_list::BanEntry {
                    address: ban
                        .address
                        .unwrap_or(Ipv6Addr::UNSPECIFIED)
                        .octets()
                        .to_vec(),
                    mask: u32::from(ban.mask),
                    name: Some(ban.name.clone()),
                    hash: Some(ban.hash.clone()),
                    reason: Some(ban.reason.clone()),
                    start: Some(crate::timefmt::iso8601(ban.start)),
                    duration: Some(ban.duration),
                })
                .collect();
            self.send(
                source,
                kind::BAN_LIST,
                &proto::BanList { bans, query: None },
            );
            return;
        }
        let mut bans = Vec::new();
        for entry in message.bans.into_iter().take(MAX_BANS) {
            let address = match entry.address.len() {
                16 => <[u8; 16]>::try_from(entry.address.as_slice())
                    .ok()
                    .map(Ipv6Addr::from),
                4 => <[u8; 4]>::try_from(entry.address.as_slice())
                    .ok()
                    .map(IpAddr::from)
                    .map(mapped),
                _ => None,
            };
            let clip = |text: Option<String>| {
                let mut text = text.unwrap_or_default();
                truncate_chars(&mut text, 1000);
                // The storage refuses texts over 1024 bytes, and with them every ban.
                truncate_bytes(&mut text, 1000);
                text
            };
            let ban = Ban {
                address: address.filter(|address| !address.is_unspecified()),
                mask: entry.mask.min(128) as u8,
                name: clip(entry.name),
                hash: clip(entry.hash),
                reason: clip(entry.reason),
                start: entry
                    .start
                    .as_deref()
                    .and_then(crate::timefmt::parse_iso8601)
                    .unwrap_or_else(now_unix),
                duration: entry.duration.unwrap_or(0),
            };
            if ban.valid() && !bans.contains(&ban) {
                bans.push(ban);
            }
        }
        self.bans = bans;
        self.persist_bans();
        tracing::info!(actor = %self.log_name(source), count = self.bans.len(), "updated the ban list");
    }

    // ---- text ----

    pub fn text_limits(&self) -> text::TextLimits {
        text::TextLimits {
            allow_html: self.settings.config.allow_html,
            text: self.settings.config.text_message_length as usize,
            image: self.settings.config.image_message_length as usize,
        }
    }

    /// The single path every stock text message takes: permission checks,
    /// limits and delivery. Channel messages are also stored (format
    /// `legacy_html`), and Nuschel readers get them from history once stored
    /// (docs/extension.md §4); direct messages to sessions are not stored.
    fn text_message(&mut self, source: u32, mut message: proto::TextMessage) {
        if self.rate_limited(source) {
            return;
        }
        match text::check(&message.message, self.text_limits()) {
            Err(text::TooLong) => {
                self.deny_type(source, DenyType::TextTooLong);
                return;
            }
            Ok(Some(changed)) => message.message = changed,
            Ok(None) => {}
        }
        if message.message.is_empty() {
            return;
        }
        if message.channel_id.len() + message.tree_id.len() + message.session.len() > 4096 {
            return;
        }
        // Murmur walks the tree once per listed id; duplicates cost nothing here.
        for ids in [
            &mut message.channel_id,
            &mut message.tree_id,
            &mut message.session,
        ] {
            ids.sort_unstable();
            ids.dedup();
        }
        message.actor = Some(source);
        let mut recipients = BTreeSet::new();
        // Every channel the message reaches, to be stored there.
        let mut reached = BTreeSet::new();
        for &channel in &message.channel_id {
            if self.channels.get(channel).is_none()
                || !self.check(source, Action::TextChannel { channel })
            {
                return;
            }
            let Some(target) = self.channels.get(channel) else {
                return;
            };
            recipients.extend(target.users.iter().copied());
            recipients.extend(target.listeners.iter().copied());
            reached.insert(channel);
        }
        let mut trees = Vec::new();
        for &channel in &message.tree_id {
            if self.channels.get(channel).is_none()
                || !self.check(source, Action::TextChannel { channel })
            {
                return;
            }
            trees.push(channel);
        }
        // Sub-channels without permission are skipped, together with what is below them.
        let mut visited = BTreeSet::new();
        while let Some(channel) = trees.pop() {
            if !visited.insert(channel) || !self.allowed(source, Action::TextChannel { channel }) {
                continue;
            }
            if let Some(target) = self.channels.get(channel) {
                trees.extend(target.children.iter().copied());
                recipients.extend(target.users.iter().copied());
                recipients.extend(target.listeners.iter().copied());
                reached.insert(channel);
            }
        }
        for &session in &message.session {
            if self.users.contains_key(&session) {
                if !self.check(source, Action::TextUser { user: session }) {
                    return;
                }
                recipients.insert(session);
            }
        }
        recipients.remove(&source);
        let Some(frame) = nuschel_protocol::encode_control(kind::TEXT_MESSAGE, &message) else {
            return;
        };
        // Nuschel readers of the reached channels get the stored message.
        let mut readers = BTreeSet::new();
        for &channel in &reached {
            if let Some(id) = self.conversation_of_channel(channel) {
                readers.extend(self.readers(&id));
            }
        }
        // Direct messages from stock senders are kept only if the operator
        // chose to (docs/extension.md §9), between two accounts, and only when
        // the sender may write to that direct conversation as a Nuschel client
        // would (to every connection of the account, not only the one listed);
        // otherwise the listed connection gets it live, as from Murmur.
        let mut directs = BTreeSet::new();
        if self.settings.config.history.archive_stock_direct_messages
            && let Some(sender) = self.users.get(&source).and_then(|user| user.account)
        {
            for session in &message.session {
                if let Some(peer) = self.users.get(session).and_then(|user| user.account)
                    && peer != sender
                    && self.allowed(source, Action::SendDirect { peer: &peer })
                    // An end-to-end conversation never stores plaintext; the
                    // message still arrives live, outside it.
                    && !self
                        .conversations
                        .direct(sender, peer)
                        .is_some_and(|id| self.mls_group(&id))
                {
                    directs.insert(peer);
                }
            }
            for peer in &directs {
                let id = self.direct_conversation(sender, *peer);
                readers.extend(self.readers(&id));
            }
        }
        for session in recipients {
            if readers.contains(&session) {
                continue;
            }
            if let Some(user) = self.users.get(&session) {
                user.link.send(frame.clone());
            }
        }
        for channel in reached {
            if let Some(id) = self.conversation_of_channel(channel) {
                self.store_stock(source, id, &message.message, &frame);
            }
        }
        if let Some(sender) = self.users.get(&source).and_then(|user| user.account) {
            for peer in directs {
                let id = self.direct_conversation(sender, peer);
                self.store_stock(source, id, &message.message, &frame);
            }
        }
    }

    // ---- channels ----

    fn channel_remove(&mut self, source: u32, channel: u32) {
        if self.channels.get(channel).is_none() {
            return;
        }
        if !self.check(source, Action::RemoveChannel { channel }) {
            return;
        }
        tracing::info!(actor = %self.log_name(source), channel, "removed channel");
        self.remove_channel(channel, None);
    }

    /// Murmur's `removeChannel`: children first, users move up to the first
    /// channel they may enter, listeners are dropped.
    pub fn remove_channel(&mut self, id: u32, destination: Option<u32>) {
        let Some(channel) = self.channels.get(id) else {
            return;
        };
        let Some(parent) = channel.parent() else {
            return;
        };
        let destination = destination
            .filter(|d| self.channels.get(*d).is_some())
            .unwrap_or(parent);
        for link in channel.links.clone() {
            if let Some(linked) = self.channels.get_mut(link) {
                linked.links.remove(&id);
            }
        }
        let Some(channel) = self.channels.get_mut(id) else {
            return;
        };
        channel.links.clear();
        let children = channel.children.clone();
        let users = channel.users.clone();
        let listeners = channel.listeners.clone();
        let temporary = channel.temporary;
        for child in children {
            self.remove_channel(child, Some(destination));
        }
        for session in users {
            let mut target = destination;
            while let Some(parent) = self.channels.get(target).and_then(Channel::parent) {
                if self.allowed(session, Action::Enter { channel: target })
                    && !self.channel_full(target, Some(session))
                {
                    break;
                }
                target = parent;
            }
            let mut message = proto::UserState {
                session: Some(session),
                channel_id: Some(target),
                ..Default::default()
            };
            self.enter_channel(session, target, &mut message);
            self.broadcast(kind::USER_STATE, &message, None);
        }
        for session in listeners {
            self.remove_listener(session, id);
            self.broadcast(
                kind::USER_STATE,
                &proto::UserState {
                    session: Some(session),
                    listening_channel_remove: vec![id],
                    ..Default::default()
                },
                None,
            );
        }
        self.broadcast(
            kind::CHANNEL_REMOVE,
            &proto::ChannelRemove { channel_id: id },
            None,
        );
        if !temporary {
            self.storage.delete_channel(id);
        }
        if let Some(parent) = self.channels.get_mut(parent) {
            parent.children.retain(|child| *child != id);
        }
        if let Some(channel) = self.channels.0.remove(&id) {
            // A temporary channel's history goes with it; a permanent one's
            // becomes administrator-only (docs/extension.md §3).
            let conversation = channel.conversation_id;
            if temporary {
                self.conversations.remove(&conversation);
                self.history.write(Storage::HistoryPurge {
                    conversation: hex(&conversation),
                });
            } else {
                self.conversations.archive(conversation);
            }
        }
        self.authz_changed(Scope::All);
        for user in self.users.values_mut() {
            let mut emptied = Vec::new();
            for (target_id, target) in &mut user.targets {
                let before = target.channels.len();
                target.channels.retain(|spec| spec.0 != id);
                if target.channels.len() != before {
                    emptied.push(*target_id);
                }
            }
            for target_id in emptied {
                user.target_cache.get_mut().remove(&target_id);
            }
        }
        self.invalidate();
    }

    fn valid_channel_name(&self, name: &str) -> bool {
        name.chars().count() <= 512
            && name.len() <= MAX_STORED_NAME
            && self.settings.channel_name.is_match(name)
    }

    fn can_nest(&self, parent: u32, channel: Option<u32>) -> bool {
        let level = self.channels.level(parent);
        let height = channel.map_or(0, |channel| self.channels.height(channel));
        level + height < self.settings.config.channel_nesting_limit as usize
    }

    fn sibling_named(&self, parent: u32, name: &str, except: Option<u32>) -> bool {
        self.channels.get(parent).is_some_and(|parent| {
            parent.children.iter().any(|child| {
                Some(*child) != except
                    && self
                        .channels
                        .get(*child)
                        .is_some_and(|child| child.name == name)
            })
        })
    }

    fn new_channel_id(&self, temporary: bool) -> Option<u32> {
        if temporary {
            // Temporary channels are numbered down from i32::MAX, as in Murmur,
            // so they never collide with permanent ones.
            (0..=i32::MAX as u32)
                .rev()
                .find(|id| self.channels.get(*id).is_none())
        } else {
            let highest = self
                .channels
                .0
                .values()
                .filter(|channel| !channel.temporary)
                .map(|channel| channel.id)
                .max()
                .unwrap_or(0);
            highest
                .checked_add(1)
                .filter(|id| self.channels.get(*id).is_none())
        }
    }

    fn channel_state_message(&mut self, source: u32, mut message: proto::ChannelState) {
        let existing = message.channel_id;
        if let Some(id) = existing {
            if self.channels.get(id).is_none() {
                return;
            }
        } else if self.rate_limited(source) {
            return;
        }
        if let Some(parent) = message.parent
            && self.channels.get(parent).is_none()
        {
            return;
        }
        message.links.clear();
        let source_version = self
            .users
            .get(&source)
            .map(|user| user.info.clone())
            .unwrap_or_default();
        if let Some(description) = &message.description {
            match text::check(description, self.text_limits()) {
                Err(text::TooLong) => {
                    self.deny_type(source, DenyType::TextTooLong);
                    return;
                }
                Ok(Some(changed)) => message.description = Some(changed),
                Ok(None) => {}
            }
        }
        if let Some(name) = &message.name {
            if name.is_empty() || !self.valid_channel_name(name) {
                self.deny_type(source, DenyType::ChannelName);
                return;
            }
            let siblings_of = message.parent.or_else(|| {
                existing
                    .filter(|id| *id != 0)
                    .and_then(|id| self.channels.get(id)?.parent())
            });
            if let Some(parent) = siblings_of
                && self.sibling_named(parent, name, None)
            {
                self.deny_type(source, DenyType::ChannelName);
                return;
            }
        }
        if let Some(parent) = message.parent
            && !self.can_nest(parent, existing)
        {
            self.deny_with(
                source,
                DenyType::NestingLimit,
                (!source_version.at_least(1, 2, 4)).then_some("Channel nesting limit reached"),
                None,
            );
            return;
        }
        match existing {
            None => self.create_channel(source, message, &source_version),
            Some(id) => self.update_channel(source, id, message),
        }
    }

    fn create_channel(
        &mut self,
        source: u32,
        mut message: proto::ChannelState,
        version: &crate::state::ClientInfo,
    ) {
        let (Some(parent), Some(name)) = (message.parent, message.name.clone()) else {
            return;
        };
        let limit = self.settings.config.channel_count_limit as usize;
        if limit != 0 && self.channels.0.len() >= limit {
            self.deny_with(
                source,
                DenyType::ChannelCountLimit,
                (!version.at_least(1, 3, 0)).then_some("Channel count limit reached"),
                None,
            );
            return;
        }
        let temporary = message.temporary();
        if !self.check(source, Action::MakeChannel { parent, temporary }) {
            return;
        }
        let Some(user) = self.users.get(&source) else {
            return;
        };
        let (user_id, hash) = (user.user_id, user.certificate_hash.clone());
        // One user's temporary channels are capped, so nobody fills
        // channel_count_limit alone; the request fails with the reason.
        let creator = temporary.then(|| match (user_id, &hash) {
            (Some(id), _) => format!("#{id}"),
            (None, hash) => format!("${}", hash.as_deref().unwrap_or_default()),
        });
        let per_user = self.settings.config.temporary_channels_per_user as usize;
        if per_user > 0
            && let Some(creator) = &creator
            && self
                .channels
                .0
                .values()
                .filter(|channel| channel.temporary && channel.creator.as_ref() == Some(creator))
                .count()
                >= per_user
        {
            self.deny_with(
                source,
                DenyType::Text,
                Some(&format!(
                    "You already have {per_user} temporary channels, the most this server allows"
                )),
                None,
            );
            return;
        }
        let Some(id) = self.new_channel_id(temporary) else {
            return;
        };
        let mut channel = Channel::new(id, Some(parent), name);
        channel.temporary = temporary;
        channel.creator = creator;
        channel.position = message.position();
        channel.max_users = message.max_users.unwrap_or(0);
        channel.set_description(message.description.clone().unwrap_or_default());
        if let Some(user_id) = user_id {
            let mut admins = Group::new();
            admins.add.insert(user_id);
            channel.node.groups.insert("admin".into(), admins);
        }
        let description_hash = channel.description_hash;
        self.conversations.add_channel(channel.conversation_id, id);
        self.channels.0.insert(id, channel);
        if let Some(parent) = self.channels.get_mut(parent) {
            parent.children.push(id);
        }
        self.invalidate();
        if !self.allowed(source, Action::EditChannel { channel: id }) {
            if let Some(channel) = self.channels.get_mut(id) {
                channel.node.acls.push(AclEntry {
                    apply_here: true,
                    apply_subs: false,
                    user_id,
                    group: if user_id.is_some() {
                        String::new()
                    } else {
                        format!("${}", hash.unwrap_or_default())
                    },
                    grant: perm::WRITE | perm::TRAVERSE,
                    deny: perm::NONE,
                });
            }
            self.permissions_changed(None);
        }
        self.persist_channel(id);
        tracing::info!(actor = %self.log_name(source), channel = id, parent, "added channel");
        message.channel_id = Some(id);
        if let Some(hash) = description_hash {
            message.description = None;
            message.description_hash = Some(hash.to_vec());
        }
        self.broadcast_channel_state(&message);
        if temporary {
            let mut state = proto::UserState {
                session: Some(source),
                channel_id: Some(id),
                ..Default::default()
            };
            self.enter_channel(source, id, &mut state);
            self.broadcast(kind::USER_STATE, &state, None);
        }
    }

    fn update_channel(&mut self, source: u32, id: u32, mut message: proto::ChannelState) {
        let Some(channel) = self.channels.get(id) else {
            return;
        };
        let (old_parent, old_name) = (channel.parent(), channel.name.clone());
        let edit = Action::EditChannel { channel: id };
        if message.name.is_some() && !self.check(source, Action::RenameChannel { channel: id }) {
            return;
        }
        if (message.description.is_some() || message.position.is_some())
            && !self.check(source, edit)
        {
            return;
        }
        if let Some(parent) = message.parent {
            if Some(parent) == old_parent || self.channels.is_ancestor(id, parent) {
                return;
            }
            if !self.check(
                source,
                Action::MoveChannel {
                    channel: id,
                    to: parent,
                },
            ) {
                return;
            }
            let name = message.name.clone().unwrap_or(old_name);
            if self.sibling_named(parent, &name, None) {
                self.deny_type(source, DenyType::ChannelName);
                return;
            }
        }
        let mut add = Vec::new();
        let mut remove = Vec::new();
        if !message.links_add.is_empty() || !message.links_remove.is_empty() {
            if !self.check(source, Action::LinkChannel { channel: id }) {
                return;
            }
            if message.links_add.len() + message.links_remove.len() > 4096 {
                return;
            }
            for &link in &message.links_remove {
                if self.channels.get(link).is_none() {
                    return;
                }
                remove.push(link);
            }
            for &link in &message.links_add {
                if self.channels.get(link).is_none()
                    || !self.check(source, Action::LinkChannel { channel: link })
                {
                    return;
                }
                add.push(link);
            }
        }
        if message.max_users.is_some() && !self.check(source, edit) {
            return;
        }
        // All checks passed.
        if let Some(parent) = message.parent {
            if let Some(old) = old_parent.and_then(|old| self.channels.get_mut(old)) {
                old.children.retain(|child| *child != id);
            }
            if let Some(new) = self.channels.get_mut(parent) {
                new.children.push(id);
            }
            if let Some(channel) = self.channels.get_mut(id) {
                channel.node.parent = Some(parent);
            }
            // Inheritance changed with the parent.
            self.authz_changed(Scope::All);
            tracing::info!(actor = %self.log_name(source), channel = id, parent, "moved channel");
        }
        let Some(channel) = self.channels.get_mut(id) else {
            return;
        };
        if let Some(name) = &message.name {
            channel.name = name.clone();
        }
        if let Some(description) = &message.description {
            channel.set_description(description.clone());
        }
        if let Some(position) = message.position {
            channel.position = position;
        }
        if let Some(max_users) = message.max_users {
            channel.max_users = max_users;
        }
        let description_hash = channel.description_hash;
        for link in add {
            self.link(id, link);
        }
        for link in remove {
            self.unlink(id, link);
        }
        self.invalidate();
        self.persist_channel(id);
        if message.description.is_some()
            && let Some(hash) = description_hash
        {
            message.description = None;
            message.description_hash = Some(hash.to_vec());
        }
        self.broadcast(kind::CHANNEL_STATE, &message, None);
    }

    fn link(&mut self, a: u32, b: u32) {
        let Some(first) = self.channels.get_mut(a) else {
            return;
        };
        first.links.insert(b);
        let first_temporary = first.temporary;
        let Some(second) = self.channels.get_mut(b) else {
            return;
        };
        second.links.insert(a);
        if a != b && !first_temporary && !second.temporary {
            self.storage.link(a, b);
        }
    }

    fn unlink(&mut self, a: u32, b: u32) {
        if let Some(first) = self.channels.get_mut(a) {
            first.links.remove(&b);
        }
        if let Some(second) = self.channels.get_mut(b) {
            second.links.remove(&a);
        }
        if a != b {
            self.storage.unlink(a, b);
        }
    }

    // ---- ACLs ----

    fn acl(&mut self, source: u32, message: proto::Acl) {
        let id = message.channel_id;
        if self.channels.get(id).is_none() {
            return;
        }
        if !self.check(source, Action::EditAcl { channel: id }) {
            return;
        }
        if self.rate_limited(source) {
            return;
        }
        if message.query() {
            self.send_acl(source, id);
        } else {
            self.set_acl(source, id, message);
        }
    }

    fn send_acl(&self, source: u32, id: u32) {
        let Some(channel) = self.channels.get(id) else {
            return;
        };
        let mut user_ids = BTreeSet::new();
        let acls = acl::visible_acls(&self.channels, id)
            .into_iter()
            .map(|(owner, entry)| {
                if let Some(user_id) = entry.user_id {
                    user_ids.insert(user_id);
                }
                proto::acl::ChanAcl {
                    apply_here: Some(entry.apply_here),
                    apply_subs: Some(entry.apply_subs),
                    inherited: Some(owner != id),
                    user_id: entry.user_id,
                    group: entry.user_id.is_none().then(|| entry.group.clone()),
                    grant: Some(entry.grant),
                    deny: Some(entry.deny),
                }
            })
            .collect();
        let parent = channel.parent();
        let groups = acl::group_names(&self.channels, id)
            .into_iter()
            .map(|name| {
                let own = channel.node.groups.get(&name);
                let inherited =
                    parent.and_then(|parent| acl::find_group(&self.channels, parent, &name));
                let mut group = proto::acl::ChanGroup {
                    name: name.clone(),
                    inherit: Some(own.is_none_or(|group| group.inherit)),
                    inheritable: Some(own.is_none_or(|group| group.inheritable)),
                    inherited: Some(inherited.is_some_and(|(_, group)| group.inheritable)),
                    ..Default::default()
                };
                if let Some(own) = own {
                    group.add = own.add.iter().copied().collect();
                    group.remove = own.remove.iter().copied().collect();
                }
                if let Some((owner, _)) = inherited {
                    group.inherited_members = acl::group_members(&self.channels, owner, &name)
                        .into_iter()
                        .collect();
                }
                user_ids.extend(
                    group
                        .add
                        .iter()
                        .chain(&group.remove)
                        .chain(&group.inherited_members)
                        .copied(),
                );
                group
            })
            .collect();
        self.send(
            source,
            kind::ACL,
            &proto::Acl {
                channel_id: id,
                inherit_acls: Some(channel.node.inherit_acl),
                groups,
                acls,
                query: None,
            },
        );
        let mut names = proto::QueryUsers::default();
        for user_id in user_ids {
            if let Some(user) = self.registry.users.get(&user_id) {
                names.ids.push(user_id);
                names.names.push(user.name.clone());
            }
        }
        if !names.ids.is_empty() {
            self.send(source, kind::QUERY_USERS, &names);
        }
    }

    fn set_acl(&mut self, source: u32, id: u32, message: proto::Acl) {
        if message.acls.len() > MAX_ACL_ENTRIES
            || message.groups.len() > MAX_GROUPS
            || message.groups.iter().any(|group| {
                group.name.len() > MAX_NAME_BYTES
                    || group.add.len() + group.remove.len() > MAX_GROUP_MEMBERS
            })
            || message.acls.iter().any(|entry| {
                entry
                    .group
                    .as_ref()
                    .is_some_and(|group| group.len() > MAX_NAME_BYTES)
            })
        {
            tracing::info!(actor = %self.log_name(source), channel = id, "refused an oversized ACL");
            return;
        }
        let valid = |user_id: &u32| self.registry.users.contains_key(user_id);
        let groups: BTreeMap<String, Group> = message
            .groups
            .iter()
            .map(|group| {
                (
                    group.name.clone(),
                    Group {
                        inherit: group.inherit(),
                        inheritable: group.inheritable(),
                        add: group.add.iter().copied().filter(valid).collect(),
                        remove: group.remove.iter().copied().filter(valid).collect(),
                    },
                )
            })
            .collect();
        let acls: Vec<AclEntry> = message
            .acls
            .iter()
            .filter(|entry| entry.user_id.is_none_or(|user_id| valid(&user_id)))
            .map(|entry| AclEntry {
                apply_here: entry.apply_here(),
                apply_subs: entry.apply_subs(),
                user_id: entry.user_id,
                group: if entry.user_id.is_some() {
                    String::new()
                } else {
                    entry.group.clone().unwrap_or_default()
                },
                grant: entry.grant() & perm::ALL,
                deny: entry.deny() & perm::ALL,
            })
            .collect();
        let Some(channel) = self.channels.get_mut(id) else {
            return;
        };
        channel.node.groups = groups;
        channel.node.acls = acls;
        channel.node.inherit_acl = message.inherit_acls();
        self.permissions_changed(None);
        self.authz_changed(Scope::All);
        let Some(user) = self.users.get(&source) else {
            return;
        };
        let (user_id, hash) = (user.user_id, user.certificate_hash.clone());
        if !self.allowed(source, Action::EditChannel { channel: id })
            && (user_id.is_some() || hash.is_some())
        {
            // Never lock the editor out of the channel they just edited.
            if let Some(channel) = self.channels.get_mut(id) {
                channel.node.acls.push(AclEntry {
                    apply_here: true,
                    apply_subs: false,
                    user_id,
                    group: if user_id.is_some() {
                        String::new()
                    } else {
                        format!("${}", hash.unwrap_or_default())
                    },
                    grant: perm::WRITE | perm::TRAVERSE,
                    deny: perm::NONE,
                });
            }
            self.permissions_changed(None);
        }
        self.persist_channel(id);
        tracing::info!(actor = %self.log_name(source), channel = id, "updated ACL");
        let Some(channel) = self.channels.get(id) else {
            return;
        };
        let restricted = channel.is_enter_restricted();
        let sessions: Vec<u32> = self.users.keys().copied().collect();
        for session in sessions {
            let state = proto::ChannelState {
                channel_id: Some(id),
                is_enter_restricted: Some(restricted),
                can_enter: Some(self.allowed(session, Action::Enter { channel: id })),
                ..Default::default()
            };
            self.send(session, kind::CHANNEL_STATE, &state);
        }
    }
}

pub fn truncate_chars(text: &mut String, limit: usize) {
    if let Some((index, _)) = text.char_indices().nth(limit) {
        text.truncate(index);
    }
}

/// At most `limit` bytes, cut at a character boundary.
pub fn truncate_bytes(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

/// The storage keeps names (users, channels, groups) of at most this many
/// bytes (crates/storage `MAX_NAME`); Murmur's limit of 512 characters can be
/// more in other scripts.
pub const MAX_STORED_NAME: usize = 1024;

/// Most access tokens one client may hold, and their length. Tokens are channel
/// passwords; thousands per message would let a client test them in bulk
/// through `can_enter` and `PermissionQuery`.
pub const MAX_TOKENS: usize = 32;
pub const MAX_TOKEN_BYTES: usize = 128;

/// Digests of the client's access tokens, deduplicated; overlong tokens are ignored.
pub fn bounded_tokens(tokens: &[String]) -> Vec<[u8; 32]> {
    let mut out = Vec::new();
    for token in tokens
        .iter()
        .filter(|token| token.len() <= MAX_TOKEN_BYTES)
        .take(MAX_TOKENS)
    {
        let digest = acl::token_digest(token);
        if !out.contains(&digest) {
            out.push(digest);
        }
    }
    out
}

/// What stock clients are told about retention (docs/extension.md §9).
pub fn retention_text(retention: nuschel_ipc::server::Retention) -> String {
    crate::retention::notice(retention, false)
}
