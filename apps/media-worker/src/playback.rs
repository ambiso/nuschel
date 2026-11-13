// SPDX-License-Identifier: EUPL-1.2
//! Allocation-free device-rate playback. Initialized before opening the stream.
//! Source positions are exact Speex rational-rate state, never rounded 10 ms blocks.
use crate::{dsp::RATE, mixer::MAX_MIX_SAMPLE, reference::ReferenceConverter};
use cpal::{FromSample, Sample, SizedSample};
use rtrb::{Consumer, Producer};
use speexdsp_resampler::State;
use std::time::Duration;

const INPUT: usize = 128;
const OUTPUT: usize = 256;
// Constant binary scaling is linear and preserves ordinary f32 precision. The
// admitted mix is <=1024*f32::MAX. Quality3 has <=288 taps (48k->8k), |sinc|
// bounded conservatively by16, and cubic interpolation L1 bounded by16. Even
// allowing >10x rounding headroom, FIR gain<2^20: scaled accumulation remains
// below f32::MAX/4. State never sees a finite extreme large enough to poison it.
// Rescale at f64 precision immediately before the device clip, never in f32.
const FILTER_SCALE: f64 = 1. / 4_294_967_296.;
const FILTER_UNSCALE: f64 = 4_294_967_296.;

fn scaled(sample: f64) -> f32 {
    if sample.is_finite() {
        (sample.clamp(-MAX_MIX_SAMPLE, MAX_MIX_SAMPLE) * FILTER_SCALE) as f32
    } else {
        0.
    }
}

struct Filtered {
    state: State,
    input: [[f32; INPUT]; 2],
    output: [[f32; OUTPUT]; 2],
}

pub(crate) struct PlaybackConverter {
    channels: usize,
    filtered: Option<Box<Filtered>>,
    #[cfg(test)]
    delay: Duration,
    reference: ReferenceConverter,
}

impl PlaybackConverter {
    pub(crate) fn new(channels: usize, rate: u32) -> Self {
        assert!(channels > 0 && (8_000..=384_000).contains(&rate));
        if rate == RATE {
            return Self {
                channels,
                filtered: None,
                #[cfg(test)]
                delay: Duration::ZERO,
                reference: ReferenceConverter::new(2, rate),
            };
        }
        let state = State::new(2, RATE as usize, rate as usize, 3)
            .expect("validated device resampling configuration");
        #[cfg(test)]
        let delay = Duration::from_secs_f64(state.get_input_latency() as f64 / f64::from(RATE));
        Self {
            channels,
            #[cfg(test)]
            delay,
            reference: ReferenceConverter::new(2, rate),
            filtered: Some(Box::new(Filtered {
                state,
                input: [[0.; INPUT]; 2],
                output: [[0.; OUTPUT]; 2],
            })),
        }
    }

    /// The source reference reaches the device after this digital-filter delay.
    #[cfg(test)]
    pub(crate) fn delay(&self) -> Duration {
        self.delay
    }

    /// The reconstructed reference represents earlier device samples. Forward
    /// playback-filter delay is already in the waveform, so do not add it again.
    pub(crate) fn reference_delay(&self) -> Duration {
        self.reference.timestamp_delay()
    }

    pub(crate) fn reference_time(&self, playback_time: u64) -> u64 {
        playback_time.saturating_sub(self.reference_delay().as_nanos() as u64)
    }

