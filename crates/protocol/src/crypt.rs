// SPDX-License-Identifier: EUPL-1.2
//! Mumble's UDP encryption: OCB2-AES128 with Mumble's packet header, nonce
//! window and its countermeasures against the published XEX* attack
//! (Inoue & Minematsu, <https://eprint.iacr.org/2019/311>, section 9).
//!
//! This mirrors stock Mumble's `CryptStateOCB2` byte for byte. It must not be
//! replaced with a generic modern OCB implementation: OCB2 differs from OCB3,
//! and Mumble's 4-byte header carries only the low IV byte and a 24-bit tag.
use aes::{
    Aes128,
    cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray},
};

pub const BLOCK: usize = 16;
pub const HEADER: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CryptStats {
    pub good: u32,
    pub late: u32,
    pub lost: u32,
    pub resync: u32,
}

pub struct CryptState {
    cipher: Aes128,
    encrypt_iv: [u8; BLOCK],
    decrypt_iv: [u8; BLOCK],
    decrypt_history: [u8; 256],
    pub stats: CryptStats,
}

impl std::fmt::Debug for CryptState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CryptState")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// The key and nonces a server sends in `CryptSetup`. `Debug` never shows them.
#[derive(Clone, PartialEq, Eq)]
pub struct CryptSetupParts {
    pub key: [u8; BLOCK],
    pub client_nonce: [u8; BLOCK],
    pub server_nonce: [u8; BLOCK],
}

impl std::fmt::Debug for CryptSetupParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CryptSetupParts { .. }")
    }
}

/// Compare without an early exit (tags and the forged-block check).
fn equal_constant_time(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0, |difference, (x, y)| difference | (x ^ y))
            == 0
}

#[derive(Debug, PartialEq, Eq)]
pub enum CryptError {
    /// A key or nonce did not have the 16-byte AES block size.
    InvalidLength,
}

type Block = [u8; BLOCK];

fn xor(a: &Block, b: &Block) -> Block {
    std::array::from_fn(|index| a[index] ^ b[index])
}

/// Multiply by x in GF(2^128) using OCB's big-endian convention.
fn times2(block: &Block) -> Block {
    let carry = block[0] >> 7;
    let mut out = [0; BLOCK];
    for index in 0..BLOCK - 1 {
        out[index] = (block[index] << 1) | (block[index + 1] >> 7);
    }
    out[BLOCK - 1] = (block[BLOCK - 1] << 1) ^ (carry * 0x87);
    out
}

fn times3(block: &Block) -> Block {
    xor(block, &times2(block))
}

impl CryptState {
    fn new(key: &[u8], encrypt_iv: &[u8], decrypt_iv: &[u8]) -> Result<Self, CryptError> {
        let key: &Block = key.try_into().map_err(|_| CryptError::InvalidLength)?;
        Ok(Self {
            cipher: Aes128::new(GenericArray::from_slice(key)),
            encrypt_iv: encrypt_iv
                .try_into()
                .map_err(|_| CryptError::InvalidLength)?,
            decrypt_iv: decrypt_iv
                .try_into()
                .map_err(|_| CryptError::InvalidLength)?,
            decrypt_history: [0; 256],
            stats: CryptStats::default(),
        })
    }

    /// The client's state from a server's `CryptSetup`: it encrypts with the
    /// client nonce and decrypts with the server nonce. Both role constructors
    /// take the fields in the same order, so swapping roles cannot silently
    /// make both directions share one nonce stream.
    pub fn client(
        key: &[u8],
        client_nonce: &[u8],
        server_nonce: &[u8],
    ) -> Result<Self, CryptError> {
        Self::new(key, client_nonce, server_nonce)
    }

    /// The server's state for the `CryptSetup` it sent.
    pub fn server(
        key: &[u8],
        client_nonce: &[u8],
        server_nonce: &[u8],
    ) -> Result<Self, CryptError> {
        Self::new(key, server_nonce, client_nonce)
    }

