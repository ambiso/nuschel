// SPDX-License-Identifier: EUPL-1.2
//! One conversation's timeline: the latest page of history, older pages
//! prepended as the reader scrolls up, and live messages appended. Pages come
//! from the local cache (ids are row ids, ordered by observation time) or,
//! on a Nuschl server, from the server's history (ids are seqs); our own
//! messages on a Nuschl server stay pending until the server acknowledges
//! their idempotency key. Edits and deletions apply to the message they name
//! and never overtake it: one that arrives first waits for its message.
use nuschl_core::{ChatMessage, MessageDocument, transfer::Offer};
use std::collections::VecDeque;

/// Messages a timeline keeps while its reader is at the newest message;
/// older ones are dropped and load again from history on scrolling up.
pub const MAX_TIMELINE: usize = 1000;

/// A message as the content worker rendered it, live or from a page.
#[derive(Clone, Debug, PartialEq)]
pub struct IncomingMessage {
    /// The local row id, or on a Nuschl server's history timeline the seq.
    pub id: Option<i64>,
    pub message: ChatMessage,
    pub document: MessageDocument,
    pub outgoing: bool,
    /// Our message on a Nuschl server until it is acknowledged: its key.
    pub pending: Option<[u8; 16]>,
    /// Nuschl servers: the message's seq in its conversation (on a server
    /// history timeline also its id; never a local row id).
    pub seq: Option<u64>,
    /// Nuschl servers: the author's registration number (who may edit it).
    pub author_id: Option<u32>,
    /// When it was last edited (Nuschl servers).
    pub edited_ms: Option<i64>,
    /// A tombstone: the message was deleted (Nuschl servers).
    pub deleted: Option<Deletion>,
}

impl IncomingMessage {
    /// A message without Nuschl server details.
    pub fn plain(
        id: Option<i64>,
        message: ChatMessage,
        document: MessageDocument,
        outgoing: bool,
    ) -> Self {
        Self {
            id,
            message,
            document,
            outgoing,
            pending: None,
            seq: None,
            author_id: None,
            edited_ms: None,
            deleted: None,
        }
    }
}

/// A deleted message's tombstone: when, and whether a moderator deleted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deletion {
    pub at_ms: i64,
    pub moderator: bool,
}

/// Where a message stands, as its timeline shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageStatus {
    /// Someone else's message, a tombstone, or one without anything to say.
    Plain,
    /// Ours, waiting for the server's acknowledgement.
    Sending,
    /// Ours, acknowledged (or kept in local history).
    Sent,
    /// The server refused ours: it was not stored.
    NotSent,
    /// Could not be kept in local history.
    NotSaved,
}

impl MessageStatus {
    /// The words a UI shows beside the time, if any.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Sending => Some("sending…"),
            Self::Sent => Some("sent"),
            Self::NotSent => Some("not sent"),
            Self::NotSaved => Some("not saved"),
        }
    }
}

/// A message in a timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub id: Option<i64>,
    /// Identifies the message to a UI (its selection, its laid-out
    /// document) while it lives; never reused.
    pub serial: u64,
    /// Changes whenever the message's content does (an edit, a deletion), so
    /// a UI rebuilds what it derived from it.
    pub revision: u32,
    pub author: String,
    pub observed_at_ms: i64,
    pub status: MessageStatus,
    pub document: MessageDocument,
    /// A file offered with this message (a magic-wormhole code).
    pub offer: Option<Offer>,
    /// Our message on a Nuschl server until it is acknowledged: its key.
    pub pending: Option<[u8; 16]>,
    /// Sent from this device (or our account's other one).
    pub outgoing: bool,
    /// Nuschl servers: its seq in the conversation (edits, deletions and
    /// read markers name it), and the author's registration number.
    pub seq: Option<u64>,
    pub author_id: Option<u32>,
    /// What was typed (a composer edits it).
    pub source: String,
    /// Nuschl servers: when it was last edited, or that it was deleted.
    pub edited_ms: Option<i64>,
    pub deleted: Option<Deletion>,
}

