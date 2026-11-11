// SPDX-License-Identifier: EUPL-1.2
//! The Nuschel extension's history (docs/extension.md §3–§7): messages with
//! per-conversation sequence numbers, scoped atomic idempotency, edits that
//! replace content, tombstones, direct conversations, `since_access`
//! intervals and read markers. `nuscheld` assigns sequence numbers and checks
//! authorisation; this stores, and checks what only the rows can tell
//! (authorship, edit counts, earlier use of an idempotency key).
use super::{MAX_NAME, ServerStore, too_long};
use crate::BeginWrite as _;
use crate::{Error, Pool};
use nuschel_ipc::server::{
    HISTORY_LOAD_PAGE, MAX_RETENTION_DAYS, MessageKind, PageQuery, Retention, SeqRows,
    ServerStorageResponse as Response, StoredAccess, StoredDirect, StoredMessage, StoredPage,
    StoredRetention,
};

/// The rows retention and the operator's erase may tombstone, as an SQL
/// predicate: live messages. It must say what `nuschel_ipc::server::expirable`
/// says; that is **the hook for records the server must keep** (MLS commits
/// within their catch-up window, docs/e2ee.md §6.3, are excluded in both).
macro_rules! expirable {
    () => {
        "kind = 0 AND deleted_ms IS NULL AND format <> 4"
    };
}

/// Message text the worker stores, whatever the server sends.
const MAX_CONTENT: usize = 1024 * 1024;
const MAX_ID: usize = 64;
const MAX_READ_MARKERS: i64 = 100_000;

const ROWS_DESC: &str = "SELECT conversation, seq, kind, target, author_account, author_user_id, author_name, format, content, reply_to, sent_ms, edited_ms, edits, deleted_ms, delete_reason FROM history_messages WHERE conversation = $1 AND seq >= $2 AND seq <= $3 ORDER BY seq DESC LIMIT $4";
const ROWS_ASC: &str = "SELECT conversation, seq, kind, target, author_account, author_user_id, author_name, format, content, reply_to, sent_ms, edited_ms, edits, deleted_ms, delete_reason FROM history_messages WHERE conversation = $1 AND seq >= $2 AND seq <= $3 ORDER BY seq LIMIT $4";
const INSERT: &str = "INSERT INTO history_messages (conversation, seq, kind, target, author_account, author_user_id, author_name, format, content, reply_to, sent_ms, edited_ms, edits, deleted_ms, delete_reason, idempotency_key, body_hash) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)";

type Row = (
    String,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
    String,
    i64,
    String,
    Option<i64>,
    i64,
    Option<i64>,
    i64,
    Option<i64>,
    Option<i64>,
);

fn kind_number(kind: MessageKind) -> i64 {
    match kind {
        MessageKind::Message => 0,
        MessageKind::Edit => 1,
        MessageKind::Delete => 2,
        MessageKind::Void => 3,
    }
}

fn kind(number: i64) -> MessageKind {
    match number {
        1 => MessageKind::Edit,
        2 => MessageKind::Delete,
        3 => MessageKind::Void,
        _ => MessageKind::Message,
    }
}

