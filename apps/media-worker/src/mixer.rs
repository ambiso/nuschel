// SPDX-License-Identifier: EUPL-1.2
//! Voice from every server the user listens to, mixed into one output.
//!
//! One worker serves the whole application (docs/architecture.md, "Voice on
//! several servers"): each server is a stream the supervisor numbers, and a
//! speaker is a sender on one stream, since session numbers are per server.
//! Every speaker has its own jitter buffer and decoder; a frame of output is
//! the sum of what each has ready, at the volume its latest packet carried
//! (the sender's, the local per-user and the server's volume in one).
use crate::{
    dsp::FRAME,
    jitter::{JitterBuffer, Pull},
};
use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

/// Speakers mixed at once, over all servers, and on one server: a busy
/// server cannot take every place from the others.
const MAX_SPEAKERS: usize = 128;
const MAX_PER_STREAM: usize = 64;
/// Decoded audio kept per speaker: 200 ms.
const MAX_DECODED: usize = 10 * FRAME;
/// Larger than any Opus packet a client sends (Mumble limits them too).
const MAX_OPUS_BYTES: usize = 1024;

struct Speaker {
    decoder: OpusDecoder,
    jitter: JitterBuffer,
    pcm: VecDeque<[f32; 2]>,
    volume: f32,
    last_packet: Instant,
}

impl Speaker {
    fn new(now: Instant) -> Result<Self, opus_pure::Error> {
        Ok(Self {
            decoder: OpusDecoder::new(48_000, 2)?,
            jitter: JitterBuffer::default(),
            pcm: VecDeque::with_capacity(MAX_DECODED),
            volume: 1.,
            last_packet: now,
        })
    }

    /// Top up decoded audio for one output frame.
    fn fill(&mut self, scratch: &mut [f32]) {
        while self.pcm.len() < FRAME {
            match self.jitter.pull() {
                Pull::Packet { opus, end } => {
                    if let Ok(count) = self.decoder.decode(&opus, MAX_PACKET_SAMPLES, scratch) {
                        self.jitter.observe_frames((count / FRAME) as u64);
                        self.extend(&scratch[..2 * count]);
                    }
                    if end {
                        let _ = self.decoder.reset_state();
                    }
                }
                Pull::Lost { fec, frames } => {
                    let size = (frames as usize * FRAME).min(MAX_PACKET_SAMPLES);
                    let decoded = match fec {
                        Some(next) => self.decoder.decode_fec(&next, size, scratch),
                        None => self.decoder.decode(&[], size, scratch),
                    };
                    if let Ok(count) = decoded {
                        self.extend(&scratch[..2 * count]);
                    } else {
                        break;
                    }
                }
                Pull::Idle => break,
            }
        }
    }

    fn extend(&mut self, samples: &[f32]) {
        let volume = self.volume;
        // Malformed packets can decode far outside full scale (libopus does the
        // same); clamp per speaker so one bad stream cannot swamp the mix.
        self.pcm.extend(samples.chunks_exact(2).map(|frame| {
            let scaled = |sample: f32| {
                let value = sample * volume;
                if value.is_finite() {
                    value.clamp(-1., 1.)
                } else {
                    0.
                }
            };
            [scaled(frame[0]), scaled(frame[1])]
        }));
        // Never accumulate more than 200 ms of decoded audio per speaker.
        let excess = self.pcm.len().saturating_sub(MAX_DECODED);
        self.pcm.drain(..excess);
    }
}

/// One voice packet for the mix.
pub struct Voice {
    pub stream: u32,
    pub sender: u32,
    pub sequence: u64,
    pub opus: Vec<u8>,
    pub end: bool,
    pub volume: f32,
}

pub struct Mixer {
    /// By (stream, sender).
    speakers: BTreeMap<(u32, u32), Speaker>,
    scratch: Vec<f32>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            speakers: BTreeMap::new(),
            scratch: vec![0.; 2 * MAX_PACKET_SAMPLES],
        }
    }
}

