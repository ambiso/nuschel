// SPDX-License-Identifier: EUPL-1.2
//! The key-transparency table (docs/e2ee.md §5.2): an ordered key-value store
//! with atomic batches for nuschel-kt's directory and `nuscheld`'s records
//! behind each binding. Keys and values are opaque bytes (base64 on the IPC);
//! the worker bounds their sizes and counts but never interprets them.
use super::ServerStore;
use crate::BeginWrite as _;
use crate::{Error, Pool};
use base64::Engine as _;
use nuschel_ipc::server::{MAX_KT_KEY_BYTES, MAX_KT_RECORDS, MAX_KT_VALUE_BYTES};

fn decode(text: &str, limit: usize) -> Result<Vec<u8>, Error> {
    // Base64 is 4/3 of the bytes; refuse before decoding anything larger.
    if text.len() > limit.div_ceil(3) * 4 {
        return Err(Error::InvalidMessage);
    }
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()
        .filter(|bytes| bytes.len() <= limit)
        .ok_or(Error::InvalidMessage)
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The smallest key above every key starting with `prefix`, if there is one
/// (`None` for an empty prefix or one of only 0xff bytes).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

impl ServerStore {
    pub async fn kt_get(&self, keys: &[String]) -> Result<Vec<Option<String>>, Error> {
        if keys.len() > MAX_KT_RECORDS {
            return Err(Error::InvalidMessage);
        }
        let keys = keys
            .iter()
            .map(|key| decode(key, MAX_KT_KEY_BYTES))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(keys.len());
        with_pool!(&self.pool, |pool| {
            for key in &keys {
                let value: Option<(Vec<u8>,)> =
                    sqlx::query_as("SELECT value FROM kt_records WHERE key = $1")
                        .bind(key)
                        .fetch_optional(pool)
                        .await?;
                out.push(value.map(|(value,)| encode(&value)));
            }
        });
        Ok(out)
    }

    pub async fn kt_scan(&self, prefix: &str) -> Result<Vec<(String, String)>, Error> {
        let prefix = decode(prefix, MAX_KT_KEY_BYTES)?;
        let limit = i64::try_from(MAX_KT_RECORDS).unwrap_or(i64::MAX) + 1;
        let rows: Vec<(Vec<u8>, Vec<u8>)> = with_pool!(&self.pool, |pool| {
            match prefix_end(&prefix) {
                Some(end) => {
                    sqlx::query_as(
                        "SELECT key, value FROM kt_records WHERE key >= $1 AND key < $2 ORDER BY key LIMIT $3",
                    )
                    .bind(&prefix)
                    .bind(end)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?
                }
                None => {
                    sqlx::query_as(
                        "SELECT key, value FROM kt_records WHERE key >= $1 ORDER BY key LIMIT $2",
                    )
                    .bind(&prefix)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?
                }
            }
        });
        // A scan the server could not take in one answer is an error, never a silent
        // truncation of a label's versions.
        if rows.len() > MAX_KT_RECORDS {
            return Err(Error::InvalidMessage);
        }
        Ok(rows
            .into_iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| (encode(&key), encode(&value)))
            .collect())
    }

    pub async fn kt_write(&self, records: &[(String, String)]) -> Result<(), Error> {
        if records.len() > MAX_KT_RECORDS {
            return Err(Error::InvalidMessage);
        }
        let records = records
            .iter()
            .map(|(key, value)| {
                Ok((
                    decode(key, MAX_KT_KEY_BYTES)?,
                    decode(value, MAX_KT_VALUE_BYTES)?,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            for (key, value) in &records {
                // An empty value deletes the key (nuschel-kt never writes one).
                if value.is_empty() {
                    sqlx::query("DELETE FROM kt_records WHERE key = $1")
                        .bind(key)
                        .execute(&mut *transaction)
                        .await?;
                    continue;
                }
                sqlx::query(
                    "INSERT INTO kt_records (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                )
                .bind(key)
                .bind(value)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_ends() {
        assert_eq!(prefix_end(b"v\x00\x05"), Some(b"v\x00\x06".to_vec()));
        assert_eq!(prefix_end(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xff\xff"), None);
        assert_eq!(prefix_end(b""), None);
    }

    #[tokio::test]
    async fn records_are_ordered_scanned_by_prefix_and_written_atomically() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(|_| Error::InvalidMessage)?;
        let store = ServerStore::open(&directory.path().join("server.sqlite3")).await?;
        let pair = |key: &[u8], value: &[u8]| (encode(key), encode(value));
        store
            .kt_write(&[
                pair(b"v\x01b", b"2"),
                pair(b"v\x01a", b"1"),
                pair(b"v\x02a", b"3"),
                pair(b"\xff\xff", b"4"),
            ])
            .await?;
        // Overwrite.
        store.kt_write(&[pair(b"v\x01a", b"one")]).await?;
        let scanned = store.kt_scan(&encode(b"v\x01")).await?;
        assert_eq!(
            scanned,
            vec![pair(b"v\x01a", b"one"), pair(b"v\x01b", b"2")]
        );
        assert_eq!(store.kt_scan(&encode(b"\xff")).await?.len(), 1);
        let got = store
            .kt_get(&[encode(b"v\x02a"), encode(b"missing")])
            .await?;
        assert_eq!(got, vec![Some(encode(b"3")), None]);
        // A batch with one invalid record writes nothing.
        assert!(
            store
                .kt_write(&[pair(b"new", b"x"), (encode(b"bad"), "not base64!".into())])
                .await
                .is_err()
        );
        assert_eq!(store.kt_get(&[encode(b"new")]).await?, vec![None]);
        // Oversized keys are refused before decoding.
        assert!(store.kt_get(&[encode(&[0; 2000])]).await.is_err());
        // An empty value deletes the key, in the same transaction as the rest.
        store
            .kt_write(&[pair(b"v\x01a", b""), pair(b"v\x03", b"5")])
            .await?;
        assert_eq!(
            store.kt_get(&[encode(b"v\x01a"), encode(b"v\x03")]).await?,
            vec![None, Some(encode(b"5"))]
        );
        assert_eq!(store.kt_scan(&encode(b"v\x01")).await?.len(), 1);
        Ok(())
    }
}
