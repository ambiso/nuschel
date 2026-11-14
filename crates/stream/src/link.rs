// SPDX-License-Identifier: EUPL-1.2
//! Watch and share links (docs/screen-share-plan.md §2), and the keys derived from the
//! link secret S:
//!
//! ```text
//! https://<host>/watch#v=1&k=<S>          posted to the channel
//! https://<host>/share#v=1&k=<S>&p=<P>    opened by the sharer only
//! ```
//!
//! S and P are 32 bytes in unpadded base64url. The host is the configured stream host,
//! in lowercase, with an optional port. Parsing accepts one canonical form of at most
//! [`Link::MAX_LEN`] bytes, on that host only: page code served from the link's origin
//! sees S.

use std::fmt;

use base64ct::{Base64UrlUnpadded, Encoding as _};
use rand_core::TryCryptoRng;

use crate::{Error, Secret, hkdf_expand, hkdf_extract};

pub(crate) const LABEL: &str = "nuschl/stream/v2";
const ENCODED_SECRET_LEN: usize = 43;

/// A parsed watch or share link. `Debug` leaves out the secrets.
#[derive(PartialEq, Eq)]
pub struct Link {
    host: String,
    secret: Secret,
    publisher: Option<Secret>,
}

impl Link {
    pub const MAX_LEN: usize = 512;

    /// A new share link on the stream host (`host[:port]`) with fresh S and P.
    pub fn generate<R: TryCryptoRng + ?Sized>(host: &str, rng: &mut R) -> Result<Self, Error> {
        check_host(host)?;
        Ok(Self {
            host: host.to_owned(),
            secret: Secret::random(rng)?,
            publisher: Some(Secret::random(rng)?),
        })
    }

    /// Parses a link on the stream host `host`.
    pub fn parse(link: &str, host: &str) -> Result<Self, Error> {
        if link.len() > Self::MAX_LEN {
            return Err(Error::TooLong);
        }
        check_host(host)?;
        let page = link
            .strip_prefix("https://")
            .and_then(|rest| rest.strip_prefix(host))
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or(Error::Malformed("link origin"))?;
        let (page, fragment) = page
            .split_once('#')
            .ok_or(Error::Malformed("link fragment"))?;
        let share = match page {
            "watch" => false,
            "share" => true,
            _ => return Err(Error::Malformed("link page")),
        };
        let mut params = fragment.split('&');
        match params.next() {
            Some("v=1") => {}
            Some(version) if version.starts_with("v=") => return Err(Error::UnsupportedVersion),
            _ => return Err(Error::Malformed("link version")),
        }
        let secret = decode_param(params.next(), "k=")?;
        let publisher = share
            .then(|| decode_param(params.next(), "p="))
            .transpose()?;
        if params.next().is_some() {
            return Err(Error::Malformed("link parameters"));
        }
        Ok(Self {
            host: host.to_owned(),
            secret,
            publisher,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn secret(&self) -> &Secret {
        &self.secret
    }

    /// P, for share links only.
    pub fn publisher(&self) -> Option<&Secret> {
        self.publisher.as_ref()
    }

    /// The link to post: S only.
    pub fn watch_url(&self) -> String {
        format!("https://{}/watch#v=1&k={}", self.host, encode(&self.secret))
    }

    pub fn share_url(&self) -> Option<String> {
        let publisher = self.publisher.as_ref()?;
        Some(format!(
            "https://{}/share#v=1&k={}&p={}",
            self.host,
            encode(&self.secret),
            encode(publisher)
        ))
    }

    pub fn keys(&self) -> LinkKeys {
        LinkKeys::derive(&self.secret)
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("host", &self.host)
            .field("share", &self.publisher.is_some())
            .finish_non_exhaustive()
    }
}

/// Lowercase DNS labels, then an optional `:port` of 1–5 digits.
fn check_host(host: &str) -> Result<(), Error> {
    let (name, port) = host.split_once(':').unwrap_or((host, "1"));
    let label = |label: &str| {
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let port_ok = (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit());
    if name.split('.').all(label) && port_ok {
        Ok(())
    } else {
        Err(Error::Malformed("link host"))
    }
}

fn decode_param(param: Option<&str>, name: &str) -> Result<Secret, Error> {
    let value = param
        .and_then(|param| param.strip_prefix(name))
        .ok_or(Error::Malformed("link parameters"))?;
    let mut bytes = [0; 32];
    if value.len() != ENCODED_SECRET_LEN
        || Base64UrlUnpadded::decode(value, &mut bytes).map(<[u8]>::len) != Ok(32)
    {
        return Err(Error::Malformed("link secret"));
    }
    Ok(Secret::from_bytes(bytes))
}

fn encode(secret: &Secret) -> String {
    let mut out = [0; ENCODED_SECRET_LEN];
    Base64UrlUnpadded::encode(secret.expose(), &mut out)
        .expect("43 characters encode 32 bytes")
        .to_owned()
}

/// The keys every link holder derives from S. The SFU gets `stream_id` and `admit_key`
/// only.
pub struct LinkKeys {
    stream_id: [u8; 16],
    admit_key: Secret,
    pub(crate) psk: Secret,
    pub(crate) signal_key: Secret,
}

impl LinkKeys {
    pub fn derive(secret: &Secret) -> Self {
        let prk = hkdf_extract(LABEL.as_bytes(), secret.expose());
        let expand = |name: &str| Secret(hkdf_expand(&prk, label(name).as_bytes()));
        Self {
            stream_id: *hkdf_expand(&prk, label("stream id").as_bytes()),
            admit_key: expand("admit"),
            psk: expand("psk"),
            signal_key: expand("signal"),
        }
    }

    /// The SFU room.
    pub fn stream_id(&self) -> &[u8; 16] {
        &self.stream_id
    }

    /// What the SFU checks viewer admission with.
    pub fn admit_key(&self) -> &Secret {
        &self.admit_key
    }

    pub(crate) fn psk_id(&self) -> Vec<u8> {
        [LABEL.as_bytes(), &self.stream_id].concat()
    }
}

impl fmt::Debug for LinkKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkKeys")
            .field("stream_id", &self.stream_id)
            .finish_non_exhaustive()
    }
}

pub(crate) fn label(name: &str) -> String {
    format!("{LABEL} {name}")
}
