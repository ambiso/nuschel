// SPDX-License-Identifier: EUPL-1.2
//! Asynchronous SQLx persistence. Only the storage worker may link this crate.

use nuschl_core::search::{MAX_QUERY_BYTES, Query};
use nuschl_core::{
    ChatMessage, HistoryEntry, HistoryPage, MAX_CONTENT_BYTES, SearchCursor, SearchPage,
    ServerRecord,
};
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
/// Messages per transaction when building the full-text index of an older
/// database; the text moves inside the database only.
const INDEX_BATCH: i64 = 512;
/// Servers one search may cover.
pub const MAX_SEARCH_SERVERS: usize = 64;
/// Records one `store_server` call may write (a server's page has at most 100).
pub const MAX_STORE_RECORDS: usize = 128;
/// SQLite's page cache for a history connection, in KiB. Its default, about
/// 2 MiB, ends up holding most of a long history once it has been scrolled or
/// searched, as private memory of the worker; pages beyond this come from the
/// OS's file cache instead (see `History::sqlite`).
const SQLITE_CACHE_KIB: i64 = 512;

/// A message row: id, server, channel, author, time, body, and a Nuschl
/// server's seq, edit and deletion times.
type Row = (
    i64,
    String,
    String,
    String,
    i64,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);
/// A search hit: a row's message, its score, and its seq (Nuschl servers),
/// by which a server-history timeline finds it.
type RankedRow = (i64, String, String, String, i64, String, i64, Option<i64>);

// Pages run in time order, then by row ID: a server's history page fetched
// later stores older messages under higher IDs than live ones stored before.
// $3 and $4 are the cursor row's time and ID; a row-value bound lets the
// index seek to it.
const PAGE: &str = "SELECT id, server, channel, author, observed_at_ms, body, seq, edited_ms, deleted_ms FROM messages WHERE server = $1 AND channel = $2 AND (observed_at_ms, id) < ($3, $4) ORDER BY observed_at_ms DESC, id DESC LIMIT $5";
const PAGE_AFTER: &str = "SELECT id, server, channel, author, observed_at_ms, body, seq, edited_ms, deleted_ms FROM messages WHERE server = $1 AND channel = $2 AND (observed_at_ms, id) > ($3, $4) ORDER BY observed_at_ms, id LIMIT $5";
const INSERT_SEARCH: &str =
    "INSERT INTO message_search (id, server, author, visible) VALUES ($1, $2, $3, $4)";
const INSERT_MESSAGE: &str = "INSERT INTO messages (server, channel, author, observed_at_ms, body, seq, edited_ms, deleted_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id";
/// Take a row out of SQLite's full-text index, with the text it was indexed with.
const FTS_DELETE: &str =
    "INSERT INTO message_fts (message_fts, rowid, visible) VALUES ('delete', $1, $2)";
const FTS_INSERT: &str = "INSERT INTO message_fts (rowid, visible) VALUES ($1, $2)";
const RESEARCH_POSTGRES: &str = "UPDATE message_search SET visible = $2, author = $3, document = CASE WHEN document IS NULL THEN NULL ELSE to_tsvector('simple', $2::text) END WHERE id = $1";
/// Add the rows of message_search with `$1 <= id < $2` to the full-text index.
const INDEX_SQLITE: &str = "INSERT INTO message_fts (rowid, visible) SELECT id, visible FROM message_search WHERE id >= $1 AND id < $2";
const INDEX_POSTGRES: &str = "UPDATE message_search SET document = to_tsvector('simple', visible) WHERE id >= $1 AND id < $2";

// Full-text search. Each match is scored from itself alone, so a score stays
// put while messages arrive and the cursor of the next page compares exactly
// (as an integer): relevance fades with age, a month-old message needing
// twice the relevance of today's. PostgreSQL's ts_rank weighs how often and
// how closely the words occur against the message's length; SQLite's bm25
// would depend on every other message, so there the relevance is how much
// of the message the match is. Only the page's rows are joined back to their
// bodies. Parameters: $1 the full-text query, $2 the time scores are for, $3
// the servers, $4 the channel or NULL, $5 an author LIKE pattern, $6 and $7
// the cursor's score and ID, $8 the limit.
const SEARCH_SQLITE: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, page.score, m.seq FROM (
    SELECT id, score FROM (
        SELECT s.id AS id, CAST(1000000000.0 / (1.0 + length(s.visible) / 1000.0) / (1.0 + MAX(0, $2 - m.observed_at_ms) / 2592000000.0) AS INTEGER) AS score
        FROM message_fts JOIN message_search s ON s.id = message_fts.rowid JOIN messages m ON m.id = s.id
        WHERE message_fts MATCH $1 AND s.server IN (SELECT value FROM json_each($3))
            AND ($4 IS NULL OR m.channel = $4) AND s.author LIKE $5 ESCAPE '\'
    ) WHERE score < $6 OR (score = $6 AND id < $7) ORDER BY score DESC, id DESC LIMIT $8
) page JOIN messages m ON m.id = page.id ORDER BY page.score DESC, page.id DESC";
const SEARCH_POSTGRES: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, page.score, m.seq FROM (
    SELECT id, score FROM (
        SELECT s.id AS id, (ts_rank(s.document, q, 1)::float8 * 1000000000.0::float8 / (1.0::float8 + GREATEST(0, $2 - m.observed_at_ms)::float8 / 2592000000.0::float8))::bigint AS score
        FROM message_search s JOIN messages m ON m.id = s.id, to_tsquery('simple', $1) q
        WHERE s.document @@ q AND s.server = ANY($3)
            AND ($4::text IS NULL OR m.channel = $4) AND s.author LIKE $5 ESCAPE '\'
    ) hits WHERE score < $6 OR (score = $6 AND id < $7) ORDER BY score DESC, id DESC LIMIT $8
) page JOIN messages m ON m.id = page.id ORDER BY page.score DESC, page.id DESC";
// `from:` alone: that author's messages, newest first. $1 the servers, $2
// the channel or NULL, $3 the author pattern, $4 the cursor's ID, $5 the limit.
const AUTHOR_SQLITE: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, 0, m.seq FROM message_search s JOIN messages m ON m.id = s.id
    WHERE s.server IN (SELECT value FROM json_each($1)) AND ($2 IS NULL OR m.channel = $2)
        AND s.author LIKE $3 ESCAPE '\' AND s.id < $4 ORDER BY s.id DESC LIMIT $5";
const AUTHOR_POSTGRES: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, 0::bigint, m.seq FROM message_search s JOIN messages m ON m.id = s.id
    WHERE s.server = ANY($1) AND ($2::text IS NULL OR m.channel = $2)
        AND s.author LIKE $3 ESCAPE '\' AND s.id < $4 ORDER BY s.id DESC LIMIT $5";

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("history message exceeds the storage limits")]
    InvalidMessage,
    #[error("the search exceeds the storage limits")]
    InvalidSearch,
    #[error("expected a sqlite:, postgres:, or postgresql: database URL")]
    UnsupportedDatabase,
    #[error("cannot lock the database: {0}")]
    Lock(String),
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

/// Store a message with its search text and index it, inside a transaction
/// (`$transaction`); evaluates to its row ID. `$body` is what is stored,
/// `$author` the search author, `$meta` the Nuschl seq, edit and deletion
/// times.
macro_rules! insert {
    ($transaction:expr, $index:expr, $message:expr, $body:expr, $author:expr, $meta:expr) => {{
        let (seq, edited_ms, deleted_ms): (Option<i64>, Option<i64>, Option<i64>) = $meta;
        let id: i64 = sqlx::query_scalar(INSERT_MESSAGE)
            .bind(&$message.server)
            .bind(&$message.channel)
            .bind(&$message.author)
            .bind($message.observed_at_ms)
            .bind($body)
            .bind(seq)
            .bind(edited_ms)
            .bind(deleted_ms)
            .fetch_one(&mut *$transaction)
            .await?;
        sqlx::query(INSERT_SEARCH)
            .bind(id)
            .bind(&$message.server)
            .bind($author)
            .bind(visible($body).to_lowercase())
            .execute(&mut *$transaction)
            .await?;
        sqlx::query($index)
            .bind(id)
            .bind(id + 1)
            .execute(&mut *$transaction)
            .await?;
        id
    }};
}

