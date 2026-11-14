// SPDX-License-Identifier: EUPL-1.2
//! `--bench input|capture|encode`: one fixed synthetic minute through the
//! capture chain and the Opus encoder, for instruction counts
//! (benchmarks.nix). Each stage includes the ones before it, so a stage costs
//! the difference to the previous one. Opens no device and reads no input.
use std::hint::black_box;

use nuschl_core::audio::AudioSettings;
use opus_pure::MAX_PACKET_BYTES;

use crate::dsp::{CapturePipeline, FRAME, RATE};

const SECONDS: usize = 60;
/// The far end talks alone until 30 s, both talk until 45 s, then the near end alone.
const BOTH_FROM: usize = 3000;
const NEAR_ALONE_FROM: usize = 4500;
/// Echo reduction is measured once the canceller has converged (10–30 s).
const MEASURED: std::ops::Range<usize> = 1000..BOTH_FROM;

pub fn run(stage: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !["input", "capture", "encode"].contains(&stage) {
        return Err(format!("unknown bench stage {stage:?}").into());
    }
    let total = SECONDS * RATE as usize;
    let mut far = talker(total, 120., 3.1, 0.);
    far[NEAR_ALONE_FROM * FRAME..].fill(0.);
    let mut near = talker(total, 210., 3.7, 1.3);
    near[..BOTH_FROM * FRAME].fill(0.);
    let echo = room(&far);
    if stage == "input" {
        black_box((far, near, echo));
        return Ok(());
    }
    let mut chain = CapturePipeline::with_reference(&AudioSettings::default(), true, 1);
    let mut encoder = (stage == "encode").then(crate::encoder::live).transpose()?;
    let mut pending = [0.; 2 * FRAME];
    let mut packet = [0; MAX_PACKET_BYTES];
    let (mut captured, mut sent) = (0., 0.);
    for index in 0..total / FRAME {
        let start = index * FRAME;
        chain.render(&far[start..start + FRAME]);
        chain.set_far_end(index < NEAR_ALONE_FROM);
        let mut frame: [f32; FRAME] = std::array::from_fn(|i| echo[start + i] + near[start + i]);
        let before = energy(&frame);
        black_box(chain.process(&mut frame, false, false));
        if MEASURED.contains(&index) {
            captured += before;
            sent += energy(&frame);
        }
        if let Some(encoder) = encoder.as_mut() {
            let half = index % 2 * FRAME;
            pending[half..half + FRAME].copy_from_slice(&frame);
            if half > 0 {
                black_box(encoder.encode(&pending, 2 * FRAME, &mut packet)?);
            }
        }
    }
    let reduction = 10. * (captured / sent.max(1e-12)).log10();
    println!(
        "{}",
        serde_json::json!({ "stage": stage, "seconds": SECONDS, "echo_reduction_db": reduction })
    );
    Ok(())
}

fn energy(frame: &[f32]) -> f64 {
    frame.iter().map(|&sample| f64::from(sample).powi(2)).sum()
}

/// A talker: harmonics of a gliding pitch with syllable-rate envelope and pauses.
pub fn talker(samples: usize, pitch: f32, syllables: f32, seed: f32) -> Vec<f32> {
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
pub fn room(signal: &[f32]) -> Vec<f32> {
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
