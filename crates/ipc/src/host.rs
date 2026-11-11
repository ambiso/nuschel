// SPDX-License-Identifier: EUPL-1.2
//! Native supervisor adapter. The portable client model does not depend on this module.

use std::{ffi::OsString, io, path::Path, process::Stdio, time::Duration};

use nuschel_core::MessageDocument;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::timeout,
};

use crate::{
    ContentRequest, ContentResponse, EncodedImage, Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    PreparedContent, write_frame,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] crate::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("content worker exceeded its deadline")]
    Timeout,
    #[error("content worker returned an invalid response or exited unsuccessfully")]
    InvalidResponse,
    #[error("content processing rejected: {0}")]
    Rejected(String),
    #[error("required sandbox helper {0} is unavailable")]
    MissingSandboxHelper(&'static str),
}

/// Build the content sandbox. Only trusted supervisors choose `executable` and `arguments`.
/// This is public so the VM confinement test exercises the same command as the application.
pub fn content_sandbox_command(
    executable: &Path,
    arguments: &[OsString],
) -> Result<Command, Error> {
    content_sandbox(executable, arguments, 5)
}

/// The math worker (`crate::math`): confined like the content worker, with
/// no network, no files beyond the runtime libraries and itself, and an empty
/// environment. It is long-lived, so its CPU limit covers its whole life;
/// each formula has its own deadline.
#[cfg(target_os = "linux")]
pub fn math_sandbox_command(executable: &Path) -> Result<Command, Error> {
    let limits = [CONTENT_MEMORY.into(), "--cpu=120".into()];
    Ok(Sandbox::new(executable, &limits)?.run(std::iter::empty::<&str>()))
}

/// The math worker under the content worker's Seatbelt profile.
#[cfg(target_os = "macos")]
pub fn math_sandbox_command(executable: &Path) -> Result<Command, Error> {
    sandbox_exec(seatbelt::CONTENT, executable, &[])
}

/// The math worker in an AppContainer of its own, with the content worker's
/// memory limit and, as on Linux, 120 CPU seconds over its whole life.
#[cfg(windows)]
pub fn math_sandbox_command(executable: &Path) -> Result<Command, Error> {
    Ok(windows_worker(executable, &windows_policies::math()))
}

/// Seconds of wall-clock time, and on Linux of CPU time, a request may take.
fn budget(request: &ContentRequest) -> u64 {
    match request {
        // Several encodes of a large picture; the user waits for it.
        ContentRequest::EncodeImage { .. } => 15,
        _ => 5,
    }
}

/// Render literal message text.
pub async fn render_content(executable: &Path, text: &str) -> Result<MessageDocument, Error> {
    rendered(
        one(
            executable,
            ContentRequest::Render {
                text: text.to_owned(),
            },
        )
        .await?,
    )
}

/// Render many messages (a history page) in one worker; `None` where one failed.
pub async fn render_many(executable: &Path, texts: &[String]) -> Vec<Option<MessageDocument>> {
    let requests: Vec<_> = texts
        .iter()
        .map(|text| ContentRequest::Render { text: text.clone() })
        .collect();
    match exchange(executable, &requests).await {
        Ok(results) => results
            .into_iter()
            .map(|result| result.and_then(rendered).ok())
            .collect(),
        Err(_) => vec![None; texts.len()],
    }
}

/// Prepare many messages (received in a burst) in one worker, each as
/// [`prepare_content`] would: `(text, legacy_html)` in, `None` where one failed.
pub async fn prepare_contents(
    executable: &Path,
    messages: &[(String, bool)],
) -> Vec<Option<PreparedContent>> {
    let requests: Vec<_> = messages
        .iter()
        .map(|(text, legacy_html)| match legacy_html {
            true => ContentRequest::LegacyHtml { html: text.clone() },
            false => ContentRequest::Render { text: text.clone() },
        })
        .collect();
    match exchange(executable, &requests).await {
        Ok(results) => results
            .into_iter()
            .zip(messages)
            .map(
                |(result, (text, legacy_html))| match (result, legacy_html) {
                    (Ok(ContentResponse::Prepared(prepared)), true) => Some(prepared),
                    (Ok(ContentResponse::Rendered(document)), false) => Some(PreparedContent {
                        source: text.clone(),
                        document,
                    }),
                    _ => None,
                },
            )
            .collect(),
        Err(_) => vec![None; messages.len()],
    }
}

/// Decode an avatar in the sandboxed content worker; returns an Image document.
pub async fn decode_avatar(executable: &Path, encoded: &[u8]) -> Result<MessageDocument, Error> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(encoded);
    rendered(one(executable, ContentRequest::Avatar { image }).await?)
}

