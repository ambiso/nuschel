// SPDX-License-Identifier: EUPL-1.2
//! The one-time Nuschel → Nuschl move, before any settings or workers open.
//!
//! Mumble certificates, history and security policy survive. Experimental
//! end-to-end keys belong to the old crypto namespace: retain the entire store
//! outside both workers' writable folders rather than trying to decrypt it or
//! silently deleting it. Quit older clients before migrating a profile.
use crate::layout::{self, Layout, Migration, Outcome};
use std::{fs, io, path::Path};

const MARKER: &str = "nuschl-namespace-v1";
const OLD_POLICY: &str = "nuschel-policy.json";

pub(crate) fn migrate(old: Option<&Layout>, new: &Layout) -> io::Result<Migration> {
    layout::create_private_dir(&new.config)?;
    let _lock = layout::take_lock(&new.config.join(layout::IDENTITY_LOCK))?;
    let mut report = Migration::default();
    let mut locks = Vec::new();
    if let Some(old) = old {
        old.check()?;
        // Keep the lock files at their original paths: an older binary must
        // never acquire a replacement lock while this move is in progress.
        for directory in [&old.config, &old.identity(), &new.identity()] {
            if directory.is_dir() {
                locks.push(layout::take_lock(&directory.join(layout::IDENTITY_LOCK))?);
            }
        }
        #[cfg(all(windows, not(test)))]
        if !new.config.join(MARKER).try_exists()? {
            retire_windows(Some(old), new)?;
        }
        archive(&old.keys(), &new.data.join("keys.nuschel-v1"), &mut report)?;
        archive(
            &old.config.join("nuschel-identity"),
            &new.data.join("keys.nuschel-config-v1"),
            &mut report,
        )?;
        // macOS has a shared config/data root. A second traversal is harmless.
        // XDG roots can also nest. Each root moves through its own mapping;
        // never traverse another root or copy a destination back into itself.
        let roots = [
            old.config.clone(),
            old.data.clone(),
            old.state.clone(),
            old.cache.clone(),
            new.config.clone(),
            new.data.clone(),
            new.state.clone(),
            new.cache.clone(),
        ];
        for (from, to) in [
            (&old.config, &new.config),
            (&old.data, &new.data),
            (&old.state, &new.state),
            (&old.cache, &new.cache),
        ] {
            merge(from, to, &roots, &mut report)?;
        }
    }
    // Explicit --profile-dir profiles retain their root across the rename.
    // Write the marker before a new worker can create any new-namespace keys.
    // If interrupted before it is written, the archives are already outside
    // keys/, and repeating the move is safe.
    if !new.config.join(MARKER).try_exists()? {
        #[cfg(all(windows, not(test)))]
        if old.is_none() {
            retire_windows(None, new)?;
        }
        archive(&new.keys(), &new.data.join("keys.nuschel-v1"), &mut report)?;
        archive(
            &new.config.join("nuschel-identity"),
            &new.data.join("keys.nuschel-config-v1"),
            &mut report,
        )?;
        for root in [&new.data, &new.config] {
            relocate(
                &root.join(OLD_POLICY),
                &root.join(crate::sticky::FILE),
                false,
                &mut report,
            )?;
        }
        let marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(new.config.join(MARKER))?;
        marker.sync_all()?;
    }
    Ok(report)
}

fn archive(from: &Path, to: &Path, report: &mut Migration) -> io::Result<()> {
    if from.try_exists()? {
        relocate(from, to, true, report)?;
        report.problems.push(format!(
            "The experimental Nuschel end-to-end identity was archived at {}. \
             Nuschl uses a new crypto namespace and creates a new end-to-end identity; \
             the Mumble certificate and local chat history are preserved.",
            to.display()
        ));
    }
    Ok(())
}

