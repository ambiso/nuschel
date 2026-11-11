// SPDX-License-Identifier: EUPL-1.2
//! Deterministic ICE/DTLS/SRTP exchange: datagrams stay in memory, no public STUN.
use nuschl_rtc::{
    Error, Event, Frame, Kind, MAX_DATAGRAM_BYTES, MAX_OPUS_BYTES, MAX_SDP_BYTES, Output, Peer,
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
}

impl Pair {
    fn new(video: bool, corrupt_fingerprint: bool) -> Self {
        let now = Instant::now();
        let addresses = [
            "127.0.0.1:41000".parse().unwrap(),
            "127.0.0.1:41001".parse().unwrap(),
        ];
        let mut peers = [
            Peer::new(addresses[0], now).unwrap(),
            Peer::new(addresses[1], now).unwrap(),
        ];
        let offer = peers[0].offer(video).unwrap();
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
        let answer = peers[1].accept_offer(&offer).unwrap();
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
        peers[0].accept_answer(&answer).unwrap();
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
                        self.video_packets += 1;
                        if self.video_packets == 2 {
                            self.drop_video = false;
                            self.dropped += 1;
                            continue;
                        }
                    }
                    self.wire.push_back((1 - from, source, data));
                }
                Output::Event(Event::Frame(frame)) => self.frames[from].push(frame),
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
    Arc::from(include_bytes!("../../../tests/fixtures/rtc-keyframe.vp8").as_slice())
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