/// Decode a server's website icon in the sandboxed content worker.
pub async fn decode_icon(executable: &Path, encoded: &[u8]) -> Result<MessageDocument, Error> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(encoded);
    rendered(one(executable, ContentRequest::Icon { image }).await?)
}

/// Decode an inline image at the viewer's size in the sandboxed content worker.
pub async fn decode_view(executable: &Path, uri: &str) -> Result<MessageDocument, Error> {
    let uri = uri.to_owned();
    rendered(one(executable, ContentRequest::View { uri }).await?)
}

/// Re-encode a pasted image as a Mumble `<img>` element of at most `limit`
/// bytes, with a thumbnail for the composer.
pub async fn encode_image(
    executable: &Path,
    image: &[u8],
    limit: u32,
) -> Result<EncodedImage, Error> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(image);
    match one(executable, ContentRequest::EncodeImage { image, limit }).await? {
        ContentResponse::Encoded(encoded) => Ok(encoded),
        _ => Err(Error::InvalidResponse),
    }
}

/// Literal text, or (`legacy_html`) Mumble rich text made inert, with its plain source.
pub async fn prepare_content(
    executable: &Path,
    text: &str,
    legacy_html: bool,
) -> Result<PreparedContent, Error> {
    if !legacy_html {
        let document = render_content(executable, text).await?;
        return Ok(PreparedContent {
            source: text.to_owned(),
            document,
        });
    }
    let html = text.to_owned();
    match one(executable, ContentRequest::LegacyHtml { html }).await? {
        ContentResponse::Prepared(prepared) => Ok(prepared),
        _ => Err(Error::InvalidResponse),
    }
}

fn rendered(response: ContentResponse) -> Result<MessageDocument, Error> {
    match response {
        ContentResponse::Rendered(document) => Ok(document),
        _ => Err(Error::InvalidResponse),
    }
}

async fn one(executable: &Path, request: ContentRequest) -> Result<ContentResponse, Error> {
    exchange(executable, std::slice::from_ref(&request))
        .await?
        .pop()
        .unwrap_or(Err(Error::InvalidResponse))
}

/// Answer `requests` in order, all in one sandboxed worker unless one fails.
/// Every request has its own budget: its reply must arrive within that many
/// seconds of the previous one, and on Linux the worker's CPU limit is the sum.
/// A worker that misses a deadline, exits early or answers out of turn is
/// killed and reaped; the request it was working on fails and the rest go to
/// a fresh worker. `Err` means no worker could be started at all.
pub async fn exchange(
    executable: &Path,
    requests: &[ContentRequest],
) -> Result<Vec<Result<ContentResponse, Error>>, Error> {
    let mut frames = Vec::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        let mut frame = Vec::new();
        write_frame(&mut frame, &Envelope::new(index as u64 + 1, request))?;
        frames.push(frame);
    }
    let budgets: Vec<u64> = requests.iter().map(budget).collect();
    let mut results = Vec::with_capacity(requests.len());
    while results.len() < requests.len() {
        let first = results.len();
        let outcome = match spawn(executable, &budgets[first..]) {
            Ok(child) => converse(child, &frames[first..], &budgets[first..], &mut results).await,
            Err(error) if first == 0 => return Err(error),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            results.push(Err(error));
        }
    }
    Ok(results)
}

fn spawn(executable: &Path, budgets: &[u64]) -> Result<Child, Error> {
    Ok(content_sandbox(executable, &[], budgets.iter().sum())?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?)
}

