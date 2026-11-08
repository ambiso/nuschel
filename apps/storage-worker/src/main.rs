// SPDX-License-Identifier: EUPL-1.2
use clap::{ArgGroup, Parser};
use nuschel_ipc::{Envelope, StorageRequest, StorageResponse, read_frame, write_frame};
use nuschel_storage::History;
use std::{io, path::PathBuf};

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
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<StorageRequest>(&mut input)? {
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
        }
        .unwrap_or_else(|error| StorageResponse::Rejected(error.to_string()));
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}
