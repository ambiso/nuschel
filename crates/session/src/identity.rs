// SPDX-License-Identifier: EUPL-1.2
//! Identity and key transparency for a session (docs/e2ee.md §3–§5).
//!
//! The crypto worker owns every secret and decision; this module moves bytes
//! between it and the server:
//!
//! - [`CryptoHandle`] supervises the sandboxed worker (one per profile, shared by
//!   every connection) and matches its answers to requests.
//! - [`drive`] runs per connection to a `kt/1` server, after login: it checks the
//!   server's head (auditing every newer one, in steps when an audit is too large),
//!   publishes our binding, monitors it, checks the contacts the user interacts with,
//!   and repeats the schedule of §5.4 every day. It answers [`IdentityCommand`]s from
//!   the frontend with [`IdentityEvent`]s.
use crate::Error;
use nuschl_ipc::{
    Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    crypto::{
        ContactView, CryptoRequest, CryptoResponse, CryptoServer, HeadOutcome, IdentityView,
        KeyTransparencySettings, KtRecordBytes, MonitorOutcome, RecoveryPolicy,
    },
    write_frame,
};
use nuschl_protocol::nuschl::{
    kind,
    v1::{self, error::Code},
};
use prost::Message as _;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot},
    time::timeout,
};

/// Request ids of the key-transparency driver (never used by the frontend).
pub(crate) const KT_REQUEST: u64 = 1 << 62;
/// How long one worker request may take (audits of large spans included).
const WORKER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the server may take to answer a key-transparency request.
const SERVER_TIMEOUT: Duration = Duration::from_secs(30);
/// Audits after which the driver gives up for this round.
const MAX_AUDIT_STEPS: usize = 64;

// ---- the worker ----

type Spawn = Box<dyn Fn() -> std::io::Result<Command> + Send + Sync>;

struct Job {
    request: CryptoRequest,
    reply: oneshot::Sender<Result<CryptoResponse, Error>>,
}

/// The crypto worker, shared by every connection of a profile. Cloning is cheap;
/// the worker is started on first use and again after it fails.
#[derive(Clone)]
pub struct CryptoHandle {
    jobs: mpsc::Sender<Job>,
}

struct Running {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    _containment: nuschl_ipc::host::Containment,
}

impl CryptoHandle {
    /// The worker in its sandbox (`nuschl_ipc::host::crypto_sandbox_command`), on
    /// `directory`, which is created owner-only.
    pub fn start(executable: PathBuf, directory: PathBuf) -> Self {
        Self::start_with(Box::new(move || {
            create_private_directory(&directory)?;
            nuschl_ipc::host::crypto_sandbox_command(&executable, &directory)
                .map_err(|error| std::io::Error::other(error.to_string()))
        }))
    }

    /// The worker from a custom command (tests, platforms without a sandbox).
    pub fn start_with(spawn: Spawn) -> Self {
        let (jobs, receiver) = mpsc::channel(64);
        tokio::spawn(supervise(Arc::new(spawn), receiver));
        Self { jobs }
    }

    pub async fn request(&self, request: CryptoRequest) -> Result<CryptoResponse, Error> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job { request, reply })
            .await
            .map_err(|_| Error::Worker("the crypto worker is gone".into()))?;
        match answer.await {
            Ok(Ok(CryptoResponse::Rejected(reason))) => Err(Error::Worker(reason)),
            Ok(result) => result,
            Err(_) => Err(Error::Worker("the crypto worker is gone".into())),
        }
    }
}

fn create_private_directory(directory: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)
}

async fn supervise(spawn: Arc<Spawn>, mut jobs: mpsc::Receiver<Job>) {
    let mut running: Option<Running> = None;
    let mut next_id = 0_u64;
    while let Some(job) = jobs.recv().await {
        if running.is_none() {
            match start(&spawn) {
                Ok(worker) => running = Some(worker),
                Err(error) => {
                    let _ = job.reply.send(Err(error));
                    continue;
                }
            }
        }
        let Some(worker) = running.as_mut() else {
            continue;
        };
        next_id += 1;
        let result = match timeout(WORKER_TIMEOUT, exchange(worker, next_id, &job.request)).await {
            Ok(result) => result,
            Err(_) => Err(Error::Timeout("waiting for the crypto worker")),
        };
        if result.is_err()
            && let Some(mut worker) = running.take()
        {
            let _ = worker.child.kill().await;
        }
        let _ = job.reply.send(result);
    }
    if let Some(mut worker) = running {
        let _ = worker.child.kill().await;
    }
}

