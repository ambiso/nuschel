// SPDX-License-Identifier: EUPL-1.2
use clap::{ArgGroup, Parser};
use nuschel_ipc::{Envelope, StorageRequest, StorageResponse, read_frame, write_frame};
use nuschel_storage::History;
use std::{
    io,
    path::PathBuf,
    sync::mpsc::{self, TryRecvError},
};

#[derive(Parser)]
#[command(about = "Nuschel SQLx storage worker; receives requests on inherited stdin/stdout")]
#[command(group(ArgGroup::new("database-config").required(true).args(["database", "database_url_file"])))]
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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
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
