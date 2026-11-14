// SPDX-License-Identifier: EUPL-1.2
//! Linux: bubblewrap under prlimit. Every worker gets all namespaces
//! unshared (no network), read-only system libraries, private /proc, /dev
//! and /tmp, itself at `/worker` and an empty environment; its role adds to that.

use std::{os::unix::fs::MetadataExt as _, path::Path};

use tokio::process::Command;

use crate::{Error, Role};

fn helper(name: &'static str) -> Result<std::path::PathBuf, Error> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .ok_or(Error::MissingSandboxHelper(name))
}

/// `--ro-bind` each of `paths` to itself where it exists.
fn bind_existing(command: &mut Command, paths: &[&str]) {
    for path in paths {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
}

pub(crate) fn command(executable: &Path, role: Role) -> Result<Command, Error> {
    let can = role.capabilities();
    let mut command = Command::new(helper("prlimit")?);
    command.arg(format!("--as={}", can.memory));
    if let Some(seconds) = can.cpu_seconds {
        command.arg(format!("--cpu={seconds}"));
    }
    command.arg("--").arg(helper("bwrap")?);
    command.args(["--die-with-parent", "--new-session", "--unshare-all"]);
    if matches!(role, Role::Preview { .. }) {
        command.args(["--json-status-fd", "2"]);
    }
    bind_existing(
        &mut command,
        &["/nix/store", "/usr/lib", "/usr/lib64", "/lib", "/lib64"],
    );
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    command
        .arg("--ro-bind")
        .arg(executable.canonicalize()?)
        .arg("/worker");
    command.args(["--chdir", "/tmp", "--clearenv"]);
    command.env_clear();
    if let Some(directory) = can.writable {
        command
            .arg("--bind")
            .arg(directory.canonicalize()?)
            .arg("/data");
    }
    if can.audio {
        media(&mut command);
    }
    if can.network {
        command.arg("--share-net");
        let resolver = [
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/gai.conf",
        ];
        bind_existing(&mut command, &resolver);
    }
    if let Some(file) = can.readable {
        command
            .arg("--ro-bind")
            .arg(file.canonicalize()?)
            .arg("/input");
    }
    command.args(["--", "/worker"]);
    let input = can.readable.map(|_| Path::new("/input"));
    command.args(role.arguments(Path::new("/data"), input));
    Ok(command)
}

/// Audio service sockets, ALSA devices and public audio configuration. No
/// session credentials, home, D-Bus socket or network namespace.
fn media(command: &mut Command) {
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
            if std::fs::metadata(&source).is_ok_and(|metadata| metadata.nlink() > 0) {
                command.arg("--ro-bind").arg(source).arg(target);
            }
        }
    }
    if Path::new("/dev/snd").exists() {
        command.args(["--dev-bind", "/dev/snd", "/dev/snd"]);
    }
    // Distribution libraries need their public defaults as well as shared
    // objects. User and /etc overrides stay outside this worker's namespace.
    bind_existing(
        command,
        &[
            "/sys/devices",
            "/sys/class/sound",
            "/proc/asound",
            "/usr/share/pipewire",
            "/usr/share/alsa",
        ],
    );
    command.args(["--setenv", "XDG_RUNTIME_DIR", "/run/audio"]);
    command.args(["--setenv", "PULSE_SERVER", "unix:/run/audio/pulse/native"]);
    for name in ["PULSE_SOURCE", "PULSE_SINK", "NUSCHL_AUDIO_BACKEND"] {
        if let Some(value) = std::env::var_os(name) {
            command.arg("--setenv").arg(name).arg(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Role;

    fn arguments(role: Role) -> Vec<String> {
        let executable = std::env::current_exe().unwrap();
        let command = super::command(&executable, role).unwrap();
        let arguments = command.as_std().get_args();
        arguments.map(|a| a.to_string_lossy().into()).collect()
    }

    #[test]
    fn preview_roles_have_fixed_arguments_and_namespace_receipt() {
        for (operation, suffix) in [
            (crate::PreviewOperation::Prepare, vec!["/worker"]),
            (
                crate::PreviewOperation::Fetch,
                vec!["/worker", "--preview-fetch"],
            ),
        ] {
            let args = arguments(Role::Preview { operation });
            assert_eq!(args[..3], ["--as=536870912", "--cpu=5", "--"]);
            let end = args.iter().rposition(|arg| arg == "--").unwrap();
            assert_eq!(args[end + 1..], suffix);
            let status: Vec<_> = args
                .windows(2)
                .filter(|pair| pair[0] == "--json-status-fd")
                .collect();
            assert_eq!(status.len(), 1);
            assert_eq!(status[0][1], "2");
            for grant in [
                "--share-net",
                "--bind",
                "--dev-bind",
                "--setenv",
                "--as-pid-1",
            ] {
                assert!(!args.iter().any(|arg| arg == grant), "{grant}: {args:?}");
            }
        }
        let ordinary = arguments(Role::Content {
            cpu_seconds: 5,
            arguments: &[],
        });
        assert!(!ordinary.iter().any(|arg| arg == "--json-status-fd"));
    }

    #[test]
    fn workers_share_one_bwrap_setup() {
        let directory = std::env::temp_dir();
        let executable = std::env::current_exe().unwrap();
        let content = arguments(Role::Content {
            cpu_seconds: 7,
            arguments: &["x".into()],
        });
        let math = arguments(Role::Math);
        let update = arguments(Role::Update { post_quantum: true });
        let codec = arguments(Role::MusicbotCodec {
            operation: "decode",
        });
        let storage = arguments(Role::Storage {
            directory: &directory,
        });
        let crypto = arguments(Role::Crypto {
            directory: &directory,
        });
        let media = arguments(Role::Media);
        let offer = arguments(Role::Transfer {
            file: Some(&executable),
        });
        let receive = arguments(Role::Transfer { file: None });
        assert_eq!(content[..3], ["--as=536870912", "--cpu=7", "--"]);
        assert_eq!(math[..3], ["--as=536870912", "--cpu=120", "--"]);
        assert_eq!(update[..3], ["--as=268435456", "--cpu=5", "--"]);
        for persistent in [&codec, &storage, &media, &crypto, &offer, &receive] {
            assert_eq!(persistent[..2], ["--as=1073741824", "--"]);
        }
        let all = [
            (&content, &["/worker", "x"][..]),
            (&math, &["/worker"]),
            (&update, &["/worker"]),
            (&codec, &["/worker", "--codec-worker", "decode"]),
            (
                &storage,
                &["/worker", "--database", "/data/history.sqlite3"],
            ),
            (&media, &["/worker", "--live"]),
            (&crypto, &["/worker", "--data", "/data"]),
            (&offer, &["/worker", "--file", "/input"]),
            (&receive, &["/worker"]),
        ];
        let history = directory.canonicalize().unwrap();
        let history = history.to_string_lossy();
        for (command, worker) in all {
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
            // Storage and crypto each get exactly the folder they are given,
            // writable, and nothing else is writable from outside.
            let binds: Vec<&str> = command
                .windows(3)
                .filter(|option| option[0] == "--bind" || option[0] == "--dev-bind")
                .filter(|option| option[1] != "/dev/snd")
                .map(|option| option[1].as_str())
                .collect();
            let writable = std::ptr::eq(command, &storage) || std::ptr::eq(command, &crypto);
            assert_eq!(
                binds,
                writable
                    .then_some(&*history)
                    .into_iter()
                    .collect::<Vec<_>>()
            );
            // Read-only, only system libraries and the worker itself, apart
            // from audio for media and name resolution and the offer for transfers.
            let system = [
                "/nix/store",
                "/usr/lib",
                "/usr/lib64",
                "/lib",
                "/lib64",
                "/worker",
            ];
            if !network && !std::ptr::eq(command, &media) {
                let read_only = command.windows(3).filter(|option| option[0] == "--ro-bind");
                for option in read_only {
                    assert!(system.contains(&option[2].as_str()), "{option:?}");
                }
            }
        }
        // The offered file is the only path bound from outside the system.
        let input = offer.iter().position(|a| a == "/input").unwrap();
        assert_eq!(offer[input - 2], "--ro-bind");
        assert!(!receive.iter().any(|a| a == "/input"));
    }
}
