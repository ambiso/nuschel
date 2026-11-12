// SPDX-License-Identifier: EUPL-1.2
//! Encoder-only synthetic fixture; no device/network/session interfaces.
#![forbid(unsafe_code)]
use opus_pure::{Application, OpusEncoder, RateControl};
use std::{fs, io::Write, time::Instant};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        6,
        "profile bitrate input.f32le output.opus-frames precision"
    );
    let profile = &args[1];
    let bitrate: i32 = args[2].parse().unwrap();
    assert!((6_000..=128_000).contains(&bitrate));
    let application = if profile.ends_with("lowdelay") {
        Application::RestrictedLowDelay
    } else if profile.ends_with("audio") {
        Application::Audio
    } else if profile == "mumble" {
        if bitrate >= 64_000 {
            Application::RestrictedLowDelay
        } else if bitrate >= 32_000 {
            Application::Audio
        } else {
            Application::Voip
        }
    } else {
        assert!(profile.ends_with("voip"));
        Application::Voip
    };
    let mut encoder = OpusEncoder::new(48_000, 1, application).unwrap();
    encoder.bitrate_bps = bitrate;
    encoder.complexity = if profile.starts_with("browser-") {
        5
    } else {
        9
    };
    if profile.starts_with("native-") {
        encoder.use_inband_fec = true;
        encoder.packet_loss_perc = 5;
    }
    if profile == "mumble" {
        encoder.rate_control = RateControl::Cbr;
    }
    let raw = fs::read(&args[3]).unwrap();
    assert!(raw.len() <= 48_000 * 4 * 30 && raw.len().is_multiple_of(960 * 4));
    let input: Vec<_> = raw
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
        .collect();
    assert!(
        input
            .iter()
            .all(|value| value.is_finite() && value.abs() <= 1.)
    );
    let precision: u8 = args[5].parse().unwrap();
    assert!([16, 24].contains(&precision));
    let mut file = fs::File::create(&args[4]).unwrap();
    let mut packet = vec![0; (bitrate as usize / 400).min(960)];
    let mut costs = Vec::new();
    for frame in input.chunks_exact(960) {
        let short: Vec<_> = if precision == 16 {
            frame
                .iter()
                .map(|sample| (*sample * 32768.).clamp(-32768., 32767.) as i16)
                .collect()
        } else {
            Vec::new()
        };
        let start = Instant::now();
        let count = if precision == 16 {
            encoder.encode_s16(&short, 960, &mut packet)
        } else {
            encoder.encode(frame, 960, &mut packet)
        }
        .unwrap();
        costs.push(start.elapsed().as_nanos());
        file.write_all(&(count as u16).to_le_bytes()).unwrap();
        file.write_all(&packet[..count]).unwrap();
    }
    costs.sort_unstable();
    println!(
        "{{\"application\":\"{:?}\",\"lookahead_samples\":{},\"complexity\":{},\"frames\":{},\"encode_p50_ns\":{},\"encode_p95_ns\":{},\"encode_max_ns\":{}}}",
        encoder.application(),
        encoder.lookahead(),
        encoder.complexity,
        costs.len(),
        costs[costs.len() / 2],
        costs[costs.len() * 95 / 100],
        costs[costs.len() - 1]
    );
}
