// SPDX-License-Identifier: EUPL-1.2
//! Device linking (docs/e2ee.md §3.4): protocol logic only. The crypto worker moves the
//! messages through the server's linking mailbox; nothing here does I/O.
//!
//! ```text
//! existing device (E)                    mailbox                 new device (N)
//! shows LinkCode "<nameplate>-dddd-dddd"              user types the code
//! ExistingDeviceLink::start ── LinkPake(A) ──►   ◄── LinkPake(B) ── NewDeviceLink::start
//! receive_pake                                                    receive_pake
//!                              ◄── LinkDeviceKeys (keys, name, confirmation) ──
//! receive_device_keys: checks the confirmation (wrong code or swapped keys fail here),
//! certifies N, seals a ProvisioningBundle to N's X-Wing key (HPKE PSK mode)
//!                              ── LinkBundle ──►                  receive_bundle
//! ```
//!
//! SPAKE2 (`spake2` 0.4, Ed25519 group, the PAKE magic-wormhole uses) runs with the full
//! code as password, E as side A (`"nuschel/device-link/v1/existing"`) and N as side B
//! (`"nuschel/device-link/v1/new"`). Peer elements must be canonical and of prime
//! order. With `K` the SPAKE2 key:
//!
//! ```text
//! transcript   = SHA-256(uint8(22) || "nuschel/device-link/v1"
//!                        || LinkPake(A).pake || LinkPake(B).pake || LinkDeviceKeys body)
//! prk          = HKDF-Extract(salt = transcript, ikm = K)
//! confirm_key  = HKDF-Expand(prk, "nuschel/device-link/v1 confirm", 32)
//! psk          = HKDF-Expand(prk, "nuschel/device-link/v1 psk", 32)
//! confirmation = HMAC-SHA-256(confirm_key, transcript)
//! LinkBundle   = HPKE PSK mode to N's kem_key, info = uint8(22) || "nuschel/device-link/v1"
//!                || transcript, psk, psk_id = "nuschel/device-link/v1", aad = ""
//! ```
//!
//! The confirmation authenticates N's keys to E (a server that swaps them, or a wrong
//! code, fails); the PSK authenticates the bundle to N (only E knows it) and binds it to
//! this session. The bundle's confidentiality rests on X-Wing and the PSK, so recorded
//! linking traffic stays safe against a quantum computer even though SPAKE2 is classical.
//!
//! ```text
//! struct { uint8 kind = 0x10; uint8 version = 1; opaque pake[33]; } LinkPake;
//!
//! struct {
//!   uint8  kind = 0x11; uint8 version = 1;
//!   opaque device_id[16]; uint32 key_epoch;
//!   SignatureAlgorithm leaf_algorithm; opaque leaf_key[32];
//!   KemAlgorithm kem_algorithm; opaque kem_key[1216];
//!   opaque display_name<0..64>;
//!   opaque confirmation[32];
//! } LinkDeviceKeys;
//!
//! struct { uint8 kind = 0x12; uint8 version = 1; opaque enc[1120];
//!          opaque ciphertext<16..2^24-1>; } LinkBundle;
//!
//! struct {
//!   uint8  kind = 0x13; uint8 version = 1;
//!   HybridPublicKey root_public_key;
//!   opaque device_signing_certificate<1..2^16-1>;
//!   opaque device_certificate<1..2^16-1>;     // for N
//!   opaque device_credential<1..2^16-1>;      // for N
//!   uint8  has_device_signing_secret; [opaque device_signing_secret[96]]
//!   uint8  has_recovery_blob; [opaque recovery_blob<1..2^16-1>]
//!   uint8  has_sync_key; [uint32 sync_key_epoch; opaque sync_key[32]]
//!   opaque application_data<0..2^22>;         // pins, contacts, history keys, bindings
//! } ProvisioningBundle;
//! ```

use std::fmt;

use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::certificate::{DeviceCertificate, DeviceCredential, DeviceSigningAuthority};
use crate::codec::{Reader, Writer, kind, label};
use crate::crypto::{self, xwing};
use crate::keys::{
    DeviceId, DeviceKeys, DevicePublicKeys, DeviceSigningKey, DisplayName, Ed25519PublicKey,
    HybridPublicKey, KemAlgorithm, SignatureAlgorithm, XWingPublicKey,
};
use crate::keystore::{DeviceSyncKey, RecoveryBlob};
use crate::roster::VerifiedIdentity;
use crate::{DeviceSigningCertificate, Error, hpke};

