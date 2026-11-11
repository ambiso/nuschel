// SPDX-License-Identifier: EUPL-1.2
//! Two local UDP endpoints; synthetic encoded media only, no capture or public services.
use nuschl_rtc::{Error, Event, Kind, MAX_DATAGRAM_BYTES, MAX_OPUS_BYTES, Output, Peer};
use std::{
    io,
    net::UdpSocket,
    sync::Arc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Endpoint {
    peer: Peer,
    socket: UdpSocket,
    wake: Instant,
    audio: usize,
    video: usize,
    audio_timestamp: Option<u64>,
    video_timestamp: Option<u64>,
    max_delay: Duration,
    estimates: usize,
    sent_video: usize,
    first_opus: Option<Arc<[u8]>>,
}

impl Endpoint {
    fn new(now: Instant) -> Result<Self> {
        let socket = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        socket.set_nonblocking(true)?;
        let peer = Peer::new(socket.local_addr()?, now)?;
        Ok(Self {
            peer,
            socket,
            wake: now,
            audio: 0,
            video: 0,
            audio_timestamp: None,
            video_timestamp: None,
            max_delay: Duration::ZERO,
            estimates: 0,
            sent_video: 0,
            first_opus: None,
        })
    }

    fn step(
        &mut self,
        now: Instant,
        started: Option<Instant>,
        opus: &[u8],
        video: &[u8],
    ) -> Result<()> {
        // One receive per turn, then drain output before reading again. A busy
        // ingress cannot starve the other endpoint's pacer or retransmissions.
        let mut buffer = [0; MAX_DATAGRAM_BYTES + 1];
        match self.socket.recv_from(&mut buffer) {
            Ok((size, source)) => self.peer.receive(source, now, &buffer[..size])?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if now >= self.wake {
            self.peer.timeout(now)?;
        }
        for _ in 0..256 {
            match self.peer.poll()? {
                Output::WakeAt(at) => {
                    self.wake = at;
                    return Ok(());
                }
                Output::Datagram {
                    source,
                    destination,
                    data,
                } => {
                    if source != self.socket.local_addr()? || !destination.ip().is_loopback() {
                        return Err("probe attempted a non-local route".into());
                    }
                    self.socket.send_to(&data, destination)?;
                }
                Output::Event(Event::Frame(frame)) => {
                    if frame.kind == Kind::Audio && self.first_opus.is_none() {
                        self.first_opus = Some(frame.data.clone());
                    }
                    let (count, previous, expected, ticks) = match frame.kind {
                        Kind::Audio => (&mut self.audio, &mut self.audio_timestamp, opus, 48_000),
                        Kind::Video => (&mut self.video, &mut self.video_timestamp, video, 90_000),
                    };
                    if frame.data.as_ref() != expected
                        || previous.is_some_and(|v| frame.timestamp <= v)
                    {
                        return Err("media was corrupted, duplicated or reordered".into());
                    }
                    *previous = Some(frame.timestamp);
                    *count += 1;
                    let started = started.ok_or("received media before sending")?;
                    // This measures encoded-frame delivery in this polling probe,
                    // not microphone-to-speaker latency or A/V synchronization.
                    let elapsed = now.duration_since(started);
                    let sent_at = Duration::from_secs_f64(frame.timestamp as f64 / ticks as f64);
                    self.max_delay = self.max_delay.max(elapsed.saturating_sub(sent_at));
                }
                Output::Event(Event::BandwidthEstimate(_)) => self.estimates += 1,
                Output::Event(Event::Disconnected) => return Err("peer disconnected".into()),
                _ => {}
            }
        }
        Err("peer exceeded the per-turn output budget".into())
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let opus: Arc<[u8]> = match args.next().as_deref() {
        None => Arc::from([0xf8, 0xff, 0xfe]), // Standard Opus silence packet.
        Some("--opus-hex") => {
            let hex = args.next().ok_or("--opus-hex requires an encoded packet")?;
            if hex.is_empty()
                || hex.len() > MAX_OPUS_BYTES * 2
                || hex.len() % 2 != 0
                || !hex.is_ascii()
            {
                return Err("invalid Opus hex length".into());
            }
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into()
        }
        Some("--help" | "-h") => {
            println!(
                "Local UDP audio/video transport probe. No microphone or public servers.\nUsage: nuschl-rtc-probe [--opus-hex HEX]"
            );
            return Ok(());
        }
        _ => return Err("usage: nuschl-rtc-probe [--opus-hex HEX]".into()),
    };
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    run(opus)
}

fn run(opus: Arc<[u8]>) -> Result<()> {
    // A locally generated test pattern, never camera capture. Repeated keyframes
    // exercise encoded-frame delivery; live video decoding/rendering is separate.
    let video: Arc<[u8]> =
        Arc::from(include_bytes!("../../../tests/fixtures/rtc-keyframe.vp8").as_slice());
    let now = Instant::now();
    let mut left = Endpoint::new(now)?;
    let mut right = Endpoint::new(now)?;
    let offer = left.peer.offer(true)?;
    let answer = right.peer.accept_offer(&offer)?;
    left.peer.accept_answer(&answer)?;
    let mut started = None;
    let mut sent = 0;
    let deadline = now + Duration::from_secs(10);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err("local transport trial timed out".into());
        }
        left.step(now, started, &opus, &video)?;
        right.step(now, started, &opus, &video)?;
        if left.peer.is_connected() && right.peer.is_connected() {
            let start = *started.get_or_insert(now);
            if sent < 50 && now.duration_since(start) >= Duration::from_millis(sent * 20) {
                for endpoint in [&mut left, &mut right] {
                    let peer = &mut endpoint.peer;
                    peer.write(
                        peer.track(Kind::Audio).ok_or("missing Opus track")?,
                        sent * 960,
                        now,
                        opus.clone(),
                    )?;
                    if sent % 3 == 0 {
                        match peer.write(
                            peer.track(Kind::Video).ok_or("missing VP8 track")?,
                            sent * 1800,
                            now,
                            video.clone(),
                        ) {
                            Ok(()) => endpoint.sent_video += 1,
                            Err(Error::Backpressure) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                sent += 1;
            }
        }
        if sent == 50
            && left.audio == 50
            && right.audio == 50
            && left.video == right.sent_video
            && right.video == left.sent_video
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    left.peer.close();
    right.peer.close();
    if left.video == 0 || right.video == 0 {
        return Err("no video was delivered".into());
    }
    let delay = left.max_delay.max(right.max_delay);
    if delay > Duration::from_millis(500) {
        return Err("encoded media accumulated over 500 ms of delay".into());
    }
    let received_hex: String = left
        .first_opus
        .ok_or("no received Opus packet")?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!(
        "{{\"audio_frames_each_way\":50,\"video_frames_received\":[{},{}],\"video_frames_dropped\":{},\"video_frame_bytes\":{},\"max_encoded_delivery_ms\":{},\"bandwidth_events\":{},\"received_opus_hex\":\"{}\"}}",
        left.video,
        right.video,
        34 - left.sent_video - right.sent_video,
        video.len(),
        delay.as_millis(),
        left.estimates + right.estimates,
        received_hex,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_udp_audio_and_video() {
        super::run(std::sync::Arc::from([0xf8, 0xff, 0xfe])).unwrap();
    }
}