fn signed(value: u64) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::InvalidMessage)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn small(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn message(row: Row) -> StoredMessage {
    let (
        conversation,
        seq,
        kind_,
        target,
        author_account,
        author_user_id,
        author_name,
        format,
        content,
        reply_to,
        sent_ms,
        edited_ms,
        edits,
        deleted_ms,
        delete_reason,
    ) = row;
    StoredMessage {
        conversation,
        seq: unsigned(seq),
        kind: kind(kind_),
        target: target.map(unsigned),
        author_account,
        author_user_id: author_user_id.map(small),
        author_name,
        format: u8::try_from(format).unwrap_or(0),
        content,
        reply_to: reply_to.map(unsigned),
        sent_ms,
        edited_ms,
        edits: small(edits),
        deleted_ms,
        delete_reason: delete_reason.map(small),
        idempotency_key: None,
        body_hash: None,
    }
}

fn check(message: &StoredMessage) -> Result<(), Error> {
    too_long(&message.conversation, MAX_ID)?;
    too_long(&message.author_name, MAX_NAME)?;
    too_long(&message.content, MAX_CONTENT)?;
    for value in [
        &message.author_account,
        &message.idempotency_key,
        &message.body_hash,
    ]
    .into_iter()
    .flatten()
    {
        too_long(value, MAX_ID)?;
    }
    Ok(())
}

/// Insert one row inside a transaction.
macro_rules! insert {
    ($transaction:expr, $message:expr) => {{
        let message: &StoredMessage = $message;
        check(message)?;
        sqlx::query(INSERT)
            .bind(&message.conversation)
            .bind(signed(message.seq)?)
            .bind(kind_number(message.kind))
            .bind(message.target.map(signed).transpose()?)
            .bind(&message.author_account)
            .bind(message.author_user_id.map(i64::from))
            .bind(&message.author_name)
            .bind(i64::from(message.format))
            .bind(&message.content)
            .bind(message.reply_to.map(signed).transpose()?)
            .bind(message.sent_ms)
            .bind(message.edited_ms)
            .bind(i64::from(message.edits))
            .bind(message.deleted_ms)
            .bind(message.delete_reason.map(i64::from))
            .bind(&message.idempotency_key)
            .bind(&message.body_hash)
            .execute(&mut *$transaction)
            .await?;
    }};
}

struct Rows<'a>(&'a ServerStore);

impl SeqRows for Rows<'_> {
    type Error = Error;

    async fn rows(
        &mut self,
        conversation: &str,
        (lo, hi): (u64, u64),
        descending: bool,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, Error> {
        let statement = if descending { ROWS_DESC } else { ROWS_ASC };
        let rows: Vec<Row> = with_pool!(&self.0.pool, |pool| {
            sqlx::query_as(statement)
                .bind(conversation)
                .bind(signed(lo.min(i64::MAX as u64))?)
                .bind(signed(hi.min(i64::MAX as u64))?)
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .fetch_all(pool)
                .await?
        });
        Ok(rows.into_iter().map(message).collect())
    }
}

impl ServerStore {
    pub async fn history_page(&self, query: &PageQuery) -> Result<StoredPage, Error> {
        too_long(&query.conversation, MAX_ID)?;
        nuschel_ipc::server::page(&mut Rows(self), query).await
    }

