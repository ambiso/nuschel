// SPDX-License-Identifier: EUPL-1.2
//! Startup and the shared server object: load the persisted state, bind TCP and
//! UDP on every configured address, and run the accept and voice loops.
use crate::{
    config::Settings,
    connection, conversations, extension, history, kt,
    limits::LeakyBucket,
    mls, password,
    state::{Ban, Channel, Counters, Registered, State},
    storage::{self, Backend, StorageHandle},
    tls, udp,
};
use bytes::Bytes;
use nuschl_protocol::control as proto;
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

/// The protocol version this server speaks (Mumble 1.5.0).
pub const PROTOCOL_VERSION: u64 = (1 << 48) | (5 << 32);

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("storage unavailable: {0}")]
    Storage(String),
    #[error(
        "another process (a running nuschld, or `nuschld admin`) is using this database; \
         stop it first"
    )]
    Held,
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    #[error("cannot bind {address}: {source}")]
    Bind {
        address: SocketAddr,
        source: io::Error,
    },
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Connection-admission state: per-prefix counts, autoban history.
#[derive(Default)]
pub struct Admission {
    per_prefix: HashMap<IpAddr, u32>,
    attempts: HashMap<IpAddr, VecDeque<Instant>>,
    banned_until: HashMap<IpAddr, Instant>,
}

/// IPv4 addresses count individually, IPv6 by /64 (what one host usually has).
pub fn prefix(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !((1_u128 << 64) - 1)).into()),
        },
    }
}

pub struct Server {
    state: Mutex<State>,
    pub settings: Arc<Settings>,
    pub storage: StorageHandle,
    /// The history reader's worker (pages, and read markers at login).
    pub history_reads: StorageHandle,
    pub counters: Arc<Counters>,
    pub udp: Vec<Arc<UdpSocket>>,
    pub acceptor: TlsAcceptor,
    /// The certificate new handshakes get; replaced on SIGHUP.
    pub certificates: Arc<tls::Certificates>,
    pub admission: Mutex<Admission>,
    /// Connections that have not authenticated yet (TLS included).
    pub pending: Arc<Semaphore>,
    /// The server's `Version` message, sent first on every connection; the
    /// second carries the Nuschl hello, for `nuschl/1` connections.
    pub version_frame: Bytes,
    pub nuschl_version_frame: Bytes,
    /// What `nuschl/1` connections are offered (`kt/1` only with a log).
    pub capabilities: extension::Capabilities,
    pub shutdown: Notify,
    /// Failed password logins per registered account (ten, then one a second).
    pub login_failures: Mutex<HashMap<u32, LeakyBucket>>,
}

impl Server {
    /// Load the certificate and key again (SIGHUP): new handshakes use them,
    /// established connections keep their session. If they do not load, the
    /// current certificate stays. A generated certificate is re-read, never
    /// generated again. Returns the new SHA-256 fingerprint.
    pub fn reload_certificate(&self) -> Result<String, tls::TlsError> {
        let config = &self.settings.config;
        let identity = tls::Identity::reload(
            config
                .certificate
                .as_deref()
                .zip(config.private_key.as_deref()),
            &config.state_directory,
        )?;
        self.certificates.replace(identity)
    }

    /// Take the state lock. A panic while it was held must not take the whole
    /// server down, so poisoning is ignored (the handlers keep it consistent).
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Admission for a new TCP connection from `address`: bans, autoban and
    /// the per-prefix cap. On success the caller must call [`Server::release`].
    pub fn admit(&self, address: IpAddr) -> bool {
        let config = &self.settings.config;
        let key = prefix(address);
        let now = Instant::now();
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if admission
            .banned_until
            .get(&key)
            .is_some_and(|until| *until > now)
        {
            return false;
        }
        admission.banned_until.retain(|_, until| *until > now);
        if config.autoban.attempts > 0 {
            let window = Duration::from_secs(config.autoban.timeframe);
            let attempts = admission.attempts.entry(key).or_default();
            while attempts
                .front()
                .is_some_and(|time| now.duration_since(*time) > window)
            {
                attempts.pop_front();
            }
            attempts.push_back(now);
            if attempts.len() > config.autoban.attempts as usize {
                attempts.clear();
                admission
                    .banned_until
                    .insert(key, now + Duration::from_secs(config.autoban.duration));
                tracing::info!(
                    address = %self.settings.addresses.ip(address),
                    "autoban: too many connection attempts"
                );
                return false;
            }
            if admission.attempts.len() > 100_000 {
                admission
                    .attempts
                    .retain(|_, attempts| !attempts.is_empty());
            }
        }
        let count = admission.per_prefix.entry(key).or_insert(0);
        if *count >= config.connections_per_address {
            return false;
        }
        *count += 1;
        drop(admission);
        let banned = {
            let mut state = self.lock();
            let now = crate::state::now_unix();
            let before = state.bans.len();
            state.bans.retain(|ban| !ban.expired(now));
            if state.bans.len() != before {
                state.persist_bans();
            }
            state.bans.iter().any(|ban| ban.matches_address(address))
        };
        if banned {
            self.release(address);
            return false;
        }
        true
    }

