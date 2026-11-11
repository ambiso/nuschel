// SPDX-License-Identifier: EUPL-1.2
//! The worker's directory (docs/e2ee.md §3.7, §8.3):
//!
//! | File | Content |
//! | --- | --- |
//! | `store.key` | the store key, 32 bytes, 0600 (for now; see below) |
//! | `keystore` | the device keystore (`DeviceKeystore`), sealed under the store key |
//! | `state` | contacts, pins and key-transparency state (JSON), sealed (purpose 5) |
//! | `recovery` | the recovery key made at creation until it is saved or skipped, sealed |
//!
//! Every write goes to a temporary file that is synced and renamed over the old one,
//! so a crash leaves the old or the new version. Files are created 0600.
//!
//! **Where the store key lives.** For now in `store.key` beside the sealed files. The
//! OS credential store (Secret Service, Keychain, DPAPI) is the follow-up: the
//! supervisor, which may talk to it, will hand the key over with `--store-key-fd`
//! (or a first IPC message), and [`KeySource`] is the only thing that changes. The
//! sealed files and their format stay.
use nuschel_identity::{DeviceKeystore, StoreKey};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

/// The largest file the worker reads back.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// Where the store key comes from.
pub enum KeySource {
    /// A file in the worker's directory, created on first use.
    File(PathBuf),
    /// Supplied by the supervisor (OS credential store); not wired up yet.
    Supplied(StoreKey),
}

pub struct Store {
    directory: PathBuf,
    key: StoreKey,
}

fn read_bounded(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(io::Error::other(format!("{} is too large", path.display())));
    }
    Ok(Some(bytes))
}

fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Writes `bytes` to `path` atomically: a synced temporary file renamed over it.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("new");
    let _ = fs::remove_file(&temporary);
    let mut file = private_options().create_new(true).open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent()
        && let Ok(directory) = fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

impl Store {
    pub fn open(directory: &Path, source: KeySource) -> io::Result<Self> {
        let key = match source {
            KeySource::Supplied(key) => key,
            KeySource::File(path) => match read_bounded(&path)? {
                Some(bytes) => {
                    let array: Zeroizing<[u8; 32]> = Zeroizing::new(
                        bytes[..]
                            .try_into()
                            .map_err(|_| io::Error::other("the store key file is not 32 bytes"))?,
                    );
                    StoreKey::from_bytes(&array)
                }
                None => {
                    let key = StoreKey::generate().map_err(io::Error::other)?;
                    let mut file = private_options().create_new(true).open(&path)?;
                    file.write_all(&key.to_bytes()[..])?;
                    file.sync_all()?;
                    key
                }
            },
        };
        Ok(Self {
            directory: directory.to_owned(),
            key,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    pub fn load_keystore(&self) -> io::Result<Option<DeviceKeystore>> {
        match read_bounded(&self.path("keystore"))? {
            Some(bytes) => DeviceKeystore::open(&self.key, &bytes)
                .map(Some)
                .map_err(|error| io::Error::other(format!("the keystore: {error}"))),
            None => Ok(None),
        }
    }

    pub fn save_keystore(&self, keystore: &DeviceKeystore) -> io::Result<()> {
        let sealed = keystore.seal(&self.key).map_err(io::Error::other)?;
        atomic_write(&self.path("keystore"), &sealed)
    }

    /// A sealed file (`state`, `recovery`), decrypted.
    pub fn load(&self, name: &str) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        match read_bounded(&self.path(name))? {
            Some(bytes) => self
                .key
                .open_state(&bytes)
                .map(Some)
                .map_err(|error| io::Error::other(format!("{name}: {error}"))),
            None => Ok(None),
        }
    }

    pub fn save(&self, name: &str, plaintext: &[u8]) -> io::Result<()> {
        let sealed = self.key.seal_state(plaintext).map_err(io::Error::other)?;
        atomic_write(&self.path(name), &sealed)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.path(name)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_files_round_trip_and_the_key_file_is_private() {
        let directory = tempfile::tempdir().unwrap();
        let open = || {
            Store::open(
                directory.path(),
                KeySource::File(directory.path().join("store.key")),
            )
            .unwrap()
        };
        let store = open();
        assert!(store.load("state").unwrap().is_none());
        store.save("state", b"{}").unwrap();
        assert_eq!(&open().load("state").unwrap().unwrap()[..], b"{}");
        let raw = fs::read(directory.path().join("state")).unwrap();
        assert!(!raw.windows(2).any(|w| w == b"{}"));
        #[cfg(unix)]
        for name in ["store.key", "state"] {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(directory.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        // Another key cannot read it.
        fs::write(directory.path().join("store.key"), [9; 32]).unwrap();
        assert!(open().load("state").is_err());
        store.remove("state").unwrap();
        store.remove("state").unwrap();
    }
}
