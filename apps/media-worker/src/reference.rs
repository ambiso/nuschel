// SPDX-License-Identifier: EUPL-1.2
//! The echo canceller's reference: what the output device actually plays, on
//! the same clock as the microphone.
//!
//! The reference is a recording of the output device (PipeWire sink capture,
//! PulseAudio monitor, WASAPI loopback, macOS process tap), so the echo of
//! every application is removed, not just Nuschl's voices. Where no such
//! recording exists, the output callback records what it hands to the device.
//!
//! Device callbacks stamp their samples on one clock (the worker's `Instant`
//! epoch, corrected by the latency cpal reports), and the worker feeds the
//! canceller each reference frame once it has sounded, up to the microphone
//! frame being processed. The canceller then always sees the far end before
//! its echo, at a steady offset, whatever the callbacks' burst sizes.

use crate::dsp::{FRAME, RATE};
use rtrb::{Consumer, Producer, RingBuffer};
use speexdsp_resampler::State;
use std::time::{Duration, Instant};

/// Stamps per stream (one per device callback) kept for the worker.
const ANCHORS: usize = 256;
/// The reference may run this far behind before the microphone stops waiting.
pub const MAX_HOLD_FRAMES: usize = 4;
/// A reference frame is fed if it starts less than this after the microphone
/// frame: everything that sounded before the microphone frame ended (one
/// frame, 10 ms) has then reached the canceller ahead of its echo, with 15 ms
/// to spare for the devices' approximate latency reports.
pub const LEAD: Duration = Duration::from_millis(25);
/// Reference audio older than this is no use to the canceller (a stalled
/// stream resuming); it is dropped rather than fed in a burst.
const STALE: Duration = Duration::from_millis(300);

/// Nanoseconds since the worker's epoch.
#[derive(Clone, Copy)]
pub struct Clock {
    epoch: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    /// Now, moved `earlier` (by capture latency) or `later` (by playback
    /// latency). Callable from device callbacks: no allocation.
    pub fn now_shifted(&self, earlier: Duration, later: Duration) -> u64 {
        let elapsed = self.epoch.elapsed() + later;
        elapsed.saturating_sub(earlier).as_nanos() as u64
    }
}

/// The producer half of a stamped stream: samples and (frame, time) anchors.
pub struct Stamper {
    anchors: Producer<(u64, u64)>,
    frames: u64,
}

impl Stamper {
    /// Mark that the next frame pushed sounds (or was captured) at `nanos`.
    pub fn anchor(&mut self, nanos: u64) {
        // A full anchor ring only loses precision; the timeline extrapolates.
        let _ = self.anchors.push((self.frames, nanos));
    }

    /// Account frames that were pushed into the sample ring.
    pub fn advance(&mut self, frames: u64) {
        self.frames += frames;
    }
}

/// The consumer half: when a frame index sounded.
pub struct Timeline {
    anchors: Consumer<(u64, u64)>,
    last: Option<(u64, u64)>,
}

pub fn stamped() -> (Stamper, Timeline) {
    let (anchors, consumer) = RingBuffer::new(ANCHORS);
    (
        Stamper { anchors, frames: 0 },
        Timeline {
            anchors: consumer,
            last: None,
        },
    )
}

impl Timeline {
    /// The time of frame `index` (48 kHz frames since the stream started), from
    /// the newest stamp at or before it; `None` before the first stamp.
    pub fn time_of(&mut self, index: u64) -> Option<u64> {
        while let Ok(&(frame, nanos)) = self.anchors.peek() {
            if frame > index && self.last.is_some() {
                break;
            }
            self.last = Some((frame, nanos));
            let _ = self.anchors.pop();
        }
        let (frame, nanos) = self.last?;
        let offset = index.saturating_sub(frame) * 1_000_000_000 / u64::from(RATE);
        Some(nanos + offset)
    }
}

/// What to do with the next reference frame for a microphone frame at `mic`.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// It sounded no later than the microphone frame: give it to the canceller.
    Feed,
    /// Too old to be useful: discard it.
    Drop,
    /// It sounds later: process the microphone frame first.
    Later,
    /// It has sounded but not arrived: hold the microphone frame if allowed.
    Wait,
}

