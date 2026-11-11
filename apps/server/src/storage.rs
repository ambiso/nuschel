// SPDX-License-Identifier: EUPL-1.2
//! Persistence through the storage worker (`nuschel-storage-worker --server`),
//! which owns SQLite/PostgreSQL and the database credential; `nuscheld` links
//! no database code (deny.toml, SECURITY.md). Writes are queued in order and
//! never awaited by the control path; reads (startup, a registered user's
//! blobs) are awaited outside the state lock. An in-memory backend serves tests
//! and `--ephemeral` runs.
use base64::Engine as _;
use bytes::Bytes;
use nuschel_ipc::{
    Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    server::{
        HISTORY_LOAD_PAGE, MessageKind, Retention, SeqRows, ServerSnapshot,
        ServerStorageRequest as Request, ServerStorageResponse as Response, StoredAccess,
        StoredBan, StoredChannel, StoredDirect, StoredMessage, StoredRetention, StoredUser,
        USER_PAGE, expirable,
    },
    write_frame,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot},
    time::timeout,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

enum Job {
    Write(Request),
    Read(Request, oneshot::Sender<Option<Response>>),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct StorageHandle {
    jobs: mpsc::UnboundedSender<Job>,
    /// Requests the storage rejected or could not answer (each is logged).
    failures: Arc<std::sync::atomic::AtomicU64>,
}

impl StorageHandle {
    fn write(&self, request: Request) {
        // A closed queue means the server is shutting down.
        let _ = self.jobs.send(Job::Write(request));
    }

    async fn read(&self, request: Request) -> Option<Response> {
        let (reply, response) = oneshot::channel();
        self.jobs.send(Job::Read(request, reply)).ok()?;
        response.await.ok().flatten()
    }

    /// Send a request and wait for its answer (`None` if storage failed).
    pub async fn request(&self, request: Request) -> Option<Response> {
        self.read(request).await
    }

    /// Queue a write whose answer nobody waits for.
    pub fn send(&self, request: Request) {
        self.write(request);
    }

