// SPDX-License-Identifier: EUPL-1.2
//! Asynchronous SQLx persistence. Only the storage worker may link this crate.

use nuschel_core::{ChatMessage, HistoryEntry, HistoryPage, MAX_CONTENT_BYTES};
use sqlx::{
    PgPool, SqlitePool,
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, str::FromStr, time::Duration};
use thiserror::Error;

const MAX_PAGE_MESSAGES: usize = 100;
const MAX_PAGE_TEXT_BYTES: usize = 96 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("history message exceeds the storage limits")]
    InvalidMessage,
    #[error("expected a sqlite:, postgres:, or postgresql: database URL")]
    UnsupportedDatabase,
}

enum Pool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}
pub struct History {
    pool: Pool,
}

impl History {
    pub async fn open(path: &Path) -> Result<Self, Error> {
        Self::sqlite(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
    }

    /// The trusted supervisor supplies this URL; it is never accepted over worker IPC.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        if url.starts_with("sqlite:") {
            Self::sqlite(SqliteConnectOptions::from_str(url)?.create_if_missing(true)).await
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(Duration::from_secs(5))
                .connect(url)
                .await?;
            sqlx::migrate!("./migrations/postgres").run(&pool).await?;
            Ok(Self {
                pool: Pool::Postgres(pool),
            })
        } else {
            Err(Error::UnsupportedDatabase)
        }
    }

    async fn sqlite(options: SqliteConnectOptions) -> Result<Self, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.foreign_keys(true).pragma("trusted_schema", "OFF"))
            .await?;
        sqlx::migrate!("./migrations/sqlite").run(&pool).await?;
        Ok(Self {
            pool: Pool::Sqlite(pool),
        })
    }

    pub async fn append(&self, message: &ChatMessage) -> Result<i64, Error> {
        if message.body.len() > MAX_CONTENT_BYTES
            || [&message.server, &message.channel, &message.author]
                .iter()
                .any(|value| value.len() > 512)
        {
            return Err(Error::InvalidMessage);
        }
        let sqlite = "INSERT INTO messages (server, channel, author, observed_at_ms, body) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id";
        let postgres = "INSERT INTO messages (server, channel, author, observed_at_ms, body) VALUES ($1, $2, $3, $4, $5) RETURNING id";
        Ok(match &self.pool {
            Pool::Sqlite(pool) => {
                sqlx::query_scalar(sqlite)
                    .bind(&message.server)
                    .bind(&message.channel)
                    .bind(&message.author)
                    .bind(message.observed_at_ms)
                    .bind(&message.body)
                    .fetch_one(pool)
                    .await?
            }
            Pool::Postgres(pool) => {
                sqlx::query_scalar(postgres)
                    .bind(&message.server)
                    .bind(&message.channel)
                    .bind(&message.author)
                    .bind(message.observed_at_ms)
                    .bind(&message.body)
                    .fetch_one(pool)
                    .await?
            }
        })
    }

    /// The most recent bounded page, retained for existing local-history callers.
    pub async fn recent(
        &self,
        server: &str,
        channel: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, Error> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        Ok(self
            .page(server, channel, None, limit)
            .await?
            .messages
            .into_iter()
            .map(|entry| entry.message)
            .collect())
    }

    /// Walk the entire archive through bounded pages. The exclusive ID cursor is
    /// stable under concurrent appends; timestamps and message bodies are not IDs.
    /// A trusted session adapter must authorize the server/channel on every request.
    pub async fn page(
        &self,
        server: &str,
        channel: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<HistoryPage, Error> {
        type Row = (i64, String, String, String, i64, String);
        let limit = limit.clamp(1, MAX_PAGE_MESSAGES);
        let fetch_limit = (limit + 1) as i64;
        let rows: Vec<Row> = match &self.pool {
            Pool::Sqlite(pool) => sqlx::query_as(
                "SELECT id, server, channel, author, observed_at_ms, body FROM messages WHERE server = ?1 AND channel = ?2 AND (?3 IS NULL OR id < ?3) ORDER BY id DESC LIMIT ?4",
            ).bind(server).bind(channel).bind(before).bind(fetch_limit).fetch_all(pool).await?,
            Pool::Postgres(pool) => sqlx::query_as(
                "SELECT id, server, channel, author, observed_at_ms, body FROM messages WHERE server = $1 AND channel = $2 AND ($3::BIGINT IS NULL OR id < $3) ORDER BY id DESC LIMIT $4",
            ).bind(server).bind(channel).bind(before).bind(fetch_limit).fetch_all(pool).await?,
        };
        let fetched = rows.len();
        let mut messages = Vec::new();
        let mut bytes = 0;
        for (id, server, channel, author, observed_at_ms, body) in rows.into_iter().take(limit) {
            let size = server.len() + channel.len() + author.len() + body.len();
            if size > MAX_PAGE_TEXT_BYTES - bytes {
                break;
            }
            bytes += size;
            messages.push(HistoryEntry {
                id,
                message: ChatMessage {
                    server,
                    channel,
                    author,
                    observed_at_ms,
                    body,
                },
            });
        }
        let next_before = (fetched > messages.len())
            .then(|| messages.last().map(|entry| entry.id))
            .flatten();
        messages.reverse();
        Ok(HistoryPage {
            messages,
            next_before,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn history_preserves_multiline_messages_and_server_boundaries() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        let message = ChatMessage {
            server: "trusted-server-a".into(),
            channel: "general".into(),
            author: "someone".into(),
            observed_at_ms: 1,
            body: "first\n\n```rust\nfn main() {}\n```".into(),
        };
        history.append(&message).await?;
        history.append(&message).await?;
        assert_eq!(
            history.recent("trusted-server-a", "general", 10).await?,
            vec![message.clone(), message]
        );
        assert!(
            history
                .recent("trusted-server-b", "general", 10)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn archive_pagination_survives_appends_and_duplicate_timestamps() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        let mut message = ChatMessage {
            server: "one".into(),
            channel: "general".into(),
            author: "someone".into(),
            observed_at_ms: 1,
            body: "same message".into(),
        };
        let mut expected = Vec::new();
        for _ in 0..105 {
            expected.push(history.append(&message).await?);
        }
        let first = history.page("one", "general", None, 3).await?;
        let mut ids = first
            .messages
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        let mut before = first.next_before;
        let new_id = history.append(&message).await?;
        // Another channel and another server never leak into the archive.
        message.channel = "private".into();
        history.append(&message).await?;
        message.channel = "general".into();
        message.server = "two".into();
        history.append(&message).await?;
        while let Some(cursor) = before {
            let page = history.page("one", "general", Some(cursor), 17).await?;
            ids.extend(page.messages.iter().map(|entry| entry.id));
            before = page.next_before;
        }
        ids.sort_unstable();
        assert_eq!(ids, expected);
        assert!(!ids.contains(&new_id));
        Ok(())
    }

    #[tokio::test]
    async fn byte_bounded_pages_do_not_skip_large_messages() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        let message = ChatMessage {
            server: "one".into(),
            channel: "general".into(),
            author: "someone".into(),
            observed_at_ms: 0,
            body: "x".repeat(MAX_CONTENT_BYTES),
        };
        let oldest = history.append(&message).await?;
        let newest = history.append(&message).await?;
        let first = history.page("one", "general", None, 100).await?;
        assert_eq!(first.messages.len(), 1);
        assert_eq!(first.messages[0].id, newest);
        let last = history
            .page("one", "general", first.next_before, 100)
            .await?;
        assert_eq!(last.messages.len(), 1);
        assert_eq!(last.messages[0].id, oldest);
        assert_eq!(last.next_before, None);
        Ok(())
    }
}
