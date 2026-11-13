// SPDX-License-Identifier: EUPL-1.2
//! Screen-share key agreement on stock and compatible servers (docs/screen-share-plan.md
//! §3): the watch link, the keys derived from its secret, the X-Wing HPKE-PSK exchange
//! between the sharer and each viewer, and rekeying. The epoch keys it agrees encrypt
//! media through `nuschl-sframe`, a separate crate, so this one links no AES.
//!
//! The crate is portable protocol and key state only. Callers pass the time (Unix
//! milliseconds) and a cryptographic RNG, and carry the messages over the SFU; the crate
//! does no I/O and keeps no global state.
//!
//! Key authority stays with the designated crypto worker, the one caller cargo-deny
//! allows. It holds the link secret, the exchange state and the identity bindings, and
//! hands out only what a later, reviewed caller needs. That integration is future work
//! and held until its SECURITY.md row is approved:
//! - the SDK driver encrypting media with SFrame (R1), given epoch keys only;
//! - the desktop handoff creating links (D1);
//! - the browser's dedicated stream worker (K2), a WASM realm that is no OS sandbox.
//!
//! Codec and content workers receive no key material.
//!
//! ```text
//! S          = 32 random bytes from the link
//! prk        = HKDF-Extract(salt = "nuschl/stream/v2", S)
//! stream_id  = HKDF-Expand(prk, "nuschl/stream/v2 stream id", 16)   // the SFU room
//! admit_key  = HKDF-Expand(prk, "nuschl/stream/v2 admit", 32)       // given to the SFU
//! psk        = HKDF-Expand(prk, "nuschl/stream/v2 psk", 32)
//! signal_key = HKDF-Expand(prk, "nuschl/stream/v2 signal", 32)
//! psk_id     = "nuschl/stream/v2" || stream_id
//!
//! viewer                                       sharer
//! fresh X-Wing (sk_v, pk_v); the identity layer signs binding_digest
//! KeyRequest{viewer_id, nonce_v, not_after, pk_v, binding} ──►
//!                                              the identity layer verifies; approve
//!                                              rekey: K_{e+1} to every viewer first
//!                    ◄── KeyGrant{viewer_id, nonce_s, enc, Seal(epoch, K)}
//! KeyAck{viewer_id, confirm} ──►               (sk_v is erased once the grant opens)
//!                    ◄── Rekey{viewer_id, counter, ChaCha20-Poly1305(channel_n, epoch, K)}
//!
//! binding_digest = SHA-256("nuschl/stream/v2 binding" || stream_id || viewer_id
//!                          || nonce_v || not_after || pk_v)
//! transcript = SHA-256("nuschl/stream/v2 transcript" || version || suite
//!                      || KeyRequest body || nonce_s)
//! HPKE       = X-Wing / HKDF-SHA256 / ChaCha20-Poly1305, PSK mode (psk, psk_id),
//!              info = "nuschl/stream/v2 grant" || transcript, aad = ""
//! confirm    = Export("nuschl/stream/v2 confirm viewer->sharer", 32)
//! channel_1  = Export("nuschl/stream/v2 channel sharer->viewer", 32)
//! channel_n+1 = HKDF-Expand(channel_n, "nuschl/stream/v2 channel ratchet", 32)
//! ```
//!
//! The grant's AEAD tag authenticates the sharer's side: it verifies only under the
//! HPKE context, which binds psk and the transcript. The exporter is bound to the same
//! context, so the export alone serves as the viewer's confirmation. Each Rekey uses the
//! next channel key, and both sides erase the one before, so a party that later learns a
//! viewer's or the sharer's state reads no earlier epoch.
//!
//! Every key message is one ChaCha20-Poly1305 envelope under `signal_key`, so only link
//! holders can read or forge it, and the SFU sees no viewer ids, keys or identity
//! bindings. Its tag is the cheap check that drops messages from non-holders; the plan's
//! separate `mac_key` is folded into it.
//!
//! ```text
//! struct { uint8 version = 1; uint8 kind; opaque nonce[12]; opaque sealed<..16 KiB>; }
//!   sealed = ChaCha20-Poly1305(signal_key, nonce, aad = version || kind, body)
//!
//! kind 1  KeyRequest       opaque viewer_id[16]; opaque nonce_v[32]; uint64 not_after;
//!                          opaque pk_v[1216]; opaque binding<0..2^16-1>;
//! kind 2  KeyGrant         opaque viewer_id[16]; opaque nonce_s[32]; opaque enc[1120];
//!                          opaque sealed_key[52];
//! kind 3  KeyAck           opaque viewer_id[16]; opaque confirm[32];
//! kind 4  Rekey            opaque viewer_id[16]; uint64 counter; opaque sealed_key[52];
//!                          (nonce = 0u32 || counter, aad = viewer_id || counter)
//! kind 5  NeedCredentials  opaque viewer_id[16]; uint8 count; opaque fingerprint[count][32];
//!
//! struct { uint32 epoch; opaque key[32]; } EpochKey;
//! struct { opaque call_binding<1..2^16-1>;
//!          uint8 count; opaque fingerprint[count][32];
//!          uint8 inline_count; opaque credential<1..2^16-1>[inline_count]; } Binding;
//! ```
//!
//! **Delivery.** [`Sharer::rekey`] returns the messages and the new [`SendKey`]. Send the
//! messages, send them again after [`RESEND_AFTER_MS`] (viewers drop duplicates as
//! replays), and switch the encryptors to the new key [`SWITCH_AFTER_MS`] after the
//! first send. A viewer that missed a Rekey catches up with the next one.
//!
//! **Who can do what.**
//! - A passive party with S and every recording cannot decrypt: sk_v and every epoch
//!   key are fresh random values, independent of S and of every earlier key.
//! - PSK mode authenticates a grant only as "from a holder of S". Any link holder that
//!   gets a KeyGrant to a viewer becomes that viewer's sharer, and every viewer holds the
//!   epoch key and could encrypt frames others accept. Media authenticity rests on the
//!   SFU admitting only the publisher that proves P (plan E1), and on identity-bound
//!   sharers (plan N1).

