// SPDX-License-Identifier: EUPL-1.2
//! A conversation: a channel's chat, or a direct conversation (stock Mumble's
//! text to a session, or a Nuschel server's stored conversation between two
//! accounts). Its timeline, unread count, read marker, who is typing, and
//! the activity lines (joins, moves, moderation, connection gaps) shown
//! among its messages.
use crate::{
    model::{TextTarget, User},
    timeline::Timeline,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Activity lines a conversation keeps (in memory only).
pub const MAX_ACTIVITY: usize = 200;

/// How long a typing indicator stays without a new one (two missed in a row).
pub const TYPING_SHOWN_MS: u64 = 6_000;

#[derive(Clone, Debug)]
pub struct ConversationState {
    /// A channel's id; direct conversations count down from `u32::MAX`.
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub topic: String,
    pub position: i32,
    pub target: TextTarget,
    /// The other person of a direct conversation, as last seen.
    pub direct_peer: Option<User>,
    /// Where local history keeps it.
    pub storage_key: String,
    /// The latest page of history, older pages prepended as the reader
    /// scrolls up, and live messages appended.
    pub timeline: Timeline,
    /// Messages that arrived while the reader was scrolled up from the newest.
    pub new_messages: usize,
    /// Messages that arrived out of sight, and whether one mentions you or
    /// is a direct message.
    pub unread: usize,
    pub unread_mention: bool,
    /// Joins, moves, moderation and connection gaps; in memory only.
    pub activity: VecDeque<ActivityLine>,
    pub links: BTreeSet<u32>,
    pub temporary: bool,
    pub max_users: u32,
    /// Raw, untrusted Mumble HTML; only the content worker renders it.
    pub description_raw: String,
    /// Its conversation on a Nuschel server, and the read marker sent last.
    pub conversation: Option<[u8; 16]>,
    pub read_seq: i64,
    /// Who is typing here (Nuschel servers), by session, and when they last
    /// said so (the platform's monotonic milliseconds).
    pub typing: BTreeMap<u32, u64>,
}

/// A one-line event among a conversation's messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityLine {
    /// Identifies the line to a UI while it lives; never reused.
    pub serial: u64,
    pub observed_at_ms: i64,
    pub text: String,
    /// A gap or a problem rather than routine (joins, moves): never folded
    /// into a run.
    pub warning: bool,
}

impl Default for ConversationState {
    fn default() -> Self {
        Self::root()
    }
}

impl ConversationState {
    /// The root channel before the server has said anything.
    pub fn root() -> Self {
        Self {
            id: 0,
            parent: None,
            name: "Root".into(),
            topic: "Connect to load channels and users".into(),
            position: 0,
            target: TextTarget::Channel(0),
            direct_peer: None,
            storage_key: "0".into(),
            timeline: Timeline::default(),
            new_messages: 0,
            unread: 0,
            unread_mention: false,
            activity: VecDeque::new(),
            links: BTreeSet::new(),
            temporary: false,
            max_users: 0,
            description_raw: String::new(),
            conversation: None,
            read_seq: 0,
            typing: BTreeMap::new(),
        }
    }

    /// A channel's conversation.
    pub fn channel(id: u32, parent: Option<u32>, name: String, position: i32) -> Self {
        Self {
            id,
            parent,
            name,
            position,
            target: TextTarget::Channel(id),
            storage_key: id.to_string(),
            ..Self::root()
        }
    }

    /// A direct conversation shown as `@ name`, with id `id`.
    pub fn direct(id: u32, name: &str, target: TextTarget, storage_key: String) -> Self {
        Self {
            id,
            name: format!("@ {name}"),
            topic: "Private conversation".into(),
            position: i32::MAX,
            target,
            storage_key,
            ..Self::root()
        }
    }

    /// Add an activity line; the oldest go beyond [`MAX_ACTIVITY`].
    pub fn log(&mut self, text: String, observed_at_ms: i64, warning: bool) {
        static SERIALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        self.activity.push_back(ActivityLine {
            serial: SERIALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            observed_at_ms,
            text,
            warning,
        });
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_front();
        }
    }

    /// Whether this is a direct conversation (either kind).
    pub fn is_direct(&self) -> bool {
        matches!(
            self.target,
            TextTarget::Direct(_) | TextTarget::DirectConversation(_)
        )
    }

    /// Who is typing here now: sessions whose indicator is not over.
    pub fn typing_now(&self, now_ms: u64) -> impl Iterator<Item = u32> + '_ {
        self.typing
            .iter()
            .filter(move |(_, at)| now_ms.saturating_sub(**at) < TYPING_SHOWN_MS)
            .map(|(session, _)| *session)
    }

    /// Drop indicators that are over; returns whether any went.
    pub fn expire_typing(&mut self, now_ms: u64) -> bool {
        let before = self.typing.len();
        self.typing
            .retain(|_, at| now_ms.saturating_sub(*at) < TYPING_SHOWN_MS);
        self.typing.len() != before
    }
}

/// An entry of a conversation as a UI lists it: messages and activity lines
/// by time; routine activity lines (joins, moves, connects…) folded into a
/// run when there are two or more in a row, warnings on their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// `activity[index]`.
    Activity(usize),
    /// `activity[first..=last]` folded; `expanded` lists them after it.
    Run {
        first: usize,
        last: usize,
        expanded: bool,
    },
    /// `timeline.messages[position]`; `grouped` under the previous message's
    /// author header (same author within [`GROUP_MS`]).
    Message { position: usize, grouped: bool },
}

