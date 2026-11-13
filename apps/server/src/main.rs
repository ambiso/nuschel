// SPDX-License-Identifier: EUPL-1.2
use clap::Parser;
use nuschld::{
    config::{Config, Settings},
    server, storage,
};
use std::{io::Read, net::IpAddr, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(version, about = "Nuschl's Mumble-compatible server")]
struct Arguments {
    /// TOML configuration file (see nuschld.example.toml).
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
    /// File with the server password (replaces `password_file`).
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// File with the SuperUser password, applied at startup.
    #[arg(long)]
    superuser_password_file: Option<PathBuf>,
    /// Key-transparency head-signing key (replaces `kt.signing_key_file`).
    #[arg(long)]
    kt_signing_key_file: Option<PathBuf>,
    /// Key-transparency VRF secret (replaces `kt.vrf_secret_file`).
    #[arg(long)]
    kt_vrf_secret_file: Option<PathBuf>,
    /// Keep all state in memory (nothing is written).
    #[arg(long)]
    ephemeral: bool,
    /// Validate the configuration and exit.
    #[arg(long)]
    check_config: bool,
    /// Read a new SuperUser password from standard input, store it and exit.
    #[arg(long)]
    set_superuser_password: bool,
    /// Erase every stored message by this registered user (name, or `#id`)
    /// and exit: they become tombstones without content (GDPR requests).
    #[arg(long, value_name = "USER")]
    erase_user: Option<String>,
    /// The same as `admin import-murmur PATH` (kept as a flag for scripts).
    #[arg(long, value_name = "PATH")]
    import_murmur: Option<PathBuf>,
    /// The Murmur virtual server to import.
    #[arg(long, default_value_t = 1, requires = "import_murmur")]
    murmur_server_id: i64,
    /// Delete this server's channels, registered users and bans before the import.
    #[arg(long, requires = "import_murmur")]
    replace_existing: bool,
    /// Read the Murmur database even where Landlock cannot fully confine the reader.
    #[arg(long, requires = "import_murmur")]
    allow_unconfined_murmur_read: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Operator commands on the database, with the server stopped
    /// (docs/server.md, "Administration").
    #[command(subcommand)]
    Admin(nuschld::admin::Admin),
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
    if let Some(path) = &arguments.password_file {
        config.password_file = Some(path.clone());
    }
    if let Some(path) = &arguments.superuser_password_file {
        config.superuser_password_file = Some(path.clone());
    }
    if let Some(path) = &arguments.kt_signing_key_file {
        config.kt.signing_key_file = Some(path.clone());
    }
    if let Some(path) = &arguments.kt_vrf_secret_file {
        config.kt.vrf_secret_file = Some(path.clone());
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
    if let Some(Command::Admin(command)) = arguments.command {
        return nuschld::admin::run(settings, backend, command).await;
    }
    if arguments.set_superuser_password {
        let mut password = String::new();
        std::io::stdin().take(4096).read_to_string(&mut password)?;
        let password = password.trim_end_matches(['\r', '\n']).to_owned();
        let mut settings = settings;
        settings.config.bind.clear();
        server::prepare_files(&settings, &backend)?;
        let storage = storage::start_locked(backend);
        server::hold_operator_lock(&storage).await?;
        let counters = Default::default();
        let mut state = server::load_state_for_admin(&settings, &storage, &counters).await?;
        server::set_superuser_password(&mut state, password, &settings).await?;
        storage.flush().await;
        println!("SuperUser password set");
        return Ok(());
    }
    if let Some(path) = &arguments.import_murmur {
        if arguments.ephemeral {
            return Err("--import-murmur needs persistent state, not --ephemeral".into());
        }
        let options = nuschld::murmur::Options {
            path: path.clone(),
            server_id: arguments.murmur_server_id,
            replace_existing: arguments.replace_existing,
            allow_unconfined_read: arguments.allow_unconfined_murmur_read,
        };
        let report = nuschld::murmur::import(&settings, backend, &options).await?;
        print!("{}", report.render());
        return Ok(());
    }
    if let Some(user) = &arguments.erase_user {
        server::prepare_files(&settings, &backend)?;
        let storage = storage::start_locked(backend);
        server::hold_operator_lock(&storage).await?;
        let counters = Default::default();
        let state = server::load_state_for_admin(&settings, &storage, &counters).await?;
        let (account, name) = server::find_account(&state, user)?;
        let erased = server::erase_account(&storage, account).await?;
        storage.flush().await;
        println!("erased {erased} messages by {name}");
        return Ok(());
    }
    let running = server::start(settings, backend).await?;
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            _ = hangup.recv() => match running.server.reload_certificate() {
                Ok(fingerprint) => {
                    tracing::info!(sha256 = %fingerprint, "reloaded the server certificate");
                }
                Err(error) => tracing::error!(
                    %error,
                    "could not reload the server certificate; keeping the current one"
                ),
            },
        }
    }
    tracing::info!("shutting down");
    running.shutdown().await;
    Ok(())
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NUSCHLD_LOG")
                // akd reports every proof and preload at info.
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,akd=warn")),
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
            eprintln!("nuschld: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(arguments)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nuschld: {error}");
            ExitCode::FAILURE
        }
    }
}
