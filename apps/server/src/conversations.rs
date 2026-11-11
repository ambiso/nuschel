// SPDX-License-Identifier: EUPL-1.2
//! Durable conversations and who may read them (docs/extension.md §3, §5).
//!
//! A conversation is a channel's (its random `conversation_id`), a direct
//! conversation between two accounts, or the archive of a deleted permanent
//! channel (administrators only). `can_read` is Enter on the channel, being in
//! it or listening to it, evaluated with persistent credentials only (the
//! access tokens from `Authenticate`, never temporary ones). The state mutex
//! is the single owner of authorisation state; `authz_epoch` counts its
//! changes, and every trigger re-evaluates subscriptions and `since_access`
//! intervals before the handler's lock is released ([`State::finish`]).
use crate::{
    acl::{self, Subject, perm},
    config::GuestHistory,
    state::{Registered, State},
};
use nuschel_ipc::server::{HistoryVisibility, ServerStorageRequest as Request};
use nuschel_protocol::nuschel::{self, kind, v1};
use std::collections::{BTreeSet, HashMap};

pub type Id = nuschel::Id;

pub fn hex(id: &Id) -> String {
    crate::tls::hex(id)
}

pub fn parse(text: &str) -> Option<Id> {
    if text.len() != 32 {
        return None;
    }
    let mut out = [0; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Channel(u32),
    /// The two accounts, in order.
    Direct(Id, Id),
    /// A deleted permanent channel's history: no ACL remains to authorise it,
    /// so only administrators (SuperUser) may read it.
    Archived,
}

/// An account's `since_access` intervals: (first seq, last seq or open).
pub type Intervals = Vec<(u64, Option<u64>)>;

/// `since_access` intervals kept per account and conversation; the oldest are
/// dropped beyond this (which only ever hides history, never reveals it).
pub const MAX_INTERVALS: usize = 32;
/// Subscriptions per account across its connections.
pub const MAX_ACCOUNT_SUBSCRIPTIONS: usize = 1024;

#[derive(Default)]
pub struct Conversations {
    kinds: HashMap<Id, Kind>,
    heads: HashMap<Id, u64>,
    directs: HashMap<(Id, Id), Id>,
    by_account: HashMap<Id, Vec<Id>>,
    /// Connections subscribed to each conversation.
    pub subscribers: HashMap<Id, BTreeSet<u32>>,
    /// `since_access` intervals: conversation → account → (start, end).
    pub access: HashMap<Id, HashMap<Id, Intervals>>,
    /// Read markers of accounts that are online: account → conversation → seq.
    pub markers: HashMap<Id, HashMap<Id, u64>>,
}

impl Conversations {
    pub fn kind(&self, id: &Id) -> Option<Kind> {
        self.kinds.get(id).copied()
    }

    pub fn head(&self, id: &Id) -> u64 {
        self.heads.get(id).copied().unwrap_or(0)
    }

    pub fn set_head(&mut self, id: Id, head: u64) {
        let current = self.heads.entry(id).or_insert(0);
        *current = (*current).max(head);
    }

    /// The next sequence number. It is assigned here, under the state lock,
    /// so storage, fan-out and access intervals all agree on the order.
    pub fn next_seq(&mut self, id: Id) -> u64 {
        let head = self.heads.entry(id).or_insert(0);
        *head += 1;
        *head
    }

    pub fn add_channel(&mut self, id: Id, channel: u32) {
        self.kinds.insert(id, Kind::Channel(channel));
    }

    /// A conversation with history but no live channel or pair of accounts.
    pub fn add_archived(&mut self, id: Id) {
        self.kinds.entry(id).or_insert(Kind::Archived);
    }

    pub fn archive(&mut self, id: Id) {
        self.kinds.insert(id, Kind::Archived);
    }

    /// Forget a conversation entirely (a temporary channel's).
    pub fn remove(&mut self, id: &Id) {
        self.kinds.remove(id);
        self.heads.remove(id);
        self.access.remove(id);
        for markers in self.markers.values_mut() {
            markers.remove(id);
        }
    }

    pub fn pair(a: Id, b: Id) -> (Id, Id) {
        if a <= b { (a, b) } else { (b, a) }
    }

    pub fn direct(&self, a: Id, b: Id) -> Option<Id> {
        self.directs.get(&Self::pair(a, b)).copied()
    }

    pub fn add_direct(&mut self, id: Id, a: Id, b: Id) {
        let (a, b) = Self::pair(a, b);
        self.kinds.insert(id, Kind::Direct(a, b));
        self.directs.insert((a, b), id);
        for account in [a, b] {
            let list = self.by_account.entry(account).or_default();
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }

    pub fn directs_of(&self, account: &Id) -> &[Id] {
        self.by_account.get(account).map_or(&[], Vec::as_slice)
    }

    pub fn marker(&self, account: Option<Id>, id: &Id) -> u64 {
        account
            .and_then(|account| self.markers.get(&account)?.get(id).copied())
            .unwrap_or(0)
    }

    /// The account's visible seq ranges in a `since_access` conversation.
    pub fn intervals(&self, id: &Id, account: &Id) -> Vec<(u64, u64)> {
        let mut ranges: Vec<(u64, u64)> = self
            .access
            .get(id)
            .and_then(|accounts| accounts.get(account))
            .map(|intervals| {
                intervals
                    .iter()
                    .map(|&(start, end)| (start, end.unwrap_or(u64::MAX)))
                    .filter(|(start, end)| start <= end)
                    .collect()
            })
            .unwrap_or_default();
        ranges.sort_unstable();
        ranges
    }
}

/// What a change of authorisation state can affect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One connection's own reading (it moved, listened, changed tokens).
    Session(u32),
    /// Everyone's (ACLs, groups, the tree, registrations).
    All,
}

#[derive(Default)]
pub struct Pending {
    pub all: bool,
    pub sessions: BTreeSet<u32>,
}

impl State {
    /// Record an authorisation change; re-evaluation runs in [`State::finish`],
    /// after the handler (and after temporary access tokens are gone again).
    pub fn authz_changed(&mut self, scope: Scope) {
        self.authz_epoch += 1;
        match scope {
            Scope::All => self.authz_pending.all = true,
            Scope::Session(session) => {
                self.authz_pending.sessions.insert(session);
            }
        }
    }

    pub fn conversation_of_channel(&self, channel: u32) -> Option<Id> {
        self.channels
            .get(channel)
            .map(|channel| channel.conversation_id)
    }

    fn guests_read(&self) -> bool {
        self.settings.config.history.guests == GuestHistory::Full
    }

    /// `can_read` (docs/extension.md §5).
    pub fn can_read(&self, session: u32, id: &Id) -> bool {
        let Some(user) = self.users.get(&session) else {
            return false;
        };
        match self.conversations.kind(id) {
            None => false,
            Some(Kind::Channel(channel)) => {
                (user.account.is_some() || self.guests_read())
                    && (user.channel == channel
                        || user.listening.contains_key(&channel)
                        || self.has(session, channel, perm::ENTER))
            }
            Some(Kind::Direct(a, b)) => user.account.is_some_and(|me| me == a || me == b),
            Some(Kind::Archived) => user.user_id == Some(0),
        }
    }

    /// `can_read` for an account that is not online: its registration and
    /// certificate only (no tokens, in no channel).
    pub fn can_read_offline(&self, registered: &Registered, channel: u32) -> bool {
        let subject = Subject {
            user_id: Some(registered.id),
            channel: u32::MAX,
            hash: registered.certificate_hash.as_deref(),
            tokens: &[],
            strong: false,
        };
        acl::effective_permissions(&self.channels, &subject, channel) & perm::ENTER != 0
    }

    /// May this connection post to the conversation (send, edit)?
    pub fn can_write(&self, session: u32, id: &Id) -> bool {
        let Some(user) = self.users.get(&session) else {
            return false;
        };
        match self.conversations.kind(id) {
            Some(Kind::Channel(channel)) => self.has(session, channel, perm::TEXT_MESSAGE),
            Some(Kind::Direct(a, b)) => {
                let Some(me) = user.account.filter(|me| *me == a || *me == b) else {
                    return false;
                };
                let peer = if me == a { b } else { a };
                // A deleted account's conversations are frozen.
                if self.registry.by_account(&peer).is_none() {
                    return false;
                }
                // Murmur's rule for messages to a user: TextMessage in their channel.
                self.users
                    .values()
                    .filter(|other| other.account == Some(peer))
                    .all(|other| self.has(session, other.channel, perm::TEXT_MESSAGE))
            }
            Some(Kind::Archived) | None => false,
        }
    }

    pub fn visibility(&self, id: &Id) -> HistoryVisibility {
        match self.conversations.kind(id) {
            Some(Kind::Channel(channel)) => self
                .channels
                .get(channel)
                .map_or(HistoryVisibility::Members, |channel| {
                    channel.history_visibility
                }),
            _ => HistoryVisibility::Members,
        }
    }

    /// The seq ranges this connection may see, and whether they are clipped.
    pub fn visible_ranges(&self, session: u32, id: &Id) -> (Vec<(u64, u64)>, bool) {
        if self.visibility(id) == HistoryVisibility::Members {
            return (vec![(1, u64::MAX)], false);
        }
        let ranges = self
            .users
            .get(&session)
            .and_then(|user| user.account)
            .map(|account| self.conversations.intervals(id, &account))
            .unwrap_or_default();
        (ranges, true)
    }

    /// Connections that negotiated history and receive a conversation's live
    /// events now: its live audience (in the channel or listening, as for
    /// stock text), the accounts of a direct conversation, and subscribers
    /// that may read it at this moment.
    pub fn readers(&self, id: &Id) -> BTreeSet<u32> {
        let history = |session: &u32| {
            self.users.get(session).is_some_and(|user| {
                user.extension
                    .as_ref()
                    .is_some_and(|extension| extension.has(nuschel::capability::HISTORY))
            })
        };
        let mut out = BTreeSet::new();
        match self.conversations.kind(id) {
            Some(Kind::Channel(channel)) => {
                if let Some(channel) = self.channels.get(channel) {
                    out.extend(channel.users.iter().chain(&channel.listeners).copied());
                }
            }
            Some(Kind::Direct(a, b)) => out.extend(
                self.users
                    .values()
                    .filter(|user| user.account == Some(a) || user.account == Some(b))
                    .map(|user| user.session),
            ),
            _ => {}
        }
        if let Some(subscribers) = self.conversations.subscribers.get(id) {
            out.extend(
                subscribers
                    .iter()
                    .filter(|session| self.can_read(**session, id))
                    .copied(),
            );
        }
        out.retain(history);
        out
    }

    /// Connections that see a conversation's messages as stock `TextMessage`s.
    pub fn stock_audience(&self, id: &Id) -> BTreeSet<u32> {
        let stock = |session: &u32| {
            self.users.get(session).is_some_and(|user| {
                !user
                    .extension
                    .as_ref()
                    .is_some_and(|extension| extension.has(nuschel::capability::HISTORY))
            })
        };
        let mut out = BTreeSet::new();
        match self.conversations.kind(id) {
            Some(Kind::Channel(channel)) => {
                if let Some(channel) = self.channels.get(channel) {
                    out.extend(channel.users.iter().chain(&channel.listeners).copied());
                }
            }
            Some(Kind::Direct(a, b)) => out.extend(
                self.users
                    .values()
                    .filter(|user| user.account == Some(a) || user.account == Some(b))
                    .map(|user| user.session),
            ),
            _ => {}
        }
        out.retain(stock);
        out
    }

    /// Re-evaluate what pending authorisation changes affect: subscriptions
    /// that lost `can_read` are dropped (and the client told), and
    /// `since_access` intervals open and close.
    pub fn reevaluate(&mut self) {
        let pending = std::mem::take(&mut self.authz_pending);
        if !pending.all && pending.sessions.is_empty() {
            return;
        }
        let sessions: Vec<u32> = if pending.all {
            self.users
                .values()
                .filter(|user| user.extension.is_some())
                .map(|user| user.session)
                .collect()
        } else {
            pending.sessions.iter().copied().collect()
        };
        for session in sessions {
            let lost: Vec<Id> = match self
                .users
                .get(&session)
                .and_then(|user| user.extension.as_ref())
            {
                Some(extension) => extension
                    .subscriptions
                    .iter()
                    .filter(|id| !self.can_read(session, id))
                    .copied()
                    .collect(),
                None => continue,
            };
            for id in lost {
                self.unsubscribe(session, &id);
                self.send(
                    session,
                    kind::UNSUBSCRIBED,
                    &v1::Unsubscribed {
                        conversation: id.to_vec(),
                    },
                );
            }
        }
        self.update_access(&pending);
    }

    pub fn unsubscribe(&mut self, session: u32, id: &Id) {
        if let Some(extension) = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
        {
            extension.subscriptions.remove(id);
        }
        if let Some(subscribers) = self.conversations.subscribers.get_mut(id) {
            subscribers.remove(&session);
            if subscribers.is_empty() {
                self.conversations.subscribers.remove(id);
            }
        }
    }

    /// Whether an account can read a channel now: through any of its
    /// connections, or by its persistent identity while offline.
    fn account_reads(&self, account: &Id, channel: u32) -> bool {
        let Some(id) = self.conversation_of_channel(channel) else {
            return false;
        };
        let mut online = false;
        for user in self
            .users
            .values()
            .filter(|user| user.account == Some(*account))
        {
            online = true;
            if self.can_read(user.session, &id) {
                return true;
            }
        }
        !online
            && self
                .registry
                .by_account(account)
                .is_some_and(|registered| self.can_read_offline(registered, channel))
    }

    /// Open and close `since_access` intervals for the accounts a change may
    /// affect. Intervals open when the server sees an account able to read
    /// (online, or while offline on a global change) and close at the head
    /// when it no longer can; a gap is never bridged.
    fn update_access(&mut self, pending: &Pending) {
        let channels: Vec<(u32, Id)> = self
            .channels
            .0
            .values()
            .filter(|channel| channel.history_visibility == HistoryVisibility::SinceAccess)
            .map(|channel| (channel.id, channel.conversation_id))
            .collect();
        if channels.is_empty() {
            return;
        }
        let accounts: BTreeSet<Id> = if pending.all {
            self.registry
                .users
                .values()
                .map(|user| user.account)
                .collect()
        } else {
            pending
                .sessions
                .iter()
                .filter_map(|session| self.users.get(session)?.account)
                .collect()
        };
        for (channel, id) in channels {
            for account in &accounts {
                let reads = self.account_reads(account, channel);
                self.set_access(id, *account, reads);
            }
        }
    }

    /// Open (at head + 1) or close (at head) an account's interval.
    pub fn set_access(&mut self, id: Id, account: Id, reads: bool) {
        let head = self.conversations.head(&id);
        let intervals = self
            .conversations
            .access
            .entry(id)
            .or_default()
            .entry(account)
            .or_default();
        let open = intervals.last().is_some_and(|(_, end)| end.is_none());
        match (reads, open) {
            (true, false) => {
                intervals.push((head + 1, None));
                if intervals.len() > MAX_INTERVALS {
                    intervals.remove(0);
                }
            }
            (false, true) => {
                if let Some(last) = intervals.last_mut() {
                    last.1 = Some(head);
                    // Nothing was readable in an interval closed at once.
                    if head < last.0 {
                        intervals.pop();
                    }
                }
            }
            _ => return,
        }
        let intervals = intervals.clone();
        self.history.write(Request::HistorySetAccess {
            conversation: hex(&id),
            account: hex(&account),
            intervals,
        });
    }

    /// Switching a channel to `since_access` keeps what its readers could see
    /// before: everyone who can read it now gets an interval from the start.
    pub fn begin_since_access(&mut self, channel: u32) {
        let Some(id) = self.conversation_of_channel(channel) else {
            return;
        };
        let accounts: Vec<Id> = self
            .registry
            .users
            .values()
            .map(|user| user.account)
            .collect();
        for account in accounts {
            if self.account_reads(&account, channel) {
                let intervals = vec![(1, None)];
                self.conversations
                    .access
                    .entry(id)
                    .or_default()
                    .insert(account, intervals.clone());
                self.history.write(Request::HistorySetAccess {
                    conversation: hex(&id),
                    account: hex(&account),
                    intervals,
                });
            }
        }
    }
}
