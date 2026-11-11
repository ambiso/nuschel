// SPDX-License-Identifier: EUPL-1.2
//! Editing and deleting messages on Nuschel servers (docs/extension.md §6).
//! Stock Mumble has neither, so nothing here is offered there. Authors edit
//! and delete their own messages (registered accounts only, as the server
//! requires); moderators delete others' only where the server's policy says
//! they may (`moderators_delete`) and they hold Write on the channel. The
//! server checks every request again; its refusals show like a failed send.
use crate::{
    backend::{CacheKey, ChangeRequest, Deletion},
    menus::PromptAction,
    state::{ChannelView, Desktop, MessageView, ServerView, Status},
};
use gpui_kit::*;
use nuschel_client::HistorySource;
use nuschel_core::MessageDocument;
use nuschel_session::{connection::TextTarget, model::permission as perm};

/// The message the composer is editing, and what it held before.
pub struct Editing {
    pub server: String,
    pub channel: u32,
    pub seq: u64,
    /// The message's serial, to mark it in the timeline.
    pub serial: u64,
    pub draft: String,
}

impl ServerView {
    /// Whether the extension negotiated `capability` on this connection.
    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|have| have == capability)
    }
    /// Our registration number, if we are registered.
    pub fn own_user_id(&self) -> Option<u32> {
        self.own_session
            .and_then(|session| self.users.get(&session))
            .and_then(|user| user.user_id)
    }
    /// Whether this conversation's timeline is the server's history (a
    /// Nuschel server's channel or direct conversation, connected), rather
    /// than what this device observed.
    pub fn archived(&self, channel: &ChannelView) -> bool {
        self.history_source == HistorySource::ServerArchive
            && channel.conversation.is_some()
            && !matches!(channel.target, TextTarget::Direct(_))
    }
    /// The conversation, if its messages can be changed now.
    fn changeable(&self, channel: &ChannelView) -> Option<[u8; 16]> {
        (self.status == Status::Connected && self.archived(channel) && self.has("history/1"))
            .then_some(channel.conversation)
            .flatten()
    }
    /// Whether we may edit this message: ours, stored, and we are registered.
    pub fn may_edit(&self, channel: &ChannelView, message: &MessageView) -> bool {
        let own = self.own_user_id();
        self.changeable(channel).is_some()
            && own.is_some()
            && message.ours(own)
            && message.deleted.is_none()
            && message.pending.is_none()
            && message.id.is_some()
            && message.offer.is_none()
    }
    /// Whether we may delete this message: our own (registered), or as a
    /// moderator where the server allows it.
    pub fn may_delete(&self, channel: &ChannelView, message: &MessageView) -> bool {
        if self.changeable(channel).is_none()
            || message.deleted.is_some()
            || message.pending.is_some()
            || message.id.is_none()
        {
            return false;
        }
        let own = self.own_user_id();
        (own.is_some() && message.ours(own)) || self.moderates(channel)
    }
    /// Moderators delete others' messages in channels they hold Write on,
    /// if the server's policy says so.
    pub fn moderates(&self, channel: &ChannelView) -> bool {
        self.policy
            .as_ref()
            .is_some_and(|policy| policy.moderators_delete)
            && matches!(channel.target, TextTarget::Channel(id) if self.may(id, perm::WRITE))
    }
    fn cache_key(&self, channel: &ChannelView) -> Option<CacheKey> {
        Some(CacheKey {
            identity: self.history_identity()?,
            channel: channel.storage_key.clone(),
        })
    }
}

impl Desktop {
    /// The open conversation's message with this serial, and its indices.
    fn located(&self, serial: u64) -> Option<(usize, usize, usize)> {
        let (server, channel) = self.selected_indices()?;
        let position = self.servers[server].channels[channel]
            .messages
            .iter()
            .position(|message| message.serial == serial)?;
        Some((server, channel, position))
    }

