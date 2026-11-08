// SPDX-License-Identifier: EUPL-1.2
//! Transport-independent Mumble wire protocol. No audio decoding or network I/O.

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;

pub mod control {
    include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
}

pub mod udp {
    include!(concat!(env!("OUT_DIR"), "/mumble_udp.rs"));
}

pub const HEADER_BYTES: usize = 6;
pub const MAX_CONTROL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlFrame {
    /// Unknown types remain framed data until the session layer decides how to handle them.
    pub kind: u16,
    /// UDPTunnel payloads are raw UDP packets, not serialized UDPTunnel protobufs.
    pub payload: Bytes,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("Mumble control frame exceeds {MAX_CONTROL_BYTES} bytes")]
    TooLarge,
}

/// Decode incrementally without allocating the advertised payload size.
/// The transport adapter must also bound its read buffer and apply a read timeout.
pub fn decode_control(buffer: &mut BytesMut) -> Result<Option<ControlFrame>, Error> {
    if buffer.len() < HEADER_BYTES {
        return Ok(None);
    }
    let kind = u16::from_be_bytes([buffer[0], buffer[1]]);
    let size = u32::from_be_bytes([buffer[2], buffer[3], buffer[4], buffer[5]]) as usize;
    if size > MAX_CONTROL_BYTES {
        return Err(Error::TooLarge);
    }
    if buffer.len() < HEADER_BYTES + size {
        return Ok(None);
    }
    buffer.advance(HEADER_BYTES);
    Ok(Some(ControlFrame {
        kind,
        payload: buffer.split_to(size).freeze(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_fragmented_and_coalesced_control_frames() -> Result<(), Error> {
        let mut buffer = BytesMut::from(&[0, 1, 0, 0, 0, 2, 7][..]);
        assert_eq!(decode_control(&mut buffer)?, None);
        buffer.extend_from_slice(&[8, 0, 4, 0, 0, 0, 0]);
        assert_eq!(
            decode_control(&mut buffer)?,
            Some(ControlFrame {
                kind: 1,
                payload: Bytes::from_static(&[7, 8])
            })
        );
        assert_eq!(
            decode_control(&mut buffer)?,
            Some(ControlFrame {
                kind: 4,
                payload: Bytes::new()
            })
        );
        assert!(buffer.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_an_oversized_header_without_waiting_for_its_body() {
        let mut buffer = BytesMut::from(&[0, 1, 255, 255, 255, 255][..]);
        assert_eq!(decode_control(&mut buffer), Err(Error::TooLarge));
    }
}
