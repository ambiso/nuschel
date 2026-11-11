// SPDX-License-Identifier: EUPL-1.2
//! Reads a Murmur (mumble-server) SQLite database for `nuscheld
//! --import-murmur`. The file is untrusted: it is opened read-only with
//! `query_only`, `trusted_schema = OFF`, cell size checks and a heap limit,
//! checked with `quick_check`, and read with fixed statements in pages by
//! rowid. Every value is decoded by its actual SQLite type, so a malformed row
//! (text where Murmur writes integers, invalid UTF-8, an oversized blob) is
//! skipped and reported instead of failing the import. Interpretation and
//! validation happen in `nuscheld` (apps/server/src/murmur.rs).
//!
//! Two layouts exist. `ServerDB.cpp` (Murmur 1.2.4 to 1.5, schema versions 6
//! to 9) and the database layer of Murmur 1.6 (schema 10 and later, other
//! table and column names, ACLs referring to groups by id). Both are reduced
//! to the same rows ([`nuschel_ipc::murmur`]).
use nuschel_ipc::murmur::{
    MAX_BLOB_BYTES, MAX_ROWS, MurmurAcl, MurmurBan, MurmurChannel, MurmurFrame, MurmurGroup,
    MurmurHeader, MurmurListener, MurmurUser, encode_blob,
};
use sqlx::{
    Connection, Decode, Row, Sqlite, SqliteConnection, TypeInfo, ValueRef,
    sqlite::{SqliteConnectOptions, SqliteRow},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    net::Ipv6Addr,
    path::Path,
};

/// Rows per statement.
const PAGE: i64 = 256;
/// Blob and text bytes held at once (comments, descriptions, textures).
const BUDGET: usize = 256 * 1024 * 1024;

type Emit<'b> = dyn FnMut(MurmurFrame) -> Result<(), String> + 'b;

/// A decoded value: `Ok(None)` is SQL NULL, `Err` a value of the wrong kind.
type Value<T> = Result<Option<T>, ()>;

fn raw_type(row: &SqliteRow, index: usize) -> Option<String> {
    let raw = row.try_get_raw(index).ok()?;
    if raw.is_null() {
        return None;
    }
    Some(raw.type_info().name().to_owned())
}

fn int(row: &SqliteRow, index: usize) -> Value<i64> {
    let Some(kind) = raw_type(row, index) else {
        return Ok(None);
    };
    let raw = row.try_get_raw(index).map_err(|_| ())?;
    match kind.as_str() {
        "INTEGER" => <i64 as Decode<Sqlite>>::decode(raw)
            .map(Some)
            .map_err(|_| ()),
        "REAL" => {
            let value = <f64 as Decode<Sqlite>>::decode(raw).map_err(|_| ())?;
            if value.fract() == 0.0 && value.abs() < 9.0e15 {
                Ok(Some(value as i64))
            } else {
                Err(())
            }
        }
        "TEXT" => {
            let text = <String as Decode<Sqlite>>::decode(raw).map_err(|_| ())?;
            text.trim().parse().map(Some).map_err(|_| ())
        }
        _ => Err(()),
    }
}

fn real(row: &SqliteRow, index: usize) -> Value<f64> {
    let Some(kind) = raw_type(row, index) else {
        return Ok(None);
    };
    let raw = row.try_get_raw(index).map_err(|_| ())?;
    match kind.as_str() {
        "REAL" => <f64 as Decode<Sqlite>>::decode(raw)
            .map(Some)
            .map_err(|_| ()),
        "INTEGER" => <i64 as Decode<Sqlite>>::decode(raw)
            .map(|value| Some(value as f64))
            .map_err(|_| ()),
        "TEXT" => {
            let text = <String as Decode<Sqlite>>::decode(raw).map_err(|_| ())?;
            text.trim().parse().map(Some).map_err(|_| ())
        }
        _ => Err(()),
    }
}

/// Text; a blob holding UTF-8 is accepted, numbers are written out.
fn text(row: &SqliteRow, index: usize) -> Value<String> {
    let Some(kind) = raw_type(row, index) else {
        return Ok(None);
    };
    let raw = row.try_get_raw(index).map_err(|_| ())?;
    match kind.as_str() {
        "TEXT" => <String as Decode<Sqlite>>::decode(raw)
            .map(Some)
            .map_err(|_| ()),
        "BLOB" => {
            let bytes = <Vec<u8> as Decode<Sqlite>>::decode(raw).map_err(|_| ())?;
            String::from_utf8(bytes).map(Some).map_err(|_| ())
        }
        "INTEGER" => <i64 as Decode<Sqlite>>::decode(raw)
            .map(|value| Some(value.to_string()))
            .map_err(|_| ()),
        "REAL" => <f64 as Decode<Sqlite>>::decode(raw)
            .map(|value| Some(value.to_string()))
            .map_err(|_| ()),
        _ => Err(()),
    }
}

fn bytes(row: &SqliteRow, index: usize) -> Value<Vec<u8>> {
    let Some(kind) = raw_type(row, index) else {
        return Ok(None);
    };
    let raw = row.try_get_raw(index).map_err(|_| ())?;
    match kind.as_str() {
        "BLOB" | "TEXT" => <Vec<u8> as Decode<Sqlite>>::decode(raw)
            .map(Some)
            .map_err(|_| ()),
        _ => Err(()),
    }
}

/// Qt writes booleans as integers; text spellings are accepted too.
fn flag(row: &SqliteRow, index: usize) -> Value<bool> {
    if let Ok(Some(text)) = text(row, index) {
        match text.trim().to_ascii_lowercase().as_str() {
            "true" => return Ok(Some(true)),
            "false" => return Ok(Some(false)),
            _ => {}
        }
    }
    int(row, index).map(|value| value.map(|value| value != 0))
}

