// SPDX-License-Identifier: EUPL-1.2
//! The music bot's codec roles (docs/musicbot.md): the private protocol between
//! the credential-bearing `nuschl-musicbot` and its `nuschl-musicbot-codec-worker`
//! children, and the bot's supervisor for those children. No credentials, URLs
//! or paths cross it; the bot names the worker executable, never the worker.
//!
//! Decode receives one framed `Decode` request (id 1), followed by opaque raw
//! media bytes until EOF. Every Progress/PCM/Done/Error reply carries that same
//! id. Before its first PCM, the child reports work that produces no audio
//! (reading an MP4 file whole, skipping to a seek position) in exact `Progress`
//! steps; the parent checks them against what it actually sent and against
//! fixed limits (`DecodeLimits`), and never lets progress extend the total time.
//! Encode and Duck exchange one framed request/reply at a time, retaining
//! codec state; request ids are checked by the parent. All roles use the
//! existing IPC envelope/version and a stricter 64 KiB allocation bound.
use crate::{Envelope, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Notify, mpsc, oneshot},
    time::Instant,
};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME: usize = 960;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_OPUS_BYTES: usize = 992;
pub const MAX_VOICE_BYTES: usize = 1024;
pub const MAX_ERROR_BYTES: usize = 512;
/// An MP4 file is read whole before decoding; one `Progress` per MiB read.
pub const INPUT_PROGRESS_BYTES: u64 = 1024 * 1024;
/// One `Progress` per 250 skipped 20 ms frames (five seconds of song).
pub const SKIP_PROGRESS_FRAMES: u64 = 250;

/// The speech probability a frame of incoming voice needs, from a ducking
/// sensitivity in percent (the mapping of the client's voice activation).
pub fn speech_threshold(sensitivity: u8) -> f32 {
    0.9 - 0.6 * f32::from(sensitivity.min(100)) / 100.
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Decode,
    Encode,
    Duck,
}

impl Role {
    pub fn argument(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Encode => "encode",
            Self::Duck => "duck",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "decode" => Some(Self::Decode),
            "encode" => Some(Self::Encode),
            "duck" => Some(Self::Duck),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Decode {
        mime: Option<String>,
        extension: Option<String>,
        skip_frames: u64,
    },
    Encode {
        samples: Vec<f32>,
        bitrate: u32,
    },
    Hear {
        sender: u32,
        opus: Vec<u8>,
        threshold: f32,
    },
    Forget {
        sender: u32,
    },
    Clear,
}

/// Work before the first PCM that produces no audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Bytes of an MP4 file read, in whole `INPUT_PROGRESS_BYTES`.
    Input,
    /// Frames skipped towards the seek position, in whole `SKIP_PROGRESS_FRAMES`.
    Skip,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Progress { stage: Stage, units: u64 },
    Pcm { samples: Vec<f32> },
    Packet { opus: Vec<u8> },
    Speech { active: bool },
    Ack,
    Done,
    Error { message: String },
}

pub fn read_message<T: DeserializeOwned>(
    input: &mut impl Read,
) -> Result<Option<Envelope<T>>, String> {
    let mut header = [0; 4];
    match input.read_exact(&mut header[..1]) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        result => result.map_err(|_| "codec IPC read failed")?,
    }
    input
        .read_exact(&mut header[1..])
        .map_err(|_| "codec IPC header truncated")?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err("codec IPC frame is too large or empty".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|_| "codec IPC body truncated")?;
    let envelope: Envelope<T> =
        serde_json::from_slice(&bytes).map_err(|_| "invalid codec IPC message")?;
    if envelope.version != PROTOCOL_VERSION {
        return Err("unsupported codec IPC version".into());
    }
    Ok(Some(envelope))
}