    /// Edit message `serial` in the composer (Enter saves, Escape cancels).
    pub fn start_edit(&mut self, serial: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, channel, position)) = self.located(serial) else {
            return;
        };
        let view = &self.servers[server];
        let (channel_view, message) = (
            &view.channels[channel],
            &view.channels[channel].messages[position],
        );
        if !view.may_edit(channel_view, message) {
            return;
        }
        let Some(seq) = message.id.and_then(|id| u64::try_from(id).ok()) else {
            return;
        };
        let source = message.source.clone();
        let draft = match self.editing.take() {
            // Switching to another message keeps the draft from before.
            Some(editing) => editing.draft,
            None => self.composer.read(cx).value().to_string(),
        };
        self.editing = Some(Editing {
            server: view.id.clone(),
            channel: channel_view.id,
            seq,
            serial,
            draft,
        });
        self.composer.update(cx, |input, cx| {
            input.set_value(source, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// Up in an empty composer: edit our newest message here, if any.
    pub fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.editing.is_some() || !self.composer.read(cx).value().is_empty() {
            return false;
        }
        let Some((server, channel)) = self.selected_indices() else {
            return false;
        };
        let view = &self.servers[server];
        let channel_view = &view.channels[channel];
        let Some(serial) = channel_view
            .messages
            .iter()
            .rev()
            .find(|message| view.may_edit(channel_view, message))
            .map(|message| message.serial)
        else {
            return false;
        };
        self.start_edit(serial, window, cx);
        true
    }

    /// Escape while editing: the composer gets back what it held before.
    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(editing) = self.editing.take() else {
            return false;
        };
        self.composer
            .update(cx, |input, cx| input.set_value(editing.draft, window, cx));
        cx.notify();
        true
    }

    /// The edit is no longer possible (another conversation opened, or the
    /// connection dropped): forget it, and keep what it replaced as the draft.
    pub fn drop_edit(&mut self) -> Option<String> {
        self.editing.take().map(|editing| editing.draft)
    }

    /// Enter while editing: send the edit. Unchanged text just ends editing;
    /// empty text asks whether to delete instead.
    pub fn submit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.as_ref() else {
            return;
        };
        let text = self.composer.read(cx).value().to_string();
        let Some((server, channel)) = self.selected_indices().filter(|(server, channel)| {
            self.servers[*server].id == editing.server
                && self.servers[*server].channels[*channel].id == editing.channel
        }) else {
            self.cancel_edit(window, cx);
            return;
        };
        let serial = editing.serial;
        let seq = editing.seq;
        let view = &self.servers[server];
        let channel_view = &view.channels[channel];
        let Some(message) = channel_view
            .messages
            .iter()
            .find(|message| message.serial == serial)
        else {
            self.cancel_edit(window, cx);
            return;
        };
        if text.trim().is_empty() {
            self.cancel_edit(window, cx);
            self.confirm_delete(serial, window, cx);
            return;
        }
        if text == message.source || !view.may_edit(channel_view, message) {
            self.cancel_edit(window, cx);
            return;
        }
        if text.len() > nuschel_protocol::nuschel::MAX_CONTENT_BYTES {
            self.servers[server].notice = Some("The edited message exceeds 64 KiB.".into());
            return;
        }
        let Some(conversation) = channel_view.conversation else {
            return;
        };
        let change = ChangeRequest {
            server: view.id.clone(),
            conversation,
            seq,
            content: Some(text),
            cache: view.cache_key(channel_view),
        };
        match self.backend.change_message(change) {
            Ok(()) => {
                self.cancel_edit(window, cx);
            }
            Err(error) => self.servers[server].notice = Some(error),
        }
        cx.notify();
    }

    /// Ask before deleting message `serial` (from its menu, or an emptied edit).
    pub fn confirm_delete(&mut self, serial: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, channel, position)) = self.located(serial) else {
            return;
        };
        let view = &self.servers[server];
        let channel_view = &view.channels[channel];
        let message = &channel_view.messages[position];
        if !view.may_delete(channel_view, message) {
            return;
        }
        let ours = message.ours(view.own_user_id());
        let detail = if ours {
            "It is removed from the server's history for everyone. People using stock Mumble \
             keep seeing it."
        } else {
            "You delete it as a moderator; others see that a moderator removed it. People \
             using stock Mumble keep seeing it."
        };
        self.open_prompt(
            server,
            PromptAction::DeleteMessage { serial },
            if ours {
                "Delete this message?"
            } else {
                "Delete this message as a moderator?"
            },
            detail,
            String::new(),
            window,
            cx,
        );
    }

    /// Delete message `serial` (confirmed).
    pub fn delete_message(&mut self, serial: u64) {
        let Some((server, channel, position)) = self.located(serial) else {
            return;
        };
        let view = &self.servers[server];
        let channel_view = &view.channels[channel];
        let message = &channel_view.messages[position];
        if !view.may_delete(channel_view, message) {
            return;
        }
        let (Some(conversation), Some(seq)) = (
            channel_view.conversation,
            message.id.and_then(|id| u64::try_from(id).ok()),
        ) else {
            return;
        };
        let change = ChangeRequest {
            server: view.id.clone(),
            conversation,
            seq,
            content: None,
            cache: view.cache_key(channel_view),
        };
        if let Err(error) = self.backend.change_message(change) {
            self.servers[server].notice = Some(error);
        }
    }

    /// A message changed on the server: show its new text, or its tombstone.
    pub fn apply_change(&mut self, server: &str, conversation: [u8; 16], seq: u64, change: Change) {
        let Some(view) = self.servers.iter_mut().find(|view| view.id == server) else {
            return;
        };
        let Some(channel) = view
            .channels
            .iter_mut()
            .find(|channel| channel.conversation == Some(conversation))
        else {
            return;
        };
        let Some(message) = channel
            .messages
            .iter_mut()
            .find(|message| message.id == Some(seq as i64))
        else {
            return;
        };
        match change {
            Change::Edited {
                source,
                document,
                edited_ms,
            } => {
                // A late edit never undoes a deletion.
                if message.deleted.is_some() {
                    return;
                }
                message.source = source;
                message.document = document.into();
                message.edited_ms = Some(edited_ms);
                message.offer = nuschel_core::transfer::parse_offer(&message.source);
            }
            Change::Deleted(deletion) => {
                message.deleted = Some(deletion);
                message.document = MessageDocument { blocks: Vec::new() }.into();
                message.source.clear();
                message.offer = None;
                message.status = None;
                message.expanded = false;
                let serial = message.serial;
                if self
                    .editing
                    .as_ref()
                    .is_some_and(|editing| editing.serial == serial)
                {
                    // The composer keeps the text; there is nothing left to edit.
                    self.editing = None;
                }
            }
        }
    }
}

/// What changed about a message (see `Desktop::apply_change`).
pub enum Change {
    Edited {
        source: String,
        document: MessageDocument,
        edited_ms: i64,
    },
    Deleted(Deletion),
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{ChannelView, Deletion, HistorySource, MessageView, ServerView, Status, perm};
    use crate::backend::RenderedMessage;
    use nuschel_core::{ChatMessage, MessageDocument};
    use nuschel_session::connection::{TextTarget, User};

    fn message(id: i64, author_id: Option<u32>, outgoing: bool) -> MessageView {
        let mut rendered = RenderedMessage::plain(
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
        rendered.into()
    }

    fn server(registered: bool, moderators_delete: bool, write: bool) -> (ServerView, ChannelView) {
        let mut server = ServerView::new_for_tests();
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
        server.policy = Some(nuschel_protocol::nuschel::v1::Policy {
            moderators_delete,
            ..Default::default()
        });
        if write {
            server.permissions.insert(3, perm::WRITE);
        } else {
            server.permissions.insert(3, 0);
        }
        let channel = ChannelView {
            id: 3,
            target: TextTarget::Channel(3),
            conversation: Some([1; 16]),
            ..Default::default()
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
}