/// Send `frames` to one worker and collect its replies into `results`, whose
/// length is the request id of the first frame minus one.
async fn converse(
    mut child: Child,
    frames: &[Vec<u8>],
    budgets: &[u64],
    results: &mut Vec<Result<ContentResponse, Error>>,
) -> Result<(), Error> {
    let _containment = contain(&child)?;
    let (Some(mut input), Some(mut output)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(Error::InvalidResponse);
    };
    let outcome = {
        // Written alongside reading, so neither pipe can fill up and stall
        // the worker. Closing stdin afterwards tells it to exit.
        let write = async move {
            for frame in frames {
                input.write_all(frame).await?;
            }
            io::Result::Ok(())
        };
        let read = async {
            for budget in budgets {
                let reply = timeout(Duration::from_secs(*budget), read_reply(&mut output))
                    .await
                    .map_err(|_| Error::Timeout)??;
                if reply.request_id != results.len() as u64 + 1 {
                    return Err(Error::InvalidResponse);
                }
                results.push(match reply.payload {
                    ContentResponse::Rejected(reason) => Err(Error::Rejected(reason)),
                    response => Ok(response),
                });
            }
            // A worker that does not then exit cleanly: its last reply is not trusted.
            match timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) if status.success() => Ok(()),
                exited => {
                    results.pop();
                    Err(match exited {
                        Err(_) => Error::Timeout,
                        Ok(Err(error)) => error.into(),
                        Ok(Ok(_)) => Error::InvalidResponse,
                    })
                }
            }
        };
        tokio::pin!(read);
        tokio::select! {
            outcome = &mut read => outcome,
            // A worker that died early fails the read at the request it was on.
            _ = write => read.await,
        }
    };
    if outcome.is_err() {
        // kill_on_drop is a fallback; explicitly reap while the runtime still exists.
        child.kill().await.ok();
    }
    outcome
}

async fn read_reply(
    output: &mut tokio::process::ChildStdout,
) -> Result<Envelope<ContentResponse>, Error> {
    let size = output.read_u32().await? as usize;
    if size > MAX_FRAME_BYTES {
        return Err(Error::Frame(crate::Error::TooLarge));
    }
    let mut bytes = vec![0; size];
    output.read_exact(&mut bytes).await?;
    let reply: Envelope<ContentResponse> = serde_json::from_slice(&bytes)?;
    if reply.version != PROTOCOL_VERSION {
        return Err(Error::InvalidResponse);
    }
    Ok(reply)
}

/// OS containment applied to a worker after it starts, held for its lifetime.
/// On Windows this is a job object that kills the worker's launch stage when
/// Nuschel lets go of it (including when Nuschel itself exits); the launch
/// stage's own job then ends the confined worker. Elsewhere the launch
/// command already carries the confinement.
pub struct Containment {
    #[cfg(windows)]
    _job: win32job::Job,
}

#[cfg(windows)]
pub fn contain(child: &tokio::process::Child) -> Result<Containment, Error> {
    let handle = child.raw_handle().ok_or(Error::InvalidResponse)?;
    let mut limits = win32job::ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&limits)
        .map_err(|error| Error::Io(io::Error::other(error.to_string())))?;
    job.assign_process(handle as isize)
        .map_err(|error| Error::Io(io::Error::other(error.to_string())))?;
    Ok(Containment { _job: job })
}

#[cfg(not(windows))]
pub fn contain(_: &tokio::process::Child) -> Result<Containment, Error> {
    Ok(Containment {})
}

/// Long-lived SQLite helper: its history folder (`Profile::history_dir`,
/// `$XDG_DATA_HOME/nuschel/history`, which holds nothing else) is writable at
/// `/data`; network and the rest of the home directory are absent. The trusted
/// supervisor chooses the path.
#[cfg(target_os = "linux")]
pub fn storage_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let mut sandbox = Sandbox::new(executable, &[PERSISTENT_MEMORY.into()])?;
    sandbox
        .0
        .arg("--bind")
        .arg(directory.canonicalize()?)
        .arg("/data");
    Ok(sandbox.run(["--database", "/data/history.sqlite3"]))
}

/// The crypto worker: its own folder (`Profile::keys_dir`,
/// `$XDG_DATA_HOME/nuschel/keys`: the sealed keystore, its state and, for now,
/// the store key file) is writable at `/data`; no network, no home, no D-Bus,
/// and not the history or the Mumble identity beside it. The trusted
/// supervisor chooses the folder.
#[cfg(target_os = "linux")]
pub fn crypto_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let mut sandbox = Sandbox::new(executable, &[PERSISTENT_MEMORY.into()])?;
    sandbox
        .0
        .arg("--bind")
        .arg(directory.canonicalize()?)
        .arg("/data");
    Ok(sandbox.run(["--data", "/data"]))
}

