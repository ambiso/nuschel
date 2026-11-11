// SPDX-License-Identifier: EUPL-1.2
use clap::{ArgGroup, Parser};
use nuschel_ipc::{
    Envelope, StorageRequest, StorageResponse, read_frame,
    server::{ServerStorageRequest as Request, ServerStorageResponse as Response},
    write_frame,
};
use nuschel_storage::{
    History,
    server::{OperatorLock, ServerStore},
};
use std::{
    io,
    path::PathBuf,
    sync::mpsc::{self, TryRecvError},
};

#[derive(Parser)]
#[command(about = "Nuschel SQLx storage worker; receives requests on inherited stdin/stdout")]
#[command(group(ArgGroup::new("database-config").required(true).args(["database", "database_url_file", "read_murmur"])))]
struct Arguments {
    /// A local SQLite path selected by the trusted supervisor.
    #[arg(long)]
    database: Option<PathBuf>,
    /// A credential file containing a SQLite or PostgreSQL URL. Never put credentials in argv.
    #[arg(long)]
    database_url_file: Option<PathBuf>,
    /// Connect, apply checked migrations, and exit without entering the IPC loop.
    #[arg(long)]
    check_database: bool,
    /// Serve `nuscheld`'s server state (channels, ACLs, users, bans) instead of history.
    #[arg(long)]
    server: bool,
    /// Read this Murmur database (read-only, untrusted) for `nuscheld
    /// --import-murmur`, write it to stdout as frames and exit.
    #[arg(long, conflicts_with_all = ["database", "database_url_file", "server", "check_database"])]
    read_murmur: Option<PathBuf>,
    /// The Murmur virtual server to read.
    #[arg(long, default_value_t = 1, requires = "read_murmur")]
    murmur_server_id: i64,
    /// With `--server`: take the operator lock before opening the database; if
    /// another process holds it, answer every request with `Held` instead.
    #[arg(long, requires = "server")]
    operator_lock: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    if let Some(path) = &arguments.read_murmur {
        return read_murmur(path, arguments.murmur_server_id).await;
    }
    if arguments.server {
        return serve_server_state(arguments).await;
    }
    let history = match (arguments.database, arguments.database_url_file) {
        (Some(path), None) => History::open(&path).await?,
        (None, Some(path)) => History::connect(std::fs::read_to_string(path)?.trim()).await?,
        _ => return Err("select exactly one database configuration".into()),
    };
    if arguments.check_database {
        return Ok(());
    }
    // Requests are read on their own thread, so that older messages can be
    // made searchable whenever none is waiting.
    let (frames, requests) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let frame = read_frame::<StorageRequest>(&mut input);
            let last = !matches!(frame, Ok(Some(_)));
            if frames.send(frame).is_err() || last {
                break;
            }
        }
    });
    let mut output = io::stdout().lock();
    let mut backfill = true;
    loop {
        let frame = if backfill {
            match requests.try_recv() {
                Ok(frame) => frame,
                Err(TryRecvError::Empty) => {
                    // A database that cannot be filled is still served.
                    backfill = history.backfill().await.unwrap_or(false);
                    continue;
                }
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match requests.recv() {
                Ok(frame) => frame,
                Err(_) => break,
            }
        };
        let Some(request) = frame? else {
            break;
        };
        let payload = match request.payload {
            StorageRequest::Append { message } => history
                .append(&message)
                .await
                .map(StorageResponse::Appended),
            StorageRequest::Recent {
                server,
                channel,
                limit,
            } => history
                .recent(&server, &channel, usize::from(limit))
                .await
                .map(StorageResponse::Messages),
            StorageRequest::HistoryPage {
                server,
                channel,
                before,
                limit,
            } => history
                .page(&server, &channel, before, usize::from(limit))
                .await
                .map(StorageResponse::HistoryPage),
            StorageRequest::HistoryAround {
                server,
                channel,
                id,
                limit,
            } => history
                .page_around(&server, &channel, id, usize::from(limit))
                .await
                .map(StorageResponse::HistoryPage),
            StorageRequest::HistoryAfter {
                server,
                channel,
                after,
                limit,
            } => history
                .page_after(&server, &channel, after, usize::from(limit))
                .await
                .map(StorageResponse::HistoryPage),
            StorageRequest::Search {
                servers,
                channel,
                query,
                after,
                limit,
            } => history
                .search(
                    &servers,
                    channel.as_deref(),
                    &query,
                    after,
                    usize::from(limit),
                )
                .await
                .map(StorageResponse::SearchResults),
        }
        .unwrap_or_else(|error| StorageResponse::Rejected(error.to_string()));
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}

/// Stream a Murmur database as frames; a failure is reported as a
/// `Failed` frame and the exit status.
async fn read_murmur(
    path: &std::path::Path,
    server: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    use nuschel_ipc::murmur::MurmurFrame;
    let mut output = io::stdout().lock();
    let mut sequence = 0_u64;
    let mut emit = |frame: MurmurFrame| {
        sequence += 1;
        write_frame(&mut output, &Envelope::new(sequence, frame)).map_err(|error| error.to_string())
    };
    // Before SQLite starts its thread, so that the thread is confined too.
    // Where Landlock cannot be used (an old kernel, a seccomp filter), the
    // reader carries on with its other limits and the report says so.
    let confinement =
        confine(path).unwrap_or_else(|error| format!("reader: not confined ({error})"));
    let _ = emit(MurmurFrame::Notes(vec![confinement]));
    if let Err(error) = nuschel_storage::murmur::read(path, server, &mut emit).await {
        let _ = emit(MurmurFrame::Failed(error.clone()));
        return Err(error.into());
    }
    Ok(())
}

/// Another process holds the operator lock: the database stays closed.
fn serve_held() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<Request>(&mut input)? {
        write_frame(
            &mut output,
            &Envelope::new(request.request_id, Response::Held),
        )?;
    }
    Ok(())
}

