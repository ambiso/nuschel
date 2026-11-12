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
    let wide_output = args.get(3).is_some_and(|arg| arg == "--wide");
    assert!(args.len() == 3 || (args.len() == 4 && wide_output));
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
                        if wide_output {
                            file.write_all(&sample.to_le_bytes()).unwrap();
                        } else {
                            // Legacy gain fixture observes a48k float device's
                            // final output, not the newly wide pre-SRC ring.
                            file.write_all(&(sample.clamp(-1., 1.) as f32).to_le_bytes())
                                .unwrap();
                        }
                    }
                }
            }
            "C" => mixer.clear(),
            _ => panic!("invalid fixture command"),
        }
    }
    // Linux-only diagnostic of this component process, not a device-worker
    // memory quota. No foreign process, profile or hardware state is read.
    let peak_rss_kib = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.split_whitespace().next()?.parse::<usize>().ok())
            })
        })
        .unwrap_or(0);
    costs.sort_unstable();
    if !costs.is_empty() {
        println!(
            "renders={} p50_ns={} p95_ns={} max_ns={} peak_rss_kib={}",
            costs.len(),
            costs[costs.len() / 2],
            costs[costs.len() * 95 / 100],
            costs[costs.len() - 1],
            peak_rss_kib
        );
    }
}
