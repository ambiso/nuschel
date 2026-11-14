// SPDX-License-Identifier: EUPL-1.2
//! The sharer's and the viewers' key-exchange state machines (wire format in the crate
//! documentation). The sharer grants only fresh epochs: [`Sharer::rekey`] moves every
//! viewer to K_{e+1} and grants K_{e+1} to the approved requests, so a newcomer receives
//! only keys created after it joined.

use std::fmt;

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Context, Hpke, HpkePrivateKey, Mode};
use hpke_rs_libcrux::HpkeLibcrux;
use libcrux_kem::Algorithm::XWingKemDraft06;
use rand_core::{TryCryptoRng, TryRng};
use subtle::ConstantTimeEq as _;
use zeroize::{Zeroize as _, Zeroizing};

use crate::credentials::{Binding, CredentialCache, Fingerprint, VerifiableBinding};
use crate::link::{LinkKeys, label};
use crate::wire::{Reader, put_opaque16};
use crate::{Error, MAX_KEY_MESSAGE, Secret, hkdf_expand, random, sha256};

const VERSION: u8 = 1;
/// KEM X-Wing, KDF HKDF-SHA256, AEAD ChaCha20-Poly1305, SFrame AES_128_GCM_SHA256_128.
const SUITE: [u8; 8] = [0x64, 0x7a, 0x00, 0x01, 0x00, 0x03, 0x00, 0x04];
const PUBLIC_KEY_LEN: usize = 1216;
const ENC_LEN: usize = 1120;
const AEAD_TAG_LEN: usize = 16;
const EPOCH_KEY_LEN: usize = 4 + 32;
const SEALED_KEY_LEN: usize = EPOCH_KEY_LEN + AEAD_TAG_LEN;

/// How far ahead a request's `not_after` may be: its lifetime plus a minute of clock
/// difference between viewer and sharer.
pub const REQUEST_LIFETIME_MS: u64 = 60_000;
const MAX_NOT_AFTER_MS: u64 = 2 * REQUEST_LIFETIME_MS;
/// The shortest time between two epochs, unless the clock stepped back.
pub const MIN_EPOCH_INTERVAL_MS: u64 = 1_000;
/// When to send a rekey's messages a second time.
pub const RESEND_AFTER_MS: u64 = 500;
/// When to switch the encryptors to a new epoch, counted from the first send.
pub const SWITCH_AFTER_MS: u64 = 1_000;
/// Requests waiting for the next rekey.
pub const MAX_PENDING: usize = 32;
/// Viewers holding keys, plus pending requests.
pub const MAX_VIEWERS: usize = 256;
/// Request nonces remembered until their `not_after`.
pub const MAX_SEEN: usize = 256;
/// Rekeys a viewer may have missed and still catch up. Further behind, every Rekey
/// fails with [`Error::LimitReached`] and the viewer has to join again.
pub const MAX_MISSED_REKEYS: u64 = 64;

pub(crate) const REQUEST: u8 = 1;
pub(crate) const GRANT: u8 = 2;
pub(crate) const ACK: u8 = 3;
pub(crate) const REKEY: u8 = 4;
pub(crate) const NEED_CREDENTIALS: u8 = 5;

pub type ViewerId = [u8; 16];

// -------------------------------------------------------------------------------------
// Epoch keys

/// The sharer's key for one epoch; [`Sharer::rekey`] returns it once. Hand it to one
/// `nuschl_sframe::SendKey`: a second set of encryptors for the same epoch repeats
/// AES-GCM nonces. The key authority never sends an epoch's key to a restarted sender
/// (a new encoder, track or worker process); it rekeys, and the new sender gets the new
/// epoch's key.
pub struct SendKey {
    epoch: u32,
    key: Secret,
}

impl SendKey {
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn into_parts(self) -> (u32, Zeroizing<[u8; 32]>) {
        (self.epoch, self.key.into_inner())
    }
}

/// A viewer's key for one epoch, for decryption.
pub struct ReceiveKey {
    epoch: u32,
    key: Secret,
}

impl ReceiveKey {
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn expose(&self) -> &[u8; 32] {
        self.key.expose()
    }
}

impl fmt::Debug for SendKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SendKey(epoch {})", self.epoch)
    }
}

impl fmt::Debug for ReceiveKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReceiveKey(epoch {})", self.epoch)
    }
}