    pub fn release(&self, address: IpAddr) {
        let key = prefix(address);
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = admission.per_prefix.get_mut(&key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                admission.per_prefix.remove(&key);
            }
        }
    }

    /// Murmur's `autobanSuccessfulConnections = false`: a login that worked
    /// does not count towards the autoban.
    pub fn forgive(&self, address: IpAddr) {
        if !self.settings.config.autoban.count_successful {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(attempts) = admission.attempts.get_mut(&prefix(address)) {
                attempts.pop_back();
            }
        }
    }

    /// Whether password guesses against `account` are paused; `failed` records
    /// one more failure.
    pub fn login_paused(&self, account: u32, failed: bool) -> bool {
        let now = Instant::now();
        let mut failures = self
            .login_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failures.len() > 100_000 {
            failures.retain(|_, bucket| bucket.exhausted(now));
        }
        let bucket = failures
            .entry(account)
            .or_insert_with(|| LeakyBucket::new(1, 10, now));
        if failed {
            bucket.limited(now);
        }
        bucket.exhausted(now)
    }

    pub fn banned_certificate(&self, hash: &str) -> bool {
        self.lock()
            .bans
            .iter()
            .any(|ban| !ban.hash.is_empty() && ban.hash == hash)
    }
}

/// A started server.
pub struct Running {
    pub server: Arc<Server>,
    /// The bound TCP addresses (UDP uses the same ones).
    pub addresses: Vec<SocketAddr>,
    /// SHA-256 of the server certificate, lowercase hex (what clients pin).
    pub fingerprint: String,
    tasks: Vec<JoinHandle<()>>,
}

impl Running {
    /// Disconnect everyone, persist what is pending and stop.
    pub async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        {
            let mut state = self.server.lock();
            let sessions: Vec<u32> = state.users.keys().copied().collect();
            for session in sessions {
                state.disconnect(session, false);
            }
        }
        self.server.shutdown.notify_waiters();
        self.server.storage.flush().await;
    }
}

fn server_version(settings: &Settings) -> proto::Version {
    let mut message = proto::Version {
        version_v1: Some((1 << 16) | (5 << 8)),
        version_v2: Some(PROTOCOL_VERSION),
        ..Default::default()
    };
    if settings.config.send_version {
        message.release = Some(format!("Nuschl {}", env!("CARGO_PKG_VERSION")));
        message.os = Some(std::env::consts::OS.to_owned());
    }
    message
}