/// Consecutive messages by one author within this window share a header.
pub const GROUP_MS: i64 = 5 * 60 * 1000;

impl ConversationState {
    /// The conversation's entries in order. A run is expanded when
    /// `expanded` holds its first line's time (which runs are open is a
    /// UI's own state).
    pub fn entries(&self, expanded: impl Fn(i64) -> bool) -> Vec<Entry> {
        let messages = &self.timeline.messages;
        let mut entries = Vec::with_capacity(messages.len() + self.activity.len());
        let mut previous: Option<(&str, i64)> = None;
        let mut activity = self.activity.iter().enumerate().peekable();
        let mut pending = Vec::new();
        for (position, message) in messages.iter().enumerate() {
            pending.clear();
            while let Some((index, _)) =
                activity.next_if(|(_, line)| line.observed_at_ms <= message.observed_at_ms)
            {
                pending.push(index);
            }
            if !pending.is_empty() {
                self.push_activity(&mut entries, &pending, &expanded);
                previous = None;
            }
            let grouped = previous.is_some_and(|(author, at)| {
                author == message.author && message.observed_at_ms.saturating_sub(at) < GROUP_MS
            });
            // A tombstone keeps the group it is in, but never starts one: the
            // message after it shows its author again.
            if message.deleted.is_none() || grouped {
                previous = Some((&message.author, message.observed_at_ms));
            } else {
                previous = None;
            }
            entries.push(Entry::Message { position, grouped });
        }
        let rest: Vec<usize> = activity.map(|(index, _)| index).collect();
        self.push_activity(&mut entries, &rest, &expanded);
        entries
    }

    fn push_activity(
        &self,
        entries: &mut Vec<Entry>,
        run: &[usize],
        expanded: impl Fn(i64) -> bool,
    ) {
        let mut lines = run.iter().copied().peekable();
        while let Some(first) = lines.next() {
            if self.activity[first].warning {
                entries.push(Entry::Activity(first));
                continue;
            }
            let mut last = first;
            while let Some(next) = lines.next_if(|next| !self.activity[*next].warning) {
                last = next;
            }
            if last == first {
                entries.push(Entry::Activity(first));
                continue;
            }
            let expanded = expanded(self.activity[first].observed_at_ms);
            entries.push(Entry::Run {
                first,
                last,
                expanded,
            });
            if expanded {
                entries.extend((first..=last).map(Entry::Activity));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::IncomingMessage;
    use nuschel_core::{ChatMessage, MessageDocument};

    fn message(author: &str, at: i64) -> IncomingMessage {
        IncomingMessage::plain(
            Some(at),
            ChatMessage {
                server: String::new(),
                channel: String::new(),
                author: author.into(),
                observed_at_ms: at,
                body: String::new(),
            },
            MessageDocument { blocks: Vec::new() },
            false,
        )
    }

    #[test]
    fn activity_folds_into_runs_among_messages_grouped_by_author() {
        let mut conversation = ConversationState::default();
        conversation.log("Ada joined".into(), 10, false);
        conversation.log("Bob joined".into(), 20, false);
        conversation.timeline.merge(message("Ada", 30));
        conversation.timeline.merge(message("Ada", 40));
        conversation.log("Disconnected".into(), 50, true);
        conversation.timeline.merge(message("Ada", 60));
        conversation.timeline.merge(message("Ada", 60 + GROUP_MS));
        conversation.log("Cy joined".into(), 70 + GROUP_MS, false);
        let entries = conversation.entries(|_| false);
        assert_eq!(
            entries,
            [
                Entry::Run {
                    first: 0,
                    last: 1,
                    expanded: false
                },
                Entry::Message {
                    position: 0,
                    grouped: false
                },
                Entry::Message {
                    position: 1,
                    grouped: true
                },
                // A warning stands alone and starts a new group.
                Entry::Activity(2),
                Entry::Message {
                    position: 2,
                    grouped: false
                },
                // Too late for the group.
                Entry::Message {
                    position: 3,
                    grouped: false
                },
                Entry::Activity(3),
            ]
        );
        let open = conversation.entries(|first| first == 10);
        assert_eq!(
            &open[..3],
            [
                Entry::Run {
                    first: 0,
                    last: 1,
                    expanded: true
                },
                Entry::Activity(0),
                Entry::Activity(1),
            ]
        );
    }

    #[test]
    fn typing_indicators_fade() {
        let mut conversation = ConversationState::default();
        conversation.typing.insert(2, 1000);
        conversation.typing.insert(3, 4000);
        assert_eq!(conversation.typing_now(5000).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(
            conversation
                .typing_now(1000 + TYPING_SHOWN_MS)
                .collect::<Vec<_>>(),
            [3]
        );
        assert!(conversation.expire_typing(1000 + TYPING_SHOWN_MS));
        assert!(!conversation.expire_typing(1000 + TYPING_SHOWN_MS));
        assert_eq!(conversation.typing.len(), 1);
    }

    #[test]
    fn the_activity_log_keeps_the_newest() {
        let mut conversation = ConversationState::default();
        for at in 0..(MAX_ACTIVITY as i64 + 5) {
            conversation.log(format!("line {at}"), at, false);
        }
        assert_eq!(conversation.activity.len(), MAX_ACTIVITY);
        assert_eq!(conversation.activity[0].observed_at_ms, 5);
    }
}
