// SPDX-License-Identifier: EUPL-1.2
use std::time::{Duration, Instant};

pub(crate) const WINDOW: Duration = Duration::from_millis(120);

/// Tokens are committed only after the transport accepts a frame. Every bucket
/// has a finite burst window; a bandwidth reduction clamps accumulated credit.
pub(crate) struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    pub fn new(now: Instant) -> Self {
        Self {
            tokens: f64::INFINITY,
            updated: now,
        }
    }

    pub fn available(&self, now: Instant, rate: u64) -> f64 {
        let cap = rate as f64 * WINDOW.as_secs_f64();
        (self.tokens + now.saturating_duration_since(self.updated).as_secs_f64() * rate as f64)
            .min(cap)
    }

    pub fn spend(&mut self, now: Instant, rate: u64, bits: f64) {
        self.tokens = self.available(now, rate) - bits;
        self.updated = now;
    }
}
