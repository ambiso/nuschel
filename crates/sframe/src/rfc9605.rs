// SPDX-License-Identifier: EUPL-1.2
//! RFC 9605 SFrame with cipher suite `AES_128_GCM_SHA256_128` (0x0004), unmodified. Each
//! KID has its own base key; a key is used for sending or receiving, and a sender owns
//! its KID's counter, so no (key, nonce) pair repeats. Receivers reject replays within
//! a 128-frame window.

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{AeadInOut as _, KeyInit as _, Nonce, Tag};
use zeroize::Zeroizing;

use crate::Error;

pub const CIPHER_SUITE: u16 = 0x0004;
/// The largest plaintext frame.
pub const MAX_FRAME_LEN: usize = 1 << 20;
const TAG_LEN: usize = 16;
const MAX_HEADER_LEN: usize = 17;
/// The most KIDs one [`Receiver`] holds.
pub const MAX_RECEIVE_KEYS: usize = 16;

struct KeyAndSalt {
    cipher: Aes128Gcm,
    salt: Zeroizing<[u8; 12]>,
}

impl KeyAndSalt {
    /// RFC 9605 §4.4.2.
    fn derive(base_key: &[u8], kid: u64) -> Self {
        use libcrux_hkdf::Algorithm::Sha256;
        let mut secret = Zeroizing::new([0; 32]);
        libcrux_hkdf::extract(Sha256, &mut secret[..], &[], base_key)
            .expect("inputs are far below the HKDF limits");
        let expand = |name: &[u8], out: &mut [u8]| {
            let info = [name, &kid.to_be_bytes(), &CIPHER_SUITE.to_be_bytes()].concat();
            libcrux_hkdf::expand(Sha256, out, &secret[..], &info).expect("at most 16 bytes");
        };
        let mut key = Zeroizing::new([0; 16]);
        let mut salt = Zeroizing::new([0; 12]);
        expand(b"SFrame 1.0 Secret key ", &mut key[..]);
        expand(b"SFrame 1.0 Secret salt ", &mut salt[..]);
        Self {
            cipher: Aes128Gcm::new(&(*key).into()),
            salt,
        }
    }

    fn nonce(&self, counter: u64) -> Nonce<Aes128Gcm> {
        let mut nonce = *self.salt;
        for (byte, counter) in nonce[4..].iter_mut().zip(counter.to_be_bytes()) {
            *byte ^= counter;
        }
        nonce.into()
    }

    fn encrypt(&self, kid: u64, counter: u64, metadata: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut out = encode_header(kid, counter);
        let header_len = out.len();
        let aad = [&out[..], metadata].concat();
        out.extend_from_slice(plaintext);
        let tag = self
            .cipher
            .encrypt_inout_detached(&self.nonce(counter), &aad, (&mut out[header_len..]).into())
            .expect("frames are far below the AES-GCM limit");
        out.extend_from_slice(&tag);
        out
    }
}

/// Encrypts under one KID, counting from 0.
pub struct Sender {
    key: KeyAndSalt,
    kid: u64,
    counter: u64,
}

impl Sender {
    pub fn new(base_key: &[u8], kid: u64) -> Self {
        Self {
            key: KeyAndSalt::derive(base_key, kid),
            kid,
            counter: 0,
        }
    }

    pub fn kid(&self) -> u64 {
        self.kid
    }

    /// `header || AES-GCM(plaintext)`, with `header || metadata` authenticated.
    pub fn encrypt(&mut self, metadata: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        if plaintext.len() > MAX_FRAME_LEN {
            return Err(Error::TooLong);
        }
        let counter = self.counter;
        self.counter = counter.checked_add(1).ok_or(Error::Exhausted)?;
        Ok(self.key.encrypt(self.kid, counter, metadata, plaintext))
    }
}

/// Decryption keys by KID, at most [`MAX_RECEIVE_KEYS`].
#[derive(Default)]
pub struct Receiver {
    keys: Vec<(u64, KeyAndSalt, ReplayWindow)>,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a key, or replaces the one for `kid` with a fresh replay window.
    pub fn insert(&mut self, kid: u64, base_key: &[u8]) -> Result<(), Error> {
        self.remove(kid);
        if self.keys.len() >= MAX_RECEIVE_KEYS {
            return Err(Error::LimitReached);
        }
        let key = KeyAndSalt::derive(base_key, kid);
        self.keys.push((kid, key, ReplayWindow::default()));
        Ok(())
    }

    pub fn remove(&mut self, kid: u64) {
        self.keys.retain(|(known, ..)| *known != kid);
    }