/// Replace a message's search text and search author, inside a transaction.
/// SQLite's full-text index has external content: a row that is indexed (at
/// or above `search_backfill.below`) is taken out with the text it was
/// indexed with before that text changes, and put back with the new text.
/// PostgreSQL recomputes a document that is already there. A message without
/// search text yet is left to the backfill, which reads its current body.
macro_rules! research {
    ($transaction:expr, $sqlite:expr, $id:expr, $visible:expr, $author:expr) => {{
        let id: i64 = $id;
        let (visible, author): (String, String) = ($visible, $author);
        let old: Option<String> =
            sqlx::query_scalar("SELECT visible FROM message_search WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *$transaction)
                .await?;
        if let Some(old) = old {
            if $sqlite {
                let below: i64 = sqlx::query_scalar("SELECT below FROM search_backfill")
                    .fetch_one(&mut *$transaction)
                    .await?;
                let indexed = id >= below;
                if indexed {
                    sqlx::query(FTS_DELETE)
                        .bind(id)
                        .bind(&old)
                        .execute(&mut *$transaction)
                        .await?;
                }
                sqlx::query("UPDATE message_search SET visible = $2, author = $3 WHERE id = $1")
                    .bind(id)
                    .bind(&visible)
                    .bind(&author)
                    .execute(&mut *$transaction)
                    .await?;
                if indexed {
                    sqlx::query(FTS_INSERT)
                        .bind(id)
                        .bind(&visible)
                        .execute(&mut *$transaction)
                        .await?;
                }
            } else {
                sqlx::query(RESEARCH_POSTGRES)
                    .bind(id)
                    .bind(&visible)
                    .bind(&author)
                    .execute(&mut *$transaction)
                    .await?;
            }
        }
    }};
}

/// Remove a message row with its search text and full-text entry, inside a
/// transaction.
macro_rules! remove {
    ($transaction:expr, $sqlite:expr, $id:expr) => {{
        let id: i64 = $id;
        let old: Option<String> =
            sqlx::query_scalar("SELECT visible FROM message_search WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *$transaction)
                .await?;
        if let Some(old) = old {
            if $sqlite {
                let below: i64 = sqlx::query_scalar("SELECT below FROM search_backfill")
                    .fetch_one(&mut *$transaction)
                    .await?;
                if id >= below {
                    sqlx::query(FTS_DELETE)
                        .bind(id)
                        .bind(&old)
                        .execute(&mut *$transaction)
                        .await?;
                }
            }
            sqlx::query("DELETE FROM message_search WHERE id = $1")
                .bind(id)
                .execute(&mut *$transaction)
                .await?;
        }
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id)
            .execute(&mut *$transaction)
            .await?;
    }};
}

pub mod murmur;
pub mod server;

/// A write transaction that takes the write lock when it begins. The server runs
/// several storage workers (state, history, key transparency, MLS) on one SQLite file;
/// a deferred transaction that reads before it writes then fails with `SQLITE_BUSY`
/// at once, without waiting for the busy timeout, whenever another worker committed
/// in between. `BEGIN IMMEDIATE` waits for the lock instead. PostgreSQL keeps `BEGIN`.
pub(crate) trait BeginWrite {
    type Database: sqlx::Database;
    async fn begin_write(&self) -> Result<sqlx::Transaction<'static, Self::Database>, sqlx::Error>;
}

impl BeginWrite for sqlx::SqlitePool {
    type Database = sqlx::Sqlite;
    async fn begin_write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, sqlx::Error> {
        self.begin_with("BEGIN IMMEDIATE").await
    }
}

impl BeginWrite for sqlx::PgPool {
    type Database = sqlx::Postgres;
    async fn begin_write(&self) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
        self.begin().await
    }
}

