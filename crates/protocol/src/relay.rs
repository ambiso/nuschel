// SPDX-License-Identifier: EUPL-1.2
//! Voice datagrams as a server sees them: parsed without copying, re-encoded per
//! recipient into a caller's buffer. Opus only; legacy CELT and Speex packets are
//! rejected. The plaintext is whatever OCB2 decrypted or the TLS tunnel carried.
//!
//! Formats (stock Mumble's `MumbleProtocol.cpp`):
//! - Legacy (before 1.5): a header byte (type in the top three bits, target or
//!   context in the low five), Mumble varints, the Opus size with the terminator
//!   bit 0x2000, the payload and optionally three native-endian floats.
//! - Protobuf (1.5+): a type byte (0 audio, 1 ping) and a `MumbleUDP` message.

use crate::{udp, voice::PROTOBUF_VERSION};
use prost::Message as _;

/// Largest plaintext a voice datagram may have (stock Mumble's 1024-byte UDP
/// buffer minus the 4-byte OCB2 header).
pub const MAX_PLAIN: usize = 1020;
/// Opus payload size limit in the legacy size field.
const MAX_OPUS: usize = 0x1fff;

pub const TARGET_NORMAL: u8 = 0;
pub const TARGET_LOOPBACK: u8 = 31;

/// The context a receiver sees, ordered by precedence (lower wins when a
/// receiver qualifies several ways).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Context {
    Normal = 0,
    Shout = 1,
    Whisper = 2,
    Listen = 3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Audio<'a> {
    /// 0 normal speech, 1–30 a voice target, 31 server loopback.
    pub target: u8,
    pub frame_number: u64,
    pub opus: &'a [u8],
    /// Three little-endian `f32`s, kept as bytes (stock clients write native
    /// endianness, which is little-endian on every platform Mumble ships for).
    pub positional: Option<[u8; 12]>,
    pub terminator: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Datagram<'a> {
    Audio(Audio<'a>),
    /// A connectivity ping. Legacy pings are answered by echoing the plaintext.
    LegacyPing,
    Ping {
        timestamp: u64,
        request_extended_information: bool,
    },
}

/// Whether a client of this protocol version uses the protobuf UDP format.
pub fn uses_protobuf(version: u64) -> bool {
    version >= PROTOBUF_VERSION
}

/// Parse a client's decrypted datagram (or tunneled payload).
pub fn parse(protobuf: bool, data: &[u8]) -> Option<Datagram<'_>> {
    let (&header, rest) = data.split_first()?;
    if protobuf {
        match header {
            0 => parse_protobuf_audio(rest).map(Datagram::Audio),
            1 => {
                let ping = udp::Ping::decode(rest).ok()?;
                Some(Datagram::Ping {
                    timestamp: ping.timestamp,
                    request_extended_information: ping.request_extended_information,
                })
            }
            _ => None,
        }
    } else {
        match header >> 5 {
            1 => Some(Datagram::LegacyPing),
            4 => parse_legacy_audio(header & 0x1f, rest).map(Datagram::Audio),
            _ => None,
        }
    }
}

fn parse_legacy_audio(target: u8, mut rest: &[u8]) -> Option<Audio<'_>> {
    let frame_number = read_mumble_varint(&mut rest)?;
    let size = read_mumble_varint(&mut rest)?;
    if size > 0x3fff {
        return None;
    }
    let length = (size & 0x1fff) as usize;
    if rest.len() < length {
        return None;
    }
    let (opus, trailer) = rest.split_at(length);
    let positional = match trailer.len() {
        0 => None,
        12 => Some(trailer.try_into().ok()?),
        _ => return None,
    };
    Some(Audio {
        target,
        frame_number,
        opus,
        positional,
        terminator: size & 0x2000 != 0,
    })
}