/// Landlock: from here on this process (and the threads it starts) may only
/// read the Murmur file: no other file, no TCP, no abstract Unix sockets, no
/// signals to other processes. Best effort on kernels with an older Landlock;
/// the result is reported.
#[cfg(target_os = "linux")]
fn confine(path: &std::path::Path) -> Result<String, String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        Scope, path_beneath_rules,
    };
    let abi = ABI::V6;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|ruleset| ruleset.handle_access(AccessNet::from_all(abi)))
        .and_then(|ruleset| ruleset.scope(Scope::from_all(abi)))
        .and_then(|ruleset| ruleset.create())
        .and_then(|ruleset| ruleset.add_rules(path_beneath_rules([path], AccessFs::from_read(abi))))
        .and_then(|ruleset| ruleset.restrict_self())
        .map_err(|error| error.to_string())?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => "reader: confined by Landlock to reading the file".into(),
        RulesetStatus::PartiallyEnforced => {
            "reader: partly confined by Landlock (the kernel lacks newer Landlock features)".into()
        }
        RulesetStatus::NotEnforced => "reader: not confined (this kernel has no Landlock)".into(),
    })
}

#[cfg(not(target_os = "linux"))]
fn confine(_: &std::path::Path) -> Result<String, String> {
    Ok("reader: not confined (Landlock is Linux only)".into())
}