/// Rebuild the channel tree from storage: children lists from parents, and
/// channels with a missing parent (or in a cycle) moved under the root.
fn build_channels(
    state: &mut State,
    stored: Vec<nuschl_ipc::server::StoredChannel>,
    links: Vec<(u32, u32)>,
) {
    for stored in stored {
        let mut channel = Channel::from_stored(stored);
        if channel.id == 0 {
            channel.node.parent = None;
        }
        state
            .conversations
            .add_channel(channel.conversation_id, channel.id);
        state.channels.0.insert(channel.id, channel);
    }
    let ids: Vec<u32> = state.channels.0.keys().copied().collect();
    for &id in &ids {
        if id == 0 {
            continue;
        }
        let mut current = state.channels.get(id).and_then(Channel::parent);
        let mut steps = 0;
        let mut valid = false;
        while let Some(parent) = current {
            steps += 1;
            if parent == id || steps > ids.len() {
                break;
            }
            if parent == 0 {
                valid = true;
                break;
            }
            current = state.channels.get(parent).and_then(Channel::parent);
        }
        if !valid && let Some(channel) = state.channels.get_mut(id) {
            tracing::warn!(
                channel = id,
                "reattached a channel without a valid parent to the root"
            );
            channel.node.parent = Some(0);
        }
    }
    let mut ordered = ids;
    ordered.sort_unstable();
    for id in ordered {
        if let Some(parent) = state.channels.get(id).and_then(Channel::parent)
            && let Some(parent) = state.channels.get_mut(parent)
        {
            parent.children.push(id);
        }
    }
    for (a, b) in links {
        if state.channels.get(a).is_some() && state.channels.get(b).is_some() {
            if let Some(channel) = state.channels.get_mut(a) {
                channel.links.insert(b);
            }
            if let Some(channel) = state.channels.get_mut(b) {
                channel.links.insert(a);
            }
        }
    }
}

/// Create the state directory owner-only and, for SQLite, the database file
/// owner-only before the worker opens it (CVE-2012-0863: a world-readable
/// Murmur database).
pub fn prepare_files(settings: &Settings, backend: &Backend) -> io::Result<()> {
    let directory = &settings.config.state_directory;
    if !directory.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(directory)?;
    }
    if let Backend::Sqlite { path, .. } = backend
        && !path.exists()
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(path)?;
    }
    Ok(())
}

fn bind_tcp(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::STREAM,
        None,
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

fn bind_udp(address: SocketAddr) -> io::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::DGRAM,
        None,
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    UdpSocket::from_std(socket.into())
}

/// Take the operator lock through a worker from [`storage::start_locked`]:
/// a running server and administrative commands never share a database.
pub async fn hold_operator_lock(storage: &StorageHandle) -> Result<(), StartError> {
    use nuschl_ipc::server::{ServerStorageRequest as Request, ServerStorageResponse as Response};
    match storage.request(Request::OperatorLock).await {
        Some(Response::Done) => Ok(()),
        Some(Response::Held) => Err(StartError::Held),
        _ => Err(StartError::Storage(
            "could not open or lock the database".into(),
        )),
    }
}

/// The server's state worker, holding the operator lock. A previous instance's
/// workers may take a moment to exit after it did, so a held lock is retried
/// for a few seconds before the server gives up.
async fn locked_storage(backend: &Backend) -> Result<StorageHandle, StartError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut waiting = false;
    loop {
        let storage = storage::start_locked(backend.clone());
        match hold_operator_lock(&storage).await {
            Err(StartError::Held) if Instant::now() < deadline => {
                if !std::mem::replace(&mut waiting, true) {
                    tracing::info!("waiting for another process to release the database");
                }
                drop(storage);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            result => return result.map(|()| storage),
        }
    }
}

/// The persisted state for administrative commands, without starting the server.
pub async fn load_state_for_admin(
    settings: &Settings,
    storage: &StorageHandle,
    counters: &Arc<Counters>,
) -> Result<State, StartError> {
    let mut settings = settings.clone();
    settings.config.superuser_password_file = None;
    load_state(&Arc::new(settings), storage, storage, counters).await
}

/// The history's conversations: heads, direct conversations, access
/// intervals; history without a live channel is a deleted channel's archive.
async fn load_history(state: &mut State, history: &StorageHandle) -> Result<(), StartError> {
    let unavailable = || StartError::Storage("could not load the history".into());
    let directs = history.history_directs().await.ok_or_else(unavailable)?;
    for direct in directs {
        if let (Some(id), Some(a), Some(b)) = (
            conversations::parse(&direct.conversation),
            conversations::parse(&direct.account_a),
            conversations::parse(&direct.account_b),
        ) {
            state.conversations.add_direct(id, a, b);
        }
    }
    for (conversation, head) in history.history_heads().await.ok_or_else(unavailable)? {
        if let Some(id) = conversations::parse(&conversation) {
            state.conversations.add_archived(id);
            state.conversations.set_head(id, head);
        }
    }
    for stored in history.history_retentions().await.ok_or_else(unavailable)? {
        if let Some(id) = conversations::parse(&stored.conversation) {
            state.conversations.retention.insert(id, stored.retention);
        }
    }
    for access in history.history_access().await.ok_or_else(unavailable)? {
        if let (Some(id), Some(account)) = (
            conversations::parse(&access.conversation),
            conversations::parse(&access.account),
        ) {
            let intervals = state
                .conversations
                .access
                .entry(id)
                .or_default()
                .entry(account)
                .or_default();
            if intervals.len() < conversations::MAX_INTERVALS {
                intervals.push((access.start, access.end));
            }
        }
    }
    Ok(())
}

