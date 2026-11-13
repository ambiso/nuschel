// SPDX-License-Identifier: EUPL-1.2
//! Worker IPC. Workers receive explicit data over inherited pipes, never credentials.

pub mod crypto;
#[cfg(not(target_family = "wasm"))]
pub mod host;
pub mod math;
pub mod murmur;
#[cfg(not(target_family = "wasm"))]
pub mod musicbot;
pub mod preview;
pub mod server;
pub mod windows_sandbox;

use std::io::{self, Read, Write};

use nuschl_core::{ChatMessage, ContentBlock, HistoryPage, MessageDocument};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
/// Large enough for a maximal message (256 KiB) after worst-case JSON escaping.
/// Large enough for the image viewer's RGBA reply (1600×1000, base64).
pub const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;

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

/// Thumbnail admission shared by the isolated worker and trusted adapters.
pub const MAX_THUMBNAIL_EDGE: u32 = 400;
pub const MAX_THUMBNAIL_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_THUMBNAIL_RGBA_BYTES: usize = 400 * 400 * 4;

/// One untrusted input for the content worker, and what to make of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentRequest {
    /// Literal message text with fenced code; answered with `Rendered`.
    Render { text: String },
    /// Mumble rich text, made inert; answered with `Prepared`.
    LegacyHtml { html: String },
    /// A base64-encoded avatar image; answered with a small `Rendered` thumbnail.
    Avatar { image: String },
    /// Bounded encoded artwork; replied with exactly one source-free Image.
    Thumbnail {
        image: String,
        max_width: u32,
        max_height: u32,
    },
    /// Sender-only metadata or a bounded carried image; never a fetch instruction.
    Preview(preview::Request),
    /// A base64-encoded website icon (favicon, also ICO/BMP); answered like `Avatar`.
    Icon { image: String },
    /// An inline `data:` image; answered with a `Rendered` image at the viewer's size.
    View { uri: String },
    /// A base64-encoded pasted image, re-encoded as a Mumble `<img>` element
    /// of at most `limit` bytes; answered with `Encoded`.
    EncodeImage { image: String, limit: u32 },
}

/// An image ready to send, and its thumbnail (an `Image` block) for the composer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodedImage {
    pub html: String,
    pub thumbnail: ContentBlock,
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
    /// Mumble rich text to send as it is.
    Encoded(EncodedImage),
    Preview(preview::Response),
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
    /// A page of messages around `id` (see `History::page_around`).
    HistoryAround {
        server: String,
        channel: String,
        id: i64,
        limit: u16,
    },
    /// The page of messages newer than `after`.
    HistoryAfter {
        server: String,
        channel: String,
        after: i64,
        limit: u16,
    },
    /// Full-text search of these servers' history, or of one conversation
    /// (`channel`) when a single server is given.
    Search {
        servers: Vec<String>,
        channel: Option<String>,
        query: String,
        after: Option<nuschl_core::SearchCursor>,
        limit: u16,
    },
    /// Messages of a Nuschl server's conversations by seq, stored once
    /// (see `History::store_server`); answered with `Stored`.
    StoreServer {
        records: Vec<nuschl_core::ServerRecord>,
    },
    /// Our own pending message (row `id`) was acknowledged as `seq`.
    SetSeq {
        id: i64,
        seq: i64,
    },
    /// The cached message `seq` was edited; answered with `Updated`.
    Edit {
        server: String,
        channel: String,
        seq: i64,
        body: String,
        edited_ms: i64,
    },
    /// The cached message `seq` was deleted: a tombstone without text.
    Delete {
        server: String,
        channel: String,
        seq: i64,
        deleted_ms: i64,
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
    SearchResults(nuschl_core::SearchPage),
    /// Row IDs of `StoreServer`'s records, in order (0: not stored).
    Stored(Vec<i64>),
    /// Whether an edit or deletion found its message (`SetSeq`: true).
    Updated(bool),
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

/// The magic-wormhole servers a transfer uses, chosen by the supervisor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WormholeServers {
    /// Rendezvous (mailbox) server, `ws://`.
    pub rendezvous: String,
    /// Transit relay, `tcp://host:port`.
    pub relay: String,
    /// Also try direct connections (this asks a public STUN server for the
    /// external address and tells the peer our addresses); otherwise relay only.
    pub direct: bool,
}

