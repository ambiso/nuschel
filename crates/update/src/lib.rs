// SPDX-License-Identifier: EUPL-1.2
//! Signed in-place updates for the Windows and macOS builds.
//!
//! A release publishes `update.json` and its minisign signature
//! `update.json.minisig`. The manifest lists, per platform, every file of the
//! installation with its download URL, size and SHA-256. Nuschl:
//!
//! 1. fetches both and verifies the signature against the release key built
//!    into the binary (plus `update-key.pub` beside the executable, which is
//!    no extra trust: whoever can write there can replace the program);
//! 2. offers versions newer than its own only (no downgrades), and refuses a
//!    manifest released before the newest one it has already accepted, so an
//!    old signed manifest cannot be replayed to hold it at an older release;
//! 3. downloads each file into a staging directory inside the installation,
//!    checking size and hash;
//! 4. swaps files by renaming, moving the old ones aside (Windows allows
//!    renaming a running executable), and rolls back if any step fails;
//! 5. removes the old files on the next start.
//!
//! Linux installations update through their package manager (e.g. Nix) and
//! are not handled here.

pub mod auth_format;
pub mod manifest;
pub mod trust;
pub mod verifier_wire;

use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
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
/// The release signing key (minisign public key file).
pub const RELEASE_KEY: &str = include_str!("../update-key.pub");
/// An additional trusted key beside the executable (test installations).
pub const LOCAL_KEY: &str = "update-key.pub";

const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 256;
const STAGING: &str = ".nuschl-update";
const RETIRED: &str = ".nuschl-old";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("update server: {0}")]
    Http(String),
    #[error("the update manifest is not signed by the release key")]
    Signature,
    #[error("the signed update manifest has expired; check for a current signed release")]
    Expired,
    #[error("invalid update manifest: {0}")]
    Manifest(String),
    #[error("{0} did not match its published size or checksum")]
    Corrupt(String),
    #[error("the installation folder is not writable: {0}")]
    NotWritable(String),
    #[error(
        "the update server offered {0}, older than a release already seen; \
         it may be replaying an old manifest"
    )]
    Stale(String),
    #[error("could not read the update state {0}")]
    State(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: String,
    /// Release time in Unix seconds by the signer's clock.
    pub released: u64,
    #[serde(default)]
    pub notes: Option<String>,
    pub platforms: BTreeMap<String, Vec<FileEntry>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Relative to the installation root, with `/` separators.
    pub path: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// A newer release for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
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

/// Keys accepted for the manifest signature.
pub fn trusted_keys(root: Option<&Path>) -> Vec<minisign_verify::PublicKey> {
    let mut keys = Vec::new();
    if let Ok(key) = minisign_verify::PublicKey::decode(RELEASE_KEY.trim()) {
        keys.push(key);
    }
    if let Some(text) = root.and_then(|root| fs::read_to_string(root.join(LOCAL_KEY)).ok())
        && let Ok(key) = minisign_verify::PublicKey::decode(text.trim())
    {
        keys.push(key);
    }
    keys
}

/// Check a manifest's signature and shape. Files are validated for the
/// requested platform only.
pub fn verify(
    manifest: &[u8],
    signature: &str,
    keys: &[minisign_verify::PublicKey],
    platform: &str,
) -> Result<(Manifest, Vec<FileEntry>), Error> {
    let signature =
        minisign_verify::Signature::decode(signature.trim()).map_err(|_| Error::Signature)?;
    if !keys
        .iter()
        .any(|key| key.verify(manifest, &signature, false).is_ok())
    {
        return Err(Error::Signature);
    }
    let parsed: Manifest =
        serde_json::from_slice(manifest).map_err(|error| Error::Manifest(error.to_string()))?;
    semver::Version::parse(&parsed.version)
        .map_err(|error| Error::Manifest(format!("version: {error}")))?;
    let files = parsed
        .platforms
        .get(platform)
        .cloned()
        .ok_or_else(|| Error::Manifest(format!("no files for {platform}")))?;
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(Error::Manifest("unexpected number of files".into()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for file in &files {
        safe_relative(&file.path)?;
        if !seen.insert(file.path.to_lowercase()) {
            return Err(Error::Manifest(format!("{} is listed twice", file.path)));
        }
        if file.size > MAX_FILE
            || file.sha256.len() != 64
            || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Error::Manifest(format!(
                "{}: bad size or checksum",
                file.path
            )));
        }
        if !(file.url.starts_with("https://") || file.url.starts_with("http://")) {
            return Err(Error::Manifest(format!("{}: unsupported URL", file.path)));
        }
    }
    Ok((parsed, files))
}

/// A manifest path as a relative path inside the installation, or an error
/// for anything that could escape it.
fn safe_relative(path: &str) -> Result<PathBuf, Error> {
    let bad = || Error::Manifest(format!("unsafe path {path:?}"));
    if path.is_empty()
        || path.len() > 240
        || path.contains('\\')
        || path.contains(':')
        || !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/ ".contains(c))
    {
        return Err(bad());
    }
    let relative = PathBuf::from(path);
    if relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == STAGING || part == RETIRED)
    {
        Ok(relative)
    } else {
        Err(bad())
    }
}

