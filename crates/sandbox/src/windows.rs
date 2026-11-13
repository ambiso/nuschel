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
    arguments: &[std::ffi::OsString],
) -> Result<tokio::process::Command, crate::Error> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let policy = policy(role).ok_or(crate::Error::MissingSandboxHelper(
        "musicbot codec containment on this platform",
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
    // An offer's file reaches the worker as an inherited read-only handle
    // (`--file-handle N`, appended by the launch stage), never as a path.
    let data = role.directory().unwrap_or(std::path::Path::new(""));
    command.args(role.arguments(data, None));
    command.args(arguments);
    Ok(command)
}

/// The role's AppContainer and job limits. Everything but audio and the
/// network runs as a less privileged AppContainer (LPAC): not even given
/// what Windows grants every AppContainer. COM, which WASAPI needs, and
/// Winsock refuse an LPAC. No confinement for the music bot's codec children
/// has been designed and proven here.
fn policy(role: Role) -> Option<Policy> {
    let (profile, capabilities): (String, &[&str]) = match role {
        Role::Content { .. } => ("nuschl.content".into(), &[]),
        Role::Math => ("nuschl.math".into(), &[]),
        Role::Update { post_quantum: true } => ("nuschl.update.mldsa44".into(), &[]),
        Role::Update {
            post_quantum: false,
        } => ("nuschl.update.ed25519".into(), &[]),
        Role::Storage { directory } => (scoped_profile("nuschl.storage", directory), &[]),
        Role::Crypto { directory } => (scoped_profile("nuschl.crypto", directory), &[]),
        Role::Media => ("nuschl.media".into(), &["microphone"]),
        // Outgoing connections to the Internet (rendezvous, relay, peers) and
        // connections to and from the local network (direct transit between
        // peers on one LAN), but no inbound connections from the Internet.
        Role::Transfer { .. } => (
            "nuschl.transfer".into(),
            &["internetClient", "privateNetworkClientServer"],
        ),
        Role::MusicbotCodec => return None,
    };
    Some(Policy {
        profile,
        less_privileged: capabilities.is_empty(),
        capabilities: capabilities.iter().map(|&name| name.into()).collect(),
        writable: role.directory().map(Into::into).into_iter().collect(),
        file: match role {
            Role::Transfer { file } => file.map(Into::into),
            _ => None,
        },
        memory: role.memory(),
        cpu_seconds: role.cpu_seconds(),
    })
}

#[cfg(test)]
mod tests {
    use super::policy;
    use crate::Role;
    use std::path::Path;

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
        let content = policy(Role::Content { cpu_seconds: 7 }).unwrap();
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
        assert!(policy(Role::MusicbotCodec).is_none());
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
