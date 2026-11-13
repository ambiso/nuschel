// SPDX-License-Identifier: EUPL-1.2
//! The secret links to the web page, one per Mumble user, kept in a JSON file.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{self, Read},
    path::PathBuf,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

const MAX_TOKENS: usize = 4096;
const MAX_TOKEN_BYTES: u64 = 4 * 1024 * 1024;

/// Who received a link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// The Mumble server, `host:port`.
    pub server: String,
    pub name: String,
    /// The SHA-1 hash of the user's certificate, as Mumble shows it.
    pub certificate: Option<String>,
}

pub struct Tokens {
    path: PathBuf,
    tokens: Mutex<HashMap<Uuid, Owner>>,
    // After a storage error, confirm the rolled-back map before returning links.
    needs_sync: AtomicBool,
}

impl Tokens {
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let tokens: HashMap<Uuid, Owner> = match crate::persist::open_private(&path) {
            Ok(file) => {
                if file.metadata()?.len() > MAX_TOKEN_BYTES {
                    return Err(io::Error::other("token file exceeds 4 MiB"));
                }
                let mut bytes = Vec::new();
                file.take(MAX_TOKEN_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_TOKEN_BYTES {
                    return Err(io::Error::other("token file exceeds 4 MiB"));
                }
                serde_json::from_slice(&bytes).map_err(io::Error::other)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error),
        };
        if tokens.len() > MAX_TOKENS {
            return Err(io::Error::other("token file exceeds 4096 links"));
        }
        Ok(Self {
            path,
            tokens: Mutex::new(tokens),
            needs_sync: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, Owner>> {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The user's link. A user with a certificate keeps theirs until `renew`
    /// replaces it; without a certificate anybody could take the name later,
    /// so every request gets a new link.
    pub fn issue(&self, mut owner: Owner, renew: bool) -> io::Result<Uuid> {
        // Mumble's optional hash field may be present but empty. It is not
        // a stable identity that different anonymous people may share.
        owner.certificate = owner.certificate.filter(|hash| !hash.is_empty());
        let mut tokens = self.lock();
        if self.needs_sync.load(Ordering::Relaxed) {
            self.save(&tokens)?;
            self.needs_sync.store(false, Ordering::Relaxed);
        }
        let same = |known: &Owner| {
            owner.certificate.is_some()
                && known.server == owner.server
                && known.certificate == owner.certificate
        };
        if !renew && let Some((token, _)) = tokens.iter().find(|(_, known)| same(known)) {
            return Ok(*token);
        }
        let replaced: Vec<_> = tokens
            .iter()
            .filter_map(|(token, known)| same(known).then_some(*token))
            .collect();
        if tokens.len() - replaced.len() >= MAX_TOKENS {
            return Err(io::Error::other("token capacity reached"));
        }
        let removed: Vec<_> = replaced
            .into_iter()
            .map(|token| (token, tokens.remove(&token).expect("locked token exists")))
            .collect();
        let token = loop {
            let candidate = Uuid::new_v4();
            if !tokens.contains_key(&candidate) && !removed.iter().any(|(old, _)| *old == candidate)
            {
                break candidate;
            }
        };
        tokens.insert(token, owner);
        if let Err(error) = self.save(&tokens) {
            tokens.remove(&token);
            tokens.extend(removed);
            self.needs_sync.store(true, Ordering::Relaxed);
            return Err(error);
        }
        Ok(token)
    }

    pub(crate) fn is_valid(&self, token: &Uuid) -> bool {
        self.lock().contains_key(token)
    }

    pub fn owner(&self, token: &Uuid) -> Option<Owner> {
        self.lock().get(token).cloned()
    }

    fn save(&self, tokens: &HashMap<Uuid, Owner>) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(tokens).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_TOKEN_BYTES {
            return Err(io::Error::other("token file exceeds 4 MiB"));
        }
        crate::persist::write(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(certificate: Option<&str>) -> Owner {
        Owner {
            server: "voice.example.org:64738".into(),
            name: "ada".into(),
            certificate: certificate.map(str::to_owned),
        }
    }

    #[test]
    fn links_survive_a_restart_and_renewal_revokes_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        let tokens = Tokens::load(path.clone()).unwrap();
        let first = tokens.issue(owner(Some("aa")), false).unwrap();
        assert_eq!(tokens.issue(owner(Some("aa")), false).unwrap(), first);
        let tokens = Tokens::load(path.clone()).unwrap();
        assert_eq!(tokens.owner(&first), Some(owner(Some("aa"))));
        let renewed = tokens.issue(owner(Some("aa")), true).unwrap();
        assert_ne!(renewed, first);
        assert_eq!(tokens.owner(&first), None);
        let restarted = Tokens::load(path).unwrap();
        assert_eq!(restarted.owner(&first), None);
        assert_eq!(restarted.owner(&renewed), Some(owner(Some("aa"))));
    }

    #[test]
    fn users_without_certificate_get_a_new_link_each_time() {
        let directory = tempfile::tempdir().unwrap();
        let tokens = Tokens::load(directory.path().join("tokens.json")).unwrap();
        let first = tokens.issue(owner(None), false).unwrap();
        let second = tokens.issue(owner(None), false).unwrap();
        assert_ne!(first, second);
        assert!(tokens.owner(&first).is_some() && tokens.owner(&second).is_some());
    }

    #[test]
    fn an_empty_certificate_is_anonymous_not_a_shared_identity() {
        let directory = tempfile::tempdir().unwrap();
        let tokens = Tokens::load(directory.path().join("tokens.json")).unwrap();
        let first = tokens.issue(owner(Some("")), false).unwrap();
        let mut another = owner(Some(""));
        another.name = "another person".into();
        let second = tokens.issue(another, false).unwrap();
        assert_ne!(first, second);
        assert_eq!(tokens.owner(&first).unwrap().certificate, None);
        assert_eq!(tokens.owner(&second).unwrap().certificate, None);
    }

    #[test]
    fn failed_issue_and_renewal_do_not_publish_or_revoke_links() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        let tokens = Tokens::load(path.clone()).unwrap();
        let first = tokens.issue(owner(Some("aa")), false).unwrap();
        let temporary = path.with_extension("json.new");
        std::fs::create_dir(&temporary).unwrap();
        assert!(tokens.issue(owner(Some("bb")), false).is_err());
        assert!(tokens.issue(owner(Some("aa")), true).is_err());
        assert!(tokens.issue(owner(Some("aa")), false).is_err());
        assert_eq!(tokens.lock().len(), 1);
        std::fs::remove_dir(&temporary).unwrap();
        assert_eq!(tokens.issue(owner(Some("aa")), false).unwrap(), first);
        assert_eq!(
            Tokens::load(path.clone()).unwrap().owner(&first),
            Some(owner(Some("aa")))
        );
        let second = tokens.issue(owner(Some("bb")), false).unwrap();
        let restarted = Tokens::load(path.clone()).unwrap();
        assert_eq!(restarted.owner(&first), Some(owner(Some("aa"))));
        assert_eq!(restarted.owner(&second), Some(owner(Some("bb"))));
        let renewed = tokens.issue(owner(Some("aa")), true).unwrap();
        let restarted = Tokens::load(path).unwrap();
        assert_eq!(restarted.owner(&first), None);
        assert_eq!(restarted.owner(&renewed), Some(owner(Some("aa"))));
        assert_eq!(restarted.owner(&second), Some(owner(Some("bb"))));
    }

    #[test]
    fn capacity_preserves_other_owners_and_allows_authenticated_renewal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        let mut saved = HashMap::new();
        let first = Uuid::from_u128(1);
        saved.insert(first, owner(Some("aa")));
        for id in 2..=MAX_TOKENS as u128 {
            saved.insert(Uuid::from_u128(id), owner(None));
        }
        let bytes = serde_json::to_vec(&saved).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let tokens = Tokens::load(path.clone()).unwrap();
        assert!(tokens.issue(owner(None), true).is_err());
        assert!(tokens.issue(owner(Some("bb")), true).is_err());
        assert_eq!(tokens.lock().len(), MAX_TOKENS);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let renewed = tokens.issue(owner(Some("aa")), true).unwrap();
        let restarted = Tokens::load(path).unwrap();
        assert_eq!(restarted.lock().len(), MAX_TOKENS);
        assert_eq!(restarted.owner(&first), None);
        assert_eq!(restarted.owner(&renewed), Some(owner(Some("aa"))));
        assert_eq!(restarted.owner(&Uuid::from_u128(2)), Some(owner(None)));
    }

    #[test]
    fn oversized_and_overcapacity_files_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_TOKEN_BYTES + 1).unwrap();
        assert!(Tokens::load(path.clone()).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_TOKEN_BYTES + 1);
        let saved: HashMap<_, _> = (1..=MAX_TOKENS as u128 + 1)
            .map(|id| (Uuid::from_u128(id), owner(None)))
            .collect();
        let bytes = serde_json::to_vec(&saved).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(Tokens::load(path.clone()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_token_files_and_temporary_files_become_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        let temporary = path.with_extension("json.new");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::write(&temporary, b"old secret").unwrap();
        for file in [&path, &temporary] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let tokens = Tokens::load(path.clone()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        tokens.issue(owner(Some("aa")), false).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!temporary.exists());
    }

    #[cfg(unix)]
    #[test]
    fn newly_written_tokens_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        let tokens = Tokens::load(path.clone()).unwrap();
        let issued = tokens.issue(owner(None), false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            Tokens::load(path).unwrap().owner(&issued),
            Some(owner(None))
        );
    }
}
