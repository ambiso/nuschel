// SPDX-License-Identifier: EUPL-1.2
//! A Mumble music bot: plays songs from Subsonic servers in one or more
//! channels; people control the queues on a web page they get a link to.
mod config;
mod covers;
mod decode;
mod mumble;
mod persist;
mod player;
mod scrobble;
mod subsonic;
mod tokens;
mod web;

use clap::Parser;
use config::Config;
use mumble::Connection;
use player::{Changes, Player};
use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex, atomic::AtomicU64},
};
use subsonic::Library;
use tokens::Tokens;

#[derive(Parser)]
#[command(version, about = "Mumble music bot for Subsonic servers")]
struct Arguments {
    /// TOML configuration (see nuschl-musicbot.example.toml).
    #[arg(long)]
    config: PathBuf,
    /// Validate the configuration and exit without credentials, state or network access.
    #[arg(long)]
    check_config: bool,
    /// Client certificates and links; systemd's `StateDirectory` by default.
    #[arg(
        long,
        env = "STATE_DIRECTORY",
        required_unless_present = "check_config"
    )]
    state_directory: Option<PathBuf>,
    /// The built web page (apps/musicbot/web/dist); the Nix package sets it.
    #[arg(long, env = "NUSCHL_MUSICBOT_WEB")]
    web_assets: Option<PathBuf>,
}

/// Everything the connections, players and web page share.
pub struct Bot {
    pub config: Config,
    pub state_directory: PathBuf,
    pub web_assets: Option<PathBuf>,
    /// The fixed codec worker beside this executable (`nuschl_ipc::musicbot::worker_path`).
    pub codec_worker: PathBuf,
    pub libraries: Arc<Vec<Library>>,
    /// One per configured channel, in the same order; a channel can play another's.
    pub players: Vec<Arc<Player>>,
    pub connections: Vec<Connection>,
    pub tokens: Tokens,
    /// What the web page's viewers watch to learn of any change.
    pub changes: Changes,
    /// Ephemeral invalidation for live, backend-owned song ratings.
    pub rating_revision: AtomicU64,
    /// Serializes checkpoints with the final shutdown save.
    pub persistence: Mutex<()>,
    /// Decoded covers in `covers/` below the state directory.
    pub covers: covers::CoverCache,
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return ExitCode::FAILURE,
    };
    runtime.block_on(application())
}

async fn application() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuschl_musicbot=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let code = match run(Arguments::parse()).await {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!("{error}");
            1
        }
    };
    // The owned playback reporter has stopped; do not wait for unrelated
    // library requests still running on other threads.
    std::process::exit(code)
}

