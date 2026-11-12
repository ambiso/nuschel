// SPDX-License-Identifier: EUPL-1.2
//! Standalone test-only probe of the integrity-verified scalar Rust Speex port.
use speexdsp_resampler::State;
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
    let mut resampler = State::new(2, 48000, rate as usize, 3).unwrap();
    let mut inputs = [
        vec![0.; pcm.len() / 2 + 1024],
        vec![0.; pcm.len() / 2 + 1024],
    ];
    for (i, frame) in pcm.chunks_exact(2).enumerate() {
        inputs[0][i] = frame[0];
        inputs[1][i] = frame[1];
    }
    let mut consumed = [0; 2];
    let allocation = allocation_counter::measure(|| {
        for channel in 0..2 {
            if rate == 48000 {
                outputs[channel].copy_from_slice(&inputs[channel][..frames]);
                consumed[channel] = frames;
                continue;
            }
            let mut at = 0;
            let mut cursor = 0;
            while at < frames {
                let count = chunks[cursor % chunks.len()].min(frames - at);
                let (read, written) = resampler
                    .process_float(
                        channel,
                        &inputs[channel][consumed[channel]..],
                        &mut outputs[channel][at..at + count],
                    )
                    .unwrap();
                assert!(read > 0 || written > 0);
                consumed[channel] += read;
                at += written;
                cursor += 1;
            }
        }
    });
    assert_eq!(allocation.count_total, 0, "f32 process allocated");
    assert_eq!(consumed[0], consumed[1], "LR must share the source clock");
    let raw: Vec<_> = outputs[0]
        .iter()
        .zip(&outputs[1])
        .flat_map(|(l, r)| [*l, *r])
        .flat_map(|v| v.to_le_bytes())
        .collect();
    std::fs::write(&arguments[2], raw).unwrap();
}
