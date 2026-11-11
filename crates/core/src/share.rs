// SPDX-License-Identifier: EUPL-1.2
//! Share links (docs/web.md, "Share links"): a link you send someone that opens
//! Nuschel if it is installed, and otherwise the web host's landing page.
//!
//! ```text
//! https://<web host>/join/<token>#s=<host>[:<port>]&c=<channel path>&fp=sha256:<hex>&policy=nuschel&n=<name>
//! nuschel://join/<token>#<the same fragment>
//! ```
//!
//! - The **token** is opaque: issued by a Nuschel server (`share/1`), which
//!   resolves it to a channel (and, for a private channel, an access token)
//!   after the client connects. Links to stock Murmur, or to a channel by its
//!   path, have no token (`/join/#…`) and name the channel in `c`.
//! - The **fragment** is never sent to the web host: the server's address, its
//!   certificate pin and policy (as in `mumble://` invites), the channel path and
//!   a display name. Values are percent-encoded; `+` is a space, as browsers'
//!   `URLSearchParams` read it.
//!
//! The landing page (web/site/join.js) reads the same format.

/// The scheme the desktop registers next to `mumble://`.
pub const SCHEME: &str = "nuschel";
/// The default Mumble port, left out of links.
pub const MUMBLE_PORT: u16 = 64738;
/// The project's web host (docs/clients.md: nuschl.eu, decided with the user),
/// for links whose server and user name none.
pub const PROJECT_WEB_HOST: Option<&str> = Some("nuschl.eu");
/// Token bytes the server issues (22 characters of base64url).
pub const TOKEN_BYTES: usize = 16;
/// The longest token accepted in a link.
pub const MAX_TOKEN_CHARS: usize = 64;
/// The longest link accepted (browsers keep far longer fragments, but
/// a link is something people paste).
pub const MAX_LINK_BYTES: usize = 8192;
const MAX_CHANNEL_DEPTH: usize = 64;
const MAX_CHANNEL_NAME: usize = 512;
const MAX_NAME: usize = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareLink {
    /// The server's token, if it issued one.
    pub token: Option<String>,
    pub host: String,
    pub port: u16,
    /// Channel names from the root, for links without a token.
    pub channel: Vec<String>,
    /// The server certificate's SHA-256, lowercase hex.
    pub certificate_sha256: Option<String>,
    /// The server must speak the Nuschel protocol (`policy=nuschel`).
    pub require_nuschel: bool,
    /// What the landing page calls the server.
    pub name: Option<String>,
}