/// Decide for one reference frame sounding at `reference` (if known) and
/// whether its samples are in the ring, for a microphone frame starting at `mic`.
pub fn step(reference: Option<u64>, available: bool, mic: u64) -> Step {
    let horizon = mic + LEAD.as_nanos() as u64;
    match reference {
        None if available => Step::Feed,
        None => Step::Later,
        Some(time) if time >= horizon => Step::Later,
        Some(_) if !available => Step::Wait,
        Some(time) if Duration::from_nanos(mic.saturating_sub(time)) > STALE => Step::Drop,
        Some(_) => Step::Feed,
    }
}

/// Device-rate audio to 48 kHz with one or two channels, interleaved: two
/// keep stereo apart for the canceller (even device channels go left, odd
/// right). Runs on the device thread: no locks, allocation or blocking.
const REFERENCE_INPUT: usize = 128;
const REFERENCE_OUTPUT: usize = 6 * REFERENCE_INPUT;

struct FilteredReference {
    state: State,
    input: [[f32; REFERENCE_INPUT]; 2],
    output: [[f32; REFERENCE_OUTPUT]; 2],
}

impl FilteredReference {
    fn convert(&mut self, frames: usize, channels: usize, ring: &mut Producer<f32>) -> (u64, u64) {
        let (mut cursor, mut generated, mut buffered) = (0, 0, 0);
        while cursor < frames {
            let left =
                self.state
                    .process_float(0, &self.input[0][cursor..frames], &mut self.output[0]);
            let right = if channels == 2 {
                self.state
                    .process_float(1, &self.input[1][cursor..frames], &mut self.output[1])
            } else {
                left
            };
            let (Ok((read, written)), Ok(other)) = (left, right) else {
                break;
            };
            if (read, written) != other || read + written == 0 {
                break;
            }
            for i in 0..written {
                buffered += write_reference([self.output[0][i], self.output[1][i]], channels, ring);
            }
            generated += written as u64;
            cursor += read;
        }
        (generated, buffered)
    }
}

pub struct ReferenceConverter {
    channels: usize,
    outputs: usize,
    rate: u32,
    filtered: Option<Box<FilteredReference>>,
    filter_delay: Duration,
    accepted: u64,
    generated: u64,
}

fn write_reference(frame: [f32; 2], channels: usize, ring: &mut Producer<f32>) -> u64 {
    if ring.slots() < channels {
        return 0;
    }
    for value in frame.into_iter().take(channels) {
        let finite = if value.is_finite() {
            value.clamp(-1., 1.)
        } else {
            0.
        };
        let _ = ring.push(finite);
    }
    1
}

impl ReferenceConverter {
    pub fn new(channels: usize, rate: u32) -> Self {
        assert!((1..=32).contains(&channels) && (8_000..=384_000).contains(&rate));
        let outputs = channels.clamp(1, 2);
        let (filtered, filter_delay) = if rate == RATE {
            (None, Duration::ZERO)
        } else {
            let state = State::new(outputs, rate as usize, RATE as usize, 3)
                .expect("validated reference resampling configuration");
            let delay = Duration::from_secs_f64(state.get_input_latency() as f64 / f64::from(rate));
            (
                Some(Box::new(FilteredReference {
                    state,
                    input: [[0.; REFERENCE_INPUT]; 2],
                    output: [[0.; REFERENCE_OUTPUT]; 2],
                })),
                delay,
            )
        };
        Self {
            channels,
            outputs,
            rate,
            filtered,
            filter_delay,
            accepted: 0,
            generated: 0,
        }
    }

    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// The next converted output frame represents an EARLIER captured source
    /// time. Includes exact fractional clock phase, not rounded callback chunks.
    pub fn timestamp_delay(&self) -> Duration {
        let source = u128::from(self.accepted) * 1_000_000_000 / u128::from(self.rate);
        let output = u128::from(self.generated) * 1_000_000_000 / u128::from(RATE);
        let nanos = (self.filter_delay.as_nanos() + source).saturating_sub(output);
        Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
    }

