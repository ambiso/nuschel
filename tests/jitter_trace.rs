// SPDX-License-Identifier: EUPL-1.2
//! Compile against a supplied old/new native jitter module for exact trace proof.
//! Fake payload bytes exercise policy only; codec reference fixtures are separate.
#[path = "jitter_module.rs"]
mod jitter;
use jitter::{JitterBuffer, Pull};
use std::time::{Duration, Instant};
fn main() {
    let mut buffer = JitterBuffer::default();
    let start = Instant::now();
    let mut clock = 0_u64;
    let mut next = 0_u64;
    let mut seed = 0x1234_5678_u64;
    for index in 0..10_000 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        clock += seed % 31_000_001;
        let frames = 1 + (seed >> 8) % 12;
        let behind = (seed >> 16) % 8;
        let sequence = next.saturating_sub(behind);
        if index % 71 == 0 {
            buffer = JitterBuffer::default();
            next = 0;
        }
        buffer.push(
            sequence,
            vec![(sequence % 256) as u8, frames as u8],
            index % 37 == 0,
            start + Duration::from_nanos(clock),
        );
        next += frames;
        for _ in 0..1 + (seed >> 24) % 3 {
            let pulled = buffer.pull();
            println!(
                "{index} {pulled:?} {:08x} {:08x}",
                buffer.target_ms().to_bits(),
                buffer.buffered_ms().to_bits()
            );
            if let Pull::Packet { opus, .. } = pulled {
                buffer.observe_frames(u64::from(opus[1]));
            }
        }
    }
}
