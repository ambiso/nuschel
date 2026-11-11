// SPDX-License-Identifier: EUPL-1.2
//! Reads a Murmur (mumble-server) SQLite database for `nuschld
//! --import-murmur`. The file is untrusted: it is opened read-only and
//! immutable (no journal, WAL or shared-memory files are looked at, and WAL
//! files are refused), with `query_only`, `trusted_schema = OFF`, cell size
//! checks and a heap limit, checked with `quick_check`, and read only from
//! ordinary tables without hidden or generated columns, with fixed statements
//! in small pages by rowid. Every column is capped in SQL (`octet_length`
//! reads no content) and every value decoded by its actual SQLite type, so a
//! malformed row (text where Murmur writes integers, invalid UTF-8, an
//! oversized blob) is skipped and reported instead of failing the import.
//! Interpretation and validation happen in `nuschld` (apps/server/src/murmur.rs).
//!
//! Two layouts exist. `ServerDB.cpp` (Murmur 1.2.4 to 1.5, schema versions 6
//! to 9) and the database layer of Murmur 1.6 (schema 10 and later, other
//! table and column names, ACLs referring to groups by id). Both are reduced
//! to the same rows ([`nuschl_ipc::murmur`]).
use nuschl_ipc::murmur::{
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

/// Rows per statement: with four 1 MiB columns, a page stays far below the
/// SQLite heap limit.
const PAGE: i64 = 32;
/// Caps for names and short values (hashes, salts, addresses).
const NAME_BYTES: usize = 4096;
const SHORT_BYTES: usize = 256;
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

/// A column that may be large: selected only up to `limit` bytes, with its
/// size alongside (`octet_length` does not read the content). Read with
/// [`Reader::capped_text`] or [`Reader::capped_bytes`].
fn capped(column: &str, limit: usize) -> String {
    format!("CASE WHEN octet_length({column}) <= {limit} THEN {column} END, octet_length({column})")
}

/// A column Murmur fills with a number or a flag: anything longer than 64
/// bytes is read as NULL (SQLite would store any text there).
fn small(column: &str) -> String {
    format!("CASE WHEN octet_length({column}) <= 64 THEN {column} END")
}

/// A short value read as text (dates); the length is checked before the cast.
fn small_text(column: &str) -> String {
    format!("CASE WHEN octet_length({column}) <= 64 THEN CAST({column} AS TEXT) END")
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
        let size = int(row, index + 1).ok().flatten();
        let value = text(row, index);
        // NULL with a size: the value was over its cap.
        if let (Ok(None), Some(size)) = (&value, size) {
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
        let size = int(row, index + 1).ok().flatten();
        let value = bytes(row, index);
        if let (Ok(None), Some(size)) = (&value, size) {
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

    /// Visit every row of `statement` (which selects `rowid` first and
    /// filters `rowid > $2`, with `$1` the server id) a page at a time, so
    /// only one page and the values kept within the budget are in memory.
    async fn each(
        &mut self,
        table: &str,
        statement: &str,
        mut visit: impl FnMut(&mut Self, &SqliteRow),
    ) -> Result<(), String> {
        let mut after = i64::MIN;
        let mut count = 0_usize;
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
            for row in &page {
                after = int(row, 0)
                    .ok()
                    .flatten()
                    .ok_or_else(|| format!("reading {table}: no rowid"))?;
                if count >= MAX_ROWS {
                    self.note(format!(
                        "{table}: more than {MAX_ROWS} rows; the rest is ignored"
                    ));
                    return Ok(());
                }
                count += 1;
                visit(self, row);
            }
            if !full {
                return Ok(());
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
/// `sub` metagroup's `,a,b,c` parameters appended. The statement caps the
/// modifiers at 31 bytes, their column's size in 1.6.
fn legacy_group_string(base: String, modifiers: Option<&str>) -> String {
    let mut prefix = String::new();
    let mut suffix = String::new();
    for modifier in modifiers.unwrap_or_default().split(';') {
        if modifier.is_empty() {
            continue;
        }
        if modifier.starts_with(',') {
            suffix.push_str(modifier);
        } else {
            prefix.insert_str(0, modifier);
        }
    }
    prefix + &base + &suffix
}

/// Read the database at `path` for virtual server `server`, emitting frames.
pub async fn read(path: &Path, server: i64, emit: &mut Emit<'_>) -> Result<(), String> {
    let mut header = Vec::with_capacity(100);
    std::fs::File::open(path)
        .and_then(|file| file.take(100).read_to_end(&mut header))
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if header.len() < 100 || header.get(..16) != Some(b"SQLite format 3\0".as_slice()) {
        return Err(format!("{} is not an SQLite database", path.display()));
    }
    // Bytes 18 and 19 are the read and write format versions: 1 for a rollback
    // journal, 2 for WAL. WAL files are refused and the file is opened
    // immutable (a `-wal` next to it is never read), so SQLite's WAL code never
    // runs on an untrusted file (deny.toml, RUSTSEC-2026-0459). Murmur uses a
    // rollback journal unless `sqlite_wal` is set.
    if header.get(18..20) != Some([1, 1].as_slice()) {
        return Err(format!(
            "{} uses SQLite's write-ahead log (Murmur's sqlite_wal); with Murmur stopped, run \
             sqlite3 {} 'PRAGMA journal_mode=DELETE' and import again",
            path.display(),
            path.display()
        ));
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(true)
        .create_if_missing(false)
        .foreign_keys(false)
        .pragma("query_only", "ON")
        .pragma("trusted_schema", "OFF")
        .pragma("cell_size_check", "ON")
        .pragma("mmap_size", "0")
        .pragma("temp_store", "MEMORY")
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
    // Ordinary tables only, by the parsed schema (not its text): a virtual
    // table would run its module (FTS and the like) on the file's data.
    const TABLES: &str =
        "SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'table'";
    let tables: HashSet<String> = sqlx::query_scalar(TABLES)
        .fetch_all(&mut reader.connection)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect();
    // Column names, and whether a table has hidden or generated columns,
    // which Murmur never creates and which would compute values on read.
    let mut columns: HashMap<String, HashSet<String>> = HashMap::new();
    let mut computed: HashSet<String> = HashSet::new();
    for table in &tables {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, hidden FROM pragma_table_xinfo($1)")
                .bind(table)
                .fetch_all(&mut reader.connection)
                .await
                .map_err(|error| error.to_string())?;
        if rows.iter().any(|(_, hidden)| *hidden != 0) {
            computed.insert(table.clone());
        }
        columns.insert(
            table.clone(),
            rows.into_iter().map(|(name, _)| name).collect(),
        );
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
    let acl_table = if layout.name == "legacy" {
        "acl"
    } else {
        "access_control_lists"
    };
    let required = [
        layout.servers,
        layout.channels,
        layout.users,
        layout.config.0,
        layout.channel_info.0,
        layout.user_info.0,
        "groups",
        "group_members",
        acl_table,
        layout.links.0,
        "bans",
    ];
    for table in required
        .iter()
        .copied()
        .chain(["channel_listeners", layout.meta])
    {
        if computed.contains(table) {
            return Err(format!(
                "the table {table} has hidden or generated columns; this is not a database \
                 Murmur wrote"
            ));
        }
    }
    for table in required {
        if !tables.contains(table) {
            return Err(format!("the table {table} is missing"));
        }
    }
    let version: Option<String> = if has(layout.meta, layout.meta_key) {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM {meta} WHERE {key} = $1",
            small_text(layout.meta_value),
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
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM {} ORDER BY server_id LIMIT 1000",
        small("server_id"),
        layout.servers
    )))
    .fetch_all(&mut reader.connection)
    .await
    .map_err(|error| error.to_string())?;
    let server_ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| int(row, 0).ok().flatten())
        .collect();
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
    let optional = |table: &str, column: &str| {
        if has(table, column) {
            column.to_owned()
        } else {
            "NULL".to_owned()
        }
    };

    // Configuration; keys are Murmur's (ASCII letters, digits, underscores).
    let (config, key, value) = layout.config;
    let statement = format!(
        "SELECT rowid, {}, {} FROM {config} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        capped(key, 128),
        capped(value, MAX_BLOB_BYTES)
    );
    let mut settings = Vec::new();
    reader
        .each(config, &statement, |reader, row| {
            let Some(name) = reader.capped_text(row, 1, || "config: a key".into()) else {
                return;
            };
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                reader.note("config: a key with characters Murmur does not use; ignored".into());
                return;
            }
            if let Some(value) = reader.capped_text(row, 3, || format!("config {name}")) {
                settings.push((name, value));
            }
        })
        .await?;
    reader.send(MurmurFrame::Config(settings))?;

    // Channels with their properties.
    let (info, info_key, info_value) = layout.channel_info;
    let statement = format!(
        "SELECT rowid, {}, {}, {} FROM {info} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small("channel_id"),
        small(info_key),
        capped(info_value, MAX_BLOB_BYTES)
    );
    let mut properties: HashMap<(i64, i64), String> = HashMap::new();
    reader
        .each(info, &statement, |reader, row| {
            let what = || format!("{info}: a row");
            let (Some(channel), Some(key)) = (
                reader.require(int(row, 1), what),
                reader.require(int(row, 2), what),
            ) else {
                return;
            };
            if ![CHANNEL_DESCRIPTION, CHANNEL_POSITION, CHANNEL_MAX_USERS].contains(&key) {
                return;
            }
            if let Some(value) =
                reader.capped_text(row, 3, || format!("channel {channel} property {key}"))
            {
                properties.insert((channel, key), value);
            }
        })
        .await?;
    let statement = format!(
        "SELECT rowid, {}, {}, {}, {} FROM {} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small("channel_id"),
        small("parent_id"),
        capped(layout.channel_name, NAME_BYTES),
        small(layout.inherit_acl),
        layout.channels
    );
    let mut channels = Vec::new();
    reader
        .each(layout.channels, &statement, |reader, row| {
            let Some(id) = reader.require(int(row, 1), || "channels: channel_id".into()) else {
                return;
            };
            let parent = reader.optional(int(row, 2), || format!("channel {id}: parent"));
            if parent.is_none() && id != 0 {
                reader.note(format!("channel {id}: no parent and not the root; skipped"));
                return;
            }
            let Some(name) = reader.capped_text(row, 3, || format!("channel {id}: name")) else {
                reader.note(format!("channel {id}: no usable name; skipped"));
                return;
            };
            let inherit_acl = reader
                .optional(flag(row, 5), || format!("channel {id}: inherit_acl"))
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
        })
        .await?;
    drop(properties);
    reader.send(MurmurFrame::Channels(channels))?;

    // Groups and their members.
    let statement = format!(
        "SELECT rowid, {}, {}, {}, {}, {} FROM groups WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small("group_id"),
        small("channel_id"),
        capped(layout.group_name, NAME_BYTES),
        small("inherit"),
        small(layout.inheritable)
    );
    let mut groups: BTreeMap<i64, MurmurGroup> = BTreeMap::new();
    reader
        .each("groups", &statement, |reader, row| {
            let what = || "groups: a row".to_owned();
            let (Some(group), Some(channel)) = (
                reader.require(int(row, 1), what),
                reader.require(int(row, 2), what),
            ) else {
                return;
            };
            let Some(name) = reader.capped_text(row, 3, || format!("group {group}: name")) else {
                return;
            };
            let inherit = reader
                .optional(flag(row, 5), || format!("group {group}: inherit"))
                .unwrap_or(true);
            let inheritable = reader
                .optional(flag(row, 6), || format!("group {group}: inheritable"))
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
        })
        .await?;
    let statement = format!(
        "SELECT rowid, {}, {}, {} FROM group_members WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small("group_id"),
        small("user_id"),
        small(layout.member_added)
    );
    reader
        .each("group_members", &statement, |reader, row| {
            let what = || "group_members: a row".to_owned();
            let (Some(group), Some(user), Some(added)) = (
                reader.require(int(row, 1), what),
                reader.require(int(row, 2), what),
                reader.require(flag(row, 3), what),
            ) else {
                return;
            };
            match groups.get_mut(&group) {
                Some(entry) if added => entry.add.push(user),
                Some(entry) => entry.remove.push(user),
                None => reader.note(format!(
                    "group_members: group {group} does not exist; skipped"
                )),
            }
        })
        .await?;

    // ACLs; 1.6 refers to groups by id and splits off tokens and modifiers.
    let acl_statement = if layout.name == "legacy" {
        format!(
            "SELECT rowid, {}, {}, {}, {}, {}, {}, {}, {} FROM acl WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            small("channel_id"),
            small("priority"),
            small("user_id"),
            capped("group_name", NAME_BYTES),
            small("apply_here"),
            small("apply_sub"),
            small("grantpriv"),
            small("revokepriv")
        )
    } else {
        format!(
            "SELECT rowid, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM access_control_lists WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            small("channel_id"),
            small("priority"),
            small("affected_user_id"),
            small("affected_group_id"),
            small("affected_meta_group_id"),
            capped("access_token", 255),
            capped("group_modifiers", 31),
            small("apply_in_current_channel"),
            small("apply_in_sub_channels"),
            small("granted_privilege_flags"),
            small("revoked_privilege_flags")
        )
    };
    let legacy = layout.name == "legacy";
    let mut acls = Vec::new();
    reader
        .each(acl_table, &acl_statement, |reader, row| {
            let what = || format!("{acl_table}: a row");
            let (Some(channel), Some(priority)) = (
                reader.require(int(row, 1), what),
                reader.require(int(row, 2), what),
            ) else {
                return;
            };
            let at = || format!("ACL {priority} in channel {channel}");
            let user_id = reader.optional(int(row, 3), at);
            let (group, rest) = if legacy {
                (reader.capped_text(row, 4, at), 6)
            } else {
                let group_id = reader.optional(int(row, 4), at);
                let meta = reader.optional(int(row, 5), at);
                let token = reader.capped_text(row, 6, at);
                let modifiers = reader.capped_text(row, 8, at);
                let base = if let Some(id) = group_id {
                    match groups.get(&id) {
                        Some(group) => Some(group.name.clone()),
                        None => {
                            reader.note(format!("{}: group {id} does not exist; skipped", at()));
                            return;
                        }
                    }
                } else if let Some(meta) = meta {
                    match meta_group(meta) {
                        Some(name) => Some(name.to_owned()),
                        None => {
                            reader.note(format!("{}: unknown meta group {meta}; skipped", at()));
                            return;
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
            let apply_here = reader.optional(flag(row, rest), at).unwrap_or(true);
            let apply_subs = reader.optional(flag(row, rest + 1), at).unwrap_or(true);
            let grant = reader.optional(int(row, rest + 2), at).unwrap_or(0);
            let deny = reader.optional(int(row, rest + 3), at).unwrap_or(0);
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
        })
        .await?;
    reader.send(MurmurFrame::Acls(acls))?;
    reader.send(MurmurFrame::Groups(groups.into_values().collect()))?;

    // Links.
    let (links_table, first, second) = layout.links;
    let statement = format!(
        "SELECT rowid, {}, {} FROM {links_table} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small(first),
        small(second)
    );
    let mut links = Vec::new();
    reader
        .each(links_table, &statement, |reader, row| {
            let what = || format!("{links_table}: a row");
            if let (Some(a), Some(b)) = (
                reader.require(int(row, 1), what),
                reader.require(int(row, 2), what),
            ) {
                links.push((a, b));
            }
        })
        .await?;
    reader.send(MurmurFrame::Links(links))?;

    // Bans: a 16-byte blob (legacy) or eight groups of hex digits (1.6).
    let statement = if legacy {
        format!(
            "SELECT rowid, {}, {}, {}, {}, {}, {}, {} FROM bans WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            capped("base", 16),
            small("mask"),
            capped("name", NAME_BYTES),
            capped("hash", SHORT_BYTES),
            capped("reason", NAME_BYTES),
            small_text("start"),
            small("duration")
        )
    } else {
        format!(
            "SELECT rowid, {}, {}, {}, {}, {}, {}, {} FROM bans WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            capped("ipv6_base_address", 64),
            small("prefix_length"),
            capped("banned_user_name", NAME_BYTES),
            capped("banned_user_cert_hash", SHORT_BYTES),
            capped("reason", NAME_BYTES),
            small_text("start_date"),
            small("duration")
        )
    };
    let mut bans = Vec::new();
    let mut index = 0_usize;
    reader
        .each("bans", &statement, |reader, row| {
            index += 1;
            let at = || format!("ban {index}");
            let address = if legacy {
                match reader.capped_bytes(row, 1, at) {
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
                reader.capped_text(row, 1, at)
            };
            bans.push(MurmurBan {
                address,
                mask: reader.optional(int(row, 3), at),
                name: reader.capped_text(row, 4, at),
                hash: reader.capped_text(row, 6, at),
                reason: reader.capped_text(row, 8, at),
                start: reader.optional(text(row, 10), at),
                duration: reader.optional(int(row, 11), at),
            });
        })
        .await?;
    reader.send(MurmurFrame::Bans(bans))?;

    // Channel listeners (Murmur 1.4 and later).
    if tables.contains("channel_listeners") {
        let statement = format!(
            "SELECT rowid, {}, {}, {}, {} FROM channel_listeners WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
            small("user_id"),
            small("channel_id"),
            small(&optional("channel_listeners", "volume_adjustment")),
            small(&optional("channel_listeners", "enabled"))
        );
        let mut listeners = Vec::new();
        reader
            .each("channel_listeners", &statement, |reader, row| {
                let what = || "channel_listeners: a row".to_owned();
                let (Some(user_id), Some(channel)) = (
                    reader.require(int(row, 1), what),
                    reader.require(int(row, 2), what),
                ) else {
                    return;
                };
                let at = || format!("listener of user {user_id} in channel {channel}");
                let volume = reader.optional(real(row, 3), at);
                let enabled = reader.optional(flag(row, 4), at).unwrap_or(true);
                listeners.push(MurmurListener {
                    user_id,
                    channel,
                    volume,
                    enabled,
                });
            })
            .await?;
        reader.send(MurmurFrame::Listeners(listeners))?;
    }

    // Registered users, their properties and textures.
    let (info, info_key, info_value) = layout.user_info;
    let statement = format!(
        "SELECT rowid, {}, {}, {} FROM {info} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small("user_id"),
        small(info_key),
        capped(info_value, MAX_BLOB_BYTES)
    );
    let mut user_properties: HashMap<(i64, i64), String> = HashMap::new();
    let mut emails = HashSet::new();
    reader
        .each(info, &statement, |reader, row| {
            let what = || format!("{info}: a row");
            let (Some(user), Some(key)) = (
                reader.require(int(row, 1), what),
                reader.require(int(row, 2), what),
            ) else {
                return;
            };
            match key {
                USER_EMAIL => {
                    emails.insert(user);
                }
                USER_COMMENT | USER_HASH => {
                    if let Some(value) =
                        reader.capped_text(row, 3, || format!("user {user} property {key}"))
                    {
                        user_properties.insert((user, key), value);
                    }
                }
                _ => {}
            }
        })
        .await?;
    let users_table = layout.users;
    let statement = format!(
        "SELECT rowid, {}, {}, {}, {}, {}, {}, {}, {} FROM {users_table} WHERE server_id = $1 AND rowid > $2 ORDER BY rowid LIMIT $3",
        small("user_id"),
        capped(layout.user_name, NAME_BYTES),
        capped(layout.password, SHORT_BYTES),
        capped(&optional(users_table, "salt"), SHORT_BYTES),
        small(&optional(users_table, layout.kdf_iterations)),
        small(&optional(users_table, layout.last_channel)),
        small_text(&optional(users_table, "last_active")),
        capped(&optional(users_table, "texture"), MAX_BLOB_BYTES),
    );
    let mut users = Vec::new();
    reader
        .each(users_table, &statement, |reader, row| {
            let Some(id) = reader.require(int(row, 1), || "users: user_id".into()) else {
                return;
            };
            let Some(name) = reader.capped_text(row, 2, || format!("user {id}: name")) else {
                reader.note(format!("user {id}: no usable name; skipped"));
                return;
            };
            let at = || format!("user {id}");
            let password = reader.capped_text(row, 4, at);
            let salt = reader.capped_text(row, 6, at);
            let kdf_iterations = reader.optional(int(row, 8), at);
            let last_channel = reader.optional(int(row, 9), at);
            let last_active = reader.optional(text(row, 10), at);
            let texture = reader
                .capped_bytes(row, 11, || format!("user {id}: texture"))
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
        })
        .await?;
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
    use nuschl_ipc::murmur::MurmurDump;

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
    /// malformed rows become notes. The result is the input of nuschld's
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
                .and_then(nuschl_ipc::murmur::decode_blob)
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
        // The fixture nuschld's import test reads.
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
CREATE TABLE "channels" ("server_id" INTEGER NOT NULL, "channel_id" INTEGER NOT NULL, "parent_id" INTEGER NOT NULL, "channel_name" VARCHAR(255), "inherit_acl" SMALLINT);
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
INSERT INTO channels VALUES (1, 0, 0, 'Root', 1);
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
        // The root is its own parent in 1.6.
        assert_eq!(dump.channels.len(), 2);
        assert_eq!(dump.channels[0].parent, Some(0));
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

    /// What a hostile file could try: a `-wal` next to it, a virtual table,
    /// generated columns, odd config keys, oversized values.
    #[tokio::test]
    async fn hostile_files_are_contained() {
        let directory = tempfile::tempdir().unwrap();
        let script = std::fs::read_to_string(fixture_path("murmur-1.5.sql")).unwrap()
            + "INSERT INTO config VALUES (1, 'bad\u{1b}[2Jkey', 'x');\n\
               INSERT INTO config VALUES (1, 'big', zeroblob(2000000));\n\
               INSERT INTO user_info VALUES (1, 7, 3, hex(zeroblob(200)));\n";
        let path = fixture(directory.path(), "murmur.sqlite", &script).await;
        // A sibling write-ahead log is never read (the file is opened immutable).
        let wal = directory.path().join("murmur.sqlite-wal");
        std::fs::write(&wal, vec![0xa5; 4096]).unwrap();
        let read = dump(&path, 1).await.unwrap();
        assert!(!directory.path().join("murmur.sqlite-shm").exists());
        assert_eq!(std::fs::read(&wal).unwrap(), vec![0xa5; 4096]);
        assert!(
            !read
                .config
                .iter()
                .any(|(key, _)| key.contains('\u{1b}') || key == "big")
        );
        for expected in [
            "config: a key with characters Murmur does not use",
            "config big: 2000000 bytes, over the limit",
        ] {
            assert!(
                read.notes.iter().any(|note| note.contains(expected)),
                "{expected} in {:?}",
                read.notes
            );
        }
        // Not a Murmur database: the users table is virtual, or computes values.
        let virtual_table = fixture(
            directory.path(),
            "virtual.sqlite",
            "CREATE VIRTUAL TABLE users USING fts4(user_id, name, pw);",
        )
        .await;
        assert!(
            dump(&virtual_table, 1)
                .await
                .unwrap_err()
                .contains("not a Murmur database")
        );
        let generated = fixture(
            directory.path(),
            "generated.sqlite",
            "CREATE TABLE users (server_id, user_id, name, pw, x GENERATED ALWAYS AS (upper(name)));",
        )
        .await;
        assert!(
            dump(&generated, 1)
                .await
                .unwrap_err()
                .contains("hidden or generated columns")
        );
    }

    #[tokio::test]
    async fn refuses_files_that_are_not_murmur_databases() {
        let directory = tempfile::tempdir().unwrap();
        let text = directory.path().join("notes.txt");
        assert!(
            dump(&directory.path().join("missing"), 1)
                .await
                .unwrap_err()
                .contains("cannot read")
        );
        std::fs::write(&text, "hello").unwrap();
        assert!(
            dump(&text, 1)
                .await
                .unwrap_err()
                .contains("not an SQLite database")
        );
        std::fs::write(&text, "definitely not SQLite at all").unwrap();
        assert!(
            dump(&text, 1)
                .await
                .unwrap_err()
                .contains("not an SQLite database")
        );
        let wal = fixture(
            directory.path(),
            "wal.sqlite",
            "PRAGMA journal_mode=WAL; CREATE TABLE users (user_id, name, pw);",
        )
        .await;
        assert!(dump(&wal, 1).await.unwrap_err().contains("write-ahead log"));
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
