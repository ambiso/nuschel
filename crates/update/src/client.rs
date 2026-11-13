use crate::{
    Error, FileEntry, Manifest,
    manifest::safe_relative,
    supervisor::{VerifierPaths, verify_hybrid},
};
use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// A capability for the installation folder: every update file operation goes
/// through it, so nothing outside the folder can be read, written or replaced,
/// even through a symlink planted inside it.
fn open_root(root: &Path) -> Result<Dir, Error> {
    Dir::open_ambient_dir(root, ambient_authority())
        .map_err(|error| Error::NotWritable(error.to_string()))
}

/// Where releases are published; `NUSCHL_UPDATE_URL` overrides it (tests).
pub const DEFAULT_URL: &str = "https://github.com/ambiso/nuschl/releases/latest/download/";
pub const MANIFEST: &str = "update.json";
/// Public hybrid pairs are embedded, never read from an adjacent writable file.
pub const RELEASE_TRUST: &str = include_str!("../update-trust.json");

const MAX_MANIFEST: u64 = 256 * 1024;
const STAGING: &str = ".nuschl-update";
const RETIRED: &str = ".nuschl-old";

/// A newer release for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub expires: u64,
    pub version: semver::Version,
    pub notes: Option<String>,
    pub files: Vec<FileEntry>,
}

/// This build's platform key in the manifest, if it updates itself.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(target_os = "macos") {
        Some("macos-universal")
    } else {
        None
    }
}

/// The folder an update replaces files in: the executable's folder on
/// Windows, the `.app` bundle on macOS.
pub fn install_root(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?;
    if cfg!(target_os = "macos") {
        // Nuschl.app/Contents/MacOS/nuschl → Nuschl.app
        let contents = directory.parent()?;
        (directory.file_name()? == "MacOS" && contents.file_name()? == "Contents")
            .then(|| contents.parent().map(Path::to_path_buf))
            .flatten()
    } else {
        Some(directory.to_path_buf())
    }
}

/// Embedded production trust; there is no runtime trust-file override.
pub fn trusted_keys() -> Result<Vec<crate::auth_format::PublicKey>, Error> {
    let keys = crate::trust::parse(RELEASE_TRUST.as_bytes()).map_err(|_| Error::Signature)?;
    if keys.is_empty() {
        return Err(Error::Unenrolled);
    }
    Ok(keys)
}

/// The newest manifest this installation has accepted, kept next to the
/// settings. Signed release times prevent rollback; the local clock also
/// enforces signed expiry and must be reasonably accurate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seen {
    pub version: semver::Version,
    pub released: u64,
}

/// Accept `manifest` only if neither its release time nor its version is
/// older than the newest manifest seen before, and remember it.
pub fn remember(state: &Path, manifest: &Manifest) -> Result<(), Error> {
    let version = semver::Version::parse(&manifest.version)
        .map_err(|error| Error::Manifest(format!("version: {error}")))?;
    let current = Seen {
        version,
        released: manifest.released,
    };
    let seen = match fs::File::open(state) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(4096).read_to_end(&mut bytes)?;
            Some(
                serde_json::from_slice::<Seen>(&bytes)
                    .map_err(|error| Error::State(format!("{}: {error}", state.display())))?,
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(seen) = &seen {
        if current.released < seen.released || current.version < seen.version {
            return Err(Error::Stale(manifest.version.clone()));
        }
        if *seen == current {
            return Ok(());
        }
    }
    // Durable before it replaces the old state: a damaged file fails closed.
    let temporary = state.with_extension(format!("{}.tmp", unique()));
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec(&current).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    fs::rename(&temporary, state)?;
    Ok(())
}

/// Whether `candidate` is newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(candidate), Ok(current)) => candidate > current,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(format!("Nuschl/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn fetch(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, Error> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| Error::Http(error.to_string()))?;
    // ureq treats reaching the configured read limit as an error, even when
    // the body ends exactly there. Reserve one sentinel byte, then enforce our
    // own inclusive bound (the signature has one exact accepted length).
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit + 1)
        .read_to_vec()
        .map_err(|error| Error::Http(error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(Error::Http("oversized update response".into()));
    }
    Ok(bytes)
}

