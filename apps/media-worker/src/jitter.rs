// SPDX-License-Identifier: EUPL-1.2
//! Per-speaker adaptive jitter buffer for Mumble voice.
//!
//! Mumble numbers voice frames in 10 ms units, so a 20 ms Opus packet advances
//! the sequence by two. Packets are held until a short, adaptive target delay is
//! buffered, then played in order. Gaps are filled by Opus in-band FEC when the
//! following packet carries it, otherwise by packet-loss concealment. Late
//! packets are dropped, and excess buffering is discarded instead of letting
//! latency grow during a long call.
use std::collections::BTreeMap;
use std::time::Instant;

pub const FRAME_MS: f32 = 10.;
const MIN_TARGET_MS: f32 = 40.;
const MAX_TARGET_MS: f32 = 200.;
/// Excess above the target that is dropped to catch up.
const CATCH_UP_MS: f32 = 120.;
/// Concealment beyond this ends playout until enough audio is buffered again.
const MAX_CONCEALED_FRAMES: u32 = 10;
const MAX_PACKETS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum Pull {
    /// Decode this packet; it covers `frames` 10 ms frames.
    Packet { opus: Vec<u8>, end: bool },
    /// The packet for the next slot is missing; recover it from `fec` (the
    /// following packet's in-band copy) when available, else conceal.
    Lost { fec: Option<Vec<u8>>, frames: u64 },
    /// Nothing to play: waiting for the initial target or after an underrun.
    Idle,
}

struct Packet {
    opus: Vec<u8>,
    end: bool,
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, Packet>,
    next: Option<u64>,
    playing: bool,
    concealed: u32,
    frames_per_packet: u64,
    jitter_ms: f32,
    last_arrival: Option<(Instant, u64)>,
    pub stats: JitterStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u32,
    pub late: u32,
    pub concealed: u32,
    pub recovered_fec: u32,
    pub dropped_to_catch_up: u32,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            playing: false,
            concealed: 0,
            frames_per_packet: 2,
            jitter_ms: 0.,
            last_arrival: None,
            stats: JitterStats::default(),
        }
    }
}

impl JitterBuffer {
    pub fn target_ms(&self) -> f32 {
        (self.frames_per_packet as f32 * FRAME_MS + 2.5 * self.jitter_ms + 10.)
            .clamp(MIN_TARGET_MS, MAX_TARGET_MS)
    }

    /// Audio buffered ahead of the playout position, in milliseconds.
    pub fn buffered_ms(&self) -> f32 {
        let Some((first, _)) = self.packets.first_key_value() else {
            return 0.;
        };
        let start = self.next.unwrap_or(*first).min(*first);
        let (last, _) = self.packets.last_key_value().unwrap_or((
            first,
            &Packet {
                opus: Vec::new(),
                end: false,
            },
        ));
        ((last + self.frames_per_packet).saturating_sub(start)) as f32 * FRAME_MS
    }