    /// Actual captured input only: flush its final partial stage this callback.
    /// No speculative silence, waiting, callback allocation or clock rounding.
    pub fn push(&mut self, data: impl Iterator<Item = f32>, ring: &mut Producer<f32>) -> u64 {
        let mut frame = [0_f32; 32];
        let (mut filled, mut staged, mut pushed) = (0, 0, 0);
        for value in data {
            frame[filled] = if value.is_finite() { value } else { 0. };
            filled += 1;
            if filled < self.channels {
                continue;
            }
            filled = 0;
            let current = if self.outputs == 1 {
                [
                    frame[..self.channels].iter().sum::<f32>() / self.channels as f32,
                    0.,
                ]
            } else {
                let (mut left, mut right) = (0., 0.);
                for (i, sample) in frame[..self.channels].iter().enumerate() {
                    if i % 2 == 0 {
                        left += sample;
                    } else {
                        right += sample;
                    }
                }
                [
                    left / self.channels.div_ceil(2) as f32,
                    right / (self.channels / 2) as f32,
                ]
            }
            .map(|value| if value.is_finite() { value } else { 0. });
            self.accepted += 1;
            if let Some(filter) = &mut self.filtered {
                filter.input[0][staged] = current[0];
                filter.input[1][staged] = current[1];
                staged += 1;
                if staged == REFERENCE_INPUT {
                    let (generated, buffered) = filter.convert(staged, self.outputs, ring);
                    self.generated += generated;
                    pushed += buffered;
                    staged = 0;
                }
            } else {
                self.generated += 1;
                pushed += write_reference(current, self.outputs, ring);
            }
        }
        if staged > 0 {
            let (generated, buffered) =
                self.filtered
                    .as_mut()
                    .unwrap()
                    .convert(staged, self.outputs, ring);
            self.generated += generated;
            pushed += buffered;
        }
        pushed
    }
}

/// Interleaved stereo reference of what the output callback handed to the device, for
/// systems without an output recording (raw ALSA).
pub struct Played {
    pub samples: Producer<f32>,
    pub stamper: Stamper,
}