async fn serve_server_state(arguments: Arguments) -> Result<(), Box<dyn std::error::Error>> {
    let url = match &arguments.database_url_file {
        Some(path) => Some(std::fs::read_to_string(path)?.trim().to_owned()),
        None => None,
    };
    // Held for the life of this process.
    let lock = if arguments.operator_lock {
        let lock = match (&arguments.database, &url) {
            (Some(path), None) => OperatorLock::sqlite(path)?,
            (None, Some(url)) => OperatorLock::url(url).await?,
            _ => return Err("select exactly one database configuration".into()),
        };
        match lock {
            Some(lock) => Some(lock),
            None if arguments.check_database => {
                return Err("another process (a running nuscheld?) holds the database".into());
            }
            None => return serve_held(),
        }
    } else {
        None
    };
    let store = match (arguments.database, url) {
        (Some(path), None) => ServerStore::open(&path).await?,
        (None, Some(url)) => ServerStore::connect(&url).await?,
        _ => return Err("select exactly one database configuration".into()),
    };
    if arguments.check_database {
        return Ok(());
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<Request>(&mut input)? {
        let done = |result: Result<(), nuschel_storage::Error>| result.map(|()| Response::Done);
        let payload = match request.payload {
            Request::Load => store.load().await.map(Response::Snapshot),
            Request::LoadUsers { after } => store.load_users(after).await.map(Response::Users),
            Request::SaveChannel { channel } => done(store.save_channel(&channel).await),
            Request::DeleteChannel { id } => done(store.delete_channel(id).await),
            Request::Link { a, b } => done(store.link(a, b).await),
            Request::Unlink { a, b } => done(store.unlink(a, b).await),
            Request::SaveUser { user } => done(store.save_user(&user).await),
            Request::DeleteUser { id } => done(store.delete_user(id).await),
            Request::SetComment { id, comment } => done(store.set_comment(id, &comment).await),
            Request::SetTexture { id, texture } => done(store.set_texture(id, &texture).await),
            Request::UserBlobs { id } => store
                .user_blobs(id)
                .await
                .map(|(comment, texture)| Response::Blobs { comment, texture }),
            Request::SaveBans { bans } => done(store.save_bans(&bans).await),
            Request::SetNextUserId { id } => done(store.set_next_user_id(id).await),
            Request::SetListener {
                user_id,
                channel,
                volume,
            } => done(store.set_listener(user_id, channel, volume).await),
            Request::RemoveListener { user_id, channel } => {
                done(store.remove_listener(user_id, channel).await)
            }
            Request::UserListeners { id } => {
                store.user_listeners(id).await.map(Response::Listeners)
            }
            Request::HistoryAppend { message } => store.history_append(&message).await,
            Request::HistoryEdit {
                target,
                author_account,
                format,
                content,
                edit_limit,
                event,
            } => {
                store
                    .history_edit(
                        target,
                        &author_account,
                        format,
                        &content,
                        edit_limit,
                        &event,
                    )
                    .await
            }
            Request::HistoryDelete {
                target,
                author_account,
                reason,
                event,
            } => {
                store
                    .history_delete(target, author_account.as_deref(), reason, &event)
                    .await
            }
            Request::HistoryPurge { conversation } => {
                done(store.history_purge(&conversation).await)
            }
            Request::HistoryCreateDirect { direct } => {
                done(store.history_create_direct(&direct).await)
            }
            Request::HistorySetAccess {
                conversation,
                account,
                intervals,
            } => done(
                store
                    .history_set_access(&conversation, &account, &intervals)
                    .await,
            ),
            Request::HistoryForgetAccount { account } => {
                done(store.history_forget_account(&account).await)
            }
            Request::HistorySetReadMarker {
                account,
                conversation,
                seq,
            } => done(
                store
                    .history_set_read_marker(&account, &conversation, seq)
                    .await,
            ),
            Request::HistoryErase {
                account,
                deleted_ms,
            } => store
                .history_erase(&account, deleted_ms)
                .await
                .map(Response::Count),
            Request::HistoryExpire {
                conversation,
                before_ms,
                deleted_ms,
                limit,
            } => store
                .history_expire(&conversation, before_ms, deleted_ms, limit)
                .await
                .map(Response::Count),
            Request::HistoryCountExpirable {
                conversation,
                before_ms,
            } => store
                .history_count_expirable(&conversation, before_ms)
                .await
                .map(Response::Count),
            Request::HistorySetRetention {
                conversation,
                retention,
            } => done(store.history_set_retention(&conversation, retention).await),
            Request::HistoryRetentions { after } => store
                .history_retentions(after.as_deref())
                .await
                .map(Response::Retentions),
            Request::HistoryAdvance { conversation, seq } => {
                done(store.history_advance(&conversation, seq).await)
            }
            Request::HistoryHeads { after } => store
                .history_heads(after.as_deref())
                .await
                .map(Response::Heads),
            Request::HistoryDirects { after } => store
                .history_directs(after.as_deref())
                .await
                .map(Response::Directs),
            Request::HistoryAccess { after } => store
                .history_access(after.as_ref())
                .await
                .map(Response::Access),
            Request::HistoryReadMarkers { account } => store
                .history_read_markers(&account)
                .await
                .map(Response::ReadMarkers),
            Request::HistoryPage { query } => store.history_page(&query).await.map(Response::Page),
            Request::OperatorLock => Ok(match lock {
                Some(_) => Response::Done,
                None => Response::Rejected("started without --operator-lock".into()),
            }),
            Request::KtGet { keys } => store.kt_get(&keys).await.map(Response::KtValues),
            Request::KtScan { prefix } => store.kt_scan(&prefix).await.map(Response::KtRecords),
            Request::KtWrite { records } => done(store.kt_write(&records).await),
        }
        .unwrap_or_else(|error| Response::Rejected(error.to_string()));
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}