/// Audio service sockets and ALSA devices only. No session credentials, home,
/// D-Bus socket, or network namespace are granted to native audio/codec processing.
#[cfg(target_os = "linux")]
pub fn media_sandbox_command(executable: &Path) -> Result<Command, Error> {
    let mut sandbox = Sandbox::new(executable, &[PERSISTENT_MEMORY.into()])?;
    let command = &mut sandbox.0;
    command.args(["--dir", "/run/audio", "--dir", "/run/audio/pulse"]);
    // PulseAudio's directory is bound rather than its socket, so a restarted
    // server's new socket shows up inside. PipeWire's socket sits directly in
    // the runtime directory: after a restart the worker reports the failure
    // and its replacement binds the new socket.
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = std::path::PathBuf::from(runtime);
        for (source, target) in [
            (runtime.join("pipewire-0"), "/run/audio/pipewire-0"),
            (runtime.join("pulse"), "/run/audio/pulse"),
        ] {
            // A file deleted under a bind mount (the sound server restarted
            // since a container was set up) still "exists", but bwrap cannot
            // bind it and the whole sandbox would fail to start.
            use std::os::unix::fs::MetadataExt as _;
            if std::fs::metadata(&source).is_ok_and(|metadata| metadata.nlink() > 0) {
                command.arg("--ro-bind").arg(source).arg(target);
            }
        }
    }
    if Path::new("/dev/snd").exists() {
        command.args(["--dev-bind", "/dev/snd", "/dev/snd"]);
    }
    for directory in ["/sys/devices", "/sys/class/sound", "/proc/asound"] {
        if Path::new(directory).exists() {
            command.args(["--ro-bind", directory, directory]);
        }
    }
    command.args([
        "--setenv",
        "XDG_RUNTIME_DIR",
        "/run/audio",
        "--setenv",
        "PULSE_SERVER",
        "unix:/run/audio/pulse/native",
    ]);
    for name in ["PULSE_SOURCE", "PULSE_SINK", "NUSCHEL_AUDIO_BACKEND"] {
        if let Some(value) = std::env::var_os(name) {
            command.arg("--setenv").arg(name).arg(value);
        }
    }
    Ok(sandbox.run(["--live"]))
}

/// magic-wormhole transfers: the network (rendezvous server, transit relay,
/// peers' direct addresses) and name resolution, and when offering, the one
/// file at `/input`, read-only. Nothing else of the filesystem: a received
/// file streams back to the supervisor, which writes it where the user chose.
#[cfg(target_os = "linux")]
pub fn transfer_sandbox_command(executable: &Path, file: Option<&Path>) -> Result<Command, Error> {
    let mut sandbox = Sandbox::new(executable, &[PERSISTENT_MEMORY.into()])?;
    let command = &mut sandbox.0;
    command.arg("--share-net");
    for name in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/gai.conf",
    ] {
        if Path::new(name).exists() {
            command.args(["--ro-bind", name, name]);
        }
    }
    let arguments: &[&str] = match file {
        Some(file) => {
            command
                .arg("--ro-bind")
                .arg(file.canonicalize()?)
                .arg("/input");
            &["--file", "/input"]
        }
        None => &[],
    };
    Ok(sandbox.run(arguments))
}

#[cfg(target_os = "linux")]
fn content_sandbox(
    executable: &Path,
    arguments: &[OsString],
    cpu_seconds: u64,
) -> Result<Command, Error> {
    let limits = [CONTENT_MEMORY.into(), format!("--cpu={cpu_seconds}")];
    Ok(Sandbox::new(executable, &limits)?.run(arguments))
}

/// Address-space limits (prlimit) for one-shot content workers and for the
/// long-lived storage and media workers, which get no CPU-time limit.
#[cfg(target_os = "linux")]
const CONTENT_MEMORY: &str = "--as=536870912";
#[cfg(target_os = "linux")]
const PERSISTENT_MEMORY: &str = "--as=1073741824";

/// The bubblewrap sandbox every Linux worker starts from, under prlimit: all
/// namespaces unshared (no network), read-only system libraries, private
/// /proc, /dev and /tmp, the worker at `/worker`, and an empty environment.
/// Callers add their own bwrap options to `.0`; [`Sandbox::run`] ends them.
#[cfg(target_os = "linux")]
struct Sandbox(Command);

#[cfg(target_os = "linux")]
impl Sandbox {
    fn new(executable: &Path, limits: &[String]) -> Result<Self, Error> {
        fn helper(name: &'static str) -> Result<std::path::PathBuf, Error> {
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join(name))
                .find(|path| path.is_file())
                .ok_or(Error::MissingSandboxHelper(name))
        }

        let mut command = Command::new(helper("prlimit")?);
        command.args(limits).arg("--").arg(helper("bwrap")?);
        command.args(["--die-with-parent", "--new-session", "--unshare-all"]);
        for directory in ["/nix/store", "/usr/lib", "/usr/lib64", "/lib", "/lib64"] {
            if Path::new(directory).exists() {
                command.args(["--ro-bind", directory, directory]);
            }
        }
        command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        command
            .arg("--ro-bind")
            .arg(executable.canonicalize()?)
            .arg("/worker");
        command.args(["--chdir", "/tmp", "--clearenv"]);
        command.env_clear();
        Ok(Self(command))
    }

    /// The worker command line, after every bwrap option.
    fn run(mut self, arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> Command {
        self.0.args(["--", "/worker"]).args(arguments);
        self.0
    }
}

