// SPDX-License-Identifier: EUPL-1.2
//! The echo test: play a speech-like signal through Nuschl's own output and
//! measure what the microphone path would send while only "the far end" talks.
//!
//! In 10 ms frames: silence while the room's noise floor is measured; the
//! signal fades in, plays at full level and fades out; a short tail. Echo is
//! measured once a canceller has settled, over full-level signal and the tail
//! only, so the fades neither dilute the measurement nor shorten it.

use crate::dsp::{FRAME, RATE};
use std::ops::Range;

/// Silence first, to learn the room's noise floor.
const BASELINE: u32 = 60;
/// Half a second from silence to full level: the sound never starts abruptly.
const FADE_IN: u32 = 50;
/// Full level lasts until six seconds after the signal began...
const FADE_OUT_START: u32 = 600;
/// ...and then the signal returns to silence over half a second.
const FADE_OUT: u32 = 50;
/// The whole signal, fades included.
const SIGNAL: u32 = FADE_OUT_START + FADE_OUT;
/// Measure only once an echo canceller has had time to adapt, as it has in
/// a call that has been running for a while (and device latency has passed).
const SETTLE: u32 = 250;
/// Keep measuring briefly after full level so the echo tail is included, and
/// let the fade-out leave the playback buffer (at most 120 ms) before the end.
const TAIL: u32 = 15;
/// Microphone frames that measure the noise floor (the device has started).
const FLOOR: Range<u32> = 10..BASELINE;
/// Microphone frames that measure the echo, and the played frames they are
/// compared with: full-level signal and its tail, never a fade-in.
const MEASURED: Range<u32> = BASELINE + SETTLE..BASELINE + FADE_OUT_START + TAIL;
/// Frames of one test on each side, playback and microphone: the report
/// comes only once both have passed the faded signal and its tail. Settings
/// says "about 7 seconds", and gives up on a test that has not reported 12 s
/// after the click (`TEST_DEADLINE` in apps/desktop/src/preview.rs), worker
/// start and device opening included: see the tests on both below.
const FRAMES: u32 = BASELINE + SIGNAL + TAIL;
const FRAME_SAMPLES: u32 = FRAME as u32;
const _: () = assert!(FADE_IN <= SETTLE, "no fade-in is measured");
const _: () = assert!(SETTLE < FADE_OUT_START, "full-level signal is measured");
const _: () = assert!(TAIL <= FADE_OUT, "every measured frame is signal");
/// Peaks are rounded off smoothly above `KNEE` and stay below `CEILING`
/// (-6 dBFS): no hard clipping, and headroom for voices playing meanwhile.
const KNEE: f64 = 0.4;
const CEILING: f64 = 0.5;
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

/// Unchanged up to `KNEE`, then a smooth curve (slope 1 at the knee) that
/// approaches `CEILING` and never exceeds it.
fn limit(sample: f64) -> f64 {
    let magnitude = sample.abs();
    if magnitude <= KNEE {
        return sample;
    }
    let over = (magnitude - KNEE) / (CEILING - KNEE);
    sample.signum() * (KNEE + (CEILING - KNEE) * over.tanh())
}

