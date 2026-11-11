// SPDX-License-Identifier: EUPL-1.2
//! Every signed format: exact size, round trip, truncation, trailing bytes, header,
//! field validation, and that any changed bit is rejected by decoding or verification.

use super::*;
use crate::{
    DeviceCertificate, DeviceCredential, DeviceKeys, DeviceSigningCertificate, Error,
    HybridPublicKey, IdentityRoot,
};

fn patch(bytes: &[u8], offset: usize, with: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out[offset..offset + with.len()].copy_from_slice(with);
    out
}

fn device_signing_certificate(exhaustive: bool) {
    let f = fixture();
    let root = f.id.root.public_key();
    let cert = f.id.device_signing.certificate();
    let bytes = cert.encode();
    assert_eq!(bytes.len(), DeviceSigningCertificate::LEN);
    assert_eq!(bytes.len(), 5439);
    let decoded = DeviceSigningCertificate::decode(&bytes).unwrap();
    assert_eq!(&decoded, cert);
    assert_eq!(decoded.encode(), bytes);
    decoded.verify(root).unwrap();
    assert_eq!(decoded.generation(), 1);
    assert_eq!(decoded.identity(), f.id.root.fingerprint());

    assert_prefixes_rejected(
        &bytes,
        &positions(bytes.len(), exhaustive),
        DeviceSigningCertificate::decode,
    );
    assert_trailing_rejected(&bytes, DeviceSigningCertificate::decode);
    assert_header_checked(&bytes, DeviceSigningCertificate::decode);
    assert_flips_rejected(&bytes, &positions(bytes.len(), exhaustive), |b| {
        DeviceSigningCertificate::decode(b)?.verify(root)
    });

    // Generation 0 does not exist.
    assert!(matches!(
        DeviceSigningCertificate::decode(&patch(&bytes, 34, &[0, 0, 0, 0])),
        Err(Error::Malformed(_))
    ));
    // Under another root, it is another identity.
    let stranger = IdentityRoot::generate().unwrap();
    assert_eq!(
        decoded.verify(stranger.public_key()),
        Err(Error::WrongIdentity)
    );
}

#[test]
fn device_signing_certificate_format() {
    device_signing_certificate(false);
}

fn device_certificate(exhaustive: bool) {
    let f = fixture();
    let cert = &f.id.device_certificate;
    let bytes = cert.encode();
    assert_eq!(bytes.len(), DeviceCertificate::MIN_LEN + "laptop".len());
    assert_eq!(DeviceCertificate::MIN_LEN, 4700);
    let decoded = DeviceCertificate::decode(&bytes).unwrap();
    assert_eq!(&decoded, cert);
    assert_eq!(decoded.encode(), bytes);
    decoded.verify(&f.identity).unwrap();
    assert_eq!(decoded.public_keys(), f.id.device.public_keys());
    assert_eq!(decoded.display_name().as_str(), "laptop");

    assert_prefixes_rejected(
        &bytes,
        &positions(bytes.len(), exhaustive),
        DeviceCertificate::decode,
    );
    assert_trailing_rejected(&bytes, DeviceCertificate::decode);
    assert_header_checked(&bytes, DeviceCertificate::decode);
    assert_flips_rejected(&bytes, &positions(bytes.len(), exhaustive), |b| {
        DeviceCertificate::decode(b)?.verify(&f.identity)
    });

    // Field validation, before any signature check.
    let name_at = DeviceCertificate::MIN_LEN - 3373;
    assert_eq!(bytes[name_at - 1], 6, "display name length");
    for (offset, with) in [
        (34, &[0u8, 0, 0, 0][..]),                            // generation 0
        (54, &[0, 0, 0, 0][..]),                              // key epoch 0
        (66, &bytes[58..66]),                                 // not_after == not_before
        (name_at, &b"lap\ntp"[..]),                           // control character in the name
        (name_at, &[0xff, 0xfe, b'a', b'b', b'c', b'd'][..]), // not UTF-8
    ] {
        assert!(
            matches!(
                DeviceCertificate::decode(&patch(&bytes, offset, with)),
                Err(Error::Malformed(_))
            ),
            "patch at {offset}"
        );
    }
    // Unknown leaf and KEM algorithms.
    assert_eq!(
        DeviceCertificate::decode(&patch(&bytes, 74, &[0x09, 0x04])).err(),
        Some(Error::UnsupportedAlgorithm(0x0904))
    );
    assert_eq!(
        DeviceCertificate::decode(&patch(&bytes, 108, &[0x00, 0x20])).err(),
        Some(Error::UnsupportedAlgorithm(0x0020))
    );
    // An invalid X-Wing key (ML-KEM coefficients out of range).
    assert_eq!(
        DeviceCertificate::decode(&patch(&bytes, 110, &[0xff; 3])).err(),
        Some(Error::InvalidPublicKey)
    );
    // A name longer than 64 bytes.
    let mut long = bytes[..name_at - 1].to_vec();
    long.push(65);
    long.extend_from_slice(&[b'a'; 65]);
    long.extend_from_slice(&bytes[name_at + 6..]);
    assert!(DeviceCertificate::decode(&long).is_err());
}

