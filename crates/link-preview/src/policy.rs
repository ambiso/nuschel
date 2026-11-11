// SPDX-License-Identifier: EUPL-1.2
//! Explicit network authority. Page-controlled targets and operator-configured
//! services are different types; an image cannot become a trusted DNS/proxy URI.
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use thiserror::Error;
use url::{Host, Url};

pub const MAX_URL_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Off,
    Direct,
    ProxyRequired,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub mode: Mode,
    /// An explicit SOCKS5 IP:port, never an ambient environment proxy.
    pub proxy: Option<String>,
    /// No resolver is supplied implicitly on the user's behalf.
    pub dns: Option<DnsSettings>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsSettings {
    /// A configured DNS-over-HTTPS JSON endpoint. Its bootstrap addresses are
    /// explicit; reaching it must not need a system DNS lookup.
    pub url: String,
    pub bootstrap: Vec<IpAddr>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("link previews are off")]
    Off,
    #[error("configure a proxy before requesting a preview")]
    MissingProxy,
    #[error("configure proxied DNS and bootstrap addresses before requesting a preview")]
    MissingDns,
    #[error("invalid or unsupported proxy; use an explicit SOCKS5 IP address and port")]
    Proxy,
    #[error("invalid proxied DNS configuration")]
    Dns,
    #[error("invalid or unsupported preview URL")]
    Url,
    #[error("private, local, or special-purpose preview addresses are refused")]
    Address,
    #[error("DNS returned no usable public addresses")]
    NoAddresses,
}

/// The user-visible URL retains its fragment; HTTP requests never send it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    url: Url,
}

impl Target {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let url = checked_url(text)?;
        match url.host().ok_or(Error::Url)? {
            Host::Ipv4(ip) => {
                if !public_ip(ip.into()) {
                    return Err(Error::Address);
                }
            }
            Host::Ipv6(ip) => {
                if !public_ip(ip.into()) {
                    return Err(Error::Address);
                }
            }
            Host::Domain(domain) => {
                let domain = domain.trim_end_matches('.');
                if !domain.contains('.')
                    || [
                        "localhost",
                        "local",
                        "internal",
                        "home.arpa",
                        "invalid",
                        "test",
                    ]
                    .iter()
                    .any(|suffix| domain == *suffix || domain.ends_with(&format!(".{suffix}")))
                {
                    return Err(Error::Address);
                }
                if domain.len() > 253
                    || domain.split('.').any(|label| {
                        label.is_empty()
                            || label.len() > 63
                            || label.starts_with('-')
                            || label.ends_with('-')
                            || !label
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    })
                {
                    return Err(Error::Url);
                }
            }
        }
        Ok(Self { url })
    }
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }
    pub fn host(&self) -> &str {
        self.url.host_str().expect("validated URL")
    }
    pub fn port(&self) -> u16 {
        self.url.port_or_known_default().expect("HTTP URL")
    }
    pub fn request_url(&self) -> Url {
        let mut url = self.url.clone();
        url.set_fragment(None);
        url
    }
    pub fn join(&self, reference: &str) -> Result<Self, Error> {
        if reference.chars().any(char::is_control) || reference.len() > MAX_URL_BYTES {
            return Err(Error::Url);
        }
        let joined = self.url.join(reference).map_err(|_| Error::Url)?;
        Self::parse(joined.as_str())
    }
}

fn checked_url(text: &str) -> Result<Url, Error> {
    if text.is_empty() || text.len() > MAX_URL_BYTES || text.chars().any(char::is_control) {
        return Err(Error::Url);
    }
    let mut url = Url::parse(text).map_err(|_| Error::Url)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
    {
        return Err(Error::Url);
    }
    if !matches!(url.port_or_known_default(), Some(80 | 443)) {
        return Err(Error::Url);
    }
    if let Some(Host::Domain(host)) = url.host() {
        let host = host.trim_end_matches('.').to_owned();
        url.set_host(Some(&host)).map_err(|_| Error::Url)?;
    }
    Ok(url)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proxy {
    address: SocketAddr,
}

impl Proxy {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let url = Url::parse(text).map_err(|_| Error::Proxy)?;
        if url.scheme() != "socks5"
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Proxy);
        }
        let ip = match url.host().ok_or(Error::Proxy)? {
            Host::Ipv4(ip) => ip.into(),
            Host::Ipv6(ip) => ip.into(),
            Host::Domain(_) => return Err(Error::Proxy),
        };
        let port = url.port().filter(|port| *port != 0).ok_or(Error::Proxy)?;
        if !service_ip(ip) {
            return Err(Error::Proxy);
        }
        Ok(Self {
            address: SocketAddr::new(ip, port),
        })
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxiedDns {
    url: Url,
    bootstrap: Vec<IpAddr>,
}