/// macOS: Seatbelt profiles applied with `sandbox-exec`. Content and storage
/// workers are pure Rust and start from deny-by-default on top of Apple's base
/// `system.sb`. The media worker needs CoreAudio's undocumented set of Mach
/// services, so it is confined by a deny list instead (no network, no writes
/// outside temporary directories, no reads of the home directory beyond audio
/// preferences) until it can be narrowed on real hardware.
#[cfg(any(target_os = "macos", test))]
mod seatbelt {
    pub const CONTENT: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-exec (literal (param "WORKER")))
(allow file-read* (literal (param "WORKER")))
"#;

    pub const STORAGE: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-exec (literal (param "WORKER")))
(allow file-read* (literal (param "WORKER")))
(allow file-read* file-write* (subpath (param "DATA")))
"#;

    pub const MEDIA: &str = r#"(version 1)
(allow default)
(deny network*)
(deny process-exec)
(allow process-exec (literal (param "WORKER")))
(deny file-write*)
(allow file-write* (subpath "/dev") (subpath "/private/tmp") (subpath "/private/var/folders"))
(deny file-read* (subpath (param "HOME")))
(allow file-read* (subpath (param "AUDIO_PREFERENCES")) (subpath (param "PREFERENCES")))
"#;

    /// Network and name resolution (mDNSResponder), and only the offered
    /// `FILE` (`/dev/null` when receiving) to read.
    pub const TRANSFER: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-exec (literal (param "WORKER")))
(allow file-read* (literal (param "WORKER")) (literal (param "FILE")))
(allow system-socket)
(allow network-bind network-inbound network-outbound)
(allow mach-lookup (global-name "com.apple.dnssd.service"))
"#;
}

#[cfg(target_os = "macos")]
fn sandbox_exec(
    profile: &str,
    executable: &Path,
    parameters: &[(&str, &Path)],
) -> Result<Command, Error> {
    let executable = executable.canonicalize()?;
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.arg("-p").arg(profile);
    command
        .arg("-D")
        .arg(format!("WORKER={}", executable.display()));
    for (name, value) in parameters {
        command.arg("-D").arg(format!("{name}={}", value.display()));
    }
    command.arg(&executable);
    command.env_clear();
    Ok(command)
}

/// No CPU-time limit here: the per-request deadline bounds the worker.
#[cfg(target_os = "macos")]
fn content_sandbox(executable: &Path, arguments: &[OsString], _: u64) -> Result<Command, Error> {
    let mut command = sandbox_exec(seatbelt::CONTENT, executable, &[])?;
    command.args(arguments);
    Ok(command)
}

#[cfg(target_os = "macos")]
pub fn storage_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let directory = directory.canonicalize()?;
    let mut command = sandbox_exec(seatbelt::STORAGE, executable, &[("DATA", &directory)])?;
    command
        .arg("--database")
        .arg(directory.join("history.sqlite3"));
    Ok(command)
}

#[cfg(target_os = "macos")]
pub fn crypto_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let directory = directory.canonicalize()?;
    let mut command = sandbox_exec(seatbelt::STORAGE, executable, &[("DATA", &directory)])?;
    command.arg("--data").arg(&directory);
    Ok(command)
}

#[cfg(target_os = "macos")]
pub fn media_sandbox_command(executable: &Path) -> Result<Command, Error> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").ok_or(Error::InvalidResponse)?);
    let mut command = sandbox_exec(
        seatbelt::MEDIA,
        executable,
        &[
            ("HOME", &home),
            ("PREFERENCES", &home.join("Library/Preferences")),
            ("AUDIO_PREFERENCES", &home.join("Library/Audio")),
        ],
    )?;
    command.arg("--live");
    if let Some(value) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", value);
    }
    Ok(command)
}

#[cfg(target_os = "macos")]
pub fn transfer_sandbox_command(executable: &Path, file: Option<&Path>) -> Result<Command, Error> {
    let file = match file {
        Some(file) => file.canonicalize()?,
        None => "/dev/null".into(),
    };
    let mut command = sandbox_exec(seatbelt::TRANSFER, executable, &[("FILE", &file)])?;
    if file != Path::new("/dev/null") {
        command.arg("--file").arg(&file);
    }
    Ok(command)
}

