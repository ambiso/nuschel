// SPDX-License-Identifier: EUPL-1.2
//! Bounded asynchronous browser storage bridge. JavaScript receives opaque
//! tickets; navigation/certificate evidence and parsed documents stay in Rust.
use nuschl_client::{
    HistoryTicket,
    input::Input,
    model::{TextTarget, User},
    timeline::{IncomingMessage, Page},
};
use nuschl_core::{ChatMessage, MAX_CONTENT_BYTES, MessageDocument};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PAGE_SIZE: usize = 100;
const MAX_PENDING: usize = 128;

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Append {
        ticket: String,
        message: ChatMessage,
        outgoing: bool,
    },
    Page {
        ticket: String,
        identity: String,
        channel: String,
        before: Option<i64>,
        limit: usize,
    },
}

struct Context {
    server: String,
    generation: u64,
    identity: String,
}
enum Pending {
    Append {
        context: Context,
        target: TextTarget,
        peer: Option<User>,
        message: Box<IncomingMessage>,
    },
    Page {
        context: Context,
        channel: String,
        ticket: HistoryTicket,
        before: Option<i64>,
    },
}
#[derive(Default)]
pub struct Storage {
    next: u64,
    pending: BTreeMap<String, Pending>,
    outgoing: Vec<Operation>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    id: i64,
    message: ChatMessage,
    outgoing: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    rows: Vec<Stored>,
    #[serde(default)]
    next_before: Option<i64>,
    #[serde(default)]
    error: Option<String>,
}