fn encode_epoch_key(epoch: u32, key: &Secret) -> Zeroizing<Vec<u8>> {
    Zeroizing::new([&epoch.to_be_bytes()[..], key.expose()].concat())
}

fn decode_epoch_key(bytes: &[u8]) -> Result<ReceiveKey, Error> {
    let mut reader = Reader::new(bytes);
    let epoch = reader.u32()?;
    let key = Secret(Zeroizing::new(reader.array()?));
    reader.finish()?;
    Ok(ReceiveKey { epoch, key })
}

// -------------------------------------------------------------------------------------
// Envelope, bodies and HPKE

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

fn binding_digest(
    keys: &LinkKeys,
    viewer: &ViewerId,
    nonce: &[u8; 32],
    not_after: u64,
    public_key: &[u8],
) -> [u8; 32] {
    sha256(&[
        label("binding").as_bytes(),
        keys.stream_id(),
        viewer,
        nonce,
        &not_after.to_be_bytes(),
        public_key,
    ])
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

fn hpke_info(transcript: &[u8; 32]) -> Vec<u8> {
    [label("grant").as_bytes(), transcript].concat()
}

/// hpke-rs needs a PRNG type. This crate draws X-Wing's encapsulation randomness from
/// the caller's RNG itself, and the receiver and the key schedule draw none.
pub(crate) struct NoRandomness;

impl TryRng for NoRandomness {
    type Error = fmt::Error;

    fn try_next_u32(&mut self) -> Result<u32, fmt::Error> {
        Err(fmt::Error)
    }

    fn try_next_u64(&mut self) -> Result<u64, fmt::Error> {
        Err(fmt::Error)
    }

    fn try_fill_bytes(&mut self, _: &mut [u8]) -> Result<(), fmt::Error> {
        Err(fmt::Error)
    }
}

impl TryCryptoRng for NoRandomness {}

pub(crate) type HpkeContext = Context<HpkeLibcrux<NoRandomness>>;
/// `(psk, psk_id)` selects PSK mode; the KAT uses base mode.
pub(crate) type Psk<'a> = Option<(&'a [u8], &'a [u8])>;

fn hpke(psk: Psk<'_>) -> Hpke<HpkeLibcrux<NoRandomness>> {
    let mode = if psk.is_some() { Mode::Psk } else { Mode::Base };
    Hpke::new_with_rng(
        mode,
        KemAlgorithm::XWingDraft06,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
        NoRandomness,
    )
}

/// X-Wing EncapsulateDerand with `eseed`, then the HPKE key schedule: what hpke-rs's
/// `setup_sender` does, with the caller's randomness. Returns `enc` and the context.
pub(crate) fn hpke_sender(
    public_key: &[u8],
    eseed: &[u8; 64],
    info: &[u8],
    psk: Psk<'_>,
) -> Result<(Vec<u8>, HpkeContext), Error> {
    let public = libcrux_kem::PublicKey::decode(XWingKemDraft06, public_key)
        .map_err(|_| Error::InvalidPublicKey)?;
    let (shared, enc) = public
        .encapsulate_derand(eseed)
        .map_err(|_| Error::InvalidPublicKey)?;
    let shared = Zeroizing::new(shared.encode());
    let (key, id) = psk.unwrap_or_default();
    let context = hpke(psk)
        .key_schedule(&shared, info, key, id)
        .map_err(|_| Error::InvalidPublicKey)?;
    Ok((enc.encode(), context))
}

pub(crate) fn hpke_receiver(
    kem_seed: &[u8; 32],
    enc: &[u8],
    info: &[u8],
    psk: Psk<'_>,
) -> Result<HpkeContext, Error> {
    // hpke-rs zeroizes its private key type on drop.
    let private = HpkePrivateKey::new(kem_seed.to_vec());
    hpke(psk)
        .setup_receiver(
            enc,
            &private,
            info,
            psk.map(|p| p.0),
            psk.map(|p| p.1),
            None,
        )
        .map_err(|_| Error::Authentication)
}

fn export(context: &HpkeContext, name: &str) -> Result<Secret, Error> {
    let bytes = Zeroizing::new(
        context
            .export(label(name).as_bytes(), 32)
            .map_err(|_| Error::Authentication)?,
    );
    Ok(Secret(Zeroizing::new(
        bytes[..].try_into().expect("32 bytes"),
    )))
}

/// The next channel key; the caller drops the current one.
fn ratchet(channel: &Secret) -> Secret {
    Secret(hkdf_expand(
        channel.expose(),
        label("channel ratchet").as_bytes(),
    ))
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
    /// Waiting for [`Sharer::approve`].
    Requested,
    /// Waiting for the next rekey.
    Approved,
    /// Holds the current epoch; its key confirmation has not arrived.
    Granted,
    Confirmed,
}

#[derive(Debug)]
pub enum SharerEvent {
    /// A viewer asked to join. Verify the binding against `digest`, then call
    /// [`Sharer::approve`] with both; without a binding policy, approve at once.
    Requested {
        viewer: ViewerId,
        /// This request's [`JoinDraft::binding_digest`], which covers pk_v.
        digest: [u8; 32],
        binding: Option<VerifiableBinding>,
    },
    /// The request names credentials this sharer lacks; send `message` to the viewer.
    NeedCredentials {
        viewer: ViewerId,
        message: Vec<u8>,
    },
    Confirmed(ViewerId),
}

/// What [`Sharer::rekey`] produces: send `messages`, each to its viewer, then switch to
/// `key` as the crate documentation describes.
#[derive(Debug)]
pub struct Rekey {
    pub key: SendKey,
    pub messages: Vec<(ViewerId, Vec<u8>)>,
}

struct Pending {
    viewer: ViewerId,
    digest: [u8; 32],
    body: Vec<u8>,
    not_after: u64,
    approved: bool,
}

struct Granted {
    viewer: ViewerId,
    /// The key for Rekey number `counter + 1`.
    channel: Secret,
    counter: u64,
    confirm: Secret,
    confirmed: bool,
    /// The request's `not_after`: an unconfirmed viewer is dropped at the next rekey
    /// after it.
    not_after: u64,
}

pub struct Sharer {
    keys: LinkKeys,
    epoch: u32,
    last_rekey: Option<u64>,
    pending: Vec<Pending>,
    viewers: Vec<Granted>,
    seen: Vec<([u8; 32], u64)>,
    credentials: CredentialCache,
}

impl Sharer {
    /// A sharer without an epoch; the first [`Sharer::rekey`] creates epoch 1.
    pub fn new(keys: LinkKeys, credentials: CredentialCache) -> Self {
        Self {
            keys,
            epoch: 0,
            last_rekey: None,
            pending: Vec::new(),
            viewers: Vec::new(),
            seen: Vec::new(),
            credentials,
        }
    }

    pub fn viewers(&self) -> impl Iterator<Item = (ViewerId, ViewerState)> + '_ {
        let pending = self.pending.iter().map(|p| {
            let state = if p.approved {
                ViewerState::Approved
            } else {
                ViewerState::Requested
            };
            (p.viewer, state)
        });
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
        let digest = binding_digest(
            &self.keys,
            &viewer,
            &request.nonce,
            request.not_after,
            request.public_key,
        );
        let binding = match &request.binding {
            None => None,
            Some(binding) => match self.credentials.resolve(binding, digest)? {
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
            digest,
            body,
            not_after,
            approved: false,
        });
        Ok(SharerEvent::Requested {
            viewer,
            digest,
            binding,
        })
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
        if !bool::from(granted.confirm.expose().ct_eq(&confirm)) {
            return Err(Error::Authentication);
        }
        granted.confirmed = true;
        Ok(SharerEvent::Confirmed(viewer))
    }

    /// Lets the next [`Sharer::rekey`] grant the request `Requested` reported with
    /// this viewer id and digest. A later request that reuses the viewer id has another
    /// digest and stays unapproved.
    pub fn approve(&mut self, viewer: &ViewerId, digest: &[u8; 32]) -> Result<(), Error> {
        let pending = self
            .pending
            .iter_mut()
            .find(|pending| pending.viewer == *viewer && pending.digest == *digest)
            .ok_or(Error::UnknownViewer)?;
        pending.approved = true;
        Ok(())
    }

    /// Turns down a request or removes a viewer; the next [`Sharer::rekey`] locks it
    /// out. Returns whether it was known.
    pub fn remove(&mut self, viewer: &ViewerId) -> bool {
        let before = self.pending.len() + self.viewers.len();
        self.pending.retain(|pending| pending.viewer != *viewer);
        self.viewers.retain(|granted| granted.viewer != *viewer);
        before != self.pending.len() + self.viewers.len()
    }

    /// Starts the next epoch: a Rekey for every viewer, then a KeyGrant for every
    /// approved request. Viewers whose confirmation is overdue and expired requests are
    /// dropped first; unapproved requests wait. Refused within
    /// [`MIN_EPOCH_INTERVAL_MS`] of the last rekey, unless the clock stepped back.
    pub fn rekey<R: TryCryptoRng + ?Sized>(
        &mut self,
        now: u64,
        rng: &mut R,
    ) -> Result<Rekey, Error> {
        if let Some(last) = self.last_rekey
            && (last..last.saturating_add(MIN_EPOCH_INTERVAL_MS)).contains(&now)
        {
            return Err(Error::TooSoon);
        }
        let epoch = self.epoch.checked_add(1).ok_or(Error::Exhausted)?;
        let key = Secret::random(rng)?;
        self.viewers
            .retain(|granted| granted.confirmed || granted.not_after >= now);
        self.pending.retain(|pending| pending.not_after >= now);
        if self
            .viewers
            .iter()
            .any(|granted| granted.counter == u64::MAX)
        {
            return Err(Error::Exhausted);
        }

        let plaintext = encode_epoch_key(epoch, &key);
        let mut grants = Vec::new();
        let mut granted = Vec::new();
        for pending in self.pending.iter().filter(|pending| pending.approved) {
            let (message, viewer) = self.grant(pending, &plaintext, rng)?;
            grants.push((pending.viewer, message));
            granted.push(viewer);
        }
        let mut messages = Vec::with_capacity(self.viewers.len() + grants.len());
        for viewer in &mut self.viewers {
            viewer.counter += 1;
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
            viewer.channel = ratchet(&viewer.channel);
            let body = [&header[..], &sealed].concat();
            messages.push((viewer.viewer, seal_message(&self.keys, REKEY, &body, rng)?));
        }
        messages.extend(grants);
        self.pending.retain(|pending| !pending.approved);
        self.viewers.extend(granted);
        self.epoch = epoch;
        self.last_rekey = Some(now);
        Ok(Rekey {
            key: SendKey { epoch, key },
            messages,
        })
    }

    fn grant<R: TryCryptoRng + ?Sized>(
        &self,
        pending: &Pending,
        epoch_key: &[u8],
        rng: &mut R,
    ) -> Result<(Vec<u8>, Granted), Error> {
        let request = Request::decode(&pending.body)?;
        let sharer_nonce: [u8; 32] = random(rng)?;
        let eseed = Zeroizing::new(random::<64, R>(rng)?);
        let transcript = transcript(&pending.body, &sharer_nonce);
        let psk_id = self.keys.psk_id();
        let (enc, mut context) = hpke_sender(
            request.public_key,
            &eseed,
            &hpke_info(&transcript),
            Some((self.keys.psk.expose(), &psk_id)),
        )?;
        let sealed = context
            .seal(&[], epoch_key)
            .map_err(|_| Error::Authentication)?;
        let body = [&pending.viewer[..], &sharer_nonce, &enc, &sealed].concat();
        let granted = Granted {
            viewer: pending.viewer,
            channel: export(&context, "channel sharer->viewer")?,
            counter: 0,
            confirm: export(&context, "confirm viewer->sharer")?,
            confirmed: false,
            not_after: pending.not_after,
        };
        Ok((seal_message(&self.keys, GRANT, &body, rng)?, granted))
    }
}

