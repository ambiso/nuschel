// SPDX-License-Identifier: EUPL-1.2
//! Native supervisor adapter. The portable client model does not depend on this module.

use std::{ffi::OsString, io, path::Path, process::Stdio, time::Duration};

use nuschel_core::MessageDocument;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use crate::{
    ContentRequest, ContentResponse, Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION, PreparedContent,
    write_frame,
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

/// Build a Linux content sandbox. Only trusted supervisors choose `executable` and `arguments`.
/// This is public so the VM confinement test exercises the same command as the application.
#[cfg(target_os = "linux")]
pub fn content_sandbox_command(
    executable: &Path,
    arguments: &[OsString],
) -> Result<Command, Error> {
    fn helper(name: &'static str) -> Result<std::path::PathBuf, Error> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
            .ok_or(Error::MissingSandboxHelper(name))
    }

    let mut command = Command::new(helper("prlimit")?);
    command.args(["--as=536870912", "--cpu=5", "--"]);
    command.arg(helper("bwrap")?);
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
    command.args(["--chdir", "/tmp", "--clearenv", "--", "/worker"]);
    command.args(arguments);
    command.env_clear();
    Ok(command)
}

/// A one-request harness with bounded output, a deadline, and kill/reap on failure.
/// The live client should retain a bounded worker pool rather than spawn once per message.
pub async fn render_content(executable: &Path, text: &str) -> Result<MessageDocument, Error> {
    Ok(prepare_content(executable, text, false).await?.document)
}

pub async fn prepare_content(
    executable: &Path,
    text: &str,
    legacy_html: bool,
) -> Result<PreparedContent, Error> {
    #[cfg(target_os = "linux")]
    let mut command = content_sandbox_command(executable, &[])?;
    #[cfg(not(target_os = "linux"))]
    let mut command = {
        // Safe Rust content processing remains process-separated on these platforms.
        // This is NOT an OS sandbox; native codec/storage workers need their own profiles.
        let mut command = Command::new(executable);
        command.env_clear();
        command
    };

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let request_id = 1;
    let mut request = Vec::new();
    write_frame(
        &mut request,
        &Envelope::new(
            request_id,
            ContentRequest {
                text: text.to_owned(),
                legacy_html,
            },
        ),
    )?;

    let exchange = async {
        let mut input = child.stdin.take().ok_or(Error::InvalidResponse)?;
        let mut output = child.stdout.take().ok_or(Error::InvalidResponse)?;
        input.write_all(&request).await?;
        input.shutdown().await?;
        drop(input);
        let size = output.read_u32().await? as usize;
        if size > MAX_FRAME_BYTES {
            return Err(Error::Frame(crate::Error::TooLarge));
        }
        let mut bytes = vec![0; size];
        output.read_exact(&mut bytes).await?;
        let response: Envelope<ContentResponse> = serde_json::from_slice(&bytes)?;
        if response.version != PROTOCOL_VERSION || response.request_id != request_id {
            return Err(Error::InvalidResponse);
        }
        if !child.wait().await?.success() {
            return Err(Error::InvalidResponse);
        }
        match response.payload {
            ContentResponse::Rendered(document) => Ok(PreparedContent {
                source: text.to_owned(),
                document,
            }),
            ContentResponse::Prepared(prepared) => Ok(prepared),
            ContentResponse::Rejected(reason) => Err(Error::Rejected(reason)),
        }
    };
    match tokio::time::timeout(Duration::from_secs(5), exchange).await {
        Ok(Ok(document)) => Ok(document),
        result => {
            // kill_on_drop is a fallback; explicitly reap while the runtime still exists.
            child.kill().await.ok();
            match result {
                Ok(Err(error)) => Err(error),
                _ => Err(Error::Timeout),
            }
        }
    }
}

/// Long-lived SQLite helper: its database directory is writable; network and the
/// rest of the home directory are absent. The trusted supervisor chooses the path.
#[cfg(target_os = "linux")]
pub fn storage_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let mut command = persistent_sandbox(executable)?;
    command
        .arg("--bind")
        .arg(directory.canonicalize()?)
        .arg("/data");
    command.args(["--", "/worker", "--database", "/data/history.sqlite3"]);
    Ok(command)
}

/// Audio service sockets and ALSA devices only. No session credentials, home,
/// D-Bus socket, or network namespace are granted to native audio/codec processing.
#[cfg(target_os = "linux")]
pub fn media_sandbox_command(executable: &Path) -> Result<Command, Error> {
    let mut command = persistent_sandbox(executable)?;
    command.args(["--dir", "/run/audio", "--dir", "/run/audio/pulse"]);
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = std::path::PathBuf::from(runtime);
        for (source, target) in [
            (runtime.join("pipewire-0"), "/run/audio/pipewire-0"),
            (runtime.join("pulse/native"), "/run/audio/pulse/native"),
        ] {
            if source.exists() {
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
    command.args(["--", "/worker", "--live"]);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn persistent_sandbox(executable: &Path) -> Result<Command, Error> {
    // Reuse the exact helper discovery/runtime mounts of the content sandbox,
    // but give long-lived workers no five-second lifetime CPU limit.
    let template = content_sandbox_command(executable, &[])?;
    let arguments = template
        .as_std()
        .get_args()
        .map(OsString::from)
        .collect::<Vec<_>>();
    // content_sandbox_command: prlimit --as=... --cpu=5 -- bwrap ... -- /worker.
    let bwrap = arguments.get(3).ok_or(Error::InvalidResponse)?;
    let mut command = Command::new(template.as_std().get_program());
    command.arg("--as=1073741824").arg("--").arg(bwrap);
    let tail = &arguments[4..];
    command.args(&tail[..tail.len().saturating_sub(2)]);
    command.env_clear();
    Ok(command)
}
