// SPDX-License-Identifier: EUPL-1.2
//! `nuschl-witness`: see the library documentation and docs/witness.md.
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "The project auditor for Nuschl's key transparency")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check and cosign the epoch logs servers submit.
    Serve {
        /// The cosigner name clients configure (for example
        /// `witness.example.org/nuschl-kt`).
        #[arg(long)]
        name: String,
        /// Where the key (if generated) and one state file per log are kept.
        #[arg(long, default_value = "/var/lib/nuschl-witness")]
        state_directory: PathBuf,
        /// The ML-DSA-44 seed: 32 bytes or 64 hex digits. Generated once in the
        /// state directory (`witness.key`) without it.
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Plain HTTP; TLS comes from a reverse proxy.
        #[arg(long, default_value = "127.0.0.1:8473")]
        listen: SocketAddr,
        /// Enrol logs it has not seen before.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        accept_new_logs: bool,
        #[arg(long, default_value_t = 10_000)]
        max_logs: usize,
        /// Per log.
        #[arg(long, default_value_t = 60)]
        submissions_per_minute: u32,
        /// New logs, witness-wide.
        #[arg(long, default_value_t = 10)]
        enrolments_per_minute: u32,
        #[arg(long, default_value_t = 16 * 1024 * 1024)]
        max_request_bytes: usize,
        /// Enrol servers on loopback, private and link-local addresses.
        #[arg(long)]
        enrol_private_addresses: bool,
    },
    /// Print the verifier key clients and servers configure (for
    /// `DEFAULT_WITNESSES` or `key_transparency.witnesses`).
    Vkey {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "/var/lib/nuschl-witness")]
        state_directory: PathBuf,
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let result = match Cli::parse().command {
        Command::Serve {
            name,
            state_directory,
            key_file,
            listen,
            accept_new_logs,
            max_logs,
            submissions_per_minute,
            enrolments_per_minute,
            max_request_bytes,
            enrol_private_addresses,
        } => {
            let config = nuschl_witness::Config {
                key_file,
                listen,
                accept_new_logs,
                max_logs,
                submissions_per_minute,
                enrolments_per_minute,
                max_request_bytes,
                enrol_private_addresses,
                ..nuschl_witness::Config::new(name, state_directory)
            };
            match nuschl_witness::start(config).await {
                Ok(running) => {
                    tokio::select! {
                        () = running.wait() => Ok(()),
                        _ = tokio::signal::ctrl_c() => Ok(()),
                    }
                }
                Err(error) => Err(error),
            }
        }
        Command::Vkey {
            name,
            state_directory,
            key_file,
        } => {
            let config = nuschl_witness::Config {
                key_file,
                ..nuschl_witness::Config::new(name, state_directory)
            };
            nuschl_witness::cosigner(&config).map(|cosigner| println!("{}", cosigner.key().vkey()))
        }
    };
    if let Err(error) = result {
        eprintln!("nuschl-witness: {error}");
        std::process::exit(1);
    }
}
