// SPDX-License-Identifier: EUPL-1.2
//! Correlated preview sends. The rate queue retains original presence authority
//! and checks it again immediately before producing a wire message.
use super::{Command, CommandAuthority, Error, Outgoing, SessionEvent, State, TextTarget, emit};
use tokio::sync::mpsc;

pub(super) struct Delivery {
    pub id: u64,
    pub authority: CommandAuthority,
}

pub(super) fn command_id(command: &Command) -> Option<u64> {
    match command {
        Command::SendPreview { id, .. } => Some(*id),
        Command::Scoped { command, .. } => match command.as_ref() {
            Command::SendPreview { id, .. } => Some(*id),
            _ => None,
        },
        _ => None,
    }
}

pub(super) async fn refused(id: u64, events: &mpsc::Sender<SessionEvent>) -> Result<(), Error> {
    emit(events, SessionEvent::LinkPreviewSendResult {
        id,
        result: Err("The destination or its permissions changed. Your message was not sent.".into()),
    }).await
}

pub(super) fn may_send(state: &State, target: &TextTarget) -> bool {
    use crate::model::permission::TEXT_MESSAGE;
    match target {
        TextTarget::Channel(id) => state.channels.contains_key(id) && state.known_permission(*id, TEXT_MESSAGE),
        TextTarget::Direct(id) => state.users.get(id).is_some_and(|user| state.known_permission(user.channel, TEXT_MESSAGE)),
        TextTarget::DirectConversation(_) => false,
    }
}

impl State {
    pub(super) fn preview_valid(&self, message: &Outgoing) -> bool {
        let Some(delivery) = message.preview.as_ref() else { return false; };
        delivery.id != 0
            && self.logged_in()
            && !self.encrypted_server()
            && self.limits.allow_html
            && !message.wire.is_empty()
            && message.wire.len() <= nuschl_core::MAX_CONTENT_BYTES
            && self.limits.check_html_total(&message.wire).is_ok()
            && self.authority_valid(&delivery.authority)
            && may_send(self, &message.target)
            && match message.target {
                TextTarget::Channel(id) => delivery.authority.channels.iter().any(|(held,_)| *held==id)
                    && message.expected_peer.is_none(),
                TextTarget::Direct(id) => self.users.get(&id).is_some_and(|user| {
                    delivery.authority.users.contains(&(id,user.instance))
                        && message.expected_peer == Some(user.instance)
                }),
                TextTarget::DirectConversation(_) => false,
            }
    }

    pub(super) async fn queue_preview(
        &mut self,
        id: u64,
        authority: CommandAuthority,
        target: TextTarget,
        html: String,
        expected_peer: Option<u64>,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        let message = Outgoing {
            preview: Some(Delivery { id, authority }),
            target,
            expected_peer,
            wire: html.clone(),
            body: html,
            html: true,
            chat: None,
        };
        if !self.preview_valid(&message) {
            return refused(id, events).await;
        }
        self.outbox.push(message);
        self.flush_outbox(events).await
    }
}
