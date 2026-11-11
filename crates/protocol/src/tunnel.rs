// SPDX-License-Identifier: EUPL-1.2
//! The web client's tunnel (docs/web.md, "The tunnel relay"): browsers cannot
//! open TCP or UDP sockets, so the web client sends a Mumble or Nuschel
//! connection through a WebSocket to the relay, which connects to the server.
//! TLS runs inside the browser; the relay only ever forwards ciphertext.
//!
//! Every WebSocket message is one binary frame: a type byte and its body.
//!
//! | Type | Name | Direction | Body |
//! |---|---|---|---|
//! | 1 | `Open` | client → relay, first and once | port (u16, big-endian), host length (u8), host (ASCII: a DNS name or an IP literal) |
//! | 2 | `Stream` | both | 1 to [`MAX_STREAM`] bytes of the TCP stream (TLS records) |
//! | 3 | `Datagram` | both | one UDP datagram, 1 to [`MAX_DATAGRAM`] bytes (OCB2-encrypted voice) |
//! | 4 | `Opened` | relay → client, once | flags (u8): bit 0 set if datagrams reach the server over UDP |
//! | 5 | `Closed` | relay → client, last | reason (u8, [`CloseReason`]) |
//!
//! Decoding is strict: unknown types, unknown flags, trailing bytes and
//! out-of-range lengths are errors, and the relay closes the tunnel on any.

/// The WebSocket subprotocol (`Sec-WebSocket-Protocol`) both sides require.
pub const SUBPROTOCOL: &str = "nuschel-tunnel.1";
/// Largest TCP payload in one `Stream` frame.
pub const MAX_STREAM: usize = 64 * 1024;
/// Largest UDP datagram: stock Mumble's 1024-byte UDP buffer.
pub const MAX_DATAGRAM: usize = 1024;
/// Longest host name (RFC 1035) or IP literal.
pub const MAX_HOST: usize = 253;
/// Largest frame on the wire, for WebSocket message limits.
pub const MAX_FRAME: usize = 1 + MAX_STREAM;

const OPEN: u8 = 1;
const STREAM: u8 = 2;
const DATAGRAM: u8 = 3;
const OPENED: u8 = 4;
const CLOSED: u8 = 5;
const FLAG_UDP: u8 = 1;

/// Why the relay closed a tunnel. Each has one fixed text, never the server's
/// or the client's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CloseReason {
    /// The server or the client ended the connection.
    Ended = 1,
    /// The client broke the tunnel protocol.
    Protocol = 2,
    /// The host name did not resolve.
    Unresolved = 3,
    /// Every address is in a refused range (private, loopback, link-local, …).
    AddressRefused = 4,
    /// The port is not one the relay forwards to.
    PortRefused = 5,
    /// The destination did not answer like a Mumble server.
    NotMumble = 6,
    /// The TCP connection to the server failed.
    Unreachable = 7,
    /// Too many new tunnels from this address; try again later.
    RateLimited = 8,
    /// Too many open tunnels (from this address, or on the relay).
    Busy = 9,
    /// Nothing moved for too long, or the handshake took too long.
    Timeout = 10,
}

impl CloseReason {
    pub const ALL: [Self; 10] = [
        Self::Ended,
        Self::Protocol,
        Self::Unresolved,
        Self::AddressRefused,
        Self::PortRefused,
        Self::NotMumble,
        Self::Unreachable,
        Self::RateLimited,
        Self::Busy,
        Self::Timeout,
    ];

    pub fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| *reason as u8 == byte)
    }

    /// What a client shows.
    pub fn text(self) -> &'static str {
        match self {
            Self::Ended => "The connection ended.",
            Self::Protocol => "The relay closed a tunnel it could not understand.",
            Self::Unresolved => "The server's name could not be found.",
            Self::AddressRefused => "The relay does not connect to private or local addresses.",
            Self::PortRefused => "The relay does not connect to this port.",
            Self::NotMumble => "The address did not answer like a Mumble server.",
            Self::Unreachable => "The server could not be reached.",
            Self::RateLimited => "Too many connections from your address; try again shortly.",
            Self::Busy => "The relay has too many connections; try again later.",
            Self::Timeout => "The connection timed out.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Open { host: &'a str, port: u16 },
    Stream(&'a [u8]),
    Datagram(&'a [u8]),
    Opened { udp: bool },
    Closed(CloseReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Empty,
    UnknownType(u8),
    Length,
    Host,
    Flags,
    Reason,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty tunnel frame",
            Self::UnknownType(_) => "unknown tunnel frame type",
            Self::Length => "tunnel frame length out of range",
            Self::Host => "invalid host in tunnel Open",
            Self::Flags => "unknown flags in tunnel Opened",
            Self::Reason => "unknown reason in tunnel Closed",
        })
    }
}

