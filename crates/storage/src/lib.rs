// SPDX-License-Identifier: EUPL-1.2
//! Asynchronous SQLx persistence. Only the storage worker may link this crate.

use nuschel_core::{ChatMessage, MAX_CONTENT_BYTES};
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

    /// A bounded page in display order. Never deduplicate messages by their text.
    pub async fn recent(
        &self,
        server: &str,
        channel: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, Error> {
        type Row = (String, String, String, i64, String);
        let limit = limit.min(MAX_PAGE_MESSAGES) as i64;
        let rows: Vec<Row> = match &self.pool {
            Pool::Sqlite(pool) => sqlx::query_as(
                "SELECT server, channel, author, observed_at_ms, body FROM messages WHERE server = ?1 AND channel = ?2 ORDER BY id DESC LIMIT ?3",
            ).bind(server).bind(channel).bind(limit).fetch_all(pool).await?,
            Pool::Postgres(pool) => sqlx::query_as(
                "SELECT server, channel, author, observed_at_ms, body FROM messages WHERE server = $1 AND channel = $2 ORDER BY id DESC LIMIT $3",
            ).bind(server).bind(channel).bind(limit).fetch_all(pool).await?,
        };
        let mut messages = Vec::new();
        let mut bytes = 0;
        for (server, channel, author, observed_at_ms, body) in rows {
            let size = server.len() + channel.len() + author.len() + body.len();
            if size > MAX_PAGE_TEXT_BYTES - bytes {
                break;
            }
            bytes += size;
            messages.push(ChatMessage {
                server,
                channel,
                author,
                observed_at_ms,
                body,
            });
        }
        messages.reverse();
        Ok(messages)
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
}