/// `MumbleUDP.Audio` without allocating. Scalars follow protobuf's last-one-wins
/// rule; unknown fields are skipped; groups and malformed lengths are rejected.
fn parse_protobuf_audio(mut data: &[u8]) -> Option<Audio<'_>> {
    let mut audio = Audio {
        target: 0,
        frame_number: 0,
        opus: &[],
        positional: None,
        terminator: false,
    };
    let mut floats = [0_u8; 12];
    let mut float_count = 0;
    while !data.is_empty() {
        let tag = read_leb128(&mut data)?;
        let (field, wire) = (tag >> 3, tag & 7);
        match (field, wire) {
            (1, 0) => {
                audio.target = u8::try_from(read_leb128(&mut data)?)
                    .ok()
                    .filter(|t| *t < 32)?
            }
            (4, 0) => audio.frame_number = read_leb128(&mut data)?,
            (5, 2) => audio.opus = read_bytes(&mut data)?,
            (6, 2) => {
                let packed = read_bytes(&mut data)?;
                if packed.len() % 4 != 0 || float_count * 4 + packed.len() > 12 {
                    return None;
                }
                floats[float_count * 4..float_count * 4 + packed.len()].copy_from_slice(packed);
                float_count += packed.len() / 4;
            }
            (6, 5) => {
                let value = read_fixed(&mut data, 4)?;
                if float_count == 3 {
                    return None;
                }
                floats[float_count * 4..float_count * 4 + 4].copy_from_slice(value);
                float_count += 1;
            }
            (16, 0) => audio.terminator = read_leb128(&mut data)? != 0,
            (_, 0) => {
                read_leb128(&mut data)?;
            }
            (_, 1) => {
                read_fixed(&mut data, 8)?;
            }
            (_, 2) => {
                read_bytes(&mut data)?;
            }
            (_, 5) => {
                read_fixed(&mut data, 4)?;
            }
            _ => return None,
        }
    }
    match float_count {
        0 => {}
        3 => audio.positional = Some(floats),
        _ => return None,
    }
    if audio.opus.len() > MAX_OPUS {
        return None;
    }
    Some(audio)
}

/// What one receiver gets: the sender, the receiver's context and volume
/// adjustment (1.0 means none), and whether positional data is passed on. The
/// caller sets `positional` only when sender and receiver share a plugin
/// context: positional data reveals a player's in-game location.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Delivery {
    pub sender: u32,
    pub context: Context,
    pub volume: f32,
    pub positional: bool,
}

/// Encode `audio` for one receiver into `out`, returning the length, or `None`
/// if it does not fit [`MAX_PLAIN`].
pub fn encode(protobuf: bool, audio: &Audio, delivery: Delivery, out: &mut [u8]) -> Option<usize> {
    let out = out.get_mut(..MAX_PLAIN.min(out.len()))?;
    let mut writer = Writer { out, at: 0 };
    let positional = audio.positional.filter(|_| delivery.positional);
    if protobuf {
        writer.byte(0)?;
        writer.leb128(2 << 3)?;
        writer.leb128(delivery.context as u64)?;
        writer.leb128(3 << 3)?;
        writer.leb128(u64::from(delivery.sender))?;
        writer.leb128(4 << 3)?;
        writer.leb128(audio.frame_number)?;
        writer.leb128((5 << 3) | 2)?;
        writer.leb128(audio.opus.len() as u64)?;
        writer.bytes(audio.opus)?;
        if let Some(position) = positional {
            writer.leb128((6 << 3) | 2)?;
            writer.leb128(12)?;
            writer.bytes(&position)?;
        }
        if delivery.volume != 1. {
            writer.leb128((7 << 3) | 5)?;
            writer.bytes(&delivery.volume.to_le_bytes())?;
        }
        if audio.terminator {
            writer.leb128(16 << 3)?;
            writer.leb128(1)?;
        }
    } else {
        writer.byte((4 << 5) | delivery.context as u8)?;
        writer.mumble_varint(u64::from(delivery.sender))?;
        writer.mumble_varint(audio.frame_number)?;
        if audio.opus.len() > MAX_OPUS {
            return None;
        }
        let size = audio.opus.len() as u64 | if audio.terminator { 0x2000 } else { 0 };
        writer.mumble_varint(size)?;
        writer.bytes(audio.opus)?;
        if let Some(position) = positional {
            writer.bytes(&position)?;
        }
    }
    Some(writer.at)
}

/// The answer to a protobuf connectivity ping.
pub fn encode_ping(timestamp: u64, out: &mut [u8]) -> Option<usize> {
    let mut writer = Writer { out, at: 0 };
    writer.byte(1)?;
    if timestamp != 0 {
        writer.leb128(1 << 3)?;
        writer.leb128(timestamp)?;
    }
    Some(writer.at)
}

/// Public server information for the server browser, answered to unencrypted pings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// Version in the 1.5 format (`major << 48 | minor << 32 | patch << 16`).
    pub version: u64,
    pub users: u32,
    pub max_users: u32,
    pub max_bandwidth: u32,
}

/// Decides whether an unencrypted ping is answered: the server's per-prefix and
/// global rate limits and its amplification policy. Taken as an argument so a
/// caller cannot forget it.
pub trait PingBudget {
    fn allow(&mut self, request_length: usize, reply_length: usize) -> bool;
}

