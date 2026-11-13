// SPDX-License-Identifier: EUPL-1.2
//! The sharer's and the viewers' key-exchange state machines (wire format in the crate
//! documentation). The sharer grants only fresh epochs: [`Sharer::rekey`] moves every
//! viewer to K_{e+1} and grants K_{e+1} to the queued requests, so a newcomer receives
//! only keys created after it joined.

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Context, Hpke, HpkePrivateKey, HpkePublicKey, Mode};
use hpke_rs_libcrux::{HpkeLibcrux, HpkeLibcruxPrng};
use libcrux_kem::Algorithm::XWingKemDraft06;
use rand_core::TryCryptoRng;
use subtle::ConstantTimeEq as _;
use zeroize::Zeroizing;

use crate::credentials::{Binding, CredentialCache, Fingerprint, VerifiableBinding};
use crate::link::{LinkKeys, label};
use crate::media::EpochKey;
use crate::wire::{Reader, put_opaque16};
use crate::{Error, MAX_KEY_MESSAGE, Secret, hmac_sha256, random, sha256};

const VERSION: u8 = 1;
/// KEM X-Wing, KDF HKDF-SHA256, AEAD ChaCha20-Poly1305, SFrame AES_128_GCM_SHA256_128.
const SUITE: [u8; 8] = [0x64, 0x7a, 0x00, 0x01, 0x00, 0x03, 0x00, 0x04];
const PUBLIC_KEY_LEN: usize = 1216;
const ENC_LEN: usize = 1120;
const AEAD_TAG_LEN: usize = 16;
const SEALED_KEY_LEN: usize = EpochKey::LEN + AEAD_TAG_LEN;

/// How far ahead a request's `not_after` may be: its lifetime plus a minute of clock
/// difference between viewer and sharer.
pub const REQUEST_LIFETIME_MS: u64 = 60_000;
const MAX_NOT_AFTER_MS: u64 = 2 * REQUEST_LIFETIME_MS;
/// The shortest time between two epochs.
pub const MIN_EPOCH_INTERVAL_MS: u64 = 1_000;
/// Requests waiting for the next rekey.
pub const MAX_PENDING: usize = 32;
/// Viewers holding keys, plus pending requests.
pub const MAX_VIEWERS: usize = 256;
/// Request nonces remembered until their `not_after`.
const MAX_SEEN: usize = 256;

pub(crate) const REQUEST: u8 = 1;
pub(crate) const GRANT: u8 = 2;
pub(crate) const ACK: u8 = 3;
pub(crate) const REKEY: u8 = 4;
pub(crate) const NEED_CREDENTIALS: u8 = 5;

pub type ViewerId = [u8; 16];

// -------------------------------------------------------------------------------------
// Envelope and bodies

pub(crate) fn seal_message<R: TryCryptoRng + ?Sized>(
    keys: &LinkKeys,
    kind: u8,
    body: &[u8],
    rng: &mut R,
) -> Result<Vec<u8>, Error> {
    if 2 + 12 + body.len() + AEAD_TAG_LEN > MAX_KEY_MESSAGE {
        return Err(Error::TooLong);
    }
    let nonce: [u8; 12] = random(rng)?;
    let mut out = vec![VERSION, kind];
    out.extend_from_slice(&nonce);
    let start = out.len();
    out.resize(start + body.len() + AEAD_TAG_LEN, 0);
    libcrux_chacha20poly1305::encrypt(
        keys.signal_key.expose(),
        body,
        &mut out[start..],
        &[VERSION, kind],
        &nonce,
    )
    .expect("buffer sized for body and tag");
    Ok(out)
}

