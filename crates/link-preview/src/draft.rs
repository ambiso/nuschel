// SPDX-License-Identifier: EUPL-1.2
//! Draft binding: which card, if any, may go out with the exact text the
//! sender is about to send.
//!
//! Pure state, portable: no clock, I/O, fetch or worker. The caller passes
//! monotonic milliseconds and performs the returned [`Effect`]s; only the
//! composer API creates fetch tickets, so received messages never can.
//!
//! A card is bound to the conversation, the connection generation, the
//! policy generation, the canonical URL and the exact draft text. Edits that
//! keep the URL keep the card (or the fetch in flight) and rebind it to the
//! new text; a changed URL, connection, carry decision or policy drops it.
//! Removing a card suppresses previews for that URL until the URL changes,
//! the message is sent or the sender asks again. Every fetch has a fresh
//! request ID, and only the one request in flight can deliver: a late result
//! for a cancelled, removed, superseded or reconnected draft is refused.
use crate::card::Card;
use crate::policy::{Settings, Target};
use std::collections::VecDeque;

/// Drafts with preview state; the least recently used is forgotten (only its
/// card is lost).
pub const MAX_CONVERSATIONS: usize = 64;
/// A longer draft never gets a preview (it is beyond the message limit).
pub const MAX_DRAFT_BYTES: usize = nuschl_core::MAX_CONTENT_BYTES;
/// Draft text kept for exact matching, across all conversations.
pub const MAX_KEPT_TEXT_BYTES: usize = 1024 * 1024;
/// Results remembered by URL (memory only, cleared on every policy change).
pub const MAX_CACHE_ENTRIES: usize = 16;
pub const MAX_CACHE_BYTES: usize = 1024 * 1024;
pub const CARD_TTL_MS: u64 = 10 * 60 * 1000;
pub const FAILURE_TTL_MS: u64 = 2 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

/// The exact composer intent consumed by a successful send. Opaque and local;
/// neither identical later text nor an old acknowledgement can recreate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmissionToken {
    revision: u64,
    policy: u64,
}

impl RequestId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// What a fetch is for. The platform answers with the ID only; everything
/// else stays here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket<K> {
    pub id: RequestId,
    pub conversation: K,
    pub connection: u64,
    pub policy: u64,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect<K> {
    Fetch(Ticket<K>),
    /// Abort this request. Its process credit is the platform's to keep
    /// until the worker has actually exited.
    Cancel(RequestId),
}

/// Why the current target cannot carry a preview (decided by the SDK from
/// the server, the conversation and the composer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ineligible {
    HtmlDisabled,
    EndToEnd,
    ExtensionConversation,
    Attachments,
}

pub use crate::metadata::Unavailable;

/// The composer as it is now.
#[derive(Debug)]
pub struct Draft<'a, K> {
    pub conversation: &'a K,
    pub connection: u64,
    /// The exact text that Send would send.
    pub text: &'a str,
    /// The preview candidate as it appears in `text` (the first web link,
    /// once terminated or pasted), if any.
    pub candidate: Option<&'a str>,
    pub carry: Result<(), Ineligible>,
}

impl<K> Clone for Draft<'_, K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K> Copy for Draft<'_, K> {}

/// What the composer shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status<'a> {
    None,
    /// Link previews are off or the proxy settings are incomplete.
    Disabled,
    Fetching,
    Ready(&'a Card),
    Unavailable(Unavailable),
    Removed,
    TextOnly(Ineligible),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    /// Not the request in flight, or its draft changed: ignored.
    Stale,
}

#[derive(Clone, Debug)]
enum Phase {
    Idle,
    Disabled,
    Fetching(RequestId),
    Ready(Card),
    Unavailable(Unavailable),
    Removed,
    TextOnly(Ineligible),
}

#[derive(Clone, Debug)]
struct State {
    revision: u64,
    connection: u64,
    text: String,
    target: Option<Target>,
    /// Removal belongs to `target`; changing that target clears it.
    removed: bool,
    phase: Phase,
    carry: Result<(), Ineligible>,
}

#[derive(Clone, Debug)]
struct Cached {
    url: String,
    at: u64,
    outcome: Result<Card, Unavailable>,
}

impl Cached {
    fn weight(&self) -> usize {
        self.url.len() + self.outcome.as_ref().map_or(0, Card::weight)
    }
}

#[derive(Clone, Debug)]
pub struct DraftPreviews<K> {
    enabled: bool,
    policy: u64,
    next_request: u64,
    next_revision: u64,
    exhausted: bool,
    in_flight: Option<RequestId>,
    /// Least recently used first.
    drafts: VecDeque<(K, State)>,
    /// Least recently used first.
    cache: VecDeque<Cached>,
}

impl<K: Clone + Eq> Default for DraftPreviews<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone + Eq> DraftPreviews<K> {
    /// Starts disabled until the first policy.
    pub fn new() -> Self {
        Self {
            enabled: false,
            policy: 0,
            next_request: 0,
            next_revision: 0,
            exhausted: false,
            in_flight: None,
            drafts: VecDeque::new(),
            cache: VecDeque::new(),
        }
    }

    pub fn policy_generation(&self) -> u64 {
        self.policy
    }

    /// A new fetch policy: previews are enabled only when it yields a route
    /// (Off and incomplete proxy settings do not). The request in flight is
    /// cancelled, results and cards are dropped, removals are kept.
    pub fn set_policy(&mut self, settings: &Settings) -> Vec<Effect<K>> {
        if self.exhausted {
            return Vec::new();
        }
        let Some(next) = self.policy.checked_add(1) else {
            self.refuse_exhaustion();
            return Vec::new();
        };
        self.policy = next;
        self.enabled = settings.route().is_ok();
        self.cache.clear();
        let effects = self.cancel_in_flight().into_iter().collect();
        for (_, state) in &mut self.drafts {
            state.phase = Phase::Idle;
        }
        effects
    }

