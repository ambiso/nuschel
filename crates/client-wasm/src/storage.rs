// SPDX-License-Identifier: EUPL-1.2
//! Bounded asynchronous browser storage bridge. JavaScript receives opaque
//! tickets; navigation/certificate evidence and parsed documents stay in Rust.
use crate::content_broker::SourceMode;
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
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

fn marked_mode() -> SourceMode {
    SourceMode::Marked
}

pub struct RawCompletion {
    pub inputs: Vec<Input>,
    pub live_mode: Option<SourceMode>,
    pub channel_instance: Option<u64>,
    pub stored_modes: BTreeMap<i64, SourceMode>,
    /// Present only for an Append that passed connection/certificate validation.
    pub save: Option<SaveResult>,
}

/// The one notice a connection may show for browser storage not committing rows.
pub const SAVE_FAILED: &str =
    "Local history could not save a message. Browser storage may be full or unavailable.";

/// Typed browser storage answer to one validated Append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Failed,
}

/// The Append's ORIGINAL pending connection, captured at admission. A storage
/// reply never selects it, and a later current view never substitutes for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveResult {
    pub server: String,
    pub generation: u64,
    pub outcome: SaveOutcome,
}

/// What one connection observed about saving a received row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthEvent {
    /// The bounded storage queue refused the row before any browser I/O.
    Busy,
    /// Browser storage answered a validated Append for the row.
    Outcome(SaveOutcome),
}

/// Whether new rows of a connection are currently being saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveAvailability {
    Available,
    Busy,
    Failing,
}

/// Current availability and the cumulative rows of that connection that were
/// not saved. The count is a decimal string, never a rounded JavaScript number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HistoryHealthView {
    pub saving: SaveAvailability,
    pub not_saved: String,
}

struct Health {
    generation: u64,
    saving: SaveAvailability,
    not_saved: u64,
    busy_notice: bool,
    failed_notice: bool,
}

/// At most one entry per catalog server, for its latest connection only.
const MAX_HEALTH_ENTRIES: usize = crate::actions::MAX_SERVERS;

/// Per-connection history-save status. Each refused or failed row changes
/// bounded state; a connection shows at most one first notice per kind. A later
/// save changes current availability, but never re-arms a notice, saves an
/// earlier row or lowers the count of rows that were not saved.
#[derive(Default)]
pub struct HistoryHealth {
    entries: BTreeMap<String, Health>,
}

impl HistoryHealth {
    /// Record `event` for connection `generation` of `server`. Returns whether
    /// it is that connection's first refusal of its kind, the only one that
    /// may produce a notice. An older generation is inert; a newer connection
    /// starts with a fresh count and notice budget.
    pub fn observe(&mut self, server: &str, generation: u64, event: HealthEvent) -> bool {
        if generation == 0 {
            return false;
        }
        let refused = event != HealthEvent::Outcome(SaveOutcome::Saved);
        match self.entries.get(server).map(|entry| entry.generation) {
            Some(old) if old > generation => return false,
            Some(old) if old == generation => {}
            // A save of a connection without refusals needs no status.
            _ if !refused => {
                self.entries.remove(server);
                return false;
            }
            None if self.entries.len() >= MAX_HEALTH_ENTRIES => return false,
            _ => {
                self.entries.insert(
                    server.to_owned(),
                    Health {
                        generation,
                        saving: SaveAvailability::Available,
                        not_saved: 0,
                        busy_notice: false,
                        failed_notice: false,
                    },
                );
            }
        }
        let entry = self.entries.get_mut(server).expect("inserted entry");
        let notice = match event {
            HealthEvent::Outcome(SaveOutcome::Saved) => {
                entry.saving = SaveAvailability::Available;
                return false;
            }
            HealthEvent::Busy => {
                entry.saving = SaveAvailability::Busy;
                &mut entry.busy_notice
            }
            HealthEvent::Outcome(SaveOutcome::Failed) => {
                entry.saving = SaveAvailability::Failing;
                &mut entry.failed_notice
            }
        };
        let first = !std::mem::replace(notice, true);
        entry.not_saved = entry.not_saved.saturating_add(1);
        first
    }

    /// The connection ended, was replaced or its server was removed.
    pub fn forget(&mut self, server: &str) {
        self.entries.remove(server);
    }

