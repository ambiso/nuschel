// SPDX-License-Identifier: EUPL-1.2
//! Limits per client address and per tunnel. Reaching one refuses or slows only
//! that tunnel; nothing here stops the relay for everyone else.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use crate::config::{Config, canonical};

/// Who a limit counts: an IPv4 address, or an IPv6 /64 (one subscriber's
/// network, which can hold many addresses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientKey([u8; 16]);

impl ClientKey {
    pub fn of(ip: IpAddr) -> Self {
        match canonical(ip) {
            IpAddr::V4(v4) => {
                let mut key = [0; 16];
                key[..4].copy_from_slice(&v4.octets());
                key[4] = 4;
                Self(key)
            }
            IpAddr::V6(v6) => {
                let mut key = [0; 16];
                key[..8].copy_from_slice(&v6.octets()[..8]);
                key[8] = 6;
                Self(key)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    RateLimited,
    Busy,
}

struct Client {
    tunnels: usize,
    opens: f64,
    updated: Instant,
}

struct State {
    clients: HashMap<ClientKey, Client>,
    tunnels: usize,
}

/// Admission of new tunnels.
#[derive(Clone)]
pub struct Admission {
    state: Arc<Mutex<State>>,
    max_tunnels: usize,
    client_tunnels: usize,
    opens_per_minute: f64,
}

/// Held while a tunnel is open.
pub struct Ticket {
    state: Arc<Mutex<State>>,
    key: ClientKey,
}

/// Forgotten clients are kept below this many entries.
const CLIENTS_KEPT: usize = 65_536;

impl Admission {
    pub fn new(config: &Config) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                clients: HashMap::new(),
                tunnels: 0,
            })),
            max_tunnels: config.max_tunnels,
            client_tunnels: config.client_tunnels,
            opens_per_minute: f64::from(config.client_opens_per_minute),
        }
    }

    pub fn admit(&self, key: ClientKey, now: Instant) -> Result<Ticket, Refusal> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.tunnels >= self.max_tunnels {
            return Err(Refusal::Busy);
        }
        if state.clients.len() >= CLIENTS_KEPT && !state.clients.contains_key(&key) {
            let full = self.opens_per_minute;
            let rate = full / 60.0;
            state.clients.retain(|_, client| {
                client.tunnels > 0
                    || client.opens + rate * now.duration_since(client.updated).as_secs_f64() < full
            });
            if state.clients.len() >= CLIENTS_KEPT {
                return Err(Refusal::Busy);
            }
        }
        let client = state.clients.entry(key).or_insert(Client {
            tunnels: 0,
            opens: self.opens_per_minute,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(client.updated).as_secs_f64();
        client.opens =
            (client.opens + elapsed * self.opens_per_minute / 60.0).min(self.opens_per_minute);
        client.updated = now;
        if client.tunnels >= self.client_tunnels {
            return Err(Refusal::Busy);
        }
        if client.opens < 1.0 {
            return Err(Refusal::RateLimited);
        }
        client.opens -= 1.0;
        client.tunnels += 1;
        state.tunnels += 1;
        Ok(Ticket {
            state: self.state.clone(),
            key,
        })
    }

    /// Open tunnels on the relay.
    pub fn open(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tunnels
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.tunnels = state.tunnels.saturating_sub(1);
        if let Some(client) = state.clients.get_mut(&self.key) {
            client.tunnels = client.tunnels.saturating_sub(1);
        }
    }
}

/// One direction of one tunnel: a token bucket that delays, never drops.
pub struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            rate: f64::from(rate),
            burst: f64::from(burst),
            tokens: f64::from(burst),
            updated: Instant::now(),
        }
    }

    /// Take `bytes`; how long to wait before the next transfer (the bucket may
    /// go into debt by one transfer).
    pub fn take(&mut self, bytes: usize, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.updated = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst) - bytes as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }

    pub async fn spend(&mut self, bytes: usize) {
        let wait = self.take(bytes, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(max: usize, per_client: usize, per_minute: u32) -> Admission {
        Admission::new(&Config {
            max_tunnels: max,
            client_tunnels: per_client,
            client_opens_per_minute: per_minute,
            ..Config::default()
        })
    }

    #[test]
    fn counts_tunnels_per_client_and_relay() {
        let admission = admission(3, 2, 100);
        let now = Instant::now();
        let a = ClientKey::of("198.51.100.1".parse().expect("ip"));
        let b = ClientKey::of("2a01:4f8::1".parse().expect("ip"));
        // Another address in the same IPv6 /64 is the same client.
        let b2 = ClientKey::of("2a01:4f8::ffff:2".parse().expect("ip"));
        assert_eq!(b, b2);
        let first = admission.admit(a, now).expect("first");
        let _second = admission.admit(a, now).expect("second");
        assert_eq!(admission.admit(a, now).err(), Some(Refusal::Busy));
        let _third = admission.admit(b, now).expect("other client");
        assert_eq!(admission.admit(b2, now).err(), Some(Refusal::Busy));
        assert_eq!(admission.open(), 3);
        drop(first);
        assert_eq!(admission.open(), 2);
        let _again = admission.admit(a, now).expect("a tunnel closed");
    }

    #[test]
    fn rate_limits_new_tunnels_and_refills() {
        let admission = admission(100, 100, 2);
        let start = Instant::now();
        let a = ClientKey::of("198.51.100.1".parse().expect("ip"));
        drop(admission.admit(a, start).expect("1"));
        drop(admission.admit(a, start).expect("2"));
        assert_eq!(admission.admit(a, start).err(), Some(Refusal::RateLimited));
        // Others are not affected.
        let b = ClientKey::of("198.51.100.2".parse().expect("ip"));
        drop(admission.admit(b, start).expect("b"));
        // Two per minute: one more after 30 seconds.
        let later = start + Duration::from_secs(31);
        drop(admission.admit(a, later).expect("refilled"));
        assert_eq!(admission.admit(a, later).err(), Some(Refusal::RateLimited));
    }

    #[test]
    fn buckets_slow_down_without_dropping() {
        let start = Instant::now();
        let mut bucket = Bucket::new(1000, 2000);
        bucket.updated = start;
        assert_eq!(bucket.take(1500, start), Duration::ZERO);
        assert_eq!(bucket.take(500, start), Duration::ZERO);
        let wait = bucket.take(1000, start);
        assert!((wait.as_secs_f64() - 1.0).abs() < 1e-6, "{wait:?}");
        // After the debt is paid, the rate holds.
        let later = start + Duration::from_secs(2);
        assert_eq!(bucket.take(1000, later), Duration::ZERO);
    }
}
