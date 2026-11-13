// SPDX-License-Identifier: EUPL-1.2
//! The extension's requests (docs/extension.md §2–§7): sending, edits and
//! deletions, history pages, subscriptions, read markers, typing, direct
//! conversations and channel settings; and what happens once a write is
//! stored. Every request is bounded before and after decoding, counts against
//! at most 16 in flight, and fails with one uniform error per cause: "no such
//! conversation" and "not allowed" are the same answer.
use crate::{
    authz::Action,
    conversations::{Conversations, Id, Kind, parse},
    history::{Delivery, Done, Read, Stock},
    state::{State, random},
    text,
};
use nuschl_ipc::server::{
    Anchor, HistoryVisibility, MessageKind, PageQuery, ServerStorageRequest as Request,
    ServerStorageResponse as Response, StoredDirect, StoredMessage, StoredPage,
};
use nuschl_protocol::{
    ControlFrame, control as proto,
    nuschl::{
        self, MAX_CONTENT_BYTES, MAX_DIRECTS, MAX_PAGE_BYTES, MAX_SUBSCRIBE_PER_MESSAGE,
        MAX_SUBSCRIPTIONS, capability, kind,
        v1::{self, error::Code},
    },
};
use prost::Message as _;
use std::{
    sync::atomic::Ordering,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

type Failure = (Code, u64);

fn fail(code: Code) -> Failure {
    (code, 0)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().min(i64::MAX as u128) as i64
        })
}

fn conversation(bytes: &[u8]) -> Result<Id, Failure> {
    nuschl::id(bytes).ok_or(fail(Code::Malformed))
}

/// What an idempotency key was used for, to tell a retry from a conflict.
fn body_hash(conversation: &Id, format: i32, reply_to: u64, content: &str) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(conversation);
    context.update(&format.to_be_bytes());
    context.update(&reply_to.to_be_bytes());
    context.update(content.as_bytes());
    hex::encode(context.finish().as_ref())
}

fn wire_kind(kind: MessageKind) -> v1::message::Kind {
    match kind {
        MessageKind::Message => v1::message::Kind::Message,
        MessageKind::Edit => v1::message::Kind::Edit,
        MessageKind::Delete => v1::message::Kind::Delete,
        MessageKind::Void => v1::message::Kind::Void,
    }
}

impl State {
    /// A stored row as clients see it; a deleted account's messages show no
    /// name or registration number (a new registrant never inherits them).
    pub fn wire_message(&self, row: StoredMessage) -> v1::Message {
        let account = row.author_account.as_deref().and_then(parse);
        let deleted = account.is_some_and(|account| self.registry.by_account(&account).is_none());
        let author = (row.kind != MessageKind::Void).then(|| v1::Author {
            account: account.map(|account| account.to_vec()).unwrap_or_default(),
            user_id: if deleted {
                0
            } else {
                row.author_user_id.unwrap_or(0)
            },
            name: if deleted {
                String::new()
            } else {
                row.author_name
            },
        });
        // MLS rows keep their message as base64 in `content` (crate::mls).
        let (content, mls, archive) = crate::mls::row_content(row.format, row.content);
        v1::Message {
            seq: row.seq,
            kind: wire_kind(row.kind) as i32,
            target: row.target.unwrap_or(0),
            author,
            format: i32::from(row.format),
            content,
            mls,
            archive,
            reply_to: row.reply_to.unwrap_or(0),
            sent_ms: row.sent_ms,
            edited_ms: row.edited_ms.unwrap_or(0),
            deleted_ms: row.deleted_ms.unwrap_or(0),
            delete_reason: row.delete_reason.unwrap_or(0),
            edits: row.edits,
        }
    }

    pub(crate) fn extension_error(&self, session: u32, request_id: u64, (code, retry): Failure) {
        self.send(
            session,
            kind::ERROR,
            &nuschl::error(request_id, code, retry),
        );
    }

    /// Answer a failed request and release its id.
    pub(crate) fn failed(&mut self, session: u32, request_id: u64, failure: Failure) {
        self.extension_error(session, request_id, failure);
        self.end_request(session, request_id);
    }

    pub(crate) fn end_request(&mut self, session: u32, request_id: u64) {
        if let Some(extension) = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
        {
            extension.end(request_id);
        }
    }

    fn begin_request(&mut self, session: u32, request_id: u64) -> bool {
        let Some(extension) = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
        else {
            return false;
        };
        match extension.begin(request_id) {
            Ok(()) => true,
            Err(code) => {
                self.extension_error(session, request_id, fail(code));
                false
            }
        }
    }

    /// Run a request that answers at once: begin, answer, release.
    pub(crate) fn immediate(
        &mut self,
        session: u32,
        request_id: u64,
        run: impl FnOnce(&mut Self) -> Result<(), Failure>,
    ) {
        if !self.begin_request(session, request_id) {
            return;
        }
        match run(self) {
            Ok(()) => self.end_request(session, request_id),
            Err(failure) => self.failed(session, request_id, failure),
        }
    }

