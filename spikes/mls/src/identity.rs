// SPDX-License-Identifier: EUPL-1.2
//! Persistent, cross-server, multi-device identity (docs/e2ee.md, "Identity").
//!
//! - **Identity root key**: hybrid Ed25519 + ML-DSA-65 (libcrux). Its fingerprint *is*
//!   the user's identity on every server; safety numbers are computed over it. Used
//!   rarely: to certify (and rotate) the device-signing key.
//! - **Device-signing key**: hybrid, certified by the root, held by the user's linked
//!   devices. Certifies device keys, device revocations and account bindings.
//! - **Device keys**: per device. An Ed25519 MLS leaf signature key (ML-DSA once MLS
//!   suites with it are usable) and an X-Wing key for device linking and for
//!   delivering keys to this device outside MLS.
//!
//! The MLS credential of a device carries the chain root -> device-signing key ->
//! device, and is verified against the MLS leaf signature key.
//!
//! ML-DSA-65 is used when the `mldsa` feature is on (the default). A hybrid
//! signature verifies only if every component the *key* has verifies, so stripping
//! the ML-DSA half of a signature does not downgrade it.

use openmls_libcrux_crypto::CryptoProvider;
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::random::OpenMlsRand;
use openmls_traits::types::{
    HashType, HpkeAeadType, HpkeCiphertext, HpkeConfig, HpkeKdfType, HpkeKemType, HpkeKeyPair,
    SignatureScheme,
};
use zeroize::Zeroizing;

use crate::DeviceSigner;

#[cfg(feature = "mldsa")]
use libcrux_ml_dsa::ml_dsa_65;

#[cfg(feature = "mldsa")]
const ML_DSA_CONTEXT: &[u8] = b"nuschl/v1";

/// HPKE with X-Wing (ML-KEM-768 + X25519), HKDF-SHA-256, ChaCha20-Poly1305: the
/// same HPKE configuration as the MLS suite, through the same libcrux code.
pub fn xwing() -> HpkeConfig {
    HpkeConfig(
        HpkeKemType::XWingKemDraft6,
        HpkeKdfType::HkdfSha256,
        HpkeAeadType::ChaCha20Poly1305,
    )
}

fn crypto() -> CryptoProvider {
    CryptoProvider::new().expect("operating system randomness")
}

// --- A minimal length-prefixed encoding (the real one would be TLS presentation
// language or protobuf, with explicit limits). ---

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn get(&mut self) -> Option<&'a [u8]> {
        let len = u32::from_be_bytes(self.0.get(..4)?.try_into().ok()?) as usize;
        let value = self.0.get(4..4 + len)?;
        self.0 = &self.0[4 + len..];
        Some(value)
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

// --- Hybrid signatures ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HybridPublic {
    pub ed25519: Vec<u8>,
    /// Empty when the key has no ML-DSA half (feature off, or a legacy key).
    pub mldsa65: Vec<u8>,
}

impl HybridPublic {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, &self.ed25519);
        put(&mut out, &self.mldsa65);
        out
    }

    fn read(reader: &mut Reader<'_>) -> Option<Self> {
        Some(Self {
            ed25519: reader.get()?.to_vec(),
            mldsa65: reader.get()?.to_vec(),
        })
    }

    /// SHA-256 over the encoding: what pins, safety numbers and KT records name.
    pub fn fingerprint(&self) -> [u8; 32] {
        crypto()
            .hash(HashType::Sha2_256, &self.encode())
            .expect("sha256")
            .try_into()
            .expect("32 bytes")
    }

    pub fn verify(&self, message: &[u8], signature: &HybridSignature) -> Result<(), &'static str> {
        crypto()
            .verify_signature(
                SignatureScheme::ED25519,
                message,
                &self.ed25519,
                &signature.ed25519,
            )
            .map_err(|_| "Ed25519 signature invalid")?;
        if self.mldsa65.is_empty() {
            return Ok(());
        }
        verify_mldsa(&self.mldsa65, message, &signature.mldsa65)
    }
}