pub fn write_message<T: Serialize>(
    output: &mut impl Write,
    envelope: &Envelope<T>,
) -> Result<(), String> {
    struct Capped(Vec<u8>);
    impl Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.len() + bytes.len() > MAX_MESSAGE_BYTES {
                return Err(io::Error::other("codec IPC frame exceeds its budget"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut bytes = Capped(Vec::new());
    serde_json::to_writer(&mut bytes, envelope).map_err(|_| "codec IPC serialization failed")?;
    output
        .write_all(&(bytes.0.len() as u32).to_be_bytes())
        .and_then(|()| output.write_all(&bytes.0))
        .and_then(|()| output.flush())
        .map_err(|_| "codec IPC write failed".into())
}

pub fn valid_pcm(samples: &[f32]) -> bool {
    samples.len() == FRAME * CHANNELS && samples.iter().all(|sample| sample.is_finite())
}

/// MP4 needs a seekable source, so the decode role reads such a file whole
/// first. The parent and the child decide this the same way.
pub fn needs_seeking(mime: Option<&str>, extension: Option<&str>) -> bool {
    let mp4_mime = mime.is_some_and(|mime| {
        matches!(
            mime,
            "audio/mp4" | "audio/x-m4a" | "audio/m4a" | "audio/aac" | "video/mp4"
        )
    });
    let mp4_extension = extension.is_some_and(|extension| {
        ["m4a", "m4b", "mp4", "alac"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    });
    mp4_mime || mp4_extension
}

/// What one decode may cost before its first PCM; `DecodeLimits::DEFAULT` in
/// production. After the first PCM only `inactivity` applies: a song has no
/// total-duration limit, and progress is then a protocol violation.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    /// The longest wait for any reply (progress, PCM or the end).
    pub inactivity: Duration,
    /// From starting the child to its first PCM or its end; never extended.
    pub before_audio: Duration,
    /// The furthest start position, in 20 ms frames.
    pub seek_frames: u64,
    /// The largest MP4 file, which is read whole before its first PCM.
    pub mp4_bytes: u64,
    /// The most other input read before the first PCM (a seek reads the
    /// skipped part of the song).
    pub seek_input_bytes: u64,
}

impl DecodeLimits {
    pub const DEFAULT: Self = Self {
        // Above the library's 30-second bound on one network read.
        inactivity: Duration::from_secs(35),
        before_audio: Duration::from_secs(300),
        // Three hours.
        seek_frames: 540_000,
        mp4_bytes: 512 * 1024 * 1024,
        seek_input_bytes: 2 * 1024 * 1024 * 1024,
    };
}

/// The parent's check of one decode child's replies before its first PCM:
/// exact, increasing steps of the stages the request allows, an input claim
/// never above what the parent sent, a bounded number of replies, and the
/// whole skip reported before any audio.
#[derive(Debug)]
pub struct ProgressCheck {
    mp4: bool,
    skip_frames: u64,
    input_limit: u64,
    input: u64,
    skipped: u64,
    replies: u64,
    audio: bool,
}

impl ProgressCheck {
    pub fn new(mp4: bool, skip_frames: u64, input_limit: u64) -> Self {
        Self {
            mp4,
            skip_frames,
            input_limit,
            input: 0,
            skipped: 0,
            replies: 0,
            audio: false,
        }
    }

    pub fn audio_started(&self) -> bool {
        self.audio
    }

    /// One `Progress` reply; `delivered` is how many media bytes the parent has
    /// handed to the child so far.
    pub fn progress(&mut self, stage: Stage, units: u64, delivered: u64) -> Result<(), String> {
        let invalid = || "music decoder reported invalid progress".to_owned();
        if self.audio {
            return Err(invalid());
        }
        let most = (self.input_limit / INPUT_PROGRESS_BYTES)
            .checked_add(self.skip_frames / SKIP_PROGRESS_FRAMES)
            .ok_or_else(invalid)?;
        self.replies = self.replies.checked_add(1).ok_or_else(invalid)?;
        if self.replies > most {
            return Err(invalid());
        }
        match stage {
            Stage::Input => {
                let next = self
                    .input
                    .checked_add(INPUT_PROGRESS_BYTES)
                    .ok_or_else(invalid)?;
                if !self.mp4
                    || self.skipped != 0
                    || units != next
                    || units > self.input_limit
                    || units > delivered
                {
                    return Err(invalid());
                }
                self.input = units;
            }
            Stage::Skip => {
                let next = self
                    .skipped
                    .checked_add(SKIP_PROGRESS_FRAMES)
                    .ok_or_else(invalid)?;
                if units != next || units > self.skip_frames {
                    return Err(invalid());
                }
                self.skipped = units;
            }
        }
        Ok(())
    }

    /// A PCM reply: only after every whole skip step was reported.
    pub fn pcm(&mut self) -> Result<(), String> {
        if !self.audio {
            let whole = self.skip_frames / SKIP_PROGRESS_FRAMES * SKIP_PROGRESS_FRAMES;
            if self.skipped != whole {
                return Err("music decoder played before reaching the seek position".into());
            }
            self.audio = true;
        }
        Ok(())
    }
}

/// The worker in a Nix-style installation: `<prefix>/bin/<bot>` finds
/// `<prefix>/libexec/nuschl-musicbot/nuschl-musicbot-codec-worker`. Nothing
/// from the environment, configuration or a child selects another program.
pub fn worker_path(bot: &Path) -> Option<PathBuf> {
    bot.parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("libexec/nuschl-musicbot/nuschl-musicbot-codec-worker"))
}

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
/// A caller stops waiting for retirement independently of the OS operation.
/// Expiry retains the original child and containment; it never frees a slot.
const RETIREMENT_GRACE: Duration = Duration::from_secs(2);

/// The credential-bearing parent's end of one confined codec child: only
/// bounded IPC and opaque transport.
pub struct Process {
    pub child: Child,
    pub input: Option<ChildStdin>,
    pub output: Option<ChildStdout>,
    _containment: crate::host::Containment,
    #[cfg(test)]
    retirement_gate: Option<Arc<Notify>>,
}

impl Process {
    /// A kill request is not a released slot: callers retain this object until
    /// the OS has confirmed exit. Errors leave ownership with the caller.
    pub async fn retire(&mut self) -> Result<(), String> {
        #[cfg(test)]
        if let Some(gate) = &self.retirement_gate {
            gate.notified().await;
        }
        if self
            .child
            .try_wait()
            .map_err(|_| "cannot inspect codec worker")?
            .is_none()
        {
            self.child
                .kill()
                .await
                .map_err(|_| "cannot stop codec worker")?;
        }
        self.child
            .wait()
            .await
            .map_err(|_| "cannot reap codec worker")?;
        self.input.take();
        self.output.take();
        Ok(())
    }
}

/// Start `executable` (the trusted caller's fixed worker path) in one role,
/// confined, with only its standard input and output.
pub async fn spawn(executable: &Path, role: Role) -> Result<Process, String> {
    if !executable.is_file() {
        return Err("the music codec worker is not installed".into());
    }
    let arguments = [
        OsString::from("--codec-worker"),
        OsString::from(role.argument()),
    ];
    let mut command = crate::host::musicbot_codec_sandbox_command(executable, &arguments)
        .map_err(|_| "secure music codec sandbox is unavailable")?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "cannot start isolated music codec worker")?;
    let containment = match crate::host::contain(&child) {
        Ok(containment) => containment,
        Err(_) => {
            child
                .kill()
                .await
                .map_err(|_| "cannot stop uncontained codec worker")?;
            return Err("cannot contain music codec worker".into());
        }
    };
    let input = child.stdin.take();
    let output = child.stdout.take();
    let mut process = Process {
        child,
        input,
        output,
        _containment: containment,
        #[cfg(test)]
        retirement_gate: None,
    };
    if process.input.is_none() || process.output.is_none() {
        process.retire().await?;
        return Err("codec worker pipes are unavailable".into());
    }
    Ok(process)
}

pub async fn write_request(
    input: &mut ChildStdin,
    id: u64,
    request: &Request,
) -> Result<(), String> {
    let mut bytes = Vec::new();
    write_message(&mut bytes, &Envelope::new(id, request))?;
    input
        .write_all(&bytes)
        .await
        .map_err(|_| "codec request write failed")?;
    input
        .flush()
        .await
        .map_err(|_| "codec request flush failed".into())
}

pub async fn read_response(output: &mut ChildStdout, id: u64) -> Result<Response, String> {
    let size = output
        .read_u32()
        .await
        .map_err(|_| "codec response header missing")? as usize;
    if size == 0 || size > MAX_MESSAGE_BYTES {
        return Err("codec response exceeds its budget".into());
    }
    let mut bytes = vec![0; size];
    output
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "codec response truncated")?;
    let reply: Envelope<Response> =
        serde_json::from_slice(&bytes).map_err(|_| "codec response is invalid")?;
    if reply.version != PROTOCOL_VERSION || reply.request_id != id {
        return Err("codec response belongs to another request or protocol".into());
    }
    if let Response::Error { message } = &reply.payload
        && (message.len() > MAX_ERROR_BYTES || message.chars().any(char::is_control))
    {
        return Err("codec error response is invalid".into());
    }
    Ok(reply.payload)
}