    /// The decoder reports how many 10 ms frames a packet really carried.
    pub fn observe_frames(&mut self, frames: u64) {
        if (1..=6).contains(&frames) {
            self.frames_per_packet = frames;
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn push(&mut self, sequence: u64, opus: Vec<u8>, end: bool, now: Instant) {
        self.stats.received = self.stats.received.saturating_add(1);
        if self.next.is_some_and(|next| sequence < next) || self.packets.contains_key(&sequence) {
            self.stats.late = self.stats.late.saturating_add(1);
            return;
        }
        // Packet spacing is the smallest gap to a neighbour; the decoder
        // corrects it from the actual packet duration via `observe_frames`.
        let before = self
            .packets
            .range(..sequence)
            .next_back()
            .map(|(previous, _)| sequence - previous);
        let after = self
            .packets
            .range(sequence + 1..)
            .next()
            .map(|(next, _)| next - sequence);
        if let Some(spacing) = before.into_iter().chain(after).min()
            && (1..=6).contains(&spacing)
            && spacing < self.frames_per_packet
        {
            self.frames_per_packet = spacing;
        }
        if let Some((arrived, previous)) = self.last_arrival
            && sequence > previous
        {
            // RFC 3550 interarrival jitter, in milliseconds.
            let expected = (sequence - previous) as f32 * FRAME_MS;
            let actual = now.duration_since(arrived).as_secs_f32() * 1000.;
            let deviation = (actual - expected).abs().min(1000.);
            self.jitter_ms += (deviation - self.jitter_ms) / 16.;
        }
        if self
            .last_arrival
            .is_none_or(|(_, previous)| sequence > previous)
        {
            self.last_arrival = Some((now, sequence));
        }
        self.packets.insert(sequence, Packet { opus, end });
        while self.packets.len() > MAX_PACKETS {
            self.packets.pop_first();
        }
    }

    /// The next action for the playout clock, which asks whenever this
    /// speaker's decoded audio runs low.
    pub fn pull(&mut self) -> Pull {
        if !self.playing {
            let end_seen = self.packets.values().any(|packet| packet.end);
            if self.packets.is_empty() || (self.buffered_ms() < self.target_ms() && !end_seen) {
                return Pull::Idle;
            }
            self.playing = true;
            self.concealed = 0;
            self.next = self
                .packets
                .first_key_value()
                .map(|(sequence, _)| *sequence);
        }
        // Drop excess so latency cannot accumulate.
        while self.buffered_ms() > self.target_ms() + CATCH_UP_MS && self.packets.len() > 1 {
            if let Some((sequence, _)) = self.packets.pop_first() {
                self.next = Some(sequence + self.frames_per_packet);
                self.stats.dropped_to_catch_up = self.stats.dropped_to_catch_up.saturating_add(1);
            }
        }
        let Some(next) = self.next else {
            self.playing = false;
            return Pull::Idle;
        };
        if let Some(packet) = self.packets.remove(&next) {
            self.concealed = 0;
            self.next = Some(next + self.frames_per_packet);
            if packet.end {
                self.playing = false;
                self.next = None;
            }
            return Pull::Packet {
                opus: packet.opus,
                end: packet.end,
            };
        }
        // Missing slot: conceal briefly, then wait to rebuffer.
        self.concealed += 1;
        if self.concealed > MAX_CONCEALED_FRAMES || (self.packets.is_empty() && self.concealed > 2)
        {
            self.playing = false;
            self.next = None;
            return Pull::Idle;
        }
        self.stats.concealed = self.stats.concealed.saturating_add(1);
        let frames = self.frames_per_packet;
        self.next = Some(next + frames);
        let fec = self
            .packets
            .get(&(next + frames))
            .map(|packet| packet.opus.clone());
        if fec.is_some() {
            self.stats.recovered_fec = self.stats.recovered_fec.saturating_add(1);
        }
        Pull::Lost { fec, frames }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn feed(buffer: &mut JitterBuffer, start: Instant, sequences: &[(u64, u64)]) {
        for (sequence, at_ms) in sequences {
            buffer.push(
                *sequence,
                vec![*sequence as u8],
                false,
                start + Duration::from_millis(*at_ms),
            );
        }
    }

    #[test]
    fn waits_for_the_target_then_plays_in_order() {
        let start = Instant::now();
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(10, 0)]);
        assert_eq!(buffer.pull(), Pull::Idle, "prebuffering");
        feed(&mut buffer, start, &[(12, 20), (14, 40)]);
        for expected in [10, 12, 14] {
            assert_eq!(
                buffer.pull(),
                Pull::Packet {
                    opus: vec![expected],
                    end: false
                }
            );
        }
    }

    #[test]
    fn reorders_recovers_with_fec_and_conceals_gaps() {
        let start = Instant::now();
        let mut buffer = JitterBuffer::default();
        // 4 arrives before 2; 6 is lost; 8 carries FEC for 6.
        feed(
            &mut buffer,
            start,
            &[(0, 0), (4, 20), (2, 25), (8, 60), (10, 80)],
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![0],
                end: false
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![2],
                end: false
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![4],
                end: false
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Lost {
                fec: Some(vec![8]),
                frames: 2
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![8],
                end: false
            }
        );
        assert_eq!(buffer.stats.recovered_fec, 1);
    }

    #[test]
    fn drops_late_and_duplicate_packets_and_stops_at_the_terminator() {
        let start = Instant::now();
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(0, 0), (2, 20), (4, 40)]);
        buffer.push(6, vec![6], true, start + Duration::from_millis(60));
        assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        buffer.push(0, vec![0], false, start + Duration::from_millis(70));
        buffer.push(4, vec![4], false, start + Duration::from_millis(70));
        assert_eq!(buffer.stats.late, 2);
        assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![6],
                end: true
            }
        );
        assert!(!buffer.is_playing());
        assert_eq!(buffer.pull(), Pull::Idle);
    }

    #[test]
    fn underrun_rebuffers_instead_of_concealing_forever() {
        let start = Instant::now();
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(0, 0), (2, 20), (4, 40)]);
        for _ in 0..3 {
            assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        }
        let mut lost = 0;
        while let Pull::Lost { .. } = buffer.pull() {
            lost += 1;
        }
        assert!(lost <= 2 && !buffer.is_playing());
    }

    #[test]
    fn jitter_raises_the_target_and_excess_is_dropped() {
        let start = Instant::now();
        let mut steady = JitterBuffer::default();
        let mut jittery = JitterBuffer::default();
        for index in 0..50_u64 {
            steady.push(
                index * 2,
                vec![],
                false,
                start + Duration::from_millis(index * 20),
            );
            let wobble = if index % 2 == 0 { 0 } else { 35 };
            jittery.push(
                index * 2,
                vec![],
                false,
                start + Duration::from_millis(index * 20 + wobble),
            );
        }
        assert!(jittery.target_ms() > steady.target_ms() + 20.);
        // A burst of 50 packets (1 s) collapses to about target + catch-up window.
        let mut burst = JitterBuffer::default();
        for index in 0..50_u64 {
            burst.push(index * 2, vec![], false, start);
        }
        assert!(matches!(burst.pull(), Pull::Packet { .. }));
        assert!(burst.buffered_ms() <= burst.target_ms() + CATCH_UP_MS + 20.);
        assert!(burst.stats.dropped_to_catch_up > 0);
    }
}
