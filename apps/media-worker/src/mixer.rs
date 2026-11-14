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
use nuschl_audio_jitter::UnderrunFade;
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
const MAX_DECODED: usize = 20 * FRAME;
/// Larger than any Opus packet a client sends (Mumble limits them too).
const MAX_OPUS_BYTES: usize = 1024;

/// Gain is frozen with decoded audio, but applied only in the wide accumulator.
/// Finite f32 PCM × gain≤4 × 128 speakers cannot overflow f64.
#[derive(Clone, Copy)]
struct QueuedFrame {
    samples: [f32; 2],
    gain: f32,
}

/// All gain components have already been sampled by the sender/client path.
/// Keep that bounded control with its encoded packet through reordering; this
/// is not a live per-user volume/mute command for previously queued audio.
#[derive(Clone)]
struct EncodedPacket {
    opus: Vec<u8>,
    gain: f32,
}

struct Speaker {
    decoder: OpusDecoder,
    jitter: JitterBuffer<EncodedPacket>,
    pcm: VecDeque<QueuedFrame>,
    /// Gain of the preceding successfully decoded packet, also used for loss.
    volume: f32,
    last_packet: Instant,
    fade: UnderrunFade,
}

impl Speaker {
    fn new(now: Instant) -> Result<Self, opus_pure::Error> {
        let mut decoder = OpusDecoder::new(48_000, 2)?;
        // Match Mumble: retain intensity-stereo energy when folded to mono.
        decoder.set_phase_inversion_disabled(true);
        Ok(Self {
            decoder,
            jitter: JitterBuffer::default(),
            pcm: VecDeque::with_capacity(MAX_DECODED),
            volume: 1.,
            last_packet: now,
            fade: UnderrunFade::default(),
        })
    }

    /// Top up decoded audio for one output frame.
    fn fill(&mut self, scratch: &mut [f32]) {
        while self.pcm.len() < FRAME {
            let was_playing = self.jitter.is_playing();
            match self.jitter.pull() {
                Pull::Packet { opus, end } => {
                    let plays = !end || self.fade.end();
                    if let Ok(count) = self.decoder.decode(&opus.opus, MAX_PACKET_SAMPLES, scratch)
                    {
                        self.jitter.observe_frames((count / FRAME) as u64);
                        self.volume = opus.gain;
                        if plays {
                            self.fade.play(&mut scratch[..2 * count]);
                            self.extend(&scratch[..2 * count]);
                        }
                    }
                    if end {
                        let _ = self.decoder.reset_state();
                    }
                }
                Pull::Lost { fec, frames } => {
                    let size = (frames as usize * FRAME).min(MAX_PACKET_SAMPLES);
                    let decoded = match fec {
                        Some(next) => self.decoder.decode_fec(&next.opus, size, scratch),
                        None => self.decoder.decode(&[], size, scratch),
                    };
                    if let Ok(count) = decoded {
                        self.fade.play(&mut scratch[..2 * count]);
                        self.extend(&scratch[..2 * count]);
                    } else {
                        break;
                    }
                }
                Pull::Idle => {
                    if self.fade.ran_dry(was_playing)
                        && let Ok(count) = self.decoder.decode(&[], FRAME, scratch)
                    {
                        self.fade.fade_out(&mut scratch[..2 * count]);
                        self.extend(&scratch[..2 * count]);
                    }
                    break;
                }
            }
        }
    }

