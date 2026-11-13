// SPDX-License-Identifier: EUPL-1.2
//! Explicit device-free access to the actual decoder/jitter/mixer for test-side
//! libopus comparisons. Fixed virtual arrival clock; no sockets/devices.
use super::*;

#[test]
#[ignore = "explicit independent PCM fixture; invoked by tests/output_headroom.py"]
fn dump() {
    let input = std::env::var_os("NUSCHL_MIX_INPUT").expect("input fixture");
    let output = std::env::var_os("NUSCHL_MIX_OUTPUT").expect("output PCM");
    assert!(std::fs::metadata(&input).unwrap().len() <= 4 * 1024 * 1024);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let streams = value.as_array().unwrap();
    assert!((1..=4).contains(&streams.len()));
    let streams: Vec<(Vec<Vec<u8>>, f32)> = streams
        .iter()
        .map(|stream| {
            let volume = stream["volume"].as_f64().unwrap() as f32;
            assert!(volume.is_finite() && (0. ..=4.).contains(&volume));
            let packets: Vec<Vec<u8>> = serde_json::from_value(stream["packets"].clone()).unwrap();
            assert!((3..=150).contains(&packets.len()));
            assert!(packets.iter().all(|packet| packet.len() <= MAX_OPUS_BYTES));
            (packets, volume)
        })
        .collect();
    let count = streams[0].0.len();
    assert!(streams.iter().all(|stream| stream.0.len() == count));
    let mut mixer = Mixer::default();
    let start = Instant::now();
    let mut raw = Vec::with_capacity(count * 960 * 8);
    let push = |mixer: &mut Mixer, index: usize| {
        for (sender, (packets, volume)) in streams.iter().enumerate() {
            mixer
                .push(
                    Voice {
                        stream: 1,
                        sender: sender as u32,
                        sequence: index as u64 * 2,
                        opus: packets[index].clone(),
                        end: index + 1 == count,
                        volume: *volume,
                    },
                    start + Duration::from_millis(index as u64 * 20),
                )
                .unwrap();
        }
    };
    // 60 ms initial headroom; every stream has exactly the same arrival clock.
    for index in 0..3 {
        push(&mut mixer, index);
    }
    for tick in 0..count * 2 {
        if tick % 2 == 0 && tick / 2 + 3 < count {
            push(&mut mixer, tick / 2 + 3);
        }
        let mut wide = [[0.; 2]; FRAME];
        let mut out = [[0.; 2]; FRAME];
        mixer.mix(&mut wide);
        finish(2, &wide, &[0.; FRAME], &mut out);
        // Interleaved stereo little-endian f64 before the device's final clip
        // (tests/output_resample.py `read_mix_dump`).
        raw.extend(out.iter().flatten().flat_map(|value| value.to_le_bytes()));
    }
    std::fs::write(output, raw).unwrap();
}