/// To the transfer worker. The first request says what it does for its whole
/// life; `Cancel` (or closing its input) may follow at any time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferRequest {
    /// Offer the one file the supervisor exposed to this worker, under
    /// `name`: a fresh code after every pickup, until cancelled.
    Offer {
        name: String,
        servers: WormholeServers,
    },
    /// Receive one file of at most `max_size` bytes with `code`; it comes
    /// back as [`TransferEvent::Data`].
    Receive {
        code: String,
        max_size: u64,
        servers: WormholeServers,
    },
    Cancel,
}

/// From the transfer worker, all with request_id 0. `peer` numbers the
/// pickups of one offer; a receiver's single transfer is peer 0.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferEvent {
    /// A code waiting for someone to receive with it.
    Code {
        code: String,
    },
    /// Someone connected with the code. For a receiver, the sender's own
    /// (untrusted) name for the file and its size.
    Started {
        peer: u32,
        name: String,
        size: u64,
    },
    Progress {
        peer: u32,
        done: u64,
        total: u64,
    },
    /// The next part of the file being received, base64.
    Data {
        bytes: String,
    },
    Done {
        peer: u32,
    },
    /// A transfer failed; `None`: waiting with a code did (a wrong code was
    /// tried, the server went away). An offer continues with a new code.
    Failed {
        peer: Option<u32>,
        reason: String,
    },
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
        settings: nuschl_core::audio::AudioSettings,
        /// Use only this audio server ("pipewire", "pulseaudio", "alsa"): the one
        /// that worked before a failure. While a sound server restarts, falling
        /// back to raw ALSA could take the hardware device from under it.
        #[serde(default)]
        host: Option<String>,
    },
    /// Push-to-talk key state from the desktop.
    PushToTalk {
        active: bool,
    },
    /// Voice from a server. One worker mixes every server the user listens
    /// to into one output (docs/architecture.md, "Voice on several servers").
    Playback {
        /// The server it came from, as the supervisor numbers them: sessions
        /// are per server, so one sender number on two servers is two people.
        #[serde(default)]
        stream: u32,
        sender: u32,
        sequence: u64,
        opus: Vec<u8>,
        end: bool,
        /// Product of the sender's volume adjustment, the local per-user
        /// volume and the server's playback volume.
        volume: f32,
    },
    /// Drop a server's voice at once (deafened there, or disconnected):
    /// what its speakers had buffered is not played.
    EndStream {
        stream: u32,
    },
    /// Local test mode instead of `SetState`: nothing is ever encoded or sent.
    /// `capture` opens the microphone; `loopback` plays the processed voice back.
    /// Changing `capture` needs a new process, as a mute change does.
    Preview {
        capture: bool,
        loopback: bool,
    },
    /// Play a speech-like signal and measure how much of it the microphone
    /// path would transmit. Answered by [`LiveMediaEvent::EchoTest`].
    EchoTest,
    /// Mix a sound effect into the playback output at `volume` (0 to 1). A
    /// worker without open devices drops it after a short wait.
    PlaySound {
        sound: nuschl_core::audio::Sound,
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
    /// Roughly ten times per second while a preview has the microphone open
    /// (the Settings meter); a call has no use for it.
    Level {
        /// Processed input level in dBFS.
        level_db: f32,
        /// Speech probability for the latest frame.
        voice: f32,
        /// Whether the gate would transmit (a preview never sends).
        transmitting: bool,
    },
    /// Device inventory and the active route, sent whenever devices are opened.
    Devices {
        inputs: Vec<nuschl_core::audio::AudioDevice>,
        outputs: Vec<nuschl_core::audio::AudioDevice>,
        input: Option<String>,
        output: Option<String>,
        echo: nuschl_core::audio::EchoSource,
        /// A noise suppressor found upstream of the microphone (e.g. an RNNoise
        /// filter-chain); Nuschl's own suppressor stays off while it is there.
        #[serde(default)]
        noise: Option<String>,
        /// What Nuschl's echo canceller uses as its reference: the output it
        /// records (all applications), or `None` for Nuschl's own playback only.
        #[serde(default)]
        reference: Option<String>,
    },
    /// Echo-canceller measurements for the adaptive controller and diagnostics.
    Echo {
        erle_db: Option<f32>,
        residual_likelihood: Option<f32>,
        delay_ms: Option<i32>,
    },
    /// The adaptive controller's view of the live graph, sent when it changes.
    Health {
        /// The microphone reaches full scale before Nuschl receives it.
        clipping: bool,
        /// Persistent residual echo: the transmit gate demands clearer speech
        /// while only the far end talks.
        echo_guard: bool,
        /// Share of far-end speech during which the local user also talked.
        double_talk: f32,
        /// Output device callbacks that ran dry in the last two seconds.
        underruns: u32,
        /// Nuschl's own playback buffer.
        playback_ms: u32,
    },
    /// Result of [`LiveMediaRequest::EchoTest`].
    EchoTest {
        /// Echo reaching Nuschl relative to the played signal, in dB; `None`
        /// when nothing rose above the room's noise (headphones, or a system
        /// canceller already removed it).
        echo_db: Option<f32>,
        /// How much Nuschl's processing reduced that echo, in dB.
        reduction_db: Option<f32>,
        /// Share of the test during which the voice gate would have transmitted.
        transmitted: f32,
    },
}