impl Message {
    /// Ours, on a Nuschl server: sent from here, or by our registration.
    pub fn ours(&self, own_user_id: Option<u32>) -> bool {
        self.outgoing || (self.author_id.is_some() && self.author_id == own_user_id)
    }
}

impl From<IncomingMessage> for Message {
    fn from(message: IncomingMessage) -> Self {
        static SERIALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let offer = nuschl_core::transfer::parse_offer(&message.message.body)
            .or_else(|| nuschl_core::transfer::offer_in(&message.document));
        let status = if message.deleted.is_some() {
            MessageStatus::Plain
        } else if message.pending.is_some() {
            MessageStatus::Sending
        } else if message.id.is_none() {
            MessageStatus::NotSaved
        } else if message.outgoing {
            MessageStatus::Sent
        } else {
            MessageStatus::Plain
        };
        Self {
            id: message.id,
            serial: SERIALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            revision: 0,
            author: message.message.author,
            observed_at_ms: message.message.observed_at_ms,
            status,
            document: message.document,
            offer,
            pending: message.pending,
            outgoing: message.outgoing,
            seq: message.seq,
            author_id: message.author_id,
            source: message.message.body,
            edited_ms: message.edited_ms,
            deleted: message.deleted,
        }
    }
}

/// Which page of a conversation's history to load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Older than this message; None is the latest page.
    Before(Option<i64>),
    /// Newer than this message.
    After(i64),
    /// Around this message, which a search result jumps to.
    Around(i64),
}

/// What changed about a message on a Nuschl server.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Edited {
        source: String,
        document: MessageDocument,
        edited_ms: i64,
    },
    Deleted(Deletion),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    /// In timeline order: by observation time, then by id.
    pub messages: Vec<Message>,
    /// Cursor for the next older page, if history has more.
    pub next_before: Option<i64>,
    /// Set while the timeline shows older history apart from the newest
    /// (after a search result opened a message): the next newer page.
    pub next_after: Option<i64>,
    /// A live message was left out of such a timeline since the newer page
    /// was asked for.
    pub missed_live: bool,
    /// The oldest stored message is loaded: there is nothing further up.
    pub beginning: bool,
    /// The latest page is in memory; live messages keep it current, so
    /// opening the conversation again needs no reload.
    pub loaded: bool,
    /// A page was asked for and has not arrived.
    pub loading: bool,
}

impl Timeline {
    /// Add a message unless the timeline has it (by id). Observation time
    /// orders it: an unsaved message still appears where it arrived.
    /// Returns whether it is new here.
    pub fn merge(&mut self, message: IncomingMessage) -> bool {
        if message
            .id
            .is_some_and(|id| self.messages.iter().any(|entry| entry.id == Some(id)))
        {
            return false;
        }
        self.messages.push(message.into());
        self.messages
            .sort_by_key(|message| (message.observed_at_ms, message.id.unwrap_or(i64::MAX)));
        true
    }

    /// A live message. While older history is shown apart from the newest
    /// (`next_after`), a stored one arrives with the newer pages instead.
    /// Returns whether it counts as new.
    pub fn receive(&mut self, message: IncomingMessage) -> bool {
        if self.next_after.is_some() && message.id.is_some() {
            self.missed_live = true;
            true
        } else {
            self.merge(message)
        }
    }

