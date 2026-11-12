// SPDX-License-Identifier: EUPL-1.2
//! Voice activity in what the bot hears, for ducking: each talker's Opus is
//! decoded (opus-pure) and every 10 ms frame is judged. A frame is speech when
//! RNNoise thinks so *and* it stands well above the talker's own background
//! level, which speech does between syllables and steady noise never does:
//! RNNoise alone takes quiet hiss for speech. An open microphone, silence,
//! hum or background noise does not duck the music.
use nnnoiseless::DenoiseState;
use opus_pure::OpusDecoder;
use std::collections::HashMap;

const RATE: i32 = 48_000;
/// RNNoise's frame: 10 ms.
const FRAME: usize = DenoiseState::FRAME_SIZE;
/// The longest Opus packet, 120 ms.
const MAX_PACKET_SAMPLES: usize = 5760;
/// RNNoise works on 16-bit scaled samples.
const SCALE: f32 = 32768.;
/// Quieter frames are not speech, whatever RNNoise thinks (as in the client's gate).
const SILENCE_DB: f32 = -60.;
/// Frames of speech in a row before a talker counts as speaking, so that a
/// click or a cough does not duck the music.
const ONSET_FRAMES: u32 = 3;
/// How far above the talker's background level a frame must be.
const SPEECH_ABOVE_FLOOR_DB: f32 = 9.;
/// The background level is the quietest frame of the last 1.5 s, kept as
/// the minima of 100 ms blocks: speech has quiet gaps that often, steady
/// noise never rises above it.
const BLOCK_FRAMES: u32 = 10;
const BLOCKS: usize = 15;
/// Talkers followed at once; more are ignored rather than allocating without
/// bound for a server that invents sessions.
const MAX_TALKERS: usize = 64;

/// The speech probability a frame needs, from a sensitivity in percent
/// (the mapping of the client's voice activation).
pub fn threshold(sensitivity: u8) -> f32 {
    0.9 - 0.6 * f32::from(sensitivity.min(100)) / 100.
}

/// Turns per-frame speech probabilities and levels into speech, per talker.
#[derive(Default)]
struct Gate {
    /// Minima of the last complete blocks, oldest first.
    blocks: std::collections::VecDeque<f32>,
    /// The minimum of the block being filled, and its frames so far.
    block: Option<f32>,
    block_frames: u32,
    voiced: u32,
}

impl Gate {
    /// One 10 ms frame; true once speech has lasted `ONSET_FRAMES` frames.
    fn frame(&mut self, probability: f32, level_db: f32, threshold: f32) -> bool {
        let block = self.block.map_or(level_db, |block| block.min(level_db));
        self.block = Some(block);
        self.block_frames += 1;
        let floor = self.blocks.iter().copied().fold(block, f32::min);
        if self.block_frames == BLOCK_FRAMES {
            if self.blocks.len() == BLOCKS {
                self.blocks.pop_front();
            }
            self.blocks.push_back(block);
            (self.block, self.block_frames) = (None, 0);
        }
        let speech = probability >= threshold
            && level_db > SILENCE_DB
            && level_db >= floor + SPEECH_ABOVE_FLOOR_DB;
        self.voiced = if speech { self.voiced + 1 } else { 0 };
        self.voiced >= ONSET_FRAMES
    }
}

struct Talker {
    decoder: OpusDecoder,
    denoise: Box<DenoiseState<'static>>,
    /// Decoded audio waiting to fill a frame.
    pending: Vec<f32>,
    gate: Gate,
}

#[derive(Default)]
pub struct Detector {
    talkers: HashMap<u32, Talker>,
}

impl Detector {
    /// Feeds one voice packet of `sender`; true when it holds speech.
    pub fn hear(&mut self, sender: u32, opus: &[u8], threshold: f32) -> bool {
        if !self.talkers.contains_key(&sender) && self.talkers.len() >= MAX_TALKERS {
            return false;
        }
        let talker = match self.talkers.entry(sender) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => match OpusDecoder::new(RATE, 1) {
                Ok(decoder) => entry.insert(Talker {
                    decoder,
                    denoise: DenoiseState::new(),
                    pending: Vec::with_capacity(MAX_PACKET_SAMPLES + FRAME),
                    gate: Gate::default(),
                }),
                Err(_) => return false,
            },
        };
        let mut decoded = [0f32; MAX_PACKET_SAMPLES];
        // A damaged packet is not speech.
        let Ok(samples) = talker
            .decoder
            .decode(opus, MAX_PACKET_SAMPLES, &mut decoded)
        else {
            return false;
        };
        talker
            .pending
            .extend_from_slice(&decoded[..samples.min(MAX_PACKET_SAMPLES)]);
        let mut speech = false;
        let mut input = [0f32; FRAME];
        let mut output = [0f32; FRAME];
        let mut used = 0;
        while talker.pending.len() - used >= FRAME {
            let frame = &talker.pending[used..used + FRAME];
            used += FRAME;
            for (scaled, sample) in input.iter_mut().zip(frame) {
                *scaled = sample * SCALE;
            }
            let probability = talker.denoise.process_frame(&mut output, &input);
            speech |= talker.gate.frame(probability, level_db(frame), threshold);
        }
        talker.pending.drain(..used);
        speech
    }

    /// The talker left; their state goes.
    pub fn forget(&mut self, sender: u32) {
        self.talkers.remove(&sender);
    }

    /// Nobody needs ducking here now.
    pub fn clear(&mut self) {
        self.talkers.clear();
    }
}

