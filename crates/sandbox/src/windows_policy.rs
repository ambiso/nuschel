// SPDX-License-Identifier: EUPL-1.2
//! The confinement a Windows worker is started under, as data.
//!
//! Windows has no `exec`: a sandbox cannot be put around a program by a helper
//! that then becomes it, as bwrap and `sandbox-exec` do. Instead the supervisor
//! starts the worker executable with [`LAUNCH_FLAG`], this policy and `--`
//! before the worker's own arguments. That first process (the launch stage,
//! `nuschl-sandbox-windows`) does not touch untrusted input: it creates the
//! AppContainer, grants it its directories, and starts the same executable
//! again inside it, in a job object, with the worker arguments only. It then
//! waits and exits with the worker's exit code, so the supervisor's process
//! handle, pipes and kill-on-close job keep working as before.
//!
//! Everything here is platform-neutral (the policy's encoding and Windows
//! command-line quoting) so that it is tested on every platform.

use std::{ffi::OsString, path::PathBuf};

/// First argument of a worker started through its launch stage.
pub const LAUNCH_FLAG: &str = "--nuschl-sandbox";

/// Appended to the worker's arguments when [`Policy::file`] is set: the
/// inherited handle of that file, opened read-only by the launch stage.
pub const FILE_HANDLE_FLAG: &str = "--file-handle";

/// A worker's AppContainer and job limits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// AppContainer profile name (letters, digits, `.`, `-`, `_`; at most 64).
    pub profile: String,
    /// Capability names, e.g. `internetClient`. None means no network at all.
    pub capabilities: Vec<String>,
    /// Directories the container may read and write, with everything in them.
    pub writable: Vec<PathBuf>,
    /// One file the worker reads, passed as an inherited read-only handle
    /// (`--file-handle N` after the worker's arguments), never by path.
    pub file: Option<PathBuf>,
    /// Committed memory per process in bytes; 0 means no limit.
    pub memory: u64,
    /// User-mode CPU seconds before the job ends the worker.
    pub cpu_seconds: Option<u64>,
    /// A less privileged AppContainer (LPAC): not in "ALL APPLICATION
    /// PACKAGES", so nothing granted to every AppContainer (such as the
    /// user's environment variables in the registry) is granted to it.
    pub less_privileged: bool,
}

impl Policy {
    /// The launch-stage arguments: [`LAUNCH_FLAG`], the policy, then `--`.
    pub fn arguments(&self) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = vec![LAUNCH_FLAG.into(), "--profile".into()];
        arguments.push(self.profile.clone().into());
        for capability in &self.capabilities {
            arguments.push("--capability".into());
            arguments.push(capability.into());
        }
        for directory in &self.writable {
            arguments.push("--writable".into());
            arguments.push(directory.into());
        }
        if let Some(file) = &self.file {
            arguments.push("--read-file".into());
            arguments.push(file.into());
        }
        if self.memory > 0 {
            arguments.push("--memory".into());
            arguments.push(self.memory.to_string().into());
        }
        if let Some(seconds) = self.cpu_seconds {
            arguments.push("--cpu".into());
            arguments.push(seconds.to_string().into());
        }
        if self.less_privileged {
            arguments.push("--less-privileged".into());
        }
        arguments.push("--".into());
        arguments
    }

    /// The policy and the worker's own arguments, if `arguments` (without the
    /// program name) start with [`LAUNCH_FLAG`]; `Ok(None)` otherwise.
    pub fn parse(
        arguments: impl IntoIterator<Item = OsString>,
    ) -> Result<Option<(Self, Vec<OsString>)>, String> {
        let mut arguments = arguments.into_iter();
        if arguments.next().is_none_or(|first| first != LAUNCH_FLAG) {
            return Ok(None);
        }
        let mut policy = Self::default();
        let mut profile = None;
        let mut memory = false;
        loop {
            let Some(option) = arguments.next() else {
                return Err("the sandbox policy does not end with --".into());
            };
            if option == "--" {
                break;
            }
            let option = option.to_string_lossy().into_owned();
            if option == "--less-privileged" && !policy.less_privileged {
                policy.less_privileged = true;
                continue;
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("{option} needs a value"))?;
            let number = |value: OsString| {
                value
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| format!("{option} needs a number"))
            };
            match option.as_str() {
                "--profile" if profile.is_none() => profile = Some(text(value)?),
                "--capability" => policy.capabilities.push(text(value)?),
                "--writable" => policy.writable.push(value.into()),
                "--read-file" if policy.file.is_none() => policy.file = Some(value.into()),
                "--memory" if !memory => {
                    memory = true;
                    policy.memory = number(value)?;
                }
                "--cpu" if policy.cpu_seconds.is_none() => {
                    policy.cpu_seconds = Some(number(value)?)
                }
                _ => return Err(format!("unknown or repeated sandbox option {option}")),
            }
        }
        policy.profile = profile.ok_or("the sandbox policy names no profile")?;
        if !valid_profile(&policy.profile) {
            return Err(format!("invalid AppContainer name {:?}", policy.profile));
        }
        Ok(Some((policy, arguments.collect())))
    }
}

fn text(value: OsString) -> Result<String, String> {
    value
        .into_string()
        .map_err(|value| format!("not Unicode: {value:?}"))
}

