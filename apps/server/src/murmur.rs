// SPDX-License-Identifier: EUPL-1.2
//! `nuschld --import-murmur`: move a Murmur (mumble-server) server here.
//!
//! The storage worker reads the Murmur database read-only
//! (`nuschl-storage-worker --read-murmur`, crates/storage/src/murmur.rs); this
//! module treats every row as untrusted and applies the server's own rules:
//! user and channel names against the configured patterns, comments and
//! descriptions against the text limits, textures against the image limit,
//! the caps on ACL entries, groups and bans, and the tree's integrity (one
//! root, no cycles, unique sibling names). What does not pass is skipped and
//! reported by id, never by content. The result is written through the normal
//! storage requests, so it works for SQLite and PostgreSQL alike.
//!
//! Passwords keep Murmur's hashes (PBKDF2-HMAC-SHA384 or legacy SHA-1) until
//! the user's next login replaces them with argon2id (crate::password).
//! Murmur's per-server settings that have an equivalent here are written as a
//! TOML fragment for the operator; the server password and a certificate
//! stored in the database go to owner-only files.
use crate::{
    acl::{AclEntry, Group, perm},
    config::Settings,
    handlers::{MAX_STORED_NAME, truncate_bytes, truncate_chars},
    password,
    server::{self, StartError},
    state::{Ban, Channel, Registered, State, now_unix, random},
    storage::{self, Backend},
    text,
};
use bytes::Bytes;
use nuschl_ipc::{
    Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    murmur::{MurmurDump, MurmurFrame, decode_blob},
    server::ServerStorageRequest,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    fmt::Write as _,
    net::Ipv6Addr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

/// How long reading the Murmur database may take.
const READ_TIMEOUT: Duration = Duration::from_secs(600);
/// Hard caps, whatever the configuration says (the handlers' caps).
const MAX_CHANNELS: usize = 100_000;
const MAX_ACL_ENTRIES: usize = 512;
const MAX_GROUPS: usize = 512;
const MAX_GROUP_MEMBERS: usize = 8192;
const MAX_GROUP_NAME: usize = 512;
const MAX_BANS: usize = 65_536;
/// Murmur's legacy raw ARGB textures (600×60, before 1.2.3); clients today
/// expect image files.
const RAW_TEXTURE_BYTES: usize = 600 * 60 * 4;

#[derive(Clone, Debug)]
pub struct Options {
    pub path: PathBuf,
    /// Murmur's virtual server (`server_id`), 1 for a single-server setup.
    pub server_id: i64,
    /// Replace existing channels, registered users and bans.
    pub replace_existing: bool,
}

/// `nuschld admin import-murmur` (and the older `--import-murmur`).
#[derive(clap::Args, Clone, Debug)]
pub struct ImportArguments {
    /// The Murmur database (`murmur.sqlite`, `mumble-server.sqlite`); opened
    /// read-only. Stop Murmur first.
    pub path: PathBuf,
    /// The Murmur virtual server to import.
    #[arg(long, default_value_t = 1)]
    pub murmur_server_id: i64,
    /// Delete this server's channels, registered users and bans first.
    #[arg(long)]
    pub replace_existing: bool,
}

impl From<ImportArguments> for Options {
    fn from(arguments: ImportArguments) -> Self {
        Self {
            path: arguments.path,
            server_id: arguments.murmur_server_id,
            replace_existing: arguments.replace_existing,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("reading the Murmur database failed: {0}")]
    Reader(String),
    #[error(
        "{0} writes to this server's storage failed (see the log above); the import is \
         incomplete. Fix the cause, then import again with --replace-existing"
    )]
    Storage(u64),
    #[error("{0}")]
    NotEmpty(String),
    #[error(transparent)]
    Start(#[from] StartError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What was imported, skipped and written.
#[derive(Debug, Default)]
pub struct Report {
    pub imported: BTreeMap<&'static str, usize>,
    pub skipped: Vec<String>,
    /// From the reader: malformed or oversized values, cut tables.
    pub notes: Vec<String>,
    /// Limits the imported data exceeds, and files written.
    pub advice: Vec<String>,
    pub sha1_passwords: usize,
    pub pbkdf2_passwords: usize,
}

impl Report {
    fn count(&mut self, what: &'static str) {
        *self.imported.entry(what).or_default() += 1;
    }

    /// An imported Murmur hash, by kind.
    fn count_password(&mut self, hash: Option<&str>) {
        match hash {
            Some(hash) if hash.starts_with(password::MURMUR_SHA1) => self.sha1_passwords += 1,
            Some(_) => self.pbkdf2_passwords += 1,
            None => {}
        }
    }

    fn skip(&mut self, reason: String) {
        self.skipped.push(reason);
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "imported from Murmur:");
        for (what, count) in &self.imported {
            let _ = writeln!(out, "  {what}: {count}");
        }
        if self.pbkdf2_passwords + self.sha1_passwords > 0 {
            let _ = writeln!(
                out,
                "  passwords: {} PBKDF2, {} legacy unsalted SHA-1; each becomes argon2id at the user's next login",
                self.pbkdf2_passwords, self.sha1_passwords
            );
        }
        if !self.skipped.is_empty() {
            let _ = writeln!(out, "skipped ({}):", self.skipped.len());
            for line in &self.skipped {
                let _ = writeln!(out, "  {line}");
            }
        }
        if !self.notes.is_empty() {
            let _ = writeln!(
                out,
                "from reading the Murmur database ({}):",
                self.notes.len()
            );
            for line in &self.notes {
                let _ = writeln!(out, "  {line}");
            }
        }
        for line in &self.advice {
            let _ = writeln!(out, "{line}");
        }
        out
    }
}

/// Run the reader and collect its frames.
pub async fn read_dump(worker: &Path, options: &Options) -> Result<MurmurDump, ImportError> {
    let mut child = Command::new(worker)
        .arg("--read-murmur")
        .arg(&options.path)
        .arg("--murmur-server-id")
        .arg(options.server_id.to_string())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            ImportError::Reader(format!("cannot start {}: {error}", worker.display()))
        })?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| ImportError::Reader("no output from the reader".into()))?;
    let read = async {
        let mut dump = MurmurDump::default();
        loop {
            let size = output
                .read_u32()
                .await
                .map_err(|_| ImportError::Reader("the reader stopped early".into()))?
                as usize;
            if size > MAX_FRAME_BYTES {
                return Err(ImportError::Reader(
                    "oversized frame from the reader".into(),
                ));
            }
            let mut body = vec![0; size];
            output.read_exact(&mut body).await?;
            let envelope: Envelope<MurmurFrame> = serde_json::from_slice(&body)
                .map_err(|error| ImportError::Reader(format!("bad frame: {error}")))?;
            if envelope.version != PROTOCOL_VERSION {
                return Err(ImportError::Reader("reader protocol mismatch".into()));
            }
            if dump.apply(envelope.payload).map_err(ImportError::Reader)? {
                return Ok(dump);
            }
        }
    };
    let dump = tokio::time::timeout(READ_TIMEOUT, read)
        .await
        .map_err(|_| ImportError::Reader("timed out".into()))??;
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    Ok(dump)
}

