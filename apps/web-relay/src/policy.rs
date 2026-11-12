// SPDX-License-Identifier: EUPL-1.2
//! Where the relay may connect: never into private, loopback, link-local or
//! other special ranges (no server-side request forgery), unless an operator
//! allows a network explicitly.

use crate::config::{Config, Network, canonical};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Ranges no public Mumble server is in (IANA special-purpose registries).
const REFUSED: &[&str] = &[
    // IPv4
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.88.99.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
    // IPv6 (IPv4-mapped addresses are checked as IPv4)
    "::/96",
    "64:ff9b::/96",
    "64:ff9b:1::/48",
    "100::/64",
    "2001::/23",
    "2001:db8::/32",
    "2002::/16",
    "3fff::/20",
    "5f00::/16",
    "fc00::/7",
    "fe80::/10",
    "fec0::/10",
    "ff00::/8",
];

pub struct Policy {
    refused: Vec<Network>,
    allowed: Vec<Network>,
    denied: Vec<Network>,
}

impl Policy {
    pub fn new(config: &Config) -> Self {
        let refused = REFUSED
            .iter()
            .map(|network| network.parse().expect("built-in network"))
            .collect::<Vec<Network>>();
        Self {
            refused,
            allowed: config.allow_networks.clone(),
            denied: config.deny_networks.clone(),
        }
    }

    /// Whether the relay may connect to `ip`.
    pub fn allows(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        // Explicit operator denies may carve exclusions from an allowed LAN.
        // Operator allowances still override the BUILTIN special ranges below.
        if self.denied.iter().any(|network| network.contains(ip)) {
            return false;
        }
        if self.allowed.iter().any(|network| network.contains(ip)) {
            return true;
        }
        if self.refused.iter().any(|network| network.contains(ip)) {
            return false;
        }
        match ip {
            IpAddr::V4(v4) => !special_v4(v4),
            IpAddr::V6(v6) => !special_v6(v6),
        }
    }
}

/// What the standard library also knows to be special (a second opinion).
fn special_v4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
}

fn special_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || ip.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str]) -> Policy {
        Policy::new(&Config {
            allow_networks: allow.iter().map(|n| n.parse().expect("network")).collect(),
            deny_networks: vec!["198.100.0.0/16".parse().expect("network")],
            ..Config::default()
        })
    }

    #[test]
    fn refuses_private_and_special_ranges() {
        let policy = policy(&[]);
        for refused in [
            "127.0.0.1",
            "127.255.0.9",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.10",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.251",
            "198.18.0.1",
            "198.100.1.1",
            "::1",
            "::",
            "fe80::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::5",
            "2002:c0a8:101::1",
            "2001:0:4136:e378::1",
        ] {
            assert!(!policy.allows(refused.parse().expect("ip")), "{refused}");
        }
        for allowed in [
            "1.1.1.1",
            "93.184.215.14",
            "172.32.0.1",
            "2a01:4f8::1",
            "::ffff:93.184.215.14",
        ] {
            assert!(policy.allows(allowed.parse().expect("ip")), "{allowed}");
        }
    }

    #[test]
    fn operators_allow_networks() {
        let policy = policy(&["127.0.0.0/8", "192.168.1.0/24"]);
        assert!(policy.allows("127.0.0.1".parse().expect("ip")));
        assert!(policy.allows("::ffff:127.0.0.1".parse().expect("ip")));
        assert!(policy.allows("192.168.1.2".parse().expect("ip")));
        assert!(!policy.allows("192.168.2.2".parse().expect("ip")));
        assert!(!policy.allows("::1".parse().expect("ip")));
    }

    #[test]
    fn explicit_denies_override_allowed_v4_subranges_and_mapped_addresses() {
        let policy = Policy::new(&Config {
            allow_networks: vec![
                "192.168.1.0/24".parse().unwrap(),
                "127.0.0.0/8".parse().unwrap(),
            ],
            deny_networks: vec![
                "192.168.1.128/25".parse().unwrap(),
                "127.0.0.55/32".parse().unwrap(),
            ],
            ..Config::default()
        });
        for allowed in [
            "192.168.1.0",
            "192.168.1.127",
            "::ffff:192.168.1.127",
            "127.0.0.53",
            "127.0.0.56",
        ] {
            assert!(policy.allows(allowed.parse().unwrap()), "{allowed}");
        }
        for denied in [
            "192.168.1.128",
            "192.168.1.255",
            "::ffff:192.168.1.128",
            "::ffff:192.168.1.255",
            "127.0.0.55",
            "::ffff:127.0.0.55",
            "192.168.2.0",
            "::1",
        ] {
            assert!(!policy.allows(denied.parse().unwrap()), "{denied}");
        }
    }

    #[test]
    fn explicit_denies_override_allowed_v6_subranges_without_widening_defaults() {
        let policy = Policy::new(&Config {
            allow_networks: vec!["fd12:3456::/32".parse().unwrap()],
            deny_networks: vec!["fd12:3456:8000::/33".parse().unwrap()],
            ..Config::default()
        });
        for allowed in ["fd12:3456::", "fd12:3456:7fff:ffff:ffff:ffff:ffff:ffff"] {
            assert!(policy.allows(allowed.parse().unwrap()), "{allowed}");
        }
        for denied in [
            "fd12:3456:8000::",
            "fd12:3456:ffff:ffff:ffff:ffff:ffff:ffff",
            "fd12:3457::1",
            "::1",
        ] {
            assert!(!policy.allows(denied.parse().unwrap()), "{denied}");
        }
    }

    #[test]
    fn explicit_deny_also_wins_broad_allowances_at_exact_boundaries() {
        let policy = Policy::new(&Config {
            allow_networks: vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()],
            deny_networks: vec![
                "198.100.0.0/16".parse().unwrap(),
                "2001:db8::/32".parse().unwrap(),
            ],
            ..Config::default()
        });
        for allowed in [
            "198.99.255.255",
            "198.101.0.0",
            "2001:db7:ffff:ffff:ffff:ffff:ffff:ffff",
            "2001:db9::",
        ] {
            assert!(policy.allows(allowed.parse().unwrap()), "{allowed}");
        }
        for denied in [
            "198.100.0.0",
            "198.100.255.255",
            "::ffff:198.100.0.0",
            "2001:db8::",
            "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            assert!(!policy.allows(denied.parse().unwrap()), "{denied}");
        }
    }
}
