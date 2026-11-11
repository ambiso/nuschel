// SPDX-License-Identifier: EUPL-1.2
//! Explicit operator key files. Nothing reads CI secrets or creates production
//! keys unless the caller explicitly invokes the signing tool with chosen paths.
use crate::{Error, signing, trust};
use nuschl_update::auth_format::PublicKey;
use serde::Deserialize;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
use zeroize::Zeroizing;

pub const MAX_SECRET_BYTES: u64 = 1024;
pub const CI_SECRET_NAME: &str = "UPDATE_HYBRID_SIGNING_KEY";

pub struct Secret {
    ed: Zeroizing<[u8; 32]>,
    ml: Zeroizing<[u8; 32]>,
}
// Deliberately no Debug or Serialize implementation for secret material.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedSecret<'a> {
    format: u16,
    algorithm: &'a str,
    ed25519_seed: &'a str,
    mldsa44_seed: &'a str,
}

fn decode_seed(text: &str) -> Result<Zeroizing<[u8; 32]>, Error> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::KeyDocument);
    }
    let mut out = Zeroizing::new([0u8; 32]);
    hex::decode_to_slice(text, &mut out[..]).map_err(|_| Error::KeyDocument)?;
    Ok(out)
}

impl Secret {
    pub fn read(path: &Path) -> Result<Self, Error> {
        let mut data = Zeroizing::new(Vec::new());
        fs::File::open(path)?
            .take(MAX_SECRET_BYTES + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > MAX_SECRET_BYTES {
            return Err(Error::KeyDocument);
        }
        // Borrowed string fields avoid copying seeds into unzeroized serde
        // Strings. Escaped/non-borrowable seed encodings are rejected.
        let parsed: BorrowedSecret<'_> =
            serde_json::from_slice(&data).map_err(|_| Error::KeyDocument)?;
        if parsed.format != 2 || parsed.algorithm != trust::ALGORITHM_NAME {
            return Err(Error::KeyDocument);
        }
        Ok(Self {
            ed: decode_seed(parsed.ed25519_seed)?,
            ml: decode_seed(parsed.mldsa44_seed)?,
        })
    }
    pub fn public_key(&self) -> Result<PublicKey, Error> {
        signing::public_key(&self.ed, &self.ml)
    }
    pub fn sign(&self, bytes: &[u8]) -> Result<nuschl_update::auth_format::Signature, Error> {
        Ok(signing::sign(&self.ed, &self.ml, bytes)?.1)
    }
    fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(256));
        out.extend_from_slice(
            b"{\"format\":2,\"algorithm\":\"ed25519+mldsa44\",\"ed25519_seed\":\"",
        );
        let append_hex = |out: &mut Vec<u8>, seed: &[u8; 32]| {
            for byte in seed {
                out.push(b"0123456789abcdef"[(byte >> 4) as usize]);
                out.push(b"0123456789abcdef"[(byte & 15) as usize]);
            }
        };
        append_hex(&mut out, &self.ed);
        out.extend_from_slice(b"\",\"mldsa44_seed\":\"");
        append_hex(&mut out, &self.ml);
        out.extend_from_slice(b"\"}\n");
        out
    }
}

fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<(), Error> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Writes only new paths, never overwrites a key. If writing the public file
/// fails after the secret was saved, the valid secret is retained for recovery
/// with export_public; it is never silently regenerated or removed.
pub fn generate(secret_path: &Path, public_path: &Path) -> Result<PublicKey, Error> {
    let normalize = |path: &Path| -> Result<std::path::PathBuf, Error> {
        let name = path.file_name().ok_or(Error::KeyDocument)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(parent.canonicalize()?.join(name))
    };
    if normalize(secret_path)? == normalize(public_path)? {
        return Err(Error::KeyDocument);
    }
    if secret_path.try_exists()? || public_path.try_exists()? {
        return Err(Error::ExistingFile);
    }
    let mut secret = Secret {
        ed: Zeroizing::new([0; 32]),
        ml: Zeroizing::new([0; 32]),
    };
    getrandom::fill(&mut secret.ed[..]).map_err(|_| Error::Randomness)?;
    getrandom::fill(&mut secret.ml[..]).map_err(|_| Error::Randomness)?;
    let public = secret.public_key()?;
    write_new(secret_path, &secret.encode(), true)?;
    write_new(public_path, &trust::encode(&public)?, false)?;
    Ok(public)
}
pub fn export_public(secret_path: &Path, public_path: &Path) -> Result<PublicKey, Error> {
    let secret = Secret::read(secret_path)?;
    let public = secret.public_key()?;
    write_new(public_path, &trust::encode(&public)?, false)?;
    Ok(public)
}
pub fn status(secret_path: &Path, public_path: &Path) -> Result<PublicKey, Error> {
    let secret = Secret::read(secret_path)?;
    let derived = secret.public_key()?;
    let mut public = Vec::new();
    fs::File::open(public_path)?
        .take(trust::MAX_TRUST_BYTES as u64 + 1)
        .read_to_end(&mut public)?;
    let enrolled = trust::parse(&public)?;
    if enrolled.iter().any(|key| key == &derived) {
        Ok(derived)
    } else {
        Err(Error::KeyDocument)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_secret_roundtrips_matches_public_and_never_overwrites_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("key.secret.json");
        let public = root.path().join("key.public.json");
        let key = generate(&secret, &public).unwrap();
        assert_eq!(status(&secret, &public).unwrap(), key);
        let original = fs::read(&secret).unwrap();
        assert!(generate(&secret, &public).is_err());
        assert_eq!(fs::read(&secret).unwrap(), original);
        let exported = root.path().join("export.public.json");
        assert_eq!(export_public(&secret, &exported).unwrap(), key);
        assert!(!fs::read_to_string(&public).unwrap().contains("seed"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn malformed_oversized_and_duplicate_secret_documents_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("test.secret");
        for bytes in [b"not a key".to_vec(), vec![b'x';MAX_SECRET_BYTES as usize + 1],
            br#"{"format":2,"format":2,"algorithm":"ed25519+mldsa44","ed25519_seed":"00","mldsa44_seed":"00"}"#.to_vec()] {
            fs::write(&file, bytes).unwrap(); assert!(Secret::read(&file).is_err());
        }
    }

    #[test]
    fn a_secret_and_public_output_alias_is_rejected_before_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("same");
        assert!(generate(&path, &root.path().join("./same")).is_err());
        assert!(!path.exists());
    }
}
