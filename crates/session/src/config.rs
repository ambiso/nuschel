// SPDX-License-Identifier: EUPL-1.2
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerBookmark {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub certificate_sha256: Option<String>,
}

impl ServerBookmark {
    pub fn new(name: &str, host: &str, port: u16, username: &str) -> Result<Self, String> {
        let host = host.trim().trim_start_matches('[').trim_end_matches(']');
        if host.is_empty()
            || host.len() > 253
            || host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "/@\\".contains(c))
            || port == 0
        {
            return Err("Enter a hostname or IP address and a port from 1 to 65535.".into());
        }
        if username.trim().is_empty()
            || username.len() > 128
            || username.chars().any(char::is_control)
        {
            return Err("Enter a username of at most 128 bytes without control characters.".into());
        }
        if name.len() > 128 || name.chars().any(char::is_control) {
            return Err(
                "The server name must be at most 128 bytes without control characters.".into(),
            );
        }
        Ok(Self {
            id: Uuid::new_v4().to_string(),
            name: if name.trim().is_empty() {
                host.into()
            } else {
                name.trim().into()
            },
            host: host.to_ascii_lowercase(),
            port,
            username: username.trim().into(),
            certificate_sha256: None,
        })
    }

    /// Certificate-bound history identity. A different certificate cannot inherit
    /// another server's local history just by reusing a display name or endpoint.
    pub fn history_identity(&self, peer_sha256: &str) -> String {
        fingerprint(format!("{}:{}:{}", self.host, self.port, peer_sha256).as_bytes())
    }
}

#[derive(Clone)]
pub struct Profile {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Profile {
    pub fn open(override_dir: Option<PathBuf>) -> io::Result<Self> {
        let profile = if let Some(root) = override_dir {
            Self {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
            }
        } else {
            let paths = directories::ProjectDirs::from("org", "Nuschel", "Nuschel")
                .ok_or_else(|| io::Error::other("could not find the application data directory"))?;
            Self {
                config_dir: paths.config_dir().to_owned(),
                data_dir: paths.data_local_dir().to_owned(),
            }
        };
        for directory in [&profile.config_dir, &profile.data_dir] {
            fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(profile)
    }

    pub fn load_servers(&self) -> io::Result<Vec<ServerBookmark>> {
        let path = self.config_dir.join("servers.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let servers: Vec<ServerBookmark> =
            serde_json::from_slice(&read_bounded(&path, 512 * 1024)?)?;
        if servers.len() > 100 {
            return Err(io::Error::other("too many saved servers"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for server in &servers {
            ServerBookmark::new(&server.name, &server.host, server.port, &server.username)
                .map_err(io::Error::other)?;
            if Uuid::parse_str(&server.id).is_err()
                || !ids.insert(&server.id)
                || server.certificate_sha256.as_ref().is_some_and(|pin| {
                    pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit())
                })
            {
                return Err(io::Error::other(
                    "invalid saved server identity or certificate fingerprint",
                ));
            }
        }
        Ok(servers)
    }

    pub fn save_servers(&self, servers: &[ServerBookmark]) -> io::Result<()> {
        if servers.len() > 100 {
            return Err(io::Error::other("at most 100 servers may be saved"));
        }
        atomic_private_write(
            &self.config_dir.join("servers.json"),
            &serde_json::to_vec_pretty(servers)?,
        )
    }

    pub fn history_path(&self) -> PathBuf {
        self.data_dir.join("history.sqlite3")
    }

    pub fn identity(&self) -> Result<ClientIdentity, Box<dyn std::error::Error + Send + Sync>> {
        let path = self.config_dir.join("identity.json");
        if path.exists() {
            return Ok(serde_json::from_slice(&read_bounded(&path, 64 * 1024)?)?);
        }
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["nuschel-client.invalid".into()])?;
        let identity = ClientIdentity {
            certificate: cert.der().to_vec(),
            private_key: signing_key.serialize_der(),
        };
        atomic_private_write(&path, &serde_json::to_vec(&identity)?)?;
        Ok(identity)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
}

pub fn fingerprint(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("configuration exceeds its size limit"));
    }
    Ok(bytes)
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_endpoints_and_binds_history_to_certificate() {
        assert!(ServerBookmark::new("", "mumble://example.org", 64738, "me").is_err());
        assert!(ServerBookmark::new("", "localhost", 0, "me").is_err());
        assert!(ServerBookmark::new("", "localhost", 64738, "").is_err());
        let server = ServerBookmark::new("", "[::1]", 64738, "me").unwrap();
        assert_eq!(server.host, "::1");
        assert_ne!(
            server.history_identity("one"),
            server.history_identity("two")
        );
    }
}