    /// RFC 9605 §4.4.4, then the replay check. The KID's window advances only after the
    /// tag verifies.
    pub fn decrypt(&mut self, metadata: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        if ciphertext.len() > MAX_HEADER_LEN + MAX_FRAME_LEN + TAG_LEN {
            return Err(Error::TooLong);
        }
        let (kid, counter, header_len) = decode_header(ciphertext)?;
        let (header, body) = ciphertext.split_at(header_len);
        let body_len = body
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(Error::Malformed("sframe tag"))?;
        let (_, key, window) = self
            .keys
            .iter_mut()
            .find(|(known, ..)| *known == kid)
            .ok_or(Error::UnknownKey)?;
        window.check(counter)?;
        let aad = [header, metadata].concat();
        let (encrypted, tag) = body.split_at(body_len);
        let tag = Tag::<Aes128Gcm>::try_from(tag).expect("16 bytes");
        let mut plaintext = encrypted.to_vec();
        key.cipher
            .decrypt_inout_detached(&key.nonce(counter), &aad, (&mut plaintext[..]).into(), &tag)
            .map_err(|_| Error::Authentication)?;
        window.accept(counter);
        Ok(plaintext)
    }
}

/// The highest counter seen and a bitmap of the 127 below it.
#[derive(Default)]
struct ReplayWindow {
    highest: Option<u64>,
    seen: u128,
}

impl ReplayWindow {
    fn check(&self, counter: u64) -> Result<(), Error> {
        let Some(highest) = self.highest else {
            return Ok(());
        };
        if counter > highest {
            return Ok(());
        }
        let age = highest - counter;
        if age >= 128 || self.seen & (1 << age) != 0 {
            return Err(Error::Replay);
        }
        Ok(())
    }

    fn accept(&mut self, counter: u64) {
        match self.highest {
            Some(highest) if counter <= highest => self.seen |= 1 << (highest - counter),
            highest => {
                let shift = highest.map_or(u64::MAX, |highest| counter - highest);
                let shift = u32::try_from(shift).unwrap_or(u32::MAX);
                self.seen = self.seen.checked_shl(shift).unwrap_or(0) | 1;
                self.highest = Some(counter);
            }
        }
    }
}

/// RFC 9605 §4.3: a config byte, then KID and CTR in the fewest big-endian bytes when
/// they are 8 or more.
pub(crate) fn encode_header(kid: u64, counter: u64) -> Vec<u8> {
    let (kid_field, kid_bytes) = compact(kid);
    let (counter_field, counter_bytes) = compact(counter);
    let mut out = Vec::with_capacity(MAX_HEADER_LEN);
    out.push(kid_field << 4 | counter_field);
    out.extend_from_slice(&kid_bytes);
    out.extend_from_slice(&counter_bytes);
    out
}

fn compact(value: u64) -> (u8, Vec<u8>) {
    match u8::try_from(value) {
        Ok(small) if small < 8 => (small, Vec::new()),
        _ => {
            let bytes = value.to_be_bytes();
            let skip = bytes.iter().take_while(|byte| **byte == 0).count();
            let len = u8::try_from(8 - skip).expect("at most 8");
            (0b1000 | (len - 1), bytes[skip..].to_vec())
        }
    }
}

