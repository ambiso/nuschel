// SPDX-License-Identifier: EUPL-1.2
//! History pages per network (docs/extension.md §6, review D2/D13): besides
//! each connection's own buckets, pages and their bytes are counted per IPv4
//! /24 or IPv6 /56 for unregistered users, and per account for registered ones,
//! so that many connections from one network cannot multiply the per-connection
//! allowance, while one busy NAT never locks out its registered users.
//!
//! The limits are generous (`history.network_pages_per_minute`,
//! `network_page_bytes_per_minute`) and only ever fail the single request, with
//! a `retry_after_ms` the client can honour; nothing is disconnected and there
//! is no global cap. `history.unlimited_networks` exempts networks (CIDR).
//!
//! Why these prefixes: IPv4 addresses are scarce and shared (NAT, CGNAT), and
//! one operator's hosts usually sit in one /24; a /32 would let anyone with a
//! handful of addresses multiply the budget. Residential IPv6 customers get a
//! /56 or /48 (RIPE-690), so a /64 would give one customer 256 or more budgets;
//! a /56 is one customer at most ISPs without lumping separate customers
//! together the way a /48 would. Admission keeps counting connections per /64.
use crate::{config::History, conversations::Id};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Instant,
};

/// Buckets kept at most; idle (full) ones are dropped first, and beyond this
/// everything is forgotten (failing open: the per-connection limits remain).
const MAX_BUCKETS: usize = 65_536;

/// Units per token: a bucket refilling `per_minute` tokens gains `per_minute`
/// units every millisecond, so integer arithmetic is exact.
const UNIT: i64 = 60_000;

/// A token bucket in integer units that may go into debt (bytes are charged
/// once a page is read, and a page larger than what is left still comes).
#[derive(Clone, Debug)]
pub struct Budget {
    per_minute: i64,
    capacity: i64,
    level: i64,
    since: Instant,
}

impl Budget {
    pub fn new(per_minute: u64, burst: u64, now: Instant) -> Self {
        let per_minute = i64::try_from(per_minute.max(1)).unwrap_or(i64::MAX / UNIT);
        let capacity = i64::try_from(burst.max(1))
            .unwrap_or(i64::MAX)
            .saturating_mul(UNIT);
        Self {
            per_minute,
            capacity,
            level: capacity,
            since: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.since).as_millis();
        let elapsed = i64::try_from(elapsed).unwrap_or(i64::MAX);
        if elapsed > 0 {
            self.since = now;
            self.level = self
                .level
                .saturating_add(elapsed.saturating_mul(self.per_minute))
                .min(self.capacity);
        }
    }

    /// Milliseconds until the level reaches `needed` units.
    fn wait(&self, needed: i64) -> u64 {
        let missing = needed.saturating_sub(self.level).max(1);
        let wait = missing / self.per_minute + i64::from(missing % self.per_minute != 0);
        u64::try_from(wait).unwrap_or(u64::MAX)
    }

    /// Take `cost` tokens now, or say how many milliseconds until they are there.
    pub fn take(&mut self, cost: u64, now: Instant) -> Result<(), u64> {
        self.refill(now);
        let needed = i64::try_from(cost)
            .unwrap_or(i64::MAX)
            .saturating_mul(UNIT)
            .min(self.capacity);
        if self.level >= needed {
            self.level -= needed;
            Ok(())
        } else {
            Err(self.wait(needed))
        }
    }

    /// Whether anything is left, or the milliseconds until something is.
    pub fn ready(&mut self, now: Instant) -> Result<(), u64> {
        self.refill(now);
        if self.level > 0 {
            Ok(())
        } else {
            Err(self.wait(1))
        }
    }

    /// Charge `cost` after the fact; the debt is at most one full bucket.
    pub fn charge(&mut self, cost: u64, now: Instant) {
        self.refill(now);
        let cost = i64::try_from(cost).unwrap_or(i64::MAX).saturating_mul(UNIT);
        self.level = self.level.saturating_sub(cost).max(-self.capacity);
    }

    fn idle(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.level >= self.capacity
    }
}

/// Whose budget a request draws from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// An unregistered user's network ([`network`]).
    Network(IpAddr),
    /// A registered user's account, wherever it connects from.
    Account(Id),
}

/// The network an address counts towards: its IPv4 /24, or its IPv6 /56
/// (IPv4-mapped addresses count as IPv4).
pub fn network(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(u32::from(v4) & !0xff)),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => network(IpAddr::V4(v4)),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !((1_u128 << 72) - 1))),
        },
    }
}

/// A network in CIDR notation (`192.0.2.0/24`, `2001:db8::/32`; a bare address
/// is a single host).
pub fn parse_cidr(text: &str) -> Option<(IpAddr, u8)> {
    let (address, length) = match text.trim().split_once('/') {
        Some((address, length)) => (address.parse::<IpAddr>().ok()?, length.parse().ok()?),
        None => {
            let address = text.trim().parse::<IpAddr>().ok()?;
            (address, if address.is_ipv4() { 32 } else { 128 })
        }
    };
    let bits = if address.is_ipv4() { 32 } else { 128 };
    (length <= bits).then_some((address, length))
}

fn contains((network, length): (IpAddr, u8), address: IpAddr) -> bool {
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        v4 => v4,
    };
    let masked = |value: u128, bits: u32| {
        let shift = bits.saturating_sub(u32::from(length));
        if shift >= 128 { 0 } else { value >> shift }
    };
    match (network, address) {
        (IpAddr::V4(net), IpAddr::V4(address)) => {
            masked(u128::from(u32::from(net)), 32) == masked(u128::from(u32::from(address)), 32)
        }
        (IpAddr::V6(net), IpAddr::V6(address)) => {
            masked(u128::from(net), 128) == masked(u128::from(address), 128)
        }
        _ => false,
    }
}

