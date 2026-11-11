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
/// Text in one history page or search reply; the IPC frame limit is 4 MiB.
const MAX_PAGE_TEXT_BYTES: usize = 96 * 1024;
/// Messages per transaction when filling search text for an older database.
const BACKFILL_BATCH: i64 = 64;

const PAGE: &str = "SELECT id, server, channel, author, observed_at_ms, body FROM messages WHERE server = $1 AND channel = $2 AND id < $3 ORDER BY id DESC LIMIT $4";
const SEARCH: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body FROM message_search s JOIN messages m ON m.id = s.id WHERE s.server = $1 AND (s.visible LIKE $2 ESCAPE '\' OR s.author LIKE $2 ESCAPE '\') ORDER BY s.id DESC LIMIT $3";
const INSERT_SEARCH: &str =
    "INSERT INTO message_search (id, server, author, visible) VALUES ($1, $2, $3, $4)";

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

/// Run the same statements on either backend; both accept `$N` placeholders.
macro_rules! with_pool {
    ($pool:expr, |$name:ident| $body:expr) => {
        match $pool {
            Pool::Sqlite($name) => $body,
            Pool::Postgres($name) => $body,
        }
    };
}

/// What search matches and returns: the text without inline image data.
fn visible(body: &str) -> String {
    body.split(nuschel_core::EMBED)
        .step_by(2)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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
            Self::ready(Pool::Postgres(pool)).await
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
        Self::ready(Pool::Sqlite(pool)).await
    }

    async fn ready(pool: Pool) -> Result<Self, Error> {
        let history = Self { pool };
        while history.backfill_search().await? {}
        Ok(history)
    }

    /// Fill search text for one batch of messages stored before it existed,
    /// newest first; returns whether there was any. Appends write both rows in
    /// one transaction and IDs only grow, so every message at or above the
    /// smallest indexed ID has its row: an interrupted fill resumes below it,
    /// and a finished one costs two index lookups.
    async fn backfill_search(&self) -> Result<bool, Error> {
        type Row = (i64, String, String, String);
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let rows: Vec<Row> = sqlx::query_as(
                "SELECT id, server, author, body FROM messages WHERE id < (SELECT COALESCE(MIN(id), $1) FROM message_search) ORDER BY id DESC LIMIT $2",
            )
            .bind(i64::MAX)
            .bind(BACKFILL_BATCH)
            .fetch_all(&mut *transaction)
            .await?;
            let found = !rows.is_empty();
            for (id, server, author, body) in rows {
                sqlx::query(INSERT_SEARCH)
                    .bind(id)
                    .bind(server)
                    .bind(author.to_lowercase())
                    .bind(visible(&body).to_lowercase())
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
            Ok(found)
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
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO messages (server, channel, author, observed_at_ms, body) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            )
            .bind(&message.server)
            .bind(&message.channel)
            .bind(&message.author)
            .bind(message.observed_at_ms)
            .bind(&message.body)
            .fetch_one(&mut *transaction)
            .await?;
            sqlx::query(INSERT_SEARCH)
                .bind(id)
                .bind(&message.server)
                .bind(message.author.to_lowercase())
                .bind(visible(&message.body).to_lowercase())
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            Ok(id)
        })
    }

    /// Case-insensitive substring search of the visible text and author within
    /// one server's history, newest first. The query is bound as a parameter
    /// with LIKE wildcards escaped; no caller-provided SQL is executed. Inline
    /// image data is neither matched nor returned: each result's body is its
    /// visible text, and the results stay within the page byte budget.
    pub async fn search(
        &self,
        server: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, Error> {
        let query = query.trim();
        if query.is_empty() || query.len() > 256 || server.len() > 512 {
            return Ok(Vec::new());
        }
        // Both sides are lowercased here, so matching is Unicode-aware and the
        // same on SQLite (whose LIKE folds ASCII only) and PostgreSQL.
        let pattern = format!(
            "%{}%",
            query
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let limit = limit.clamp(1, MAX_PAGE_MESSAGES) as i64;
        type Row = (i64, String, String, String, i64, String);
        let rows: Vec<Row> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(SEARCH)
                .bind(server)
                .bind(&pattern)
                .bind(limit)
                .fetch_all(pool)
                .await?
        });
        let mut results = Vec::new();
        let mut bytes = 0;
        for (id, server, channel, author, observed_at_ms, body) in rows {
            let body = visible(&body);
            let size = server.len() + channel.len() + author.len() + body.len();
            if !results.is_empty() && size > MAX_PAGE_TEXT_BYTES.saturating_sub(bytes) {
                break;
            }
            bytes += size;
            results.push(HistoryEntry {
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
        Ok(results)
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
        // A plain bound, not `$3 IS NULL OR id < $3`, lets the index seek to it.
        let before = before.unwrap_or(i64::MAX);
        let rows: Vec<Row> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(PAGE)
                .bind(server)
                .bind(channel)
                .bind(before)
                .bind(fetch_limit)
                .fetch_all(pool)
                .await?
        });
        let fetched = rows.len();
        let mut messages = Vec::new();
        let mut bytes = 0;
        for (id, server, channel, author, observed_at_ms, body) in rows.into_iter().take(limit) {
            let size = server.len() + channel.len() + author.len() + body.len();
            // A page always makes progress: the first message is returned even
            // when it alone exceeds the byte budget.
            if !messages.is_empty() && size > MAX_PAGE_TEXT_BYTES.saturating_sub(bytes) {
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

    fn message(server: &str, author: &str, body: &str) -> ChatMessage {
        ChatMessage {
            server: server.into(),
            channel: "general".into(),
            author: author.into(),
            observed_at_ms: 0,
            body: body.into(),
        }
    }

    fn image(payload: &str) -> String {
        let embed = nuschel_core::EMBED;
        format!("{embed}data:image/png;base64,{payload}{embed}")
    }

    #[tokio::test]
    async fn page_and_search_queries_seek_their_indexes() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        let Pool::Sqlite(pool) = &history.pool else {
            unreachable!()
        };
        // The old page query, `(?3 IS NULL OR id < ?3)`, sought only the
        // server and channel, then filtered every newer message by ID.
        for (query, seek) in [
            (
                PAGE,
                "INDEX messages_channel (server=? AND channel=? AND id<?)",
            ),
            (SEARCH, "INDEX message_search_server (server=?)"),
        ] {
            let plan: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {query}")))
                    .bind("server")
                    .bind("general")
                    .bind(i64::MAX)
                    .bind(1_i64)
                    .fetch_all(pool)
                    .await?;
            let plan: Vec<_> = plan.into_iter().map(|row| row.3).collect();
            assert!(
                plan[0].starts_with("SEARCH") && plan[0].ends_with(seek),
                "{plan:?}"
            );
            // Rows come out of the index in order: no full scan, no sort.
            assert!(
                plan.iter()
                    .all(|step| !step.starts_with("SCAN") && !step.contains("TEMP B-TREE")),
                "{plan:?}"
            );
        }
        Ok(())
    }

    async fn image_data_cannot_crowd_out_search_results(history: &History) -> Result<(), Error> {
        history
            .append(&message("crowd", "ada", "Ärger mit dem Server"))
            .await?;
        // More newer messages than one search returns match only in image data.
        for _ in 0..150 {
            history
                .append(&message("crowd", "bob", &image("ärgerärger")))
                .await?;
        }
        let found = history.search("crowd", "ÄRGER", 100).await?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message.body, "Ärger mit dem Server");
        assert_eq!(history.search("crowd", "ADA", 100).await?.len(), 1);
        Ok(())
    }

    async fn search_results_stay_within_the_byte_budget(history: &History) -> Result<(), Error> {
        let text = format!("needle {}", "x".repeat(40 * 1024));
        for _ in 0..10 {
            let body = format!("{text}{}", image(&"A".repeat(100 * 1024)));
            history.append(&message("budget", "ada", &body)).await?;
        }
        let found = history.search("budget", "needle", 100).await?;
        // Images are left out of the bodies; two 40 KiB texts fit in 96 KiB.
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|entry| entry.message.body == text));
        Ok(())
    }

    /// Messages in a database at the first schema version, as a search-text
    /// fill interrupted after its first batch left it. Returns their number.
    async fn older_database(pool: &Pool) -> Result<i64, Error> {
        match pool {
            Pool::Sqlite(pool) => {
                sqlx::migrate!("./migrations/sqlite")
                    .run_to(1, pool)
                    .await?
            }
            Pool::Postgres(pool) => {
                sqlx::migrate!("./migrations/postgres")
                    .run_to(1, pool)
                    .await?
            }
        }
        let count = 2 * BACKFILL_BATCH + 5;
        for index in 0..count {
            let body = if index == 0 {
                "the oldest Needle".to_string()
            } else {
                format!("message {index}{}", image("needle"))
            };
            with_pool!(pool, |pool| {
                sqlx::query("INSERT INTO messages (server, channel, author, observed_at_ms, body) VALUES ('old', 'general', 'ada', 0, $1)")
                    .bind(body)
                    .execute(pool)
                    .await?;
            });
        }
        match pool {
            Pool::Sqlite(pool) => {
                sqlx::migrate!("./migrations/sqlite")
                    .run_to(2, pool)
                    .await?
            }
            Pool::Postgres(pool) => {
                sqlx::migrate!("./migrations/postgres")
                    .run_to(2, pool)
                    .await?
            }
        }
        with_pool!(pool, |pool| {
            let newest: Vec<(i64, String)> =
                sqlx::query_as("SELECT id, body FROM messages ORDER BY id DESC LIMIT $1")
                    .bind(BACKFILL_BATCH)
                    .fetch_all(pool)
                    .await?;
            for (id, body) in newest {
                sqlx::query(INSERT_SEARCH)
                    .bind(id)
                    .bind("old")
                    .bind("ada")
                    .bind(visible(&body).to_lowercase())
                    .execute(pool)
                    .await?;
            }
        });
        Ok(count)
    }

    async fn an_older_database_gains_search_text(
        history: &History,
        count: i64,
    ) -> Result<(), Error> {
        let rows: i64 = with_pool!(&history.pool, |pool| {
            sqlx::query_scalar("SELECT COUNT(*) FROM message_search")
                .fetch_one(pool)
                .await?
        });
        assert_eq!(rows, count);
        let found = history.search("old", "needle", 100).await?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message.body, "the oldest Needle");
        // New messages are searchable at once; reopening finds nothing to fill.
        history
            .append(&message("old", "ada", "fresh needle"))
            .await?;
        assert_eq!(history.search("old", "needle", 100).await?.len(), 2);
        assert!(!history.backfill_search().await?);
        Ok(())
    }

    #[tokio::test]
    async fn search_on_sqlite() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(sqlx::Error::Io)?;
        let path = directory.path().join("history.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await?;
        let count = older_database(&Pool::Sqlite(pool.clone())).await?;
        pool.close().await;
        let history = History::open(&path).await?;
        an_older_database_gains_search_text(&history, count).await?;
        image_data_cannot_crowd_out_search_results(&history).await?;
        search_results_stay_within_the_byte_budget(&history).await
    }

    /// `NUSCHEL_TEST_POSTGRES_URL=postgresql://... cargo test -p nuschel-storage
    /// -- --ignored` runs the same checks on PostgreSQL. The database must be a
    /// throwaway one: its history tables are dropped first.
    #[tokio::test]
    #[ignore = "needs a throwaway PostgreSQL database"]
    async fn search_on_postgres() -> Result<(), Error> {
        let url = std::env::var("NUSCHEL_TEST_POSTGRES_URL").expect("NUSCHEL_TEST_POSTGRES_URL");
        let pool = PgPool::connect(&url).await?;
        sqlx::query("DROP TABLE IF EXISTS message_search, messages, _sqlx_migrations")
            .execute(&pool)
            .await?;
        let count = older_database(&Pool::Postgres(pool.clone())).await?;
        pool.close().await;
        let history = History::connect(&url).await?;
        an_older_database_gains_search_text(&history, count).await?;
        image_data_cannot_crowd_out_search_results(&history).await?;
        search_results_stay_within_the_byte_budget(&history).await?;
        let page = history.page("crowd", "general", None, 100).await?;
        let older = history
            .page("crowd", "general", page.next_before, 100)
            .await?;
        assert_eq!(page.messages.len() + older.messages.len(), 151);
        assert_eq!(older.next_before, None);
        Ok(())
    }

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
    async fn search_is_scoped_case_insensitive_and_literal() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        for (server, channel, body) in [
            ("one", "general", "Deploy the Rust server"),
            ("one", "dev", "100% done_now"),
            ("two", "general", "rust elsewhere"),
            (
                "one",
                "general",
                "\u{FFFC}data:image/png;base64,rustrust\u{FFFC}",
            ),
        ] {
            history
                .append(&ChatMessage {
                    server: server.into(),
                    channel: channel.into(),
                    author: "someone".into(),
                    observed_at_ms: 0,
                    body: body.into(),
                })
                .await?;
        }
        let found = history.search("one", "RUST", 10).await?;
        assert_eq!(
            found.len(),
            1,
            "scoped to the server, case-insensitive, ignores image data"
        );
        assert_eq!(
            history.search("one", "%", 10).await?.len(),
            1,
            "wildcards are literal"
        );
        assert_eq!(history.search("one", "_", 10).await?.len(), 1);
        assert!(history.search("one", "  ", 10).await?.is_empty());
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
