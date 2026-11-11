// SPDX-License-Identifier: EUPL-1.2
//! The relay's settings (`nuschl-web-relay.toml`). Every limit fails only the
//! tunnel that reaches it, never the relay (docs/web.md, "Limits").

use serde::Deserialize;
use std::{net::IpAddr, ops::RangeInclusive, path::PathBuf, str::FromStr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listen {
    Tcp(std::net::SocketAddr),
    /// A Unix socket for the web server in front (`unix:/run/…/relay.sock`):
    /// only processes allowed into its directory reach it, so the client
    /// address the proxy reports can be trusted.
    Unix(PathBuf),
}

impl FromStr for Listen {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        match text.strip_prefix("unix:") {
            Some(path) if path.starts_with('/') => Ok(Self::Unix(path.into())),
            Some(_) => Err("a unix: listen path must be absolute".into()),
            None => text
                .parse()
                .map(Self::Tcp)
                .map_err(|_| format!("invalid listen address {text:?}")),
        }
    }
}

/// An IP network (`192.0.2.0/24`, `2001:db8::/32`, or one address).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    pub address: IpAddr,
    pub prefix: u8,
}

impl Network {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.address, canonical(ip)) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                prefix_matches(&network.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                prefix_matches(&network.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) as the IPv4 address.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

fn prefix_matches(network: &[u8], ip: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    let rest = prefix % 8;
    if network[..full] != ip[..full] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xff_u8 << (8 - rest);
    network[full] & mask == ip[full] & mask
}

impl FromStr for Network {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let invalid = || format!("invalid network {text:?}");
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let address = canonical(address.parse::<IpAddr>().map_err(|_| invalid())?);
        let bits = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_| invalid())?,
            None => bits,
        };
        if prefix > bits {
            return Err(invalid());
        }
        Ok(Self { address, prefix })
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Ports (`64738`, `64738-64799`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ports(pub RangeInclusive<u16>);

impl FromStr for Ports {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let invalid = || format!("invalid port range {text:?}");
        let (low, high) = text.split_once('-').unwrap_or((text, text));
        let low = low.trim().parse::<u16>().map_err(|_| invalid())?;
        let high = high.trim().parse::<u16>().map_err(|_| invalid())?;
        if low == 0 || low > high {
            return Err(invalid());
        }
        Ok(Self(low..=high))
    }
}

impl<'de> Deserialize<'de> for Ports {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Number(u16),
            Text(String),
        }
        match Either::deserialize(deserializer)? {
            Either::Number(port) => port.to_string(),
            Either::Text(text) => text,
        }
        .parse()
        .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Listen {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: Listen,
    /// Destination ports the relay connects to. Mumble's default port and the
    /// ones next to it unless configured.
    pub ports: Vec<Ports>,
    /// Networks allowed although they are in a refused range (a LAN server,
    /// tests). Never needed for public servers.
    pub allow_networks: Vec<Network>,
    /// Networks refused in addition to the private and special ranges.
    pub deny_networks: Vec<Network>,
    /// Origins (`https://web.example.org`) whose pages may open tunnels; empty
    /// allows any.
    pub allowed_origins: Vec<String>,
    /// TCP peers whose `X-Real-IP` header names the client (the web server in
    /// front). A Unix-socket peer is always trusted.
    pub trusted_proxies: Vec<Network>,
    /// Refuse destinations that do not answer a Mumble UDP ping, instead of
    /// accepting a TLS handshake as the proof.
    pub require_ping: bool,
    /// Open tunnels on the relay; beyond it new tunnels are refused as busy.
    pub max_tunnels: usize,
    /// Open tunnels per client address (an IPv4 address or an IPv6 /64).
    pub client_tunnels: usize,
    /// New tunnels per client address and minute (the burst is the same).
    pub client_opens_per_minute: u32,
    /// Each tunnel's sustained rate per direction; a tunnel above it is slowed,
    /// not closed.
    pub bytes_per_second: u32,
    /// What a tunnel may send at once above its rate, per direction.
    pub burst_bytes: u32,
    /// A tunnel in which nothing moves for this long is closed.
    pub idle_timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: Listen::Tcp(([127, 0, 0, 1], 8090).into()),
            ports: vec![Ports(64738..=64799)],
            allow_networks: Vec::new(),
            deny_networks: Vec::new(),
            allowed_origins: Vec::new(),
            trusted_proxies: vec![
                "127.0.0.1".parse().expect("loopback"),
                "::1".parse().expect("loopback"),
            ],
            require_ping: false,
            max_tunnels: 4096,
            client_tunnels: 16,
            client_opens_per_minute: 30,
            bytes_per_second: 512 * 1024,
            burst_bytes: 4 * 1024 * 1024,
            idle_timeout_seconds: 120,
        }
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let config: Self =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        config.check()?;
        Ok(config)
    }

    pub fn check(&self) -> Result<(), String> {
        if self.ports.is_empty() {
            return Err("ports must name at least one port".into());
        }
        if self.max_tunnels == 0 || self.client_tunnels == 0 || self.client_opens_per_minute == 0 {
            return Err("tunnel limits must be at least 1".into());
        }
        if self.bytes_per_second < 16 * 1024 || self.burst_bytes < 128 * 1024 {
            return Err(
                "bytes_per_second must be at least 16384 and burst_bytes at least 131072".into(),
            );
        }
        if self.idle_timeout_seconds < 30 {
            return Err(
                "idle_timeout_seconds must be at least 30 (Mumble pings every 5–15 s)".into(),
            );
        }
        Ok(())
    }

    pub fn port_allowed(&self, port: u16) -> bool {
        self.ports.iter().any(|ports| ports.0.contains(&port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings() {
        let config: Config = toml::from_str(
            r#"
            listen = "unix:/run/nuschl-web-relay/relay.sock"
            ports = [64738, "10000-10010"]
            allow_networks = ["192.168.1.0/24", "fd00::1"]
            allowed_origins = ["https://web.example.org"]
            require_ping = true
            "#,
        )
        .expect("valid");
        config.check().expect("checked");
        assert_eq!(
            config.listen,
            Listen::Unix("/run/nuschl-web-relay/relay.sock".into())
        );
        assert!(config.port_allowed(64738) && config.port_allowed(10005));
        assert!(!config.port_allowed(64739) && !config.port_allowed(22));
        assert!(config.allow_networks[0].contains("192.168.1.77".parse().expect("ip")));
        assert!(config.allow_networks[0].contains("::ffff:192.168.1.77".parse().expect("ip")));
        assert!(!config.allow_networks[0].contains("192.168.2.1".parse().expect("ip")));
        assert!(config.allow_networks[1].contains("fd00::1".parse().expect("ip")));
        assert!(!config.allow_networks[1].contains("fd00::2".parse().expect("ip")));
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
        assert!("10.0.0.0/33".parse::<Network>().is_err());
        assert!("0-5".parse::<Ports>().is_err());
        assert!("9-5".parse::<Ports>().is_err());
        assert!("relay.sock".parse::<Listen>().is_err());
        assert!("unix:relay.sock".parse::<Listen>().is_err());
        Config::default().check().expect("defaults are valid");
    }
}
