// SPDX-License-Identifier: EUPL-1.2
//! Signed in-place updates for the Windows and macOS builds.
//!
//! A release publishes `update.json` and its minisign signature
//! `update.json.minisig`. The manifest lists, per platform, every file of the
//! installation with its download URL, size and SHA-256. Nuschel:
//!
//! 1. fetches both and verifies the signature against the release key built
//!    into the binary (plus `update-key.pub` beside the executable, which is
//!    no extra trust: whoever can write there can replace the program);
//! 2. offers versions newer than its own only (no downgrades);
//! 3. downloads each file into a staging directory inside the installation,
//!    checking size and hash;
//! 4. swaps files by renaming, moving the old ones aside (Windows allows
//!    renaming a running executable), and rolls back if any step fails;
//! 5. removes the old files on the next start.
//!
//! Linux installations update through their package manager (e.g. Nix) and
//! are not handled here.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Where releases are published; `NUSCHEL_UPDATE_URL` overrides it (tests).
pub const DEFAULT_URL: &str = "https://github.com/ambiso/nuschel/releases/latest/download/";
pub const MANIFEST: &str = "update.json";
/// The release signing key (minisign public key file).
pub const RELEASE_KEY: &str = include_str!("../../../release/update-key.pub");
/// An additional trusted key beside the executable (test installations).
pub const LOCAL_KEY: &str = "update-key.pub";

const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 256;
const STAGING: &str = ".nuschel-update";
const RETIRED: &str = ".nuschel-old";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("update server: {0}")]
    Http(String),
    #[error("the update manifest is not signed by the release key")]
    Signature,
    #[error("invalid update manifest: {0}")]
    Manifest(String),
    #[error("{0} did not match its published size or checksum")]
    Corrupt(String),
    #[error("the installation folder is not writable: {0}")]
    NotWritable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: String,
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
        // Nuschel.app/Contents/MacOS/nuschel → Nuschel.app
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
        .user_agent(format!("Nuschel/{}", env!("CARGO_PKG_VERSION")))
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