/// The newest manifest this installation has accepted, kept next to the
/// settings. Only release times from signed manifests are compared, never
/// the local clock, so a wrong system time changes nothing.
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
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| Error::Http(error.to_string()))
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
pub fn check(root: Option<&Path>, current: &str, state: &Path) -> Result<Option<Update>, Error> {
    let Some(platform) = platform() else {
        return Ok(None);
    };
    check_at(&base_url(), &trusted_keys(root), platform, current, state)
}

pub fn check_at(
    base: &str,
    keys: &[minisign_verify::PublicKey],
    platform: &str,
    current: &str,
    state: &Path,
) -> Result<Option<Update>, Error> {
    let agent = agent();
    let manifest = fetch(&agent, &format!("{base}{MANIFEST}"), MAX_MANIFEST)?;
    let signature = fetch(&agent, &format!("{base}{MANIFEST}.minisig"), 4096)?;
    let signature = String::from_utf8(signature).map_err(|_| Error::Signature)?;
    let (manifest, files) = verify(&manifest, &signature, keys, platform)?;
    remember(state, &manifest)?;
    if !is_newer(&manifest.version, current) {
        return Ok(None);
    }
    Ok(Some(Update {
        version: semver::Version::parse(&manifest.version)
            .map_err(|error| Error::Manifest(error.to_string()))?,
        notes: manifest.notes,
        files,
    }))
}

/// Downloaded and verified files, ready to swap in.
pub struct Staged {
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
        let digest = hex(&hasher.finalize());
        if written != file.size || !digest.eq_ignore_ascii_case(&file.sha256) {
            return Err(Error::Corrupt(file.path.clone()));
        }
        done += written;
        files.push(relative);
    }
    Ok(Staged { root: dir, files })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Swap the staged files in. Each replaced file moves to `<root>/.nuschl-old`
