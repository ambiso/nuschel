// SPDX-License-Identifier: EUPL-1.2
//! Sound effects: one family of short struck tones, synthesised once when the
//! worker starts and mixed into the playback output after the voices.
//!
//! All notes come from D major pentatonic (D E F♯ A B), so overlapping sounds
//! never clash. Rising pitch means on, joined or connected; falling pitch
//! means off, left or disconnected. Each kind of event has its timbre: bells
//! for the connection, marimba for people, soft wood for your own mute and
//! deafen, glass for messages, a short tick for push-to-talk. Notes start with
//! a raised-cosine attack, every sound fades to exact silence, and no partial
//! is above 7 kHz. Each sound is scaled to a loudness rather than an
//! amplitude, so the set is matched by construction.
use crate::dsp::RATE;
use nuschl_core::audio::Sound;
use std::{
    f64::consts::{PI, TAU},
    time::{Duration, Instant},
};

/// Loudness of a regular sound at full volume in LUFS (the loudest 400 ms,
/// K-weighted): a little below speech (about -20), as loud as stock Mumble's
/// quieter cues. The desktop's default volume (70 %) is 3 dB below.
const LOUDNESS: f64 = -25.;
const MAX_PARTIAL_HZ: f64 = 7000.;
/// Pitch bends (push-to-talk) take this long.
const GLIDE: f64 = 0.03;

/// A struck tone: partials as (frequency ratio, amplitude, decay to 1/e in s).
struct Timbre {
    attack: f64,
    partials: &'static [(f64, f64, f64)],
}

/// Bells for the connection: harmonic, a faint inharmonic shimmer, long ring.
const BELL: Timbre = Timbre {
    attack: 0.008,
    partials: &[
        (1., 1., 0.5),
        (2., 0.28, 0.3),
        (3., 0.08, 0.18),
        (4.07, 0.05, 0.1),
    ],
};
/// Marimba for people: a tuned bar's 1 : 3.9 : 9.2 partials, the upper ones short.
const MARIMBA: Timbre = Timbre {
    attack: 0.005,
    partials: &[(1., 1., 0.3), (3.93, 0.18, 0.05), (9.2, 0.03, 0.012)],
};
/// Soft wood for your own state: dry and dark.
const WOOD: Timbre = Timbre {
    attack: 0.003,
    partials: &[(1., 1., 0.07), (2., 0.3, 0.045), (3., 0.08, 0.025)],
};
/// Glass for messages: a free bar's 1 : 2.76 : 5.4 modes over a soft octave.
const GLASS: Timbre = Timbre {
    attack: 0.004,
    partials: &[
        (1., 1., 0.4),
        (2., 0.1, 0.2),
        (2.76, 0.12, 0.12),
        (5.4, 0.03, 0.05),
    ],
};
/// A short tick for push-to-talk.
const TICK: Timbre = Timbre {
    attack: 0.003,
    partials: &[(1., 1., 0.04), (2., 0.12, 0.02)],
};

const F_SHARP_4: f64 = 369.99;
const A4: f64 = 440.;
const B4: f64 = 493.88;
const D5: f64 = 587.33;
const E5: f64 = 659.26;
const F5: f64 = 698.46;
const F_SHARP_5: f64 = 739.99;
const A5: f64 = 880.;
const D6: f64 = 1174.66;

struct Design {
    timbre: &'static Timbre,
    /// (onset in s, frequency in Hz, relative amplitude)
    notes: &'static [(f64, f64, f64)],
    /// Each note glides to this multiple of its frequency (1: no bend).
    bend: f64,
    /// Length in s; the last third (at most 150 ms) fades out.
    length: f64,
    /// Loudness relative to [`LOUDNESS`], in dB.
    level: f64,
}