/// A column that may be large: selected only up to the byte limit, with its
/// size alongside (`octet_length` does not read the content).
fn capped(column: &str) -> String {
    format!(
        "CASE WHEN octet_length({column}) <= {MAX_BLOB_BYTES} THEN {column} END, octet_length({column})"
    )
}

struct Reader<'a, 'b> {
    connection: SqliteConnection,
    server: i64,
    notes: Vec<String>,
    held: usize,
    emit: &'a mut Emit<'b>,
}

impl Reader<'_, '_> {
    fn note(&mut self, note: String) {
        if self.notes.len() < 10_000 {
            self.notes.push(note);
        } else if self.notes.len() == 10_000 {
            self.notes.push("further notes omitted".into());
        }
    }

    /// A required value: reports and returns `None` when absent or malformed.
    fn require<T>(&mut self, value: Value<T>, what: impl FnOnce() -> String) -> Option<T> {
        match value {
            Ok(Some(value)) => Some(value),
            Ok(None) => {
                self.note(format!("{}: missing, row skipped", what()));
                None
            }
            Err(()) => {
                self.note(format!("{}: malformed, row skipped", what()));
                None
            }
        }
    }

    /// An optional value: reports a malformed one and drops it.
    fn optional<T>(&mut self, value: Value<T>, what: impl FnOnce() -> String) -> Option<T> {
        match value {
            Ok(value) => value,
            Err(()) => {
                self.note(format!("{}: malformed, ignored", what()));
                None
            }
        }
    }

    /// A capped text column (two result columns, see [`capped`]).
    fn capped_text(
        &mut self,
        row: &SqliteRow,
        index: usize,
        what: impl Fn() -> String,
    ) -> Option<String> {
        let size = int(row, index + 1).ok().flatten().unwrap_or(0);
        let value = text(row, index);
        if matches!(value, Ok(None)) && size > MAX_BLOB_BYTES as i64 {
            self.note(format!("{}: {size} bytes, over the limit; ignored", what()));
            return None;
        }
        let value = self.optional(value, &what)?;
        self.hold(value.len(), what)?;
        Some(value)
    }

    fn capped_bytes(
        &mut self,
        row: &SqliteRow,
        index: usize,
        what: impl Fn() -> String,
    ) -> Option<Vec<u8>> {
        let size = int(row, index + 1).ok().flatten().unwrap_or(0);
        let value = bytes(row, index);
        if matches!(value, Ok(None)) && size > MAX_BLOB_BYTES as i64 {
            self.note(format!("{}: {size} bytes, over the limit; ignored", what()));
            return None;
        }
        let value = self.optional(value, &what)?;
        self.hold(value.len(), what)?;
        Some(value)
    }

    /// Count blob bytes against the budget; past it, values are dropped.
    fn hold(&mut self, size: usize, what: impl Fn() -> String) -> Option<()> {
        if self.held.saturating_add(size) > BUDGET {
            self.note(format!(
                "{}: over the import's total of {} MiB of blobs; ignored",
                what(),
                BUDGET >> 20
            ));
            return None;
        }
        self.held += size;
        Some(())
    }

    /// Every row of `statement` (which selects `rowid` first and filters
    /// `rowid > $2`, with `$1` the server id), page by page.
    async fn rows(&mut self, table: &str, statement: &str) -> Result<Vec<SqliteRow>, String> {
        let mut out = Vec::new();
        let mut after = i64::MIN;
        loop {
            // Statements are built from this file's table and column names only.
            let page: Vec<SqliteRow> = sqlx::query(sqlx::AssertSqlSafe(statement.to_owned()))
                .bind(self.server)
                .bind(after)
                .bind(PAGE)
                .fetch_all(&mut self.connection)
                .await
                .map_err(|error| format!("reading {table}: {error}"))?;
            let full = page.len() as i64 >= PAGE;
            for row in page {
                after = int(&row, 0)
                    .ok()
                    .flatten()
                    .ok_or_else(|| format!("reading {table}: no rowid"))?;
                if out.len() >= MAX_ROWS {
                    self.note(format!(
                        "{table}: more than {MAX_ROWS} rows; the rest is ignored"
                    ));
                    return Ok(out);
                }
                out.push(row);
            }
            if !full {
                return Ok(out);
            }
        }
    }

    /// Emit rows in frames below the frame limit.
    fn send(&mut self, frame: MurmurFrame) -> Result<(), String> {
        for part in frame.split() {
            (self.emit)(part)?;
        }
        Ok(())
    }
}