    /// A fresh server state from 48 bytes of OS randomness, and the parts to
    /// send in `CryptSetup`, so what is kept and what is sent cannot diverge.
    pub fn generate_server(random: [u8; 48]) -> (Self, CryptSetupParts) {
        let mut parts = CryptSetupParts {
            key: [0; BLOCK],
            client_nonce: [0; BLOCK],
            server_nonce: [0; BLOCK],
        };
        parts.key.copy_from_slice(&random[..BLOCK]);
        parts
            .client_nonce
            .copy_from_slice(&random[BLOCK..2 * BLOCK]);
        parts.server_nonce.copy_from_slice(&random[2 * BLOCK..]);
        let state = Self {
            cipher: Aes128::new(GenericArray::from_slice(&parts.key)),
            encrypt_iv: parts.server_nonce,
            decrypt_iv: parts.client_nonce,
            decrypt_history: [0; 256],
            stats: CryptStats::default(),
        };
        (state, parts)
    }

    /// The server resynchronised our receive nonce.
    pub fn set_decrypt_iv(&mut self, iv: &[u8]) -> Result<(), CryptError> {
        self.decrypt_iv = iv.try_into().map_err(|_| CryptError::InvalidLength)?;
        self.stats.resync = self.stats.resync.saturating_add(1);
        Ok(())
    }

    /// Our send nonce, returned when the server asks to resynchronise.
    pub fn encrypt_iv(&self) -> [u8; BLOCK] {
        self.encrypt_iv
    }

    fn aes_encrypt(&self, block: &Block) -> Block {
        let mut block = GenericArray::from(*block);
        self.cipher.encrypt_block(&mut block);
        block.into()
    }

    fn aes_decrypt(&self, block: &Block) -> Block {
        let mut block = GenericArray::from(*block);
        self.cipher.decrypt_block(&mut block);
        block.into()
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut out = vec![0; plain.len() + HEADER];
        self.encrypt_into(plain, &mut out);
        out
    }

    /// Encrypt into `out` without allocating; returns the packet length
    /// (`plain.len() + HEADER`), or `None` if `out` is too small.
    pub fn encrypt_into(&mut self, plain: &[u8], out: &mut [u8]) -> Option<usize> {
        let length = plain.len() + HEADER;
        let out = out.get_mut(..length)?;
        for byte in self.encrypt_iv.iter_mut() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
        let tag = self.ocb_encrypt(plain, &mut out[HEADER..], &self.encrypt_iv.clone());
        out[0] = self.encrypt_iv[0];
        out[1..HEADER].copy_from_slice(&tag[..3]);
        Some(length)
    }

    pub fn decrypt(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        let mut plain = vec![0; packet.len().checked_sub(HEADER)?];
        self.decrypt_into(packet, &mut plain)?;
        Some(plain)
    }

