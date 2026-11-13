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
//! supervisors choose the executable, the role and the extra arguments.

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
    /// The worker started but could not be contained. It has been told to
    /// stop; reap `child` before releasing anything that counts the process.
    #[error("cannot contain the worker: {error}")]
    Uncontained { error: io::Error, child: Box<Child> },
}

/// A kind of worker and what it is given beyond its standard input and output.
#[derive(Clone, Copy, Debug)]
pub enum Role<'a> {
    /// Untrusted message content, one-shot; CPU seconds over its whole life.
    Content { cpu_seconds: u64 },
    /// LaTeX formulas; long-lived, so its CPU limit covers its whole life.
    Math,
    /// The update verifier, one-shot. `post_quantum` selects a distinct
    /// Windows profile, not an algorithm inside the executable.
    Update { post_quantum: bool },
    /// The music bot's codec children: Linux only.
    MusicbotCodec,
    /// History storage: only `directory`, writable.
    Storage { directory: &'a Path },
    /// The crypto worker: only `directory`, writable.
    Crypto { directory: &'a Path },
    /// Audio devices and the sound server.
    Media,
    /// magic-wormhole: the network and, when offering, `file` read-only.
    Transfer { file: Option<&'a Path> },
}

const MIB: u64 = 1 << 20;

impl<'a> Role<'a> {
    /// Address space (Linux) or committed memory (Windows) in bytes.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn memory(self) -> u64 {
        match self {
            Role::Content { .. } | Role::Math => 512 * MIB,
            Role::Update { .. } => 256 * MIB,
            _ => 1024 * MIB,
        }
    }

    /// CPU seconds over the worker's life; macOS has no such limit.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn cpu_seconds(self) -> Option<u64> {
        match self {
            Role::Content { cpu_seconds } => Some(cpu_seconds),
            Role::Math => Some(120),
            Role::Update { .. } => Some(5),
            _ => None,
        }
    }

    fn directory(self) -> Option<&'a Path> {
        match self {
            Role::Storage { directory } | Role::Crypto { directory } => Some(directory),
            _ => None,
        }
    }

    /// The worker's own arguments, given where it sees its directory and its
    /// offered file.
    fn arguments(self, data: &Path, input: Option<&Path>) -> Vec<OsString> {
        match self {
            Role::Storage { .. } => vec!["--database".into(), data.join("history.sqlite3").into()],
            Role::Crypto { .. } => vec!["--data".into(), data.into()],
            Role::Media => vec!["--live".into()],
            Role::Transfer { .. } => {
                input.map_or_else(Vec::new, |input| vec!["--file".into(), input.into()])
            }
            _ => Vec::new(),
        }
    }
}

/// The command that starts `executable` confined as `role`, with `arguments`
/// after the role's own.
pub fn command(executable: &Path, role: Role, arguments: &[OsString]) -> Result<Command, Error> {
    #[cfg(target_os = "linux")]
    return linux::command(executable, role, arguments);
    #[cfg(target_os = "macos")]
    return macos::command(executable, role, arguments);
    #[cfg(windows)]
    return windows::command(executable, role, arguments);
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
/// contain it.
pub fn spawn(mut command: Command) -> Result<Worker, Error> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
            let _ = child.start_kill();
            let child = Box::new(child);
            Err(Error::Uncontained { error, child })
        }
    }
}

/// Kill `child` if it still runs and return only once its exit is confirmed,
/// retrying for as long as that takes: a kill request is not an exit, and
/// whatever counts the process must not be released before it.
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