impl fmt::Debug for Sharer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sharer")
            .field("epoch", &self.epoch)
            .field("pending", &self.pending.len())
            .field("viewers", &self.viewers.len())
            .finish_non_exhaustive()
    }
}

// -------------------------------------------------------------------------------------
// Viewer

#[derive(Debug)]
pub enum ViewerEvent {
    /// The first key; send `ack` to the sharer.
    Granted {
        key: ReceiveKey,
        ack: Vec<u8>,
    },
    Rekeyed(ReceiveKey),
    /// The sharer lacks these credentials: join again with them inline.
    NeedCredentials(Vec<Fingerprint>),
}

/// A join before its request is sealed: the identity layer signs
/// [`JoinDraft::binding_digest`], which covers pk_v.
pub struct JoinDraft {
    keys: LinkKeys,
    id: ViewerId,
    nonce: [u8; 32],
    not_after: u64,
    kem_seed: Secret,
    public_key: Vec<u8>,
}

impl JoinDraft {
    pub fn binding_digest(&self) -> [u8; 32] {
        binding_digest(
            &self.keys,
            &self.id,
            &self.nonce,
            self.not_after,
            &self.public_key,
        )
    }

    /// Seals the KeyRequest, valid for [`REQUEST_LIFETIME_MS`].
    pub fn finish<R: TryCryptoRng + ?Sized>(
        self,
        binding: Option<&Binding>,
        rng: &mut R,
    ) -> Result<(Viewer, Vec<u8>), Error> {
        let mut request = [
            &self.id[..],
            &self.nonce,
            &self.not_after.to_be_bytes(),
            &self.public_key,
        ]
        .concat();
        let mut encoded_binding = Vec::new();
        if let Some(binding) = binding {
            binding.encode(&mut encoded_binding)?;
        }
        put_opaque16(&mut request, &encoded_binding)?;
        let message = seal_message(&self.keys, REQUEST, &request, rng)?;
        let named = binding.map(|b| b.fingerprints.clone()).unwrap_or_default();
        let viewer = Viewer {
            keys: self.keys,
            id: self.id,
            state: State::Joining {
                request,
                not_after: self.not_after,
                kem_seed: self.kem_seed,
                named,
            },
        };
        Ok((viewer, message))
    }
}