/// Returns (KID, CTR, header length). Only the minimal encoding is accepted.
pub(crate) fn decode_header(data: &[u8]) -> Result<(u64, u64, usize), Error> {
    let config = *data.first().ok_or(Error::Malformed("sframe header"))?;
    let mut offset = 1;
    let mut field = |bits: u8| -> Result<u64, Error> {
        if bits & 0b1000 == 0 {
            return Ok(u64::from(bits));
        }
        let len = usize::from(bits & 0b111) + 1;
        let bytes = data
            .get(offset..offset + len)
            .ok_or(Error::Malformed("sframe header"))?;
        offset += len;
        Ok(bytes
            .iter()
            .fold(0, |value, byte| value << 8 | u64::from(*byte)))
    };
    let kid = field(config >> 4)?;
    let counter = field(config & 0b1111)?;
    if data[..offset] != encode_header(kid, counter)[..] {
        return Err(Error::Malformed("sframe header encoding"));
    }
    Ok((kid, counter, offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::hex;

    #[test]
    fn rfc9605_header_vectors() {
        let vectors = include_str!("../test-vectors/rfc9605-headers.txt");
        let mut count = 0;
        for line in vectors.lines().filter(|line| !line.starts_with('#')) {
            let [kid, counter, header] = line.split(' ').collect::<Vec<_>>()[..] else {
                panic!("bad vector line {line}");
            };
            let kid = u64::from_str_radix(kid, 16).unwrap();
            let counter = u64::from_str_radix(counter, 16).unwrap();
            let header = hex(header);
            assert_eq!(encode_header(kid, counter), header);
            assert_eq!(decode_header(&header), Ok((kid, counter, header.len())));
            count += 1;
        }
        assert_eq!(count, 289);
    }

    /// RFC 9605 Appendix C.3, cipher suite 0x0004.
    #[test]
    fn rfc9605_encryption_vector() {
        let base_key = hex("000102030405060708090a0b0c0d0e0f");
        let metadata = hex("4945544620534672616d65205747");
        let plaintext = hex("64726166742d696574662d736672616d652d656e63");
        let ciphertext = hex(concat!(
            "9901234567b7412c2513a1b66dbb48841bbaf17f598751176ad847681a69c6d0",
            "b091c07018ce4adb34eb"
        ));
        let key = KeyAndSalt::derive(&base_key, 0x123);
        assert_eq!(key.salt.to_vec(), hex("75234edefe07819026751816"));
        assert_eq!(key.nonce(0x4567).to_vec(), hex("75234edefe07819026755d71"));
        assert_eq!(
            key.encrypt(0x123, 0x4567, &metadata, &plaintext),
            ciphertext
        );

        let mut receiver = Receiver::new();
        receiver.insert(0x123, &base_key).unwrap();
        assert_eq!(receiver.decrypt(&metadata, &ciphertext), Ok(plaintext));
        assert_eq!(receiver.decrypt(&metadata, &ciphertext), Err(Error::Replay));
    }

    #[test]
    fn tampering_metadata_and_unknown_keys_fail() {
        let mut sender = Sender::new(&[7; 32], 1000);
        let mut receiver = Receiver::new();
        receiver.insert(1000, &[7; 32]).unwrap();
        let frame = sender.encrypt(b"m", b"frame bytes").unwrap();
        for index in 0..frame.len() {
            let mut broken = frame.clone();
            broken[index] ^= 0x01;
            assert!(receiver.decrypt(b"m", &broken).is_err(), "byte {index}");
        }
        for len in 0..frame.len() {
            assert!(
                receiver.decrypt(b"m", &frame[..len]).is_err(),
                "length {len}"
            );
        }
        assert_eq!(receiver.decrypt(b"x", &frame), Err(Error::Authentication));
        assert_eq!(receiver.decrypt(b"m", &frame), Ok(b"frame bytes".to_vec()));

        let mut other = Sender::new(&[7; 32], 1001);
        let foreign = other.encrypt(b"m", b"frame bytes").unwrap();
        assert_eq!(receiver.decrypt(b"m", &foreign), Err(Error::UnknownKey));
        receiver.remove(1000);
        let next = sender.encrypt(b"m", b"frame bytes").unwrap();
        assert_eq!(receiver.decrypt(b"m", &next), Err(Error::UnknownKey));
    }

    #[test]
    fn replay_window_accepts_reordering_once() {
        let mut sender = Sender::new(&[1; 32], 3);
        let mut receiver = Receiver::new();
        receiver.insert(3, &[1; 32]).unwrap();
        let frames: Vec<_> = (0..300)
            .map(|_| sender.encrypt(&[], b"x").unwrap())
            .collect();
        // Newest first within the window, then everything again.
        for frame in frames[150..200].iter().rev() {
            assert!(receiver.decrypt(&[], frame).is_ok());
        }
        for (counter, frame) in frames.iter().enumerate() {
            // 199 - 127 = 72 is the oldest counter inside the window; 150..200 were
            // seen already.
            let fresh = (72..150).contains(&counter) || counter >= 200;
            let result = receiver.decrypt(&[], frame);
            assert_eq!(result.is_ok(), fresh, "counter {counter}");
        }
        // A jump far ahead clears the window.
        let mut far = Sender::new(&[1; 32], 3);
        far.counter = 10_000;
        assert!(
            receiver
                .decrypt(&[], &far.encrypt(&[], b"x").unwrap())
                .is_ok()
        );
        assert_eq!(
            receiver.decrypt(&[], &frames[299]),
            Err(Error::Replay),
            "older than the window"
        );
    }

    #[test]
    fn headers_decode_exactly_their_canonical_form() {
        // Every config byte with up to two following bytes.
        for config in 0..=u8::MAX {
            for tail in [&[][..], &[0], &[1], &[0, 1], &[0xff, 0], &[8, 9]] {
                let data = [&[config][..], tail].concat();
                if let Ok((kid, counter, len)) = decode_header(&data) {
                    assert_eq!(encode_header(kid, counter), data[..len]);
                }
            }
        }
        // KID 5 written with a length byte, and CTR 0x0100 with a leading zero.
        assert!(decode_header(&[0x80, 0x05, 0x00]).is_err());
        assert!(decode_header(&[0x0a, 0x00, 0x01, 0x00]).is_err());
    }

    #[test]
    fn bounds() {
        let mut sender = Sender::new(&[2; 32], 9);
        assert_eq!(
            sender.encrypt(&[], &vec![0; MAX_FRAME_LEN + 1]),
            Err(Error::TooLong)
        );
        assert!(sender.encrypt(&[], &vec![0; MAX_FRAME_LEN]).is_ok());
        let mut receiver = Receiver::new();
        assert_eq!(
            receiver.decrypt(&[], &vec![0; MAX_HEADER_LEN + MAX_FRAME_LEN + TAG_LEN + 1]),
            Err(Error::TooLong)
        );
        for kid in 0..MAX_RECEIVE_KEYS as u64 {
            receiver.insert(kid, &[0; 32]).unwrap();
        }
        assert_eq!(receiver.insert(99, &[0; 32]), Err(Error::LimitReached));
        receiver.insert(0, &[1; 32]).unwrap();

        sender.counter = u64::MAX;
        assert_eq!(sender.encrypt(&[], b"x"), Err(Error::Exhausted));
    }
}