/// Cost of JSON framing for live voice, which decides whether a binary audio
/// IPC is worth its extra parsing surface. Run with
/// `cargo test --release -p nuschl-ipc -- --ignored --nocapture voice_ipc_cost`.
#[cfg(test)]
mod voice_ipc_cost {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "measurement, not a check"]
    fn voice_ipc_cost() {
        // A 20 ms Opus voice packet at 40 kb/s is about 100 bytes.
        let opus: Vec<u8> = (0..100_u8).map(|byte| byte.wrapping_mul(37)).collect();
        let event = Envelope::new(
            0,
            LiveMediaEvent::Packet {
                epoch: 3,
                sequence: 123_456,
                opus: opus.clone(),
                end: false,
            },
        );
        let request = Envelope::new(
            0,
            LiveMediaRequest::Playback {
                stream: 1,
                sender: 42,
                sequence: 123_456,
                opus,
                end: false,
                volume: 1.,
            },
        );
        let rounds = 200_000;
        let mut bytes = Vec::new();
        let started = Instant::now();
        for _ in 0..rounds {
            bytes.clear();
            write_frame(&mut bytes, &event).unwrap();
            let decoded: Envelope<LiveMediaEvent> =
                read_frame(&mut bytes.as_slice()).unwrap().unwrap();
            std::hint::black_box(decoded);
            bytes.clear();
            write_frame(&mut bytes, &request).unwrap();
            let decoded: Envelope<LiveMediaRequest> =
                read_frame(&mut bytes.as_slice()).unwrap().unwrap();
            std::hint::black_box(decoded);
        }
        let per_packet = started.elapsed().as_secs_f64() / rounds as f64;
        // Each round encodes and decodes one outgoing and one incoming packet;
        // 50 packets/s each way, with four people talking at once.
        let per_second = per_packet * 50. * 4.;
        println!(
            "JSON voice frame: {} bytes; {:.2} µs to encode and decode one packet each way; \
             {:.3}% of one core with four simultaneous talkers",
            bytes.len(),
            per_packet * 1e6,
            per_second * 100.
        );
    }
}

#[cfg(test)]
mod hostile_frames {
    use super::*;

    #[test]
    fn playback_without_a_stream_is_stream_zero() -> Result<(), serde_json::Error> {
        let request: LiveMediaRequest = serde_json::from_str(
            r#"{"kind":"playback","sender":3,"sequence":2,"opus":[1],"end":false,"volume":1.0}"#,
        )?;
        assert!(matches!(
            request,
            LiveMediaRequest::Playback {
                stream: 0,
                sender: 3,
                ..
            }
        ));
        Ok(())
    }
    #[test]
    fn random_frames_are_rejected_without_panicking() {
        let mut state = 0x1234_5678_9abc_def0_u64;
        for _ in 0..20_000 {
            let mut bytes = Vec::new();
            let length = (state % 96) as usize;
            for _ in 0..length {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push(state as u8);
            }
            // Frequently claim a plausible length so JSON parsing is reached.
            if bytes.len() >= 4 && state.is_multiple_of(3) {
                let claimed = ((bytes.len() - 4) as u32).to_be_bytes();
                bytes[..4].copy_from_slice(&claimed);
            }
            let _ = read_frame::<LiveMediaRequest>(&mut bytes.as_slice());
            let _ = read_frame::<ContentRequest>(&mut bytes.as_slice());
        }
    }
}
