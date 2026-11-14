// SPDX-License-Identifier: EUPL-1.2
//! Sender-only composer state. No renderer or received message starts a fetch.
//! The platform owns physical worker retirement; cancelling here only revokes
//! the result and asks the platform to stop its owned work.
use crate::{Client, server::Status as ConnectionStatus};
pub use nuschl_link_preview::{
    card::Card,
    draft::{Delivery, Ineligible, RequestId, Status, SubmissionToken},
    metadata::Unavailable,
    policy::{Mode, Settings},
};
use nuschl_link_preview::{
    draft::{Draft, DraftPreviews, Effect, Ticket},
    policy::Target,
    projection::{self, Projection},
};

pub const DEBOUNCE_MS: u64 = 750;

/// Original visible destination. Public server/certificate information stays
/// in the parent; a fetch worker needs only the target and selected route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub server: String,
    pub channel: u32,
    pub instance: u64,
    pub peer: Option<(u32, u64)>,
    pub certificate: String,
    pub own: (u32, u64),
}

pub type FetchTicket = Ticket<Context>;

/// One submission, independently of its fetch or body. Never reused, even
/// when a Client is recreated around an existing native driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendId(u64);

impl SendId {
    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn from_raw(id: u64) -> Option<Self> {
        (id != 0).then_some(Self(id))
    }

    pub(crate) fn next() -> Option<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        allocate_send_id(&NEXT)
    }
}

fn allocate_send_id(next: &std::sync::atomic::AtomicU64) -> Option<SendId> {
    use std::sync::atomic::Ordering;
    next.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
        if id == 0 { None } else { id.checked_add(1) }
    })
    .ok()
    .and_then(SendId::from_raw)
}

/// One preview-bearing logical message. The native adapter must check this
/// frozen authority both when enqueuing and when consuming the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewSend {
    pub id: SendId,
    pub context: Context,
    pub connection: u64,
    pub target: crate::model::TextTarget,
    pub html: String,
    pub expected_peer: Option<u64>,
    /// The same limits used when projecting this exact HTML. The actor may
    /// refuse changed rich-text limits without parsing HTML in the session.
    pub limits: nuschl_core::model::ServerLimits,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    context: Context,
    connection: u64,
    text: String,
    candidate: Option<String>,
    carry: Result<(), Ineligible>,
}

impl Snapshot {
    fn draft(&self) -> Draft<'_, Context> {
        Draft {
            conversation: &self.context,
            connection: self.connection,
            text: &self.text,
            candidate: self.candidate.as_deref(),
            carry: self.carry,
        }
    }
}

struct Pending {
    ticket: FetchTicket,
    due: u64,
}

#[derive(Default)]
pub(crate) struct Previews {
    drafts: DraftPreviews<Context>,
    settings: Settings,
    current: Option<Snapshot>,
    pending: Option<Pending>,
    admitted: Option<RequestId>,
    revision: u64,
}

impl Previews {
    fn effects(&mut self, effects: Vec<Effect<Context>>, now: u64) -> Vec<RequestId> {
        let mut cancel = Vec::new();
        for effect in effects {
            match effect {
                Effect::Fetch(ticket) => {
                    self.pending = Some(Pending {
                        ticket,
                        due: now.saturating_add(DEBOUNCE_MS),
                    });
                }
                Effect::Cancel(id) => {
                    if self
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.ticket.id == id)
                    {
                        self.pending = None;
                    }
                    if self.admitted == Some(id) {
                        self.admitted = None;
                        cancel.push(id);
                    }
                }
            }
        }
        cancel
    }

    fn pause(&mut self, now: u64) -> Vec<RequestId> {
        let effects = self.drafts.pause();
        self.effects(effects, now)
    }

    fn deactivate(&mut self, now: u64) -> Vec<RequestId> {
        if self.current.take().is_some() {
            self.revision = self.revision.saturating_add(1);
        }
        self.pause(now)
    }

    fn observe(&mut self, snapshot: Snapshot, now: u64) -> Vec<RequestId> {
        let changed = self.current.as_ref().is_none_or(|old| {
            old.context != snapshot.context
                || old.connection != snapshot.connection
                || old.candidate != snapshot.candidate
                || old.carry != snapshot.carry
        });
        let mut cancel = if changed { self.pause(now) } else { Vec::new() };
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        self.current = Some(snapshot);
        let effects = self
            .current
            .as_ref()
            .map(|source| self.drafts.update(source.draft(), now))
            .unwrap_or_default();
        cancel.extend(self.effects(effects, now));
        cancel
    }
}