pub(crate) fn open_message(keys: &LinkKeys, message: &[u8]) -> Result<(u8, Vec<u8>), Error> {
    if message.len() > MAX_KEY_MESSAGE {
        return Err(Error::TooLong);
    }
    let mut reader = Reader::new(message);
    let [version, kind] = reader.array()?;
    if version != VERSION {
        return Err(Error::UnsupportedVersion);
    }
    let nonce = reader.array()?;
    let sealed = reader.take(message.len() - 14)?;
    let mut body = vec![0; sealed.len().saturating_sub(AEAD_TAG_LEN)];
    libcrux_chacha20poly1305::decrypt(
        keys.signal_key.expose(),
        &mut body,
        sealed,
        &[version, kind],
        &nonce,
    )
    .map_err(|_| Error::Authentication)?;
    Ok((kind, body))
}

struct Request<'a> {
    viewer: ViewerId,
    nonce: [u8; 32],
    not_after: u64,
    public_key: &'a [u8; PUBLIC_KEY_LEN],
    binding: Option<Binding>,
}

impl<'a> Request<'a> {
    fn decode(body: &'a [u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(body);
        let viewer = reader.array()?;
        let nonce = reader.array()?;
        let not_after = reader.u64()?;
        let public_key = reader.take(PUBLIC_KEY_LEN)?.try_into().expect("taken");
        let binding = reader.opaque16(0)?;
        let binding = if binding.is_empty() {
            None
        } else {
            let mut binding_reader = Reader::new(binding);
            let decoded = Binding::decode(&mut binding_reader)?;
            binding_reader.finish()?;
            Some(decoded)
        };
        reader.finish()?;
        Ok(Self {
            viewer,
            nonce,
            not_after,
            public_key,
            binding,
        })
    }
}

pub(crate) fn transcript(request: &[u8], sharer_nonce: &[u8; 32]) -> [u8; 32] {
    sha256(&[
        label("transcript").as_bytes(),
        &[VERSION],
        &SUITE,
        request,
        sharer_nonce,
    ])
}

/// HPKE with a DRBG seeded from the caller's RNG.
fn hpke<R: TryCryptoRng + ?Sized>(rng: &mut R) -> Result<Hpke<HpkeLibcrux>, Error> {
    let seed = Zeroizing::new(random(rng)?);
    let prng = HpkeLibcruxPrng::try_from_seed(*seed).map_err(|_| Error::Randomness)?;
    Ok(Hpke::new_with_rng(
        Mode::Psk,
        KemAlgorithm::XWingDraft06,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
        prng,
    ))
}

fn hpke_info(transcript: &[u8; 32]) -> Vec<u8> {
    [label("grant").as_bytes(), transcript].concat()
}

/// The confirmations and the rekey channel key, from the HPKE exporter.
struct Exports {
    confirm_sharer: [u8; 32],
    confirm_viewer: [u8; 32],
    channel: Secret,
}

impl Exports {
    fn new(context: &Context<HpkeLibcrux>, transcript: &[u8; 32]) -> Result<Self, Error> {
        let export = |name: &str| -> Result<Secret, Error> {
            let bytes = Zeroizing::new(
                context
                    .export(label(name).as_bytes(), 32)
                    .map_err(|_| Error::Authentication)?,
            );
            Ok(Secret::from_bytes(bytes[..].try_into().expect("32 bytes")))
        };
        let confirm = |name| export(name).map(|key| hmac_sha256(key.expose(), transcript));
        Ok(Self {
            confirm_sharer: confirm("confirm sharer->viewer")?,
            confirm_viewer: confirm("confirm viewer->sharer")?,
            channel: export("channel sharer->viewer")?,
        })
    }
}

fn channel_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn rekey_header(viewer: &ViewerId, counter: u64) -> Vec<u8> {
    [&viewer[..], &counter.to_be_bytes()].concat()
}

// -------------------------------------------------------------------------------------
// Sharer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerState {
    /// Waiting for the next rekey.
    Requested,
    /// Holds the current epoch; its key confirmation has not arrived.
    Granted,
    Confirmed,
}

