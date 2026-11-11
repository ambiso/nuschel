// SPDX-License-Identifier: EUPL-1.2
//! Server state between `nuscheld` and its storage worker: channels with their
//! ACLs and groups, links, registered users, blobs and bans. The server keeps
//! the authoritative state in memory and writes through; the worker owns the
//! database (SQLite or PostgreSQL) and its credential.
use serde::{Deserialize, Serialize};

/// Registered users per `LoadUsers` page, so a large server never needs one
/// oversized frame.
pub const USER_PAGE: u32 = 1000;

/// Who may read a channel's earlier history (docs/extension.md §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryVisibility {
    /// Members read the retained history, including messages from before they joined.
    #[default]
    #[serde(alias = "all")]
    Members,
    /// Only what was said while the reader could read the channel (the
    /// recorded access intervals).
    #[serde(alias = "since_joining")]
    SinceAccess,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAcl {
    pub apply_here: bool,
    pub apply_subs: bool,
    pub user_id: Option<u32>,
    pub group: Option<String>,
    pub grant: u32,
    pub deny: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredGroup {
    pub name: String,
    pub inherit: bool,
    pub inheritable: bool,
    pub add: Vec<u32>,
    pub remove: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredChannel {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub description: String,
    pub position: i32,
    pub max_users: u32,
    pub inherit_acl: bool,
    /// 128 random bits as 32 lowercase hex digits, assigned once and never
    /// reused, even when a channel id is (docs/extension.md, "Identifiers").
    pub conversation_id: String,
    pub history_visibility: HistoryVisibility,
    /// In priority order.
    pub acls: Vec<StoredAcl>,
    pub groups: Vec<StoredGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredUser {
    pub id: u32,
    /// The immutable 128-bit account id (32 hex digits); the registration
    /// number is only Mumble's wire alias. Assigned by the server when missing.
    #[serde(default)]
    pub account: Option<String>,
    pub name: String,
    /// An argon2id PHC string.
    pub password_hash: Option<String>,
    /// SHA-1 of the client certificate, lowercase hex (Mumble's user identity).
    pub certificate_hash: Option<String>,
    pub last_channel: Option<u32>,
    /// Unix seconds.
    pub last_active: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredBan {
    /// An IP address; IPv4 bans are stored as IPv4-mapped IPv6 with the mask
    /// counted on 128 bits, as Mumble does.
    pub address: Option<String>,
    pub mask: u8,
    pub name: String,
    pub hash: String,
    pub reason: String,
    /// Unix seconds.
    pub start: i64,
    /// Seconds; 0 means permanent.
    pub duration: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSnapshot {
    pub channels: Vec<StoredChannel>,
    pub links: Vec<(u32, u32)>,
    pub bans: Vec<StoredBan>,
    /// Registration ids below this were handed out once and are never reused.
    pub next_user_id: u32,
}

/// Rows per history load page (heads, direct conversations, access intervals).
pub const HISTORY_LOAD_PAGE: u32 = 1000;
/// Rows in one history page (docs/extension.md §6).
pub const MAX_HISTORY_ROWS: u32 = 100;
/// Visible seq ranges one history page may be clipped to.
pub const MAX_RANGES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    #[default]
    Message,
    /// Refers to `target`, whose content was replaced.
    Edit,
    /// Refers to `target`, which became a tombstone.
    Delete,
    /// A sequence number that carries nothing (a retried send).
    Void,
}

/// One row of a conversation's history. Ids are 32 lowercase hex digits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredMessage {
    pub conversation: String,
    pub seq: u64,
    pub kind: MessageKind,
    pub target: Option<u64>,
    pub author_account: Option<String>,
    pub author_user_id: Option<u32>,
    pub author_name: String,
    /// `nuschel.v1.Format`.
    pub format: u8,
    pub content: String,
    pub reply_to: Option<u64>,
    /// Unix milliseconds.
    pub sent_ms: i64,
    pub edited_ms: Option<i64>,
    pub edits: u32,
    pub deleted_ms: Option<i64>,
    /// 0 the author, 1 a moderator, 2 expired, 3 erased by the operator.
    pub delete_reason: Option<u32>,
    /// Scoped to (author account, conversation); never sent to other users and
    /// not returned in pages.
    pub idempotency_key: Option<String>,
    /// SHA-256 of what the key was used for, to detect conflicting retries.
    pub body_hash: Option<String>,
}

impl StoredMessage {
    /// A row that only keeps `seq` explicit.
    pub fn void(conversation: &str, seq: u64, sent_ms: i64) -> Self {
        Self {
            conversation: conversation.to_owned(),
            seq,
            kind: MessageKind::Void,
            sent_ms,
            ..Self::default()
        }
    }
}

/// How long a conversation's messages are kept (docs/extension.md §6). The
/// server default is forever (user decision); shorter retention is an explicit
/// server or channel setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    #[default]
    Forever,
    /// Messages older than this many days (at least 1) become tombstones.
    Days(u32),
    /// Not archived: live delivery only; nothing is stored and no history
    /// pages are served.
    None,
}

/// The longest finite retention, in days (100 years); longer is forever.
pub const MAX_RETENTION_DAYS: u32 = 36_500;

impl Retention {
    /// How much it keeps, for comparisons: none < days < forever.
    fn rank(self) -> (u8, u32) {
        match self {
            Self::None => (0, 0),
            Self::Days(days) => (1, days),
            Self::Forever => (2, 0),
        }
    }

    /// Whether changing from `before` to this removes messages `before` kept.
    pub fn shorter_than(self, before: Self) -> bool {
        self.rank() < before.rank()
    }

    /// The tighter of two retentions (a channel's, capped by the server's maximum).
    pub fn min(self, other: Self) -> Self {
        if self.shorter_than(other) {
            self
        } else {
            other
        }
    }

    /// Messages sent before this (Unix milliseconds) are expired; `None` keeps everything.
    pub fn cutoff_ms(self, now_ms: i64) -> Option<i64> {
        match self {
            Self::Forever => None,
            Self::Days(days) => {
                Some(now_ms.saturating_sub(i64::from(days).saturating_mul(86_400_000)))
            }
            Self::None => Some(now_ms),
        }
    }
}

/// A conversation's own retention, overriding the server default. Kept per
/// conversation, so a deleted channel's archive keeps its channel's setting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRetention {
    pub conversation: String,
    pub retention: Retention,
}

/// Rows one `HistoryExpire` tombstones at most, so a large expiry never holds
/// the database's write lock for long; the server repeats until fewer come back.
pub const EXPIRE_BATCH: u32 = 1000;

/// Whether retention and the operator's erase may turn this row into a
/// tombstone: live messages. **The hook for records the server must keep**:
/// a kind that must survive (MLS commits within their catch-up window,
/// docs/e2ee.md §6.3) is excluded here, in the storage worker's
/// `expirable!` SQL predicate (crates/storage/src/server/history.rs), which
/// must say the same.
pub fn expirable(row: &StoredMessage) -> bool {
    row.kind == MessageKind::Message && row.deleted_ms.is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    Latest,
    Before(u64),
    After(u64),
    Around(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub conversation: String,
    pub anchor: Anchor,
    pub limit: u32,
    /// Inclusive, sorted and disjoint seq ranges the reader may see.
    pub ranges: Vec<(u64, u64)>,
    /// Content bytes; a page ends early at this cap but never skips a row, and
    /// the row nearest the anchor always comes, however large.
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredPage {
    /// Chronological.
    pub rows: Vec<StoredMessage>,
    pub more_before: bool,
    pub more_after: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredDirect {
    pub conversation: String,
    pub account_a: String,
    pub account_b: String,
}

/// An interval in which an account could read a `since_access` channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAccess {
    pub conversation: String,
    pub account: String,
    pub start: u64,
    /// `None` while open.
    pub end: Option<u64>,
}

/// Rows in a seq range, nearest the anchor first; what [`page`] needs from a store.
pub trait SeqRows {
    type Error;
    /// Rows with `lo <= seq <= hi`, descending or ascending, at most `limit`.
    fn rows(
        &mut self,
        conversation: &str,
        range: (u64, u64),
        descending: bool,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<StoredMessage>, Self::Error>>;
}

/// Rows in the visible ranges, from the anchor outwards.
async fn visible<S: SeqRows>(
    store: &mut S,
    query: &PageQuery,
    bound: (u64, u64),
    descending: bool,
    limit: usize,
) -> Result<Vec<StoredMessage>, S::Error> {
    let mut out = Vec::new();
    let mut ranges: Vec<(u64, u64)> = query
        .ranges
        .iter()
        .take(MAX_RANGES)
        .filter_map(|&(lo, hi)| {
            let (lo, hi) = (lo.max(bound.0), hi.min(bound.1));
            (lo <= hi).then_some((lo, hi))
        })
        .collect();
    if descending {
        ranges.reverse();
    }
    for range in ranges {
        if out.len() >= limit {
            break;
        }
        let rows = store
            .rows(&query.conversation, range, descending, limit - out.len())
            .await?;
        out.extend(rows);
    }
    Ok(out)
}

/// One history page (docs/extension.md §6): chronological, at most `limit`
/// (≤ 100) rows, clipped to the visible ranges, ending early at the byte cap
/// without ever skipping a row.
pub async fn page<S: SeqRows>(store: &mut S, query: &PageQuery) -> Result<StoredPage, S::Error> {
    let limit = query.limit.clamp(1, MAX_HISTORY_ROWS) as usize;
    let all = (1, u64::MAX);
    // `older` is nearest-first (descending), `newer` ascending.
    let (mut older, mut newer, mut more_before, mut more_after) = match query.anchor {
        Anchor::Latest | Anchor::Before(_) => {
            let below = match query.anchor {
                Anchor::Before(seq) => seq.saturating_sub(1),
                _ => u64::MAX,
            };
            let mut rows = visible(store, query, (1, below), true, limit + 1).await?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            let after = match query.anchor {
                Anchor::Before(seq) => !visible(store, query, (seq, u64::MAX), false, 1)
                    .await?
                    .is_empty(),
                _ => false,
            };
            (rows, Vec::new(), more, after)
        }
        Anchor::After(seq) => {
            let mut rows = visible(
                store,
                query,
                (seq.saturating_add(1), u64::MAX),
                false,
                limit + 1,
            )
            .await?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            let before = !visible(store, query, (1, seq), true, 1).await?.is_empty();
            (Vec::new(), rows, before, more)
        }
        Anchor::Around(seq) => {
            let half = limit.div_ceil(2);
            let mut below = visible(store, query, (1, seq), true, half + 1).await?;
            let mut above = visible(
                store,
                query,
                (seq.saturating_add(1), all.1),
                false,
                limit - half + 1,
            )
            .await?;
            let more_before = below.len() > half;
            let more_after = above.len() > limit - half;
            below.truncate(half);
            above.truncate(limit - half);
            (below, above, more_before, more_after)
        }
    };
    // The byte cap: drop from the far ends, never the row nearest the anchor.
    let size = |rows: &[StoredMessage]| rows.iter().map(|row| row.content.len()).sum::<usize>();
    let cap = query.max_bytes as usize;
    while size(&older) + size(&newer) > cap && older.len() + newer.len() > 1 {
        if !newer.is_empty() && (newer.len() >= older.len() || older.len() <= 1) {
            newer.pop();
            more_after = true;
        } else {
            older.pop();
            more_before = true;
        }
    }
    older.reverse();
    older.extend(newer);
    for row in &mut older {
        row.idempotency_key = None;
        row.body_hash = None;
    }
    Ok(StoredPage {
        rows: older,
        more_before,
        more_after,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerStorageRequest {
    /// Channels, links and bans; registered users come in pages.
    Load,
    LoadUsers {
        /// Return users with ids above this one.
        after: Option<u32>,
    },
    SaveChannel {
        channel: StoredChannel,
    },
    DeleteChannel {
        id: u32,
    },
    Link {
        a: u32,
        b: u32,
    },
    Unlink {
        a: u32,
        b: u32,
    },
    SaveUser {
        user: StoredUser,
    },
    DeleteUser {
        id: u32,
    },
    SetComment {
        id: u32,
        comment: String,
    },
    /// Base64-encoded image bytes; empty clears it.
    SetTexture {
        id: u32,
        texture: String,
    },
    UserBlobs {
        id: u32,
    },
    SaveBans {
        bans: Vec<StoredBan>,
    },
    SetNextUserId {
        id: u32,
    },
    /// Store a message (or a `Void` for its seq when the key was used before):
    /// `Appended` or `Conflict`.
    HistoryAppend {
        message: StoredMessage,
    },
    /// Replace the content of the author's message `target` and store `event`
    /// (or a `Void`): `Done`, `Missing` or `LimitReached`.
    HistoryEdit {
        target: u64,
        author_account: String,
        format: u8,
        content: String,
        edit_limit: u32,
        event: StoredMessage,
    },
    /// Make `target` a tombstone, by its author (`author_account`) or a
    /// moderator (`None`), and store `event` (or a `Void`): `Done` or `Missing`.
    HistoryDelete {
        target: u64,
        author_account: Option<String>,
        reason: u32,
        event: StoredMessage,
    },
    /// Remove a conversation's history (a temporary channel went away).
    HistoryPurge {
        conversation: String,
    },
    HistoryCreateDirect {
        direct: StoredDirect,
    },
    /// Replace an account's access intervals for a conversation.
    HistorySetAccess {
        conversation: String,
        account: String,
        intervals: Vec<(u64, Option<u64>)>,
    },
    /// An account was deleted: its read markers and access intervals go.
    HistoryForgetAccount {
        account: String,
    },
    /// Raise (never lower) an account's read marker.
    HistorySetReadMarker {
        account: String,
        conversation: String,
        seq: u64,
    },
    /// Tombstone every message by this account (the operator's erase): `Count`.
    HistoryErase {
        account: String,
        deleted_ms: i64,
    },
    /// Tombstone at most `limit` expirable messages of one conversation sent
    /// before `before_ms` (retention): `Count`.
    HistoryExpire {
        conversation: String,
        before_ms: i64,
        deleted_ms: i64,
        limit: u32,
    },
    /// Expirable messages of one conversation sent before `before_ms`: `Count`
    /// (what shortening its retention would remove).
    HistoryCountExpirable {
        conversation: String,
        before_ms: i64,
    },
    /// Set (`Some`) or clear (`None`: the server default) a conversation's
    /// own retention.
    HistorySetRetention {
        conversation: String,
        retention: Option<Retention>,
    },
    /// Conversations with their own retention, in pages (`Retentions`).
    HistoryRetentions {
        after: Option<String>,
    },
    /// Record that a live-only conversation used `seq` without storing it,
    /// so sequence numbers are never reused after a restart.
    HistoryAdvance {
        conversation: String,
        seq: u64,
    },
    /// Conversations with history (or a live-only head) and their highest
    /// seq, in pages.
    HistoryHeads {
        after: Option<String>,
    },
    HistoryDirects {
        after: Option<String>,
    },
    HistoryAccess {
        after: Option<(String, String, u64)>,
    },
    HistoryReadMarkers {
        account: String,
    },
    HistoryPage {
        query: PageQuery,
    },
    /// Key transparency (nuschel-kt's `KtStore`, docs/e2ee.md §5.2): the values of
    /// these keys, in order (`KtValues`). Keys and values are base64; the record layout
    /// belongs to nuschel-kt and `nuscheld`, and the worker never interprets it.
    KtGet {
        keys: Vec<String>,
    },
    /// Every record whose key starts with `prefix`, in key order (`KtRecords`).
    KtScan {
        prefix: String,
    },
    /// Insert or overwrite every record in one transaction: all or none (`Done`).
    KtWrite {
        records: Vec<(String, String)>,
    },
}

/// Keys in one `KtGet`, records in one `KtWrite` or `KtRecords`.
pub const MAX_KT_RECORDS: usize = 100_000;
/// A key-transparency key, and a value (a tree node, a value state, a head, an
/// audit proof of one epoch, or the server's record behind a binding), decoded.
pub const MAX_KT_KEY_BYTES: usize = 1024;
pub const MAX_KT_VALUE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ServerStorageResponse {
    Snapshot(ServerSnapshot),
    Users(Vec<StoredUser>),
    /// Comment text and base64-encoded texture.
    Blobs {
        comment: String,
        texture: String,
    },
    Done,
    Rejected(String),
    Appended {
        seq: u64,
        /// The key was used before for the same message, stored as `seq`.
        duplicate: bool,
    },
    /// The idempotency key was used before for a different message.
    Conflict,
    /// No such message, or not the requester's.
    Missing,
    LimitReached,
    Count(u64),
    Heads(Vec<(String, u64)>),
    Directs(Vec<StoredDirect>),
    Access(Vec<StoredAccess>),
    ReadMarkers(Vec<(String, u64)>),
    Retentions(Vec<StoredRetention>),
    Page(StoredPage),
    KtValues(Vec<Option<String>>),
    KtRecords(Vec<(String, String)>),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rows(Vec<StoredMessage>);

    impl SeqRows for Rows {
        type Error = ();
        async fn rows(
            &mut self,
            _: &str,
            (lo, hi): (u64, u64),
            descending: bool,
            limit: usize,
        ) -> Result<Vec<StoredMessage>, ()> {
            let rows = self
                .0
                .iter()
                .filter(|row| (lo..=hi).contains(&row.seq))
                .cloned();
            Ok(if descending {
                rows.rev().take(limit).collect()
            } else {
                rows.take(limit).collect()
            })
        }
    }

    fn run(query: &PageQuery, sizes: &[usize]) -> StoredPage {
        let rows = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| StoredMessage {
                seq: index as u64 + 1,
                content: "x".repeat(*size),
                idempotency_key: Some("secret".into()),
                ..StoredMessage::default()
            })
            .collect();
        let mut store = Rows(rows);
        let mut future = std::pin::pin!(page(&mut store, query));
        match future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(Ok(page)) => page,
            _ => panic!("the page did not complete"),
        }
    }

    fn query(anchor: Anchor, limit: u32, ranges: Vec<(u64, u64)>, max_bytes: u32) -> PageQuery {
        PageQuery {
            conversation: String::new(),
            anchor,
            limit,
            ranges,
            max_bytes,
        }
    }

    fn seqs(page: &StoredPage) -> Vec<u64> {
        page.rows.iter().map(|row| row.seq).collect()
    }

    #[test]
    fn pages_trim_from_the_far_end_and_keep_the_nearest_row() {
        let all = vec![(1, u64::MAX)];
        // Around 5 with a cap of three rows of 10 bytes: the anchor stays.
        let page = run(&query(Anchor::Around(5), 6, all.clone(), 30), &[10; 9]);
        assert_eq!(seqs(&page), [4, 5, 6]);
        assert!(page.more_before && page.more_after);
        // A row larger than the cap comes alone.
        let page = run(&query(Anchor::Latest, 5, all.clone(), 10), &[5, 5, 100]);
        assert_eq!(seqs(&page), [3]);
        assert!(page.more_before);
        // Ranges clip every anchor; the keys never leave the store.
        let page = run(
            &query(Anchor::Around(4), 10, vec![(1, 2), (6, 6)], 1000),
            &[1; 8],
        );
        assert_eq!(seqs(&page), [1, 2, 6]);
        assert!(!page.more_before && !page.more_after);
        assert!(page.rows.iter().all(|row| row.idempotency_key.is_none()));
        let page = run(&query(Anchor::After(2), 10, vec![], 1000), &[1; 8]);
        assert!(page.rows.is_empty() && !page.more_before && !page.more_after);
    }
}