async fn run(arguments: Arguments) -> Result<(), String> {
    let text = std::fs::read_to_string(&arguments.config)
        .map_err(|error| format!("{}: {error}", arguments.config.display()))?;
    // Relative secret files are systemd credentials (LoadCredential), or sit beside the configuration.
    let files = std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .or_else(|| arguments.config.parent().map(PathBuf::from))
        .unwrap_or_default();
    let config = Config::parse(&text, &files)
        .map_err(|error| format!("{}: {error}", arguments.config.display()))?;
    if arguments.check_config {
        println!("configuration accepted");
        return Ok(());
    }
    let state_directory = arguments
        .state_directory
        .ok_or("--state-directory is required")?;
    // Codecs run only in this separate, confined executable; nothing selects another.
    let codec_worker = std::env::current_exe()
        .ok()
        .and_then(|executable| nuschl_ipc::musicbot::worker_path(&executable))
        .ok_or("cannot locate the music codec worker")?;
    prepare_state_directory(&state_directory)
        .map_err(|error| format!("{}: {error}", state_directory.display()))?;
    let libraries = config
        .libraries
        .iter()
        .enumerate()
        .map(|(index, library)| Ok(Library::new(index, library, library.credentials()?)))
        .collect::<Result<Vec<_>, String>>()?;
    let tokens = Tokens::load(state_directory.join("tokens.json"))
        .map_err(|error| format!("tokens.json: {error}"))?;
    let changes = Changes::default();
    let players = config
        .channels
        .iter()
        .map(|channel| {
            Arc::new(Player::new(
                channel.name.clone(),
                config.audio.volume,
                config.audio.ducking(),
                changes.clone(),
            ))
        })
        .collect();
    let connections = config
        .channels
        .iter()
        .enumerate()
        .map(|(index, channel)| {
            Connection::new(
                channel.name.clone(),
                format!("{}:{}", channel.host, channel.port),
                index,
            )
        })
        .collect();
    let covers = covers::CoverCache::open(
        state_directory.join("covers"),
        config.web.cover_cache_mib.saturating_mul(1024 * 1024),
    )
    .map_err(|error| format!("{}: {error}", state_directory.join("covers").display()))?;
    let bot = Arc::new(Bot {
        state_directory,
        web_assets: arguments.web_assets,
        codec_worker,
        libraries: Arc::new(libraries),
        players,
        connections,
        tokens,
        changes,
        rating_revision: AtomicU64::new(0),
        persistence: Mutex::new(()),
        covers,
        config,
    });
    // The queues of the last run come back before anything plays.
    persist::restore(&bot).map_err(|error| format!("cannot restore queues.json: {error}"))?;
    let listener = tokio::net::TcpListener::bind(bot.config.web.listen)
        .await
        .map_err(|error| format!("web: {}: {error}", bot.config.web.listen))?;
    tracing::info!("web page on {}", bot.config.web.listen);
    // Capture restored-history cursors synchronously before audio tasks start.
    let reporter = scrobble::start(&bot);
    tokio::spawn(persist::keep(bot.clone()));
    for index in 0..bot.libraries.len() {
        let libraries = bot.libraries.clone();
        tokio::task::spawn_blocking(move || {
            let library = &libraries[index];
            match library.ping() {
                Ok(()) => tracing::info!("{}: library reachable", library.name),
                Err(error) => tracing::warn!("{}: {error}", library.name),
            }
        });
    }
    for player in &bot.players {
        tokio::spawn(player.clone().run(
            bot.libraries.clone(),
            bot.config.audio.bitrate,
            bot.codec_worker.clone(),
        ));
    }
    for index in 0..bot.connections.len() {
        tokio::spawn(mumble::run(bot.clone(), index));
    }
    // Event streams stay open, so the web server is not drained on shutdown.
    // The owned reporter joins its actual call and drains a bounded snapshot.
    let result = tokio::select! {
        result = axum::serve(listener, web::router(bot.clone())) => {
            result.map_err(|error| format!("web: {error}"))
        }
        signal = shutdown() => {
            tracing::info!("{signal}: saving the queues and stopping");
            persist::save(&bot).map_err(|error| format!("cannot save the queues: {error}"))
        }
    };
    reporter.shutdown().await;
    result
}

/// Ctrl-C, or SIGTERM from systemd.
async fn shutdown() -> &'static str {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(error) => {
                    tracing::warn!("no SIGTERM handler: {error}");
                    let _ = tokio::signal::ctrl_c().await;
                    return "interrupted";
                }
            };
        tokio::select! {
            _ = terminate.recv() => "terminated",
            _ = tokio::signal::ctrl_c() => "interrupted",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "interrupted"
    }
}

fn prepare_state_directory(path: &std::path::Path) -> std::io::Result<()> {
    nuschl_session::layout::create_private_dir(path)?;
    for name in [
        "tokens.json",
        "tokens.json.new",
        "queues.json",
        "queues.json.new",
    ] {
        match persist::open_private(&path.join(name)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod state_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn startup_hardens_only_the_configured_root_and_its_state_files() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = parent.path().join("standalone");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in [
            "tokens.json",
            "tokens.json.new",
            "queues.json",
            "queues.json.new",
        ] {
            let path = root.join(name);
            std::fs::write(&path, b"preserved secret").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        prepare_state_directory(&root).unwrap();
        assert_eq!(
            std::fs::metadata(parent.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in [
            "tokens.json",
            "tokens.json.new",
            "queues.json",
            "queues.json.new",
        ] {
            let path = root.join(name);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(std::fs::read(path).unwrap(), b"preserved secret");
        }
        let new_root = parent.path().join("new");
        prepare_state_directory(&new_root).unwrap();
        assert_eq!(
            std::fs::metadata(new_root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
