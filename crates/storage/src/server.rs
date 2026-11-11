// SPDX-License-Identifier: EUPL-1.2
//! `nuschld`'s persistent state: channels, ACLs, groups, links, registered
//! users (with comments and avatars) and bans. Only the storage worker links
//! this; the server sends typed requests and never SQL. Statements bind every
//! value and work on both SQLite and PostgreSQL.
use crate::BeginWrite as _;
use crate::{Error, Pool};
use nuschl_ipc::server::{
    HistoryVisibility, ServerSnapshot, StoredAcl, StoredBan, StoredChannel, StoredGroup,
    StoredUser, USER_PAGE,
};
use sqlx::{
    migrate::Migrator,
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{collections::HashMap, path::Path, str::FromStr, time::Duration};

mod history;
mod kt;
mod listeners;
mod lock;

pub use lock::OperatorLock;

/// Its own migrations table, so a database can also hold the history schema.
const MIGRATIONS_TABLE: &str = "_nuschld_migrations";

/// Upper bounds the worker enforces on what it stores, whatever the server sends.
pub(crate) const MAX_NAME: usize = 1024;
const MAX_TEXT: usize = 8 * 1024 * 1024;

pub struct ServerStore {
    pool: Pool,
}

fn visibility(text: &str) -> HistoryVisibility {
    match text {
        "since_access" | "since_joining" => HistoryVisibility::SinceAccess,
        _ => HistoryVisibility::Members,
    }
}

fn visibility_text(value: HistoryVisibility) -> &'static str {
    match value {
        HistoryVisibility::Members => "members",
        HistoryVisibility::SinceAccess => "since_access",
    }
}