/// Load the state and set the SuperUser password from its file, if configured.
async fn load_state(
    settings: &Arc<Settings>,
    storage: &StorageHandle,
    history: &StorageHandle,
    counters: &Arc<Counters>,
) -> Result<State, StartError> {
    let snapshot = storage
        .load()
        .await
        .ok_or_else(|| StartError::Storage("could not load the server state".into()))?;
    let users = storage
        .load_users()
        .await
        .ok_or_else(|| StartError::Storage("could not load registered users".into()))?;
    let mut state = State::new(settings.clone(), storage.clone(), counters.clone());
    build_channels(&mut state, snapshot.channels, snapshot.links);
    for user in users {
        // Accounts from before the extension get their immutable id now.
        let account = user.account.as_deref().and_then(conversations::parse);
        let generated = account.is_none();
        let id = user.id;
        state.registry.insert(Registered {
            id: user.id,
            account: account.unwrap_or_else(crate::state::random),
            name: user.name,
            password_hash: user.password_hash,
            certificate_hash: user.certificate_hash,
            last_channel: user.last_channel,
            last_active: user.last_active,
        });
        if generated {
            state.persist_registration(id);
        }
    }
    state.registry.next_id = snapshot.next_user_id;
    state.bans = snapshot.bans.into_iter().map(Ban::from_stored).collect();
    state.initialise_root();
    load_history(&mut state, history).await?;
    if let Some(path) = &settings.config.superuser_password_file {
        let password = std::fs::read_to_string(path)?
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        set_superuser_password(&mut state, password, settings).await?;
    }
    Ok(state)
}

/// Replace the SuperUser password unless it already is this one.
pub async fn set_superuser_password(
    state: &mut State,
    password: String,
    settings: &Settings,
) -> Result<(), StartError> {
    if password.is_empty() {
        return Err(StartError::Storage(
            "the SuperUser password must not be empty".into(),
        ));
    }
    let current = state
        .registry
        .users
        .get(&0)
        .and_then(|user| user.password_hash.clone());
    if let Some(hash) = current
        && password::verify(password.clone(), hash).await == Some(true)
    {
        return Ok(());
    }
    let cost = password::Cost {
        memory_kib: settings.config.password_hashing.memory_kib,
        passes: settings.config.password_hashing.passes,
    };
    let hash = password::hash(password, cost)
        .await
        .ok_or_else(|| StartError::Storage("could not hash the SuperUser password".into()))?;
    if let Some(user) = state.registry.users.get_mut(&0) {
        user.password_hash = Some(hash);
    }
    state.persist_registration(0);
    tracing::info!("set the SuperUser password");
    Ok(())
}

/// A registered user by name or `#id`, for administrative commands.
pub fn find_account(state: &State, user: &str) -> Result<(conversations::Id, String), StartError> {
    match user.strip_prefix('#').map(str::parse::<u32>) {
        Some(Ok(id)) => state.registry.users.get(&id),
        _ => state.registry.by_name(user),
    }
    .map(|registered| (registered.account, registered.name.clone()))
    .ok_or_else(|| StartError::Storage(format!("no registered user {user}")))
}