fn design(sound: Sound) -> Design {
    let plain = |timbre, notes, length| Design {
        timbre,
        notes,
        bend: 1.,
        length,
        level: 0.,
    };
    match sound {
        // A rising major arpeggio, and a falling minor one.
        Sound::Connected => plain(
            &BELL,
            &[
                (0., D5, 0.8),
                (0.09, F_SHARP_5, 0.85),
                (0.18, A5, 0.9),
                (0.27, D6, 1.),
            ],
            0.8,
        ),
        Sound::Disconnected => plain(
            &BELL,
            &[(0., A5, 1.), (0.11, F5, 0.9), (0.22, D5, 0.85)],
            0.75,
        ),
        // A fifth up or down.
        Sound::UserJoined => plain(&MARIMBA, &[(0., A4, 0.85), (0.08, E5, 1.)], 0.4),
        Sound::UserLeft => plain(&MARIMBA, &[(0., E5, 1.), (0.08, A4, 0.85)], 0.4),
        // A fourth, quick; deafening adds a lower fourth.
        Sound::Unmuted => plain(&WOOD, &[(0., B4, 0.9), (0.055, E5, 1.)], 0.25),
        Sound::Muted => plain(&WOOD, &[(0., E5, 1.), (0.055, B4, 0.9)], 0.25),
        Sound::Undeafened => plain(
            &WOOD,
            &[(0., F_SHARP_4, 0.9), (0.05, B4, 0.9), (0.1, E5, 1.)],
            0.32,
        ),
        Sound::Deafened => plain(
            &WOOD,
            &[(0., E5, 1.), (0.05, B4, 0.9), (0.1, F_SHARP_4, 0.9)],
            0.32,
        ),
        // A fourth's glide, quieter than everything else.
        Sound::TalkOn => Design {
            timbre: &TICK,
            notes: &[(0., A5, 1.)],
            bend: D6 / A5,
            length: 0.1,
            level: -8.,
        },
        Sound::TalkOff => Design {
            timbre: &TICK,
            notes: &[(0., D6, 1.)],
            bend: A5 / D6,
            length: 0.1,
            level: -8.,
        },
        // One glass note; a mention or direct message adds a higher one.
        Sound::Message => plain(&GLASS, &[(0., A5, 1.)], 0.35),
        Sound::Mention => Design {
            level: 2.,
            ..plain(&GLASS, &[(0., A5, 0.8), (0.075, D6, 1.)], 0.4)
        },
    }
}

fn render(design: &Design) -> Vec<f32> {
    let rate = f64::from(RATE);
    let length = (design.length * rate) as usize;
    let mut out = vec![0_f64; length];
    let timbre = design.timbre;
    for &(onset, note, amplitude) in design.notes {
        let start = ((onset * rate) as usize).min(length);
        for &(ratio, partial, decay) in timbre.partials {
            let frequency = note * ratio;
            if frequency * design.bend.max(1.) > MAX_PARTIAL_HZ {
                continue;
            }
            // Per-sample factors for the decay and the glide; the oscillator
            // is a rotating phasor, so the loop needs no trigonometry.
            let damping = (-1. / (decay * rate)).exp();
            let glide = (design.bend.ln() / (GLIDE * rate)).exp();
            let mut step = TAU * frequency / rate;
            let (mut sin, mut cos) = step.sin_cos();
            let (mut re, mut im, mut level) = (1_f64, 0_f64, amplitude * partial);
            for (index, sample) in out[start..].iter_mut().enumerate() {
                let time = index as f64 / rate;
                let attack = if time < timbre.attack {
                    0.5 - 0.5 * (PI * time / timbre.attack).cos()
                } else {
                    1.
                };
                *sample += level * attack * im;
                level *= damping;
                if time < GLIDE && design.bend != 1. {
                    step *= glide;
                    (sin, cos) = step.sin_cos();
                }
                (re, im) = (re * cos - im * sin, re * sin + im * cos);
                if level < 1e-9 {
                    break;
                }
            }
        }
    }
    // A raised-cosine fade that reaches zero at the last sample.
    let fade = (length / 3).min((0.15 * rate) as usize).max(1);
    for (index, sample) in out[length - fade..].iter_mut().enumerate() {
        *sample *= 0.5 + 0.5 * (PI * (index + 1) as f64 / fade as f64).cos();
    }
    let gain = 10_f64.powf((LOUDNESS + design.level - loudness(&out)) / 20.);
    out.iter().map(|sample| (sample * gain) as f32).collect()
}