    /// The composer changed (or any input of its decision did). Effects say
    /// which fetch to start or cancel.
    pub fn update(&mut self, draft: Draft<'_, K>, now: u64) -> Vec<Effect<K>> {
        let mut effects = Vec::new();
        if self.exhausted {
            return effects;
        }
        let target = candidate(draft.text, draft.candidate);
        if !self.touch(draft.conversation, draft.connection, &mut effects) {
            return effects;
        }
        // Taken out while deciding; nothing below cancels it implicitly.
        let (key, mut state) = self.drafts.pop_back().expect("touched");
        if state.connection != draft.connection
            || state.text != draft.text
            || state.target != target
            || state.carry != draft.carry
        {
            let Some(revision) = self.new_revision() else {
                if !matches!(state.phase, Phase::Removed) {
                    state.phase = Phase::Disabled;
                }
                self.drafts.push_back((key, state));
                return Vec::new();
            };
            state.revision = revision;
        }
        state.carry = draft.carry;
        let target_changed = state.target != target;
        if state.connection != draft.connection || target_changed {
            effects.extend(self.cancel_phase(&state.phase));
            state.connection = draft.connection;
            if target_changed {
                state.removed = false;
            }
            state.target = target;
            state.phase = Phase::Idle;
        }
        // Do not retain the capacity of a formerly large draft after it is
        // cleared: the aggregate kept-text budget applies to retained storage.
        state.text = if draft.text.len() <= MAX_DRAFT_BYTES {
            draft.text.to_owned()
        } else {
            String::new()
        };
        state.phase = match (state.phase, &state.target) {
            (phase, _) if draft.carry.is_err() || !self.enabled || state.target.is_none() => {
                effects.extend(self.cancel_phase(&phase));
                match draft.carry {
                    Err(reason) => Phase::TextOnly(reason),
                    Ok(()) if !self.enabled => Phase::Disabled,
                    Ok(()) => Phase::Idle,
                }
            }
            (phase, _) if state.removed => {
                effects.extend(self.cancel_phase(&phase));
                Phase::Removed
            }
            (phase @ (Phase::Fetching(_) | Phase::Ready(_) | Phase::Unavailable(_)), _) => phase,
            (_, Some(target)) => {
                let target = target.clone();
                self.evaluate(
                    draft.conversation,
                    draft.connection,
                    target,
                    now,
                    &mut effects,
                )
            }
            (_, None) => Phase::Idle,
        };
        self.drafts.push_back((key, state));
        self.enforce_text_budget(&mut effects);
        effects
    }

    /// A fetch result from the platform.
    pub fn deliver(
        &mut self,
        id: RequestId,
        outcome: Result<Card, Unavailable>,
        now: u64,
    ) -> Delivery {
        if self.in_flight != Some(id) {
            return Delivery::Stale;
        }
        self.in_flight = None;
        let Some((_, state)) = self
            .drafts
            .iter_mut()
            .find(|(_, state)| matches!(state.phase, Phase::Fetching(pending) if pending == id))
        else {
            return Delivery::Stale;
        };
        let Some(target) = state.target.clone() else {
            state.phase = Phase::Idle;
            return Delivery::Stale;
        };
        // A worker cannot substitute another page's card.
        let outcome = outcome.and_then(|card| {
            if card.is_for(target.as_str()) {
                Ok(card)
            } else {
                Err(Unavailable::Malformed)
            }
        });
        state.phase = match &outcome {
            Ok(card) => Phase::Ready(card.clone()),
            Err(reason) => Phase::Unavailable(*reason),
        };
        // `set_policy` cancels the request in flight, so an accepted result
        // always belongs to the current, enabled policy.
        self.remember(target.as_str(), outcome, now);
        Delivery::Accepted
    }

    /// The card that may go out with exactly this draft, if any. The caller
    /// passes the same inputs it would pass to [`Self::update`], computed
    /// again at Send.
    pub fn for_send(&self, draft: Draft<'_, K>) -> Option<&Card> {
        if !self.enabled || draft.carry.is_err() || draft.text.len() > MAX_DRAFT_BYTES {
            return None;
        }
        let target = candidate(draft.text, draft.candidate)?;
        let (_, state) = self
            .drafts
            .iter()
            .find(|(key, _)| key == draft.conversation)?;
        let Phase::Ready(card) = &state.phase else {
            return None;
        };
        let bound = state.connection == draft.connection
            && state.text == draft.text
            && state.target.as_ref().map(Target::as_str) == Some(target.as_str())
            && !state.removed
            && card.is_for(target.as_str());
        bound.then_some(card)
    }

    pub fn submission_token(
        &self,
        conversation: &K,
        connection: u64,
        text: &str,
    ) -> Option<SubmissionToken> {
        if self.exhausted {
            return None;
        }
        let (_, state) = self.drafts.iter().find(|(key, state)| {
            key == conversation && state.connection == connection && state.text == text
        })?;
        Some(SubmissionToken {
            revision: state.revision,
            policy: self.policy,
        })
    }

    /// Whether a successful send still names this conversation's original
    /// composer intent, including when another conversation is selected.
    pub fn submission_is_current(&self, conversation: &K, token: SubmissionToken) -> bool {
        !self.exhausted
            && token.policy == self.policy
            && self
                .drafts
                .iter()
                .any(|(key, state)| key == conversation && state.revision == token.revision)
    }

    /// Only an acknowledged successful send consumes the original intent.
    /// Capturing a token or a failed submission changes nothing by itself.
    pub fn sent_if_current(&mut self, conversation: &K, token: SubmissionToken) -> Vec<Effect<K>> {
        if !self.submission_is_current(conversation, token) {
            return Vec::new();
        }
        self.forget(conversation)
    }

