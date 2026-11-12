// SPDX-License-Identifier: EUPL-1.2
//! The voice bitrate each server gets, for a UI to show
//! ([`nuschl_core::bitrate`]). Enforcement is the driver's: each connection
//! admits frames against its own effective target, and the one encoder runs
//! at the lowest target among the servers that receive the microphone. This
//! mirrors that from the client's state, so every frontend explains the same
//! numbers with the same reasons.
use crate::{
    Client,
    server::{ServerState, Status},
};
use nuschl_core::{audio::DEFAULT_SERVER_BANDWIDTH, bitrate::VoiceBitrate};

/// The per-user budget a connection plans for: the server's advertised one,
/// or Mumble's conservative default until it says.
pub fn budget(server: &ServerState) -> u32 {
    server
        .limits
        .as_ref()
        .map_or(DEFAULT_SERVER_BANDWIDTH, |limits| limits.bandwidth())
}

/// The microphone reaches this server (the driver's `speaking_to`).
fn receives_microphone(server: &ServerState, requested: u32) -> bool {
    server.status == Status::Connected
        && server.has_mumble_voice()
        && !server.muted
        && !server.deafened
        && VoiceBitrate::resolve(requested, budget(server)).transmits()
}

/// What `servers[index]` gets of a `requested` target: its own effective
/// rate and reason, lowered with [`BitrateLimit::SharedMicrophone`] while
/// another server that also receives the microphone allows less.
///
/// [`BitrateLimit::SharedMicrophone`]: nuschl_core::bitrate::BitrateLimit::SharedMicrophone
pub fn for_server(servers: &[ServerState], index: usize, requested: u32) -> Option<VoiceBitrate> {
    let server = servers.get(index)?;
    let own = VoiceBitrate::resolve(requested, budget(server));
    if !receives_microphone(server, requested) {
        return Some(own);
    }
    let encoder = servers
        .iter()
        .filter(|other| receives_microphone(other, requested))
        .map(|other| VoiceBitrate::resolve(requested, budget(other)).effective)
        .min();
    Some(encoder.map_or(own, |encoder| own.shared(encoder)))
}

impl Client {
    /// [`for_server`] over this client's servers.
    pub fn voice_bitrate(&self, index: usize, requested: u32) -> Option<VoiceBitrate> {
        for_server(&self.servers, index, requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_core::{bitrate::BitrateLimit, bookmark::ServerBookmark, model::ServerLimits};

    fn server(name: &str, budget: Option<u32>, speaking: bool) -> ServerState {
        let mut server =
            ServerState::new(ServerBookmark::new(name, "localhost", 64738, "me").unwrap());
        server.status = Status::Connected;
        server.muted = !speaking;
        server.deafened = false;
        server.limits = budget.map(|budget| ServerLimits {
            max_bandwidth: Some(budget),
            ..Default::default()
        });
        server
    }

    #[test]
    fn each_server_explains_its_own_budget_and_the_shared_encoder() {
        let servers = [
            server("nuschld", Some(558_000), true),
            server("stock", Some(72_000), true),
            server("listening", Some(40_000), false),
        ];
        let wide = for_server(&servers, 0, 256_000).unwrap();
        // Both receive the microphone: one encoder at the stock server's 48k.
        assert_eq!(
            (wide.requested, wide.effective, wide.limit),
            (256_000, 48_000, BitrateLimit::SharedMicrophone)
        );
        let stock = for_server(&servers, 1, 256_000).unwrap();
        assert_eq!(
            (stock.effective, stock.limit),
            (48_000, BitrateLimit::ServerBudget)
        );
        // A muted server shows what its own budget allows.
        let listening = for_server(&servers, 2, 256_000).unwrap();
        assert_eq!(
            (listening.effective, listening.limit),
            (16_000, BitrateLimit::ServerBudget)
        );
        assert!(for_server(&servers, 3, 256_000).is_none());
    }

    #[test]
    fn a_server_too_small_for_voice_neither_sends_nor_lowers_the_others() {
        let servers = [
            server("nuschld", Some(558_000), true),
            server("tiny", Some(29_999), true),
        ];
        assert_eq!(
            for_server(&servers, 0, 128_000).unwrap().limit,
            BitrateLimit::Requested
        );
        let tiny = for_server(&servers, 1, 128_000).unwrap();
        assert_eq!(
            (tiny.effective, tiny.limit),
            (0, BitrateLimit::NoServerBudget)
        );
        // Until a server reports its budget: Mumble's conservative default.
        let unreported = [server("new", None, true)];
        assert_eq!(
            for_server(&unreported, 0, 128_000).unwrap().effective,
            48_000
        );
        // An end-to-end server gets no plain voice and lowers nobody.
        let mut encrypted = server("e2ee", Some(72_000), true);
        encrypted.capabilities.push("e2ee/1".into());
        let servers = [server("nuschld", Some(558_000), true), encrypted];
        assert_eq!(for_server(&servers, 0, 192_000).unwrap().effective, 192_000);
    }
}
