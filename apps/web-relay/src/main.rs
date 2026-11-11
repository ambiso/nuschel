// SPDX-License-Identifier: EUPL-1.2
use clap::Parser;
use nuschl_web_relay::{Config, Relay};
use std::path::PathBuf;

/// The web client's tunnel relay (docs/web.md).
#[derive(Parser)]
#[command(version)]
struct Arguments {
    /// Settings (nuschl-web-relay.toml); defaults without one.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Where to listen, overriding the settings: `127.0.0.1:8090` or
    /// `unix:/run/nuschl-web-relay/relay.sock`.
    #[arg(long)]
    listen: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuschl_web_relay=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut config = match &arguments.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    if let Some(listen) = &arguments.listen {
        config.listen = listen.parse()?;
    }
    config.check()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let relay = Relay::new(config);
        tokio::select! {
            result = relay.run() => result.map_err(Into::into),
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}