impl ShareLink {
    /// Parse `https://<web host>/join/<token>#…`, `http://…` (local hosts) or
    /// `nuschel://join/<token>#…`. Any web host is accepted: the link names the
    /// server itself, and the client checks the server as for any connection.
    pub fn parse(url: &str) -> Result<Self, String> {
        let invalid = || "not a Nuschel share link".to_string();
        if url.len() > MAX_LINK_BYTES {
            return Err(invalid());
        }
        let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
        let path = if scheme.eq_ignore_ascii_case(SCHEME) {
            rest
        } else if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http") {
            // Skip the web host; the path starts at its first slash.
            let slash = rest.find(['/', '#', '?']).ok_or_else(invalid)?;
            rest[slash..].strip_prefix('/').ok_or_else(invalid)?
        } else {
            return Err(invalid());
        };
        let (path, fragment) = path.split_once('#').ok_or_else(invalid)?;
        let path = path.split('?').next().unwrap_or_default();
        let token = path
            .strip_prefix("join/")
            .or_else(|| (path == "join").then_some(""))
            .ok_or_else(invalid)?
            .trim_end_matches('/');
        let token = if token.is_empty() {
            None
        } else if valid_token(token) {
            Some(token.to_owned())
        } else {
            return Err("the share link's token is malformed".into());
        };
        let mut link = Self {
            token,
            port: MUMBLE_PORT,
            ..Self::default()
        };
        let mut server = None;
        let mut seen = 0_u8;
        for pair in fragment.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let name = decode(name)?;
            let value = decode(value)?;
            let option = match name.as_str() {
                "s" => 1,
                "c" => 2,
                "fp" => 4,
                "policy" => 8,
                "n" => 16,
                _ => 0,
            };
            if seen & option != 0 {
                return Err("the share link repeats an option".into());
            }
            seen |= option;
            match name.as_str() {
                "s" => server = Some(value),
                "c" => {
                    link.channel = value
                        .split('/')
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned)
                        .collect();
                    if link.channel.len() > MAX_CHANNEL_DEPTH
                        || link
                            .channel
                            .iter()
                            .any(|part| part.len() > MAX_CHANNEL_NAME)
                    {
                        return Err("the share link's channel path is too long".into());
                    }
                }
                "fp" => {
                    let hex = value
                        .strip_prefix("sha256:")
                        .filter(|hex| hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit()))
                        .ok_or("the share link's fp= must be sha256: and 64 hex digits")?;
                    link.certificate_sha256 = Some(hex.to_ascii_lowercase());
                }
                "policy" => {
                    for policy in value.split(['+', ' ']).filter(|part| !part.is_empty()) {
                        if policy != "nuschel" {
                            return Err("the share link requires an unsupported policy".into());
                        }
                        link.require_nuschel = true;
                    }
                }
                "n" => {
                    let name: String = value.chars().take(MAX_NAME).collect();
                    link.name = Some(name).filter(|name| !name.trim().is_empty());
                }
                _ => {}
            }
        }
        let server = server.ok_or("the share link names no server")?;
        let (host, port) = split_server(&server).ok_or("the share link's server is malformed")?;
        link.host = host;
        link.port = port;
        // What this link writes must parse again (percent-encoding lengthens it).
        if link.app_url().len() > MAX_LINK_BYTES {
            return Err("the share link is too long".into());
        }
        Ok(link)
    }

    /// The fragment, without `#`.
    pub fn fragment(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let server = if self.port == MUMBLE_PORT {
            host
        } else {
            format!("{host}:{}", self.port)
        };
        let mut parts = vec![format!("s={}", encode(&server, ":[]"))];
        if !self.channel.is_empty() {
            let path: Vec<String> = self.channel.iter().map(|part| encode(part, "")).collect();
            parts.push(format!("c={}", path.join("/")));
        }
        if let Some(pin) = &self.certificate_sha256 {
            parts.push(format!("fp=sha256:{pin}"));
        }
        if self.require_nuschel {
            parts.push("policy=nuschel".into());
        }
        if let Some(name) = &self.name {
            parts.push(format!("n={}", encode(name, "")));
        }
        parts.join("&")
    }

    /// The link to share: the web host's landing page.
    pub fn web_url(&self, web_host: &str) -> String {
        format!(
            "https://{web_host}/join/{}#{}",
            self.token.as_deref().unwrap_or_default(),
            self.fragment()
        )
    }

    /// The same link for the installed app (`nuschel://`).
    pub fn app_url(&self) -> String {
        format!(
            "{SCHEME}://join/{}#{}",
            self.token.as_deref().unwrap_or_default(),
            self.fragment()
        )
    }
}

/// Whether `token` can be a server's share token (base64url, 16–64 characters).
pub fn valid_token(token: &str) -> bool {
    (16..=MAX_TOKEN_CHARS).contains(&token.len())
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A token's 16 bytes as base64url without padding.
pub fn encode_token(bytes: &[u8; TOKEN_BYTES]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(22);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0_u32, |n, (index, byte)| {
            n | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * index) & 63) as usize]));
        }
    }
    out
}

/// The 16 bytes of a token [`encode_token`] wrote.
pub fn decode_token(token: &str) -> Option<[u8; TOKEN_BYTES]> {
    if token.len() != 22 {
        return None;
    }
    let mut bits = 0_u32;
    let mut count = 0;
    let mut out = Vec::with_capacity(TOKEN_BYTES);
    for byte in token.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        bits = bits << 6 | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    // The last character's two spare bits must be zero (one spelling per token).
    if bits != 0 {
        return None;
    }
    out.try_into().ok()
}

fn split_server(text: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        (host, tail.strip_prefix(':'))
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, Some(port)),
            Some(_) => return None,
            None => (text, None),
        }
    };
    let port = match port {
        Some(port) => port.parse::<u16>().ok().filter(|port| *port != 0)?,
        None => MUMBLE_PORT,
    };
    let valid = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_:".contains(&byte));
    valid.then(|| (host.to_ascii_lowercase(), port))
}