/// Why a codec request failed.
#[derive(Debug)]
pub enum Failure {
    /// No codec process is held any more: the failed child's exit was
    /// confirmed (or none started), so a later request may start another.
    Recovered(String),
    /// The child's exit could not be confirmed: the client keeps owning it
    /// and starts no replacement while it does.
    Retained(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recovered(message) | Self::Retained(message) => formatter.write_str(message),
        }
    }
}

/// One unique mutable owner per encoder/connection; no request queue or spawned
/// per-request tasks. A cancelled future leaves `busy` set and must retire the
/// old process before another request. Failure to reap never creates a new one.
/// The exchange and the subsequent retirement attempt have separate deadlines.
/// Expired retirement leaves the original child owned, without a replacement.
pub struct Client {
    role: Role,
    executable: PathBuf,
    process: Option<Process>,
    next: u64,
    busy: bool,
    #[cfg(test)]
    close_grace: Duration,
}

impl Client {
    pub fn new(role: Role, executable: PathBuf) -> Self {
        Self {
            role,
            executable,
            process: None,
            next: 1,
            busy: false,
            #[cfg(test)]
            close_grace: RETIREMENT_GRACE,
        }
    }
    pub async fn close(&mut self) -> Result<(), String> {
        let grace = {
            #[cfg(test)]
            {
                self.close_grace
            }
            #[cfg(not(test))]
            {
                RETIREMENT_GRACE
            }
        };
        self.close_until(Instant::now() + grace).await
    }

    async fn close_until(&mut self, deadline: Instant) -> Result<(), String> {
        // Cancellation of this borrowed future must leave the child unusable.
        self.busy = true;
        if let Some(process) = self.process.as_mut() {
            tokio::time::timeout_at(deadline, process.retire())
                .await
                .map_err(|_| "codec worker retirement is still pending".to_owned())??;
            self.process = None;
        }
        self.busy = false;
        Ok(())
    }
    pub fn started(&self) -> bool {
        self.process.is_some()
    }

    /// The previous operation cannot be reused, even though its error has
    /// already been delivered. Keep this owner until close confirms reaping.
    pub fn retirement_pending(&self) -> bool {
        self.busy && self.process.is_some()
    }

    /// Ends the current child after `error`: recovered only once its exit is
    /// confirmed. A malformed or failed owner is never reused, even when
    /// termination itself fails; a later request retries the reaping.
    async fn end(&mut self, error: String) -> Failure {
        self.busy = true;
        match self.close().await {
            Ok(()) => Failure::Recovered(error),
            Err(unconfirmed) => Failure::Retained(unconfirmed),
        }
    }

    async fn request(&mut self, request: Request) -> Result<Response, Failure> {
        if self.busy {
            self.close().await.map_err(Failure::Retained)?;
        }
        if self.process.is_none() {
            let process = spawn(&self.executable, self.role)
                .await
                .map_err(Failure::Recovered)?;
            self.process = Some(process);
            self.next = 1;
        }
        let id = self.next;
        let Some(next) = self.next.checked_add(1) else {
            return Err(self.end("codec request ids exhausted".into()).await);
        };
        self.next = next;
        self.busy = true;
        let process = self.process.as_mut().unwrap();
        let exchange = async {
            write_request(process.input.as_mut().unwrap(), id, &request).await?;
            read_response(process.output.as_mut().unwrap(), id).await
        };
        match tokio::time::timeout(RESPONSE_TIMEOUT, exchange).await {
            Ok(Ok(response)) => {
                self.busy = false;
                Ok(response)
            }
            Ok(Err(error)) => Err(self.end(error).await),
            Err(_) => Err(self.end("codec worker did not answer in time".into()).await),
        }
    }