/// Windows: each worker runs in its own AppContainer (no capabilities unless
/// listed: no network, no access to the user's files) inside a job object,
/// started by its own executable's launch stage (see
/// [`crate::windows_sandbox`] and `nuschel-sandbox-windows`). The launch
/// stage itself runs with an empty environment apart from Windows' own
/// directories, without a console window, inside the kill-on-close job of
/// [`contain`]; it refuses to start a worker it cannot confine, as Linux
/// refuses without bwrap.
#[cfg(windows)]
fn windows_worker(executable: &Path, policy: &crate::windows_sandbox::Policy) -> Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(executable);
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.creation_flags(CREATE_NO_WINDOW);
    command.args(policy.arguments());
    command
}

/// Committed-memory limits of the Windows workers' jobs, as on Linux.
#[cfg(any(windows, test))]
mod windows_limits {
    pub const CONTENT: u64 = 512 << 20;
    pub const PERSISTENT: u64 = 1 << 30;
}

/// The AppContainer policies of the Windows workers, kept apart from process
/// creation so that they are checked on every platform.
#[cfg(any(windows, test))]
mod windows_policies {
    use super::windows_limits::{CONTENT, PERSISTENT};
    use crate::windows_sandbox::{Policy, scoped_profile};
    use std::path::Path;

    /// No capabilities, no files: decoding happens on what arrives over stdin.
    /// Content, storage and crypto are less privileged AppContainers (LPAC):
    /// they are not even given what Windows grants every AppContainer.
    pub fn content(cpu_seconds: u64) -> Policy {
        Policy {
            profile: "nuschel.content".into(),
            less_privileged: true,
            memory: CONTENT,
            cpu_seconds: Some(cpu_seconds),
            ..Policy::default()
        }
    }

    /// Like content (a less privileged AppContainer): no capabilities, no
    /// files; it parses untrusted LaTeX.
    /// It lives long, so its CPU limit covers its whole life (it is
    /// restarted when it runs out), as on Linux.
    pub fn math() -> Policy {
        Policy {
            profile: "nuschel.math".into(),
            less_privileged: true,
            memory: CONTENT,
            cpu_seconds: Some(120),
            ..Policy::default()
        }
    }

    /// Only the history folder (`Profile::history_dir`), read and write.
    pub fn storage(directory: &Path) -> Policy {
        Policy {
            profile: scoped_profile("nuschel.storage", directory),
            less_privileged: true,
            writable: vec![directory.to_owned()],
            memory: PERSISTENT,
            ..Policy::default()
        }
    }

    /// Only the crypto worker's folder (`Profile::keys_dir`: keystore, state,
    /// store key), read and write.
    pub fn crypto(directory: &Path) -> Policy {
        Policy {
            profile: scoped_profile("nuschel.crypto", directory),
            less_privileged: true,
            writable: vec![directory.to_owned()],
            memory: PERSISTENT,
            ..Policy::default()
        }
    }

    /// Audio endpoints through the Windows audio service; the microphone
    /// capability lets it capture. No network, no files. A full AppContainer:
    /// COM, which WASAPI needs, refuses a less privileged one.
    pub fn media() -> Policy {
        Policy {
            profile: "nuschel.media".into(),
            capabilities: vec!["microphone".into()],
            memory: PERSISTENT,
            ..Policy::default()
        }
    }

    /// Outgoing connections to the Internet (rendezvous, relay, peers) and
    /// connections to and from the local network (direct transit between
    /// peers on one LAN), but no inbound connections from the Internet; an
    /// offer reads its one file through a handle, never by path. A full
    /// AppContainer: Winsock refuses a less privileged one.
    pub fn transfer(file: Option<&Path>) -> Policy {
        Policy {
            profile: "nuschel.transfer".into(),
            capabilities: vec!["internetClient".into(), "privateNetworkClientServer".into()],
            file: file.map(Path::to_owned),
            memory: PERSISTENT,
            ..Policy::default()
        }
    }
}

#[cfg(windows)]
fn content_sandbox(
    executable: &Path,
    arguments: &[OsString],
    cpu_seconds: u64,
) -> Result<Command, Error> {
    let mut command = windows_worker(executable, &windows_policies::content(cpu_seconds));
    command.args(arguments);
    Ok(command)
}

#[cfg(windows)]
pub fn storage_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let mut command = windows_worker(executable, &windows_policies::storage(directory));
    command
        .arg("--database")
        .arg(directory.join("history.sqlite3"));
    Ok(command)
}

