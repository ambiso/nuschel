// SPDX-License-Identifier: EUPL-1.2
//! Registered users' passwords as argon2id PHC strings. Hashing is deliberately
//! expensive (19 MiB, two passes, OWASP's minimum), so it runs on the blocking
//! pool behind a small semaphore: a flood of logins can neither stall the
//! control path nor allocate unbounded memory. Passwords are never logged.
//!
//! Accounts imported from Murmur keep Murmur's hash until their next
//! successful login, which replaces it with argon2id ([`is_imported`]):
//! PBKDF2-HMAC-SHA384 with a hex salt and 48-byte output (`PBKDF2.cpp`), or the
//! legacy unsalted SHA-1 of the UTF-8 password (`ServerDB::getLegacySHA1Hash`).
//! Both are compared in constant time.
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;

/// Compare two secrets (or digests) without an early exit.
pub fn equal_constant_time(a: &[u8], b: &[u8]) -> bool {
    let difference = a
        .iter()
        .zip(b)
        .fold(u8::from(a.len() != b.len()), |difference, (x, y)| {
            difference | (x ^ y)
        });
    std::hint::black_box(difference) == 0
}

/// Longest password considered; longer input is rejected without hashing.
pub const MAX_PASSWORD_BYTES: usize = 1024;

static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
/// Hashes running or waiting; beyond this, logins are refused for now.
static QUEUED: AtomicUsize = AtomicUsize::new(0);
const MAX_QUEUED: usize = 32;

/// Counts a hash as queued for as long as it lives.
struct Ticket;