impl ProxiedDns {
    pub fn parse(settings: &DnsSettings) -> Result<Self, Error> {
        let url = checked_url(&settings.url).map_err(|_| Error::Dns)?;
        if url.scheme() != "https"
            || url.fragment().is_some()
            || settings.bootstrap.is_empty()
            || settings.bootstrap.len() > 4
            || settings.bootstrap.iter().any(|ip| !service_ip(*ip))
        {
            return Err(Error::Dns);
        }
        Ok(Self {
            url,
            bootstrap: settings.bootstrap.clone(),
        })
    }
    pub fn url(&self) -> &Url {
        &self.url
    }
    pub fn bootstrap(&self) -> &[IpAddr] {
        &self.bootstrap
    }
}

/// Constructing this value performs no DNS, socket, worker or environment I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Direct,
    Proxy { proxy: Proxy, dns: ProxiedDns },
}

impl Settings {
    pub fn route(&self) -> Result<Route, Error> {
        match self.mode {
            Mode::Off => Err(Error::Off),
            Mode::Direct => Ok(Route::Direct),
            Mode::ProxyRequired => {
                let proxy = Proxy::parse(self.proxy.as_deref().ok_or(Error::MissingProxy)?)?;
                let dns = ProxiedDns::parse(self.dns.as_ref().ok_or(Error::MissingDns)?)?;
                Ok(Route::Proxy { proxy, dns })
            }
        }
    }
}

/// Pin a bounded DNS result before connecting; never resolve the hostname again
/// while opening the socket/tunnel. Reject a mixed public/private answer too.
pub fn validate_addresses(addresses: &[IpAddr]) -> Result<Vec<IpAddr>, Error> {
    if addresses.is_empty() || addresses.len() > 16 {
        return Err(Error::NoAddresses);
    }
    if addresses.iter().any(|ip| !public_ip(*ip)) {
        return Err(Error::Address);
    }
    let mut pinned = Vec::new();
    for ip in addresses {
        if !pinned.contains(ip) {
            pinned.push(*ip);
        }
    }
    Ok(pinned)
}

fn service_ip(ip: IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast()
}

// Same audited special-range baseline as the web relay. The preview target
// policy has no operator allowlist that page-controlled metadata can manipulate.
const REFUSED: &[&str] = &[
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

pub fn public_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    };
    !REFUSED.iter().any(|network| {
        network
            .parse::<IpNet>()
            .expect("built-in network")
            .contains(&ip)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_never_fall_back_from_off_or_an_incomplete_proxy() {
        assert_eq!(Settings::default().route(), Err(Error::Off));
        let mut settings = Settings {
            mode: Mode::ProxyRequired,
            ..Default::default()
        };
        assert_eq!(settings.route(), Err(Error::MissingProxy));
        settings.proxy = Some("socks5://127.0.0.1:9050".into());
        assert_eq!(settings.route(), Err(Error::MissingDns));
        settings.dns = Some(DnsSettings {
            url: "https://resolver.example/dns-query".into(),
            bootstrap: vec!["192.168.1.2".parse().unwrap()],
        });
        assert!(matches!(settings.route(), Ok(Route::Proxy { .. })));
        settings.mode = Mode::Direct;
        assert_eq!(settings.route(), Ok(Route::Direct));
    }
    #[test]
    fn numeric_and_redirect_tricks_do_not_reach_local_addresses() {
        for input in [
            "http://127.1/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost./",
            "http://host.local/",
            "http://service.home.arpa/",
            "file:///etc/passwd",
            "http://user:password@example.org/",
            "https://example.org:22/",
            "https://exam\nple.org/",
        ] {
            assert!(Target::parse(input).is_err(), "{input}");
        }
        let target = Target::parse("https://Example.ORG/path#section").unwrap();
        assert_eq!(target.host(), "example.org");
        assert_eq!(target.request_url().as_str(), "https://example.org/path");
        assert!(target.join("//127.0.0.1/private").is_err());
        assert!(target.join("data:image/png;base64,AAAA").is_err());
        assert_eq!(
            target.join("../image.png").unwrap().as_str(),
            "https://example.org/image.png"
        );
    }
    #[test]
    fn resolved_private_or_rebound_mixed_answers_are_rejected() {
        for denied in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "::1",
            "::ffff:192.168.1.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "ff02::1",
        ] {
            assert!(!public_ip(denied.parse().unwrap()), "{denied}");
        }
        let public = "93.184.215.14".parse().unwrap();
        assert_eq!(validate_addresses(&[public, public]).unwrap(), vec![public]);
        assert_eq!(
            validate_addresses(&[public, "127.0.0.1".parse().unwrap()]),
            Err(Error::Address)
        );
        assert!(validate_addresses(&[]).is_err());
    }
    #[test]
    fn proxy_and_dns_services_need_explicit_addresses_and_no_credentials() {
        assert!(Proxy::parse("socks5://127.0.0.1:9050").is_ok());
        for proxy in [
            "socks5h://127.0.0.1:9050",
            "socks5://proxy.example:9050",
            "http://127.0.0.1:9050",
            "socks5://user:password@127.0.0.1:9050",
            "socks5://0.0.0.0:9050",
        ] {
            assert!(Proxy::parse(proxy).is_err(), "{proxy}");
        }
    }
}