/// The loudest 400 ms in LUFS, K-weighted as in ITU-R BS.1770 (48 kHz).
fn loudness(samples: &[f64]) -> f64 {
    const SHELF: [f64; 5] = [
        1.535_124_859_586_97,
        -2.691_696_189_406_38,
        1.198_392_810_852_85,
        -1.690_659_293_182_41,
        0.732_480_774_215_85,
    ];
    const HIGH_PASS: [f64; 5] = [1., -2., 1., -1.990_047_454_833_98, 0.990_072_250_366_21];
    let mut stages = [Biquad::new(SHELF), Biquad::new(HIGH_PASS)];
    let window = (0.4 * f64::from(RATE)) as usize;
    let power: Vec<f64> = samples
        .iter()
        .copied()
        .chain(std::iter::repeat_n(0., window))
        .map(|sample| {
            let weighted = stages
                .iter_mut()
                .fold(sample, |value, stage| stage.process(value));
            weighted * weighted
        })
        .collect();
    let (mut sum, mut loudest) = (0., 0_f64);
    for (index, value) in power.iter().enumerate() {
        sum += value;
        if index >= window {
            sum -= power[index - window];
        }
        loudest = loudest.max(sum);
    }
    -0.691 + 10. * (loudest / window as f64).max(1e-20).log10()
}

/// A second-order section: b0, b1, b2, a1, a2 (a0 = 1).
struct Biquad {
    coefficients: [f64; 5],
    state: [f64; 2],
}

impl Biquad {
    fn new(coefficients: [f64; 5]) -> Self {
        Self {
            coefficients,
            state: [0.; 2],
        }
    }

    fn process(&mut self, input: f64) -> f64 {
        let [b0, b1, b2, a1, a2] = self.coefficients;
        let output = b0 * input + self.state[0];
        self.state[0] = b1 * input - a1 * output + self.state[1];
        self.state[1] = b2 * input - a2 * output;
        output
    }
}

/// Sounds playing at once; more replace the one that has played longest.
const VOICES: usize = 6;
/// A sound waits this long for the output device to open, then it is dropped.
const MAX_WAIT: Duration = Duration::from_secs(2);
/// A replaced sound fades out over 5 ms instead of stopping with a click.
const FADE: usize = 240;

#[derive(Clone, Copy)]
struct Voice {
    sound: Sound,
    position: usize,
    gain: f32,
    requested: Instant,
    /// Samples left of a fade-out.
    fading: Option<usize>,
}

/// The rendered sounds and those playing. Mixing never allocates.
pub struct Effects {
    sounds: Vec<Vec<f32>>,
    voices: [Option<Voice>; VOICES],
}

impl Effects {
    pub fn new() -> Self {
        Self {
            sounds: Sound::ALL
                .iter()
                .map(|sound| render(&design(*sound)))
                .collect(),
            voices: [None; VOICES],
        }
    }

    /// Start `sound` at `volume` (0 to 1). It replaces the same sound or its
    /// opposite: a burst of messages or a quick toggle plays once.
    pub fn play(&mut self, sound: Sound, volume: f32, now: Instant) {
        if volume.is_nan() || volume <= 0. {
            return;
        }
        for slot in &mut self.voices {
            match slot {
                Some(voice) if voice.sound != sound && voice.sound != opposite(sound) => {}
                Some(voice) if voice.position > 0 => {
                    voice.fading.get_or_insert(FADE);
                }
                _ => *slot = None,
            }
        }
        let voice = Voice {
            sound,
            position: 0,
            gain: volume.min(1.),
            requested: now,
            fading: None,
        };
        let slot = match self.voices.iter().position(Option::is_none) {
            Some(free) => free,
            None => (0..VOICES)
                .max_by_key(|index| self.voices[*index].map_or(0, |voice| voice.position))
                .unwrap_or(0),
        };
        self.voices[slot] = Some(voice);
    }

    /// Add the playing sounds to `out`.
    pub fn mix(&mut self, out: &mut [f64], now: Instant) {
        for slot in &mut self.voices {
            let Some(voice) = slot else {
                continue;
            };
            if voice.position == 0 && now.saturating_duration_since(voice.requested) > MAX_WAIT {
                *slot = None;
                continue;
            }
            let samples = &self.sounds[voice.sound as usize];
            for (out, sample) in out.iter_mut().zip(&samples[voice.position..]) {
                let fade = match voice.fading.as_mut() {
                    Some(left) => {
                        *left = left.saturating_sub(1);
                        *left as f32 / FADE as f32
                    }
                    None => 1.,
                };
                *out += f64::from(*sample) * f64::from(voice.gain) * f64::from(fade);
                voice.position += 1;
            }
            if voice.position >= samples.len() || voice.fading == Some(0) {
                *slot = None;
            }
        }
    }
}

