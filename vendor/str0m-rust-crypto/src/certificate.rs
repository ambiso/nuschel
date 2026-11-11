//! Self-signed DTLS certificates using rcgen's external signer and RustCrypto.
//!
//! No rcgen crypto-provider feature is necessary: P-256 key generation and
//! signatures use the same RustCrypto implementation as this DTLS provider.

use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use p256::elliptic_curve::Generate;
use p256::pkcs8::EncodePrivateKey;
use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, PublicKeyData};
use sha2::{Digest, Sha256};
use str0m_proto::crypto::dtls::DtlsCert;

struct CertificateKey {
    key: SigningKey,
    public: p256::EncodedPoint,
}

impl PublicKeyData for CertificateKey {
    fn der_bytes(&self) -> &[u8] {
        // X.509 SubjectPublicKeyInfo carries the uncompressed SEC1 point as its
        // BIT STRING; rcgen writes the surrounding algorithm identifiers/DER.
        self.public.as_bytes()
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for CertificateKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        let signature: Signature = self
            .key
            .try_sign(message)
            .map_err(|_| rcgen::Error::RemoteKeyError)?;
        // X.509 requires an ASN.1 ECDSA signature, not fixed-width r || s.
        Ok(signature.to_der().as_bytes().to_vec())
    }
}

pub(super) fn generate() -> Option<DtlsCert> {
    // Return failure rather than panicking if the system CSPRNG is unavailable.
    let key = SigningKey::try_generate().ok()?;
    let public = key.verifying_key().to_encoded_point(false);
    let signer = CertificateKey { key, public };

    let mut params = CertificateParams::new(Vec::<String>::new()).ok()?;
    let mut name = DistinguishedName::new();
    name.push(DnType::OrganizationName, "DTLS");
    name.push(DnType::CommonName, "DTLS Peer");
    params.distinguished_name = name;
    params.is_ca = IsCa::NoCa;
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = params.not_before + time::Duration::days(365);

    // Every invocation creates a fresh OS-random key. Its public-key digest gives
    // a process-independent unique serial without another RNG/backend. Keep it
    // positive, nonzero and below RFC 5280's 20-octet limit (Firefox needs unique
    // serials across peers, not a per-process counter).
    let digest = Sha256::digest(signer.public.as_bytes());
    let mut serial = [0_u8; 16];
    serial.copy_from_slice(&digest[..16]);
    serial[0] &= 0x7f;
    serial[15] |= 1;
    params.serial_number = Some(serial.to_vec().into());

    let certificate = params.self_signed(&signer).ok()?;
    let private_key = signer.key.to_pkcs8_der().ok()?;
    Some(DtlsCert {
        certificate: certificate.der().to_vec(),
        private_key: private_key.as_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier};
    use p256::pkcs8::{
        DecodePrivateKey,
        der::{Decode, Encode},
    };
    use x509_cert::Certificate;

    #[test]
    fn certificate_is_p256_with_a_valid_self_signature_and_matching_pkcs8_key() {
        let generated = generate().expect("system entropy and certificate generation");
        let certificate = Certificate::from_der(&generated.certificate).expect("X.509 DER");
        assert_eq!(
            certificate.signature_algorithm.oid.to_string(),
            "1.2.840.10045.4.3.2"
        );
        assert_eq!(
            certificate.tbs_certificate.signature,
            certificate.signature_algorithm
        );
        assert_eq!(
            certificate.tbs_certificate.issuer,
            certificate.tbs_certificate.subject
        );
        let subject_key = &certificate.tbs_certificate.subject_public_key_info;
        assert_eq!(subject_key.algorithm.oid.to_string(), "1.2.840.10045.2.1");
        let verifying =
            VerifyingKey::from_sec1_bytes(subject_key.subject_public_key.as_bytes().unwrap())
                .expect("P-256 public key");
        let signing =
            SigningKey::from_pkcs8_der(&generated.private_key).expect("PKCS#8 P-256 private key");
        assert_eq!(signing.verifying_key(), &verifying);

        let signature = Signature::from_der(certificate.signature.as_bytes().unwrap())
            .expect("DER ECDSA signature");
        let mut signed = certificate.tbs_certificate.to_der().unwrap();
        verifying
            .verify(&signed, &signature)
            .expect("valid self-signature");
        *signed.last_mut().unwrap() ^= 1;
        assert!(verifying.verify(&signed, &signature).is_err());
    }

    #[test]
    fn independently_generated_certificates_have_distinct_positive_serials_and_keys() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        let parsed_a = Certificate::from_der(&a.certificate).unwrap();
        let parsed_b = Certificate::from_der(&b.certificate).unwrap();
        let serial_a = parsed_a.tbs_certificate.serial_number.as_bytes();
        let serial_b = parsed_b.tbs_certificate.serial_number.as_bytes();
        assert_ne!(serial_a, serial_b);
        assert!(serial_a.len() <= 20 && serial_a.iter().any(|byte| *byte != 0));
        assert!(serial_b.len() <= 20 && serial_b.iter().any(|byte| *byte != 0));
        assert_ne!(a.private_key, b.private_key);
        assert_ne!(a.certificate, b.certificate);
    }
}
