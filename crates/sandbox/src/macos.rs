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

#[cfg(target_os = "macos")]
pub(crate) fn command(
    executable: &std::path::Path,
    role: crate::Role,
    arguments: &[std::ffi::OsString],
) -> Result<tokio::process::Command, crate::Error> {
    use crate::{Error, Role};
    use std::path::{Path, PathBuf};

    let executable = executable.canonicalize()?;
    let data = role.directory().map(Path::canonicalize).transpose()?;
    let mut input = None;
    let mut parameters: Vec<(&str, PathBuf)> = Vec::new();
    let profile = match role {
        Role::Content { .. } | Role::Math | Role::Update { .. } => CONTENT,
        Role::Storage { .. } | Role::Crypto { .. } => {
            parameters.extend(data.clone().map(|data| ("DATA", data)));
            STORAGE
        }
        Role::Media => {
            let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("no HOME"))?;
            let home = PathBuf::from(home);
            parameters.push(("PREFERENCES", home.join("Library/Preferences")));
            parameters.push(("AUDIO_PREFERENCES", home.join("Library/Audio")));
            parameters.push(("HOME", home));
            MEDIA
        }
        Role::Transfer { file } => {
            input = file.map(Path::canonicalize).transpose()?;
            let file = input.clone().unwrap_or_else(|| "/dev/null".into());
            parameters.push(("FILE", file));
            TRANSFER
        }
        Role::MusicbotCodec => {
            return Err(Error::MissingSandboxHelper(
                "musicbot codec containment on this platform",
            ));
        }
    };
    let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
    command.arg("-p").arg(profile);
    command
        .arg("-D")
        .arg(format!("WORKER={}", executable.display()));
    for (name, value) in parameters {
        command.arg("-D").arg(format!("{name}={}", value.display()));
    }
    command.arg(&executable);
    command.args(role.arguments(data.as_deref().unwrap_or(Path::new("")), input.as_deref()));
    command.args(arguments);
    command.env_clear();
    if let (Role::Media, Some(value)) = (role, std::env::var_os("TMPDIR")) {
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
