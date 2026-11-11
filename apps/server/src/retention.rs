// SPDX-License-Identifier: EUPL-1.2
//! Retention (docs/extension.md §6): how long each conversation keeps its
//! messages. The server default is forever (user decision: "we don't want
//! people to lose data because of such a setting being too low by default");
//! shorter retention is an explicit setting of the server (`history.retention`)
//! or of a channel (`ChannelSettings`, Write on the channel), never more than
//! `history.max_retention_days`. "None" means not archived: live delivery
//! only, nothing stored, no history pages.
//!
//! Expiry runs hourly on its own storage worker and tombstones expirable rows
//! in batches (`nuschl_ipc::server::expirable` is the hook for records that
//! must stay); history pages hide expired content at once, so a message older
//! than its conversation's retention is never served, whenever the timer runs.
use crate::{
    chat::now_ms,
    conversations::{Id, hex},
    server::Server,
    state::State,
    storage::StorageHandle,
};
use nuschl_ipc::server::{
    EXPIRE_BATCH, Retention, ServerStorageRequest as Request, ServerStorageResponse as Response,
};
use nuschl_protocol::nuschl::v1;
use std::{sync::Arc, time::Duration};

/// How often expiry runs.
pub const INTERVAL: Duration = Duration::from_secs(3600);

impl State {
    /// A conversation's own retention (its channel's setting), if it has one.
    pub fn own_retention(&self, id: &Id) -> Option<Retention> {
        self.conversations.retention.get(id).copied()
    }

    /// What a conversation keeps: its own retention or the server default,
    /// never more than the server's maximum.
    pub fn retention(&self, id: &Id) -> Retention {
        let history = &self.settings.config.history;
        self.own_retention(id).map_or_else(
            || history.default_retention(),
            |own| own.min(history.maximum_retention()),
        )
    }

    /// Nothing is stored for this conversation: live delivery only.
    pub fn live_only(&self, id: &Id) -> bool {
        self.retention(id) == Retention::None
    }

    /// What a conversation's retention would be with `own` (`None`: the
    /// server default).
    pub fn retention_with(&self, own: Option<Retention>) -> Retention {
        let history = &self.settings.config.history;
        own.map_or_else(
            || history.default_retention(),
            |own| own.min(history.maximum_retention()),
        )
    }

    /// Set or clear a conversation's own retention and persist it (ordered
    /// with the history writes). Returns whether what it keeps changed.
    pub fn set_retention(&mut self, id: Id, own: Option<Retention>) -> bool {
        let before = self.retention(&id);
        match own {
            Some(retention) => self.conversations.retention.insert(id, retention),
            None => self.conversations.retention.remove(&id),
        };
        self.history.write(Request::HistorySetRetention {
            conversation: hex(&id),
            retention: own,
        });
        before != self.retention(&id)
    }

    /// Conversations with stored messages that expire, and their cutoffs.
    pub fn expiring(&self, now_ms: i64) -> Vec<(String, i64)> {
        self.conversations
            .ids()
            .filter(|id| self.conversations.head(id) > 0)
            .filter_map(|id| Some((hex(id), self.retention(id).cutoff_ms(now_ms)?)))
            .collect()
    }

    /// A channel now keeps something else: stock clients in it (or listening)
    /// are told again (docs/extension.md §9).
    pub fn retention_changed(&mut self, channel: u32) {
        let stock: Vec<u32> = self
            .users
            .values()
            .filter(|user| {
                user.extension.is_none()
                    && (user.channel == channel || user.listening.contains_key(&channel))
            })
            .map(|user| user.session)
            .collect();
        for session in stock {
            if let Some(user) = self.users.get_mut(&session) {
                user.notified.remove(&channel);
            }
            self.retention_notice(session, channel);
        }
    }

    /// A conversation's retention on the wire (`ChannelInfo`): days (0 is
    /// forever) and whether it is live-only.
    pub fn wire_retention(&self, id: &Id) -> (u32, bool) {
        wire(self.retention(id))
    }
}

/// Days (0 is forever) and live-only, as `Policy` and `ChannelInfo` carry them.
pub fn wire(retention: Retention) -> (u32, bool) {
    match retention {
        Retention::Forever => (0, false),
        Retention::Days(days) => (days, false),
        Retention::None => (0, true),
    }
}