#[cfg(feature = "mldsa")]
fn verify_mldsa(key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), &'static str> {
    let key =
        ml_dsa_65::MLDSA65VerificationKey::new(key.try_into().map_err(|_| "ML-DSA key size")?);
    let signature = ml_dsa_65::MLDSA65Signature::new(
        signature
            .try_into()
            .map_err(|_| "ML-DSA signature missing")?,
    );
    ml_dsa_65::verify(&key, message, ML_DSA_CONTEXT, &signature)
        .map_err(|_| "ML-DSA signature invalid")
}

#[cfg(not(feature = "mldsa"))]
fn verify_mldsa(_: &[u8], _: &[u8], _: &[u8]) -> Result<(), &'static str> {
    // A client that cannot verify the ML-DSA half must not accept the key.
    Err("ML-DSA not supported by this build")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HybridSignature {
    pub ed25519: Vec<u8>,
    pub mldsa65: Vec<u8>,
}

/// A hybrid signing key (identity root or device-signing key).
pub struct HybridKey {
    crypto: CryptoProvider,
    ed_private: Zeroizing<Vec<u8>>,
    public: HybridPublic,
    #[cfg(feature = "mldsa")]
    mldsa: ml_dsa_65::MLDSA65KeyPair,
}

impl HybridKey {
    pub fn generate() -> Self {
        let crypto = crypto();
        let (ed_private, ed_public) = crypto
            .signature_key_gen(SignatureScheme::ED25519)
            .expect("ed25519");
        #[cfg(feature = "mldsa")]
        let mldsa = ml_dsa_65::generate_key_pair(crypto.random_array().expect("randomness"));
        #[cfg(feature = "mldsa")]
        let mldsa65 = mldsa.verification_key.as_slice().to_vec();
        #[cfg(not(feature = "mldsa"))]
        let mldsa65 = Vec::new();
        Self {
            ed_private: Zeroizing::new(ed_private),
            public: HybridPublic {
                ed25519: ed_public,
                mldsa65,
            },
            #[cfg(feature = "mldsa")]
            mldsa,
            crypto,
        }
    }

    pub fn public(&self) -> &HybridPublic {
        &self.public
    }

    pub fn sign(&self, message: &[u8]) -> HybridSignature {
        let ed25519 = self
            .crypto
            .sign(SignatureScheme::ED25519, message, &self.ed_private)
            .expect("ed25519");
        #[cfg(feature = "mldsa")]
        let mldsa65 = ml_dsa_65::sign(
            &self.mldsa.signing_key,
            message,
            ML_DSA_CONTEXT,
            self.crypto.random_array().expect("randomness"),
        )
        .expect("ml-dsa")
        .as_slice()
        .to_vec();
        #[cfg(not(feature = "mldsa"))]
        let mldsa65 = Vec::new();
        HybridSignature { ed25519, mldsa65 }
    }
}

// --- Certificates ---

/// What a certificate states. Each kind has its own domain-separation label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// Root -> device-signing key. `generation` increases on rotation.
    DeviceSigningKey { key: HybridPublic, generation: u32 },
    /// Device-signing key -> one device.
    Device {
        device_id: [u8; 16],
        display_name: String,
        /// The MLS leaf signature key (Ed25519 today).
        leaf_key: Vec<u8>,
        /// The device's X-Wing public key.
        kem_key: Vec<u8>,
    },
    /// Device-signing key: this device is revoked (lost or retired).
    RevokeDevice { device_id: [u8; 16] },
    /// Device-signing key: this identity holds this account on this server
    /// (published in that server's key-transparency log). The account is the
    /// immutable 128-bit account id, never Murmur's reusable registration number.
    Account { server: [u8; 32], account: [u8; 16] },
    /// Device-signing key: this identity also uses this Mumble certificate.
    MumbleCertificate { sha1: [u8; 20] },
}

