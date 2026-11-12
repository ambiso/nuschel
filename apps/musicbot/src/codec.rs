// SPDX-License-Identifier: EUPL-1.2
//! Private parent/codec-role protocol. No credentials, URLs or paths cross it.
//!
//! Decode receives one framed `Decode` request (id 1), followed by opaque raw
//! media bytes until EOF. Every PCM/Done/Error reply carries that same id.
//! Encode and Duck exchange one framed request/reply at a time, retaining
//! codec state; request ids are checked by the parent. All roles use the
//! existing IPC envelope/version and a stricter 64 KiB allocation bound.
use nuschl_ipc::{Envelope, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME: usize = 960;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_OPUS_BYTES: usize = 992;
pub const MAX_VOICE_BYTES: usize = 1024;
pub const MAX_ERROR_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Decode,
    Encode,
    Duck,
}

impl Role {
    pub fn argument(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Encode => "encode",
            Self::Duck => "duck",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "decode" => Some(Self::Decode),
            "encode" => Some(Self::Encode),
            "duck" => Some(Self::Duck),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Decode {
        mime: Option<String>,
        extension: Option<String>,
        skip_frames: u64,
    },
    Encode {
        samples: Vec<f32>,
        bitrate: u32,
    },
    Hear {
        sender: u32,
        opus: Vec<u8>,
        threshold: f32,
    },
    Forget { sender: u32 },
    Clear,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Pcm { samples: Vec<f32> },
    Packet { opus: Vec<u8> },
    Speech { active: bool },
    Ack,
    Done,
    Error { message: String },
}

pub fn read_message<T: DeserializeOwned>(
    input: &mut impl Read,
) -> Result<Option<Envelope<T>>, String> {
    let mut header = [0; 4];
    match input.read_exact(&mut header[..1]) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        result => result.map_err(|_| "codec IPC read failed")?,
    }
    input
        .read_exact(&mut header[1..])
        .map_err(|_| "codec IPC header truncated")?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err("codec IPC frame is too large or empty".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|_| "codec IPC body truncated")?;
    let envelope: Envelope<T> =
        serde_json::from_slice(&bytes).map_err(|_| "invalid codec IPC message")?;
    if envelope.version != PROTOCOL_VERSION {
        return Err("unsupported codec IPC version".into());
    }
    Ok(Some(envelope))
}

pub fn write_message<T: Serialize>(
    output: &mut impl Write,
    envelope: &Envelope<T>,
) -> Result<(), String> {
    struct Capped(Vec<u8>);
    impl Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.len() + bytes.len() > MAX_MESSAGE_BYTES {
                return Err(io::Error::other("codec IPC frame exceeds its budget"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut bytes = Capped(Vec::new());
    serde_json::to_writer(&mut bytes, envelope).map_err(|_| "codec IPC serialization failed")?;
    output
        .write_all(&(bytes.0.len() as u32).to_be_bytes())
        .and_then(|()| output.write_all(&bytes.0))
        .and_then(|()| output.flush())
        .map_err(|_| "codec IPC write failed".into())
}

pub fn valid_pcm(samples: &[f32]) -> bool {
    samples.len() == FRAME * CHANNELS && samples.iter().all(|sample| sample.is_finite())
}
