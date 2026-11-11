// SPDX-License-Identifier: EUPL-1.2
//! The echo test: play a speech-like signal through Nuschl's own output and
//! measure what the microphone path would send while only "the far end" talks.

use crate::dsp::{FRAME, RATE};

/// Silence first, to learn the room's noise floor.
const BASELINE: u32 = 60;
/// Six seconds of signal.
const SIGNAL: u32 = 600;
/// Measure only once an echo canceller has had time to adapt, as it has in
/// a call that has been running for a while (and device latency has passed).
const SETTLE: u32 = 250;
/// Keep measuring briefly after the signal so the echo tail is included.
const TAIL: u32 = 15;
/// Echo must exceed the noise floor by this factor (3 dB) to count.
const DETECTION: f64 = 2.;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoReport {
    pub echo_db: Option<f32>,
    pub reduction_db: Option<f32>,
    pub transmitted: f32,
}

#[derive(Default)]
struct Energy {
    sum: f64,
    frames: u32,
}

impl Energy {
    fn add(&mut self, samples: &[f32]) {
        self.sum +=
            samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len() as f64;
        self.frames += 1;
    }
    fn mean(&self) -> f64 {
        if self.frames == 0 {
            0.
        } else {
            self.sum / f64::from(self.frames)
        }
    }
}

/// One syllable (or the pause after it) of the test signal.
#[derive(Default, Clone, Copy)]
struct Syllable {
    remaining: u32,
    length: u32,
    voiced: bool,
    pitch: (f64, f64),
    formants: (f64, f64),
    fricative: bool,
}

/// Vowel formants (F1, F2) in Hz, roughly /a/, /e/, /i/, /o/, /u/.
const VOWELS: [(f64, f64); 5] = [
    (730., 1090.),
    (530., 1840.),
    (270., 2290.),
    (570., 840.),
    (300., 870.),
];

#[derive(Default)]
pub struct EchoProbe {
    rendered: u32,
    captured: u32,
    phase: f64,
    noise: u32,
    syllable: Syllable,
    render: Energy,
    floor_raw: Energy,
    floor_processed: Energy,
    raw: Energy,
    processed: Energy,
    transmitted: u32,
}

fn db(ratio: f64) -> f32 {
    (10. * ratio.max(1e-12).log10()) as f32
}

impl EchoProbe {
    pub fn new() -> Self {
        Self {
            noise: 0x1234_5678,
            ..Self::default()
        }
    }

    /// Add the next frame of the test signal to `mix` (silence while measuring
    /// the floor and after the signal).
    pub fn render(&mut self, mix: &mut [f32; FRAME]) {
        let frame = self.rendered;
        self.rendered += 1;
        if !(BASELINE..BASELINE + SIGNAL).contains(&frame) {
            return;
        }
        let mut signal = [0.; FRAME];
        for out in &mut signal {
            *out = self.next_sample();
        }
        self.render.add(&signal);
        for (out, sample) in mix.iter_mut().zip(signal) {
            *out += sample;
        }
    }

    /// Uniform in [0, 1) from a fixed-seed generator: every test plays the same signal.
    fn random(&mut self) -> f64 {
        self.noise = self
            .noise
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        f64::from(self.noise >> 8) / f64::from(1_u32 << 24)
    }

    /// Speech-like and deliberately aperiodic: syllables and pauses of random
    /// length, each with its own pitch contour and vowel, some with breath
    /// noise. A regular signal would give an echo canceller's delay estimator
    /// several equally good answers.
    fn next_sample(&mut self) -> f32 {
        if self.syllable.remaining == 0 {
            let rate = f64::from(RATE);
            self.syllable = if self.syllable.voiced {
                let length = ((0.04 + 0.16 * self.random()) * rate) as u32;
                Syllable {
                    remaining: length,
                    length,
                    ..Syllable::default()
                }
            } else {
                let length = ((0.12 + 0.25 * self.random()) * rate) as u32;
                let start = 110. + 130. * self.random();
                let vowel = VOWELS[(self.random() * VOWELS.len() as f64) as usize % VOWELS.len()];
                Syllable {
                    remaining: length,
                    length,
                    voiced: true,
                    pitch: (start, start * (0.8 + 0.4 * self.random())),
                    formants: vowel,
                    fricative: self.random() < 0.4,
                }
            };
        }
        let syllable = self.syllable;
        self.syllable.remaining -= 1;
        if !syllable.voiced {
            return 0.;
        }
        let progress = 1. - f64::from(syllable.remaining) / f64::from(syllable.length.max(1));
        let envelope = (std::f64::consts::PI * progress).sin().powf(0.5);
        let pitch = syllable.pitch.0 + (syllable.pitch.1 - syllable.pitch.0) * progress;
        self.phase =
            (self.phase + std::f64::consts::TAU * pitch / f64::from(RATE)) % std::f64::consts::TAU;
        let formant = |frequency: f64, centre: f64, width: f64| {
            (-((frequency - centre) / width).powi(2)).exp()
        };
        let mut voiced = 0.;
        for harmonic in 1..=28 {
            let frequency = pitch * f64::from(harmonic);
            let weight = (0.25
                + formant(frequency, syllable.formants.0, 160.)
                + 0.7 * formant(frequency, syllable.formants.1, 260.)
                + 0.3 * formant(frequency, 2700., 450.))
                / f64::from(harmonic).sqrt();
            voiced += weight * (self.phase * f64::from(harmonic)).sin();
        }
        let breath = (self.random() - 0.5) * if syllable.fricative { 1.2 } else { 0.3 };
        // Fricatives sit at the start of a syllable.
        let breath = breath * if progress < 0.3 { 1. } else { 0.3 };
        ((voiced * 0.17 + breath) * envelope).clamp(-0.9, 0.9) as f32
    }

