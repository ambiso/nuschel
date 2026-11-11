// SPDX-License-Identifier: EUPL-1.2
//! The entry point of the libFuzzer target in `fuzz/` and of the randomized decoder test.
//! Not a stable API.

use std::sync::OnceLock;

use crate::crypto::{Secret32, ed25519};
use crate::keys::{HybridSecret, HybridSignature};
use crate::keystore::Sealed;
use crate::link::ProvisioningBundle;
use crate::{
    AccountStatement, DeviceCertificate, DeviceCredential, DeviceRevocation,
    DeviceSigningCertificate, HybridPublicKey, IdentityExport, IdentityRoot, LinkBundle, LinkCode,
    LinkDeviceKeys, LinkPake, RecoveryBlob, RecoveryKey, SafetyNumber, SafetyQr, SealedToDevice,
};

/// A fixed key to run the verification code on whatever decodes.
fn key() -> &'static IdentityRoot {
    static KEY: OnceLock<IdentityRoot> = OnceLock::new();
    KEY.get_or_init(|| {
        IdentityRoot::from_secret(HybridSecret::from_seeds(
            Secret32::from_slice(&[1; 32]).expect("32 bytes"),
            Secret32::from_slice(&[2; 32]).expect("32 bytes"),
        ))
    })
}

/// Runs composite verification on the trailing signature of a decoded structure.
fn verify_hybrid_tail(data: &[u8]) {
    let (body, signature) = data.split_at(data.len() - HybridSignature::LEN);
    let signature = HybridSignature(
        signature
            .to_vec()
            .into_boxed_slice()
            .try_into()
            .expect("length"),
    );
    assert!(key().public_key().verify("fuzz", body, &signature).is_err());
}

macro_rules! round_trip {
    ($data:expr, $($ty:ty),+ $(,)?) => {$(
        if let Ok(value) = <$ty>::decode($data) {
            assert_eq!(&value.encode()[..], $data, concat!(stringify!($ty), " is not canonical"));
        }
    )+};
}

/// Feeds `data` to every decoder. Whatever decodes must re-encode to exactly `data`,
/// and the signature checks run on it without panicking.
pub fn decode_everything(data: &[u8]) {
    round_trip!(
        data,
        HybridPublicKey,
        DeviceSigningCertificate,
        DeviceCertificate,
        DeviceCredential,
        DeviceRevocation,
        AccountStatement,
        SealedToDevice,
        LinkPake,
        LinkDeviceKeys,
        LinkBundle,
        RecoveryBlob,
        IdentityExport,
        Sealed,
    );
    if let Ok(bundle) = ProvisioningBundle::decode(data) {
        assert_eq!(
            &bundle.encode()[..],
            data,
            "ProvisioningBundle is not canonical"
        );
    }
    if DeviceSigningCertificate::decode(data).is_ok()
        || DeviceCertificate::decode(data).is_ok()
        || DeviceRevocation::decode(data).is_ok()
        || AccountStatement::decode(data).is_ok()
    {
        verify_hybrid_tail(data);
    }
    if DeviceCredential::decode(data).is_ok() {
        let (body, signature) = data.split_at(data.len() - 64);
        let signature: [u8; 64] = signature.try_into().expect("64 bytes");
        let public = key().public_key().ed25519().as_bytes();
        assert!(ed25519::verify(public, body, &signature).is_err());
    }
    if let Ok(qr) = SafetyQr::parse(data) {
        assert_eq!(&qr.as_bytes()[..], data);
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(number) = SafetyNumber::parse(text) {
            assert!(text == number.digits() || text == number.to_string());
        }
        if let Ok(code) = LinkCode::parse(text) {
            assert_eq!(code.as_str(), text.trim());
            assert_eq!(
                LinkCode::parse(code.as_str()).expect("canonical").as_str(),
                code.as_str()
            );
        }
        if let Ok(key) = RecoveryKey::parse(text) {
            let shown = key.display();
            let again = RecoveryKey::parse(&shown).expect("display parses");
            assert_eq!(&again.display()[..], &shown[..]);
        }
    }
}
