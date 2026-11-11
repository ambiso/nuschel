// SPDX-License-Identifier: EUPL-1.2
//! A harness for hostile control and voice input: a state with a channel tree,
//! a SuperUser, registered and anonymous users (including a legacy client), fed
//! frames as if they came from those connections. Used by the libFuzzer target
//! in `fuzz/` and by a deterministic test on stable Rust. No network, no runtime.
use crate::{
    auth::new_user,
    config::Config,
    routing::Datagrams,
    state::{ClientInfo, Counters, Link, Peer, Registered, State},
    storage::StorageHandle,
};
use bytes::Bytes;
use nuschel_protocol::{ControlFrame, control as proto, crypt::CryptState};
use std::{net::SocketAddr, sync::Arc, time::Instant};

pub struct Harness {
    pub state: State,
    sessions: Vec<u32>,
    out: Datagrams,
}

impl Harness {
    pub fn new() -> Self {
        let config = Config {
            message_limit: 1000,
            message_burst: 1000,
            listeners_per_user: Some(4),
            ..Config::default()
        };
        let settings = Arc::new(
            config
                .settings()
                .unwrap_or_else(|_| unreachable!("default settings are valid")),
        );
        let mut state = State::new(
            settings,
            StorageHandle::detached(),
            Arc::new(Counters::default()),
        );
        state.initialise_root();
        state.registry.insert(Registered {
            id: 1,
            name: "Registered".into(),
            password_hash: None,
            certificate_hash: Some("ab".repeat(20)),
            last_channel: None,
            last_active: None,
        });
        let mut harness = Self {
            state,
            sessions: Vec::new(),
            out: Datagrams::default(),
        };
        for (name, user_id, hash, version) in [
            ("SuperUser", Some(0), None, 5),
            ("Registered", Some(1), Some("ab".repeat(20)), 5),
            ("Anonymous", None, Some("cd".repeat(20)), 5),
            ("Legacy", None, None, 3),
        ] {
            harness.add_user(name, user_id, hash, version);
        }
        harness
    }

    fn add_user(&mut self, name: &str, user_id: Option<u32>, hash: Option<String>, minor: u64) {
        let Some(session) = self.state.allocate_session() else {
            return;
        };
        let (link, _frames, _queued) = Link::new(1 << 20);
        let peer = Peer {
            address: SocketAddr::from(([10, 0, 0, session as u8], 50_000)),
            certificates: Vec::new(),
            certificate_hash: hash,
            negotiation: Default::default(),
            info: ClientInfo {
                version: (1 << 48) | (minor << 32),
                ..Default::default()
            },
            link,
        };
        let (crypt, _) = CryptState::generate_server([session as u8; 48]);
        let config = self.state.settings.config.clone();
        let user = new_user(
            &config,
            session,
            name.into(),
            user_id,
            &peer,
            &proto::Authenticate::default(),
            crypt,
            Instant::now(),
        );
        self.state.users.insert(session, user);
        if let Some(root) = self.state.channels.get_mut(0) {
            root.users.push(session);
        }
        self.state
            .hosts
            .entry(peer.address.ip())
            .or_default()
            .push(session);
        self.state.invalidate();
        self.sessions.push(session);
    }

    /// Interpret `data` as frames: a sender byte, a type byte (mapped onto
    /// Mumble's range and the extension block), a length byte, then the body.
    pub fn run(&mut self, mut data: &[u8]) {
        while data.len() >= 3 {
            let sender = self.sessions[usize::from(data[0]) % self.sessions.len()];
            let kind = match data[1] {
                kind @ 0..=31 => u16::from(kind),
                other => 0x4e00 | u16::from(other),
            };
            let length = usize::from(data[2]).min(data.len() - 3);
            let payload = Bytes::copy_from_slice(&data[3..3 + length]);
            data = &data[3 + length..];
            if !self.state.users.contains_key(&sender) {
                continue;
            }
            self.state
                .handle(sender, ControlFrame { kind, payload }, &mut self.out);
            self.out.clear();
        }
        self.check_invariants();
    }

    /// The tree and memberships stay consistent whatever the input.
    fn check_invariants(&self) {
        for (id, channel) in &self.state.channels.0 {
            if let Some(parent) = channel.parent() {
                assert!(
                    self.state
                        .channels
                        .get(parent)
                        .is_some_and(|parent| parent.children.contains(id)),
                    "channel {id} is missing from its parent"
                );
            }
            for session in &channel.users {
                assert_eq!(
                    self.state.users.get(session).map(|user| user.channel),
                    Some(*id)
                );
            }
        }
        for (session, user) in &self.state.users {
            assert!(
                self.state
                    .channels
                    .get(user.channel)
                    .is_some_and(|channel| channel.users.contains(session)),
                "user {session} is not in its channel"
            );
        }
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    #[test]
    fn random_and_structured_frames_keep_the_state_consistent() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Valid messages that exercise deep paths, mixed into random input.
        let channel = proto::ChannelState {
            parent: Some(0),
            name: Some("Fuzz".into()),
            temporary: Some(true),
            ..Default::default()
        }
        .encode_to_vec();
        let register = proto::UserState {
            user_id: Some(0),
            ..Default::default()
        }
        .encode_to_vec();
        for _ in 0..300 {
            let mut harness = Harness::new();
            let mut data = Vec::new();
            for _ in 0..40 {
                let sender = next() as u8;
                match next() % 4 {
                    0 => data.extend_from_slice(&[sender, 7, channel.len() as u8]),
                    1 => data.extend_from_slice(&[sender, 9, register.len() as u8]),
                    _ => {
                        let length = (next() % 64) as u8;
                        data.extend_from_slice(&[sender, (next() % 40) as u8, length]);
                        data.extend((0..length).map(|_| next() as u8));
                        continue;
                    }
                }
                let body = if data[data.len() - 2] == 7 {
                    &channel
                } else {
                    &register
                };
                data.extend_from_slice(body);
            }
            harness.run(&data);
        }
    }
}
