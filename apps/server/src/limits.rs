// SPDX-License-Identifier: EUPL-1.2
//! Rate limits as stock Murmur applies them: a leaky bucket for control
//! messages (`messagelimit` per second, `messageburst` deep) and a sliding
//! record of voice packet sizes for the per-user bandwidth cap.
use std::time::{Duration, Instant};

/// Murmur's `LeakyBucket`: each message adds a token, tokens drain at
/// `per_second`; a message that would overflow `burst` is limited.
#[derive(Clone, Debug)]
pub struct LeakyBucket {
    per_minute: u32,
    burst: u32,
    tokens: u64,
    since: Instant,
}

impl LeakyBucket {
    pub fn new(per_second: u32, burst: u32, now: Instant) -> Self {
        Self::per_minute(per_second.max(1).saturating_mul(60), burst, now)
    }

    /// A bucket that drains `per_minute` tokens a minute, for rates below one a
    /// second.
    pub fn per_minute(per_minute: u32, burst: u32, now: Instant) -> Self {
        Self {
            per_minute: per_minute.max(1),
            burst: burst.max(1),
            tokens: 0,
            since: now,
        }
    }

    /// Whether the bucket is full right now, without adding to it.
    pub fn exhausted(&mut self, now: Instant) -> bool {
        self.drain(now);
        self.tokens >= u64::from(self.burst)
    }

    /// Whether a message arriving `now` must be dropped.
    pub fn limited(&mut self, now: Instant) -> bool {
        self.drain(now);
        let limited = self.tokens + 1 > u64::from(self.burst);
        if !limited {
            self.tokens += 1;
        }
        limited
    }

    fn drain(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.since).as_millis();
        let drained = elapsed.saturating_mul(u128::from(self.per_minute)) / 60_000;
        // Restart the clock only once a whole token has drained, or frequent
        // messages would never drain anything.
        if drained > 0 {
            self.since = now;
        }
        self.tokens = self
            .tokens
            .saturating_sub(u64::try_from(drained).unwrap_or(u64::MAX));
    }
}

/// A budget of bytes per minute, refilled continuously (history pages).
#[derive(Clone, Debug)]
pub struct ByteBucket {
    per_minute: u64,
    available: u64,
    since: Instant,
}

impl ByteBucket {
    pub fn new(per_minute: u64, now: Instant) -> Self {
        Self {
            per_minute: per_minute.max(1),
            available: per_minute.max(1),
            since: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.since).as_millis();
        let refill = elapsed.saturating_mul(u128::from(self.per_minute)) / 60_000;
        if refill > 0 {
            self.since = now;
            self.available = self
                .available
                .saturating_add(u64::try_from(refill).unwrap_or(u64::MAX))
                .min(self.per_minute);
        }
    }

    /// Whether anything is left; a request may overdraw what remains once.
    pub fn exhausted(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.available == 0
    }

    pub fn spend(&mut self, bytes: u64, now: Instant) {
        self.refill(now);
        self.available = self.available.saturating_sub(bytes);
    }

    /// Milliseconds until something is available again.
    pub fn retry_after_ms(&self) -> u64 {
        (60_000 / self.per_minute).max(1)
    }
}

/// A budget of units per second, refilled continuously; a second's worth may
/// be spent at once (the whisper fan-out budget).
#[derive(Clone, Debug)]
pub struct Budget {
    per_second: u64,
    available: u64,
    since: Instant,
}

impl Budget {
    pub fn new(per_second: u64, now: Instant) -> Self {
        Self {
            per_second: per_second.max(1),
            available: per_second.max(1),
            since: now,
        }
    }

    /// Spend `units` if they are available. A full budget pays for anything,
    /// so a request larger than a second's worth is not refused for ever.
    pub fn take(&mut self, units: u64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.since).as_micros();
        let refill = elapsed.saturating_mul(u128::from(self.per_second)) / 1_000_000;
        if refill > 0 {
            self.since = now;
            self.available = self
                .available
                .saturating_add(u64::try_from(refill).unwrap_or(u64::MAX))
                .min(self.per_second);
        }
        if units <= self.available || self.available == self.per_second {
            self.available = self.available.saturating_sub(units);
            true
        } else {
            false
        }
    }
}

const SLOTS: usize = 360;

/// Murmur's `BandwidthRecord`: the last 360 voice packets with their arrival
/// times. A packet is refused when the rate over that window would exceed the
/// cap, so a burst cannot exceed it either.
#[derive(Clone, Debug)]
pub struct BandwidthRecord {
    sizes: Box<[u16; SLOTS]>,
    times: Box<[Instant; SLOTS]>,
    next: usize,
    sum: u64,
    pub online_since: Instant,
    pub idle_since: Instant,
}

impl BandwidthRecord {
    pub fn new(now: Instant) -> Self {
        Self {
            sizes: Box::new([0; SLOTS]),
            times: Box::new([now; SLOTS]),
            next: 0,
            sum: 0,
            online_since: now,
            idle_since: now,
        }
    }

