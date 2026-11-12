// SPDX-License-Identifier: EUPL-1.2
//! The music bot's codec roles (docs/musicbot.md): the private protocol between
//! the credential-bearing `nuschl-musicbot` and its `nuschl-musicbot-codec-worker`
//! children, and the bot's supervisor for those children. No credentials, URLs
//! or paths cross it; the bot names the worker executable, never the worker.
//!
//! Decode receives one framed `Decode` request (id 1), followed by opaque raw
//! media bytes until EOF. Every PCM/Done/Error reply carries that same id.
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
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Notify, mpsc},
};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME: usize = 960;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_OPUS_BYTES: usize = 992;
pub const MAX_VOICE_BYTES: usize = 1024;
pub const MAX_ERROR_BYTES: usize = 512;

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

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
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

/// The worker in a Nix-style installation: `<prefix>/bin/<bot>` finds
/// `<prefix>/libexec/nuschl-musicbot/nuschl-musicbot-codec-worker`. Nothing
/// from the environment, configuration or a child selects another program.
pub fn worker_path(bot: &Path) -> Option<PathBuf> {
    bot.parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("libexec/nuschl-musicbot/nuschl-musicbot-codec-worker"))
}

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(35);

/// The credential-bearing parent's end of one confined codec child: only
/// bounded IPC and opaque transport.
pub struct Process {
    pub child: Child,
    pub input: Option<ChildStdin>,
    pub output: Option<ChildStdout>,
    _containment: crate::host::Containment,
}

impl Process {
    /// A kill request is not a released slot: callers retain this object until
    /// the OS has confirmed exit. Errors leave ownership with the caller.
    pub async fn retire(&mut self) -> Result<(), String> {
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

/// One unique mutable owner per encoder/connection; no request queue or spawned
/// per-request tasks. A cancelled future leaves `busy` set and must retire the
/// old process before another request. Failure to reap never creates a new one.
pub struct Client {
    role: Role,
    executable: PathBuf,
    process: Option<Process>,
    next: u64,
    busy: bool,
}

impl Client {
    pub fn new(role: Role, executable: PathBuf) -> Self {
        Self {
            role,
            executable,
            process: None,
            next: 1,
            busy: false,
        }
    }
    pub async fn close(&mut self) -> Result<(), String> {
        if let Some(process) = self.process.as_mut() {
            process.retire().await?;
            self.process = None;
        }
        self.busy = false;
        Ok(())
    }
    pub fn started(&self) -> bool {
        self.process.is_some()
    }

    async fn request(&mut self, request: Request) -> Result<Response, String> {
        if self.busy {
            self.close().await?;
        }
        if self.process.is_none() {
            self.process = Some(spawn(&self.executable, self.role).await?);
            self.next = 1;
        }
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or("codec request ids exhausted")?;
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
            result => {
                // Keep process/credit if this cannot confirm termination.
                self.close().await?;
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "codec worker did not answer in time".into(),
                })
            }
        }
    }

    async fn unexpected<T>(&mut self) -> Result<T, String> {
        // Quarantine a malformed owner even when termination itself fails.
        // A subsequent request must retry reaping, never reuse this protocol.
        self.busy = true;
        self.close().await?;
        Err("codec worker returned an unexpected response".into())
    }

    pub async fn encode(&mut self, samples: Vec<f32>, bitrate: u32) -> Result<Vec<u8>, String> {
        if !valid_pcm(&samples) || samples.iter().any(|v| v.abs() > 1.) {
            return Err("invalid encoder PCM".into());
        }
        match self.request(Request::Encode { samples, bitrate }).await? {
            Response::Packet { opus } if !opus.is_empty() && opus.len() <= MAX_OPUS_BYTES => {
                Ok(opus)
            }
            Response::Error { message } => Err(message),
            _ => self.unexpected().await,
        }
    }
    pub async fn hear(
        &mut self,
        sender: u32,
        opus: Vec<u8>,
        threshold: f32,
    ) -> Result<bool, String> {
        if opus.len() > MAX_VOICE_BYTES
            || !threshold.is_finite()
            || !(0.0..=1.0).contains(&threshold)
        {
            return Err("invalid ducking input".into());
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
            Response::Error { message } => Err(message),
            _ => self.unexpected().await,
        }
    }
    pub async fn forget(&mut self, sender: u32) -> Result<(), String> {
        if !self.started() {
            return Ok(());
        }
        self.ack(Request::Forget { sender }).await
    }
    pub async fn clear(&mut self) -> Result<(), String> {
        if !self.started() {
            return Ok(());
        }
        self.ack(Request::Clear).await
    }
    async fn ack(&mut self, request: Request) -> Result<(), String> {
        match self.request(request).await? {
            Response::Ack => Ok(()),
            Response::Error { message } => Err(message),
            _ => self.unexpected().await,
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
/// `sender`, skipping `skip_frames` first. Returns only after the child has
/// actually exited and the pending blocking read of `media` has returned;
/// cancellation (`cancelled` then `wake`) closes promptly but keeps both.
pub async fn decode(
    executable: &Path,
    media: Media<impl Read + Send + 'static>,
    skip_frames: u64,
    sender: &mpsc::Sender<Frame>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
) -> Result<(), String> {
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
    let mut writing = tokio::spawn(async move {
        write_request(&mut input, 1, &request).await?;
        while let Some(bytes) = incoming.recv().await {
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
    let result = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Ok(());
        }
        let response = tokio::select! {
            _ = wake.notified() => { if cancelled.load(Ordering::Relaxed) { break Ok(()); } else { continue; } }
            _ = sender.closed() => { break Ok(()); }
            response = tokio::time::timeout(DECODE_TIMEOUT, read_response(&mut output, 1)) => response,
        };
        match response {
            Ok(Ok(Response::Pcm { samples })) if valid_pcm(&samples) => {
                // Consumer backpressure does not count as a decoder stall.
                tokio::select! {
                    _ = wake.notified() => { if cancelled.load(Ordering::Relaxed) { break Ok(()); } }
                    result = sender.send(Ok(samples)) => if result.is_err() { break Ok(()); },
                }
            }
            Ok(Ok(Response::Done)) => break Ok(()),
            Ok(Ok(Response::Error { message })) => break Err(message),
            Ok(Err(error)) => break Err(error),
            Err(_) => break Err("music decoder did not produce audio within 35 seconds".into()),
            _ => break Err("music decoder returned an invalid response".into()),
        }
    };
    // Closing the bounded chunk receiver releases a blocked producer; a read
    // already submitted to the library must still finish before this slot is reusable.
    writing.abort();
    let _ = (&mut writing).await;
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
