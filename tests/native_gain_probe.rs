// SPDX-License-Identifier: EUPL-1.2
//! Actual native mixer on a virtual clock; no device/capture/network APIs.
#![forbid(unsafe_code)]
#[allow(dead_code)]
mod dsp {
    pub const FRAME: usize = 480;
}
#[path = "../apps/media-worker/src/jitter.rs"]
#[allow(dead_code)]
mod jitter;
#[path = "../apps/media-worker/src/mixer.rs"]
#[allow(dead_code)]
mod mixer;
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert_eq!(args.len(), 3);
    assert!(fs::metadata(&args[1]).unwrap().len() <= 4 * 1024 * 1024);
    let events = fs::read_to_string(&args[1]).unwrap();
    let mut file = fs::File::create(&args[2]).unwrap();
    let mut mixer = mixer::Mixer::default();
    let origin = Instant::now();
    let mut costs = Vec::new();
    for (index, line) in events.lines().enumerate() {
        assert!(index < 10000);
        let fields: Vec<_> = line.split_whitespace().collect();
        match fields[0] {
            "P" | "B" => {
                assert_eq!(fields.len(), 6);
                assert!(fields[5].len() <= 2048);
                let opus = (0..fields[5].len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&fields[5][at..at + 2], 16).unwrap())
                    .collect();
                mixer
                    .push(
                        mixer::Voice {
                            stream: 1,
                            sender: 1,
                            sequence: fields[1].parse().unwrap(),
                            opus,
                            end: fields[4] == "1",
                            volume: fields[3].parse().unwrap(),
                        },
                        origin + Duration::from_micros(fields[2].parse().unwrap()),
                    )
                    .unwrap();
            }
            "R" | "A" => {
                let started = Instant::now();
                let mut wide = [[0.; 2]; 480];
                let mut output = [[0.; 2]; 480];
                mixer.mix(&mut wide);
                mixer::finish(2, &wide, &[0.; 480], &mut output);
                costs.push(started.elapsed().as_nanos());
                for frame in output {
                    for sample in frame {
                        file.write_all(&sample.to_le_bytes()).unwrap();
                    }
                }
            }
            "C" => mixer.clear(),
            _ => panic!("invalid fixture command"),
        }
    }
    costs.sort_unstable();
    if !costs.is_empty() {
        println!(
            "renders={} p50_ns={} p95_ns={} max_ns={}",
            costs.len(),
            costs[costs.len() / 2],
            costs[costs.len() * 95 / 100],
            costs[costs.len() - 1]
        );
    }
}
