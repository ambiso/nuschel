// SPDX-License-Identifier: EUPL-1.2
//! HPKE (RFC 9180) with X-Wing (KEM 0x647A), HKDF-SHA-256 and ChaCha20-Poly1305: the
//! same configuration and the same libcrux code as the MLS suite. Used for the device
//! linking bundle (PSK mode) and for delivering keys to one device outside MLS
//! ([`SealedToDevice`], base mode).
//!
//! ```text
//! struct {
//!   uint8  kind = 0x05; uint8 version = 1;
//!   opaque recipient_device[16];
//!   uint32 recipient_key_epoch;
//!   opaque enc[1120];                       // X-Wing ciphertext
//!   opaque ciphertext<16..2^24-1>;          // AEAD output with tag
//! } SealedToDevice;
//!
//! info = uint8(26) || "nuschl/sealed-to-device/v1" || recipient_device
//!        || recipient_key_epoch || caller_info
//! ```

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Hpke, HpkePrivateKey, HpkePublicKey, Mode};
use hpke_rs_libcrux::HpkeLibcrux;
use zeroize::Zeroizing;

use crate::Error;
use crate::codec::{Reader, Writer, kind, label};
use crate::crypto::{Secret32, xwing};
use crate::keys::{DeviceId, DeviceKeys, XWingPublicKey};

const SEALED_TO_DEVICE_LABEL: &str = "nuschl/sealed-to-device/v1";

/// The largest plaintext sealed to a device.
pub const MAX_SEALED_PLAINTEXT: usize = (1 << 24) - 1 - 16;

fn hpke(mode: Mode) -> Result<Hpke<HpkeLibcrux>, Error> {
    Hpke::try_new(
        mode,
        KemAlgorithm::XWingDraft06,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
    )
    .map_err(|_| Error::Randomness)
}

/// Seals `plaintext` (empty AAD) to an X-Wing key; with a PSK (`psk`, `psk_id`) in PSK
/// mode, otherwise in base mode.
pub(crate) fn seal(
    recipient: &XWingPublicKey,
    info: &[u8],
    plaintext: &[u8],
    psk: Option<(&[u8; 32], &[u8])>,
) -> Result<(Box<[u8; xwing::CIPHERTEXT_LEN]>, Vec<u8>), Error> {
    let mode = if psk.is_some() { Mode::Psk } else { Mode::Base };
    let (psk, psk_id) = match psk {
        Some((psk, id)) => (Some(&psk[..]), Some(id)),
        None => (None, None),
    };
    let public = HpkePublicKey::new(recipient.as_bytes().to_vec());
    let (enc, ciphertext) = hpke(mode)?
        .seal(&public, info, &[], plaintext, psk, psk_id, None)
        .map_err(|_| Error::Crypto)?;
    let enc = enc
        .into_boxed_slice()
        .try_into()
        .map_err(|_| Error::Crypto)?;
    Ok((enc, ciphertext))
}

pub(crate) fn open(
    kem_seed: &Secret32,
    enc: &[u8; xwing::CIPHERTEXT_LEN],
    info: &[u8],
    ciphertext: &[u8],
    psk: Option<(&[u8; 32], &[u8])>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let mode = if psk.is_some() { Mode::Psk } else { Mode::Base };
    let (psk, psk_id) = match psk {
        Some((psk, id)) => (Some(&psk[..]), Some(id)),
        None => (None, None),
    };
    // hpke-rs zeroizes its private key type on drop.
    let private = HpkePrivateKey::new(kem_seed.expose().to_vec());
    hpke(mode)?
        .open(enc, &private, info, &[], ciphertext, psk, psk_id, None)
        .map(Zeroizing::new)
        .map_err(|_| Error::DecryptionFailed)
}

/// A message sealed to one key epoch of one device, for key delivery outside MLS (for
/// example a new device-sync key, docs/e2ee.md §3.7).
#[derive(Clone, PartialEq, Eq)]
pub struct SealedToDevice {
    recipient_device: DeviceId,
    recipient_key_epoch: u32,
    enc: Box<[u8; xwing::CIPHERTEXT_LEN]>,
    ciphertext: Vec<u8>,
}

impl SealedToDevice {
    pub const MAX_LEN: usize = 2 + 16 + 4 + xwing::CIPHERTEXT_LEN + 3 + (1 << 24) - 1;