fn valid_hash(text: &str) -> Option<String> {
    let text = text.trim().to_ascii_lowercase();
    (text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(text)
}

/// Murmur's dates: SQLite's `YYYY-MM-DD HH:MM:SS`, Qt's ISO form, or (1.6)
/// Unix seconds.
fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse().ok();
    }
    crate::timefmt::parse_iso8601(&text.replacen(' ', "T", 1))
}

fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

fn to_u32(value: i64) -> Option<u32> {
    u32::try_from(value).ok()
}

/// Murmur stores permissions as a signed int (`static_cast<int>`), so -1
/// (every bit, as Ice may set it) is valid.
fn permission_bits(value: i64) -> Option<u32> {
    match i32::try_from(value) {
        Ok(signed) => Some(signed as u32),
        Err(_) => u32::try_from(value).ok(),
    }
}

/// Murmur's channel and user ids are signed 32-bit; temporary channels here
/// count down from `i32::MAX`.
fn to_id(value: i64) -> Option<u32> {
    to_u32(value).filter(|id| *id < i32::MAX as u32)
}

/// Whether the target database holds anything besides the root channel and
/// SuperUser: (channels, users, bans).
fn existing(state: &State) -> (Vec<u32>, Vec<u32>, usize) {
    let channels = state
        .channels
        .0
        .keys()
        .copied()
        .filter(|id| *id != 0)
        .collect();
    let users = state
        .registry
        .users
        .keys()
        .copied()
        .filter(|id| *id != 0)
        .collect();
    (channels, users, state.bans.len())
}

/// Remove channels (but the root), registered users (but SuperUser) and bans.
fn clear(state: &mut State) {
    let top: Vec<u32> = state
        .channels
        .get(0)
        .map(|root| root.children.clone())
        .unwrap_or_default();
    for id in top {
        // The storage removes descendants, ACLs, groups and links with it.
        state.storage.delete_channel(id);
    }
    state.channels.0.retain(|id, _| *id == 0);
    if let Some(root) = state.channels.get_mut(0) {
        root.children.clear();
        root.links.clear();
    }
    let users: Vec<u32> = state
        .registry
        .users
        .keys()
        .copied()
        .filter(|id| *id != 0)
        .collect();
    for id in users {
        if let Some(removed) = state.registry.remove(id) {
            state.storage.delete_user(id);
            state
                .storage
                .send(ServerStorageRequest::HistoryForgetAccount {
                    account: hex::encode(removed.account),
                });
        }
    }
    state.bans.clear();
    state.persist_bans();
}

