// SPDX-License-Identifier: EUPL-1.2
//! WebRTC transport. Encoded media crosses this boundary; codecs and devices do not.
//!
//! The caller owns UDP, authenticated signaling and scheduling. Drain `poll` to
//! `WakeAt` after each input, then wake by that deadline. Never drain an unbounded
//! socket backlog before polling output: doing so can starve RTCP and the pacer.
//!
//! `offer_tracks` binds semantic purposes to MIDs in a structured description.
//! The caller must authenticate that entire value and provide the expected fresh
//! signaling ID at both acceptance steps. The legacy string API supports only
//! microphone plus optional camera; it never guesses roles for a second stream.
//! This is an encoded-media transport foundation, not a capture, calling,
//! ownership/consent, relay, or end-to-end media encryption implementation.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, Input, Output as RtcOutput, Rtc};
use thiserror::Error;

pub const MAX_SDP_BYTES: usize = 32 * 1024;
pub const MAX_DATAGRAM_BYTES: usize = 2048;
pub const MAX_OPUS_BYTES: usize = 1275;
pub const MAX_VIDEO_BYTES: usize = 256 * 1024;
const INITIAL_BITRATE: u64 = 2_000_000;
mod pacing;
mod signaling;
use pacing::{Bucket, WINDOW};
pub use signaling::{Description, Purpose, SignalingId, TrackBinding};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
}

impl Kind {
    fn codec(self) -> Codec {
        match self {
            Self::Audio => Codec::Opus,
            Self::Video => Codec::Vp8,
        }
    }
    fn limit(self) -> usize {
        match self {
            Self::Audio => MAX_OPUS_BYTES,
            Self::Video => MAX_VIDEO_BYTES,
        }
    }
}

/// A connection-local track identifier; do not persist it as a user identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track(Mid);

impl Track {
    pub fn mid(self) -> String {
        self.0.to_string()
    }
}

struct TrackState {
    track: Track,
    purpose: Purpose,
    bucket: Bucket,
    next: Instant,
    timestamp: Option<u64>,
}

#[derive(Debug)]
pub struct Frame {
    pub track: Track,
    pub kind: Kind,
    pub purpose: Purpose,
    /// Extended RTP clock ticks: 48 kHz audio, 90 kHz video.
    pub timestamp: u64,
    pub contiguous: bool,
    pub data: Arc<[u8]>,
}

#[derive(Debug)]
pub enum Event {
    Connected,
    Disconnected,
    Frame(Frame),
    /// The encoder/controller must act on this; estimating bandwidth alone does
    /// not adapt a video stream. This trial has no live video encoder yet.
    BandwidthEstimate(u64),
    RequestKeyframe(Track),
}