    /// Store a message. A key used before by the same account in the same
    /// conversation returns the original seq (same message) or `Conflict`
    /// (another one); the seq the server assigned then becomes a void.
    pub async fn history_append(&self, message: &StoredMessage) -> Result<Response, Error> {
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            if let (Some(account), Some(key)) = (&message.author_account, &message.idempotency_key)
            {
                let earlier: Option<(i64, Option<String>)> = sqlx::query_as(
                    "SELECT seq, body_hash FROM history_messages WHERE conversation = $1 AND author_account = $2 AND idempotency_key = $3",
                )
                .bind(&message.conversation)
                .bind(account)
                .bind(key)
                .fetch_optional(&mut *transaction)
                .await?;
                if let Some((seq, hash)) = earlier {
                    insert!(
                        transaction,
                        &StoredMessage::void(&message.conversation, message.seq, message.sent_ms)
                    );
                    transaction.commit().await?;
                    return Ok(if hash == message.body_hash {
                        Response::Appended {
                            seq: unsigned(seq),
                            duplicate: true,
                        }
                    } else {
                        Response::Conflict
                    });
                }
            }
            insert!(transaction, message);
            transaction.commit().await?;
            Ok(Response::Appended {
                seq: message.seq,
                duplicate: false,
            })
        })
    }

    /// Replace the content of the author's own message, at most `edit_limit`
    /// times; earlier versions are overwritten (`secure_delete` on SQLite).
    pub async fn history_edit(
        &self,
        target: u64,
        author_account: &str,
        format: u8,
        content: &str,
        edit_limit: u32,
        event: &StoredMessage,
    ) -> Result<Response, Error> {
        too_long(content, MAX_CONTENT)?;
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            let edits: Option<i64> = sqlx::query_scalar(
                "SELECT edits FROM history_messages WHERE conversation = $1 AND seq = $2 AND kind = 0 AND deleted_ms IS NULL AND author_account = $3",
            )
            .bind(&event.conversation)
            .bind(signed(target)?)
            .bind(author_account)
            .fetch_optional(&mut *transaction)
            .await?;
            let outcome = match edits {
                None => Response::Missing,
                Some(edits) if edits >= i64::from(edit_limit) => Response::LimitReached,
                Some(_) => Response::Done,
            };
            if !matches!(outcome, Response::Done) {
                insert!(
                    transaction,
                    &StoredMessage::void(&event.conversation, event.seq, event.sent_ms)
                );
                transaction.commit().await?;
                return Ok(outcome);
            }
            sqlx::query(
                // Never revive a row that expired or was erased meanwhile.
                "UPDATE history_messages SET content = $3, format = $4, edited_ms = $5, edits = edits + 1 WHERE conversation = $1 AND seq = $2 AND deleted_ms IS NULL",
            )
            .bind(&event.conversation)
            .bind(signed(target)?)
            .bind(content)
            .bind(i64::from(format))
            .bind(event.sent_ms)
            .execute(&mut *transaction)
            .await?;
            insert!(transaction, event);
            transaction.commit().await?;
            Ok(Response::Done)
        })
    }

    /// Make a message a tombstone: content and reply are dropped, seq stays.
    pub async fn history_delete(
        &self,
        target: u64,
        author_account: Option<&str>,
        reason: u32,
        event: &StoredMessage,
    ) -> Result<Response, Error> {
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            let found: Option<i64> = match author_account {
                Some(account) => sqlx::query_scalar(
                    "SELECT seq FROM history_messages WHERE conversation = $1 AND seq = $2 AND kind = 0 AND deleted_ms IS NULL AND author_account = $3",
                )
                .bind(&event.conversation)
                .bind(signed(target)?)
                .bind(account)
                .fetch_optional(&mut *transaction)
                .await?,
                None => sqlx::query_scalar(
                    "SELECT seq FROM history_messages WHERE conversation = $1 AND seq = $2 AND kind = 0 AND deleted_ms IS NULL",
                )
                .bind(&event.conversation)
                .bind(signed(target)?)
                .fetch_optional(&mut *transaction)
                .await?,
            };
            if found.is_none() {
                insert!(
                    transaction,
                    &StoredMessage::void(&event.conversation, event.seq, event.sent_ms)
                );
                transaction.commit().await?;
                return Ok(Response::Missing);
            }
            sqlx::query(
                "UPDATE history_messages SET content = '', reply_to = NULL, deleted_ms = $3, delete_reason = $4 WHERE conversation = $1 AND seq = $2",
            )
            .bind(&event.conversation)
            .bind(signed(target)?)
            .bind(event.sent_ms)
            .bind(i64::from(reason))
            .execute(&mut *transaction)
            .await?;
            insert!(transaction, event);
            transaction.commit().await?;
            Ok(Response::Done)
        })
    }

    pub async fn history_purge(&self, conversation: &str) -> Result<(), Error> {
        too_long(conversation, MAX_ID)?;
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            for statement in [
                "DELETE FROM history_messages WHERE conversation = $1",
                "DELETE FROM history_access WHERE conversation = $1",
                "DELETE FROM history_read_markers WHERE conversation = $1",
                "DELETE FROM history_retention WHERE conversation = $1",
                "DELETE FROM history_heads WHERE conversation = $1",
            ] {
                sqlx::query(statement)
                    .bind(conversation)
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
            Ok(())
        })
    }

    pub async fn history_create_direct(&self, direct: &StoredDirect) -> Result<(), Error> {
        for value in [&direct.conversation, &direct.account_a, &direct.account_b] {
            too_long(value, MAX_ID)?;
        }
        with_pool!(&self.pool, |pool| {
            sqlx::query(
                "INSERT INTO history_directs (conversation, account_a, account_b) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(&direct.conversation)
            .bind(&direct.account_a)
            .bind(&direct.account_b)
            .execute(pool)
            .await?;
            Ok(())
        })
    }

    pub async fn history_set_access(
        &self,
        conversation: &str,
        account: &str,
        intervals: &[(u64, Option<u64>)],
    ) -> Result<(), Error> {
        too_long(conversation, MAX_ID)?;
        too_long(account, MAX_ID)?;
        if intervals.len() > nuschel_ipc::server::MAX_RANGES {
            return Err(Error::InvalidMessage);
        }
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            sqlx::query("DELETE FROM history_access WHERE conversation = $1 AND account = $2")
                .bind(conversation)
                .bind(account)
                .execute(&mut *transaction)
                .await?;
            for (start, end) in intervals {
                sqlx::query(
                    "INSERT INTO history_access (conversation, account, start_seq, end_seq) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                )
                .bind(conversation)
                .bind(account)
                .bind(signed(*start)?)
                .bind(end.map(signed).transpose()?)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
            Ok(())
        })
    }

    pub async fn history_forget_account(&self, account: &str) -> Result<(), Error> {
        too_long(account, MAX_ID)?;
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            for statement in [
                "DELETE FROM history_access WHERE account = $1",
                "DELETE FROM history_read_markers WHERE account = $1",
            ] {
                sqlx::query(statement)
                    .bind(account)
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
            Ok(())
        })
    }

    pub async fn history_set_read_marker(
        &self,
        account: &str,
        conversation: &str,
        seq: u64,
    ) -> Result<(), Error> {
        too_long(account, MAX_ID)?;
        too_long(conversation, MAX_ID)?;
        with_pool!(&self.pool, |pool| {
            sqlx::query(
                "INSERT INTO history_read_markers (account, conversation, seq) VALUES ($1, $2, $3) \
                 ON CONFLICT (account, conversation) DO UPDATE SET seq = CASE WHEN excluded.seq > history_read_markers.seq THEN excluded.seq ELSE history_read_markers.seq END",
            )
            .bind(account)
            .bind(conversation)
            .bind(signed(seq)?)
            .execute(pool)
            .await?;
            Ok(())
        })
    }

    /// Tombstone every message by `account` (the operator's erase). MLS commits
    /// (format 4) stay: members need them to catch up (docs/e2ee.md §6.3).
    pub async fn history_erase(&self, account: &str, deleted_ms: i64) -> Result<u64, Error> {
        too_long(account, MAX_ID)?;
        let statement = concat!(
            "UPDATE history_messages SET content = '', reply_to = NULL, deleted_ms = $2, delete_reason = 3 WHERE ",
            expirable!(),
            " AND author_account = $1"
        );
        with_pool!(&self.pool, |pool| {
            Ok(sqlx::query(statement)
                .bind(account)
                .bind(deleted_ms)
                .execute(pool)
                .await?
                .rows_affected())
        })
    }

    /// Tombstone at most `limit` expirable messages of one conversation sent
    /// before `before_ms` (retention), oldest first. Content is overwritten in
    /// place (`secure_delete` on SQLite; PostgreSQL keeps old row versions until
    /// VACUUM, see docs/server.md).
    pub async fn history_expire(
        &self,
        conversation: &str,
        before_ms: i64,
        deleted_ms: i64,
        limit: u32,
    ) -> Result<u64, Error> {
        too_long(conversation, MAX_ID)?;
        let statement = concat!(
            "UPDATE history_messages SET content = '', reply_to = NULL, deleted_ms = $3, delete_reason = 2 \
             WHERE conversation = $1 AND seq IN (SELECT seq FROM history_messages WHERE conversation = $4 AND ",
            expirable!(),
            " AND sent_ms < $2 ORDER BY seq LIMIT $5)"
        );
        with_pool!(&self.pool, |pool| {
            Ok(sqlx::query(statement)
                .bind(conversation)
                .bind(before_ms)
                .bind(deleted_ms)
                .bind(conversation)
                .bind(i64::from(limit))
                .execute(pool)
                .await?
                .rows_affected())
        })
    }

    /// Expirable messages of one conversation sent before `before_ms`.
    pub async fn history_count_expirable(
        &self,
        conversation: &str,
        before_ms: i64,
    ) -> Result<u64, Error> {
        too_long(conversation, MAX_ID)?;
        let statement = concat!(
            "SELECT COUNT(*) FROM history_messages WHERE conversation = $1 AND ",
            expirable!(),
            " AND sent_ms < $2"
        );
        let count: i64 = with_pool!(&self.pool, |pool| {
            sqlx::query_scalar(statement)
                .bind(conversation)
                .bind(before_ms)
                .fetch_one(pool)
                .await?
        });
        Ok(unsigned(count))
    }

    /// Set or clear (`None`: the server default) a conversation's retention.
    pub async fn history_set_retention(
        &self,
        conversation: &str,
        retention: Option<Retention>,
    ) -> Result<(), Error> {
        too_long(conversation, MAX_ID)?;
        let row = retention.map(|retention| match retention {
            Retention::Forever => ("forever", None),
            Retention::Days(days) => ("days", Some(i64::from(days.clamp(1, MAX_RETENTION_DAYS)))),
            Retention::None => ("none", None),
        });
        with_pool!(&self.pool, |pool| {
            match row {
                None => {
                    sqlx::query("DELETE FROM history_retention WHERE conversation = $1")
                        .bind(conversation)
                        .execute(pool)
                        .await?;
                }
                Some((mode, days)) => {
                    sqlx::query(
                        "INSERT INTO history_retention (conversation, mode, days) VALUES ($1, $2, $3) \
                         ON CONFLICT (conversation) DO UPDATE SET mode = excluded.mode, days = excluded.days",
                    )
                    .bind(conversation)
                    .bind(mode)
                    .bind(days)
                    .execute(pool)
                    .await?;
                }
            }
            Ok(())
        })
    }

    pub async fn history_retentions(
        &self,
        after: Option<&str>,
    ) -> Result<Vec<StoredRetention>, Error> {
        let rows: Vec<(String, String, Option<i64>)> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(
                "SELECT conversation, mode, days FROM history_retention WHERE conversation > $1 ORDER BY conversation LIMIT $2",
            )
            .bind(after.unwrap_or(""))
            .bind(i64::from(HISTORY_LOAD_PAGE))
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .map(|(conversation, mode, days)| StoredRetention {
                conversation,
                retention: match (mode.as_str(), days) {
                    ("none", _) => Retention::None,
                    ("days", Some(days)) => Retention::Days(
                        u32::try_from(days.clamp(1, i64::from(MAX_RETENTION_DAYS))).unwrap_or(1),
                    ),
                    // Anything unreadable keeps messages rather than losing them.
                    _ => Retention::Forever,
                },
            })
            .collect())
    }

    /// Raise a live-only conversation's recorded head.
    pub async fn history_advance(&self, conversation: &str, seq: u64) -> Result<(), Error> {
        too_long(conversation, MAX_ID)?;
        with_pool!(&self.pool, |pool| {
            sqlx::query(
                "INSERT INTO history_heads (conversation, head) VALUES ($1, $2) \
                 ON CONFLICT (conversation) DO UPDATE SET head = CASE WHEN excluded.head > history_heads.head THEN excluded.head ELSE history_heads.head END",
            )
            .bind(conversation)
            .bind(signed(seq)?)
            .execute(pool)
            .await?;
            Ok(())
        })
    }

    pub async fn history_heads(&self, after: Option<&str>) -> Result<Vec<(String, u64)>, Error> {
        let rows: Vec<(String, i64)> = with_pool!(&self.pool, |pool| {
            // Stored rows, and the heads of live-only conversations.
            sqlx::query_as(
                "SELECT conversation, MAX(seq) FROM (SELECT conversation, seq FROM history_messages \
                 UNION ALL SELECT conversation, head AS seq FROM history_heads) AS heads \
                 WHERE conversation > $1 GROUP BY conversation ORDER BY conversation LIMIT $2",
            )
            .bind(after.unwrap_or(""))
            .bind(i64::from(HISTORY_LOAD_PAGE))
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .map(|(conversation, head)| (conversation, unsigned(head)))
            .collect())
    }

    pub async fn history_directs(&self, after: Option<&str>) -> Result<Vec<StoredDirect>, Error> {
        let rows: Vec<(String, String, String)> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(
                "SELECT conversation, account_a, account_b FROM history_directs WHERE conversation > $1 ORDER BY conversation LIMIT $2",
            )
            .bind(after.unwrap_or(""))
            .bind(i64::from(HISTORY_LOAD_PAGE))
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .map(|(conversation, account_a, account_b)| StoredDirect {
                conversation,
                account_a,
                account_b,
            })
            .collect())
    }

    pub async fn history_access(
        &self,
        after: Option<&(String, String, u64)>,
    ) -> Result<Vec<StoredAccess>, Error> {
        let (conversation, account, start) = match after {
            Some((conversation, account, start)) => {
                (conversation.as_str(), account.as_str(), signed(*start)?)
            }
            None => ("", "", -1),
        };
        let rows: Vec<(String, String, i64, Option<i64>)> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(
                "SELECT conversation, account, start_seq, end_seq FROM history_access \
                 WHERE (conversation, account, start_seq) > ($1, $2, $3) ORDER BY conversation, account, start_seq LIMIT $4",
            )
            .bind(conversation)
            .bind(account)
            .bind(start)
            .bind(i64::from(HISTORY_LOAD_PAGE))
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .map(|(conversation, account, start, end)| StoredAccess {
                conversation,
                account,
                start: unsigned(start),
                end: end.map(unsigned),
            })
            .collect())
    }

    pub async fn history_read_markers(&self, account: &str) -> Result<Vec<(String, u64)>, Error> {
        too_long(account, MAX_ID)?;
        let rows: Vec<(String, i64)> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(
                "SELECT conversation, seq FROM history_read_markers WHERE account = $1 ORDER BY conversation LIMIT $2",
            )
            .bind(account)
            .bind(MAX_READ_MARKERS)
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .map(|(conversation, seq)| (conversation, unsigned(seq)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschel_ipc::server::Anchor;

    fn row(seq: u64, content: &str, key: Option<&str>) -> StoredMessage {
        StoredMessage {
            conversation: "c".repeat(32),
            seq,
            author_account: Some("a".repeat(32)),
            author_user_id: Some(1),
            author_name: "Alice".into(),
            format: 1,
            content: content.into(),
            sent_ms: 1000 + seq as i64,
            idempotency_key: key.map(str::to_owned),
            body_hash: key.map(|_| format!("hash of {content}")),
            ..StoredMessage::default()
        }
    }

    fn query(anchor: Anchor, limit: u32, ranges: Vec<(u64, u64)>) -> PageQuery {
        PageQuery {
            conversation: "c".repeat(32),
            anchor,
            limit,
            ranges,
            max_bytes: 1024,
        }
    }

    #[tokio::test]
    async fn history_pages_idempotency_edits_and_tombstones() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(|_| Error::InvalidMessage)?;
        let store = ServerStore::open(&directory.path().join("server.sqlite3")).await?;
        for seq in 1..=5 {
            let response = store
                .history_append(&row(seq, &format!("m{seq}"), Some(&format!("{seq:032}"))))
                .await?;
            assert!(matches!(
                response,
                Response::Appended {
                    duplicate: false,
                    ..
                }
            ));
        }
        // A retry with the same key and body returns the first seq, and the
        // seq assigned for the retry becomes a void; another body conflicts.
        let mut retry = row(6, "m2", Some(&format!("{:032}", 2)));
        assert!(matches!(
            store.history_append(&retry).await?,
            Response::Appended {
                seq: 2,
                duplicate: true
            }
        ));
        retry.seq = 7;
        retry.content = "different".into();
        retry.body_hash = Some("other".into());
        assert!(matches!(
            store.history_append(&retry).await?,
            Response::Conflict
        ));
        let page = store
            .history_page(&query(Anchor::Latest, 3, vec![(1, u64::MAX)]))
            .await?;
        assert_eq!(
            page.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [5, 6, 7]
        );
        assert!(page.more_before && !page.more_after);
        assert_eq!(page.rows[1].kind, MessageKind::Void);
        assert!(page.rows.iter().all(|row| row.idempotency_key.is_none()));
        // Clipped to ranges: around 3 within [2, 4] only.
        let page = store
            .history_page(&query(Anchor::Around(3), 10, vec![(2, 4)]))
            .await?;
        assert_eq!(
            page.rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert!(!page.more_before && !page.more_after);
        // Edits by the author only, up to the limit; content is replaced.
        let edit = StoredMessage {
            seq: 8,
            kind: MessageKind::Edit,
            target: Some(1),
            ..row(8, "", None)
        };
        let other = "b".repeat(32);
        assert!(matches!(
            store.history_edit(1, &other, 1, "x", 5, &edit).await?,
            Response::Missing
        ));
        let edit = StoredMessage { seq: 9, ..edit };
        assert!(matches!(
            store
                .history_edit(1, &"a".repeat(32), 1, "edited", 1, &edit)
                .await?,
            Response::Done
        ));
        let edit = StoredMessage { seq: 10, ..edit };
        assert!(matches!(
            store
                .history_edit(1, &"a".repeat(32), 1, "again", 1, &edit)
                .await?,
            Response::LimitReached
        ));
        // A moderator deletion keeps the seq and drops the content.
        let delete = StoredMessage {
            seq: 11,
            kind: MessageKind::Delete,
            target: Some(1),
            ..row(11, "", None)
        };
        assert!(matches!(
            store.history_delete(1, None, 1, &delete).await?,
            Response::Done
        ));
        let page = store
            .history_page(&query(Anchor::After(0), 2, vec![(1, u64::MAX)]))
            .await?;
        assert_eq!(page.rows[0].content, "");
        assert_eq!(page.rows[0].edits, 1);
        assert_eq!(page.rows[0].delete_reason, Some(1));
        assert!(page.rows[0].deleted_ms.is_some());
        assert_eq!(store.history_heads(None).await?, [("c".repeat(32), 11)]);
        // Read markers only go up.
        let account = "a".repeat(32);
        store
            .history_set_read_marker(&account, &"c".repeat(32), 5)
            .await?;
        store
            .history_set_read_marker(&account, &"c".repeat(32), 3)
            .await?;
        assert_eq!(
            store.history_read_markers(&account).await?,
            [("c".repeat(32), 5)]
        );
        store
            .history_set_access(&"c".repeat(32), &account, &[(1, Some(3)), (6, None)])
            .await?;
        assert_eq!(store.history_access(None).await?.len(), 2);
        assert_eq!(store.history_erase(&account, 5).await?, 4);
        store.history_purge(&"c".repeat(32)).await?;
        assert!(store.history_heads(None).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn retention_expires_in_batches_and_keeps_live_only_heads() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(|_| Error::InvalidMessage)?;
        let store = ServerStore::open(&directory.path().join("server.sqlite3")).await?;
        let conversation = "c".repeat(32);
        let other = "d".repeat(32);
        for seq in 1..=5 {
            store
                .history_append(&row(seq, &format!("m{seq}"), None))
                .await?;
        }
        store
            .history_append(&StoredMessage {
                conversation: other.clone(),
                ..row(1, "elsewhere", None)
            })
            .await?;
        // Rows 1..=3 were sent before 1004; only expirable ones count.
        assert_eq!(store.history_count_expirable(&conversation, 1004).await?, 3);
        assert_eq!(store.history_expire(&conversation, 1004, 9, 2).await?, 2);
        assert_eq!(store.history_expire(&conversation, 1004, 9, 2).await?, 1);
        assert_eq!(store.history_expire(&conversation, 1004, 9, 2).await?, 0);
        let page = store
            .history_page(&query(Anchor::After(0), 10, vec![(1, u64::MAX)]))
            .await?;
        let expired: Vec<_> = page
            .rows
            .iter()
            .map(|row| (row.content.as_str(), row.delete_reason))
            .collect();
        assert_eq!(
            expired,
            [
                ("", Some(2)),
                ("", Some(2)),
                ("", Some(2)),
                ("m4", None),
                ("m5", None)
            ]
        );
        // An expired message cannot be edited back to life.
        let edit = StoredMessage {
            seq: 6,
            kind: MessageKind::Edit,
            target: Some(1),
            ..row(6, "", None)
        };
        assert!(matches!(
            store
                .history_edit(1, &"a".repeat(32), 1, "revived", 5, &edit)
                .await?,
            Response::Missing
        ));
        // Other conversations are untouched.
        assert_eq!(store.history_count_expirable(&other, i64::MAX).await?, 1);
        // Retention per conversation, set, replaced and cleared.
        store
            .history_set_retention(&conversation, Some(Retention::Days(7)))
            .await?;
        store
            .history_set_retention(&other, Some(Retention::None))
            .await?;
        store
            .history_set_retention(&conversation, Some(Retention::Days(30)))
            .await?;
        assert_eq!(
            store.history_retentions(None).await?,
            [
                StoredRetention {
                    conversation: conversation.clone(),
                    retention: Retention::Days(30)
                },
                StoredRetention {
                    conversation: other.clone(),
                    retention: Retention::None
                },
            ]
        );
        store.history_set_retention(&conversation, None).await?;
        assert_eq!(store.history_retentions(None).await?.len(), 1);
        // A live-only conversation's head counts, and only goes up.
        let live = "e".repeat(32);
        store.history_advance(&live, 9).await?;
        store.history_advance(&live, 4).await?;
        store.history_advance(&other, 3).await?;
        assert_eq!(
            store.history_heads(None).await?,
            [
                (conversation.clone(), 6),
                (other.clone(), 3),
                (live.clone(), 9)
            ]
        );
        store.history_purge(&other).await?;
        assert_eq!(store.history_retentions(None).await?.len(), 0);
        assert_eq!(store.history_heads(None).await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn mls_commits_survive_retention_and_the_erase() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(|_| Error::InvalidMessage)?;
        let store = ServerStore::open(&directory.path().join("server.sqlite3")).await?;
        let conversation = "c".repeat(32);
        for seq in 1..=4 {
            let mut message = row(seq, "x", None);
            // Rows 1 and 3 are commits (MLS_HANDSHAKE), 2 and 4 application data.
            message.format = if seq % 2 == 1 { 4 } else { 3 };
            store.history_append(&message).await?;
        }
        assert_eq!(
            store
                .history_count_expirable(&conversation, i64::MAX)
                .await?,
            2
        );
        assert_eq!(
            store.history_expire(&conversation, i64::MAX, 7, 10).await?,
            2
        );
        assert_eq!(store.history_erase(&"a".repeat(32), 8).await?, 0);
        let page = store
            .history_page(&query(Anchor::After(0), 10, vec![(1, u64::MAX)]))
            .await?;
        let kept: Vec<u64> = page
            .rows
            .iter()
            .filter(|row| row.deleted_ms.is_none())
            .map(|row| row.seq)
            .collect();
        assert_eq!(kept, [1, 3]);
        Ok(())
    }
}