#[derive(Debug)]
pub enum SharerEvent {
    /// A viewer asked to join and waits for the next [`Sharer::rekey`]. A binding is
    /// for the identity layer to verify; [`Sharer::remove`] turns the request down.
    Requested {
        viewer: ViewerId,
        binding: Option<VerifiableBinding>,
    },
    /// The request names credentials this sharer lacks; send `message` to the viewer.
    NeedCredentials {
        viewer: ViewerId,
        message: Vec<u8>,
    },
    Confirmed(ViewerId),
}

struct Pending {
    viewer: ViewerId,
    body: Vec<u8>,
    not_after: u64,
}

struct Granted {
    viewer: ViewerId,
    channel: Secret,
    counter: u64,
    confirm_viewer: [u8; 32],
    confirmed: bool,
    /// The request's `not_after`: an unconfirmed viewer is dropped at the next rekey
    /// after it.
    not_after: u64,
}

pub struct Sharer {
    keys: LinkKeys,
    epoch: EpochKey,
    epoch_started: u64,
    pending: Vec<Pending>,
    viewers: Vec<Granted>,
    seen: Vec<([u8; 32], u64)>,
    credentials: CredentialCache,
}

impl Sharer {
    /// Starts at epoch 1, which no viewer ever receives.
    pub fn new<R: TryCryptoRng + ?Sized>(
        keys: LinkKeys,
        credentials: CredentialCache,
        now: u64,
        rng: &mut R,
    ) -> Result<Self, Error> {
        Ok(Self {
            keys,
            epoch: EpochKey::random(1, rng)?,
            epoch_started: now,
            pending: Vec::new(),
            viewers: Vec::new(),
            seen: Vec::new(),
            credentials,
        })
    }

    /// The key to encrypt media with.
    pub fn epoch(&self) -> &EpochKey {
        &self.epoch
    }