/// What `CreateAppContainerProfile` accepts.
fn valid_profile(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// A stable profile name for `kind` confined to `directory`, so that two
/// Nuschl profiles' storage or crypto workers never share a container (each
/// container may open only the directories granted to it). FNV-1a over the
/// path's UTF-8 form; collisions only merge two containers' grants.
pub fn scoped_profile(kind: &str, directory: &std::path::Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in directory.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{kind}.{hash:016x}")
}

/// Append `argument` to a Windows command line in UTF-16, quoted so that
/// `CommandLineToArgvW` and the Rust and C runtimes read it back unchanged.
pub fn append_argument(command_line: &mut Vec<u16>, argument: &[u16]) {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    if !command_line.is_empty() {
        command_line.push(u16::from(b' '));
    }
    let plain = !argument.is_empty()
        && !argument
            .iter()
            .any(|&unit| matches!(unit, 0x20 | 0x09 | 0x0a | 0x0b | 0x22));
    if plain {
        command_line.extend_from_slice(argument);
        return;
    }
    command_line.push(QUOTE);
    let mut backslashes = 0;
    for &unit in argument {
        if unit == BACKSLASH {
            backslashes += 1;
            continue;
        }
        // Backslashes are literal unless a quote follows them.
        let doubled = if unit == QUOTE {
            2 * backslashes + 1
        } else {
            backslashes
        };
        command_line.extend(std::iter::repeat_n(BACKSLASH, doubled));
        command_line.push(unit);
        backslashes = 0;
    }
    // Before the closing quote, every backslash is doubled.
    command_line.extend(std::iter::repeat_n(BACKSLASH, 2 * backslashes));
    command_line.push(QUOTE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_round_trips_through_arguments() {
        let policy = Policy {
            profile: "nuschl.transfer".into(),
            capabilities: vec!["internetClient".into(), "privateNetworkClientServer".into()],
            writable: vec![PathBuf::from(r"C:\Users\a b\history"), "--".into()],
            file: Some(r"C:\Users\a\Documents\offer.zip".into()),
            memory: 1 << 30,
            cpu_seconds: Some(7),
            less_privileged: true,
        };
        let mut arguments = policy.arguments();
        assert_eq!(arguments[0], LAUNCH_FLAG);
        assert_eq!(arguments.last().unwrap(), "--");
        arguments.extend(["--file".into(), "x".into(), "--".into()]);
        let (parsed, worker) = Policy::parse(arguments).unwrap().unwrap();
        assert_eq!(parsed, policy);
        // The worker's own arguments, even a second `--`, are passed on as they are.
        assert_eq!(worker, ["--file", "x", "--"]);
    }

    #[test]
    fn plain_worker_arguments_are_not_a_policy() {
        assert_eq!(Policy::parse(["--live".into()]).unwrap(), None);
        assert_eq!(Policy::parse([]).unwrap(), None);
    }

    #[test]
    fn malformed_policies_are_refused() {
        let parse = |arguments: &[&str]| Policy::parse(arguments.iter().map(OsString::from));
        assert!(parse(&[LAUNCH_FLAG, "--profile", "nuschl.content"]).is_err());
        assert!(parse(&[LAUNCH_FLAG, "--"]).is_err());
        assert!(parse(&[LAUNCH_FLAG, "--profile", "a\\b", "--"]).is_err());
        assert!(parse(&[LAUNCH_FLAG, "--profile", "a", "--memory", "lots", "--"]).is_err());
        assert!(parse(&[LAUNCH_FLAG, "--profile", "a", "--unknown", "1", "--"]).is_err());
        let two_files = [LAUNCH_FLAG, "--profile", "a", "--read-file", "x"];
        assert!(parse(&[&two_files[..], &["--read-file", "y", "--"]].concat()).is_err());
        // Every single-valued option at most once.
        for repeated in [
            &["--profile", "b"][..],
            &["--memory", "1", "--memory", "2"],
            &["--cpu", "1", "--cpu", "2"],
            &["--less-privileged", "--less-privileged"],
        ] {
            let arguments = [&[LAUNCH_FLAG, "--profile", "a"][..], repeated, &["--"]].concat();
            assert!(parse(&arguments).is_err(), "{repeated:?}");
        }
        assert!(parse(&[LAUNCH_FLAG, "--profile", &"a".repeat(65), "--"]).is_err());
        assert!(parse(&[LAUNCH_FLAG, "--profile", "nuschl.content", "--"]).is_ok());
    }

    #[test]
    fn scoped_profiles_are_stable_and_distinct() {
        let a = scoped_profile("nuschl.crypto", std::path::Path::new(r"C:\Users\a\crypto"));
        let b = scoped_profile("nuschl.crypto", std::path::Path::new(r"C:\Users\b\crypto"));
        let suffix = a.strip_prefix("nuschl.crypto.").unwrap();
        assert!(suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(
            a,
            scoped_profile("nuschl.crypto", std::path::Path::new(r"C:\USERS\A\Crypto"))
        );
        assert!(valid_profile(&a), "{a}");
    }

    #[test]
    fn command_lines_quote_like_the_c_runtime() {
        let line = |arguments: &[&str]| {
            let mut line = Vec::new();
            for argument in arguments {
                append_argument(&mut line, &argument.encode_utf16().collect::<Vec<_>>());
            }
            String::from_utf16(&line).unwrap()
        };
        assert_eq!(line(&["a.exe", "--live"]), "a.exe --live");
        assert_eq!(line(&[r"C:\a b\w.exe", ""]), r#""C:\a b\w.exe" """#);
        assert_eq!(line(&[r"C:\dir\", r"C:\a b\"]), r#"C:\dir\ "C:\a b\\""#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(line(&[r#"a\"b"#]), r#""a\\\"b""#);
        assert_eq!(line(&["tab\there"]), "\"tab\there\"");
    }
}
