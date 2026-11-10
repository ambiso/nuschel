// SPDX-License-Identifier: EUPL-1.2
//! Capture processing: echo cancellation, noise suppression, gain control and
//! the transmit gate, in 10 ms frames at 48 kHz mono.
//!
//! Order follows WebRTC's recommendation that echo cancellation sees the
//! microphone before any non-linear processing:
//! high-pass + AEC3 (Sonora) → RNNoise → AGC2 with limiter (Sonora) → gate.
use nnnoiseless::DenoiseState;
use nuschel_core::audio::{AudioSettings, TransmitMode};
use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter},
};

pub const RATE: u32 = 48_000;
pub const FRAME: usize = 480;
const SCALE: f32 = 32768.;
/// Voice-activity hangover keeps word endings and short pauses.
const VOICE_HOLD_FRAMES: u32 = 30;
/// Below this level the gate never opens, whatever the speech probability.
const SILENCE_DB: f32 = -60.;
/// Extra speech probability required while the echo guard is up.
const ECHO_GUARD_MARGIN: f32 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameResult {
    pub level_db: f32,
    pub voice: f32,
    pub transmit: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EchoMetrics {
    pub erle_db: Option<f32>,
    pub residual_likelihood: Option<f32>,
    pub delay_ms: Option<i32>,
}

pub struct CapturePipeline {
    front: AudioProcessing,
    echo_cancellation: bool,
    gain: Option<AudioProcessing>,
    denoise: Box<DenoiseState<'static>>,
    noise_suppression: bool,
    gate: Gate,
    input: [f32; FRAME],
    output: [f32; FRAME],
    /// Whether the far end is playing (see `set_far_end`).
    far_end: bool,
    /// Automatic gain applied while only the near end could be heard, and the
    /// ceiling it is held under while the far end plays.
    near_gain: f32,
    gain_ceiling: Option<f32>,
}

fn stream() -> StreamConfig {
    StreamConfig::new(RATE, 1)
}

impl CapturePipeline {
    pub fn new(settings: &AudioSettings, echo_cancellation: bool) -> Self {
        let front = AudioProcessing::builder()
            .config(Config {
                high_pass_filter: Some(HighPassFilter::default()),
                echo_canceller: echo_cancellation.then(EchoCanceller::default),
                ..Default::default()
            })
            .capture_config(stream())
            .render_config(stream())
            .echo_detector(echo_cancellation)
            .build();
        let gain = settings.automatic_gain.then(|| {
            AudioProcessing::builder()
                .config(Config {
                    gain_controller2: Some(GainController2 {
                        adaptive_digital: Some(AdaptiveDigital {
                            // Voice chat favours steady loudness over a large boost of a
                            // quiet or distant microphone.
                            max_gain_db: 30.,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .capture_config(stream())
                .build()
        });
        Self {
            front,
            echo_cancellation,
            gain,
            denoise: DenoiseState::new(),
            noise_suppression: settings.noise_suppression,
            gate: Gate::new(settings),
            input: [0.; FRAME],
            output: [0.; FRAME],
            far_end: false,
            near_gain: 1.,
            gain_ceiling: None,
        }
    }

    /// Tell the chain whether the far end is audible. While it is, automatic
    /// gain may fall but not rise: what is left of the echo after cancellation
    /// and noise suppression is speech-like, and raising the gain on it would
    /// send the far end's own voice back to them.
    pub fn set_far_end(&mut self, active: bool) {
        self.far_end = active;
        if !active {
            self.gain_ceiling = None;
        }
    }

    pub fn echo_cancellation(&self) -> bool {
        self.echo_cancellation
    }

    /// Far-end (playback) audio, exactly as handed to the output device.
    pub fn render(&mut self, frame: &[f32]) {
        if self.echo_cancellation && frame.len() == FRAME {
            let mut sink = [0.; FRAME];
            let _ = self.front.process_render_f32(&[frame], &mut [&mut sink]);
        }
    }

    /// Hint the canceller with the known device buffering, in milliseconds.
    pub fn set_delay_hint(&mut self, delay_ms: i32) {
        if self.echo_cancellation {
            let _ = self.front.set_stream_delay_ms(delay_ms.clamp(0, 500));
        }
    }

    /// Process one capture frame in place. `echo_guard` asks the voice gate
    /// for clearer speech (set by the adaptive controller while residual echo
    /// persists and only the far end is talking).
    pub fn process(
        &mut self,
        frame: &mut [f32; FRAME],
        push_to_talk: bool,
        echo_guard: bool,
    ) -> FrameResult {
        self.input.copy_from_slice(frame);
        if self
            .front
            .process_capture_f32(&[&self.input], &mut [&mut self.output])
            .is_ok()
        {
            frame.copy_from_slice(&self.output);
        }
        // RNNoise works on 16-bit scaled samples and also estimates speech.
        for (scaled, sample) in self.input.iter_mut().zip(frame.iter()) {
            *scaled = sample * SCALE;
        }
        let voice = self.denoise.process_frame(&mut self.output, &self.input);
        if self.noise_suppression {
            for (sample, clean) in frame.iter_mut().zip(self.output.iter()) {
                *sample = clean / SCALE;
            }
        }
        if let Some(gain) = self.gain.as_mut() {
            self.input.copy_from_slice(frame);
            if gain
                .process_capture_f32(&[&self.input], &mut [&mut self.output])
                .is_ok()
            {
                frame.copy_from_slice(&self.output);
                let before = rms(&self.input);
                if before > 1e-5 {
                    let applied = rms(frame) / before;
                    if self.far_end {
                        let ceiling = *self.gain_ceiling.get_or_insert(self.near_gain);
                        if applied > ceiling {
                            let scale = ceiling / applied;
                            frame.iter_mut().for_each(|sample| *sample *= scale);
                        }
                    } else {
                        // Smoothed over roughly half a second of frames.
                        self.near_gain += (applied - self.near_gain) * 0.02;
                    }
                }
            }
        } else {
            soft_limit(frame);
        }
        for sample in frame.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.;
            }
            *sample = sample.clamp(-1., 1.);
        }
        let level_db = level_db(frame);
        let transmit = self.gate.update(voice, level_db, push_to_talk, echo_guard);
        FrameResult {
            level_db,
            voice,
            transmit,
        }
    }

    pub fn echo_metrics(&self) -> EchoMetrics {
        if !self.echo_cancellation {
            return EchoMetrics::default();
        }
        let stats = self.front.statistics();
        EchoMetrics {
            erle_db: stats.echo_return_loss_enhancement.map(|value| value as f32),
            residual_likelihood: stats.residual_echo_likelihood.map(|value| value as f32),
            delay_ms: stats.delay_ms,
        }
    }
}

fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

pub fn level_db(frame: &[f32]) -> f32 {
    let energy =
        frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len().max(1) as f32;
    (10. * energy.max(1e-12).log10()).max(-100.)
}

/// Gentle limiter for when AGC2 (which includes one) is disabled.
fn soft_limit(frame: &mut [f32]) {
    const KNEE: f32 = 0.85;
    for sample in frame {
        let magnitude = sample.abs();
        if magnitude > KNEE {
            let over = (magnitude - KNEE) / (1. - KNEE);
            *sample = sample.signum() * (KNEE + (1. - KNEE) * over.tanh());
        }
    }
}

struct Gate {
    mode: TransmitMode,
    open_threshold: f32,
    close_threshold: f32,
    hangover: u32,
    release_frames: u32,
    release: u32,
    open: bool,
}

impl Gate {
    fn new(settings: &AudioSettings) -> Self {
        let sensitivity = settings.vad_sensitivity.clamp(0., 1.);
        let open_threshold = 0.9 - 0.6 * sensitivity;
        Self {
            mode: settings.transmit,
            open_threshold,
            close_threshold: (open_threshold - 0.3).max(0.05),
            hangover: 0,
            release_frames: settings.ptt_release_ms / 10,
            release: 0,
            open: false,
        }
    }

    fn update(&mut self, voice: f32, level_db: f32, push_to_talk: bool, echo_guard: bool) -> bool {
        self.open = match self.mode {
            TransmitMode::Continuous => true,
            TransmitMode::PushToTalk => {
                if push_to_talk {
                    self.release = self.release_frames;
                    true
                } else if self.release > 0 {
                    self.release -= 1;
                    true
                } else {
                    false
                }
            }
            TransmitMode::VoiceActivity => {
                let mut threshold = if self.open {
                    self.close_threshold
                } else {
                    self.open_threshold
                };
                if echo_guard {
                    threshold = (threshold + ECHO_GUARD_MARGIN).min(0.95);
                }
                if voice >= threshold && level_db > SILENCE_DB {
                    self.hangover = VOICE_HOLD_FRAMES;
                    true
                } else if self.hangover > 0 {
                    self.hangover -= 1;
                    true
                } else {
                    false
                }
            }
        };
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech_like(frame: usize) -> [f32; FRAME] {
        // A voiced, amplitude-modulated harmonic series that RNNoise treats as speech.
        std::array::from_fn(|index| {
            let t = (frame * FRAME + index) as f32 / RATE as f32;
            let envelope = 0.5 + 0.5 * (2. * std::f32::consts::PI * 4. * t).sin();
            let mut value = 0.;
            for harmonic in 1..=12 {
                value += (2. * std::f32::consts::PI * 140. * harmonic as f32 * t).sin()
                    / harmonic as f32;
            }
            0.12 * envelope * value
        })
    }

    fn noise(frame: usize, amplitude: f32) -> [f32; FRAME] {
        let mut state = 0x9e37_79b9_u32.wrapping_mul(frame as u32 + 1);
        std::array::from_fn(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            amplitude * ((state as f32 / u32::MAX as f32) * 2. - 1.)
        })
    }

    #[test]
    fn voice_activity_opens_for_speech_and_stays_closed_for_steady_noise() {
        let settings = AudioSettings::default();
        let mut pipeline = CapturePipeline::new(&settings, false);
        let mut noise_open = 0;
        for index in 0..300 {
            let mut frame = noise(index, 0.01);
            if pipeline.process(&mut frame, false, false).transmit && index > 100 {
                noise_open += 1;
            }
        }
        let mut speech_open = 0;
        for index in 0..300 {
            let mut frame = speech_like(index);
            if pipeline.process(&mut frame, false, false).transmit {
                speech_open += 1;
            }
        }
        assert!(
            noise_open < 20,
            "steady noise kept the gate open for {noise_open} frames"
        );
        assert!(
            speech_open > 150,
            "speech opened the gate for only {speech_open} frames"
        );
    }

    #[test]
    fn noise_suppression_reduces_steady_noise() {
        let mut quiet = CapturePipeline::new(
            &AudioSettings {
                automatic_gain: false,
                ..Default::default()
            },
            false,
        );
        let mut raw = CapturePipeline::new(
            &AudioSettings {
                automatic_gain: false,
                noise_suppression: false,
                ..Default::default()
            },
            false,
        );
        let (mut suppressed, mut untouched) = (0., 0.);
        for index in 0..200 {
            let mut a = noise(index, 0.05);
            let mut b = a;
            let a = quiet.process(&mut a, false, false).level_db;
            let b = raw.process(&mut b, false, false).level_db;
            if index > 50 {
                suppressed += a;
                untouched += b;
            }
        }
        assert!(
            suppressed < untouched - 6. * 150.,
            "RNNoise removed less than 6 dB of steady noise"
        );
    }

    #[test]
    fn echo_guard_demands_clearer_speech_but_not_in_push_to_talk() {
        let settings = AudioSettings::default();
        assert!(Gate::new(&settings).update(0.75, -20., false, false));
        let mut guarded = Gate::new(&settings);
        assert!(
            !guarded.update(0.75, -20., false, true),
            "residual echo opened the gate"
        );
        assert!(
            guarded.update(0.97, -20., false, true),
            "clear speech must still pass"
        );
        let mut ptt = Gate::new(&AudioSettings {
            transmit: TransmitMode::PushToTalk,
            ..AudioSettings::default()
        });
        assert!(
            ptt.update(0.0, -20., true, true),
            "push-to-talk is the user's explicit choice"
        );
    }

    #[test]
    fn push_to_talk_and_continuous_modes() {
        let mut ptt = CapturePipeline::new(
            &AudioSettings {
                transmit: TransmitMode::PushToTalk,
                ptt_release_ms: 50,
                ..Default::default()
            },
            false,
        );
        let mut frame = speech_like(0);
        assert!(
            !ptt.process(&mut frame, false, false).transmit,
            "speech alone does not open PTT"
        );
        assert!(ptt.process(&mut frame, true, false).transmit);
        let held = (0..10)
            .filter(|_| ptt.process(&mut [0.; FRAME], false, false).transmit)
            .count();
        assert_eq!(held, 5, "release hold of 50 ms");
        let mut continuous = CapturePipeline::new(
            &AudioSettings {
                transmit: TransmitMode::Continuous,
                ..Default::default()
            },
            false,
        );
        assert!(continuous.process(&mut [0.; FRAME], false, false).transmit);
    }

    #[test]
    fn echo_canceller_attenuates_a_delayed_copy_of_the_far_end() {
        let settings = AudioSettings {
            noise_suppression: false,
            automatic_gain: false,
            ..Default::default()
        };
        let mut with = CapturePipeline::new(&settings, true);
        let mut without = CapturePipeline::new(&settings, false);
        let delay = 4; // 40 ms acoustic path
        let far: Vec<[f32; FRAME]> = (0..600)
            .map(|index| speech_like(index).map(|s| s * 2.))
            .collect();
        let (mut cancelled, mut raw) = (0., 0.);
        for index in 0..far.len() {
            with.render(&far[index]);
            let echo = if index >= delay {
                far[index - delay].map(|s| s * 0.5)
            } else {
                [0.; FRAME]
            };
            let (mut a, mut b) = (echo, echo);
            let a = with.process(&mut a, false, false).level_db;
            let b = without.process(&mut b, false, false).level_db;
            if index > 400 {
                cancelled += a;
                raw += b;
            }
        }
        let frames = (far.len() - 401) as f32;
        let attenuation = (raw - cancelled) / frames;
        assert!(
            attenuation > 15.,
            "AEC3 attenuated echo by only {attenuation:.1} dB"
        );
    }
}

/// Quality and cost of the Rust DSP ports on synthetic but realistic input.
/// Measurements, not checks; run in release:
/// `cargo test --release -p nuschel-media-worker -- --ignored --nocapture evaluation`.
#[cfg(test)]
mod evaluation {
    use super::*;
    use opus_pure::{Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder};
    use std::time::Instant;

    /// A talker: harmonics of a gliding pitch with syllable-rate envelope and pauses.
    fn talker(samples: usize, pitch: f32, syllables: f32, seed: f32) -> Vec<f32> {
        let mut phase = 0.;
        (0..samples)
            .map(|index| {
                let t = index as f32 / RATE as f32;
                let f0 = pitch * (1. + 0.12 * (2. * std::f32::consts::PI * 0.7 * t + seed).sin());
                phase += f0 / RATE as f32;
                let syllable = (2. * std::f32::consts::PI * syllables * t + seed)
                    .sin()
                    .max(0.);
                let pause = if (t + seed).rem_euclid(2.3) < 0.4 {
                    0.
                } else {
                    1.
                };
                let mut value = 0.;
                for harmonic in 1..=16 {
                    let formant = 1. / (1. + ((harmonic as f32 * f0 - 700.) / 500.).powi(2));
                    value += (2. * std::f32::consts::PI * phase * harmonic as f32).sin()
                        * (0.3 / harmonic as f32 + formant);
                }
                0.08 * syllable * pause * value
            })
            .collect()
    }

    /// A small room: 40 ms to the first reflection, then 64 sparse taps
    /// decaying over 250 ms (roughly a 0.4 s reverberation time).
    fn room(signal: &[f32]) -> Vec<f32> {
        let mut state = 0x2545_f491_u32;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as f32 / u32::MAX as f32
        };
        let first = RATE as usize * 40 / 1000;
        let span = RATE as usize / 4;
        let mut taps = vec![(first, 0.5)];
        for _ in 0..64 {
            let offset = (random() * span as f32) as usize;
            let decay = (-6.9 * offset as f32 / (0.4 * RATE as f32)).exp();
            taps.push((first + offset, 0.25 * decay * (random() * 2. - 1.)));
        }
        let mut out = vec![0.; signal.len()];
        for (delay, gain) in taps {
            for (index, sample) in signal.iter().enumerate() {
                if let Some(slot) = out.get_mut(index + delay) {
                    *slot += sample * gain;
                }
            }
        }
        out
    }

    fn frame_of(signal: &[f32], index: usize) -> [f32; FRAME] {
        std::array::from_fn(|offset| signal[index * FRAME + offset])
    }

    fn energy_db(frames: &[[f32; FRAME]]) -> f32 {
        let energy: f32 = frames.iter().flatten().map(|sample| sample * sample).sum();
        10. * (energy / (frames.len() * FRAME) as f32).max(1e-12).log10()
    }

    #[test]
    #[ignore = "measurement, not a check"]
    fn evaluation() {
        let seconds = 12;
        let total = seconds * RATE as usize;
        let far = talker(total, 120., 3.1, 0.0);
        let near = talker(total, 210., 3.7, 1.3);
        let echo = room(&far);
        let frames = total / FRAME;
        // 0–6 s far end only, 6–9 s both talk, 9–12 s near end only.
        let near_active = |index: usize| index >= 600;
        let far_active = |index: usize| index < 900;
        let settings = AudioSettings {
            noise_suppression: false,
            automatic_gain: false,
            transmit: TransmitMode::Continuous,
            ..Default::default()
        };
        let mut aec = CapturePipeline::new(&settings, true);
        let mut reference = CapturePipeline::new(&settings, true);
        aec.set_delay_hint(10);
        let (mut mic_far_only, mut out_far_only) = (Vec::new(), Vec::new());
        let (mut out_double, mut near_alone) = (Vec::new(), Vec::new());
        let mut likelihood = Vec::new();
        for index in 0..frames {
            let far_frame = if far_active(index) {
                frame_of(&far, index)
            } else {
                [0.; FRAME]
            };
            let echo_frame = if far_active(index) || index < 925 {
                frame_of(&echo, index)
            } else {
                [0.; FRAME]
            };
            let near_frame = if near_active(index) {
                frame_of(&near, index)
            } else {
                [0.; FRAME]
            };
            aec.render(&far_frame);
            let mut mic: [f32; FRAME] = std::array::from_fn(|i| echo_frame[i] + near_frame[i]);
            let raw = mic;
            aec.process(&mut mic, false, false);
            let mut alone = near_frame;
            reference.process(&mut alone, false, false);
            if (300..600).contains(&index) {
                mic_far_only.push(raw);
                out_far_only.push(mic);
            }
            if (650..900).contains(&index) {
                out_double.push(mic);
                near_alone.push(alone);
            }
            if index % 100 == 99 {
                likelihood.push(aec.echo_metrics().residual_likelihood.unwrap_or(f32::NAN));
            }
        }
        let erle = energy_db(&mic_far_only) - energy_db(&out_far_only);
        let near_loss = energy_db(&near_alone) - energy_db(&out_double);
        println!("AEC3 (Sonora), reverberant room, 40 ms path:");
        println!("  echo reduction while only the far end talks (3–6 s): {erle:.1} dB");
        println!(
            "  near-end level change during double talk: {:+.1} dB",
            -near_loss
        );
        println!("  reported ERLE: {:?}", aec.echo_metrics().erle_db);
        println!("  residual-echo likelihood per second: {likelihood:.2?}");

        // CPU cost of the full capture chain and the codec.
        let full = AudioSettings::default();
        let mut chain = CapturePipeline::new(&full, true);
        let started = Instant::now();
        for index in 0..frames {
            chain.render(&frame_of(&far, index));
            let mut mic: [f32; FRAME] =
                std::array::from_fn(|i| echo[index * FRAME + i] + near[index * FRAME + i]);
            std::hint::black_box(chain.process(&mut mic, false, false));
        }
        let chain_rtf = started.elapsed().as_secs_f64() / seconds as f64;
        let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip).unwrap();
        encoder.bitrate_bps = 40_000;
        encoder.use_inband_fec = true;
        encoder.packet_loss_perc = 5;
        let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
        let mut packet = vec![0; MAX_PACKET_BYTES];
        let mut decoded = vec![0.; MAX_PACKET_SAMPLES];
        let (mut encode, mut decode, mut bytes) = (0., 0., 0);
        for chunk in near.as_chunks::<{ 2 * FRAME }>().0 {
            let started = Instant::now();
            let size = encoder.encode(chunk, 2 * FRAME, &mut packet).unwrap();
            encode += started.elapsed().as_secs_f64();
            bytes += size;
            let started = Instant::now();
            decoder
                .decode(&packet[..size], MAX_PACKET_SAMPLES, &mut decoded)
                .unwrap();
            decode += started.elapsed().as_secs_f64();
        }
        println!("CPU, one core (fraction of real time):");
        println!(
            "  capture chain (HPF + AEC3 + RNNoise + AGC2): {:.2}%",
            chain_rtf * 100.
        );
        println!(
            "  opus-pure encode at 40 kb/s with FEC: {:.2}%, decode: {:.3}% per talker ({:.1} kb/s actual)",
            encode / seconds as f64 * 100.,
            decode / seconds as f64 * 100.,
            bytes as f64 * 8. / seconds as f64 / 1000.
        );
    }
}
