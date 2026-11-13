// SPDX-License-Identifier: EUPL-1.2
//! Finished cover answers on disk, shared by every viewer, so the library and
//! the artwork worker handle each cover and size at most once a day.
//!
//! Each file holds one line with its key as JSON, then the answer the web page
//! receives. File names are hashes of the key; the stored key is compared on
//! every read, so a name collision is only a miss. Answers are checked again
//! when read, so only well-formed RGBA leaves the cache.
use axum::body::Bytes;
use md5::{Digest, Md5};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

/// How long a cover is served before the library is asked again.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// At most this many covers, however small, so the index and its scans stay
/// small too.
pub const MAX_ENTRIES: usize = 4096;
const TEMPORARY: &str = ".tmp-";

/// The library's key, the cover ID and the edge length.
pub type CoverKey = (String, String, u32);

pub struct CoverCache {
    directory: PathBuf,
    budget: u64,
    index: Mutex<Index>,
}

#[derive(Default)]
struct Index {
    bytes: u64,
    tick: u64,
    /// By file name.
    entries: HashMap<String, Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    size: u64,
    stored: SystemTime,
    used: u64,
}

impl CoverCache {
    /// Indexes what an earlier run left, oldest first, and drops whatever is
    /// expired, unfinished or beyond `budget` bytes or [`MAX_ENTRIES`]. A zero
    /// budget turns the cache off and empties it.
    pub fn open(directory: PathBuf, budget: u64) -> std::io::Result<Self> {
        let mut found = Vec::new();
        match std::fs::read_dir(&directory) {
            Ok(listing) => {
                for item in listing {
                    let item = item?;
                    let name = item.file_name().to_string_lossy().into_owned();
                    let metadata = item.metadata()?;
                    let stored = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    if name.starts_with(TEMPORARY) || !metadata.is_file() || expired(stored) {
                        remove(&directory.join(&name));
                    } else {
                        found.push((name, metadata.len(), stored));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if budget > 0 {
            nuschl_session::layout::create_private_dir(&directory)?;
        }
        found.sort_by_key(|(_, _, stored)| *stored);
        let mut index = Index::default();
        for (name, size, stored) in found {
            index.tick += 1;
            index.bytes += size;
            let used = index.tick;
            index.entries.insert(name, Entry { size, stored, used });
        }
        let cache = Self {
            directory,
            budget,
            index: Mutex::new(Index::default()),
        };
        for name in cache.make_room(&mut index, 0, 0) {
            remove(&cache.directory.join(name));
        }
        *cache.lock() = index;
        Ok(cache)
    }

    /// A cache that keeps nothing and touches no files.
    pub fn disabled() -> Self {
        Self {
            directory: PathBuf::new(),
            budget: 0,
            index: Mutex::new(Index::default()),
        }
    }

    pub async fn get(&self, key: &CoverKey) -> Option<Bytes> {
        let name = file_name(key);
        // The lock is never held across an await.
        let size = {
            let mut index = self.lock();
            let entry = *index.entries.get(&name)?;
            if expired(entry.stored) {
                index.bytes -= entry.size;
                index.entries.remove(&name);
                None
            } else {
                index.tick += 1;
                let tick = index.tick;
                if let Some(entry) = index.entries.get_mut(&name) {
                    entry.used = tick;
                }
                Some(entry.size)
            }
        };
        let path = self.directory.join(&name);
        let Some(size) = size else {
            discard(&path).await;
            return None;
        };
        match read_at_most(&path, size)
            .await
            .and_then(|file| answer(key, file))
        {
            Some(Ok(body)) => Some(body),
            // Another key with the same file name: leave it alone.
            Some(Err(Collision)) => None,
            None => {
                {
                    let mut index = self.lock();
                    if let Some(entry) = index.entries.remove(&name) {
                        index.bytes -= entry.size;
                    }
                }
                discard(&path).await;
                None
            }
        }
    }

    /// Keeps `body` unless it would take more than an eighth of the budget,
    /// so a few large covers cannot push out everything else.
    pub async fn insert(&self, key: &CoverKey, body: &[u8]) {
        let mut file = match serde_json::to_vec(key) {
            Ok(line) => line,
            Err(_) => return,
        };
        file.push(b'\n');
        file.extend_from_slice(body);
        let size = file.len() as u64;
        if self.budget == 0 || size > self.budget / 8 {
            return;
        }
        let name = file_name(key);
        let temporary = self
            .directory
            .join(format!("{TEMPORARY}{}", uuid::Uuid::new_v4().simple()));
        if let Err(error) = write(&temporary, &self.directory.join(&name), &file).await {
            tracing::warn!("cover cache: {error}");
            discard(&temporary).await;
            return;
        }
        let evicted = {
            let mut index = self.lock();
            if let Some(old) = index.entries.remove(&name) {
                index.bytes -= old.size;
            }
            let evicted = self.make_room(&mut index, size, 1);
            index.tick += 1;
            index.bytes += size;
            let entry = Entry {
                size,
                stored: SystemTime::now(),
                used: index.tick,
            };
            index.entries.insert(name, entry);
            evicted
        };
        for name in evicted {
            discard(&self.directory.join(name)).await;
        }
    }

    /// Takes expired and then least recently used entries out of the index
    /// until `incoming` more bytes and `slots` more entries fit; returns their
    /// file names.
    fn make_room(&self, index: &mut Index, incoming: u64, slots: usize) -> Vec<String> {
        let mut evicted: Vec<String> = index
            .entries
            .iter()
            .filter(|(_, entry)| expired(entry.stored))
            .map(|(name, _)| name.clone())
            .collect();
        for name in &evicted {
            if let Some(entry) = index.entries.remove(name) {
                index.bytes -= entry.size;
            }
        }
        while index.bytes + incoming > self.budget || index.entries.len() + slots > MAX_ENTRIES {
            let Some(oldest) = index
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(name, _)| name.clone())
            else {
                break;
            };
            if let Some(entry) = index.entries.remove(&oldest) {
                index.bytes -= entry.size;
            }
            evicted.push(oldest);
        }
        evicted
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The whole file when it is exactly `size` bytes long; nothing is read past
/// that, so a file that grew cannot make a request read more.
async fn read_at_most(path: &Path, size: u64) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut contents = Vec::with_capacity(usize::try_from(size).ok()?);
    file.take(size + 1).read_to_end(&mut contents).await.ok()?;
    (contents.len() as u64 == size).then_some(contents)
}

fn expired(stored: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(stored)
        .is_ok_and(|age| age >= MAX_AGE)
}

fn file_name(key: &CoverKey) -> String {
    let digest = Md5::digest(serde_json::to_vec(key).unwrap_or_default());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Collision;

/// The stored answer when the file belongs to `key` and holds well-formed
/// pixels no larger than the key's size; `None` for anything damaged.
fn answer(key: &CoverKey, file: Vec<u8>) -> Option<Result<Bytes, Collision>> {
    let split = file.iter().position(|byte| *byte == b'\n')?;
    let stored: CoverKey = serde_json::from_slice(&file[..split]).ok()?;
    if stored != *key {
        return Some(Err(Collision));
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Pixels {
        width: u32,
        height: u32,
        rgba: String,
    }
    let pixels: Pixels = serde_json::from_slice(&file[split + 1..]).ok()?;
    let bytes = u64::from(pixels.width) * u64::from(pixels.height) * 4;
    let fits = (1..=key.2).contains(&pixels.width) && (1..=key.2).contains(&pixels.height);
    if !fits || pixels.rgba.len() as u64 != bytes.div_ceil(3) * 4 {
        return None;
    }
    let text = pixels.rgba.as_bytes();
    let padding = text.iter().rev().take_while(|byte| **byte == b'=').count();
    let alphabet = text[..text.len() - padding]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'));
    if !alphabet || padding as u64 != (3 - bytes % 3) % 3 {
        return None;
    }
    let mut file = file;
    Some(Ok(Bytes::from(file.split_off(split + 1))))
}

async fn write(temporary: &Path, path: &Path, contents: &[u8]) -> std::io::Result<()> {
    tokio::fs::write(temporary, contents).await?;
    tokio::fs::rename(temporary, path).await
}

/// For startup, before anything is served.
fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!("cover cache: {}: {error}", path.display()),
    }
}

async fn discard(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!("cover cache: {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"width":1,"height":1,"rgba":"AAAAAA=="}"#;

    fn key(id: &str) -> CoverKey {
        ("k".into(), id.into(), 300)
    }

    fn file_size(id: &str) -> u64 {
        (serde_json::to_vec(&key(id)).unwrap().len() + 1 + BODY.len()) as u64
    }

    #[tokio::test]
    async fn covers_survive_a_restart_and_the_oldest_leave_first() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("covers");
        let budget = file_size("a") * 8;
        let cache = CoverCache::open(path.clone(), budget).unwrap();
        for id in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            cache.insert(&key(id), BODY).await;
        }
        // Reading "a" makes "b" the least recently used one.
        assert_eq!(cache.get(&key("a")).await.as_deref(), Some(BODY));
        cache.insert(&key("i"), BODY).await;
        assert!(cache.get(&key("b")).await.is_none());
        assert!(!path.join(file_name(&key("b"))).exists());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 8);

        // A new run finds them; a smaller budget keeps the newest files.
        drop(cache);
        let reopened = CoverCache::open(path.clone(), file_size("a") * 2).unwrap();
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 2);
        assert_eq!(reopened.get(&key("i")).await.as_deref(), Some(BODY));
        // A zero budget turns the cache off and empties it.
        drop(reopened);
        let off = CoverCache::open(path.clone(), 0).unwrap();
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        off.insert(&key("a"), BODY).await;
        assert!(off.get(&key("a")).await.is_none());
    }

    #[tokio::test]
    async fn damaged_old_unfinished_and_foreign_files_are_not_served() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("covers");
        let cache = CoverCache::open(path.clone(), 1 << 20).unwrap();
        for id in ["damaged", "grown", "old", "abc", "xyz"] {
            cache.insert(&key(id), BODY).await;
        }
        // More than an eighth of the budget is never stored.
        let small = CoverCache::open(directory.path().join("small"), file_size("x") * 4).unwrap();
        small.insert(&key("x"), BODY).await;
        assert!(small.get(&key("x")).await.is_none());

        let damaged = path.join(file_name(&key("damaged")));
        let mut file = std::fs::read(&damaged).unwrap();
        let length = file.len();
        file[length - 4] = b'!';
        std::fs::write(&damaged, file).unwrap();
        assert!(cache.get(&key("damaged")).await.is_none());
        assert!(!damaged.exists(), "damaged files are removed");

        // A file longer than indexed is not read beyond that and not served.
        let grown = path.join(file_name(&key("grown")));
        let mut file = std::fs::read(&grown).unwrap();
        file.extend_from_slice(b" ");
        std::fs::write(&grown, file).unwrap();
        assert!(cache.get(&key("grown")).await.is_none());
        assert!(!grown.exists());

        // Pixels larger than the key's size are not served.
        let tiny: CoverKey = ("k".into(), "tiny".into(), 1);
        cache
            .insert(&tiny, br#"{"width":2,"height":1,"rgba":"AAAAAAAAAAA="}"#)
            .await;
        assert!(cache.get(&tiny).await.is_none());
        assert!(!path.join(file_name(&tiny)).exists());

        // A file whose stored key differs is someone else's, never served.
        let foreign = path.join(file_name(&key("abc")));
        let other = std::fs::read(path.join(file_name(&key("xyz")))).unwrap();
        std::fs::write(&foreign, other).unwrap();
        assert!(cache.get(&key("abc")).await.is_none());
        assert!(foreign.exists());

        // Expired and unfinished files are gone after a restart.
        let old = std::fs::File::options()
            .write(true)
            .open(path.join(file_name(&key("old"))))
            .unwrap();
        old.set_modified(SystemTime::now() - MAX_AGE - Duration::from_secs(1))
            .unwrap();
        std::fs::write(path.join(format!("{TEMPORARY}leftover")), b"x").unwrap();
        drop(cache);
        let reopened = CoverCache::open(path.clone(), 1 << 20).unwrap();
        assert!(reopened.get(&key("old")).await.is_none());
        assert!(!path.join(file_name(&key("old"))).exists());
        assert!(!path.join(format!("{TEMPORARY}leftover")).exists());
    }

    #[tokio::test]
    async fn the_number_of_covers_is_capped_as_well() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("covers");
        let cache = CoverCache::open(path.clone(), 1 << 30).unwrap();
        for index in 0..=MAX_ENTRIES {
            cache.insert(&key(&index.to_string()), BODY).await;
        }
        assert_eq!(cache.lock().entries.len(), MAX_ENTRIES);
        assert!(cache.get(&key("0")).await.is_none(), "the oldest left");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), MAX_ENTRIES);
    }

    #[tokio::test]
    async fn a_disabled_cache_keeps_nothing() {
        let cache = CoverCache::disabled();
        cache.insert(&key("a"), BODY).await;
        assert!(cache.get(&key("a")).await.is_none());
    }
}