fn start(spawn: &Spawn) -> Result<Running, Error> {
    let mut child = spawn()?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let containment =
        nuschl_ipc::host::contain(&child).map_err(|e| Error::Worker(e.to_string()))?;
    let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(Error::Worker("crypto worker pipes missing".into()));
    };
    Ok(Running {
        child,
        input,
        output,
        _containment: containment,
    })
}

async fn exchange(
    worker: &mut Running,
    id: u64,
    request: &CryptoRequest,
) -> Result<CryptoResponse, Error> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &Envelope::new(id, request))
        .map_err(|e| Error::Worker(e.to_string()))?;
    worker.input.write_all(&bytes).await?;
    worker.input.flush().await?;
    let size = worker.output.read_u32().await? as usize;
    if size > MAX_FRAME_BYTES {
        return Err(Error::Worker("oversized crypto worker response".into()));
    }
    let mut body = vec![0; size];
    worker.output.read_exact(&mut body).await?;
    let envelope: Envelope<CryptoResponse> =
        serde_json::from_slice(&body).map_err(|e| Error::Worker(e.to_string()))?;
    if envelope.version != PROTOCOL_VERSION || envelope.request_id != id {
        return Err(Error::Worker("crypto worker protocol mismatch".into()));
    }
    Ok(envelope.payload)
}

// ---- the driver ----

/// What a connection needs for key transparency.
#[derive(Clone)]
pub struct IdentitySetup {
    pub crypto: CryptoHandle,
    /// This device's name in its certificate, if the identity is created now.
    pub device_name: String,
    pub recovery: RecoveryPolicy,
    /// Required key-transparency witnesses (docs/e2ee.md §5.9).
    pub witnessing: KeyTransparencySettings,
}

/// What the frontend asks about identities on this server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityCommand {
    /// Registration changed: publish and start MLS on this connection promptly.
    Refresh,
    /// Look up and check a registered user: when their profile opens (`direct:
    /// false`) or a direct conversation with them does (`direct: true`).
    Check {
        user_id: Option<u32>,
        account: Option<[u8; 16]>,
        direct: bool,
        /// The name the server shows for them (not a local nickname): on a
        /// `kt-witness/1` server, its `name:` label must name the same account.
        name: Option<String>,
    },
    /// The user compared safety numbers out of band.
    MarkVerified {
        account: [u8; 16],
    },
    UnmarkVerified {
        account: [u8; 16],
    },
    /// The user accepted a contact's new identity.
    AcceptKeyChange {
        account: [u8; 16],
    },
    /// Our own identity (to show it, or the recovery key that waits).
    Own,
    ShowRecoveryKey,
    RecoveryKeySaved,
    SkipRecoveryKey,
    AcknowledgeAlarm,
}

/// What the driver reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityEvent {
    /// Our identity; `created` once, when it was made for this server.
    Own(IdentityView),
    /// The recovery key made at creation, to show once.
    RecoveryKey(String),
    /// The server's log: an accepted head, a failure (retried), misbehaviour.
    Log(HeadOutcome),
    /// Our binding on this server: published, and what self-monitoring found.
    Published {
        current: bool,
    },
    Monitor(MonitorOutcome),
    /// A contact's state (`user_id` when the check named one).
    Contact {
        user_id: Option<u32>,
        view: ContactView,
    },
    /// A request failed (the worker or the server refused); shown as text.
    Failed(String),
}

/// What [`drive`] needs from the connection.
pub struct Driver {
    pub crypto: CryptoHandle,
    pub server: CryptoServer,
    /// The log descriptor from the server's hello.
    pub descriptor: Vec<u8>,
    pub device_name: String,
    pub recovery: RecoveryPolicy,
    /// Frames to send (`kind`, encoded body), on the connection's writer.
    pub frames: mpsc::Sender<(u16, Vec<u8>)>,
    /// Replies to the driver's requests (`KT_REQUEST` ids), from the reader.
    pub replies: mpsc::Receiver<(u16, bytes::Bytes)>,
    pub commands: mpsc::Receiver<IdentityCommand>,
    pub events: mpsc::Sender<crate::connection::SessionEvent>,
    /// With `mls/1`: end-to-end conversations (crate::mls).
    pub mls: Option<MlsLink>,
    /// `kt-witness/1` was negotiated (docs/e2ee.md §5.9).
    pub witness: bool,
    pub witnessing: KeyTransparencySettings,
}

