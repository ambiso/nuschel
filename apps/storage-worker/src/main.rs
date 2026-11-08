// SPDX-License-Identifier: EUPL-1.2
use std::{io, path::PathBuf};

use clap::Parser;
use nuschel_ipc::{Envelope, StorageRequest, StorageResponse, read_frame, write_frame};
use nuschel_storage::History;

#[derive(Parser)]
#[command(about = "Nuschel history worker; receives requests on inherited stdin/stdout")]
struct Arguments {
    /// Set by the trusted supervisor, never by a chat message.
    #[arg(long)]
    database: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let history = History::open(&Arguments::parse().database)?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<StorageRequest>(&mut input)? {
        let payload = match request.payload {
            StorageRequest::Append { message } => {
                history.append(&message).map(StorageResponse::Appended)
            }
            StorageRequest::Recent {
                server,
                channel,
                limit,
            } => history
                .recent(&server, &channel, usize::from(limit))
                .map(StorageResponse::Messages),
        }
        .unwrap_or_else(|error| StorageResponse::Rejected(error.to_string()));
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}
