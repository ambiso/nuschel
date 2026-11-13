// SPDX-License-Identifier: EUPL-1.2
//! Bounded, processed PCM from one unmuted epoch. The gate authorizes sending;
//! prefix storage alone never opens it. No device or callback work lives here.
use crate::dsp::FRAME;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const PREFIX_FRAMES: usize = 8; // 80 ms covers measured classifier startup.
const MAX_PENDING_FRAMES: usize = PREFIX_FRAMES + 2;
const PACKET_PERIOD: Duration = Duration::from_millis(20);
struct Frame {
    samples: [f32; FRAME],
    prefix: bool,
}
pub struct Packet {
    pub samples: [f32; 2 * FRAME],
    pub end: bool,
}
pub struct CaptureQueue {
    prefix: VecDeque<[f32; FRAME]>,
    pending: VecDeque<Frame>,
    active: bool,
    ending: bool,
    next: Option<Instant>,
}
impl Default for CaptureQueue {
    fn default() -> Self {
        Self {
            prefix: VecDeque::with_capacity(PREFIX_FRAMES),
            pending: VecDeque::with_capacity(MAX_PENDING_FRAMES),
            active: false,
            ending: false,
            next: None,
        }
    }
}
impl CaptureQueue {
    pub fn reset(&mut self) {
        self.prefix.clear();
        self.pending.clear();
        self.active = false;
        self.ending = false;
        self.next = None;
    }
    /// Called only on processed, eligible unmuted PCM. `prefix_allowed` is
    /// false for PTT/continuous, local preview and the echo guard.
    pub fn push(&mut self, samples: [f32; FRAME], gate: bool, prefix_allowed: bool) {
        if !prefix_allowed {
            self.prefix.clear();
            // Also revoke any not-yet-emitted prefix when the guard changes.
            self.pending.retain(|frame| !frame.prefix);
        }
        if gate {
            let reopening = !self.active || self.ending;
            let prefix_frames = if reopening && prefix_allowed {
                self.prefix.len()
            } else {
                0
            };
            // Make room BEFORE appending, so a reopening edge cannot allocate
            // a larger queue or discard the next word's retained onset.
            while self.pending.len() + prefix_frames >= MAX_PENDING_FRAMES {
                self.pending.pop_front();
                self.pending.pop_front();
            }
            if reopening && prefix_allowed {
                self.pending
                    .extend(self.prefix.drain(..).map(|samples| Frame {
                        samples,
                        prefix: true,
                    }));
            }
            self.prefix.clear();
            self.active = true;
            self.ending = false;
            self.pending.push_back(Frame {
                samples,
                prefix: false,
            });
        } else {
            self.ending = self.active;
            if prefix_allowed {
                if self.prefix.len() == PREFIX_FRAMES {
                    self.prefix.pop_front();
                }
                self.prefix.push_back(samples);
            }
        }
    }
    pub fn queued(&self) -> usize {
        self.pending.len()
    }
    /// One packet on the 20 ms sample clock. Small scheduler jitter preserves
    /// phase; a large stall starts a new deadline instead of a catch-up burst.
    /// There is no initial fill timer.
    pub fn due(&mut self, now: Instant) -> Option<Packet> {
        if !self.active
            || self.next.is_some_and(|next| now < next)
            || (self.pending.len() < 2 && !self.ending)
        {
            return None;
        }
        let mut samples = [0.; 2 * FRAME];
        for half in samples.as_chunks_mut::<FRAME>().0 {
            if let Some(frame) = self.pending.pop_front() {
                half.copy_from_slice(&frame.samples);
            }
        }
        let end = self.ending && self.pending.is_empty();
        if end {
            self.active = false;
            self.ending = false;
            self.next = None;
        } else {
            self.next = Some(match self.next {
                Some(previous) if now < previous + PACKET_PERIOD => previous + PACKET_PERIOD,
                _ => now + PACKET_PERIOD,
            });
        }
        Some(Packet { samples, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_preserves_the_onset_without_opening_a_closed_gate_or_waiting_to_fill() {
        let mut queue = CaptureQueue::default();
        let now = Instant::now();
        for marker in 1..=4 {
            queue.push([marker as f32; FRAME], false, true);
            assert!(queue.due(now).is_none());
        }
        queue.push([5.; FRAME], true, true);
        let first = queue.due(now).unwrap();
        assert_eq!(first.samples[..FRAME], [1.; FRAME]);
        assert_eq!(first.samples[FRAME..], [2.; FRAME]);
        assert!(queue.due(now + Duration::from_millis(19)).is_none());
        let second = queue.due(now + PACKET_PERIOD).unwrap();
        assert_eq!(second.samples[..FRAME], [3.; FRAME]);
        assert_eq!(second.samples[FRAME..], [4.; FRAME]);
        assert!(
            queue.due(now + PACKET_PERIOD).is_none(),
            "no catch-up burst"
        );
    }
    #[test]
    fn a_reopened_gate_keeps_the_next_onset_while_the_previous_tail_is_draining() {
        let mut queue = CaptureQueue::default();
        let now = Instant::now();
        let frame = |marker| [marker as f32 / 100.; FRAME];
        for marker in 1..=8 {
            queue.push(frame(marker), false, true);
        }
        queue.push(frame(9), true, true);
        let mut markers = Vec::new();
        let mut take = |packet: Packet| {
            markers.push(packet.samples[0]);
            markers.push(packet.samples[FRAME]);
        };
        take(queue.due(now).unwrap());
        for marker in 10..=12 {
            queue.push(frame(marker), false, true);
        }
        take(queue.due(now + PACKET_PERIOD).unwrap());
        // Old speech is still draining here; this is a real closed→open edge.
        assert!(queue.active && queue.ending);
        queue.push(frame(13), true, true);
        queue.push(frame(14), true, true);
        assert!(queue.pending.len() <= MAX_PENDING_FRAMES);
        for tick in 2..=6 {
            let at = now + PACKET_PERIOD * tick;
            take(queue.due(at).unwrap());
            assert!(queue.due(at).is_none(), "reopening must not flood packets");
        }
        assert_eq!(
            markers,
            (1..=14).map(|v| v as f32 / 100.).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reset_and_echo_guard_revoke_prefix_and_ptt_never_replays_it() {
        let now = Instant::now();
        for reset in [true, false] {
            let mut queue = CaptureQueue::default();
            queue.push([0.9; FRAME], false, true);
            if reset {
                queue.reset();
            }
            queue.push([0.1; FRAME], true, false);
            assert!(queue.due(now).is_none());
            queue.push([0.2; FRAME], true, false);
            let packet = queue.due(now).unwrap();
            assert!(packet.samples.iter().all(|x| *x <= 0.2));
        }
        let mut queue = CaptureQueue::default();
        queue.push([0.9; FRAME], false, true);
        queue.push([0.1; FRAME], true, true);
        queue.push([0.2; FRAME], true, false);
        let packet = queue.due(now).unwrap();
        assert!(packet.samples.iter().all(|x| *x <= 0.2));
    }
    #[test]
    fn resetting_an_already_open_epoch_removes_its_unsent_prefix_and_pcm() {
        let mut queue = CaptureQueue::default();
        let now = Instant::now();
        queue.push([0.9; FRAME], false, true);
        queue.push([0.8; FRAME], true, true);
        queue.reset(); // mute/deaf/epoch/settings/preview/device transition.
        queue.push([0.1; FRAME], true, false);
        queue.push([0.2; FRAME], true, false);
        let packet = queue.due(now).unwrap();
        assert!(packet.samples.iter().all(|v| *v <= 0.2));
    }

    #[test]
    fn memory_latency_and_end_padding_are_bounded_even_after_a_stall() {
        let mut queue = CaptureQueue::default();
        let now = Instant::now();
        for _ in 0..10000 {
            queue.push([0.1; FRAME], false, true);
        }
        assert_eq!(queue.prefix.len(), PREFIX_FRAMES);
        assert!(queue.due(now).is_none());
        for _ in 0..10000 {
            queue.push([0.2; FRAME], true, true);
        }
        assert!(queue.pending.len() <= MAX_PENDING_FRAMES);
        assert!(queue.due(now).is_some());
        assert!(queue.due(now).is_none());
        queue.reset();
        queue.push([0.3; FRAME], true, false);
        queue.push([0.; FRAME], false, false);
        let last = queue.due(now).unwrap();
        assert!(last.end);
        assert_eq!(last.samples[FRAME..], [0.; FRAME]);
        assert!(queue.due(now + PACKET_PERIOD).is_none());
    }
}
