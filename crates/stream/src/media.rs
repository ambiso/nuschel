// SPDX-License-Identifier: EUPL-1.2
//! Epoch keys and AV1 frames (docs/screen-share-plan.md §3, "SFrame"). Each encoding of
//! an epoch has its own KID, `epoch << 8 | encoding`, and so its own SFrame key and
//! counter. On the wire a frame is one envelope byte, then the SFrame ciphertext; the
//! byte is authenticated as SFrame metadata:
//!
//! ```text
//! 0x08  key frame    (an AV1 sequence-header OBU header without a size field)
//! 0x28  delta frame
//! ```
//!
//! Browser packetizers parse the payload as OBUs, and receivers without the Dependency
//! Descriptor find key frames by the 0x08 byte (S0b results on #139).

use std::fmt;

use rand_core::TryCryptoRng;

use crate::sframe::{Receiver, Sender};
use crate::wire::Reader;
use crate::{Error, Secret};

/// Encodings (simulcast streams) per epoch.
pub const MAX_ENCODINGS: u8 = 4;
/// How long a receiver keeps the previous epoch after a rekey.
pub const PREVIOUS_EPOCH_MS: u64 = 2_000;
const KEY_FRAME: u8 = 0x08;
const DELTA_FRAME: u8 = 0x28;

/// One epoch's random SFrame base key.
#[derive(Clone, PartialEq, Eq)]
pub struct EpochKey {
    epoch: u32,
    key: Secret,
}

impl EpochKey {
    pub(crate) const LEN: usize = 4 + 32;

    pub(crate) fn random<R: TryCryptoRng + ?Sized>(epoch: u32, rng: &mut R) -> Result<Self, Error> {
        Ok(Self {
            epoch,
            key: Secret::random(rng)?,
        })
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    fn kid(&self, encoding: u8) -> u64 {
        u64::from(self.epoch) << 8 | u64::from(encoding)
    }

    pub(crate) fn encode(&self) -> zeroize::Zeroizing<Vec<u8>> {
        zeroize::Zeroizing::new([&self.epoch.to_be_bytes()[..], self.key.expose()].concat())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes);
        let epoch = reader.u32()?;
        let key = Secret::from_bytes(reader.array()?);
        reader.finish()?;
        Ok(Self { epoch, key })
    }
}

impl fmt::Debug for EpochKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EpochKey")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// Encrypts one encoding's AV1 frames under one epoch.
pub struct FrameEncryptor(Sender);

impl FrameEncryptor {
    pub fn new(key: &EpochKey, encoding: u8) -> Result<Self, Error> {
        if encoding >= MAX_ENCODINGS {
            return Err(Error::LimitReached);
        }
        Ok(Self(Sender::new(key.key.expose(), key.kid(encoding))))
    }

    pub fn encrypt(&mut self, key_frame: bool, frame: &[u8]) -> Result<Vec<u8>, Error> {
        let envelope = if key_frame { KEY_FRAME } else { DELTA_FRAME };
        let mut out = vec![envelope];
        out.extend(self.0.encrypt(&[envelope], frame)?);
        Ok(out)
    }
}

/// Decrypts AV1 frames under the newest epoch and, for [`PREVIOUS_EPOCH_MS`] after a
/// rekey, the one before it.
#[derive(Default)]
pub struct FrameDecryptor {
    keys: Receiver,
    current: Option<u32>,
    previous: Option<(u32, u64)>,
}

impl FrameDecryptor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a newer epoch; `now` starts the previous epoch's grace period.
    pub fn install(&mut self, key: &EpochKey, now: u64) -> Result<(), Error> {
        if self.current.is_some_and(|current| key.epoch <= current) {
            return Err(Error::StaleEpoch);
        }
        if let Some((previous, _)) = self.previous.take() {
            self.remove(previous);
        }
        self.previous = self
            .current
            .map(|current| (current, now.saturating_add(PREVIOUS_EPOCH_MS)));
        self.current = Some(key.epoch);
        for encoding in 0..MAX_ENCODINGS {
            self.keys.insert(key.kid(encoding), key.key.expose())?;
        }
        Ok(())
    }

    /// Returns whether the frame is a key frame, and the frame.
    pub fn decrypt(&mut self, data: &[u8], now: u64) -> Result<(bool, Vec<u8>), Error> {
        if let Some((previous, until)) = self.previous
            && now >= until
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
            self.keys
                .remove(u64::from(epoch) << 8 | u64::from(encoding));
        }
    }
}
