// SPDX-License-Identifier: EUPL-1.2
//! Deterministic ICE/DTLS/SRTP exchange: datagrams stay in memory, no public STUN.
use nuschl_rtc::{
    Error, Event, Frame, Kind, MAX_DATAGRAM_BYTES, MAX_OPUS_BYTES, MAX_SDP_BYTES, Output, Peer,
    Purpose, SignalingId, Track,
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

struct Pair {
    peers: [Peer; 2],
    addresses: [SocketAddr; 2],
    wake: [Instant; 2],
    now: Instant,
    frames: [Vec<Frame>; 2],
    wire: VecDeque<(usize, SocketAddr, Vec<u8>)>,
    video_pt: u8,
    drop_video: bool,
    video_packets: usize,
    dropped: usize,
    loss_ssrc: Option<[u8; 4]>,
    keyframes: [Vec<Track>; 2],
}

impl Pair {
    fn new(video: bool, corrupt_fingerprint: bool) -> Self {
        Self::setup(video, corrupt_fingerprint, &[])
    }

    fn multi() -> Self {
        Self::setup(
            true,
            false,
            &[
                Purpose::Microphone,
                Purpose::SystemAudio,
                Purpose::Camera,
                Purpose::ScreenVideo,
            ],
        )
    }

    fn setup(video: bool, corrupt_fingerprint: bool, purposes: &[Purpose]) -> Self {
        let now = Instant::now();
        let addresses = [
            "127.0.0.1:41000".parse().unwrap(),
            "127.0.0.1:41001".parse().unwrap(),
        ];
        let mut peers = [
            Peer::new(addresses[0], now).unwrap(),
            Peer::new(addresses[1], now).unwrap(),
        ];
        let offer = if purposes.is_empty() {
            peers[0].offer(video).unwrap()
        } else {
            let id = SignalingId::new([42; 16]).unwrap();
            let offer = peers[0].offer_tracks(purposes, id).unwrap();
            let answer = peers[1].accept_offer_tracks(&offer, id).unwrap();
            peers[0].accept_answer_tracks(&answer, id).unwrap();
            offer.sdp
        };
        let video_pt = offer
            .lines()
            .find(|l| l.ends_with(" VP8/90000"))
            .map(|l| {
                l.trim_start_matches("a=rtpmap:")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .unwrap_or(0);
        let answer = if purposes.is_empty() {
            peers[1].accept_offer(&offer).unwrap()
        } else {
            String::new()
        };
        let answer = if corrupt_fingerprint {
            answer
                .lines()
                .map(|line| {
                    if line.starts_with("a=fingerprint:") {
                        format!("a=fingerprint:sha-256 {}", ["00"; 32].join(":"))
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\r\n")
                + "\r\n"
        } else {
            answer
        };
        if purposes.is_empty() {
            peers[0].accept_answer(&answer).unwrap();
        }
        Self {
            peers,
            addresses,
            wake: [now; 2],
            now,
            frames: [vec![], vec![]],
            wire: VecDeque::new(),
            video_pt,
            drop_video: false,
            video_packets: 0,
            dropped: 0,
            loss_ssrc: None,
            keyframes: [vec![], vec![]],
        }
    }

    fn drain(&mut self, from: usize) -> Result<(), Error> {
        for _ in 0..4096 {
            match self.peers[from].poll()? {
                Output::WakeAt(at) => {
                    self.wake[from] = at;
                    return Ok(());
                }
                Output::Datagram {
                    source,
                    destination,
                    data,
                } => {
                    assert_eq!(source, self.addresses[from]);
                    assert_eq!(destination, self.addresses[1 - from]);
                    if self.drop_video
                        && from == 0
                        && data.len() > 12
                        && data[0] >> 6 == 2
                        && data[1] & 0x7f == self.video_pt
                    {
                        let ssrc = [data[8], data[9], data[10], data[11]];
                        let selected = *self.loss_ssrc.get_or_insert(ssrc);
                        if ssrc == selected {
                            self.video_packets += 1;
                            if self.video_packets == 2 {
                                // Drop an interior fragment of ONE SSRC. With
                                // two video tracks, packet #2 overall can be the
                                // first packet of the other stream: RTP cannot
                                // NACK an initial sequence number it never saw.
                                self.drop_video = false;
                                self.dropped += 1;
                                continue;
                            }
                        }
                    }
                    self.wire.push_back((1 - from, source, data));
                }
                Output::Event(Event::Frame(frame)) => self.frames[from].push(frame),
                Output::Event(Event::RequestKeyframe(track)) => self.keyframes[from].push(track),
                _ => {}
            }
        }
        panic!("output failed to quiesce");
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.drain(0)?;
        self.drain(1)?;
        for _ in 0..8192 {
            let Some((to, source, data)) = self.wire.pop_front() else {
                return Ok(());
            };
            self.peers[to].receive(source, self.now, &data)?;
            self.drain(to)?;
        }
        panic!("datagram exchange failed to quiesce");
    }

    fn advance(&mut self, duration: Duration) -> Result<(), Error> {
        let end = self.now + duration;
        self.flush()?;
        // One-ms virtual-clock steps let both peers service deadlines fairly.
        while self.now < end {
            self.now = (self.now + Duration::from_millis(1)).min(end);
            for peer in 0..2 {
                if self.now >= self.wake[peer] {
                    self.peers[peer].timeout(self.now)?;
                }
            }
            self.flush()?;
        }
        Ok(())
    }

    fn connect(&mut self) {
        self.advance(Duration::from_millis(500)).unwrap();
        assert!(self.peers.iter().all(Peer::is_connected));
    }

    fn write(&mut self, from: usize, kind: Kind, timestamp: u64, data: Arc<[u8]>) {
        let track = self.peers[from].track(kind).unwrap();
        self.peers[from]
            .write(track, timestamp, self.now, data)
            .unwrap();
    }
}

fn video() -> Arc<[u8]> {
    Arc::from(include_bytes!("fixtures/rtc-keyframe.vp8").as_slice())
}

#[test]
fn bidirectional_opus_and_fragmented_video_preserve_bytes_and_clocks() {
    let mut pair = Pair::new(true, false);
    pair.connect();
    let video = video();
    for frame in 0..12 {
        for from in 0..2 {
            pair.write(
                from,
                Kind::Audio,
                frame * 960,
                Arc::from([0xf8, 0xff, 0xfe]),
            );
            if frame % 3 == 0 {
                pair.write(from, Kind::Video, frame * 1800, video.clone());
            }
        }
        pair.advance(Duration::from_millis(20)).unwrap();
    }
    pair.advance(Duration::from_millis(500)).unwrap();
    for frames in &pair.frames {
        let audio: Vec<_> = frames.iter().filter(|f| f.kind == Kind::Audio).collect();
        let videos: Vec<_> = frames.iter().filter(|f| f.kind == Kind::Video).collect();
        assert_eq!(audio.len(), 12);
        assert_eq!(videos.len(), 4);
        for (index, frame) in audio.iter().enumerate() {
            assert_eq!(frame.timestamp, index as u64 * 960);
            assert_eq!(frame.data.as_ref(), [0xf8, 0xff, 0xfe]);
        }
        for (index, frame) in videos.iter().enumerate() {
            assert_eq!(frame.timestamp, index as u64 * 5400);
            assert_eq!(frame.data, video);
        }
    }
}

#[test]
fn video_packet_loss_is_recovered_without_blocking_audio() {
    let mut pair = Pair::new(true, false);
    pair.connect();
    pair.drop_video = true;
    let video = video();
    for i in 0..20 {
        pair.write(0, Kind::Audio, i * 960, Arc::from([0xf8, 0xff, 0xfe]));
        if i % 3 == 0 {
            pair.write(0, Kind::Video, i * 1800, video.clone());
        }
        pair.advance(Duration::from_millis(20)).unwrap();
        // Audio must make progress during recovery, rather than all arriving
        // after the final video frame is recovered.
        assert!(
            pair.frames[1]
                .iter()
                .filter(|f| f.kind == Kind::Audio)
                .count()
                >= i.saturating_sub(2) as usize
        );
    }
    pair.advance(Duration::from_secs(2)).unwrap();
    assert_eq!(pair.dropped, 1);
    let received: Vec<_> = pair.frames[1]
        .iter()
        .filter(|f| f.kind == Kind::Video)
        .collect();
    assert_eq!(received.len(), 7);
    assert!(received.iter().all(|f| f.data == video));
    assert_eq!(
        pair.frames[1]
            .iter()
            .filter(|f| f.kind == Kind::Audio)
            .count(),
        20
    );
}

#[test]
fn altered_dtls_fingerprint_never_connects_both_peers() {
    let mut pair = Pair::new(false, true);
    // A rejected handshake may report an error or remain disconnected.
    let _ = pair.advance(Duration::from_secs(1));
    assert!(!pair.peers.iter().all(Peer::is_connected));
    assert!(pair.frames.iter().all(Vec::is_empty));
}

#[test]
fn microphone_carries_the_largest_admitted_voice_frame_on_a_jittered_cadence() {
    // 640 bytes per 20 ms is the largest frame voice admission accepts at the
    // highest selectable target (256 kb/s); capture wakes up a ms early or late.
    let mut pair = Pair::new(false, false);
    pair.connect();
    let frame: Arc<[u8]> = vec![0x5a; 640].into();
    for index in 0..50 {
        pair.write(0, Kind::Audio, index * 960, frame.clone());
        let step = if index % 2 == 0 { 21 } else { 19 };
        pair.advance(Duration::from_millis(step)).unwrap();
    }
    pair.advance(Duration::from_millis(500)).unwrap();
    let audio: Vec<_> = pair.frames[1]
        .iter()
        .filter(|f| f.kind == Kind::Audio)
        .collect();
    assert_eq!(audio.len(), 50);
    assert!(audio.iter().all(|f| f.data == frame));

    // A sustained stream above the reservation is still refused.
    let oversized: Arc<[u8]> = vec![0x5a; 960].into();
    let track = pair.peers[0].track(Kind::Audio).unwrap();
    let mut refused = false;
    for index in 50..100 {
        let now = pair.now;
        refused |= matches!(
            pair.peers[0].write(track, index * 960, now, oversized.clone()),
            Err(Error::Backpressure)
        );
        pair.advance(Duration::from_millis(20)).unwrap();
    }
    assert!(refused);
}

#[test]
fn video_backpressure_keeps_audio_available_and_does_not_queue_stale_frames() {
    let mut pair = Pair::new(true, false);
    pair.connect();
    let video = video();
    pair.write(0, Kind::Video, 0, video.clone());
    let track = pair.peers[0].track(Kind::Video).unwrap();
    for i in 1..100 {
        assert!(matches!(
            pair.peers[0].write(track, i * 1800, pair.now, video.clone()),
            Err(Error::Backpressure)
        ));
    }
    pair.write(0, Kind::Audio, 0, Arc::from([0xf8, 0xff, 0xfe]));
    pair.advance(Duration::from_millis(150)).unwrap();
    assert_eq!(
        pair.frames[1]
            .iter()
            .filter(|f| f.kind == Kind::Video)
            .count(),
        1
    );
    assert_eq!(
        pair.frames[1]
            .iter()
            .filter(|f| f.kind == Kind::Audio)
            .count(),
        1
    );
    pair.write(0, Kind::Video, 18_000, video);
    pair.advance(Duration::from_millis(150)).unwrap();
    assert_eq!(
        pair.frames[1]
            .iter()
            .filter(|f| f.kind == Kind::Video)
            .count(),
        2
    );
}

#[test]
fn tampered_and_replayed_srtp_do_not_emit_media() {
    let mut pair = Pair::new(false, false);
    pair.connect();
    pair.write(0, Kind::Audio, 0, Arc::from([0xf8, 0xff, 0xfe]));
    pair.peers[0].timeout(pair.now).unwrap();
    pair.drain(0).unwrap();
    let index = pair
        .wire
        .iter()
        .position(|(_, _, data)| {
            data.len() > 12 && data[0] >> 6 == 2 && !(192..=223).contains(&data[1])
        })
        .expect("encrypted RTP");
    let (to, source, original) = pair.wire.remove(index).unwrap();
    let mut tampered = original.clone();
    *tampered.last_mut().unwrap() ^= 1;
    let _ = pair.peers[to].receive(source, pair.now, &tampered);
    pair.advance(Duration::from_millis(25)).unwrap();
    assert!(pair.frames[to].is_empty());
    pair.peers[to].receive(source, pair.now, &original).unwrap();
    pair.advance(Duration::from_millis(25)).unwrap();
    assert_eq!(pair.frames[to].len(), 1);
    let _ = pair.peers[to].receive(source, pair.now, &original);
    pair.advance(Duration::from_millis(25)).unwrap();
    assert_eq!(pair.frames[to].len(), 1);
}

#[test]
fn rejects_unbounded_signaling_media_and_backwards_time() {
    let now = Instant::now();
    let mut peer = Peer::new("127.0.0.1:41000".parse().unwrap(), now).unwrap();
    assert!(matches!(
        peer.accept_offer(&"x".repeat(MAX_SDP_BYTES + 1)),
        Err(Error::Sdp)
    ));
    assert!(matches!(
        peer.accept_offer("v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"),
        Err(Error::Sdp)
    ));
    assert!(matches!(
        peer.accept_answer("invalid"),
        Err(Error::SignalingState)
    ));
    assert!(matches!(
        peer.receive(
            "127.0.0.1:41001".parse().unwrap(),
            now,
            &vec![0; MAX_DATAGRAM_BYTES + 1]
        ),
        Err(Error::Datagram)
    ));
    peer.offer(false).unwrap();
    assert!(matches!(peer.offer(false), Err(Error::SignalingState)));
    let track = peer.track(Kind::Audio).unwrap();
    assert!(matches!(
        peer.write(track, 0, now, Arc::from([0xf8, 0xff, 0xfe])),
        Err(Error::NotConnected)
    ));
    assert!(matches!(
        peer.timeout(now - Duration::from_millis(1)),
        Err(Error::Clock)
    ));
    let mut pair = Pair::new(false, false);
    pair.connect();
    let track = pair.peers[0].track(Kind::Audio).unwrap();
    assert!(matches!(
        pair.peers[0].write(track, 0, pair.now, vec![0; MAX_OPUS_BYTES + 1].into()),
        Err(Error::Frame)
    ));
    pair.peers[0].close();
    assert!(matches!(
        pair.peers[0].write(track, 0, pair.now, Arc::from([0xf8, 0xff, 0xfe])),
        Err(Error::NotConnected)
    ));
}

#[test]
fn four_bidirectional_tracks_preserve_independent_mids_roles_bytes_and_timestamps() {
    let mut pair = Pair::multi();
    pair.connect();
    let purposes = [
        Purpose::Microphone,
        Purpose::SystemAudio,
        Purpose::Camera,
        Purpose::ScreenVideo,
    ];
    for peer in &pair.peers {
        assert!(
            peer.track(Kind::Audio).is_none(),
            "ambiguous kind must not pick microphone"
        );
        assert!(
            peer.track(Kind::Video).is_none(),
            "ambiguous kind must not pick camera"
        );
        let mids: Vec<_> = purposes
            .iter()
            .map(|&p| peer.track_for(p).unwrap().mid())
            .collect();
        for (i, mid) in mids.iter().enumerate() {
            assert!(!mids[..i].contains(mid));
        }
    }
    for tick in 0..20 {
        for from in 0..2 {
            for (index, &purpose) in purposes.iter().enumerate() {
                if purpose.kind() == Kind::Video && tick % 4 != 0 {
                    continue;
                }
                let timestamp = if purpose.kind() == Kind::Audio {
                    tick * 960
                } else {
                    tick * 1800
                } + index as u64 * 90_000;
                let data = if purpose.kind() == Kind::Audio {
                    Arc::from([0xf8, 0xff, 0xfe])
                } else {
                    video()
                };
                let track = pair.peers[from].track_for(purpose).unwrap();
                pair.peers[from]
                    .write(track, timestamp, pair.now, data)
                    .unwrap();
            }
        }
        pair.advance(Duration::from_millis(20)).unwrap();
    }
    pair.advance(Duration::from_millis(500)).unwrap();
    for to in 0..2 {
        for (index, &purpose) in purposes.iter().enumerate() {
            let frames: Vec<_> = pair.frames[to]
                .iter()
                .filter(|f| f.purpose == purpose)
                .collect();
            assert_eq!(
                frames.len(),
                if purpose.kind() == Kind::Audio { 20 } else { 5 }
            );
            for (n, frame) in frames.iter().enumerate() {
                assert_eq!(frame.kind, purpose.kind());
                assert_eq!(frame.track, pair.peers[to].track_for(purpose).unwrap());
                assert_eq!(
                    frame.track.mid(),
                    pair.peers[1 - to].track_for(purpose).unwrap().mid()
                );
                let expected = index as u64 * 90_000
                    + n as u64
                        * if purpose.kind() == Kind::Audio {
                            960
                        } else {
                            7200
                        };
                assert_eq!(frame.timestamp, expected);
                if purpose.kind() == Kind::Audio {
                    assert_eq!(frame.data.as_ref(), [0xf8, 0xff, 0xfe]);
                } else {
                    assert_eq!(frame.data, video());
                }
            }
        }
    }
}

#[test]
fn one_video_loss_does_not_block_either_audio_role_or_the_other_video() {
    let mut pair = Pair::multi();
    pair.connect();
    pair.drop_video = true;
    for tick in 0..24 {
        for purpose in [
            Purpose::Camera,
            Purpose::ScreenVideo,
            Purpose::Microphone,
            Purpose::SystemAudio,
        ] {
            if purpose.kind() == Kind::Video && tick % 4 != 0 {
                continue;
            }
            let track = pair.peers[0].track_for(purpose).unwrap();
            pair.peers[0]
                .write(
                    track,
                    tick * if purpose.kind() == Kind::Audio {
                        960
                    } else {
                        1800
                    },
                    pair.now,
                    if purpose.kind() == Kind::Audio {
                        Arc::from([0xf8, 0xff, 0xfe])
                    } else {
                        video()
                    },
                )
                .unwrap();
        }
        pair.advance(Duration::from_millis(20)).unwrap();
        for purpose in [Purpose::Microphone, Purpose::SystemAudio] {
            assert!(
                pair.frames[1]
                    .iter()
                    .filter(|f| f.purpose == purpose)
                    .count()
                    >= tick.saturating_sub(2) as usize
            );
        }
    }
    pair.advance(Duration::from_secs(2)).unwrap();
    assert_eq!(pair.dropped, 1);
    for purpose in [
        Purpose::Microphone,
        Purpose::SystemAudio,
        Purpose::Camera,
        Purpose::ScreenVideo,
    ] {
        let frames: Vec<_> = pair.frames[1]
            .iter()
            .filter(|f| f.purpose == purpose)
            .collect();
        assert_eq!(
            frames.len(),
            if purpose.kind() == Kind::Audio { 24 } else { 6 },
            "{purpose:?}: {:?}",
            frames
                .iter()
                .map(|f| (f.timestamp, f.contiguous))
                .collect::<Vec<_>>()
        );
        if purpose.kind() == Kind::Video {
            assert!(frames.iter().all(|f| f.data == video()));
        }
    }
}

#[test]
fn greedy_camera_cannot_consume_screen_share_or_either_audio_reserve() {
    let mut pair = Pair::multi();
    pair.connect();
    for tick in 0..16 {
        let camera = pair.peers[0].track_for(Purpose::Camera).unwrap();
        pair.peers[0]
            .write(camera, tick * 7200, pair.now, video())
            .unwrap();
        for i in 1..100 {
            assert!(matches!(
                pair.peers[0].write(camera, tick * 7200 + i, pair.now, video()),
                Err(Error::Backpressure)
            ));
        }
        for purpose in [
            Purpose::ScreenVideo,
            Purpose::Microphone,
            Purpose::SystemAudio,
        ] {
            let track = pair.peers[0].track_for(purpose).unwrap();
            pair.peers[0]
                .write(
                    track,
                    tick * if purpose.kind() == Kind::Audio {
                        3840
                    } else {
                        7200
                    },
                    pair.now,
                    if purpose.kind() == Kind::Audio {
                        Arc::from([0xf8, 0xff, 0xfe])
                    } else {
                        video()
                    },
                )
                .unwrap();
        }
        pair.advance(Duration::from_millis(80)).unwrap();
    }
    pair.advance(Duration::from_millis(500)).unwrap();
    for purpose in [
        Purpose::Microphone,
        Purpose::SystemAudio,
        Purpose::Camera,
        Purpose::ScreenVideo,
    ] {
        assert_eq!(
            pair.frames[1]
                .iter()
                .filter(|f| f.purpose == purpose)
                .count(),
            16
        );
    }
}

#[test]
fn keyframe_requests_are_bound_to_the_requested_video_mid() {
    let mut pair = Pair::multi();
    pair.connect();
    for purpose in [Purpose::Camera, Purpose::ScreenVideo] {
        let track = pair.peers[0].track_for(purpose).unwrap();
        pair.peers[0].write(track, 0, pair.now, video()).unwrap();
    }
    pair.advance(Duration::from_millis(150)).unwrap();
    for purpose in [Purpose::Camera, Purpose::ScreenVideo] {
        pair.keyframes[0].clear();
        let requested = pair.peers[1].track_for(purpose).unwrap();
        pair.peers[1].request_keyframe(requested).unwrap();
        pair.advance(Duration::from_millis(500)).unwrap();
        assert!(!pair.keyframes[0].is_empty());
        assert!(
            pair.keyframes[0]
                .iter()
                .all(|&t| pair.peers[0].purpose(t) == Some(purpose))
        );
    }
    let audio = pair.peers[1].track_for(Purpose::SystemAudio).unwrap();
    assert!(matches!(
        pair.peers[1].request_keyframe(audio),
        Err(Error::Frame)
    ));
}

#[test]
fn each_audio_mid_rejects_tampering_and_replay_independently() {
    let mut pair = Pair::multi();
    pair.connect();
    for purpose in [Purpose::Microphone, Purpose::SystemAudio] {
        pair.frames[1].clear();
        let track = pair.peers[0].track_for(purpose).unwrap();
        pair.peers[0]
            .write(track, 0, pair.now, Arc::from([0xf8, 0xff, 0xfe]))
            .unwrap();
        pair.peers[0].timeout(pair.now).unwrap();
        pair.drain(0).unwrap();
        let index = pair
            .wire
            .iter()
            .position(|(_, _, d)| d.len() > 12 && d[0] >> 6 == 2 && !(192..=223).contains(&d[1]))
            .unwrap();
        let (to, source, original) = pair.wire.remove(index).unwrap();
        let mut tampered = original.clone();
        *tampered.last_mut().unwrap() ^= 1;
        let _ = pair.peers[to].receive(source, pair.now, &tampered);
        pair.advance(Duration::from_millis(25)).unwrap();
        assert!(pair.frames[to].is_empty());
        pair.peers[to].receive(source, pair.now, &original).unwrap();
        pair.advance(Duration::from_millis(25)).unwrap();
        assert_eq!(pair.frames[to].len(), 1);
        assert_eq!(pair.frames[to][0].purpose, purpose);
        let _ = pair.peers[to].receive(source, pair.now, &original);
        pair.advance(Duration::from_millis(25)).unwrap();
        assert_eq!(pair.frames[to].len(), 1);
    }
}

#[test]
fn a_flooded_system_audio_track_cannot_spend_the_microphone_or_video_allocations() {
    let mut pair = Pair::multi();
    pair.connect();
    let system = pair.peers[0].track_for(Purpose::SystemAudio).unwrap();
    let microphone = pair.peers[0].track_for(Purpose::Microphone).unwrap();
    for tick in 0..12 {
        pair.peers[0]
            .write(system, tick * 3840, pair.now, Arc::from([0xf8, 0xff, 0xfe]))
            .unwrap();
        for extra in 1..100 {
            assert!(matches!(
                pair.peers[0].write(
                    system,
                    tick * 3840 + extra,
                    pair.now,
                    Arc::from([0xf8, 0xff, 0xfe])
                ),
                Err(Error::Backpressure)
            ));
        }
        // Oversized frames and backwards clocks consume no media budget or
        // timestamp: the correct frame with this timestamp must still succeed.
        assert!(matches!(
            pair.peers[0].write(
                microphone,
                tick * 3840,
                pair.now,
                vec![0; MAX_OPUS_BYTES + 1].into()
            ),
            Err(Error::Frame)
        ));
        assert!(matches!(
            pair.peers[0].write(
                microphone,
                tick * 3840,
                pair.now - Duration::from_nanos(1),
                Arc::from([0xf8, 0xff, 0xfe])
            ),
            Err(Error::Clock)
        ));
        pair.peers[0]
            .write(
                microphone,
                tick * 3840,
                pair.now,
                Arc::from([0xf8, 0xff, 0xfe]),
            )
            .unwrap();
        for purpose in [Purpose::Camera, Purpose::ScreenVideo] {
            let track = pair.peers[0].track_for(purpose).unwrap();
            pair.peers[0]
                .write(track, tick * 7200, pair.now, video())
                .unwrap();
        }
        pair.advance(Duration::from_millis(80)).unwrap();
        assert!(matches!(
            pair.peers[0].write(system, tick * 3840, pair.now, Arc::from([0xf8, 0xff, 0xfe])),
            Err(Error::Frame)
        ));
    }
    pair.advance(Duration::from_millis(500)).unwrap();
    for purpose in [
        Purpose::Microphone,
        Purpose::SystemAudio,
        Purpose::Camera,
        Purpose::ScreenVideo,
    ] {
        assert_eq!(
            pair.frames[1]
                .iter()
                .filter(|f| f.purpose == purpose)
                .count(),
            12
        );
    }
}
