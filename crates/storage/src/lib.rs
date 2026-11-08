// SPDX-License-Identifier: EUPL-1.2
//! SQLite implementation. Only the storage worker may link this crate.

use std::path::Path;

use nuschel_core::{ChatMessage, MAX_CONTENT_BYTES};
use rusqlite::{Connection, params};
use thiserror::Error;

const MAX_PAGE_MESSAGES: usize = 100;
const MAX_PAGE_TEXT_BYTES: usize = 96 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("history message exceeds the storage limits")]
    InvalidMessage,
    #[error("history database schema version {0} is newer than this application supports")]
    FutureSchema(i64),
}

pub struct History {
    connection: Connection,
}

impl History {
    pub fn open(path: &Path) -> Result<Self, Error> {
        Self::from_connection(Connection::open(path)?)
    }

    fn from_connection(connection: Connection) -> Result<Self, Error> {
        connection.execute_batch("PRAGMA trusted_schema = OFF; PRAGMA foreign_keys = ON;")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > 1 {
            return Err(Error::FutureSchema(version));
        }
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY,
                 server TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 author TEXT NOT NULL,
                 observed_at_ms INTEGER NOT NULL,
                 body TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS messages_channel ON messages (server, channel, id DESC);
             PRAGMA user_version = 1;",
        )?;
        Ok(Self { connection })
    }

    pub fn append(&self, message: &ChatMessage) -> Result<i64, Error> {
        if message.body.len() > MAX_CONTENT_BYTES
            || [&message.server, &message.channel, &message.author]
                .iter()
                .any(|value| value.len() > 512)
        {
            return Err(Error::InvalidMessage);
        }
        self.connection.execute(
            "INSERT INTO messages (server, channel, author, observed_at_ms, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![message.server, message.channel, message.author, message.observed_at_ms, message.body],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Return a bounded page in display order, newest page first. Never deduplicate by text.
    pub fn recent(
        &self,
        server: &str,
        channel: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT server, channel, author, observed_at_ms, body FROM messages
             WHERE server = ?1 AND channel = ?2 ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![server, channel, limit.min(MAX_PAGE_MESSAGES) as i64],
            |row| {
                Ok(ChatMessage {
                    server: row.get(0)?,
                    channel: row.get(1)?,
                    author: row.get(2)?,
                    observed_at_ms: row.get(3)?,
                    body: row.get(4)?,
                })
            },
        )?;
        let mut messages = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let message = row?;
            let size = message.server.len()
                + message.channel.len()
                + message.author.len()
                + message.body.len();
            if size > MAX_PAGE_TEXT_BYTES - bytes {
                break;
            }
            bytes += size;
            messages.push(message);
        }
        messages.reverse();
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_preserves_multiline_messages_and_server_boundaries() -> Result<(), Error> {
        let history = History::from_connection(Connection::open_in_memory()?)?;
        let message = ChatMessage {
            server: "trusted-server-a".into(),
            channel: "general".into(),
            author: "someone".into(),
            observed_at_ms: 1,
            body: "first\n\n```rust\nfn main() {}\n```".into(),
        };
        history.append(&message)?;
        history.append(&message)?;
        assert_eq!(
            history.recent("trusted-server-a", "general", 10)?,
            vec![message.clone(), message]
        );
        assert!(
            history
                .recent("trusted-server-b", "general", 10)?
                .is_empty()
        );
        Ok(())
    }
}
