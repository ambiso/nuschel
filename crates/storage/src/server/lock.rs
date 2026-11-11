// SPDX-License-Identifier: EUPL-1.2
//! The operator lock: a running `nuschld` (its state worker) and `nuschld
//! admin` each hold it for as long as they use the database, because both keep
//! the server state in memory and write it back; two of them on one database
//! would overwrite each other. It is taken before the database is opened, so a
//! refused command never applies migrations under a running server.
//!
//! SQLite: an exclusive `flock` on `<database>.lock` next to the file (empty,
//! created readable; if it cannot be opened or locked at all the worker warns
//! and goes on, rather than keep a server from starting). PostgreSQL: a session advisory lock on a connection of its own, which
//! PostgreSQL releases when the connection (the worker process) ends.
use crate::Error;
use sqlx::{Connection, PgConnection, sqlite::SqliteConnectOptions};
use std::{path::Path, str::FromStr};

/// `pg_try_advisory_lock` key: "nuschld" in ASCII.
const ADVISORY_KEY: i64 = 0x6e75_7363_6865_6c64;

/// Held until dropped (or the process ends).
pub enum OperatorLock {
    File(std::fs::File),
    Postgres(PgConnection),
    /// An in-memory database, or a file system without locks (with a warning).
    Unlocked,
}

impl OperatorLock {
    /// Lock a SQLite database file; `None` if another process holds it.
    pub fn sqlite(path: &Path) -> Result<Option<Self>, Error> {
        let mut name = path.as_os_str().to_owned();
        name.push(".lock");
        let name = Path::new(&name);
        // The file holds nothing; readable, so that whoever created it (an
        // operator as root, say) never keeps the server from locking it. A
        // shared lock needs no write access.
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o644);
        let opened = options.open(name).or_else(|_| std::fs::File::open(name));
        let file = match opened {
            Ok(file) => file,
            Err(error) => {
                eprintln!(
                    "nuschl-storage-worker: cannot open {}: {error}; continuing without the operator lock",
                    name.display()
                );
                return Ok(Some(Self::Unlocked));
            }
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(Self::File(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                // Refusing to start on a file system without locks would cost
                // availability; say so instead.
                eprintln!(
                    "nuschl-storage-worker: cannot lock {}: {error}; continuing without the operator lock",
                    name.display()
                );
                Ok(Some(Self::Unlocked))
            }
        }
    }

    /// Lock the database behind a URL; `None` if another process holds it.
    pub async fn url(url: &str) -> Result<Option<Self>, Error> {
        if url.starts_with("sqlite:") {
            let options = SqliteConnectOptions::from_str(url)?;
            let path = options.get_filename();
            if path.as_os_str().is_empty() || path == Path::new(":memory:") {
                return Ok(Some(Self::Unlocked));
            }
            Self::sqlite(path)
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let mut connection = PgConnection::connect(url).await?;
            let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(ADVISORY_KEY)
                .fetch_one(&mut connection)
                .await?;
            Ok(locked.then_some(Self::Postgres(connection)))
        } else {
            Err(Error::UnsupportedDatabase)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.sqlite3");
        let first = OperatorLock::sqlite(&path).unwrap();
        assert!(matches!(first, Some(OperatorLock::File(_))));
        assert!(OperatorLock::sqlite(&path).unwrap().is_none());
        drop(first);
        assert!(OperatorLock::sqlite(&path).unwrap().is_some());
    }
}