fn candidate(text: &str, pasted: bool, previous: Option<&str>) -> Option<String> {
    for (start, end) in nuschl_legacy_html::link_ranges(text) {
        let raw = text.get(start..end)?;
        if !(raw
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
            || raw
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("http://")))
        {
            continue;
        }
        // Don't fetch a hostname while the sender is still typing it. A pasted
        // candidate stays complete while later edits leave that URL unchanged.
        if end == text.len() && !pasted && previous != Some(raw) {
            return None;
        }
        Target::parse(raw).ok()?;
        return Some(raw.to_owned());
    }
    None
}

impl Client {
    fn preview_destination(&self) -> Option<(Context, u64, Result<(), Ineligible>)> {
        let (server, channel) = self.selected_indices()?;
        let server = self.servers.get(server)?;
        let channel = server.channels.get(channel)?;
        if server.status != ConnectionStatus::Connected {
            return None;
        }
        let certificate = server.peer_sha256.clone()?;
        let own = server.own_session.and_then(|id| server.users.get(&id))?;
        let carry = if server.mls {
            Err(Ineligible::EndToEnd)
        } else if matches!(
            channel.target,
            crate::model::TextTarget::DirectConversation(_)
        ) {
            // Stock HTML cannot durably carry typed preview metadata to an
            // offline account. Do not silently change that conversation's path.
            Err(Ineligible::ExtensionConversation)
        } else if !server.limits.clone().unwrap_or_default().allow_html {
            Err(Ineligible::HtmlDisabled)
        } else {
            Ok(())
        };
        Some((
            Context {
                server: server.id.clone(),
                channel: channel.id,
                instance: channel.instance,
                peer: channel
                    .direct_peer
                    .as_ref()
                    .map(|peer| (peer.session, peer.instance)),
                certificate,
                own: (own.session, own.instance),
            },
            server.generation,
            carry,
        ))
    }

    fn cancel_preview(&self, cancel: Vec<RequestId>) {
        for id in cancel {
            self.platform().cancel_link_preview(id);
        }
    }

    /// Called only by a sender composer. Text, attachment and editing changes
    /// are explicit inputs; receiving a message cannot create a candidate.
    pub fn composing_link_preview(
        &mut self,
        text: &str,
        pasted: bool,
        attachments: bool,
        editing: bool,
        now: u64,
    ) {
        let destination = (!editing && text.len() <= nuschl_core::MAX_CONTENT_BYTES)
            .then(|| self.preview_destination())
            .flatten();
        let Some((context, connection, mut carry)) = destination else {
            let cancel = self.previews.deactivate(now);
            self.cancel_preview(cancel);
            return;
        };
        if attachments {
            carry = Err(Ineligible::Attachments);
        }
        let previous = self
            .previews
            .current
            .as_ref()
            .filter(|old| old.context == context)
            .and_then(|old| old.candidate.as_deref());
        let candidate = candidate(text, pasted, previous);
        let cancel = self.previews.observe(
            Snapshot {
                context,
                connection,
                text: text.to_owned(),
                candidate,
                carry,
            },
            now,
        );
        self.cancel_preview(cancel);
        if self.sending() {
            self.pause_submitted_link_preview(now);
        }
    }

    /// A bounded timer wake, not a render-frame polling hook.
    pub fn poll_link_preview(&mut self, now: u64) {
        self.validate_link_preview(now);
        let Some(pending) = self.previews.pending.take_if(|pending| now >= pending.due) else {
            return;
        };
        self.previews.admitted = Some(pending.ticket.id);
        let id = pending.ticket.id;
        if let Err(reason) = self
            .platform()
            .link_preview(self.previews.settings.clone(), pending.ticket)
        {
            self.receive_link_preview(id, Err(reason), now);
        }
    }

    pub fn link_preview_deadline(&self) -> Option<u64> {
        self.previews.pending.as_ref().map(|pending| pending.due)
    }

    pub fn link_preview_settings(&self) -> &Settings {
        &self.previews.settings
    }