    /// Account one captured frame before (`raw`) and after Nuschl's
    /// processing, and whether the gate would transmit it. Returns the report
    /// once the test is over.
    pub fn observe(
        &mut self,
        raw: &[f32],
        processed: &[f32],
        transmit: bool,
    ) -> Option<EchoReport> {
        let frame = self.captured;
        self.captured += 1;
        if (10..BASELINE).contains(&frame) {
            self.floor_raw.add(raw);
            self.floor_processed.add(processed);
        } else if (BASELINE + SETTLE..BASELINE + SIGNAL + TAIL).contains(&frame) {
            self.raw.add(raw);
            self.processed.add(processed);
            self.transmitted += u32::from(transmit);
        }
        (frame + 1 >= BASELINE + SIGNAL + TAIL).then(|| self.report())
    }

    fn report(&self) -> EchoReport {
        let echo = self.raw.mean() - self.floor_raw.mean();
        let detected = self.raw.mean() > self.floor_raw.mean() * DETECTION && echo > 1e-9;
        let residual = (self.processed.mean() - self.floor_processed.mean()).max(1e-12);
        EchoReport {
            echo_db: detected.then(|| db(echo / self.render.mean().max(1e-12))),
            reduction_db: detected.then(|| db(echo / residual).clamp(0., 60.)),
            transmitted: self.transmitted as f32 / self.raw.frames.max(1) as f32,
        }
    }
}

/// Total frames of one test, for callers that need a deadline.
#[cfg(test)]
pub const FRAMES: u32 = BASELINE + SIGNAL + TAIL;

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the probe against a synthetic room: `coupling` scales the echo,
    /// `cancel` scales what remains after processing.
    fn run(coupling: f32, cancel: f32, gate: impl Fn(f32) -> bool) -> EchoReport {
        let mut probe = EchoProbe::new();
        let mut delay = std::collections::VecDeque::from(vec![0.; 3 * FRAME]);
        let mut noise = 1_u32;
        for _ in 0..FRAMES + 5 {
            let mut mix = [0.; FRAME];
            probe.render(&mut mix);
            delay.extend(mix);
            let mut raw = [0.; FRAME];
            for sample in &mut raw {
                noise = noise.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let room = (f64::from(noise >> 16) / 65_536. - 0.5) as f32 * 0.002;
                *sample = delay.pop_front().unwrap_or_default() * coupling + room;
            }
            let processed = raw.map(|sample| sample * cancel);
            let level = raw.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
            if let Some(report) = probe.observe(&raw, &processed, gate(level)) {
                return report;
            }
        }
        panic!("the probe never finished");
    }

    #[test]
    fn the_signal_sits_near_minus_twenty_dbfs() {
        let mut probe = EchoProbe::new();
        let mut energy = Energy::default();
        for _ in 0..BASELINE + SIGNAL {
            let mut mix = [0.; FRAME];
            probe.render(&mut mix);
            energy.add(&mix);
        }
        let level = db(energy.mean() * f64::from(BASELINE + SIGNAL) / f64::from(SIGNAL));
        assert!((-30. ..-14.).contains(&level), "{level} dBFS");
    }

    #[test]
    fn headphones_produce_no_echo() {
        let report = run(0., 1., |_| false);
        assert_eq!(report.echo_db, None);
        assert_eq!(report.reduction_db, None);
        assert_eq!(report.transmitted, 0.);
    }

    #[test]
    fn loud_uncancelled_echo_is_reported_and_leaks() {
        let report = run(0.5, 1., |level| level > 1e-4);
        let echo = report.echo_db.expect("echo detected");
        assert!((-7. ..-5.).contains(&echo), "{echo} dB");
        assert!(report.reduction_db.unwrap() < 1.);
        // Syllable gaps close the gate about half of the time.
        assert!(report.transmitted > 0.25, "{}", report.transmitted);
    }

    #[test]
    fn cancelled_echo_reports_the_reduction() {
        let report = run(0.5, 0.01, |_| false);
        let reduction = report.reduction_db.expect("echo detected");
        assert!(reduction > 30., "{reduction} dB");
        assert_eq!(report.transmitted, 0.);
    }
}