enum State {
    Joining {
        request: Vec<u8>,
        not_after: u64,
        kem_seed: Secret,
        named: Vec<Fingerprint>,
    },
    /// Holds the key for Rekey number `counter + 1`, and nothing that opens the grant.
    Granted {
        channel: Secret,
        counter: u64,
        epoch: u32,
    },
    /// The sharer asked for credentials; this join is over.
    Refused,
}

/// One join: a fresh X-Wing key pair, used for this request only.
pub struct Viewer {
    keys: LinkKeys,
    id: ViewerId,
    state: State,
}

impl Viewer {
    /// Draws the viewer id, nonce and X-Wing key pair.
    pub fn prepare<R: TryCryptoRng + ?Sized>(
        keys: LinkKeys,
        now: u64,
        rng: &mut R,
    ) -> Result<JoinDraft, Error> {
        let id = random(rng)?;
        let nonce = random(rng)?;
        let kem_seed = Secret::random(rng)?;
        let (private, public) = libcrux_kem::key_gen_derand(XWingKemDraft06, kem_seed.expose())
            .map_err(|_| Error::Randomness)?;
        if let libcrux_kem::PrivateKey::XWingKemDraft06(mut private) = private {
            private.seed.zeroize();
        }
        Ok(JoinDraft {
            keys,
            id,
            nonce,
            not_after: now.saturating_add(REQUEST_LIFETIME_MS),
            kem_seed,
            public_key: public.encode(),
        })
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
        match (kind, &self.state) {
            (GRANT, State::Joining { .. }) => self.grant(reader, now, rng),
            (REKEY, State::Granted { .. }) => self.rekey(reader),
            (NEED_CREDENTIALS, State::Joining { named, .. }) => {
                let count = usize::from(reader.u8()?);
                let missing: Vec<Fingerprint> = (0..count)
                    .map(|_| reader.array())
                    .collect::<Result<_, _>>()?;
                reader.finish()?;
                let named_once = missing
                    .iter()
                    .enumerate()
                    .all(|(i, f)| named.contains(f) && !missing[..i].contains(f));
                if missing.is_empty() || !named_once {
                    return Err(Error::Unexpected);
                }
                self.state = State::Refused;
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
        let State::Joining {
            request,
            not_after,
            kem_seed,
            ..
        } = &self.state
        else {
            return Err(Error::Unexpected);
        };
        let sharer_nonce = reader.array()?;
        let enc = reader.take(ENC_LEN)?;
        let sealed = reader.take(SEALED_KEY_LEN)?;
        reader.finish()?;
        if now > *not_after {
            return Err(Error::Expired);
        }
        let transcript = transcript(request, &sharer_nonce);
        let psk_id = self.keys.psk_id();
        let mut context = hpke_receiver(
            kem_seed.expose(),
            enc,
            &hpke_info(&transcript),
            Some((self.keys.psk.expose(), &psk_id)),
        )?;
        let plaintext = Zeroizing::new(
            context
                .open(&[], sealed)
                .map_err(|_| Error::Authentication)?,
        );
        let key = decode_epoch_key(&plaintext)?;
        let confirm = export(&context, "confirm viewer->sharer")?;
        let ack = [&self.id[..], confirm.expose()].concat();
        let ack = seal_message(&self.keys, ACK, &ack, rng)?;
        self.state = State::Granted {
            channel: export(&context, "channel sharer->viewer")?,
            counter: 0,
            epoch: key.epoch,
        };
        Ok(ViewerEvent::Granted { key, ack })
    }

    fn rekey(&mut self, mut reader: Reader<'_>) -> Result<ViewerEvent, Error> {
        let State::Granted {
            channel,
            counter: last,
            epoch,
        } = &mut self.state
        else {
            return Err(Error::Unexpected);
        };
        let counter = reader.u64()?;
        let sealed = reader.take(SEALED_KEY_LEN)?;
        reader.finish()?;
        if counter <= *last {
            return Err(Error::Replay);
        }
        if counter - *last - 1 > MAX_MISSED_REKEYS {
            return Err(Error::LimitReached);
        }
        let mut skipped = None;
        for _ in *last + 1..counter {
            skipped = Some(ratchet(skipped.as_ref().unwrap_or(channel)));
        }
        let key = skipped.as_ref().unwrap_or(channel);
        let mut plaintext = Zeroizing::new(vec![0; EPOCH_KEY_LEN]);
        libcrux_chacha20poly1305::decrypt(
            key.expose(),
            &mut plaintext,
            sealed,
            &rekey_header(&self.id, counter),
            &channel_nonce(counter),
        )
        .map_err(|_| Error::Authentication)?;
        let received = decode_epoch_key(&plaintext)?;
        if received.epoch <= *epoch {
            return Err(Error::StaleEpoch);
        }
        let next = ratchet(key);
        *channel = next;
        *last = counter;
        *epoch = received.epoch;
        Ok(ViewerEvent::Rekeyed(received))
    }
}

impl fmt::Debug for Viewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Viewer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{T0, join, link, rng, sharer};

    #[test]
    fn epochs_and_counters_are_bounded() {
        let link = link(40);
        let mut sharer = sharer(&link);
        sharer.epoch = u32::MAX;
        assert_eq!(sharer.rekey(T0, &mut rng(1)).err(), Some(Error::Exhausted));

        let mut sharer = self::sharer(&link);
        join(&link, &mut sharer, T0, 41);
        sharer.viewers[0].counter = u64::MAX;
        assert_eq!(
            sharer.rekey(T0 + 1000, &mut rng(2)).err(),
            Some(Error::Exhausted)
        );
    }

    /// L2: after a Rekey both sides hold only the next channel key, which opens no
    /// earlier Rekey, and the viewer keeps nothing that opens its grant.
    #[test]
    fn channel_keys_ratchet_forward() {
        let link = link(42);
        let keys = link.keys();
        let mut sharer = sharer(&link);
        let (mut viewer, _, _) = join(&link, &mut sharer, T0, 43);
        let mut rng = rng(44);
        let mut sealed = Vec::new();
        for now in [T0 + 1000, T0 + 2000] {
            let message = sharer.rekey(now, &mut rng).unwrap().messages.remove(0).1;
            assert!(viewer.receive(&message, now, &mut rng).is_ok());
            let (_, body) = open_message(&keys, &message).unwrap();
            sealed.push((
                u64::from_be_bytes(body[16..24].try_into().unwrap()),
                body[24..].to_vec(),
            ));
        }
        let State::Granted { channel, .. } = &viewer.state else {
            panic!("granted");
        };
        assert_eq!(channel.expose(), sharer.viewers[0].channel.expose());
        for (counter, sealed) in &sealed {
            let mut plaintext = [0; EPOCH_KEY_LEN];
            let opened = libcrux_chacha20poly1305::decrypt(
                channel.expose(),
                &mut plaintext,
                sealed,
                &rekey_header(&viewer.id, *counter),
                &channel_nonce(*counter),
            );
            assert!(opened.is_err(), "counter {counter}");
        }
    }
}