/// The update source: `NUSCHL_UPDATE_URL` (plain HTTP allowed, for tests on
/// loopback; signatures still apply) or the release page.
pub fn base_url() -> String {
    let mut url = std::env::var("NUSCHL_UPDATE_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

/// Ask the release server for a newer version for this platform. `state` is
/// where the newest manifest seen is remembered (see [`remember`]).
pub fn check(_root: Option<&Path>, current: &str, state: &Path) -> Result<Option<Update>, Error> {
    let Some(platform) = platform() else {
        return Ok(None);
    };
    check_at(
        &base_url(),
        &trusted_keys()?,
        &VerifierPaths::discover()?,
        platform,
        current,
        state,
    )
}

pub fn check_at(
    base: &str,
    keys: &[crate::auth_format::PublicKey],
    workers: &VerifierPaths,
    platform: &str,
    current: &str,
    state: &Path,
) -> Result<Option<Update>, Error> {
    if keys.is_empty() {
        return Err(Error::Unenrolled);
    }
    let agent = agent();
    let manifest = fetch(&agent, &format!("{base}{MANIFEST}"), MAX_MANIFEST)?;
    let signature = fetch(
        &agent,
        &format!("{base}{MANIFEST}.sig"),
        crate::auth_format::SIGNATURE_BYTES as u64,
    )?;
    let manifest = verify_hybrid(&manifest, &signature, keys, workers, now()?)?;
    let files = manifest
        .platforms
        .get(platform)
        .cloned()
        .ok_or_else(|| Error::Manifest(format!("no files for {platform}")))?;
    remember(state, &manifest)?;
    if !is_newer(&manifest.version, current) {
        return Ok(None);
    }
    Ok(Some(Update {
        expires: manifest.expires,
        version: semver::Version::parse(&manifest.version)
            .map_err(|error| Error::Manifest(error.to_string()))?,
        notes: manifest.notes,
        files,
    }))
}

/// Downloaded and verified files, ready to swap in.
pub struct Staged {
    expires: u64,
    root: Dir,
    files: Vec<PathBuf>,
}

/// Download every file into `<root>/.nuschl-update`, checking size and hash.
/// `progress` receives bytes done and the total.
pub fn download(
    update: &Update,
    root: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<Staged, Error> {
    require_current(update.expires)?;
    let dir = open_root(root)?;
    let _ = dir.remove_dir_all(STAGING);
    dir.create_dir_all(STAGING)
        .map_err(|error| Error::NotWritable(error.to_string()))?;
    let total = update.files.iter().map(|file| file.size).sum();
    let mut done = 0;
    let agent = agent();
    let mut files = Vec::new();
    for file in &update.files {
        let relative = safe_relative(&file.path)?;
        let target = Path::new(STAGING).join(&relative);
        if let Some(parent) = target.parent() {
            dir.create_dir_all(parent)?;
        }
        let mut response = agent
            .get(&file.url)
            .call()
            .map_err(|error| Error::Http(error.to_string()))?;
        let mut reader = response
            .body_mut()
            .with_config()
            .limit(file.size + 1)
            .reader();
        let mut output = dir.create(&target)?;
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let count = reader
                .read(&mut buffer)
                .map_err(|error| Error::Http(error.to_string()))?;
            if count == 0 {
                break;
            }
            written += count as u64;
            if written > file.size {
                return Err(Error::Corrupt(file.path.clone()));
            }
            hasher.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
            progress(done + written, total);
        }
        output.sync_all()?;
        let digest = hex::encode(hasher.finalize());
        if written != file.size || !digest.eq_ignore_ascii_case(&file.sha256) {
            return Err(Error::Corrupt(file.path.clone()));
        }
        done += written;
        files.push(relative);
    }
    Ok(Staged {
        root: dir,
        files,
        expires: update.expires,
    })
}

fn now() -> Result<u64, Error> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| Error::Expired)
}
fn require_current(expires: u64) -> Result<(), Error> {
    if expires <= now()? {
        Err(Error::Expired)
    } else {
        Ok(())
    }
}