/// The operator's erase (docs/extension.md §9): every message stored by an
/// account becomes a tombstone without content. Copies already on clients
/// (Nuschl caches, stock clients' logs) remain.
pub async fn erase_account(
    storage: &StorageHandle,
    account: conversations::Id,
) -> Result<u64, StartError> {
    match storage
        .request(nuschl_ipc::server::ServerStorageRequest::HistoryErase {
            account: conversations::hex(&account),
            deleted_ms: crate::chat::now_ms(),
        })
        .await
    {
        Some(nuschl_ipc::server::ServerStorageResponse::Count(count)) => Ok(count),
        _ => Err(StartError::Storage("could not erase the messages".into())),
    }
}

/// The default backend: the storage worker next to this executable.
pub fn default_backend(settings: &Settings) -> io::Result<Backend> {
    let worker = match &settings.config.storage_worker {
        Some(path) => path.clone(),
        None => std::env::current_exe()?
            .parent()
            .map(|directory| directory.join("nuschl-storage-worker"))
            .ok_or_else(|| io::Error::other("cannot locate nuschl-storage-worker"))?,
    };
    Ok(match &settings.config.database_url_file {
        Some(path) => Backend::UrlFile {
            worker,
            path: path.clone(),
        },
        None => Backend::Sqlite {
            worker,
            path: settings.config.database_path(),
        },
    })
}

