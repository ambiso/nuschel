// SPDX-License-Identifier: EUPL-1.2
//! `nuscheld admin` (docs/server.md, "Administration"): operator commands
//! that work offline on the database, through the storage worker as the server
//! does. They take the operator lock first, so they refuse to run while a
//! server (or another admin command) uses the database: both keep the state in
//! memory and write it back. Changes go through the same state code as the
//! server's handlers (validation, cascades, persistence).
use crate::{
    config::Settings,
    conversations::{self, Id, hex},
    history,
    retention::wire,
    server::{self, StartError},
    state::{Ban, Counters, State, mapped, now_unix},
    storage::{self, Backend, StorageHandle},
    timefmt::iso8601,
};
use clap::{Args, Subcommand};
use nuschel_ipc::server::{
    Anchor, HistoryVisibility, MAX_RETENTION_DAYS, MessageKind, PageQuery, Retention,
    ServerStorageRequest as Request, ServerStorageResponse as Response,
};
use std::{
    io::{Read, Write},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Operator commands on the database; stop the server first.
#[derive(Subcommand, Debug)]
pub enum Admin {
    /// Registered users.
    #[command(subcommand)]
    Users(Users),
    /// Channels and their retention.
    #[command(subcommand)]
    Channels(Channels),
    /// The ban list.
    #[command(subcommand)]
    Bans(Bans),
    /// Stored messages.
    #[command(subcommand)]
    History(HistoryCommand),
    /// Check the configuration, and that the database opens (or is in use).
    CheckConfig,
    /// Import a Murmur (mumble-server) SQLite database: registered users with
    /// their passwords, channels, ACLs, groups, links, listeners and bans
    /// (docs/server.md, "Importing a Murmur server"). Refuses a server that
    /// has channels, users or bans unless `--replace-existing` is given.
    ImportMurmur(crate::murmur::ImportArguments),
}

#[derive(Subcommand, Debug)]
pub enum Users {
    /// Every registered user.
    List,
    /// One user (a name, or `#id`): account, certificate, ACL entries, groups.
    Show { user: String },
    /// Give a user another name.
    Rename { user: String, name: String },
    /// Unregister a user: their ACL entries and group memberships go, their
    /// account id is never reused, their messages stay (see `history erase-user`).
    Remove {
        user: String,
        /// Really do it.
        #[arg(long)]
        yes: bool,
    },
    /// Set a user's password (SuperUser is `#0`), read from standard input or
    /// a file.
    SetPassword {
        user: String,
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum Channels {
    /// Every channel with its history visibility and retention.
    List,
    /// Set a channel's retention: `forever`, `none` (not archived), a number of
    /// days, or `default` (the server's). Shortening removes stored messages and
    /// needs `--yes`.
    Retention {
        /// A channel id or unique name.
        channel: String,
        retention: String,
        /// Confirm removing what a shorter retention no longer keeps.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Bans {
    /// The ban list, numbered.
    List,
    /// Ban an address or network, a certificate, or both.
    Add(BanArguments),
    /// Remove a ban by its number in `bans list`.
    Remove { number: usize },
}

#[derive(Args, Debug)]
pub struct BanArguments {
    /// An address or network (`192.0.2.7`, `2001:db8::/48`).
    #[arg(long)]
    address: Option<String>,
    /// A certificate hash (SHA-1, 40 hex digits).
    #[arg(long)]
    certificate: Option<String>,
    /// The banned user's name, for the list.
    #[arg(long, default_value = "")]
    name: String,
    #[arg(long, default_value = "")]
    reason: String,
    /// Seconds; 0 is permanent.
    #[arg(long, default_value_t = 0)]
    duration: u32,
}

#[derive(Subcommand, Debug)]
pub enum HistoryCommand {
    /// Export one channel's stored messages as JSON (a channel id or name, or a
    /// conversation id for a deleted channel's archive or a direct conversation).
    Export {
        conversation: String,
        /// Write here (created 0600) instead of standard output.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Turn every stored message by a registered user into a tombstone (GDPR
    /// requests). Copies clients already hold remain.
    EraseUser {
        user: String,
        /// Really do it.
        #[arg(long)]
        yes: bool,
    },
}

/// The database opened for administration: the state as the server would load
/// it, with history writes going straight to storage.
pub struct Opened {
    pub state: State,
    pub storage: StorageHandle,
    writer: tokio::task::JoinHandle<()>,
}

/// Take the operator lock and load the state.
pub async fn open(settings: &Settings, backend: Backend) -> Result<Opened> {
    // A new database file is created owner-only, as the server creates it.
    server::prepare_files(settings, &backend)?;
    let storage = storage::start_locked(backend);
    server::hold_operator_lock(&storage).await?;
    let counters = Arc::new(Counters::default());
    let mut state = server::load_state_for_admin(settings, &storage, &counters).await?;
    let (queues, writer) = history::direct(storage.clone());
    state.history = queues;
    Ok(Opened {
        state,
        storage,
        writer,
    })
}

impl Opened {
    /// Wait until every change is stored.
    pub async fn close(self) {
        let Self {
            state,
            storage,
            writer,
        } = self;
        drop(state);
        let _ = writer.await;
        storage.flush().await;
    }
}

fn find_user(state: &State, user: &str) -> Result<u32> {
    let found = match user.strip_prefix('#').map(str::parse::<u32>) {
        Some(Ok(id)) => state.registry.users.get(&id),
        _ => state.registry.by_name(user),
    };
    Ok(found
        .ok_or_else(|| format!("no registered user {user}"))?
        .id)
}

fn find_channel(state: &State, channel: &str) -> Result<u32> {
    if let Ok(id) = channel.parse::<u32>()
        && state.channels.get(id).is_some()
    {
        return Ok(id);
    }
    let named: Vec<u32> = state
        .channels
        .0
        .values()
        .filter(|candidate| candidate.name == channel)
        .map(|candidate| candidate.id)
        .collect();
    match named.as_slice() {
        [id] => Ok(*id),
        [] => Err(format!("no channel {channel}").into()),
        ids => {
            Err(format!("several channels are called {channel} (ids {ids:?}); use the id").into())
        }
    }
}

fn describe(retention: Retention) -> String {
    match retention {
        Retention::Forever => "forever".into(),
        Retention::Days(1) => "1 day".into(),
        Retention::Days(days) => format!("{days} days"),
        Retention::None => "none (not archived)".into(),
    }
}

/// `forever`, `none`, `default` (`Ok(None)`) or days.
fn parse_retention(text: &str) -> Result<Option<Retention>> {
    Ok(match text {
        "forever" => Some(Retention::Forever),
        "none" | "live-only" => Some(Retention::None),
        "default" => None,
        days => match days.parse::<u32>() {
            Ok(days @ 1..=MAX_RETENTION_DAYS) => Some(Retention::Days(days)),
            _ => {
                return Err(format!(
                    "retention must be forever, none, default or 1 to {MAX_RETENTION_DAYS} days, not {text:?}"
                )
                .into());
            }
        },
    })
}

fn when(unix: Option<i64>) -> String {
    unix.map_or_else(|| "-".into(), iso8601)
}

/// Run an admin command.
pub async fn run(settings: Settings, backend: Backend, command: Admin) -> Result<()> {
    if let Admin::CheckConfig = command {
        return check(&settings, backend).await;
    }
    // The import takes the operator lock and loads the state itself.
    if let Admin::ImportMurmur(arguments) = command {
        let report = crate::murmur::import(&settings, backend, &arguments.into()).await?;
        print!("{}", report.render());
        return Ok(());
    }
    let mut opened = open(&settings, backend).await?;
    let outcome = match command {
        Admin::Users(command) => users(&mut opened, &settings, command).await,
        Admin::Channels(command) => channels(&mut opened, command).await,
        Admin::Bans(command) => bans(&mut opened.state, command),
        Admin::History(command) => history_command(&mut opened, command).await,
        Admin::CheckConfig | Admin::ImportMurmur(_) => Ok(()),
    };
    opened.close().await;
    outcome
}

async fn check(settings: &Settings, backend: Backend) -> Result<()> {
    println!("configuration accepted");
    server::prepare_files(settings, &backend)?;
    let storage = storage::start_locked(backend);
    match server::hold_operator_lock(&storage).await {
        Ok(()) => {
            let snapshot = storage
                .load()
                .await
                .ok_or("could not load the server state")?;
            let users = storage
                .load_users()
                .await
                .ok_or("could not load the users")?;
            println!(
                "database opens: {} channels, {} registered users, {} bans; no server is using it",
                snapshot.channels.len(),
                users.len(),
                snapshot.bans.len()
            );
            let retention = settings.config.history.default_retention();
            println!("history retention by default: {}", describe(retention));
            Ok(())
        }
        Err(StartError::Held) => {
            println!("database in use by a running server (or another admin command)");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn users(opened: &mut Opened, settings: &Settings, command: Users) -> Result<()> {
    let state = &mut opened.state;
    match command {
        Users::List => {
            println!(
                "{:>6}  {:<24} {:<32}  {:<8} {:<12} LAST ACTIVE",
                "ID", "NAME", "ACCOUNT", "PASSWORD", "CERTIFICATE"
            );
            for user in state.registry.users.values() {
                println!(
                    "{:>6}  {:<24} {:<32}  {:<8} {:<12} {}",
                    user.id,
                    user.name,
                    hex(&user.account),
                    if user.password_hash.is_some() {
                        "yes"
                    } else {
                        "no"
                    },
                    user.certificate_hash
                        .as_deref()
                        .map_or("-", |hash| hash.get(..10).unwrap_or(hash)),
                    when(user.last_active)
                );
            }
        }
        Users::Show { user } => {
            let id = find_user(state, &user)?;
            let user = state.registry.users[&id].clone();
            println!("id:           {}", user.id);
            println!("name:         {}", user.name);
            println!("account:      {}", hex(&user.account));
            println!(
                "password:     {}",
                if user.password_hash.is_some() {
                    "set"
                } else {
                    "none"
                }
            );
            println!(
                "certificate:  {}",
                user.certificate_hash.as_deref().unwrap_or("none")
            );
            println!("last active:  {}", when(user.last_active));
            if let Some(channel) = user.last_channel.and_then(|id| state.channels.get(id)) {
                println!("last channel: {} ({})", channel.name, channel.id);
            }
            for channel in state.channels.0.values() {
                for acl in channel
                    .node
                    .acls
                    .iter()
                    .filter(|acl| acl.user_id == Some(id))
                {
                    println!(
                        "ACL entry:    channel {} ({}): grant {:#x}, deny {:#x}",
                        channel.name, channel.id, acl.grant, acl.deny
                    );
                }
                for (name, group) in &channel.node.groups {
                    if group.add.contains(&id) {
                        println!("group:        {name} in {} ({})", channel.name, channel.id);
                    }
                }
            }
            println!(
                "direct conversations: {}",
                state.conversations.directs_of(&user.account).len()
            );
        }
        Users::Rename { user, name } => {
            let id = find_user(state, &user)?;
            let name = name.trim().to_owned();
            if id == 0 {
                return Err("SuperUser cannot be renamed".into());
            }
            if !state.valid_username(&name) {
                return Err(format!("{name:?} is not a valid user name on this server").into());
            }
            if state
                .registry
                .by_name(&name)
                .is_some_and(|other| other.id != id)
            {
                return Err(format!("another user is already called {name}").into());
            }
            let mut registered = state.registry.users[&id].clone();
            let old = std::mem::replace(&mut registered.name, name.clone());
            state.registry.insert(registered);
            state.persist_registration(id);
            println!("renamed {old} (#{id}) to {name}");
        }
        Users::Remove { user, yes } => {
            let id = find_user(state, &user)?;
            if id == 0 {
                return Err("SuperUser cannot be removed".into());
            }
            let name = state.registry.users[&id].name.clone();
            if !yes {
                return Err(format!(
                    "this unregisters {name} (#{id}): their ACL entries and group memberships \
                     go and the account can never come back (its messages stay; see `history \
                     erase-user`). Run again with --yes"
                )
                .into());
            }
            state.unregister(id);
            println!("removed {name} (#{id})");
        }
        Users::SetPassword {
            user,
            password_file,
        } => {
            let id = find_user(state, &user)?;
            let mut password = String::new();
            match password_file {
                Some(path) => std::fs::File::open(path)?
                    .take(4096)
                    .read_to_string(&mut password)?,
                None => std::io::stdin().take(4096).read_to_string(&mut password)?,
            };
            let password = password.trim_end_matches(['\r', '\n']).to_owned();
            if password.is_empty() {
                return Err("the password must not be empty".into());
            }
            if id == 0 {
                server::set_superuser_password(state, password, settings).await?;
            } else {
                let cost = crate::password::Cost {
                    memory_kib: settings.config.password_hashing.memory_kib,
                    passes: settings.config.password_hashing.passes,
                };
                let hash = crate::password::hash(password, cost)
                    .await
                    .ok_or("could not hash the password")?;
                if let Some(registered) = state.registry.users.get_mut(&id) {
                    registered.password_hash = Some(hash);
                }
                state.persist_registration(id);
            }
            println!("set the password of {}", state.registry.users[&id].name);
        }
    }
    Ok(())
}

async fn channels(opened: &mut Opened, command: Channels) -> Result<()> {
    let state = &mut opened.state;
    match command {
        Channels::List => {
            println!(
                "{:>6}  {:>6}  {:<24} {:<12} RETENTION",
                "ID", "PARENT", "NAME", "VISIBILITY"
            );
            for channel in state.channels.0.values() {
                let id = channel.conversation_id;
                let own = if state.own_retention(&id).is_some() {
                    "own"
                } else {
                    "server default"
                };
                println!(
                    "{:>6}  {:>6}  {:<24} {:<12} {} ({own})",
                    channel.id,
                    channel
                        .node
                        .parent
                        .map_or_else(|| "-".to_owned(), |parent| parent.to_string()),
                    channel.name,
                    match channel.history_visibility {
                        HistoryVisibility::Members => "members",
                        HistoryVisibility::SinceAccess => "since_access",
                    },
                    describe(state.retention(&id)),
                );
            }
        }
        Channels::Retention {
            channel,
            retention,
            yes,
        } => {
            let channel = find_channel(state, &channel)?;
            let own = parse_retention(&retention)?;
            let (name, id) = state
                .channels
                .get(channel)
                .map(|channel| (channel.name.clone(), channel.conversation_id))
                .ok_or("no such channel")?;
            let before = state.retention(&id);
            let after = state.retention_with(own);
            if after.shorter_than(before) {
                let now = crate::chat::now_ms();
                let cutoff = after.cutoff_ms(now).unwrap_or(i64::MIN);
                let removed = match opened
                    .storage
                    .request(Request::HistoryCountExpirable {
                        conversation: hex(&id),
                        before_ms: cutoff,
                    })
                    .await
                {
                    Some(Response::Count(count)) => count,
                    _ => return Err("could not count the stored messages".into()),
                };
                let older = match after {
                    Retention::None => "all of them: the channel is no longer archived".into(),
                    _ => format!("those sent before {}", iso8601(cutoff / 1000)),
                };
                if !yes && removed > 0 {
                    return Err(format!(
                        "shortening the retention of {name} from {} to {} removes {removed} \
                         stored messages ({older}) when the server next runs its expiry, and \
                         they are not served from then on. Run again with --yes",
                        describe(before),
                        describe(after)
                    )
                    .into());
                }
                println!("{removed} stored messages will expire ({older})");
            }
            state.set_retention(id, own);
            println!(
                "retention of {name} ({channel}): {}{}",
                describe(state.retention(&id)),
                if own.is_none() {
                    " (server default)"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

fn bans(state: &mut State, command: Bans) -> Result<()> {
    match command {
        Bans::List => {
            for (number, ban) in state.bans.iter().enumerate() {
                let address = ban.address.map_or_else(
                    || "-".to_owned(),
                    |address| match address.to_ipv4_mapped() {
                        Some(v4) => format!("{v4}/{}", ban.mask.saturating_sub(96)),
                        None => format!("{address}/{}", ban.mask),
                    },
                );
                let until = if ban.duration == 0 {
                    "permanent".to_owned()
                } else {
                    format!(
                        "until {}",
                        iso8601(ban.start.saturating_add(i64::from(ban.duration)))
                    )
                };
                println!(
                    "{number:>4}  {address:<24} {:<12} {:<16} {until:<28} {}",
                    if ban.hash.is_empty() {
                        "-"
                    } else {
                        ban.hash.get(..10).unwrap_or(&ban.hash)
                    },
                    if ban.name.is_empty() { "-" } else { &ban.name },
                    ban.reason
                );
            }
        }
        Bans::Add(arguments) => {
            let network = match &arguments.address {
                None => None,
                Some(text) => {
                    let (address, length) = crate::page_limits::parse_cidr(text)
                        .ok_or_else(|| format!("{text:?} is not an address or network"))?;
                    let mask = match address {
                        IpAddr::V4(_) => length.saturating_add(96),
                        IpAddr::V6(_) => length,
                    };
                    Some((mapped(address), mask))
                }
            };
            let hash = arguments
                .certificate
                .as_deref()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if !hash.is_empty()
                && (hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err("a certificate hash is 40 hex digits (SHA-1)".into());
            }
            let ban = Ban {
                address: network.map(|(address, _)| address),
                mask: network.map_or(128, |(_, mask)| mask),
                name: arguments.name,
                hash,
                reason: arguments.reason,
                start: now_unix(),
                duration: arguments.duration,
            };
            if !ban.valid() {
                return Err("a ban needs --address, --certificate or both".into());
            }
            state.bans.push(ban);
            state.persist_bans();
            println!("added ban {}", state.bans.len() - 1);
        }
        Bans::Remove { number } => {
            if number >= state.bans.len() {
                return Err(format!("there is no ban {number}; see `bans list`").into());
            }
            state.bans.remove(number);
            state.persist_bans();
            println!("removed ban {number}");
        }
    }
    Ok(())
}

/// A conversation by channel id or name, or a conversation id.
fn find_conversation(state: &State, text: &str) -> Result<(Id, String)> {
    if let Some(id) = conversations::parse(text)
        && state.conversations.kind(&id).is_some()
    {
        let name = match state.conversations.kind(&id) {
            Some(conversations::Kind::Channel(channel)) => state
                .channels
                .get(channel)
                .map_or_else(String::new, |channel| channel.name.clone()),
            Some(conversations::Kind::Direct(..)) => "direct conversation".into(),
            _ => "archive of a deleted channel".into(),
        };
        return Ok((id, name));
    }
    let channel = find_channel(state, text)?;
    let channel = state.channels.get(channel).ok_or("no such channel")?;
    Ok((channel.conversation_id, channel.name.clone()))
}

async fn history_command(opened: &mut Opened, command: HistoryCommand) -> Result<()> {
    match command {
        HistoryCommand::Export {
            conversation,
            output,
        } => {
            let (id, name) = find_conversation(&opened.state, &conversation)?;
            let document = export(opened, id, &name).await?;
            let text = serde_json::to_string_pretty(&document)?;
            match output {
                None => println!("{text}"),
                Some(path) => {
                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create(true).truncate(true);
                    #[cfg(unix)]
                    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                    let mut file = options.open(&path)?;
                    file.write_all(text.as_bytes())?;
                    file.write_all(b"\n")?;
                    eprintln!(
                        "exported {} rows of {name} to {}",
                        document["messages"].as_array().map_or(0, Vec::len),
                        path.display()
                    );
                }
            }
        }
        HistoryCommand::EraseUser { user, yes } => {
            let id = find_user(&opened.state, &user)?;
            let registered = &opened.state.registry.users[&id];
            let (account, name) = (registered.account, registered.name.clone());
            if !yes {
                return Err(format!(
                    "this turns every message {name} (#{id}) stored on this server into a \
                     tombstone, for good. Copies clients already hold remain. Run again with --yes"
                )
                .into());
            }
            let erased = server::erase_account(&opened.storage, account).await?;
            println!("erased {erased} messages by {name}");
        }
    }
    Ok(())
}

/// One conversation's rows as JSON, oldest first; content past the
/// conversation's retention is left out as pages leave it out.
async fn export(opened: &Opened, id: Id, name: &str) -> Result<serde_json::Value> {
    use serde_json::json;
    let now = crate::chat::now_ms();
    let retention = opened.state.retention(&id);
    let cutoff = retention.cutoff_ms(now);
    let mut rows = Vec::new();
    let mut after = 0;
    loop {
        let page = match opened
            .storage
            .request(Request::HistoryPage {
                query: PageQuery {
                    conversation: hex(&id),
                    anchor: Anchor::After(after),
                    limit: 100,
                    ranges: vec![(1, u64::MAX)],
                    max_bytes: 4 * 1024 * 1024,
                },
            })
            .await
        {
            Some(Response::Page(page)) => page,
            _ => return Err("could not read the history".into()),
        };
        let Some(last) = page.rows.last().map(|row| row.seq) else {
            break;
        };
        after = last;
        for mut row in page.rows {
            if cutoff.is_some_and(|cutoff| row.sent_ms < cutoff)
                && nuschel_ipc::server::expirable(&row)
            {
                row.content.clear();
                row.reply_to = None;
                row.deleted_ms = Some(now);
                row.delete_reason = Some(2);
            }
            rows.push(json!({
                "seq": row.seq,
                "kind": match row.kind {
                    MessageKind::Message => "message",
                    MessageKind::Edit => "edit",
                    MessageKind::Delete => "delete",
                    MessageKind::Void => "void",
                },
                "target": row.target,
                "author": {
                    "account": row.author_account,
                    "user_id": row.author_user_id,
                    "name": row.author_name,
                },
                "format": match row.format {
                    1 => "text".to_owned(),
                    2 => "legacy_html".to_owned(),
                    other => other.to_string(),
                },
                "content": row.content,
                "reply_to": row.reply_to,
                "sent_ms": row.sent_ms,
                "edited_ms": row.edited_ms,
                "edits": row.edits,
                "deleted_ms": row.deleted_ms,
                "delete_reason": row.delete_reason,
            }));
        }
        if !page.more_after {
            break;
        }
    }
    let (days, live_only) = wire(retention);
    Ok(json!({
        "conversation": hex(&id),
        "name": name,
        "exported_ms": now,
        "retention": { "days": days, "live_only": live_only },
        "head": opened.state.conversations.head(&id),
        "messages": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_arguments() {
        assert_eq!(
            parse_retention("forever").unwrap(),
            Some(Retention::Forever)
        );
        assert_eq!(parse_retention("none").unwrap(), Some(Retention::None));
        assert_eq!(parse_retention("default").unwrap(), None);
        assert_eq!(parse_retention("30").unwrap(), Some(Retention::Days(30)));
        assert!(parse_retention("0").is_err());
        assert!(parse_retention("36501").is_err());
        assert!(parse_retention("soon").is_err());
    }
}