/// Swap the staged files in. Each replaced file moves to `<root>/.nuschl-old`
/// (a running executable can be renamed but not overwritten on Windows). Any
/// failure puts every file back.
pub fn apply(staged: Staged) -> Result<(), Error> {
    require_current(staged.expires)?;
    let dir = &staged.root;
    let retired = Path::new(RETIRED).join(unique());
    let mut moved: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let result = (|| -> Result<(), Error> {
        for relative in &staged.files {
            let backup = if dir.exists(relative) {
                let backup = retired.join(relative);
                if let Some(parent) = backup.parent() {
                    dir.create_dir_all(parent)?;
                }
                dir.rename(relative, dir, &backup)?;
                Some(backup)
            } else {
                None
            };
            moved.push((relative.clone(), backup));
            if let Some(parent) = relative
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                dir.create_dir_all(parent)?;
            }
            dir.rename(Path::new(STAGING).join(relative), dir, relative)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let executable = relative
                    .components()
                    .any(|component| component.as_os_str() == "MacOS");
                if executable {
                    dir.set_permissions(
                        relative,
                        cap_std::fs::Permissions::from_std(fs::Permissions::from_mode(0o755)),
                    )?;
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (live, backup) in moved.into_iter().rev() {
            let _ = dir.remove_file(&live);
            if let Some(backup) = backup {
                let _ = dir.rename(backup, dir, &live);
            }
        }
        let _ = dir.remove_dir_all(STAGING);
        return Err(error);
    }
    let _ = dir.remove_dir_all(STAGING);
    Ok(())
}

fn unique() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{}", now.as_millis(), std::process::id())
}

/// Remove what earlier updates left behind; files still in use stay until
/// the next start.
pub fn cleanup(root: &Path) {
    let Ok(dir) = open_root(root) else {
        return;
    };
    let _ = dir.remove_dir_all(STAGING);
    if let Ok(entries) = dir.read_dir(RETIRED) {
        for entry in entries.flatten() {
            let _ = dir.remove_dir_all(Path::new(RETIRED).join(entry.file_name()));
        }
        let _ = dir.remove_dir(RETIRED);
    }
}

/// Read and verify everything without the network (used by tests and by
/// `nuschl-cli` to vet a release before publishing).
pub fn verify_files(root: &Path, files: &[FileEntry]) -> Result<(), Error> {
    let dir = open_root(root)?;
    for file in files {
        let bytes = dir.read(safe_relative(&file.path)?)?;
        if bytes.len() as u64 != file.size || hex::encode(Sha256::digest(&bytes)) != file.sha256 {
            return Err(Error::Corrupt(file.path.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn a_replayed_older_manifest_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("update-state.json");
        let release = |version: &str, released| Manifest {
            format: 2,
            algorithm: crate::trust::ALGORITHM_NAME.into(),
            expires: u64::MAX,
            version: version.into(),
            released,
            notes: None,
            platforms: BTreeMap::new(),
        };
        remember(&state, &release("0.2.0", 100)).unwrap();
        remember(&state, &release("0.3.0", 200)).unwrap();
        // Checking again finds the same manifest.
        remember(&state, &release("0.3.0", 200)).unwrap();
        // Still newer than an installed 0.1.0, but older than what was seen.
        for (version, released) in [("0.2.0", 100), ("0.2.9", 300), ("0.4.0", 150)] {
            assert!(
                matches!(
                    remember(&state, &release(version, released)),
                    Err(Error::Stale(_))
                ),
                "{version} {released}"
            );
        }
        remember(&state, &release("0.4.0", 300)).unwrap();
        let seen: Seen = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert_eq!(
            (seen.version.to_string(), seen.released),
            ("0.4.0".into(), 300)
        );
        // A damaged state fails closed rather than forgetting what was seen.
        fs::write(&state, b"{").unwrap();
        assert!(matches!(
            remember(&state, &release("0.5.0", 400)),
            Err(Error::State(_))
        ));
    }

    #[test]
    fn same_version_metadata_refresh_advances_state_without_bypassing_clock_or_rollback() {
        const NOW: u64 = 1_800_000_000;
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("refresh-state.json");
        let raw = |released, expires| {
            serde_json::to_vec(&serde_json::json!({
                "format": 2, "algorithm": crate::trust::ALGORITHM_NAME,
                "version": "0.1.0", "released": released, "expires": expires,
                "platforms": {"windows-x86_64": [{"path": "nuschl.exe",
                    "url": "https://example.invalid/v0.1.0/nuschl.exe",
                    "sha256": "ab".repeat(32), "size": 11}]}
            }))
            .unwrap()
        };
        let original = crate::manifest::parse(&raw(NOW - 100, NOW + 100), NOW).unwrap();
        let renewed_bytes = raw(NOW, NOW + 86400);
        let renewed = crate::manifest::parse(&renewed_bytes, NOW).unwrap();
        assert_eq!(original.platforms, renewed.platforms);
        remember(&state, &original).unwrap();
        remember(&state, &renewed).unwrap();
        let seen: Seen = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert_eq!(seen.version.to_string(), "0.1.0");
        assert_eq!(seen.released, NOW);
        assert!(!is_newer(&renewed.version, &original.version));
        assert!(matches!(remember(&state, &original), Err(Error::Stale(_))));
        assert!(matches!(
            crate::manifest::parse(&renewed_bytes, NOW + 86400),
            Err(Error::Expired)
        ));
        assert!(matches!(
            crate::manifest::parse(&renewed_bytes, NOW - 301),
            Err(Error::Manifest(_))
        ));
        assert!(crate::manifest::parse(&renewed_bytes, NOW - 300).is_ok());
    }

    #[test]
    fn expiry_is_rechecked_before_network_and_before_installation() {
        let (root, files) = installation();
        let update = Update {
            expires: 0,
            version: semver::Version::new(9, 0, 0),
            notes: None,
            files,
        };
        assert!(matches!(
            download(&update, root.path(), |_, _| panic!("expired download")),
            Err(Error::Expired)
        ));
        let mut staged = stage(root.path(), &[("nuschl.exe", b"replacement")]);
        staged.expires = 0;
        assert!(matches!(apply(staged), Err(Error::Expired)));
        assert_eq!(
            fs::read(root.path().join("nuschl.exe")).unwrap(),
            b"old program"
        );
    }

    #[test]
    fn only_newer_versions_are_offered() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    fn installation() -> (tempfile::TempDir, Vec<FileEntry>) {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("nuschl.exe"), b"old program").unwrap();
        fs::write(root.path().join("nuschl-media-worker.exe"), b"old worker").unwrap();
        let entry = |path: &str, body: &[u8]| FileEntry {
            path: path.into(),
            url: String::new(),
            sha256: hex::encode(Sha256::digest(body)),
            size: body.len() as u64,
        };
        (
            root,
            vec![
                entry("nuschl.exe", b"new program"),
                entry("nuschl-media-worker.exe", b"new worker"),
                entry("licenses/EUPL-1.2", b"licence"),
            ],
        )
    }

    fn stage(root: &Path, files: &[(&str, &[u8])]) -> Staged {
        let staging = root.join(STAGING);
        for (path, body) in files {
            let target = staging.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, body).unwrap();
        }
        Staged {
            expires: u64::MAX,
            root: open_root(root).unwrap(),
            files: files.iter().map(|(path, _)| PathBuf::from(path)).collect(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_in_the_installation_cannot_redirect_writes() {
        let (root, _) = installation();
        let outside = tempfile::tempdir().unwrap();
        // The staging folder is a symlink to somewhere else.
        std::os::unix::fs::symlink(outside.path(), root.path().join(STAGING)).unwrap();
        let mut files = vec![FileEntry {
            path: "nuschl.exe".into(),
            url: "http://127.0.0.1:9/".into(),
            sha256: "00".repeat(32),
            size: 1,
        }];
        let update = Update {
            expires: u64::MAX,
            version: semver::Version::new(9, 0, 0),
            notes: None,
            files: std::mem::take(&mut files),
        };
        // download() replaces the staging folder inside the root; whatever
        // happens, nothing may appear in the directory the symlink pointed to.
        let _ = download(&update, root.path(), |_, _| {});
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        // A live file that is a symlink to an outside file is moved aside, not
        // written through.
        let target = outside.path().join("victim");
        fs::write(&target, b"keep").unwrap();
        fs::remove_file(root.path().join("nuschl.exe")).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("nuschl.exe")).unwrap();
        apply(stage(root.path(), &[("nuschl.exe", b"new program")])).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn apply_swaps_every_file_and_cleanup_removes_the_old_ones() {
        let (root, entries) = installation();
        let staged = stage(
            root.path(),
            &[
                ("nuschl.exe", b"new program"),
                ("nuschl-media-worker.exe", b"new worker"),
                ("licenses/EUPL-1.2", b"licence"),
            ],
        );
        apply(staged).unwrap();
        verify_files(root.path(), &entries).unwrap();
        assert!(root.path().join(RETIRED).exists());
        cleanup(root.path());
        assert!(!root.path().join(RETIRED).exists());
        assert!(!root.path().join(STAGING).exists());
    }

    #[test]
    fn a_failed_swap_restores_the_old_files() {
        let (root, _) = installation();
        // The second staged file is missing: the swap fails half way.
        let mut staged = stage(root.path(), &[("nuschl.exe", b"new program")]);
        staged.files.push(PathBuf::from("nuschl-media-worker.exe"));
        assert!(apply(staged).is_err());
        assert_eq!(
            fs::read(root.path().join("nuschl.exe")).unwrap(),
            b"old program"
        );
        assert_eq!(
            fs::read(root.path().join("nuschl-media-worker.exe")).unwrap(),
            b"old worker"
        );
    }
}