/// Validate the dump against the server's rules and write it.
pub fn apply(
    state: &mut State,
    dump: MurmurDump,
    replace_existing: bool,
) -> Result<Report, ImportError> {
    let mut report = Report {
        notes: dump.notes.clone(),
        ..Report::default()
    };
    // Whole-import failures come before anything is changed.
    // (Murmur 1.6 gives the root itself as its parent, earlier versions NULL.)
    if !dump.channels.iter().any(|channel| channel.id == 0) {
        return Err(ImportError::Reader(
            "the Murmur database has no root channel for this virtual server (Murmur creates \
             it when the server first starts); nothing was changed"
                .into(),
        ));
    }
    let (channels, users, bans) = existing(state);
    if !channels.is_empty() || !users.is_empty() || bans > 0 {
        if !replace_existing {
            return Err(ImportError::NotEmpty(format!(
                "this server already has {} channels, {} registered users and {bans} bans besides \
                 the root channel and SuperUser. Nothing was changed. --replace-existing deletes \
                 them (their messages stay in the history: deleted channels' history becomes an \
                 archive only SuperUser reads, deleted accounts show as deleted users) and imports \
                 Murmur's in their place.",
                channels.len(),
                users.len()
            )));
        }
        report.advice.push(format!(
            "replaced {} channels, {} registered users and {bans} bans",
            channels.len(),
            users.len()
        ));
        clear(state);
    }
    let legacy = dump.header.layout != "1.6";
    let limits = state.text_limits();
    let settings = state.settings.clone();

    // ---- the channel tree: one root, parents first, no cycles ----
    let mut by_id: BTreeMap<u32, &nuschl_ipc::murmur::MurmurChannel> = BTreeMap::new();
    for channel in &dump.channels {
        match to_id(channel.id) {
            Some(id) if by_id.contains_key(&id) => {
                report.skip(format!("channel {id}: duplicate id"));
            }
            Some(id) => {
                by_id.insert(id, channel);
            }
            None => report.skip(format!("channel {}: invalid id", channel.id)),
        }
    }
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (&id, channel) in &by_id {
        if id == 0 {
            continue;
        }
        match channel.parent.and_then(to_id) {
            Some(parent) if parent != id => children.entry(parent).or_default().push(id),
            _ => report.skip(format!("channel {id}: invalid parent")),
        }
    }
    let Some(root) = by_id.get(&0) else {
        return Err(ImportError::Reader(
            "the Murmur database has no root channel (id 0)".into(),
        ));
    };
    // Root: Murmur's ACLs, groups, description replace this server's.
    let mut imported_channels: BTreeSet<u32> = BTreeSet::from([0]);
    {
        let root_name = root.name.trim().to_owned();
        let description = root.description.clone().unwrap_or_default();
        let description = match text::check(&description, limits) {
            Ok(changed) => changed.unwrap_or(description),
            Err(text::TooLong) => {
                report.skip("channel 0: description over the text limits; dropped".into());
                String::new()
            }
        };
        if let Some(channel) = state.channels.get_mut(0) {
            if !root_name.is_empty() && root_name.chars().count() <= 512 && !has_control(&root_name)
            {
                channel.name = root_name;
            }
            channel.set_description(description);
            channel.node.inherit_acl = true;
            channel.node.acls.clear();
            channel.node.groups.clear();
        }
    }
    let mut order = Vec::new();
    let mut skipped = HashSet::new();
    let mut queue = VecDeque::from([(0_u32, 0_usize)]);
    let mut deepest = 0;
    while let Some((parent, depth)) = queue.pop_front() {
        let mut kids = children.remove(&parent).unwrap_or_default();
        // Murmur loads siblings ordered by name; the first of equal names wins.
        kids.sort_by(|a, b| by_id[a].name.cmp(&by_id[b].name).then(a.cmp(b)));
        let mut names = HashSet::new();
        for id in kids {
            let channel = by_id[&id];
            let name = channel.name.as_str();
            if order.len() >= MAX_CHANNELS {
                report.skip(format!(
                    "channel {id} and its subchannels: more than {MAX_CHANNELS} channels"
                ));
                skipped.insert(id);
                continue;
            }
            if name.is_empty()
                || name.chars().count() > 512
                || name.len() > MAX_STORED_NAME
                || !settings.channel_name.is_match(name)
            {
                report.skip(format!(
                    "channel {id} and its subchannels: the name does not match channel_name_pattern"
                ));
                skipped.insert(id);
                continue;
            }
            if !names.insert(name.to_owned()) {
                report.skip(format!(
                    "channel {id} and its subchannels: a sibling has the same name"
                ));
                skipped.insert(id);
                continue;
            }
            imported_channels.insert(id);
            order.push((id, parent));
            deepest = deepest.max(depth + 1);
            queue.push_back((id, depth + 1));
        }
    }
    // Whatever was not reached hangs off a skipped channel or is in a cycle.
    for (parent, kids) in children {
        // Below a skipped channel: reported with it.
        let mut current = parent;
        let mut below_skipped = false;
        for _ in 0..=by_id.len() {
            if skipped.contains(&current) {
                below_skipped = true;
                break;
            }
            match by_id
                .get(&current)
                .and_then(|channel| channel.parent)
                .and_then(to_id)
            {
                Some(next) if current != 0 => current = next,
                _ => break,
            }
        }
        if below_skipped {
            continue;
        }
        for id in kids {
            report.skip(format!(
                "channel {id}: its parent {parent} does not exist, or the parents form a cycle"
            ));
        }
    }
    let config = &settings.config;
    if order.len() + 1 > config.channel_count_limit as usize && config.channel_count_limit > 0 {
        report.advice.push(format!(
            "note: {} channels exceed channel_count_limit = {}; nobody can create more until it is raised",
            order.len() + 1,
            config.channel_count_limit
        ));
    }
    if deepest >= config.channel_nesting_limit as usize && config.channel_nesting_limit > 0 {
        report.advice.push(format!(
            "note: channels nest {deepest} deep, channel_nesting_limit is {}",
            config.channel_nesting_limit
        ));
    }

    // ---- registered users ----
    let mut user_ids: BTreeSet<u32> = BTreeSet::new();
    let mut names: HashSet<String> = HashSet::from(["superuser".to_owned()]);
    let mut blobs: Vec<(u32, String, Bytes)> = Vec::new();
    let mut highest = 0_u32;
    for user in &dump.users {
        let Some(id) = to_id(user.id) else {
            report.skip(format!("user {}: invalid id", user.id));
            continue;
        };
        if !user_ids.insert(id) {
            report.skip(format!("user {id}: duplicate id"));
            continue;
        }
        let password_hash = match (&user.password, user.kdf_iterations) {
            (Some(hash), _) if hash.trim().is_empty() => None,
            (None, _) => None,
            (Some(hash), Some(iterations)) if iterations > 0 => {
                let stored =
                    password::murmur_pbkdf2(iterations, user.salt.as_deref().unwrap_or(""), hash);
                if stored.is_none() {
                    report.skip(format!(
                        "user {id}: malformed PBKDF2 password or more than {} iterations; \
                         imported without a password",
                        password::MAX_MURMUR_ITERATIONS
                    ));
                }
                stored
            }
            (Some(hash), _) => {
                let stored = password::murmur_sha1(hash);
                if stored.is_none() {
                    report.skip(format!(
                        "user {id}: malformed SHA-1 password; imported without a password"
                    ));
                }
                stored
            }
        };
        if id == 0 {
            // SuperUser: this server's password stays if it has one.
            let ours = state.registry.users.get(&0).cloned();
            if let Some(mut superuser) = ours {
                if superuser.password_hash.is_none() && password_hash.is_some() {
                    report.count_password(password_hash.as_deref());
                    superuser.password_hash = password_hash;
                    report
                        .advice
                        .push("SuperUser keeps Murmur's password".into());
                } else if password_hash.is_some() {
                    report.advice.push(
                        "SuperUser keeps this server's password (Murmur's was not imported)".into(),
                    );
                }
                state.registry.insert(superuser);
                state.persist_registration(0);
            }
            continue;
        }
        let name = user.name.as_str();
        if !state.valid_username(name) {
            user_ids.remove(&id);
            report.skip(format!(
                "user {id}: the name does not match username_pattern"
            ));
            continue;
        }
        if !names.insert(name.to_lowercase()) {
            user_ids.remove(&id);
            report.skip(format!(
                "user {id}: another account has the same name (names are unique regardless of case)"
            ));
            continue;
        }
        let certificate_hash = match user.certificate_hash.as_deref() {
            Some(hash) => {
                let valid = valid_hash(hash);
                if valid.is_none() {
                    report.skip(format!("user {id}: malformed certificate hash; dropped"));
                }
                valid
            }
            None => None,
        };
        let comment = match user.comment.clone() {
            Some(comment) => match text::check(&comment, limits) {
                Ok(changed) => changed.unwrap_or(comment),
                Err(text::TooLong) => {
                    report.skip(format!("user {id}: comment over the text limits; dropped"));
                    String::new()
                }
            },
            None => String::new(),
        };
        let texture = match user.texture.as_deref().map(decode_blob) {
            Some(Some(texture)) if texture.len() == RAW_TEXTURE_BYTES => {
                report.skip(format!(
                    "user {id}: texture in the raw format of Mumble before 1.2.3; dropped"
                ));
                Bytes::new()
            }
            Some(Some(texture))
                if config.image_message_length > 0
                    && texture.len() > config.image_message_length as usize =>
            {
                report.skip(format!(
                    "user {id}: texture over image_message_length; dropped"
                ));
                Bytes::new()
            }
            Some(Some(texture)) => Bytes::from(texture),
            Some(None) => {
                report.skip(format!("user {id}: undecodable texture; dropped"));
                Bytes::new()
            }
            None => Bytes::new(),
        };
        let last_channel = user
            .last_channel
            .and_then(to_id)
            .filter(|channel| imported_channels.contains(channel));
        let last_active = user.last_active.as_deref().and_then(parse_time);
        report.count_password(password_hash.as_deref());
        state.registry.insert(Registered {
            id,
            account: random(),
            name: name.to_owned(),
            password_hash,
            certificate_hash,
            last_channel,
            last_active,
        });
        state.persist_registration(id);
        if !comment.is_empty() || !texture.is_empty() {
            blobs.push((id, comment, texture));
        }
        highest = highest.max(id);
        report.count("registered users");
    }
    for (id, comment, texture) in blobs {
        if !comment.is_empty() {
            state.storage.set_comment(id, comment);
            report.count("comments");
        }
        if !texture.is_empty() {
            use base64::Engine as _;
            state.storage.set_texture(
                id,
                base64::engine::general_purpose::STANDARD.encode(&texture),
            );
            report.count("avatars");
        }
    }
    state.registry.next_id = state.registry.next_id.max(highest.saturating_add(1));
    state.storage.set_next_user_id(state.registry.next_id);
    let registered = |id: u32| state.registry.users.contains_key(&id);

    // ---- ACLs and groups ----
    let mut acls: BTreeMap<u32, Vec<&nuschl_ipc::murmur::MurmurAcl>> = BTreeMap::new();
    for acl in &dump.acls {
        acls.entry(to_id(acl.channel).unwrap_or(u32::MAX))
            .or_default()
            .push(acl);
    }
    let mut node_acls: BTreeMap<u32, Vec<AclEntry>> = BTreeMap::new();
    for (channel, mut entries) in acls {
        if !imported_channels.contains(&channel) {
            report.skip(format!(
                "{} ACL entries of channel {channel}: the channel was not imported",
                entries.len()
            ));
            continue;
        }
        entries.sort_by_key(|acl| acl.priority);
        let mut list = Vec::new();
        for acl in entries {
            let at = format!("ACL {} of channel {channel}", acl.priority);
            if list.len() >= MAX_ACL_ENTRIES {
                report.skip(format!("{at}: more than {MAX_ACL_ENTRIES} entries"));
                continue;
            }
            let (Some(grant), Some(deny)) = (permission_bits(acl.grant), permission_bits(acl.deny))
            else {
                report.skip(format!("{at}: invalid permission bits"));
                continue;
            };
            let (user_id, group) = match (acl.user_id, acl.group.as_deref()) {
                (Some(user), _) => match to_id(user).filter(|id| registered(*id)) {
                    Some(id) => (Some(id), String::new()),
                    None => {
                        report.skip(format!("{at}: user {user} was not imported"));
                        continue;
                    }
                },
                (None, Some(group))
                    if !group.is_empty()
                        && group.len() <= MAX_GROUP_NAME
                        && !has_control(group) =>
                {
                    (None, group.to_owned())
                }
                _ => {
                    report.skip(format!("{at}: applies to nobody or has an invalid group"));
                    continue;
                }
            };
            list.push(AclEntry {
                apply_here: acl.apply_here,
                apply_subs: acl.apply_subs,
                user_id,
                group,
                grant: grant & perm::ALL,
                deny: deny & perm::ALL,
            });
            report.count("ACL entries");
        }
        node_acls.insert(channel, list);
    }
    let mut node_groups: BTreeMap<u32, BTreeMap<String, Group>> = BTreeMap::new();
    for group in &dump.groups {
        let channel = to_id(group.channel).unwrap_or(u32::MAX);
        let at = format!(
            "group {:?}… of channel {channel}",
            group.name.chars().take(32).collect::<String>()
        );
        if !imported_channels.contains(&channel) {
            report.skip(format!("{at}: the channel was not imported"));
            continue;
        }
        if group.name.is_empty() || group.name.len() > MAX_GROUP_NAME || has_control(&group.name) {
            report.skip(format!("a group of channel {channel}: invalid name"));
            continue;
        }
        let groups = node_groups.entry(channel).or_default();
        if groups.len() >= MAX_GROUPS || groups.contains_key(&group.name) {
            report.skip(format!("{at}: duplicate or more than {MAX_GROUPS} groups"));
            continue;
        }
        let members = |ids: &[i64], report: &mut Report| -> BTreeSet<u32> {
            let mut out = BTreeSet::new();
            for &member in ids {
                match to_id(member).filter(|id| registered(*id)) {
                    Some(id) => {
                        out.insert(id);
                    }
                    None => report.skip(format!("{at}: member {member} was not imported")),
                }
            }
            out
        };
        let add = members(&group.add, &mut report);
        let remove = members(&group.remove, &mut report);
        if add.len() + remove.len() > MAX_GROUP_MEMBERS {
            report.skip(format!("{at}: more than {MAX_GROUP_MEMBERS} members"));
            continue;
        }
        groups.insert(
            group.name.clone(),
            Group {
                inherit: group.inherit,
                inheritable: group.inheritable,
                add,
                remove,
            },
        );
        report.count("groups");
    }

    // ---- channels, parents first (the storage enforces parents exist) ----
    if let Some(channel) = state.channels.get_mut(0) {
        channel.node.acls = node_acls.remove(&0).unwrap_or_default();
        channel.node.groups = node_groups.remove(&0).unwrap_or_default();
        if let Some(murmur) = by_id.get(&0) {
            channel.node.inherit_acl = murmur.inherit_acl;
        }
    }
    state.persist_channel(0);
    for &(id, parent) in &order {
        let murmur = by_id[&id];
        let mut channel = Channel::new(id, Some(parent), murmur.name.clone());
        if let Some(description) = murmur.description.clone() {
            match text::check(&description, limits) {
                Ok(changed) => channel.set_description(changed.unwrap_or(description)),
                Err(text::TooLong) => {
                    report.skip(format!(
                        "channel {id}: description over the text limits; dropped"
                    ));
                }
            }
        }
        channel.position = murmur
            .position
            .as_deref()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        channel.max_users = murmur
            .max_users
            .as_deref()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        channel.node.inherit_acl = murmur.inherit_acl;
        channel.node.acls = node_acls.remove(&id).unwrap_or_default();
        channel.node.groups = node_groups.remove(&id).unwrap_or_default();
        state
            .conversations
            .add_channel(channel.conversation_id, channel.id);
        state.channels.0.insert(id, channel);
        if let Some(parent) = state.channels.get_mut(parent) {
            parent.children.push(id);
        }
        state.persist_channel(id);
        report.count("channels");
    }

    // ---- links ----
    let mut links = BTreeSet::new();
    for &(a, b) in &dump.links {
        match (to_id(a), to_id(b)) {
            (Some(a), Some(b))
                if a != b && imported_channels.contains(&a) && imported_channels.contains(&b) =>
            {
                links.insert((a.min(b), a.max(b)));
            }
            _ => report.skip(format!("link {a}–{b}: a channel was not imported")),
        }
    }
    for (a, b) in links {
        if let Some(channel) = state.channels.get_mut(a) {
            channel.links.insert(b);
        }
        if let Some(channel) = state.channels.get_mut(b) {
            channel.links.insert(a);
        }
        state.storage.link(a, b);
        report.count("links");
    }

    // ---- channel listeners ----
    for listener in &dump.listeners {
        let (Some(user), Some(channel)) = (to_id(listener.user_id), to_id(listener.channel)) else {
            continue;
        };
        if !listener.enabled {
            continue;
        }
        if !registered(user) || !imported_channels.contains(&channel) {
            report.skip(format!(
                "listener of user {user} in channel {channel}: not imported"
            ));
            continue;
        }
        let volume = listener
            .volume
            .filter(|volume| volume.is_finite() && (0.0..=16.0).contains(volume))
            .unwrap_or(1.0) as f32;
        state.storage.set_listener(user, channel, volume);
        report.count("channel listeners");
    }

    // ---- bans ----
    let now = now_unix();
    let mut imported_bans = Vec::new();
    for (index, murmur) in dump.bans.iter().enumerate() {
        let at = format!("ban {}", index + 1);
        if imported_bans.len() >= MAX_BANS {
            report.skip(format!("{at}: more than {MAX_BANS} bans"));
            continue;
        }
        let address = murmur
            .address
            .as_deref()
            .and_then(|text| text.trim().parse::<Ipv6Addr>().ok())
            .filter(|address| !address.is_unspecified());
        let mask = murmur.mask.unwrap_or(0);
        let address = address.filter(|_| (8..=128).contains(&mask));
        let hash = murmur.hash.as_deref().and_then(valid_hash);
        // Murmur 1.5 loads only bans with an address (`Ban::isValid`); 1.6
        // also certificate-hash bans.
        if address.is_none() && (legacy || hash.is_none()) {
            report.skip(format!("{at}: no valid address and prefix length"));
            continue;
        }
        let Some(duration) = murmur.duration.and_then(to_u32) else {
            report.skip(format!("{at}: invalid duration"));
            continue;
        };
        let start = match murmur.start.as_deref().and_then(parse_time) {
            Some(start) => start,
            None => {
                report.skip(format!("{at}: unreadable start; counted from now"));
                now
            }
        };
        // The storage refuses texts over 1024 bytes, and with them every ban.
        let clip = |text: &Option<String>| {
            let mut text = text.clone().unwrap_or_default();
            truncate_chars(&mut text, 1000);
            truncate_bytes(&mut text, 1000);
            text
        };
        let ban = Ban {
            private_address: false,
            address,
            mask: u8::try_from(mask).unwrap_or(128),
            name: clip(&murmur.name),
            hash: hash.unwrap_or_default(),
            reason: clip(&murmur.reason),
            start,
            duration,
        };
        if ban.expired(now) {
            report.count("expired bans (not imported)");
            continue;
        }
        if !imported_bans.contains(&ban) {
            imported_bans.push(ban);
            report.count("bans");
        }
    }
    state.bans = imported_bans;
    state.persist_bans();
    if dump.users.iter().any(|user| user.email) {
        report.advice.push(
            "note: e-mail addresses of registered users were not imported (nuschld has no \
             e-mail login)"
                .into(),
        );
    }
    Ok(report)
}

