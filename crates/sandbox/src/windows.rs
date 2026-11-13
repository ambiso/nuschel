// SPDX-License-Identifier: EUPL-1.2
//! Windows: each worker runs in its own AppContainer (no capabilities unless
//! listed: no network, no access to the user's files) inside a job object,
//! started by its own executable's launch stage ([`crate::windows_policy`],
//! `nuschl-sandbox-windows`). The launch stage itself runs with an empty
//! environment apart from Windows' own directories, without a console
//! window, inside the kill-on-close job of [`crate::spawn`]; it refuses to
//! start a worker it cannot confine. The policies are kept apart from
//! process creation so that they are checked on every platform.

use crate::{
    Role,
    windows_policy::{Policy, scoped_profile},
};

#[cfg(windows)]
pub(crate) fn command(
    executable: &std::path::Path,
    role: Role,
) -> Result<tokio::process::Command, crate::Error> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let policy = policy(role).ok_or(crate::Error::MissingSandboxHelper(
        "containment for this role on this platform",
    ))?;
    let mut command = tokio::process::Command::new(executable);
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.creation_flags(CREATE_NO_WINDOW);
    command.args(policy.arguments());
    // A readable file reaches the worker as an inherited read-only handle
    // (`--file-handle N`, appended by the launch stage), never as a path.
    let data = role.capabilities().writable;
    command.args(role.arguments(data.unwrap_or(std::path::Path::new("")), None));
    Ok(command)
}

/// The role's AppContainer and job limits. A writable directory scopes the
/// profile name to it, so two Nuschl profiles' workers never share a container.
fn policy(role: Role) -> Option<Policy> {
    let can = role.capabilities();
    if can.linux_only {
        return None;
    }
    let mut capabilities = Vec::new();
    if can.audio {
        capabilities.push("microphone".into());
    }
    if can.network {
        // Outgoing connections to the Internet (rendezvous, relay, peers) and
        // connections to and from the local network (direct transit between
        // peers on one LAN), but no inbound connections from the Internet.
        capabilities.extend(["internetClient".into(), "privateNetworkClientServer".into()]);
    }
    let profile = format!("nuschl.{}", can.name);
    Some(Policy {
        profile: match can.writable {
            Some(directory) => scoped_profile(&profile, directory),
            None => profile,
        },
        capabilities,
        writable: can.writable.map(Into::into).into_iter().collect(),
        file: can.readable.map(Into::into),
        memory: can.memory,
        cpu_seconds: can.cpu_seconds,
        less_privileged: can.lpac,
    })
}

#[cfg(test)]
mod tests {
    use super::policy;
    use crate::{Role, windows_policy::scoped_profile};
    use std::path::Path;

    /// The launch stage's arguments for each role.
    #[test]
    fn roles_map_to_their_launch_policy() {
        let directory = Path::new(r"C:\data");
        let file = Path::new(r"C:\offer.zip");
        let storage = scoped_profile("nuschl.storage", directory);
        let crypto = scoped_profile("nuschl.crypto", directory);
        let pure = "--memory 1073741824 --less-privileged --";
        for (role, expected) in [
            (Role::Content { cpu_seconds: 7, arguments: &[] }, "nuschl.content --memory 536870912 --cpu 7 --less-privileged --".into()),
            (Role::Math, "nuschl.math --memory 536870912 --cpu 120 --less-privileged --".into()),
            (Role::Update { post_quantum: true }, "nuschl.update.mldsa44 --memory 268435456 --cpu 5 --less-privileged --".into()),
            (Role::Update { post_quantum: false }, "nuschl.update.ed25519 --memory 268435456 --cpu 5 --less-privileged --".into()),
            (Role::Storage { directory }, format!(r"{storage} --writable C:\data {pure}")),
            (Role::Crypto { directory }, format!(r"{crypto} --writable C:\data {pure}")),
            (Role::Media, "nuschl.media --capability microphone --memory 1073741824 --".into()),
            (Role::Transfer { file: Some(file) }, r"nuschl.transfer --capability internetClient --capability privateNetworkClientServer --read-file C:\offer.zip --memory 1073741824 --".into()),
            (Role::Transfer { file: None }, "nuschl.transfer --capability internetClient --capability privateNetworkClientServer --memory 1073741824 --".into()),
        ] {
            let arguments = policy(role).unwrap().arguments();
            let arguments: Vec<_> = arguments.iter().map(|a| a.to_string_lossy()).collect();
            assert_eq!(arguments.join(" "), format!("--nuschl-sandbox --profile {expected}"));
        }
    }

