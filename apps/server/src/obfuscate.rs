// SPDX-License-Identifier: EUPL-1.2
//! Client addresses in logs (murmur.ini `obfuscate`). With `obfuscate = true`
//! every logged client address is an HMAC-SHA256 under a key drawn at startup
//! and kept only in memory: the same address gives the same token for the
//! life of the process, so one client's lines can be followed and an abusive
//! address correlated, but logs cannot be mapped back to addresses, even by
//! trying all 2³² IPv4 addresses, and tokens differ after a restart. Murmur
//! hashes with SHA-1 under a 32-bit random number, which a search over the key
//! and the IPv4 space undoes. IPv4 and its IPv4-mapped IPv6 form give the same
//! token. The server's own listening addresses are logged as configured.
use ring::hmac;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

#[derive(Clone, Default)]
pub struct Addresses(Option<Arc<hmac::Key>>);

impl fmt::Debug for Addresses {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(if self.0.is_some() {
            "Addresses(obfuscated)"
        } else {
            "Addresses(plain)"
        })
    }
}

impl Addresses {
    pub fn new(obfuscate: bool) -> Self {
        Self(obfuscate.then(|| {
            Arc::new(hmac::Key::new(
                hmac::HMAC_SHA256,
                &crate::state::random::<32>(),
            ))
        }))
    }

    pub fn obfuscated(&self) -> bool {
        self.0.is_some()
    }

    /// An address as logs show it.
    pub fn ip(&self, ip: IpAddr) -> String {
        match &self.0 {
            None => ip.to_string(),
            Some(key) => {
                let tag = hmac::sign(key, &crate::state::mapped(ip).octets());
                format!("<<{}>>", hex::encode(&tag.as_ref()[..8]))
            }
        }
    }

    /// An address and port as logs show them.
    pub fn socket(&self, address: SocketAddr) -> String {
        match &self.0 {
            None => address.to_string(),
            Some(_) => format!("{}:{}", self.ip(address.ip()), address.port()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_hide_addresses_and_stay_stable_per_key() {
        let v4: IpAddr = "192.0.2.7".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        let plain = Addresses::new(false);
        assert_eq!(plain.ip(v4), "192.0.2.7");
        assert_eq!(
            plain.socket(SocketAddr::new(v4, 5)),
            "192.0.2.7:5".to_owned()
        );
        let hidden = Addresses::new(true);
        let token = hidden.ip(v4);
        assert!(token.starts_with("<<") && token.len() == 20, "{token}");
        assert!(!token.contains("192"));
        assert_eq!(hidden.ip(mapped), token);
        assert_ne!(hidden.ip(other), token);
        assert_eq!(
            hidden.socket(SocketAddr::new(v4, 64738)),
            format!("{token}:64738")
        );
        // A new start draws a new key.
        assert_ne!(Addresses::new(true).ip(v4), token);
        assert_eq!(format!("{hidden:?}"), "Addresses(obfuscated)");
    }
}