mod credentials;
mod exchange;
mod link;
mod wire;

#[cfg(test)]
mod tests;

use std::fmt;

use rand_core::TryCryptoRng;
use subtle::ConstantTimeEq as _;
use zeroize::Zeroizing;

pub use credentials::{Binding, CredentialCache, Fingerprint, VerifiableBinding, fingerprint};
pub use exchange::{
    JoinDraft, RESEND_AFTER_MS, ReceiveKey, Rekey, SWITCH_AFTER_MS, SendKey, Sharer, SharerEvent,
    Viewer, ViewerEvent, ViewerId, ViewerState,
};
pub use link::{Link, LinkKeys};

/// The largest key message, including identity credentials sent inline.
pub const MAX_KEY_MESSAGE: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("too long")]
    TooLong,
    #[error("unsupported version")]
    UnsupportedVersion,
    #[error("authentication failed")]
    Authentication,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("expired")]
    Expired,
    #[error("replayed")]
    Replay,
    #[error("stale epoch")]
    StaleEpoch,
    #[error("unknown viewer")]
    UnknownViewer,
    #[error("unexpected message")]
    Unexpected,
    #[error("limit reached")]
    LimitReached,
    #[error("rekey less than a second after the last one")]
    TooSoon,
    #[error("counter exhausted")]
    Exhausted,
    #[error("randomness unavailable")]
    Randomness,
}

/// 32 secret bytes: zeroized on drop, compared in constant time, redacted in `Debug`.
pub struct Secret(Zeroizing<[u8; 32]>);

impl Secret {
    pub fn random<R: TryCryptoRng + ?Sized>(rng: &mut R) -> Result<Self, Error> {
        let mut secret = Self::from_bytes([0; 32]);
        fill(rng, &mut secret.0[..])?;
        Ok(secret)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    fn into_inner(self) -> Zeroizing<[u8; 32]> {
        self.0
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&*other.0).into()
    }
}

impl Eq for Secret {}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

fn fill<R: TryCryptoRng + ?Sized>(rng: &mut R, out: &mut [u8]) -> Result<(), Error> {
    rng.try_fill_bytes(out).map_err(|_| Error::Randomness)
}

fn random<const N: usize, R: TryCryptoRng + ?Sized>(rng: &mut R) -> Result<[u8; N], Error> {
    let mut out = [0; N];
    fill(rng, &mut out)?;
    Ok(out)
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use libcrux_sha2::Digest as _;
    let mut hasher = libcrux_sha2::Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let mut out = [0; 32];
    hasher.finish(&mut out);
    out
}

fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut prk = Zeroizing::new([0; 32]);
    libcrux_hkdf::extract(libcrux_hkdf::Algorithm::Sha256, &mut prk[..], salt, ikm)
        .expect("inputs are far below the HKDF limits");
    prk
}

fn hkdf_expand<const N: usize>(prk: &[u8; 32], info: &[u8]) -> Zeroizing<[u8; N]> {
    let mut okm = Zeroizing::new([0; N]);
    libcrux_hkdf::expand(libcrux_hkdf::Algorithm::Sha256, &mut okm[..], prk, info)
        .expect("outputs are at most 32 bytes");
    okm
}
