// SPDX-License-Identifier: EUPL-1.2
//! Registered users' passwords as argon2id PHC strings. Hashing is deliberately
//! expensive (19 MiB, two passes, OWASP's minimum), so it runs on the blocking
//! pool behind a small semaphore: a flood of logins can neither stall the
//! control path nor allocate unbounded memory. Passwords are never logged.
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

fn verify_now(password: &str, stored: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    // Refuse anything but argon2id so a tampered database cannot select a weak scheme.
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
