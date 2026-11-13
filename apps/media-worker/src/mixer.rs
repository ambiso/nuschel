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
    flow: Flow,
}

/// Whether decoded audio is reaching the mix (issue #206). Only the edges of
/// an underrun fade; every other sample is the decoder's, at its packet gain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// The decoder is new or reset: nothing has played since the speaker
    /// appeared or its transmission ended, and nothing needs to fade.
    Stopped,
    Playing,
    /// Playout ran dry mid-transmission and faded out. The decoder still
    /// carries that waveform, so whatever plays next fades in.
    Underrun,
}

/// Raised-cosine ramp over the first 10 ms of interleaved stereo: up from
/// silence when `rising`, else down to silence.
fn fade(samples: &mut [f32], rising: bool) {
    for (index, frame) in samples
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .take(FRAME)
        .enumerate()
    {
        let up = 0.5 - 0.5 * (std::f32::consts::PI * (index as f32 + 0.5) / FRAME as f32).cos();
        let gain = if rising { up } else { 1. - up };
        frame[0] *= gain;
        frame[1] *= gain;
    }
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
            flow: Flow::Stopped,
        })
    }

    /// Top up decoded audio for one output frame.
    fn fill(&mut self, scratch: &mut [f32]) {
        while self.pcm.len() < FRAME {
            match self.jitter.pull() {
                Pull::Packet { opus, end } => {
                    if let Ok(count) = self.decoder.decode(&opus.opus, MAX_PACKET_SAMPLES, scratch)
                    {
                        self.jitter.observe_frames((count / FRAME) as u64);
                        self.volume = opus.gain;
                        // A terminator straight after an underrun only ends
                        // audio that has already faded out.
                        if !(end && self.flow == Flow::Underrun) {
                            self.play(&mut scratch[..2 * count]);
                        }
                    }
                    if end {
                        let _ = self.decoder.reset_state();
                        self.flow = Flow::Stopped;
                    }
                }
                Pull::Lost { fec, frames } => {
                    let size = (frames as usize * FRAME).min(MAX_PACKET_SAMPLES);
                    let decoded = match fec {
                        Some(next) => self.decoder.decode_fec(&next.opus, size, scratch),
                        None => self.decoder.decode(&[], size, scratch),
                    };
                    if let Ok(count) = decoded {
                        self.play(&mut scratch[..2 * count]);
                    } else {
                        break;
                    }
                }
                Pull::Idle => {
                    // Ran dry mid-waveform: continue it with 10 ms of
                    // concealment that fades to silence, instead of a step.
                    if self.flow == Flow::Playing {
                        self.flow = Flow::Underrun;
                        if let Ok(count) = self.decoder.decode(&[], FRAME, scratch) {
                            fade(&mut scratch[..2 * count], false);
                            self.extend(&scratch[..2 * count]);
                        }
                    }
                    break;
                }
            }
        }
    }

    /// Queue decoded or concealed audio, fading in when it resumes an underrun.
    fn play(&mut self, samples: &mut [f32]) {
        if self.flow == Flow::Underrun {
            fade(samples, true);
        }
        self.flow = Flow::Playing;
        self.extend(samples);
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
            speaker.flow = Flow::Stopped;
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
    use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder, RateControl};

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

    /// 20 ms stereo packets of synthetic music, encoded like the music bot's:
    /// a chord with overtones, a bass line and a melody with soft attacks.
    fn music(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        encoder.rate_control = RateControl::ConstrainedVbr;
        encoder.bitrate_bps = 96_000;
        let mut packet = [0_u8; MAX_PACKET_BYTES];
        let wave = |hz: f32, t: f32| (std::f32::consts::TAU * hz * t).sin();
        (0..count)
            .map(|index| {
                let samples: Vec<f32> = (0..2 * FRAME)
                    .flat_map(|n| {
                        let t = (index * 2 * FRAME + n) as f32 / 48_000.;
                        let note = [440., 494., 554., 659.][(t * 4.) as usize % 4];
                        let attack = (t * 4.).fract();
                        let envelope = (attack * 40.).min(1.) * (1. - 0.6 * attack);
                        let chord: f32 = [220., 277.18, 329.63]
                            .iter()
                            .map(|hz| wave(*hz, t) + 0.3 * wave(2. * hz, t))
                            .sum();
                        let bass = 0.2 * wave(110., t);
                        [
                            0.08 * chord + bass + 0.15 * envelope * wave(note, t),
                            0.06 * chord + bass + 0.12 * envelope * wave(1.5 * note, t),
                        ]
                    })
                    .collect();
                let size = encoder.encode(&samples, 2 * FRAME, &mut packet).unwrap();
                packet[..size].to_vec()
            })
            .collect()
    }

    /// One packet of a speaker, queued when the virtual clock reaches `at` ms.
    struct Arrival {
        at: u64,
        sequence: u64,
        opus: Vec<u8>,
        end: bool,
        volume: f32,
    }

    /// Mix `ticks` 10 ms frames of one speaker on a virtual clock.
    fn clocked(arrivals: &[Arrival], ticks: u64) -> Vec<[f64; 2]> {
        let start = Instant::now();
        let mut mixer = Mixer::default();
        let mut output = Vec::new();
        let mut next = 0;
        for tick in 0..ticks {
            while let Some(arrival) = arrivals.get(next).filter(|a| a.at <= 10 * tick) {
                let voice = Voice {
                    stream: 1,
                    sender: 1,
                    sequence: arrival.sequence,
                    opus: arrival.opus.clone(),
                    end: arrival.end,
                    volume: arrival.volume,
                };
                mixer
                    .push(voice, start + Duration::from_millis(arrival.at))
                    .unwrap();
                next += 1;
            }
            let mut wide = [[0.; 2]; FRAME];
            mixer.mix(&mut wide);
            output.extend(wide);
        }
        output
    }

    /// The speaker's decoder on its own, as an independent oracle.
    fn plain() -> OpusDecoder {
        let mut decoder = OpusDecoder::new(48_000, 2).unwrap();
        decoder.set_phase_inversion_disabled(true);
        decoder
    }

    /// Decode a packet, or conceal `frames` when `opus` is empty.
    fn decoded(decoder: &mut OpusDecoder, opus: &[u8], frames: usize) -> Vec<f32> {
        let mut pcm = vec![0.; 2 * MAX_PACKET_SAMPLES];
        let count = decoder.decode(opus, frames, &mut pcm).unwrap();
        pcm.truncate(2 * count);
        pcm
    }

    /// Recover the 20 ms before `next` from its in-band FEC.
    fn recovered(decoder: &mut OpusDecoder, next: &[u8]) -> Vec<f32> {
        let mut pcm = vec![0.; 2 * MAX_PACKET_SAMPLES];
        let count = decoder.decode_fec(next, 2 * FRAME, &mut pcm).unwrap();
        pcm.truncate(2 * count);
        pcm
    }

    /// The specified 10 ms raised-cosine edge: up from or down to silence.
    fn ramped(mut pcm: Vec<f32>, rising: bool) -> Vec<f32> {
        for (index, frame) in pcm.as_chunks_mut::<2>().0.iter_mut().take(480).enumerate() {
            let up = 0.5 - 0.5 * (std::f32::consts::PI * (index as f32 + 0.5) / 480.).cos();
            for sample in frame {
                *sample *= if rising { up } else { 1. - up };
            }
        }
        pcm
    }

    /// Raw stereo at `volume`, with the same arithmetic as `Mixer::mix`.
    fn gained(pcm: &[f32], volume: f32) -> Vec<[f64; 2]> {
        let wide = |sample: f32| 0. + f64::from(sample) * f64::from(volume);
        pcm.as_chunks::<2>()
            .0
            .iter()
            .map(|frame| frame.map(wide))
            .collect()
    }

    /// Where `expected` lies in `output` after nothing but silence from `from`:
    /// every one of its samples must be bit-identical. Returns its end.
    fn find(output: &[[f64; 2]], from: usize, expected: &[[f64; 2]]) -> usize {
        let silent = |frames: &[[f64; 2]]| frames.iter().take_while(|v| **v == [0.; 2]).count();
        let start = from + silent(&output[from..]) - silent(expected);
        let actual = &output[start..start + expected.len()];
        let differs = actual
            .iter()
            .zip(expected)
            .position(|(a, b)| a.map(f64::to_bits) != b.map(f64::to_bits));
        assert_eq!(differs, None, "first difference after {start} frames");
        start + expected.len()
    }

    #[test]
    fn an_underrun_fades_out_and_the_resumed_stream_fades_in() {
        // Packets 0..15 arrive on time, then the sender stalls for 300 ms:
        // playout fades to silence and back instead of stepping (issue #206).
        let arrivals: Vec<Arrival> = tone(40, 0.5)
            .into_iter()
            .enumerate()
            .map(|(index, opus)| Arrival {
                at: 20 * index as u64 + if index < 15 { 0 } else { 300 },
                sequence: 2 * index as u64,
                opus,
                end: false,
                volume: 1.,
            })
            .collect();
        let left: Vec<f64> = clocked(&arrivals, 140)
            .iter()
            .map(|frame| frame[0])
            .collect();
        let silent = left
            .chunks(FRAME)
            .filter(|frame| frame.iter().all(|v| *v == 0.));
        assert!(silent.count() > 12, "the stall must underrun");
        // A 0.5 sine at 440 Hz changes by at most 0.029 per sample.
        let step = left
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0., f64::max);
        assert!(step < 0.05, "{step}");
    }

    #[test]
    fn an_underrun_changes_only_its_two_10_ms_edges() {
        // Music packets 0..20 arrive on time, then the sender stalls for 300 ms
        // and sends 20..40, the last as its terminator. The jitter buffer
        // conceals 40 ms as before and runs dry; 10 ms more concealment fades
        // out, and the resumed packet fades in over its first 10 ms. Every
        // other sample is the decoder's own at its packet's gain.
        let packets = music(40);
        let volume = |index: usize| [1., 0.5, 2.][index % 3];
        let arrivals: Vec<Arrival> = packets
            .iter()
            .enumerate()
            .map(|(index, opus)| Arrival {
                at: 20 * index as u64 + if index < 20 { 0 } else { 300 },
                sequence: 2 * index as u64,
                opus: opus.clone(),
                end: index == 39,
                volume: volume(index),
            })
            .collect();
        let output = clocked(&arrivals, 160);
        let mut decoder = plain();
        let mut before = Vec::new();
        for (index, opus) in packets[..20].iter().enumerate() {
            before.extend(gained(
                &decoded(&mut decoder, opus, MAX_PACKET_SAMPLES),
                volume(index),
            ));
        }
        for _ in 0..2 {
            before.extend(gained(&decoded(&mut decoder, &[], 2 * FRAME), volume(19)));
        }
        let tail = ramped(decoded(&mut decoder, &[], FRAME), false);
        before.extend(gained(&tail, volume(19)));
        let mut after = Vec::new();
        for (index, opus) in packets.iter().enumerate().skip(20) {
            let pcm = decoded(&mut decoder, opus, MAX_PACKET_SAMPLES);
            let pcm = if index == 20 { ramped(pcm, true) } else { pcm };
            after.extend(gained(&pcm, volume(index)));
        }
        let resumed = find(&output, 0, &before);
        assert!(
            output[resumed..resumed + 20 * FRAME]
                .iter()
                .all(|v| *v == [0.; 2])
        );
        let end = find(&output, resumed, &after);
        assert!(output[end..].iter().all(|frame| *frame == [0.; 2]));
        // The edges are 10 ms long and stay strictly between silence and unity.
        let ramp = ramped(vec![1.; 2 * FRAME + 2], true);
        assert!(ramp[..2 * FRAME].windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(ramp[0] > 0. && ramp[2 * FRAME - 1] < 1. && ramp[2 * FRAME] == 1.);
    }

    #[test]
    fn without_an_underrun_music_is_bit_identical_to_plain_decoding() {
        // Two transmissions of music, each closed by its terminator, with a
        // reordered pair, a packet recovered from FEC, one concealed and
        // changing gains. Nothing runs dry, so nothing fades: steady-state
        // output and concealment are unchanged (issue #206).
        let packets = music(50);
        let volume = |index: usize| [1., 0.5, 2., 0.75][index % 4];
        let mut arrivals: Vec<Arrival> = packets
            .iter()
            .enumerate()
            .filter(|(index, _)| ![10, 20, 21].contains(index))
            .map(|(index, opus)| {
                let index = index as u64;
                let (at, sequence) = match index {
                    5 => (110, 10),
                    6 => (105, 12),
                    0..30 => (20 * index, 2 * index),
                    // The next transmission restarts its sequence numbers.
                    _ => (750 + 20 * (index - 30), 2 * (index - 30)),
                };
                Arrival {
                    at,
                    sequence,
                    opus: opus.clone(),
                    end: index == 29 || index == 49,
                    volume: volume(index as usize),
                }
            })
            .collect();
        arrivals.sort_by_key(|arrival| arrival.at);
        let output = clocked(&arrivals, 130);
        let mut decoder = plain();
        let (mut first, mut second, mut gain) = (Vec::new(), Vec::new(), 1.);
        for (index, opus) in packets.iter().enumerate() {
            let pcm = match index {
                10 => recovered(&mut decoder, &packets[11]),
                // Nothing after it has arrived yet: concealment, then FEC.
                20 => decoded(&mut decoder, &[], 2 * FRAME),
                21 => recovered(&mut decoder, &packets[22]),
                _ => {
                    gain = volume(index);
                    decoded(&mut decoder, opus, MAX_PACKET_SAMPLES)
                }
            };
            let transmission = if index < 30 { &mut first } else { &mut second };
            transmission.extend(gained(&pcm, gain));
            if index == 29 {
                decoder.reset_state().unwrap();
            }
        }
        let end = find(&output, 0, &first);
        let end = find(&output, end, &second);
        assert!(output[end..].iter().all(|frame| *frame == [0.; 2]));
    }

    #[test]
    fn a_terminator_after_an_underrun_is_dropped_and_the_next_stream_starts_clean() {
        // The sender stalls after packet 14 and later sends only its
        // terminator: that is the tail of audio which already faded out, so it
        // stays silent. The decoder resets as for any terminator, and the next
        // transmission plays bit-identically, without a fade.
        let tone = tone(16, 0.5);
        let music = music(10);
        let mut arrivals: Vec<Arrival> = tone
            .iter()
            .enumerate()
            .map(|(index, opus)| Arrival {
                at: if index < 15 { 20 * index as u64 } else { 600 },
                sequence: 2 * index as u64,
                opus: opus.clone(),
                end: index == 15,
                volume: 1.,
            })
            .collect();
        arrivals.extend(music.iter().enumerate().map(|(index, opus)| Arrival {
            at: 800 + 20 * index as u64,
            sequence: 2 * index as u64,
            opus: opus.clone(),
            end: index == 9,
            volume: 1.,
        }));
        let output = clocked(&arrivals, 120);
        assert!(output[40 * FRAME..80 * FRAME].iter().all(|v| *v == [0.; 2]));
        let mut decoder = plain();
        let expected: Vec<[f64; 2]> = music
            .iter()
            .flat_map(|opus| gained(&decoded(&mut decoder, opus, MAX_PACKET_SAMPLES), 1.))
            .collect();
        let end = find(&output, 40 * FRAME, &expected);
        assert!(output[end..].iter().all(|frame| *frame == [0.; 2]));
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
