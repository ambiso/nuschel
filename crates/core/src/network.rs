// SPDX-License-Identifier: EUPL-1.2
//! Connection policy, applied before gathering peer addresses or opening sockets.
//! Servers and relays are chosen locally; a peer's advertised address is never
//! a configured server. Future RTC adapters must apply this before ICE gathering.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    ConfiguredServer,
    ConfiguredRelay,
    DirectPeer,
    PeerDiscovery,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerPolicy {
    pub relay_only: bool,
}

impl PeerPolicy {
    pub const fn permits(self, destination: Destination) -> bool {
        match destination {
            Destination::ConfiguredServer | Destination::ConfiguredRelay => true,
            Destination::DirectPeer | Destination::PeerDiscovery => !self.relay_only,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_only_preserves_servers_but_excludes_discovery_and_direct_peers() {
        let policy = PeerPolicy { relay_only: true };
        assert!(policy.permits(Destination::ConfiguredServer));
        assert!(policy.permits(Destination::ConfiguredRelay));
        assert!(!policy.permits(Destination::DirectPeer));
        assert!(!policy.permits(Destination::PeerDiscovery));
        assert!(PeerPolicy::default().permits(Destination::DirectPeer));
    }
}
