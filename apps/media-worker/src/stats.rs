// SPDX-License-Identifier: EUPL-1.2
//! Opt-in (`NUSCHL_AUDIO_TIMING=1`) call statistics for benchmarks.nix, kept by
//! the worker loop: capture gaps, playback underruns and capture-to-send
//! latency. Every 10 s it prints the totals so far and the latency histogram
//! (milliseconds:packets) to stderr.
use std::time::{Duration, Instant};

const BINS: usize = 500;
const FRAME_NS: u64 = 10_000_000;
const EVERY: Duration = Duration::from_secs(10);

pub struct Stats {
    enabled: bool,
    /// Capture time of the end of the newest processed microphone frame.
    newest: Option<u64>,
    capture_gaps: u64,
    underruns: u64,
    send_ms: [u32; BINS + 1],
    printed: Instant,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            enabled: std::env::var_os("NUSCHL_AUDIO_TIMING").is_some_and(|v| v == "1"),
            newest: None,
            capture_gaps: 0,
            underruns: 0,
            send_ms: [0; BINS + 1],
            printed: Instant::now(),
        }
    }

    /// Microphone frame `index` (since the devices opened) captured at `start`.
    /// A frame starting 10 ms or more after the previous one ended follows lost samples.
    pub fn capture(&mut self, index: u64, start: Option<u64>) {
        if index == 0 {
            self.newest = None;
        }
        if let Some(start) = start {
            if self.newest.is_some_and(|end| start >= end + FRAME_NS) {
                self.capture_gaps += 1;
            }
            self.newest = Some(start + FRAME_NS);
        }
    }

    /// A packet is sent at `now`; `queued` processed frames are still waiting behind it.
    pub fn send(&mut self, now: u64, queued: usize) {
        if let Some(newest) = self.newest {
            let latency = (now + queued as u64 * FRAME_NS).saturating_sub(newest) / 1_000_000;
            self.send_ms[(latency as usize).min(BINS)] += 1;
        }
    }

    /// Output callbacks that found nothing to play, counted once per window.
    pub fn underruns(&mut self, count: u32) {
        self.underruns += u64::from(count);
        if self.printed.elapsed() >= EVERY {
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
            "audio-stats pid={} capture_gaps={} underruns={} send_ms={}",
            std::process::id(),
            self.capture_gaps,
            self.underruns,
            histogram.join(",")
        );
    }
}