    pub fn viewers(&self) -> impl Iterator<Item = (ViewerId, ViewerState)> + '_ {
        let pending = self
            .pending
            .iter()
            .map(|p| (p.viewer, ViewerState::Requested));
        let granted = self.viewers.iter().map(|v| {
            let state = if v.confirmed {
                ViewerState::Confirmed
            } else {
                ViewerState::Granted
            };
            (v.viewer, state)
        });
        pending.chain(granted)
    }

    /// Handles a KeyRequest or KeyAck.
    pub fn receive<R: TryCryptoRng + ?Sized>(
        &mut self,
        message: &[u8],
        now: u64,
        rng: &mut R,
    ) -> Result<SharerEvent, Error> {
        let (kind, body) = open_message(&self.keys, message)?;
        match kind {
            REQUEST => self.request(body, now, rng),
            ACK => self.ack(&body),
            _ => Err(Error::Unexpected),
        }
    }

    fn request<R: TryCryptoRng + ?Sized>(
        &mut self,
        body: Vec<u8>,
        now: u64,
        rng: &mut R,
    ) -> Result<SharerEvent, Error> {
        let request = Request::decode(&body)?;
        if request.not_after < now || request.not_after > now.saturating_add(MAX_NOT_AFTER_MS) {
            return Err(Error::Expired);
        }
        libcrux_kem::PublicKey::decode(XWingKemDraft06, request.public_key)
            .map_err(|_| Error::InvalidPublicKey)?;
        self.seen.retain(|(_, not_after)| *not_after >= now);
        if self.seen.iter().any(|(nonce, _)| *nonce == request.nonce)
            || self.viewers().any(|(viewer, _)| viewer == request.viewer)
        {
            return Err(Error::Replay);
        }
        if self.seen.len() >= MAX_SEEN
            || self.pending.len() >= MAX_PENDING
            || self.pending.len() + self.viewers.len() >= MAX_VIEWERS
        {
            return Err(Error::LimitReached);
        }
        self.seen.push((request.nonce, request.not_after));
        let viewer = request.viewer;
        let binding = match &request.binding {
            None => None,
            Some(binding) => match self.credentials.resolve(binding)? {
                Ok(binding) => Some(binding),
                Err(missing) => {
                    let mut reply = viewer.to_vec();
                    reply.push(u8::try_from(missing.len()).expect("at most 4"));
                    reply.extend(missing.iter().flatten());
                    let message = seal_message(&self.keys, NEED_CREDENTIALS, &reply, rng)?;
                    return Ok(SharerEvent::NeedCredentials { viewer, message });
                }
            },
        };
        let not_after = request.not_after;
        self.pending.push(Pending {
            viewer,
            body,
            not_after,
        });
        Ok(SharerEvent::Requested { viewer, binding })
    }

    fn ack(&mut self, body: &[u8]) -> Result<SharerEvent, Error> {
        let mut reader = Reader::new(body);
        let viewer: ViewerId = reader.array()?;
        let confirm: [u8; 32] = reader.array()?;
        reader.finish()?;
        let granted = self
            .viewers
            .iter_mut()
            .find(|granted| granted.viewer == viewer)
            .ok_or(Error::UnknownViewer)?;
        if granted.confirmed {
            return Err(Error::Replay);
        }
        if !bool::from(granted.confirm_viewer.ct_eq(&confirm)) {
            return Err(Error::Authentication);
        }
        granted.confirmed = true;
        Ok(SharerEvent::Confirmed(viewer))
    }

    /// Turns down a request or removes a viewer; the next [`Sharer::rekey`] locks it
    /// out. Returns whether it was known.
    pub fn remove(&mut self, viewer: &ViewerId) -> bool {
        let before = self.pending.len() + self.viewers.len();
        self.pending.retain(|pending| pending.viewer != *viewer);
        self.viewers.retain(|granted| granted.viewer != *viewer);
        before != self.pending.len() + self.viewers.len()
    }

    /// Starts the next epoch: a Rekey for every viewer, then a KeyGrant for every queued
    /// request, each with the viewer it is for. Viewers whose confirmation is overdue
    /// and expired requests are dropped first.
    pub fn rekey<R: TryCryptoRng + ?Sized>(
        &mut self,
        now: u64,
        rng: &mut R,
    ) -> Result<Vec<(ViewerId, Vec<u8>)>, Error> {
        if now < self.epoch_started.saturating_add(MIN_EPOCH_INTERVAL_MS) {
            return Err(Error::TooSoon);
        }
        let epoch = self.epoch.epoch().checked_add(1).ok_or(Error::Exhausted)?;
        let key = EpochKey::random(epoch, rng)?;
        self.viewers
            .retain(|granted| granted.confirmed || granted.not_after >= now);
        self.pending.retain(|pending| pending.not_after >= now);

        let mut grants = Vec::with_capacity(self.pending.len());
        let mut granted = Vec::with_capacity(self.pending.len());
        for pending in &self.pending {
            let (message, viewer) = self.grant(pending, &key, rng)?;
            grants.push((pending.viewer, message));
            granted.push(viewer);
        }
        let mut messages = Vec::with_capacity(self.viewers.len() + grants.len());
        let plaintext = key.encode();
        for viewer in &mut self.viewers {
            viewer.counter = viewer.counter.checked_add(1).ok_or(Error::Exhausted)?;
            let header = rekey_header(&viewer.viewer, viewer.counter);
            let mut sealed = [0; SEALED_KEY_LEN];
            libcrux_chacha20poly1305::encrypt(
                viewer.channel.expose(),
                &plaintext,
                &mut sealed,
                &header,
                &channel_nonce(viewer.counter),
            )
            .expect("buffer sized for key and tag");
            let body = [&header[..], &sealed].concat();
            messages.push((viewer.viewer, seal_message(&self.keys, REKEY, &body, rng)?));
        }
        messages.extend(grants);
        self.pending.clear();
        self.viewers.extend(granted);
        self.epoch = key;
        self.epoch_started = now;
        Ok(messages)
    }

    fn grant<R: TryCryptoRng + ?Sized>(
        &self,
        pending: &Pending,
        key: &EpochKey,
        rng: &mut R,
    ) -> Result<(Vec<u8>, Granted), Error> {
        let request = Request::decode(&pending.body)?;
        let sharer_nonce: [u8; 32] = random(rng)?;
        let transcript = transcript(&pending.body, &sharer_nonce);
        let (enc, mut context) = hpke(rng)?
            .setup_sender(
                &HpkePublicKey::new(request.public_key.to_vec()),
                &hpke_info(&transcript),
                Some(self.keys.psk.expose()),
                Some(&self.keys.psk_id()),
                None,
            )
            .map_err(|_| Error::InvalidPublicKey)?;
        let sealed = context
            .seal(&[], &key.encode())
            .map_err(|_| Error::Authentication)?;
        let exports = Exports::new(&context, &transcript)?;
        let body = [
            &pending.viewer[..],
            &sharer_nonce,
            &enc,
            &sealed,
            &exports.confirm_sharer,
        ]
        .concat();
        let granted = Granted {
            viewer: pending.viewer,
            channel: exports.channel,
            counter: 0,
            confirm_viewer: exports.confirm_viewer,
            confirmed: false,
            not_after: pending.not_after,
        };
        Ok((seal_message(&self.keys, GRANT, &body, rng)?, granted))
    }
}

