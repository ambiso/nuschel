// SPDX-License-Identifier: EUPL-1.2
//! The sticky extension policy (docs/extension.md §1, "Downgrade protection").
//! Once a server negotiated `nuschl/1` or a capability, a later connection
//! that offers less fails with an error the user can act on; nothing falls
//! back to the weaker mode, and only an explicit reset (per server, in its
//! settings) clears the record. Records are kept per server address, not per
//! certificate: certificates rotate, and a rotation must not reset them. They
//! live in their own file in the data folder (`Profile::policy`), apart from
//! the bookmarks, so a managed or replaced bookmark cannot drop them, and
//! apart from the configuration, which people keep with their dotfiles and
//! may reset.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
};

pub(crate) const FILE: &str = "nuschl-policy.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_RECORDS: usize = 4096;
const MAX_CAPABILITIES: usize = 32;

/// What a server must offer, because it offered it before (or an invite said so).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    /// The server must select ALPN `nuschl/1` and send a hello.
    #[serde(alias = "nuschel")]
    pub nuschl: bool,
    /// Capabilities the server must keep offering.
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

impl Requirement {
    pub fn is_empty(&self) -> bool {
        !self.nuschl && self.capabilities.is_empty()
    }

    /// Whether a negotiation result meets the requirement; the error is what
    /// the user is shown.
    pub fn check(&self, nuschl: bool, capabilities: &[String]) -> Result<(), String> {
        if self.nuschl && !nuschl {
            return Err(
                "This server used the Nuschl protocol before but now answers as a plain \
                 Mumble server. That can mean someone is impersonating it. Nuschl did not log \
                 in. If the operator changed the server, reset this server's security record."
                    .into(),
            );
        }
        let missing: Vec<&str> = self
            .capabilities
            .iter()
            .filter(|required| !capabilities.contains(required))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "This server no longer offers {} although it did before. That can mean \
                 someone is impersonating it. Nuschl did not log in. If the operator changed \
                 the server, reset this server's security record.",
                missing.join(", ")
            ));
        }
        Ok(())
    }

    /// Raise the requirement to what was negotiated; it never goes down.
    /// Returns whether it changed.
    pub fn raise(&mut self, nuschl: bool, capabilities: &[String]) -> bool {
        let before = self.clone();
        self.nuschl |= nuschl;
        for capability in capabilities {
            if self.capabilities.len() < MAX_CAPABILITIES {
                self.capabilities.insert(capability.clone());
            }
        }
        *self != before
    }
}

/// The records, keyed by `host:port`.
#[derive(Clone, Debug, Default)]
pub struct PolicyStore {
    path: PathBuf,
    records: BTreeMap<String, Requirement>,
}

pub fn key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

impl PolicyStore {
    /// Load the records in `directory` (`Profile::policy` knows which). A
    /// missing file is empty; an unreadable one is an error (silently
    /// starting empty would drop every record).
    pub fn load(directory: &Path) -> io::Result<Self> {
        let path = directory.join(FILE);
        let records = if path.exists() {
            let records: BTreeMap<String, Requirement> =
                serde_json::from_slice(&crate::config::read_bounded(&path, MAX_FILE_BYTES)?)?;
            if records.len() > MAX_RECORDS
                || records.values().any(|record| {
                    record.capabilities.len() > MAX_CAPABILITIES
                        || record.capabilities.iter().any(|c| c.len() > 64)
                })
            {
                return Err(io::Error::other(
                    "the Nuschl policy file exceeds its limits",
                ));
            }
            records
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, records })
    }

    pub fn get(&self, host: &str, port: u16) -> Requirement {
        self.records
            .get(&key(host, port))
            .cloned()
            .unwrap_or_default()
    }

    /// Raise the record for a server after a negotiation (or an invite) and
    /// save it if it changed.
    pub fn raise(
        &mut self,
        host: &str,
        port: u16,
        nuschl: bool,
        capabilities: &[String],
    ) -> io::Result<bool> {
        let key = key(host, port);
        if !self.records.contains_key(&key) && self.records.len() >= MAX_RECORDS {
            return Err(io::Error::other("too many Nuschl policy records"));
        }
        let changed = self
            .records
            .entry(key)
            .or_default()
            .raise(nuschl, capabilities);
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    /// The user's explicit reset for one server.
    pub fn reset(&mut self, host: &str, port: u16) -> io::Result<()> {
        if self.records.remove(&key(host, port)).is_some() {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> io::Result<()> {
        crate::config::atomic_private_write(&self.path, &serde_json::to_vec_pretty(&self.records)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn requirements_only_grow_and_weaker_answers_fail() {
        let mut requirement = Requirement::default();
        assert!(
            requirement.check(false, &[]).is_ok(),
            "no record: stock is fine"
        );
        assert!(requirement.raise(true, &caps(&["history/1"])));
        assert!(!requirement.raise(false, &[]), "never lowered");
        assert!(
            requirement
                .check(true, &caps(&["history/1", "typing/1"]))
                .is_ok()
        );
        let stock = requirement.check(false, &[]).unwrap_err();
        assert!(stock.contains("plain Mumble"), "{stock}");
        let missing = requirement.check(true, &caps(&["typing/1"])).unwrap_err();
        assert!(missing.contains("history/1"), "{missing}");
    }

    #[test]
    fn records_persist_per_address_until_reset() {
        let directory = tempdir();
        let mut store = PolicyStore::load(&directory).unwrap();
        assert!(store.get("Example.org", 64738).is_empty());
        assert!(
            store
                .raise("Example.org", 64738, true, &caps(&["history/1"]))
                .unwrap()
        );
        let reloaded = PolicyStore::load(&directory).unwrap();
        assert!(reloaded.get("example.org", 64738).nuschl);
        assert!(reloaded.get("example.org", 1).is_empty(), "per address");
        let mut reloaded = reloaded;
        reloaded.reset("example.org", 64738).unwrap();
        assert!(
            PolicyStore::load(&directory)
                .unwrap()
                .get("example.org", 64738)
                .is_empty()
        );
        std::fs::write(directory.join(FILE), b"{not json").unwrap();
        assert!(
            PolicyStore::load(&directory).is_err(),
            "never silently empty"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("nuschl-sticky-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