fn encode(text: &str, keep: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || b"-._~".contains(&byte)
            || keep.as_bytes().contains(&byte)
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or("invalid percent-encoding in the share link")?;
                out.push(hex);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    let decoded = String::from_utf8(out).map_err(|_| "the share link is not valid UTF-8")?;
    if decoded.chars().any(char::is_control) {
        return Err("the share link contains control characters".into());
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_links_with_and_without_tokens() {
        let token = encode_token(&[7; TOKEN_BYTES]);
        assert_eq!(token.len(), 22);
        assert_eq!(decode_token(&token), Some([7; TOKEN_BYTES]));
        let link = ShareLink {
            token: Some(token.clone()),
            host: "voice.example.org".into(),
            port: 64739,
            channel: Vec::new(),
            certificate_sha256: Some("ab".repeat(32)),
            require_nuschel: true,
            name: Some("Our place & more".into()),
        };
        let web = link.web_url("web.example.org");
        assert_eq!(
            web,
            format!(
                "https://web.example.org/join/{token}#s=voice.example.org:64739&fp=sha256:{}&policy=nuschel&n=Our%20place%20%26%20more",
                "ab".repeat(32)
            )
        );
        assert_eq!(ShareLink::parse(&web), Ok(link.clone()));
        assert_eq!(ShareLink::parse(&link.app_url()), Ok(link));

        // A stock server: everything in the fragment, IPv6, a channel path.
        let stock = ShareLink {
            token: None,
            host: "2001:db8::1".into(),
            port: MUMBLE_PORT,
            channel: vec!["Lobby".into(), "Games / Fun".into()],
            certificate_sha256: None,
            require_nuschel: false,
            name: None,
        };
        let web = stock.web_url("web.example.org");
        assert_eq!(
            web,
            "https://web.example.org/join/#s=[2001:db8::1]&c=Lobby/Games%20%2F%20Fun"
        );
        // Encoded slashes inside a name are separators after decoding; the path
        // is split on the decoded text.
        let parsed = ShareLink::parse(&web).expect("parses");
        assert_eq!(parsed.host, "2001:db8::1");
        assert_eq!(parsed.channel, ["Lobby", "Games ", " Fun"]);
    }

    #[test]
    fn accepts_what_browsers_write() {
        let link = ShareLink::parse(
            "https://Web.Example.org:8443/join/AAAAAAAAAAAAAAAAAAAAAA/?ref=x#s=Voice.Example.org&c=%2FLobby&n=A+B&unknown=1",
        )
        .expect("parses");
        assert_eq!(link.token.as_deref(), Some("AAAAAAAAAAAAAAAAAAAAAA"));
        assert_eq!(link.host, "voice.example.org");
        assert_eq!(link.port, MUMBLE_PORT);
        assert_eq!(link.channel, ["Lobby"]);
        assert_eq!(link.name.as_deref(), Some("A B"));
        assert!(ShareLink::parse("NUSCHEL://join#s=a.b").is_ok());
    }

    #[test]
    fn security_options_are_decoded_once_and_never_ambiguous() {
        for fragment in [
            "policy=nuschel",
            "pol%69cy=nuschel",
            "policy=nuschel+nuschel",
            "policy=nuschel%2Bnuschel",
        ] {
            let link = ShareLink::parse(&format!("nuschel://join/#s=a.b&{fragment}")).unwrap();
            assert!(link.require_nuschel, "{fragment}");
        }
        for fragment in [
            "policy=nuschel&policy=",
            "policy=&pol%69cy=nuschel",
            "policy=nuschel+future",
            "policy=future",
            "s=a.b&%73=other.example",
            "fp=sha256:bad&fp=",
        ] {
            assert!(
                ShareLink::parse(&format!("nuschel://join/#s=a.b&{fragment}")).is_err(),
                "{fragment}"
            );
        }
    }

    #[test]
    fn refuses_broken_links() {
        for url in [
            "mumble://a.b/",
            "https://web/join/AAAAAAAAAAAAAAAAAAAAAA",
            "https://web/other/x#s=a.b",
            "https://web/join/short#s=a.b",
            "https://web/join/AAAAAAAAAAAAAAAAAAAA!A#s=a.b",
            "https://web/join/#c=Lobby",
            "https://web/join/#s=",
            "https://web/join/#s=a.b:0",
            "https://web/join/#s=a.b:70000",
            "https://web/join/#s=a/b",
            "https://web/join/#s=a.b&fp=sha256:12",
            "https://web/join/#s=a.b&c=%0a",
            "https://web/join/#s=a.b&c=%zz",
            "https://web/join/#s=::1",
            "ftp://web/join/#s=a.b",
        ] {
            assert!(ShareLink::parse(url).is_err(), "{url}");
        }
        assert!(ShareLink::parse(&format!("nuschel://join/#s=a.b&c={}", "x/".repeat(65))).is_err());
        // Found by fuzzing: a path of short names that percent-encode to more
        // than a link may hold is refused, so whatever parses writes itself again.
        let wide = "\u{a81b}".repeat(400);
        let long = format!("nuschel://join/#s=a.b&c={wide}/{wide}");
        assert!(long.len() < MAX_LINK_BYTES);
        assert!(ShareLink::parse(&long).is_err());
        assert_eq!(decode_token("AAAAAAAAAAAAAAAAAAAAAB"), None);
        assert_eq!(decode_token("AAAAAAAAAAAAAAAAAAAAA"), None);
        assert!(!valid_token("a b"));
    }
}