    pub fn set_link_preview_settings(&mut self, settings: Settings, now: u64) {
        // The first settings always apply: the drafts start disabled.
        if self.previews.drafts.policy_generation() != 0 && self.previews.settings == settings {
            return;
        }
        self.previews.settings = settings;
        self.previews.revision = self.previews.revision.saturating_add(1);
        let effects = self.previews.drafts.set_policy(&self.previews.settings);
        let cancel = self.previews.effects(effects, now);
        self.cancel_preview(cancel);
        if let Some(current) = self.previews.current.clone() {
            let cancel = self.previews.observe(current, now);
            self.cancel_preview(cancel);
        }
    }

    pub fn remove_link_preview(&mut self, now: u64) {
        let Some(current) = self.previews.current.as_ref() else {
            return;
        };
        let effects = self.previews.drafts.remove(&current.context);
        self.previews.revision = self.previews.revision.saturating_add(1);
        let cancel = self.previews.effects(effects, now);
        self.cancel_preview(cancel);
    }

    pub fn retry_link_preview(&mut self, now: u64) {
        let Some(current) = self.previews.current.as_ref() else {
            return;
        };
        let effects = match self.previews.drafts.status(&current.context) {
            Status::Removed => self.previews.drafts.restore(current.draft(), now),
            _ => self.previews.drafts.retry(current.draft(), now),
        };
        self.previews.revision = self.previews.revision.saturating_add(1);
        let cancel = self.previews.effects(effects, now);
        self.cancel_preview(cancel);
    }