const LINK_LABEL: &str = "nuschel/device-link/v1";
const SIDE_EXISTING: &[u8] = b"nuschel/device-link/v1/existing";
const SIDE_NEW: &[u8] = b"nuschel/device-link/v1/new";
const CONFIRM_INFO: &[u8] = b"nuschel/device-link/v1 confirm";
const PSK_INFO: &[u8] = b"nuschel/device-link/v1 psk";
const PAKE_LEN: usize = 33;

/// The largest application payload a provisioning bundle carries (4 MiB).
pub const MAX_APPLICATION_DATA: usize = 1 << 22;

/// The short code: the mailbox nameplate and 8 random digits (about 26.6 bits; an
/// active attacker gets one guess per linking attempt, and a failed attempt is shown).
pub struct LinkCode(Zeroizing<String>);

impl LinkCode {
    pub const SECRET_DIGITS: usize = 8;

    /// A fresh code for the nameplate the server's mailbox assigned.
    pub fn generate(nameplate: u32) -> Result<Self, Error> {
        let mut digits = Zeroizing::new(Vec::with_capacity(Self::SECRET_DIGITS));
        while digits.len() < Self::SECRET_DIGITS {
            let mut bytes = Zeroizing::new([0u8; 16]);
            crypto::fill_random(&mut bytes[..])?;
            for &b in bytes.iter() {
                // Rejection sampling: 250 is the largest multiple of 10 below 256.
                if b < 250 && digits.len() < Self::SECRET_DIGITS {
                    digits.push(b'0' + b % 10);
                }
            }
        }
        let digits = std::str::from_utf8(&digits).expect("ASCII digits");
        Ok(Self(Zeroizing::new(format!(
            "{nameplate}-{}-{}",
            &digits[..4],
            &digits[4..]
        ))))
    }

    /// Parses a typed code: `<nameplate>-dddd-dddd`, the nameplate in decimal without
    /// leading zeros. Surrounding whitespace is ignored, nothing else.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        let mut parts = text.split('-');
        let (Some(nameplate), Some(a), Some(b), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::Malformed("link code format"));
        };
        let canonical_number = !nameplate.is_empty()
            && nameplate.bytes().all(|c| c.is_ascii_digit())
            && (nameplate == "0" || !nameplate.starts_with('0'))
            && nameplate.parse::<u32>().is_ok();
        let digits = |s: &str| s.len() == 4 && s.bytes().all(|c| c.is_ascii_digit());
        if !canonical_number || !digits(a) || !digits(b) {
            return Err(Error::Malformed("link code format"));
        }
        Ok(Self(Zeroizing::new(text.to_owned())))
    }

    pub fn nameplate(&self) -> u32 {
        self.0
            .split('-')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("validated")
    }

    /// The code as shown to the user.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn password(&self) -> Password {
        Password::new(self.0.as_bytes())
    }
}

impl fmt::Debug for LinkCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkCode({}-redacted)", self.nameplate())
    }
}

/// A SPAKE2 message (side byte and element).
#[derive(Clone, PartialEq, Eq)]
pub struct LinkPake([u8; PAKE_LEN]);

impl LinkPake {
    pub const LEN: usize = 2 + PAKE_LEN;

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.header(kind::LINK_PAKE);
        writer.bytes(&self.0);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::LEN)?;
        reader.header(kind::LINK_PAKE)?;
        let pake = reader.array()?;
        reader.finish()?;
        Ok(Self(pake))
    }

    /// The peer's message must come from the other side and carry a prime-order point.
    fn check_side(&self, side: u8) -> Result<(), Error> {
        let element: &[u8; 32] = self.0[1..].try_into().expect("33 bytes");
        if self.0[0] != side || !crypto::valid_spake2_element(element) {
            return Err(Error::LinkAuthentication);
        }
        Ok(())
    }
}

impl fmt::Debug for LinkPake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkPake")
    }
}

fn pake_message(bytes: Vec<u8>) -> LinkPake {
    LinkPake(
        bytes
            .try_into()
            .expect("spake2 Ed25519 messages are 33 bytes"),
    )
}

/// The new device's keys and name with the key confirmation.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkDeviceKeys {
    keys: DevicePublicKeys,
    display_name: DisplayName,
    confirmation: [u8; 32],
}