    async fn unexpected<T>(&mut self) -> Result<T, Failure> {
        Err(self
            .end("codec worker returned an unexpected response".into())
            .await)
    }

    pub async fn encode(&mut self, samples: Vec<f32>, bitrate: u32) -> Result<Vec<u8>, String> {
        if !valid_pcm(&samples) || samples.iter().any(|v| v.abs() > 1.) {
            return Err("invalid encoder PCM".into());
        }
        let response = self
            .request(Request::Encode { samples, bitrate })
            .await
            .map_err(|failure| failure.to_string())?;
        match response {
            Response::Packet { opus } if !opus.is_empty() && opus.len() <= MAX_OPUS_BYTES => {
                Ok(opus)
            }
            Response::Error { message } => Err(message),
            _ => self
                .unexpected()
                .await
                .map_err(|failure: Failure| failure.to_string()),
        }
    }
    /// A damaged packet (empty, oversized, or with an invalid threshold) is
    /// not speech and costs no request.
    pub async fn hear(
        &mut self,
        sender: u32,
        opus: Vec<u8>,
        threshold: f32,
    ) -> Result<bool, Failure> {
        if opus.is_empty()
            || opus.len() > MAX_VOICE_BYTES
            || !threshold.is_finite()
            || !(0.0..=1.0).contains(&threshold)
        {
            return Ok(false);
        }
        match self
            .request(Request::Hear {
                sender,
                opus,
                threshold,
            })
            .await?
        {
            Response::Speech { active } => Ok(active),
            _ => self.unexpected().await,
        }
    }
    pub async fn forget(&mut self, sender: u32) -> Result<(), Failure> {
        if !self.started() {
            return Ok(());
        }
        self.ack(Request::Forget { sender }).await
    }
    pub async fn clear(&mut self) -> Result<(), String> {
        if !self.started() {
            return Ok(());
        }
        self.ack(Request::Clear)
            .await
            .map_err(|failure| failure.to_string())
    }
    async fn ack(&mut self, request: Request) -> Result<(), Failure> {
        match self.request(request).await? {
            Response::Ack => Ok(()),
            _ => self.unexpected().await,
        }
    }
}

/// Recovered speech-detector failures one Mumble session tolerates before
/// ducking stops for the rest of it.
pub const DUCK_FAILURES: usize = 3;

/// Ending the speech detector could not be confirmed. Its process stays owned;
/// ducking is unavailable and the connection must not start another detector.
#[derive(Debug)]
pub struct Unavailable(pub String);

/// The speech detector of one Mumble connection: a lazy confined duck child
/// whose failures never cost the connection. A failed request whose child
/// exit was confirmed costs only that packet: its sender is quarantined, so
/// poison input is never retried, and after `DUCK_FAILURES` such failures
/// ducking stops until the next Mumble session (`new_session`). Only an
/// unconfirmed exit is reported, as `Unavailable`.
pub struct Ducker {
    client: Client,
    /// At most `DUCK_FAILURES` senders: each one is a recovered failure.
    quarantined: Vec<u32>,
    failures: usize,
}

impl Ducker {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            client: Client::new(Role::Duck, executable),
            quarantined: Vec::new(),
            failures: 0,
        }
    }

    /// Whether a duck child currently exists.
    pub fn running(&self) -> bool {
        self.client.started()
    }

    pub fn suspended(&self) -> bool {
        self.failures >= DUCK_FAILURES
    }

    /// The one fixed status line while ducking is suspended.
    pub fn notice(&self) -> Option<&'static str> {
        self.suspended().then_some(
            "Ducking stopped on this connection after the speech detector failed three times; \
             it resumes after the next reconnection.",
        )
    }

    /// A genuinely new Mumble session: the old child is retired and its
    /// failures and quarantine are forgotten. Nothing else resets them.
    pub async fn new_session(&mut self) -> Result<(), Unavailable> {
        self.client.close().await.map_err(Unavailable)?;
        self.quarantined.clear();
        self.failures = 0;
        Ok(())
    }

    /// One voice packet of `sender`; true when it holds speech.
    pub async fn hear(
        &mut self,
        sender: u32,
        opus: &[u8],
        threshold: f32,
    ) -> Result<bool, Unavailable> {
        if self.suspended() || self.quarantined.contains(&sender) {
            return Ok(false);
        }
        match self.client.hear(sender, opus.to_vec(), threshold).await {
            Ok(active) => Ok(active),
            Err(failure) => self.failed(Some(sender), failure).map(|()| false),
        }
    }

    /// `sender` left; its detector state goes (the quarantine stays).
    pub async fn forget(&mut self, sender: u32) -> Result<(), Unavailable> {
        if self.suspended() {
            return Ok(());
        }
        match self.client.forget(sender).await {
            Ok(()) => Ok(()),
            Err(failure) => self.failed(None, failure),
        }
    }

    /// Nobody needs ducking: retire the child once. While it is not running,
    /// this and later packets send nothing.
    pub async fn disable(&mut self) -> Result<(), Unavailable> {
        if self.client.started() {
            self.client.close().await.map_err(Unavailable)?;
        }
        Ok(())
    }

    fn failed(&mut self, sender: Option<u32>, failure: Failure) -> Result<(), Unavailable> {
        match failure {
            Failure::Retained(unconfirmed) => Err(Unavailable(unconfirmed)),
            Failure::Recovered(_) => {
                if let Some(sender) = sender
                    && !self.quarantined.contains(&sender)
                {
                    self.quarantined.push(sender);
                }
                self.failures = self.failures.saturating_add(1);
                Ok(())
            }
        }
    }
}