    /// Fold a page read backwards into the timeline. The latest page
    /// (`before` is None) starts it over, keeping only what arrived since;
    /// an older page is prepended if it continues from the oldest loaded
    /// message. Returns whether the page was used.
    pub fn take_page(
        &mut self,
        before: Option<i64>,
        messages: Vec<IncomingMessage>,
        next_before: Option<i64>,
    ) -> bool {
        match before {
            None => {
                self.loaded = true;
                self.next_after = None;
                self.missed_live = false;
                // What arrived since the page was asked for stays: newer by
                // time (local pages run by time, and rows stored later may
                // hold older messages) or by id (a server's seqs; our own
                // message keeps this device's clock).
                let newest = messages
                    .iter()
                    .filter_map(|message| Some((message.message.observed_at_ms, message.id?)))
                    .max();
                let newest_id = messages.iter().filter_map(|message| message.id).max();
                self.messages.retain(|message| {
                    message.id.is_none_or(|id| {
                        newest.is_none_or(|newest| (message.observed_at_ms, id) > newest)
                            || newest_id.is_none_or(|newest| id > newest)
                    })
                });
            }
            // Trimmed while the page was on its way: the next request
            // starts from the new oldest message.
            Some(before) if self.next_before != Some(before) => return false,
            Some(_) => {}
        }
        for message in messages {
            self.merge(message);
        }
        self.next_before = next_before;
        self.beginning = next_before.is_none();
        true
    }

    /// Fold a page read around a message (a search result) or after one.
    /// Around replaces the timeline (keeping what arrived since, if it
    /// reaches the newest message); after continues it if it was asked for
    /// from the newest loaded message. Returns whether the page was used.
    pub fn take_window(
        &mut self,
        page: Page,
        messages: Vec<IncomingMessage>,
        (next_before, next_after): (Option<i64>, Option<i64>),
    ) -> bool {
        match page {
            Page::Around(_) => {
                let newest = messages.iter().filter_map(|message| message.id).max();
                self.messages.retain(|message| {
                    next_after.is_none() && message.id.is_none_or(|id| Some(id) > newest)
                });
                for message in messages {
                    self.merge(message);
                }
                self.next_before = next_before;
                self.beginning = next_before.is_none();
                self.next_after = next_after;
                self.missed_live = false;
                self.loaded = next_after.is_none();
                true
            }
            Page::After(after) if self.next_after == Some(after) => {
                for message in messages {
                    self.merge(message);
                }
                // A live message left out meanwhile is newer than this page.
                self.next_after = next_after.or_else(|| {
                    self.missed_live
                        .then(|| self.messages.iter().filter_map(|message| message.id).max())
                        .flatten()
                });
                self.missed_live = false;
                self.loaded = self.next_after.is_none();
                true
            }
            Page::After(_) | Page::Before(_) => false,
        }
    }

    /// Drop the oldest messages beyond `cap`; they load again from history.
    pub fn trim_oldest(&mut self, cap: usize) {
        if self.messages.len() <= cap {
            return;
        }
        self.messages.drain(..self.messages.len() - cap);
        // The oldest in timeline order, which need not have the smallest id.
        if let Some(oldest) = self.messages.iter().find_map(|message| message.id) {
            self.next_before = Some(oldest);
            self.beginning = false;
        }
    }

    /// Forget the messages: the next opening loads the latest page again
    /// (another history source, or another server identity).
    pub fn reset(&mut self) {
        self.loaded = false;
        self.messages.clear();
        self.next_before = None;
    }

    /// Our message with idempotency key `key` was acknowledged as `seq`, or
    /// refused (`None`). Returns whether the timeline shows it.
    pub fn settle(&mut self, key: [u8; 16], seq: Option<u64>) -> bool {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.pending == Some(key))
        else {
            return false;
        };
        message.pending = None;
        match seq {
            Some(seq) => {
                message.id = Some(seq as i64);
                message.seq = Some(seq);
                message.status = MessageStatus::Sent;
            }
            None => message.status = MessageStatus::NotSent,
        }
        true
    }

    /// Apply `change` to message `seq`, or hand it back if the timeline does
    /// not show that message. Returns whether the message was deleted.
    pub fn apply_change(&mut self, seq: u64, change: Change) -> Result<bool, Change> {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.seq == Some(seq))
        else {
            return Err(change);
        };
        match change {
            Change::Edited {
                source,
                document,
                edited_ms,
            } => {
                // A late edit never undoes a deletion, nor a newer edit.
                if message.deleted.is_some()
                    || message.edited_ms.is_some_and(|shown| shown > edited_ms)
                {
                    return Ok(false);
                }
                message.offer = nuschl_core::transfer::parse_offer(&source)
                    .or_else(|| nuschl_core::transfer::offer_in(&document));
                message.source = source;
                message.document = document;
                message.edited_ms = Some(edited_ms);
                message.revision = message.revision.wrapping_add(1);
                Ok(false)
            }
            Change::Deleted(deletion) => {
                message.deleted = Some(deletion);
                message.document = MessageDocument { blocks: Vec::new() };
                message.source.clear();
                message.offer = None;
                message.status = MessageStatus::Plain;
                message.revision = message.revision.wrapping_add(1);
                Ok(true)
            }
        }
    }

    /// The newest seq shown (Nuschl servers), for the read marker.
    pub fn newest_seq(&self) -> Option<u64> {
        self.messages.iter().filter_map(|message| message.seq).max()
    }
}