/// (a running executable can be renamed but not overwritten on Windows). Any
/// failure puts every file back.
pub fn apply(staged: Staged) -> Result<(), Error> {
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
        if bytes.len() as u64 != file.size || hex(&Sha256::digest(&bytes)) != file.sha256 {
            return Err(Error::Corrupt(file.path.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> (minisign::SecretKey, minisign_verify::PublicKey) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let public =
            minisign_verify::PublicKey::decode(&pair.pk.to_box().unwrap().to_string()).unwrap();
        (pair.sk, public)
    }

    fn sign(secret: &minisign::SecretKey, data: &[u8]) -> String {
        minisign::sign(None, secret, std::io::Cursor::new(data), None, None)
            .unwrap()
            .to_string()
    }

    fn manifest(version: &str, path: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": version,
            "released": 1_800_000_000,
            "platforms": { "windows-x86_64": [{
                "path": path, "url": "https://example.invalid/a",
                "sha256": "00".repeat(32), "size": 1
            }]}
        }))
        .unwrap()
    }

    /// `NUSCHL_UPDATE_DIST=dist cargo test -p nuschl-update -- --ignored`
    /// checks a release made by release/make-update.py against the built-in key.
    #[test]
    #[ignore = "needs a release directory"]
    fn a_published_manifest_verifies_with_the_release_key() {
        let dist = PathBuf::from(std::env::var("NUSCHL_UPDATE_DIST").unwrap());
        let manifest = fs::read(dist.join(MANIFEST)).unwrap();
        let signature = fs::read_to_string(dist.join(format!("{MANIFEST}.minisig"))).unwrap();
        for platform in ["windows-x86_64", "macos-universal"] {
            let (_, files) = verify(&manifest, &signature, &trusted_keys(None), platform).unwrap();
            assert!(!files.is_empty(), "{platform}");
        }
    }

    #[test]
    fn the_release_key_parses() {
        assert!(minisign_verify::PublicKey::decode(RELEASE_KEY.trim()).is_ok());
    }

    #[test]
    fn only_signed_manifests_are_accepted() {
        let (secret, public) = keypair();
        let (_, other) = keypair();
        let body = manifest("9.0.0", "nuschl.exe");
        let signature = sign(&secret, &body);
        assert!(
            verify(
                &body,
                &signature,
                std::slice::from_ref(&public),
                "windows-x86_64"
            )
            .is_ok()
        );
        assert!(matches!(
            verify(&body, &signature, &[other], "windows-x86_64"),
            Err(Error::Signature)
        ));
        let mut tampered = body.clone();
        tampered[20] ^= 1;
        assert!(matches!(
            verify(&tampered, &signature, &[public], "windows-x86_64"),
            Err(Error::Signature)
        ));
    }

    #[test]
    fn paths_cannot_leave_the_installation() {
        let (secret, public) = keypair();
        for path in [
            "../evil.exe",
            "/etc/passwd",
            "C:/x.exe",
            "a\\b.exe",
            "a//b",
            ".nuschl-old/x",
            "",
        ] {
            let body = manifest("9.0.0", path);
            let result = verify(
                &body,
                &sign(&secret, &body),
                std::slice::from_ref(&public),
                "windows-x86_64",
            );
            assert!(
                matches!(result, Err(Error::Manifest(_))),
                "{path:?}: {result:?}"
            );
        }
    }

    #[test]
    fn manifests_without_a_release_time_are_refused() {
        let (secret, public) = keypair();
        let body = br#"{"version":"9.0.0","platforms":{"windows-x86_64":[]}}"#;
        let result = verify(body, &sign(&secret, body), &[public], "windows-x86_64");
        assert!(matches!(result, Err(Error::Manifest(_))), "{result:?}");
    }

    #[test]
    fn a_replayed_older_manifest_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("update-state.json");
        let release = |version: &str, released| Manifest {
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
            sha256: hex(&Sha256::digest(body)),
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

    #[test]
    fn a_full_update_over_loopback_http() {
        // A local server (127.0.0.1 only) serves a signed manifest and one file.
        let (secret, public) = keypair();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let body = b"new program".to_vec();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "version": "9.1.0",
            "released": 1_800_000_000,
            "platforms": { "windows-x86_64": [{
                "path": "nuschl.exe", "url": format!("{base}nuschl.exe"),
                "sha256": hex(&Sha256::digest(&body)), "size": body.len()
            }]}
        }))
        .unwrap();
        let signature = sign(&secret, &manifest);
        let routes: BTreeMap<String, Vec<u8>> = [
            ("/update.json".to_string(), manifest),
            ("/update.json.minisig".to_string(), signature.into_bytes()),
            ("/nuschl.exe".to_string(), body),
        ]
        .into();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let mut request = [0; 2048];
                let count = stream.read(&mut request).unwrap();
                let line = String::from_utf8_lossy(&request[..count]);
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let reply = routes.get(&path).cloned().unwrap_or_default();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                )
                .unwrap();
                stream.write_all(&reply).unwrap();
            }
        });
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("update-state.json");
        let update = check_at(&base, &[public], "windows-x86_64", "0.1.0", &state)
            .unwrap()
            .expect("a newer version");
        assert_eq!(update.version.to_string(), "9.1.0");
        assert!(state.exists());
        fs::write(root.path().join("nuschl.exe"), b"old").unwrap();
        let mut reported = 0;
        let staged = download(&update, root.path(), |done, _| reported = done).unwrap();
        assert_eq!(reported, 11);
        apply(staged).unwrap();
        assert_eq!(
            fs::read(root.path().join("nuschl.exe")).unwrap(),
            b"new program"
        );
    }
}