/// Capacity of a reference ring, in 48 kHz frames per channel.
pub const RING_FRAMES: usize = 40 * FRAME;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeline_maps_frames_through_the_newest_stamp() {
        let (mut stamper, mut timeline) = stamped();
        assert_eq!(timeline.time_of(0), None);
        stamper.anchor(1_000_000);
        stamper.advance(480);
        stamper.anchor(11_000_000);
        stamper.advance(480);
        assert_eq!(timeline.time_of(0), Some(1_000_000));
        assert_eq!(timeline.time_of(240), Some(6_000_000));
        // The second stamp corrects the clock (11 ms, not 10 ms).
        assert_eq!(timeline.time_of(480), Some(11_000_000));
        assert_eq!(timeline.time_of(960), Some(21_000_000));
    }

    #[test]
    fn reference_frames_are_fed_once_they_have_sounded() {
        let ms = 1_000_000;
        let lead = LEAD.as_nanos() as u64;
        assert_eq!(step(Some(10 * ms), true, 20 * ms), Step::Feed);
        // Starts before the lead runs out after the microphone frame: fed first.
        assert_eq!(step(Some(20 * ms + lead - 1), true, 20 * ms), Step::Feed);
        assert_eq!(step(Some(20 * ms + lead), true, 20 * ms), Step::Later);
        assert_eq!(step(Some(10 * ms), false, 20 * ms), Step::Wait);
        assert_eq!(step(Some(10 * ms), true, 400 * ms), Step::Drop);
        assert_eq!(step(None, true, 20 * ms), Step::Feed);
        assert_eq!(step(None, false, 20 * ms), Step::Later);
    }

    #[test]
    fn stereo_stays_apart_and_surround_folds_into_two_channels() {
        let (mut ring, mut out) = RingBuffer::new(64);
        let mut stereo = ReferenceConverter::new(2, 48_000);
        assert_eq!(stereo.outputs(), 2);
        let pushed = stereo.push([0.5_f32, -0.25, 0.5, -0.25].into_iter(), &mut ring);
        assert_eq!(pushed, 2);
        let mut values = Vec::new();
        while let Ok(value) = out.pop() {
            values.push(value);
        }
        assert_eq!(values[2..], [0.5, -0.25]);
        // 5.1: FL FR FC LFE RL RR → left = mean(FL, FC, RL), right = mean(FR, LFE, RR).
        let mut surround = ReferenceConverter::new(6, 48_000);
        let frame = [0.3_f32, 0.0, 0.3, 0.0, 0.3, 0.0];
        surround.push(frame.into_iter().chain(frame), &mut ring);
        let mut last = [0.; 2];
        while let Ok(value) = out.pop() {
            last = [last[1], value];
        }
        assert!(
            (last[0] - 0.3).abs() < 1e-6 && last[1].abs() < 1e-6,
            "{last:?}"
        );
    }

    #[test]
    fn reference_filter_delay_maps_the_impulse_to_its_original_capture_instant() {
        let base = 1_000_000_000_u64;
        for rate in [
            8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000,
        ] {
            let source_frames = rate as usize / 10 + 1;
            let mut input = vec![0_f32; source_frames * 2];
            input[..2].copy_from_slice(&[0.25, -0.125]);
            let (mut ring, mut output) = RingBuffer::new(12_000);
            let (mut stamper, mut timeline) = stamped();
            let mut converter = ReferenceConverter::new(2, rate);
            let (mut cursor, mut index) = (0, 0);
            let chunks = [1, 127, 37, 480, 256];
            while cursor < source_frames {
                let count = chunks[index % chunks.len()].min(source_frames - cursor);
                let capture_time = base + (cursor as u64 * 1_000_000_000 / u64::from(rate));
                // Capture-side compensation goes BACK, unlike playback delay.
                stamper.anchor(capture_time - converter.timestamp_delay().as_nanos() as u64);
                let produced = converter.push(
                    input[cursor * 2..(cursor + count) * 2].iter().copied(),
                    &mut ring,
                );
                stamper.advance(produced);
                cursor += count;
                index += 1;
            }
            let samples: Vec<_> = std::iter::from_fn(|| output.pop().ok()).collect();
            let peak = samples
                .chunks_exact(2)
                .enumerate()
                .max_by(|(_, a), (_, b)| a[0].abs().total_cmp(&b[0].abs()))
                .unwrap()
                .0;
            assert!(samples[peak * 2].abs() > 0.01);
            let represented = timeline.time_of(peak as u64).unwrap();
            assert!(
                represented.abs_diff(base) <= 1_000_000_000 / u64::from(RATE) + 2,
                "{rate}: reference impulse represented at {represented}, actual capture {base}"
            );
        }
    }

    #[test]
    fn non48_reference_clocks_and_irregular_callback_boundaries_stay_exact() {
        for rate in [11_025, 22_050, 44_100, 96_000] {
            let input: Vec<_> = (0..rate)
                .flat_map(|i| {
                    let value = (i as f32 * 0.03).sin() * 0.2;
                    [value, -0.5 * value]
                })
                .collect();
            let convert = |chunks: &[usize]| {
                let (mut ring, mut out) = RingBuffer::new(96_064);
                let mut converter = ReferenceConverter::new(2, rate);
                let mut cursor = 0;
                let mut count = 0;
                let mut index = 0;
                while cursor < input.len() {
                    let take = (chunks[index % chunks.len()] * 2).min(input.len() - cursor);
                    count +=
                        converter.push(input[cursor..cursor + take].iter().copied(), &mut ring);
                    cursor += take;
                    index += 1;
                }
                assert_eq!(count, 48_000, "one second remains one second at {rate}");
                let values: Vec<_> = std::iter::from_fn(|| out.pop().ok()).collect();
                assert_eq!(
                    values.len(),
                    96_000,
                    "reference timestamps count frames, not scalars"
                );
                values
            };
            assert_eq!(
                convert(&[rate as usize]),
                convert(&[1, 127, 480, 37, 256]),
                "{rate}"
            );
        }
    }

    #[test]
    fn mono_devices_give_a_mono_reference_at_48_khz() {
        let (mut ring, mut out) = RingBuffer::new(256);
        let mut converter = ReferenceConverter::new(1, 24_000);
        assert_eq!(converter.outputs(), 1);
        let pushed = converter.push(std::iter::repeat_n(0.25_f32, 50), &mut ring);
        assert_eq!(pushed, 100);
        let mut count = 0;
        while out.pop().is_ok() {
            count += 1;
        }
        assert_eq!(count, 100);
    }
}