    /// Run a request whose answer comes from the history tasks; it keeps its
    /// id until then.
    pub(crate) fn deferred(
        &mut self,
        session: u32,
        request_id: u64,
        run: impl FnOnce(&mut Self) -> Result<(), Failure>,
    ) {
        if !self.begin_request(session, request_id) {
            return;
        }
        if let Err(failure) = run(self) {
            self.failed(session, request_id, failure);
        }
    }

    pub(crate) fn text_limited(&mut self, session: u32) -> Result<(), Failure> {
        if self.rate_limited(session) {
            let retry = 1000 / u64::from(self.settings.config.message_limit.max(1));
            return Err((Code::RateLimited, retry.max(1)));
        }
        Ok(())
    }

    fn extension_limited(
        &mut self,
        session: u32,
        bucket: impl FnOnce(&mut crate::extension::Extension) -> &mut crate::limits::LeakyBucket,
    ) -> Result<(), Failure> {
        let limited = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
            .is_none_or(|extension| bucket(extension).limited(Instant::now()));
        if limited {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            return Err((Code::RateLimited, 1000));
        }
        Ok(())
    }

    fn author(&self, session: u32) -> (Option<Id>, Option<u32>, String) {
        self.users
            .get(&session)
            .map_or((None, None, String::new()), |user| {
                (user.account, user.user_id, user.name.clone())
            })
    }