    /// Decrypt into `out` without allocating; returns the plaintext length.
    /// A failed packet leaves the nonce state as it was (`out` may hold garbage).
    pub fn decrypt_into(&mut self, packet: &[u8], out: &mut [u8]) -> Option<usize> {
        if packet.len() < HEADER || out.len() < packet.len() - HEADER {
            return None;
        }
        let saved = self.decrypt_iv;
        let iv_byte = packet[0];
        let mut restore = false;
        let mut late = 0;
        let mut lost: i32 = 0;
        if self.decrypt_iv[0].wrapping_add(1) == iv_byte {
            // In order, as expected.
            if iv_byte > self.decrypt_iv[0] {
                self.decrypt_iv[0] = iv_byte;
            } else if iv_byte < self.decrypt_iv[0] {
                self.decrypt_iv[0] = iv_byte;
                carry_up(&mut self.decrypt_iv);
            } else {
                return None;
            }
        } else {
            // Out of order or a repeat.
            let mut diff = i32::from(iv_byte) - i32::from(self.decrypt_iv[0]);
            if diff > 128 {
                diff -= 256;
            } else if diff < -128 {
                diff += 256;
            }
            let current = self.decrypt_iv[0];
            if iv_byte < current && diff > -30 && diff < 0 {
                // Late packet without wraparound.
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = iv_byte;
                restore = true;
            } else if iv_byte > current && diff > -30 && diff < 0 {
                // Late packet from before the low byte wrapped.
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = iv_byte;
                borrow_down(&mut self.decrypt_iv);
                restore = true;
            } else if iv_byte > current && diff > 0 {
                lost = i32::from(iv_byte) - i32::from(current) - 1;
                self.decrypt_iv[0] = iv_byte;
            } else if iv_byte < current && diff > 0 {
                lost = 256 - i32::from(current) + i32::from(iv_byte) - 1;
                self.decrypt_iv[0] = iv_byte;
                carry_up(&mut self.decrypt_iv);
            } else {
                return None;
            }
            if self.decrypt_history[usize::from(self.decrypt_iv[0])] == self.decrypt_iv[1] {
                // Replay.
                self.decrypt_iv = saved;
                return None;
            }
        }
        let plain = &mut out[..packet.len() - HEADER];
        let iv = self.decrypt_iv;
        let tag = self.ocb_decrypt(&packet[HEADER..], plain, &iv);
        if tag.is_none_or(|tag| !equal_constant_time(&tag[..3], &packet[1..HEADER])) {
            self.decrypt_iv = saved;
            return None;
        }
        self.decrypt_history[usize::from(self.decrypt_iv[0])] = self.decrypt_iv[1];
        if restore {
            self.decrypt_iv = saved;
        }
        self.stats.good = self.stats.good.saturating_add(1);
        self.stats.late = self.stats.late.saturating_add(late);
        self.stats.lost = if lost >= 0 {
            self.stats.lost.saturating_add(lost as u32)
        } else {
            self.stats.lost.saturating_sub(lost.unsigned_abs())
        };
        Some(packet.len() - HEADER)
    }

    fn ocb_encrypt(&self, mut plain: &[u8], mut out: &mut [u8], nonce: &Block) -> Block {
        let mut delta = self.aes_encrypt(nonce);
        let mut checksum = [0; BLOCK];
        while plain.len() > BLOCK {
            let block: &Block = plain[..BLOCK].try_into().unwrap_or(&[0; BLOCK]);
            // XEX* countermeasure: the second-to-last block must not be all zero
            // apart from its last byte. Flip a bit instead, which keeps digital
            // silence encodable and decodes to an inaudible change.
            let flip =
                plain.len() - BLOCK <= BLOCK && block[..BLOCK - 1].iter().all(|byte| *byte == 0);
            delta = times2(&delta);
            let mut tmp = xor(&delta, block);
            if flip {
                tmp[0] ^= 1;
            }
            let encrypted = xor(&delta, &self.aes_encrypt(&tmp));
            out[..BLOCK].copy_from_slice(&encrypted);
            checksum = xor(&checksum, block);
            if flip {
                checksum[0] ^= 1;
            }
            plain = &plain[BLOCK..];
            out = &mut out[BLOCK..];
        }
        delta = times2(&delta);
        let mut length = [0; BLOCK];
        length[BLOCK - 4..].copy_from_slice(&((plain.len() as u32) * 8).to_be_bytes());
        let pad = self.aes_encrypt(&xor(&length, &delta));
        let mut tmp = pad;
        tmp[..plain.len()].copy_from_slice(plain);
        checksum = xor(&checksum, &tmp);
        let final_block = xor(&pad, &tmp);
        out[..plain.len()].copy_from_slice(&final_block[..plain.len()]);
        delta = times3(&delta);
        self.aes_encrypt(&xor(&delta, &checksum))
    }

