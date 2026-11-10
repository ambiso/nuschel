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

    pub fn process(&mut self, frame: &mut [f32; FRAME], push_to_talk: bool) -> FrameResult {
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
        let transmit = self.gate.update(voice, level_db, push_to_talk);
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

    fn update(&mut self, voice: f32, level_db: f32, push_to_talk: bool) -> bool {
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
                let threshold = if self.open {
                    self.close_threshold
                } else {
                    self.open_threshold
                };
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
            if pipeline.process(&mut frame, false).transmit && index > 100 {
                noise_open += 1;
            }
        }
        let mut speech_open = 0;
        for index in 0..300 {
            let mut frame = speech_like(index);
            if pipeline.process(&mut frame, false).transmit {
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
            let a = quiet.process(&mut a, false).level_db;
            let b = raw.process(&mut b, false).level_db;
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
            !ptt.process(&mut frame, false).transmit,
            "speech alone does not open PTT"
        );
        assert!(ptt.process(&mut frame, true).transmit);
        let held = (0..10)
            .filter(|_| ptt.process(&mut [0.; FRAME], false).transmit)
            .count();
        assert_eq!(held, 5, "release hold of 50 ms");
        let mut continuous = CapturePipeline::new(
            &AudioSettings {
                transmit: TransmitMode::Continuous,
                ..Default::default()
            },
            false,
        );
        assert!(continuous.process(&mut [0.; FRAME], false).transmit);
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
            let a = with.process(&mut a, false).level_db;
            let b = without.process(&mut b, false).level_db;
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
