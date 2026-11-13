// SPDX-License-Identifier: EUPL-1.2
//! Each library's key names it in cover caches, cover URLs and shared views.
//! Keys are random values kept in `library_keys.json` in the state directory,
//! so they survive restarts and reordering. A library whose name or URL
//! changes gets a new key; keys of libraries no longer configured are dropped.
use crate::subsonic::LibraryIdentity;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read},
    path::Path,
};

const FILE: &str = "library_keys.json";
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Saved {
    library: LibraryIdentity,
    key: String,
}

/// One key per entry of `libraries`, in the same order. Entries with the same
/// name and URL share their key.
pub fn load(directory: &Path, libraries: &[LibraryIdentity]) -> io::Result<Vec<String>> {
    let path = directory.join(FILE);
    let saved: Vec<Saved> = match crate::persist::open_private(&path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(io::Error::other("file exceeds 1 MiB"));
            }
            serde_json::from_slice(&bytes).map_err(io::Error::other)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let key_of = |entries: &[Saved], library: &LibraryIdentity| {
        entries
            .iter()
            .find(|entry| entry.library == *library)
            .map(|entry| entry.key.clone())
    };
    let mut current: Vec<Saved> = Vec::new();
    for library in libraries {
        if key_of(&current, library).is_some() {
            continue;
        }
        let key = key_of(&saved, library)
            .filter(|key| is_key(key) && current.iter().all(|entry| entry.key != *key))
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        current.push(Saved {
            library: library.clone(),
            key,
        });
    }
    if current != saved {
        let bytes = serde_json::to_vec(&current).map_err(io::Error::other)?;
        crate::persist::write(&path, &bytes)?;
    }
    Ok(libraries
        .iter()
        .filter_map(|library| key_of(&current, library))
        .collect())
}

fn is_key(key: &str) -> bool {
    key.len() == 32
        && key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(name: &str, url: &str) -> LibraryIdentity {
        LibraryIdentity {
            name: name.into(),
            url: url.into(),
        }
    }

    #[test]
    fn keys_are_random_and_kept_across_restarts_and_reordering() {
        let directory = tempfile::tempdir().unwrap();
        let jazz = library("Jazz", "https://music.example.org");
        let rock = library("Rock", "https://music.example.org");
        let keys = load(directory.path(), &[jazz.clone(), rock.clone()]).unwrap();
        assert!(keys.iter().all(|key| is_key(key)), "{keys:?}");
        assert_ne!(keys[0], keys[1]);
        let text = std::fs::read_to_string(directory.path().join(FILE)).unwrap();
        assert!(text.contains(&keys[0]) && text.contains(&keys[1]));

        // Another directory knows nothing of these keys.
        let elsewhere = tempfile::tempdir().unwrap();
        let other = load(elsewhere.path(), &[jazz.clone(), rock.clone()]).unwrap();
        assert!(other.iter().all(|key| !keys.contains(key)), "{other:?}");

        let reordered = load(directory.path(), &[rock.clone(), jazz.clone()]).unwrap();
        assert_eq!(reordered, [keys[1].clone(), keys[0].clone()]);

        // The same name and URL twice is one library to a shared view.
        let twice = load(directory.path(), &[jazz.clone(), jazz.clone()]).unwrap();
        assert_eq!(twice, [keys[0].clone(), keys[0].clone()]);
    }

    #[test]
    fn a_changed_name_or_url_gets_a_new_key_and_old_ones_are_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let jazz = library("Jazz", "https://music.example.org");
        let [before] = load(directory.path(), std::slice::from_ref(&jazz))
            .unwrap()
            .try_into()
            .unwrap();
        let moved = library("Jazz", "https://music.example.org/other");
        let renamed = library("Blues", "https://music.example.org");
        let after = load(directory.path(), &[moved, renamed]).unwrap();
        assert!(!after.contains(&before), "{after:?}");
        assert_ne!(after[0], after[1]);
        let text = std::fs::read_to_string(directory.path().join(FILE)).unwrap();
        assert!(!text.contains(&before), "{text}");
        // Back to the first configuration: the dropped key does not return.
        let again = load(directory.path(), &[jazz]).unwrap();
        assert_ne!(again, [before]);
    }

    #[test]
    fn damaged_files_fail_and_invalid_keys_are_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE);
        let jazz = library("Jazz", "https://music.example.org");
        std::fs::write(&path, b"{").unwrap();
        assert!(load(directory.path(), std::slice::from_ref(&jazz)).is_err());
        std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(load(directory.path(), std::slice::from_ref(&jazz)).is_err());

        let saved = serde_json::json!([{
            "library": { "name": "Jazz", "url": "https://music.example.org" },
            "key": "Not a key",
        }]);
        std::fs::write(&path, saved.to_string()).unwrap();
        let keys = load(directory.path(), std::slice::from_ref(&jazz)).unwrap();
        assert!(is_key(&keys[0]), "{keys:?}");
        assert_eq!(load(directory.path(), &[jazz]).unwrap(), keys);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
