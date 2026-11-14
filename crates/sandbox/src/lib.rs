// SPDX-License-Identifier: EUPL-1.2
//! How Nuschl confines its worker processes: one [`Role`] per kind of
//! worker, turned into a launch [`command`] for this platform, then started
//! and held by [`spawn`].
//!
//! - Linux: bubblewrap under prlimit (`linux`).
//! - macOS: Seatbelt profiles applied with `sandbox-exec` (`macos`).
//! - Windows: the worker's own launch stage puts it in an AppContainer and a
//!   job object (`windows`, [`windows_policy`]). That launch stage is the
//!   separate `nuschl-sandbox-windows` crate: it is the one first-party crate
//!   allowed unsafe code, and only the workers link it.
//!
//! Every builder refuses rather than start a worker unconfined. Only trusted
//! supervisors choose the executable and the role.

use std::{ffi::OsString, io, path::Path, process::Stdio};

use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(any(windows, test))]
mod windows;
pub mod windows_policy;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("required sandbox helper {0} is unavailable")]
    MissingSandboxHelper(&'static str),
    /// The worker started but could not be contained; it has exited.
    #[error("cannot contain the worker: {0}")]
    Uncontained(io::Error),
}

/// A kind of worker and what it is given beyond its standard input and output.
#[derive(Clone, Copy, Debug)]
pub enum Role<'a> {
    /// Bounded link-preview preparation or HTTP parsing, with a trusted Linux
    /// namespace-init receipt on standard error.
    Preview { operation: PreviewOperation },
    /// Untrusted message content, one-shot; CPU seconds over its whole life.
    /// `arguments` are the desktop's sandbox diagnostic's; workers get none.
    Content {
        cpu_seconds: u64,
        arguments: &'a [OsString],
    },
    /// LaTeX formulas; long-lived, so its CPU limit covers its whole life.
    Math,
    /// The update verifier, one-shot. `post_quantum` selects a distinct
    /// Windows profile, not an algorithm inside the executable.
    Update { post_quantum: bool },
    /// The music bot's codec children, started as `--codec-worker operation`.
    MusicbotCodec { operation: &'static str },
    /// History storage: only `directory`, writable.
    Storage { directory: &'a Path },
    /// The crypto worker: only `directory`, writable.
    Crypto { directory: &'a Path },
    /// Audio devices and the sound server.
    Media,
    /// magic-wormhole: the network and, when offering, `file` read-only.
    Transfer { file: Option<&'a Path> },
}

/// Fixed content-worker entry points used by the preview supervisors.
#[derive(Clone, Copy, Debug)]
pub enum PreviewOperation {
    /// One bounded content-protocol request, including document/image preparation.
    Prepare,
    /// Incremental HTTP response parsing through the preview-fetch protocol.
    Fetch,
}

/// What a role's worker may use beyond its standard input and output, a
/// private /tmp and the system libraries. Each platform module maps these to
/// its own mechanism and grants nothing else.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Capabilities<'a> {
    /// Windows AppContainer profile name, after `nuschl.`.
    name: &'static str,
    /// Address space (Linux) or committed memory (Windows) in bytes.
    memory: u64,
    /// CPU seconds over the worker's life (Linux and Windows).
    cpu_seconds: Option<u64>,
    /// One directory, readable and writable, with everything in it.
    writable: Option<&'a Path>,
    /// One file, read-only.
    readable: Option<&'a Path>,
    /// Audio devices and the sound server.
    audio: bool,
    /// Outgoing connections and the local network, with name resolution.
    network: bool,
    /// Windows: a less privileged AppContainer (LPAC), not even given what
    /// Windows grants every AppContainer. COM, which WASAPI needs, and
    /// Winsock refuse an LPAC.
    lpac: bool,
    /// Confinement designed and proven on Linux only; elsewhere it is refused.
    linux_only: bool,
}

const MIB: u64 = 1 << 20;

/// No capability: 1 GiB, no CPU limit, LPAC on Windows.
const NOTHING: Capabilities<'static> = Capabilities {
    name: "",
    memory: 1024 * MIB,
    cpu_seconds: None,
    writable: None,
    readable: None,
    audio: false,
    network: false,
    lpac: true,
    linux_only: false,
};

impl<'a> Role<'a> {
    /// What each role may do.
    #[rustfmt::skip]
    fn capabilities(self) -> Capabilities<'a> {
        match self {
            Role::Preview { .. } => Capabilities { name: "preview", memory: 512 * MIB, cpu_seconds: Some(5), linux_only: true, ..NOTHING },
            Role::Content { cpu_seconds, .. } => Capabilities { name: "content", memory: 512 * MIB, cpu_seconds: Some(cpu_seconds), ..NOTHING },
            Role::Math => Capabilities { name: "math", memory: 512 * MIB, cpu_seconds: Some(120), ..NOTHING },
            Role::Update { post_quantum: true } => Capabilities { name: "update.mldsa44", memory: 256 * MIB, cpu_seconds: Some(5), ..NOTHING },
            Role::Update { post_quantum: false } => Capabilities { name: "update.ed25519", memory: 256 * MIB, cpu_seconds: Some(5), ..NOTHING },
            Role::MusicbotCodec { .. } => Capabilities { name: "musicbot-codec", linux_only: true, ..NOTHING },
            Role::Storage { directory } => Capabilities { name: "storage", writable: Some(directory), ..NOTHING },
            Role::Crypto { directory } => Capabilities { name: "crypto", writable: Some(directory), ..NOTHING },
            Role::Media => Capabilities { name: "media", audio: true, lpac: false, ..NOTHING },
            Role::Transfer { file } => Capabilities { name: "transfer", network: true, readable: file, lpac: false, ..NOTHING },
        }
    }

    /// The worker's own arguments, given where it sees its writable
    /// directory and its readable file.
    fn arguments(self, data: &Path, input: Option<&Path>) -> Vec<OsString> {
        match self {
            Role::Preview {
                operation: PreviewOperation::Prepare,
            } => Vec::new(),
            Role::Preview {
                operation: PreviewOperation::Fetch,
            } => vec!["--preview-fetch".into()],
            Role::Content { arguments, .. } => arguments.to_vec(),
            Role::MusicbotCodec { operation } => vec!["--codec-worker".into(), operation.into()],
            Role::Storage { .. } => vec!["--database".into(), data.join("history.sqlite3").into()],
            Role::Crypto { .. } => vec!["--data".into(), data.into()],
            Role::Media => vec!["--live".into()],
            Role::Transfer { .. } => {
                input.map_or_else(Vec::new, |input| vec!["--file".into(), input.into()])
            }
            Role::Math | Role::Update { .. } => Vec::new(),
        }
    }
}