    /// Seals to a device's certified public keys. `info` names the purpose; the
    /// recipient must pass the same value to open it.
    pub fn seal(
        recipient: &crate::DevicePublicKeys,
        info: &[u8],
        plaintext: &[u8],
    ) -> Result<Self, Error> {
        if plaintext.len() > MAX_SEALED_PLAINTEXT {
            return Err(Error::TooLong);
        }
        let full_info = Self::info(recipient.device_id, recipient.key_epoch, info);
        let (enc, ciphertext) = seal(&recipient.kem_key, &full_info, plaintext, None)?;
        Ok(Self {
            recipient_device: recipient.device_id,
            recipient_key_epoch: recipient.key_epoch,
            enc,
            ciphertext,
        })
    }

    fn info(device: DeviceId, key_epoch: u32, info: &[u8]) -> Vec<u8> {
        let mut out = label(SEALED_TO_DEVICE_LABEL);
        out.extend_from_slice(device.as_bytes());
        out.extend_from_slice(&key_epoch.to_be_bytes());
        out.extend_from_slice(info);
        out
    }

    pub fn recipient(&self) -> (DeviceId, u32) {
        (self.recipient_device, self.recipient_key_epoch)
    }

    pub(crate) fn open(&self, keys: &DeviceKeys, info: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        if keys.device_id() != self.recipient_device || keys.key_epoch() != self.recipient_key_epoch
        {
            return Err(Error::KeyMismatch);
        }
        let full_info = Self::info(self.recipient_device, self.recipient_key_epoch, info);
        keys.open_hpke(&self.enc, &full_info, &self.ciphertext, None)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.header(kind::SEALED_TO_DEVICE);
        writer.bytes(self.recipient_device.as_bytes());
        writer.u32(self.recipient_key_epoch);
        writer.bytes(&self.enc[..]);
        writer.opaque24(&self.ciphertext);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::MAX_LEN)?;
        reader.header(kind::SEALED_TO_DEVICE)?;
        let recipient_device = DeviceId::from_bytes(reader.array()?);
        let recipient_key_epoch = reader.u32()?;
        if recipient_key_epoch == 0 {
            return Err(Error::Malformed("key epoch 0"));
        }
        let enc = reader.boxed()?;
        let ciphertext = reader.opaque24(16, (1 << 24) - 1)?.to_vec();
        reader.finish()?;
        Ok(Self {
            recipient_device,
            recipient_key_epoch,
            enc,
            ciphertext,
        })
    }
}

impl std::fmt::Debug for SealedToDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedToDevice")
            .field("recipient_device", &self.recipient_device)
            .field("recipient_key_epoch", &self.recipient_key_epoch)
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_to_device_round_trip_and_tampering() {
        let keys = DeviceKeys::generate().unwrap();
        let sealed = SealedToDevice::seal(&keys.public_keys(), b"sync-key", b"secret").unwrap();
        let bytes = sealed.encode();
        let decoded = SealedToDevice::decode(&bytes).unwrap();
        assert_eq!(decoded, sealed);
        assert_eq!(&keys.open(&decoded, b"sync-key").unwrap()[..], b"secret");
        assert_eq!(
            keys.open(&decoded, b"other").err(),
            Some(Error::DecryptionFailed)
        );

        let other = DeviceKeys::generate().unwrap();
        assert_eq!(
            other.open(&decoded, b"sync-key").err(),
            Some(Error::KeyMismatch)
        );
        let rotated = keys.rotate().unwrap();
        assert_eq!(
            rotated.open(&decoded, b"sync-key").err(),
            Some(Error::KeyMismatch)
        );

        // Any change to the X-Wing ciphertext or the AEAD output fails.
        for index in [22, 22 + 500, 22 + 1119, 22 + 1120 + 3, bytes.len() - 1] {
            let mut broken = bytes.clone();
            broken[index] ^= 1;
            let decoded = SealedToDevice::decode(&broken).unwrap();
            assert_eq!(
                keys.open(&decoded, b"sync-key").err(),
                Some(Error::DecryptionFailed)
            );
        }
        // Readdressing to another device id is refused before decryption, and with a
        // forged header the info string no longer matches.
        let mut readdressed = bytes.clone();
        readdressed[2] ^= 1;
        let decoded = SealedToDevice::decode(&readdressed).unwrap();
        assert_eq!(
            keys.open(&decoded, b"sync-key").err(),
            Some(Error::KeyMismatch)
        );

        assert_eq!(
            SealedToDevice::decode(&bytes[..bytes.len() - 1]).err(),
            Some(Error::Truncated)
        );
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            SealedToDevice::decode(&trailing).err(),
            Some(Error::TrailingBytes)
        );
    }
}
