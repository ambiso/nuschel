// SPDX-License-Identifier: EUPL-1.2
use clap::Parser;
use nuscheld::{
    config::{Config, Settings},
    server, storage,
};
use std::{io::Read, net::IpAddr, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(version, about = "Nuschel's Mumble-compatible server")]
struct Arguments {
    /// TOML configuration file (see nuscheld.example.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Replaces `bind` from the configuration.
    #[arg(long)]
    listen_address: Vec<IpAddr>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    state_directory: Option<PathBuf>,
    /// SQLite database (replaces `database`).
    #[arg(long, conflicts_with = "database_url_file")]
    sqlite_path: Option<PathBuf>,
    /// Runtime credential path; the URL itself is never a command-line argument.
    #[arg(long)]
    database_url_file: Option<PathBuf>,
    #[arg(long, requires = "tls_key_file")]
    tls_certificate_file: Option<PathBuf>,
    #[arg(long, requires = "tls_certificate_file")]
    tls_key_file: Option<PathBuf>,
    /// Keep all state in memory (nothing is written).
    #[arg(long)]
    ephemeral: bool,
    /// Validate the configuration and exit.
    #[arg(long)]
    check_config: bool,
    /// Read a new SuperUser password from standard input, store it and exit.
    #[arg(long)]
    set_superuser_password: bool,
}

fn settings(arguments: &Arguments) -> Result<Settings, Box<dyn std::error::Error>> {
    let mut config = match &arguments.config {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    if !arguments.listen_address.is_empty() {
        config.bind = arguments.listen_address.clone();
    }
    if let Some(port) = arguments.port {
        config.port = port;
    }
    if let Some(directory) = &arguments.state_directory {
        config.state_directory = directory.clone();
    }
    if let Some(path) = &arguments.sqlite_path {
        config.database = path.clone();
        config.database_url_file = None;
    }
    if let Some(path) = &arguments.database_url_file {
        config.database_url_file = Some(path.clone());
    }
    if let (Some(certificate), Some(key)) =
        (&arguments.tls_certificate_file, &arguments.tls_key_file)
    {
        config.certificate = Some(certificate.clone());
        config.private_key = Some(key.clone());
    }
    Ok(config.settings()?)
}

async fn run(arguments: Arguments) -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings(&arguments)?;
    if arguments.check_config {
        println!("configuration accepted");
        return Ok(());
    }
    let backend = if arguments.ephemeral {
        storage::Backend::Memory(storage::MemoryStore::shared())
    } else {
        server::default_backend(&settings)?
    };
    if arguments.set_superuser_password {
        let mut password = String::new();
        std::io::stdin().take(4096).read_to_string(&mut password)?;
        let password = password.trim_end_matches(['\r', '\n']).to_owned();
        let mut settings = settings;
        settings.config.bind.clear();
        let storage = storage::start(backend);
        let counters = Default::default();
        let mut state = server::load_state_for_admin(&settings, &storage, &counters).await?;
        server::set_superuser_password(&mut state, password, &settings).await?;
        storage.flush().await;
        println!("SuperUser password set");
        return Ok(());
    }
    let running = server::start(settings, backend).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    tracing::info!("shutting down");
    running.shutdown().await;
    Ok(())
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NUSCHELD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("nuscheld: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(arguments)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nuscheld: {error}");
            ExitCode::FAILURE
        }
    }
}
