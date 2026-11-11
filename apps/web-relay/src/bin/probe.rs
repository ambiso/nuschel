// SPDX-License-Identifier: EUPL-1.2
//! Log in to a Mumble server through the relay, with TLS on this side
//! (the loopback test and the vm-web check).
use clap::Parser;
use nuschl_web_relay::probe::{self, Probe};

#[derive(Parser)]
#[command(version)]
struct Arguments {
    /// The relay's WebSocket URL, e.g. ws://127.0.0.1:8090/relay.
    #[arg(long)]
    relay: String,
    /// The Origin header to send, as a browser would.
    #[arg(long)]
    origin: Option<String>,
    /// The Mumble server, host[:port].
    #[arg(long)]
    server: String,
    #[arg(long, default_value = "relay-probe")]
    username: String,
    /// The server certificate's SHA-256 (hex).
    #[arg(long)]
    pin: Option<String>,
}

fn main() {
    let arguments = Arguments::parse();
    let (host, port) = match arguments.server.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') || host.starts_with('[') => (
            host.trim_matches(|c| c == '[' || c == ']').to_owned(),
            port.parse().unwrap_or(64738),
        ),
        _ => (arguments.server.clone(), 64738),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    match runtime.block_on(probe::run(Probe {
        relay: arguments.relay,
        origin: arguments.origin,
        host,
        port,
        username: arguments.username,
        pin: arguments.pin,
    })) {
        Ok(outcome) => {
            println!(
                "connected session={} udp={} udp_answered={} sha256={} release={}",
                outcome.session,
                outcome.udp,
                outcome.udp_answered,
                outcome.fingerprint,
                outcome.release.unwrap_or_default()
            );
        }
        Err(error) => {
            println!("failed: {error}");
            std::process::exit(1);
        }
    }
}
