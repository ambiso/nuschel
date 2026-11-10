// SPDX-License-Identifier: EUPL-1.2
//! Worker IPC. Workers receive explicit data over inherited pipes, never credentials.

#[cfg(not(target_family = "wasm"))]
pub mod host;

use std::io::{self, Read, Write};

use nuschel_core::{ChatMessage, HistoryPage, MessageDocument};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub version: u16,
    pub request_id: u64,
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn new(request_id: u64, payload: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            payload,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentRequest {
    pub text: String,
    #[serde(default)]
    pub legacy_html: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedContent {
    pub source: String,
    pub document: MessageDocument,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContentResponse {
    Rendered(MessageDocument),
    Prepared(PreparedContent),
    Rejected(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageRequest {
    Append {
        message: ChatMessage,
    },
    Recent {
        server: String,
        channel: String,
        limit: u16,
    },
    HistoryPage {
        server: String,
        channel: String,
        before: Option<i64>,
        limit: u16,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StorageResponse {
    Appended(i64),
    Messages(Vec<ChatMessage>),
    HistoryPage(HistoryPage),
    Rejected(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MediaRequest {
    DecodeOpus { packet: Vec<u8> },
    EncodeOpus { samples: Vec<f32> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MediaResponse {
    Samples(Vec<f32>),
    Packet(Vec<u8>),
    Rejected(String),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("worker frame exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("unsupported worker protocol version {0}")]
    Version(u16),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// A length limit is checked before allocation or deserialization.
pub fn read_frame<T: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<Option<Envelope<T>>, Error> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header[..1]) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        result => result?,
    }
    reader.read_exact(&mut header[1..])?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(Error::TooLarge);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let envelope: Envelope<T> = serde_json::from_slice(&body)?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(Error::Version(envelope.version));
    }
    Ok(Some(envelope))
}

/// Serialize into a capped buffer before writing any bytes to the pipe.
pub fn write_frame<T: Serialize>(
    writer: &mut impl Write,
    value: &Envelope<T>,
) -> Result<(), Error> {
    let mut buffer = CappedBuffer {
        bytes: Vec::new(),
        full: false,
    };
    if let Err(error) = serde_json::to_writer(&mut buffer, value) {
        return Err(if buffer.full {
            Error::TooLarge
        } else {
            Error::Json(error)
        });
    }
    writer.write_all(&(buffer.bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&buffer.bytes)?;
    writer.flush()?;
    Ok(())
}

struct CappedBuffer {
    bytes: Vec<u8>,
    full: bool,
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES.saturating_sub(self.bytes.len()) {
            self.full = true;
            return Err(io::Error::other("worker frame too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_frames_before_reading_the_body() {
        let header = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert!(matches!(
            read_frame::<ContentRequest>(&mut &header[..]),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn rejects_truncated_frames_and_unknown_protocols() -> Result<(), Error> {
        assert!(matches!(
            read_frame::<u8>(&mut &[0, 0][..]),
            Err(Error::Io(_))
        ));
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Envelope {
                version: 2,
                request_id: 1,
                payload: 3_u8,
            },
        )?;
        assert!(matches!(
            read_frame::<u8>(&mut &bytes[..]),
            Err(Error::Version(2))
        ));
        bytes.pop();
        assert!(matches!(
            read_frame::<u8>(&mut &bytes[..]),
            Err(Error::Io(_))
        ));
        Ok(())
    }

    #[test]
    fn failed_serialization_does_not_partially_write_a_frame() {
        let mut output = Vec::new();
        let value = Envelope::new(1, "x".repeat(MAX_FRAME_BYTES));
        assert!(matches!(
            write_frame(&mut output, &value),
            Err(Error::TooLarge)
        ));
        assert!(output.is_empty());
    }
}

/// Live audio has independent events: request_id 0 denotes a stream event.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveMediaRequest {
    SetState {
        epoch: u64,
        muted: bool,
        deafened: bool,
    },
    Configure {
        bitrate: u32,
        /// Measured server-to-client loss, used to tune Opus in-band FEC.
        #[serde(default)]
        packet_loss_percent: u8,
        settings: nuschel_core::audio::AudioSettings,
    },
    /// Push-to-talk key state from the desktop.
    PushToTalk {
        active: bool,
    },
    Playback {
        sender: u32,
        sequence: u64,
        opus: Vec<u8>,
        end: bool,
        /// Product of the sender's volume adjustment and the local per-user volume.
        volume: f32,
    },
    Shutdown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveMediaEvent {
    Status {
        ready: bool,
        message: String,
    },
    Packet {
        epoch: u64,
        sequence: u64,
        opus: Vec<u8>,
        end: bool,
    },
    /// Roughly ten times per second while the microphone is open.
    Level {
        /// Processed input level in dBFS.
        level_db: f32,
        /// Speech probability for the latest frame.
        voice: f32,
        transmitting: bool,
    },
    /// Device inventory and the active route, sent whenever devices are opened.
    Devices {
        inputs: Vec<nuschel_core::audio::AudioDevice>,
        outputs: Vec<nuschel_core::audio::AudioDevice>,
        input: Option<String>,
        output: Option<String>,
        echo: nuschel_core::audio::EchoSource,
    },
    /// Echo-canceller measurements for the adaptive controller and diagnostics.
    Echo {
        erle_db: Option<f32>,
        residual_likelihood: Option<f32>,
        delay_ms: Option<i32>,
    },
}
