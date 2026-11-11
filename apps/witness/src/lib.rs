// SPDX-License-Identifier: EUPL-1.2
//! `nuschel-witness`: the project auditor for Nuschel's key transparency
//! (docs/witness.md, docs/e2ee.md §5.9).
//!
//! Servers submit their akd epochs over HTTP (`POST /nuschel-kt/v1/add-epochs`, a
//! `nuschel_kt::witness::auditor::AddEpochs` in nuschel-kt's strict encoding). The
//! auditor checks every append-only proof from the epoch it holds, appends the
//! epochs to its copy of the server's epoch log, and cosigns the checkpoint with its
//! ML-DSA-44 key if the result is the server's checkpoint. It keeps one small state
//! file per origin and never cosigns two logs for one origin, or an older checkpoint
//! after a newer one.
//!
//! - **Enrolment.** The first submission for an origin `<host>:<port>/nuschel-kt/v1`
//!   is accepted only if the server at `host:port` announces the same log in its
//!   Nuschel hello (`kt_log`), as seen from the network, the way ACME checks a name.
//!   Private and loopback addresses are refused unless configured.
//! - **Plain HTTP.** TLS comes from a reverse proxy (the NixOS module's nginx with
//!   ACME); the listener defaults to loopback.
//! - **Limits.** Request bodies up to `max_request_bytes`, submissions per origin and
//!   minute, enrolments per minute, `max_logs` origins, 64 connections at once,
//!   timeouts on every read.
//! - **Monitoring.** `GET /nuschel-kt/v1/checkpoints/<hex SHA-256 of the origin>` is the
//!   newest checkpoint it cosigned for the origin; `GET /nuschel-kt/v1/vkey` its key.

use nuschel_kt::witness::auditor::{AddEpochs, AddError, LogState, add_epochs};
use nuschel_kt::witness::note::{Cosigner, Note};
use nuschel_kt::witness::{ORIGIN_SUFFIX, note};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::timeout;
use zeroize::Zeroizing;

/// The path servers submit to, below the witness's URL.
pub const ADD_EPOCHS_PATH: &str = "/nuschel-kt/v1/add-epochs";
const VKEY_PATH: &str = "/nuschel-kt/v1/vkey";
const CHECKPOINTS_PATH: &str = "/nuschel-kt/v1/checkpoints/";
const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_CONNECTIONS: usize = 64;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(120);
const ENROL_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest `Version` frame read during enrolment.
const MAX_VERSION_FRAME: usize = 64 * 1024;

/// How the witness runs.
#[derive(Clone, Debug)]
pub struct Config {
    /// The cosigner name, as clients configure it (a schema-less URL is customary,
    /// for example `witness.example.org/nuschel-kt`).
    pub name: String,
    /// The ML-DSA-44 seed: 32 bytes or 64 hex digits. Without it one is generated
    /// once in the state directory (`witness.key`).
    pub key_file: Option<PathBuf>,
    pub state_directory: PathBuf,
    pub listen: SocketAddr,
    /// Enrol origins it has not seen before.
    pub accept_new_logs: bool,
    pub max_logs: usize,
    /// Per origin.
    pub submissions_per_minute: u32,
    /// New origins, witness-wide.
    pub enrolments_per_minute: u32,
    pub max_request_bytes: usize,
    /// Enrol servers on loopback, private and link-local addresses (tests and
    /// private deployments).
    pub enrol_private_addresses: bool,
}

impl Config {
    pub fn new(name: impl Into<String>, state_directory: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            key_file: None,
            state_directory: state_directory.into(),
            listen: SocketAddr::from(([127, 0, 0, 1], 8473)),
            accept_new_logs: true,
            max_logs: 10_000,
            submissions_per_minute: 60,
            enrolments_per_minute: 10,
            max_request_bytes: 16 * 1024 * 1024,
            enrol_private_addresses: false,
        }
    }
}

// ---- keys ----

/// 32 raw bytes, or 64 hex digits with optional surrounding whitespace.
fn parse_seed(bytes: &[u8]) -> io::Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    if bytes.len() == 32 {
        key.copy_from_slice(bytes);
        return Ok(key);
    }
    let text = bytes.trim_ascii();
    let bad = || io::Error::other("a key file holds 32 bytes or 64 hex digits");
    if text.len() != 64 {
        return Err(bad());
    }
    for (index, pair) in text.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        match (digit(pair[0]), digit(pair[1])) {
            (Some(high), Some(low)) => key[index] = (high * 16 + low) as u8,
            _ => return Err(bad()),
        }
    }
    Ok(key)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The configured seed, or the one generated once in the state directory.