/// What search matches and returns: the text without inline image data.
fn visible(body: &str) -> String {
    body.split(nuschl_core::EMBED)
        .step_by(2)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry(
    (id, server, channel, author, observed_at_ms, body, seq, edited_ms, deleted_ms): Row,
) -> HistoryEntry {
    HistoryEntry {
        id,
        message: ChatMessage {
            server,
            channel,
            author,
            observed_at_ms,
            body,
        },
        seq,
        edited_ms,
        deleted_ms,
    }
}

/// Whether a message fits what one row may hold.
fn storable(message: &ChatMessage) -> bool {
    message.body.len() <= MAX_CONTENT_BYTES
        && [&message.server, &message.channel, &message.author]
            .iter()
            .all(|value| value.len() <= 512)
}

fn size(entry: &HistoryEntry) -> usize {
    let message = &entry.message;
    message.server.len() + message.channel.len() + message.author.len() + message.body.len()
}

/// Takes entries while they fit the page's text budget; the first always
/// does, so every page makes progress.
#[derive(Default)]
struct Budget(usize);
impl Budget {
    fn admits(&mut self, entry: &HistoryEntry, first: bool) -> bool {
        let size = size(entry);
        if !first && size > MAX_PAGE_TEXT_BYTES.saturating_sub(self.0) {
            return false;
        }
        self.0 += size;
        true
    }
}

/// Up to `limit` of `rows` within the budget, and whether any were left.
fn bounded(rows: Vec<Row>, limit: usize) -> (Vec<HistoryEntry>, bool) {
    let fetched = rows.len();
    let mut budget = Budget::default();
    let mut entries = Vec::new();
    for row in rows.into_iter().take(limit) {
        let entry = entry(row);
        if !budget.admits(&entry, entries.is_empty()) {
            break;
        }
        entries.push(entry);
    }
    let more = fetched > entries.len();
    (entries, more)
}

/// The query in FTS5 syntax. Every phrase is quoted, so no operator or
/// column filter can come from what was typed.
fn fts5(query: &Query) -> String {
    let mut expression = String::new();
    for phrase in &query.phrases {
        expression.push_str(" \"");
        expression.push_str(&phrase.join(" ").replace('"', "\"\""));
        expression.push('"');
    }
    if query.prefix {
        expression.push('*');
    }
    expression
}

/// The query in PostgreSQL's tsquery syntax. Tokens are runs of letters and
/// digits, quoted, so they carry no operators.
fn tsquery(query: &Query) -> String {
    let last = query.phrases.len().wrapping_sub(1);
    query
        .phrases
        .iter()
        .enumerate()
        .map(|(index, phrase)| {
            let mut words: Vec<String> = phrase
                .iter()
                .map(|token| format!("'{}'", token.replace('\'', "''")))
                .collect();
            if query.prefix
                && index == last
                && let Some(word) = words.last_mut()
            {
                word.push_str(":*");
            }
            format!("({})", words.join(" <-> "))
        })
        .collect::<Vec<_>>()
        .join(" & ")
}

/// A LIKE pattern for names starting with `name`, wildcards taken literally.
fn starts_with(name: &str) -> String {
    let escaped = name
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

/// A JSON array of strings, for SQLite's json_each.
fn json_array(values: &[String]) -> String {
    let mut json = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        json.push('"');
        for c in value.chars() {
            match c {
                '"' | '\\' => {
                    json.push('\\');
                    json.push(c);
                }
                c if u32::from(c) < 0x20 => json.push_str(&format!("\\u{:04x}", u32::from(c))),
                c => json.push(c),
            }
        }
        json.push('"');
    }
    json.push(']');
    json
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

    /// Every connection the pool opens (it retires idle ones) gets these
    /// settings. The page cache is capped (`SQLITE_CACHE_KIB`); memory-mapped
    /// reads stay off and temporary files stay files (in the sandbox's private
    /// /tmp), SQLite's defaults, so neither moves into the worker's memory.
    async fn sqlite(options: SqliteConnectOptions) -> Result<Self, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(
                options
                    .foreign_keys(true)
                    .pragma("trusted_schema", "OFF")
                    .pragma("cache_size", (-SQLITE_CACHE_KIB).to_string()),
            )
            .await?;
        sqlx::migrate!("./migrations/sqlite").run(&pool).await?;
        Ok(Self {
            pool: Pool::Sqlite(pool),
        })
    }

    /// The statement that adds a range of message_search rows to the
    /// full-text index.
    fn index_statement(&self) -> &'static str {
        match self.pool {
            Pool::Sqlite(_) => INDEX_SQLITE,
            Pool::Postgres(_) => INDEX_POSTGRES,
        }
    }

    /// Make one batch of older messages searchable; returns whether there
    /// may be more. Search text is filled first, then indexed, newest first
    /// both times. The worker calls this while no request waits, so a large
    /// history never holds up opening it; until it is done, search reports
    /// that it is still indexing.
    pub async fn backfill(&self) -> Result<bool, Error> {
        Ok(self.backfill_search().await? || self.backfill_index().await?)
    }

    /// Whether older messages are still to be made searchable.
    async fn backlog(&self) -> Result<bool, Error> {
        with_pool!(&self.pool, |pool| {
            let pending: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM messages WHERE id < (SELECT COALESCE(MIN(id), $1) FROM message_search)) OR EXISTS (SELECT 1 FROM message_search WHERE id < (SELECT below FROM search_backfill))",
            )
            .bind(i64::MAX)
            .fetch_one(pool)
            .await?;
            Ok(pending)
        })
    }

    /// Fill search text for one batch of messages stored before it existed,
    /// newest first; returns whether there was any. Appends write both rows in
    /// one transaction and IDs only grow, so every message at or above the
    /// smallest indexed ID has its row: an interrupted fill resumes below it,
    /// and a finished one costs two index lookups.
    async fn backfill_search(&self) -> Result<bool, Error> {
        type Old = (i64, String, String, String);
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let rows: Vec<Old> = sqlx::query_as(
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

    /// Index one batch of search text stored before the full-text index
    /// existed, newest first; returns whether there was any. search_backfill
    /// holds the lowest indexed ID: appends index their own row, so every
    /// row at or above it is indexed, and rows the search-text fill adds
    /// later lie below it.
    async fn backfill_index(&self) -> Result<bool, Error> {
        let index = self.index_statement();
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let below: i64 = sqlx::query_scalar("SELECT below FROM search_backfill")
                .fetch_one(&mut *transaction)
                .await?;
            let lowest: Option<i64> = sqlx::query_scalar(
                "SELECT MIN(id) FROM (SELECT id FROM message_search WHERE id < $1 ORDER BY id DESC LIMIT $2) batch",
            )
            .bind(below)
            .bind(INDEX_BATCH)
            .fetch_one(&mut *transaction)
            .await?;
            let Some(lowest) = lowest else {
                return Ok(false);
            };
            sqlx::query(index)
                .bind(lowest)
                .bind(below)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("UPDATE search_backfill SET below = $1")
                .bind(lowest)
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            Ok(true)
        })
    }

    pub async fn append(&self, message: &ChatMessage) -> Result<i64, Error> {
        if !storable(message) {
            return Err(Error::InvalidMessage);
        }
        let index = self.index_statement();
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let id = insert!(
                transaction,
                index,
                message,
                message.body.as_str(),
                message.author.to_lowercase(),
                (None, None, None)
            );
            transaction.commit().await?;
            Ok(id)
        })
    }

    /// Store messages of a Nuschl server's conversations by their seq, in
    /// one transaction: a message already stored under the same server,
    /// channel (the local storage key) and seq is updated, so one seen live
    /// and again in a history page is kept once. A row deleted before stays a
    /// tombstone; text older than the stored edit is not written back.
    /// Tombstones keep no text and no search entry. Returns each record's row
    /// ID in order, or 0 for a record beyond the limits `append` enforces
    /// (not stored).
    pub async fn store_server(&self, records: &[ServerRecord]) -> Result<Vec<i64>, Error> {
        if records.len() > MAX_STORE_RECORDS {
            return Err(Error::InvalidMessage);
        }
        let index = self.index_statement();
        let sqlite = matches!(self.pool, Pool::Sqlite(_));
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let mut ids = Vec::with_capacity(records.len());
            for record in records {
                let message = &record.message;
                if !storable(message) {
                    ids.push(0);
                    continue;
                }
                let existing: Option<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
                    "SELECT id, edited_ms, deleted_ms FROM messages WHERE server = $1 AND channel = $2 AND seq = $3",
                )
                .bind(&message.server)
                .bind(&message.channel)
                .bind(record.seq)
                .fetch_optional(&mut *transaction)
                .await?;
                let id = match existing {
                    Some((id, edited_ms, deleted_ms)) => {
                        let deleted_ms = deleted_ms.or(record.deleted_ms);
                        // A late copy from before the stored edit keeps the edit.
                        let current = record.edited_ms >= edited_ms;
                        let edited_ms = if current { record.edited_ms } else { edited_ms };
                        let author = if deleted_ms.is_some() {
                            String::new()
                        } else {
                            message.author.to_lowercase()
                        };
                        if deleted_ms.is_some() || current {
                            let body = if deleted_ms.is_some() {
                                ""
                            } else {
                                message.body.as_str()
                            };
                            sqlx::query(
                                "UPDATE messages SET author = $2, observed_at_ms = $3, body = $4, edited_ms = $5, deleted_ms = $6 WHERE id = $1",
                            )
                            .bind(id)
                            .bind(&message.author)
                            .bind(message.observed_at_ms)
                            .bind(body)
                            .bind(edited_ms)
                            .bind(deleted_ms)
                            .execute(&mut *transaction)
                            .await?;
                            research!(
                                transaction,
                                sqlite,
                                id,
                                visible(body).to_lowercase(),
                                author
                            );
                        }
                        id
                    }
                    None => {
                        let (body, author) = if record.deleted_ms.is_some() {
                            ("", String::new())
                        } else {
                            (message.body.as_str(), message.author.to_lowercase())
                        };
                        insert!(
                            transaction,
                            index,
                            message,
                            body,
                            author,
                            (Some(record.seq), record.edited_ms, record.deleted_ms)
                        )
                    }
                };
                ids.push(id);
            }
            transaction.commit().await?;
            Ok(ids)
        })
    }

    /// Our own message, stored while it was pending, was acknowledged as
    /// `seq`. If its conversation already has a row with that seq (a history
    /// page stored it first), this pending row goes instead.
    pub async fn set_seq(&self, id: i64, seq: i64) -> Result<(), Error> {
        let sqlite = matches!(self.pool, Pool::Sqlite(_));
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let found: Option<(String, String)> =
                sqlx::query_as("SELECT server, channel FROM messages WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut *transaction)
                    .await?;
            if let Some((server, channel)) = found {
                let taken: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM messages WHERE server = $1 AND channel = $2 AND seq = $3 AND id <> $4",
                )
                .bind(&server)
                .bind(&channel)
                .bind(seq)
                .bind(id)
                .fetch_optional(&mut *transaction)
                .await?;
                if taken.is_some() {
                    remove!(transaction, sqlite, id);
                } else {
                    sqlx::query("UPDATE messages SET seq = $2 WHERE id = $1")
                        .bind(id)
                        .bind(seq)
                        .execute(&mut *transaction)
                        .await?;
                }
            }
            transaction.commit().await?;
            Ok(())
        })
    }

    /// The author edited the cached message `seq` of a Nuschl server's
    /// conversation: its text is overwritten (earlier versions are not kept,
    /// as on the server). Returns whether such a message, not deleted, was
    /// cached.
    pub async fn edit(
        &self,
        server: &str,
        channel: &str,
        seq: i64,
        body: &str,
        edited_ms: i64,
    ) -> Result<bool, Error> {
        if body.len() > MAX_CONTENT_BYTES || server.len() > 512 || channel.len() > 512 {
            return Err(Error::InvalidMessage);
        }
        let sqlite = matches!(self.pool, Pool::Sqlite(_));
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let found: Option<(i64, String)> = sqlx::query_as(
                "SELECT id, author FROM messages WHERE server = $1 AND channel = $2 AND seq = $3 AND deleted_ms IS NULL AND (edited_ms IS NULL OR edited_ms <= $4)",
            )
            .bind(server)
            .bind(channel)
            .bind(seq)
            .bind(edited_ms)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some((id, author)) = found else {
                return Ok(false);
            };
            sqlx::query("UPDATE messages SET body = $2, edited_ms = $3 WHERE id = $1")
                .bind(id)
                .bind(body)
                .bind(edited_ms)
                .execute(&mut *transaction)
                .await?;
            research!(
                transaction,
                sqlite,
                id,
                visible(body).to_lowercase(),
                author.to_lowercase()
            );
            transaction.commit().await?;
            Ok(true)
        })
    }

    /// The cached message `seq` of a Nuschl server's conversation was
    /// deleted: it becomes a tombstone, its text gone from the row, the
    /// search text and the full-text index (as the server's `secure_delete`
    /// removes it). Returns whether such a message was cached.
    pub async fn delete(
        &self,
        server: &str,
        channel: &str,
        seq: i64,
        deleted_ms: i64,
    ) -> Result<bool, Error> {
        if server.len() > 512 || channel.len() > 512 {
            return Err(Error::InvalidMessage);
        }
        let sqlite = matches!(self.pool, Pool::Sqlite(_));
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin().await?;
            let found: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM messages WHERE server = $1 AND channel = $2 AND seq = $3",
            )
            .bind(server)
            .bind(channel)
            .bind(seq)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(id) = found else {
                return Ok(false);
            };
            sqlx::query(
                "UPDATE messages SET body = '', deleted_ms = COALESCE(deleted_ms, $2) WHERE id = $1",
            )
            .bind(id)
            .bind(deleted_ms)
            .execute(&mut *transaction)
            .await?;
            research!(transaction, sqlite, id, String::new(), String::new());
            transaction.commit().await?;
            Ok(true)
        })
    }

    /// Full-text search of the visible text in the given servers' history,
    /// or one conversation (`channel`) of a single server. Every word must
    /// match, the last one also as a prefix while it is typed (no space
    /// after it); "quoted words" must appear together in order; `from:name`
    /// keeps messages whose author's name starts with `name`. Matching is
    /// case-insensitive, on runs of letters and digits.
    ///
    /// Results are ranked by relevance and recency and come in pages within
    /// the byte budget: `next` continues after the last one. The query is
    /// bound as a parameter in the database's full-text syntax with every
    /// token quoted; no caller-provided SQL or operator is executed. Inline
    /// image data is neither matched nor returned: each result's body is its
    /// visible text.
    pub async fn search(
        &self,
        servers: &[String],
        channel: Option<&str>,
        text: &str,
        after: Option<SearchCursor>,
        limit: usize,
    ) -> Result<SearchPage, Error> {
        if text.len() > MAX_QUERY_BYTES
            || servers.len() > MAX_SEARCH_SERVERS
            || servers
                .iter()
                .chain(channel.map(str::to_string).as_ref())
                .any(|value| value.len() > 512)
            || (channel.is_some() && servers.len() != 1)
        {
            return Err(Error::InvalidSearch);
        }
        let query = Query::parse(text);
        let indexing = self.backlog().await?;
        if query.is_empty() || servers.is_empty() {
            return Ok(SearchPage {
                hits: Vec::new(),
                next: None,
                indexing,
            });
        }
        let limit = limit.clamp(1, MAX_PAGE_MESSAGES);
        let now_ms = after.map_or_else(nuschl_core::now_ms, |cursor| cursor.now_ms);
        let (score, id) = after.map_or((i64::MAX, i64::MAX), |cursor| (cursor.score, cursor.id));
        let author = query
            .from
            .as_deref()
            .map_or_else(|| "%".to_string(), starts_with);
        let fetch = (limit + 1) as i64;
        let rows: Vec<RankedRow> = match &self.pool {
            Pool::Sqlite(pool) if query.phrases.is_empty() => {
                sqlx::query_as(AUTHOR_SQLITE)
                    .bind(json_array(servers))
                    .bind(channel)
                    .bind(&author)
                    .bind(id)
                    .bind(fetch)
                    .fetch_all(pool)
                    .await?
            }
            Pool::Sqlite(pool) => {
                sqlx::query_as(SEARCH_SQLITE)
                    .bind(fts5(&query))
                    .bind(now_ms)
                    .bind(json_array(servers))
                    .bind(channel)
                    .bind(&author)
                    .bind(score)
                    .bind(id)
                    .bind(fetch)
                    .fetch_all(pool)
                    .await?
            }
            Pool::Postgres(pool) if query.phrases.is_empty() => {
                sqlx::query_as(AUTHOR_POSTGRES)
                    .bind(servers)
                    .bind(channel)
                    .bind(&author)
                    .bind(id)
                    .bind(fetch)
                    .fetch_all(pool)
                    .await?
            }
            Pool::Postgres(pool) => {
                sqlx::query_as(SEARCH_POSTGRES)
                    .bind(tsquery(&query))
                    .bind(now_ms)
                    .bind(servers)
                    .bind(channel)
                    .bind(&author)
                    .bind(score)
                    .bind(id)
                    .bind(fetch)
                    .fetch_all(pool)
                    .await?
            }
        };
        let fetched = rows.len();
        let mut budget = Budget::default();
        let mut hits = Vec::new();
        let mut last = None;
        for (id, server, channel, author, observed_at_ms, body, score, seq) in
            rows.into_iter().take(limit)
        {
            let hit = entry((
                id,
                server,
                channel,
                author,
                observed_at_ms,
                visible(&body),
                seq,
                None,
                None,
            ));
            if !budget.admits(&hit, hits.is_empty()) {
                break;
            }
            hits.push(hit);
            last = Some(SearchCursor { now_ms, score, id });
        }
        Ok(SearchPage {
            next: last.filter(|_| fetched > hits.len()),
            hits,
            indexing,
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

    async fn rows(
        &self,
        statement: &'static str,
        (server, channel): (&str, &str),
        (time, id): (i64, i64),
        limit: usize,
    ) -> Result<Vec<Row>, Error> {
        Ok(with_pool!(&self.pool, |pool| {
            sqlx::query_as(statement)
                .bind(server)
                .bind(channel)
                .bind(time)
                .bind(id)
                .bind(limit as i64)
                .fetch_all(pool)
                .await?
        }))
    }

    /// Where row `id` lies in a conversation's time order: its time, or, if
    /// it is not in this conversation, that of the closest older row by ID;
    /// None if there is none.
    async fn position(&self, server: &str, channel: &str, id: i64) -> Result<Option<i64>, Error> {
        with_pool!(&self.pool, |pool| {
            let own: Option<i64> = sqlx::query_scalar(
                "SELECT observed_at_ms FROM messages WHERE id = $1 AND server = $2 AND channel = $3",
            )
            .bind(id)
            .bind(server)
            .bind(channel)
            .fetch_optional(pool)
            .await?;
            if own.is_some() {
                return Ok(own);
            }
            let older: Option<i64> = sqlx::query_scalar(
                "SELECT observed_at_ms FROM messages WHERE server = $1 AND channel = $2 AND id < $3 ORDER BY id DESC LIMIT 1",
            )
            .bind(server)
            .bind(channel)
            .bind(id)
            .fetch_optional(pool)
            .await?;
            Ok(older)
        })
    }

    /// Walk the entire archive through bounded pages, in time order (ties by
    /// row ID). The exclusive cursor is a row ID, stable under concurrent
    /// appends; message bodies are not IDs. A trusted session adapter must
    /// authorize the server/channel on every request.
    pub async fn page(
        &self,
        server: &str,
        channel: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<HistoryPage, Error> {
        let limit = limit.clamp(1, MAX_PAGE_MESSAGES);
        let bound = match before {
            None => (i64::MAX, i64::MAX),
            Some(id) => match self.position(server, channel, id).await? {
                Some(time) => (time, id),
                // Nothing at or before the cursor: nothing older.
                None => {
                    return Ok(HistoryPage {
                        messages: Vec::new(),
                        next_before: None,
                        next_after: None,
                    });
                }
            },
        };
        let rows = self.rows(PAGE, (server, channel), bound, limit + 1).await?;
        let (mut messages, more) = bounded(rows, limit);
        let next_before = more
            .then(|| messages.last().map(|entry| entry.id))
            .flatten();
        messages.reverse();
        Ok(HistoryPage {
            messages,
            next_before,
            next_after: None,
        })
    }

    /// The page of messages newer than `after`, oldest first; `next_after`
    /// continues it.
    pub async fn page_after(
        &self,
        server: &str,
        channel: &str,
        after: i64,
        limit: usize,
    ) -> Result<HistoryPage, Error> {
        let limit = limit.clamp(1, MAX_PAGE_MESSAGES);
        let bound = match self.position(server, channel, after).await? {
            Some(time) => (time, after),
            None => (i64::MIN, i64::MIN),
        };
        let rows = self
            .rows(PAGE_AFTER, (server, channel), bound, limit + 1)
            .await?;
        let (messages, more) = bounded(rows, limit);
        let next_after = more
            .then(|| messages.last().map(|entry| entry.id))
            .flatten();
        Ok(HistoryPage {
            messages,
            next_before: None,
            next_after,
        })
    }

    /// A page with message `id` (or, if it is not in this conversation, the
    /// closest older one) in the middle, for jumping to a search result.
    /// Within the byte budget the message comes first, then its neighbours
    /// alternately older and newer; both cursors continue from the edges.
    pub async fn page_around(
        &self,
        server: &str,
        channel: &str,
        id: i64,
        limit: usize,
    ) -> Result<HistoryPage, Error> {
        let half = limit.clamp(2, MAX_PAGE_MESSAGES) / 2;
        let (older, newer) = match self.position(server, channel, id).await? {
            // The message itself (or the closest older one) and what is older.
            Some(time) => (
                self.rows(
                    PAGE,
                    (server, channel),
                    (time, id.saturating_add(1)),
                    half + 1,
                )
                .await?,
                self.rows(PAGE_AFTER, (server, channel), (time, id), half + 1)
                    .await?,
            ),
            None => (
                Vec::new(),
                self.rows(
                    PAGE_AFTER,
                    (server, channel),
                    (i64::MIN, i64::MIN),
                    half + 1,
                )
                .await?,
            ),
        };
        let (older_fetched, newer_fetched) = (older.len(), newer.len());
        let mut older = older.into_iter().take(half).map(entry).peekable();
        let mut newer = newer.into_iter().take(half).map(entry).peekable();
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let mut budget = Budget::default();
        let (mut older_open, mut newer_open) = (true, true);
        while older_open || newer_open {
            for (open, side, taken) in [
                (&mut older_open, &mut older, &mut before),
                (&mut newer_open, &mut newer, &mut after),
            ] {
                if !*open {
                    continue;
                }
                let first = budget.0 == 0;
                match side.next_if(|entry| budget.admits(entry, first)) {
                    Some(entry) => taken.push(entry),
                    None => *open = false,
                }
            }
        }
        let next_before = (older_fetched > before.len())
            .then(|| before.last().map(|entry| entry.id))
            .flatten();
        let next_after = (newer_fetched > after.len())
            .then(|| after.last().or(before.first()).map(|entry| entry.id))
            .flatten();
        before.reverse();
        before.extend(after);
        Ok(HistoryPage {
            messages: before,
            next_before,
            next_after,
        })
    }
}

#[cfg(test)]
mod tests {

    /// SQLx includes comments in its SHA-384 migration checksums. This digest
    /// is from Nuschel's pre-rename 0004 migration, before the
    /// branding change; preserve its original text, including the old name.
    #[tokio::test]
    async fn legacy_history_opens_with_its_original_migration_checksum() -> Result<(), Error> {
        const CHECKSUM: [u8; 48] = [
            100, 182, 96, 254, 82, 30, 193, 4, 99, 214, 34, 87, 211, 213, 113, 42, 250, 159, 145,
            176, 170, 113, 200, 161, 57, 117, 8, 135, 74, 103, 164, 225, 208, 95, 107, 158, 240,
            46, 179, 211, 250, 111, 65, 220, 199, 115, 47, 237,
        ];
        for migrator in [
            sqlx::migrate!("./migrations/sqlite"),
            sqlx::migrate!("./migrations/postgres"),
        ] {
            let fourth = migrator
                .iter()
                .find(|migration| migration.version == 4)
                .unwrap();
            assert_eq!(fourth.checksum.as_ref(), CHECKSUM);
        }
        let root = tempfile::tempdir().map_err(sqlx::Error::Io)?;
        let old = root.path().join("nuschel.sqlite3");
        let new = root.path().join("nuschl.sqlite3");
        let history = History::open(&old).await?;
        history
            .append(&message("saved", "ada", "before\nrename"))
            .await?;
        let Pool::Sqlite(pool) = &history.pool else {
            unreachable!()
        };
        // A database produced by the old implementation records this digest.
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 4")
            .bind(CHECKSUM.as_slice())
            .execute(pool)
            .await?;
        pool.close().await;
        drop(history);
        std::fs::rename(&old, &new).map_err(sqlx::Error::Io)?;
        let migrated = History::open(&new).await?;
        let page = migrated.page("saved", "general", None, 100).await?;
        assert_eq!(page.messages[0].message.body, "before\nrename");
        Ok(())
    }
    use super::*;

    /// The page cache is capped on every history connection: the first, one
    /// the pool opens to replace a closed one, after reopening the database,
    /// and through a URL. Memory-mapped reads and temporary storage keep
    /// SQLite's defaults (off, files).
    #[tokio::test]
    async fn every_history_connection_caps_the_page_cache() -> Result<(), Error> {
        async fn settings(history: &History) -> Result<(i64, i64, i64), Error> {
            let Pool::Sqlite(pool) = &history.pool else {
                unreachable!()
            };
            let mut connection = pool.acquire().await?;
            let mut values = [0; 3];
            for (value, pragma) in values
                .iter_mut()
                .zip(["cache_size", "mmap_size", "temp_store"])
            {
                *value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("PRAGMA {pragma}")))
                    .fetch_one(&mut *connection)
                    .await?;
            }
            // Changed on this connection only: a connection opened later must
            // not show it.
            sqlx::query("PRAGMA cache_size = 7")
                .execute(&mut *connection)
                .await?;
            Ok((values[0], values[1], values[2]))
        }
        let capped = (-SQLITE_CACHE_KIB, 0, 0);
        let directory = tempfile::tempdir().map_err(sqlx::Error::Io)?;
        let path = directory.path().join("history.sqlite3");
        let history = History::open(&path).await?;
        assert_eq!(settings(&history).await?, capped);
        let Pool::Sqlite(pool) = &history.pool else {
            unreachable!()
        };
        pool.acquire().await?.close().await?;
        assert_eq!(settings(&history).await?, capped);
        pool.close().await;
        drop(history);
        let history = History::open(&path).await?;
        assert_eq!(settings(&history).await?, capped);
        drop(history);
        let url = format!("sqlite://{}", path.display());
        let history = History::connect(&url).await?;
        assert_eq!(settings(&history).await?, capped);
        Ok(())
    }

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
        let embed = nuschl_core::EMBED;
        format!("{embed}data:image/png;base64,{payload}{embed}")
    }

    /// The bodies one server's history finds for `query`, best first.
    async fn find(history: &History, server: &str, query: &str) -> Result<Vec<String>, Error> {
        let page = history
            .search(&[server.to_string()], None, query, None, 100)
            .await?;
        Ok(page.hits.into_iter().map(|hit| hit.message.body).collect())
    }

    async fn plan(pool: &SqlitePool, query: &str, binds: &[&str]) -> Result<Vec<String>, Error> {
        let sql = format!("EXPLAIN QUERY PLAN {query}");
        let mut statement = sqlx::query_as(sqlx::AssertSqlSafe(sql));
        for bind in binds {
            statement = statement.bind(*bind);
        }
        let plan: Vec<(i64, i64, i64, String)> = statement.fetch_all(pool).await?;
        Ok(plan.into_iter().map(|row| row.3).collect())
    }

    #[tokio::test]
    async fn page_and_search_queries_seek_their_indexes() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        let Pool::Sqlite(pool) = &history.pool else {
            unreachable!()
        };
        // An early page query, `(?3 IS NULL OR id < ?3)`, sought only the
        // server and channel, then filtered every newer message by ID. Pages
        // seek to the cursor's time (ties by ID are filtered within it).
        for (query, seek) in [
            (
                PAGE,
                "INDEX messages_time (server=? AND channel=? AND observed_at_ms<?)",
            ),
            (
                PAGE_AFTER,
                "INDEX messages_time (server=? AND channel=? AND observed_at_ms>?)",
            ),
        ] {
            let plan = plan(pool, query, &["server", "general", "1", "1", "1"]).await?;
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
        // Search starts from the full-text index; everything else is found
        // by primary key, and only the matches are sorted.
        let plan = plan(pool, SEARCH_SQLITE, &["\"x\"", "0", "[]"]).await?;
        assert!(
            plan.iter()
                .any(|step| step.contains("message_fts VIRTUAL TABLE INDEX")),
            "{plan:?}"
        );
        assert!(
            plan.iter().all(|step| !step.starts_with("SCAN")
                || step.contains("VIRTUAL TABLE")
                || step.starts_with("SCAN (subquery")
                || step == "SCAN page"),
            "{plan:?}"
        );
        Ok(())
    }

    async fn image_data_cannot_crowd_out_search_results(history: &History) -> Result<(), Error> {
        history
            .append(&message("crowd", "ada", "Ärger mit dem Server"))
            .await?;
        // More newer messages than one search returns match only in image data.
        for _ in 0..150 {
            history
                .append(&message("crowd", "bob", &image("ärger ärger")))
                .await?;
        }
        assert_eq!(
            find(history, "crowd", "ÄRGER").await?,
            ["Ärger mit dem Server"]
        );
        assert_eq!(find(history, "crowd", "from:ADA").await?.len(), 1);
        Ok(())
    }

    /// No result is left out: a byte budget splits them into pages.
    async fn search_pages_within_the_byte_budget(history: &History) -> Result<(), Error> {
        let text = format!("needle {}", "x".repeat(40 * 1024));
        for _ in 0..5 {
            let body = format!("{text}{}", image(&"A".repeat(100 * 1024)));
            history.append(&message("budget", "ada", &body)).await?;
        }
        let servers = ["budget".to_string()];
        let mut found = 0;
        let mut after = None;
        loop {
            let page = history.search(&servers, None, "needle", after, 100).await?;
            // Images are left out of the bodies; two 40 KiB texts fit in 96 KiB.
            assert!(page.hits.len() <= 2 && !page.hits.is_empty());
            assert!(page.hits.iter().all(|hit| hit.message.body == text));
            found += page.hits.len();
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(found, 5);
        Ok(())
    }

    async fn search_pages_reach_every_match(history: &History) -> Result<(), Error> {
        let mut expected = Vec::new();
        for n in 0..230 {
            let body = format!("match number {n}");
            expected.push(history.append(&message("paging", "ada", &body)).await?);
        }
        let servers = ["paging".to_string()];
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = history.search(&servers, None, "match", after, 100).await?;
            assert!(page.hits.len() <= 100);
            seen.extend(page.hits.iter().map(|hit| hit.id));
            // A message arriving meanwhile neither repeats nor hides one.
            history
                .append(&message("paging", "ada", "match arriving later"))
                .await?;
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        seen.retain(|id| expected.contains(id));
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "no result repeats");
        assert_eq!(sorted, expected);
        Ok(())
    }

    async fn full_text_matches_words_phrases_and_prefixes(history: &History) -> Result<(), Error> {
        for (channel, author, body) in [
            ("general", "Ada Lovelace", "Deploy the Rust server tonight"),
            ("general", "Grace", "The server rust is deep"),
            ("dev", "Ada Lovelace", "rusty servers everywhere"),
            ("general", "Linus", "Ärger mit dem Server; 100% done_now"),
        ] {
            let mut message = message("words", author, body);
            message.channel = channel.into();
            history.append(&message).await?;
        }
        history
            .append(&message("elsewhere", "Grace", "rust server"))
            .await?;
        let mut both = find(history, "words", "SERVER rust ").await?;
        both.sort();
        assert_eq!(
            both,
            ["Deploy the Rust server tonight", "The server rust is deep"],
            "every word, in any order and case"
        );
        assert_eq!(
            find(history, "words", "\"rust server\"").await?,
            ["Deploy the Rust server tonight"],
            "a quoted phrase only in its order"
        );
        assert_eq!(find(history, "words", "rust serv").await?.len(), 2);
        assert_eq!(
            find(history, "words", "serv").await?.len(),
            4,
            "the last word is a prefix"
        );
        assert!(
            find(history, "words", "serv ").await?.is_empty(),
            "until a space ends it"
        );
        assert!(
            find(history, "words", "rus serv").await?.is_empty(),
            "earlier words are whole"
        );
        assert_eq!(
            find(history, "words", "\"rust serv").await?.len(),
            1,
            "an open quote too"
        );
        assert_eq!(find(history, "words", "ärger").await?.len(), 1);
        assert!(
            find(history, "words", "arger").await?.is_empty(),
            "diacritics count"
        );
        assert_eq!(find(history, "words", "100 done now").await?.len(), 1);
        // Authors by the start of their name; with words, both must match.
        assert_eq!(find(history, "words", "from:ada").await?.len(), 2);
        assert_eq!(
            find(history, "words", "from:\"ada love\" server")
                .await?
                .len(),
            2
        );
        assert_eq!(find(history, "words", "from:ada server ").await?.len(), 1);
        assert!(
            find(history, "words", "from:%").await?.is_empty(),
            "wildcards are literal"
        );
        // Typed operators are words like any other, never syntax.
        for typed in [
            "rust OR deep",
            "NEAR(rust server)",
            "visible:rust",
            "rust NOT deep",
            "'",
        ] {
            assert!(find(history, "words", typed).await?.is_empty(), "{typed}");
        }
        assert_eq!(find(history, "words", "^rust* -deep").await?.len(), 1);
        assert!(find(history, "words", "% _ \"\" ").await?.is_empty());
        // Scoped to the servers given, or one conversation of one server.
        let servers = ["words".to_string(), "elsewhere".to_string()];
        let all = history
            .search(&servers, None, "rust server", None, 100)
            .await?;
        assert_eq!(all.hits.len(), 3);
        let dev = history
            .search(&servers[..1], Some("dev"), "serv", None, 100)
            .await?;
        assert_eq!(dev.hits.len(), 1);
        assert!(matches!(
            history
                .search(&servers, Some("dev"), "serv", None, 100)
                .await,
            Err(Error::InvalidSearch)
        ));
        Ok(())
    }

    async fn results_rank_relevance_and_recency(history: &History) -> Result<(), Error> {
        const DAY_MS: i64 = 24 * 60 * 60 * 1000;
        let now = nuschl_core::now_ms();
        for (age_days, body) in [
            (400, "old launch"),
            (1, "new launch"),
            (10, "launch launch launch"),
            (
                10,
                "launch is one word in a long sentence about many other things entirely",
            ),
        ] {
            let mut message = message("rank", "ada", body);
            message.observed_at_ms = now - age_days * DAY_MS;
            history.append(&message).await?;
        }
        let found = find(history, "rank", "launch ").await?;
        let at = |body: &str| found.iter().position(|found| found == body).unwrap();
        assert!(at("new launch") < at("old launch"), "{found:?}");
        assert!(
            at("launch launch launch")
                < at("launch is one word in a long sentence about many other things entirely"),
            "{found:?}"
        );
        Ok(())
    }

    async fn pages_around_and_after_a_message(history: &History) -> Result<(), Error> {
        let mut ids = Vec::new();
        for n in 0..200 {
            ids.push(
                history
                    .append(&message("around", "ada", &format!("message {n}")))
                    .await?,
            );
            if n % 7 == 0 {
                let mut noise = message("around", "ada", "elsewhere");
                noise.channel = "other".into();
                history.append(&noise).await?;
            }
        }
        let page_ids = |page: &HistoryPage| {
            page.messages
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
        };
        let around = history
            .page_around("around", "general", ids[100], 40)
            .await?;
        assert_eq!(page_ids(&around), ids[81..=120]);
        assert_eq!(around.next_before, Some(ids[81]));
        assert_eq!(around.next_after, Some(ids[120]));
        let after = history
            .page_after("around", "general", ids[120], 40)
            .await?;
        assert_eq!(page_ids(&after), ids[121..=160]);
        let last = history
            .page_after("around", "general", ids[160], 40)
            .await?;
        assert_eq!(page_ids(&last), ids[161..]);
        assert_eq!(last.next_after, None, "the newest message is included");
        let before = history
            .page("around", "general", around.next_before, 40)
            .await?;
        assert_eq!(page_ids(&before), ids[41..=80]);
        // At either end, that cursor is spent.
        let newest = history
            .page_around("around", "general", ids[199], 40)
            .await?;
        assert_eq!(page_ids(&newest), ids[180..]);
        assert_eq!(newest.next_after, None);
        let oldest = history.page_around("around", "general", ids[0], 40).await?;
        assert_eq!(page_ids(&oldest), ids[..=20]);
        assert_eq!(oldest.next_before, None);
        // The message itself comes first when the budget is tight.
        let big = "x".repeat(60 * 1024);
        let mut large = Vec::new();
        for _ in 0..5 {
            large.push(history.append(&message("around", "ada", &big)).await?);
        }
        let tight = history
            .page_around("around", "general", large[2], 40)
            .await?;
        assert_eq!(page_ids(&tight), [large[2]]);
        assert_eq!(tight.next_before, Some(large[2]));
        assert_eq!(tight.next_after, Some(large[2]));
        Ok(())
    }

    /// Every check against one database, as both backends run them.
    async fn search_checks(history: &History) -> Result<(), Error> {
        image_data_cannot_crowd_out_search_results(history).await?;
        search_pages_within_the_byte_budget(history).await?;
        search_pages_reach_every_match(history).await?;
        full_text_matches_words_phrases_and_prefixes(history).await?;
        results_rank_relevance_and_recency(history).await?;
        pages_around_and_after_a_message(history).await
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
        let count = 2 * INDEX_BATCH + 5;
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

    async fn an_older_database_becomes_searchable(
        history: &History,
        count: i64,
    ) -> Result<(), Error> {
        let old = ["old".to_string()];
        let before = history.search(&old, None, "oldest", None, 100).await?;
        assert!(before.hits.is_empty() && before.indexing);
        let mut batches = 0;
        while history.backfill().await? {
            batches += 1;
            // Each batch is committed: what is indexed stays searchable.
            assert!(
                history
                    .search(&old, None, "oldest", None, 100)
                    .await?
                    .indexing
                    || batches > 0
            );
        }
        assert!(batches >= 3, "filled in batches, not at once ({batches})");
        let rows: i64 = with_pool!(&history.pool, |pool| {
            sqlx::query_scalar("SELECT COUNT(*) FROM message_search")
                .fetch_one(pool)
                .await?
        });
        assert_eq!(rows, count);
        let found = history.search(&old, None, "needle", None, 100).await?;
        assert!(!found.indexing);
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.hits[0].message.body, "the oldest Needle");
        // New messages are searchable at once; there is nothing left to fill.
        history
            .append(&message("old", "ada", "fresh needle"))
            .await?;
        assert_eq!(find(history, "old", "needle").await?.len(), 2);
        assert!(!history.backfill().await?);
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
        an_older_database_becomes_searchable(&history, count).await?;
        search_checks(&history).await
    }

    /// `NUSCHL_TEST_POSTGRES_URL=postgresql://... cargo test -p nuschl-storage
    /// -- --ignored` runs the same checks on PostgreSQL. The database must be a
    /// throwaway one: its history tables are dropped first.
    #[tokio::test]
    #[ignore = "needs a throwaway PostgreSQL database"]
    async fn search_on_postgres() -> Result<(), Error> {
        let url = std::env::var("NUSCHL_TEST_POSTGRES_URL").expect("NUSCHL_TEST_POSTGRES_URL");
        let pool = PgPool::connect(&url).await?;
        sqlx::query(
            "DROP TABLE IF EXISTS search_backfill, message_search, messages, _sqlx_migrations",
        )
        .execute(&pool)
        .await?;
        let count = older_database(&Pool::Postgres(pool.clone())).await?;
        pool.close().await;
        let history = History::connect(&url).await?;
        an_older_database_becomes_searchable(&history, count).await?;
        search_checks(&history).await?;
        server_message_checks(&history).await?;
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
    async fn search_is_scoped_case_insensitive_and_ignores_images() -> Result<(), Error> {
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
        assert_eq!(
            find(&history, "one", "RUST").await?,
            ["Deploy the Rust server"],
            "scoped to the server, case-insensitive, ignores image data"
        );
        assert_eq!(find(&history, "one", "done_now").await?, ["100% done_now"]);
        assert!(find(&history, "one", "  ").await?.is_empty());
        let long = "x ".repeat(MAX_QUERY_BYTES);
        assert!(matches!(
            history.search(&["one".into()], None, &long, None, 10).await,
            Err(Error::InvalidSearch)
        ));
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

    fn record(seq: i64, at: i64, body: &str) -> ServerRecord {
        ServerRecord {
            message: ChatMessage {
                server: "nuschl".into(),
                channel: "3".into(),
                author: "Ada".into(),
                observed_at_ms: at,
                body: body.into(),
            },
            seq,
            edited_ms: None,
            deleted_ms: None,
        }
    }

    async fn seqs(history: &History) -> Result<Vec<Option<i64>>, Error> {
        let page = history.page("nuschl", "3", None, 100).await?;
        Ok(page.messages.iter().map(|entry| entry.seq).collect())
    }

    /// SQLite's external-content full-text index still matches its content.
    async fn index_is_consistent(history: &History) -> Result<(), Error> {
        if let Pool::Sqlite(pool) = &history.pool {
            sqlx::query("INSERT INTO message_fts (message_fts) VALUES ('integrity-check')")
                .execute(pool)
                .await?;
        }
        Ok(())
    }

    /// A Nuschl server's messages in the cache, as both backends run them.
    async fn server_message_checks(history: &History) -> Result<(), Error> {
        // Live messages first, then an older history page: time order.
        let live = history
            .store_server(&[record(5, 500, "five"), record(6, 600, "six")])
            .await?;
        let page: Vec<ServerRecord> = (1..=6)
            .map(|seq| record(seq, seq * 100, &format!("message {seq}")))
            .collect();
        let stored = history.store_server(&page).await?;
        assert_eq!(
            &stored[4..],
            &live[..],
            "seen live and in a page: stored once"
        );
        assert_eq!(seqs(history).await?, (1..=6).map(Some).collect::<Vec<_>>());
        let newest = history.page("nuschl", "3", None, 2).await?;
        assert_eq!(newest.messages[0].message.body, "message 5");
        let older = history.page("nuschl", "3", newest.next_before, 2).await?;
        assert_eq!(older.messages[1].seq, Some(4), "{older:?}");
        let around = history.page_around("nuschl", "3", stored[2], 2).await?;
        assert_eq!(
            around.messages.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Some(3), Some(4)]
        );
        let after = history.page_after("nuschl", "3", stored[3], 100).await?;
        assert_eq!(
            after.messages.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Some(5), Some(6)]
        );
        let search = |query: &'static str| async move {
            let page = history
                .search(&["nuschl".to_string()], Some("3"), query, None, 100)
                .await?;
            Ok::<_, Error>(page.hits.iter().map(|hit| hit.id).collect::<Vec<_>>())
        };
        assert_eq!(search("message 2").await?, [stored[1]]);
        // A hit says its seq, by which a server-history timeline finds it.
        let hits = history
            .search(&["nuschl".to_string()], Some("3"), "message 2", None, 100)
            .await?;
        assert_eq!(hits.hits[0].seq, Some(2));

        // An edit overwrites the text and what search finds.
        assert!(history.edit("nuschl", "3", 2, "edited words", 650).await?);
        assert!(search("message 2").await?.is_empty());
        assert_eq!(search("edited").await?, [stored[1]]);
        // An older edit arriving late changes nothing.
        assert!(!history.edit("nuschl", "3", 2, "older words", 600).await?);
        assert!(search("older").await?.is_empty());
        // A late copy from before the edit does not undo it; a newer one wins.
        history.store_server(&[record(2, 200, "message 2")]).await?;
        let entry = |page: HistoryPage, seq| page.messages.into_iter().find(|e| e.seq == Some(seq));
        let edited = entry(history.page("nuschl", "3", None, 100).await?, 2).expect("seq 2");
        assert_eq!(
            (edited.message.body.as_str(), edited.edited_ms),
            ("edited words", Some(650))
        );
        let mut newer = record(2, 200, "edited again");
        newer.edited_ms = Some(700);
        history.store_server(&[newer]).await?;
        assert_eq!(search("again").await?, [stored[1]]);

        // A deletion leaves a tombstone without text, also in search.
        assert!(history.delete("nuschl", "3", 2, 800).await?);
        let tombstone = entry(history.page("nuschl", "3", None, 100).await?, 2).expect("seq 2");
        assert_eq!(tombstone.message.body, "");
        assert_eq!(tombstone.deleted_ms, Some(800));
        assert!(search("again").await?.is_empty());
        assert!(
            !history.edit("nuschl", "3", 2, "back", 900).await?,
            "a tombstone stays"
        );
        history
            .store_server(&[record(2, 200, "resurrected")])
            .await?;
        let tombstone = entry(history.page("nuschl", "3", None, 100).await?, 2).expect("seq 2");
        assert_eq!(
            (tombstone.message.body.as_str(), tombstone.deleted_ms),
            ("", Some(800))
        );
        // A page's tombstone for a message never seen keeps no text either.
        let mut gone = record(7, 700, "secret");
        gone.deleted_ms = Some(750);
        history.store_server(&[gone]).await?;
        assert!(search("secret").await?.is_empty());
        assert!(!history.edit("nuschl", "3", 99, "x", 1).await?);
        assert!(!history.delete("nuschl", "3", 99, 1).await?);

        // Our own pending message gets its seq; if a page stored it first,
        // the pending copy goes.
        let pending = history
            .append(&ChatMessage {
                server: "nuschl".into(),
                channel: "3".into(),
                author: "Ada".into(),
                observed_at_ms: 1000,
                body: "mine".into(),
            })
            .await?;
        history.set_seq(pending, 8).await?;
        assert_eq!(
            history.store_server(&[record(8, 1001, "mine")]).await?,
            [pending]
        );
        let raced = history
            .append(&ChatMessage {
                server: "nuschl".into(),
                channel: "3".into(),
                author: "Ada".into(),
                observed_at_ms: 1100,
                body: "raced".into(),
            })
            .await?;
        let first = history.store_server(&[record(9, 1101, "raced")]).await?;
        history.set_seq(raced, 9).await?;
        assert_eq!(search("raced").await?, first);
        assert_eq!(seqs(history).await?, (1..=9).map(Some).collect::<Vec<_>>());
        // Records beyond the limits are skipped, not the whole page.
        let mut huge = record(10, 1200, "");
        huge.message.body = "x".repeat(MAX_CONTENT_BYTES + 1);
        assert_eq!(
            history
                .store_server(&[huge, record(11, 1300, "fine")])
                .await?[0],
            0
        );
        assert!(matches!(
            history
                .store_server(&vec![record(12, 1, "x"); MAX_STORE_RECORDS + 1])
                .await,
            Err(Error::InvalidMessage)
        ));
        index_is_consistent(history).await
    }

    #[tokio::test]
    async fn server_messages_on_sqlite() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        // Local observations in the same conversation keep their place by time.
        history
            .append(&ChatMessage {
                server: "nuschl".into(),
                channel: "other".into(),
                author: "Bob".into(),
                observed_at_ms: 0,
                body: "elsewhere".into(),
            })
            .await?;
        server_message_checks(&history).await
    }

    #[tokio::test]
    async fn rows_stored_later_with_older_times_page_in_time_order() -> Result<(), Error> {
        let history = History::connect("sqlite::memory:").await?;
        let at = |observed_at_ms, body: &str| ChatMessage {
            observed_at_ms,
            ..message("one", "ada", body)
        };
        let newest = history.append(&at(300, "third")).await?;
        let oldest = history.append(&at(100, "first")).await?;
        let middle = history.append(&at(200, "second")).await?;
        let tie = history.append(&at(200, "second, later")).await?;
        let page = history.page("one", "general", None, 100).await?;
        let ids = page
            .messages
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [oldest, middle, tie, newest]);
        let first = history.page("one", "general", None, 2).await?;
        assert_eq!(
            first.messages.iter().map(|e| e.id).collect::<Vec<_>>(),
            [tie, newest]
        );
        let rest = history.page("one", "general", first.next_before, 2).await?;
        assert_eq!(
            rest.messages.iter().map(|e| e.id).collect::<Vec<_>>(),
            [oldest, middle]
        );
        assert_eq!(rest.next_before, None);
        let after = history.page_after("one", "general", middle, 100).await?;
        assert_eq!(
            after.messages.iter().map(|e| e.id).collect::<Vec<_>>(),
            [tie, newest]
        );
        Ok(())
    }

    #[test]
    fn queries_become_quoted_full_text_syntax() {
        let query = Query::parse(r#"rust "exact Phrase" it's ser"#);
        assert_eq!(fts5(&query), r#" "rust" "exact phrase" "it s" "ser"*"#);
        assert_eq!(
            tsquery(&query),
            "('rust') & ('exact' <-> 'phrase') & ('it' <-> 's') & ('ser':*)"
        );
        assert_eq!(starts_with("a_b%\\"), "a\\_b\\%\\\\%");
        assert_eq!(
            json_array(&["a\"b\\".into(), "\n".into()]),
            r#"["a\"b\\","\u000a"]"#
        );
    }
}
