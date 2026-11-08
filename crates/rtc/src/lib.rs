// SPDX-License-Identifier: EUPL-1.2
//! WebRTC transport. Encoded media crosses this boundary; codecs and devices do not.
//!
//! The caller owns UDP, authenticated signaling and scheduling. Drain `poll` to
//! `WakeAt` after each input, then wake by that deadline. Never drain an unbounded
//! socket backlog before polling output: doing so can starve RTCP and the pacer.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, Input, Output as RtcOutput, Rtc};
use thiserror::Error;

pub const MAX_SDP_BYTES: usize = 32 * 1024;
pub const MAX_DATAGRAM_BYTES: usize = 2048;
pub const MAX_OPUS_BYTES: usize = 1275;
pub const MAX_VIDEO_BYTES: usize = 256 * 1024;
const INITIAL_BITRATE: u64 = 2_000_000;
const AUDIO_RESERVE_BPS: u64 = 96_000;
const MAX_VIDEO_QUEUE_TIME: Duration = Duration::from_millis(120);

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

#[derive(Debug)]
pub struct Frame {
    pub track: Track,
    pub kind: Kind,
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
    #[error("invalid local UDP address")]
    Address,
    #[error("invalid or oversized datagram")]
    Datagram,
    #[error("invalid track, codec, or encoded frame length")]
    Frame,
    #[error("media transport is not connected")]
    NotConnected,
    #[error("drop this video frame: the bandwidth or latency budget is exhausted")]
    Backpressure,
    #[error("time must advance monotonically")]
    Clock,
    #[error(transparent)]
    Rtc(#[from] str0m::RtcError),
}

enum Signaling {
    New,
    Offering(SdpPendingOffer),
    Negotiated,
}

/// Native WebRTC state only. No codecs, microphone handles or session passwords.
pub struct Peer {
    rtc: Rtc,
    address: SocketAddr,
    signaling: Signaling,
    tracks: Vec<(Track, Kind)>,
    now: Instant,
    closed: bool,
    bandwidth_bps: u64,
    next_video: Instant,
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
            tracks: Vec::with_capacity(2),
            now,
            closed: false,
            bandwidth_bps: INITIAL_BITRATE,
            next_video: now,
        })
    }

    pub fn offer(&mut self, video: bool) -> Result<String, Error> {
        if !matches!(self.signaling, Signaling::New) {
            return Err(Error::SignalingState);
        }
        let mut api = self.rtc.sdp_api();
        let audio = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        self.tracks.push((Track(audio), Kind::Audio));
        if video {
            let video = api.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
            self.tracks.push((Track(video), Kind::Video));
        }
        let (offer, pending) = api.apply().ok_or(Error::SignalingState)?;
        self.signaling = Signaling::Offering(pending);
        Ok(offer.to_sdp_string())
    }

    /// SDP fingerprints must arrive over authenticated signaling before use in a
    /// real call. Successful DTLS verifies that fingerprint, not a user identity.
    pub fn accept_offer(&mut self, sdp: &str) -> Result<String, Error> {
        if !matches!(self.signaling, Signaling::New) {
            return Err(Error::SignalingState);
        }
        validate_sdp(sdp)?;
        let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| Error::Sdp)?;
        let answer = self.rtc.sdp_api().accept_offer(offer)?;
        self.signaling = Signaling::Negotiated;
        Ok(answer.to_sdp_string())
    }

    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), Error> {
        if !matches!(self.signaling, Signaling::Offering(_)) {
            return Err(Error::SignalingState);
        }
        validate_sdp(sdp)?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|_| Error::Sdp)?;
        let Signaling::Offering(pending) =
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
        self.tracks
            .iter()
            .find(|(_, k)| *k == kind)
            .map(|(t, _)| *t)
    }

    pub fn is_connected(&self) -> bool {
        !self.closed && self.rtc.is_connected()
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.rtc.disconnect();
    }

    /// Submit an already encoded frame. Audio uses a 48 kHz clock, video 90 kHz.
    /// The caller bounds its capture queue and drops stale video on backpressure.
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
        let kind = self
            .tracks
            .iter()
            .find(|(t, _)| *t == track)
            .map(|(_, k)| *k)
            .ok_or(Error::Frame)?;
        if data.is_empty() || data.len() > kind.limit() {
            return Err(Error::Frame);
        }
        self.clock(now)?;
        let video_interval = if kind == Kind::Video {
            // Reserve audio and transport overhead before admitting video. Do
            // not build a video queue while waiting for a slower network or a
            // future encoder to react to the estimate.
            let budget =
                (self.bandwidth_bps.min(10_000_000) * 4 / 5).saturating_sub(AUDIO_RESERVE_BPS);
            if budget == 0 || now < self.next_video {
                return Err(Error::Backpressure);
            }
            let interval = Duration::from_secs_f64(data.len() as f64 * 8.0 / budget as f64);
            if interval > MAX_VIDEO_QUEUE_TIME {
                return Err(Error::Backpressure);
            }
            Some(interval)
        } else {
            None
        };
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
        if let Some(interval) = video_interval {
            self.next_video = now + interval;
        }
        Ok(())
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
                        if !self.tracks.iter().any(|(t, _)| t.0 == media.mid) {
                            self.tracks.push((Track(media.mid), kind));
                        }
                    }
                    RtcEvent::MediaData(media) => {
                        let kind = match media.params.spec().codec {
                            Codec::Opus => Kind::Audio,
                            Codec::Vp8 => Kind::Video,
                            _ => return Err(Error::Frame),
                        };
                        if media.data.is_empty() || media.data.len() > kind.limit() {
                            return Err(Error::Frame);
                        }
                        return Ok(Output::Event(Event::Frame(Frame {
                            track: Track(media.mid),
                            kind,
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

fn validate_sdp(sdp: &str) -> Result<(), Error> {
    if sdp.is_empty() || sdp.len() > MAX_SDP_BYTES || sdp.lines().count() > 512 {
        return Err(Error::Sdp);
    }
    let mut audio = 0;
    let mut video = 0;
    for line in sdp.lines().filter(|line| line.starts_with("m=")) {
        match line.split_whitespace().next() {
            Some("m=audio") => audio += 1,
            Some("m=video") => video += 1,
            _ => return Err(Error::Sdp),
        }
    }
    if audio != 1 || video > 1 {
        return Err(Error::Sdp);
    }
    Ok(())
}
