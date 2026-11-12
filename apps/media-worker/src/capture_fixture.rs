// SPDX-License-Identifier: EUPL-1.2
//! Explicit device-free fixture of the actual production CaptureConverter.
use super::*;

#[test]
#[ignore = "explicit disk-backed capture SRC baseline/reference measurement"]
fn dump() {
    let source = std::env::var_os("NUSCHL_CAPTURE_INPUT").unwrap();
    assert!(std::fs::metadata(&source).unwrap().len() <= 8 * 1024 * 1024);
    let raw = std::fs::read(source).unwrap();
    let channels: usize = std::env::var("NUSCHL_CAPTURE_CHANNELS")
        .unwrap()
        .parse()
        .unwrap();
    let rate: u32 = std::env::var("NUSCHL_CAPTURE_RATE")
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=32).contains(&channels) && (8000..=384000).contains(&rate));
    assert_eq!(raw.len() % (4 * channels), 0);
    let source: Vec<f32> = raw
        .chunks_exact(4)
        .map(|s| f32::from_le_bytes(s.try_into().unwrap()))
        .collect();
    assert!(source.iter().all(|s| s.is_finite()));
    let chunks: Vec<usize> = std::env::var("NUSCHL_CAPTURE_CHUNKS")
        .unwrap()
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        !chunks.is_empty() && chunks.len() <= 32 && chunks.iter().all(|v| (1..=8192).contains(v))
    );
    let frames = source.len() / channels;
    let (mut ring, mut output) = RingBuffer::new(frames * 48000 / rate as usize + 2048);
    let mut converter = CaptureConverter::new(channels, rate);
    let (mut cursor, mut index) = (0, 0);
    let mut metadata = Vec::new();
    while cursor < frames {
        let count = chunks[index % chunks.len()].min(frames - cursor);
        // Same capture-time helper as the actual input callback.
        let capture_time = 1_000_000_000 + cursor as u64 * 1_000_000_000 / u64::from(rate);
        let represented = converter.capture_time(capture_time);
        let begin = Instant::now();
        let generated = converter.push(
            &source[cursor * channels..(cursor + count) * channels],
            &mut ring,
        );
        metadata.push(serde_json::json!({"device_start":cursor,"device_frames":count,"queued":generated,"raw_capture_time_ns":capture_time,"capture_time_ns":represented,"callback_ns":begin.elapsed().as_nanos()}));
        cursor += count;
        index += 1;
    }
    let target = std::path::PathBuf::from(std::env::var_os("NUSCHL_CAPTURE_OUTPUT").unwrap());
    let pcm: Vec<u8> = std::iter::from_fn(|| output.pop().ok())
        .flat_map(f32::to_le_bytes)
        .collect();
    std::fs::write(&target, pcm).unwrap();
    std::fs::write(
        target.with_extension("json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
}