/// Edits and deletions of messages that are not in a timeline yet, by
/// conversation: a page on its way takes seconds; older ones were for
/// messages that are not loaded, and go.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WaitingChanges {
    changes: VecDeque<WaitingChange>,
}

#[derive(Clone, Debug, PartialEq)]
struct WaitingChange {
    conversation: [u8; 16],
    seq: u64,
    change: Change,
    /// On the platform's monotonic clock, in milliseconds.
    since_ms: u64,
}

impl WaitingChanges {
    /// Changes kept, and for how long.
    pub const MAX: usize = 256;
    pub const KEEP_MS: u64 = 60_000;

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Keep `change` until its message shows; the oldest go beyond [`Self::MAX`].
    pub fn push(&mut self, conversation: [u8; 16], seq: u64, change: Change, now_ms: u64) {
        while self.changes.len() >= Self::MAX {
            self.changes.pop_front();
        }
        self.changes.push_back(WaitingChange {
            conversation,
            seq,
            change,
            since_ms: now_ms,
        });
    }

    /// Offer each change that is not too old to `apply`, which hands back
    /// those whose message still does not show.
    pub fn retry(
        &mut self,
        now_ms: u64,
        mut apply: impl FnMut([u8; 16], u64, Change) -> Result<(), Change>,
    ) {
        for waiting in std::mem::take(&mut self.changes) {
            if now_ms.saturating_sub(waiting.since_ms) > Self::KEEP_MS {
                continue;
            }
            let WaitingChange {
                conversation,
                seq,
                since_ms,
                ..
            } = waiting;
            if let Err(change) = apply(conversation, seq, waiting.change) {
                self.changes.push_back(WaitingChange {
                    conversation,
                    seq,
                    change,
                    since_ms,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: Option<i64>, observed_at_ms: i64) -> IncomingMessage {
        IncomingMessage::plain(
            id,
            ChatMessage {
                server: "server".into(),
                channel: "2".into(),
                author: "Ada".into(),
                observed_at_ms,
                body: format!("message {id:?}"),
            },
            MessageDocument { blocks: Vec::new() },
            false,
        )
    }

    fn page(ids: std::ops::RangeInclusive<i64>) -> Vec<IncomingMessage> {
        ids.map(|id| message(Some(id), id * 1000)).collect()
    }

    fn ids(timeline: &Timeline) -> Vec<Option<i64>> {
        timeline.messages.iter().map(|message| message.id).collect()
    }

    #[test]
    fn local_pages_follow_time_not_row_ids() {
        // A server page kept after live messages: newer rows, older times.
        let mut timeline = Timeline::default();
        let mut live = message(Some(2), 9500);
        live.outgoing = true;
        timeline.merge(live);
        timeline.merge(message(Some(1), 500));
        timeline.take_page(
            None,
            vec![message(Some(10), 5000), message(Some(3), 9000)],
            Some(10),
        );
        // What arrived after the page's newest stays; what is older goes.
        assert_eq!(ids(&timeline), [Some(10), Some(3), Some(2)]);
        timeline.trim_oldest(2);
        assert_eq!(
            timeline.next_before,
            Some(3),
            "the oldest shown, not the smallest id"
        );
    }

    #[test]
    fn our_messages_are_pending_until_the_server_acknowledges_them() {
        let mut timeline = Timeline::default();
        let mut pending = message(None, 1000);
        pending.outgoing = true;
        pending.pending = Some([7; 16]);
        timeline.merge(pending);
        assert_eq!(timeline.messages[0].status, MessageStatus::Sending);
        assert!(!timeline.settle([8; 16], Some(3)));
        assert!(timeline.settle([7; 16], Some(42)));
        let shown = &timeline.messages[0];
        assert_eq!(
            (shown.id, shown.status, shown.pending),
            (Some(42), MessageStatus::Sent, None)
        );
        // A latest page keeps it (by seq) and does not show it twice.
        assert!(!timeline.merge(message(Some(42), 1000)));
        // A refusal says so.
        let mut refused = message(None, 2000);
        refused.pending = Some([9; 16]);
        timeline.merge(refused);
        assert!(timeline.settle([9; 16], None));
        assert_eq!(timeline.messages[1].status, MessageStatus::NotSent);
    }

    #[test]
    fn older_pages_prepend_to_one_timeline_until_the_beginning() {
        let mut timeline = Timeline::default();
        assert!(timeline.take_page(None, page(81..=120), Some(81)));
        assert!(!timeline.beginning);
        // A live message joins the same timeline; its echo from history does not repeat it.
        assert!(timeline.merge(message(Some(121), 121_000)));
        assert!(!timeline.merge(message(Some(121), 121_000)));
        assert!(timeline.take_page(Some(81), page(41..=80), Some(41)));
        assert!(timeline.take_page(Some(41), page(1..=40), None));
        assert!(timeline.beginning);
        assert_eq!(timeline.next_before, None);
        assert_eq!(ids(&timeline), (1..=121).map(Some).collect::<Vec<_>>());
    }

    #[test]
    fn a_page_for_a_stale_cursor_is_dropped() {
        let mut timeline = Timeline::default();
        timeline.take_page(None, page(81..=120), Some(81));
        timeline.trim_oldest(20);
        assert_eq!(timeline.next_before, Some(101));
        // Asked for before the trim: it no longer continues the timeline.
        assert!(!timeline.take_page(Some(81), page(41..=80), Some(41)));
        assert_eq!(ids(&timeline), (101..=120).map(Some).collect::<Vec<_>>());
        assert!(timeline.take_page(Some(101), page(61..=100), Some(61)));
        assert_eq!(timeline.messages.len(), 60);
    }

    #[test]
    fn the_latest_page_starts_over_but_keeps_what_arrived_since() {
        let mut timeline = Timeline::default();
        timeline.take_page(None, page(81..=120), Some(81));
        timeline.take_page(Some(81), page(41..=80), Some(41));
        timeline.merge(message(Some(121), 121_000));
        timeline.merge(message(None, 122_000));
        // Reopened: the latest page again, with the newer and unsaved messages.
        timeline.take_page(None, page(82..=120), Some(82));
        assert_eq!(
            ids(&timeline),
            (82..=121)
                .map(Some)
                .chain(std::iter::once(None))
                .collect::<Vec<_>>()
        );
        assert_eq!(timeline.next_before, Some(82));
    }

    #[test]
    fn trimming_drops_only_the_oldest_and_rewinds_the_cursor() {
        let mut timeline = Timeline::default();
        timeline.take_page(None, page(1..=40), None);
        assert!(timeline.beginning);
        timeline.trim_oldest(40);
        assert!(
            timeline.beginning,
            "nothing beyond the cap, nothing dropped"
        );
        timeline.trim_oldest(30);
        assert_eq!(ids(&timeline).first(), Some(&Some(11)));
        assert_eq!(timeline.messages.len(), 30);
        assert_eq!(timeline.next_before, Some(11));
        assert!(!timeline.beginning);
    }

    #[test]
    fn a_search_window_stands_apart_until_newer_pages_reach_the_newest() {
        let mut timeline = Timeline::default();
        timeline.take_page(None, page(181..=200), Some(181));
        // Around 50: the timeline is that window, apart from the newest.
        assert!(timeline.take_window(Page::Around(50), page(40..=60), (Some(40), Some(60))));
        assert_eq!(ids(&timeline), (40..=60).map(Some).collect::<Vec<_>>());
        assert!(!timeline.loaded);
        // A live message waits for the newer pages, but is noted.
        assert!(timeline.receive(message(Some(201), 201_000)));
        assert!(timeline.missed_live);
        assert_eq!(timeline.messages.len(), 21);
        // A page after anything but the newest loaded message is stale.
        assert!(!timeline.take_window(Page::After(50), page(51..=70), (None, Some(70))));
        assert!(timeline.take_window(Page::After(60), page(61..=80), (None, None)));
        // The page ended, but the live message is newer: one more page.
        assert_eq!(timeline.next_after, Some(80));
        assert!(!timeline.missed_live);
        assert!(timeline.take_window(Page::After(80), page(81..=201), (None, None)));
        assert_eq!(timeline.next_after, None);
        assert!(timeline.loaded);
        assert_eq!(timeline.messages.len(), 162);
        // Live messages join it directly again.
        assert!(timeline.receive(message(Some(202), 202_000)));
        assert_eq!(timeline.messages.last().and_then(|m| m.id), Some(202));
    }

    fn edit(text: &str, at: i64) -> Change {
        Change::Edited {
            source: text.into(),
            document: MessageDocument { blocks: Vec::new() },
            edited_ms: at,
        }
    }

    #[test]
    fn changes_apply_in_order_and_never_revive_a_tombstone() {
        let mut timeline = Timeline::default();
        let mut stored = message(Some(5), 5);
        stored.seq = Some(5);
        timeline.merge(stored);
        let shown = |timeline: &Timeline| {
            let message = &timeline.messages[0];
            (
                message.source.clone(),
                message.edited_ms,
                message.deleted.is_some(),
                message.revision,
            )
        };
        assert_eq!(timeline.apply_change(5, edit("newer", 20)), Ok(false));
        // An older edit that arrives late changes nothing.
        assert_eq!(timeline.apply_change(5, edit("older", 10)), Ok(false));
        assert_eq!(shown(&timeline), ("newer".into(), Some(20), false, 1));
        // One for a message not shown is handed back.
        assert_eq!(
            timeline.apply_change(6, edit("later", 30)),
            Err(edit("later", 30))
        );
        let deleted = Change::Deleted(Deletion {
            at_ms: 40,
            moderator: true,
        });
        assert_eq!(timeline.apply_change(5, deleted), Ok(true));
        // A tombstone stays one.
        assert_eq!(timeline.apply_change(5, edit("again", 50)), Ok(false));
        assert_eq!(shown(&timeline), (String::new(), Some(20), true, 2));
        assert_eq!(timeline.messages[0].status, MessageStatus::Plain);
    }

    #[test]
    fn waiting_changes_apply_once_their_message_shows_and_expire() {
        let mut waiting = WaitingChanges::default();
        waiting.push([1; 16], 5, edit("first", 1), 0);
        waiting.push([1; 16], 6, edit("second", 2), 1000);
        let mut applied = Vec::new();
        waiting.retry(2000, |conversation, seq, change| {
            if seq == 5 {
                applied.push((conversation, seq));
                Ok(())
            } else {
                Err(change)
            }
        });
        assert_eq!(applied, [([1; 16], 5)]);
        assert!(!waiting.is_empty());
        // Past the minute, the other one goes without being offered.
        waiting.retry(1000 + WaitingChanges::KEEP_MS + 1, |_, _, _| {
            panic!("expired")
        });
        assert!(waiting.is_empty());
        for seq in 0..(WaitingChanges::MAX as u64 + 10) {
            waiting.push([2; 16], seq, edit("many", 0), 0);
        }
        let mut offered = Vec::new();
        waiting.retry(0, |_, seq, _| {
            offered.push(seq);
            Ok(())
        });
        assert_eq!(offered.len(), WaitingChanges::MAX);
        assert_eq!(offered[0], 10, "the oldest went first");
    }
}
