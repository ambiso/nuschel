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

/// Decode an avatar in the sandboxed content worker; returns an Image document.
pub async fn decode_avatar(executable: &Path, encoded: &[u8]) -> Result<MessageDocument, Error> {
    use base64::Engine as _;
    let text = base64::engine::general_purpose::STANDARD.encode(encoded);
    Ok(exchange(
        executable,
        ContentRequest {
            text,
            legacy_html: false,
            avatar: true,
        },
    )
    .await?
    .document)
}

pub async fn prepare_content(
    executable: &Path,
    text: &str,
    legacy_html: bool,
) -> Result<PreparedContent, Error> {
    exchange(
        executable,
        ContentRequest {
            text: text.to_owned(),
            legacy_html,
            avatar: false,
        },
    )
    .await
}

async fn exchange(executable: &Path, content: ContentRequest) -> Result<PreparedContent, Error> {
    let text = content.text.clone();
    let mut command = content_sandbox_command(executable, &[])?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let _containment = contain(&child)?;
    let request_id = 1;
    let mut request = Vec::new();
    write_frame(&mut request, &Envelope::new(request_id, content))?;

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
                source: text.clone(),
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

/// OS containment applied to a worker after it starts, held for its lifetime.
/// On Windows this is a job object that kills the worker when Nuschel lets go
/// of it (including when Nuschel itself exits); elsewhere the launch command
/// already carries the confinement.
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

#[cfg(target_os = "macos")]
pub fn content_sandbox_command(
    executable: &Path,
    arguments: &[OsString],
) -> Result<Command, Error> {
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

/// Windows: no OS sandbox yet. Workers run without the user's environment and
/// without a console window, inside a kill-on-close job object (see
/// [`contain`]). AppContainer or restricted-token confinement needs unsafe
/// Win32 process creation and must be validated on Windows first.
#[cfg(windows)]
fn windows_worker(executable: &Path) -> Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(executable);
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
pub fn content_sandbox_command(
    executable: &Path,
    arguments: &[OsString],
) -> Result<Command, Error> {
    let mut command = windows_worker(executable);
    command.args(arguments);
    Ok(command)
}

#[cfg(windows)]
pub fn storage_sandbox_command(executable: &Path, directory: &Path) -> Result<Command, Error> {
    let mut command = windows_worker(executable);
    command
        .arg("--database")
        .arg(directory.join("history.sqlite3"));
    Ok(command)
}

#[cfg(windows)]
pub fn media_sandbox_command(executable: &Path) -> Result<Command, Error> {
    let mut command = windows_worker(executable);
    command.arg("--live");
    Ok(command)
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
        for profile in [seatbelt::CONTENT, seatbelt::STORAGE, seatbelt::MEDIA] {
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
