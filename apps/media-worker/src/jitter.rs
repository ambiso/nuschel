// SPDX-License-Identifier: EUPL-1.2
//! Native clock adapter for the shared, platform-independent jitter policy.
pub use nuschl_audio_jitter::Pull;
#[cfg(test)]
use nuschl_audio_jitter::{CATCH_UP_MS, MAX_SEQUENCE};
use std::time::Instant;

pub struct JitterBuffer<P = Vec<u8>> {
    inner: nuschl_audio_jitter::JitterBuffer<P>,
    origin: Option<Instant>,
}

impl<P> Default for JitterBuffer<P> {
    fn default() -> Self {
        Self {
            inner: nuschl_audio_jitter::JitterBuffer::default(),
            origin: None,
        }
    }
}

impl<P: Clone> JitterBuffer<P> {
    pub fn push(&mut self, sequence: u64, opus: P, end: bool, now: Instant) {
        let origin = *self.origin.get_or_insert(now);
        self.inner
            .push(sequence, opus, end, now.saturating_duration_since(origin));
    }
    pub fn pull(&mut self) -> Pull<P> {
        self.inner.pull()
    }
    pub fn observe_frames(&mut self, frames: u64) {
        self.inner.observe_frames(frames);
    }
    pub fn is_playing(&self) -> bool {
        self.inner.is_playing()
    }
    #[cfg(test)]
    pub fn target_ms(&self) -> f32 {
        self.inner.target_ms()
    }
    #[cfg(test)]
    pub fn buffered_ms(&self) -> f32 {
        self.inner.buffered_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn variable_duration_loss_preserves_the_next_native_packet() {
        let now = Instant::now();
        let mut buffer = JitterBuffer::default();
        buffer.push(0, vec![120], false, now);
        buffer.push(13, vec![20], true, now + Duration::from_millis(130));
        assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        buffer.observe_frames(12);
        assert_eq!(
            buffer.pull(),
            Pull::Lost {
                fec: Some(vec![20]),
                frames: 1
            }
        );
        assert!(matches!(buffer.pull(), Pull::Packet { end: true, .. }));
    }

    #[test]
    fn decoded_long_and_changing_durations_advance_from_the_packet_origin() {
        let mut buffer = JitterBuffer::default();
        let now = Instant::now();
        buffer.push(0, vec![1], false, now);
        buffer.push(12, vec![2], false, now);
        buffer.push(14, vec![3], true, now);
        assert!(matches!(buffer.pull(), Pull::Packet { opus, .. } if opus == [1]));
        buffer.observe_frames(12);
        assert!(matches!(buffer.pull(), Pull::Packet { opus, .. } if opus == [2]));
        buffer.observe_frames(2);
        assert!(matches!(buffer.pull(), Pull::Packet { opus, end:true } if opus == [3]));
        buffer.observe_frames(12);
        assert_eq!(
            buffer.pull(),
            Pull::Idle,
            "terminator must not reopen next tick"
        );
    }

    #[test]
    fn sequence_numbers_near_the_end_of_their_range_are_ignored() {
        // Found writing the voice_playback fuzz target: the next slot and the
        // buffered length overflowed (a panic with overflow checks, a garbled
        // stream without).
        let mut buffer = JitterBuffer::default();
        let start = Instant::now();
        for index in 0..4_u64 {
            let at = start + Duration::from_millis(20 * index);
            buffer.push(u64::MAX - 7 + 2 * index, vec![1], index == 3, at);
        }
        assert_eq!(buffer.buffered_ms(), 0.);
        assert_eq!(buffer.pull(), Pull::Idle);
        buffer.push(MAX_SEQUENCE, vec![2], true, start);
        assert!(matches!(buffer.pull(), Pull::Packet { end: true, .. }));
    }

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
    }

    #[test]
    fn drops_late_and_duplicate_packets_and_stops_at_the_terminator() {
        let start = Instant::now();
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(0, 0), (2, 20), (4, 40)]);
        buffer.push(6, vec![6], true, start + Duration::from_millis(60));
        assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        // A late packet and a duplicate (with different contents) are ignored.
        buffer.push(0, vec![0], false, start + Duration::from_millis(70));
        buffer.push(4, vec![44], false, start + Duration::from_millis(70));
        for expected in [2, 4] {
            assert_eq!(
                buffer.pull(),
                Pull::Packet {
                    opus: vec![expected],
                    end: false
                }
            );
        }
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
        let mut steady: JitterBuffer = JitterBuffer::default();
        let mut jittery: JitterBuffer = JitterBuffer::default();
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
        let mut burst: JitterBuffer = JitterBuffer::default();
        for index in 0..50_u64 {
            burst.push(index * 2, vec![], false, start);
        }
        assert!(matches!(burst.pull(), Pull::Packet { .. }));
        assert!(burst.buffered_ms() <= burst.target_ms() + CATCH_UP_MS + 20.);
    }
}