/// A `ChannelSettings` retention: `Some(None)` follows the server default,
/// `None` is malformed.
pub fn from_wire(setting: &v1::Retention) -> Option<Option<Retention>> {
    use nuschl_ipc::server::MAX_RETENTION_DAYS;
    use v1::retention::Kind;
    match setting.kind? {
        Kind::ServerDefault(_) => Some(None),
        Kind::Forever(_) => Some(Some(Retention::Forever)),
        Kind::LiveOnly(_) => Some(Some(Retention::None)),
        Kind::Days(days) if (1..=MAX_RETENTION_DAYS).contains(&days) => {
            Some(Some(Retention::Days(days)))
        }
        Kind::Days(_) => None,
    }
}

/// What stock clients are told (docs/extension.md §9): in the welcome text
/// about the server default, and on entering a channel about the channel's.
pub fn notice(retention: Retention, channel: bool) -> String {
    let place = if channel {
        "this channel"
    } else {
        "this server's channels"
    };
    let kept = match retention {
        Retention::Forever => "kept without a time limit".to_owned(),
        Retention::Days(1) => "kept for 1 day".to_owned(),
        Retention::Days(days) => format!("kept for {days} days"),
        Retention::None => {
            let unless = if channel {
                ""
            } else {
                ", unless a channel says otherwise when you enter it"
            };
            return format!(
                "Text in {place} is not archived{unless}: people who join later cannot read \
                 it. Some users of this server receive messages without being shown in the \
                 channel."
            );
        }
    };
    format!(
        "Text in {place} is {kept}, and people who join a channel later can read it. Some \
         users of this server receive messages without being shown in the channel."
    )
}

/// Expire what is due now, in batches; the number of messages tombstoned, or
/// `None` if storage failed (the next run continues).
pub async fn expire_now(server: &Server, storage: &StorageHandle) -> Option<u64> {
    let now = now_ms();
    let due = server.lock().expiring(now);
    let mut total = 0;
    for (conversation, before_ms) in due {
        loop {
            let request = Request::HistoryExpire {
                conversation: conversation.clone(),
                before_ms,
                deleted_ms: now,
                limit: EXPIRE_BATCH,
            };
            let Some(Response::Count(count)) = storage.request(request).await else {
                return None;
            };
            total += count;
            if count < u64::from(EXPIRE_BATCH) {
                break;
            }
        }
    }
    Some(total)
}

/// The hourly expiry, on its own storage worker so that neither chat writes nor
/// history pages wait behind it.
pub async fn run(server: Arc<Server>, storage: StorageHandle) {
    let mut tick = tokio::time::interval(INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match expire_now(&server, &storage).await {
            Some(0) => {}
            Some(count) => tracing::info!(count, "expired messages past their retention"),
            None => tracing::warn!("expiring messages failed; the next run continues"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_orders_by_what_it_keeps() {
        let (none, week, year, forever) = (
            Retention::None,
            Retention::Days(7),
            Retention::Days(365),
            Retention::Forever,
        );
        assert!(none.shorter_than(week) && week.shorter_than(year));
        assert!(year.shorter_than(forever) && !forever.shorter_than(forever));
        assert_eq!(forever.min(week), week);
        assert_eq!(none.min(forever), none);
        assert_eq!(forever.cutoff_ms(1_000), None);
        assert_eq!(none.cutoff_ms(1_000), Some(1_000));
        assert_eq!(
            Retention::Days(1).cutoff_ms(86_400_000 + 5),
            Some(5),
            "a day earlier"
        );
        assert_eq!(wire(none), (0, true));
        assert_eq!(wire(week), (7, false));
        assert_eq!(wire(forever), (0, false));
    }

    #[test]
    fn wire_settings_are_bounded() {
        use v1::retention::Kind;
        let setting = |kind| v1::Retention { kind: Some(kind) };
        assert_eq!(from_wire(&setting(Kind::ServerDefault(true))), Some(None));
        assert_eq!(
            from_wire(&setting(Kind::LiveOnly(true))),
            Some(Some(Retention::None))
        );
        assert_eq!(
            from_wire(&setting(Kind::Days(30))),
            Some(Some(Retention::Days(30)))
        );
        assert!(from_wire(&setting(Kind::Days(0))).is_none());
        assert!(from_wire(&setting(Kind::Days(36_501))).is_none());
        assert!(from_wire(&v1::Retention { kind: None }).is_none());
    }

    #[test]
    fn stock_notices_say_what_is_kept() {
        assert!(notice(Retention::Forever, false).contains("kept without a time limit"));
        assert!(notice(Retention::Days(30), true).contains("this channel is kept for 30 days"));
        let none = notice(Retention::None, true);
        assert!(none.contains("not archived") && !none.contains("unless"));
        assert!(notice(Retention::None, false).contains("unless a channel says otherwise"));
    }
}