/// The `mls/1` side of the driver: frontend commands and server pushes (Welcomes,
/// and rows of end-to-end conversations).
pub struct MlsLink {
    pub commands: mpsc::Receiver<crate::mls::MlsCommand>,
    pub pushes: mpsc::Receiver<(u16, bytes::Bytes)>,
}

pub(crate) struct Context {
    pub(crate) crypto: CryptoHandle,
    pub(crate) server: CryptoServer,
    pub(crate) device_name: String,
    pub(crate) recovery: RecoveryPolicy,
    frames: mpsc::Sender<(u16, Vec<u8>)>,
    replies: mpsc::Receiver<(u16, bytes::Bytes)>,
    pub(crate) events: mpsc::Sender<crate::connection::SessionEvent>,
    next_request: u64,
    pub(crate) account: Vec<u8>,
    descriptor: Vec<u8>,
    mls_enabled: bool,
    /// Self-monitoring found our newest binding not published yet.
    publication_pending: bool,
    /// Our identity exists (created on first need).
    identity_ready: bool,
    /// End-to-end conversations, on an end-to-end server (`e2ee/1`).
    pub(crate) mls: Option<crate::mls::MlsState>,
    /// `kt-witness/1`: replies carry witnessed checkpoints, and our number and name
    /// labels are monitored.
    witness: bool,
    witnessing: KeyTransparencySettings,
    /// Our registration number and name label, from the status reply.
    registration: Option<(u32, String)>,
}

/// A server reply, or its error.
pub(crate) enum Reply {
    Frame(u16, bytes::Bytes),
    Error(v1::Error),
}

impl Context {
    pub(crate) async fn emit(&self, event: IdentityEvent) {
        let _ = self
            .events
            .send(crate::connection::SessionEvent::Identity(event))
            .await;
    }

    async fn worker(&self, request: CryptoRequest) -> Result<CryptoResponse, Error> {
        self.crypto.request(request).await
    }

    /// Sends a request and waits for its reply; replies to earlier, abandoned
    /// requests are dropped.
    pub(crate) async fn ask(
        &mut self,
        kind: u16,
        build: impl FnOnce(u64) -> Vec<u8>,
    ) -> Result<Reply, Error> {
        self.next_request += 1;
        let id = KT_REQUEST | self.next_request;
        self.frames
            .send((kind, build(id)))
            .await
            .map_err(|_| Error::Protocol("the connection is closed".into()))?;
        let deadline = tokio::time::Instant::now() + SERVER_TIMEOUT;
        loop {
            let (kind, payload) = tokio::time::timeout_at(deadline, self.replies.recv())
                .await
                .map_err(|_| Error::Timeout("waiting for the key-transparency log"))?
                .ok_or_else(|| Error::Protocol("the connection is closed".into()))?;
            let request_id = v1::Done::decode(payload.clone()).map_or(0, |done| done.request_id);
            if request_id != id {
                continue;
            }
            if kind == kind::ERROR {
                let error = v1::Error::decode(payload)
                    .map_err(|_| Error::Protocol("invalid Nuschl error".into()))?;
                return Ok(Reply::Error(error));
            }
            return Ok(Reply::Frame(kind, payload));
        }
    }

    /// Sends a message that has no answer.
    pub(crate) async fn tell(&self, kind: u16, payload: Vec<u8>) -> Result<(), Error> {
        self.frames
            .send((kind, payload))
            .await
            .map_err(|_| Error::Protocol("the connection is closed".into()))
    }

    async fn head_at(&mut self, epoch: u64) -> Result<Vec<u8>, Error> {
        match self
            .ask(kind::KT_HEAD, |request_id| {
                v1::KtHead { request_id, epoch }.encode_to_vec()
            })
            .await?
        {
            Reply::Frame(kind::KT_HEAD_REPLY, payload) => Ok(v1::KtHeadReply::decode(payload)
                .map_err(|_| Error::Protocol("invalid head reply".into()))?
                .head),
            _ => Err(Error::Protocol("the server gave no head".into())),
        }
    }