/// Merge disjoint roots without overwriting anything, including interrupted
/// earlier moves. A conflict stops startup before a fresh identity can be made.
fn merge(
    from: &Path,
    to: &Path,
    roots: &[std::path::PathBuf],
    report: &mut Migration,
) -> io::Result<()> {
    if !from.try_exists()? || from == to {
        return Ok(());
    }
    layout::create_private_dir(to)?;
    let mut entries = fs::read_dir(from)?.collect::<io::Result<Vec<_>>>()?;
    // SQLite's sidecars must reach the destination before the main database.
    entries.sort_by_key(|entry| (entry.file_name() == layout::HISTORY_FILE, entry.file_name()));
    for entry in entries {
        if roots.contains(&entry.path()) {
            continue;
        }
        let name = entry.file_name();
        if name == layout::IDENTITY_LOCK {
            continue;
        }
        let destination = to.join(if name == OLD_POLICY {
            crate::sticky::FILE.as_ref()
        } else {
            name.as_os_str()
        });
        let kind = entry.file_type()?;
        if kind.is_dir() {
            merge(&entry.path(), &destination, roots, report)?;
            // A lock or a file retained after a failed move can keep it nonempty.
            let _ = fs::remove_dir(entry.path());
        } else {
            relocate(&entry.path(), &destination, false, report)?;
        }
    }
    Ok(())
}

fn relocate(from: &Path, to: &Path, directory: bool, report: &mut Migration) -> io::Result<()> {
    if !from.try_exists()? {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        layout::create_private_dir(parent)?;
    }
    let outcome = if directory {
        layout::move_dir(from, to)
    } else {
        layout::move_file(from, to)
    }?;
    match outcome {
        Outcome::Conflict => {
            return Err(io::Error::other(format!(
                "Rename conflict: {} differs from {}. Both were retained; resolve this before starting Nuschl.",
                from.display(),
                to.display()
            )));
        }
        Outcome::Absent | Outcome::Duplicate => {}
        _ => report
            .moved
            .push(format!("Moved {} to {}.", from.display(), to.display())),
    }
    Ok(())
}

