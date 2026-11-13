// SPDX-License-Identifier: EUPL-1.2
//! Screen-share media encryption (docs/screen-share-plan.md §3, "SFrame"): RFC 9605
//! SFrame ([`rfc9605`]) and the AV1 frame envelope, over the epoch keys that
//! `nuschl-stream` agrees.
//!
//! Each encoding of an epoch has its own KID, `epoch << 8 | encoding`, and so its own
//! SFrame key and counter. On the wire a frame is one envelope byte, then the SFrame
//! ciphertext; the byte is authenticated as SFrame metadata:
//!
//! ```text
//! 0x08  key frame    (an AV1 sequence-header OBU header without a size field)
//! 0x28  delta frame
//! ```
//!
//! Browser packetizers parse the payload as OBUs, and receivers without the Dependency
//! Descriptor find key frames by the 0x08 byte (S0b results on #139).
//!
//! AES-GCM nonces stay unique while each (epoch, encoding) has exactly one encryptor.
//! [`SendKey::encryptors`] consumes the epoch's key and returns one encryptor per
//! encoding. Restarting a sender (an encoder restart, a new track, a new worker process)
//! requires a rekey: counters start at zero again only under a new epoch.
//!
//! This crate is the AES code of screen sharing, kept apart from the key agreement so
//! the crypto worker links no AES. Its callers, the SDK driver and the browser stream
//! worker, are held until their SECURITY.md rows are reviewed.

pub mod rfc9605;

use std::fmt;

use zeroize::Zeroizing;

use crate::rfc9605::{Receiver, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("too long")]
    TooLong,
    #[error("authentication failed")]
    Authentication,
    #[error("replayed")]
    Replay,
    #[error("unknown key id")]
    UnknownKey,
    #[error("stale epoch")]
    StaleEpoch,
    #[error("limit reached")]
    LimitReached,
    #[error("counter exhausted")]
    Exhausted,
}

/// Encodings (simulcast streams) per epoch.
pub const MAX_ENCODINGS: u8 = 4;
/// How long a receiver keeps the previous epoch after installing a newer one.
pub const PREVIOUS_EPOCH_MS: u64 = 2_000;
const KEY_FRAME: u8 = 0x08;
const DELTA_FRAME: u8 = 0x28;

fn kid(epoch: u32, encoding: u8) -> u64 {
    u64::from(epoch) << 8 | u64::from(encoding)
}

/// One epoch's base key for sending.
pub struct SendKey {
    epoch: u32,
    key: Zeroizing<[u8; 32]>,
}

impl SendKey {
    pub fn new(epoch: u32, key: Zeroizing<[u8; 32]>) -> Self {
        Self { epoch, key }
    }

    /// The epoch's only encryptors, one per encoding, each counting from 0. The key is
    /// consumed:
    ///
    /// ```compile_fail
    /// let key = nuschl_sframe::SendKey::new(2, [7; 32].into());
    /// let first = key.encryptors();
    /// let second = key.encryptors();
    /// ```
    pub fn encryptors(self) -> [FrameEncryptor; MAX_ENCODINGS as usize] {
        std::array::from_fn(|encoding| {
            let encoding = u8::try_from(encoding).expect("fewer than 256 encodings");
            FrameEncryptor(Sender::new(&self.key[..], kid(self.epoch, encoding)))
        })
    }
}

impl fmt::Debug for SendKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendKey")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// Encrypts one encoding's AV1 frames under one epoch.
pub struct FrameEncryptor(Sender);

impl FrameEncryptor {
    pub fn encrypt(&mut self, key_frame: bool, frame: &[u8]) -> Result<Vec<u8>, Error> {
        let envelope = if key_frame { KEY_FRAME } else { DELTA_FRAME };
        let mut out = vec![envelope];
        out.extend(self.0.encrypt(&[envelope], frame)?);
        Ok(out)
    }
}

/// Decrypts AV1 frames under the newest epoch and, for [`PREVIOUS_EPOCH_MS`] after
/// installing it, the one before. A clock that steps back ends that period at once.
#[derive(Default)]
pub struct FrameDecryptor {
    keys: Receiver,
    current: Option<u32>,
    /// The previous epoch and when its successor was installed.
    previous: Option<(u32, u64)>,
}