    /// Audits from the latest accepted head to the target the worker remembers,
    /// halving spans the server finds too large.
    async fn audit(&mut self, mut from: u64, to: u64) -> Result<HeadOutcome, Error> {
        let mut end = to;
        for _ in 0..MAX_AUDIT_STEPS {
            let reply = self
                .ask(kind::KT_AUDIT, |request_id| {
                    v1::KtAudit {
                        request_id,
                        from_epoch: from,
                        to_epoch: end,
                    }
                    .encode_to_vec()
                })
                .await?;
            let audit = match reply {
                Reply::Frame(kind::KT_AUDIT_REPLY, payload) => {
                    v1::KtAuditReply::decode(payload)
                        .map_err(|_| Error::Protocol("invalid audit reply".into()))?
                        .response
                }
                Reply::Error(error) if error.code == Code::TooLarge as i32 && end > from + 1 => {
                    end = from + (end - from) / 2;
                    continue;
                }
                Reply::Error(error) => {
                    return Ok(HeadOutcome::Failed {
                        reason: format!("the server cannot prove its log: {}", error.detail),
                    });
                }
                Reply::Frame(..) => return Err(Error::Protocol("unexpected reply".into())),
            };
            let head = self.head_at(end).await?;
            match self
                .worker(CryptoRequest::Advance {
                    server: self.server.key.clone(),
                    head,
                    audit,
                })
                .await?
            {
                CryptoResponse::Head(HeadOutcome::NeedAudit {
                    from: next,
                    to: target,
                }) => {
                    from = next;
                    end = target;
                }
                CryptoResponse::Head(outcome) => return Ok(outcome),
                _ => return Err(Error::Worker("unexpected worker answer".into())),
            }
        }
        Ok(HeadOutcome::Failed {
            reason: "too many audit steps".into(),
        })
    }

    /// A worker request that may need an audit first (its head was newer).
    async fn checked(&mut self, request: CryptoRequest) -> Result<CryptoResponse, Error> {
        for _ in 0..2 {
            match self.worker(request.clone()).await? {
                CryptoResponse::NeedAudit { from, to } => {
                    let outcome = self.audit(from, to).await?;
                    if !matches!(outcome, HeadOutcome::Accepted { .. }) {
                        self.emit(IdentityEvent::Log(outcome.clone())).await;
                        return Ok(CryptoResponse::Head(outcome));
                    }
                }
                response => return Ok(response),
            }
        }
        Err(Error::Worker("the log kept moving".into()))
    }

    /// What a reply's `witnessed` field means to the worker: `None` without
    /// `kt-witness/1`.
    fn witnessed(&self, bytes: Vec<u8>) -> Option<Vec<u8>> {
        self.witness.then_some(bytes)
    }

    /// The origin to name in requests: the server as we reach it.
    fn origin(&self) -> String {
        if self.witness {
            self.server.key.clone()
        } else {
            String::new()
        }
    }

    async fn connect(&mut self, descriptor: Vec<u8>) -> Result<bool, Error> {
        let origin = self.origin();
        let status = match self
            .ask(kind::KT_STATUS, |request_id| {
                v1::KtStatus { request_id, origin }.encode_to_vec()
            })
            .await?
        {
            Reply::Frame(kind::KT_STATUS_REPLY, payload) => v1::KtStatusReply::decode(payload)
                .map_err(|_| Error::Protocol("invalid status".into()))?,
            _ => return Err(Error::Protocol("the server gave no status".into())),
        };
        self.account = status.account.clone();
        self.registration = match (self.witness, status.user_id) {
            (true, Some(user_id)) if status.account.len() == 16 => Some((user_id, status.name)),
            _ => None,
        };
        let mut outcome = match self
            .worker(CryptoRequest::Connect {
                server: self.server.clone(),
                descriptor,
                account: status.account,
                head: status.head,
                witnessed: self.witnessed(status.witnessed),
                witnessing: self.witnessing.clone(),
                registration: self.registration.clone(),
            })
            .await?
        {
            CryptoResponse::Head(outcome) => outcome,
            _ => return Err(Error::Worker("unexpected worker answer".into())),
        };
        if let HeadOutcome::NeedAudit { from, to } = outcome {
            outcome = self.audit(from, to).await?;
        }
        let accepted = matches!(outcome, HeadOutcome::Accepted { .. });
        self.emit(IdentityEvent::Log(outcome)).await;
        Ok(accepted)
    }

