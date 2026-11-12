// SPDX-License-Identifier: EUPL-1.2
//! Test-only access to the real converter for independent reference comparisons.
//! No device/context is constructed; explicit ignored-test invocation only.
use super::*;

#[test]
#[ignore = "explicit disk-backed PCM fixture; invoked by tests/output_resample.py"]
fn dump() {
    let input_path = std::env::var_os("NUSCHL_OUTPUT_INPUT").expect("input fixture path");
    let output_path = std::env::var_os("NUSCHL_OUTPUT_OUTPUT").expect("output fixture path");
    let rate: u32 = std::env::var("NUSCHL_OUTPUT_RATE")
        .unwrap()
        .parse()
        .unwrap();
    assert!((8_000..=192_000).contains(&rate));
    assert!(std::fs::metadata(&input_path).unwrap().len() <= 8 * 1024 * 1024);
    let bytes = std::fs::read(input_path).unwrap();
    let wide = std::env::var("NUSCHL_OUTPUT_FORMAT").as_deref() == Ok("f64le");
    let width = if wide { 16 } else { 8 };
    assert_eq!(bytes.len() % width, 0);
    let input: Vec<[f64; 2]> = bytes
        .chunks_exact(width)
        .map(|frame| {
            if wide {
                [
                    f64::from_le_bytes(frame[..8].try_into().unwrap()),
                    f64::from_le_bytes(frame[8..].try_into().unwrap()),
                ]
            } else {
                [
                    f64::from(f32::from_le_bytes(frame[..4].try_into().unwrap())),
                    f64::from(f32::from_le_bytes(frame[4..].try_into().unwrap())),
                ]
            }
        })
        .collect();
    assert!(input.iter().flatten().all(|v| v.is_finite()));
    let output_frames = input.len() as u64 * u64::from(rate) / u64::from(RATE);
    let chunks: Vec<usize> = std::env::var("NUSCHL_OUTPUT_CHUNKS")
        .unwrap_or("127,256,37,960".into())
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        !chunks.is_empty() && chunks.len() <= 32 && chunks.iter().all(|n| (1..=8192).contains(n))
    );
    let (mut queued, mut ring) = RingBuffer::<[f64; 2]>::new(input.len() + 1024);
    queued.push_entire_slice(&input).unwrap();
    queued.push_entire_slice(&[[0.; 2]; 1024]).unwrap();
    let (mut reference, mut read) = RingBuffer::<f32>::new((input.len() + 2048) * 2);
    let mut converter = PlaybackConverter::new(2, rate);
    let mut output = Vec::with_capacity(output_frames as usize * 2);
    let mut cursor = 0;
    let mut stamps = Vec::new();
    while output.len() < output_frames as usize * 2 {
        let count = chunks[cursor % chunks.len()].min(output_frames as usize - output.len() / 2);
        let mut chunk = vec![0_f32; count * 2];
        let delay = converter.reference_delay().as_nanos();
        let played_ns = 1_000_000_000 + output.len() as u64 / 2 * 1_000_000_000 / u64::from(rate);
        let reference_time = converter.reference_time(played_ns);
        let start = std::time::Instant::now();
        let (_, generated) = converter.pull(&mut chunk, &mut ring, &mut reference);
        let elapsed = start.elapsed().as_nanos();
        stamps.push(serde_json::json!({"device_start":output.len()/2, "device_frames":count, "reference_delay_ns":delay, "reference_time_ns":reference_time, "pull_ns":elapsed, "reference_buffered_frames":generated}));
        output.extend(chunk);
        cursor += 1;
    }
    let raw: Vec<u8> = output
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    std::fs::write(output_path, raw).unwrap();
    if let Some(path) = std::env::var_os("NUSCHL_OUTPUT_REFERENCE") {
        let raw: Vec<u8> = std::iter::from_fn(|| read.pop().ok())
            .flat_map(f32::to_le_bytes)
            .collect();
        std::fs::write(&path, raw).unwrap();
        std::fs::write(
            std::path::Path::new(&path).with_extension("json"),
            serde_json::to_vec(&stamps).unwrap(),
        )
        .unwrap();
    }
}