/// murmur.ini keys of the per-server `config` table with an equivalent here.
#[derive(Clone, Copy)]
enum Kind {
    Text,
    Unsigned,
    Bool,
    Pattern,
    /// -1 is unlimited (`None`).
    Limit,
    Version,
}

const SETTINGS: &[(&str, &str, Kind)] = &[
    ("welcometext", "welcome_text", Kind::Text),
    ("registername", "registered_name", Kind::Text),
    ("users", "max_users", Kind::Unsigned),
    ("usersperchannel", "max_users_per_channel", Kind::Unsigned),
    ("bandwidth", "max_bandwidth", Kind::Unsigned),
    ("timeout", "timeout", Kind::Unsigned),
    ("textmessagelength", "text_message_length", Kind::Unsigned),
    ("imagemessagelength", "image_message_length", Kind::Unsigned),
    ("allowhtml", "allow_html", Kind::Bool),
    ("defaultchannel", "default_channel", Kind::Unsigned),
    ("rememberchannel", "remember_channel", Kind::Bool),
    (
        "rememberchannelduration",
        "remember_channel_duration",
        Kind::Unsigned,
    ),
    ("allowping", "allow_ping", Kind::Bool),
    ("certrequired", "certificate_required", Kind::Bool),
    (
        "channelnestinglimit",
        "channel_nesting_limit",
        Kind::Unsigned,
    ),
    ("channelcountlimit", "channel_count_limit", Kind::Unsigned),
    ("username", "username_pattern", Kind::Pattern),
    ("channelname", "channel_name_pattern", Kind::Pattern),
    ("messagelimit", "message_limit", Kind::Unsigned),
    ("messageburst", "message_burst", Kind::Unsigned),
    ("pluginmessagelimit", "plugin_message_limit", Kind::Unsigned),
    // Murmur reads the per-server plugin limit under this misspelt key.
    ("mpluginessagelimit", "plugin_message_limit", Kind::Unsigned),
    ("pluginmessageburst", "plugin_message_burst", Kind::Unsigned),
    ("listenersperchannel", "listeners_per_channel", Kind::Limit),
    ("listenersperuser", "listeners_per_user", Kind::Limit),
    (
        "broadcastlistenervolumeadjustments",
        "broadcast_listener_volume_adjustments",
        Kind::Bool,
    ),
    ("suggestversion", "suggest_version", Kind::Version),
    ("suggestpositional", "suggest_positional", Kind::Bool),
    ("suggestpushtotalk", "suggest_push_to_talk", Kind::Bool),
    ("allowrecording", "allow_recording", Kind::Bool),
    ("sendversion", "send_version", Kind::Bool),
];