impl LinkDeviceKeys {
    pub const MAX_LEN: usize =
        2 + 16 + 4 + 2 + 32 + 2 + XWingPublicKey::LEN + 1 + DisplayName::MAX_LEN + 32;

    fn write_body(keys: &DevicePublicKeys, display_name: &DisplayName, writer: &mut Writer) {
        writer.header(kind::LINK_DEVICE_KEYS);
        writer.bytes(keys.device_id.as_bytes());
        writer.u32(keys.key_epoch);
        writer.u16(SignatureAlgorithm::Ed25519.code());
        writer.bytes(keys.leaf_key.as_bytes());
        writer.u16(KemAlgorithm::XWing.code());
        writer.bytes(keys.kem_key.as_bytes());
        display_name.write(writer);
    }

    fn body(keys: &DevicePublicKeys, display_name: &DisplayName) -> Vec<u8> {
        let mut writer = Writer::new();
        Self::write_body(keys, display_name, &mut writer);
        writer.into_vec()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        Self::write_body(&self.keys, &self.display_name, &mut writer);
        writer.bytes(&self.confirmation);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::MAX_LEN)?;
        reader.header(kind::LINK_DEVICE_KEYS)?;
        let device_id = DeviceId::from_bytes(reader.array()?);
        let key_epoch = match reader.u32()? {
            0 => return Err(Error::Malformed("key epoch 0")),
            epoch => epoch,
        };
        SignatureAlgorithm::expect(&mut reader, SignatureAlgorithm::Ed25519)?;
        let leaf_key = Ed25519PublicKey::read(&mut reader)?;
        KemAlgorithm::expect(&mut reader, KemAlgorithm::XWing)?;
        let kem_key = XWingPublicKey::read(&mut reader)?;
        let display_name = DisplayName::read(&mut reader)?;
        let confirmation = reader.array()?;
        reader.finish()?;
        Ok(Self {
            keys: DevicePublicKeys {
                device_id,
                key_epoch,
                leaf_key,
                kem_key,
            },
            display_name,
            confirmation,
        })
    }
}

impl fmt::Debug for LinkDeviceKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkDeviceKeys")
            .field("keys", &self.keys)
            .field("display_name", &self.display_name)
            .finish_non_exhaustive()
    }
}

/// The sealed provisioning bundle.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkBundle {
    enc: Box<[u8; xwing::CIPHERTEXT_LEN]>,
    ciphertext: Vec<u8>,
}

impl LinkBundle {
    pub const MAX_LEN: usize = 2 + xwing::CIPHERTEXT_LEN + 3 + (1 << 24) - 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.header(kind::LINK_BUNDLE);
        writer.bytes(&self.enc[..]);
        writer.opaque24(&self.ciphertext);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::MAX_LEN)?;
        reader.header(kind::LINK_BUNDLE)?;
        let enc = reader.boxed()?;
        let ciphertext = reader.opaque24(16, (1 << 24) - 1)?.to_vec();
        reader.finish()?;
        Ok(Self { enc, ciphertext })
    }
}

impl fmt::Debug for LinkBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkBundle({} bytes)", self.ciphertext.len())
    }
}

/// Session keys derived after SPAKE2, once the transcript is complete.
struct SessionKeys {
    transcript: [u8; 32],
    confirm_key: Zeroizing<[u8; 32]>,
    psk: Zeroizing<[u8; 32]>,
}

impl SessionKeys {
    fn derive(
        spake_key: &[u8],
        pake_existing: &LinkPake,
        pake_new: &LinkPake,
        body: &[u8],
    ) -> Self {
        let transcript = crypto::sha256(&[&label(LINK_LABEL), &pake_existing.0, &pake_new.0, body]);
        let prk = crypto::hkdf_extract(&transcript, spake_key);
        Self {
            transcript,
            confirm_key: crypto::hkdf_expand(&prk, CONFIRM_INFO),
            psk: crypto::hkdf_expand(&prk, PSK_INFO),
        }
    }

    fn confirmation(&self) -> [u8; 32] {
        crypto::hmac_sha256(&self.confirm_key[..], &self.transcript)
    }

    fn hpke_info(&self) -> Vec<u8> {
        let mut info = label(LINK_LABEL);
        info.extend_from_slice(&self.transcript);
        info
    }
}

// ---------------------------------------------------------------------------------
// Existing device

/// The existing device, after showing the code.
pub struct ExistingDeviceLink {
    spake: Spake2<Ed25519Group>,
    own: LinkPake,
}