    /// Navigation suspends pending work without forgetting a removed URL or
    /// a completed card. The platform still retains physical worker credit.
    pub fn pause(&mut self) -> Vec<Effect<K>> {
        self.cancel_in_flight().into_iter().collect()
    }

    /// Explicitly retry this draft's unavailable target, without reviving a
    /// removed card. Only `restore` undoes the sender's Remove decision.
    pub fn retry(&mut self, draft: Draft<'_, K>, now: u64) -> Vec<Effect<K>> {
        if let Some(target) = candidate(draft.text, draft.candidate)
            && let Some(index) = self.drafts.iter().position(|(key, state)| {
                key == draft.conversation
                    && state.connection == draft.connection
                    && state.target.as_ref() == Some(&target)
                    && matches!(state.phase, Phase::Unavailable(_))
            })
        {
            let Some(revision) = self.new_revision() else {
                return Vec::new();
            };
            self.drafts[index].1.revision = revision;
            self.drafts[index].1.phase = Phase::Idle;
            self.cache
                .retain(|entry| entry.url != target.as_str() || entry.outcome.is_ok());
        }
        self.update(draft, now)
    }

    /// The conversation's composer is gone.
    pub fn forget(&mut self, conversation: &K) -> Vec<Effect<K>> {
        let Some(index) = self.drafts.iter().position(|(key, _)| key == conversation) else {
            return Vec::new();
        };
        let (_, state) = self.drafts.remove(index).expect("found");
        self.cancel_phase(&state.phase).into_iter().collect()
    }

    /// The sender removed the card (×): no preview for this URL until it
    /// changes, the message is sent or [`Self::restore`] is called. A result
    /// still in flight is cancelled and refused if it arrives.
    pub fn remove(&mut self, conversation: &K) -> Vec<Effect<K>> {
        let Some(index) = self.drafts.iter().position(|(key, _)| key == conversation) else {
            return Vec::new();
        };
        if self.drafts[index].1.target.is_none() {
            return Vec::new();
        }
        let Some(revision) = self.new_revision() else {
            return Vec::new();
        };
        self.drafts[index].1.revision = revision;
        let phase = std::mem::replace(&mut self.drafts[index].1.phase, Phase::Idle);
        let effects = self.cancel_phase(&phase).into_iter().collect();
        let state = &mut self.drafts[index].1;
        state.removed = true;
        state.phase = Phase::Removed;
        effects
    }

    /// The sender explicitly asked for the removed preview again.
    pub fn restore(&mut self, draft: Draft<'_, K>, now: u64) -> Vec<Effect<K>> {
        if let Some(index) = self
            .drafts
            .iter()
            .position(|(key, state)| key == draft.conversation && state.removed)
        {
            let Some(revision) = self.new_revision() else {
                return Vec::new();
            };
            let state = &mut self.drafts[index].1;
            state.revision = revision;
            state.removed = false;
            if matches!(state.phase, Phase::Removed) {
                state.phase = Phase::Idle;
            }
        }
        self.update(draft, now)
    }