/// Shortest protobuf info ping answered. Stock 1.5 clients send a randomised
/// timestamp, so their requests are almost always 12 to 14 bytes; shorter ones
/// would let three bytes reflect into about seventeen.
pub const MIN_PROTOBUF_INFO_PING: usize = 12;

/// Answer an unencrypted ping from someone without a session: the legacy
/// 12-byte request (four zero bytes and an opaque 8-byte timestamp) gets the
/// 24-byte reply; a protobuf ping of at least [`MIN_PROTOBUF_INFO_PING`] bytes
/// asking for extended information gets a protobuf reply. A reply is never more
/// than twice the request, and only sent if `budget` allows it.
pub fn answer_info_ping(
    request: &[u8],
    info: ServerInfo,
    budget: &mut impl PingBudget,
    out: &mut [u8; 64],
) -> Option<usize> {
    let length = if request.len() == 12 && request[..4] == [0; 4] {
        let legacy = ((info.version >> 48) as u32) << 16
            | (((info.version >> 32) & 0xffff) as u32).min(255) << 8
            | (((info.version >> 16) & 0xffff) as u32).min(255);
        out[..4].copy_from_slice(&legacy.to_be_bytes());
        out[4..12].copy_from_slice(&request[4..12]);
        out[12..16].copy_from_slice(&info.users.to_be_bytes());
        out[16..20].copy_from_slice(&info.max_users.to_be_bytes());
        out[20..24].copy_from_slice(&info.max_bandwidth.to_be_bytes());
        24
    } else {
        let (&1, body) = request.split_first()? else {
            return None;
        };
        if request.len() < MIN_PROTOBUF_INFO_PING {
            return None;
        }
        let ping = udp::Ping::decode(body).ok()?;
        if !ping.request_extended_information {
            return None;
        }
        let mut reply = udp::Ping {
            timestamp: ping.timestamp,
            request_extended_information: false,
            server_version_v2: info.version,
            user_count: info.users,
            max_user_count: info.max_users,
            max_bandwidth_per_user: info.max_bandwidth,
        };
        // The reply echoes the client's (randomised) timestamp, so it is about
        // sixteen bytes longer than the request; drop the least useful fields
        // until it fits twice the request. The version must stay: it is what
        // marks a reply as carrying server information.
        if 1 + reply.encoded_len() > 2 * request.len() {
            reply.max_bandwidth_per_user = 0;
        }
        if 1 + reply.encoded_len() > 2 * request.len() {
            reply.max_user_count = 0;
        }
        out[0] = 1;
        let mut rest = &mut out[1..];
        let before = rest.len();
        reply.encode(&mut rest).ok()?;
        1 + before - rest.len()
    };
    (length <= 2 * request.len() && budget.allow(request.len(), length)).then_some(length)
}

struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn byte(&mut self, byte: u8) -> Option<()> {
        *self.out.get_mut(self.at)? = byte;
        self.at += 1;
        Some(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Option<()> {
        self.out
            .get_mut(self.at..self.at + bytes.len())?
            .copy_from_slice(bytes);
        self.at += bytes.len();
        Some(())
    }

    fn leb128(&mut self, mut value: u64) -> Option<()> {
        loop {
            if value < 0x80 {
                return self.byte(value as u8);
            }
            self.byte(value as u8 | 0x80)?;
            value >>= 7;
        }
    }

    fn mumble_varint(&mut self, value: u64) -> Option<()> {
        if value < 0x80 {
            self.byte(value as u8)
        } else if value < 0x4000 {
            self.bytes(&[(value >> 8) as u8 | 0x80, value as u8])
        } else if value < 0x20_0000 {
            self.bytes(&[(value >> 16) as u8 | 0xc0, (value >> 8) as u8, value as u8])
        } else if value < 0x1000_0000 {
            self.bytes(&[
                (value >> 24) as u8 | 0xe0,
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ])
        } else if value <= u64::from(u32::MAX) {
            self.byte(0xf0)?;
            self.bytes(&(value as u32).to_be_bytes())
        } else {
            self.byte(0xf4)?;
            self.bytes(&value.to_be_bytes())
        }
    }
}

fn read_mumble_varint(bytes: &mut &[u8]) -> Option<u64> {
    let (&first, rest) = bytes.split_first()?;
    let (mut value, count) = match first {
        0..=0x7f => (u64::from(first), 0),
        0x80..=0xbf => (u64::from(first & 0x3f), 1),
        0xc0..=0xdf => (u64::from(first & 0x1f), 2),
        0xe0..=0xef => (u64::from(first & 0xf), 3),
        0xf0 => (0, 4),
        0xf4 => (0, 8),
        // Negative numbers (0xf8, 0xfc) are never valid sequence numbers or sizes.
        _ => return None,
    };
    let (digits, rest) = rest.split_at_checked(count)?;
    for byte in digits {
        value = (value << 8) | u64::from(*byte);
    }
    *bytes = rest;
    Some(value)
}

fn read_leb128(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            *bytes = &bytes[index + 1..];
            return Some(value);
        }
    }
    None
}