/// One decoded 20 ms frame, or why decoding stopped.
pub type Frame = Result<Vec<f32>, String>;

/// Opaque media for the decode role, and the hints it may use.
pub struct Media<R> {
    pub reader: R,
    pub mime: Option<String>,
    pub extension: Option<String>,
}

/// Feed `media` to a confined decode child and pass its validated frames to
/// `sender`, skipping `skip_frames` first, within `limits`. Over-limit requests
/// and files fail explicitly; they are never cut short. Returns only after the
/// child has actually exited and the pending blocking read of `media` has
/// returned; cancellation (`cancelled` then `wake`) closes promptly but keeps both.
pub async fn decode(
    executable: &Path,
    media: Media<impl Read + Send + 'static>,
    skip_frames: u64,
    sender: &mpsc::Sender<Frame>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    limits: &DecodeLimits,
) -> Result<(), String> {
    decode_inner(
        executable,
        media,
        skip_frames,
        sender,
        cancelled,
        wake,
        &mut DecodeCompletion {
            limits: *limits,
            terminal: None,
        },
    )
    .await
}

/// Deliver the logical terminal result independently of retirement. Returning
/// from this future still requires actual child exit and completion of the
/// outstanding media read; that final result includes any late download error.
pub struct DecodeCompletion {
    pub limits: DecodeLimits,
    pub terminal: Option<oneshot::Sender<Result<(), String>>>,
}

pub async fn decode_notifying(
    executable: &Path,
    media: Media<impl Read + Send + 'static>,
    skip_frames: u64,
    sender: &mpsc::Sender<Frame>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    mut completion: DecodeCompletion,
) -> Result<(), String> {
    let result = decode_inner(
        executable,
        media,
        skip_frames,
        sender,
        cancelled,
        wake,
        &mut completion,
    )
    .await;
    // Validation and launch failures occur before the exchange's notification.
    notify_terminal(&mut completion.terminal, &result);
    result
}

fn notify_terminal(
    terminal: &mut Option<oneshot::Sender<Result<(), String>>>,
    result: &Result<(), String>,
) {
    if let Some(terminal) = terminal.take() {
        let _ = terminal.send(result.clone());
    }
}

async fn decode_inner(
    executable: &Path,
    media: Media<impl Read + Send + 'static>,
    skip_frames: u64,
    sender: &mpsc::Sender<Frame>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    completion: &mut DecodeCompletion,
) -> Result<(), String> {
    let limits = &completion.limits;
    if skip_frames > limits.seek_frames {
        return Err(format!(
            "playback cannot start more than {} minutes into a song",
            limits.seek_frames / 3000
        ));
    }
    let mp4 = needs_seeking(media.mime.as_deref(), media.extension.as_deref());
    let input_limit = if mp4 {
        limits.mp4_bytes
    } else {
        limits.seek_input_bytes
    };
    let mut process = spawn(executable, Role::Decode).await?;
    let mut input = process.input.take().ok_or("decoder input missing")?;
    let mut output = process.output.take().ok_or("decoder output missing")?;
    let request = Request::Decode {
        mime: media.mime,
        extension: media.extension,
        skip_frames,
    };
    let (chunks, mut incoming) = mpsc::channel::<Vec<u8>>(2);
    let pump_flag = cancelled.clone();
    let reader = media.reader;
    let pumping = tokio::task::spawn_blocking(move || pump(reader, chunks, &pump_flag));
    // Counted before each write: the child cannot have read more than this.
    let delivered = Arc::new(AtomicU64::new(0));
    let audio = Arc::new(AtomicBool::new(false));
    let (sent, playing) = (delivered.clone(), audio.clone());
    let mut writing = tokio::spawn(async move {
        write_request(&mut input, 1, &request).await?;
        while let Some(bytes) = incoming.recv().await {
            let total = sent
                .load(Ordering::Relaxed)
                .checked_add(bytes.len() as u64)
                .ok_or("music input size overflowed")?;
            if !playing.load(Ordering::Relaxed) && total > input_limit {
                return Err(if mp4 {
                    format!("the file is larger than {} MiB", input_limit >> 20)
                } else {
                    format!(
                        "playback would read more than {} MiB before it starts",
                        input_limit >> 20
                    )
                });
            }
            sent.store(total, Ordering::Relaxed);
            input
                .write_all(&bytes)
                .await
                .map_err(|_| "decoder stream write failed".to_owned())?;
        }
        input
            .shutdown()
            .await
            .map_err(|_| "decoder stream close failed".to_owned())
    });
    // Replies arrive through a channel, so selecting on cancellation or the
    // input's end never drops a partly read frame.
    let (replies_sender, mut replies) = mpsc::channel(1);
    let mut reading = tokio::spawn(async move {
        loop {
            let reply = read_response(&mut output, 1).await;
            let last = !matches!(reply, Ok(Response::Progress { .. } | Response::Pcm { .. }));
            if replies_sender.send(reply).await.is_err() || last {
                break;
            }
        }
    });
    let mut written = false;
    let mut check = ProgressCheck::new(mp4, skip_frames, input_limit);
    let before_audio = Instant::now() + limits.before_audio;
    // Only replies (and the consumer taking a frame) move this; nothing else does.
    let mut quiet_until = Instant::now() + limits.inactivity;
    let result = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Ok(());
        }
        let until = if check.audio_started() {
            quiet_until
        } else {
            quiet_until.min(before_audio)
        };
        let response = tokio::select! {
            _ = wake.notified() => { if cancelled.load(Ordering::Relaxed) { break Ok(()); } else { continue; } }
            _ = sender.closed() => { break Ok(()); }
            finished = &mut writing, if !written => {
                written = true;
                match finished {
                    Ok(Ok(())) => continue,
                    Ok(Err(error)) => break Err(error),
                    Err(_) => break Err("music decoder input task failed".into()),
                }
            }
            response = tokio::time::timeout_at(until, replies.recv()) => response,
        };
        quiet_until = Instant::now() + limits.inactivity;
        match response {
            Ok(Some(Ok(Response::Progress { stage, units }))) => {
                if let Err(error) = check.progress(stage, units, delivered.load(Ordering::Relaxed))
                {
                    break Err(error);
                }
            }
            Ok(Some(Ok(Response::Pcm { samples }))) if valid_pcm(&samples) => {
                if let Err(error) = check.pcm() {
                    break Err(error);
                }
                audio.store(true, Ordering::Relaxed);
                // Consumer backpressure does not count as a decoder stall.
                tokio::select! {
                    _ = wake.notified() => { if cancelled.load(Ordering::Relaxed) { break Ok(()); } }
                    result = sender.send(Ok(samples)) => if result.is_err() { break Ok(()); },
                }
                quiet_until = Instant::now() + limits.inactivity;
            }
            Ok(Some(Ok(Response::Done))) => break Ok(()),
            Ok(Some(Ok(Response::Error { message }))) => break Err(message),
            Ok(Some(Err(error))) => break Err(error),
            Ok(None) => break Err("music decoder output ended".into()),
            Err(_) if !check.audio_started() && Instant::now() >= before_audio => {
                break Err(format!(
                    "music decoder did not start playing within {} seconds",
                    limits.before_audio.as_secs()
                ));
            }
            Err(_) => {
                break Err(format!(
                    "music decoder made no progress within {} seconds",
                    limits.inactivity.as_secs()
                ));
            }
            _ => break Err("music decoder returned an invalid response".into()),
        }
    };
    // No frame-channel capacity or OS exit is required to settle the operation.
    // Success here is provisional: the owner's final result still includes the
    // network read, and must be checked before recording successful history.
    notify_terminal(&mut completion.terminal, &result);
    // Closing the bounded chunk receiver releases a blocked producer; a read
    // already submitted to the library must still finish before this slot is reusable.
    if !written {
        writing.abort();
        let _ = (&mut writing).await;
    }
    reading.abort();
    let _ = (&mut reading).await;
    finish_decode(
        process,
        pumping,
        cancelled,
        result,
        &mut completion.terminal,
    )
    .await
}