/// The whole test's gain at `sample`, counted from the start of the signal:
/// a raised cosine up from silence over `FADE_IN`, full level, and a raised
/// cosine back down over `FADE_OUT`. The slope is zero where each fade
/// begins and ends, so neither the start, the end nor a join is a step.
fn ramp(sample: u32) -> f64 {
    const RISE: u32 = FADE_IN * FRAME_SAMPLES;
    const FALL: u32 = FADE_OUT * FRAME_SAMPLES;
    const END: u32 = SIGNAL * FRAME_SAMPLES;
    let eased = |part: u32, whole: u32| {
        (std::f64::consts::FRAC_PI_2 * f64::from(part) / f64::from(whole))
            .sin()
            .powi(2)
    };
    if sample < RISE {
        eased(sample, RISE)
    } else if sample >= END - FALL {
        eased(END.saturating_sub(sample), FALL)
    } else {
        1.
    }
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
        self.rendered = frame.saturating_add(1);
        let Some(offset) = frame
            .checked_sub(BASELINE)
            .filter(|offset| *offset < SIGNAL)
        else {
            return;
        };
        let mut signal = [0.; FRAME];
        for (sample, out) in (offset * FRAME_SAMPLES..).zip(&mut signal) {
            *out = (limit(self.next_sample()) * ramp(sample)) as f32;
        }
        if MEASURED.contains(&frame) {
            self.render.add(&signal);
        }
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
    /// several equally good answers. Full scale is limited and faded by `render`.
    fn next_sample(&mut self) -> f64 {
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
        (voiced * 0.17 + breath) * envelope
    }

    /// Account one captured frame before (`raw`) and after Nuschl's
    /// processing, and whether the gate would transmit it. Returns the report
    /// once the test is over: the microphone has heard the tail, and the
    /// faded signal has been played to its end and has left Nuschl's buffer.
    pub fn observe(
        &mut self,
        raw: &[f32],
        processed: &[f32],
        transmit: bool,
    ) -> Option<EchoReport> {
        let frame = self.captured;
        self.captured = frame.saturating_add(1);
        if FLOOR.contains(&frame) {
            self.floor_raw.add(raw);
            self.floor_processed.add(processed);
        } else if MEASURED.contains(&frame) {
            self.raw.add(raw);
            self.processed.add(processed);
            self.transmitted += u32::from(transmit);
        }
        (self.captured >= FRAMES && self.rendered >= FRAMES).then(|| self.report())
    }

    /// The echo relative to what was played over the same frames, how much
    /// processing removed, and how often the gate would have sent it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Every sample one test plays, rendered frame by frame as the worker
    /// does, and some silence after it.
    fn played() -> Vec<f32> {
        let mut probe = EchoProbe::new();
        let mut out = Vec::new();
        for _ in 0..FRAMES + 10 {
            let mut mix = [0.; FRAME];
            probe.render(&mut mix);
            out.extend(mix);
        }
        out
    }

    /// The samples of `frames`, frames counted from the start of the test.
    fn span(frames: Range<u32>) -> Range<usize> {
        frames.start as usize * FRAME..frames.end as usize * FRAME
    }

    fn rms_db(samples: &[f32]) -> f32 {
        db(
            samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>()
                / samples.len().max(1) as f64,
        )
    }

    fn peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0., |peak, sample| sample.abs().max(peak))
    }

    /// Run the probe against a synthetic room: the echo arrives `delay`
    /// frames late (device latency) scaled by `coupling`, `cancel` scales
    /// what remains after processing, and the room adds noise at about
    /// -65 dBFS. Returns the report and the frames it took.
    fn run(
        coupling: f32,
        delay: usize,
        cancel: f32,
        gate: impl Fn(f32) -> bool,
    ) -> (EchoReport, u32) {
        let mut probe = EchoProbe::new();
        let mut path = VecDeque::from(vec![0.; delay * FRAME]);
        let mut noise = 1_u32;
        for frame in 1..=FRAMES + 5 {
            let mut mix = [0.; FRAME];
            probe.render(&mut mix);
            path.extend(mix);
            let mut raw = [0.; FRAME];
            for sample in &mut raw {
                noise = noise.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let room = (f64::from(noise >> 16) / 65_536. - 0.5) as f32 * 0.002;
                *sample = path.pop_front().unwrap_or_default() * coupling + room;
            }
            let processed = raw.map(|sample| sample * cancel);
            let level = raw.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
            if let Some(report) = probe.observe(&raw, &processed, gate(level)) {
                return (report, frame);
            }
        }
        panic!("the probe never finished");
    }

    #[test]
    fn the_sound_spans_exactly_the_timeline() {
        let played = played();
        let first = played.iter().position(|sample| *sample != 0.);
        let last = played.iter().rposition(|sample| *sample != 0.);
        // Silence for the floor, then sound from the signal's first frame...
        assert_eq!(first.map(|sample| sample / FRAME), Some(BASELINE as usize));
        // ...whose last frame, in the fade-out, is the last that sounds.
        assert_eq!(
            last.map(|sample| sample / FRAME),
            Some((BASELINE + SIGNAL) as usize - 1)
        );
        // A restarted test is a new probe: the same signal from silence again.
        assert_eq!(played, self::played());
        // The report ends the test after the tail, FRAMES in.
        let (_, frames) = run(0., 0, 1., |_| false);
        assert_eq!(frames, FRAMES);
    }

    fn seconds(frames: u32) -> f64 {
        f64::from(frames * FRAME_SAMPLES) / f64::from(RATE)
    }

    /// Settings (`audio_check` in apps/desktop/src/settings.rs) says the test
    /// plays its sound "for about 7 seconds": the whole test, until the
    /// verdict, lasts 6.5 to 7.5 s, and what sounds of it barely less.
    #[test]
    fn settings_says_about_seven_seconds() {
        let (_, frames) = run(0., 0, 1., |_| false);
        let test = seconds(frames);
        assert!((6.5..7.5).contains(&test), "test: {test} s");
        let played = played();
        let first = played.iter().position(|sample| *sample != 0.);
        let last = played.iter().rposition(|sample| *sample != 0.);
        let sound = (last.unwrap() + 1 - first.unwrap()) as f64 / f64::from(RATE);
        assert!((6. ..=test).contains(&sound), "sound: {sound} s");
    }

    /// Settings' fixed deadline (`TEST_DEADLINE`, 12 s, in
    /// apps/desktop/src/preview.rs) runs from the click and so also covers
    /// starting the worker and opening the devices. From starting an
    /// unsandboxed worker to its report, against a private PulseAudio, this
    /// took 7.3 to 8.7 s (2026-11-13).
    #[test]
    fn the_test_leaves_room_to_start_within_settings_deadline() {
        const DEADLINE: f64 = 12.;
        // Starting the sandboxed worker and opening the devices, generously.
        const START: f64 = 3.;
        let test = seconds(FRAMES);
        assert!(test + START <= DEADLINE, "{test} s + {START} s");
    }

    #[test]
    fn the_report_waits_until_the_signal_has_been_played() {
        let mut probe = EchoProbe::new();
        let silence = [0.; FRAME];
        // The microphone runs ahead (playback stalled): no verdict, and so
        // the fade-out is never cut short by the test ending early.
        for _ in 0..2 * FRAMES {
            assert_eq!(probe.observe(&silence, &silence, false), None);
        }
        for _ in 0..FRAMES - 1 {
            probe.render(&mut [0.; FRAME]);
            assert_eq!(probe.observe(&silence, &silence, false), None);
        }
        probe.render(&mut [0.; FRAME]);
        assert!(probe.observe(&silence, &silence, false).is_some());
    }

    #[test]
    fn the_test_fades_in_and_out_without_a_step() {
        let played = played();
        let signal = &played[span(BASELINE..BASELINE + SIGNAL)];
        // Never louder than the faded ceiling allows: no sudden full level.
        for (sample, value) in (0..).zip(signal) {
            let bound = CEILING * ramp(sample) + 1e-7;
            assert!(f64::from(value.abs()) <= bound, "sample {sample}: {value}");
        }
        assert_eq!(signal[0], 0.);
        assert!(signal[signal.len() - 1].abs() < 1e-6);
        let tenth = FRAME * 10;
        assert!(peak(&signal[..tenth]) < 0.05, "first 100 ms");
        assert!(peak(&signal[signal.len() - tenth..]) < 0.05, "last 100 ms");
        // The gain rises and falls monotonically, is exactly full in between,
        // and never jumps: its largest step, joins and both ends included,
        // is that of the raised cosine itself.
        let end = SIGNAL * FRAME_SAMPLES;
        let gains: Vec<f64> = (0..=end).map(ramp).collect();
        assert_eq!((gains[0], gains[gains.len() - 1]), (0., 0.));
        let rise = (FADE_IN * FRAME_SAMPLES) as usize;
        let fall = (FADE_OUT_START * FRAME_SAMPLES) as usize;
        assert!(gains[..=rise].windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(gains[rise..fall].iter().all(|gain| *gain == 1.));
        assert!(gains[fall..].windows(2).all(|pair| pair[0] >= pair[1]));
        let steepest =
            std::f64::consts::FRAC_PI_2 / f64::from(FADE_IN.min(FADE_OUT) * FRAME_SAMPLES);
        let largest = gains
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0., f64::max);
        assert!(largest <= steepest * 1.001, "{largest} > {steepest}");
    }

    #[test]
    fn syllables_begin_and_end_without_a_click() {
        let played = played();
        let level = rms_db(&played[span(BASELINE + FADE_IN..BASELINE + FADE_OUT_START)]);
        // Each edge between sound and silence (every syllable's start and end,
        // and the test's) is at least 20 dB below the speech level.
        let edge = 10_f32.powf((level - 20.) / 20.);
        let mut edges = 0;
        for pair in played.windows(2) {
            if (pair[0] == 0.) != (pair[1] == 0.) {
                edges += 1;
                assert!(peak(pair) <= edge, "{pair:?} against {edge}");
            }
        }
        assert!(edges > 30, "{edges} edges");
    }

    #[test]
    fn the_level_is_representative_and_bounded() {
        let played = played();
        assert!(f64::from(peak(&played)) <= CEILING);
        let full = rms_db(&played[span(BASELINE + FADE_IN..BASELINE + FADE_OUT_START)]);
        assert!((-21. ..-15.).contains(&full), "{full} dBFS");
        // The limiter: identity up to the knee, smooth there, odd, monotonic,
        // and bounded for any input.
        let inputs: Vec<f64> = (0..=4_000).map(|step| f64::from(step) * 0.001).collect();
        for pair in inputs.windows(2) {
            assert!(limit(pair[0]) <= limit(pair[1]));
        }
        for &input in inputs.iter().chain(&[10., 1e6, f64::MAX, f64::INFINITY]) {
            assert!(limit(input) <= CEILING && limit(-input) == -limit(input));
            if input <= KNEE {
                assert_eq!(limit(input), input);
            }
        }
        assert!((limit(KNEE + 1e-4) - (KNEE + 1e-4)).abs() < 1e-8);
    }

    #[test]
    fn the_measured_window_holds_full_level_signal_only() {
        let played = played();
        // Played frames in the window are at full level up to the fade-out,
        // whose first frames only the tail sees.
        let mut window = (MEASURED.start - BASELINE) * FRAME_SAMPLES
            ..(MEASURED.end - BASELINE - TAIL) * FRAME_SAMPLES;
        assert!(window.all(|sample| ramp(sample) == 1.));
        let full = rms_db(&played[span(BASELINE + FADE_IN..BASELINE + FADE_OUT_START)]);
        let measured = &played[span(MEASURED)];
        let level = rms_db(measured);
        assert!((level - full).abs() < 1.5, "{level} against {full} dBFS");
        // Mostly speech, with natural pauses, for the gate to react to.
        let sounding = measured
            .chunks(FRAME)
            .filter(|frame| frame.iter().any(|sample| *sample != 0.))
            .count();
        let share = sounding as f32 / (MEASURED.end - MEASURED.start) as f32;
        assert!((0.6..0.9).contains(&share), "{share}");
    }

    #[test]
    fn headphones_produce_no_echo() {
        let (report, _) = run(0., 3, 1., |_| false);
        assert_eq!(report.echo_db, None);
        assert_eq!(report.reduction_db, None);
        assert_eq!(report.transmitted, 0.);
    }

    #[test]
    fn echo_below_the_room_noise_is_not_reported() {
        // -60 dB of coupling puts the echo near -78 dBFS, under the room.
        let (report, _) = run(0.001, 3, 1., |_| false);
        assert_eq!(report.echo_db, None);
        assert_eq!(report.reduction_db, None);
    }

    #[test]
    fn realistic_coupling_is_measured() {
        // -31 dB of acoustic coupling, as in tests/media_live.py.
        let coupling = 10_f32.powf(-31. / 20.);
        let (report, _) = run(coupling, 3, 1., |_| false);
        let echo = report.echo_db.expect("echo detected");
        assert!((echo + 31.).abs() < 0.1, "{echo} dB");
    }

    #[test]
    fn loud_uncancelled_echo_is_reported_and_leaks() {
        let (report, _) = run(0.5, 3, 1., |level| level > 1e-4);
        let echo = report.echo_db.expect("echo detected");
        // 20 log10(0.5), to a tenth of a dB: the echo is compared with what
        // played over the same frames, not with the quieter fades.
        assert!((echo + 6.02).abs() < 0.08, "{echo} dB");
        assert!(report.reduction_db.unwrap() < 1.);
        // Syllable gaps close the gate some of the time.
        assert!(report.transmitted > 0.5, "{}", report.transmitted);
    }

    #[test]
    fn late_echo_is_still_measured() {
        // 150 ms of device latency: the tail still catches the echo.
        let (report, _) = run(0.5, TAIL as usize, 1., |_| false);
        let echo = report.echo_db.expect("echo detected");
        assert!((-7. ..-5.5).contains(&echo), "{echo} dB");
    }

    #[test]
    fn cancelled_echo_reports_the_reduction() {
        let (report, _) = run(0.5, 3, 0.01, |_| false);
        let reduction = report.reduction_db.expect("echo detected");
        assert!(reduction > 30., "{reduction} dB");
        assert_eq!(report.transmitted, 0.);
    }
}