pub async fn start(settings: Settings, backend: Backend) -> Result<Running, StartError> {
    let settings = Arc::new(settings);
    prepare_files(&settings, &backend)?;
    // Three workers on one database: server state, history writes, history
    // reads; a history page never waits behind a write, nor a chat behind a
    // channel save.
    // The state worker holds the operator lock (taken before it opens the
    // database) for as long as the server runs.
    let storage = locked_storage(&backend).await?;
    let history_writer = storage::start(backend.clone());
    let history_reads = storage::start(backend.clone());
    let counters = Arc::new(Counters::default());
    let mut state = load_state(&settings, &storage, &history_reads, &counters).await?;
    state.share_links = crate::share::Links::load(&storage)
        .await
        .ok_or_else(|| StartError::Storage("could not load share links".into()))?;
    let (queues, receivers) = history::queues();
    state.history = queues;
    // Key transparency on its own storage worker: a lookup never waits behind
    // a history write.
    let mut mls_seed = None;
    let kt_service = if settings.config.kt.enabled {
        let keys = kt::Keys::load(&settings.config.kt, &settings.config.state_directory)?;
        mls_seed = Some(keys.signing_seed());
        let (handle, service) =
            kt::open(&settings.config.kt, keys, storage::start(backend.clone())).await?;
        state.kt = Some(handle);
        Some(service)
    } else {
        None
    };
    let kt_log = state.kt.as_ref().map(|kt| kt.descriptor.clone());
    // End-to-end mode runs the MLS delivery service, which needs key transparency
    // (credentials are checked against it; the settings require it), on its own
    // storage worker.
    let e2ee = settings.config.security == crate::config::Security::E2ee;
    let mls_service = match (&state.kt, e2ee, mls_seed) {
        (Some(kt), true, Some(seed)) => {
            // The groups' external sender: the log's head-signing key, which every
            // client pins with the log descriptor.
            let public: [u8; 32] = nuschl_kt::LogDescriptor::decode(&kt.descriptor)
                .map_err(|error| StartError::Storage(format!("mls: the log descriptor: {error}")))?
                .signing_key()
                .to_bytes()
                .try_into()
                .map_err(|_| StartError::Storage("mls: the log key is not Ed25519".into()))?;
            let signer = nuschl_e2ee::ds::ServerSigner::new(&seed, public)
                .map_err(|error| StartError::Storage(format!("mls: {error}")))?;
            let (handle, service) = mls::open(
                &settings.config.mls,
                storage::start(backend.clone()),
                kt.log_id.0,
                signer,
            )
            .await?;
            state.mls = Some(handle);
            Some(service)
        }
        _ => None,
    };
    let mls_offered = mls_service.is_some();
    if e2ee {
        // Said plainly, so nobody believes more is protected than is (TODO.md,
        // phase 4; docs/server.md, "Security mode").
        tracing::warn!(
            "end-to-end mode: stock clients are refused and text travels only in MLS \
             groups; voice, whispers and shouts are disabled until SFrame (phase 4e), \
             and read markers are still visible to the server"
        );
    }
    if state
        .registry
        .users
        .get(&0)
        .is_none_or(|user| user.password_hash.is_none())
    {
        tracing::info!(
            "SuperUser has no password and cannot log in; set one with superuser_password_file \
             or `nuschld --set-superuser-password`"
        );
    }
    let config = &settings.config;
    let identity = tls::Identity::load_or_generate(
        config
            .certificate
            .as_deref()
            .zip(config.private_key.as_deref()),
        &config.state_directory,
    )?;
    let fingerprint = identity.fingerprint();
    tracing::info!(sha256 = %fingerprint, "server certificate");
    let (tls_config, certificates) = tls::server_config(
        identity,
        config.minimum_tls_version == crate::config::TlsVersion::Tls13,
    )?;
    let acceptor = TlsAcceptor::from(tls_config);

    // Bind everything first; with port 0 (tests) UDP takes the port TCP got.
    let mut listeners = Vec::new();
    let mut sockets = Vec::new();
    for &ip in &config.bind {
        let requested = SocketAddr::new(ip, config.port);
        let listener = match bind_tcp(requested) {
            Ok(listener) => listener,
            Err(error) if ip.is_ipv6() && config.bind.len() > 1 => {
                tracing::warn!(address = %requested, %error, "IPv6 unavailable; continuing without it");
                continue;
            }
            Err(source) => {
                return Err(StartError::Bind {
                    address: requested,
                    source,
                });
            }
        };
        let address = listener.local_addr()?;
        let socket = bind_udp(address).map_err(|source| StartError::Bind { address, source })?;
        tracing::info!(%address, "listening on TCP and UDP");
        listeners.push((listener, address));
        sockets.push(Arc::new(socket));
    }
    let server = Arc::new(Server {
        state: Mutex::new(state),
        settings: settings.clone(),
        storage,
        history_reads: history_reads.clone(),
        counters,
        udp: sockets.clone(),
        acceptor,
        certificates,
        admission: Mutex::new(Admission::default()),
        pending: Arc::new(Semaphore::new(config.max_pending_connections as usize)),
        version_frame: extension::version_frame(&server_version(&settings), None),
        nuschl_version_frame: extension::version_frame(
            &server_version(&settings),
            Some(&extension::server_hello(
                &settings,
                kt_log.as_deref(),
                mls_offered,
            )),
        ),
        capabilities: extension::server_capabilities(kt_log.is_some(), mls_offered, e2ee),
        shutdown: Notify::new(),
        login_failures: Mutex::new(HashMap::new()),
    });
    let mut tasks = Vec::new();
    let mut addresses = Vec::new();
    for (listener, address) in listeners {
        addresses.push(address);
        tasks.push(tokio::spawn(connection::accept(server.clone(), listener)));
    }
    for (index, socket) in sockets.into_iter().enumerate() {
        tasks.push(tokio::spawn(udp::run(server.clone(), index, socket)));
    }
    tasks.extend(history::spawn(
        &server,
        history_writer,
        history_reads,
        receivers,
    ));
    // Retention on its own worker, started at the first expiry that has work.
    tasks.push(tokio::spawn(crate::retention::run(
        server.clone(),
        storage::start(backend.clone()),
    )));
    if settings.config.statistics_interval > 0 {
        tasks.push(tokio::spawn(statistics(server.clone())));
    }
    if let Some(service) = kt_service {
        tasks.push(tokio::spawn(service.run(server.clone())));
    }
    if let Some(service) = mls_service {
        tasks.push(tokio::spawn(service.run(server.clone())));
    }
    Ok(Running {
        server,
        addresses,
        fingerprint,
        tasks,
    })
}

/// What happened in one statistics interval, from the counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statistics {
    pub connections: u64,
    pub logins: u64,
    pub rejected: u64,
    pub control_messages: u64,
    pub control_bytes: u64,
    pub voice_packets_in: u64,
    pub voice_bytes_in: u64,
    pub voice_packets_out: u64,
    pub voice_bytes_out: u64,
    pub rate_limited: u64,
    pub fanout_dropped: u64,
}

