// SPDX-License-Identifier: EUPL-1.2
//! Tests across modules. Single-module tests live next to their code.

mod formats;
mod keystore;
mod linking;
mod policy;

use crate::{
    DeviceId, DeviceRevocation, DisplayName, NewIdentity, Validity, VerifiedIdentity, WHOLE_DEVICE,
};

pub(crate) const NOW: u64 = 1_800_000_000;

pub(crate) fn validity() -> Validity {
    Validity::new(NOW - 100, NOW + 1000).unwrap()
}

pub(crate) struct Fixture {
    pub id: NewIdentity,
    pub identity: VerifiedIdentity,
    pub revocation: DeviceRevocation,
}

pub(crate) fn fixture() -> Fixture {
    let id = NewIdentity::create(&DisplayName::new("laptop").unwrap(), validity(), NOW).unwrap();
    let identity = id.verified_identity().unwrap();
    let revocation = id
        .device_signing
        .revoke_device(DeviceId::from_bytes([9; 16]), WHOLE_DEVICE, NOW)
        .unwrap();
    Fixture {
        id,
        identity,
        revocation,
    }
}

/// Byte positions to mutate: everything near the structural fields at both ends, and a
/// stride through the bulk key and signature material. `exhaustive` (release-mode
/// `--ignored` runs) takes every byte.
pub(crate) fn positions(len: usize, exhaustive: bool) -> Vec<usize> {
    (0..len)
        .filter(|&i| exhaustive || i < 160 || i % 17 == 0 || i + 160 >= len)
        .collect()
}

/// Flips one bit at each position (a different bit per position) and requires `accept`
/// to fail every time.
pub(crate) fn assert_flips_rejected<T>(
    bytes: &[u8],
    positions: &[usize],
    accept: impl Fn(&[u8]) -> Result<T, crate::Error>,
) {
    for &i in positions {
        let mut mutated = bytes.to_vec();
        mutated[i] ^= 1 << (i % 8);
        assert!(
            accept(&mutated).is_err(),
            "flipping bit {} of byte {i} (of {}) was accepted",
            i % 8,
            bytes.len()
        );
    }
}

/// Every proper prefix fails to decode.
pub(crate) fn assert_truncations_rejected<T>(
    bytes: &[u8],
    decode: impl Fn(&[u8]) -> Result<T, crate::Error>,
) {
    assert_prefixes_rejected(bytes, &(0..bytes.len()).collect::<Vec<_>>(), decode);
}

/// The prefixes of the given lengths fail to decode.
pub(crate) fn assert_prefixes_rejected<T>(
    bytes: &[u8],
    lengths: &[usize],
    decode: impl Fn(&[u8]) -> Result<T, crate::Error>,
) {
    for &len in lengths {
        assert!(len < bytes.len());
        assert!(
            decode(&bytes[..len]).is_err(),
            "prefix of {len} bytes accepted"
        );
    }
}

/// A trailing byte fails to decode.
pub(crate) fn assert_trailing_rejected<T>(
    bytes: &[u8],
    decode: impl Fn(&[u8]) -> Result<T, crate::Error>,
) {
    let mut longer = bytes.to_vec();
    longer.push(0);
    match decode(&longer) {
        Err(crate::Error::TrailingBytes | crate::Error::TooLong) => {}
        Err(other) => panic!("trailing byte gave {other:?}"),
        Ok(_) => panic!("trailing byte accepted"),
    }
}

/// Another kind byte and unknown versions are refused with the right error.
pub(crate) fn assert_header_checked<T>(
    bytes: &[u8],
    decode: impl Fn(&[u8]) -> Result<T, crate::Error>,
) {
    let mut wrong = bytes.to_vec();
    wrong[0] ^= 0x40;
    assert_eq!(
        decode(&wrong).err(),
        Some(crate::Error::WrongKind(wrong[0]))
    );
    for version in [0, 2, 0xff] {
        let mut wrong = bytes.to_vec();
        wrong[1] = version;
        assert_eq!(
            decode(&wrong).err(),
            Some(crate::Error::UnsupportedVersion(version))
        );
    }
}
