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
        ServerSnapshot, ServerStorageRequest as Request, ServerStorageResponse as Response,
        StoredBan, StoredChannel, StoredUser, USER_PAGE,
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

    /// A handle whose writes go nowhere (fuzzing and unit tests of the state).
    pub fn detached() -> Self {
        let (jobs, _) = mpsc::unbounded_channel();
        Self { jobs }
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
    let (jobs, receiver) = mpsc::unbounded_channel();
    tokio::spawn(run(backend, receiver));
    StorageHandle { jobs }
}

async fn run(backend: Backend, mut jobs: mpsc::UnboundedReceiver<Job>) {
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
                        match Worker::spawn(executable, path, url) {
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
            tracing::error!(%error, "storage request was rejected; this change is not persisted");
        } else if response.is_none() {
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
    fn spawn(executable: &PathBuf, path: &PathBuf, url_file: bool) -> std::io::Result<Self> {
        let mut command = Command::new(executable);
        command
            .arg("--server")
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
    bans: Vec<StoredBan>,
    next_user_id: u32,
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
                Response::Done
            }
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
        }
    }
}