    async fn publish(&mut self) -> Result<(), Error> {
        let bundle = match self
            .worker(CryptoRequest::PublishBundle {
                server: self.server.key.clone(),
                device_name: self.device_name.clone(),
                recovery: self.recovery,
            })
            .await?
        {
            CryptoResponse::Bundle(bundle) => bundle,
            _ => return Err(Error::Worker("unexpected worker answer".into())),
        };
        if bundle.created
            && let Ok(CryptoResponse::Identity(view)) = self.worker(CryptoRequest::Identity).await
        {
            self.emit(IdentityEvent::Own(IdentityView {
                created: true,
                ..view
            }))
            .await;
        }
        let record = record_message(&bundle.record);
        let root = bundle.root;
        match self
            .ask(kind::KT_PUBLISH, move |request_id| {
                v1::KtPublish {
                    request_id,
                    root,
                    record: Some(record),
                }
                .encode_to_vec()
            })
            .await?
        {
            Reply::Frame(kind::KT_PUBLISHED, payload) => {
                let published = v1::KtPublished::decode(payload)
                    .map_err(|_| Error::Protocol("invalid publish reply".into()))?;
                self.worker(CryptoRequest::Published {
                    server: self.server.key.clone(),
                    value: published.value,
                    current: published.current,
                })
                .await?;
                self.publication_pending = !published.current;
                self.emit(IdentityEvent::Published {
                    current: published.current,
                })
                .await;
            }
            Reply::Error(error) => {
                self.emit(IdentityEvent::Failed(format!(
                    "The server did not accept your identity: {}",
                    error.detail
                )))
                .await;
            }
            Reply::Frame(..) => return Err(Error::Protocol("unexpected reply".into())),
        }
        Ok(())
    }

    /// A history and its witnessed checkpoint.
    async fn history(
        &mut self,
        account: Vec<u8>,
    ) -> Result<Result<(Vec<u8>, Option<Vec<u8>>), v1::Error>, Error> {
        self.label_history(account, v1::KtLabel::Account).await
    }

    async fn label_history(
        &mut self,
        account: Vec<u8>,
        label: v1::KtLabel,
    ) -> Result<Result<(Vec<u8>, Option<Vec<u8>>), v1::Error>, Error> {
        let origin = self.origin();
        match self
            .ask(kind::KT_HISTORY, move |request_id| {
                v1::KtHistory {
                    request_id,
                    account,
                    origin,
                    label: label as i32,
                }
                .encode_to_vec()
            })
            .await?
        {
            Reply::Frame(kind::KT_HISTORY_REPLY, payload) => {
                let reply = v1::KtHistoryReply::decode(payload)
                    .map_err(|_| Error::Protocol("invalid history reply".into()))?;
                Ok(Ok((reply.response, self.witnessed(reply.witnessed))))
            }
            Reply::Error(error) => Ok(Err(error)),
            Reply::Frame(..) => Err(Error::Protocol("unexpected reply".into())),
        }
    }

    /// Looks up our own account and hands it to the worker, which learns our other
    /// devices from it; nothing is shown (a failure, such as nothing published yet,
    /// is the monitor's to report).
    async fn own_lookup(&mut self) -> Result<(), Error> {
        let Ok(account) = <[u8; 16]>::try_from(self.account.as_slice()) else {
            return Ok(());
        };
        let origin = self.origin();
        let reply = self
            .ask(kind::KT_LOOKUP, move |request_id| {
                v1::KtLookup {
                    request_id,
                    target: Some(v1::kt_lookup::Target::Account(account.to_vec())),
                    origin,
                }
                .encode_to_vec()
            })
            .await?;
        if let Reply::Frame(kind::KT_LOOKUP_REPLY, payload) = reply
            && let Ok(reply) = v1::KtLookupReply::decode(payload)
            && reply.account == account
        {
            let witnessed = self.witnessed(reply.witnessed);
            let _ = self
                .checked(CryptoRequest::Contact {
                    server: self.server.key.clone(),
                    account: account.to_vec(),
                    lookup: reply.response,
                    record: reply.record.map(record_bytes),
                    direct: false,
                    user_id: None,
                    witnessed,
                    registration: Vec::new(),
                    name_lookup: Vec::new(),
                    name: None,
                })
                .await;
        }
        Ok(())
    }