/// The update source: `NUSCHEL_UPDATE_URL` (plain HTTP allowed, for tests on
/// loopback; signatures still apply) or the release page.
pub fn base_url() -> String {
    let mut url = std::env::var("NUSCHEL_UPDATE_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

/// Ask the release server for a newer version for this platform.
pub fn check(root: Option<&Path>, current: &str) -> Result<Option<Update>, Error> {
    let Some(platform) = platform() else {
        return Ok(None);
    };
    check_at(&base_url(), &trusted_keys(root), platform, current)
}

pub fn check_at(
    base: &str,
    keys: &[minisign_verify::PublicKey],
    platform: &str,
    current: &str,
) -> Result<Option<Update>, Error> {
    let agent = agent();
    let manifest = fetch(&agent, &format!("{base}{MANIFEST}"), MAX_MANIFEST)?;
    let signature = fetch(&agent, &format!("{base}{MANIFEST}.minisig"), 4096)?;
    let signature = String::from_utf8(signature).map_err(|_| Error::Signature)?;
    let (manifest, files) = verify(&manifest, &signature, keys, platform)?;
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
#[derive(Debug)]
pub struct Staged {
    root: PathBuf,
    staging: PathBuf,
    files: Vec<PathBuf>,
}

/// Download every file into `<root>/.nuschel-update`, checking size and hash.
/// `progress` receives bytes done and the total.
pub fn download(
    update: &Update,
    root: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<Staged, Error> {
    let staging = root.join(STAGING);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| Error::NotWritable(error.to_string()))?;
    let total = update.files.iter().map(|file| file.size).sum();
    let mut done = 0;
    let agent = agent();
    let mut files = Vec::new();
    for file in &update.files {
        let relative = safe_relative(&file.path)?;
        let target = staging.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
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
        let mut output = fs::File::create(&target)?;
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
    Ok(Staged {
        root: root.to_path_buf(),
        staging,
        files,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Swap the staged files in. Each replaced file moves to `<root>/.nuschel-old`
/// (a running executable can be renamed but not overwritten on Windows). Any
/// failure puts every file back.
pub fn apply(staged: Staged) -> Result<(), Error> {
    let retired = staged.root.join(RETIRED).join(unique());
    let mut moved: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let result = (|| -> Result<(), Error> {
        for relative in &staged.files {
            let live = staged.root.join(relative);
            let backup = if live.exists() {
                let backup = retired.join(relative);
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&live, &backup)?;
                Some(backup)
            } else {
                None
            };
            moved.push((live.clone(), backup));
            if let Some(parent) = live.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(staged.staging.join(relative), &live)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let executable = relative
                    .components()
                    .any(|component| component.as_os_str() == "MacOS");
                if executable {
                    fs::set_permissions(&live, fs::Permissions::from_mode(0o755))?;
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (live, backup) in moved.into_iter().rev() {
            let _ = fs::remove_file(&live);
            if let Some(backup) = backup {
                let _ = fs::rename(backup, &live);
            }
        }
        let _ = fs::remove_dir_all(&staged.staging);
        return Err(error);
    }
    let _ = fs::remove_dir_all(&staged.staging);
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
    let _ = fs::remove_dir_all(root.join(STAGING));
    if let Ok(entries) = fs::read_dir(root.join(RETIRED)) {
        for entry in entries.flatten() {
            let _ = fs::remove_dir_all(entry.path());
        }
        let _ = fs::remove_dir(root.join(RETIRED));
    }
}

/// Read and verify everything without the network (used by tests and by
/// `nuschel-cli` to vet a release before publishing).
pub fn verify_files(root: &Path, files: &[FileEntry]) -> Result<(), Error> {
    for file in files {
        let path = root.join(safe_relative(&file.path)?);
        let bytes = fs::read(&path)?;
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
            "platforms": { "windows-x86_64": [{
                "path": path, "url": "https://example.invalid/a",
                "sha256": "00".repeat(32), "size": 1
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn the_release_key_parses() {
        assert!(minisign_verify::PublicKey::decode(RELEASE_KEY.trim()).is_ok());
    }

    #[test]
    fn only_signed_manifests_are_accepted() {
        let (secret, public) = keypair();
        let (_, other) = keypair();
        let body = manifest("9.0.0", "nuschel.exe");
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
            ".nuschel-old/x",
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
    fn only_newer_versions_are_offered() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    fn installation() -> (tempfile::TempDir, Vec<FileEntry>) {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("nuschel.exe"), b"old program").unwrap();
        fs::write(root.path().join("nuschel-media-worker.exe"), b"old worker").unwrap();
        let entry = |path: &str, body: &[u8]| FileEntry {
            path: path.into(),
            url: String::new(),
            sha256: hex(&Sha256::digest(body)),
            size: body.len() as u64,
        };
        (
            root,
            vec![
                entry("nuschel.exe", b"new program"),
                entry("nuschel-media-worker.exe", b"new worker"),
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
            root: root.to_path_buf(),
            staging,
            files: files.iter().map(|(path, _)| PathBuf::from(path)).collect(),
        }
    }

    #[test]
    fn apply_swaps_every_file_and_cleanup_removes_the_old_ones() {
        let (root, entries) = installation();
        let staged = stage(
            root.path(),
            &[
                ("nuschel.exe", b"new program"),
                ("nuschel-media-worker.exe", b"new worker"),
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
        let mut staged = stage(root.path(), &[("nuschel.exe", b"new program")]);
        staged.files.push(PathBuf::from("nuschel-media-worker.exe"));
        assert!(apply(staged).is_err());
        assert_eq!(
            fs::read(root.path().join("nuschel.exe")).unwrap(),
            b"old program"
        );
        assert_eq!(
            fs::read(root.path().join("nuschel-media-worker.exe")).unwrap(),
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
            "platforms": { "windows-x86_64": [{
                "path": "nuschel.exe", "url": format!("{base}nuschel.exe"),
                "sha256": hex(&Sha256::digest(&body)), "size": body.len()
            }]}
        }))
        .unwrap();
        let signature = sign(&secret, &manifest);
        let routes: BTreeMap<String, Vec<u8>> = [
            ("/update.json".to_string(), manifest),
            ("/update.json.minisig".to_string(), signature.into_bytes()),
            ("/nuschel.exe".to_string(), body),
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
        let update = check_at(&base, &[public], "windows-x86_64", "0.1.0")
            .unwrap()
            .expect("a newer version");
        assert_eq!(update.version.to_string(), "9.1.0");
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("nuschel.exe"), b"old").unwrap();
        let mut reported = 0;
        let staged = download(&update, root.path(), |done, _| reported = done).unwrap();
        assert_eq!(reported, 11);
        apply(staged).unwrap();
        assert_eq!(
            fs::read(root.path().join("nuschel.exe")).unwrap(),
            b"new program"
        );
    }
}