    /// Status of exactly this connection, if any of its rows were not saved.
    pub fn view(&self, server: &str, generation: u64) -> Option<HistoryHealthView> {
        self.entries
            .get(server)
            .filter(|entry| entry.generation == generation)
            .map(|entry| HistoryHealthView {
                saving: entry.saving,
                not_saved: entry.not_saved.to_string(),
            })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Append {
        ticket: String,
        message: ChatMessage,
        outgoing: bool,
        mode: SourceMode,
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
/// Source facts captured at admission, never selected by a storage reply.
pub(crate) struct SourceProvenance {
    pub(crate) mode: SourceMode,
    pub(crate) channel_instance: Option<u64>,
}
enum Pending {
    Append {
        context: Context,
        target: TextTarget,
        peer: Option<User>,
        message: Box<IncomingMessage>,
        mode: SourceMode,
        channel_instance: Option<u64>,
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
    bytes: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    id: i64,
    message: ChatMessage,
    outgoing: bool,
    #[serde(default = "marked_mode")]
    mode: SourceMode,
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
        self.append_raw(
            server,
            generation,
            target,
            peer,
            message,
            SourceProvenance {
                mode: SourceMode::Marked,
                channel_instance: None,
            },
        )
    }
    pub(crate) fn append_raw(
        &mut self,
        server: String,
        generation: u64,
        target: TextTarget,
        peer: Option<User>,
        message: IncomingMessage,
        provenance: SourceProvenance,
    ) -> Result<(), String> {
        let SourceProvenance {
            mode,
            channel_instance,
        } = provenance;
        let charge = message
            .message
            .body
            .len()
            .saturating_mul(2)
            .saturating_add(4096);
        if message.message.body.len() > MAX_CONTENT_BYTES
            || self.bytes.saturating_add(charge) > MAX_PENDING_BYTES
        {
            return Err("Local history is busy; this message could not be saved.".into());
        }
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
            mode,
        });
        self.pending.insert(
            id,
            Pending::Append {
                context,
                target,
                peer,
                message: Box::new(message),
                mode,
                channel_instance,
            },
        );
        self.bytes += charge;
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
        if self.bytes.saturating_add(4096) > MAX_PENDING_BYTES {
            return Err("Local history is busy.".into());
        }
        let id = self.allocate()?;
        self.bytes += 4096;
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
        let (mut inputs, save) =
            self.complete_impl(id, raw, current, |source, _, _| render(source));
        if let Some(save) = save {
            inputs.insert(
                0,
                match save.outcome {
                    SaveOutcome::Saved => Input::HistoryRecovered,
                    SaveOutcome::Failed => Input::HistoryError(SAVE_FAILED.into()),
                },
            );
        }
        inputs
    }

    /// New rows retain raw source/mode. Old rows are explicitly marked legacy
    /// normalized representation; no history is discarded or reparsed here.
    /// A validated Append reports its typed outcome for its original
    /// connection in `save`, instead of one history input per row.
    pub fn complete_raw(
        &mut self,
        id: &str,
        raw: &str,
        current: impl Fn(&str, u64, &str) -> bool,
    ) -> RawCompletion {
        let (live_mode, channel_instance) = match self.pending.get(id) {
            Some(Pending::Append {
                mode,
                channel_instance,
                ..
            }) => (Some(*mode), *channel_instance),
            _ => (None, None),
        };
        let mut stored_modes = BTreeMap::new();
        let (inputs, save) = self.complete_impl(id, raw, current, |_, mode, id| {
            if let Some(id) = id {
                stored_modes.insert(id, mode);
            }
            pending_document()
        });
        RawCompletion {
            inputs,
            live_mode,
            channel_instance,
            stored_modes,
            save,
        }
    }

    fn complete_impl(
        &mut self,
        id: &str,
        raw: &str,
        current: impl Fn(&str, u64, &str) -> bool,
        mut render: impl FnMut(&str, SourceMode, Option<i64>) -> MessageDocument,
    ) -> (Vec<Input>, Option<SaveResult>) {
        let Some(pending) = self.pending.remove(id) else {
            return (Vec::new(), None);
        };
        self.bytes -= match &pending {
            Pending::Append { message, .. } => message
                .message
                .body
                .len()
                .saturating_mul(2)
                .saturating_add(4096),
            Pending::Page { .. } => 4096,
        };
        let context = match &pending {
            Pending::Append { context, .. } | Pending::Page { context, .. } => context,
        };
        if !current(&context.server, context.generation, &context.identity) {
            return (Vec::new(), None);
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
                ..
            } => {
                message.id = response
                    .filter(|_| !failure)
                    .and_then(|response| response.id)
                    .filter(|id| *id > 0 && *id <= 9_007_199_254_740_991);
                let save = SaveResult {
                    server: context.server.clone(),
                    generation: context.generation,
                    outcome: if message.id.is_some() {
                        SaveOutcome::Saved
                    } else {
                        SaveOutcome::Failed
                    },
                };
                let row = Input::Message {
                    server: context.server,
                    generation: context.generation,
                    target,
                    storage_key: message.message.channel.clone(),
                    peer,
                    message: *message,
                };
                (vec![row], Some(save))
            }
            Pending::Page {
                context,
                channel,
                ticket,
                before,
            } => {
                let failed = || {
                    (vec![Input::HistoryFailed { server: context.server.clone(), ticket: ticket.clone(), reason: "Local history could not load this page. Retry after checking browser storage.".into() }], None)
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
                        let document = render(&row.message.body, row.mode, Some(row.id));
                        IncomingMessage::plain(Some(row.id), row.message, document, row.outgoing)
                    })
                    .collect();
                let page = Input::History {
                    server: context.server,
                    ticket,
                    before,
                    messages,
                    next_before: response.next_before,
                };
                (vec![page], None)
            }
        }
    }
}