    fn extend(&mut self, samples: &[f32]) {
        self.pcm
            .extend(samples.as_chunks::<2>().0.iter().map(|frame| QueuedFrame {
                samples: [frame[0], frame[1]].map(
                    |sample: f32| {
                        if sample.is_finite() { sample } else { 0. }
                    },
                ),
                gain: self.volume,
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
        let duration = opus_pure::packet::samples_48k(&opus).ok();
        if opus.is_empty()
            || opus.len() > MAX_OPUS_BYTES
            || !volume.is_finite()
            || opus_pure::packet::validate(&opus).is_err()
            || duration.is_none_or(|frames| {
                frames == 0 || frames > MAX_PACKET_SAMPLES || frames % FRAME != 0
            })
        {
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
            speaker.fade = UnderrunFade::default();
        }
        speaker.last_packet = now;
        speaker.jitter.push(
            sequence,
            EncodedPacket {
                opus,
                gain: volume.clamp(0., 4.),
            },
            end,
            now,
        );
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
    pub fn mix(&mut self, mix: &mut [[f64; 2]; FRAME]) {
        for speaker in self.speakers.values_mut() {
            speaker.fill(&mut self.scratch);
            for (out, sample) in mix
                .iter_mut()
                .zip(speaker.pcm.drain(..speaker.pcm.len().min(FRAME)))
            {
                out[0] += f64::from(sample.samples[0]) * f64::from(sample.gain);
                out[1] += f64::from(sample.samples[1]) * f64::from(sample.gain);
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

/// All voice/local effects and mono mapping are combined at f64 precision.
/// Device-rate SRC runs before the only unit-amplitude clip in playback.rs.
/// At most128 speakers × gain4, six local sound voices × gain1, one
/// loopback frame and one echo probe fit below1024 times finite f32::MAX.
/// This is a representational guard, not full-scale audio clipping.
pub(crate) const MAX_MIX_SAMPLE: f64 = f32::MAX as f64 * 1024.;

fn finite_mix(sample: f64) -> f64 {
    if sample.is_finite() {
        sample.clamp(-MAX_MIX_SAMPLE, MAX_MIX_SAMPLE)
    } else {
        0.
    }
}
pub fn finish(
    channels: usize,
    wide: &[[f64; 2]; FRAME],
    mono: &[f64; FRAME],
    out: &mut [[f64; 2]; FRAME],
) {
    for ((voices, effect), output) in wide.iter().zip(mono).zip(out) {
        if channels == 1 {
            // Fold before any conversion; large opposing LR must cancel before
            // local effects are added, without overflowing an intermediate f32.
            let heard = finite_mix((voices[0] + voices[1]) * 0.5 + effect);
            *output = [heard; 2];
        } else {
            for channel in 0..2 {
                output[channel] = finite_mix(voices[channel] + effect);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder};

    #[test]
    fn malformed_framing_is_rejected_before_speaker_or_timing_mutation() {
        let now = Instant::now();
        let malformed = || Voice {
            stream: 1,
            sender: 1,
            sequence: 2,
            opus: vec![0x09, 0],
            end: false,
            volume: 4.,
        };
        let mut injected = Mixer::default();
        injected.push(malformed(), now).unwrap();
        assert!(injected.speakers.is_empty());
        let packets = tone(3, 0.2);
        let mut clean = Mixer::default();
        for mixer in [&mut injected, &mut clean] {
            feed(mixer, 1, 1, &packets, 0.25);
        }
        let before = injected.speakers[&(1, 1)].last_packet;
        injected
            .push(malformed(), now + Duration::from_secs(2))
            .unwrap();
        assert_eq!(injected.speakers[&(1, 1)].last_packet, before);
        assert_eq!(injected.speakers[&(1, 1)].volume, 1.);
        for _ in 0..12 {
            let mut actual = [[0.; 2]; FRAME];
            let mut expected = [[0.; 2]; FRAME];
            injected.mix(&mut actual);
            clean.mix(&mut expected);
            assert_eq!(actual, expected);
        }
    }

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
    fn peak(mixer: &mut Mixer, frames: usize) -> f64 {
        let mut loudest = 0_f64;
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

    fn queued(mixer: &mut Mixer, key: (u32, u32), frames: &[[f32; 2]], gain: f32) {
        let mut speaker = Speaker::new(Instant::now()).unwrap();
        speaker.volume = gain;
        speaker.extend(&frames.iter().flatten().copied().collect::<Vec<_>>());
        mixer.speakers.insert(key, speaker);
    }

    #[test]
    fn an_underrun_fades_out_and_back_in_and_a_late_terminator_stays_silent() {
        // A 0.5 sine: packets 0..15 arrive on time, the sender stalls for
        // 300 ms, sends 15..30 and runs dry again, and its terminator comes
        // last. Playout fades to silence and back instead of stepping, and the
        // terminator only ends audio that has already faded out (issue #206).
        let packets = tone(31, 0.5);
        let start = Instant::now();
        let mut mixer = Mixer::default();
        let mut left = Vec::new();
        for tick in 0..140 {
            for (index, opus) in packets.iter().enumerate() {
                let at = match index {
                    0..15 => 20 * index,
                    15..30 => 20 * index + 300,
                    _ => 1200,
                };
                if at == 10 * tick {
                    let voice = Voice {
                        stream: 1,
                        sender: 1,
                        sequence: 2 * index as u64,
                        opus: opus.clone(),
                        end: index == 30,
                        volume: 1.,
                    };
                    let now = start + Duration::from_millis(at as u64);
                    mixer.push(voice, now).unwrap();
                }
            }
            let mut wide = [[0.; 2]; FRAME];
            mixer.mix(&mut wide);
            left.extend(wide.iter().map(|frame| frame[0]));
        }
        let silent = left
            .chunks(FRAME)
            .filter(|frame| frame.iter().all(|v| *v == 0.));
        assert!(silent.count() > 40, "the stream must run dry twice");
        // A 0.5 sine at 440 Hz changes by at most 0.029 per sample.
        let step = left
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0., f64::max);
        assert!(step < 0.05, "{step}");
        assert!(
            left[110 * FRAME..].iter().all(|v| *v == 0.),
            "terminator played"
        );
    }

    #[test]
    fn voice_receivers_explicitly_match_mumbles_phase_setting() {
        let mut speaker = Speaker::new(Instant::now()).unwrap();
        assert!(speaker.decoder.phase_inversion_disabled());
        speaker.decoder.reset_state().unwrap();
        assert!(speaker.decoder.phase_inversion_disabled());
    }

    #[test]
    fn opposing_headroom_and_effects_are_summed_before_one_mix_clip() {
        let mut mixer = Mixer::default();
        queued(&mut mixer, (1, 1), &[[1.5, -1.5]; FRAME], 1.);
        queued(&mut mixer, (2, 1), &[[-1.25, 1.25]; FRAME], 1.);
        let mut wide = [[0.; 2]; FRAME];
        let mut out = [[0.; 2]; FRAME];
        let count = allocation_counter::measure(|| {
            mixer.mix(&mut wide);
            finish(2, &wide, &[0.; FRAME], &mut out);
        });
        assert_eq!(count.count_total, 0);
        assert!(out.iter().all(|frame| *frame == [0.25, -0.25]));
        // A voice sum above full scale must still cancel against local effects.
        finish(2, &[[1.5, 2.]; FRAME], &[-1.25; FRAME], &mut out);
        assert!(out.iter().all(|frame| *frame == [0.25, 0.75]));
    }

    #[test]
    fn gain_is_frozen_per_frame_and_finite_extremes_never_overflow_the_sum() {
        assert_eq!(std::mem::size_of::<QueuedFrame>(), 12);
        let mut mixer = Mixer::default();
        for speaker in 0..MAX_SPEAKERS {
            let sign = if speaker % 2 == 0 { 1. } else { -1. };
            queued(
                &mut mixer,
                (speaker as u32, 1),
                &[[sign * f32::MAX, sign * 1.5]; FRAME],
                4.,
            );
        }
        let mut wide = [[0.; 2]; FRAME];
        mixer.mix(&mut wide);
        assert!(wide.iter().all(|frame| *frame == [0., 0.]));
        let mut speaker = Speaker::new(Instant::now()).unwrap();
        speaker.volume = 4.;
        speaker.extend(&[f32::MAX, -f32::MAX, f32::NAN, f32::INFINITY]);
        speaker.volume = 0.;
        speaker.extend(&[1.5, -1.25]);
        mixer.clear();
        mixer.speakers.insert((1, 1), speaker);
        mixer.mix(&mut wide);
        assert_eq!(
            wide[0],
            [f64::from(f32::MAX) * 4., -f64::from(f32::MAX) * 4.]
        );
        assert_eq!(wide[1], [0.; 2]);
        assert_eq!(wide[2], [0.; 2]);
        assert!(wide.iter().flatten().all(|sample| sample.is_finite()));
        let mut out = [[0.; 2]; FRAME];
        finish(2, &wide, &[0.; FRAME], &mut out);
        assert_eq!(
            out[0],
            [f64::from(f32::MAX) * 4., -f64::from(f32::MAX) * 4.]
        );
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

#[cfg(test)]
#[path = "mixer_fixture.rs"]
mod mixer_fixture;
