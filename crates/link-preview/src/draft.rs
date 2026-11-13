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
/// One fetch pipeline per client: a newer request cancels the older.
pub const MAX_IN_FLIGHT: usize = 1;
/// Results remembered by URL (memory only, cleared on every policy change).
pub const MAX_CACHE_ENTRIES: usize = 16;
pub const MAX_CACHE_BYTES: usize = 1024 * 1024;
pub const CARD_TTL_MS: u64 = 10 * 60 * 1000;
pub const FAILURE_TTL_MS: u64 = 2 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

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
    connection: u64,
    text: String,
    target: Option<Target>,
    suppressed: Option<String>,
    phase: Phase,
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
    /// Starts disabled (the default policy is Off).
    pub fn new() -> Self {
        Self {
            enabled: false,
            policy: 0,
            next_request: 0,
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
        self.policy += 1;
        self.enabled = settings.route().is_ok();
        self.cache.clear();
        let effects = self.cancel_in_flight();
        for (_, state) in &mut self.drafts {
            state.phase = Phase::Idle;
        }
        effects
    }

    /// The composer changed (or any input of its decision did). Effects say
    /// which fetch to start or cancel.
    pub fn update(&mut self, draft: Draft<'_, K>, now: u64) -> Vec<Effect<K>> {
        let mut effects = Vec::new();
        let target = candidate(draft.text, draft.candidate);
        self.touch(draft.conversation, draft.connection, &mut effects);
        // Taken out while deciding; nothing below cancels it implicitly.
        let (key, mut state) = self.drafts.pop_back().expect("touched");
        if state.connection != draft.connection {
            effects.extend(self.cancel_phase(&state.phase));
            state.connection = draft.connection;
            state.phase = Phase::Idle;
        }
        if state.target.as_ref().map(Target::as_str) != target.as_ref().map(Target::as_str) {
            effects.extend(self.cancel_phase(&state.phase));
            state.suppressed = None;
            state.target = target;
            state.phase = Phase::Idle;
        }
        state.text.clear();
        if draft.text.len() <= MAX_DRAFT_BYTES {
            state.text.push_str(draft.text);
        }
        state.phase = match (state.phase, &state.target) {
            (phase, _) if draft.carry.is_err() || !self.enabled || state.target.is_none() => {
                effects.extend(self.cancel_phase(&phase));
                match draft.carry {
                    Err(reason) => Phase::TextOnly(reason),
                    Ok(()) if !self.enabled => Phase::Disabled,
                    Ok(()) => Phase::Idle,
                }
            }
            (phase, Some(target)) if state.suppressed.as_deref() == Some(target.as_str()) => {
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
            && state.suppressed.as_deref() != Some(target.as_str())
            && card.is_for(target.as_str());
        bound.then_some(card)
    }

    /// The message was sent: removal ends and the draft starts over.
    pub fn sent(&mut self, conversation: &K) -> Vec<Effect<K>> {
        self.forget(conversation)
    }

    /// The conversation's composer is gone.
    pub fn forget(&mut self, conversation: &K) -> Vec<Effect<K>> {
        let Some(index) = self.drafts.iter().position(|(key, _)| key == conversation) else {
            return Vec::new();
        };
        let (_, state) = self.drafts.remove(index).expect("found");
        self.cancel_phase(&state.phase)
    }

    /// The sender removed the card (×): no preview for this URL until it
    /// changes, the message is sent or [`Self::restore`] is called. A result
    /// still in flight is cancelled and refused if it arrives.
    pub fn remove(&mut self, conversation: &K) -> Vec<Effect<K>> {
        let Some(index) = self.drafts.iter().position(|(key, _)| key == conversation) else {
            return Vec::new();
        };
        let phase = std::mem::replace(&mut self.drafts[index].1.phase, Phase::Idle);
        let effects = self.cancel_phase(&phase);
        let state = &mut self.drafts[index].1;
        match &state.target {
            Some(target) => {
                state.suppressed = Some(target.as_str().to_owned());
                state.phase = Phase::Removed;
            }
            None => state.phase = phase,
        }
        effects
    }

    /// The sender explicitly asked for the removed preview again.
    pub fn restore(&mut self, draft: Draft<'_, K>, now: u64) -> Vec<Effect<K>> {
        if let Some((_, state)) = self
            .drafts
            .iter_mut()
            .find(|(key, _)| key == draft.conversation)
        {
            state.suppressed = None;
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
    fn touch(&mut self, conversation: &K, connection: u64, effects: &mut Vec<Effect<K>>) {
        if let Some(index) = self.drafts.iter().position(|(key, _)| key == conversation) {
            let entry = self.drafts.remove(index).expect("found");
            self.drafts.push_back(entry);
        } else {
            if self.drafts.len() == MAX_CONVERSATIONS
                && let Some((_, evicted)) = self.drafts.pop_front()
            {
                effects.extend(self.cancel_phase(&evicted.phase));
            }
            self.drafts.push_back((
                conversation.clone(),
                State {
                    connection,
                    text: String::new(),
                    target: None,
                    suppressed: None,
                    phase: Phase::Idle,
                },
            ));
        }
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
        self.next_request += 1;
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

    fn cancel_phase(&mut self, phase: &Phase) -> Vec<Effect<K>> {
        match phase {
            Phase::Fetching(id) if self.in_flight == Some(*id) => self.cancel_in_flight(),
            _ => Vec::new(),
        }
    }

    /// Cancel the request in flight; its draft returns to Idle, so its next
    /// update asks again.
    fn cancel_in_flight(&mut self) -> Vec<Effect<K>> {
        let Some(id) = self.in_flight.take() else {
            return Vec::new();
        };
        for (_, state) in &mut self.drafts {
            if matches!(state.phase, Phase::Fetching(pending) if pending == id) {
                state.phase = Phase::Idle;
            }
        }
        vec![Effect::Cancel(id)]
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
            Settings::default(),
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
        assert!(previews.sent(&1).is_empty());
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
        let effects = previews.set_policy(&Settings::default());
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