#[derive(Debug)]
pub enum Output {
    WakeAt(Instant),
    Datagram {
        source: SocketAddr,
        destination: SocketAddr,
        data: Vec<u8>,
    },
    Event(Event),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid signaling state; this trial supports one initial offer/answer")]
    SignalingState,
    #[error("invalid, oversized, or unsupported SDP")]
    Sdp,
    #[error("invalid, mismatched, or stale track description")]
    Description,
    #[error("invalid local UDP address")]
    Address,
    #[error("invalid or oversized datagram")]
    Datagram,
    #[error("invalid track, codec, encoded frame length, or RTP timestamp")]
    Frame,
    #[error("media transport is not connected")]
    NotConnected,
    #[error("drop or defer this frame: the bandwidth or latency budget is exhausted")]
    Backpressure,
    #[error("time must advance monotonically")]
    Clock,
    #[error(transparent)]
    Rtc(#[from] str0m::RtcError),
}

enum Signaling {
    New,
    Offering {
        pending: SdpPendingOffer,
        description: Description,
        legacy: bool,
    },
    Negotiated,
}

/// Native WebRTC state only. No codecs, microphone handles or session passwords.
pub struct Peer {
    rtc: Rtc,
    address: SocketAddr,
    signaling: Signaling,
    tracks: Vec<TrackState>,
    now: Instant,
    closed: bool,
    bandwidth_bps: u64,
    total_budget: Bucket,
    video_budget: Bucket,
}

impl Peer {
    /// Use an explicitly bound, reachable host address. TURN and interface
    /// discovery are separate future adapters, never implicit public services.
    pub fn new(address: SocketAddr, now: Instant) -> Result<Self, Error> {
        if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(Error::Address);
        }
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            .set_fingerprint_verification(true)
            .set_mtu(1200..=1200)
            .set_reordering_size_audio(8)
            .set_reordering_timeout_audio(Some(Duration::from_millis(20)))
            .set_reordering_size_video(64)
            .set_send_buffer_audio(32)
            .set_send_buffer_video(512)
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(INITIAL_BITRATE)))
            .build(now);
        rtc.add_local_candidate(Candidate::host(address, "udp").map_err(|_| Error::Address)?)
            .ok_or(Error::Address)?;
        Ok(Self {
            rtc,
            address,
            signaling: Signaling::New,
            tracks: Vec::with_capacity(4),
            now,
            closed: false,
            bandwidth_bps: INITIAL_BITRATE,
            total_budget: Bucket::new(now),
            video_budget: Bucket::new(now),
        })
    }

    pub fn offer(&mut self, video: bool) -> Result<String, Error> {
        let purposes = if video {
            &[Purpose::Microphone, Purpose::Camera][..]
        } else {
            &[Purpose::Microphone][..]
        };
        // Compatibility API has no semantic manifest on its wire. It is
        // deliberately restricted to the original one-audio/one-video layout.
        let description = self.make_offer(purposes, SignalingId::new([1; 16])?, true)?;
        Ok(description.sdp)
    }

    /// Add up to two audio and two video tracks. Authenticate the returned
    /// description as one value; assigning roles from bare SDP is forbidden.
    pub fn offer_tracks(
        &mut self,
        purposes: &[Purpose],
        id: SignalingId,
    ) -> Result<Description, Error> {
        self.make_offer(purposes, id, false)
    }

    fn make_offer(
        &mut self,
        purposes: &[Purpose],
        id: SignalingId,
        legacy: bool,
    ) -> Result<Description, Error> {
        if self.closed || !matches!(self.signaling, Signaling::New) {
            return Err(Error::SignalingState);
        }
        if purposes.is_empty()
            || purposes.len() > 4
            || purposes
                .iter()
                .enumerate()
                .any(|(i, p)| purposes[..i].contains(p))
        {
            return Err(Error::Description);
        }
        let mut api = self.rtc.sdp_api();
        let mut bindings = Vec::with_capacity(purposes.len());
        for &purpose in purposes {
            let kind = match purpose.kind() {
                Kind::Audio => MediaKind::Audio,
                Kind::Video => MediaKind::Video,
            };
            let mid = api.add_media(kind, Direction::SendRecv, None, None, None);
            bindings.push(TrackBinding {
                mid: mid.to_string(),
                purpose,
            });
        }
        let (offer, pending) = api.apply().ok_or(Error::SignalingState)?;
        let description = Description {
            id,
            sdp: offer.to_sdp_string(),
            bindings,
        };
        signaling::validate(&description, id)?;
        self.bind_tracks(&description.bindings);
        self.signaling = Signaling::Offering {
            pending,
            description: description.clone(),
            legacy,
        };
        Ok(description)
    }

    /// SDP fingerprints must arrive over authenticated signaling before use in a
    /// real call. Successful DTLS verifies that fingerprint, not a user identity.
    pub fn accept_offer(&mut self, sdp: &str) -> Result<String, Error> {
        let bindings = signaling::legacy_bindings(sdp)?;
        let description = Description {
            id: SignalingId::new([1; 16])?,
            sdp: sdp.to_owned(),
            bindings,
        };
        Ok(self.accept_offer_tracks(&description, description.id)?.sdp)
    }

    /// The caller MUST authenticate the entire description and compare the
    /// expected fresh call ID before invoking this API. SDP labels carry no role.
    pub fn accept_offer_tracks(
        &mut self,
        description: &Description,
        expected: SignalingId,
    ) -> Result<Description, Error> {
        if self.closed || !matches!(self.signaling, Signaling::New) {
            return Err(Error::SignalingState);
        }
        signaling::validate(description, expected)?;
        let offer = SdpOffer::from_sdp_string(&description.sdp).map_err(|_| Error::Sdp)?;
        let answer = self.rtc.sdp_api().accept_offer(offer)?;
        let answer = Description {
            id: expected,
            sdp: answer.to_sdp_string(),
            bindings: description.bindings.clone(),
        };
        if let Err(error) = signaling::validate(&answer, expected) {
            self.close();
            return Err(error);
        }
        self.bind_tracks(&answer.bindings);
        self.signaling = Signaling::Negotiated;
        Ok(answer)
    }

    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), Error> {
        let Signaling::Offering {
            description,
            legacy: true,
            ..
        } = &self.signaling
        else {
            return Err(Error::SignalingState);
        };
        let bindings = signaling::legacy_bindings(sdp)?;
        let answer = Description {
            id: description.id,
            sdp: sdp.to_owned(),
            bindings,
        };
        self.accept_answer_tracks(&answer, answer.id)
    }

    /// As for `accept_offer_tracks`, authentication is the caller's obligation.
    /// The answer must preserve the offered purpose for every MID and call ID.
    pub fn accept_answer_tracks(
        &mut self,
        description: &Description,
        expected: SignalingId,
    ) -> Result<(), Error> {
        if self.closed {
            return Err(Error::SignalingState);
        }
        let Signaling::Offering {
            description: offered,
            ..
        } = &self.signaling
        else {
            return Err(Error::SignalingState);
        };
        signaling::validate(description, expected)?;
        if offered.id != expected
            || offered.bindings.len() != description.bindings.len()
            || !offered
                .bindings
                .iter()
                .all(|b| description.bindings.contains(b))
        {
            return Err(Error::Description);
        }
        let answer = SdpAnswer::from_sdp_string(&description.sdp).map_err(|_| Error::Sdp)?;
        let Signaling::Offering { pending, .. } =
            std::mem::replace(&mut self.signaling, Signaling::Negotiated)
        else {
            return Err(Error::SignalingState);
        };
        if let Err(error) = self.rtc.sdp_api().accept_answer(pending, answer) {
            self.close();
            return Err(error.into());
        }
        Ok(())
    }

    pub fn track(&self, kind: Kind) -> Option<Track> {
        let mut tracks = self.tracks.iter().filter(|t| t.purpose.kind() == kind);
        let track = tracks.next()?.track;
        tracks.next().is_none().then_some(track)
    }

    pub fn track_for(&self, purpose: Purpose) -> Option<Track> {
        self.tracks
            .iter()
            .find(|t| t.purpose == purpose)
            .map(|t| t.track)
    }

    pub fn purpose(&self, track: Track) -> Option<Purpose> {
        self.tracks
            .iter()
            .find(|t| t.track == track)
            .map(|t| t.purpose)
    }

    fn bind_tracks(&mut self, bindings: &[TrackBinding]) {
        self.tracks = bindings
            .iter()
            .map(|b| TrackState {
                track: Track(Mid::from(b.mid.as_str())),
                purpose: b.purpose,
                bucket: Bucket::new(self.now),
                next: self.now,
                timestamp: None,
            })
            .collect();
    }

    /// Request a keyframe for this video MID only; audio is never eligible.
    pub fn request_keyframe(&mut self, track: Track) -> Result<(), Error> {
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        if self.purpose(track).is_none_or(|p| p.kind() != Kind::Video) {
            return Err(Error::Frame);
        }
        self.rtc
            .writer(track.0)
            .ok_or(Error::Frame)?
            .request_keyframe(None, KeyframeRequestKind::Pli)?;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        !self.closed && self.rtc.is_connected()
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.rtc.disconnect();
    }

    /// Submit an already encoded frame. Audio uses a 48 kHz clock, video 90 kHz.
    /// Timestamps increase independently per track. The caller bounds its capture
    /// queue and drops stale video on backpressure; a failed write consumes no
    /// frame budget or timestamp. Both audio roles have dedicated allocations,
    /// videos get equal shares, and the total admission budget follows BWE with
    /// transport headroom. An encoder must still adapt its rate/size to feedback.
    pub fn write(
        &mut self,
        track: Track,
        timestamp: u64,
        now: Instant,
        data: Arc<[u8]>,
    ) -> Result<(), Error> {
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        let index = self
            .tracks
            .iter()
            .position(|t| t.track == track)
            .ok_or(Error::Frame)?;
        let kind = self.tracks[index].purpose.kind();
        if data.is_empty()
            || data.len() > kind.limit()
            || self.tracks[index]
                .timestamp
                .is_some_and(|last| timestamp <= last)
        {
            return Err(Error::Frame);
        }
        self.clock(now)?;
        let (total, video, rate) = self.rates(self.tracks[index].purpose);
        // Include a conservative packet overhead estimate, on top of the 20%
        // transport headroom. Small audio packets must not bypass the budget.
        let bits = ((data.len() + data.len().div_ceil(1000) * 80) * 8) as f64;
        if rate == 0
            || now < self.tracks[index].next
            || self.total_budget.available(now, total) < bits
            || self.tracks[index].bucket.available(now, rate) < bits
            || (kind == Kind::Video && self.video_budget.available(now, video) < bits)
        {
            return Err(Error::Backpressure);
        }
        let interval = Duration::from_secs_f64(bits / rate as f64).max(if kind == Kind::Audio {
            Duration::from_micros(2500)
        } else {
            Duration::from_millis(1)
        });
        if interval > WINDOW {
            return Err(Error::Backpressure);
        }
        let writer = self.rtc.writer(track.0).ok_or(Error::Frame)?;
        let params = *writer
            .payload_params()
            .find(|p| p.spec().codec == kind.codec())
            .ok_or(Error::Frame)?;
        writer.write(
            params.pt(),
            now,
            MediaTime::new(timestamp, params.spec().clock_rate),
            data,
        )?;
        self.total_budget.spend(now, total, bits);
        if kind == Kind::Video {
            self.video_budget.spend(now, video, bits);
        }
        let state = &mut self.tracks[index];
        state.bucket.spend(now, rate, bits);
        state.next = now + interval;
        state.timestamp = Some(timestamp);
        Ok(())
    }

    fn rates(&self, purpose: Purpose) -> (u64, u64, u64) {
        let total = self.bandwidth_bps.min(10_000_000) * 4 / 5;
        let microphone = if self.track_for(Purpose::Microphone).is_some() {
            128_000
        } else {
            0
        };
        let system = if self.track_for(Purpose::SystemAudio).is_some() {
            256_000
        } else {
            0
        };
        let audio = microphone + system;
        // Under severe congestion, both audio roles retain proportional shares;
        // video is shut off rather than borrowing either audio allocation.
        let video = total.saturating_sub(audio);
        let role_rate = match purpose {
            Purpose::Microphone => microphone * total.min(audio) / audio.max(1),
            Purpose::SystemAudio => system * total.min(audio) / audio.max(1),
            Purpose::Camera | Purpose::ScreenVideo => {
                let count = self
                    .tracks
                    .iter()
                    .filter(|t| t.purpose.kind() == Kind::Video)
                    .count() as u64;
                video / count.max(1)
            }
        };
        (total, video, role_rate)
    }

    /// Malformed datagrams return an error; a socket adapter may discard them
    /// rather than disconnect an otherwise authenticated peer.
    pub fn receive(&mut self, source: SocketAddr, now: Instant, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() || data.len() > MAX_DATAGRAM_BYTES {
            return Err(Error::Datagram);
        }
        let contents = data.try_into().map_err(|_| Error::Datagram)?;
        self.clock(now)?;
        self.rtc.handle_input(Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination: self.address,
                contents,
            },
        ))?;
        Ok(())
    }

    pub fn timeout(&mut self, now: Instant) -> Result<(), Error> {
        self.clock(now)?;
        self.rtc.handle_input(Input::Timeout(now))?;
        Ok(())
    }

    /// Returns one action at a time, avoiding an unbounded application queue.
    pub fn poll(&mut self) -> Result<Output, Error> {
        // Bound ignored events per call so a busy peer cannot starve its siblings.
        for _ in 0..64 {
            match self.rtc.poll_output()? {
                RtcOutput::Timeout(at) => return Ok(Output::WakeAt(at)),
                RtcOutput::Transmit(v) => {
                    return Ok(Output::Datagram {
                        source: v.source,
                        destination: v.destination,
                        data: v.contents.to_vec(),
                    });
                }
                RtcOutput::Event(event) => match event {
                    RtcEvent::Connected => return Ok(Output::Event(Event::Connected)),
                    RtcEvent::Closed => {
                        self.closed = true;
                        return Ok(Output::Event(Event::Disconnected));
                    }
                    RtcEvent::MediaAdded(media) => {
                        let kind = if media.kind == MediaKind::Audio {
                            Kind::Audio
                        } else {
                            Kind::Video
                        };
                        if self
                            .purpose(Track(media.mid))
                            .is_none_or(|p| p.kind() != kind)
                        {
                            self.close();
                            return Err(Error::Frame);
                        }
                    }
                    RtcEvent::MediaData(media) => {
                        let kind = match media.params.spec().codec {
                            Codec::Opus => Kind::Audio,
                            Codec::Vp8 => Kind::Video,
                            _ => return Err(Error::Frame),
                        };
                        let purpose = self.purpose(Track(media.mid)).ok_or(Error::Frame)?;
                        if purpose.kind() != kind
                            || media.data.is_empty()
                            || media.data.len() > kind.limit()
                        {
                            return Err(Error::Frame);
                        }
                        return Ok(Output::Event(Event::Frame(Frame {
                            track: Track(media.mid),
                            kind,
                            purpose,
                            timestamp: media.time.numer(),
                            contiguous: media.contiguous,
                            data: media.data,
                        })));
                    }
                    RtcEvent::EgressBitrateEstimate(estimate) => {
                        let rate = match estimate {
                            str0m::bwe::BweKind::Twcc { estimate, .. }
                            | str0m::bwe::BweKind::Remb { estimate, .. } => estimate,
                            _ => continue,
                        };
                        self.bandwidth_bps = rate.as_u64();
                        return Ok(Output::Event(Event::BandwidthEstimate(rate.as_u64())));
                    }
                    RtcEvent::KeyframeRequest(request) => {
                        if self
                            .purpose(Track(request.mid))
                            .is_none_or(|p| p.kind() != Kind::Video)
                        {
                            return Err(Error::Frame);
                        }
                        return Ok(Output::Event(Event::RequestKeyframe(Track(request.mid))));
                    }
                    _ => {}
                },
            }
        }
        Ok(Output::WakeAt(self.now))
    }

    fn clock(&mut self, now: Instant) -> Result<(), Error> {
        if now < self.now {
            return Err(Error::Clock);
        }
        self.now = now;
        Ok(())
    }
}