fn id(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

pub(crate) fn too_long(text: &str, limit: usize) -> Result<(), Error> {
    if text.len() > limit {
        Err(Error::InvalidMessage)
    } else {
        Ok(())
    }
}

type ChannelRow = (
    i64,
    Option<i64>,
    String,
    String,
    i64,
    i64,
    i64,
    String,
    String,
);
type AclRow = (i64, i64, i64, Option<i64>, Option<String>, i64, i64);
type GroupRow = (i64, String, i64, i64);
type MemberRow = (i64, String, i64, i64);
type UserRow = (
    i64,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
type BanRow = (Option<String>, i64, String, String, String, i64, i64, bool);

impl ServerStore {
    pub async fn open(path: &Path) -> Result<Self, Error> {
        Self::sqlite(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
    }

    /// The trusted supervisor supplies this URL from a credential file.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        if url.starts_with("sqlite:") {
            Self::sqlite(SqliteConnectOptions::from_str(url)?.create_if_missing(true)).await
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(Duration::from_secs(5))
                .connect(url)
                .await?;
            let mut migrator: Migrator = sqlx::migrate!("./server-migrations/postgres");
            migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
            migrator.run(&pool).await?;
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
            // Edited and deleted message text is overwritten, not left in free pages.
            .connect_with(
                options
                    .foreign_keys(true)
                    .pragma("trusted_schema", "OFF")
                    .pragma("secure_delete", "ON"),
            )
            .await?;
        let mut migrator: Migrator = sqlx::migrate!("./server-migrations/sqlite");
        migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
        migrator.run(&pool).await?;
        Ok(Self {
            pool: Pool::Sqlite(pool),
        })
    }

    pub async fn load(&self) -> Result<ServerSnapshot, Error> {
        with_pool!(&self.pool, |pool| {
            let channels: Vec<ChannelRow> = sqlx::query_as(
                "SELECT id, parent, name, description, position, max_users, inherit_acl, conversation_id, history_visibility FROM channels ORDER BY id",
            )
            .fetch_all(pool)
            .await?;
            let acls: Vec<AclRow> = sqlx::query_as(
                "SELECT channel, apply_here, apply_subs, user_id, group_name, grant_flags, deny_flags FROM channel_acls ORDER BY channel, priority",
            )
            .fetch_all(pool)
            .await?;
            let groups: Vec<GroupRow> = sqlx::query_as(
                "SELECT channel, name, inherit, inheritable FROM channel_groups ORDER BY channel, name",
            )
            .fetch_all(pool)
            .await?;
            let members: Vec<MemberRow> = sqlx::query_as(
                "SELECT channel, name, user_id, added FROM channel_group_members ORDER BY channel, name, user_id",
            )
            .fetch_all(pool)
            .await?;
            let links: Vec<(i64, i64)> =
                sqlx::query_as("SELECT a, b FROM channel_links ORDER BY a, b")
                    .fetch_all(pool)
                    .await?;
            let bans: Vec<BanRow> = sqlx::query_as(
                "SELECT address, mask, name, hash, reason, start, duration, private_address FROM bans",
            )
            .fetch_all(pool)
            .await?;
            let next_user_id: Option<i64> =
                sqlx::query_scalar("SELECT value FROM server_meta WHERE key = 'next_user_id'")
                    .fetch_optional(pool)
                    .await?;
            let mut snapshot = ServerSnapshot {
                next_user_id: next_user_id.map_or(0, id),
                ..ServerSnapshot::default()
            };
            let mut index = HashMap::new();
            for (
                id_,
                parent,
                name,
                description,
                position,
                max_users,
                inherit_acl,
                conversation_id,
                history,
            ) in channels
            {
                index.insert(id_, snapshot.channels.len());
                snapshot.channels.push(StoredChannel {
                    id: id(id_),
                    parent: parent.map(id),
                    name,
                    description,
                    position: i32::try_from(position).unwrap_or(0),
                    max_users: id(max_users),
                    inherit_acl: inherit_acl != 0,
                    conversation_id,
                    history_visibility: visibility(&history),
                    acls: Vec::new(),
                    groups: Vec::new(),
                });
            }
            for (channel, apply_here, apply_subs, user_id, group, grant, deny) in acls {
                if let Some(&at) = index.get(&channel) {
                    snapshot.channels[at].acls.push(StoredAcl {
                        apply_here: apply_here != 0,
                        apply_subs: apply_subs != 0,
                        user_id: user_id.map(id),
                        group,
                        grant: id(grant),
                        deny: id(deny),
                    });
                }
            }
            for (channel, name, inherit, inheritable) in groups {
                if let Some(&at) = index.get(&channel) {
                    snapshot.channels[at].groups.push(StoredGroup {
                        name,
                        inherit: inherit != 0,
                        inheritable: inheritable != 0,
                        add: Vec::new(),
                        remove: Vec::new(),
                    });
                }
            }
            for (channel, name, user_id, added) in members {
                let Some(&at) = index.get(&channel) else {
                    continue;
                };
                if let Some(group) = snapshot.channels[at]
                    .groups
                    .iter_mut()
                    .find(|group| group.name == name)
                {
                    if added != 0 {
                        group.add.push(id(user_id));
                    } else {
                        group.remove.push(id(user_id));
                    }
                }
            }
            snapshot.links = links.into_iter().map(|(a, b)| (id(a), id(b))).collect();
            snapshot.bans = bans
                .into_iter()
                .map(
                    |(address, mask, name, hash, reason, start, duration, private_address)| {
                        StoredBan {
                            private_address,
                            address,
                            mask: u8::try_from(mask).unwrap_or(128),
                            name,
                            hash,
                            reason,
                            start,
                            duration: id(duration),
                        }
                    },
                )
                .collect();
            Ok(snapshot)
        })
    }

    pub async fn load_users(&self, after: Option<u32>) -> Result<Vec<StoredUser>, Error> {
        let after = after.map_or(-1, i64::from);
        let rows: Vec<UserRow> = with_pool!(&self.pool, |pool| {
            sqlx::query_as(
                "SELECT id, account, name, password_hash, certificate_hash, last_channel, last_active FROM users WHERE id > $1 ORDER BY id LIMIT $2",
            )
            .bind(after)
            .bind(i64::from(USER_PAGE))
            .fetch_all(pool)
            .await?
        });
        Ok(rows
            .into_iter()
            .map(
                |(
                    id_,
                    account,
                    name,
                    password_hash,
                    certificate_hash,
                    last_channel,
                    last_active,
                )| {
                    StoredUser {
                        id: id(id_),
                        account,
                        name,
                        password_hash,
                        certificate_hash,
                        last_channel: last_channel.map(id),
                        last_active,
                    }
                },
            )
            .collect())
    }

    /// Insert or replace a channel together with its ACLs and groups.
    pub async fn save_channel(&self, channel: &StoredChannel) -> Result<(), Error> {
        too_long(&channel.name, MAX_NAME)?;
        too_long(&channel.description, MAX_TEXT)?;
        too_long(&channel.conversation_id, 64)?;
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            sqlx::query(
                "INSERT INTO channels (id, parent, name, description, position, max_users, inherit_acl, conversation_id, history_visibility) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (id) DO UPDATE SET parent = excluded.parent, name = excluded.name, description = excluded.description, position = excluded.position, max_users = excluded.max_users, inherit_acl = excluded.inherit_acl, conversation_id = excluded.conversation_id, history_visibility = excluded.history_visibility",
            )
            .bind(i64::from(channel.id))
            .bind(channel.parent.map(i64::from))
            .bind(&channel.name)
            .bind(&channel.description)
            .bind(i64::from(channel.position))
            .bind(i64::from(channel.max_users))
            .bind(i64::from(channel.inherit_acl))
            .bind(&channel.conversation_id)
            .bind(visibility_text(channel.history_visibility))
            .execute(&mut *transaction)
            .await?;
            for statement in [
                "DELETE FROM channel_acls WHERE channel = $1",
                "DELETE FROM channel_group_members WHERE channel = $1",
                "DELETE FROM channel_groups WHERE channel = $1",
            ] {
                sqlx::query(statement)
                    .bind(i64::from(channel.id))
                    .execute(&mut *transaction)
                    .await?;
            }
            for (priority, acl) in channel.acls.iter().enumerate() {
                if let Some(group) = &acl.group {
                    too_long(group, MAX_NAME)?;
                }
                sqlx::query(
                    "INSERT INTO channel_acls (channel, priority, apply_here, apply_subs, user_id, group_name, grant_flags, deny_flags) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(i64::from(channel.id))
                .bind(priority as i64)
                .bind(i64::from(acl.apply_here))
                .bind(i64::from(acl.apply_subs))
                .bind(acl.user_id.map(i64::from))
                .bind(&acl.group)
                .bind(i64::from(acl.grant))
                .bind(i64::from(acl.deny))
                .execute(&mut *transaction)
                .await?;
            }
            for group in &channel.groups {
                too_long(&group.name, MAX_NAME)?;
                sqlx::query("INSERT INTO channel_groups (channel, name, inherit, inheritable) VALUES ($1, $2, $3, $4)")
                    .bind(i64::from(channel.id))
                    .bind(&group.name)
                    .bind(i64::from(group.inherit))
                    .bind(i64::from(group.inheritable))
                    .execute(&mut *transaction)
                    .await?;
                for (members, added) in [(&group.add, 1_i64), (&group.remove, 0)] {
                    for member in members {
                        sqlx::query(
                            "INSERT INTO channel_group_members (channel, name, user_id, added) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                        )
                        .bind(i64::from(channel.id))
                        .bind(&group.name)
                        .bind(i64::from(*member))
                        .bind(added)
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
            }
            transaction.commit().await?;
            Ok(())
        })
    }

    /// Removes the channel, its descendants, ACLs, groups and links (cascade).
    pub async fn delete_channel(&self, channel: u32) -> Result<(), Error> {
        self.execute("DELETE FROM channels WHERE id = $1", &[i64::from(channel)])
            .await
    }

    pub async fn link(&self, a: u32, b: u32) -> Result<(), Error> {
        let (a, b) = (a.min(b), a.max(b));
        self.execute(
            "INSERT INTO channel_links (a, b) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[i64::from(a), i64::from(b)],
        )
        .await
    }

    pub async fn unlink(&self, a: u32, b: u32) -> Result<(), Error> {
        let (a, b) = (a.min(b), a.max(b));
        self.execute(
            "DELETE FROM channel_links WHERE a = $1 AND b = $2",
            &[i64::from(a), i64::from(b)],
        )
        .await
    }

    async fn execute(&self, statement: &'static str, values: &[i64]) -> Result<(), Error> {
        with_pool!(&self.pool, |pool| {
            let mut query = sqlx::query(statement);
            for value in values {
                query = query.bind(*value);
            }
            query.execute(pool).await?;
            Ok(())
        })
    }

    pub async fn save_user(&self, user: &StoredUser) -> Result<(), Error> {
        too_long(&user.name, MAX_NAME)?;
        for value in [&user.password_hash, &user.certificate_hash, &user.account]
            .into_iter()
            .flatten()
        {
            too_long(value, MAX_NAME)?;
        }
        with_pool!(&self.pool, |pool| {
            sqlx::query(
                "INSERT INTO users (id, name, name_key, password_hash, certificate_hash, last_channel, last_active, account) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name, name_key = excluded.name_key, password_hash = excluded.password_hash, certificate_hash = excluded.certificate_hash, last_channel = excluded.last_channel, last_active = excluded.last_active, account = excluded.account",
            )
            .bind(i64::from(user.id))
            .bind(&user.name)
            .bind(user.name.to_lowercase())
            .bind(&user.password_hash)
            .bind(&user.certificate_hash)
            .bind(user.last_channel.map(i64::from))
            .bind(user.last_active)
            .bind(&user.account)
            .execute(pool)
            .await?;
            Ok(())
        })
    }

    pub async fn delete_user(&self, user: u32) -> Result<(), Error> {
        self.execute("DELETE FROM users WHERE id = $1", &[i64::from(user)])
            .await
    }

    pub async fn set_comment(&self, user: u32, comment: &str) -> Result<(), Error> {
        self.set_blob("UPDATE users SET comment = $2 WHERE id = $1", user, comment)
            .await
    }

    pub async fn set_texture(&self, user: u32, texture: &str) -> Result<(), Error> {
        self.set_blob("UPDATE users SET texture = $2 WHERE id = $1", user, texture)
            .await
    }

    async fn set_blob(&self, statement: &'static str, user: u32, value: &str) -> Result<(), Error> {
        too_long(value, MAX_TEXT)?;
        with_pool!(&self.pool, |pool| {
            sqlx::query(statement)
                .bind(i64::from(user))
                .bind(value)
                .execute(pool)
                .await?;
            Ok(())
        })
    }

    pub async fn user_blobs(&self, user: u32) -> Result<(String, String), Error> {
        let row: Option<(String, String)> = with_pool!(&self.pool, |pool| {
            sqlx::query_as("SELECT comment, texture FROM users WHERE id = $1")
                .bind(i64::from(user))
                .fetch_optional(pool)
                .await?
        });
        Ok(row.unwrap_or_default())
    }

    /// Only ever raised, so registration ids are never handed out twice.
    pub async fn set_next_user_id(&self, next: u32) -> Result<(), Error> {
        self.execute(
            "INSERT INTO server_meta (key, value) VALUES ('next_user_id', $1) \
             ON CONFLICT (key) DO UPDATE SET value = CASE WHEN excluded.value > server_meta.value THEN excluded.value ELSE server_meta.value END",
            &[i64::from(next)],
        )
        .await
    }

    pub async fn save_bans(&self, bans: &[StoredBan]) -> Result<(), Error> {
        with_pool!(&self.pool, |pool| {
            let mut transaction = pool.begin_write().await?;
            sqlx::query("DELETE FROM bans")
                .execute(&mut *transaction)
                .await?;
            for ban in bans {
                for text in [&ban.name, &ban.hash, &ban.reason] {
                    too_long(text, MAX_NAME)?;
                }
                sqlx::query(
                    "INSERT INTO bans (address, mask, name, hash, reason, start, duration, private_address) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(&ban.address)
                .bind(i64::from(ban.mask))
                .bind(&ban.name)
                .bind(&ban.hash)
                .bind(&ban.reason)
                .bind(ban.start)
                .bind(i64::from(ban.duration))
                .bind(ban.private_address)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: u32, parent: Option<u32>) -> StoredChannel {
        StoredChannel {
            id,
            parent,
            name: format!("channel {id}"),
            description: String::new(),
            position: 0,
            max_users: 0,
            inherit_acl: true,
            conversation_id: format!("{id:032x}"),
            history_visibility: HistoryVisibility::SinceAccess,
            acls: vec![StoredAcl {
                apply_here: true,
                apply_subs: false,
                user_id: None,
                group: Some("admin".into()),
                grant: 1,
                deny: 0,
            }],
            groups: vec![StoredGroup {
                name: "admin".into(),
                inherit: true,
                inheritable: true,
                add: vec![1, 2],
                remove: vec![3],
            }],
        }
    }

    #[tokio::test]
    async fn state_survives_reopening_and_deletes_cascade() -> Result<(), Error> {
        let directory = tempfile::tempdir().map_err(|_| Error::InvalidMessage)?;
        let path = directory.path().join("server.sqlite3");
        {
            let store = ServerStore::open(&path).await?;
            // The history schema can share the database.
            crate::History::open(&path).await?;
            store.save_channel(&channel(0, None)).await?;
            store.save_channel(&channel(1, Some(0))).await?;
            store.save_channel(&channel(2, Some(1))).await?;
            store.link(2, 0).await?;
            store
                .save_user(&StoredUser {
                    id: 1,
                    account: Some("ab".repeat(16)),
                    name: "Alice".into(),
                    password_hash: None,
                    certificate_hash: Some("ab".into()),
                    last_channel: Some(2),
                    last_active: Some(5),
                })
                .await?;
            store.set_comment(1, "hello").await?;
            store.set_next_user_id(7).await?;
            store.set_next_user_id(3).await?;
            store
                .save_bans(&[StoredBan {
                    private_address: true,
                    address: Some("::ffff:10.0.0.1".into()),
                    mask: 128,
                    name: "Mallory".into(),
                    hash: String::new(),
                    reason: "spam".into(),
                    start: 1,
                    duration: 0,
                }])
                .await?;
        }
        let store = ServerStore::open(&path).await?;
        crate::History::open(&path).await?;
        let snapshot = store.load().await?;
        assert_eq!(snapshot.channels.len(), 3);
        assert_eq!(snapshot.channels[2], channel(2, Some(1)));
        assert_eq!(snapshot.links, vec![(0, 2)]);
        assert_eq!(snapshot.bans.len(), 1);
        assert!(snapshot.bans[0].private_address);
        assert_eq!(snapshot.next_user_id, 7, "the high mark never goes down");
        let users = store.load_users(None).await?;
        assert_eq!(users[0].name, "Alice");
        assert_eq!(users[0].account, Some("ab".repeat(16)));
        assert_eq!(store.user_blobs(1).await?.0, "hello");
        assert!(store.load_users(Some(1)).await?.is_empty());
        store.delete_channel(1).await?;
        let snapshot = store.load().await?;
        assert_eq!(snapshot.channels.len(), 1, "children cascade");
        assert!(snapshot.links.is_empty());
        Ok(())
    }
}
