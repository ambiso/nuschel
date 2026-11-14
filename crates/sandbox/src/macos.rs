// SPDX-License-Identifier: EUPL-1.2
//! macOS: Seatbelt profiles applied with `sandbox-exec`, without memory or CPU
//! limits (the supervisors' deadlines bound the workers). Content and storage
//! workers are pure Rust and start from deny-by-default on top of Apple's base
//! `system.sb`. The media worker needs CoreAudio's undocumented set of Mach
//! services, so it is confined by a deny list instead (no network, no writes
//! outside temporary directories, no reads of the home directory beyond audio
//! preferences) until it can be narrowed on real hardware. No confinement for
//! the music bot's persistent codec children has been designed and proven, so
//! they are refused.
//!
//! A role's capabilities select one profile: audio MEDIA, network TRANSFER
//! (with its readable file), a writable directory STORAGE, otherwise CONTENT.
//! A capability its profile has no rule for is not granted.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::{Error, Role};

const CONTENT: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-exec (literal (param "WORKER")))
(allow file-read* (literal (param "WORKER")))
"#;

const STORAGE: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-exec (literal (param "WORKER")))
(allow file-read* (literal (param "WORKER")))
(allow file-read* file-write* (subpath (param "DATA")))
"#;

const MEDIA: &str = r#"(version 1)
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
const TRANSFER: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-exec (literal (param "WORKER")))
(allow file-read* (literal (param "WORKER")) (literal (param "FILE")))
(allow system-socket)
(allow network-bind network-inbound network-outbound)
(allow mach-lookup (global-name "com.apple.dnssd.service"))
"#;

/// Compiled on every platform for its tests; only macOS runs it.
pub(crate) fn command(executable: &Path, role: Role) -> Result<Command, Error> {
    let can = role.capabilities();
    if can.linux_only {
        return Err(Error::MissingSandboxHelper(
            "containment for this role on this platform",
        ));
    }
    let executable = executable.canonicalize()?;
    let data = can.writable.map(Path::canonicalize).transpose()?;
    let input = can.readable.map(Path::canonicalize).transpose()?;
    let mut parameters = vec![("WORKER", executable.clone())];
    let profile = if can.audio {
        let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("no HOME"))?;
        let home = PathBuf::from(home);
        parameters.push(("PREFERENCES", home.join("Library/Preferences")));
        parameters.push(("AUDIO_PREFERENCES", home.join("Library/Audio")));
        parameters.push(("HOME", home));
        MEDIA
    } else if can.network {
        let file = input.clone().unwrap_or_else(|| "/dev/null".into());
        parameters.push(("FILE", file));
        TRANSFER
    } else if let Some(data) = &data {
        parameters.push(("DATA", data.clone()));
        STORAGE
    } else {
        CONTENT
    };
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.arg("-p").arg(profile);
    for (name, value) in parameters {
        command.arg("-D").arg(format!("{name}={}", value.display()));
    }
    command.arg(&executable);
    command.args(role.arguments(data.as_deref().unwrap_or(Path::new("")), input.as_deref()));
    command.env_clear();
    if can.audio
        && let Some(value) = std::env::var_os("TMPDIR")
    {
        command.env("TMPDIR", value);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::{CONTENT, MEDIA, STORAGE, TRANSFER};

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
        for profile in [CONTENT, STORAGE, MEDIA, TRANSFER] {
            assert!(profile.starts_with("(version 1)"));
            assert!(balanced(profile), "{profile}");
            assert!(profile.contains(r#"(allow process-exec (literal (param "WORKER")))"#));
        }
        for profile in [CONTENT, STORAGE] {
            assert!(profile.contains("(deny default)"));
            assert!(
                !profile.contains("network"),
                "pure workers get no network rule at all"
            );
        }
        assert!(MEDIA.contains("(deny network*)"));
        assert!(MEDIA.contains("(deny file-write*)"));
        assert!(TRANSFER.contains("(deny default)"));
        assert!(!TRANSFER.contains("file-write"));
    }

    /// The launch command each role gets; built on every platform.
    #[test]
    fn roles_map_to_their_profile_and_arguments() {
        use crate::Role;
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        let directory = std::env::temp_dir().canonicalize().unwrap();
        let launch = |role: Role| {
            let command = super::command(&me, role).unwrap();
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .all(|(name, _)| name == "TMPDIR")
            );
            let arguments = command.as_std().get_args();
            arguments
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let worker = format!("WORKER={}", me.display());
        let data = format!("DATA={}", directory.display());
        let offer = format!("FILE={}", me.display());
        let database = directory.join("history.sqlite3").display().to_string();
        let (me_text, directory_text) = (me.display().to_string(), directory.display().to_string());
        let (me, directory) = (me.as_path(), directory.as_path());
        let pure = ["-p", CONTENT, "-D", &worker, &me_text];
        let stored = ["-p", STORAGE, "-D", &worker, "-D", &data, &me_text];
        let sending = ["-p", TRANSFER, "-D", &worker, "-D", &offer, &me_text];
        let receiving = [
            "-p",
            TRANSFER,
            "-D",
            &worker,
            "-D",
            "FILE=/dev/null",
            &me_text,
        ];
        let arguments = ["x".into()];
        for (role, expected) in [
            (
                Role::Content {
                    cpu_seconds: 7,
                    arguments: &arguments,
                },
                [&pure[..], &["x"]].concat(),
            ),
            (Role::Math, pure.to_vec()),
            (Role::Update { post_quantum: true }, pure.to_vec()),
            (
                Role::Storage { directory },
                [&stored[..], &["--database", &database]].concat(),
            ),
            (
                Role::Crypto { directory },
                [&stored[..], &["--data", &directory_text]].concat(),
            ),
            (
                Role::Transfer { file: Some(me) },
                [&sending[..], &["--file", &me_text]].concat(),
            ),
            (Role::Transfer { file: None }, receiving.to_vec()),
        ] {
            assert_eq!(launch(role), expected, "{role:?}");
        }
        for operation in [
            crate::PreviewOperation::Prepare,
            crate::PreviewOperation::Fetch,
        ] {
            assert!(super::command(me, Role::Preview { operation }).is_err());
        }
        let media = launch(Role::Media);
        assert_eq!(media[..2], ["-p", MEDIA]);
        assert_eq!(media[media.len() - 2..], [&me_text, "--live"]);
        assert!(
            super::command(
                me,
                Role::MusicbotCodec {
                    operation: "decode"
                }
            )
            .is_err()
        );
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
        assert!(run(CONTENT, "/usr/bin/true", &[]));
        let home = std::env::var("HOME").unwrap();
        assert!(!run(
            CONTENT,
            "/usr/bin/touch",
            &[&format!("{home}/nuschl-sandbox-escape")]
        ));
    }
}