// -------------------------------------------------------------------------------------
// Viewer

#[derive(Debug)]
pub enum ViewerEvent {
    /// The first key; send `ack` to the sharer.
    Granted {
        key: EpochKey,
        ack: Vec<u8>,
    },
    Rekeyed(EpochKey),
    /// The sharer lacks these credentials: join again with them inline.
    NeedCredentials(Vec<Fingerprint>),
}

/// One join: a fresh X-Wing key pair, used for this request only.
pub struct Viewer {
    keys: LinkKeys,
    id: ViewerId,
    request: Vec<u8>,
    not_after: u64,
    kem_seed: Secret,
    hpke: Hpke<HpkeLibcrux>,
    channel: Option<(Secret, u64)>,
    epoch: u32,
}

impl Viewer {
    /// Returns the viewer and its KeyRequest, valid for [`REQUEST_LIFETIME_MS`].
    pub fn join<R: TryCryptoRng + ?Sized>(
        keys: LinkKeys,
        binding: Option<&Binding>,
        now: u64,
        rng: &mut R,
    ) -> Result<(Self, Vec<u8>), Error> {
        let id: ViewerId = random(rng)?;
        let nonce: [u8; 32] = random(rng)?;
        let kem_seed = Secret::random(rng)?;
        let (_, public_key) = libcrux_kem::key_gen_derand(XWingKemDraft06, kem_seed.expose())
            .map_err(|_| Error::Randomness)?;
        let not_after = now.saturating_add(REQUEST_LIFETIME_MS);
        let mut request = [
            &id[..],
            &nonce,
            &not_after.to_be_bytes(),
            &public_key.encode(),
        ]
        .concat();
        let mut encoded_binding = Vec::new();
        if let Some(binding) = binding {
            binding.encode(&mut encoded_binding)?;
        }
        put_opaque16(&mut request, &encoded_binding)?;
        let message = seal_message(&keys, REQUEST, &request, rng)?;
        let viewer = Self {
            hpke: hpke(rng)?,
            keys,
            id,
            request,
            not_after,
            kem_seed,
            channel: None,
            epoch: 0,
        };
        Ok((viewer, message))
    }

    pub fn id(&self) -> ViewerId {
        self.id
    }