impl Mixer {
    /// Queue a packet for its speaker. Malformed packets, and new speakers
    /// beyond the limits, are dropped.
    pub fn push(&mut self, voice: Voice, now: Instant) -> Result<(), opus_pure::Error> {
        let Voice {
            stream,
            sender,
            sequence,
            opus,
            end,
            volume,
        } = voice;
        if opus.is_empty() || opus.len() > MAX_OPUS_BYTES || !volume.is_finite() {
            return Ok(());
        }
        let key = (stream, sender);
        if !self.speakers.contains_key(&key)
            && (self.speakers.len() >= MAX_SPEAKERS || self.on_stream(stream) >= MAX_PER_STREAM)
        {
            return Ok(());
        }
        let speaker = match self.speakers.entry(key) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(Speaker::new(now)?),
        };
        // A restarted stream (sequence reset after silence) gets a fresh buffer.
        if now.saturating_duration_since(speaker.last_packet) > Duration::from_secs(1)
            && !speaker.jitter.is_playing()
        {
            speaker.jitter = JitterBuffer::default();
            let _ = speaker.decoder.reset_state();
        }
        speaker.volume = volume.clamp(0., 4.);
        speaker.last_packet = now;
        speaker.jitter.push(sequence, opus, end, now);
        Ok(())
    }

    /// Speakers on `stream`.
    fn on_stream(&self, stream: u32) -> usize {
        self.speakers
            .range((stream, 0)..=(stream, u32::MAX))
            .count()
    }

    /// Forget a server's speakers and what they had buffered.
    pub fn end_stream(&mut self, stream: u32) {
        self.speakers.retain(|(on, _), _| *on != stream);
    }

    /// Forget everyone (deafened).
    pub fn clear(&mut self) {
        self.speakers.clear();
    }

    /// Add one frame of every speaker's audio to `mix`.
    pub fn mix(&mut self, mix: &mut [[f32; 2]; FRAME]) {
        for speaker in self.speakers.values_mut() {
            speaker.fill(&mut self.scratch);
            for (out, sample) in mix
                .iter_mut()
                .zip(speaker.pcm.drain(..speaker.pcm.len().min(FRAME)))
            {
                out[0] += sample[0];
                out[1] += sample[1];
            }
        }
    }

    /// Drop speakers silent for a while.
    pub fn cleanup(&mut self, now: Instant) {
        self.speakers.retain(|_, speaker| {
            now.saturating_duration_since(speaker.last_packet) < Duration::from_secs(5)
                || speaker.jitter.is_playing()
        });
    }

    #[cfg(test)]
    fn speakers(&self) -> Vec<(u32, u32)> {
        self.speakers.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder};

    /// 20 ms Opus packets of a tone at `amplitude`.
    fn tone(count: usize, amplitude: f32) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip).unwrap();
        encoder.bitrate_bps = 48_000;
        let mut packet = [0_u8; MAX_PACKET_BYTES];
        (0..count)
            .map(|index| {
                let samples: Vec<f32> = (0..2 * FRAME)
                    .map(|n| {
                        let t = (index * 2 * FRAME + n) as f32 / 48_000.;
                        amplitude * (std::f32::consts::TAU * 440. * t).sin()
                    })
                    .collect();
                let size = encoder.encode(&samples, 2 * FRAME, &mut packet).unwrap();
                packet[..size].to_vec()
            })
            .collect()
    }

    fn feed(mixer: &mut Mixer, stream: u32, sender: u32, packets: &[Vec<u8>], volume: f32) {
        let now = Instant::now();
        for (index, opus) in packets.iter().enumerate() {
            mixer
                .push(
                    Voice {
                        stream,
                        sender,
                        sequence: 2 * index as u64,
                        opus: opus.clone(),
                        end: false,
                        volume,
                    },
                    now,
                )
                .unwrap();
        }
    }

    /// The loudest sample over `frames` mixed frames.
    fn peak(mixer: &mut Mixer, frames: usize) -> f32 {
        let mut loudest = 0_f32;
        for _ in 0..frames {
            let mut frame = [[0.; 2]; FRAME];
            mixer.mix(&mut frame);
            loudest = frame
                .iter()
                .flatten()
                .fold(loudest, |peak, sample| peak.max(sample.abs()));
        }
        loudest
    }

    #[test]
    fn the_same_session_on_two_servers_is_two_speakers() {
        let packets = tone(20, 0.25);
        let mut mixer = Mixer::default();
        feed(&mut mixer, 1, 7, &packets, 1.);
        feed(&mut mixer, 2, 7, &packets, 1.);
        assert_eq!(mixer.speakers(), [(1, 7), (2, 7)]);
        // Both servers' voices are in the mix: louder than either alone.
        let both = peak(&mut mixer, 20);
        let mut alone = Mixer::default();
        feed(&mut alone, 1, 7, &packets, 1.);
        let one = peak(&mut alone, 20);
        assert!(one > 0.1, "a single server's voice played at {one}");
        assert!(
            both > one * 1.5,
            "two servers mixed to {both}, one alone {one}"
        );
    }

    #[test]
    fn ending_a_stream_silences_only_that_server() {
        let packets = tone(20, 0.25);
        let mut mixer = Mixer::default();
        feed(&mut mixer, 1, 7, &packets, 1.);
        feed(&mut mixer, 2, 3, &packets, 1.);
        feed(&mut mixer, 2, 4, &packets, 1.);
        mixer.end_stream(2);
        assert_eq!(mixer.speakers(), [(1, 7)]);
        assert!(peak(&mut mixer, 20) > 0.1, "the other server went quiet");
        mixer.end_stream(1);
        assert_eq!(peak(&mut mixer, 5), 0.);
    }

    #[test]
    fn a_servers_volume_scales_its_voices() {
        let packets = tone(20, 0.25);
        let mut full = Mixer::default();
        feed(&mut full, 1, 7, &packets, 1.);
        let mut quiet = Mixer::default();
        feed(&mut quiet, 1, 7, &packets, 0.25);
        let (full, quiet) = (peak(&mut full, 20), peak(&mut quiet, 20));
        assert!((quiet / full - 0.25).abs() < 0.05, "{quiet} vs {full}");
        let mut silent = Mixer::default();
        feed(&mut silent, 1, 7, &packets, 0.);
        assert_eq!(peak(&mut silent, 20), 0.);
    }

    #[test]
    fn one_busy_server_cannot_take_every_place() {
        let packet = tone(1, 0.1);
        let mut mixer = Mixer::default();
        for sender in 0..100 {
            feed(&mut mixer, 1, sender, &packet, 1.);
        }
        assert_eq!(mixer.speakers().len(), MAX_PER_STREAM);
        for sender in 0..100 {
            feed(&mut mixer, 2, sender, &packet, 1.);
        }
        assert_eq!(mixer.speakers().len(), MAX_SPEAKERS);
        assert_eq!(mixer.on_stream(2), MAX_SPEAKERS - MAX_PER_STREAM);
    }

    #[test]
    fn malformed_packets_are_dropped() {
        let mut mixer = Mixer::default();
        let now = Instant::now();
        for (opus, volume) in [
            (Vec::new(), 1.),
            (vec![0; MAX_OPUS_BYTES + 1], 1.),
            (tone(1, 0.1).remove(0), f32::NAN),
            (tone(1, 0.1).remove(0), f32::INFINITY),
        ] {
            let voice = Voice {
                stream: 1,
                sender: 1,
                sequence: 0,
                opus,
                end: false,
                volume,
            };
            mixer.push(voice, now).unwrap();
        }
        assert!(mixer.speakers().is_empty());
    }
}