    fn ocb_decrypt(
        &self,
        mut encrypted: &[u8],
        mut out: &mut [u8],
        nonce: &Block,
    ) -> Option<Block> {
        let mut delta = self.aes_encrypt(nonce);
        let mut checksum = [0; BLOCK];
        while encrypted.len() > BLOCK {
            let block: &Block = encrypted[..BLOCK].try_into().ok()?;
            delta = times2(&delta);
            let plain = xor(&delta, &self.aes_decrypt(&xor(&delta, block)));
            out[..BLOCK].copy_from_slice(&plain);
            checksum = xor(&checksum, &plain);
            encrypted = &encrypted[BLOCK..];
            out = &mut out[BLOCK..];
        }
        delta = times2(&delta);
        let mut length = [0; BLOCK];
        length[BLOCK - 4..].copy_from_slice(&((encrypted.len() as u32) * 8).to_be_bytes());
        let pad = self.aes_encrypt(&xor(&length, &delta));
        let mut tmp = [0; BLOCK];
        tmp[..encrypted.len()].copy_from_slice(encrypted);
        let tmp = xor(&tmp, &pad);
        checksum = xor(&checksum, &tmp);
        out[..encrypted.len()].copy_from_slice(&tmp[..encrypted.len()]);
        // XEX* countermeasure: a forged final block would equal delta ^ len.
        // `len` only affects the last byte, so compare all others.
        let forged = equal_constant_time(&tmp[..BLOCK - 1], &delta[..BLOCK - 1]);
        delta = times3(&delta);
        let tag = self.aes_encrypt(&xor(&delta, &checksum));
        (!forged).then_some(tag)
    }
}

fn carry_up(iv: &mut Block) {
    for byte in iv.iter_mut().skip(1) {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            break;
        }
    }
}