fn read_bytes<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = usize::try_from(read_leb128(bytes)?).ok()?;
    let (value, rest) = bytes.split_at_checked(length)?;
    *bytes = rest;
    Some(value)
}

fn read_fixed<'a>(bytes: &mut &'a [u8], width: usize) -> Option<&'a [u8]> {
    let (value, rest) = bytes.split_at_checked(width)?;
    *bytes = rest;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSITION: [u8; 12] = [0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64];

    fn delivery(context: Context) -> Delivery {
        Delivery {
            sender: 300,
            context,
            volume: 1.,
            positional: true,
        }
    }

    #[test]
    fn legacy_client_packet_is_relayed_with_the_sender_session() {
        // Opus, target 5, sequence 128, terminator, three bytes, position.
        let mut packet = vec![0x85, 0x80, 0x80, 0xa0, 3, 0xf8, 0xff, 0xfe];
        packet.extend_from_slice(&POSITION);
        let Some(Datagram::Audio(audio)) = parse(false, &packet) else {
            panic!("not audio");
        };
        assert_eq!(
            (
                audio.target,
                audio.frame_number,
                audio.opus,
                audio.terminator
            ),
            (5, 128, &[0xf8, 0xff, 0xfe][..], true)
        );
        assert_eq!(audio.positional, Some(POSITION));
        let mut out = [0; 1024];
        let length = encode(false, &audio, delivery(Context::Whisper), &mut out).unwrap();
        // The client decoder sees the original sender, payload and end flag.
        let decoded = crate::voice::decode_voice(&out[..length]).unwrap();
        assert_eq!(out[0], 0x82);
        assert_eq!((decoded.sender, decoded.sequence), (300, 128));
        assert_eq!(decoded.opus, [0xf8, 0xff, 0xfe]);
        assert!(decoded.end);
        // Without positional delivery the trailer is dropped.
        let mut plain = delivery(Context::Normal);
        plain.positional = false;
        let short = encode(false, &audio, plain, &mut out).unwrap();
        assert_eq!(short, length - 12);
    }

    #[test]
    fn protobuf_packets_match_prost_in_both_directions() {
        let mut packet = vec![0];
        udp::Audio {
            header: Some(udp::audio::Header::Target(7)),
            sender_session: 99,
            frame_number: 1 << 40,
            opus_data: vec![1, 2, 3, 4],
            positional_data: vec![1., 2., 3.],
            volume_adjustment: 0.5,
            is_terminator: true,
        }
        .encode(&mut packet)
        .unwrap();
        let Some(Datagram::Audio(audio)) = parse(true, &packet) else {
            panic!("not audio");
        };
        assert_eq!((audio.target, audio.frame_number), (7, 1 << 40));
        assert_eq!(audio.positional, Some(POSITION));
        let mut out = [0; 1024];
        let mut listen = delivery(Context::Listen);
        listen.volume = 0.25;
        let length = encode(true, &audio, listen, &mut out).unwrap();
        assert_eq!(out[0], 0);
        let sent = udp::Audio::decode(&out[1..length]).unwrap();
        assert_eq!(sent.header, Some(udp::audio::Header::Context(3)));
        assert_eq!(sent.sender_session, 300);
        assert_eq!(sent.frame_number, 1 << 40);
        assert_eq!(sent.opus_data, [1, 2, 3, 4]);
        assert_eq!(sent.positional_data, [1., 2., 3.]);
        assert_eq!(sent.volume_adjustment, 0.25);
        assert!(sent.is_terminator);
    }

    #[test]
    fn formats_convert_between_legacy_and_protobuf_clients() {
        let legacy = [0x80, 10, 2, 0xaa, 0xbb];
        let Some(Datagram::Audio(audio)) = parse(false, &legacy) else {
            panic!("not audio");
        };
        let mut out = [0; 1024];
        let length = encode(true, &audio, delivery(Context::Normal), &mut out).unwrap();
        let decoded = crate::voice::decode_voice(&out[..length]).unwrap();
        assert_eq!(
            (decoded.sender, decoded.sequence, decoded.end),
            (300, 10, false)
        );
        assert_eq!(decoded.opus, [0xaa, 0xbb]);
    }

    #[test]
    fn pings_and_unsupported_codecs() {
        assert_eq!(parse(false, &[0x20, 5]), Some(Datagram::LegacyPing));
        let mut ping = vec![1];
        udp::Ping {
            timestamp: 77,
            ..Default::default()
        }
        .encode(&mut ping)
        .unwrap();
        assert_eq!(
            parse(true, &ping),
            Some(Datagram::Ping {
                timestamp: 77,
                request_extended_information: false
            })
        );
        let mut out = [0; 16];
        let length = encode_ping(77, &mut out).unwrap();
        assert_eq!(&out[..length], &ping[..]);
        // CELT alpha, Speex, CELT beta and unknown protobuf types are dropped.
        for header in [0x00, 0x40, 0x60, 0xa0] {
            assert_eq!(parse(false, &[header, 1, 1, 1]), None);
        }
        assert_eq!(parse(true, &[2, 0]), None);
    }

    struct Unlimited;
    impl PingBudget for Unlimited {
        fn allow(&mut self, _: usize, _: usize) -> bool {
            true
        }
    }

    #[test]
    fn server_browser_pings() {
        let info = ServerInfo {
            version: PROTOBUF_VERSION,
            users: 3,
            max_users: 100,
            max_bandwidth: 72000,
        };
        let mut out = [0; 64];
        let request = [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            answer_info_ping(&request, info, &mut Unlimited, &mut out),
            Some(24)
        );
        assert_eq!(&out[..4], &[0, 1, 5, 0]);
        assert_eq!(&out[4..12], &request[4..]);
        assert_eq!(&out[12..16], &3_u32.to_be_bytes());
        let mut request = vec![1];
        udp::Ping {
            timestamp: 0x1234_5678_9abc_def0,
            request_extended_information: true,
            ..Default::default()
        }
        .encode(&mut request)
        .unwrap();
        assert!(request.len() >= MIN_PROTOBUF_INFO_PING);
        let length = answer_info_ping(&request, info, &mut Unlimited, &mut out).unwrap();
        assert!(length <= 2 * request.len());
        let reply = udp::Ping::decode(&out[1..length]).unwrap();
        assert_eq!(
            (reply.timestamp, reply.user_count, reply.server_version_v2),
            (0x1234_5678_9abc_def0, 3, PROTOBUF_VERSION)
        );
        // Three bytes asking for information (the 6x reflection) get nothing.
        assert_eq!(
            answer_info_ping(&[1, 0x10, 1], info, &mut Unlimited, &mut out),
            None
        );
        // A connectivity ping without the request is not answered unencrypted.
        assert_eq!(
            answer_info_ping(&[1, 8, 9], info, &mut Unlimited, &mut out),
            None
        );
        // The budget has the last word.
        struct Closed;
        impl PingBudget for Closed {
            fn allow(&mut self, _: usize, _: usize) -> bool {
                false
            }
        }
        assert_eq!(
            answer_info_ping(&[0; 12], info, &mut Closed, &mut out),
            None
        );
    }

    #[test]
    fn hostile_datagrams_never_panic_or_overflow() {
        let mut state = 0x1234_5678_9abc_def1_u64;
        let mut out = [0; 1024];
        let mut info_out = [0; 64];
        let info = ServerInfo {
            version: 0,
            users: 0,
            max_users: 0,
            max_bandwidth: 0,
        };
        for round in 0..100_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let length = (state % 80) as usize;
            let mut bytes: Vec<u8> = (0..length)
                .map(|index| (state >> (index % 8 * 8)) as u8 ^ index as u8)
                .collect();
            if let Some(first) = bytes.first_mut() {
                *first = [0, 1, 0x80, 0x85, 0x20, *first][round % 6];
            }
            for protobuf in [false, true] {
                if let Some(Datagram::Audio(audio)) = parse(protobuf, &bytes) {
                    for target in [false, true] {
                        if let Some(length) =
                            encode(target, &audio, delivery(Context::Shout), &mut out)
                        {
                            assert!(length <= MAX_PLAIN);
                        }
                    }
                }
            }
            if let Some(length) = answer_info_ping(&bytes, info, &mut Unlimited, &mut info_out) {
                assert!(length <= 2 * bytes.len());
            }
        }
    }

    #[test]
    fn oversized_packets_do_not_fit() {
        let opus = [0x55; 1015];
        let audio = Audio {
            target: 0,
            frame_number: u64::MAX,
            opus: &opus,
            positional: None,
            terminator: false,
        };
        let mut out = [0; 2048];
        assert_eq!(
            encode(false, &audio, delivery(Context::Normal), &mut out),
            None
        );
        assert_eq!(
            encode(true, &audio, delivery(Context::Normal), &mut out),
            None
        );
    }
}