/// The command that starts `executable` confined as `role`.
pub fn command(executable: &Path, role: Role) -> Result<Command, Error> {
    #[cfg(target_os = "linux")]
    return linux::command(executable, role);
    #[cfg(target_os = "macos")]
    return macos::command(executable, role);
    #[cfg(windows)]
    return windows::command(executable, role);
}

/// The executable `name` in the running executable's directory, where the
/// workers are installed.
pub fn sibling(name: &str) -> io::Result<std::path::PathBuf> {
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| io::Error::other("executable has no directory"))?;
    Ok(parent.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

/// OS containment applied to a worker after it starts, held for its lifetime.
/// On Windows this is a job object that kills the worker's launch stage when
/// Nuschl lets go of it (including when Nuschl itself exits); the launch
/// stage's own job then ends the confined worker. Elsewhere the launch
/// command already carries the confinement.
pub struct Containment {
    #[cfg(windows)]
    _job: win32job::Job,
}

/// Contain a worker started from [`command`] by other means than [`spawn`]
/// (the Windows confinement check, which keeps the worker's standard error).
pub fn contain(child: &Child) -> io::Result<Containment> {
    #[cfg(windows)]
    {
        let handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("the worker has already exited"))?;
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
        job.assign_process(handle as isize)
            .map_err(io::Error::other)?;
        Ok(Containment { _job: job })
    }
    #[cfg(not(windows))]
    {
        let _ = child;
        Ok(Containment {})
    }
}

/// A started, contained worker. Dropping it kills the process; callers that
/// count processes reap `child` first.
pub struct Worker {
    pub child: Child,
    pub input: ChildStdin,
    pub output: ChildStdout,
    pub containment: Containment,
}

/// Start `command` (from [`command`], or a trusted test's own) with piped
/// standard input and output, no standard error, killed when dropped, and
/// contain it. A worker that cannot be contained is reaped before the error
/// returns.
pub async fn spawn(command: Command) -> Result<Worker, Error> {
    spawn_with_stderr(command, Stdio::null()).await
}

/// Start a fixed preview role with its trusted namespace-init receipt piped on
/// standard error. Its owner acquires the receipt before input and retains the
/// process permit through observed namespace retirement.
pub async fn spawn_preview(
    executable: &Path,
    operation: PreviewOperation,
) -> Result<Worker, Error> {
    spawn_with_stderr(
        command(executable, Role::Preview { operation })?,
        Stdio::piped(),
    )
    .await
}

/// Start the media role. On Linux its namespace-init receipt is piped on
/// standard error, as for the preview roles, so its owner can wait for the
/// whole sandbox to exit before starting another.
pub async fn spawn_media(executable: &Path) -> Result<Worker, Error> {
    let stderr = if cfg!(target_os = "linux") {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    spawn_with_stderr(command(executable, Role::Media)?, stderr).await
}

async fn spawn_with_stderr(mut command: Command, stderr: Stdio) -> Result<Worker, Error> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()?;
    let parts =
        contain(&child).and_then(
            |containment| match (child.stdin.take(), child.stdout.take()) {
                (Some(input), Some(output)) => Ok((input, output, containment)),
                _ => Err(io::Error::other("the worker's pipes are missing")),
            },
        );
    match parts {
        Ok((input, output, containment)) => Ok(Worker {
            child,
            input,
            output,
            containment,
        }),
        Err(error) => {
            reap(&mut child).await;
            Err(Error::Uncontained(error))
        }
    }
}

/// Kill `child` if it still runs and return only once its exit is confirmed,
/// retrying for as long as that takes: a kill request is not an exit, and
/// whatever counts the process must not be released before it. `child` is
/// the direct child: bwrap on Linux, the launch stage on Windows.
pub async fn reap(child: &mut Child) {
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Unlike start_kill, kill().await waits for the actual process exit.
        if child.kill().await.is_ok() || child.wait().await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