    /// A frame in the reserved block from an authenticated connection.
    pub fn extension_frame(&mut self, session: u32, frame: &ControlFrame) {
        let Some(extension) = self
            .users
            .get(&session)
            .and_then(|user| user.extension.as_ref())
        else {
            // Nothing in the block is ever processed for a stock connection.
            self.counters
                .extension_frames
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        let request_id = v1::Done::decode(frame.payload.clone()).map_or(0, |done| done.request_id);
        let negotiated = nuschl::required_capability(frame.kind)
            .is_some_and(|capability| extension.has(capability));
        if !negotiated {
            self.extension_error(session, request_id, fail(Code::Unsupported));
            return;
        }
        macro_rules! decoded {
            ($type:ty) => {
                match <$type>::decode(frame.payload.clone()) {
                    Ok(message) => message,
                    Err(_) => {
                        self.extension_error(session, request_id, fail(Code::Malformed));
                        return;
                    }
                }
            };
        }
        match frame.kind {
            kind::PRIVACY => {
                let message = decoded!(v1::Privacy);
                self.set_address_privacy(session, message.relay_only);
            }
            kind::CHAT_SEND => {
                let message = decoded!(v1::ChatSend);
                self.deferred(session, message.request_id, |state| {
                    state.chat_send(session, message)
                });
            }
            kind::CHAT_EDIT => {
                let message = decoded!(v1::ChatEdit);
                self.deferred(session, message.request_id, |state| {
                    state.chat_edit(session, message)
                });
            }
            kind::CHAT_DELETE => {
                let message = decoded!(v1::ChatDelete);
                self.deferred(session, message.request_id, |state| {
                    state.chat_delete(session, message)
                });
            }
            kind::HISTORY_REQUEST => {
                let message = decoded!(v1::HistoryRequest);
                self.deferred(session, message.request_id, |state| {
                    state.history_request(session, message)
                });
            }
            kind::SUBSCRIBE => {
                let message = decoded!(v1::Subscribe);
                self.immediate(session, message.request_id, |state| {
                    state.subscribe(session, message)
                });
            }
            kind::UNSUBSCRIBE => {
                let message = decoded!(v1::Unsubscribe);
                for id in message
                    .conversations
                    .iter()
                    .take(MAX_SUBSCRIBE_PER_MESSAGE)
                    .filter_map(|bytes| nuschl::id(bytes))
                {
                    self.unsubscribe(session, &id);
                }
            }
            kind::READ_MARKER => {
                let message = decoded!(v1::ReadMarker);
                self.read_marker(session, message);
            }
            kind::TYPING => {
                let message = decoded!(v1::Typing);
                self.typing(session, message);
            }
            kind::OPEN_DIRECT => {
                let message = decoded!(v1::OpenDirect);
                self.immediate(session, message.request_id, |state| {
                    state.open_direct(session, message)
                });
            }
            kind::LIST_DIRECT => {
                let message = decoded!(v1::ListDirect);
                self.immediate(session, message.request_id, |state| {
                    state.list_direct(session, message.request_id)
                });
            }
            kind::CHANNEL_SETTINGS => {
                let message = decoded!(v1::ChannelSettings);
                self.immediate(session, message.request_id, |state| {
                    state.channel_settings(session, message)
                });
            }
            kind::KT_STATUS
            | kind::KT_PUBLISH
            | kind::KT_LOOKUP
            | kind::KT_HISTORY
            | kind::KT_AUDIT
            | kind::KT_HEAD => self.kt_frame(session, frame),
            kind::MLS_KEY_PACKAGE_UPLOAD
            | kind::MLS_KEY_PACKAGE_CLAIM
            | kind::MLS_COMMIT
            | kind::MLS_SEND
            | kind::MLS_WELCOME_FETCH
            | kind::MLS_WELCOME_ACK
            | kind::MLS_JOIN_INFO_REQUEST
            | kind::MLS_OPENED => self.mls_frame(session, frame),
            kind::SHARE_LINK_CREATE | kind::SHARE_LINK_REVOKE | kind::SHARE_LINK_RESOLVE => {
                self.share_frame(session, frame)
            }
            kind::KNOCK_REQUEST | kind::KNOCK_DECIDE | kind::KNOCK_CANCEL => {
                self.knock_frame(session, frame)
            }
            _ => self.extension_error(session, request_id, fail(Code::Unsupported)),
        }
    }

    // ---- §4 sending ----

    fn chat_send(&mut self, session: u32, message: v1::ChatSend) -> Result<(), Failure> {
        let id = conversation(&message.conversation)?;
        // An end-to-end server never takes plaintext (docs/e2ee.md §7.2, §10).
        if self.end_to_end() {
            return Err(fail(Code::Unsupported));
        }
        let key = nuschl::id(&message.idempotency_key).ok_or(fail(Code::Malformed))?;
        if message.format != v1::Format::Text as i32 || message.content.is_empty() {
            return Err(fail(Code::Malformed));
        }
        if message.content.len() > MAX_CONTENT_BYTES {
            return Err(fail(Code::TooLarge));
        }
        self.text_limited(session)?;
        self.authorize(session, Action::Send(&id))?;
        if message.reply_to > self.conversations.head(&id) {
            return Err(fail(Code::Malformed));
        }
        // Stock users' copy is made here, from the same text, under Murmur's
        // text rules (docs/extension.md §4).
        let html =
            nuschl_legacy_html::legacy_body(&message.content, self.settings.config.allow_html);
        let html = match text::check(&html, self.text_limits()) {
            Err(text::TooLong) => return Err(fail(Code::TooLarge)),
            Ok(Some(changed)) => changed,
            Ok(None) => html,
        };
        let (account, user_id, name) = self.author(session);
        let seq = self.conversations.next_seq(id);
        // Unregistered senders have no durable identity to scope a key to, and
        // a live-only conversation stores none.
        let keyed = account.filter(|_| !self.live_only(&id));
        let row = StoredMessage {
            conversation: hex::encode(id),
            seq,
            kind: MessageKind::Message,
            author_account: account.map(hex::encode),
            author_user_id: user_id,
            author_name: name,
            format: v1::Format::Text as u8,
            reply_to: (message.reply_to > 0).then_some(message.reply_to),
            sent_ms: now_ms(),
            idempotency_key: keyed.map(|_| hex::encode(key)),
            body_hash: keyed
                .map(|_| body_hash(&id, message.format, message.reply_to, &message.content)),
            content: message.content,
            ..StoredMessage::default()
        };
        let wire = self.wire_message(row.clone());
        self.history.store(
            self.append_or_advance(row),
            Done::Request {
                session,
                request_id: message.request_id,
                conversation: id,
                seq,
                delivery: Delivery::Message {
                    message: wire,
                    stock: Some(Stock {
                        actor: session,
                        html,
                    }),
                },
            },
        );
        Ok(())
    }

    fn chat_edit(&mut self, session: u32, message: v1::ChatEdit) -> Result<(), Failure> {
        let id = conversation(&message.conversation)?;
        // Edits in end-to-end conversations are envelopes inside MLS.
        if self.end_to_end() {
            return Err(fail(Code::Unsupported));
        }
        if message.format != v1::Format::Text as i32 || message.content.is_empty() {
            return Err(fail(Code::Malformed));
        }
        if message.content.len() > MAX_CONTENT_BYTES {
            return Err(fail(Code::TooLarge));
        }
        self.text_limited(session)?;
        // Unregistered authors cannot edit; only while they may still post,
        // and not where nothing is stored.
        self.authorize(session, Action::Edit(&id))?;
        let (account, user_id, name) = self.author(session);
        let account = account.ok_or(fail(Code::NotFound))?;
        if self.live_only(&id)
            || message.target == 0
            || message.target > self.conversations.head(&id)
        {
            return Err(fail(Code::NotFound));
        }
        let seq = self.conversations.next_seq(id);
        let sent_ms = now_ms();
        let event = StoredMessage {
            conversation: hex::encode(id),
            seq,
            kind: MessageKind::Edit,
            target: Some(message.target),
            author_account: Some(hex::encode(account)),
            author_user_id: user_id,
            author_name: name,
            format: v1::Format::Text as u8,
            sent_ms,
            ..StoredMessage::default()
        };
        let mut wire = self.wire_message(event.clone());
        wire.content.clone_from(&message.content);
        wire.edited_ms = sent_ms;
        self.history.store(
            Request::HistoryEdit {
                target: message.target,
                author_account: hex::encode(account),
                format: v1::Format::Text as u8,
                content: message.content,
                edit_limit: self.settings.config.history.edit_limit,
                event,
            },
            Done::Request {
                session,
                request_id: message.request_id,
                conversation: id,
                seq,
                delivery: Delivery::Event { message: wire },
            },
        );
        Ok(())
    }

    fn chat_delete(&mut self, session: u32, message: v1::ChatDelete) -> Result<(), Failure> {
        let id = conversation(&message.conversation)?;
        // Deletions in end-to-end conversations are envelopes inside MLS, and a
        // commit row must never become a tombstone (docs/e2ee.md §6.3).
        if self.end_to_end() {
            return Err(fail(Code::Unsupported));
        }
        self.text_limited(session)?;
        let (account, user_id, name) = self.author(session);
        // Authors may delete their own messages at any time, even after
        // losing access; moderators any message in their channels.
        let moderator = self.allowed(session, Action::Moderate(&id));
        if !moderator {
            self.authorize(session, Action::DeleteOwn(&id))?;
        }
        if message.target == 0 || message.target > self.conversations.head(&id) {
            return Err(fail(Code::NotFound));
        }
        let reason = u32::from(moderator);
        let seq = self.conversations.next_seq(id);
        let sent_ms = now_ms();
        let event = StoredMessage {
            conversation: hex::encode(id),
            seq,
            kind: MessageKind::Delete,
            target: Some(message.target),
            author_account: account.map(hex::encode),
            author_user_id: user_id,
            author_name: name,
            sent_ms,
            ..StoredMessage::default()
        };
        let mut wire = self.wire_message(event.clone());
        wire.deleted_ms = sent_ms;
        wire.delete_reason = reason;
        self.history.store(
            Request::HistoryDelete {
                target: message.target,
                author_account: if moderator {
                    None
                } else {
                    account.map(hex::encode)
                },
                reason,
                event,
            },
            Done::Request {
                session,
                request_id: message.request_id,
                conversation: id,
                seq,
                delivery: Delivery::Event { message: wire },
            },
        );
        Ok(())
    }

    // ---- what happens once stored ----

    /// Finish a stored write, in the order writes were queued.
    pub fn stored(&mut self, done: Done, response: Option<Response>) {
        match done {
            Done::Nothing => {}
            Done::Mls(stored) => self.mls_stored(*stored, response),
            Done::Request {
                session,
                request_id,
                conversation,
                seq,
                delivery,
            } => {
                let acknowledged = match response {
                    Some(Response::Appended {
                        duplicate: false, ..
                    })
                    | Some(Response::Done) => {
                        self.deliver(&conversation, delivery, session);
                        Ok(seq)
                    }
                    // A retry of a message stored before: its original seq.
                    Some(Response::Appended {
                        seq: original,
                        duplicate: true,
                    }) => Ok(original),
                    Some(Response::Conflict) => Err(fail(Code::IdempotencyConflict)),
                    Some(Response::Missing) => Err(fail(Code::NotFound)),
                    Some(Response::LimitReached) => Err(fail(Code::LimitReached)),
                    _ => Err((Code::Busy, 1000)),
                };
                match acknowledged {
                    Ok(seq) => {
                        self.send(
                            session,
                            kind::CHAT_ACK,
                            &v1::ChatAck {
                                request_id,
                                conversation: conversation.to_vec(),
                                seq,
                                server_ms: now_ms(),
                            },
                        );
                        self.end_request(session, request_id);
                    }
                    Err(failure) => self.failed(session, request_id, failure),
                }
            }
            Done::Stock {
                source,
                conversation,
                message,
                fallback,
            } => match response {
                // Stored, or a live-only conversation's seq recorded.
                Some(Response::Appended {
                    duplicate: false, ..
                })
                | Some(Response::Done) => self.deliver(
                    &conversation,
                    Delivery::Message {
                        message,
                        stock: None,
                    },
                    source,
                ),
                // Not stored: Nuschl users in the channel still see it live.
                _ => {
                    for session in self.readers(&conversation) {
                        if session != source
                            && self.allowed(
                                session,
                                Action::SeeLive {
                                    conversation: &conversation,
                                    seq: message.seq,
                                },
                            )
                            && let Some(user) = self.users.get(&session)
                        {
                            user.link.send(fallback.clone());
                        }
                    }
                }
            },
        }
    }

    /// Live fan-out, under the lock, to whoever may read the conversation now
    /// (never `origin`, which has the acknowledgement).
    pub(crate) fn deliver(&mut self, id: &Id, delivery: Delivery, origin: u32) {
        let (message, stock) = match delivery {
            Delivery::Message { message, stock } => (message, stock),
            Delivery::Event { message } => (message, None),
        };
        // Events show a target's content: only to those who may see it.
        let shown = if message.kind == v1::message::Kind::Message as i32 {
            message.seq
        } else {
            message.target
        };
        let Some(frame) = nuschl::encode(
            kind::CHAT_EVENT,
            &v1::ChatEvent {
                conversation: id.to_vec(),
                message: Some(message),
            },
        ) else {
            return;
        };
        for session in self.readers(id) {
            if session != origin
                && self.allowed(
                    session,
                    Action::SeeLive {
                        conversation: id,
                        seq: shown,
                    },
                )
                && let Some(user) = self.users.get(&session)
            {
                user.link.send(frame.clone());
            }
        }
        let Some(stock) = stock else {
            return;
        };
        let audience: Vec<u32> = self
            .stock_audience(id)
            .into_iter()
            .filter(|session| {
                *session != origin
                    && self.allowed(
                        *session,
                        Action::SeeLive {
                            conversation: id,
                            seq: shown,
                        },
                    )
            })
            .collect();
        if audience.is_empty() {
            return;
        }
        let mut text = proto::TextMessage {
            actor: self.users.contains_key(&stock.actor).then_some(stock.actor),
            message: stock.html,
            ..Default::default()
        };
        match self.conversations.kind(id) {
            Some(Kind::Channel(channel)) => text.channel_id.push(channel),
            _ => text.session.clone_from(&audience),
        }
        if let Some(frame) =
            nuschl_protocol::encode_control(nuschl_protocol::kind::TEXT_MESSAGE, &text)
        {
            for session in audience {
                if let Some(user) = self.users.get(&session) {
                    user.link.send(frame.clone());
                }
            }
        }
    }

    /// Store a stock client's message in a channel's conversation; Nuschl
    /// readers get it once stored (docs/extension.md §4).
    pub fn store_stock(&mut self, source: u32, id: Id, html: &str, fallback: &bytes::Bytes) {
        if html.len() > nuschl::MAX_LEGACY_CONTENT_BYTES {
            // Too large to keep: Nuschl users in the channel see it live only.
            self.stored(
                Done::Stock {
                    source,
                    conversation: id,
                    message: v1::Message {
                        seq: self.conversations.head(&id).saturating_add(1),
                        ..Default::default()
                    },
                    fallback: fallback.clone(),
                },
                None,
            );
            return;
        }
        let (account, user_id, name) = self.author(source);
        let seq = self.conversations.next_seq(id);
        let row = StoredMessage {
            conversation: hex::encode(id),
            seq,
            kind: MessageKind::Message,
            author_account: account.map(hex::encode),
            author_user_id: user_id,
            author_name: name,
            format: v1::Format::LegacyHtml as u8,
            content: html.to_owned(),
            sent_ms: now_ms(),
            ..StoredMessage::default()
        };
        let message = self.wire_message(row.clone());
        self.history.store(
            self.append_or_advance(row),
            Done::Stock {
                source,
                conversation: id,
                message,
                fallback: fallback.clone(),
            },
        );
    }

    /// Store a new message, or in a live-only conversation only record its
    /// seq (so it is never reused) and keep nothing else.
    fn append_or_advance(&self, row: StoredMessage) -> Request {
        let live_only = parse(&row.conversation).is_some_and(|id| self.live_only(&id));
        if live_only {
            Request::HistoryAdvance {
                conversation: row.conversation,
                seq: row.seq,
            }
        } else {
            Request::HistoryAppend { message: row }
        }
    }

    // ---- §6 history ----

    fn history_request(
        &mut self,
        session: u32,
        message: v1::HistoryRequest,
    ) -> Result<(), Failure> {
        use v1::history_request::Anchor as Wire;
        let id = conversation(&message.conversation)?;
        let anchor = match message.anchor {
            Some(Wire::Latest(_)) => Anchor::Latest,
            Some(Wire::Before(seq)) => Anchor::Before(seq),
            Some(Wire::After(seq)) => Anchor::After(seq),
            Some(Wire::Around(seq)) => Anchor::Around(seq),
            None => return Err(fail(Code::Malformed)),
        };
        if message.limit as usize > nuschl::MAX_PAGE_MESSAGES {
            return Err(fail(Code::Malformed));
        }
        self.extension_limited(session, |extension| &mut extension.pages)?;
        if let Some(extension) = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
            && extension.page_bytes.exhausted(Instant::now())
        {
            return Err((Code::RateLimited, extension.page_bytes.retry_after_ms()));
        }
        // And per network (or account), so connections cannot multiply it.
        if let Some(key) = self.page_key(session)
            && let Err(retry) = self.page_limits.page(key, Instant::now())
        {
            self.counters.rate_limited.fetch_add(1, Ordering::Relaxed);
            return Err((Code::RateLimited, retry));
        }
        self.authorize(session, Action::ReadHistory(&id))?;
        if self.live_only(&id) {
            // Not archived: no pages, even of what was kept before.
            self.send(
                session,
                kind::HISTORY_PAGE,
                &v1::HistoryPage {
                    request_id: message.request_id,
                    conversation: id.to_vec(),
                    head: self.conversations.head(&id),
                    ..Default::default()
                },
            );
            self.end_request(session, message.request_id);
            return Ok(());
        }
        let (ranges, clipped) = self.visible_ranges(session, &id);
        let read = Read {
            session,
            request_id: message.request_id,
            conversation: id,
            query: PageQuery {
                conversation: hex::encode(id),
                anchor,
                limit: if message.limit == 0 {
                    50
                } else {
                    message.limit
                },
                ranges,
                max_bytes: MAX_PAGE_BYTES as u32,
            },
            epoch: self.authz_epoch,
            clipped,
        };
        if !self.history.read(read) {
            return Err((Code::Busy, 500));
        }
        Ok(())
    }

    /// Whose per-network page budget a connection draws from (page_limits.rs).
    fn page_key(&self, session: u32) -> Option<crate::page_limits::Key> {
        let user = self.users.get(&session)?;
        self.page_limits.key(user.address.ip(), user.account)
    }

    /// A page came back: re-check against the current authorisation state
    /// (the epoch moved: `can_read` and the visible ranges again) and send it.
    pub fn page_read(&mut self, read: Read, page: Option<StoredPage>) {
        let (session, request_id, id) = (read.session, read.request_id, read.conversation);
        if !self.users.contains_key(&session) {
            return;
        }
        let Some(page) = page else {
            self.failed(session, request_id, (Code::Busy, 1000));
            return;
        };
        let (ranges, clipped) = if read.epoch == self.authz_epoch {
            (read.query.ranges, read.clipped)
        } else {
            if let Err(denial) = self.authorize(session, Action::ReadHistory(&id)) {
                self.failed(session, request_id, denial.into());
                return;
            }
            self.visible_ranges(session, &id)
        };
        let visible = |seq: u64| ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&seq));
        // Past the conversation's retention nothing is served, whether or not
        // the hourly expiry has reached it yet (and none at all if live-only).
        let now = now_ms();
        let cutoff = self.retention(&id).cutoff_ms(now);
        let before = page.rows.len();
        let mut bytes = 0;
        let messages: Vec<v1::Message> = page
            .rows
            .into_iter()
            .filter(|row| visible(row.seq))
            .map(|row| {
                // An event about a message the reader may not see says nothing.
                let mut row = match row.target {
                    Some(target) if !visible(target) => {
                        StoredMessage::void(&row.conversation, row.seq, row.sent_ms)
                    }
                    _ => row,
                };
                if cutoff.is_some_and(|cutoff| row.sent_ms < cutoff)
                    && nuschl_ipc::server::expirable(&row)
                {
                    row.content.clear();
                    row.reply_to = None;
                    row.deleted_ms = Some(now);
                    row.delete_reason = Some(2);
                }
                bytes += row.content.len() as u64;
                self.wire_message(row)
            })
            .collect();
        let dropped = messages.len() != before;
        if let Some(extension) = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
        {
            extension.page_bytes.spend(bytes, Instant::now());
        }
        if let Some(key) = self.page_key(session) {
            self.page_limits.spend(key, bytes, Instant::now());
        }
        self.send(
            session,
            kind::HISTORY_PAGE,
            &v1::HistoryPage {
                request_id,
                conversation: id.to_vec(),
                messages,
                more_before: page.more_before,
                more_after: page.more_after,
                restricted: clipped && (dropped || !page.more_before),
                head: self.conversations.head(&id),
            },
        );
        self.end_request(session, request_id);
    }

    // ---- §5 subscriptions ----

    fn subscribe(&mut self, session: u32, message: v1::Subscribe) -> Result<(), Failure> {
        if message.conversations.len() > MAX_SUBSCRIBE_PER_MESSAGE {
            return Err(fail(Code::Malformed));
        }
        let ids = message
            .conversations
            .iter()
            .map(|bytes| conversation(bytes))
            .collect::<Result<Vec<Id>, Failure>>()?;
        self.extension_limited(session, |extension| &mut extension.requests)?;
        let account = self.users.get(&session).and_then(|user| user.account);
        let mut account_total: usize = self
            .users
            .values()
            .filter(|user| account.is_some() && user.account == account)
            .filter_map(|user| user.extension.as_ref())
            .map(|extension| extension.subscriptions.len())
            .sum();
        let mut accepted = Vec::new();
        for id in ids {
            if !self.allowed(session, Action::ReadHistory(&id)) {
                continue;
            }
            let Some(extension) = self
                .users
                .get_mut(&session)
                .and_then(|user| user.extension.as_mut())
            else {
                break;
            };
            if !extension.subscriptions.contains(&id) {
                if extension.subscriptions.len() >= MAX_SUBSCRIPTIONS
                    || account_total >= crate::conversations::MAX_ACCOUNT_SUBSCRIPTIONS
                {
                    continue;
                }
                extension.subscriptions.insert(id);
                account_total += 1;
                self.conversations
                    .subscribers
                    .entry(id)
                    .or_default()
                    .insert(session);
            }
            accepted.push(v1::ConversationState {
                conversation: id.to_vec(),
                head: self.conversations.head(&id),
                read: self.conversations.marker(account, &id),
            });
        }
        self.send(
            session,
            kind::SUBSCRIBED,
            &v1::Subscribed {
                request_id: message.request_id,
                conversations: accepted,
            },
        );
        Ok(())
    }

    // ---- §7 read markers and typing ----

    fn read_marker(&mut self, session: u32, message: v1::ReadMarker) {
        if self
            .extension_limited(session, |extension| &mut extension.markers)
            .is_err()
        {
            return;
        }
        let Some(id) = nuschl::id(&message.conversation) else {
            return;
        };
        let Some(account) = self.users.get(&session).and_then(|user| user.account) else {
            return;
        };
        if !self.allowed(session, Action::ReadMarker(&id))
            || message.seq > self.conversations.head(&id)
            || message.seq <= self.conversations.marker(Some(account), &id)
        {
            return;
        }
        let markers = self.conversations.markers.entry(account).or_default();
        if markers.len() >= 16_384 && !markers.contains_key(&id) {
            return;
        }
        markers.insert(id, message.seq);
        self.history.write(Request::HistorySetReadMarker {
            account: hex::encode(account),
            conversation: hex::encode(id),
            seq: message.seq,
        });
        // Synced to the account's other devices.
        let others: Vec<u32> = self
            .users
            .values()
            .filter(|user| {
                user.session != session
                    && user.account == Some(account)
                    && user
                        .extension
                        .as_ref()
                        .is_some_and(|extension| extension.has(capability::HISTORY))
            })
            .map(|user| user.session)
            .collect();
        for other in others {
            self.send(other, kind::READ_MARKER, &message);
        }
    }

    fn typing(&mut self, session: u32, message: v1::Typing) {
        let Some(id) = nuschl::id(&message.conversation) else {
            return;
        };
        let now = Instant::now();
        let Some(extension) = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
        else {
            return;
        };
        if extension
            .typing
            .get(&id)
            .is_some_and(|last| now.duration_since(*last).as_secs() < 3)
        {
            return;
        }
        if extension.typing.len() >= 64 {
            extension
                .typing
                .retain(|_, last| now.duration_since(*last).as_secs() < 3);
        }
        extension.typing.insert(id, now);
        if !self.allowed(session, Action::Typing(&id)) {
            return;
        }
        let typing = v1::Typing {
            conversation: id.to_vec(),
            session,
        };
        for reader in self.readers(&id) {
            if reader != session
                && self.users.get(&reader).is_some_and(|user| {
                    user.extension
                        .as_ref()
                        .is_some_and(|extension| extension.has(capability::TYPING))
                })
            {
                self.send(reader, kind::TYPING, &typing);
            }
        }
    }

    // ---- §3 direct conversations ----

    fn direct(&self, id: &Id, me: &Id) -> Option<v1::Direct> {
        let Some(Kind::Direct(a, b)) = self.conversations.kind(id) else {
            return None;
        };
        let peer = if a == *me { b } else { a };
        let registered = self.registry.by_account(&peer);
        Some(v1::Direct {
            conversation: id.to_vec(),
            user_id: registered.map_or(0, |user| user.id),
            name: registered.map(|user| user.name.clone()).unwrap_or_default(),
            account: peer.to_vec(),
            head: self.conversations.head(id),
            read: self.conversations.marker(Some(*me), id),
        })
    }

    /// The direct conversation of two accounts, created on first use.
    pub fn direct_conversation(&mut self, me: Id, peer: Id) -> Id {
        if let Some(id) = self.conversations.direct(me, peer) {
            return id;
        }
        let id: Id = random();
        self.conversations.add_direct(id, me, peer);
        let (a, b) = Conversations::pair(me, peer);
        self.history.write(Request::HistoryCreateDirect {
            direct: StoredDirect {
                conversation: hex::encode(id),
                account_a: hex::encode(a),
                account_b: hex::encode(b),
            },
        });
        id
    }

    fn open_direct(&mut self, session: u32, message: v1::OpenDirect) -> Result<(), Failure> {
        self.extension_limited(session, |extension| &mut extension.requests)?;
        let peer = self
            .registry
            .users
            .get(&message.user_id)
            .map(|user| user.account)
            .ok_or(fail(Code::NotFound))?;
        self.authorize(session, Action::OpenDirect { peer: &peer })?;
        let me = self
            .users
            .get(&session)
            .and_then(|user| user.account)
            .ok_or(fail(Code::NotFound))?;
        if self.conversations.direct(me, peer).is_none()
            && self.conversations.directs_of(&me).len() >= MAX_DIRECTS
        {
            return Err(fail(Code::LimitReached));
        }
        let id = self.direct_conversation(me, peer);
        let direct = self.direct(&id, &me);
        self.send(
            session,
            kind::DIRECT_OPENED,
            &v1::DirectOpened {
                request_id: message.request_id,
                direct,
            },
        );
        Ok(())
    }

    fn list_direct(&mut self, session: u32, request_id: u64) -> Result<(), Failure> {
        self.extension_limited(session, |extension| &mut extension.requests)?;
        self.authorize(session, Action::ListDirects)?;
        let me = self
            .users
            .get(&session)
            .and_then(|user| user.account)
            .ok_or(fail(Code::NotFound))?;
        let directs = self
            .conversations
            .directs_of(&me)
            .iter()
            .take(MAX_DIRECTS)
            .filter_map(|id| self.direct(id, &me))
            .collect();
        self.send(
            session,
            kind::DIRECT_LIST,
            &v1::DirectList {
                request_id,
                directs,
            },
        );
        Ok(())
    }

    // ---- channel settings ----

    /// A channel's extension properties for Nuschl connections.
    pub fn channel_info(&self, channel: &crate::state::Channel) -> v1::ChannelInfo {
        let (retention_days, live_only) = self.wire_retention(&channel.conversation_id);
        v1::ChannelInfo {
            conversation: channel.conversation_id.to_vec(),
            visibility: match channel.history_visibility {
                HistoryVisibility::Members => v1::Visibility::Members,
                HistoryVisibility::SinceAccess => v1::Visibility::SinceAccess,
            } as i32,
            retention_days,
            live_only,
            own_retention: self.own_retention(&channel.conversation_id).is_some(),
        }
    }

    /// Send a `ChannelState` to everyone; Nuschl connections also get the
    /// channel's extension properties, stock clients never see them.
    pub fn broadcast_channel_state(&self, message: &proto::ChannelState) {
        let Some(plain) =
            nuschl_protocol::encode_control(nuschl_protocol::kind::CHANNEL_STATE, message)
        else {
            return;
        };
        let extended = message
            .channel_id
            .and_then(|id| self.channels.get(id))
            .and_then(|channel| nuschl::channel_state_frame(message, self.channel_info(channel)));
        for user in self.users.values() {
            match (&extended, &user.extension) {
                (Some(frame), Some(_)) => user.link.send(frame.clone()),
                _ => user.link.send(plain.clone()),
            }
        }
    }

    fn channel_settings(
        &mut self,
        session: u32,
        message: v1::ChannelSettings,
    ) -> Result<(), Failure> {
        self.extension_limited(session, |extension| &mut extension.requests)?;
        let channel = message.channel;
        self.authorize(session, Action::ChannelSettings { channel })?;
        let Some(conversation) = self.conversation_of_channel(channel) else {
            return Err(fail(Code::NotFound));
        };
        let visibility = match message.visibility.map(v1::Visibility::try_from) {
            None => None,
            Some(Ok(v1::Visibility::Members)) => Some(HistoryVisibility::Members),
            Some(Ok(v1::Visibility::SinceAccess)) => Some(HistoryVisibility::SinceAccess),
            Some(Err(_)) => return Err(fail(Code::Malformed)),
        };
        let retention = match &message.retention {
            None => None,
            Some(setting) => {
                Some(crate::retention::from_wire(setting).ok_or(fail(Code::Malformed))?)
            }
        };
        // Shortening removes what is stored now: the client must have said so
        // to the person changing it (docs/extension.md §6).
        if let Some(own) = retention
            && self
                .retention_with(own)
                .shorter_than(self.retention(&conversation))
            && !message.confirm_expiry
        {
            return Err(fail(Code::ConfirmationRequired));
        }
        let mut changed = false;
        if let Some(visibility) = visibility
            && self.channels.get_mut(channel).is_some_and(|channel| {
                std::mem::replace(&mut channel.history_visibility, visibility) != visibility
            })
        {
            if visibility == HistoryVisibility::SinceAccess {
                self.begin_since_access(channel);
            }
            // Pages in flight recompute what their readers may see.
            self.authz_epoch += 1;
            self.persist_channel(channel);
            tracing::info!(actor = %self.log_name(session), channel, ?visibility, "changed history visibility");
            changed = true;
        }
        if let Some(own) = retention
            && self.own_retention(&conversation) != own
        {
            let kept = self.set_retention(conversation, own);
            tracing::info!(actor = %self.log_name(session), channel, retention = ?own, effective = ?self.retention(&conversation), "changed channel retention");
            if kept {
                self.retention_changed(channel);
            }
            changed = true;
        }
        if changed {
            self.broadcast_channel_state(&proto::ChannelState {
                channel_id: Some(channel),
                ..Default::default()
            });
        }
        self.send(
            session,
            kind::DONE,
            &v1::Done {
                request_id: message.request_id,
            },
        );
        Ok(())
    }
}
