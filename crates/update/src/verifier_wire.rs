// SPDX-License-Identifier: EUPL-1.2
//! One bounded request per fixed-algorithm process, over inherited pipes only.
//! Each verifier receives its own public key/signature and the parent's exact
//! pair-bound transcript. No other signature, private material or result enters.
use crate::auth_format::{
    ED_PUBLIC_BYTES, ED_SIGNATURE_BYTES, MAX_SIGNED_BYTES, ML_PUBLIC_BYTES, ML_SIGNATURE_BYTES,
};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};

const REQUEST_MAGIC: &[u8; 8] = b"NUSUVR2\0";
const REPLY_MAGIC: &[u8; 8] = b"NUSUVP2\0";
const REPLY_DOMAIN: &[u8] = b"nuschl.update.verifier.reply.v2\0";
pub const REPLY_BYTES: usize = 8 + 1 + 32;
pub const MAX_REQUEST_BYTES: usize = 13 + ML_PUBLIC_BYTES + ML_SIGNATURE_BYTES + MAX_SIGNED_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Ed25519,
    MlDsa44,
}
impl Algorithm {
    pub fn tag(self) -> u8 {
        match self {
            Self::Ed25519 => 1,
            Self::MlDsa44 => 2,
        }
    }
    pub fn executable(self) -> &'static str {
        match self {
            Self::Ed25519 => "nuschl-update-verify-ed25519",
            Self::MlDsa44 => "nuschl-update-verify-mldsa44",
        }
    }
    fn sizes(self) -> (usize, usize) {
        match self {
            Self::Ed25519 => (ED_PUBLIC_BYTES, ED_SIGNATURE_BYTES),
            Self::MlDsa44 => (ML_PUBLIC_BYTES, ML_SIGNATURE_BYTES),
        }
    }
}

#[derive(Debug)]
pub struct Request {
    algorithm: Algorithm,
    public: Vec<u8>,
    signature: Vec<u8>,
    message: Vec<u8>,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid update verifier frame")
}
impl Request {
    pub fn new(
        algorithm: Algorithm,
        public: &[u8],
        signature: &[u8],
        message: &[u8],
    ) -> io::Result<Self> {
        let (key_size, signature_size) = algorithm.sizes();
        if public.len() != key_size
            || signature.len() != signature_size
            || message.is_empty()
            || message.len() > MAX_SIGNED_BYTES
        {
            return Err(invalid());
        }
        Ok(Self {
            algorithm,
            public: public.to_vec(),
            signature: signature.to_vec(),
            message: message.to_vec(),
        })
    }
    pub fn public(&self) -> &[u8] {
        &self.public
    }
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }
    pub fn message(&self) -> &[u8] {
        &self.message
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(13 + self.public.len() + self.signature.len() + self.message.len());
        out.extend_from_slice(REQUEST_MAGIC);
        out.push(self.algorithm.tag());
        out.extend_from_slice(&(self.message.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.public);
        out.extend_from_slice(&self.signature);
        out.extend_from_slice(&self.message);
        out
    }
    /// `expected` comes from the executable's fixed entry point, never a caller
    /// algorithm switch. A foreign algorithm or any trailing byte is rejected.
    pub fn read(mut input: impl Read, expected: Algorithm) -> io::Result<Self> {
        let mut header = [0; 13];
        input.read_exact(&mut header)?;
        if &header[..8] != REQUEST_MAGIC || header[8] != expected.tag() {
            return Err(invalid());
        }
        let length = u32::from_be_bytes(header[9..13].try_into().map_err(|_| invalid())?) as usize;
        if length == 0 || length > MAX_SIGNED_BYTES {
            return Err(invalid());
        }
        let (key_size, signature_size) = expected.sizes();
        let mut public = vec![0; key_size];
        let mut signature = vec![0; signature_size];
        let mut message = vec![0; length];
        input.read_exact(&mut public)?;
        input.read_exact(&mut signature)?;
        input.read_exact(&mut message)?;
        if input.read(&mut [0])? != 0 {
            return Err(invalid());
        }
        Self::new(expected, &public, &signature, &message)
    }
    /// A successful exit plus this exact bounded reply is mandatory. The parent
    /// computes it independently; replies cannot be replayed across jobs/engines.
    pub fn expected_reply(&self) -> [u8; REPLY_BYTES] {
        let mut hash = Sha256::new();
        hash.update(REPLY_DOMAIN);
        hash.update(self.encode());
        let mut out = [0; REPLY_BYTES];
        out[..8].copy_from_slice(REPLY_MAGIC);
        out[8] = self.algorithm.tag();
        out[9..].copy_from_slice(&hash.finalize());
        out
    }
    pub fn accepted(&self, mut output: impl Write) -> io::Result<()> {
        output.write_all(&self.expected_reply())?;
        output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_engine_exact_framing_and_request_bound_replies() {
        let request = Request::new(
            Algorithm::Ed25519,
            &[1; 32],
            &[2; 64],
            b"exact raw transcript",
        )
        .unwrap();
        let frame = request.encode();
        let parsed = Request::read(frame.as_slice(), Algorithm::Ed25519).unwrap();
        assert_eq!(parsed.expected_reply(), request.expected_reply());
        assert!(Request::read(frame.as_slice(), Algorithm::MlDsa44).is_err());
        let mut trailing = frame.clone();
        trailing.push(0);
        assert!(Request::read(trailing.as_slice(), Algorithm::Ed25519).is_err());
        for size in [0, 12, frame.len() - 1] {
            assert!(Request::read(&frame[..size], Algorithm::Ed25519).is_err());
        }
        let changed = Request::new(
            Algorithm::Ed25519,
            &[1; 32],
            &[2; 64],
            b"different transcript",
        )
        .unwrap();
        assert_ne!(changed.expected_reply(), request.expected_reply());
        let mut oversized = frame;
        oversized[9..13].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Request::read(oversized.as_slice(), Algorithm::Ed25519).is_err());
    }
}