impl Statement {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::DeviceSigningKey { key, generation } => {
                put(&mut out, b"nuschl/device-signing-key/v1");
                put(&mut out, &key.encode());
                put(&mut out, &generation.to_be_bytes());
            }
            Self::Device {
                device_id,
                display_name,
                leaf_key,
                kem_key,
            } => {
                put(&mut out, b"nuschl/device/v1");
                put(&mut out, device_id);
                put(&mut out, display_name.as_bytes());
                put(&mut out, leaf_key);
                put(&mut out, kem_key);
            }
            Self::RevokeDevice { device_id } => {
                put(&mut out, b"nuschl/revoke-device/v1");
                put(&mut out, device_id);
            }
            Self::Account { server, account } => {
                put(&mut out, b"nuschl/account/v1");
                put(&mut out, server);
                put(&mut out, account);
            }
            Self::MumbleCertificate { sha1 } => {
                put(&mut out, b"nuschl/mumble-certificate/v1");
                put(&mut out, sha1);
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        let statement = match r.get()? {
            b"nuschl/device-signing-key/v1" => {
                let key = HybridPublic::read(&mut Reader(r.get()?))?;
                let generation = u32::from_be_bytes(r.get()?.try_into().ok()?);
                Self::DeviceSigningKey { key, generation }
            }
            b"nuschl/device/v1" => Self::Device {
                device_id: r.get()?.try_into().ok()?,
                display_name: String::from_utf8(r.get()?.to_vec()).ok()?,
                leaf_key: r.get()?.to_vec(),
                kem_key: r.get()?.to_vec(),
            },
            b"nuschl/revoke-device/v1" => Self::RevokeDevice {
                device_id: r.get()?.try_into().ok()?,
            },
            b"nuschl/account/v1" => Self::Account {
                server: r.get()?.try_into().ok()?,
                account: r.get()?.try_into().ok()?,
            },
            b"nuschl/mumble-certificate/v1" => Self::MumbleCertificate {
                sha1: r.get()?.try_into().ok()?,
            },
            _ => return None,
        };
        r.done().then_some(statement)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cert {
    pub issuer: HybridPublic,
    pub statement: Statement,
    pub signature: HybridSignature,
}

impl Cert {
    pub fn issue(issuer: &HybridKey, statement: Statement) -> Self {
        let signature = issuer.sign(&statement.encode());
        Self {
            issuer: issuer.public().clone(),
            statement,
            signature,
        }
    }

    pub fn verify(&self) -> Result<(), &'static str> {
        self.issuer
            .verify(&self.statement.encode(), &self.signature)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, &self.issuer.encode());
        put(&mut out, &self.statement.encode());
        put(&mut out, &self.signature.ed25519);
        put(&mut out, &self.signature.mldsa65);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        let cert = Self {
            issuer: HybridPublic::read(&mut Reader(r.get()?))?,
            statement: Statement::decode(r.get()?)?,
            signature: HybridSignature {
                ed25519: r.get()?.to_vec(),
                mldsa65: r.get()?.to_vec(),
            },
        };
        r.done().then_some(cert)
    }
}

/// A user's identity as held by a device that may link other devices.
pub struct UserIdentity {
    /// In production: not kept on devices in the clear, but encrypted under the
    /// recovery key (docs/e2ee.md); used only to rotate the device-signing key.
    pub root: HybridKey,
    pub device_signing: HybridKey,
    pub device_signing_cert: Cert,
}

impl UserIdentity {
    pub fn generate() -> Self {
        let root = HybridKey::generate();
        let device_signing = HybridKey::generate();
        let device_signing_cert = Cert::issue(
            &root,
            Statement::DeviceSigningKey {
                key: device_signing.public().clone(),
                generation: 1,
            },
        );
        Self {
            root,
            device_signing,
            device_signing_cert,
        }
    }

    pub fn root_public(&self) -> &HybridPublic {
        self.root.public()
    }