    pub fn link_preview_status(&self) -> Status<'_> {
        self.previews
            .current
            .as_ref()
            .map_or(Status::None, |current| {
                self.previews.drafts.status(&current.context)
            })
    }

    pub fn link_preview_binding(&self) -> Option<(&Context, u64, u64, u64)> {
        self.previews.current.as_ref().map(|current| {
            (
                &current.context,
                current.connection,
                self.previews.drafts.policy_generation(),
                self.previews.revision,
            )
        })
    }

    pub(crate) fn validate_link_preview(&mut self, now: u64) {
        let destination = self.preview_destination();
        let valid = self.previews.current.as_ref().is_none_or(|current| {
            destination
                .as_ref()
                .is_some_and(|(context, connection, carry)| {
                    *context == current.context
                        && *connection == current.connection
                        && (current.carry == Err(Ineligible::Attachments)
                            || *carry == current.carry)
                })
        });
        if !valid {
            let cancel = self.previews.deactivate(now);
            self.cancel_preview(cancel);
        }
    }

    pub(crate) fn receive_link_preview(
        &mut self,
        id: RequestId,
        result: Result<Card, Unavailable>,
        now: u64,
    ) {
        self.validate_link_preview(now);
        if self.previews.admitted != Some(id) {
            return;
        }
        self.previews.admitted = None;
        if self.previews.drafts.deliver(id, result, now) == Delivery::Accepted {
            self.previews.revision = self.previews.revision.saturating_add(1);
        }
    }

    pub(crate) fn link_preview_wire(
        &self,
        body: &str,
    ) -> Option<(Context, u64, String, nuschl_core::model::ServerLimits)> {
        let source = self.previews.current.as_ref()?;
        let (context, connection, carry) = self.preview_destination()?;
        if context != source.context
            || connection != source.connection
            || carry.is_err()
            || source.text != body
        {
            return None;
        }
        let card = self.previews.drafts.for_send(source.draft())?;
        self.previews
            .drafts
            .submission_token(&context, connection, body)?;
        let (server, _) = self.selected_indices()?;
        let limits = self.servers[server].limits.clone().unwrap_or_default();
        match projection::project(body, card, &limits) {
            Projection::Card { html, .. } => Some((context, connection, html, limits)),
            Projection::TextOnly(_) => None,
        }
    }

    pub(crate) fn link_preview_submission(&self, body: &str) -> Option<(Context, SubmissionToken)> {
        let current = self.previews.current.as_ref()?;
        let (context, connection, _) = self.preview_destination()?;
        if context != current.context || connection != current.connection || current.text != body {
            return None;
        }
        self.previews
            .drafts
            .submission_token(&context, connection, body)
            .map(|token| (context, token))
    }

    pub(crate) fn pause_submitted_link_preview(&mut self, now: u64) {
        let cancel = self.previews.pause(now);
        self.cancel_preview(cancel);
    }

    pub(crate) fn acknowledge_link_preview(
        &mut self,
        context: &Context,
        token: SubmissionToken,
        now: u64,
    ) -> bool {
        if !self.previews.drafts.submission_is_current(context, token) {
            return false;
        }
        let effects = self.previews.drafts.sent_if_current(context, token);
        let cancel = self.previews.effects(effects, now);
        self.cancel_preview(cancel);
        self.previews.revision = self.previews.revision.saturating_add(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_ids_do_not_wrap_or_reuse_zero() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let next = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_send_id(&next).unwrap().get(), u64::MAX - 1);
        assert_eq!(allocate_send_id(&next), None);
        assert_eq!(allocate_send_id(&next), None);
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(allocate_send_id(&AtomicU64::new(0)), None);
        assert_eq!(SendId::from_raw(0), None);
    }

    fn source(text: &str) -> Snapshot {
        Snapshot {
            context: Context {
                server: "fixture".into(),
                channel: 1,
                instance: 2,
                peer: None,
                certificate: "fixture-certificate".into(),
                own: (5, 6),
            },
            connection: 3,
            text: text.into(),
            candidate: Some("https://example.org/".into()),
            carry: Ok(()),
        }
    }

    fn direct() -> Previews {
        let mut previews = Previews::default();
        previews.settings.mode = Mode::Direct;
        let effects = previews.drafts.set_policy(&previews.settings);
        assert!(previews.effects(effects, 0).is_empty());
        previews
    }

    #[test]
    fn candidate_requires_completed_or_explicitly_pasted_web_link() {
        assert_eq!(candidate("https://example.org/", false, None), None);
        for (text, pasted) in [
            ("https://example.org/", true),
            ("https://example.org/ ", false),
        ] {
            assert_eq!(
                candidate(text, pasted, None).as_deref(),
                Some("https://example.org/")
            );
        }
        assert_eq!(
            candidate("https://example.org/", false, Some("https://example.org/")).as_deref(),
            Some("https://example.org/")
        );
        assert_eq!(candidate("mumble://example.org/ ", true, None), None);
        assert_eq!(candidate("http://127.0.0.1/ ", true, None), None);
    }

    #[test]
    fn debounce_cancel_and_late_delivery_preserve_new_destination() {
        let mut previews = direct();
        assert!(
            previews
                .observe(source("https://example.org/ "), 10)
                .is_empty()
        );
        let first = previews.pending.as_ref().unwrap().ticket.clone();
        assert_eq!(previews.pending.as_ref().unwrap().due, 10 + DEBOUNCE_MS);
        previews.pending = None;
        previews.admitted = Some(first.id);
        let mut next = source("https://example.org/ ");
        next.context.instance += 1;
        assert_eq!(previews.observe(next.clone(), 20), vec![first.id]);
        assert_eq!(
            previews
                .drafts
                .deliver(first.id, Err(Unavailable::Network), 21),
            Delivery::Stale
        );
        assert_eq!(previews.current.as_ref(), Some(&next));
        assert_ne!(previews.pending.as_ref().unwrap().ticket.id, first.id);
    }

    #[test]
    fn previews_before_the_first_policy_and_off_never_schedule_network_work() {
        let mut previews = Previews::default();
        previews.observe(source("https://example.org/ "), 0);
        assert!(previews.pending.is_none());
        previews.settings.mode = Mode::Off;
        let effects = previews.drafts.set_policy(&previews.settings);
        previews.effects(effects, 0);
        previews.observe(source("https://example.org/ "), 1);
        assert!(previews.pending.is_none());
    }

    #[test]
    fn paused_submission_does_not_erase_new_remove_on_late_ack() {
        let mut previews = direct();
        let current = source("https://example.org/ ");
        previews.observe(current.clone(), 0);
        let token = previews
            .drafts
            .submission_token(&current.context, current.connection, &current.text)
            .unwrap();
        previews.pause(1);
        previews.drafts.remove(&current.context);
        assert!(
            previews
                .drafts
                .sent_if_current(&current.context, token)
                .is_empty()
        );
        assert_eq!(previews.drafts.status(&current.context), Status::Removed);
    }
}
