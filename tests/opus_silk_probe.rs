// SPDX-License-Identifier: EUPL-1.2
//! Device-free Opus reference probe. Input: `decode|fec frames hex|-` per line.
#![forbid(unsafe_code)]
use opus_pure::{Error, OpusDecoder};
use std::{fs, io::Write};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 6);
    let rate = args[1].parse().unwrap();
    let channels: usize = args[2].parse().unwrap();
    assert!(fs::metadata(&args[3]).unwrap().len() < 4 * 1024 * 1024);
    let mut decoder = OpusDecoder::new(rate, channels).unwrap();
    decoder.set_phase_inversion_disabled(true);
    let mut out = fs::File::create(&args[4]).unwrap();
    let mut records = Vec::new();
    for (index, line) in fs::read_to_string(&args[3]).unwrap().lines().enumerate() {
        assert!(index < 1000);
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        if fields[0] == "reset" {
            decoder.reset_state().unwrap();
            assert!(decoder.phase_inversion_disabled());
            records.push(format!(
                "{{\"result\":0,\"range\":{}}}",
                decoder.final_range()
            ));
            continue;
        }
        let frames: usize = fields[1].parse().unwrap();
        assert!((1..=5760).contains(&frames));
        let hex = fields[2];
        let bytes: Vec<u8> = if hex == "-" {
            Vec::new()
        } else {
            assert!(hex.len() <= 65536 && hex.len() % 2 == 0);
            (0..hex.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
                .collect()
        };
        // Fresh output each call rules out buffer reuse as the source of errors.
        let mut pcm = vec![0.; frames * channels];
        let result = match fields[0] {
            "decode" => decoder.decode(&bytes, frames, &mut pcm),
            "fec" => decoder.decode_fec(&bytes, frames, &mut pcm),
            _ => panic!("unknown operation"),
        };
        let count = match result {
            Ok(count) => {
                assert!(count <= frames && pcm[..count * channels].iter().all(|v| v.is_finite()));
                for value in &pcm[..count * channels] {
                    out.write_all(&value.to_le_bytes()).unwrap();
                }
                count as i32
            }
            Err(Error::InvalidPacket(_)) => -4,
            Err(Error::InvalidArgument(_)) => -1,
            Err(_) => -99,
        };
        records.push(format!(
            "{{\"result\":{count},\"range\":{}}}",
            decoder.final_range()
        ));
    }
    fs::write(&args[5], format!("[{}]\n", records.join(","))).unwrap();
}
