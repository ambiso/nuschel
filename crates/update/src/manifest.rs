// SPDX-License-Identifier: EUPL-1.2
//! Strict v2 manifest semantics. Authentication signs the original bytes, never
//! this parsed representation. Nothing here establishes signing-key trust.
use crate::{Error, auth_format::MAX_MANIFEST_BYTES, trust::ALGORITHM_NAME};
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Component, PathBuf},
};

pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_FILES: usize = 256;
pub const MAX_VALIDITY_SECONDS: u64 = 30 * 24 * 60 * 60;
pub const FUTURE_SKEW_SECONDS: u64 = 300;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u16,
    pub algorithm: String,
    pub version: String,
    pub released: u64,
    pub expires: u64,
    #[serde(default)]
    pub notes: Option<String>,
    pub platforms: BTreeMap<String, Vec<FileEntry>>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// serde's derived structs reject duplicate named fields, but BTreeMap normally
/// overwrites duplicate platform keys. Reject duplicates recursively before any
/// typed parsing, including escaped spellings of the same object member.
struct Unique;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Check;
        impl<'de> Visitor<'de> for Check {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
                let mut keys = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                    map.next_value::<Unique>()?;
                }
                Ok(Unique)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
                while seq.next_element::<Unique>()?.is_some() {}
                Ok(Unique)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_none<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique)
            }
        }
        deserializer.deserialize_any(Check)
    }
}
pub fn parse(bytes: &[u8], now: u64) -> Result<Manifest, Error> {
    if bytes.is_empty() || bytes.len() > MAX_MANIFEST_BYTES {
        return Err(invalid("manifest size"));
    }
    serde_json::from_slice::<Unique>(bytes).map_err(|_| invalid("duplicate or invalid JSON"))?;
    let manifest: Manifest = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
    if manifest.format != 2 || manifest.algorithm != ALGORITHM_NAME {
        return Err(invalid("only hybrid v2 manifests are accepted"));
    }
    semver::Version::parse(&manifest.version).map_err(|_| invalid("version"))?;
    if manifest.version.len() > 100
        || manifest
            .notes
            .as_ref()
            .is_some_and(|notes| notes.len() > 8192)
    {
        return Err(invalid("oversized version or notes"));
    }
    if manifest.expires <= manifest.released
        || manifest.expires - manifest.released > MAX_VALIDITY_SECONDS
        || manifest.released > now.saturating_add(FUTURE_SKEW_SECONDS)
    {
        return Err(invalid(
            "invalid release/expiry interval or clock in the future",
        ));
    }
    if manifest.expires <= now {
        return Err(Error::Expired);
    }
    if manifest.platforms.is_empty() || manifest.platforms.len() > 2 {
        return Err(invalid("platform count"));
    }
    for (platform, files) in &manifest.platforms {
        if !["windows-x86_64", "macos-universal"].contains(&platform.as_str())
            || files.is_empty()
            || files.len() > MAX_FILES
        {
            return Err(invalid("unsupported platform or file count"));
        }
        let mut seen = BTreeSet::new();
        for file in files {
            safe_relative(&file.path)?;
            if !seen.insert(file.path.to_ascii_lowercase()) {
                return Err(invalid("duplicate installation path"));
            }
            if file.size > MAX_FILE_BYTES
                || file.sha256.len() != 64
                || !file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(invalid("file size or lowercase SHA-256"));
            }
            validate_url(&file.url)?;
        }
    }
    Ok(manifest)
}
pub fn validate_url(source: &str) -> Result<(), Error> {
    if source.len() > 2048 || source.chars().any(char::is_control) {
        return Err(invalid("URL"));
    }
    let url = url::Url::parse(source).map_err(|_| invalid("URL"))?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
    {
        return Err(invalid(
            "HTTPS URL required (plain HTTP only on loopback fixtures)",
        ));
    }
    Ok(())
}
pub(crate) fn safe_relative(path: &str) -> Result<PathBuf, Error> {
    if path.is_empty()
        || path.len() > 240
        || path.contains(['\\', ':'])
        || !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/ ".contains(c))
    {
        return Err(invalid("unsafe installation path"));
    }
    for part in path.split('/') {
        let base = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let device = ["CON", "PRN", "AUX", "NUL"].contains(&base.as_str())
            || (base.len() == 4
                && (base.starts_with("COM") || base.starts_with("LPT"))
                && matches!(base.as_bytes()[3], b'1'..=b'9'));
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part.eq_ignore_ascii_case(".nuschl-update")
            || part.eq_ignore_ascii_case(".nuschl-old")
            || device
        {
            return Err(invalid("unsafe or aliased installation path"));
        }
    }
    let relative = PathBuf::from(path);
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(invalid("unsafe installation path"));
    }
    Ok(relative)
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Manifest(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: u64 = 1_800_000_000;
    fn valid() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"format":2,"algorithm":ALGORITHM_NAME,"version":"0.1.0","released":NOW,"expires":NOW+86400,"platforms":{"windows-x86_64":[{"path":"nuschl.exe","url":"https://example.invalid/a","sha256":"00".repeat(32),"size":1}]}})).unwrap()
    }
    #[test]
    fn exact_v2_expiry_and_duplicate_maps_are_required() {
        assert!(parse(&valid(), NOW).is_ok());
        assert!(matches!(parse(&valid(), NOW + 86400), Err(Error::Expired)));
        let mut value: serde_json::Value = serde_json::from_slice(&valid()).unwrap();
        for field in ["format", "algorithm", "expires"] {
            let old = value.as_object_mut().unwrap().remove(field).unwrap();
            assert!(parse(&serde_json::to_vec(&value).unwrap(), NOW).is_err());
            value[field] = old;
        }
        let source = String::from_utf8(valid()).unwrap();
        let duplicate = source.replacen(
            "\"platforms\":{",
            "\"platforms\":{\"windows-x86_64\":[],",
            1,
        );
        assert!(parse(duplicate.as_bytes(), NOW).is_err());
        assert!(parse(br#"{"format":2,"f\u006frmat":2}"#, NOW).is_err());
        value["released"] = serde_json::json!(NOW + FUTURE_SKEW_SECONDS + 1);
        assert!(parse(&serde_json::to_vec(&value).unwrap(), NOW).is_err());
    }
    #[test]
    fn filesystem_aliases_credentials_and_network_downgrades_are_rejected() {
        for path in [
            "../a",
            "a/./b",
            "a//b",
            "CON.exe",
            "a/NUL",
            "name.",
            "name ",
            ".NUSCHL-UPDATE/x",
        ] {
            assert!(safe_relative(path).is_err(), "{path}");
        }
        for url in [
            "http://example.invalid/a",
            "https://user:secret@example.invalid/a",
            "https://example.invalid/a#fragment",
        ] {
            assert!(validate_url(url).is_err());
        }
        assert!(validate_url("http://127.0.0.1:12345/a").is_ok());
        assert!(validate_url("https://example.invalid/a").is_ok());
    }
}
