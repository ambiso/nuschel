// SPDX-License-Identifier: EUPL-1.2
//! Voice from a hostile server as the media worker plays it: packets with any
//! sequence numbers through a speaker's jitter buffer (the media worker's own
//! `jitter.rs`, included as a module: the worker is a binary) and the Opus
//! decoder (opus-pure), with in-band FEC, concealment and resets as
//! `Speaker::fill` asks for them. Nothing may panic (debug assertions are on,
//! so neither may the arithmetic overflow), and no decode may produce more
//! samples than it was given room for.
//!
//! Records: a flags byte (bit 0 ends the stream, bits 1–2 say how many 10 ms
//! frames are pulled after the packet, bit 3 that an 8-byte big-endian
//! sequence number follows instead of a one-byte step), the step or sequence,
//! the milliseconds since the previous packet, a length byte and the packet.
#![no_main]

#[allow(dead_code)]
#[path = "../../apps/media-worker/src/jitter.rs"]
mod jitter;

use jitter::{JitterBuffer, Pull};
use libfuzzer_sys::fuzz_target;
use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder};
use std::time::{Duration, Instant};

/// 10 ms at 48 kHz (the media worker's `dsp::FRAME`).
const FRAME: usize = 480;

struct Speaker {
    decoder: OpusDecoder,
    jitter: JitterBuffer,
    scratch: Vec<f32>,
}

impl Speaker {
    /// One round of `Speaker::fill`: false once there is nothing to play.
    fn pull(&mut self) -> bool {
        match self.jitter.pull() {
            Pull::Packet { opus, end } => {
                if let Ok(count) = self
                    .decoder
                    .decode(&opus, MAX_PACKET_SAMPLES, &mut self.scratch)
                {
                    assert!(count <= MAX_PACKET_SAMPLES);
                    self.jitter.observe_frames((count / FRAME) as u64);
                }
                if end {
                    let _ = self.decoder.reset_state();
                }
                true
            }
            Pull::Lost { fec, frames } => {
                let size = (frames as usize * FRAME).min(MAX_PACKET_SAMPLES);
                let decoded = match fec {
                    Some(next) => self.decoder.decode_fec(&next, size, &mut self.scratch),
                    None => self.decoder.decode(&[], size, &mut self.scratch),
                };
                decoded.is_ok_and(|count| {
                    assert!(count <= size);
                    true
                })
            }
            Pull::Idle => false,
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut speaker = Speaker {
        decoder: OpusDecoder::new(48_000, 1).expect("a mono decoder"),
        jitter: JitterBuffer::default(),
        scratch: vec![0.; MAX_PACKET_SAMPLES],
    };
    let mut now = Instant::now();
    let mut sequence = 0_u64;
    let mut rest = data;
    while let Some((&flags, after)) = rest.split_first() {
        rest = after;
        if flags & 8 != 0 {
            let Some((bytes, after)) = rest.split_first_chunk::<8>() else {
                break;
            };
            sequence = u64::from_be_bytes(*bytes);
            rest = after;
        } else if let Some((&step, after)) = rest.split_first() {
            sequence = sequence.saturating_add(u64::from(step));
            rest = after;
        }
        let [elapsed, length, after @ ..] = rest else {
            break;
        };
        now += Duration::from_millis(u64::from(*elapsed));
        let length = usize::from(*length).min(after.len());
        speaker
            .jitter
            .push(sequence, after[..length].to_vec(), flags & 1 != 0, now);
        rest = &after[length..];
        for _ in 0..(flags >> 1) & 3 {
            speaker.pull();
        }
    }
    // Play out what is left.
    for _ in 0..80 {
        if !speaker.pull() {
            break;
        }
    }
});