#[test]
fn device_certificate_format() {
    device_certificate(false);
}

#[test]
fn device_credential_format() {
    let f = fixture();
    let credential = &f.id.device_credential;
    let leaf = f.id.device.leaf_key().as_bytes();
    let bytes = credential.encode();
    assert_eq!(bytes.len(), DeviceCredential::LEN);
    assert_eq!(bytes.len(), 172, "docs/e2ee.md: about 170 bytes");
    let decoded = DeviceCredential::decode(&bytes).unwrap();
    assert_eq!(&decoded, credential);
    assert_eq!(decoded.encode(), bytes);
    decoded.verify(&f.identity, leaf, NOW).unwrap();
    assert!(f.id.device_certificate.matches_credential(&decoded));

    assert_truncations_rejected(&bytes, DeviceCredential::decode);
    assert_trailing_rejected(&bytes, DeviceCredential::decode);
    assert_header_checked(&bytes, DeviceCredential::decode);
    // Small enough for every bit of every byte.
    for i in 0..bytes.len() {
        for bit in 0..8 {
            let mut mutated = bytes.clone();
            mutated[i] ^= 1 << bit;
            assert!(
                DeviceCredential::decode(&mutated)
                    .and_then(|c| c.verify(&f.identity, leaf, NOW))
                    .is_err(),
                "bit {bit} of byte {i} accepted"
            );
        }
    }
    assert_eq!(
        decoded.verify(
            &f.identity,
            DeviceKeys::generate().unwrap().leaf_key().as_bytes(),
            NOW
        ),
        Err(Error::KeyMismatch)
    );
    assert_eq!(
        DeviceCredential::decode(&patch(&bytes, 74, &[0xFE, 0x01])).err(),
        Some(Error::UnsupportedAlgorithm(0xFE01)),
        "a hybrid leaf algorithm is not defined for v1 credentials"
    );
    assert!(matches!(
        DeviceCredential::decode(&patch(&bytes, 54, &[0, 0, 0, 0])),
        Err(Error::Malformed(_))
    ));
}

fn device_revocation(exhaustive: bool) {
    let f = fixture();
    let revocation = &f.revocation;
    let bytes = revocation.encode();
    assert_eq!(bytes.len(), crate::DeviceRevocation::LEN);
    assert_eq!(bytes.len(), 3439);
    let decoded = crate::DeviceRevocation::decode(&bytes).unwrap();
    assert_eq!(&decoded, revocation);
    assert_eq!(decoded.encode(), bytes);
    decoded.verify(&f.identity).unwrap();
    assert_eq!(decoded.through_key_epoch(), WHOLE_DEVICE);

    assert_prefixes_rejected(
        &bytes,
        &positions(bytes.len(), exhaustive),
        crate::DeviceRevocation::decode,
    );
    assert_trailing_rejected(&bytes, crate::DeviceRevocation::decode);
    assert_header_checked(&bytes, crate::DeviceRevocation::decode);
    assert_flips_rejected(&bytes, &positions(bytes.len(), exhaustive), |b| {
        crate::DeviceRevocation::decode(b)?.verify(&f.identity)
    });
    assert!(matches!(
        crate::DeviceRevocation::decode(&patch(&bytes, 54, &[0, 0, 0, 0])),
        Err(Error::Malformed(_))
    ));
}

#[test]
fn device_revocation_format() {
    device_revocation(false);
}

#[test]
fn hybrid_public_key_flips() {
    let root = IdentityRoot::generate().unwrap();
    let bytes = root.public_key().encode();
    // Any change is either rejected or a different key with a different fingerprint.
    for i in positions(bytes.len(), false) {
        let mut mutated = bytes.clone();
        mutated[i] ^= 1 << (i % 8);
        if let Ok(key) = HybridPublicKey::decode(&mutated) {
            assert_ne!(key.fingerprint(), root.fingerprint());
        }
    }
}

/// Every byte of every signed format (release mode:
/// `cargo test --release -p nuschel-identity -- --ignored`).
#[test]
#[ignore]
fn exhaustive_flips() {
    device_signing_certificate(true);
    device_certificate(true);
    device_revocation(true);
}
