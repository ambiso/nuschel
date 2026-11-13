// SPDX-License-Identifier: EUPL-1.2
//! One response on a supervisor-approved transport. No URL, socket, hostname,
//! credentials or executable selection crosses this content-role protocol.
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const SCHEMA: u16 = 1;
pub const MAX_CHUNK_BYTES: usize = 16 * 1024;
pub const MAX_REQUEST_BYTES: usize = 32 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_HEAD_BYTES: usize = 32 * 1024;
pub const MAX_CHUNKS: usize = 512;
pub const MAX_INFORMATIONAL: usize = 4;
pub const RAW_OVERHEAD_BYTES: usize = 96 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Document,
    Image,
    Dns,
}
impl Purpose {
    pub fn body_limit(self) -> usize {
        match self {
            Self::Document => 256 * 1024,
            Self::Image => 2 * 1024 * 1024,
            Self::Dns => 16 * 1024,
        }
    }
    pub fn raw_limit(self) -> usize {
        self.body_limit() + RAW_OVERHEAD_BYTES
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Start { schema: u16, purpose: Purpose },
    Chunk { data: String, eof: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Malformed,
    TooLarge,
    Unsupported,
    Unfinished,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply {
    More,
    Complete(Response),
    Rejected(Failure),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub schema: u16,
    pub status: u16,
    pub location: Option<String>,
    pub content_type: Option<String>,
    /// Base64; check the encoded bound before decoding or allocating its body.
    pub body: String,
}
impl Response {
    pub fn decode_body(&self, purpose: Purpose) -> Result<Vec<u8>, Failure> {
        if self.schema != SCHEMA
            || !(200..=599).contains(&self.status)
            || self
                .location
                .as_ref()
                .is_some_and(|s| s.len() > 2048 || s.chars().any(char::is_control))
            || self
                .content_type
                .as_ref()
                .is_some_and(|s| s.len() > 256 || !s.is_ascii() || s.chars().any(char::is_control))
            || self.body.len() > purpose.body_limit().div_ceil(3) * 4
        {
            return Err(Failure::Malformed);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.body)
            .map_err(|_| Failure::Malformed)?;
        if bytes.len() > purpose.body_limit() {
            return Err(Failure::TooLarge);
        }
        Ok(bytes)
    }
}

pub fn decode_chunk(data: &str) -> Result<Vec<u8>, Failure> {
    if data.len() > MAX_CHUNK_BYTES.div_ceil(3) * 4 {
        return Err(Failure::TooLarge);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| Failure::Malformed)?;
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(Failure::TooLarge);
    }
    Ok(bytes)
}

/// Strict small-frame reader for the dedicated HTTP parser role. The global
/// content frame limit is intentionally not used for streamed transport input.
pub fn read_request(
    reader: &mut impl std::io::Read,
) -> std::io::Result<Option<crate::Envelope<Request>>> {
    use std::io::{Error, ErrorKind};
    let mut size = [0; 4];
    loop {
        match reader.read(&mut size[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    reader.read_exact(&mut size[1..])?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_REQUEST_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "preview parser request frame limit",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    let envelope: crate::Envelope<Request> = serde_json::from_slice(&bytes)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid preview parser request"))?;
    if envelope.version != crate::PROTOCOL_VERSION {
        return Err(Error::new(ErrorKind::InvalidData, "preview parser version"));
    }
    Ok(Some(envelope))
}

pub fn encode_request(id: u64, request: &Request) -> std::io::Result<Vec<u8>> {
    let bytes =
        serde_json::to_vec(&crate::Envelope::new(id, request)).map_err(std::io::Error::other)?;
    framed(bytes, MAX_REQUEST_BYTES)
}
pub fn write_reply(
    writer: &mut impl std::io::Write,
    id: u64,
    reply: &Reply,
) -> std::io::Result<()> {
    let bytes =
        serde_json::to_vec(&crate::Envelope::new(id, reply)).map_err(std::io::Error::other)?;
    writer.write_all(&framed(bytes, MAX_RESPONSE_BYTES)?)?;
    writer.flush()
}
pub fn decode_reply(id: u64, bytes: &[u8]) -> std::io::Result<Reply> {
    use std::io::{Error, ErrorKind};
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "preview parser reply frame limit",
        ));
    }
    let reply: crate::Envelope<Reply> = serde_json::from_slice(bytes)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid preview parser reply"))?;
    if reply.version != crate::PROTOCOL_VERSION || reply.request_id != id {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "preview parser reply identity",
        ));
    }
    Ok(reply.payload)
}
fn framed(bytes: Vec<u8>, cap: usize) -> std::io::Result<Vec<u8>> {
    if bytes.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "preview parser frame limit",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() + 4);
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_and_decoded_limits_and_response_scalars_are_checked() {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        assert!(decode_chunk(&encode(&vec![0; MAX_CHUNK_BYTES])).is_ok());
        assert_eq!(
            decode_chunk(&encode(&vec![0; MAX_CHUNK_BYTES + 1])),
            Err(Failure::TooLarge)
        );
        assert!(decode_chunk("!").is_err());
        let mut reply = Response {
            schema: SCHEMA,
            status: 200,
            location: None,
            content_type: Some("text/html; charset=utf-8".into()),
            body: encode(b"page"),
        };
        assert_eq!(reply.decode_body(Purpose::Document).unwrap(), b"page");
        reply.schema += 1;
        assert!(reply.decode_body(Purpose::Document).is_err());
        reply.schema = SCHEMA;
        reply.status = 101;
        assert!(reply.decode_body(Purpose::Document).is_err());
        reply.status = 200;
        reply.location = Some("https://example.org/\r\nx".into());
        assert!(reply.decode_body(Purpose::Document).is_err());
        reply.location = None;
        reply.body = encode(&vec![0; Purpose::Dns.body_limit() + 1]);
        assert!(reply.decode_body(Purpose::Dns).is_err());
    }
}