    /// Rotates the device-signing key (after a device with it was lost) without
    /// changing the identity: contacts' verification stays valid.
    pub fn rotate_device_signing_key(&mut self) {
        let generation = match &self.device_signing_cert.statement {
            Statement::DeviceSigningKey { generation, .. } => generation + 1,
            _ => unreachable!(),
        };
        self.device_signing = HybridKey::generate();
        self.device_signing_cert = Cert::issue(
            &self.root,
            Statement::DeviceSigningKey {
                key: self.device_signing.public().clone(),
                generation,
            },
        );
    }

    pub fn certify_device(&self, keys: &DeviceKeys, display_name: &str) -> Cert {
        Cert::issue(
            &self.device_signing,
            Statement::Device {
                device_id: keys.device_id,
                display_name: display_name.into(),
                leaf_key: keys.signer.public().to_vec(),
                kem_key: keys.kem.public.clone(),
            },
        )
    }

    pub fn sign_statement(&self, statement: Statement) -> Cert {
        Cert::issue(&self.device_signing, statement)
    }

    /// The MLS credential content for one device: the chain to the root.
    pub fn credential_chain(&self, device_cert: &Cert) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, &self.root_public().encode());
        put(&mut out, &self.device_signing_cert.encode());
        put(&mut out, &device_cert.encode());
        out
    }
}

/// One device's own keys.
pub struct DeviceKeys {
    pub device_id: [u8; 16],
    /// MLS leaf signature key.
    pub signer: DeviceSigner,
    /// X-Wing key pair for linking and key delivery outside MLS.
    pub kem: HpkeKeyPair,
}

impl DeviceKeys {
    pub fn generate(scheme: SignatureScheme) -> Self {
        let crypto = crypto();
        let ikm: Zeroizing<Vec<u8>> = Zeroizing::new(crypto.random_vec(32).expect("randomness"));
        Self {
            device_id: crypto.random_array().expect("randomness"),
            signer: DeviceSigner::generate(scheme),
            kem: crypto
                .derive_hpke_keypair(xwing(), &ikm)
                .expect("x-wing key pair"),
        }
    }
}

/// A verified member device, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDevice {
    pub identity: [u8; 32],
    pub device_id: [u8; 16],
    pub display_name: String,
    pub kem_key: Vec<u8>,
}

/// Verifies an MLS credential chain and its binding to the leaf signature key.
pub fn verify_chain(chain: &[u8], leaf_key: &[u8]) -> Result<VerifiedDevice, &'static str> {
    let mut r = Reader(chain);
    let root = HybridPublic::read(&mut Reader(r.get().ok_or("truncated")?)).ok_or("bad root")?;
    let signing = Cert::decode(r.get().ok_or("truncated")?).ok_or("bad signing cert")?;
    let device = Cert::decode(r.get().ok_or("truncated")?).ok_or("bad device cert")?;
    if !r.done() {
        return Err("trailing data");
    }
    if signing.issuer != root {
        return Err("device-signing key not issued by the root");
    }
    signing.verify()?;
    let Statement::DeviceSigningKey { key, .. } = &signing.statement else {
        return Err("not a device-signing key certificate");
    };
    if &device.issuer != key {
        return Err("device not certified by the device-signing key");
    }
    device.verify()?;
    let Statement::Device {
        device_id,
        display_name,
        leaf_key: certified,
        kem_key,
    } = device.statement
    else {
        return Err("not a device certificate");
    };
    if certified != leaf_key {
        return Err("credential does not match the MLS leaf key");
    }
    Ok(VerifiedDevice {
        identity: root.fingerprint(),
        device_id,
        display_name,
        kem_key,
    })
}

// --- Safety numbers ---