impl Ticket {
    fn take() -> Option<Self> {
        if QUEUED.fetch_add(1, Ordering::Relaxed) >= MAX_QUEUED {
            QUEUED.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Self)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        QUEUED.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cost {
    pub memory_kib: u32,
    pub passes: u32,
}

impl Default for Cost {
    fn default() -> Self {
        Self {
            memory_kib: 19 * 1024,
            passes: 2,
        }
    }
}

fn hasher(cost: Cost) -> Option<Argon2<'static>> {
    let params = Params::new(cost.memory_kib, cost.passes, 1, None).ok()?;
    Some(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash synchronously (setup commands); `None` if the password is too long or
/// the system random source failed.
pub fn hash_now(password: &str, cost: Cost) -> Option<String> {
    if password.len() > MAX_PASSWORD_BYTES {
        return None;
    }
    let mut salt = [0_u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut salt).ok()?;
    let salt = SaltString::encode_b64(&salt).ok()?;
    hasher(cost)?
        .hash_password(password.as_bytes(), &salt)
        .ok()
        .map(|hash| hash.to_string())
}

/// Murmur's PBKDF2 hashes: `$murmur-pbkdf2-sha384$i=<iterations>$<salt hex>$<hash hex>`.
pub const MURMUR_PBKDF2: &str = "$murmur-pbkdf2-sha384$";
/// Murmur's legacy hashes: `$murmur-sha1$<hash hex>`.
pub const MURMUR_SHA1: &str = "$murmur-sha1$";
/// Iterations accepted from Murmur (its benchmark aims at 10 ms, at least
/// 1000); more would let one stored row cost seconds per login attempt.
pub const MAX_MURMUR_ITERATIONS: u32 = 2_000_000;
const MURMUR_SALT_BYTES: usize = 64;

/// A stored hash for Murmur's PBKDF2 columns (`pw`, `salt`, `kdfiterations`),
/// or `None` if they are malformed or too expensive.
pub fn murmur_pbkdf2(iterations: i64, salt: &str, hash: &str) -> Option<String> {
    let iterations = u32::try_from(iterations).ok()?;
    let salt_bytes = hex::decode(salt.trim()).ok()?;
    let hash_bytes = hex::decode(hash.trim()).ok()?;
    if !(1..=MAX_MURMUR_ITERATIONS).contains(&iterations)
        || salt_bytes.is_empty()
        || salt_bytes.len() > MURMUR_SALT_BYTES
        || hash_bytes.len() != 48
    {
        return None;
    }
    Some(format!(
        "{MURMUR_PBKDF2}i={iterations}${}${}",
        hex::encode(&salt_bytes),
        hex::encode(&hash_bytes)
    ))
}

/// A stored hash for Murmur's legacy SHA-1 `pw` column.
pub fn murmur_sha1(hash: &str) -> Option<String> {
    let bytes = hex::decode(hash.trim()).ok()?;
    (bytes.len() == 20).then(|| format!("{MURMUR_SHA1}{}", hex::encode(&bytes)))
}

/// A hash imported from Murmur, replaced with argon2id after the next login.
pub fn is_imported(stored: &str) -> bool {
    stored.starts_with(MURMUR_PBKDF2) || stored.starts_with(MURMUR_SHA1)
}

fn verify_murmur(password: &str, stored: &str) -> Option<bool> {
    if let Some(rest) = stored.strip_prefix(MURMUR_PBKDF2) {
        let mut parts = rest.split('$');
        let iterations = parts.next()?.strip_prefix("i=")?.parse::<u32>().ok()?;
        let (salt, hash) = (
            hex::decode(parts.next()?).ok()?,
            hex::decode(parts.next()?).ok()?,
        );
        if parts.next().is_some() || iterations > MAX_MURMUR_ITERATIONS {
            return Some(false);
        }
        let iterations = std::num::NonZeroU32::new(iterations)?;
        // `verify` compares in constant time.
        return Some(
            ring::pbkdf2::verify(
                ring::pbkdf2::PBKDF2_HMAC_SHA384,
                iterations,
                &salt,
                password.as_bytes(),
                &hash,
            )
            .is_ok(),
        );
    }
    let expected = hex::decode(stored.strip_prefix(MURMUR_SHA1)?).ok()?;
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, password.as_bytes());
    Some(equal_constant_time(digest.as_ref(), &expected))
}

fn verify_now(password: &str, stored: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    // Imported Murmur hashes are accepted until the first login replaces them.
    // A database writer could select such a scheme, but could as well store an
    // argon2id hash of a password of their choice.
    if is_imported(stored) {
        return verify_murmur(password, stored).unwrap_or(false);
    }
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    // Refuse anything but argon2id (and the imported formats above).
    if parsed.algorithm != argon2::ARGON2ID_IDENT {
        return false;
    }
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub async fn hash(password: String, cost: Cost) -> Option<String> {
    let _ticket = Ticket::take()?;
    let _permit = WORKERS.clone().acquire_owned().await.ok()?;
    tokio::task::spawn_blocking(move || hash_now(&password, cost))
        .await
        .ok()
        .flatten()
}

/// `None` when too many hashes are already running or waiting: the caller
/// answers "try again later" instead of queueing without bound.
pub async fn verify(password: String, stored: String) -> Option<bool> {
    let _ticket = Ticket::take()?;
    let _permit = WORKERS.clone().acquire_owned().await.ok()?;
    tokio::task::spawn_blocking(move || verify_now(&password, &stored))
        .await
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: Cost = Cost {
        memory_kib: 64,
        passes: 1,
    };

    #[test]
    fn hashes_verify_and_differ_by_salt() {
        let first = hash_now("correct horse", CHEAP).unwrap();
        let second = hash_now("correct horse", CHEAP).unwrap();
        assert!(first.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{first}");
        assert_ne!(first, second);
        assert!(verify_now("correct horse", &first));
        assert!(!verify_now("correct horsf", &first));
        assert!(!verify_now("correct horse", "not a hash"));
        assert!(!verify_now(&"x".repeat(MAX_PASSWORD_BYTES + 1), &first));
        assert!(hash_now(&"x".repeat(MAX_PASSWORD_BYTES + 1), CHEAP).is_none());
    }

    #[test]
    fn murmur_hashes_verify() {
        // PBKDF2-HMAC-SHA384 as Murmur's `PBKDF2::getHash` computes it (8-byte
        // hex salt, 48-byte hex output). The vector is from Python's
        // hashlib.pbkdf2_hmac("sha384", b"hunter2", bytes.fromhex("0011223344556677"), 1000).
        let stored = murmur_pbkdf2(
            1000,
            "0011223344556677",
            "c2fa5023e808bd1adafccf5f995d5147a0d17e0113e9bda3fa8380faa58088467346ba1eb6bbf7e6374da78caa9b784c",
        )
        .unwrap();
        assert!(is_imported(&stored));
        assert!(verify_now("hunter2", &stored));
        assert!(!verify_now("hunter3", &stored));
        let derived = [7_u8; 48];
        // SHA-1 of "password", as `getLegacySHA1Hash` stores it.
        let legacy = murmur_sha1("5baa61e4c9b93f3f0682250b6cf8331b7ee68fd8").unwrap();
        assert!(verify_now("password", &legacy));
        assert!(!verify_now("Password", &legacy));
        // Malformed or too expensive rows are refused at import.
        assert!(murmur_sha1("5baa61e4").is_none());
        assert!(murmur_pbkdf2(0, "00", &hex::encode(derived)).is_none());
        assert!(
            murmur_pbkdf2(
                i64::from(MAX_MURMUR_ITERATIONS) + 1,
                "00",
                &hex::encode(derived)
            )
            .is_none()
        );
        assert!(murmur_pbkdf2(1000, "zz", &hex::encode(derived)).is_none());
        assert!(!verify_now("x", "$murmur-pbkdf2-sha384$i=1$00"));
    }

    #[test]
    fn other_algorithms_are_refused() {
        let argon2i = Argon2::new(
            Algorithm::Argon2i,
            Version::V0x13,
            Params::new(64, 1, 1, None).unwrap(),
        );
        let salt = SaltString::encode_b64(&[1; 16]).unwrap();
        let weak = argon2i.hash_password(b"pw", &salt).unwrap().to_string();
        assert!(!verify_now("pw", &weak));
    }
}
