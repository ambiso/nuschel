// SPDX-License-Identifier: EUPL-1.2
//! Hybrid-signed v2 updates. The native parent accepts only the AND of two
//! separately confined fixed-algorithm verifiers; legacy signatures are refused.
pub mod auth_format;
pub mod manifest;
pub mod trust;
pub mod verifier_wire;
pub use manifest::{FileEntry, Manifest};
#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
mod supervisor;
#[cfg(feature = "client")]
pub use client::*;
#[cfg(feature = "client")]
pub use supervisor::{VerifierPaths, verify_hybrid};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("update server: {0}")]
    Http(String),
    #[error("no production hybrid update key is enrolled in this build")]
    Unenrolled,
    #[error("required update verifier failed or exceeded its deadline")]
    Verifier,
    #[error("the update manifest is not signed by the release key")]
    Signature,
    #[error("the signed update manifest has expired; check for a current signed release")]
    Expired,
    #[error("invalid update manifest: {0}")]
    Manifest(String),
    #[error("{0} did not match its published size or checksum")]
    Corrupt(String),
    #[error("the installation folder is not writable: {0}")]
    NotWritable(String),
    #[error(
        "the update server offered {0}, older than a release already seen; \
         it may be replaying an old manifest"
    )]
    Stale(String),
    #[error("could not read the update state {0}")]
    State(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