    /// Record a packet of `size` bytes (IP, UDP, crypt header and payload) if
    /// it fits within `bytes_per_second`.
    pub fn add_frame(&mut self, size: usize, bytes_per_second: u64, now: Instant) -> bool {
        // The oldest slot (the one about to be replaced) bounds the window.
        // Slots start at the connection time, as Murmur's timers do.
        let elapsed = now
            .saturating_duration_since(self.times[self.next])
            .as_micros();
        if elapsed == 0 {
            return false;
        }
        let size = size.min(usize::from(u16::MAX)) as u64;
        let sum = self.sum - u64::from(self.sizes[self.next]) + size;
        if u128::from(sum) * 1_000_000 / elapsed > u128::from(bytes_per_second) {
            return false;
        }
        self.sizes[self.next] = size as u16;
        self.times[self.next] = now;
        self.sum = sum;
        self.next = (self.next + 1) % SLOTS;
        true
    }

    /// Bytes per second over the last second, for user statistics.
    pub fn bandwidth(&self, now: Instant) -> u32 {
        let mut sum = 0_u64;
        let mut elapsed = Duration::ZERO;
        for step in 1..SLOTS {
            let index = (self.next + SLOTS - step) % SLOTS;
            if self.sizes[index] == 0 {
                break;
            }
            let age = now.saturating_duration_since(self.times[index]);
            if age > Duration::from_secs(1) {
                break;
            }
            sum += u64::from(self.sizes[index]);
            elapsed = age;
        }
        if elapsed < Duration::from_millis(250) {
            return 0;
        }
        u32::try_from(sum * 1_000_000 / elapsed.as_micros().max(1) as u64).unwrap_or(u32::MAX)
    }

    /// Seconds since the last voice packet or explicit activity.
    pub fn idle_seconds(&self, now: Instant) -> u32 {
        let since = self.times[(self.next + SLOTS - 1) % SLOTS].max(self.idle_since);
        now.saturating_duration_since(since).as_secs() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fanout_budget_refills_per_second_and_pays_for_one_oversized_request() {
        let start = Instant::now();
        let mut budget = Budget::new(100, start);
        assert!(budget.take(60, start));
        assert!(!budget.take(60, start));
        assert!(budget.take(40, start));
        assert!(!budget.take(1, start));
        // Half a second refills half.
        assert!(budget.take(50, start + Duration::from_millis(500)));
        assert!(!budget.take(1, start + Duration::from_millis(500)));
        // A full budget pays for more than a second's worth once.
        assert!(budget.take(1000, start + Duration::from_secs(5)));
        assert!(!budget.take(1, start + Duration::from_secs(5)));
    }

    #[test]
    fn leaky_bucket_allows_a_burst_then_the_steady_rate() {
        let start = Instant::now();
        let mut bucket = LeakyBucket::new(1, 5, start);
        for _ in 0..5 {
            assert!(!bucket.limited(start));
        }
        assert!(bucket.limited(start), "sixth message in the same instant");
        assert!(bucket.limited(start + Duration::from_millis(999)));
        assert!(
            !bucket.limited(start + Duration::from_millis(1000)),
            "one token drained"
        );
        assert!(bucket.limited(start + Duration::from_millis(1001)));
        // Long silence drains everything but never below zero.
        let later = start + Duration::from_secs(3600);
        for _ in 0..5 {
            assert!(!bucket.limited(later));
        }
        assert!(bucket.limited(later));
    }

    #[test]
    fn leaky_bucket_drains_across_frequent_calls() {
        // Calls every 300 ms at 2 tokens/s must still drain (Murmur restarts the
        // timer only when at least one token drained).
        let start = Instant::now();
        let mut bucket = LeakyBucket::new(2, 1, start);
        let mut allowed = 0;
        for step in 0..100 {
            if !bucket.limited(start + Duration::from_millis(300 * step)) {
                allowed += 1;
            }
        }
        // 30 seconds at 2 per second with a burst of one: about one in two.
        assert!((45..=60).contains(&allowed), "{allowed}");
    }

    #[test]
    fn bandwidth_record_caps_the_rate() {
        let start = Instant::now();
        let mut record = BandwidthRecord::new(start);
        // 20 ms packets of 100 bytes are 5,000 B/s.
        let mut time = start;
        for _ in 0..1000 {
            time += Duration::from_millis(20);
            assert!(record.add_frame(100, 6000, time));
        }
        assert!(
            (4900..=5100).contains(&record.bandwidth(time)),
            "{}",
            record.bandwidth(time)
        );
        // Doubling the packet size exceeds a 6,000 B/s cap after a while.
        let mut refused = 0;
        for _ in 0..1000 {
            time += Duration::from_millis(20);
            if !record.add_frame(200, 6000, time) {
                refused += 1;
            }
        }
        assert!(refused > 300, "{refused}");
        // A burst right after connecting is measured against the connection age.
        let mut fresh = BandwidthRecord::new(start);
        assert!(!fresh.add_frame(100, 6000, start));
        let soon = start + Duration::from_millis(100);
        let burst = (0..360)
            .filter(|_| fresh.add_frame(100, 6000, soon))
            .count();
        assert_eq!(burst, 6, "600 bytes in 100 ms at 6,000 B/s");
        assert_eq!(fresh.idle_seconds(start + Duration::from_secs(5)), 4);
    }
}
