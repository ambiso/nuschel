// SPDX-License-Identifier: EUPL-1.2
//! Nuschl in a terminal: the client SDK's second view after the desktop.
//! The same profile and saved servers as the desktop (or `--profile-dir`),
//! the same sandboxed workers beside the executable, and voice through the
//! same media worker. A full screen by default; plain lines for screen
//! readers and pipes (`--plain`). Over SSH voice is off unless asked for.
mod app;
mod document;
mod input;
mod keys;
mod plain;
mod terminal;
#[cfg(test)]
mod tests;
mod view;

use clap::Parser;
use nuschl_client::{Client, native::Driver};
use nuschl_session::config::Profile;
use std::{io::IsTerminal, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(version, about = "Nuschl in a terminal")]
struct Arguments {
    /// A separate profile (settings, servers, history, identity) in this folder.
    #[arg(long)]
    profile_dir: Option<PathBuf>,
    /// Connect at start: `mumble://[name[:password]@]host[:port][/channel/path]`.
    #[arg(value_name = "URL")]
    url: Option<String>,
    /// Print plain lines instead of drawing a full screen (screen readers,
    /// logs, pipes). Also when the output is not a terminal or TERM=dumb.
    #[arg(long)]
    plain: bool,
    /// Never open audio devices: every connection stays muted and deafened.
    /// The default over SSH.
    #[arg(long, conflicts_with = "audio")]
    no_audio: bool,
    /// Use audio devices even over SSH.
    #[arg(long)]
    audio: bool,
    /// The program that opens web links (default: xdg-open, open or
    /// explorer; none over SSH, where links are only shown).
    #[arg(long, value_name = "PROGRAM")]
    opener: Option<String>,
}

fn default_opener(ssh: bool) -> Option<String> {
    if ssh {
        None
    } else if cfg!(target_os = "macos") {
        Some("open".into())
    } else if cfg!(windows) {
        Some("explorer".into())
    } else {
        Some("xdg-open".into())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let plain = arguments.plain
        || !std::io::stdout().is_terminal()
        || std::env::var("TERM").is_ok_and(|term| term == "dumb");
    let audio = arguments.audio || (!arguments.no_audio && !ssh);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let profile = Profile::open(arguments.profile_dir)?;
    let loaded = profile.load();
    let (driver, events) = Driver::new(
        runtime.handle().clone(),
        profile.clone(),
        loaded.settings.clone(),
    );
    let driver = Arc::new(driver);
    let client = Client::new(
        Box::new(driver.clone()),
        loaded.servers,
        nuschl_client::native::preferences(&loaded.settings),
    );
    let options = app::Options {
        audio,
        opener: arguments.opener.or_else(|| default_opener(ssh)),
        plain,
    };
    let mut app = app::App::new(client, Some(profile), options);
    for problem in loaded.problems {
        app.warn(problem);
    }
    if ssh && !audio {
        app.say("Over SSH voice is off: you neither speak nor listen (--audio turns it on).");
    }
    let result = runtime.block_on(async {
        app.start(arguments.url.as_deref());
        if plain {
            plain::run(app, events).await
        } else {
            terminal::run(app, events).await
        }
    });
    // Connections end with the driver; workers stop with their pipes.
    drop(driver);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(result?)
}
