// SPDX-License-Identifier: EUPL-1.2
//! MLS for Nuschel (docs/e2ee.md §6): OpenMLS 0.9.2 with the libcrux provider,
//! behind Nuschel's own types. This is the only crate that uses the OpenMLS API;
//! the crypto worker uses [`client`], `nuscheld` uses [`ds`].
//!
//! - [`SUITE`]: the one ciphersuite constant. **Provisional**: OpenMLS's code point
//!   0x004F, not the draft's 0x0010 (see its documentation).
//! - [`binding`]: the conversation binding extension and group ids (§6.1).
//! - [`prevalidate`]: structural checks of key packages, trees and message framing
//!   before OpenMLS or libcrux see them (short X-Wing keys made libcrux-kem 0.0.9
//!   panic; it is patched to 0.0.10, and a planted key would still break commits).
//! - [`envelope`]: the application payload with per-sender counters (§6.4).
//! - [`ds`]: the delivery service's view of a group (`PublicGroup`), and key
//!   package checks for the directory (§6.3).
//! - [`client`]: a device's groups: key packages, creating a group, Welcomes,
//!   commits and application messages, with every member's credential handed to
//!   the caller for verification before anything is merged.
//!
//! Every call into OpenMLS on untrusted input runs under [`guard`], which turns a
//! panic into an error. The workspace builds with `panic = "unwind"` (no profile
//! sets `abort`), so this contains panics in the server and the worker alike; the
//! prevalidation is what keeps the known ones from happening at all.
#![forbid(unsafe_code)]

pub mod binding;
pub mod client;
pub mod ds;
pub mod envelope;
pub mod prevalidate;

mod provider;

pub use binding::{BINDING_EXTENSION, ConversationBinding, ConversationKind};

use openmls::prelude::Ciphersuite;

/// **PROVISIONAL CODE POINT.** Groups made with it break at the OpenMLS bump.
///
/// `MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519`: X-Wing (ML-KEM-768 + X25519)
/// HPKE, AES-128-GCM, SHA-256, Ed25519 leaf signatures. This is the suite
/// docs/e2ee.md §1.1 targets, under OpenMLS 0.9.2's private code point 0x004F
/// (feature `draft-ietf-mls-pq-ciphersuites`). draft-ietf-mls-pq-ciphersuites-07
/// assigns it 0x0010 with HPKE KEM 0x647a, and OpenMLS `main` already changes the
/// X-Wing key derivation. Groups and key packages made with this constant will
/// not be readable after the bump to the release that carries the draft's code
/// points (accepted while Nuschel is in early development, TODO.md); the bump
/// changes only this constant (and [`SUITE_ID`]).
pub const SUITE: Ciphersuite = Ciphersuite::MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519;

/// [`SUITE`]'s code point on the wire (checked by [`prevalidate`]).
pub const SUITE_ID: u16 = 0x004F;

/// An X-Wing public key (ML-KEM-768 encapsulation key and X25519 point).
pub const SUITE_KEM_PUBLIC_KEY_LEN: usize = 1216;

/// The MLS credential type carrying a nuschel-identity `DeviceCredential`
/// (private use, docs/e2ee.md §3.3).
pub const CREDENTIAL_TYPE: u16 = nuschel_identity::MLS_CREDENTIAL_TYPE;

/// Devices not seen for this long are removed from groups, and commits are kept at
/// least this long (the catch-up window, docs/e2ee.md §6.3; the user's answer to
/// open question 8).
pub const INACTIVE_DEVICE_DAYS: u64 = 90;

/// A device that has not opened a channel for this long leaves its group
/// (subscriber membership, docs/e2ee.md §6.2; the user's answer to open question 4).
pub const UNOPENED_CHANNEL_DAYS: u64 = 30;

/// What went wrong, as text for logs and the user; never contains secrets.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("malformed MLS data: {0}")]
    Malformed(#[from] prevalidate::Malformed),
    #[error("MLS refused it: {0}")]
    Mls(String),
    #[error("the MLS library failed on this input")]
    Panicked,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("no such group")]
    UnknownGroup,
    #[error("storage: {0}")]
    Storage(String),
}

fn mls(error: impl std::fmt::Debug) -> Error {
    let mut text = format!("{error:?}");
    text.truncate(200);
    Error::Mls(text)
}

/// Runs `f`, turning a panic inside OpenMLS or libcrux into [`Error::Panicked`].
pub fn guard<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(Err(Error::Panicked))
}
