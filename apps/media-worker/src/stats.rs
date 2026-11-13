// SPDX-License-Identifier: EUPL-1.2
//! Opt-in (`NUSCHL_AUDIO_TIMING=1`) call statistics for benchmarks.nix, kept by
//! the worker loop: capture gaps, playback underruns, late packets and
//! capture-to-send latency. Every 10 s and at shutdown it prints the totals so
//! far and the latency histogram (milliseconds:packets) to stderr, from the
//! worker loop; only test harnesses enable it.
use std::time::{Duration, Instant};

const BINS: usize = 500;
const FRAME_NS: u64 = 10_000_000;
/// Stamps settle during the first second after the devices open (48 kHz frames).
const SETTLE: u64 = 48_000;
const EVERY: Duration = Duration::from_secs(10);

pub struct Stats {
    enabled: bool,
    /// Capture time of the end of the newest processed microphone frame.
    newest: Option<u64>,
    capture_gaps: u64,
    underruns: u64,
    late_packets: u64,
    send_ms: [u32; BINS + 1],
    printed: Instant,
}

impl Stats {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            newest: None,
            capture_gaps: 0,
            underruns: 0,
            late_packets: 0,
            send_ms: [0; BINS + 1],
            printed: Instant::now(),
        }
    }

    /// Microphone frame `index` (since the devices opened) captured at `start`.
    /// A capture gap: a frame stamped 10 ms or more after the previous one
    /// ended, from a late or missing callback interval. The first second is
    /// not counted.
    pub fn capture(&mut self, index: u64, start: Option<u64>) {
        if index == 0 {
            self.newest = None;
        }
        if let Some(start) = start {
            if index >= SETTLE && self.newest.is_some_and(|end| start >= end + FRAME_NS) {
                self.capture_gaps += 1;
            }
            self.newest = Some(start + FRAME_NS);
        }
    }

    /// A packet is sent at `now`; `queued` processed frames are still waiting
    /// behind it, and the transmit queue has sent `late` packets a period or more
    /// after their deadline so far.
    pub fn send(&mut self, now: u64, queued: usize, late: u64) {
        self.late_packets = late;
        if let Some(newest) = self.newest {
            let latency = (now + queued as u64 * FRAME_NS).saturating_sub(newest) / 1_000_000;
            self.send_ms[(latency as usize).min(BINS)] += 1;
        }
    }

    /// Output callbacks that found nothing to play, counted once per window.
    pub fn underruns(&mut self, count: u32) {
        self.underruns += u64::from(count);
        if self.enabled && self.printed.elapsed() >= EVERY {
            self.print();
        }
    }

    pub fn print(&mut self) {
        if !self.enabled {
            return;
        }
        self.printed = Instant::now();
        let histogram: Vec<String> = (self.send_ms.iter().enumerate())
            .filter(|(_, count)| **count > 0)
            .map(|(ms, count)| format!("{ms}:{count}"))
            .collect();
        eprintln!(
            "audio-stats pid={} capture_gaps={} underruns={} late_packets={} send_ms={}",
            std::process::id(),
            self.capture_gaps,
            self.underruns,
            self.late_packets,
            histogram.join(",")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_gaps_and_capture_to_send_latency() {
        let mut stats = Stats::new(false);
        stats.capture(0, Some(0));
        // A jump while the stamps settle is not counted.
        stats.capture(480, Some(2 * FRAME_NS));
        stats.capture(SETTLE, Some(3 * FRAME_NS));
        // 10 ms of samples were lost before this frame.
        stats.capture(SETTLE + 480, Some(5 * FRAME_NS));
        assert_eq!(stats.capture_gaps, 1);
        // The newest frame ends at 60 ms; one frame waits behind the packet,
        // whose last sample ends at 50 ms. Sent at 70 ms: 20 ms.
        stats.send(7 * FRAME_NS, 1, 0);
        assert_eq!(stats.send_ms[20], 1);
    }
}
