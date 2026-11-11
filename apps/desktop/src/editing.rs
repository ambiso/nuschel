// SPDX-License-Identifier: EUPL-1.2
//! Editing and deleting messages on Nuschel servers (docs/extension.md §6).
//! Stock Mumble has neither, so nothing here is offered there. Authors edit
//! and delete their own messages (registered accounts only, as the server
//! requires); moderators delete others' only where the server's policy says
//! they may (`moderators_delete`) and they hold Write on the channel. The
//! server checks every request again; its refusals show like a failed send.
use crate::{
    menus::PromptAction,
    state::{Desktop, Status},
};
use gpui_kit::*;
use nuschel_client::{Client, platform::ChangeRequest};

/// The message the composer is editing, and what it held before.
pub struct Editing {
    pub server: String,
    pub channel: u32,
    pub channel_conversation: Option<[u8; 16]>,
    pub seq: u64,
    /// The message's serial, to mark it in the timeline.
    pub serial: u64,
    pub draft: String,
}

impl Desktop {
    /// The open conversation's message with this serial, and its indices.
    fn located(&self, serial: u64) -> Option<(usize, usize, usize)> {
        let (server, channel) = self.selected_indices()?;
        let position = self.client.servers[server].channels[channel]
            .timeline
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
        let view = &self.client.servers[server];
        let (channel_view, message) = (
            &view.channels[channel],
            &view.channels[channel].timeline.messages[position],
        );
        if !view.may_edit(channel_view, message) {
            return;
        }
        let Some(seq) = message.seq else {
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
            channel_conversation: channel_view.conversation,
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
        let view = &self.client.servers[server];
        let channel_view = &view.channels[channel];
        let Some(serial) = channel_view
            .timeline
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
    /// empty text asks whether to delete instead. Offline, the edit waits
    /// (nothing typed is lost).
    pub fn submit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.as_ref() else {
            return;
        };
        let text = self.composer.read(cx).value().to_string();
        let Some((server, channel)) = self.selected_indices().filter(|(server, channel)| {
            self.client.servers[*server].id == editing.server
                && self.client.servers[*server].channels[*channel].id == editing.channel
        }) else {
            return;
        };
        let seq = editing.seq;
        let view = &self.client.servers[server];
        let channel_view = &view.channels[channel];
        // By seq: a reloaded timeline has new serials.
        let message = channel_view
            .timeline
            .messages
            .iter()
            .find(|message| message.seq == Some(seq));
        if let Some(message) = message {
            let serial = message.serial;
            if let Some(editing) = self.editing.as_mut() {
                editing.serial = serial;
            }
            if text.trim().is_empty() {
                self.cancel_edit(window, cx);
                self.confirm_delete(serial, window, cx);
                return;
            }
            if text == message.source {
                self.cancel_edit(window, cx);
                return;
            }
        }
        let view = &self.client.servers[server];
        let channel_view = &view.channels[channel];
        let editable = match message {
            Some(message) => view.may_edit(channel_view, message),
            // Out of the loaded timeline: the server checks it.
            None => view.changeable(channel_view).is_some() && view.own_user_id().is_some(),
        };
        if !editable {
            self.client.servers[server].notice =
                Some(if self.client.servers[server].status == Status::Connected {
                    "This message can no longer be edited.".into()
                } else {
                    "Connect again to save your edit; Escape discards it.".into()
                });
            cx.notify();
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        if text.len() > nuschel_protocol::nuschel::MAX_CONTENT_BYTES {
            self.client.servers[server].notice = Some("The edited message exceeds 64 KiB.".into());
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
            cache: Client::cache_key(view, channel_view),
            moderator: false,
        };
        match self.client.change_message(change) {
            Ok(()) => {
                self.cancel_edit(window, cx);
            }
            Err(error) => self.client.servers[server].notice = Some(error),
        }
        cx.notify();
    }

    /// Ask before deleting message `serial` (from its menu, or an emptied edit).
    pub fn confirm_delete(&mut self, serial: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, channel, position)) = self.located(serial) else {
            return;
        };
        let view = &self.client.servers[server];
        let channel_view = &view.channels[channel];
        let message = &channel_view.timeline.messages[position];
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
        let view = &self.client.servers[server];
        let channel_view = &view.channels[channel];
        let message = &channel_view.timeline.messages[position];
        if !view.may_delete(channel_view, message) {
            return;
        }
        let (Some(conversation), Some(seq)) = (channel_view.conversation, message.seq) else {
            return;
        };
        let change = ChangeRequest {
            server: view.id.clone(),
            conversation,
            seq,
            content: None,
            cache: Client::cache_key(view, channel_view),
            moderator: view.moderates(channel_view),
        };
        if let Err(error) = self.client.change_message(change) {
            self.client.servers[server].notice = Some(error);
        }
    }
}