fn opposite(sound: Sound) -> Sound {
    match sound {
        Sound::Connected => Sound::Disconnected,
        Sound::Disconnected => Sound::Connected,
        Sound::UserJoined => Sound::UserLeft,
        Sound::UserLeft => Sound::UserJoined,
        Sound::Muted => Sound::Unmuted,
        Sound::Unmuted => Sound::Muted,
        Sound::Deafened => Sound::Undeafened,
        Sound::Undeafened => Sound::Deafened,
        Sound::TalkOn => Sound::TalkOff,
        Sound::TalkOff => Sound::TalkOn,
        Sound::Message | Sound::Mention => sound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn high_frequency_db(samples: &[f32]) -> f64 {
        // Eighth-order Butterworth high-pass at 8 kHz (four RBJ sections).
        let omega = TAU * 8000. / f64::from(RATE);
        let mut stages = [0.509_795_6, 0.601_344_9, 0.899_976_2, 2.562_915_4].map(|q| {
            let alpha = omega.sin() / (2. * q);
            let a0 = 1. + alpha;
            let c = omega.cos();
            Biquad::new([
                (1. + c) / 2. / a0,
                -(1. + c) / a0,
                (1. + c) / 2. / a0,
                -2. * c / a0,
                (1. - alpha) / a0,
            ])
        });
        let (mut high, mut total) = (0., 0.);
        for sample in samples.iter().map(|sample| f64::from(*sample)) {
            let filtered = stages
                .iter_mut()
                .fold(sample, |value, stage| stage.process(value));
            high += filtered * filtered;
            total += sample * sample;
        }
        10. * (high / total).log10()
    }

    #[test]
    fn sounds_are_listed_in_discriminant_order() {
        for (index, sound) in Sound::ALL.iter().enumerate() {
            assert_eq!(*sound as usize, index);
        }
    }

    #[test]
    fn every_sound_is_short_clean_and_matched_in_loudness() {
        let effects = Effects::new();
        for (sound, samples) in Sound::ALL.iter().zip(&effects.sounds) {
            let milliseconds = samples.len() * 1000 / RATE as usize;
            assert!(
                (80..=800).contains(&milliseconds),
                "{sound:?}: {milliseconds} ms"
            );
            let peak = samples
                .iter()
                .fold(0_f32, |peak, sample| peak.max(sample.abs()));
            assert!(
                peak < 0.35,
                "{sound:?}: peak {peak} leaves less than 9 dB of headroom"
            );
            // No click: the first and last samples are (nearly) silent.
            assert!(samples[0].abs() < 1e-6 && samples[samples.len() - 1].abs() < 1e-6);
            let dc =
                samples.iter().map(|sample| f64::from(*sample)).sum::<f64>() / samples.len() as f64;
            assert!(dc.abs() < 1e-4, "{sound:?}: DC offset {dc}");
            let wide: Vec<f64> = samples.iter().map(|sample| f64::from(*sample)).collect();
            let expected = LOUDNESS + design(*sound).level;
            assert!(
                (loudness(&wide) - expected).abs() < 0.1,
                "{sound:?}: {} LUFS",
                loudness(&wide)
            );
            let high = high_frequency_db(samples);
            assert!(high < -50., "{sound:?}: {high:.0} dB above 8 kHz");
        }
    }

    #[test]
    fn rising_pitch_means_on_and_falling_off() {
        let change = |sound| {
            let design = design(sound);
            let first = design.notes[0].1;
            let last = design.notes[design.notes.len() - 1].1;
            last * design.bend / first
        };
        for sound in [
            Sound::Connected,
            Sound::UserJoined,
            Sound::Unmuted,
            Sound::Undeafened,
            Sound::TalkOn,
        ] {
            assert!(change(sound) > 1.2, "{sound:?} must rise");
            assert!(
                change(opposite(sound)) < 0.85,
                "{:?} must fall",
                opposite(sound)
            );
        }
    }

    #[test]
    fn mixing_applies_the_volume_and_never_allocates() {
        let mut effects = Effects::new();
        let now = Instant::now();
        let mut frame = [0.1_f64; crate::dsp::FRAME];
        let counted = allocation_counter::measure(|| {
            effects.play(Sound::Message, 0.5, now);
            effects.mix(&mut frame, now);
            effects.play(Sound::TalkOn, 3., now);
            for _ in 0..100 {
                effects.mix(&mut [0.; crate::dsp::FRAME], now);
            }
        });
        assert_eq!(counted.count_total, 0, "mixing allocated");
        let message = &effects.sounds[Sound::Message as usize];
        for (mixed, sample) in frame.iter().zip(message) {
            assert!((mixed - (0.1 + 0.5 * f64::from(*sample))).abs() < 1e-7);
        }
        // The volume is at most 1; nothing plays at 0 or NaN.
        effects.play(Sound::Connected, 7., now);
        assert_eq!(effects.voices.iter().flatten().last().unwrap().gain, 1.);
        let mut quiet = Effects::new();
        quiet.play(Sound::Connected, 0., now);
        quiet.play(Sound::Connected, f32::NAN, now);
        assert!(quiet.voices.iter().all(Option::is_none));
    }

    #[test]
    fn a_replaced_sound_fades_out_instead_of_clicking() {
        let mut effects = Effects::new();
        let now = Instant::now();
        effects.play(Sound::Unmuted, 1., now);
        let mut before = [0.; crate::dsp::FRAME];
        effects.mix(&mut before, now);
        // Muting again at once: the unmute sound fades while the mute sound starts.
        effects.play(Sound::Muted, 1., now);
        let mut out = vec![0.; 2 * FADE];
        effects.mix(&mut out, now);
        let unmuted = &effects.sounds[Sound::Unmuted as usize];
        let muted = &effects.sounds[Sound::Muted as usize];
        let largest_step = |samples: &[f32]| {
            samples
                .windows(2)
                .map(|pair| (pair[1] - pair[0]).abs())
                .fold(0., f32::max)
        };
        let bound = largest_step(unmuted) + largest_step(muted) + 1e-6;
        let mut previous = before[before.len() - 1];
        for sample in &out {
            assert!(
                (sample - previous).abs() <= f64::from(bound),
                "a discontinuity"
            );
            previous = *sample;
        }
        assert!(
            (out[FADE + 10] - f64::from(muted[FADE + 10])).abs() < 1e-6,
            "only the new sound is left"
        );
        assert_eq!(effects.voices.iter().flatten().count(), 1);
        // A sound that has not started yet (no output) is simply replaced.
        let mut waiting = Effects::new();
        waiting.play(Sound::Unmuted, 1., now);
        waiting.play(Sound::Muted, 1., now);
        let waiting: Vec<_> = waiting
            .voices
            .iter()
            .flatten()
            .map(|voice| voice.sound)
            .collect();
        assert_eq!(waiting, [Sound::Muted]);
    }

    #[test]
    fn a_sound_without_an_output_is_dropped() {
        let mut effects = Effects::new();
        let requested = Instant::now();
        effects.play(Sound::Connected, 1., requested);
        let mut frame = [0.; crate::dsp::FRAME];
        effects.mix(&mut frame, requested + MAX_WAIT + Duration::from_millis(1));
        assert!(frame.iter().all(|sample| *sample == 0.));
        assert!(effects.voices.iter().all(Option::is_none));
    }

    /// `NUSCHL_SOUNDS_DIR=/tmp/sounds cargo test -p nuschl-media-worker --
    /// --ignored write_sounds` writes 48 kHz 16-bit WAV files to listen to.
    #[test]
    #[ignore = "writes files"]
    fn write_sounds() {
        let directory = std::path::PathBuf::from(std::env::var("NUSCHL_SOUNDS_DIR").unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        let effects = Effects::new();
        for (sound, samples) in Sound::ALL.iter().zip(&effects.sounds) {
            let data: Vec<u8> = samples
                .iter()
                .flat_map(|sample| ((sample * 32767.).round() as i16).to_le_bytes())
                .collect();
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            for field in [16_u32, 1 | (1 << 16), RATE, RATE * 2, 2 | (16 << 16)] {
                wav.extend_from_slice(&field.to_le_bytes());
            }
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(&data);
            std::fs::write(directory.join(format!("{sound:?}.wav")), wav).unwrap();
        }
    }
}