/// The table and column names of one layout.
struct Layout {
    name: &'static str,
    meta: &'static str,
    meta_key: &'static str,
    meta_value: &'static str,
    version_key: &'static str,
    servers: &'static str,
    config: (&'static str, &'static str, &'static str),
    channels: &'static str,
    channel_name: &'static str,
    inherit_acl: &'static str,
    channel_info: (&'static str, &'static str, &'static str),
    users: &'static str,
    user_name: &'static str,
    password: &'static str,
    kdf_iterations: &'static str,
    last_channel: &'static str,
    user_info: (&'static str, &'static str, &'static str),
    group_name: &'static str,
    inheritable: &'static str,
    member_added: &'static str,
    links: (&'static str, &'static str, &'static str),
}

const LEGACY: Layout = Layout {
    name: "legacy",
    meta: "meta",
    meta_key: "keystring",
    meta_value: "value",
    version_key: "version",
    servers: "servers",
    config: ("config", "key", "value"),
    channels: "channels",
    channel_name: "name",
    inherit_acl: "inheritacl",
    channel_info: ("channel_info", "key", "value"),
    users: "users",
    user_name: "name",
    password: "pw",
    kdf_iterations: "kdfiterations",
    last_channel: "lastchannel",
    user_info: ("user_info", "key", "value"),
    group_name: "name",
    inheritable: "inheritable",
    member_added: "addit",
    links: ("channel_links", "channel_id", "link_id"),
};

const V16: Layout = Layout {
    name: "1.6",
    meta: "meta",
    meta_key: "meta_key",
    meta_value: "meta_value",
    version_key: "schema_version",
    servers: "virtual_servers",
    config: ("config", "config_name", "config_value"),
    channels: "channels",
    channel_name: "channel_name",
    inherit_acl: "inherit_acl",
    channel_info: ("channel_properties", "property_key", "property_value"),
    users: "users",
    user_name: "user_name",
    password: "password_hash",
    kdf_iterations: "kdf_iterations",
    last_channel: "last_channel_id",
    user_info: ("user_properties", "property_key", "property_value"),
    group_name: "group_name",
    inheritable: "is_inheritable",
    member_added: "add_to_group",
    links: ("channel_links", "first_channel_id", "second_channel_id"),
};

/// Keys of `channel_info`/`channel_properties` and `user_info`/`user_properties`
/// (`ServerDB::ChannelInfo`, `ServerDB::UserInfo`; the same numbers in 1.6).
const CHANNEL_DESCRIPTION: i64 = 0;
const CHANNEL_POSITION: i64 = 1;
const CHANNEL_MAX_USERS: i64 = 2;
const USER_EMAIL: i64 = 1;
const USER_COMMENT: i64 = 2;
const USER_HASH: i64 = 3;

/// 1.6's `DBAcl::MetaGroup`.
fn meta_group(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "none",
        1 => "all",
        2 => "auth",
        3 => "strong",
        4 => "in",
        5 => "out",
        6 => "sub",
        _ => return None,
    })
}

/// 1.6's `getLegacyGroupData`: prefix modifiers are prepended in order, the
/// `sub` metagroup's `,a,b,c` parameters appended.
fn legacy_group_string(base: String, modifiers: Option<&str>) -> String {
    let mut group = base;
    for modifier in modifiers.unwrap_or_default().split(';') {
        if modifier.is_empty() {
            continue;
        }
        if modifier.starts_with(',') {
            group.push_str(modifier);
        } else {
            group.insert_str(0, modifier);
        }
    }
    group
}

/// Read the database at `path` for virtual server `server`, emitting frames.
pub async fn read(path: &Path, server: i64, emit: &mut Emit<'_>) -> Result<(), String> {
    let mut header = [0_u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if &header != b"SQLite format 3\0" {
        return Err(format!("{} is not an SQLite database", path.display()));
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .foreign_keys(false)
        .pragma("query_only", "ON")
        .pragma("trusted_schema", "OFF")
        .pragma("cell_size_check", "ON")
        .pragma("mmap_size", "0")
        .pragma("hard_heap_limit", "1073741824");
    let connection = SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut reader = Reader {
        connection,
        server,
        notes: Vec::new(),
        held: 0,
        emit,
    };
    let check: Vec<String> = sqlx::query_scalar("PRAGMA quick_check(1)")
        .fetch_all(&mut reader.connection)
        .await
        .map_err(|error| format!("the database is damaged: {error}"))?;
    if check.first().map(String::as_str) != Some("ok") {
        return Err(format!(
            "the database is damaged (quick_check: {})",
            check.join("; ").chars().take(200).collect::<String>()
        ));
    }
    let tables: HashSet<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&mut reader.connection)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect();
    let mut columns: HashMap<String, HashSet<String>> = HashMap::new();
    for table in &tables {
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info($1)")
            .bind(table)
            .fetch_all(&mut reader.connection)
            .await
            .map_err(|error| error.to_string())?;
        columns.insert(table.clone(), names.into_iter().collect());
    }
    let has =
        |table: &str, column: &str| columns.get(table).is_some_and(|set| set.contains(column));
    let layout = if has("users", "pw") {
        &LEGACY
    } else if has("users", "password_hash") && has("users", "user_name") {
        &V16
    } else if tables.contains("players") {
        return Err(
            "this database is from Murmur before 1.2.4; start it once with Murmur 1.5 to \
             upgrade it, then import"
                .into(),
        );
    } else {
        return Err("this is not a Murmur database (no users table)".into());
    };
    let version: Option<String> = if has(layout.meta, layout.meta_key) {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT CAST({value} AS TEXT) FROM {meta} WHERE {key} = $1",
            value = layout.meta_value,
            meta = layout.meta,
            key = layout.meta_key
        )))
        .bind(layout.version_key)
        .fetch_optional(&mut reader.connection)
        .await
        .map_err(|error| error.to_string())?
        .flatten()
    } else {
        None
    };
    let schema_version = version
        .as_deref()
        .and_then(|version| version.trim().parse().ok())
        .unwrap_or(0);
    let mut server_ids = Vec::new();
    if tables.contains(layout.servers) {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT server_id FROM {} ORDER BY server_id LIMIT 1000",
            layout.servers
        )))
        .fetch_all(&mut reader.connection)
        .await
        .map_err(|error| error.to_string())?;
        server_ids = rows
            .iter()
            .filter_map(|row| int(row, 0).ok().flatten())
            .collect();
    }
    if !server_ids.contains(&server) {
        return Err(format!(
            "virtual server {server} is not in this database (it has {server_ids:?})"
        ));
    }
    (reader.emit)(MurmurFrame::Header(MurmurHeader {
        layout: layout.name.into(),
        schema_version,
        server_ids,
    }))?;
    let required = [
        layout.channels,
        layout.users,
        layout.config.0,
        layout.channel_info.0,
        layout.user_info.0,
        "groups",
        "group_members",
        layout.links.0,
        "bans",
    ];
    for table in required {
        if !tables.contains(table) {
            return Err(format!("the table {table} is missing"));
        }
    }
    let optional = |table: &str, column: &str| {
        if has(table, column) {
            column.to_owned()
        } else {
            "NULL".to_owned()
        }
    };

    // Configuration.
    let (config, key, value) = layout.config;
    let statement = format!(
        "SELECT rowid, {key}, {} FROM {config} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(value)
    );
    let rows = reader.rows(config, &statement).await?;
    let mut settings = Vec::new();
    for row in rows {
        let Some(name) = reader.require(text(&row, 1), || "config: a key".into()) else {
            continue;
        };
        if name.len() > 128 {
            reader.note("config: a key over 128 bytes; ignored".into());
            continue;
        }
        if let Some(value) = reader.capped_text(&row, 2, || format!("config {name}")) {
            settings.push((name, value));
        }
    }
    reader.send(MurmurFrame::Config(settings))?;

    // Channels with their properties.
    let (info, info_key, info_value) = layout.channel_info;
    let statement = format!(
        "SELECT rowid, channel_id, {key}, {} FROM {info} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(info_value),
        key = info_key
    );
    let mut properties: HashMap<(i64, i64), String> = HashMap::new();
    for row in reader.rows(info, &statement).await? {
        let what = || format!("{info}: a row");
        let (Some(channel), Some(key)) = (
            reader.require(int(&row, 1), what),
            reader.require(int(&row, 2), what),
        ) else {
            continue;
        };
        if ![CHANNEL_DESCRIPTION, CHANNEL_POSITION, CHANNEL_MAX_USERS].contains(&key) {
            continue;
        }
        if let Some(value) =
            reader.capped_text(&row, 3, || format!("channel {channel} property {key}"))
        {
            properties.insert((channel, key), value);
        }
    }
    let statement = format!(
        "SELECT rowid, channel_id, parent_id, {}, {} FROM {} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(layout.channel_name),
        layout.inherit_acl,
        layout.channels
    );
    let mut channels = Vec::new();
    for row in reader.rows(layout.channels, &statement).await? {
        let Some(id) = reader.require(int(&row, 1), || "channels: channel_id".into()) else {
            continue;
        };
        let parent = reader.optional(int(&row, 2), || format!("channel {id}: parent"));
        if parent.is_none() && id != 0 {
            reader.note(format!("channel {id}: no parent and not the root; skipped"));
            continue;
        }
        let Some(name) = reader.capped_text(&row, 3, || format!("channel {id}: name")) else {
            reader.note(format!("channel {id}: no usable name; skipped"));
            continue;
        };
        let inherit_acl = reader
            .optional(flag(&row, 5), || format!("channel {id}: inherit_acl"))
            .unwrap_or(true);
        channels.push(MurmurChannel {
            id,
            parent,
            name,
            inherit_acl,
            description: properties.remove(&(id, CHANNEL_DESCRIPTION)),
            position: properties.remove(&(id, CHANNEL_POSITION)),
            max_users: properties.remove(&(id, CHANNEL_MAX_USERS)),
        });
    }
    drop(properties);
    reader.send(MurmurFrame::Channels(channels))?;

    // Groups and their members.
    let statement = format!(
        "SELECT rowid, group_id, channel_id, {}, inherit, {} FROM groups WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(layout.group_name),
        layout.inheritable
    );
    let mut groups: BTreeMap<i64, MurmurGroup> = BTreeMap::new();
    for row in reader.rows("groups", &statement).await? {
        let what = || "groups: a row".to_owned();
        let (Some(group), Some(channel)) = (
            reader.require(int(&row, 1), what),
            reader.require(int(&row, 2), what),
        ) else {
            continue;
        };
        let Some(name) = reader.capped_text(&row, 3, || format!("group {group}: name")) else {
            continue;
        };
        let inherit = reader
            .optional(flag(&row, 5), || format!("group {group}: inherit"))
            .unwrap_or(true);
        let inheritable = reader
            .optional(flag(&row, 6), || format!("group {group}: inheritable"))
            .unwrap_or(true);
        groups.insert(
            group,
            MurmurGroup {
                channel,
                name,
                inherit,
                inheritable,
                add: Vec::new(),
                remove: Vec::new(),
            },
        );
    }
    let statement = format!(
        "SELECT rowid, group_id, user_id, {} FROM group_members WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        layout.member_added
    );
    for row in reader.rows("group_members", &statement).await? {
        let what = || "group_members: a row".to_owned();
        let (Some(group), Some(user), Some(added)) = (
            reader.require(int(&row, 1), what),
            reader.require(int(&row, 2), what),
            reader.require(flag(&row, 3), what),
        ) else {
            continue;
        };
        match groups.get_mut(&group) {
            Some(entry) if added => entry.add.push(user),
            Some(entry) => entry.remove.push(user),
            None => reader.note(format!(
                "group_members: group {group} does not exist; skipped"
            )),
        }
    }

    // ACLs; 1.6 refers to groups by id and splits off tokens and modifiers.
    let acl_statement = if layout.name == "legacy" {
        format!(
            "SELECT rowid, channel_id, priority, user_id, {}, apply_here, apply_sub, grantpriv, revokepriv FROM acl WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            capped("group_name")
        )
    } else {
        format!(
            "SELECT rowid, channel_id, priority, affected_user_id, affected_group_id, affected_meta_group_id, {}, {}, apply_in_current_channel, apply_in_sub_channels, granted_privilege_flags, revoked_privilege_flags FROM access_control_lists WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            capped("access_token"),
            capped("group_modifiers")
        )
    };
    let acl_table = if layout.name == "legacy" {
        "acl"
    } else {
        "access_control_lists"
    };
    if !tables.contains(acl_table) {
        return Err(format!("the table {acl_table} is missing"));
    }
    let mut acls = Vec::new();
    for row in reader.rows(acl_table, &acl_statement).await? {
        let what = || format!("{acl_table}: a row");
        let (Some(channel), Some(priority)) = (
            reader.require(int(&row, 1), what),
            reader.require(int(&row, 2), what),
        ) else {
            continue;
        };
        let at = || format!("ACL {priority} in channel {channel}");
        let user_id = reader.optional(int(&row, 3), at);
        let (group, rest) = if layout.name == "legacy" {
            (reader.capped_text(&row, 4, at), 6)
        } else {
            let group_id = reader.optional(int(&row, 4), at);
            let meta = reader.optional(int(&row, 5), at);
            let token = reader.capped_text(&row, 6, at);
            let modifiers = reader.capped_text(&row, 8, at);
            let base = if let Some(id) = group_id {
                match groups.get(&id) {
                    Some(group) => Some(group.name.clone()),
                    None => {
                        reader.note(format!("{}: group {id} does not exist; skipped", at()));
                        continue;
                    }
                }
            } else if let Some(meta) = meta {
                match meta_group(meta) {
                    Some(name) => Some(name.to_owned()),
                    None => {
                        reader.note(format!("{}: unknown meta group {meta}; skipped", at()));
                        continue;
                    }
                }
            } else {
                token.map(|token| format!("#{token}"))
            };
            (
                base.map(|base| legacy_group_string(base, modifiers.as_deref())),
                10,
            )
        };
        let apply_here = reader.optional(flag(&row, rest), at).unwrap_or(true);
        let apply_subs = reader.optional(flag(&row, rest + 1), at).unwrap_or(true);
        let grant = reader.optional(int(&row, rest + 2), at).unwrap_or(0);
        let deny = reader.optional(int(&row, rest + 3), at).unwrap_or(0);
        acls.push(MurmurAcl {
            channel,
            priority,
            user_id,
            group,
            apply_here,
            apply_subs,
            grant,
            deny,
        });
    }
    reader.send(MurmurFrame::Acls(acls))?;
    reader.send(MurmurFrame::Groups(groups.into_values().collect()))?;

    // Links.
    let (links_table, first, second) = layout.links;
    let statement = format!(
        "SELECT rowid, {first}, {second} FROM {links_table} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3"
    );
    let mut links = Vec::new();
    for row in reader.rows(links_table, &statement).await? {
        let what = || format!("{links_table}: a row");
        if let (Some(a), Some(b)) = (
            reader.require(int(&row, 1), what),
            reader.require(int(&row, 2), what),
        ) {
            links.push((a, b));
        }
    }
    reader.send(MurmurFrame::Links(links))?;

    // Bans: a 16-byte blob (legacy) or eight groups of hex digits (1.6).
    let statement = if layout.name == "legacy" {
        format!(
            "SELECT rowid, base, mask, {}, {}, {}, CAST(start AS TEXT), duration FROM bans WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            capped("name"),
            capped("hash"),
            capped("reason")
        )
    } else {
        format!(
            "SELECT rowid, ipv6_base_address, prefix_length, {}, {}, {}, CAST(start_date AS TEXT), duration FROM bans WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            capped("banned_user_name"),
            capped("banned_user_cert_hash"),
            capped("reason")
        )
    };
    let mut bans = Vec::new();
    for (index, row) in reader
        .rows("bans", &statement)
        .await?
        .into_iter()
        .enumerate()
    {
        let at = || format!("ban {}", index + 1);
        let address = if layout.name == "legacy" {
            match reader.optional(bytes(&row, 1), at) {
                Some(bytes) => match <[u8; 16]>::try_from(bytes.as_slice()) {
                    Ok(octets) => Some(Ipv6Addr::from(octets).to_string()),
                    Err(_) => {
                        reader.note(format!(
                            "{}: address of {} bytes; ignored",
                            at(),
                            bytes.len()
                        ));
                        None
                    }
                },
                None => None,
            }
        } else {
            reader.optional(text(&row, 1), at)
        };
        bans.push(MurmurBan {
            address,
            mask: reader.optional(int(&row, 2), at),
            name: reader.capped_text(&row, 3, at),
            hash: reader.capped_text(&row, 5, at),
            reason: reader.capped_text(&row, 7, at),
            start: reader.optional(text(&row, 9), at),
            duration: reader.optional(int(&row, 10), at),
        });
    }
    reader.send(MurmurFrame::Bans(bans))?;

    // Channel listeners (Murmur 1.4 and later).
    if tables.contains("channel_listeners") {
        let statement = format!(
            "SELECT rowid, user_id, channel_id, {}, {} FROM channel_listeners WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            optional("channel_listeners", "volume_adjustment"),
            optional("channel_listeners", "enabled")
        );
        let mut listeners = Vec::new();
        for row in reader.rows("channel_listeners", &statement).await? {
            let what = || "channel_listeners: a row".to_owned();
            let (Some(user_id), Some(channel)) = (
                reader.require(int(&row, 1), what),
                reader.require(int(&row, 2), what),
            ) else {
                continue;
            };
            let at = || format!("listener of user {user_id} in channel {channel}");
            let volume = reader.optional(real(&row, 3), at);
            let enabled = reader.optional(flag(&row, 4), at).unwrap_or(true);
            listeners.push(MurmurListener {
                user_id,
                channel,
                volume,
                enabled,
            });
        }
        reader.send(MurmurFrame::Listeners(listeners))?;
    }

    // Registered users, their properties and textures.
    let (info, info_key, info_value) = layout.user_info;
    let statement = format!(
        "SELECT rowid, user_id, {key}, {} FROM {info} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(info_value),
        key = info_key
    );
    let mut user_properties: HashMap<(i64, i64), String> = HashMap::new();
    let mut emails = HashSet::new();
    for row in reader.rows(info, &statement).await? {
        let what = || format!("{info}: a row");
        let (Some(user), Some(key)) = (
            reader.require(int(&row, 1), what),
            reader.require(int(&row, 2), what),
        ) else {
            continue;
        };
        match key {
            USER_EMAIL => {
                emails.insert(user);
            }
            USER_COMMENT | USER_HASH => {
                if let Some(value) =
                    reader.capped_text(&row, 3, || format!("user {user} property {key}"))
                {
                    user_properties.insert((user, key), value);
                }
            }
            _ => {}
        }
    }
    let users_table = layout.users;
    let statement = format!(
        "SELECT rowid, user_id, {}, {}, {}, {}, {}, CAST({} AS TEXT), {} FROM {users_table} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(layout.user_name),
        capped(layout.password),
        capped(&optional(users_table, "salt")),
        optional(users_table, layout.kdf_iterations),
        optional(users_table, layout.last_channel),
        optional(users_table, "last_active"),
        capped(&optional(users_table, "texture")),
    );
    let mut users = Vec::new();
    for row in reader.rows(users_table, &statement).await? {
        let Some(id) = reader.require(int(&row, 1), || "users: user_id".into()) else {
            continue;
        };
        let Some(name) = reader.capped_text(&row, 2, || format!("user {id}: name")) else {
            reader.note(format!("user {id}: no usable name; skipped"));
            continue;
        };
        let at = || format!("user {id}");
        let password = reader.capped_text(&row, 4, at);
        let salt = reader.capped_text(&row, 6, at);
        let kdf_iterations = reader.optional(int(&row, 8), at);
        let last_channel = reader.optional(int(&row, 9), at);
        let last_active = reader.optional(text(&row, 10), at);
        let texture = reader
            .capped_bytes(&row, 11, || format!("user {id}: texture"))
            .filter(|texture| !texture.is_empty())
            .map(|texture| encode_blob(&texture));
        users.push(MurmurUser {
            id,
            name,
            password,
            salt,
            kdf_iterations,
            last_channel,
            last_active,
            certificate_hash: user_properties.remove(&(id, USER_HASH)),
            comment: user_properties.remove(&(id, USER_COMMENT)),
            texture,
            email: emails.contains(&id),
        });
    }
    reader.send(MurmurFrame::Users(users))?;
    let notes = std::mem::take(&mut reader.notes);
    reader.send(MurmurFrame::Notes(notes))?;
    (reader.emit)(MurmurFrame::End)?;
    let _ = reader.connection.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschel_ipc::murmur::MurmurDump;

    #[test]
    fn group_strings_round_trip_through_the_16_modifiers() {
        // 1.6 stores "~!sub,1,2" as modifiers "!;~;,1,2" on the meta group "sub".
        assert_eq!(
            legacy_group_string("sub".into(), Some("!;~;,1,2")),
            "~!sub,1,2"
        );
        assert_eq!(legacy_group_string("#secret".into(), None), "#secret");
        assert_eq!(legacy_group_string("admin".into(), Some("!")), "!admin");
        assert_eq!(meta_group(1), Some("all"));
        assert_eq!(meta_group(7), None);
    }

    async fn fixture(directory: &Path, name: &str, script: &str) -> std::path::PathBuf {
        let path = directory.join(name);
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(script.to_owned()))
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        path
    }

    async fn dump(path: &Path, server: i64) -> Result<MurmurDump, String> {
        let mut dump = MurmurDump::default();
        let mut done = false;
        let mut emit = |frame: MurmurFrame| {
            done |= dump.apply(frame)?;
            Ok(())
        };
        read(path, server, &mut emit).await?;
        assert!(done);
        Ok(dump)
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
    }

    /// Murmur 1.5's schema with Murmur's own statements: everything is read,
    /// malformed rows become notes. The result is the input of nuscheld's
    /// import test (tests/fixtures/murmur-1.5.json; `UPDATE_FIXTURES=1`
    /// rewrites it).
    #[tokio::test]
    async fn reads_a_murmur_15_database() {
        let directory = tempfile::tempdir().unwrap();
        let script = std::fs::read_to_string(fixture_path("murmur-1.5.sql")).unwrap();
        let path = fixture(directory.path(), "murmur.sqlite", &script).await;
        let dump = dump(&path, 1).await.unwrap();
        assert_eq!(dump.header.layout, "legacy");
        assert_eq!(dump.header.schema_version, 9);
        assert_eq!(dump.header.server_ids, [1, 2]);
        assert!(dump.config.contains(&("users".into(), "42".into())));
        assert!(!dump.config.iter().any(|(_, value)| value.contains("other")));
        let names: Vec<&str> = dump.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Root",
                "Lobby",
                "Secret",
                "Sub",
                "bad>name",
                "Child of bad",
                "Orphan"
            ]
        );
        let lobby = &dump.channels[1];
        assert_eq!(lobby.description.as_deref(), Some("The <i>lobby</i>"));
        assert_eq!(lobby.position.as_deref(), Some("5"));
        assert!(!dump.channels[3].inherit_acl);
        let alice = dump.users.iter().find(|user| user.name == "alice").unwrap();
        assert_eq!(alice.kdf_iterations, Some(1000));
        assert_eq!(alice.salt.as_deref(), Some("0011223344556677"));
        assert_eq!(alice.last_active.as_deref(), Some("2025-11-02 11:00:00"));
        let eve = dump.users.iter().find(|user| user.name == "eve").unwrap();
        assert_eq!(eve.comment.as_deref(), Some("Hi, I am <b>eve</b>"));
        assert_eq!(
            eve.texture
                .as_deref()
                .and_then(nuschel_ipc::murmur::decode_blob)
                .map(|t| t.len()),
            Some(16)
        );
        assert!(
            dump.users
                .iter()
                .any(|user| user.name == "bob" && user.email)
        );
        assert!(
            !dump
                .users
                .iter()
                .any(|user| user.name == "other-server-user")
        );
        assert_eq!(dump.acls.len(), 8);
        assert_eq!(dump.groups.len(), 2);
        assert_eq!(dump.groups[1].add, [2, 999]);
        assert_eq!(dump.links.len(), 3);
        assert_eq!(dump.bans[0].address.as_deref(), Some("::ffff:192.0.2.1"));
        assert_eq!(dump.bans[0].start.as_deref(), Some("2025-01-01T00:00:00"));
        assert_eq!(dump.bans[3].address, None);
        assert_eq!(dump.listeners.len(), 3);
        assert_eq!(dump.listeners[0].volume, Some(0.5));
        assert!(!dump.listeners[1].enabled);
        // Malformed ids are notes, not failures; a group that does not exist too.
        for expected in [
            "channels: channel_id: malformed",
            "users: user_id: malformed",
            "group 3 does not exist",
            "ban 4: address of 4 bytes",
        ] {
            assert!(
                dump.notes.iter().any(|note| note.contains(expected)),
                "{expected} in {:?}",
                dump.notes
            );
        }
        // The fixture nuscheld's import test reads.
        let json = dump.to_json();
        let golden = fixture_path("murmur-1.5.json");
        if std::env::var_os("UPDATE_FIXTURES").is_some() {
            std::fs::write(&golden, &json).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&golden).unwrap_or_default(),
            json,
            "tests/fixtures/murmur-1.5.json is outdated; run with UPDATE_FIXTURES=1"
        );

        // Another virtual server, and one that does not exist.
        let other = super::tests::dump(&path, 2).await.unwrap();
        assert_eq!(other.channels.len(), 1);
        assert!(
            super::tests::dump(&path, 3)
                .await
                .unwrap_err()
                .contains("not in this database")
        );
    }

    /// Murmur 1.6's layout (schema 11): other names, ACLs referring to groups
    /// by id, meta groups and tokens in their own columns, Unix times.
    #[tokio::test]
    async fn reads_a_murmur_16_database() {
        const SCRIPT: &str = r#"
CREATE TABLE "meta" ("meta_key" VARCHAR(100) PRIMARY KEY, "meta_value" TEXT);
CREATE TABLE "virtual_servers" ("server_id" INTEGER PRIMARY KEY);
CREATE TABLE "config" ("server_id" INTEGER NOT NULL, "config_name" VARCHAR(100), "config_value" TEXT);
CREATE TABLE "channels" ("server_id" INTEGER NOT NULL, "channel_id" INTEGER NOT NULL, "parent_id" INTEGER, "channel_name" VARCHAR(255), "inherit_acl" SMALLINT);
CREATE TABLE "channel_properties" ("server_id" INTEGER NOT NULL, "channel_id" INTEGER NOT NULL, "property_key" INTEGER, "property_value" TEXT);
CREATE TABLE "users" ("server_id" INTEGER NOT NULL, "user_id" INTEGER NOT NULL, "user_name" VARCHAR(255), "password_hash" VARCHAR(255), "salt" VARCHAR(128), "kdf_iterations" INTEGER, "last_channel_id" INTEGER, "texture" BLOB, "last_active" BIGINT, "last_disconnect" BIGINT);
CREATE TABLE "user_properties" ("server_id" INTEGER NOT NULL, "user_id" INTEGER NOT NULL, "property_key" INTEGER, "property_value" TEXT);
CREATE TABLE "groups" ("server_id" INTEGER NOT NULL, "group_id" INTEGER NOT NULL, "group_name" VARCHAR(255), "channel_id" INTEGER NOT NULL, "inherit" SMALLINT, "is_inheritable" SMALLINT);
CREATE TABLE "group_members" ("server_id" INTEGER NOT NULL, "group_id" INTEGER NOT NULL, "user_id" INTEGER NOT NULL, "add_to_group" SMALLINT);
CREATE TABLE "access_control_lists" ("server_id" INTEGER NOT NULL, "channel_id" INTEGER NOT NULL, "priority" INTEGER, "affected_user_id" INTEGER, "affected_group_id" INTEGER, "affected_meta_group_id" INTEGER, "access_token" VARCHAR(255) DEFAULT NULL, "group_modifiers" VARCHAR(31) DEFAULT NULL, "apply_in_current_channel" SMALLINT, "apply_in_sub_channels" SMALLINT, "granted_privilege_flags" INTEGER, "revoked_privilege_flags" INTEGER);
CREATE TABLE "channel_links" ("server_id" INTEGER NOT NULL, "first_channel_id" INTEGER NOT NULL, "second_channel_id" INTEGER NOT NULL);
CREATE TABLE "bans" ("server_id" INTEGER NOT NULL, "ipv6_base_address" VARCHAR(45), "prefix_length" INTEGER NOT NULL, "banned_user_cert_hash" VARCHAR(255), "banned_user_name" VARCHAR(255), "reason" TEXT, "start_date" BIGINT, "duration" INTEGER);
CREATE TABLE "channel_listeners" ("server_id" INTEGER NOT NULL, "user_id" INTEGER NOT NULL, "channel_id" INTEGER NOT NULL, "volume_adjustment" FLOAT, "enabled" SMALLINT);
INSERT INTO meta VALUES ('schema_version', '11');
INSERT INTO virtual_servers VALUES (1);
INSERT INTO channels VALUES (1, 0, NULL, 'Root', 1);
INSERT INTO channels VALUES (1, 1, 0, 'Lobby', 1);
INSERT INTO users VALUES (1, 0, 'SuperUser', NULL, NULL, NULL, NULL, NULL, NULL, NULL);
INSERT INTO users VALUES (1, 1, 'alice', 'c2fa5023e808bd1adafccf5f995d5147a0d17e0113e9bda3fa8380faa58088467346ba1eb6bbf7e6374da78caa9b784c', '0011223344556677', 1000, 1, NULL, 1762819200, NULL);
INSERT INTO groups VALUES (1, 5, 'admin', 0, 1, 1);
INSERT INTO group_members VALUES (1, 5, 1, 1);
INSERT INTO access_control_lists VALUES (1, 0, 1, NULL, 5, NULL, NULL, NULL, 1, 1, 1, 0);
INSERT INTO access_control_lists VALUES (1, 1, 1, NULL, NULL, 6, NULL, '!;~;,1,2', 1, 1, 0, 4);
INSERT INTO access_control_lists VALUES (1, 1, 2, NULL, NULL, NULL, 'letmein', NULL, 1, 1, 4, 0);
INSERT INTO access_control_lists VALUES (1, 1, 3, 1, NULL, NULL, NULL, NULL, 1, 1, 64, 0);
INSERT INTO access_control_lists VALUES (1, 1, 4, NULL, 77, NULL, NULL, NULL, 1, 1, 64, 0);
INSERT INTO channel_links VALUES (1, 0, 1);
INSERT INTO bans VALUES (1, '0000:0000:0000:0000:0000:ffff:c000:0201', 128, '', 'troll', 'spam', 1735689600, 0);
INSERT INTO bans VALUES (1, '0000:0000:0000:0000:0000:0000:0000:0000', 0, 'cccccccccccccccccccccccccccccccccccccccc', 'by certificate', NULL, 1735689600, 0);
INSERT INTO channel_listeners VALUES (1, 1, 0, 2.0, 1);
"#;
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path(), "mumble-server.sqlite", SCRIPT).await;
        let dump = dump(&path, 1).await.unwrap();
        assert_eq!(dump.header.layout, "1.6");
        assert_eq!(dump.header.schema_version, 11);
        assert_eq!(dump.channels.len(), 2);
        let groups: Vec<Option<&str>> = dump.acls.iter().map(|acl| acl.group.as_deref()).collect();
        assert_eq!(
            groups,
            [Some("admin"), Some("~!sub,1,2"), Some("#letmein"), None]
        );
        assert_eq!(dump.acls[3].user_id, Some(1));
        assert!(
            dump.notes
                .iter()
                .any(|note| note.contains("group 77 does not exist"))
        );
        assert_eq!(dump.groups[0].add, [1]);
        assert_eq!(dump.users[1].last_active.as_deref(), Some("1762819200"));
        assert_eq!(
            dump.bans[0].address.as_deref(),
            Some("0000:0000:0000:0000:0000:ffff:c000:0201")
        );
        assert_eq!(
            dump.bans[1].hash.as_deref(),
            Some("cccccccccccccccccccccccccccccccccccccccc")
        );
        assert_eq!(dump.bans[0].start.as_deref(), Some("1735689600"));
        assert_eq!(dump.listeners[0].volume, Some(2.0));
    }

    #[tokio::test]
    async fn refuses_files_that_are_not_murmur_databases() {
        let directory = tempfile::tempdir().unwrap();
        let text = directory.path().join("notes.txt");
        std::fs::write(&text, "hello").unwrap();
        assert!(dump(&text, 1).await.unwrap_err().contains("cannot read"));
        std::fs::write(&text, "definitely not SQLite at all").unwrap();
        assert!(
            dump(&text, 1)
                .await
                .unwrap_err()
                .contains("not an SQLite database")
        );
        let empty = fixture(directory.path(), "empty.sqlite", "CREATE TABLE t (x);").await;
        assert!(
            dump(&empty, 1)
                .await
                .unwrap_err()
                .contains("not a Murmur database")
        );
        let damaged = fixture(
            directory.path(),
            "damaged.sqlite",
            "CREATE TABLE users (user_id, name, pw); INSERT INTO users VALUES (1, 'a', 'b');",
        )
        .await;
        let mut bytes = std::fs::read(&damaged).unwrap();
        let len = bytes.len();
        for byte in &mut bytes[len / 2..] {
            *byte = 0xff;
        }
        std::fs::write(&damaged, bytes).unwrap();
        assert!(dump(&damaged, 1).await.is_err());
    }
}