impl std::error::Error for Error {}

/// Parse one WebSocket message.
pub fn decode(message: &[u8]) -> Result<Frame<'_>, Error> {
    let (&kind, body) = message.split_first().ok_or(Error::Empty)?;
    match kind {
        OPEN => {
            let [high, low, length, host @ ..] = body else {
                return Err(Error::Length);
            };
            if host.len() != usize::from(*length) {
                return Err(Error::Length);
            }
            let host = std::str::from_utf8(host).map_err(|_| Error::Host)?;
            if !valid_host(host) {
                return Err(Error::Host);
            }
            Ok(Frame::Open {
                host,
                port: u16::from_be_bytes([*high, *low]),
            })
        }
        STREAM if (1..=MAX_STREAM).contains(&body.len()) => Ok(Frame::Stream(body)),
        DATAGRAM if (1..=MAX_DATAGRAM).contains(&body.len()) => Ok(Frame::Datagram(body)),
        STREAM | DATAGRAM => Err(Error::Length),
        OPENED => match body {
            [flags] if flags & !FLAG_UDP == 0 => Ok(Frame::Opened {
                udp: flags & FLAG_UDP != 0,
            }),
            [_] => Err(Error::Flags),
            _ => Err(Error::Length),
        },
        CLOSED => match body {
            [reason] => CloseReason::from_byte(*reason)
                .map(Frame::Closed)
                .ok_or(Error::Reason),
            _ => Err(Error::Length),
        },
        other => Err(Error::UnknownType(other)),
    }
}

/// Encode one frame; `None` if it breaks a limit [`decode`] enforces.
pub fn encode(frame: &Frame<'_>) -> Option<Vec<u8>> {
    match *frame {
        Frame::Open { host, port } => {
            if !valid_host(host) {
                return None;
            }
            let mut out = Vec::with_capacity(4 + host.len());
            out.push(OPEN);
            out.extend_from_slice(&port.to_be_bytes());
            out.push(u8::try_from(host.len()).ok()?);
            out.extend_from_slice(host.as_bytes());
            Some(out)
        }
        Frame::Stream(bytes) => {
            ((1..=MAX_STREAM).contains(&bytes.len())).then(|| prefixed(STREAM, bytes))
        }
        Frame::Datagram(bytes) => {
            ((1..=MAX_DATAGRAM).contains(&bytes.len())).then(|| prefixed(DATAGRAM, bytes))
        }
        Frame::Opened { udp } => Some(vec![OPENED, if udp { FLAG_UDP } else { 0 }]),
        Frame::Closed(reason) => Some(vec![CLOSED, reason as u8]),
    }
}

fn prefixed(kind: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + bytes.len());
    out.push(kind);
    out.extend_from_slice(bytes);
    out
}

/// A DNS name (letters, digits, hyphens and underscores in dot-separated labels
/// of 1–63 bytes, no leading or trailing hyphen) or an IP literal without
/// brackets or a zone.
pub fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOST {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    })
}

/// Whether `bytes` start like a TLS ClientHello: a handshake record of a TLS
/// version, whose first handshake message is a ClientHello. Needs at least six
/// bytes; `None` until then.
pub fn starts_like_client_hello(bytes: &[u8]) -> Option<bool> {
    let header: &[u8; 6] = bytes.get(..6)?.try_into().ok()?;
    Some(header[0] == 0x16 && header[1] == 3 && header[2] <= 4 && header[5] == 1)
}

/// Whether `bytes` start like a TLS server's first answer: a handshake record
/// with a ServerHello, or an alert. Needs at least six bytes for a handshake
/// (`None` until then).
pub fn starts_like_server_hello(bytes: &[u8]) -> Option<bool> {
    let (&content, rest) = bytes.split_first()?;
    match content {
        0x15 => Some(rest.first().is_none_or(|&major| major == 3)),
        0x16 => {
            let header: &[u8; 6] = bytes.get(..6)?.try_into().ok()?;
            Some(header[1] == 3 && header[2] <= 4 && header[5] == 2)
        }
        _ => Some(false),
    }
}

/// The 12-byte unencrypted ping every Mumble server answers (four zero bytes
/// and an opaque 8-byte value it echoes).
pub fn mumble_ping(ident: u64) -> [u8; 12] {
    let mut ping = [0; 12];
    ping[4..].copy_from_slice(&ident.to_be_bytes());
    ping
}