    pub fn status(&self, conversation: &K) -> Status<'_> {
        let Some((_, state)) = self.drafts.iter().find(|(key, _)| key == conversation) else {
            return if self.enabled {
                Status::None
            } else {
                Status::Disabled
            };
        };
        match &state.phase {
            Phase::Idle => Status::None,
            Phase::Disabled => Status::Disabled,
            Phase::Fetching(_) => Status::Fetching,
            Phase::Ready(card) => Status::Ready(card),
            Phase::Unavailable(reason) => Status::Unavailable(*reason),
            Phase::Removed => Status::Removed,
            Phase::TextOnly(reason) => Status::TextOnly(*reason),
        }
    }

    /// Make the draft for `conversation` the last, most recently used one; a
    /// new one evicts the least recently used beyond [`MAX_CONVERSATIONS`].
    fn touch(&mut self, conversation: &K, connection: u64, effects: &mut Vec<Effect<K>>) -> bool {
        if let Some(index) = self.drafts.iter().position(|(key, _)| key == conversation) {
            let entry = self.drafts.remove(index).expect("found");
            self.drafts.push_back(entry);
        } else {
            let Some(revision) = self.new_revision() else {
                return false;
            };
            if self.drafts.len() == MAX_CONVERSATIONS
                && let Some((_, evicted)) = self.drafts.pop_front()
            {
                effects.extend(self.cancel_phase(&evicted.phase));
            }
            self.drafts.push_back((
                conversation.clone(),
                State {
                    revision,
                    connection,
                    text: String::new(),
                    target: None,
                    removed: false,
                    phase: Phase::Idle,
                    carry: Ok(()),
                },
            ));
        }
        true
    }

    fn refuse_exhaustion(&mut self) {
        self.exhausted = true;
        self.enabled = false;
        self.in_flight = None;
        self.cache.clear();
        for (_, state) in &mut self.drafts {
            if !matches!(state.phase, Phase::Removed) {
                state.phase = Phase::Disabled;
            }
        }
    }

    fn new_revision(&mut self) -> Option<u64> {
        if self.exhausted {
            return None;
        }
        let Some(next) = self.next_revision.checked_add(1) else {
            self.refuse_exhaustion();
            return None;
        };
        self.next_revision = next;
        Some(next)
    }

    /// Forget the least recently used drafts while their kept text exceeds
    /// the budget (the most recent draft is always kept).
    fn enforce_text_budget(&mut self, effects: &mut Vec<Effect<K>>) {
        let mut total: usize = self.drafts.iter().map(|(_, state)| state.text.len()).sum();
        while total > MAX_KEPT_TEXT_BYTES && self.drafts.len() > 1 {
            let (_, evicted) = self.drafts.pop_front().expect("more than one");
            total -= evicted.text.len();
            effects.extend(self.cancel_phase(&evicted.phase));
        }
    }

    /// A cached result, or a new fetch (cancelling the one in flight).
    fn evaluate(
        &mut self,
        conversation: &K,
        connection: u64,
        target: Target,
        now: u64,
        effects: &mut Vec<Effect<K>>,
    ) -> Phase {
        if let Some(outcome) = self.cached(target.as_str(), now) {
            return match outcome {
                Ok(card) => Phase::Ready(card),
                Err(reason) => Phase::Unavailable(reason),
            };
        }
        effects.extend(self.cancel_in_flight());
        let Some(next) = self.next_request.checked_add(1) else {
            self.refuse_exhaustion();
            return Phase::Unavailable(Unavailable::Malformed);
        };
        self.next_request = next;
        let id = RequestId(self.next_request);
        self.in_flight = Some(id);
        effects.push(Effect::Fetch(Ticket {
            id,
            conversation: conversation.clone(),
            connection,
            policy: self.policy,
            target,
        }));
        Phase::Fetching(id)
    }

    fn cancel_phase(&mut self, phase: &Phase) -> Option<Effect<K>> {
        match phase {
            Phase::Fetching(id) if self.in_flight == Some(*id) => self.cancel_in_flight(),
            _ => None,
        }
    }

    /// Cancel the request in flight; its draft returns to Idle, so its next
    /// update asks again.
    fn cancel_in_flight(&mut self) -> Option<Effect<K>> {
        let id = self.in_flight.take()?;
        for (_, state) in &mut self.drafts {
            if matches!(state.phase, Phase::Fetching(pending) if pending == id) {
                state.phase = Phase::Idle;
            }
        }
        Some(Effect::Cancel(id))
    }

    fn cached(&mut self, url: &str, now: u64) -> Option<Result<Card, Unavailable>> {
        self.cache.retain(|entry| {
            let ttl = if entry.outcome.is_ok() {
                CARD_TTL_MS
            } else {
                FAILURE_TTL_MS
            };
            now.saturating_sub(entry.at) < ttl
        });
        let index = self.cache.iter().position(|entry| entry.url == url)?;
        let entry = self.cache.remove(index).expect("found");
        let outcome = entry.outcome.clone();
        self.cache.push_back(entry);
        Some(outcome)
    }

    fn remember(&mut self, url: &str, outcome: Result<Card, Unavailable>, now: u64) {
        self.cache.retain(|entry| entry.url != url);
        let entry = Cached {
            url: url.to_owned(),
            at: now,
            outcome,
        };
        if entry.weight() > MAX_CACHE_BYTES {
            return;
        }
        self.cache.push_back(entry);
        let mut total: usize = self.cache.iter().map(Cached::weight).sum();
        while self.cache.len() > MAX_CACHE_ENTRIES || total > MAX_CACHE_BYTES {
            let evicted = self.cache.pop_front().expect("not empty");
            total -= evicted.weight();
        }
    }
}

