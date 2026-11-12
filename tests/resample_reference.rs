// SPDX-License-Identifier: EUPL-1.2
//! Test-side probe includes the actual unchanged recorded-reference converter.
mod dsp {
    pub const FRAME: usize = 480;
    pub const RATE: u32 = 48000;
}
#[path = "../apps/media-worker/src/reference.rs"]
mod reference;
use rtrb::RingBuffer;
fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    assert_eq!(arguments.len(), 5, "INPUT OUTPUT SOURCE_RATE CHUNKS");
    assert!(std::fs::metadata(&arguments[1]).unwrap().len() <= 8 * 1024 * 1024);
    let bytes = std::fs::read(&arguments[1]).unwrap();
    assert_eq!(bytes.len() % 8, 0);
    let input: Vec<_> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let rate: u32 = arguments[3].parse().unwrap();
    assert!((8000..=384000).contains(&rate));
    let chunks: Vec<usize> = arguments[4]
        .split(',')
        .map(|c| c.parse().unwrap())
        .collect();
    assert!(
        !chunks.is_empty() && chunks.len() <= 32 && chunks.iter().all(|c| (1..=8192).contains(c))
    );
    let frames = input.len() / 2 * 48000 / rate as usize;
    let (mut ring, mut consumer) = RingBuffer::new((frames + 1) * 2);
    let mut converter = reference::ReferenceConverter::new(2, rate);
    let (mut cursor, mut chunk, mut count) = (0, 0, 0);
    while cursor < input.len() {
        let take = (chunks[chunk % chunks.len()] * 2).min(input.len() - cursor);
        count += converter.push(input[cursor..cursor + take].iter().copied(), &mut ring);
        cursor += take;
        chunk += 1;
    }
    assert_eq!(count, frames as u64);
    let mut raw = Vec::with_capacity(frames * 8);
    while let Ok(sample) = consumer.pop() {
        raw.extend(sample.to_le_bytes());
    }
    assert_eq!(raw.len(), frames * 8);
    std::fs::write(&arguments[2], raw).unwrap();
}