/// Whether `reply` is a server's 24-byte answer to [`mumble_ping`] with `ident`.
pub fn is_ping_reply(reply: &[u8], ident: u64) -> bool {
    reply.len() == 24 && reply[4..12] == ident.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let stream = [0x16, 3, 1, 0, 5, 1];
        for frame in [
            Frame::Open {
                host: "voice.example.org",
                port: 64738,
            },
            Frame::Open {
                host: "2001:db8::1",
                port: 1,
            },
            Frame::Stream(&stream),
            Frame::Datagram(&[7; MAX_DATAGRAM]),
            Frame::Opened { udp: true },
            Frame::Opened { udp: false },
            Frame::Closed(CloseReason::NotMumble),
        ] {
            let bytes = encode(&frame).expect("encodable");
            assert_eq!(decode(&bytes), Ok(frame));
        }
        for reason in CloseReason::ALL {
            assert_eq!(CloseReason::from_byte(reason as u8), Some(reason));
            assert!(!reason.text().is_empty());
        }
    }

    #[test]
    fn hostile_frames_are_errors() {
        assert_eq!(decode(&[]), Err(Error::Empty));
        assert_eq!(decode(&[9]), Err(Error::UnknownType(9)));
        assert_eq!(decode(&[STREAM]), Err(Error::Length));
        assert_eq!(
            decode(&prefixed(STREAM, &[0; MAX_STREAM + 1])),
            Err(Error::Length)
        );
        assert_eq!(
            decode(&prefixed(DATAGRAM, &[0; MAX_DATAGRAM + 1])),
            Err(Error::Length)
        );
        assert_eq!(decode(&[OPENED, 2]), Err(Error::Flags));
        assert_eq!(decode(&[OPENED, 1, 0]), Err(Error::Length));
        assert_eq!(decode(&[CLOSED, 0]), Err(Error::Reason));
        assert_eq!(decode(&[CLOSED, 1, 1]), Err(Error::Length));
        // Open: the length byte must match, the host must be a host.
        assert_eq!(decode(&[OPEN, 0, 1, 3, b'a', b'b']), Err(Error::Length));
        assert_eq!(decode(&[OPEN, 0, 1, 1, b'a', b'b']), Err(Error::Length));
        assert_eq!(decode(&[OPEN, 0, 1]), Err(Error::Length));
        for host in [
            "",
            "a..b",
            "-a.b",
            "a-.b",
            "a b",
            "[::1]",
            "fe80::1%eth0",
            "a/b",
            "a:1",
        ] {
            assert!(!valid_host(host), "{host:?}");
            assert_eq!(encode(&Frame::Open { host, port: 1 }), None);
        }
        let long = vec!["a".repeat(63); 4].join(".");
        assert!(long.len() > MAX_HOST);
        assert!(!valid_host(&long));
        assert!(valid_host("_srv.example-1.org"));
        assert_eq!(decode(&[OPEN, 0, 1, 2, 0xff, 0xfe]), Err(Error::Host));
    }

    #[test]
    fn tls_shapes() {
        // A real TLS 1.3 ClientHello starts with a TLS 1.0 record version.
        assert_eq!(
            starts_like_client_hello(&[0x16, 3, 1, 2, 0, 1, 0]),
            Some(true)
        );
        assert_eq!(starts_like_client_hello(&[0x16, 3, 1]), None);
        assert_eq!(starts_like_client_hello(b"GET / HTTP/1.1\r\n"), Some(false));
        assert_eq!(
            starts_like_client_hello(&[0x16, 3, 3, 0, 5, 2]),
            Some(false)
        );
        assert_eq!(
            starts_like_server_hello(&[0x16, 3, 3, 0, 90, 2]),
            Some(true)
        );
        assert_eq!(
            starts_like_server_hello(&[0x15, 3, 3, 0, 2, 2, 70]),
            Some(true)
        );
        assert_eq!(starts_like_server_hello(&[0x15]), Some(true));
        assert_eq!(starts_like_server_hello(&[0x16, 3]), None);
        assert_eq!(starts_like_server_hello(b"SSH-2.0-OpenSSH"), Some(false));
        assert_eq!(starts_like_server_hello(b"220 smtp ready"), Some(false));
        assert_eq!(starts_like_server_hello(&[]), None);
    }

    #[test]
    fn pings() {
        let ping = mumble_ping(0x0102_0304_0506_0708);
        assert_eq!(&ping[..4], &[0; 4]);
        let mut reply = [0_u8; 24];
        reply[4..12].copy_from_slice(&ping[4..]);
        assert!(is_ping_reply(&reply, 0x0102_0304_0506_0708));
        assert!(!is_ping_reply(&reply, 1));
        assert!(!is_ping_reply(&reply[..23], 0x0102_0304_0506_0708));
    }
}