fn borrow_down(iv: &mut Block) {
    for byte in iv.iter_mut().skip(1) {
        let previous = *byte;
        *byte = byte.wrapping_sub(1);
        if previous != 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Block = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    #[test]
    fn matches_ocb2_draft_test_vectors_used_by_mumble() {
        // draft-krovetz-ocb-00, also asserted by Mumble's TestCrypt.
        let state = CryptState::client(&KEY, &KEY, &KEY).unwrap();
        let tag = state.ocb_encrypt(&[], &mut [], &KEY);
        assert_eq!(
            tag,
            [
                0xBF, 0x31, 0x08, 0x13, 0x07, 0x73, 0xAD, 0x5E, 0xC7, 0x0E, 0xC6, 0x9E, 0x78, 0x75,
                0xA7, 0xB0
            ]
        );
        let source: Vec<u8> = (0..40).collect();
        let mut encrypted = [0; 40];
        let tag = state.ocb_encrypt(&source, &mut encrypted, &KEY);
        assert_eq!(
            tag,
            [
                0x9D, 0xB0, 0xCD, 0xF8, 0x80, 0xF7, 0x3E, 0x3E, 0x10, 0xD4, 0xEB, 0x32, 0x17, 0x76,
                0x66, 0x88
            ]
        );
        assert_eq!(
            encrypted,
            [
                0xF7, 0x5D, 0x6B, 0xC8, 0xB4, 0xDC, 0x8D, 0x66, 0xB8, 0x36, 0xA2, 0xB0, 0x8B, 0x32,
                0xA6, 0x36, 0x9F, 0x1C, 0xD3, 0xC5, 0x22, 0x8D, 0x79, 0xFD, 0x6C, 0x26, 0x7F, 0x5F,
                0x6A, 0xA7, 0xB2, 0x31, 0xC7, 0xDF, 0xB9, 0xD5, 0x99, 0x51, 0xAE, 0x9C
            ]
        );
        let mut decrypted = [0; 40];
        assert_eq!(
            state.ocb_decrypt(&encrypted, &mut decrypted, &KEY),
            Some(tag)
        );
        assert_eq!(decrypted.as_slice(), source.as_slice());
    }

    fn pair() -> (CryptState, CryptState) {
        let client_nonce = [7; BLOCK];
        let server_nonce = [9; BLOCK];
        (
            CryptState::client(&KEY, &client_nonce, &server_nonce).unwrap(),
            CryptState::server(&KEY, &client_nonce, &server_nonce).unwrap(),
        )
    }

    #[test]
    fn generated_server_state_talks_to_the_client_it_describes() {
        let random: [u8; 48] = std::array::from_fn(|index| (index as u8).wrapping_mul(7));
        let (mut server, parts) = CryptState::generate_server(random);
        assert_eq!(format!("{parts:?}"), "CryptSetupParts { .. }");
        let mut client =
            CryptState::client(&parts.key, &parts.client_nonce, &parts.server_nonce).unwrap();
        assert_eq!(
            server.decrypt(&client.encrypt(b"up")).as_deref(),
            Some(&b"up"[..])
        );
        assert_eq!(
            client.decrypt(&server.encrypt(b"down")).as_deref(),
            Some(&b"down"[..])
        );
        // The two directions use different nonces.
        assert_ne!(client.encrypt_iv(), server.encrypt_iv());
    }

    #[test]
    fn round_trips_every_length_and_rejects_tampering() {
        let (mut client, mut server) = pair();
        for length in 0..100 {
            let plain: Vec<u8> = (0..length).map(|byte| byte as u8 ^ 0x5a).collect();
            let packet = client.encrypt(&plain);
            assert_eq!(packet.len(), length + HEADER);
            let mut forged = packet.clone();
            if let Some(byte) = forged.last_mut() {
                *byte ^= 1;
            } else {
                forged[1] ^= 1;
            }
            assert_eq!(server.decrypt(&forged), None);
            assert_eq!(server.decrypt(&packet).as_deref(), Some(plain.as_slice()));
        }
        assert_eq!(server.stats.good, 100);
    }

    #[test]
    fn silence_is_still_encodable_with_the_xex_countermeasure() {
        let (mut client, mut server) = pair();
        let mut silence = [0_u8; 32];
        silence[31] = 5;
        let decoded = server.decrypt(&client.encrypt(&silence)).unwrap();
        assert_eq!(decoded[0], 1, "first byte of the critical block is flipped");
        assert_eq!(decoded[1..], silence[1..]);
    }

    #[test]
    fn tracks_lost_and_late_packets_and_rejects_replays() {
        let (mut client, mut server) = pair();
        let packets: Vec<_> = (0..300)
            .map(|index| client.encrypt(&[index as u8; 20]))
            .collect();
        assert!(server.decrypt(&packets[0]).is_some());
        assert!(server.decrypt(&packets[3]).is_some());
        assert_eq!(server.stats.lost, 2);
        assert!(
            server.decrypt(&packets[2]).is_some(),
            "late packet within the window"
        );
        assert_eq!((server.stats.late, server.stats.lost), (1, 1));
        assert_eq!(server.decrypt(&packets[2]), None, "replay");
        assert_eq!(server.decrypt(&packets[3]), None, "replay");
        // Crossing the low-byte wrap keeps the higher IV bytes in step.
        for packet in &packets[4..300] {
            assert!(server.decrypt(packet).is_some());
        }
        assert_eq!(server.stats.good, 299);
    }

    #[test]
    fn resynchronises_after_a_nonce_reset() {
        let (mut client, mut server) = pair();
        for _ in 0..200 {
            client.encrypt(b"skip");
        }
        let packet = client.encrypt(b"far ahead");
        assert_eq!(server.decrypt(&packet), None);
        server.set_decrypt_iv(&client.encrypt_iv()).unwrap();
        assert_eq!(
            server.decrypt(&client.encrypt(b"next")).as_deref(),
            Some(&b"next"[..])
        );
        assert_eq!(server.stats.resync, 1);
    }
}