fn level_db(frame: &[f32]) -> f32 {
    let power = frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32;
    10. * power.max(1e-12).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, OpusEncoder};

    /// Encodes mono 48 kHz audio into 20 ms Opus packets.
    fn packets(audio: &[f32]) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new(RATE, 1, Application::Voip).unwrap();
        audio
            .as_chunks::<960>()
            .0
            .iter()
            .map(|frame| {
                let mut packet = [0u8; 1275];
                let length = encoder.encode(frame, 960, &mut packet).unwrap();
                packet[..length].to_vec()
            })
            .collect()
    }

    fn speech_in(detector: &mut Detector, packets: &[Vec<u8>]) -> usize {
        packets
            .iter()
            .filter(|packet| detector.hear(7, packet, threshold(50)))
            .count()
    }

    fn noise(samples: usize, amplitude: f32, mut state: u32) -> Vec<f32> {
        (0..samples)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32 - 0.5) * amplitude
            })
            .collect()
    }

    #[test]
    fn open_microphone_silence_noise_and_hum_are_not_speech() {
        let mut detector = Detector::default();
        let mut only = |audio: &[f32]| {
            detector.forget(7);
            speech_in(&mut detector, &packets(audio))
        };
        assert_eq!(only(&vec![0.0; 48_000]), 0);
        // Loud and quiet hiss (RNNoise alone rates quiet hiss as speech), and mains hum.
        assert_eq!(only(&noise(96_000, 0.1, 0x2545_f491)), 0);
        assert_eq!(only(&noise(192_000, 0.02, 0x1234_5678)), 0);
        let hum: Vec<f32> = (0..96_000)
            .map(|n| {
                let t = n as f32 / RATE as f32;
                (1..6)
                    .map(|k| (std::f32::consts::TAU * 100. * k as f32 * t).sin() / k as f32)
                    .sum::<f32>()
                    * 0.05
            })
            .collect();
        assert_eq!(only(&hum), 0);
    }

    #[test]
    fn noise_after_silence_is_learnt_within_the_window() {
        let mut detector = Detector::default();
        speech_in(&mut detector, &packets(&vec![0.0; 48_000]));
        let noisy = packets(&noise(192_000, 0.1, 0x2545_f491));
        // The first 1.5 s may still look like a rise; afterwards it is background.
        let (learning, learnt) = noisy.split_at(80);
        speech_in(&mut detector, learning);
        assert_eq!(speech_in(&mut detector, learnt), 0);
    }

    #[test]
    fn speech_needs_rnnoise_a_rise_above_the_background_and_some_duration() {
        let mut gate = Gate::default();
        // A steady background that RNNoise wrongly calls speech.
        assert!((0..300).all(|_| !gate.frame(0.95, -40., 0.6)));
        // Speech 15 dB above it: after three frames.
        assert!(!gate.frame(0.95, -25., 0.6));
        assert!(!gate.frame(0.95, -25., 0.6));
        assert!(gate.frame(0.95, -25., 0.6));
        // Loud but not speech-like.
        assert!(!gate.frame(0.2, -25., 0.6));
        // Back to the background: the floor followed it down again.
        assert!(!gate.frame(0.95, -40., 0.6));
        // A background that grows louder is learnt within the window (1.5 s).
        let mut gate = Gate::default();
        gate.frame(0., -60., 0.6);
        let window = (BLOCKS as u32 + 1) * BLOCK_FRAMES;
        let speech_frames = (0..500).filter(|_| gate.frame(0.95, -30., 0.6)).count() as u32;
        assert!(
            speech_frames > 0 && speech_frames <= window,
            "{speech_frames}"
        );
        assert!(!gate.frame(0.95, -30., 0.6), "the floor did not catch up");
    }

    #[test]
    fn damaged_packets_and_too_many_talkers_are_ignored() {
        let mut detector = Detector::default();
        assert!(!detector.hear(1, &[0xff; 3], threshold(100)));
        for sender in 0..MAX_TALKERS as u32 + 10 {
            detector.hear(sender, &nuschl_core::audio::SILENT_OPUS, threshold(50));
        }
        assert_eq!(detector.talkers.len(), MAX_TALKERS);
        detector.forget(0);
        assert_eq!(detector.talkers.len(), MAX_TALKERS - 1);
    }

    #[test]
    fn sensitivity_lowers_the_threshold() {
        assert!(threshold(0) > threshold(50) && threshold(50) > threshold(100));
        assert!((threshold(50) - 0.6).abs() < 1e-6);
    }
}