impl FrameDecryptor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a newer epoch's key.
    pub fn install(&mut self, epoch: u32, key: &[u8; 32], now: u64) -> Result<(), Error> {
        if self.current.is_some_and(|current| epoch <= current) {
            return Err(Error::StaleEpoch);
        }
        if let Some((previous, _)) = self.previous.take() {
            self.remove(previous);
        }
        self.previous = self.current.map(|current| (current, now));
        self.current = Some(epoch);
        for encoding in 0..MAX_ENCODINGS {
            self.keys.insert(kid(epoch, encoding), key)?;
        }
        Ok(())
    }

    /// Returns whether the frame is a key frame, and the frame.
    pub fn decrypt(&mut self, data: &[u8], now: u64) -> Result<(bool, Vec<u8>), Error> {
        if let Some((previous, since)) = self.previous
            && (now < since || now - since >= PREVIOUS_EPOCH_MS)
        {
            self.remove(previous);
            self.previous = None;
        }
        let (&envelope, ciphertext) = data.split_first().ok_or(Error::Malformed("frame"))?;
        if envelope != KEY_FRAME && envelope != DELTA_FRAME {
            return Err(Error::Malformed("frame envelope"));
        }
        let frame = self.keys.decrypt(&[envelope], ciphertext)?;
        Ok((envelope == KEY_FRAME, frame))
    }

    fn remove(&mut self, epoch: u32) {
        for encoding in 0..MAX_ENCODINGS {
            self.keys.remove(kid(epoch, encoding));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn hex(text: &str) -> Vec<u8> {
        assert!(text.len().is_multiple_of(2), "odd hex length");
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    const T0: u64 = 1_800_000_000_000;

    fn encryptors(epoch: u32, key: u8) -> [FrameEncryptor; MAX_ENCODINGS as usize] {
        SendKey::new(epoch, [key; 32].into()).encryptors()
    }

    #[test]
    fn encodings_get_their_own_keys_and_the_envelope_is_authenticated() {
        let [mut first, mut second, ..] = encryptors(2, 7);
        let mut decryptor = FrameDecryptor::new();
        decryptor.install(2, &[7; 32], T0).unwrap();

        let a = first.encrypt(true, b"same frame").unwrap();
        let b = second.encrypt(true, b"same frame").unwrap();
        assert_ne!(a[2..], b[2..], "distinct keys under distinct KIDs");
        assert_eq!(
            decryptor.decrypt(&a, T0),
            Ok((true, b"same frame".to_vec()))
        );
        assert_eq!(
            decryptor.decrypt(&b, T0),
            Ok((true, b"same frame".to_vec()))
        );

        let delta = first.encrypt(false, b"delta").unwrap();
        assert_eq!(delta[0], DELTA_FRAME);
        for (from, to) in [(DELTA_FRAME, KEY_FRAME), (KEY_FRAME, DELTA_FRAME)] {
            let mut flipped = if from == KEY_FRAME {
                first.encrypt(true, b"k").unwrap()
            } else {
                first.encrypt(false, b"d").unwrap()
            };
            flipped[0] = to;
            assert_eq!(decryptor.decrypt(&flipped, T0), Err(Error::Authentication));
        }
        let mut other = delta.clone();
        other[0] = 0x10;
        assert_eq!(
            decryptor.decrypt(&other, T0),
            Err(Error::Malformed("frame envelope"))
        );
        assert_eq!(decryptor.decrypt(&[], T0), Err(Error::Malformed("frame")));
        assert_eq!(
            decryptor.decrypt(&delta, T0),
            Ok((false, b"delta".to_vec()))
        );
    }

    #[test]
    fn previous_epoch_window() {
        let [mut e2, ..] = encryptors(2, 2);
        let [mut e3, ..] = encryptors(3, 3);
        let [mut e4, ..] = encryptors(4, 4);
        let mut decryptor = FrameDecryptor::new();
        decryptor.install(2, &[2; 32], T0).unwrap();
        decryptor.install(3, &[3; 32], T0 + 1000).unwrap();
        assert!(
            decryptor
                .decrypt(&e2.encrypt(false, b"x").unwrap(), T0 + 2999)
                .is_ok()
        );
        assert_eq!(
            decryptor.decrypt(&e2.encrypt(false, b"x").unwrap(), T0 + 3000),
            Err(Error::UnknownKey)
        );
        assert_eq!(
            decryptor.install(2, &[2; 32], T0 + 3000),
            Err(Error::StaleEpoch)
        );
        assert_eq!(
            decryptor.install(3, &[3; 32], T0 + 3000),
            Err(Error::StaleEpoch)
        );

        // Two rekeys within two seconds: only the newest previous epoch is kept.
        let mut decryptor = FrameDecryptor::new();
        decryptor.install(2, &[2; 32], T0).unwrap();
        decryptor.install(3, &[3; 32], T0 + 1000).unwrap();
        decryptor.install(4, &[4; 32], T0 + 2000).unwrap();
        let e2_frame = e2.encrypt(false, b"x").unwrap();
        assert_eq!(
            decryptor.decrypt(&e2_frame, T0 + 2000),
            Err(Error::UnknownKey)
        );
        assert!(
            decryptor
                .decrypt(&e3.encrypt(false, b"x").unwrap(), T0 + 2000)
                .is_ok()
        );
        assert!(
            decryptor
                .decrypt(&e4.encrypt(false, b"x").unwrap(), T0 + 2000)
                .is_ok()
        );

        // A clock that steps back ends the previous epoch's period.
        assert_eq!(
            decryptor.decrypt(&e3.encrypt(false, b"x").unwrap(), T0 - 3_600_000),
            Err(Error::UnknownKey)
        );
        assert!(
            decryptor
                .decrypt(&e4.encrypt(false, b"x").unwrap(), T0 - 3_600_000)
                .is_ok()
        );
    }
}