    /// No locks, allocation or future output prefetch. Returns underrun and
    /// generated 48k reference FRAMES successfully buffered (not source frames).
    pub(crate) fn pull<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        ring: &mut Consumer<[f64; 2]>,
        played: &mut Producer<f32>,
    ) -> (bool, u64)
    where
        f32: FromSample<T>,
    {
        let dry = self.render(data, ring);
        // The consumer is dropped when a system monitor is available or own AEC
        // is unnecessary. One atomic read avoids all unused reverse-SRC work;
        // the output waveform/render state above is independent of this choice.
        if played.is_abandoned() {
            return (dry, 0);
        }
        // Read the actually quantized CPAL samples, not a pre-map/pre-clip copy.
        // Other speaker slots are silent, not averaged into the front-LR pair.
        let channels = self.channels;
        let heard = data.chunks_exact(channels).flat_map(|frame| {
            let left = f32::from_sample(frame[0]);
            let right = f32::from_sample(frame[if channels == 1 { 0 } else { 1 }]);
            [left, right]
        });
        (dry, self.reference.push(heard, played))
    }

    fn render<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        ring: &mut Consumer<[f64; 2]>,
    ) -> bool {
        let mut dry = false;
        let Some(filter) = &mut self.filtered else {
            for output in data.chunks_exact_mut(self.channels) {
                let frame = ring.pop().unwrap_or_else(|_| {
                    dry = true;
                    [0.; 2]
                });
                write_device(output, frame);
            }
            return dry;
        };
        let mut cursor = 0;
        let frames = data.len() / self.channels;
        while cursor < frames {
            // Peek without reserving future source frames. Only the portion
            // Speex consumes is committed; unconsumed real frames remain queued.
            let available = ring.slots().min(INPUT);
            let chunk = if available > 0 {
                ring.read_chunk(available).ok()
            } else {
                None
            };
            let offered = if let Some(chunk) = &chunk {
                let (first, second) = chunk.as_slices();
                for (i, frame) in first.iter().chain(second).enumerate() {
                    filter.input[0][i] = scaled(frame[0]);
                    filter.input[1][i] = scaled(frame[1]);
                }
                available
            } else {
                dry = true;
                filter.input[0].fill(0.);
                filter.input[1].fill(0.);
                INPUT
            };
            let count = (frames - cursor).min(OUTPUT);
            let left = filter.state.process_float(
                0,
                &filter.input[0][..offered],
                &mut filter.output[0][..count],
            );
            let right = filter.state.process_float(
                1,
                &filter.input[1][..offered],
                &mut filter.output[1][..count],
            );
            let (Ok((read, written)), Ok(other)) = (left, right) else {
                data[cursor * self.channels..].fill(T::from_sample(0.));
                return true;
            };
            if (read, written) != other || read + written == 0 {
                data[cursor * self.channels..].fill(T::from_sample(0.));
                return true;
            }
            if let Some(chunk) = chunk {
                chunk.commit(read);
            }
            // An empty ring offered silence for THIS call only. Its unconsumed
            // tail is discarded, so a later callback can immediately see audio.
            for index in 0..written {
                write_device(
                    &mut data
                        [(cursor + index) * self.channels..(cursor + index + 1) * self.channels],
                    [
                        f64::from(filter.output[0][index]) * FILTER_UNSCALE,
                        f64::from(filter.output[1][index]) * FILTER_UNSCALE,
                    ],
                );
            }
            cursor += written;
        }
        dry
    }
}

fn write_device<T: SizedSample + FromSample<f32>>(output: &mut [T], frame: [f64; 2]) {
    // Portable CPAL has no speaker mask: mono deliberately folds LR; >=2 maps
    // front-left/right, leaving additional channels silent rather than duplicating.
    output.fill(T::from_sample(0.));
    if output.len() == 1 {
        output[0] = T::from_sample(device_sample((frame[0] + frame[1]) * 0.5));
    } else {
        output[0] = T::from_sample(device_sample(frame[0]));
        output[1] = T::from_sample(device_sample(frame[1]));
    }
}