pub fn pending_document() -> MessageDocument {
    MessageDocument {
        blocks: vec![nuschl_core::ContentBlock::text("[Preparing message…]")],
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

    fn append_raw(storage: &mut Storage, server: &str, generation: u64) -> Result<(), String> {
        storage.append_raw(
            server.into(),
            generation,
            TextTarget::Channel(0),
            None,
            message(),
            SourceProvenance {
                mode: SourceMode::Plain,
                channel_instance: Some(11),
            },
        )
    }
    /// The browser adapter's validation: same server, connection and certificate.
    fn connection(server: &'static str, generation: u64) -> impl Fn(&str, u64, &str) -> bool {
        move |id, old_generation, identity| {
            id == server && old_generation == generation && identity == "ab".repeat(32)
        }
    }

    #[test]
    fn validated_appends_report_a_typed_outcome_for_their_original_connection() {
        for (response, outcome, id) in [
            ("{\"id\":42}", SaveOutcome::Saved, Some(42)),
            ("{\"error\":\"quota\"}", SaveOutcome::Failed, None),
            ("not json", SaveOutcome::Failed, None),
        ] {
            let mut storage = Storage::default();
            append_raw(&mut storage, "server", 4).unwrap();
            let ticket = issued(&mut storage);
            let completed = storage.complete_raw(&ticket, response, connection("server", 4));
            assert_eq!(
                completed.save,
                Some(SaveResult {
                    server: "server".into(),
                    generation: 4,
                    outcome,
                })
            );
            assert_eq!(completed.live_mode, Some(SourceMode::Plain));
            assert_eq!(completed.channel_instance, Some(11));
            // The unsaved or saved row itself is admitted exactly as before;
            // its health is the typed outcome, not one history input per row.
            assert!(matches!(
                completed.inputs.as_slice(),
                [Input::Message { server, generation: 4, message, .. }]
                    if server == "server" && message.id == id && message.message.body == "one\n\ntwo"
            ));
            assert_eq!(storage.bytes, 0);
        }
    }

    #[test]
    fn stale_re_added_changed_certificate_and_repeated_completions_are_inert() {
        let mut storage = Storage::default();
        for _ in 0..4 {
            append_raw(&mut storage, "server", 4).unwrap();
        }
        let tickets: Vec<_> = storage
            .take()
            .into_iter()
            .map(|operation| match operation {
                Operation::Append { ticket, .. } | Operation::Page { ticket, .. } => ticket,
            })
            .collect();
        // Reconnected, removed and re-added under a fresh connection, or the
        // pinned certificate changed: the original pending context is not current.
        let checks: [Box<dyn Fn(&str, u64, &str) -> bool>; 3] = [
            Box::new(connection("server", 5)),
            Box::new(connection("other", 4)),
            Box::new(|id: &str, generation: u64, identity: &str| {
                id == "server" && generation == 4 && identity == "cd".repeat(32)
            }),
        ];
        for (ticket, current) in tickets.iter().zip(checks) {
            let completed = storage.complete_raw(ticket, "{\"error\":\"quota\"}", current);
            assert!(completed.inputs.is_empty());
            assert_eq!(completed.save, None);
        }
        let last = &tickets[3];
        assert!(
            storage
                .complete_raw(last, "{\"id\":1}", connection("server", 4))
                .save
                .is_some()
        );
        let repeated = storage.complete_raw(last, "{\"error\":\"quota\"}", connection("server", 4));
        assert!(repeated.inputs.is_empty() && repeated.save.is_none());
        let unknown = storage.complete_raw("999", "{\"id\":1}", connection("server", 4));
        assert!(unknown.inputs.is_empty() && unknown.save.is_none());
        assert_eq!(storage.bytes, 0);
    }

    #[test]
    fn pages_never_report_save_outcomes() {
        let mut storage = Storage::default();
        let ticket = Navigation::default().browse(
            Conversation {
                server: "server".into(),
                channel: 0,
            },
            HistorySource::LocalObserved,
        );
        for response in ["{\"rows\":[]}", "{\"error\":\"quota\"}"] {
            storage
                .page(
                    "server".into(),
                    4,
                    "ab".repeat(32),
                    "0".into(),
                    ticket.clone(),
                    Page::Before(None),
                )
                .unwrap();
            let id = issued(&mut storage);
            let completed = storage.complete_raw(&id, response, connection("server", 4));
            assert_eq!(completed.save, None);
            assert_eq!(completed.inputs.len(), 1);
        }
    }

    #[test]
    fn a_connection_shows_one_first_notice_per_kind_and_counts_every_unsaved_row() {
        let mut health = HistoryHealth::default();
        let busy: Vec<_> = (0..10_000)
            .map(|_| health.observe("server", 4, HealthEvent::Busy))
            .collect();
        assert_eq!(busy.iter().filter(|first| **first).count(), 1);
        assert!(busy[0]);
        let view = health.view("server", 4).unwrap();
        assert_eq!(view.saving, SaveAvailability::Busy);
        assert_eq!(view.not_saved, "10000");
        let failed = (0..10_000)
            .filter(|_| health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Failed)))
            .count();
        assert_eq!(failed, 1);
        assert_eq!(
            health.view("server", 4),
            Some(HistoryHealthView {
                saving: SaveAvailability::Failing,
                not_saved: "20000".into(),
            })
        );
        // A later save changes current availability only: earlier rows stay
        // unsaved and counted, and the notice budget is not re-armed.
        assert!(!health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Saved)));
        assert_eq!(
            health.view("server", 4),
            Some(HistoryHealthView {
                saving: SaveAvailability::Available,
                not_saved: "20000".into(),
            })
        );
        assert!(!health.observe("server", 4, HealthEvent::Busy));
        assert!(!health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Failed)));
        let view = health.view("server", 4).unwrap();
        assert_eq!(view.saving, SaveAvailability::Failing);
        assert_eq!(view.not_saved, "20002");
    }

    #[test]
    fn unsaved_row_counts_saturate() {
        let mut health = HistoryHealth::default();
        assert!(health.observe("server", 4, HealthEvent::Busy));
        health.entries.get_mut("server").unwrap().not_saved = u64::MAX - 1;
        assert!(!health.observe("server", 4, HealthEvent::Busy));
        assert!(health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Failed)));
        assert!(!health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Failed)));
        assert_eq!(
            health.view("server", 4).unwrap().not_saved,
            u64::MAX.to_string()
        );
    }

    #[test]
    fn newer_connections_reset_and_older_or_forgotten_ones_are_inert() {
        let mut health = HistoryHealth::default();
        assert!(health.observe("server", 5, HealthEvent::Busy));
        // An older connection cannot change the newer one's status.
        assert!(!health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Failed)));
        assert!(!health.observe("server", 4, HealthEvent::Outcome(SaveOutcome::Saved)));
        assert_eq!(health.view("server", 4), None);
        assert_eq!(
            health.view("server", 5),
            Some(HistoryHealthView {
                saving: SaveAvailability::Busy,
                not_saved: "1".into(),
            })
        );
        // A newer connection starts with a fresh count and notice budget.
        assert!(health.observe("server", 6, HealthEvent::Busy));
        assert_eq!(health.view("server", 5), None);
        assert_eq!(health.view("server", 6).unwrap().not_saved, "1");
        // A newer connection that only saves needs no status.
        assert!(!health.observe("server", 7, HealthEvent::Outcome(SaveOutcome::Saved)));
        assert_eq!(health.view("server", 6), None);
        assert_eq!(health.view("server", 7), None);
        assert!(health.entries.is_empty());
        // Removal, replacement or reconnect forgets the server; a late result
        // of that connection is then refused as stale by the adapter's
        // liveness check, and a save alone never creates an entry.
        assert!(health.observe("server", 8, HealthEvent::Busy));
        health.forget("server");
        assert_eq!(health.view("server", 8), None);
        assert!(!health.observe("server", 8, HealthEvent::Outcome(SaveOutcome::Saved)));
        assert!(health.entries.is_empty());
        assert!(!health.observe("server", 0, HealthEvent::Busy));
        assert!(health.entries.is_empty());
    }

    #[test]
    fn status_entries_stay_within_the_catalog_bound() {
        let mut health = HistoryHealth::default();
        for index in 0..MAX_HEALTH_ENTRIES {
            assert!(health.observe(&format!("server-{index}"), 1, HealthEvent::Busy));
        }
        assert!(!health.observe("extra", 1, HealthEvent::Busy));
        assert_eq!(health.view("extra", 1), None);
        assert_eq!(health.entries.len(), MAX_HEALTH_ENTRIES);
        // An existing server's newer connection replaces its own entry.
        assert!(health.observe("server-0", 2, HealthEvent::Busy));
        assert_eq!(health.entries.len(), MAX_HEALTH_ENTRIES);
        health.forget("server-1");
        assert!(health.observe("extra", 1, HealthEvent::Busy));
        assert_eq!(health.entries.len(), MAX_HEALTH_ENTRIES);
    }

    #[test]
    fn a_burst_of_refused_and_failed_rows_yields_one_notice_per_kind_in_order() {
        let mut storage = Storage::default();
        let mut health = HistoryHealth::default();
        // Mirrors the browser adapter: a refused row and a validated Append
        // outcome each update the connection's health in arrival order.
        let mut notices = Vec::new();
        for index in 0..300 {
            if append_raw(&mut storage, "server", 4).is_err()
                && health.observe("server", 4, HealthEvent::Busy)
            {
                notices.push(("busy", index));
            }
        }
        let tickets: Vec<_> = storage
            .take()
            .into_iter()
            .map(|operation| match operation {
                Operation::Append { ticket, .. } | Operation::Page { ticket, .. } => ticket,
            })
            .collect();
        assert_eq!(tickets.len(), MAX_PENDING);
        let mut rows = Vec::new();
        for (index, ticket) in tickets.iter().enumerate() {
            let response = if index % 2 == 0 {
                "{\"error\":\"quota\"}"
            } else {
                "{\"id\":7}"
            };
            let completed = storage.complete_raw(ticket, response, connection("server", 4));
            let save = completed.save.unwrap();
            assert_eq!((save.server.as_str(), save.generation), ("server", 4));
            if health.observe(
                &save.server,
                save.generation,
                HealthEvent::Outcome(save.outcome),
            ) {
                notices.push(("failed", index));
            }
            rows.extend(completed.inputs);
        }
        assert_eq!(notices, [("busy", MAX_PENDING), ("failed", 0)]);
        assert_eq!(rows.len(), MAX_PENDING);
        assert!(rows.iter().all(|row| matches!(row, Input::Message { .. })));
        assert_eq!(
            health.view("server", 4),
            Some(HistoryHealthView {
                // The last answer was a successful save.
                saving: SaveAvailability::Available,
                not_saved: (300 - MAX_PENDING + MAX_PENDING / 2).to_string(),
            })
        );
        assert_eq!(storage.bytes, 0);
    }
}