    async fn monitor(&mut self) -> Result<(), Error> {
        if self.account.len() != 16 {
            return Ok(());
        }
        let (history, witnessed) = match self.history(self.account.clone()).await? {
            Ok(history) => history,
            // Nothing published yet: the publication is still waiting for its epoch.
            Err(error) if error.code == Code::NotFound as i32 => return Ok(()),
            Err(error) => {
                self.emit(IdentityEvent::Monitor(MonitorOutcome::Failed {
                    reason: error.detail,
                }))
                .await;
                return Ok(());
            }
        };
        if let CryptoResponse::Monitor(outcome) = self
            .checked(CryptoRequest::Monitor {
                server: self.server.key.clone(),
                history,
                witnessed,
            })
            .await?
        {
            if let MonitorOutcome::Consistent { current, .. } = outcome {
                self.publication_pending = !current;
            }
            self.emit(IdentityEvent::Monitor(outcome)).await;
        }
        self.monitor_labels().await
    }

    /// Our registration number and name in the log (`kt-witness/1`): they must name
    /// our account from the first check on. Only problems are reported.
    async fn monitor_labels(&mut self) -> Result<(), Error> {
        let Some((_, name)) = self.registration.clone() else {
            return Ok(());
        };
        for (label, name) in [
            (v1::KtLabel::Registration, None),
            (v1::KtLabel::Name, Some(name)),
        ] {
            let (history, witnessed) = match self.label_history(self.account.clone(), label).await?
            {
                Ok(history) => history,
                // Not published yet (it waits for the next epoch).
                Err(_) => continue,
            };
            if let CryptoResponse::Monitor(outcome) = self
                .checked(CryptoRequest::MonitorLabel {
                    server: self.server.key.clone(),
                    name,
                    history,
                    witnessed,
                })
                .await?
                && !matches!(outcome, MonitorOutcome::Consistent { .. })
            {
                self.emit(IdentityEvent::Monitor(outcome)).await;
            }
        }
        Ok(())
    }

    pub(crate) async fn check_contact(
        &mut self,
        user_id: Option<u32>,
        account: Option<[u8; 16]>,
        direct: bool,
        name: Option<String>,
    ) -> Result<(), Error> {
        // A safety number needs our identity: made on first need (also for guests,
        // who publish nothing).
        if !self.identity_ready
            && let Ok(CryptoResponse::Identity(view)) = self
                .worker(CryptoRequest::CreateIdentity {
                    device_name: self.device_name.clone(),
                    recovery: self.recovery,
                })
                .await
        {
            if view.created {
                self.emit(IdentityEvent::Own(view)).await;
            }
            self.identity_ready = true;
        }
        let account_given = account;
        let target = match (account, user_id) {
            (Some(account), _) => v1::kt_lookup::Target::Account(account.to_vec()),
            (None, Some(user_id)) => v1::kt_lookup::Target::UserId(user_id),
            (None, None) => return Ok(()),
        };
        let origin = self.origin();
        let reply = self
            .ask(kind::KT_LOOKUP, move |request_id| {
                v1::KtLookup {
                    request_id,
                    target: Some(target),
                    origin,
                }
                .encode_to_vec()
            })
            .await?;
        let response = match reply {
            Reply::Frame(kind::KT_LOOKUP_REPLY, payload) => {
                let reply = v1::KtLookupReply::decode(payload)
                    .map_err(|_| Error::Protocol("invalid lookup reply".into()))?;
                let account = reply.account.clone();
                let witnessed = self.witnessed(reply.witnessed);
                self.checked(CryptoRequest::Contact {
                    server: self.server.key.clone(),
                    account: account.clone(),
                    lookup: reply.response,
                    record: reply.record.map(record_bytes),
                    direct,
                    user_id: account_given.is_none().then_some(user_id).flatten(),
                    witnessed,
                    registration: reply.registration,
                    name_lookup: reply.name_lookup,
                    name,
                })
                .await?
            }
            Reply::Error(error) => match account {
                Some(account) => {
                    self.worker(CryptoRequest::ContactUnavailable {
                        server: self.server.key.clone(),
                        account: account.to_vec(),
                        not_found: error.code == Code::NotFound as i32,
                        reason: error.detail,
                    })
                    .await?
                }
                None => {
                    // An unregistered user, or one without an identity on this server.
                    self.emit(IdentityEvent::Failed(error.detail)).await;
                    return Ok(());
                }
            },
            Reply::Frame(..) => return Err(Error::Protocol("unexpected reply".into())),
        };
        if let CryptoResponse::Contact(view) = response {
            let check_history = direct
                || matches!(
                    view.state,
                    nuschl_ipc::crypto::ContactState::Verified { .. }
                );
            let account = view.account.clone();
            self.emit(IdentityEvent::Contact { user_id, view }).await;
            if check_history {
                self.contact_history(user_id, account).await?;
            }
        }
        Ok(())
    }