    /// Handles a KeyGrant, Rekey or NeedCredentials. Messages for other viewers fail
    /// with [`Error::UnknownViewer`].
    pub fn receive<R: TryCryptoRng + ?Sized>(
        &mut self,
        message: &[u8],
        now: u64,
        rng: &mut R,
    ) -> Result<ViewerEvent, Error> {
        let (kind, body) = open_message(&self.keys, message)?;
        let mut reader = Reader::new(&body);
        if reader.array::<16>()? != self.id {
            return Err(Error::UnknownViewer);
        }
        match (kind, self.channel.is_some()) {
            (GRANT, false) => self.grant(reader, now, rng),
            (REKEY, true) => self.rekey(reader),
            (NEED_CREDENTIALS, false) => {
                let count = usize::from(reader.u8()?);
                if count == 0 || count > Binding::MAX_CREDENTIALS {
                    return Err(Error::Malformed("credential count"));
                }
                let missing = (0..count)
                    .map(|_| reader.array())
                    .collect::<Result<_, _>>()?;
                reader.finish()?;
                Ok(ViewerEvent::NeedCredentials(missing))
            }
            _ => Err(Error::Unexpected),
        }
    }

    fn grant<R: TryCryptoRng + ?Sized>(
        &mut self,
        mut reader: Reader<'_>,
        now: u64,
        rng: &mut R,
    ) -> Result<ViewerEvent, Error> {
        let sharer_nonce = reader.array()?;
        let enc = reader.take(ENC_LEN)?;
        let sealed = reader.take(SEALED_KEY_LEN)?;
        let confirm: [u8; 32] = reader.array()?;
        reader.finish()?;
        if now > self.not_after {
            return Err(Error::Expired);
        }
        let transcript = transcript(&self.request, &sharer_nonce);
        let mut context = self
            .hpke
            .setup_receiver(
                enc,
                &HpkePrivateKey::new(self.kem_seed.expose().to_vec()),
                &hpke_info(&transcript),
                Some(self.keys.psk.expose()),
                Some(&self.keys.psk_id()),
                None,
            )
            .map_err(|_| Error::Authentication)?;
        let plaintext = Zeroizing::new(
            context
                .open(&[], sealed)
                .map_err(|_| Error::Authentication)?,
        );
        let exports = Exports::new(&context, &transcript)?;
        if !bool::from(exports.confirm_sharer.ct_eq(&confirm)) {
            return Err(Error::Authentication);
        }
        let key = EpochKey::decode(&plaintext)?;
        let ack = [&self.id[..], &exports.confirm_viewer].concat();
        let ack = seal_message(&self.keys, ACK, &ack, rng)?;
        self.channel = Some((exports.channel, 0));
        self.epoch = key.epoch();
        Ok(ViewerEvent::Granted { key, ack })
    }

    fn rekey(&mut self, mut reader: Reader<'_>) -> Result<ViewerEvent, Error> {
        let counter = reader.u64()?;
        let sealed = reader.take(SEALED_KEY_LEN)?;
        reader.finish()?;
        let (channel, last) = self.channel.as_mut().expect("checked by receive");
        if counter <= *last {
            return Err(Error::Replay);
        }
        let mut plaintext = Zeroizing::new(vec![0; EpochKey::LEN]);
        libcrux_chacha20poly1305::decrypt(
            channel.expose(),
            &mut plaintext,
            sealed,
            &rekey_header(&self.id, counter),
            &channel_nonce(counter),
        )
        .map_err(|_| Error::Authentication)?;
        let key = EpochKey::decode(&plaintext)?;
        if key.epoch() <= self.epoch {
            return Err(Error::StaleEpoch);
        }
        *last = counter;
        self.epoch = key.epoch();
        Ok(ViewerEvent::Rekeyed(key))
    }
}

impl std::fmt::Debug for Viewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewer")
            .field("id", &self.id)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Sharer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sharer")
            .field("epoch", &self.epoch.epoch())
            .field("pending", &self.pending.len())
            .field("viewers", &self.viewers.len())
            .finish_non_exhaustive()
    }
}
