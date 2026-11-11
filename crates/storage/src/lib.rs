// SPDX-License-Identifier: EUPL-1.2
//! Asynchronous SQLx persistence. Only the storage worker may link this crate.

use nuschel_core::search::{MAX_QUERY_BYTES, Query};
use nuschel_core::{
    ChatMessage, HistoryEntry, HistoryPage, MAX_CONTENT_BYTES, SearchCursor, SearchPage,
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

type Row = (i64, String, String, String, i64, String);
type RankedRow = (i64, String, String, String, i64, String, i64);

const PAGE: &str = "SELECT id, server, channel, author, observed_at_ms, body FROM messages WHERE server = $1 AND channel = $2 AND id < $3 ORDER BY id DESC LIMIT $4";
const PAGE_AFTER: &str = "SELECT id, server, channel, author, observed_at_ms, body FROM messages WHERE server = $1 AND channel = $2 AND id > $3 ORDER BY id LIMIT $4";
const INSERT_SEARCH: &str =
    "INSERT INTO message_search (id, server, author, visible) VALUES ($1, $2, $3, $4)";
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
const SEARCH_SQLITE: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, page.score FROM (
    SELECT id, score FROM (
        SELECT s.id AS id, CAST(1000000000.0 / (1.0 + length(s.visible) / 1000.0) / (1.0 + MAX(0, $2 - m.observed_at_ms) / 2592000000.0) AS INTEGER) AS score
        FROM message_fts JOIN message_search s ON s.id = message_fts.rowid JOIN messages m ON m.id = s.id
        WHERE message_fts MATCH $1 AND s.server IN (SELECT value FROM json_each($3))
            AND ($4 IS NULL OR m.channel = $4) AND s.author LIKE $5 ESCAPE '\'
    ) WHERE score < $6 OR (score = $6 AND id < $7) ORDER BY score DESC, id DESC LIMIT $8
) page JOIN messages m ON m.id = page.id ORDER BY page.score DESC, page.id DESC";
const SEARCH_POSTGRES: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, page.score FROM (
    SELECT id, score FROM (
        SELECT s.id AS id, (ts_rank(s.document, q, 1)::float8 * 1000000000.0::float8 / (1.0::float8 + GREATEST(0, $2 - m.observed_at_ms)::float8 / 2592000000.0::float8))::bigint AS score
        FROM message_search s JOIN messages m ON m.id = s.id, to_tsquery('simple', $1) q
        WHERE s.document @@ q AND s.server = ANY($3)
            AND ($4::text IS NULL OR m.channel = $4) AND s.author LIKE $5 ESCAPE '\'
    ) hits WHERE score < $6 OR (score = $6 AND id < $7) ORDER BY score DESC, id DESC LIMIT $8
) page JOIN messages m ON m.id = page.id ORDER BY page.score DESC, page.id DESC";
// `from:` alone: that author's messages, newest first. $1 the servers, $2
// the channel or NULL, $3 the author pattern, $4 the cursor's ID, $5 the limit.
const AUTHOR_SQLITE: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, 0 FROM message_search s JOIN messages m ON m.id = s.id
    WHERE s.server IN (SELECT value FROM json_each($1)) AND ($2 IS NULL OR m.channel = $2)
        AND s.author LIKE $3 ESCAPE '\' AND s.id < $4 ORDER BY s.id DESC LIMIT $5";
const AUTHOR_POSTGRES: &str = r"SELECT m.id, m.server, m.channel, m.author, m.observed_at_ms, m.body, 0::bigint FROM message_search s JOIN messages m ON m.id = s.id
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

fn entry((id, server, channel, author, observed_at_ms, body): Row) -> HistoryEntry {
    HistoryEntry {
        id,
        message: ChatMessage {
            server,
            channel,
            author,
            observed_at_ms,
            body,
        },
    }
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
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
        if message.body.len() > MAX_CONTENT_BYTES
            || [&message.server, &message.channel, &message.author]
                .iter()
                .any(|value| value.len() > 512)
        {
            return Err(Error::InvalidMessage);
        }
        let index = self.index_statement();
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
            sqlx::query(index)
                .bind(id)
                .bind(id + 1)
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            Ok(id)
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
        let now_ms = after.map_or_else(now_ms, |cursor| cursor.now_ms);
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
        for (id, server, channel, author, observed_at_ms, body, score) in
            rows.into_iter().take(limit)
        {
            let hit = entry((id, server, channel, author, observed_at_ms, visible(&body)));
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
        cursor: i64,
        limit: usize,
    ) -> Result<Vec<Row>, Error> {
        Ok(with_pool!(&self.pool, |pool| {
            sqlx::query_as(statement)
                .bind(server)
                .bind(channel)
                .bind(cursor)
                .bind(limit as i64)
                .fetch_all(pool)
                .await?
        }))
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
        let limit = limit.clamp(1, MAX_PAGE_MESSAGES);
        // A plain bound, not `$3 IS NULL OR id < $3`, lets the index seek to it.
        let before = before.unwrap_or(i64::MAX);
        let rows = self
            .rows(PAGE, (server, channel), before, limit + 1)
            .await?;
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
        let rows = self
            .rows(PAGE_AFTER, (server, channel), after, limit + 1)
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
        let older = self
            .rows(PAGE, (server, channel), id.saturating_add(1), half + 1)
            .await?;
        let newer = self
            .rows(PAGE_AFTER, (server, channel), id, half + 1)
            .await?;
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
        // The old page query, `(?3 IS NULL OR id < ?3)`, sought only the
        // server and channel, then filtered every newer message by ID.
        for (query, seek) in [
            (
                PAGE,
                "INDEX messages_channel (server=? AND channel=? AND id<?)",
            ),
            (
                PAGE_AFTER,
                "INDEX messages_channel (server=? AND channel=? AND id>?)",
            ),
        ] {
            let plan = plan(pool, query, &["server", "general", "1", "1"]).await?;
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
        let now = now_ms();
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

    /// `NUSCHEL_TEST_POSTGRES_URL=postgresql://... cargo test -p nuschel-storage
    /// -- --ignored` runs the same checks on PostgreSQL. The database must be a
    /// throwaway one: its history tables are dropped first.
    #[tokio::test]
    #[ignore = "needs a throwaway PostgreSQL database"]
    async fn search_on_postgres() -> Result<(), Error> {
        let url = std::env::var("NUSCHEL_TEST_POSTGRES_URL").expect("NUSCHEL_TEST_POSTGRES_URL");
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