impl ExistingDeviceLink {
    pub fn start(code: &LinkCode) -> (Self, LinkPake) {
        let (spake, message) = Spake2::<Ed25519Group>::start_a(
            &code.password(),
            &Identity::new(SIDE_EXISTING),
            &Identity::new(SIDE_NEW),
        );
        let own = pake_message(message);
        (
            Self {
                spake,
                own: own.clone(),
            },
            own,
        )
    }

    pub fn receive_pake(self, theirs: &LinkPake) -> Result<ExistingAwaitingKeys, Error> {
        theirs.check_side(b'B')?;
        let key = self
            .spake
            .finish(&theirs.0)
            .map_err(|_| Error::LinkAuthentication)?;
        Ok(ExistingAwaitingKeys {
            spake_key: Zeroizing::new(key),
            own: self.own,
            theirs: theirs.clone(),
        })
    }
}

impl fmt::Debug for ExistingDeviceLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExistingDeviceLink")
    }
}

/// The existing device, waiting for the new device's keys.
pub struct ExistingAwaitingKeys {
    spake_key: Zeroizing<Vec<u8>>,
    own: LinkPake,
    theirs: LinkPake,
}

impl ExistingAwaitingKeys {
    /// Checks the key confirmation. Failure means a wrong code or modified messages; the
    /// user should start over with a new code.
    pub fn receive_device_keys(self, message: &LinkDeviceKeys) -> Result<NewDeviceKeys, Error> {
        let body = LinkDeviceKeys::body(&message.keys, &message.display_name);
        let keys = SessionKeys::derive(&self.spake_key, &self.own, &self.theirs, &body);
        if !crypto::ct_eq(&keys.confirmation(), &message.confirmation) {
            return Err(Error::LinkAuthentication);
        }
        Ok(NewDeviceKeys {
            device: message.keys.clone(),
            display_name: message.display_name.clone(),
            keys,
        })
    }
}

#[cfg(test)]
impl ExistingAwaitingKeys {
    /// The state machines consume themselves; tests try several forgeries against one
    /// session.
    pub(crate) fn duplicate_for_tests(&self) -> Self {
        Self {
            spake_key: self.spake_key.clone(),
            own: self.own.clone(),
            theirs: self.theirs.clone(),
        }
    }
}

impl fmt::Debug for ExistingAwaitingKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExistingAwaitingKeys")
    }
}

/// The new device's authenticated keys, ready to be certified and provisioned.
pub struct NewDeviceKeys {
    device: DevicePublicKeys,
    display_name: DisplayName,
    keys: SessionKeys,
}

impl NewDeviceKeys {
    pub fn device_keys(&self) -> &DevicePublicKeys {
        &self.device
    }

    /// The name the new device proposed.
    pub fn display_name(&self) -> &DisplayName {
        &self.display_name
    }

    /// Seals the bundle to the new device. The bundle must certify exactly the
    /// authenticated keys.
    pub fn seal(self, bundle: &ProvisioningBundle) -> Result<LinkBundle, Error> {
        if bundle.device_certificate.public_keys() != self.device
            || !bundle
                .device_certificate
                .matches_credential(&bundle.device_credential)
        {
            return Err(Error::KeyMismatch);
        }
        let plaintext = bundle.encode();
        let (enc, ciphertext) = hpke::seal(
            &self.device.kem_key,
            &self.keys.hpke_info(),
            &plaintext,
            Some((&self.keys.psk, LINK_LABEL.as_bytes())),
        )?;
        Ok(LinkBundle { enc, ciphertext })
    }
}

impl fmt::Debug for NewDeviceKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewDeviceKeys")
            .field("device", &self.device)
            .field("display_name", &self.display_name)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------
// New device

/// The new device, after the user typed the code.
pub struct NewDeviceLink {
    spake: Spake2<Ed25519Group>,
    own: LinkPake,
    device: DevicePublicKeys,
    display_name: DisplayName,
}

impl NewDeviceLink {
    pub fn start(
        code: &LinkCode,
        device: &DeviceKeys,
        display_name: DisplayName,
    ) -> (Self, LinkPake) {
        let (spake, message) = Spake2::<Ed25519Group>::start_b(
            &code.password(),
            &Identity::new(SIDE_EXISTING),
            &Identity::new(SIDE_NEW),
        );
        let own = pake_message(message);
        (
            Self {
                spake,
                own: own.clone(),
                device: device.public_keys(),
                display_name,
            },
            own,
        )
    }

