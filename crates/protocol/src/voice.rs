// SPDX-License-Identifier: EUPL-1.2
//! Mumble's plaintext voice packet formats. TLS supplies encryption for TCP tunnels.
use prost::Message;
use thiserror::Error;

use crate::udp;

pub const MAX_VOICE_BYTES: usize = 1020;
pub const PROTOBUF_VERSION: u64 = (1_u64 << 48) | (5_u64 << 32);

#[derive(Clone, Debug, PartialEq)]
pub struct VoicePacket {
    pub sender: u32,
    /// Mumble counts 10 ms frames; a 20 ms Opus packet advances this by two.
    pub sequence: u64,
    pub opus: Vec<u8>,
    pub end: bool,
    pub volume: f32,
}

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("invalid or oversized Mumble voice packet")]
    Invalid,
    #[error("unsupported voice codec or packet type")]
    Unsupported,
}

pub fn encode_voice(
    version: u64,
    target: u32,
    packet: &VoicePacket,
) -> Result<Vec<u8>, VoiceError> {
    if target > 31 || packet.opus.is_empty() || packet.opus.len() > 1000 {
        return Err(VoiceError::Invalid);
    }
    let mut bytes = Vec::with_capacity(packet.opus.len() + 24);
    if version >= PROTOBUF_VERSION {
        bytes.push(0);
        udp::Audio {
            header: Some(udp::audio::Header::Target(target)),
            sender_session: 0,
            frame_number: packet.sequence,
            opus_data: packet.opus.clone(),
            positional_data: Vec::new(),
            volume_adjustment: 0.,
            is_terminator: packet.end,
        }
        .encode(&mut bytes)
        .map_err(|_| VoiceError::Invalid)?;
    } else {
        bytes.push(0x80 | target as u8);
        write_varint(packet.sequence, &mut bytes);
        write_varint(
            packet.opus.len() as u64 | if packet.end { 0x2000 } else { 0 },
            &mut bytes,
        );
        bytes.extend_from_slice(&packet.opus);
    }
    if bytes.len() > MAX_VOICE_BYTES {
        return Err(VoiceError::Invalid);
    }
    Ok(bytes)
}

/// Server-to-client packets include the original sender. Unsupported legacy codecs
/// are ignored by the session; malformed packets never reach the codec worker.
pub fn decode_voice(bytes: &[u8]) -> Result<VoicePacket, VoiceError> {
    if bytes.is_empty() || bytes.len() > MAX_VOICE_BYTES {
        return Err(VoiceError::Invalid);
    }
    if bytes[0] == 0 {
        let audio = udp::Audio::decode(&bytes[1..]).map_err(|_| VoiceError::Invalid)?;
        if audio.opus_data.is_empty()
            || (!audio.positional_data.is_empty() && audio.positional_data.len() != 3)
        {
            return Err(VoiceError::Invalid);
        }
        let volume = if audio.volume_adjustment == 0. {
            1.
        } else {
            audio.volume_adjustment
        };
        if !volume.is_finite() || volume < 0. {
            return Err(VoiceError::Invalid);
        }
        Ok(VoicePacket {
            sender: audio.sender_session,
            sequence: audio.frame_number,
            opus: audio.opus_data,
            end: audio.is_terminator,
            volume: volume.min(4.),
        })
    } else if bytes[0] >> 5 == 4 {
        let mut rest = &bytes[1..];
        let sender = u32::try_from(read_varint(&mut rest)?).map_err(|_| VoiceError::Invalid)?;
        let sequence = read_varint(&mut rest)?;
        let length = read_varint(&mut rest)?;
        if length > 0x3fff {
            return Err(VoiceError::Invalid);
        }
        let size = (length & 0x1fff) as usize;
        if size == 0 || rest.len() < size || !matches!(rest.len() - size, 0 | 12) {
            return Err(VoiceError::Invalid);
        }
        Ok(VoicePacket {
            sender,
            sequence,
            opus: rest[..size].to_vec(),
            end: length & 0x2000 != 0,
            volume: 1.,
        })
    } else {
        Err(VoiceError::Unsupported)
    }
}