fn device_sample(sample: f64) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1., 1.) as f32
    } else {
        0.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn mono_folds_wide_headroom_before_clipping_and_reference_matches_the_device() {
        use crate::{dsp::FRAME, mixer};
        let wide = [[1.5_f64, -1.25]; FRAME];
        for channels in [1, 2, 6] {
            let (mut queued, mut input) = RingBuffer::<[f64; 2]>::new(FRAME);
            let (mut played, mut reference) = RingBuffer::<f32>::new(2 * FRAME);
            let mut converter = PlaybackConverter::new(channels, RATE);
            let mut ring_frame = [[0_f64; 2]; FRAME];
            let mut output = vec![0_f32; channels * FRAME];
            let count = allocation_counter::measure(|| {
                mixer::finish(channels, &wide, &[0.; FRAME], &mut ring_frame);
                queued.push_entire_slice(&ring_frame).unwrap();
                assert_eq!(
                    converter.pull(&mut output, &mut input, &mut played),
                    (false, FRAME as u64)
                );
            });
            assert_eq!(count.count_total, 0);
            for device in output.chunks_exact(channels) {
                let expected = if channels == 1 { [0.125; 2] } else { [1., -1.] };
                assert_eq!(device[0], expected[0]);
                if channels > 1 {
                    assert_eq!(device[1], expected[1]);
                }
                assert!(device.iter().skip(2).all(|sample| *sample == 0.));
                assert_eq!(reference.pop().unwrap(), expected[0]);
                assert_eq!(reference.pop().unwrap(), expected[1]);
            }
        }
        // Fold voices before adding mono effects; retain f64 until protection.
        let mut ring_frame = [[0.; 2]; FRAME];
        mixer::finish(1, &wide, &[0.5; FRAME], &mut ring_frame);
        assert!(ring_frame.iter().all(|frame| *frame == [0.625; 2]));
        let bound = f64::from(f32::MAX) * 4.;
        mixer::finish(
            1,
            &[[bound, -bound]; FRAME],
            &[0.25; FRAME],
            &mut ring_frame,
        );
        assert!(ring_frame.iter().all(|frame| *frame == [0.25; 2]));
    }

    #[test]
    fn digital_delay_matches_the_device_impulse_and_reference_keeps_stereo() {
        for rate in [
            8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000,
        ] {
            let (mut queued, mut input) = RingBuffer::new(20_000);
            queued.push([0.25, -0.125]).unwrap();
            queued.push_entire_slice(&[[0.; 2]; 19_999]).unwrap();
            let (mut played, mut reference) = RingBuffer::<f32>::new(40_000);
            let mut converter = PlaybackConverter::new(2, rate);
            let delay = converter.delay().as_secs_f64() * f64::from(rate);
            let mut output = [0_f32; 1024];
            let (dry, handed) = converter.pull(&mut output, &mut input, &mut played);
            assert!(!dry);
            let peak = output
                .as_chunks::<2>()
                .0
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a[0].abs().total_cmp(&b[0].abs()))
                .unwrap()
                .0;
            assert!(
                (peak as f64 - delay).abs() <= 0.6,
                "{rate}: peak{peak}, declared{delay}"
            );
            assert_eq!(
                reference.slots() as u64,
                handed * 2,
                "reference timestamps count reconstructed48k frames"
            );
            let samples: Vec<_> = std::iter::from_fn(|| reference.pop().ok()).collect();
            assert!(
                samples
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .any(|frame| frame[0].abs() > 0.001)
            );
            assert!(
                samples
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|frame| (frame[1] + frame[0] * 0.5).abs() < 1e-7)
            );
            assert_eq!(handed, (512_u64 * 48_000).div_ceil(u64::from(rate)));
        }
    }

    #[test]
    fn short_underrun_callbacks_never_reserve_silence_ahead_of_new_audio() {
        let (mut queued, mut input) = RingBuffer::<[f64; 2]>::new(1024);
        let (mut played, _reference) = RingBuffer::<f32>::new(4096);
        let mut converter = PlaybackConverter::new(2, 384_000);
        let mut one = [0_f32; 2];
        assert!(converter.pull(&mut one, &mut input, &mut played).0);
        queued.push([0.25, -0.125]).unwrap();
        queued.push_entire_slice(&[[0.; 2]; 511]).unwrap();
        let initial = input.slots();
        let measured = allocation_counter::measure(|| {
            for _ in 0..16 {
                converter.pull(&mut one, &mut input, &mut played);
            }
        });
        assert_eq!(measured.count_total, 0);
        assert!(
            input.slots() < initial,
            "real source must be consumed, not hidden behind speculative silence"
        );
        let mut output = [0_f32; 1024];
        converter.pull(&mut output, &mut input, &mut played);
        let peak = output
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a[0].abs().total_cmp(&b[0].abs()))
            .unwrap();
        assert!(peak.1[0].abs() > 0.1, "the real marker must reach output");
        assert!(
            peak.0 as f64 <= converter.delay().as_secs_f64() * 384_000. + 16.,
            "no extra silence before marker"
        );
    }

    #[test]
    fn filtered_underruns_and_recovery_never_allocate_or_replay_old_samples() {
        for rate in [8_000, 11_025, 22_050, 44_100, 96_000, 192_000, 384_000] {
            let (mut queued, mut input) = RingBuffer::<[f64; 2]>::new(8_000);
            let (mut played, mut reference) = RingBuffer::<f32>::new(20_000);
            let mut converter = PlaybackConverter::new(2, rate);
            let mut output = vec![1_f32; (rate / 50) as usize * 2];
            let empty = allocation_counter::measure(|| {
                assert!(converter.pull(&mut output, &mut input, &mut played).0);
            });
            assert_eq!(empty.count_total, 0);
            assert!(output.iter().all(|v| *v == 0.));
            while reference.pop().is_ok() {}
            queued.push_entire_slice(&[[0.25, -0.125]; 3840]).unwrap();
            let recovered = allocation_counter::measure(|| {
                converter.pull(&mut output, &mut input, &mut played);
            });
            assert_eq!(recovered.count_total, 0);
            for frame in output[output.len() - 128..].as_chunks::<2>().0 {
                assert!(
                    (frame[0] - 0.25).abs() < 1e-4 && (frame[1] + 0.125).abs() < 1e-4,
                    "{rate}: {frame:?}"
                );
            }
        }
    }

    #[test]
    fn fallback_uses_clipped_quantized_front_channels_not_wide_source_or_surround_mean() {
        for channels in [1, 2, 6] {
            let (mut queued, mut input) = RingBuffer::new(16);
            queued
                .push_entire_slice(&[[1.5, -0.123456789]; 16])
                .unwrap();
            let (mut played, mut reference) = RingBuffer::new(32);
            let mut converter = PlaybackConverter::new(channels, RATE);
            let mut output = vec![0_i16; channels * 8];
            let count = allocation_counter::measure(|| {
                assert_eq!(
                    converter.pull(&mut output, &mut input, &mut played),
                    (false, 8)
                );
            });
            assert_eq!(count.count_total, 0);
            for frame in output.chunks_exact(channels) {
                let expected = [
                    f32::from(frame[0]) / 32768.,
                    f32::from(frame[if channels == 1 { 0 } else { 1 }]) / 32768.,
                ];
                assert_eq!(
                    [reference.pop().unwrap(), reference.pop().unwrap()],
                    expected
                );
                assert!(frame.iter().skip(2).all(|sample| *sample == 0));
            }
        }
    }

    #[test]
    fn finite_extremes_do_not_poison_filtered_state_or_hide_later_quiet_audio() {
        assert!(MAX_MIX_SAMPLE * FILTER_SCALE * (1_u64 << 20) as f64 <= f64::from(f32::MAX) * 0.25);
        for rate in [
            8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000,
        ] {
            let (mut queued, mut input) = RingBuffer::new(6144);
            for index in 0..512 {
                let value = if index % 2 == 0 {
                    MAX_MIX_SAMPLE
                } else {
                    -MAX_MIX_SAMPLE
                };
                queued.push([value, -value]).unwrap();
            }
            // Corrupt internal values become silence before they reach FIR memory.
            queued.push([f64::NAN, f64::INFINITY]).unwrap();
            queued.push_entire_slice(&[[0.; 2]; 2047]).unwrap();
            queued.push_entire_slice(&[[0.25, -0.125]; 2048]).unwrap();
            let (mut played, mut reference) = RingBuffer::new(16384);
            let mut converter = PlaybackConverter::new(2, rate);
            let mut output = vec![0_f32; (4096_u64 * u64::from(rate) / 48_000) as usize * 2];
            let measured = allocation_counter::measure(|| {
                converter.pull(&mut output, &mut input, &mut played);
            });
            assert_eq!(measured.count_total, 0);
            assert!(
                output
                    .iter()
                    .all(|value| value.is_finite() && value.abs() <= 1.)
            );
            for frame in output[output.len() - 64..].as_chunks::<2>().0 {
                assert!(
                    (frame[0] - 0.25).abs() < 1e-4 && (frame[1] + 0.125).abs() < 1e-4,
                    "{rate}: {frame:?}"
                );
            }
            while let Ok(value) = reference.pop() {
                assert!(value.is_finite() && value.abs() <= 1.);
            }
        }
    }

    #[test]
    fn reference_overflow_does_not_freeze_its_rational_clock_or_split_stereo_frames() {
        for rate in [11_025, 44_100, 96_000] {
            let (mut queued, mut input) = RingBuffer::new(4096);
            let (mut other_queued, mut other_input) = RingBuffer::new(4096);
            queued.push_entire_slice(&[[0.2, -0.1]; 4096]).unwrap();
            other_queued
                .push_entire_slice(&[[0.2, -0.1]; 4096])
                .unwrap();
            let (mut full, mut full_out) = RingBuffer::<f32>::new(3);
            let (mut roomy, _roomy_out) = RingBuffer::<f32>::new(32768);
            let mut overflow = PlaybackConverter::new(2, rate);
            let mut ordinary = PlaybackConverter::new(2, rate);
            let mut total_device = 0_u64;
            let mut generated = 0_u64;
            for count in [1, 127, 37, 256, 480, 441] {
                let mut actual = vec![0_f32; count * 2];
                let mut control = vec![0_f32; count * 2];
                let (_, buffered) = overflow.pull(&mut actual, &mut input, &mut full);
                let (_, produced) = ordinary.pull(&mut control, &mut other_input, &mut roomy);
                assert_eq!(actual, control);
                assert!(buffered <= 1);
                assert_eq!(full_out.slots() % 2, 0, "never buffer half a stereo frame");
                while full_out.pop().is_ok() {}
                assert_eq!(overflow.reference_delay(), ordinary.reference_delay());
                total_device += count as u64;
                generated += produced;
                assert_eq!(
                    generated,
                    (total_device * 48_000).div_ceil(u64::from(rate)),
                    "no one-frame clock drift at{rate}"
                );
            }
        }
    }

    #[test]
    fn absent_fallback_consumer_never_advances_reverse_filter_or_changes_device_audio() {
        for rate in [44_100, 96_000] {
            let (mut queued, mut input) = RingBuffer::new(1024);
            let (mut other_queued, mut other_input) = RingBuffer::new(1024);
            queued.push_entire_slice(&[[2., -0.125]; 1024]).unwrap();
            other_queued
                .push_entire_slice(&[[2., -0.125]; 1024])
                .unwrap();
            let (mut abandoned, consumer) = RingBuffer::<f32>::new(1024);
            drop(consumer);
            let (mut active, _consumer) = RingBuffer::<f32>::new(1024);
            let mut idle = PlaybackConverter::new(2, rate);
            let mut control = PlaybackConverter::new(2, rate);
            let unchanged = idle.reference_delay();
            let mut output = [0_f32; 2];
            let mut expected = [0_f32; 2];
            let allocations = allocation_counter::measure(|| {
                assert_eq!(
                    idle.pull(&mut output, &mut input, &mut abandoned),
                    (false, 0)
                );
                assert!(control.pull(&mut expected, &mut other_input, &mut active).1 > 0);
            });
            assert_eq!(allocations.count_total, 0);
            assert_eq!(output, expected);
            assert_eq!(idle.reference_delay(), unchanged);
            assert_ne!(
                control.reference_delay(),
                unchanged,
                "active rational phase must advance"
            );
        }
    }

    #[test]
    fn a_reopened_playback_stream_starts_without_old_filter_tail_or_reference_phase() {
        for rate in [11_025, 44_100, 96_000] {
            let (mut source, mut input) = RingBuffer::new(4096);
            source.push_entire_slice(&[[4., -3.]; 4096]).unwrap();
            let (mut played, _reference) = RingBuffer::new(16384);
            let mut old = PlaybackConverter::new(2, rate);
            let initial_delay = old.reference_delay();
            old.pull(&mut [0_f32; 254], &mut input, &mut played);
            // Device re-open constructs a new converter; old rings/state drop.
            drop((old, source, input, played));
            let (mut source, mut input) = RingBuffer::new(4096);
            source.push_entire_slice(&[[0.; 2]; 4096]).unwrap();
            let (mut played, mut reference) = RingBuffer::new(16384);
            let mut fresh = PlaybackConverter::new(2, rate);
            assert_eq!(fresh.reference_delay(), initial_delay);
            let mut output = [1_f32; 512];
            let allocations = allocation_counter::measure(|| {
                fresh.pull(&mut output, &mut input, &mut played);
            });
            assert_eq!(allocations.count_total, 0);
            assert!(output.iter().all(|value| *value == 0.));
            while let Ok(value) = reference.pop() {
                assert_eq!(value, 0.);
            }
        }
    }
}
