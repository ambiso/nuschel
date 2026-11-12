// SPDX-License-Identifier: EUPL-1.2
//! Device-free decoder probe; reference fixture/CTL comparison lives in Python.
#![forbid(unsafe_code)]
use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder};
use std::{fs, io::Write};

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert!((4..=5).contains(&args.len()));
    let disabled = args[3] == "1";
    let reset_at = args
        .get(4)
        .map(|value| value.to_str().unwrap().parse::<usize>().unwrap());
    assert!(fs::metadata(&args[1]).unwrap().len() <= 4 * 1024 * 1024);
    let bytes = fs::read(&args[1]).unwrap();
    let mut decoder = OpusDecoder::new(48_000, 2).unwrap();
    #[cfg(phase_control)]
    decoder.set_phase_inversion_disabled(disabled);
    #[cfg(not(phase_control))]
    assert!(!disabled, "baseline decoder does not expose this control");
    let mut out = fs::File::create(&args[2]).unwrap();
    let mut scratch = [0.; MAX_PACKET_SAMPLES * 2];
    let mut at = 0;
    let mut packet_index = 0;
    let mut ranges = Vec::new();
    while at < bytes.len() {
        assert!(packet_index < 150 && at + 2 <= bytes.len());
        let length = usize::from(u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()));
        at += 2;
        assert!((1..=1024).contains(&length) && at + length <= bytes.len());
        if reset_at == Some(packet_index) {
            decoder.reset_state().unwrap();
        }
        let count = decoder
            .decode(&bytes[at..at + length], MAX_PACKET_SAMPLES, &mut scratch)
            .unwrap();
        assert_eq!(count, 960);
        for sample in &scratch[..count * 2] {
            out.write_all(&sample.to_le_bytes()).unwrap();
        }
        ranges.push(decoder.final_range());
        at += length;
        packet_index += 1;
    }
    println!("{ranges:?}");
}