    async fn contact_history(
        &mut self,
        user_id: Option<u32>,
        account: Vec<u8>,
    ) -> Result<(), Error> {
        let (history, witnessed) = match self.history(account.clone()).await? {
            Ok(history) => history,
            // Not a contact in the server's eyes (or nothing published): the lookup stands.
            Err(_) => return Ok(()),
        };
        if let CryptoResponse::Contact(view) = self
            .checked(CryptoRequest::ContactHistory {
                server: self.server.key.clone(),
                account,
                history,
                witnessed,
            })
            .await?
        {
            self.emit(IdentityEvent::Contact { user_id, view }).await;
        }
        Ok(())
    }

    /// The §5.4 schedule: on connect (`connected`) and every day.
    async fn run_due(&mut self, connected: bool) -> Result<(), Error> {
        let due = match self
            .worker(CryptoRequest::Due {
                server: self.server.key.clone(),
                connected,
            })
            .await?
        {
            CryptoResponse::Due(due) => due,
            _ => return Err(Error::Worker("unexpected worker answer".into())),
        };
        if due.monitor || due.publish {
            // Our own account first: the identity's other devices and the binding they
            // published (self-monitoring accepts it, and a publication keeps them).
            self.own_lookup().await?;
        }
        if due.publish {
            self.publish().await?;
        }
        if due.monitor {
            self.monitor().await?;
        }
        for account in due.histories {
            self.contact_history(None, account).await?;
        }
        Ok(())
    }

    async fn command(&mut self, command: IdentityCommand) -> Result<(), Error> {
        let server = self.server.key.clone();
        let response = match command {
            IdentityCommand::Refresh => {
                if self.connect(self.descriptor.clone()).await? {
                    self.run_due(true).await?;
                    if self.mls_enabled {
                        self.mls_start().await?;
                    }
                }
                return Ok(());
            }
            IdentityCommand::Check {
                user_id,
                account,
                direct,
                name,
            } => return self.check_contact(user_id, account, direct, name).await,
            IdentityCommand::MarkVerified { account } => {
                self.worker(CryptoRequest::MarkVerified {
                    server,
                    account: account.to_vec(),
                })
                .await
            }
            IdentityCommand::UnmarkVerified { account } => {
                self.worker(CryptoRequest::UnmarkVerified {
                    server,
                    account: account.to_vec(),
                })
                .await
            }
            IdentityCommand::AcceptKeyChange { account } => {
                self.worker(CryptoRequest::AcceptKeyChange {
                    server,
                    account: account.to_vec(),
                })
                .await
            }
            IdentityCommand::Own => self.worker(CryptoRequest::Identity).await,
            IdentityCommand::ShowRecoveryKey => self.worker(CryptoRequest::ShowRecoveryKey).await,
            IdentityCommand::RecoveryKeySaved => self.worker(CryptoRequest::RecoveryKeySaved).await,
            IdentityCommand::SkipRecoveryKey => self.worker(CryptoRequest::SkipRecoveryKey).await,
            IdentityCommand::AcknowledgeAlarm => {
                self.worker(CryptoRequest::AcknowledgeAlarm { server })
                    .await
            }
        };
        match response {
            Ok(CryptoResponse::Contact(view)) => {
                self.emit(IdentityEvent::Contact {
                    user_id: None,
                    view,
                })
                .await
            }
            Ok(CryptoResponse::Identity(view)) => self.emit(IdentityEvent::Own(view)).await,
            Ok(CryptoResponse::RecoveryKey(key)) => {
                self.emit(IdentityEvent::RecoveryKey(key)).await
            }
            Ok(_) => {}
            Err(error) => self.emit(IdentityEvent::Failed(error.to_string())).await,
        }
        Ok(())
    }
}

fn record_message(record: &KtRecordBytes) -> v1::KtRecord {
    v1::KtRecord {
        device_signing_certificate: record.device_signing_certificate.clone(),
        devices: record.devices.clone(),
        revocations: record.revocations.clone(),
        statement: record.statement.clone(),
    }
}

fn record_bytes(record: v1::KtRecord) -> KtRecordBytes {
    KtRecordBytes {
        device_signing_certificate: record.device_signing_certificate,
        devices: record.devices,
        revocations: record.revocations,
        statement: record.statement,
    }
}