fn fingerprint_digits(identity: &HybridPublic) -> String {
    // Signal's numeric fingerprint construction (5200 iterations of SHA-512 over
    // version || key, then key appended each round), over the hybrid root key and
    // without a server-specific identifier: the identity is the same everywhere.
    let crypto = crypto();
    let key = identity.encode();
    let mut hash = {
        let mut input = vec![0, 1];
        input.extend_from_slice(&key);
        crypto.hash(HashType::Sha2_512, &input).expect("sha512")
    };
    for _ in 1..5200 {
        let mut input = hash.clone();
        input.extend_from_slice(&key);
        hash = crypto.hash(HashType::Sha2_512, &input).expect("sha512");
    }
    hash[..30]
        .chunks(5)
        .map(|chunk| {
            let n = chunk.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            format!("{:05}", n % 100_000)
        })
        .collect()
}

/// 60 digits (12 groups of 5) over both identity root keys, the same on both
/// sides: the lower half first.
pub fn safety_number(a: &HybridPublic, b: &HybridPublic) -> String {
    let (x, y) = (fingerprint_digits(a), fingerprint_digits(b));
    let (first, second) = if x <= y { (x, y) } else { (y, x) };
    format!("{first}{second}")
}

/// QR payload: version, then both fingerprints in the same order.
pub fn safety_qr(a: &HybridPublic, b: &HybridPublic) -> Vec<u8> {
    let (x, y) = (a.fingerprint(), b.fingerprint());
    let (first, second) = if x <= y { (x, y) } else { (y, x) };
    let mut out = b"NSQ1".to_vec();
    out.extend_from_slice(&first);
    out.extend_from_slice(&second);
    out
}

// --- Device linking ---

/// What the new device shows as a QR code: a rendezvous id and the hash of its
/// keys (the keys themselves travel through the server's linking mailbox, where
/// the hash authenticates them).
pub struct LinkOffer {
    pub rendezvous: [u8; 16],
    pub kem_key: Vec<u8>,
    pub leaf_key: Vec<u8>,
}

impl LinkOffer {
    pub fn new(new_device: &DeviceKeys) -> Self {
        Self {
            rendezvous: crypto().random_array().expect("randomness"),
            kem_key: new_device.kem.public.clone(),
            leaf_key: new_device.signer.public().to_vec(),
        }
    }

    /// The QR code content: 4 + 16 + 32 bytes.
    pub fn qr(&self) -> Vec<u8> {
        let mut keys = Vec::new();
        put(&mut keys, &self.kem_key);
        put(&mut keys, &self.leaf_key);
        let hash = crypto().hash(HashType::Sha2_256, &keys).expect("sha256");
        let mut out = b"NSL1".to_vec();
        out.extend_from_slice(&self.rendezvous);
        out.extend_from_slice(&hash);
        out
    }

    /// The existing device checks the keys fetched from the mailbox against the QR.
    pub fn matches_qr(&self, qr: &[u8]) -> bool {
        self.qr() == qr
    }
}

/// What the existing device sends to the new one, sealed to its X-Wing key: the
/// device certificate, the identity chain, and (in production) the device-signing
/// key if the user lets this device link others, contact pins and verified
/// identities, account bindings, and history keys.
pub fn seal_provisioning(offer: &LinkOffer, payload: &[u8]) -> HpkeCiphertext {
    let mut info = b"nuschl/link/v1".to_vec();
    info.extend_from_slice(&offer.rendezvous);
    crypto()
        .hpke_seal(xwing(), &offer.kem_key, &info, &offer.leaf_key, payload)
        .expect("hpke seal")
}

pub fn open_provisioning(
    keys: &DeviceKeys,
    rendezvous: &[u8; 16],
    sealed: &HpkeCiphertext,
) -> Result<Vec<u8>, &'static str> {
    let mut info = b"nuschl/link/v1".to_vec();
    info.extend_from_slice(rendezvous);
    crypto()
        .hpke_open(
            xwing(),
            sealed,
            keys.kem.private.as_ref(),
            &info,
            keys.signer.public(),
        )
        .map_err(|_| "cannot open provisioning bundle")
}
