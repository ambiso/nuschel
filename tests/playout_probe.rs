// SPDX-License-Identifier: EUPL-1.2
//! Device-free actual library engine probe for independent reference fixtures.
#![forbid(unsafe_code)]
use nuschl_audio_wasm::VoicePlayout;
use std::{fs, io::Write};
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert_eq!(args.len(), 3);
    assert!(fs::metadata(&args[1]).unwrap().len() <= 4 * 1024 * 1024);
    let source = fs::read_to_string(&args[1]).unwrap();
    let mut output = fs::File::create(&args[2]).unwrap();
    let mut receiver = VoicePlayout::new().unwrap();
    for (index, line) in source.lines().enumerate() {
        assert!(index < 10_000);
        let fields: Vec<_> = line.split_whitespace().collect();
        match fields[0] {
            "P" | "B" => {
                assert_eq!(fields.len(), 6);
                assert!(fields[5].len() <= 2048);
                let bytes: Vec<_> = (0..fields[5].len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&fields[5][at..at + 2], 16).unwrap())
                    .collect();
                let result = receiver.push(
                    fields[1].parse().unwrap(),
                    &bytes,
                    fields[4] == "1",
                    fields[3].parse().unwrap(),
                    fields[2].parse().unwrap(),
                );
                if fields[0] == "B" {
                    assert!(result.is_err());
                } else {
                    result.unwrap();
                }
            }
            "R" | "E" => {
                let result = receiver.render();
                let samples = if fields[0] == "E" {
                    assert!(result.is_err());
                    Vec::new()
                } else {
                    result.unwrap()
                };
                assert!(samples.is_empty() || samples.len() == 960);
                println!(
                    "{} {} {} {}",
                    fields[1],
                    samples.len(),
                    receiver.last_render_kind(),
                    receiver.gain()
                );
                let zeros = [0_f32; 960];
                let samples = if samples.is_empty() {
                    &zeros[..]
                } else {
                    &samples
                };
                for sample in samples {
                    output.write_all(&sample.to_le_bytes()).unwrap();
                }
            }
            "C" => receiver.reset().unwrap(),
            _ => panic!("invalid fixture event"),
        }
    }
}
