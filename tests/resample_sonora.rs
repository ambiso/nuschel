// SPDX-License-Identifier: EUPL-1.2
//! Standalone test-side probe of the locked Sonora SRC, not production code.
use sonora_common_audio::sinc_resampler::{SincResampler, SincResamplerCallback};
struct Input<'a> {
    samples: &'a [f32],
    at: usize,
}
impl SincResamplerCallback for Input<'_> {
    fn run(&mut self, count: usize, output: &mut [f32]) {
        for value in &mut output[..count] {
            *value = self.samples.get(self.at).copied().unwrap_or(0.);
            self.at += 1;
        }
    }
}
fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    assert_eq!(arguments.len(), 5, "INPUT OUTPUT RATE CHUNKS");
    assert!(std::fs::metadata(&arguments[1]).unwrap().len() <= 8 * 1024 * 1024);
    let bytes = std::fs::read(&arguments[1]).unwrap();
    assert_eq!(bytes.len() % 8, 0);
    let pcm: Vec<_> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    assert!(pcm.iter().all(|v| v.is_finite()));
    let rate: u32 = arguments[3].parse().unwrap();
    assert!((8_000..=192_000).contains(&rate));
    let chunks: Vec<usize> = arguments[4]
        .split(',')
        .map(|c| c.parse().unwrap())
        .collect();
    assert!(
        !chunks.is_empty() && chunks.len() <= 32 && chunks.iter().all(|c| (1..=8192).contains(c))
    );
    let frames = pcm.len() / 2 * rate as usize / 48000;
    let mut outputs = [vec![0_f32; frames], vec![0_f32; frames]];
    let mut consumed = [0; 2];
    for channel in 0..2 {
        let input: Vec<_> = pcm.iter().skip(channel).step_by(2).copied().collect();
        let mut source = Input {
            samples: &input,
            at: 0,
        };
        let mut resampler = SincResampler::new(48000. / f64::from(rate), 128);
        let mut at = 0;
        let mut cursor = 0;
        while at < frames {
            let count = chunks[cursor % chunks.len()].min(frames - at);
            resampler.resample(count, &mut outputs[channel][at..at + count], &mut source);
            cursor += 1;
            at += count;
        }
        consumed[channel] = source.at;
    }
    assert_eq!(consumed[0], consumed[1], "LR must share the source clock");
    let raw: Vec<_> = outputs[0]
        .iter()
        .zip(&outputs[1])
        .flat_map(|(l, r)| [*l, *r])
        .flat_map(|v| v.to_le_bytes())
        .collect();
    std::fs::write(&arguments[2], raw).unwrap();
}