    #[test]
    fn update_verifiers_have_distinct_lpac_profiles_without_external_authority() {
        let ed = policy(Role::Update {
            post_quantum: false,
        })
        .unwrap();
        let ml = policy(Role::Update { post_quantum: true }).unwrap();
        assert_eq!(ed.profile, "nuschl.update.ed25519");
        assert_eq!(ml.profile, "nuschl.update.mldsa44");
        for policy in [ed, ml] {
            assert!(policy.less_privileged);
            assert!(policy.capabilities.is_empty());
            assert!(policy.writable.is_empty());
            assert!(policy.file.is_none());
            assert_eq!(policy.cpu_seconds, Some(5));
            assert_eq!(policy.memory, 256 * 1024 * 1024);
        }
    }

    #[test]
    fn policies_grant_only_what_each_worker_needs() {
        // Profile::history_dir and keys_dir on Windows: siblings in the
        // local data folder.
        let history = Path::new(r"C:\Users\a\AppData\Local\Nuschl\Nuschl\data\history");
        let crypto = Path::new(r"C:\Users\a\AppData\Local\Nuschl\Nuschl\data\keys");
        let offer = Path::new(r"C:\Users\a\Documents\offer.zip");
        let content = policy(Role::Content {
            cpu_seconds: 7,
            arguments: &[],
        })
        .unwrap();
        assert_eq!(content.profile, "nuschl.content");
        assert_eq!(content.cpu_seconds, Some(7));
        assert_eq!(content.memory, 512 << 20);
        let math = policy(Role::Math).unwrap();
        assert_eq!(math.profile, "nuschl.math");
        assert_eq!(math.cpu_seconds, Some(120));
        assert_eq!(math.memory, 512 << 20);
        let storage = policy(Role::Storage { directory: history }).unwrap();
        assert_eq!(storage.writable, [history]);
        assert!(storage.profile.starts_with("nuschl.storage."));
        let keys = policy(Role::Crypto { directory: crypto }).unwrap();
        assert_eq!(keys.writable, [crypto]);
        assert!(keys.profile.starts_with("nuschl.crypto."));
        assert_ne!(
            keys.profile,
            policy(Role::Crypto { directory: history }).unwrap().profile
        );
        // Not the container earlier versions granted the whole data folder.
        let parent = history.parent().unwrap();
        let whole = policy(Role::Storage { directory: parent }).unwrap();
        assert_ne!(storage.profile, whole.profile);
        let media = policy(Role::Media).unwrap();
        assert_eq!(media.capabilities, ["microphone"]);
        let send = policy(Role::Transfer { file: Some(offer) }).unwrap();
        let receive = policy(Role::Transfer { file: None }).unwrap();
        assert_eq!(send.file.as_deref(), Some(offer));
        assert_eq!(receive.file, None);
        assert!(
            policy(Role::MusicbotCodec {
                operation: "decode"
            })
            .is_none()
        );
        for policy in [&content, &math, &storage, &keys, &media, &send, &receive] {
            // Only transfers reach the network; only storage and crypto get
            // a directory; every worker has a memory limit.
            let network = policy.capabilities.iter().any(|c| c.contains("Client"));
            assert_eq!(network, policy.profile == "nuschl.transfer", "{policy:?}");
            let directories = !policy.writable.is_empty();
            assert_eq!(
                directories,
                std::ptr::eq(policy, &storage) || std::ptr::eq(policy, &keys)
            );
            assert!(policy.memory >= 512 << 20);
            assert!(
                policy
                    .capabilities
                    .iter()
                    .all(|c| c != "internetClientServer")
            );
        }
        assert!(media.cpu_seconds.is_none() && storage.cpu_seconds.is_none());
        for policy in [&content, &math, &storage, &keys] {
            assert!(policy.less_privileged, "{policy:?}");
        }
        assert!(!media.less_privileged && !send.less_privileged);
    }
}
