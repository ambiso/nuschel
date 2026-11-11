// SPDX-License-Identifier: EUPL-1.2
//! Direct messages as durable direct conversations on Nuschel servers
//! (docs/extension.md §3, "Direct conversations"). Between two registered
//! accounts, a direct message goes to the conversation the server keeps for
//! the pair: it is stored (and reaches the other account when it next logs
//! in), retried with its idempotency key after a reconnect, paged from the
//! server's history, and counted unread from the account's read marker, as
//! channels are. The server sends stock Mumble clients their copy.
//!
//! Everything else keeps the stock path (a `TextMessage` to the session):
//! stock Mumble servers, unregistered people on either side, and a server
//! that refuses to open the conversation. Those conversations show local
//! history, and say so.
use crate::{
    backend::direct_conversation_key,
    state::{ChannelView, Desktop, ServerView},
};
use nuschel_protocol::nuschel::{self, v1};
use nuschel_session::connection::{Command, NuschelRequest, TextTarget, User};

impl ServerView {
    /// Whether direct messages with `peer` go to a durable conversation.
    pub fn durable_directs_with(&self, peer: &User) -> bool {
        self.history_source == nuschel_client::HistorySource::ServerArchive
            && self.has("history/1")
            && self.own_user_id().is_some()
            // 0 is SuperUser, and also what the server says for a deleted
            // account: never matched to a conversation.
            && peer.user_id.is_some_and(|id| id != 0)
    }

    /// The direct conversation with registered user `user_id`, if known.
    fn direct_with(&self, user_id: u32) -> Option<[u8; 16]> {
        self.directs
            .iter()
            .find(|(_, direct)| user_id != 0 && direct.user_id == user_id)
            .map(|(id, _)| *id)
    }

    /// The online session of registered user `user_id`.
    fn online(&self, user_id: u32) -> Option<&User> {
        self.users
            .values()
            .find(|user| user_id != 0 && user.user_id == Some(user_id))
    }

    /// The timeline of direct conversation `id`, created if needed; returns
    /// its index in `channels`.
    pub fn direct_view(&mut self, id: [u8; 16], name: &str, peer: Option<User>) -> usize {
        let key = direct_conversation_key(&id);
        if let Some(index) = self
            .channels
            .iter()
            .position(|channel| channel.conversation == Some(id) || channel.storage_key == key)
        {
            let channel = &mut self.channels[index];
            channel.conversation = Some(id);
            channel.target = TextTarget::DirectConversation(id);
            if peer.is_some() {
                channel.direct_peer = peer;
            }
            return index;
        }
        let mut channel_id = u32::MAX;
        while self.channels.iter().any(|channel| channel.id == channel_id) {
            channel_id -= 1;
        }
        self.channels.push(ChannelView {
            id: channel_id,
            parent: None,
            name: format!("@ {name}"),
            topic: "Private conversation".into(),
            position: i32::MAX,
            target: TextTarget::DirectConversation(id),
            direct_peer: peer,
            storage_key: key,
            conversation: Some(id),
            ..Default::default()
        });
        self.channels.len() - 1
    }
}

impl Desktop {
    /// Open the direct conversation with `session` on a Nuschel server; false
    /// if direct messages with them take the stock path.
    pub fn open_durable_direct(&mut self, index: usize, peer: &User) -> bool {
        let server = &mut self.servers[index];
        let Some(user_id) = peer.user_id.filter(|_| server.durable_directs_with(peer)) else {
            return false;
        };
        let name = crate::names::display_name(peer, &self.settings).to_string();
        if let Some(id) = server.direct_with(user_id) {
            let channel = server.direct_view(id, &name, Some(peer.clone()));
            self.pending_browse = Some((server.id.clone(), server.channels[channel].id));
            return true;
        }
        let request_id = crate::backend::request_id();
        let open = v1::OpenDirect {
            request_id,
            user_id,
        };
        let command = Command::Nuschel(NuschelRequest::OpenDirect(open));
        if self.backend.command(&server.id, command).is_err() {
            return false;
        }
        while self.direct_requests.len() >= 16 {
            self.direct_requests.pop_first();
        }
        self.direct_requests
            .insert(request_id, (server.id.clone(), peer.session, true));
        true
    }

    /// The server named one of our direct conversations (opened by us).
    pub fn direct_opened(&mut self, index: usize, opened: v1::DirectOpened) {
        let request = self.direct_requests.remove(&opened.request_id);
        let Some(direct) = opened.direct else {
            return;
        };
        let Some(id) = nuschel::id(&direct.conversation) else {
            return;
        };
        let server = &mut self.servers[index];
        let peer = server.online(direct.user_id).cloned();
        let name = peer
            .as_ref()
            .map(|peer| crate::names::display_name(peer, &self.settings).to_string())
            .unwrap_or_else(|| direct.name.clone());
        server.directs.insert(id, direct);
        let channel = server.direct_view(id, &name, peer);
        if request.is_some_and(|(_, _, open)| open) {
            self.pending_browse = Some((server.id.clone(), server.channels[channel].id));
        }
    }

    /// Our direct conversations and how far we read them: unread ones get a
    /// place in the list (with their count) even while the other is offline.
    pub fn directs_listed(&mut self, index: usize, directs: Vec<v1::Direct>) {
        let selected = self.navigation.selection().cloned();
        let server = &mut self.servers[index];
        for direct in directs {
            let Some(id) = nuschel::id(&direct.conversation) else {
                continue;
            };
            let unread = direct.head.saturating_sub(direct.read);
            let shown = server
                .channels
                .iter()
                .any(|channel| channel.conversation == Some(id));
            if shown || unread > 0 {
                let peer = server.online(direct.user_id).cloned();
                let name = peer
                    .as_ref()
                    .map(|peer| crate::names::display_name(peer, &self.settings).to_string())
                    .unwrap_or_else(|| {
                        if direct.name.is_empty() {
                            "Deleted user".into()
                        } else {
                            direct.name.clone()
                        }
                    });
                let channel = server.direct_view(id, &name, peer);
                let channel = &mut server.channels[channel];
                channel.read_seq = channel.read_seq.max(direct.read as i64);
                let open = selected.as_ref().is_some_and(|selected| {
                    selected.server == server.id && selected.channel == channel.id
                });
                if !open {
                    channel.unread = usize::try_from(unread).unwrap_or(usize::MAX).min(999);
                    // A direct message is meant for us, as a mention is.
                    channel.unread_mention = unread > 0;
                }
            }
            server.directs.insert(id, direct);
        }
    }

    /// The server would not open the conversation (the other person cannot
    /// receive text there, say): fall back to the stock path.
    pub fn direct_failed(&mut self, index: usize, error: v1::Error) {
        let Some((server_id, session, open)) = self.direct_requests.remove(&error.request_id)
        else {
            return;
        };
        let server = &mut self.servers[index];
        if server.id != server_id || !open {
            return;
        }
        if let Some(peer) = server.users.get(&session).cloned() {
            let id = self.stock_direct_view(index, &peer);
            self.pending_browse = Some((server_id, id));
        }
    }
}
