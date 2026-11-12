// SPDX-License-Identifier: EUPL-1.2
//! Test-only access to the real converter for independent reference comparisons.
//! No device/context is constructed; explicit ignored-test invocation only.
use super::*;
use crate::dsp::RATE;

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
    let (mut reference, read) = RingBuffer::<f32>::new((input.len() + 2048) * 2);
    let mut read = Some(read);
    if std::env::var_os("NUSCHL_OUTPUT_DROP_REFERENCE").is_some() {
        drop(read.take());
    }
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
        let raw: Vec<u8> = std::iter::from_fn(|| read.as_mut().and_then(|read| read.pop().ok()))
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

#[test]
#[ignore = "explicit synthetic source for device/fallback/AEC proof"]
fn echo_source() {
    let mut probe = crate::probe::EchoProbe::new();
    let mut output = Vec::new();
    for _ in 0..800 {
        let mut source = [0_f32; FRAME];
        probe.render(&mut source);
        let effects = source.map(|sample| f64::from(sample) * 12.);
        let mut wide = [[0_f64; 2]; FRAME];
        crate::mixer::finish(2, &[[0.; 2]; FRAME], &effects, &mut wide);
        output.extend(
            wide.iter()
                .flatten()
                .flat_map(|sample| sample.to_le_bytes()),
        );
    }
    std::fs::write(std::env::var_os("NUSCHL_ECHO_SOURCE").unwrap(), output).unwrap();
}

#[test]
#[ignore = "explicit device-free fallback reference plus independent synthetic room"]
fn fallback_echo() {
    let load = |name| {
        let path = std::env::var_os(name).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() <= 16 * 1024 * 1024);
        let raw = std::fs::read(path).unwrap();
        assert_eq!(raw.len() % 4, 0);
        let samples: Vec<_> = raw
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect();
        assert!(
            samples
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.)
        );
        samples
    };
    let render = load("NUSCHL_ECHO_REFERENCE");
    let mic = load("NUSCHL_ECHO_MIC");
    assert_eq!(render.len(), mic.len() * 2);
    assert_eq!(mic.len() % FRAME, 0);
    let settings = nuschl_core::audio::AudioSettings {
        noise_suppression: false,
        automatic_gain: false,
        ..Default::default()
    };
    let mut chain = crate::dsp::CapturePipeline::with_reference(&settings, true, 2);
    chain.set_delay_hint(40);
    chain.set_far_end(true);
    let (mut before, mut after, mut sent, mut measured) = (0., 0., 0, 0);
    for (index, frame) in mic.chunks_exact(FRAME).enumerate() {
        chain.render(&render[index * 2 * FRAME..(index + 1) * 2 * FRAME]);
        let mut output: [f32; FRAME] = frame.try_into().unwrap();
        let result = chain.process(&mut output, false, true);
        if (300..600).contains(&index) {
            before += frame
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>();
            after += output
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>();
            sent += usize::from(result.transmit);
            measured += 1;
        }
    }
    assert!(before > 1e-4, "room must contain a real echo signal");
    let reduction = 10. * (before / after.max(1e-30)).log10();
    let transmitted = sent as f64 / measured as f64;
    let report = serde_json::json!({"reduction_db":reduction,"transmitted":transmitted,"measured_frames":measured,"host_devices":false,"kind":"actual fallback reference plus independent C room; direct production AEC pipeline"});
    std::fs::write(
        std::env::var_os("NUSCHL_ECHO_RESULT").unwrap(),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    assert!(reduction >= 15. && transmitted <= 0.25, "{report}");
}