    pub fn receive_pake(
        self,
        theirs: &LinkPake,
    ) -> Result<(NewAwaitingBundle, LinkDeviceKeys), Error> {
        theirs.check_side(b'A')?;
        let spake_key = Zeroizing::new(
            self.spake
                .finish(&theirs.0)
                .map_err(|_| Error::LinkAuthentication)?,
        );
        let body = LinkDeviceKeys::body(&self.device, &self.display_name);
        let keys = SessionKeys::derive(&spake_key, theirs, &self.own, &body);
        let message = LinkDeviceKeys {
            keys: self.device.clone(),
            display_name: self.display_name,
            confirmation: keys.confirmation(),
        };
        Ok((
            NewAwaitingBundle {
                device: self.device,
                keys,
            },
            message,
        ))
    }
}

impl fmt::Debug for NewDeviceLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NewDeviceLink")
    }
}

/// The new device, waiting for the bundle.
pub struct NewAwaitingBundle {
    device: DevicePublicKeys,
    keys: SessionKeys,
}

impl NewAwaitingBundle {
    /// Opens and checks the bundle: it must come from this session (PSK), chain to the
    /// root it names, and certify exactly this device's keys, valid at `now`.
    pub fn receive_bundle(
        self,
        device: &DeviceKeys,
        bundle: &LinkBundle,
        now: u64,
    ) -> Result<ProvisionedDevice, Error> {
        if device.public_keys() != self.device {
            return Err(Error::KeyMismatch);
        }
        let plaintext = device
            .open_hpke(
                &bundle.enc,
                &self.keys.hpke_info(),
                &bundle.ciphertext,
                Some((&self.keys.psk, LINK_LABEL.as_bytes())),
            )
            .map_err(|_| Error::LinkAuthentication)?;
        let bundle = ProvisioningBundle::decode(&plaintext)?;
        bundle.check_for(device, now)
    }
}

impl fmt::Debug for NewAwaitingBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewAwaitingBundle")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------
// The bundle

/// What the existing device gives the new one (docs/e2ee.md §3.4 step 4).
pub struct ProvisioningBundle {
    root: HybridPublicKey,
    device_signing_certificate: DeviceSigningCertificate,
    device_certificate: DeviceCertificate,
    device_credential: DeviceCredential,
    device_signing_key: Option<DeviceSigningKey>,
    recovery_blob: Option<RecoveryBlob>,
    sync_key: Option<DeviceSyncKey>,
    application_data: Zeroizing<Vec<u8>>,
}

impl ProvisioningBundle {
    const SECRET_CAPACITY: usize = 2
        + HybridPublicKey::LEN
        + 2
        + DeviceSigningCertificate::LEN
        + 2
        + DeviceCertificate::MAX_LEN
        + 2
        + DeviceCredential::LEN
        + 1
        + DeviceSigningKey::SECRET_LEN
        + 1
        + 2
        + RecoveryBlob::MAX_LEN
        + 1
        + 4
        + 32
        + 3
        + MAX_APPLICATION_DATA;

    /// A bundle with the identity and the new device's certificate and credential.
    pub fn new(
        identity: &VerifiedIdentity,
        device_certificate: DeviceCertificate,
        device_credential: DeviceCredential,
    ) -> Self {
        Self {
            root: identity.root_key().clone(),
            device_signing_certificate: identity.device_signing_certificate().clone(),
            device_certificate,
            device_credential,
            device_signing_key: None,
            recovery_blob: None,
            sync_key: None,
            application_data: Zeroizing::new(Vec::new()),
        }
    }

    /// Lets the new device link others: includes the device-signing private key.
    pub fn with_device_signing(
        mut self,
        authority: &DeviceSigningAuthority,
    ) -> Result<Self, Error> {
        if authority.certificate() != &self.device_signing_certificate {
            return Err(Error::KeyMismatch);
        }
        self.device_signing_key = Some(authority.key().duplicate());
        Ok(self)
    }

    pub fn with_recovery_blob(mut self, blob: RecoveryBlob) -> Self {
        self.recovery_blob = Some(blob);
        self
    }

    pub fn with_sync_key(mut self, key: &DeviceSyncKey) -> Self {
        self.sync_key = Some(key.duplicate());
        self
    }

