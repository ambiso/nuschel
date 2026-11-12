// SPDX-License-Identifier: EUPL-1.2
//! Allocation-free device-rate playback. Initialized before opening the stream.
//! Source positions are exact Speex rational-rate state, never rounded 10 ms blocks.
use crate::dsp::RATE;
use cpal::{FromSample, SizedSample};
use rtrb::{Consumer, Producer};
use speexdsp_resampler::State;
use std::time::Duration;

const INPUT: usize = 128;
const OUTPUT: usize = 256;

struct Filtered {
    state: State,
    input: [[f32; INPUT]; 2],
    at: usize,
    ready: usize,
    output: [[f32; OUTPUT]; 2],
}

pub(crate) struct PlaybackConverter {
    channels: usize,
    filtered: Option<Box<Filtered>>,
    delay: Duration,
}

impl PlaybackConverter {
    pub(crate) fn new(channels: usize, rate: u32) -> Self {
        assert!(channels > 0 && (8_000..=384_000).contains(&rate));
        if rate == RATE {
            return Self {
                channels,
                filtered: None,
                delay: Duration::ZERO,
            };
        }
        let state = State::new(2, RATE as usize, rate as usize, 3)
            .expect("validated device resampling configuration");
        let delay = Duration::from_secs_f64(state.get_input_latency() as f64 / f64::from(RATE));
        Self {
            channels,
            delay,
            filtered: Some(Box::new(Filtered {
                state,
                input: [[0.; INPUT]; 2],
                at: 0,
                ready: 0,
                output: [[0.; OUTPUT]; 2],
            })),
        }
    }

    /// The source reference reaches the device after this digital-filter delay.
    pub(crate) fn delay(&self) -> Duration {
        self.delay
    }

    /// No locks, allocation or future output prefetch. Returns underrun and the
    /// source FRAMES successfully handed to the interleaved LR reference ring.
    pub(crate) fn pull<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        ring: &mut Consumer<[f32; 2]>,
        played: &mut Producer<f32>,
    ) -> (bool, u64) {
        let mut dry = false;
        let mut handed = 0;
        let Some(filter) = &mut self.filtered else {
            for output in data.chunks_exact_mut(self.channels) {
                let frame = ring.pop().unwrap_or_else(|_| {
                    dry = true;
                    [0.; 2]
                });
                write_device(output, frame);
                handed += reference(played, self.channels, frame);
            }
            return (dry, handed);
        };
        let mut cursor = 0;
        let frames = data.len() / self.channels;
        while cursor < frames {
            if filter.at == filter.ready {
                // A small fixed lookahead, independent of callback size. Silence
                // preserves the device clock during an underrun; never wait for IO.
                for i in 0..INPUT {
                    let frame = ring.pop().unwrap_or_else(|_| {
                        dry = true;
                        [0.; 2]
                    });
                    filter.input[0][i] = frame[0];
                    filter.input[1][i] = frame[1];
                }
                filter.at = 0;
                filter.ready = INPUT;
            }
            let count = (frames - cursor).min(OUTPUT);
            let left = filter.state.process_float(
                0,
                &filter.input[0][filter.at..filter.ready],
                &mut filter.output[0][..count],
            );
            let right = filter.state.process_float(
                1,
                &filter.input[1][filter.at..filter.ready],
                &mut filter.output[1][..count],
            );
            let (Ok((read, written)), Ok(other)) = (left, right) else {
                data[cursor * self.channels..].fill(T::from_sample(0.));
                return (true, handed);
            };
            if (read, written) != other || read + written == 0 {
                data[cursor * self.channels..].fill(T::from_sample(0.));
                return (true, handed);
            }
            for index in filter.at..filter.at + read {
                handed += reference(
                    played,
                    self.channels,
                    [filter.input[0][index], filter.input[1][index]],
                );
            }
            filter.at += read;
            for index in 0..written {
                write_device(
                    &mut data
                        [(cursor + index) * self.channels..(cursor + index + 1) * self.channels],
                    [filter.output[0][index], filter.output[1][index]],
                );
            }
            cursor += written;
        }
        (dry, handed)
    }
}

fn write_device<T: SizedSample + FromSample<f32>>(output: &mut [T], frame: [f32; 2]) {
    // Portable CPAL has no speaker mask: mono deliberately folds LR; >=2 maps
    // front-left/right, leaving additional channels silent rather than duplicating.
    output.fill(T::from_sample(0.));
    if output.len() == 1 {
        output[0] = T::from_sample(((frame[0] + frame[1]) * 0.5).clamp(-1., 1.));
    } else {
        output[0] = T::from_sample(frame[0].clamp(-1., 1.));
        output[1] = T::from_sample(frame[1].clamp(-1., 1.));
    }
}

fn reference(played: &mut Producer<f32>, channels: usize, frame: [f32; 2]) -> u64 {
    let heard = if channels == 1 {
        [(frame[0] + frame[1]) * 0.5; 2]
    } else {
        frame
    };
    u64::from(played.push_entire_slice(&heard).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

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
                .chunks_exact(2)
                .enumerate()
                .max_by(|(_, a), (_, b)| a[0].abs().total_cmp(&b[0].abs()))
                .unwrap()
                .0;
            assert!(
                (peak as f64 - delay).abs() <= 0.6,
                "{rate}: peak{peak}, declared{delay}"
            );
            assert_eq!(
                [reference.pop().unwrap(), reference.pop().unwrap()],
                [0.25, -0.125]
            );
            assert_eq!(
                reference.slots() as u64 + 2,
                handed * 2,
                "timestamps count source frames"
            );
        }
    }

    #[test]
    fn filtered_underruns_and_recovery_never_allocate_or_replay_old_samples() {
        for rate in [8_000, 11_025, 22_050, 44_100, 96_000, 192_000, 384_000] {
            let (mut queued, mut input) = RingBuffer::<[f32; 2]>::new(8_000);
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
            for frame in output[output.len() - 128..].chunks_exact(2) {
                assert!(
                    (frame[0] - 0.25).abs() < 1e-4 && (frame[1] + 0.125).abs() < 1e-4,
                    "{rate}: {frame:?}"
                );
            }
        }
    }
}
