// SPDX-License-Identifier: EUPL-1.2
//! Actual device samples to48k mono, without callback batching or speculative input.
use crate::reference::ReferenceConverter;
use cpal::{FromSample, Sample, SizedSample};
use rtrb::Producer;

pub(crate) struct CaptureConverter {
    channels: usize,
    filtered: ReferenceConverter,
}

impl CaptureConverter {
    pub(crate) fn new(channels: usize, rate: u32) -> Self {
        assert!((1..=32).contains(&channels) && (8000..=384000).contains(&rate));
        Self {
            channels,
            filtered: ReferenceConverter::new(1, rate),
        }
    }

    /// Converted samples describe earlier physical capture instants. Includes
    /// exact rational phase and FIR delay, not a rounded callback-size offset.
    pub(crate) fn capture_time(&self, device_capture_time: u64) -> u64 {
        device_capture_time.saturating_sub(self.filtered.timestamp_delay().as_nanos() as u64)
    }

    /// Preserve whole-channel averaging, then the existing unit input bound.
    /// Finite f32 extremes cannot overflow the f64 sum of at most32 channels;
    /// a frame containing nonfinite input remains silence. The mono engine
    /// consumes actual input immediately, including a final partial block, and
    /// advances its generated clock even if the bounded ring drops samples.
    pub(crate) fn push<T>(&mut self, data: &[T], ring: &mut Producer<f32>) -> u64
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mono = data.chunks_exact(self.channels).map(|frame| {
            let average = frame
                .iter()
                .map(|sample| f64::from(f32::from_sample(*sample)))
                .sum::<f64>()
                / self.channels as f64;
            if average.is_finite() {
                average.clamp(-1., 1.) as f32
            } else {
                0.
            }
        });
        self.filtered.push(mono, ring)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn channel_average_precedes_unit_protection_and_nonfinite_frames_are_silent() {
        let (mut ring, mut output) = RingBuffer::new(16);
        let mut capture = CaptureConverter::new(2, 48000);
        capture.push(
            &[
                1.5_f32,
                -1.25,
                f32::MAX,
                -f32::MAX,
                f32::MAX,
                f32::MAX,
                f32::NAN,
                0.5,
                f32::INFINITY,
                0.5,
            ],
            &mut ring,
        );
        let actual: Vec<_> = std::iter::from_fn(|| output.pop().ok()).collect();
        assert_eq!(actual, [0.125, 0., 1., 0., 0.]);
    }

    #[test]
    fn first_real_input_frame_is_consumed_without_whole_callback_prefill() {
        for rate in [8000, 11025, 22050, 44100, 48000, 96000, 192000, 384000] {
            let (mut ring, mut output) = RingBuffer::new(32);
            let mut capture = CaptureConverter::new(1, rate);
            let queued = capture.push(&[0.5_f32], &mut ring);
            assert_eq!(queued, 48000_u64.div_ceil(u64::from(rate)), "{rate}");
            assert_eq!(output.slots() as u64, queued);
            assert!(std::iter::from_fn(|| output.pop().ok()).all(|sample| sample.is_finite()));
            let clock = capture.capture_time(1_000_000_000);
            assert_eq!(capture.push::<f32>(&[], &mut ring), 0);
            assert_eq!(clock, capture.capture_time(1_000_000_000));
        }
    }

    #[test]
    fn callback_work_is_allocation_free_for_multichannel_capture_and_full_rings() {
        for channels in [1, 2, 6, 32] {
            for rate in [11025, 44100, 48000, 96000] {
                let source = vec![0.125_f32; channels * 1024];
                let (mut ring, _output) = RingBuffer::new(1);
                let mut capture = CaptureConverter::new(channels, rate);
                let allocations = allocation_counter::measure(|| {
                    for frame in source.chunks(channels * 37) {
                        capture.push(frame, &mut ring);
                    }
                });
                assert_eq!(allocations.count_total, 0);
            }
        }
    }

    #[test]
    fn dropped_capture_does_not_freeze_filter_clock_or_replay_it_later() {
        for rate in [11025, 44100, 96000] {
            let (mut tiny, mut tiny_out) = RingBuffer::new(1);
            let (mut roomy, _roomy_out) = RingBuffer::new(16384);
            let mut overflow = CaptureConverter::new(1, rate);
            let mut control = CaptureConverter::new(1, rate);
            for count in [1, 127, 37, 256, 480] {
                let source = vec![0.2_f32; count];
                assert!(overflow.push(&source, &mut tiny) <= 1);
                control.push(&source, &mut roomy);
                while tiny_out.pop().is_ok() {}
                assert_eq!(
                    overflow.capture_time(1_000_000_000),
                    control.capture_time(1_000_000_000)
                );
            }
            overflow.push(&[0_f32; 1024], &mut tiny);
            while tiny_out.pop().is_ok() {}
            overflow.push(&[0_f32; 128], &mut tiny);
            assert_eq!(
                tiny_out.pop().unwrap(),
                0.,
                "old dropped input must not be queued for replay"
            );
        }
    }

    #[test]
    fn channel_extremes_and_stream_reopen_do_not_poison_filtered_state() {
        for rate in [8000, 11025, 44100, 96000, 384000] {
            let (mut ring, mut output) = RingBuffer::new(32768);
            let mut capture = CaptureConverter::new(2, rate);
            capture.push(&[f32::MAX; 256], &mut ring);
            capture.push(&[f32::NAN; 256], &mut ring);
            capture.push(&[0.125_f32; 4096], &mut ring);
            let samples: Vec<_> = std::iter::from_fn(|| output.pop().ok()).collect();
            assert!(
                samples
                    .iter()
                    .all(|sample| sample.is_finite() && sample.abs() <= 1.)
            );
            assert!((samples.last().unwrap() - 0.125).abs() < 1e-4);
            drop(capture);
            let mut reopened = CaptureConverter::new(2, rate);
            reopened.push(&[0_f32; 256], &mut ring);
            assert!(std::iter::from_fn(|| output.pop().ok()).all(|sample| sample == 0.));
        }
    }
}