#[cfg(windows)]
pub fn crypto_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let mut command = windows_worker(executable, &windows_policies::crypto(directory));
    command.arg("--data").arg(directory);
    Ok(command)
}

#[cfg(windows)]
pub fn media_sandbox_command(executable: &Path) -> Result<Command, Error> {
    let mut command = windows_worker(executable, &windows_policies::media());
    command.arg("--live");
    Ok(command)
}

/// An offer's file reaches the worker as an inherited read-only handle
/// (`--file-handle N`, appended by the launch stage), never as a path.
#[cfg(windows)]
pub fn transfer_sandbox_command(executable: &Path, file: Option<&Path>) -> Result<Command, Error> {
    Ok(windows_worker(
        executable,
        &windows_policies::transfer(file),
    ))
}

#[cfg(test)]
mod tests {
    use super::seatbelt;

    fn balanced(profile: &str) -> bool {
        let mut depth = 0_i32;
        for character in profile.chars() {
            depth += match character {
                '(' => 1,
                ')' => -1,
                _ => 0,
            };
            if depth < 0 {
                return false;
            }
        }
        depth == 0
    }

    #[test]
    fn seatbelt_profiles_are_well_formed_and_restrictive() {
        for profile in [
            seatbelt::CONTENT,
            seatbelt::STORAGE,
            seatbelt::MEDIA,
            seatbelt::TRANSFER,
        ] {
            assert!(profile.starts_with("(version 1)"));
            assert!(balanced(profile), "{profile}");
            assert!(profile.contains(r#"(allow process-exec (literal (param "WORKER")))"#));
        }
        for profile in [seatbelt::CONTENT, seatbelt::STORAGE] {
            assert!(profile.contains("(deny default)"));
            assert!(
                !profile.contains("network"),
                "pure workers get no network rule at all"
            );
        }
        assert!(seatbelt::MEDIA.contains("(deny network*)"));
        assert!(seatbelt::MEDIA.contains("(deny file-write*)"));
        assert!(seatbelt::TRANSFER.contains("(deny default)"));
        assert!(!seatbelt::TRANSFER.contains("file-write"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_workers_share_one_bwrap_setup() {
        let executable = std::env::current_exe().unwrap();
        let arguments = |command: tokio::process::Command| -> Vec<String> {
            let arguments = command.as_std().get_args();
            arguments.map(|a| a.to_string_lossy().into()).collect()
        };
        let content = arguments(super::content_sandbox(&executable, &["x".into()], 7).unwrap());
        let storage = super::storage_sandbox_command(&executable, &std::env::temp_dir());
        let storage = arguments(storage.unwrap());
        let media = arguments(super::media_sandbox_command(&executable).unwrap());
        let crypto = super::crypto_sandbox_command(&executable, &std::env::temp_dir());
        let crypto = arguments(crypto.unwrap());
        let offer = super::transfer_sandbox_command(&executable, Some(&executable));
        let offer = arguments(offer.unwrap());
        let receive = arguments(super::transfer_sandbox_command(&executable, None).unwrap());
        assert_eq!(content[..3], ["--as=536870912", "--cpu=7", "--"]);
        assert_eq!(storage[..2], ["--as=1073741824", "--"]);
        assert_eq!(media[..2], ["--as=1073741824", "--"]);
        assert_eq!(crypto[..2], ["--as=1073741824", "--"]);
        for (command, worker) in [
            (&content, &["/worker", "x"][..]),
            (
                &storage,
                &["/worker", "--database", "/data/history.sqlite3"],
            ),
            (&media, &["/worker", "--live"]),
            (&crypto, &["/worker", "--data", "/data"]),
            (&offer, &["/worker", "--file", "/input"]),
            (&receive, &["/worker"]),
        ] {
            let end = command.iter().rposition(|a| a == "--").unwrap();
            assert_eq!(&command[end + 1..], worker);
            let options = &command[..end];
            let position = |option: &str| options.iter().position(|a| a == option);
            for option in ["--die-with-parent", "--unshare-all", "--clearenv"] {
                assert!(position(option).is_some(), "{option} in {options:?}");
            }
            // Variables are set after the environment is cleared.
            assert!(position("--setenv").is_none_or(|set| set > position("--clearenv").unwrap()));
            // Only transfers keep the network namespace.
            let network = std::ptr::eq(command, &offer) || std::ptr::eq(command, &receive);
            assert_eq!(position("--share-net").is_some(), network, "{options:?}");
        }
        // Storage and crypto each get exactly the folder they are given,
        // writable, and nothing else is writable from outside.
        let history = std::env::temp_dir().canonicalize().unwrap();
        let history = history.to_string_lossy();
        for (command, writable) in [
            (&content, None),
            (&storage, Some(&*history)),
            (&media, None),
            (&crypto, Some(&*history)),
            (&offer, None),
            (&receive, None),
        ] {
            let binds: Vec<&str> = command
                .windows(3)
                .filter(|option| option[0] == "--bind" || option[0] == "--dev-bind")
                .filter(|option| option[1] != "/dev/snd")
                .map(|option| option[1].as_str())
                .collect();
            assert_eq!(
                binds,
                writable.into_iter().collect::<Vec<_>>(),
                "{command:?}"
            );
        }
        // The offered file is the only path bound from outside the system.
        let input = offer.iter().position(|a| a == "/input").unwrap();
        assert_eq!(offer[input - 2], "--ro-bind");
        assert!(!receive.iter().any(|a| a == "/input"));
    }

    #[test]
    fn windows_policies_grant_only_what_each_worker_needs() {
        use super::{windows_limits, windows_policies as policy};
        use std::path::Path;
        // Profile::history_dir and keys_dir on Windows: siblings in the
        // local data folder.
        let history = Path::new(r"C:\Users\a\AppData\Local\Nuschel\Nuschel\data\history");
        let crypto = Path::new(r"C:\Users\a\AppData\Local\Nuschel\Nuschel\data\keys");
        let offer = Path::new(r"C:\Users\a\Documents\offer.zip");
        let content = policy::content(7);
        assert_eq!(content.profile, "nuschel.content");
        assert_eq!(content.cpu_seconds, Some(7));
        assert_eq!(content.memory, windows_limits::CONTENT);
        let math = policy::math();
        assert_eq!(math.profile, "nuschel.math");
        assert_eq!(math.cpu_seconds, Some(120));
        assert_eq!(math.memory, windows_limits::CONTENT);
        let storage = policy::storage(history);
        assert_eq!(storage.writable, [history]);
        assert!(storage.profile.starts_with("nuschel.storage."));
        let keys = policy::crypto(crypto);
        assert_eq!(keys.writable, [crypto]);
        assert!(keys.profile.starts_with("nuschel.crypto."));
        assert_ne!(keys.profile, policy::crypto(history).profile);
        // Not the container earlier versions granted the whole data folder.
        assert_ne!(
            storage.profile,
            policy::storage(history.parent().unwrap()).profile
        );
        let media = policy::media();
        assert_eq!(media.capabilities, ["microphone"]);
        let send = policy::transfer(Some(offer));
        let receive = policy::transfer(None);
        assert_eq!(send.file.as_deref(), Some(offer));
        assert_eq!(receive.file, None);
        for policy in [&content, &math, &storage, &keys, &media, &send, &receive] {
            // Only transfers reach the network; only storage and crypto get
            // a directory; every worker has a memory limit.
            let network = policy.capabilities.iter().any(|c| c.contains("Client"));
            assert_eq!(network, policy.profile == "nuschel.transfer", "{policy:?}");
            let directories = !policy.writable.is_empty();
            assert_eq!(
                directories,
                std::ptr::eq(policy, &storage) || std::ptr::eq(policy, &keys)
            );
            assert!(policy.memory >= windows_limits::CONTENT);
            assert!(
                policy
                    .capabilities
                    .iter()
                    .all(|c| c != "internetClientServer")
            );
        }
        assert!(media.cpu_seconds.is_none() && storage.cpu_seconds.is_none());
        // Everything but audio and the network runs as a less privileged AppContainer.
        for policy in [&content, &math, &storage, &keys] {
            assert!(policy.less_privileged, "{policy:?}");
        }
        assert!(!media.less_privileged && !send.less_privileged);
    }

    /// Runs only on macOS: the profiles must actually load and confine.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profiles_load_and_confine_on_macos() {
        use std::process::Command;
        let run = |profile: &str, program: &str, arguments: &[&str]| {
            Command::new("/usr/bin/sandbox-exec")
                .args(["-p", profile, "-D", &format!("WORKER={program}")])
                .args(["-D", "DATA=/private/tmp"])
                .arg(program)
                .args(arguments)
                .status()
                .unwrap()
                .success()
        };
        assert!(run(seatbelt::CONTENT, "/usr/bin/true", &[]));
        let home = std::env::var("HOME").unwrap();
        assert!(!run(
            seatbelt::CONTENT,
            "/usr/bin/touch",
            &[&format!("{home}/nuschel-sandbox-escape")]
        ));
    }
}