impl Statistics {
    /// The counters' totals since startup.
    pub fn totals(counters: &Counters) -> Self {
        let load = |counter: &std::sync::atomic::AtomicU64| {
            counter.load(std::sync::atomic::Ordering::Relaxed)
        };
        Self {
            connections: load(&counters.connections),
            logins: load(&counters.logins),
            rejected: load(&counters.rejected),
            control_messages: load(&counters.control_messages),
            control_bytes: load(&counters.control_bytes),
            voice_packets_in: load(&counters.voice_packets_in),
            voice_bytes_in: load(&counters.voice_bytes_in),
            voice_packets_out: load(&counters.voice_packets_out),
            voice_bytes_out: load(&counters.voice_bytes_out),
            rate_limited: load(&counters.rate_limited),
            fanout_dropped: load(&counters.fanout_dropped),
        }
    }

    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            connections: self.connections.saturating_sub(earlier.connections),
            logins: self.logins.saturating_sub(earlier.logins),
            rejected: self.rejected.saturating_sub(earlier.rejected),
            control_messages: self
                .control_messages
                .saturating_sub(earlier.control_messages),
            control_bytes: self.control_bytes.saturating_sub(earlier.control_bytes),
            voice_packets_in: self
                .voice_packets_in
                .saturating_sub(earlier.voice_packets_in),
            voice_bytes_in: self.voice_bytes_in.saturating_sub(earlier.voice_bytes_in),
            voice_packets_out: self
                .voice_packets_out
                .saturating_sub(earlier.voice_packets_out),
            voice_bytes_out: self.voice_bytes_out.saturating_sub(earlier.voice_bytes_out),
            rate_limited: self.rate_limited.saturating_sub(earlier.rate_limited),
            fanout_dropped: self.fanout_dropped.saturating_sub(earlier.fanout_dropped),
        }
    }
}

/// Rolling statistics: every `statistics_interval` seconds one log line with
/// what happened in that interval (and the users online), for operators
/// reading the journal. Counts only, no names or addresses.
async fn statistics(server: Arc<Server>) {
    let seconds = server.settings.config.statistics_interval;
    let mut tick = tokio::time::interval(Duration::from_secs(seconds));
    tick.tick().await;
    let mut last = Statistics::totals(&server.counters);
    loop {
        tick.tick().await;
        let now = Statistics::totals(&server.counters);
        let delta = now.since(&last);
        last = now;
        let (users, channels) = {
            let state = server.lock();
            (state.users.len(), state.channels.0.len())
        };
        tracing::info!(
            interval_seconds = seconds,
            users,
            channels,
            connections = delta.connections,
            logins = delta.logins,
            rejected = delta.rejected,
            control_messages = delta.control_messages,
            control_bytes = delta.control_bytes,
            voice_packets_in = delta.voice_packets_in,
            voice_bytes_in = delta.voice_bytes_in,
            voice_packets_out = delta.voice_packets_out,
            voice_bytes_out = delta.voice_bytes_out,
            rate_limited = delta.rate_limited,
            whisper_fanout_dropped = delta.fanout_dropped,
            "statistics"
        );
    }
}

/// A rate limiter for unencrypted pings: per address prefix and global, and
/// the amplification policy (replies no larger than requests unless allowed).
pub struct PingLimiter {
    global: LeakyBucket,
    prefixes: HashMap<IpAddr, LeakyBucket>,
    source: IpAddr,
    allow_double: bool,
}

impl PingLimiter {
    pub fn new(allow_double: bool) -> Self {
        Self {
            global: LeakyBucket::new(500, 1000, Instant::now()),
            prefixes: HashMap::new(),
            source: IpAddr::from([0, 0, 0, 0]),
            allow_double,
        }
    }

    pub fn from(&mut self, source: IpAddr) -> &mut Self {
        self.source = source;
        self
    }
}

impl nuschl_protocol::relay::PingBudget for PingLimiter {
    fn allow(&mut self, request_length: usize, reply_length: usize) -> bool {
        if reply_length > request_length && !self.allow_double {
            return false;
        }
        let now = Instant::now();
        if self.prefixes.len() > 65_536 {
            self.prefixes.clear();
        }
        let bucket = self
            .prefixes
            .entry(prefix(self.source))
            .or_insert_with(|| LeakyBucket::new(4, 16, now));
        !bucket.limited(now) && !self.global.limited(now)
    }
}