pub fn load_seed(
    key_file: Option<&Path>,
    state_directory: &Path,
) -> io::Result<Zeroizing<[u8; 32]>> {
    let read = |path: &Path| -> io::Result<Zeroizing<[u8; 32]>> {
        let bytes = Zeroizing::new(std::fs::read(path)?);
        if bytes.len() > 256 {
            return Err(io::Error::other(format!("{} is too long", path.display())));
        }
        parse_seed(&bytes).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
    };
    if let Some(path) = key_file {
        return read(path);
    }
    let generated = state_directory.join("witness.key");
    if generated.exists() {
        return read(&generated);
    }
    std::fs::create_dir_all(state_directory)?;
    let mut seed = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut seed[..]).map_err(|_| io::Error::other("no system randomness"))?;
    let text = Zeroizing::new(format!("{}\n", hex(&seed[..])));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&generated)?;
    io::Write::write_all(&mut file, text.as_bytes())?;
    file.sync_all()?;
    tracing::info!(path = %generated.display(), "generated the witness key");
    Ok(seed)
}

/// The cosigner for `config`.
pub fn cosigner(config: &Config) -> io::Result<Cosigner> {
    let seed = load_seed(config.key_file.as_deref(), &config.state_directory)?;
    Cosigner::from_seed(&config.name, &seed).ok_or_else(|| {
        io::Error::other(format!(
            "{:?} is not a valid cosigner name (1 to 255 bytes, no spaces or '+')",
            config.name
        ))
    })
}

// ---- state ----

/// A per-origin rate limit: `per_minute` tokens, refilled continuously.
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn take(&mut self, per_minute: u32) -> bool {
        let now = Instant::now();
        let rate = f64::from(per_minute) / 60.0;
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * rate)
            .min(f64::from(per_minute.max(1)));
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn full(per_minute: u32) -> Self {
        Self {
            tokens: f64::from(per_minute.max(1)),
            at: Instant::now(),
        }
    }
}

type Slot = Arc<Mutex<Option<LogState>>>;

struct Witness {
    config: Config,
    cosigner: Cosigner,
    vkey: String,
    slots: std::sync::Mutex<HashMap<String, Slot>>,
    logs: AtomicUsize,
    buckets: std::sync::Mutex<HashMap<String, Bucket>>,
    enrolments: std::sync::Mutex<Bucket>,
    tls: Arc<rustls::ClientConfig>,
}

fn file_stem(origin: &str) -> String {
    hex(&Sha256::digest(origin.as_bytes()))
}

fn logs_directory(state_directory: &Path) -> PathBuf {
    state_directory.join("logs")
}

/// Writes `bytes` to `path` atomically: a synced temporary file, renamed, and the
/// directory synced.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("no directory"))?;
    let temporary = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&temporary)?;
        io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(directory)?.sync_all()
}

impl Witness {
    fn state_path(&self, origin: &str) -> PathBuf {
        logs_directory(&self.config.state_directory).join(format!("{}.state", file_stem(origin)))
    }

    fn note_path(&self, origin: &str) -> PathBuf {
        logs_directory(&self.config.state_directory).join(format!("{}.note", file_stem(origin)))
    }

    fn slot(&self, origin: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        // Slots are tiny; drop the unused ones if the map grows far past the limit.
        if slots.len() > 4 * self.config.max_logs + 1024 {
            slots.retain(|_, slot| {
                Arc::strong_count(slot) > 1 || slot.try_lock().is_ok_and(|s| s.is_some())
            });
        }
        slots.entry(origin.to_owned()).or_default().clone()
    }