/// Runs for the life of a `kt/1` connection (see the module documentation).
/// Failures are reported as events; only a closed connection ends it.
pub async fn drive(driver: Driver) {
    let Driver {
        crypto,
        server,
        descriptor,
        device_name,
        recovery,
        frames,
        replies,
        mut commands,
        events,
        mls,
        witness,
        witnessing,
    } = driver;
    let (mut mls_commands, mut pushes) = match mls {
        Some(link) => (Some(link.commands), Some(link.pushes)),
        None => (None, None),
    };
    let mut context = Context {
        crypto,
        server,
        device_name,
        recovery,
        frames,
        replies,
        events,
        next_request: 0,
        account: Vec::new(),
        descriptor: descriptor.clone(),
        mls_enabled: mls_commands.is_some(),
        publication_pending: false,
        identity_ready: false,
        mls: None,
        witness,
        witnessing,
        registration: None,
    };
    let healthy = match context.connect(descriptor.clone()).await {
        Ok(accepted) => accepted,
        Err(error) => {
            context
                .emit(IdentityEvent::Failed(format!(
                    "Key transparency is unavailable: {error}"
                )))
                .await;
            false
        }
    };
    if healthy && let Err(error) = context.run_due(true).await {
        context.emit(IdentityEvent::Failed(error.to_string())).await;
    }
    // End-to-end conversations need the log: without an accepted head, the worker
    // cannot check anyone's credential, so MLS waits for the next hourly round.
    if mls_commands.is_some()
        && healthy
        && let Err(error) = context.mls_start().await
    {
        context
            .emit(IdentityEvent::Failed(format!(
                "End-to-end encryption: {error}"
            )))
            .await;
    }
    // While our binding waits for its epoch (about ten seconds), look again after
    // 2, 4, 8, … seconds; then hourly.
    let mut rechecks = 0_u32;
    let recheck_at =
        |rechecks: u32| tokio::time::Instant::now() + Duration::from_secs(2_u64 << rechecks.min(5));
    let recheck = tokio::time::sleep_until(recheck_at(0));
    tokio::pin!(recheck);
    let mut daily = tokio::time::interval(Duration::from_secs(3600));
    daily.tick().await;
    let mut history_retry = tokio::time::interval(Duration::from_secs(5));
    history_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                if let Err(error) = context.command(command).await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                    if matches!(error, Error::Protocol(_)) {
                        break;
                    }
                }
            }
            command = async {
                match mls_commands.as_mut() {
                    Some(commands) => commands.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(command) = command else { break };
                if let Err(error) = context.mls_command(command).await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                    if matches!(error, Error::Protocol(_)) {
                        break;
                    }
                }
            }
            push = async {
                match pushes.as_mut() {
                    Some(pushes) => pushes.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some((kind, payload)) = push else { break };
                if let Err(error) = context.mls_push(kind, payload).await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                    if matches!(error, Error::Protocol(_)) {
                        break;
                    }
                }
            }
            _ = async {
                match context.mls.as_ref().and_then(crate::mls::MlsState::upload_due) {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Err(error) = context.mls_upload().await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                }
                // Welcomes may have waited for our first key packages.
                if let Err(error) = context.mls_fetch_welcomes().await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                }
            }
            _ = &mut recheck, if context.publication_pending && rechecks < 8 => {
                rechecks += 1;
                recheck.as_mut().reset(recheck_at(rechecks));
                if let Err(error) = context.monitor().await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                }
            }
            _ = history_retry.tick(), if mls_commands.is_some() => {
                if let Err(error) = context.mls_history_retry().await {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                }
            }
            _ = daily.tick() => {
                // A fresh head every hour (it must stay younger than 25 hours), then
                // whatever the daily schedule asks for.
                let result = match context.connect(descriptor.clone()).await {
                    Ok(true) => match context.run_due(false).await {
                        // End-to-end conversations start once the log is healthy.
                        Ok(()) if mls_commands.is_some() && context.mls.is_none() => {
                            context.mls_start().await
                        }
                        Ok(()) => {
                            if let Some(mls) = context.mls.as_mut() {
                                mls.hourly();
                            }
                            Ok(())
                        }
                        result => result,
                    },
                    Ok(false) => Ok(()),
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    context.emit(IdentityEvent::Failed(error.to_string())).await;
                }
            }
        }
    }
}