/// A TOML basic string.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            character if character.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(character));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn murmur_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Murmur's 1.x version numbers: "1.4.0", or the 1.5 encodings as a number.
fn murmur_version(text: &str) -> Option<String> {
    let text = text.trim();
    if crate::config::parse_version(text).is_some() {
        return Some(text.to_owned());
    }
    let number: u64 = text.parse().ok()?;
    let (major, minor, patch) = if number > u64::from(u32::MAX) {
        (
            number >> 48,
            (number >> 32) & 0xffff,
            (number >> 16) & 0xffff,
        )
    } else {
        (number >> 16, (number >> 8) & 0xff, number & 0xff)
    };
    Some(format!("{major}.{minor}.{patch}"))
}

/// What the operator should add to the configuration: TOML lines, the server
/// password, and a certificate with its key (PEM), from Murmur's `config` table.
#[derive(Debug, Default)]
pub struct Outputs {
    pub toml: String,
    pub password: Option<String>,
    pub certificate: Option<(String, String)>,
}

pub fn config_outputs(config: &[(String, String)], report: &mut Report) -> Outputs {
    let mut outputs = Outputs::default();
    let values: BTreeMap<&str, &str> = config
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let mut seen = HashSet::new();
    for &(murmur, ours, kind) in SETTINGS {
        let Some(&value) = values.get(murmur) else {
            continue;
        };
        if !seen.insert(ours) {
            continue;
        }
        let line = match kind {
            Kind::Text if value.len() <= 1024 * 1024 => Some(toml_string(value)),
            Kind::Text => None,
            Kind::Unsigned => value
                .trim()
                .parse::<u64>()
                .ok()
                .map(|value| value.to_string()),
            Kind::Bool => murmur_bool(value).map(|value| value.to_string()),
            Kind::Pattern => regex::Regex::new(&format!("^(?:{value})$"))
                .ok()
                .map(|_| toml_string(value)),
            Kind::Limit => match value.trim().parse::<i64>() {
                Ok(limit) if limit < 0 => Some(String::new()),
                Ok(limit) => u32::try_from(limit).ok().map(|limit| limit.to_string()),
                Err(_) => None,
            },
            Kind::Version => murmur_version(value).map(|version| toml_string(&version)),
        };
        match line {
            // Unlimited is the default (the key absent).
            Some(line) if line.is_empty() => {
                let _ = writeln!(
                    outputs.toml,
                    "# {ours}: unlimited (Murmur's -1), the default"
                );
            }
            Some(line) => {
                let _ = writeln!(outputs.toml, "{ours} = {line}");
            }
            None => report.skip(format!("setting {murmur}: unusable value; not converted")),
        }
    }
    if let Some(&password) = values.get("password")
        && !password.is_empty()
    {
        outputs.password = Some(password.to_owned());
    }
    match (values.get("certificate"), values.get("key")) {
        (Some(&certificate), Some(&key)) if !certificate.is_empty() && !key.is_empty() => {
            if values
                .get("passphrase")
                .is_some_and(|value| !value.is_empty())
            {
                report.skip(
                    "the certificate in the Murmur database: its key is encrypted with a \
                     passphrase; not converted"
                        .into(),
                );
            } else {
                outputs.certificate = Some((certificate.to_owned(), key.to_owned()));
            }
        }
        _ => {}
    }
    let known: HashSet<&str> = SETTINGS
        .iter()
        .map(|(murmur, _, _)| *murmur)
        .chain(["password", "certificate", "key", "passphrase"])
        .collect();
    let ignored: Vec<&str> = values
        .keys()
        .copied()
        .filter(|key| !known.contains(key))
        .collect();
    if !ignored.is_empty() {
        report.advice.push(format!(
            "Murmur settings without an equivalent here, not converted: {}",
            ignored.join(", ")
        ));
    }
    outputs
}