    fn load(&self, origin: &str) -> io::Result<Option<LogState>> {
        match std::fs::read(self.state_path(origin)) {
            Ok(bytes) => {
                let state = LogState::decode(&bytes)
                    .map_err(|e| io::Error::other(format!("state of {origin}: {e}")))?;
                if state.origin != origin {
                    return Err(io::Error::other(format!(
                        "the state file of {origin} names another origin"
                    )));
                }
                Ok(Some(state))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn limited(&self, origin: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() > 4 * self.config.max_logs + 1024 {
            buckets.clear();
        }
        let per_minute = self.config.submissions_per_minute;
        !buckets
            .entry(origin.to_owned())
            .or_insert_with(|| Bucket::full(per_minute))
            .take(per_minute)
    }

    async fn add(&self, body: &[u8]) -> Response {
        let request = match AddEpochs::decode(body) {
            Ok(request) => request,
            Err(error) => return Response::text(400, format!("malformed submission: {error}\n")),
        };
        let origin = request.origin.clone();
        if !origin.ends_with(ORIGIN_SUFFIX) {
            return Response::text(400, "the origin must end in /nuschel-kt/v1\n");
        }
        if self.limited(&origin) {
            return Response::text(429, "too many submissions for this origin\n");
        }
        let slot = self.slot(&origin);
        let mut held = slot.lock().await;
        if held.is_none() {
            match self.load(&origin) {
                Ok(state) => *held = state,
                Err(error) => {
                    tracing::error!(%origin, %error, "cannot read the state");
                    return Response::text(500, "internal error\n");
                }
            }
        }
        let new = held.is_none();
        if new && request.old_size == 0 {
            if !self.config.accept_new_logs {
                return Response::text(403, "this witness accepts no new logs\n");
            }
            if self.logs.load(Ordering::Relaxed) >= self.config.max_logs {
                return Response::text(403, "this witness holds as many logs as it may\n");
            }
            let allowed = self
                .enrolments
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(self.config.enrolments_per_minute);
            if !allowed {
                return Response::text(429, "too many new logs; try again later\n");
            }
            if let Err(response) = self.enrol(&origin, &request.descriptor).await {
                return response;
            }
        }
        let state = held.clone();
        let submitted = request.checkpoint.clone();
        let checked =
            tokio::task::spawn_blocking(move || add_epochs(state.as_ref(), &request)).await;
        let accepted = match checked {
            Ok(Ok(accepted)) => accepted,
            Ok(Err(AddError::Conflict { size })) => {
                return Response {
                    status: 409,
                    content_type: "text/x.tlog.size",
                    body: format!("{size}\n").into_bytes(),
                };
            }
            Ok(Err(error)) => {
                if matches!(error, AddError::Forbidden(_)) {
                    tracing::warn!(%origin, %error, "refused a submission (possible evidence)");
                } else {
                    tracing::info!(%origin, %error, "refused a submission");
                }
                return Response::text(error.status(), format!("{error}\n"));
            }
            Err(_) => return Response::text(500, "internal error\n"),
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let cosignature = match self.cosigner.cosign(&accepted.checkpoint, timestamp) {
            Ok(line) => line,
            Err(error) => {
                tracing::error!(%error, "cannot cosign");
                return Response::text(500, "internal error\n");
            }
        };
        let mut state = accepted.state;
        state.timestamp = timestamp;
        let saved = state
            .encode()
            .map_err(|e| io::Error::other(e.to_string()))
            .and_then(|bytes| write_atomically(&self.state_path(&origin), &bytes));
        if let Err(error) = saved {
            tracing::error!(%origin, %error, "cannot save the state; not cosigning");
            return Response::text(500, "internal error\n");
        }
        // The newest cosigned checkpoint, for monitors (best effort).
        let mut published = Note {
            text: accepted.checkpoint.body(),
            signatures: Vec::new(),
        };
        if let Ok(note) = Note::parse(&submitted) {
            published.signatures = note.signatures;
        }
        published.signatures.push(cosignature.clone());
        let _ = write_atomically(&self.note_path(&origin), published.format().as_bytes());
        if new {
            self.logs.fetch_add(1, Ordering::Relaxed);
            tracing::info!(%origin, size = state.size(), "enrolled a log");
        } else {
            tracing::debug!(%origin, size = state.size(), "cosigned");
        }
        *held = Some(state);
        Response {
            status: 200,
            content_type: "text/x.tlog.cosignature",
            body: cosignature.line().into_bytes(),
        }
    }

    /// The enrolment check: the server at the origin's `host:port` must announce
    /// this log in its Nuschel hello.
    async fn enrol(&self, origin: &str, descriptor: &[u8]) -> Result<(), Response> {
        let refused = |status, text: String| {
            tracing::info!(%origin, reason = %text, "refused to enrol a log");
            Response::text(status, format!("{text}\n"))
        };
        let address = origin.strip_suffix(ORIGIN_SUFFIX).unwrap_or(origin);
        let Some((host, port)) = address.rsplit_once(':') else {
            return Err(refused(400, "the origin has no port".into()));
        };
        let Ok(port) = port.parse::<u16>() else {
            return Err(refused(400, "the origin's port is not a number".into()));
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        match timeout(ENROL_TIMEOUT, self.announced_log(host, port)).await {
            Ok(Ok(announced)) if announced == descriptor => Ok(()),
            Ok(Ok(_)) => Err(refused(
                403,
                format!("the server at {address} announces another key-transparency log"),
            )),
            Ok(Err(error)) => Err(refused(502, format!("cannot check {address}: {error}"))),
            Err(_) => Err(refused(504, format!("cannot check {address}: timed out"))),
        }
    }

    /// Connects to `host:port` as a Nuschel client and returns the `kt_log` its hello
    /// announces.
    async fn announced_log(&self, host: &str, port: u16) -> io::Result<Vec<u8>> {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await?
            .filter(|a| self.config.enrol_private_addresses || public(a.ip()))
            .collect();
        let Some(address) = addresses.first() else {
            return Err(io::Error::other("no public address"));
        };
        let stream = TcpStream::connect(address).await?;
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| io::Error::other("not a host name"))?;
        let connector = tokio_rustls::TlsConnector::from(self.tls.clone());
        let mut tls = connector.connect(name, stream).await?;
        let mut header = [0u8; nuschel_protocol::HEADER_BYTES];
        tls.read_exact(&mut header).await?;
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let length = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
        if kind != nuschel_protocol::kind::VERSION || length > MAX_VERSION_FRAME {
            return Err(io::Error::other(
                "the server did not start with its version",
            ));
        }
        let mut body = vec![0; length];
        tls.read_exact(&mut body).await?;
        let hello = nuschel_protocol::nuschel::hello(&body)
            .map_err(|_| io::Error::other("a malformed Nuschel hello"))?
            .ok_or_else(|| io::Error::other("not a Nuschel server"))?;
        if hello.kt_log.is_empty() {
            return Err(io::Error::other("the server offers no key transparency"));
        }
        Ok(hello.kt_log)
    }
}

/// Whether enrolment may connect to `ip` without `enrol_private_addresses`.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
                && v6.to_ipv4_mapped().is_none_or(|v4| public(IpAddr::V4(v4)))
        }
    }
}

// ---- TLS for enrolment ----

/// Servers use self-signed certificates and clients pin them on first use; the
/// enrolment check needs only what the network shows at `host:port`, so any
/// certificate is accepted (the handshake's signature is still checked).
#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config() -> io::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::other(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    config.alpn_protocols = vec![nuschel_protocol::nuschel::ALPN.to_vec()];
    Ok(Arc::new(config))
}