/// The candidate as a fetch target: it must occur in the draft, and the draft
/// must be within the message limit. A URL the target policy refuses is not
/// a candidate.
fn candidate(text: &str, candidate: Option<&str>) -> Option<Target> {
    let candidate = candidate?;
    if text.len() > MAX_DRAFT_BYTES || !text.contains(candidate) {
        return None;
    }
    Target::parse(candidate).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::tests::card;
    use crate::policy::{DnsSettings, Mode};

    type Previews = DraftPreviews<u32>;

    fn direct() -> Settings {
        Settings {
            mode: Mode::Direct,
            ..Settings::default()
        }
    }

    fn enabled() -> Previews {
        let mut previews = Previews::new();
        assert!(previews.set_policy(&direct()).is_empty());
        previews
    }

    fn draft<'a>(
        conversation: &'a u32,
        text: &'a str,
        candidate: Option<&'a str>,
    ) -> Draft<'a, u32> {
        Draft {
            conversation,
            connection: 1,
            text,
            candidate,
            carry: Ok(()),
        }
    }

    const URL: &str = "https://example.org/a";
    const OTHER: &str = "https://example.net/b";

    #[test]
    fn old_send_ack_cannot_erase_a_new_remove_or_identical_retyped_text() {
        let mut previews = enabled();
        previews.update(draft(&1, URL, Some(URL)), 0);
        let original = previews.submission_token(&1, 1, URL).unwrap();
        assert!(previews.submission_is_current(&1, original));
        assert!(!previews.submission_is_current(&2, original));
        previews.remove(&1);
        assert!(!previews.submission_is_current(&1, original));
        assert!(previews.sent_if_current(&1, original).is_empty());
        assert_eq!(previews.status(&1), Status::Removed);
        let removed = previews.submission_token(&1, 1, URL).unwrap();
        assert_ne!(original, removed);
        assert!(previews.submission_is_current(&1, removed));
        previews.update(draft(&1, "changed", None), 1);
        previews.update(draft(&1, URL, Some(URL)), 2);
        let retyped = previews.submission_token(&1, 1, URL).unwrap();
        assert_ne!(original, retyped);
        assert_ne!(removed, retyped);
        assert!(!previews.submission_is_current(&1, original));
        assert!(!previews.submission_is_current(&1, removed));
        assert!(previews.submission_is_current(&1, retyped));
        assert!(previews.sent_if_current(&1, removed).is_empty());
        assert!(previews.submission_token(&1, 1, URL).is_some());
    }

    #[test]
    fn submission_capture_is_inert_pause_preserves_it_and_only_current_success_consumes() {
        let mut previews = enabled();
        let pending = fetched(&previews.update(draft(&1, URL, Some(URL)), 0))[0].id;
        let token = previews.submission_token(&1, 1, URL).unwrap();
        // A failed submission makes no cleanup call; capturing alone changes nothing.
        assert_eq!(previews.status(&1), Status::Fetching);
        assert_eq!(previews.submission_token(&1, 1, URL), Some(token));
        assert_eq!(cancelled(&previews.pause()), [pending]);
        assert!(previews.submission_is_current(&1, token));
        assert_eq!(previews.submission_token(&1, 1, URL), Some(token));
        previews.sent_if_current(&1, token);
        assert!(!previews.submission_is_current(&1, token));
        assert!(previews.submission_token(&1, 1, URL).is_none());
        // A text-only send can consume a pending preview without requiring Ready.
        let pending = fetched(&previews.update(draft(&1, URL, Some(URL)), 1))[0].id;
        let token = previews.submission_token(&1, 1, URL).unwrap();
        assert_eq!(cancelled(&previews.sent_if_current(&1, token)), [pending]);
        assert!(previews.submission_token(&1, 1, URL).is_none());
        // A reused conversation/text gets a fresh token, and policy changes invalidate it.
        previews.update(draft(&1, URL, Some(URL)), 2);
        let token = previews.submission_token(&1, 1, URL).unwrap();
        previews.set_policy(&direct());
        assert!(!previews.submission_is_current(&1, token));
        assert!(previews.sent_if_current(&1, token).is_empty());
        assert!(previews.submission_token(&1, 1, URL).is_some());
    }

    #[test]
    fn counter_exhaustion_refuses_work_and_never_reuses_ids_or_tokens() {
        for counter in 0..3 {
            let mut previews = enabled();
            let old = fetched(&previews.update(draft(&1, URL, Some(URL)), 0))[0].id;
            let token = previews.submission_token(&1, 1, URL).unwrap();
            match counter {
                0 => {
                    previews.policy = u64::MAX;
                    assert!(previews.set_policy(&direct()).is_empty());
                }
                1 => {
                    previews.next_request = u64::MAX;
                    assert!(fetched(&previews.update(draft(&2, OTHER, Some(OTHER)), 1)).is_empty());
                }
                _ => {
                    previews.next_revision = u64::MAX;
                    assert!(previews.update(draft(&2, OTHER, Some(OTHER)), 1).is_empty());
                }
            }
            assert!(previews.exhausted);
            assert!(!previews.submission_is_current(&1, token));
            assert!(previews.submission_token(&1, 1, URL).is_none());
            assert!(previews.sent_if_current(&1, token).is_empty());
            assert_eq!(
                previews.deliver(old, Ok(card(URL, "late", "")), 2),
                Delivery::Stale
            );
            assert!(previews.update(draft(&1, URL, Some(URL)), 3).is_empty());
        }
    }

    #[test]
    fn navigation_pauses_pending_work_without_reviving_removed_urls() {
        let mut previews = enabled();
        let pending = fetched(&previews.update(draft(&1, URL, Some(URL)), 0))[0].id;
        assert_eq!(cancelled(&previews.pause()), [pending]);
        assert_eq!(
            previews.deliver(pending, Ok(card(URL, "late", "")), 1),
            Delivery::Stale
        );
        previews.update(draft(&1, URL, Some(URL)), 2);
        previews.remove(&1);
        previews.pause();
        previews.update(draft(&2, "no URL", None), 3);
        assert!(previews.update(draft(&1, URL, Some(URL)), 4).is_empty());
        assert_eq!(previews.status(&1), Status::Removed);
    }

    #[test]
    fn retry_only_resets_the_matching_failure_and_never_undoes_remove() {
        let mut previews = enabled();
        let pending = fetched(&previews.update(draft(&1, URL, Some(URL)), 0))[0].id;
        assert_eq!(
            previews.deliver(pending, Err(Unavailable::Network), 1),
            Delivery::Accepted
        );
        assert!(previews.update(draft(&1, URL, Some(URL)), 2).is_empty());
        let next = fetched(&previews.retry(draft(&1, URL, Some(URL)), 3));
        assert_eq!(next.len(), 1);
        assert_ne!(next[0].id, pending);
        previews.remove(&1);
        assert!(previews.retry(draft(&1, URL, Some(URL)), 4).is_empty());
        assert_eq!(previews.status(&1), Status::Removed);
    }

    #[test]
    fn cleared_drafts_do_not_retain_capacity_outside_the_text_budget() {
        let mut previews = enabled();
        let large = "x".repeat(MAX_DRAFT_BYTES);
        for conversation in 0..MAX_CONVERSATIONS as u32 {
            previews.update(draft(&conversation, &large, None), 0);
            previews.update(draft(&conversation, "", None), 1);
        }
        assert!(
            previews
                .drafts
                .iter()
                .map(|(_, state)| state.text.capacity())
                .sum::<usize>()
                <= MAX_KEPT_TEXT_BYTES
        );
    }

    fn fetched(effects: &[Effect<u32>]) -> Vec<Ticket<u32>> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Fetch(ticket) => Some(ticket.clone()),
                Effect::Cancel(_) => None,
            })
            .collect()
    }

    fn cancelled(effects: &[Effect<u32>]) -> Vec<RequestId> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Cancel(id) => Some(*id),
                Effect::Fetch(_) => None,
            })
            .collect()
    }

    /// Start a preview for `text` and deliver its card.
    fn ready(previews: &mut Previews, conversation: &u32, text: &str, url: &str) -> Card {
        let effects = previews.update(draft(conversation, text, Some(url)), 0);
        let ticket = fetched(&effects).pop().expect("a fetch");
        let card = card(url, "Title", "Description");
        assert_eq!(
            previews.deliver(ticket.id, Ok(card.clone()), 0),
            Delivery::Accepted
        );
        card
    }

    #[test]
    fn off_or_incomplete_proxy_settings_start_nothing() {
        let mut previews = Previews::new();
        let text = format!("see {URL} ");
        assert!(previews.update(draft(&1, &text, Some(URL)), 0).is_empty());
        assert_eq!(previews.status(&1), Status::Disabled);
        for settings in [
            Settings {
                mode: Mode::Off,
                ..Settings::default()
            },
            Settings {
                mode: Mode::ProxyRequired,
                ..Settings::default()
            },
            Settings {
                mode: Mode::ProxyRequired,
                proxy: Some("socks5://127.0.0.1:9050".into()),
                dns: None,
            },
            Settings {
                mode: Mode::ProxyRequired,
                proxy: Some("socks5h://127.0.0.1:9050".into()),
                dns: Some(DnsSettings {
                    url: "https://dns.example/dns-query".into(),
                    bootstrap: vec!["192.0.2.53".parse().unwrap()],
                }),
            },
        ] {
            previews.set_policy(&settings);
            assert!(previews.update(draft(&1, &text, Some(URL)), 0).is_empty());
            assert!(
                previews
                    .update(draft(&1, &format!("{text}x"), Some(URL)), 0)
                    .is_empty()
            );
            assert_eq!(previews.status(&1), Status::Disabled);
            assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), None);
        }
        // A complete proxy configuration enables previews.
        previews.set_policy(&Settings {
            mode: Mode::ProxyRequired,
            proxy: Some("socks5://127.0.0.1:9050".into()),
            dns: Some(DnsSettings {
                url: "https://dns.example/dns-query".into(),
                bootstrap: vec!["192.0.2.53".parse().unwrap()],
            }),
        });
        assert_eq!(
            fetched(&previews.update(draft(&1, &text, Some(URL)), 0)).len(),
            1
        );
    }

    #[test]
    fn a_card_goes_out_only_with_the_exact_text_it_is_bound_to() {
        let mut previews = enabled();
        let text = format!("see {URL} ");
        let effects = previews.update(draft(&1, &text, Some(URL)), 0);
        let [Effect::Fetch(ticket)] = &effects[..] else {
            panic!("{effects:?}");
        };
        assert_eq!(ticket.conversation, 1);
        assert_eq!(ticket.connection, 1);
        assert_eq!(ticket.policy, previews.policy_generation());
        assert_eq!(ticket.target.as_str(), URL);
        assert_eq!(previews.status(&1), Status::Fetching);
        // Send before the card is ready: text only.
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), None);
        let card = card(URL, "Title", "");
        assert_eq!(
            previews.deliver(ticket.id, Ok(card.clone()), 0),
            Delivery::Accepted
        );
        assert_eq!(previews.status(&1), Status::Ready(&card));
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), Some(&card));
        // Any other text, conversation, connection or carry decision: none.
        assert_eq!(
            previews.for_send(draft(&1, &format!("{text}!"), Some(URL))),
            None
        );
        assert_eq!(previews.for_send(draft(&2, &text, Some(URL))), None);
        assert_eq!(
            previews.for_send(Draft {
                connection: 2,
                ..draft(&1, &text, Some(URL))
            }),
            None
        );
        assert_eq!(
            previews.for_send(Draft {
                carry: Err(Ineligible::Attachments),
                ..draft(&1, &text, Some(URL))
            }),
            None
        );
        assert_eq!(previews.for_send(draft(&1, &text, None)), None);
        // A second delivery for the same request is refused.
        assert_eq!(previews.deliver(ticket.id, Ok(card), 0), Delivery::Stale);
    }

    #[test]
    fn edits_that_keep_the_url_keep_the_card_or_the_fetch() {
        let mut previews = enabled();
        let kept = ready(&mut previews, &1, &format!("{URL} a"), URL);
        // Ready: rebinding to the edited text needs no fetch.
        assert!(
            previews
                .update(draft(&1, &format!("{URL} ab"), Some(URL)), 1)
                .is_empty()
        );
        assert_eq!(
            previews.for_send(draft(&1, &format!("{URL} ab"), Some(URL))),
            Some(&kept)
        );
        assert_eq!(
            previews.for_send(draft(&1, &format!("{URL} a"), Some(URL))),
            None
        );

        // Fetching: the request in flight serves the edited text.
        let mut previews = enabled();
        let ticket = fetched(&previews.update(draft(&1, &format!("{URL} a"), Some(URL)), 0))
            .pop()
            .unwrap();
        assert!(
            previews
                .update(draft(&1, &format!("{URL} ab"), Some(URL)), 0)
                .is_empty()
        );
        let card = card(URL, "T", "");
        assert_eq!(
            previews.deliver(ticket.id, Ok(card.clone()), 0),
            Delivery::Accepted
        );
        assert_eq!(
            previews.for_send(draft(&1, &format!("{URL} a"), Some(URL))),
            None
        );
        assert_eq!(
            previews.for_send(draft(&1, &format!("{URL} ab"), Some(URL))),
            Some(&card)
        );
    }

    #[test]
    fn a_changed_url_cancels_and_late_results_never_attach() {
        let mut previews = enabled();
        let first = fetched(&previews.update(draft(&1, &format!("{URL} "), Some(URL)), 0))
            .pop()
            .unwrap();
        let effects = previews.update(draft(&1, &format!("{OTHER} "), Some(OTHER)), 0);
        assert_eq!(cancelled(&effects), vec![first.id]);
        let second = fetched(&effects).pop().unwrap();
        assert!(second.id > first.id);
        assert_eq!(
            previews.deliver(first.id, Ok(card(URL, "Old", "")), 0),
            Delivery::Stale
        );
        assert_eq!(previews.status(&1), Status::Fetching);
        // The worker cannot answer with another page's card.
        assert_eq!(
            previews.deliver(second.id, Ok(card(URL, "Old", "")), 0),
            Delivery::Accepted
        );
        assert_eq!(
            previews.status(&1),
            Status::Unavailable(Unavailable::Malformed)
        );
        assert_eq!(
            previews.for_send(draft(&1, &format!("{OTHER} "), Some(OTHER))),
            None
        );
        // Removing the link drops everything.
        assert!(previews.update(draft(&1, "no link", None), 0).is_empty());
        assert_eq!(previews.status(&1), Status::None);
    }

    #[test]
    fn removal_survives_late_replies_and_edits_until_the_url_changes_or_send() {
        let mut previews = enabled();
        let text = format!("{URL} ");
        let ticket = fetched(&previews.update(draft(&1, &text, Some(URL)), 0))
            .pop()
            .unwrap();
        assert_eq!(cancelled(&previews.remove(&1)), vec![ticket.id]);
        assert_eq!(previews.status(&1), Status::Removed);
        assert_eq!(
            previews.deliver(ticket.id, Ok(card(URL, "T", "")), 0),
            Delivery::Stale
        );
        assert_eq!(previews.status(&1), Status::Removed);
        // Edits keeping the URL stay removed and fetch nothing.
        assert!(
            previews
                .update(draft(&1, &format!("{text}more"), Some(URL)), 0)
                .is_empty()
        );
        assert_eq!(previews.status(&1), Status::Removed);
        assert_eq!(
            previews.for_send(draft(&1, &format!("{text}more"), Some(URL))),
            None
        );
        // A different URL may preview; going back is a change too.
        assert_eq!(
            fetched(&previews.update(draft(&1, OTHER, Some(OTHER)), 0)).len(),
            1
        );
        assert_eq!(
            fetched(&previews.update(draft(&1, &text, Some(URL)), 0)).len(),
            1
        );

        // Removing a ready card, then sending: the next draft may preview
        // the same URL again, from the remembered result.
        let mut previews = enabled();
        let card = ready(&mut previews, &1, &text, URL);
        assert!(previews.remove(&1).is_empty());
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), None);
        let token = previews.submission_token(&1, 1, &text).unwrap();
        assert!(previews.sent_if_current(&1, token).is_empty());
        assert!(previews.update(draft(&1, &text, Some(URL)), 1).is_empty());
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), Some(&card));

        // Asking again restores it without a removal in the way.
        assert!(previews.remove(&1).is_empty());
        assert!(previews.restore(draft(&1, &text, Some(URL)), 1).is_empty());
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), Some(&card));
    }

    #[test]
    fn reconnects_conversation_switches_and_policy_changes_invalidate_results() {
        // Reconnect mid-fetch.
        let mut previews = enabled();
        let text = format!("{URL} ");
        let ticket = fetched(&previews.update(draft(&1, &text, Some(URL)), 0))
            .pop()
            .unwrap();
        let reconnected = Draft {
            connection: 2,
            ..draft(&1, &text, Some(URL))
        };
        let effects = previews.update(reconnected, 0);
        assert_eq!(cancelled(&effects), vec![ticket.id]);
        let again = fetched(&effects).pop().unwrap();
        assert_eq!(again.connection, 2);
        assert_eq!(
            previews.deliver(ticket.id, Ok(card(URL, "T", "")), 0),
            Delivery::Stale
        );

        // Another conversation takes the single pipeline.
        let mut previews = enabled();
        let first = fetched(&previews.update(draft(&1, &text, Some(URL)), 0))
            .pop()
            .unwrap();
        let effects = previews.update(draft(&2, &text, Some(URL)), 0);
        assert_eq!(cancelled(&effects), vec![first.id]);
        let second = fetched(&effects).pop().unwrap();
        assert_eq!(second.conversation, 2);
        assert_eq!(
            previews.deliver(first.id, Ok(card(URL, "T", "")), 0),
            Delivery::Stale
        );
        assert_eq!(previews.status(&1), Status::None);
        let card = card(URL, "T", "");
        assert_eq!(
            previews.deliver(second.id, Ok(card.clone()), 0),
            Delivery::Accepted
        );
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), None);
        assert_eq!(previews.for_send(draft(&2, &text, Some(URL))), Some(&card));
        // The first conversation's next update is served from the result.
        assert!(previews.update(draft(&1, &text, Some(URL)), 0).is_empty());
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), Some(&card));

        // A policy change cancels, forgets results and cards, keeps removals.
        let pending = fetched(&previews.update(draft(&3, &format!("{OTHER} "), Some(OTHER)), 0))
            .pop()
            .unwrap();
        assert!(previews.remove(&2).is_empty());
        assert_eq!(cancelled(&previews.set_policy(&direct())), vec![pending.id]);
        assert_eq!(
            previews.deliver(pending.id, Ok(card.clone()), 0),
            Delivery::Stale
        );
        // Nothing from the earlier policy is remembered.
        assert!(previews.cache.is_empty());
        assert_eq!(previews.for_send(draft(&1, &text, Some(URL))), None);
        assert_eq!(
            fetched(&previews.update(draft(&1, &text, Some(URL)), 0)).len(),
            1
        );
        assert!(previews.update(draft(&2, &text, Some(URL)), 0).is_empty());
        assert_eq!(previews.status(&2), Status::Removed);
        // Off: nothing pending, nothing sent.
        let effects = previews.set_policy(&Settings {
            mode: Mode::Off,
            ..Settings::default()
        });
        assert_eq!(cancelled(&effects).len(), 1);
        assert!(previews.update(draft(&1, &text, Some(URL)), 0).is_empty());
        assert_eq!(previews.status(&1), Status::Disabled);
    }

    #[test]
    fn targets_that_cannot_carry_a_card_fetch_nothing() {
        let mut previews = enabled();
        let text = format!("{URL} ");
        for reason in [
            Ineligible::HtmlDisabled,
            Ineligible::EndToEnd,
            Ineligible::ExtensionConversation,
            Ineligible::Attachments,
        ] {
            let blocked = Draft {
                carry: Err(reason),
                ..draft(&1, &text, Some(URL))
            };
            assert!(previews.update(blocked, 0).is_empty());
            assert_eq!(previews.status(&1), Status::TextOnly(reason));
        }
        // Adding an attachment while fetching cancels the fetch.
        let ticket = fetched(&previews.update(draft(&1, &text, Some(URL)), 0))
            .pop()
            .unwrap();
        let effects = previews.update(
            Draft {
                carry: Err(Ineligible::Attachments),
                ..draft(&1, &text, Some(URL))
            },
            0,
        );
        assert_eq!(cancelled(&effects), vec![ticket.id]);
        assert_eq!(
            previews.deliver(ticket.id, Ok(card(URL, "T", "")), 0),
            Delivery::Stale
        );
    }

    #[test]
    fn candidates_must_be_in_the_draft_and_pass_the_target_policy() {
        let mut previews = enabled();
        for (text, candidate) in [
            ("no link here", URL),
            ("http://127.0.0.1/ x", "http://127.0.0.1/"),
            ("https://localhost/ x", "https://localhost/"),
            ("https://example.org:8443/ x", "https://example.org:8443/"),
            ("mumble://example.org/ x", "mumble://example.org/"),
        ] {
            assert!(
                previews
                    .update(draft(&1, text, Some(candidate)), 0)
                    .is_empty()
            );
            assert_eq!(previews.status(&1), Status::None, "{text}");
        }
        let long = format!("{URL} {}", "x".repeat(MAX_DRAFT_BYTES));
        assert!(previews.update(draft(&1, &long, Some(URL)), 0).is_empty());
        assert_eq!(previews.for_send(draft(&1, &long, Some(URL))), None);
    }

    #[test]
    fn results_are_remembered_within_bounds_and_lifetimes() {
        let mut previews = enabled();
        let card = ready(&mut previews, &1, URL, URL);
        // Back to a remembered URL within its lifetime: no fetch.
        assert_eq!(
            fetched(&previews.update(draft(&1, OTHER, Some(OTHER)), 1)).len(),
            1
        );
        assert!(fetched(&previews.update(draft(&1, URL, Some(URL)), CARD_TTL_MS - 1)).is_empty());
        assert_eq!(previews.for_send(draft(&1, URL, Some(URL))), Some(&card));
        // After it: fetched again.
        assert_eq!(
            fetched(&previews.update(draft(&1, OTHER, Some(OTHER)), CARD_TTL_MS)).len(),
            1
        );
        assert_eq!(
            fetched(&previews.update(draft(&1, URL, Some(URL)), CARD_TTL_MS)).len(),
            1
        );

        // Failures are remembered for a shorter time.
        let mut previews = enabled();
        let ticket = fetched(&previews.update(draft(&1, URL, Some(URL)), 0))
            .pop()
            .unwrap();
        previews.deliver(ticket.id, Err(Unavailable::NoMetadata), 0);
        assert_eq!(
            fetched(&previews.update(draft(&1, OTHER, Some(OTHER)), 1)).len(),
            1
        );
        assert!(
            fetched(&previews.update(draft(&1, URL, Some(URL)), FAILURE_TTL_MS - 1)).is_empty()
        );
        assert_eq!(
            previews.status(&1),
            Status::Unavailable(Unavailable::NoMetadata)
        );
        assert_eq!(
            fetched(&previews.update(draft(&1, OTHER, Some(OTHER)), FAILURE_TTL_MS)).len(),
            1
        );
        assert_eq!(
            fetched(&previews.update(draft(&1, URL, Some(URL)), FAILURE_TTL_MS)).len(),
            1
        );

        // At most MAX_CACHE_ENTRIES results.
        let mut previews = enabled();
        let urls: Vec<String> = (0..=MAX_CACHE_ENTRIES)
            .map(|n| format!("https://site{n}.example/"))
            .collect();
        for url in &urls {
            ready(&mut previews, &1, url, url);
        }
        assert_eq!(previews.cache.len(), MAX_CACHE_ENTRIES);
        assert_eq!(
            fetched(&previews.update(draft(&1, &urls[0], Some(&urls[0])), 0)).len(),
            1
        );
        assert!(previews.cache.iter().map(Cached::weight).sum::<usize>() <= MAX_CACHE_BYTES);
    }

    #[test]
    fn drafts_are_bounded_in_count_and_kept_text() {
        let mut previews = enabled();
        let ticket = fetched(&previews.update(draft(&0, URL, Some(URL)), 0))
            .pop()
            .unwrap();
        for conversation in 1..MAX_CONVERSATIONS as u32 {
            previews.update(draft(&conversation, "text", None), 0);
        }
        assert_eq!(previews.drafts.len(), MAX_CONVERSATIONS);
        // One more forgets the least recently used, cancelling its fetch.
        let effects = previews.update(draft(&999, "text", None), 0);
        assert_eq!(cancelled(&effects), vec![ticket.id]);
        assert_eq!(previews.drafts.len(), MAX_CONVERSATIONS);
        assert_eq!(
            previews.deliver(ticket.id, Ok(card(URL, "T", "")), 0),
            Delivery::Stale
        );

        let big = "x".repeat(MAX_DRAFT_BYTES);
        for conversation in 0..MAX_CONVERSATIONS as u32 {
            previews.update(draft(&conversation, &big, None), 0);
        }
        let kept: usize = previews
            .drafts
            .iter()
            .map(|(_, state)| state.text.len())
            .sum();
        assert!(kept <= MAX_KEPT_TEXT_BYTES);
        assert!(
            previews
                .drafts
                .iter()
                .any(|(key, _)| *key == MAX_CONVERSATIONS as u32 - 1)
        );
    }
}
