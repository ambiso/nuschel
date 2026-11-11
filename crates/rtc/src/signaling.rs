// SPDX-License-Identifier: EUPL-1.2
use crate::{Error, Kind, MAX_SDP_BYTES};
use std::fmt;

/// Semantic identity supplied by the authenticated call protocol, never by SDP
/// `msid`, track names, RTP arrival order or an untrusted remote display label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Microphone,
    SystemAudio,
    Camera,
    ScreenVideo,
}

impl Purpose {
    pub fn kind(self) -> Kind {
        match self {
            Self::Microphone | Self::SystemAudio => Kind::Audio,
            Self::Camera | Self::ScreenVideo => Kind::Video,
        }
    }
}

/// A fresh, call-attempt-specific identifier allocated by the caller's
/// authenticated signaling protocol. Never reuse it for a subsequent call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalingId([u8; 16]);

impl SignalingId {
    pub fn new(bytes: [u8; 16]) -> Result<Self, Error> {
        if bytes == [0; 16] {
            return Err(Error::Description);
        }
        Ok(Self(bytes))
    }

    pub fn bytes(self) -> [u8; 16] {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackBinding {
    pub mid: String,
    pub purpose: Purpose,
}

/// Authenticate the **whole** value, including ID and bindings, before passing
/// it to `accept_*_tracks`. DTLS authenticates the SDP fingerprint only; this
/// transport cannot establish user identity or validate a signaling signature.
/// No serialization format or network signaling is implicit here.
#[derive(Clone, PartialEq, Eq)]
pub struct Description {
    pub id: SignalingId,
    pub sdp: String,
    pub bindings: Vec<TrackBinding>,
}

impl fmt::Debug for Description {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SDP includes ICE credentials and addresses; do not log it by default.
        f.debug_struct("Description")
            .field("id", &self.id)
            .field("bindings", &self.bindings)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Section {
    pub mid: String,
    pub kind: Kind,
    codec: bool,
}

/// Bound allocation before handing SDP to the third-party parser. We support
/// exactly one Opus/VP8 stream per m-line, no application, simulcast or RID.
pub(crate) fn sections(sdp: &str) -> Result<Vec<Section>, Error> {
    if sdp.is_empty() || sdp.len() > MAX_SDP_BYTES || sdp.lines().count() > 512 {
        return Err(Error::Sdp);
    }
    let mut result: Vec<Section> = Vec::with_capacity(4);
    for line in sdp.lines() {
        if line.starts_with("m=") {
            if result.len() == 4 {
                return Err(Error::Sdp);
            }
            let mut fields = line.split_whitespace();
            let kind = match fields.next() {
                Some("m=audio") => Kind::Audio,
                Some("m=video") => Kind::Video,
                _ => return Err(Error::Sdp),
            };
            if fields.next() == Some("0") || fields.next() != Some("UDP/TLS/RTP/SAVPF") {
                return Err(Error::Sdp);
            }
            result.push(Section {
                mid: String::new(),
                kind,
                codec: false,
            });
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            let section = result.last_mut().ok_or(Error::Sdp)?;
            if !section.mid.is_empty() || !valid_mid(mid) {
                return Err(Error::Sdp);
            }
            section.mid = mid.to_owned();
        } else if let Some(mapping) = line.strip_prefix("a=rtpmap:") {
            let section = result.last_mut().ok_or(Error::Sdp)?;
            let (_, codec) = mapping.split_once(' ').ok_or(Error::Sdp)?;
            let expected = match section.kind {
                Kind::Audio => codec.eq_ignore_ascii_case("opus/48000/2"),
                Kind::Video => codec.eq_ignore_ascii_case("VP8/90000"),
            };
            // RTX is a retransmission of VP8, never an independent semantic track.
            if !expected
                && !(section.kind == Kind::Video && codec.eq_ignore_ascii_case("rtx/90000"))
            {
                return Err(Error::Sdp);
            }
            section.codec |= expected;
        } else if line.starts_with("a=rid:") || line.starts_with("a=simulcast:") {
            return Err(Error::Sdp);
        }
    }
    if result.is_empty()
        || result.iter().any(|s| s.mid.is_empty() || !s.codec)
        || result
            .iter()
            .enumerate()
            .any(|(i, s)| result[..i].iter().any(|p| p.mid == s.mid))
        || result.iter().filter(|s| s.kind == Kind::Audio).count() > 2
        || result.iter().filter(|s| s.kind == Kind::Video).count() > 2
    {
        return Err(Error::Sdp);
    }
    Ok(result)
}

fn valid_mid(mid: &str) -> bool {
    !mid.is_empty()
        && mid.len() <= 16
        && mid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub(crate) fn validate(description: &Description, expected: SignalingId) -> Result<(), Error> {
    if description.id != expected
        || description.bindings.is_empty()
        || description.bindings.len() > 4
    {
        return Err(Error::Description);
    }
    let media = sections(&description.sdp)?;
    if media.len() != description.bindings.len() {
        return Err(Error::Description);
    }
    for (i, binding) in description.bindings.iter().enumerate() {
        if !valid_mid(&binding.mid)
            || description.bindings[..i]
                .iter()
                .any(|p| p.mid == binding.mid || p.purpose == binding.purpose)
            || !media
                .iter()
                .any(|m| m.mid == binding.mid && m.kind == binding.purpose.kind())
        {
            return Err(Error::Description);
        }
    }
    Ok(())
}

pub(crate) fn legacy_bindings(sdp: &str) -> Result<Vec<TrackBinding>, Error> {
    let media = sections(sdp)?;
    if media.iter().filter(|m| m.kind == Kind::Audio).count() != 1
        || media.iter().filter(|m| m.kind == Kind::Video).count() > 1
    {
        return Err(Error::Description);
    }
    Ok(media
        .into_iter()
        .map(|m| TrackBinding {
            mid: m.mid,
            purpose: match m.kind {
                Kind::Audio => Purpose::Microphone,
                Kind::Video => Purpose::Camera,
            },
        })
        .collect())
}