// ---- HTTP ----

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            411 => "Length Required",
            413 => "Content Too Large",
            422 => "Unprocessable Content",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            502 => "Bad Gateway",
            504 => "Gateway Timeout",
            _ => "Internal Server Error",
        };
        let mut out = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// A request head and as much of the body as came with it.
struct Request {
    method: String,
    path: String,
    length: Option<usize>,
    chunked: bool,
    rest: Vec<u8>,
}

async fn read_head(stream: &mut TcpStream) -> Result<Request, Response> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut request = httparse::Request::new(&mut headers);
            match request.parse(&buffer[..end + 4]) {
                Ok(httparse::Status::Complete(_)) => {}
                _ => return Err(Response::text(400, "malformed request\n")),
            }
            let mut length = None;
            let mut chunked = false;
            for header in request.headers.iter() {
                if header.name.eq_ignore_ascii_case("content-length") {
                    let value = std::str::from_utf8(header.value)
                        .ok()
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .ok_or_else(|| Response::text(400, "bad Content-Length\n"))?;
                    if length.is_some_and(|l| l != value) {
                        return Err(Response::text(400, "two Content-Lengths\n"));
                    }
                    length = Some(value);
                } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = true;
                }
            }
            return Ok(Request {
                method: request.method.unwrap_or_default().to_owned(),
                path: request.path.unwrap_or_default().to_owned(),
                length,
                chunked,
                rest: buffer[end + 4..].to_vec(),
            });
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(Response::text(431, "request head too large\n"));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| Response::text(400, "\n"))?;
        if n == 0 {
            return Err(Response::text(400, "incomplete request\n"));
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

async fn handle(witness: &Witness, stream: &mut TcpStream) -> Response {
    let request = match timeout(HEAD_TIMEOUT, read_head(stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(response)) => return response,
        Err(_) => return Response::text(400, "timed out\n"),
    };
    let path = request.path.split('?').next().unwrap_or_default();
    match (request.method.as_str(), path) {
        ("GET", "/") => Response::text(200, format!("nuschel-witness\n{}\n", witness.vkey)),
        ("GET", VKEY_PATH) => Response::text(200, format!("{}\n", witness.vkey)),
        ("GET", path) if path.starts_with(CHECKPOINTS_PATH) => {
            let stem = &path[CHECKPOINTS_PATH.len()..];
            if stem.len() != 64 || !stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Response::text(404, "no such log\n");
            }
            match std::fs::read(
                logs_directory(&witness.config.state_directory).join(format!("{stem}.note")),
            ) {
                Ok(note) => Response {
                    status: 200,
                    content_type: "text/plain; charset=utf-8",
                    body: note,
                },
                Err(_) => Response::text(404, "no such log\n"),
            }
        }
        ("POST", ADD_EPOCHS_PATH) => {
            if request.chunked {
                return Response::text(411, "send a Content-Length\n");
            }
            let Some(length) = request.length else {
                return Response::text(411, "send a Content-Length\n");
            };
            if length > witness.config.max_request_bytes || request.rest.len() > length {
                return Response::text(413, "the submission is too large\n");
            }
            let mut body = request.rest;
            let missing = length - body.len();
            let start = body.len();
            body.resize(length, 0);
            match timeout(
                BODY_TIMEOUT,
                stream.read_exact(&mut body[start..start + missing]),
            )
            .await
            {
                Ok(Ok(_)) => witness.add(&body).await,
                _ => Response::text(400, "incomplete body\n"),
            }
        }
        (_, ADD_EPOCHS_PATH | VKEY_PATH | "/") => Response::text(405, "method not allowed\n"),
        _ => Response::text(404, "not found\n"),
    }
}