fn write_varint(value: u64, bytes: &mut Vec<u8>) {
    if value < 0x80 {
        bytes.push(value as u8);
    } else if value < 0x4000 {
        bytes.extend_from_slice(&[(value >> 8) as u8 | 0x80, value as u8]);
    } else if value < 0x20_0000 {
        bytes.extend_from_slice(&[(value >> 16) as u8 | 0xc0, (value >> 8) as u8, value as u8]);
    } else if value < 0x1000_0000 {
        bytes.extend_from_slice(&[
            (value >> 24) as u8 | 0xe0,
            (value >> 16) as u8,
            (value >> 8) as u8,
            value as u8,
        ]);
    } else if value <= u32::MAX as u64 {
        bytes.push(0xf0);
        bytes.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        bytes.push(0xf4);
        bytes.extend_from_slice(&value.to_be_bytes());
    }
}

fn read_varint(bytes: &mut &[u8]) -> Result<u64, VoiceError> {
    let (&first, remaining) = bytes.split_first().ok_or(VoiceError::Invalid)?;
    *bytes = remaining;
    let (mut value, count) = match first {
        0..=0x7f => (u64::from(first), 0),
        0x80..=0xbf => (u64::from(first & 0x3f), 1),
        0xc0..=0xdf => (u64::from(first & 0x1f), 2),
        0xe0..=0xef => (u64::from(first & 0xf), 3),
        0xf0 => (0, 4),
        0xf4 => (0, 8),
        _ => return Err(VoiceError::Invalid),
    };
    if bytes.len() < count {
        return Err(VoiceError::Invalid);
    }
    for byte in &bytes[..count] {
        value = (value << 8) | u64::from(*byte);
    }
    *bytes = &bytes[count..];
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_stock_legacy_packet_and_uses_mumble_varints() {
        let packet =
            decode_voice(&[0x80, 0x81, 0x23, 0x80, 0x80, 0xa0, 3, 0xf8, 0xff, 0xfe]).unwrap();
        assert_eq!(packet.sender, 0x123);
        assert_eq!(packet.sequence, 128);
        assert_eq!(packet.opus, [0xf8, 0xff, 0xfe]);
        assert!(packet.end);
        assert_eq!(
            encode_voice(0, 0, &packet).unwrap(),
            [0x80, 0x80, 0x80, 0xa0, 3, 0xf8, 0xff, 0xfe]
        );
        for value in [0, 127, 128, 16383, 16384, 0x1f_ffff, 0x1000_0000, u64::MAX] {
            let mut bytes = Vec::new();
            write_varint(value, &mut bytes);
            assert_eq!(read_varint(&mut bytes.as_slice()).unwrap(), value);
        }
    }
    #[test]
    fn protobuf_packets_include_header_and_original_sender() {
        let mut bytes = vec![0];
        udp::Audio {
            sender_session: 42,
            frame_number: 100,
            opus_data: vec![0xf8, 0xff, 0xfe],
            is_terminator: true,
            header: Some(udp::audio::Header::Context(0)),
            ..Default::default()
        }
        .encode(&mut bytes)
        .unwrap();
        let packet = decode_voice(&bytes).unwrap();
        assert_eq!(packet.sender, 42);
        assert!(packet.end);
        let sent = encode_voice(PROTOBUF_VERSION, 31, &packet).unwrap();
        assert_eq!(sent[0], 0);
        let sent = udp::Audio::decode(&sent[1..]).unwrap();
        assert_eq!(sent.header, Some(udp::audio::Header::Target(31)));
        assert_eq!(sent.sender_session, 0);
    }
    #[test]
    fn rejects_truncated_lengths_and_non_finite_volume() {
        for packet in [&[][..], &[0x80, 1, 0xf4, 0], &[0x80, 1, 0, 100, 1]] {
            assert!(decode_voice(packet).is_err());
        }
        let mut bytes = vec![0];
        udp::Audio {
            opus_data: vec![1],
            volume_adjustment: f32::NAN,
            ..Default::default()
        }
        .encode(&mut bytes)
        .unwrap();
        assert!(decode_voice(&bytes).is_err());
    }
}