/// The supervisor retains both resources after settling the logical operation.
async fn finish_decode(
    mut process: Process,
    pumping: tokio::task::JoinHandle<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    result: Result<(), String>,
    terminal: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    notify_terminal(terminal, &result);
    loop {
        match process.retire().await {
            Ok(()) => break,
            Err(_) => {
                // Never release the actual process permit on a failed kill/reap.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    let downloaded = pumping
        .await
        .map_err(|_| "music download task failed".to_owned())?;
    if cancelled.load(Ordering::Relaxed) {
        Ok(())
    } else {
        result.and(downloaded)
    }
}

fn pump(
    mut input: impl Read,
    chunks: mpsc::Sender<Vec<u8>>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut bytes = [0; 32 * 1024];
    while !cancelled.load(Ordering::Relaxed) {
        let count = match input.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("the music library audio download failed".into()),
        };
        if cancelled.load(Ordering::Relaxed)
            || chunks.blocking_send(bytes[..count].to_vec()).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // The gate withholds retirement acknowledgement deterministically; this is
    // not a claim to reproduce an unkillable kernel task. The owned child and
    // the eventual kill/reap are real. No test mode is included in release bins.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "owned subprocess fixture"]
    fn retirement_child() {
        if std::env::var_os("NUSCHL_RETIREMENT_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[cfg(target_os = "linux")]
    fn held_client(role: Role) -> (Client, Arc<Notify>, u32) {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "musicbot::tests::retirement_child", "--ignored"])
            .env("NUSCHL_RETIREMENT_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let containment = crate::host::contain(&child).unwrap();
        let gate = Arc::new(Notify::new());
        let process = Process {
            input: child.stdin.take(),
            output: child.stdout.take(),
            child,
            _containment: containment,
            retirement_gate: Some(gate.clone()),
        };
        let mut client = Client::new(role, PathBuf::from("/nonexistent/no-replacement"));
        client.process = Some(process);
        client.close_grace = Duration::from_millis(20);
        (client, gate, pid)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_exchange_and_cancelled_close_retain_the_same_child() {
        let (mut client, gate, pid) = held_client(Role::Encode);
        // The fixture's libtest stdout is an invalid framed reply, driving the
        // real request -> end -> bounded close path.
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            client.encode(vec![0.; FRAME * CHANNELS], 96_000),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.contains("retirement"));
        assert!(client.retirement_pending());
        assert_eq!(client.process.as_ref().unwrap().child.id(), Some(pid));
        assert!(
            client
                .process
                .as_mut()
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        // Busy preflight cannot start another executable or reuse the pipes.
        assert!(matches!(
            client.request(Request::Clear).await,
            Err(Failure::Retained(_))
        ));
        assert_eq!(client.process.as_ref().unwrap().child.id(), Some(pid));
        assert!(
            tokio::time::timeout(Duration::from_millis(1), client.close())
                .await
                .is_err()
        );
        assert!(client.retirement_pending());
        client.close_grace = Duration::from_secs(1);
        gate.notify_one();
        client.close().await.unwrap();
        assert!(!client.started() && !client.retirement_pending());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        // Only now can a fresh launch be attempted (and fail discovery).
        assert!(matches!(
            client.request(Request::Clear).await,
            Err(Failure::Recovered(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn duck_disable_and_new_session_keep_unconfirmed_credit() {
        let (client, gate, pid) = held_client(Role::Duck);
        let mut ducker = Ducker {
            client,
            quarantined: vec![4],
            failures: 1,
        };
        assert!(
            ducker
                .hear(9, &nuschl_core::audio::SILENT_OPUS, 0.6)
                .await
                .is_err()
        );
        assert!(ducker.disable().await.is_err());
        assert!(ducker.new_session().await.is_err());
        assert_eq!(ducker.quarantined, [4]);
        assert_eq!(ducker.failures, 1);
        assert_eq!(
            ducker.client.process.as_ref().unwrap().child.id(),
            Some(pid)
        );
        ducker.client.close_grace = Duration::from_secs(1);
        gate.notify_one();
        ducker.new_session().await.unwrap();
        assert!(!ducker.running() && ducker.quarantined.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn decode_terminal_precedes_reaping_and_a_controlled_blocking_read() {
        let (mut client, gate, pid) = held_client(Role::Decode);
        let process = client.process.take().unwrap();
        let (entered, started) = oneshot::channel();
        let (release, read_release) = std::sync::mpsc::channel();
        let pumping = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            read_release.recv().unwrap();
            Err("late network failure".to_owned())
        });
        started.await.unwrap();
        let (terminal, ready) = oneshot::channel();
        let mut owner = tokio::spawn(async move {
            finish_decode(
                process,
                pumping,
                Arc::new(AtomicBool::new(false)),
                Ok(()),
                &mut Some(terminal),
            )
            .await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), ready)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        assert!(!owner.is_finished());
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        gate.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut owner)
                .await
                .is_err()
        );
        // The read is independently still owned even after real process exit.
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        release.send(()).unwrap();
        assert_eq!(owner.await.unwrap(), Err("late network failure".into()));
    }

    #[test]
    fn sensitivity_lowers_the_threshold() {
        assert!(speech_threshold(0) > speech_threshold(50));
        assert!(speech_threshold(50) > speech_threshold(100));
        assert!((speech_threshold(50) - 0.6).abs() < 1e-6);
        assert_eq!(speech_threshold(255), speech_threshold(100));
    }

    #[test]
    fn the_worker_has_one_fixed_place_beside_the_bot() {
        assert_eq!(
            worker_path(Path::new(
                "/nix/store/x-nuschl-musicbot/bin/.nuschl-musicbot-wrapped"
            )),
            Some(PathBuf::from(
                "/nix/store/x-nuschl-musicbot/libexec/nuschl-musicbot/nuschl-musicbot-codec-worker"
            ))
        );
        assert_eq!(worker_path(Path::new("nuschl-musicbot")), None);
    }

    #[tokio::test]
    async fn a_missing_worker_is_refused_before_any_launch() {
        let error = spawn(
            Path::new("/nonexistent/nuschl-musicbot-codec-worker"),
            Role::Encode,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error, "the music codec worker is not installed");
    }

    const MIB: u64 = INPUT_PROGRESS_BYTES;

    #[test]
    fn progress_must_be_exact_ordered_steps_within_what_was_sent() {
        // MP4 read whole, then a seek of 600 frames.
        let mut check = ProgressCheck::new(true, 600, 512 * MIB);
        check.progress(Stage::Input, MIB, MIB).unwrap();
        check.progress(Stage::Input, 2 * MIB, 3 * MIB).unwrap();
        check.progress(Stage::Skip, 250, 3 * MIB).unwrap();
        check.progress(Stage::Skip, 500, 3 * MIB).unwrap();
        check.pcm().unwrap();
        check.pcm().unwrap();
        // No progress after the first PCM.
        assert!(check.progress(Stage::Skip, 750, 3 * MIB).is_err());

        let rejected = |mp4: bool, skip: u64, steps: &[(Stage, u64, u64)]| {
            let mut check = ProgressCheck::new(mp4, skip, 512 * MIB);
            let (last, before) = steps.split_last().unwrap();
            for &(stage, units, delivered) in before {
                check.progress(stage, units, delivered).unwrap();
            }
            check.progress(last.0, last.1, last.2).is_err()
        };
        // Input more than the parent sent, a skipped or repeated step, input
        // that is not MP4, input after skipping began.
        assert!(rejected(true, 0, &[(Stage::Input, MIB, MIB - 1)]));
        assert!(rejected(true, 0, &[(Stage::Input, 2 * MIB, 2 * MIB)]));
        assert!(rejected(
            true,
            0,
            &[(Stage::Input, MIB, MIB), (Stage::Input, MIB, MIB)]
        ));
        assert!(rejected(false, 0, &[(Stage::Input, MIB, MIB)]));
        assert!(rejected(
            true,
            500,
            &[(Stage::Skip, 250, 0), (Stage::Input, MIB, MIB)]
        ));
        // A skip step beyond the request, not a whole step, or decreasing.
        assert!(rejected(false, 249, &[(Stage::Skip, 250, 0)]));
        assert!(rejected(false, 600, &[(Stage::Skip, 200, 0)]));
        assert!(rejected(
            false,
            600,
            &[(Stage::Skip, 250, 0), (Stage::Skip, 250, 0)]
        ));
        // MP4 input beyond the limit.
        let mut check = ProgressCheck::new(true, 0, 2 * MIB);
        check.progress(Stage::Input, MIB, 3 * MIB).unwrap();
        check.progress(Stage::Input, 2 * MIB, 3 * MIB).unwrap();
        assert!(check.progress(Stage::Input, 3 * MIB, 3 * MIB).is_err());
    }

    #[test]
    fn audio_before_the_whole_skip_was_reported_is_refused() {
        let mut check = ProgressCheck::new(false, 600, 2048 * MIB);
        check.progress(Stage::Skip, 250, 0).unwrap();
        assert!(check.pcm().is_err());
        let mut check = ProgressCheck::new(false, 249, 2048 * MIB);
        check.pcm().unwrap();
        assert!(check.audio_started());
    }

    #[test]
    fn progress_replies_are_bounded_by_the_request() {
        let mut check = ProgressCheck::new(false, 500, 0);
        check.progress(Stage::Skip, 250, 0).unwrap();
        check.progress(Stage::Skip, 500, 0).unwrap();
        assert!(check.progress(Stage::Skip, 750, 0).is_err());
        // Absurd values are refused, never wrapped.
        let mut check = ProgressCheck::new(true, u64::MAX, u64::MAX);
        assert!(check.progress(Stage::Skip, u64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn mp4_is_recognised_alike_on_both_sides() {
        assert!(needs_seeking(Some("audio/mp4"), None));
        assert!(needs_seeking(None, Some("M4B")));
        assert!(!needs_seeking(Some("audio/flac"), Some("flac")));
        assert!(!needs_seeking(None, None));
    }

    #[tokio::test]
    async fn a_seek_beyond_the_limit_fails_before_any_launch() {
        let (sender, _frames) = mpsc::channel(1);
        let media = Media {
            reader: io::empty(),
            mime: None,
            extension: None,
        };
        let error = decode(
            Path::new("/nonexistent/nuschl-musicbot-codec-worker"),
            media,
            DecodeLimits::DEFAULT.seek_frames + 1,
            &sender,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Notify::new()),
            &DecodeLimits::DEFAULT,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "playback cannot start more than 180 minutes into a song"
        );
    }

    #[test]
    fn an_unconfirmed_exit_is_unavailable_and_recovered_ones_are_bounded() {
        let mut ducker = Ducker::new(PathBuf::from("/nonexistent/nuschl-musicbot-codec-worker"));
        assert!(matches!(
            ducker.failed(Some(1), Failure::Retained("cannot reap".into())),
            Err(Unavailable(_))
        ));
        assert_eq!(ducker.failures, 0);
        for sender in [7, 7, 8] {
            ducker
                .failed(Some(sender), Failure::Recovered("died".into()))
                .unwrap();
        }
        assert!(ducker.suspended() && ducker.notice().is_some());
        assert_eq!(ducker.quarantined, [7, 8]);
    }

    #[tokio::test]
    async fn a_missing_worker_suspends_ducking_instead_of_retrying_forever() {
        let mut ducker = Ducker::new(PathBuf::from("/nonexistent/nuschl-musicbot-codec-worker"));
        let packet = nuschl_core::audio::SILENT_OPUS;
        for sender in 0..10 {
            assert!(!ducker.hear(sender, &packet, 0.6).await.unwrap());
        }
        // Three failed launches, each quarantining its sender, then nothing.
        assert!(ducker.suspended() && !ducker.running());
        assert_eq!(ducker.quarantined, [0, 1, 2]);
        // Damaged input costs no request at all.
        let mut fresh = Ducker::new(PathBuf::from("/nonexistent/nuschl-musicbot-codec-worker"));
        for opus in [Vec::new(), vec![1; MAX_VOICE_BYTES + 1]] {
            assert!(!fresh.hear(1, &opus, 0.6).await.unwrap());
        }
        assert!(!fresh.hear(1, &packet, f32::NAN).await.unwrap());
        assert_eq!(fresh.failures, 0);
        // Only a new session resets the count.
        fresh.disable().await.unwrap();
        ducker.disable().await.unwrap();
        assert!(ducker.suspended());
        ducker.new_session().await.unwrap();
        assert!(!ducker.suspended() && ducker.quarantined.is_empty());
    }

    #[tokio::test]
    async fn cancelled_read_keeps_its_actual_thread_until_completion() {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, finish) = std::sync::mpsc::channel();
        struct PendingRead {
            entered: std::sync::mpsc::Sender<()>,
            finish: std::sync::mpsc::Receiver<()>,
        }
        impl Read for PendingRead {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.entered.send(()).unwrap();
                self.finish.recv().unwrap();
                bytes[0] = 1;
                Ok(1)
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = flag.clone();
        let (sender, receiver) = mpsc::channel(2);
        let mut worker = tokio::task::spawn_blocking(move || {
            pump(PendingRead { entered, finish }, sender, &worker_flag)
        });
        tokio::task::spawn_blocking(move || started.recv().unwrap())
            .await
            .unwrap();
        flag.store(true, Ordering::Relaxed);
        drop(receiver);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut worker)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        worker.await.unwrap().unwrap();
    }

    #[test]
    fn opaque_pump_is_bounded_and_cancellation_stops_before_read() {
        let (sender, mut receiver) = mpsc::channel(2);
        let cancelled = AtomicBool::new(true);
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("read after cancellation")
            }
        }
        pump(NoRead, sender, &cancelled).unwrap();
        assert!(receiver.try_recv().is_err());
        let (sender, mut receiver) = mpsc::channel(2);
        pump(
            Cursor::new(vec![42; 40_000]),
            sender,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(receiver.try_recv().unwrap().len(), 32 * 1024);
        assert_eq!(receiver.try_recv().unwrap().len(), 40_000 - 32 * 1024);
        assert!(receiver.try_recv().is_err());
    }
}