fn write_private(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(&temporary)?;
    std::io::Write::write_all(&mut file, contents)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

/// Write the outputs next to the state and tell the operator what to do.
pub fn write_outputs(
    outputs: &Outputs,
    settings: &Settings,
    report: &mut Report,
) -> std::io::Result<()> {
    let directory = &settings.config.state_directory;
    if !outputs.toml.is_empty() {
        let path = directory.join("murmur-import.toml");
        let mut text = String::from(
            "# Murmur's per-server settings (its database's config table), converted by\n\
             # nuschld --import-murmur. Merge them into the configuration file; settings\n\
             # from murmur.ini are not in the database and need to be carried over by hand.\n",
        );
        text.push_str(&outputs.toml);
        write_private(&path, text.as_bytes(), 0o600)?;
        report.advice.push(format!(
            "wrote Murmur's settings to {}; merge them into the configuration",
            path.display()
        ));
    }
    if let Some(password) = &outputs.password {
        let path = directory.join("murmur-server-password");
        write_private(&path, password.as_bytes(), 0o600)?;
        report.advice.push(format!(
            "wrote the server password to {}; set password_file to it (or keep it elsewhere)",
            path.display()
        ));
    }
    if let Some((certificate, key)) = &outputs.certificate {
        let parsed = crate::tls::Identity::from_pem(certificate.as_bytes(), key.as_bytes())
            .and_then(crate::tls::check_identity);
        match parsed {
            Err(error) => report.skip(format!(
                "the certificate in the Murmur database: unusable ({error}); not converted"
            )),
            Ok(()) => {
                let generated_certificate = directory.join("certificate.pem");
                let generated_key = directory.join("private-key.pem");
                let configured = settings.config.certificate.is_some();
                let (certificate_path, key_path) =
                    if !configured && !generated_certificate.exists() && !generated_key.exists() {
                        (generated_certificate, generated_key)
                    } else {
                        (
                            directory.join("murmur-certificate.pem"),
                            directory.join("murmur-private-key.pem"),
                        )
                    };
                write_private(&key_path, key.as_bytes(), 0o600)?;
                write_private(&certificate_path, certificate.as_bytes(), 0o644)?;
                let fingerprint =
                    crate::tls::Identity::from_pem(certificate.as_bytes(), key.as_bytes())
                        .map(|identity| identity.fingerprint())
                        .unwrap_or_default();
                if certificate_path.ends_with("certificate.pem") {
                    report.advice.push(format!(
                        "installed Murmur's certificate (SHA-256 {fingerprint}) as {}: clients \
                         that pinned it keep trusting this server",
                        certificate_path.display()
                    ));
                } else {
                    report.advice.push(format!(
                        "wrote Murmur's certificate (SHA-256 {fingerprint}) to {} and its key \
                         to {}; point certificate and private_key at them so clients that \
                         pinned it keep trusting this server",
                        certificate_path.display(),
                        key_path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The whole import: read, check, write, report.
pub async fn import(
    settings: &Settings,
    backend: Backend,
    options: &Options,
) -> Result<Report, ImportError> {
    let worker = match &backend {
        Backend::Sqlite { worker, .. } | Backend::UrlFile { worker, .. } => worker.clone(),
        Backend::Memory(_) => {
            return Err(ImportError::Reader(
                "an import into an --ephemeral server would be lost".into(),
            ));
        }
    };
    let mut settings = settings.clone();
    settings.config.bind.clear();
    // The state directory 0700 and a new SQLite database 0600, as at startup.
    server::prepare_files(&settings, &backend)?;
    // The operator lock first, as `nuschld admin` takes it: an import never
    // runs against a database a live server (or another admin command) holds.
    let storage = storage::start_locked(backend);
    server::hold_operator_lock(&storage).await?;
    let dump = read_dump(&worker, options).await?;
    let counters = Default::default();
    let mut state = server::load_state_for_admin(&settings, &storage, &counters).await?;
    let config = dump.config.clone();
    let result = apply(&mut state, dump, options.replace_existing);
    // Whatever was queued is written (or has failed) before anything else.
    storage.flush().await;
    let mut report = result?;
    if storage.failures() > 0 {
        return Err(ImportError::Storage(storage.failures()));
    }
    let outputs = config_outputs(&config, &mut report);
    if let Err(error) = write_outputs(&outputs, &settings, &mut report) {
        report.skip(format!(
            "writing Murmur's settings, server password or certificate next to the state \
             failed ({error}); the users, channels and bans are imported"
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_versions() {
        assert_eq!(parse_time("2025-11-11 00:00:00"), Some(1_762_819_200));
        assert_eq!(parse_time("2025-11-11T00:00:00.000Z"), Some(1_762_819_200));
        assert_eq!(parse_time("1762819200"), Some(1_762_819_200));
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(murmur_version("1.4.0").as_deref(), Some("1.4.0"));
        assert_eq!(murmur_version("66560").as_deref(), Some("1.4.0"));
        assert_eq!(
            murmur_version(&((1_u64 << 48) | (5 << 32)).to_string()).as_deref(),
            Some("1.5.0")
        );
        assert_eq!(toml_string("a\"b\\\n\u{1}"), "\"a\\\"b\\\\\\n\\u0001\"");
    }

    #[test]
    fn settings_become_toml_and_secrets_files() {
        let mut report = Report::default();
        let config = [
            ("welcometext", "Hi <b>there</b>"),
            ("users", "50"),
            ("allowhtml", "false"),
            ("listenersperuser", "-1"),
            ("username", "[a-z]+"),
            ("channelname", "("),
            ("password", "letmein"),
            ("registerhostname", "example.org"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        let outputs = config_outputs(&config, &mut report);
        assert!(
            outputs
                .toml
                .contains("welcome_text = \"Hi <b>there</b>\"\n")
        );
        assert!(outputs.toml.contains("max_users = 50\n"));
        assert!(outputs.toml.contains("allow_html = false\n"));
        assert!(outputs.toml.contains("username_pattern = \"[a-z]+\"\n"));
        assert!(!outputs.toml.contains("channel_name_pattern"));
        assert!(!outputs.toml.contains("letmein"));
        assert_eq!(outputs.password.as_deref(), Some("letmein"));
        assert!(
            report
                .skipped
                .iter()
                .any(|line| line.contains("channelname"))
        );
        assert!(
            report
                .advice
                .iter()
                .any(|line| line.contains("registerhostname"))
        );
        // The fragment is valid configuration.
        crate::config::Config::from_toml(&outputs.toml).unwrap();
    }
}
