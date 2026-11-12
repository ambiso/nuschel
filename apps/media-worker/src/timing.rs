// SPDX-License-Identifier: EUPL-1.2
//! Opt-in stage timings only: no captured samples, devices or identities.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Clone)]
pub struct Timing(Arc<State>);
struct State {
    enabled: bool,
    start: Instant,
    first_capture_us: AtomicU64,
    capture_reported: AtomicBool,
}
impl Default for Timing {
    fn default() -> Self {
        Self::new()
    }
}
impl Timing {
    pub fn new() -> Self {
        Self(Arc::new(State {
            enabled: std::env::var_os("NUSCHL_AUDIO_TIMING").is_some_and(|v| v == "1"),
            start: Instant::now(),
            first_capture_us: AtomicU64::new(u64::MAX),
            capture_reported: AtomicBool::new(false),
        }))
    }
    pub fn mark(&self, stage: &str) {
        if self.0.enabled {
            self.report(stage, self.0.start.elapsed().as_micros() as u64);
        }
    }
    fn report(&self, stage: &str, elapsed_us: u64) {
        eprintln!("audio-timing stage={stage} elapsed_us={elapsed_us}");
    }
    /// Called once on first PCM arrival; no allocation, locks, logging or I/O
    /// on the audio callback. Subsequent callbacks only read one atomic.
    pub fn capture(&self) {
        if self.0.enabled && self.0.first_capture_us.load(Ordering::Relaxed) == u64::MAX {
            let _ = self.0.first_capture_us.compare_exchange(
                u64::MAX,
                self.0.start.elapsed().as_micros() as u64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }
    /// The worker loop prints the timestamp recorded by the callback, even if
    /// sequential device setup kept it busy when the samples arrived.
    pub fn report_capture(&self) {
        if !self.0.enabled {
            return;
        }
        let time = self.0.first_capture_us.load(Ordering::Relaxed);
        if time != u64::MAX && !self.0.capture_reported.swap(true, Ordering::Relaxed) {
            self.report("first_capture_callback", time);
        }
    }
}