/// The launcher hashes canonical paths (including Windows' verbatim prefix).
/// Reconstruct a now-missing tail too, for retries after an interrupted move.
#[cfg(any(windows, test))]
fn canonical_worker_path(path: &Path) -> io::Result<std::path::PathBuf> {
    let mut parent = std::path::absolute(path)?;
    let mut missing = Vec::new();
    loop {
        match parent.canonicalize() {
            Ok(mut canonical) => {
                canonical.extend(missing.into_iter().rev());
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = parent.file_name().ok_or(error)?.to_owned();
                missing.push(name);
                parent.pop();
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(windows, not(test)))]
fn retire_windows(old: Option<&Layout>, new: &Layout) -> io::Result<()> {
    use nuschl_sandbox::windows_policy::scoped_profile;
    fn retire_legacy_profile(name: &str, directories: &[std::path::PathBuf]) -> io::Result<()> {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        // The unsafe Win32 adapter remains in a short-lived launch-stage process,
        // just as it does when starting a sandbox. No unsafe dependency enters
        // the session library or desktop. These arguments are local paths/names.
        let worker = crate::workers::sibling("nuschl-storage-worker")?;
        let mut command = Command::new(worker);
        command
            .arg("--retire-nuschel-profile")
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .creation_flags(0x0800_0000);
        for directory in directories {
            command.arg("--writable").arg(directory);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "could not retire {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }
    let existing =
        |layout: &Layout| layout.data.exists() || layout.config.join("nuschel-identity").exists();
    if !old.is_some_and(existing)
        && !new.keys().exists()
        && !new.config.join("nuschel-identity").exists()
    {
        return Ok(());
    }
    if old.is_some() {
        for kind in ["content", "math", "media", "transfer"] {
            retire_legacy_profile(&format!("nuschel.{kind}"), &[])?;
        }
    }
    for layout in old.into_iter().chain(std::iter::once(new)) {
        for (kind, directory) in [
            ("storage", layout.data.clone()),
            ("storage", layout.history()),
            ("crypto", layout.keys()),
            ("crypto", layout.config.join("nuschel-identity")),
        ] {
            let directory = canonical_worker_path(&directory)?;
            let name = scoped_profile(&format!("nuschel.{kind}"), &directory);
            // Include the destination on a retry after an interrupted move.
            let paths = [
                directory,
                new.data.clone(),
                new.config.join("nuschel-identity"),
            ];
            retire_legacy_profile(&name, &paths)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn preserves_legacy_profile_and_archives_experimental_crypto_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let old = Layout::under(&temp.path().join("nuschel"));
        let new = Layout::under(&temp.path().join("nuschl"));
        for (root, name, bytes) in [
            (&old.config, "settings.json", b"settings".as_slice()),
            (&old.config, "servers.json", b"bookmarks"),
            (
                &old.identity(),
                "identity.json",
                b"Mumble certificate and private key",
            ),
            (&old.history(), "history.sqlite3", b"history"),
            (
                &old.history(),
                "history.sqlite3-wal",
                b"uncheckpointed history",
            ),
            (&old.data, OLD_POLICY, b"sticky security requirements"),
            (&old.keys(), "keystore", b"old sealed store"),
            (&old.keys(), "store.key", b"old store key"),
            (&old.state, "last-channels.json", b"last channels"),
            (&old.cache, "channels.json", b"cached tree"),
        ] {
            write(&root.join(name), bytes);
        }
        let report = migrate(Some(&old), &new).unwrap();
        assert_eq!(
            fs::read(new.identity().join("identity.json")).unwrap(),
            b"Mumble certificate and private key"
        );
        assert_eq!(
            fs::read(new.history().join("history.sqlite3-wal")).unwrap(),
            b"uncheckpointed history"
        );
        assert_eq!(
            fs::read(new.config.join("servers.json")).unwrap(),
            b"bookmarks"
        );
        assert_eq!(
            fs::read(new.data.join(crate::sticky::FILE)).unwrap(),
            b"sticky security requirements"
        );
        assert_eq!(
            fs::read(new.data.join("keys.nuschel-v1/store.key")).unwrap(),
            b"old store key"
        );
        assert!(!new.keys().exists());
        assert_eq!(report.problems.len(), 1);
        write(&new.keys().join("keystore"), b"new namespace store");
        let second = migrate(Some(&old), &new).unwrap();
        assert!(second.moved.is_empty());
        assert!(second.problems.is_empty());
        assert_eq!(
            fs::read(new.keys().join("keystore")).unwrap(),
            b"new namespace store"
        );
    }

    #[test]
    fn retired_worker_names_use_the_same_canonical_paths_as_the_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().canonicalize().unwrap();
        assert_eq!(canonical_worker_path(temp.path()).unwrap(), existing);
        assert_eq!(
            canonical_worker_path(&temp.path().join("missing/keys")).unwrap(),
            existing.join("missing/keys")
        );
    }

    #[test]
    fn nested_xdg_roots_do_not_copy_another_root_or_the_destination_into_itself() {
        let temp = tempfile::tempdir().unwrap();
        let old = Layout {
            data: temp.path().join("nuschel"),
            config: temp.path().join("nuschel/nuschel"),
            state: temp.path().join("state/nuschel"),
            cache: temp.path().join("cache/nuschel"),
        };
        let new = Layout {
            data: temp.path().join("nuschl"),
            config: temp.path().join("nuschel/nuschl"),
            state: temp.path().join("state/nuschl"),
            cache: temp.path().join("cache/nuschl"),
        };
        write(&old.config.join("settings.json"), b"settings");
        write(&old.history().join(layout::HISTORY_FILE), b"history");
        migrate(Some(&old), &new).unwrap();
        assert_eq!(
            fs::read(new.config.join("settings.json")).unwrap(),
            b"settings"
        );
        assert_eq!(
            fs::read(new.history().join(layout::HISTORY_FILE)).unwrap(),
            b"history"
        );
        assert!(!new.data.join("nuschel").exists());
        assert!(!new.data.join("nuschl").exists());
        assert!(migrate(Some(&old), &new).unwrap().moved.is_empty());
    }

    #[test]
    fn a_real_mumble_identity_and_bookmarks_load_after_the_move() {
        let temp = tempfile::tempdir().unwrap();
        let old = Layout::under(&temp.path().join("nuschel"));
        let root = temp.path().join("nuschl");
        let new = Layout::under(&root);
        let identity = crate::config::ClientIdentity::generate().unwrap();
        let mut bookmark =
            crate::config::ServerBookmark::new("Team", "127.0.0.1", 64738, "Ada").unwrap();
        bookmark.certificate_sha256 = Some("ab".repeat(32));
        write(
            &old.identity().join("identity.json"),
            &serde_json::to_vec(&identity).unwrap(),
        );
        write(
            &old.config.join("settings.json"),
            br#"{"enter_sends":false}"#,
        );
        write(
            &old.config.join("servers.json"),
            &serde_json::to_vec(&vec![bookmark.clone()]).unwrap(),
        );
        write(
            &old.data.join(OLD_POLICY),
            br#"{"127.0.0.1:64738":{"nuschel":true}}"#,
        );
        migrate(Some(&old), &new).unwrap();
        let profile = crate::config::Profile::open(Some(root)).unwrap();
        let loaded = profile.load();
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert!(!loaded.settings.enter_sends);
        assert_eq!(loaded.servers[0].id, bookmark.id);
        assert_eq!(
            loaded.servers[0].certificate_sha256,
            bookmark.certificate_sha256
        );
        let migrated = profile.identity().unwrap();
        assert_eq!(migrated.certificate, identity.certificate);
        assert_eq!(migrated.private_key, identity.private_key);
        assert!(migrated.is_rsa());
        assert!(migrated.to_pkcs12("migration test").is_ok());
        let policy = profile.policy().unwrap();
        assert!(policy.get("127.0.0.1", 64738).check(false, &[]).is_err());
    }

    #[test]
    fn conflicting_identity_stops_startup_and_retains_both() {
        let temp = tempfile::tempdir().unwrap();
        let old = Layout::under(&temp.path().join("nuschel"));
        let new = Layout::under(&temp.path().join("nuschl"));
        write(&old.identity().join("identity.json"), b"old identity");
        write(&new.identity().join("identity.json"), b"other identity");
        assert!(
            migrate(Some(&old), &new)
                .unwrap_err()
                .to_string()
                .contains("Rename conflict")
        );
        assert_eq!(
            fs::read(old.identity().join("identity.json")).unwrap(),
            b"old identity"
        );
        assert_eq!(
            fs::read(new.identity().join("identity.json")).unwrap(),
            b"other identity"
        );
        assert!(!new.config.join(MARKER).exists());
    }

    #[test]
    fn explicit_profile_archives_old_keys_and_renames_policy_only_once() {
        let temp = tempfile::tempdir().unwrap();
        let profile = Layout::under(temp.path());
        write(
            &profile.config.join("nuschel-identity/keystore"),
            b"early keys",
        );
        write(&profile.data.join(OLD_POLICY), b"policy");
        migrate(None, &profile).unwrap();
        assert_eq!(
            fs::read(profile.data.join("keys.nuschel-config-v1/keystore")).unwrap(),
            b"early keys"
        );
        assert_eq!(
            fs::read(profile.data.join(crate::sticky::FILE)).unwrap(),
            b"policy"
        );
        write(&profile.keys().join("state"), b"new state");
        migrate(None, &profile).unwrap();
        assert_eq!(
            fs::read(profile.keys().join("state")).unwrap(),
            b"new state"
        );
    }
}
