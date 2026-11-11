// SPDX-License-Identifier: EUPL-1.2
//! Registered users' channel listeners (Murmur's `channel_listeners`): kept
//! across reconnects, removed with the user or the channel (cascade).
use super::ServerStore;
use crate::{Error, Pool};

/// Listeners kept per user; Murmur's `listenersperuser` bounds them earlier.
const MAX_PER_USER: i64 = 4096;

impl ServerStore {
    pub async fn set_listener(&self, user: u32, channel: u32, volume: f32) -> Result<(), Error> {
        if !volume.is_finite() {
            return Err(Error::InvalidMessage);
        }
        with_pool!(&self.pool, |pool| {
            sqlx::query(
                "INSERT INTO channel_listeners (user_id, channel, volume) VALUES ($1, $2, $3) \
                 ON CONFLICT (user_id, channel) DO UPDATE SET volume = excluded.volume",
            )
            .bind(i64::from(user))
            .bind(i64::from(channel))
            .bind(f64::from(volume))
            .execute(pool)
            .await?;
            Ok(())
        })
    }

    pub async fn remove_listener(&self, user: u32, channel: u32) -> Result<(), Error> {
        with_pool!(&self.pool, |pool| {
            sqlx::query("DELETE FROM channel_listeners WHERE user_id = $1 AND channel = $2")
                .bind(i64::from(user))
                .bind(i64::from(channel))
                .execute(pool)
                .await?;
            Ok(())
        })
    }

    /// Channel and volume, by channel.
    pub async fn user_listeners(&self, user: u32) -> Result<Vec<(u32, f32)>, Error> {
        let rows: Vec<(i64, f64)> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(
                "SELECT channel, volume FROM channel_listeners WHERE user_id = $1 ORDER BY channel LIMIT $2",
            )
            .bind(i64::from(user))
            .bind(MAX_PER_USER)
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .filter_map(|(channel, volume)| Some((u32::try_from(channel).ok()?, volume as f32)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschel_ipc::server::{HistoryVisibility, StoredChannel, StoredUser};

    fn channel(id: u32) -> StoredChannel {
        StoredChannel {
            id,
            parent: (id != 0).then_some(0),
            name: format!("channel {id}"),
            description: String::new(),
            position: 0,
            max_users: 0,
            inherit_acl: true,
            conversation_id: format!("{id:032x}"),
            history_visibility: HistoryVisibility::Members,
            acls: Vec::new(),
            groups: Vec::new(),
        }
    }

    #[tokio::test]
    async fn listeners_are_kept_and_cascade() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(|_| Error::InvalidMessage)?;
        let store = ServerStore::open(&directory.path().join("server.sqlite3")).await?;
        for id in [0, 1, 2] {
            store.save_channel(&channel(id)).await?;
        }
        store
            .save_user(&StoredUser {
                id: 4,
                account: None,
                name: "Alice".into(),
                password_hash: None,
                certificate_hash: None,
                last_channel: None,
                last_active: None,
            })
            .await?;
        store.set_listener(4, 1, 1.0).await?;
        store.set_listener(4, 2, 0.5).await?;
        store.set_listener(4, 2, 0.25).await?;
        assert_eq!(store.user_listeners(4).await?, [(1, 1.0), (2, 0.25)]);
        // Unknown users and channels are refused by the foreign keys.
        assert!(store.set_listener(5, 1, 1.0).await.is_err());
        assert!(store.set_listener(4, 9, 1.0).await.is_err());
        assert!(store.set_listener(4, 1, f32::NAN).await.is_err());
        store.remove_listener(4, 1).await?;
        store.delete_channel(2).await?;
        assert!(store.user_listeners(4).await?.is_empty());
        store.set_listener(4, 0, 1.0).await?;
        store.delete_user(4).await?;
        assert!(store.user_listeners(4).await?.is_empty());
        Ok(())
    }
}