struct Buckets {
    pages: Budget,
    bytes: Budget,
}

/// The per-network (and per-account) budgets for history pages.
pub struct NetworkLimits {
    pages_per_minute: u64,
    bytes_per_minute: u64,
    unlimited: Vec<(IpAddr, u8)>,
    buckets: HashMap<Key, Buckets>,
}

impl NetworkLimits {
    pub fn new(history: &History) -> Self {
        Self {
            pages_per_minute: u64::from(history.network_pages_per_minute),
            bytes_per_minute: u64::from(history.network_page_bytes_per_minute),
            unlimited: history
                .unlimited_networks
                .iter()
                .filter_map(|text| parse_cidr(text))
                .collect(),
            buckets: HashMap::new(),
        }
    }

    /// Whose budget a connection from `address` uses; `None` if exempt.
    pub fn key(&self, address: IpAddr, account: Option<Id>) -> Option<Key> {
        if self
            .unlimited
            .iter()
            .any(|network| contains(*network, address))
        {
            return None;
        }
        Some(account.map_or_else(|| Key::Network(network(address)), Key::Account))
    }

    fn buckets(&mut self, key: Key, now: Instant) -> &mut Buckets {
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(&key) {
            self.buckets
                .retain(|_, buckets| !(buckets.pages.idle(now) && buckets.bytes.idle(now)));
            if self.buckets.len() >= MAX_BUCKETS {
                self.buckets.clear();
            }
        }
        let (pages, bytes) = (self.pages_per_minute, self.bytes_per_minute);
        self.buckets.entry(key).or_insert_with(|| Buckets {
            // A burst of a third of a minute's pages; a full minute of bytes.
            pages: Budget::new(pages, pages.div_ceil(3).max(1), now),
            bytes: Budget::new(bytes, bytes, now),
        })
    }

    /// Take one page from `key`'s budget: `Err` with the milliseconds to wait.
    pub fn page(&mut self, key: Key, now: Instant) -> Result<(), u64> {
        let buckets = self.buckets(key, now);
        buckets.bytes.ready(now)?;
        buckets.pages.take(1, now)
    }

    /// A page of `bytes` was read for `key`.
    pub fn spend(&mut self, key: Key, bytes: u64, now: Instant) {
        if let Some(buckets) = self.buckets.get_mut(&key) {
            buckets.bytes.charge(bytes, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn networks_are_ipv4_slash_24_and_ipv6_slash_56() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        assert_eq!(network(ip("192.0.2.77")), ip("192.0.2.0"));
        assert_eq!(network(ip("::ffff:192.0.2.77")), ip("192.0.2.0"));
        assert_eq!(
            network(ip("2001:db8:1:2ff:aaaa::1")),
            ip("2001:db8:1:200::")
        );
        assert_eq!(parse_cidr("10.0.0.0/8"), Some((ip("10.0.0.0"), 8)));
        assert_eq!(parse_cidr("::1"), Some((ip("::1"), 128)));
        assert_eq!(parse_cidr("10.0.0.0/33"), None);
        assert_eq!(parse_cidr("nonsense"), None);
        assert!(contains((ip("10.0.0.0"), 8), ip("10.200.3.4")));
        assert!(contains((ip("127.0.0.0"), 8), ip("::ffff:127.0.0.1")));
        assert!(!contains((ip("10.0.0.0"), 8), ip("11.0.0.1")));
        assert!(contains((ip("::"), 0), ip("2001:db8::1")));
        assert!(!contains((ip("::1"), 128), ip("127.0.0.1")));
    }

    #[test]
    fn budgets_fail_the_request_and_say_when_to_retry() {
        let start = Instant::now();
        let mut budget = Budget::new(60, 2, start);
        assert_eq!(budget.take(1, start), Ok(()));
        assert_eq!(budget.take(1, start), Ok(()));
        assert_eq!(budget.take(1, start), Err(1000), "one a second");
        assert_eq!(budget.take(1, start + Duration::from_millis(400)), Err(600));
        assert_eq!(budget.take(1, start + Duration::from_secs(1)), Ok(()));
        // Bytes may go into debt; the next request waits until it is repaid.
        let mut bytes = Budget::new(6_000, 1_000, start);
        assert_eq!(bytes.ready(start), Ok(()));
        bytes.charge(1_500, start);
        assert_eq!(bytes.ready(start), Err(5_001));
        assert_eq!(bytes.ready(start + Duration::from_millis(5_001)), Ok(()));
    }

    #[test]
    fn registered_users_have_their_own_budget_and_exempt_networks_none() {
        let history = History {
            network_pages_per_minute: 3,
            unlimited_networks: vec!["10.1.0.0/16".into()],
            ..History::default()
        };
        let mut limits = NetworkLimits::new(&history);
        let now = Instant::now();
        let guest = limits.key("192.0.2.1".parse().unwrap(), None).unwrap();
        let neighbour = limits.key("192.0.2.200".parse().unwrap(), None).unwrap();
        assert_eq!(guest, neighbour, "one /24");
        assert_eq!(limits.page(guest, now), Ok(()));
        assert!(limits.page(neighbour, now).is_err(), "a burst of one");
        let account = limits.key("192.0.2.1".parse().unwrap(), Some([7; 16]));
        assert_eq!(limits.page(account.unwrap(), now), Ok(()));
        assert_eq!(limits.key("10.1.2.3".parse().unwrap(), None), None);
    }
}