impl Storage {
    fn allocate(&mut self) -> Result<String, String> {
        if self.pending.len() >= MAX_PENDING {
            return Err("Local history is busy; this message could not be saved.".into());
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("History ticket counter exhausted.")?;
        Ok(self.next.to_string())
    }
    pub fn append(
        &mut self,
        server: String,
        generation: u64,
        target: TextTarget,
        peer: Option<User>,
        message: IncomingMessage,
    ) -> Result<(), String> {
        let id = self.allocate()?;
        let context = Context {
            server,
            generation,
            identity: message.message.server.clone(),
        };
        self.outgoing.push(Operation::Append {
            ticket: id.clone(),
            message: message.message.clone(),
            outgoing: message.outgoing,
        });
        self.pending.insert(
            id,
            Pending::Append {
                context,
                target,
                peer,
                message: Box::new(message),
            },
        );
        Ok(())
    }
    pub fn page(
        &mut self,
        server: String,
        generation: u64,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    ) -> Result<(), String> {
        let Page::Before(before) = page else {
            return Err("History search windows are not available in the browser yet.".into());
        };
        let id = self.allocate()?;
        self.outgoing.push(Operation::Page {
            ticket: id.clone(),
            identity: identity.clone(),
            channel: channel.clone(),
            before,
            limit: PAGE_SIZE,
        });
        self.pending.insert(
            id,
            Pending::Page {
                context: Context {
                    server,
                    generation,
                    identity,
                },
                channel,
                ticket,
                before,
            },
        );
        Ok(())
    }
    pub fn take(&mut self) -> Vec<Operation> {
        std::mem::take(&mut self.outgoing)
    }

    /// Unknown/repeated responses have no effects. The connection generation and
    /// certificate-scoped identity must still match before any result is applied.
    pub fn complete(
        &mut self,
        id: &str,
        raw: &str,
        current: impl Fn(&str, u64, &str) -> bool,
        render: impl Fn(&str) -> MessageDocument,
    ) -> Vec<Input> {
        let Some(pending) = self.pending.remove(id) else {
            return Vec::new();
        };
        let context = match &pending {
            Pending::Append { context, .. } | Pending::Page { context, .. } => context,
        };
        if !current(&context.server, context.generation, &context.identity) {
            return Vec::new();
        }
        let response = (raw.len() <= 8 * 1024 * 1024)
            .then(|| serde_json::from_str::<Response>(raw).ok())
            .flatten();
        let failure = response.as_ref().is_none_or(|reply| reply.error.is_some());
        match pending {
            Pending::Append {
                context,
                target,
                peer,
                mut message,
            } => {
                message.id = response
                    .filter(|_| !failure)
                    .and_then(|response| response.id)
                    .filter(|id| *id > 0 && *id <= 9_007_199_254_740_991);
                let mut result = Vec::new();
                if message.id.is_none() {
                    result.push(Input::HistoryError("Local history could not save a message. Browser storage may be full or unavailable.".into()));
                } else {
                    result.push(Input::HistoryRecovered);
                }
                result.push(Input::Message {
                    server: context.server,
                    generation: context.generation,
                    target,
                    storage_key: message.message.channel.clone(),
                    peer,
                    message: *message,
                });
                result
            }
            Pending::Page {
                context,
                channel,
                ticket,
                before,
            } => {
                let failed = || {
                    vec![Input::HistoryFailed { server: context.server.clone(), ticket: ticket.clone(), reason: "Local history could not load this page. Retry after checking browser storage.".into() }]
                };
                let Some(response) = response.filter(|_| !failure) else {
                    return failed();
                };
                if response.rows.len() > PAGE_SIZE
                    || response.id.is_some()
                    || response.rows.iter().any(|row| {
                        row.id <= 0
                            || row.message.server != context.identity
                            || row.message.channel != channel
                            || row.message.body.len() > MAX_CONTENT_BYTES
                            || row.message.author.len() > 512
                    })
                    || response.next_before.is_some_and(|cursor| {
                        response.rows.first().is_none_or(|row| row.id != cursor)
                    })
                {
                    return failed();
                }
                let messages = response
                    .rows
                    .into_iter()
                    .map(|row| {
                        let document = render(&row.message.body);
                        IncomingMessage::plain(Some(row.id), row.message, document, row.outgoing)
                    })
                    .collect();
                vec![Input::History {
                    server: context.server,
                    ticket,
                    before,
                    messages,
                    next_before: response.next_before,
                }]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_client::{Conversation, HistorySource, Navigation};
    use nuschl_core::ContentBlock;
    fn message() -> IncomingMessage {
        IncomingMessage::plain(
            None,
            ChatMessage {
                server: "ab".repeat(32),
                channel: "0".into(),
                author: "Ada".into(),
                observed_at_ms: 123,
                body: "one\n\ntwo".into(),
            },
            render("one\n\ntwo"),
            true,
        )
    }
    fn render(text: &str) -> MessageDocument {
        MessageDocument {
            blocks: vec![ContentBlock::text(text)],
        }
    }
    fn issued(storage: &mut Storage) -> String {
        match storage.take().pop().unwrap() {
            Operation::Append { ticket, .. } | Operation::Page { ticket, .. } => ticket,
        }
    }
    #[test]
    fn writes_only_claim_saved_after_ack_and_fail_visibly_without_losing_text() {
        let mut storage = Storage::default();
        storage
            .append("server".into(), 4, TextTarget::Channel(0), None, message())
            .unwrap();
        let ticket = issued(&mut storage);
        let inputs = storage.complete(
            &ticket,
            "{\"id\":42}",
            |server, generation, identity| {
                server == "server" && generation == 4 && identity == "ab".repeat(32)
            },
            render,
        );
        assert!(
            matches!(&inputs[1], Input::Message { message, .. } if message.id == Some(42) && message.message.body == "one\n\ntwo")
        );
        assert!(
            storage
                .complete(&ticket, "{\"id\":43}", |_, _, _| true, render)
                .is_empty()
        );
        storage
            .append("server".into(), 4, TextTarget::Channel(0), None, message())
            .unwrap();
        let ticket = issued(&mut storage);
        let inputs = storage.complete(&ticket, "{\"error\":\"quota\"}", |_, _, _| true, render);
        assert!(matches!(inputs[0], Input::HistoryError(_)));
        assert!(
            matches!(&inputs[1], Input::Message { message, .. } if message.id.is_none() && message.message.body == "one\n\ntwo")
        );
    }
    #[test]
    fn generation_and_certificate_changes_drop_late_writes() {
        for (generation, identity) in [(5, "ab".repeat(32)), (4, "cd".repeat(32))] {
            let mut storage = Storage::default();
            storage
                .append("server".into(), 4, TextTarget::Channel(0), None, message())
                .unwrap();
            let ticket = issued(&mut storage);
            assert!(
                storage
                    .complete(
                        &ticket,
                        "{\"id\":9}",
                        |_, old_generation, old_identity| old_generation == generation
                            && old_identity == identity,
                        render
                    )
                    .is_empty()
            );
        }
    }
    #[test]
    fn page_scope_and_opaque_navigation_evidence_survive_async_roundtrip() {
        let mut storage = Storage::default();
        let mut nav = Navigation::default();
        let conversation = Conversation {
            server: "server".into(),
            channel: 0,
        };
        let history = nav.browse(conversation.clone(), HistorySource::LocalObserved);
        storage
            .page(
                "server".into(),
                4,
                "ab".repeat(32),
                "0".into(),
                history.clone(),
                Page::Before(None),
            )
            .unwrap();
        let ticket = issued(&mut storage);
        nav.browse(
            Conversation {
                server: "other".into(),
                channel: 0,
            },
            HistorySource::LocalObserved,
        );
        nav.browse(conversation, HistorySource::LocalObserved);
        let response = serde_json::json!({"rows":[{"id":7,"message":message().message,"outgoing":true}],"next_before":7}).to_string();
        let inputs = storage.complete(&ticket, &response, |_, _, _| true, render);
        assert!(
            matches!(&inputs[0], Input::History {ticket, messages, next_before: Some(7), ..} if ticket == &history && !nav.accepts(ticket) && messages[0].id == Some(7))
        );
        for identity in ["ab".repeat(32), "cd".repeat(32)] {
            let history = nav.browse(
                Conversation {
                    server: "server".into(),
                    channel: 0,
                },
                HistorySource::LocalObserved,
            );
            storage
                .page(
                    "server".into(),
                    4,
                    "ab".repeat(32),
                    "0".into(),
                    history,
                    Page::Before(None),
                )
                .unwrap();
            let ticket = issued(&mut storage);
            let mut row = message().message;
            row.server = identity.clone();
            if identity == "ab".repeat(32) {
                row.channel = "different".into();
            }
            let response = serde_json::json!({"rows":[{"id":7,"message":row,"outgoing":false}],"next_before":null}).to_string();
            assert!(matches!(
                storage.complete(&ticket, &response, |_, _, _| true, render)[0],
                Input::HistoryFailed { .. }
            ));
        }
    }
    #[test]
    fn bounded_pending_and_pages_cannot_accept_unknown_metadata() {
        let mut storage = Storage::default();
        for _ in 0..MAX_PENDING {
            storage
                .append("server".into(), 1, TextTarget::Channel(0), None, message())
                .unwrap();
        }
        assert!(
            storage
                .append("server".into(), 1, TextTarget::Channel(0), None, message())
                .is_err()
        );
        let mut nav = Navigation::default();
        for response in [
            "{}",
            "{\"rows\":[],\"next_before\":4}",
            "{\"rows\":[],\"delete_all\":true}",
        ] {
            let mut storage = Storage::default();
            let ticket = nav.browse(
                Conversation {
                    server: "server".into(),
                    channel: 0,
                },
                HistorySource::LocalObserved,
            );
            storage
                .page(
                    "server".into(),
                    1,
                    "ab".repeat(32),
                    "0".into(),
                    ticket,
                    Page::Before(None),
                )
                .unwrap();
            let id = issued(&mut storage);
            let inputs = storage.complete(&id, response, |_, _, _| true, render);
            if response == "{}" {
                assert!(matches!(inputs[0], Input::History { .. }));
            } else {
                assert!(matches!(inputs[0], Input::HistoryFailed { .. }));
            }
        }
    }
}