/// A running witness.
pub struct Running {
    pub address: SocketAddr,
    pub vkey: String,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Serves until the task is aborted.
    pub async fn wait(mut self) {
        let _ = (&mut self.task).await;
    }

    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Opens the state directory, binds the listener and serves.
pub async fn start(config: Config) -> io::Result<Running> {
    if !note::valid_name(&config.name) {
        return Err(io::Error::other(format!(
            "{:?} is not a valid cosigner name",
            config.name
        )));
    }
    std::fs::create_dir_all(logs_directory(&config.state_directory))?;
    let cosigner = cosigner(&config)?;
    let vkey = cosigner.key().vkey();
    let logs = std::fs::read_dir(logs_directory(&config.state_directory))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "state"))
        .count();
    let listener = TcpListener::bind(config.listen).await?;
    let address = listener.local_addr()?;
    tracing::info!(%address, %vkey, logs, "nuschel-witness listening");
    let witness = Arc::new(Witness {
        enrolments: std::sync::Mutex::new(Bucket::full(config.enrolments_per_minute)),
        config,
        cosigner,
        vkey: vkey.clone(),
        slots: Default::default(),
        logs: AtomicUsize::new(logs),
        buckets: Default::default(),
        tls: tls_config()?,
    });
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let Ok(permit) = connections.clone().try_acquire_owned() else {
                let _ = stream
                    .write_all(&Response::text(503, "busy\n").encode())
                    .await;
                continue;
            };
            let witness = witness.clone();
            tokio::spawn(async move {
                let response = handle(&witness, &mut stream).await;
                let _ = timeout(HEAD_TIMEOUT, async {
                    let _ = stream.write_all(&response.encode()).await;
                    let _ = stream.shutdown().await;
                })
                .await;
                drop(permit);
            });
        }
    });
    Ok(Running {
        address,
        vkey,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_are_raw_or_hex_and_generated_once_privately() {
        assert_eq!(*parse_seed(&[7; 32]).unwrap(), [7; 32]);
        assert_eq!(
            *parse_seed(format!("{}\n", "0a".repeat(32)).as_bytes()).unwrap(),
            [10; 32]
        );
        assert!(parse_seed(b"short").is_err());
        let directory = tempfile::tempdir().unwrap();
        let first = load_seed(None, directory.path()).unwrap();
        assert_eq!(*first, *load_seed(None, directory.path()).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(directory.path().join("witness.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn enrolment_refuses_private_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!public(private.parse().unwrap()), "{private}");
        }
        for open in ["93.184.216.34", "2001:4860:4860::8888"] {
            assert!(public(open.parse().unwrap()), "{open}");
        }
    }
}
