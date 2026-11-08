// SPDX-License-Identifier: EUPL-1.2
use clap::Parser;
use std::{net::IpAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Nuschel server foundation (network sessions are not implemented yet)"
)]
struct Arguments {
    #[arg(long, default_value = "0.0.0.0")]
    listen_address: IpAddr,
    #[arg(long, default_value_t = 64738)]
    port: u16,
    #[arg(long, conflicts_with = "database_url_file")]
    sqlite_path: Option<PathBuf>,
    /// Runtime credential path; the URL itself is never a command-line argument.
    #[arg(long)]
    database_url_file: Option<PathBuf>,
    #[arg(long, requires = "tls_key_file")]
    tls_certificate_file: Option<PathBuf>,
    #[arg(long, requires = "tls_certificate_file")]
    tls_key_file: Option<PathBuf>,
    /// Validate configuration without starting a listener.
    #[arg(long)]
    check_config: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    if arguments.port == 0 {
        return Err("port must be nonzero".into());
    }
    if arguments.check_config {
        println!("configuration accepted; Mumble network sessions are not implemented yet");
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init()
        .map_err(|error| error.to_string())?;
    Err("server session handling is not implemented; no listener was started".into())
}