    /// Load everything that comes in pages until a short page.
    async fn pages<T, K>(
        &self,
        request: impl Fn(Option<K>) -> Request,
        unpack: impl Fn(Response) -> Option<Vec<T>>,
        key: impl Fn(&T) -> K,
    ) -> Option<Vec<T>> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = unpack(self.read(request(after)).await?)?;
            let full = page.len() >= HISTORY_LOAD_PAGE as usize;
            after = page.last().map(&key);
            out.extend(page);
            if !full || after.is_none() {
                return Some(out);
            }
        }
    }

    pub async fn history_heads(&self) -> Option<Vec<(String, u64)>> {
        self.pages(
            |after| Request::HistoryHeads { after },
            |response| match response {
                Response::Heads(page) => Some(page),
                _ => None,
            },
            |(conversation, _)| conversation.clone(),
        )
        .await
    }

    pub async fn history_directs(&self) -> Option<Vec<StoredDirect>> {
        self.pages(
            |after| Request::HistoryDirects { after },
            |response| match response {
                Response::Directs(page) => Some(page),
                _ => None,
            },
            |direct| direct.conversation.clone(),
        )
        .await
    }

    pub async fn history_access(&self) -> Option<Vec<StoredAccess>> {
        self.pages(
            |after| Request::HistoryAccess { after },
            |response| match response {
                Response::Access(page) => Some(page),
                _ => None,
            },
            |access| {
                (
                    access.conversation.clone(),
                    access.account.clone(),
                    access.start,
                )
            },
        )
        .await
    }

    /// Conversations with their own retention.
    pub async fn history_retentions(&self) -> Option<Vec<StoredRetention>> {
        self.pages(
            |after| Request::HistoryRetentions { after },
            |response| match response {
                Response::Retentions(page) => Some(page),
                _ => None,
            },
            |stored| stored.conversation.clone(),
        )
        .await
    }

    pub async fn history_read_markers(&self, account: String) -> Vec<(String, u64)> {
        match self.read(Request::HistoryReadMarkers { account }).await {
            Some(Response::ReadMarkers(markers)) => markers,
            _ => Vec::new(),
        }
    }

    pub fn save_channel(&self, channel: StoredChannel) {
        self.write(Request::SaveChannel { channel });
    }

    pub fn delete_channel(&self, id: u32) {
        self.write(Request::DeleteChannel { id });
    }

    pub fn link(&self, a: u32, b: u32) {
        self.write(Request::Link { a, b });
    }

    pub fn unlink(&self, a: u32, b: u32) {
        self.write(Request::Unlink { a, b });
    }

    pub fn save_user(&self, user: StoredUser) {
        self.write(Request::SaveUser { user });
    }

    pub fn delete_user(&self, id: u32) {
        self.write(Request::DeleteUser { id });
    }

    pub fn set_comment(&self, id: u32, comment: String) {
        self.write(Request::SetComment { id, comment });
    }

    pub fn set_texture(&self, id: u32, texture: String) {
        self.write(Request::SetTexture { id, texture });
    }

    pub fn save_bans(&self, bans: Vec<StoredBan>) {
        self.write(Request::SaveBans { bans });
    }

    pub fn set_next_user_id(&self, id: u32) {
        self.write(Request::SetNextUserId { id });
    }

    /// Remember a registered user's listener (volume must be finite).
    pub fn set_listener(&self, user_id: u32, channel: u32, volume: f32) {
        if volume.is_finite() {
            self.write(Request::SetListener {
                user_id,
                channel,
                volume,
            });
        }
    }

    pub fn remove_listener(&self, user_id: u32, channel: u32) {
        self.write(Request::RemoveListener { user_id, channel });
    }

    /// A registered user's remembered listeners: channel and volume.
    pub async fn user_listeners(&self, id: u32) -> Vec<(u32, f32)> {
        match self.read(Request::UserListeners { id }).await {
            Some(Response::Listeners(listeners)) => listeners,
            _ => Vec::new(),
        }
    }

    /// A handle whose writes go nowhere (fuzzing and unit tests of the state).
    pub fn detached() -> Self {
        let (jobs, _) = mpsc::unbounded_channel();
        Self {
            jobs,
            failures: Default::default(),
        }
    }

    /// How many requests have failed so far (rejected or unanswered).
    pub fn failures(&self) -> u64 {
        self.failures.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Everything but the registered users.
    pub async fn load(&self) -> Option<ServerSnapshot> {
        match self.read(Request::Load).await? {
            Response::Snapshot(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    pub async fn load_users(&self) -> Option<Vec<StoredUser>> {
        let mut users = Vec::new();
        let mut after = None;
        loop {
            let Response::Users(page) = self.read(Request::LoadUsers { after }).await? else {
                return None;
            };
            let full = page.len() >= USER_PAGE as usize;
            after = page.last().map(|user| user.id);
            users.extend(page);
            if !full || after.is_none() {
                return Some(users);
            }
        }
    }

    /// A registered user's comment and avatar.
    pub async fn user_blobs(&self, id: u32) -> (String, Bytes) {
        match self.read(Request::UserBlobs { id }).await {
            Some(Response::Blobs { comment, texture }) => (
                comment,
                base64::engine::general_purpose::STANDARD
                    .decode(texture)
                    .map(Bytes::from)
                    .unwrap_or_default(),
            ),
            _ => (String::new(), Bytes::new()),
        }
    }

    /// Wait until every queued write has been processed.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.jobs.send(Job::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

/// Where state is kept.
#[derive(Clone)]
pub enum Backend {
    /// The storage worker with a SQLite path.
    Sqlite { worker: PathBuf, path: PathBuf },
    /// The storage worker with a credential file holding the database URL.
    UrlFile { worker: PathBuf, path: PathBuf },
    /// Kept in memory (shared, so a test can restart a server on the same state).
    Memory(Arc<Mutex<MemoryStore>>),
}

pub fn start(backend: Backend) -> StorageHandle {
    start_with(backend, false)
}

/// A worker that takes the operator lock before it opens the database (a
/// running server's state worker, `nuscheld admin`); ask it
/// `OperatorLock` first: `Held` means another process uses the database.
pub fn start_locked(backend: Backend) -> StorageHandle {
    start_with(backend, true)
}

fn start_with(backend: Backend, lock: bool) -> StorageHandle {
    let (jobs, receiver) = mpsc::unbounded_channel();
    let failures: Arc<std::sync::atomic::AtomicU64> = Default::default();
    tokio::spawn(run(backend, receiver, lock, failures.clone()));
    StorageHandle { jobs, failures }
}

async fn run(
    backend: Backend,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    lock: bool,
    failures: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut worker: Option<Worker> = None;
    let mut request_id = 0_u64;
    while let Some(job) = jobs.recv().await {
        let (request, reply) = match job {
            Job::Write(request) => (request, None),
            Job::Read(request, reply) => (request, Some(reply)),
            Job::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        // Its caller reports a held lock itself.
        let probe = matches!(request, Request::OperatorLock);
        let response = match &backend {
            Backend::Memory(store) => Some(
                store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .apply(request),
            ),
            Backend::Sqlite {
                worker: executable,
                path,
            }
            | Backend::UrlFile {
                worker: executable,
                path,
            } => {
                let url = matches!(backend, Backend::UrlFile { .. });
                // One retry with a fresh worker if it died.
                let mut response = None;
                for _ in 0..2 {
                    if worker.is_none() {
                        match Worker::spawn(executable, path, url, lock) {
                            Ok(spawned) => worker = Some(spawned),
                            Err(error) => {
                                tracing::error!(%error, "cannot start the storage worker");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                continue;
                            }
                        }
                    }
                    let Some(current) = worker.as_mut() else {
                        continue;
                    };
                    request_id += 1;
                    match timeout(REQUEST_TIMEOUT, current.exchange(request_id, &request)).await {
                        Ok(Ok(answer)) => {
                            response = Some(answer);
                            break;
                        }
                        Ok(Err(error)) => tracing::error!(%error, "storage worker failed"),
                        Err(_) => tracing::error!("storage worker timed out"),
                    }
                    worker = None;
                }
                response
            }
        };
        if let Some(Response::Rejected(error)) = &response {
            failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(%error, "storage request was rejected; this change is not persisted");
        } else if let Some(Response::Held) = &response
            && !probe
        {
            tracing::error!("another process holds the database; this change is not persisted");
        } else if response.is_none() {
            failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!("storage is unavailable; this change is not persisted");
        }
        if let Some(reply) = reply {
            let _ = reply.send(response);
        }
    }
}

struct Worker {
    _child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl Worker {
    fn spawn(
        executable: &PathBuf,
        path: &PathBuf,
        url_file: bool,
        lock: bool,
    ) -> std::io::Result<Self> {
        let mut command = Command::new(executable);
        command.arg("--server");
        if lock {
            command.arg("--operator-lock");
        }
        command
            .arg(if url_file {
                "--database-url-file"
            } else {
                "--database"
            })
            .arg(path)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("worker stdin missing"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("worker stdout missing"))?;
        Ok(Self {
            _child: child,
            input,
            output,
        })
    }

    async fn exchange(&mut self, id: u64, request: &Request) -> std::io::Result<Response> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Envelope::new(id, request)).map_err(std::io::Error::other)?;
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        let size = self.output.read_u32().await? as usize;
        if size > MAX_FRAME_BYTES {
            return Err(std::io::Error::other("oversized storage response"));
        }
        let mut body = vec![0; size];
        self.output.read_exact(&mut body).await?;
        let envelope: Envelope<Response> =
            serde_json::from_slice(&body).map_err(std::io::Error::other)?;
        if envelope.version != PROTOCOL_VERSION || envelope.request_id != id {
            return Err(std::io::Error::other("storage protocol mismatch"));
        }
        Ok(envelope.payload)
    }
}

/// The storage worker's behaviour, in memory.
#[derive(Default)]
pub struct MemoryStore {
    channels: BTreeMap<u32, StoredChannel>,
    links: BTreeSet<(u32, u32)>,
    users: BTreeMap<u32, (StoredUser, String, String)>,
    /// (user, channel) → volume.
    listeners: BTreeMap<(u32, u32), f32>,
    bans: Vec<StoredBan>,
    next_user_id: u32,
    pub history: MemoryHistory,
    /// The key-transparency table.
    pub kt: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// The history tables, in memory.
#[derive(Default)]
pub struct MemoryHistory {
    pub messages: BTreeMap<(String, u64), StoredMessage>,
    directs: BTreeMap<String, StoredDirect>,
    access: BTreeMap<(String, String, u64), Option<u64>>,
    /// (account, conversation) → seq.
    markers: BTreeMap<(String, String), u64>,
    /// (conversation, account, key) → seq and body hash: the unique index.
    keys: std::collections::HashMap<(String, String, String), (u64, Option<String>)>,
    /// Conversations' own retention, and live-only conversations' heads.
    retention: BTreeMap<String, Retention>,
    heads: BTreeMap<String, u64>,
}

struct MemoryRows<'a>(&'a BTreeMap<(String, u64), StoredMessage>);

impl SeqRows for MemoryRows<'_> {
    type Error = ();

    async fn rows(
        &mut self,
        conversation: &str,
        (lo, hi): (u64, u64),
        descending: bool,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, ()> {
        let range = self
            .0
            .range((conversation.to_owned(), lo)..=(conversation.to_owned(), hi))
            .map(|(_, row)| row.clone());
        Ok(if descending {
            range.rev().take(limit).collect()
        } else {
            range.take(limit).collect()
        })
    }
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

/// Run a future that never waits (the in-memory store) to completion.
fn ready<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

impl MemoryHistory {
    fn insert(&mut self, row: StoredMessage) {
        self.messages
            .insert((row.conversation.clone(), row.seq), row);
    }

    fn void(&mut self, event: &StoredMessage) {
        self.insert(StoredMessage::void(
            &event.conversation,
            event.seq,
            event.sent_ms,
        ));
    }

    /// The author's live message `target`, if any.
    fn live(
        &mut self,
        conversation: &str,
        target: u64,
        author: Option<&str>,
    ) -> Option<&mut StoredMessage> {
        self.messages
            .get_mut(&(conversation.to_owned(), target))
            .filter(|row| {
                row.kind == MessageKind::Message
                    && row.deleted_ms.is_none()
                    && author.is_none_or(|author| row.author_account.as_deref() == Some(author))
            })
    }

    /// One conversation's rows, oldest first.
    fn conversation(&mut self, conversation: &str) -> impl Iterator<Item = &mut StoredMessage> {
        self.messages
            .range_mut((conversation.to_owned(), 0)..=(conversation.to_owned(), u64::MAX))
            .map(|(_, row)| row)
    }

    fn tombstone(row: &mut StoredMessage, deleted_ms: i64, reason: u32) {
        row.content.clear();
        row.reply_to = None;
        row.deleted_ms = Some(deleted_ms);
        row.delete_reason = Some(reason);
    }

    fn apply(&mut self, request: Request) -> Response {
        match request {
            Request::HistoryAppend { message } => {
                let key = message
                    .author_account
                    .clone()
                    .zip(message.idempotency_key.clone())
                    .map(|(account, key)| (message.conversation.clone(), account, key));
                let earlier = key.as_ref().and_then(|key| self.keys.get(key).cloned());
                match earlier {
                    Some((seq, hash)) => {
                        self.void(&message);
                        if hash == message.body_hash {
                            Response::Appended {
                                seq,
                                duplicate: true,
                            }
                        } else {
                            Response::Conflict
                        }
                    }
                    None => {
                        let seq = message.seq;
                        if let Some(key) = key {
                            self.keys.insert(key, (seq, message.body_hash.clone()));
                        }
                        self.insert(message);
                        Response::Appended {
                            seq,
                            duplicate: false,
                        }
                    }
                }
            }
            Request::HistoryEdit {
                target,
                author_account,
                format,
                content,
                edit_limit,
                event,
            } => {
                let outcome = match self.live(&event.conversation, target, Some(&author_account)) {
                    None => Response::Missing,
                    Some(row) if row.edits >= edit_limit => Response::LimitReached,
                    Some(row) => {
                        row.content = content;
                        row.format = format;
                        row.edited_ms = Some(event.sent_ms);
                        row.edits += 1;
                        Response::Done
                    }
                };
                if matches!(outcome, Response::Done) {
                    self.insert(event);
                } else {
                    self.void(&event);
                }
                outcome
            }
            Request::HistoryDelete {
                target,
                author_account,
                reason,
                event,
            } => match self.live(&event.conversation, target, author_account.as_deref()) {
                Some(row) => {
                    Self::tombstone(row, event.sent_ms, reason);
                    self.insert(event);
                    Response::Done
                }
                None => {
                    self.void(&event);
                    Response::Missing
                }
            },
            Request::HistoryPurge { conversation } => {
                self.retention.remove(&conversation);
                self.heads.remove(&conversation);
                self.messages.retain(|(id, _), _| *id != conversation);
                self.keys.retain(|(id, _, _), _| *id != conversation);
                self.access.retain(|(id, _, _), _| *id != conversation);
                self.markers.retain(|(_, id), _| *id != conversation);
                Response::Done
            }
            Request::HistoryCreateDirect { direct } => {
                self.directs
                    .entry(direct.conversation.clone())
                    .or_insert(direct);
                Response::Done
            }
            Request::HistorySetAccess {
                conversation,
                account,
                intervals,
            } => {
                self.access
                    .retain(|(id, who, _), _| *id != conversation || *who != account);
                for (start, end) in intervals {
                    self.access
                        .insert((conversation.clone(), account.clone(), start), end);
                }
                Response::Done
            }
            Request::HistoryForgetAccount { account } => {
                self.access.retain(|(_, who, _), _| *who != account);
                self.markers.retain(|(who, _), _| *who != account);
                Response::Done
            }
            Request::HistorySetReadMarker {
                account,
                conversation,
                seq,
            } => {
                let marker = self.markers.entry((account, conversation)).or_insert(0);
                *marker = (*marker).max(seq);
                Response::Done
            }
            Request::HistoryErase {
                account,
                deleted_ms,
            } => {
                let mut count = 0;
                // MLS commits stay: members need them to catch up (docs/e2ee.md §6.3).
                for row in self.messages.values_mut().filter(|row| {
                    expirable(row) && row.author_account.as_deref() == Some(account.as_str())
                }) {
                    Self::tombstone(row, deleted_ms, 3);
                    count += 1;
                }
                Response::Count(count)
            }
            Request::HistoryExpire {
                conversation,
                before_ms,
                deleted_ms,
                limit,
            } => {
                let mut count = 0;
                for row in self
                    .conversation(&conversation)
                    .filter(|row| expirable(row) && row.sent_ms < before_ms)
                    .take(limit as usize)
                {
                    Self::tombstone(row, deleted_ms, 2);
                    count += 1;
                }
                Response::Count(count)
            }
            Request::HistoryCountExpirable {
                conversation,
                before_ms,
            } => Response::Count(
                self.conversation(&conversation)
                    .filter(|row| expirable(row) && row.sent_ms < before_ms)
                    .count() as u64,
            ),
            Request::HistorySetRetention {
                conversation,
                retention,
            } => {
                match retention {
                    Some(retention) => self.retention.insert(conversation, retention),
                    None => self.retention.remove(&conversation),
                };
                Response::Done
            }
            Request::HistoryRetentions { after } => Response::Retentions(
                self.retention
                    .iter()
                    .filter(|(id, _)| after.as_ref().is_none_or(|after| *id > after))
                    .take(HISTORY_LOAD_PAGE as usize)
                    .map(|(conversation, retention)| StoredRetention {
                        conversation: conversation.clone(),
                        retention: *retention,
                    })
                    .collect(),
            ),
            Request::HistoryAdvance { conversation, seq } => {
                let head = self.heads.entry(conversation).or_insert(0);
                *head = (*head).max(seq);
                Response::Done
            }
            Request::HistoryHeads { after } => {
                let mut heads = BTreeMap::<String, u64>::new();
                for (conversation, seq) in self
                    .messages
                    .keys()
                    .map(|(id, seq)| (id, seq))
                    .chain(self.heads.iter())
                {
                    if after.as_ref().is_none_or(|after| conversation > after) {
                        let head = heads.entry(conversation.clone()).or_insert(0);
                        *head = (*head).max(*seq);
                    }
                }
                Response::Heads(heads.into_iter().take(HISTORY_LOAD_PAGE as usize).collect())
            }
            Request::HistoryDirects { after } => Response::Directs(
                self.directs
                    .values()
                    .filter(|direct| {
                        after
                            .as_ref()
                            .is_none_or(|after| direct.conversation > *after)
                    })
                    .take(HISTORY_LOAD_PAGE as usize)
                    .cloned()
                    .collect(),
            ),
            Request::HistoryAccess { after } => Response::Access(
                self.access
                    .iter()
                    .filter(|(key, _)| after.as_ref().is_none_or(|after| *key > after))
                    .take(HISTORY_LOAD_PAGE as usize)
                    .map(|((conversation, account, start), end)| StoredAccess {
                        conversation: conversation.clone(),
                        account: account.clone(),
                        start: *start,
                        end: *end,
                    })
                    .collect(),
            ),
            Request::HistoryReadMarkers { account } => Response::ReadMarkers(
                self.markers
                    .iter()
                    .filter(|((who, _), _)| *who == account)
                    .map(|((_, conversation), seq)| (conversation.clone(), *seq))
                    .collect(),
            ),
            Request::HistoryPage { query } => {
                match ready(nuschel_ipc::server::page(
                    &mut MemoryRows(&self.messages),
                    &query,
                )) {
                    Some(Ok(page)) => Response::Page(page),
                    _ => Response::Rejected("page failed".into()),
                }
            }
            _ => Response::Rejected("not a history request".into()),
        }
    }
}

impl MemoryStore {
    pub fn shared() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    fn apply(&mut self, request: Request) -> Response {
        match request {
            Request::Load => Response::Snapshot(ServerSnapshot {
                channels: self.channels.values().cloned().collect(),
                links: self.links.iter().copied().collect(),
                bans: self.bans.clone(),
                next_user_id: self.next_user_id,
            }),
            Request::LoadUsers { after } => Response::Users(
                self.users
                    .range(after.map_or(0, |after| after.saturating_add(1))..)
                    .take(USER_PAGE as usize)
                    .map(|(_, (user, _, _))| user.clone())
                    .collect(),
            ),
            Request::SaveChannel { channel } => {
                self.channels.insert(channel.id, channel);
                Response::Done
            }
            Request::DeleteChannel { id } => {
                let mut doomed = vec![id];
                while let Some(current) = doomed.pop() {
                    self.channels.remove(&current);
                    self.listeners.retain(|(_, channel), _| *channel != current);
                    self.links.retain(|(a, b)| *a != current && *b != current);
                    doomed.extend(
                        self.channels
                            .values()
                            .filter(|channel| channel.parent == Some(current))
                            .map(|channel| channel.id),
                    );
                }
                Response::Done
            }
            Request::Link { a, b } => {
                self.links.insert((a.min(b), a.max(b)));
                Response::Done
            }
            Request::Unlink { a, b } => {
                self.links.remove(&(a.min(b), a.max(b)));
                Response::Done
            }
            Request::SaveUser { user } => {
                let entry = self
                    .users
                    .entry(user.id)
                    .or_insert_with(|| (user.clone(), String::new(), String::new()));
                entry.0 = user;
                Response::Done
            }
            Request::DeleteUser { id } => {
                self.users.remove(&id);
                self.listeners.retain(|(user, _), _| *user != id);
                Response::Done
            }
            // The database's foreign keys: both must exist.
            Request::SetListener {
                user_id,
                channel,
                volume,
            } => {
                if self.users.contains_key(&user_id) && self.channels.contains_key(&channel) {
                    self.listeners.insert((user_id, channel), volume);
                    Response::Done
                } else {
                    Response::Rejected("no such user or channel".into())
                }
            }
            Request::RemoveListener { user_id, channel } => {
                self.listeners.remove(&(user_id, channel));
                Response::Done
            }
            Request::UserListeners { id } => Response::Listeners(
                self.listeners
                    .range((id, 0)..=(id, u32::MAX))
                    .map(|(&(_, channel), &volume)| (channel, volume))
                    .collect(),
            ),
            Request::SetComment { id, comment } => {
                if let Some(entry) = self.users.get_mut(&id) {
                    entry.1 = comment;
                }
                Response::Done
            }
            Request::SetTexture { id, texture } => {
                if let Some(entry) = self.users.get_mut(&id) {
                    entry.2 = texture;
                }
                Response::Done
            }
            Request::UserBlobs { id } => {
                let (comment, texture) = self
                    .users
                    .get(&id)
                    .map(|(_, comment, texture)| (comment.clone(), texture.clone()))
                    .unwrap_or_default();
                Response::Blobs { comment, texture }
            }
            Request::SaveBans { bans } => {
                self.bans = bans;
                Response::Done
            }
            Request::SetNextUserId { id } => {
                self.next_user_id = self.next_user_id.max(id);
                Response::Done
            }
            // In memory, nobody else can hold it.
            Request::OperatorLock => Response::Done,
            Request::KtGet { keys } => {
                let values: Option<Vec<Option<String>>> = keys
                    .iter()
                    .map(|key| {
                        let key = unbase64(key)?;
                        Some(self.kt.get(&key).map(|value| base64(value)))
                    })
                    .collect();
                values.map_or_else(
                    || Response::Rejected("malformed key".into()),
                    Response::KtValues,
                )
            }
            Request::KtScan { prefix } => match unbase64(&prefix) {
                Some(prefix) => Response::KtRecords(
                    self.kt
                        .range(prefix.clone()..)
                        .take_while(|(key, _)| key.starts_with(&prefix))
                        .map(|(key, value)| (base64(key), base64(value)))
                        .collect(),
                ),
                None => Response::Rejected("malformed prefix".into()),
            },
            Request::KtWrite { records } => {
                let decoded: Option<Vec<(Vec<u8>, Vec<u8>)>> = records
                    .iter()
                    .map(|(key, value)| Some((unbase64(key)?, unbase64(value)?)))
                    .collect();
                match decoded {
                    Some(records) => {
                        // An empty value deletes the key.
                        for (key, value) in records {
                            if value.is_empty() {
                                self.kt.remove(&key);
                            } else {
                                self.kt.insert(key, value);
                            }
                        }
                        Response::Done
                    }
                    None => Response::Rejected("malformed record".into()),
                }
            }
            history => self.history.apply(history),
        }
    }
}