    /// The crypto worker's own payload: pins, verified contacts, history keys, account
    /// bindings, in its own encoding (at most [`MAX_APPLICATION_DATA`] bytes).
    pub fn with_application_data(mut self, data: &[u8]) -> Result<Self, Error> {
        if data.len() > MAX_APPLICATION_DATA {
            return Err(Error::TooLong);
        }
        self.application_data = Zeroizing::new(data.to_vec());
        Ok(self)
    }

    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut writer = Writer::secret(Self::SECRET_CAPACITY);
        writer.header(kind::PROVISIONING_BUNDLE);
        self.root.write(&mut writer);
        writer.opaque16(&self.device_signing_certificate.encode());
        writer.opaque16(&self.device_certificate.encode());
        writer.opaque16(&self.device_credential.encode());
        writer.presence(self.device_signing_key.is_some());
        if let Some(key) = &self.device_signing_key {
            key.write_secret(&mut writer);
        }
        writer.presence(self.recovery_blob.is_some());
        if let Some(blob) = &self.recovery_blob {
            writer.opaque16(&blob.encode());
        }
        writer.presence(self.sync_key.is_some());
        if let Some(key) = &self.sync_key {
            key.write(&mut writer);
        }
        writer.opaque24(&self.application_data);
        writer.finish()
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::SECRET_CAPACITY)?;
        reader.header(kind::PROVISIONING_BUNDLE)?;
        let root = HybridPublicKey::read(&mut reader)?;
        let device_signing_certificate =
            DeviceSigningCertificate::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        let device_certificate =
            DeviceCertificate::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        let device_credential =
            DeviceCredential::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        let device_signing_key = if reader.presence()? {
            Some(DeviceSigningKey::read_secret(&mut reader)?)
        } else {
            None
        };
        let recovery_blob = if reader.presence()? {
            Some(RecoveryBlob::decode(
                reader.opaque16(1, usize::from(u16::MAX))?,
            )?)
        } else {
            None
        };
        let sync_key = if reader.presence()? {
            Some(DeviceSyncKey::read(&mut reader)?)
        } else {
            None
        };
        let application_data = Zeroizing::new(reader.opaque24(0, MAX_APPLICATION_DATA)?.to_vec());
        reader.finish()?;
        Ok(Self {
            root,
            device_signing_certificate,
            device_certificate,
            device_credential,
            device_signing_key,
            recovery_blob,
            sync_key,
            application_data,
        })
    }

    /// Everything the new device checks before it trusts the bundle.
    fn check_for(self, device: &DeviceKeys, now: u64) -> Result<ProvisionedDevice, Error> {
        let identity = VerifiedIdentity::verify(self.root, self.device_signing_certificate)?;
        self.device_certificate.verify(&identity)?;
        if self.device_certificate.public_keys() != device.public_keys() {
            return Err(Error::KeyMismatch);
        }
        self.device_certificate.validity().check(now)?;
        self.device_credential
            .verify(&identity, device.leaf_key().as_bytes(), now)?;
        if !self
            .device_certificate
            .matches_credential(&self.device_credential)
        {
            return Err(Error::KeyMismatch);
        }
        let device_signing = match self.device_signing_key {
            Some(key) => Some(DeviceSigningAuthority::new(
                key,
                identity.device_signing_certificate().clone(),
            )?),
            None => None,
        };
        Ok(ProvisionedDevice {
            identity,
            device_certificate: self.device_certificate,
            device_credential: self.device_credential,
            device_signing,
            recovery_blob: self.recovery_blob,
            sync_key: self.sync_key,
            application_data: self.application_data,
        })
    }
}

impl fmt::Debug for ProvisioningBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisioningBundle")
            .field("device_certificate", &self.device_certificate)
            .field("device_signing_key", &self.device_signing_key.is_some())
            .finish_non_exhaustive()
    }
}

/// A verified bundle, as the new device keeps it (see
/// [`DeviceKeystore`](crate::DeviceKeystore)).
pub struct ProvisionedDevice {
    pub identity: VerifiedIdentity,
    pub device_certificate: DeviceCertificate,
    pub device_credential: DeviceCredential,
    pub device_signing: Option<DeviceSigningAuthority>,
    pub recovery_blob: Option<RecoveryBlob>,
    pub sync_key: Option<DeviceSyncKey>,
    pub application_data: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for ProvisionedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisionedDevice")
            .field("identity", &self.identity.fingerprint())
            .field("device_certificate", &self.device_certificate)
            .field("device_signing", &self.device_signing.is_some())
            .finish_non_exhaustive()
    }
}
