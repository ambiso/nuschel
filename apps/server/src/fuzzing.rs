// SPDX-License-Identifier: EUPL-1.2
//! A harness for hostile control and voice input: a state with a channel tree,
//! a SuperUser, registered and anonymous users (including a legacy client), fed
//! frames as if they came from those connections. Used by the libFuzzer target
//! in `fuzz/` and by a deterministic test on stable Rust. No network, no runtime.
use crate::{
    auth::new_user,
    config::{Config, Settings},
    routing::Datagrams,
    state::{ClientInfo, Counters, Link, Peer, Registered, State},
    storage::StorageHandle,
};
use bytes::Bytes;
use nuschel_protocol::{ControlFrame, control as proto, crypt::CryptState};
use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Instant,
};

pub struct Harness {
    pub state: State,
    sessions: Vec<u32>,
    out: Datagrams,
}

impl Harness {
    pub fn new() -> Self {
        // Compiling the name patterns took most of each fuzzing run; the
        // settings never change, so every harness shares them.
        static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();
        let settings = SETTINGS.get_or_init(|| {
            let config = Config {
                message_limit: 1000,
                message_burst: 1000,
                listeners_per_user: Some(4),
                ..Config::default()
            };
            Arc::new(
                config
                    .settings()
                    .unwrap_or_else(|_| unreachable!("default settings are valid")),
            )
        });
        let mut state = State::new(
            settings.clone(),
            StorageHandle::detached(),
            Arc::new(Counters::default()),
        );
        state.initialise_root();
        // Key transparency: requests are authorised and queued (to nowhere).
        state.kt = Some(crate::kt::KtHandle::detached(
            vec![0; 71],
            nuschel_kt::LogId([7; 32]),
        ));
        state.registry.insert(Registered {
            id: 1,
            account: [1; 16],
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
        for (name, user_id, hash, version, nuschel) in [
            ("SuperUser", Some(0), None, 5, false),
            ("Registered", Some(1), Some("ab".repeat(20)), 5, true),
            ("Anonymous", None, Some("cd".repeat(20)), 5, true),
            ("Legacy", None, None, 3, false),
        ] {
            harness.add_user(name, user_id, hash, version, nuschel);
        }
        harness
    }

    fn add_user(
        &mut self,
        name: &str,
        user_id: Option<u32>,
        hash: Option<String>,
        minor: u64,
        nuschel: bool,
    ) {
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
        let mut user = new_user(
            &config,
            session,
            name.into(),
            user_id,
            &peer,
            &proto::Authenticate::default(),
            crypt,
            Instant::now(),
        );
        // Nuschel connections, so the extension's handlers are reached.
        if nuschel {
            user.extension = Some(Box::new(crate::extension::Extension::new(
                crate::extension::server_capabilities(true),
                &config,
                Instant::now(),
            )));
            user.account = user_id
                .and_then(|id| self.state.registry.users.get(&id))
                .map(|registered| registered.account);
        }
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
                other => 0x4e00 | u16::from(other - 32),
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

    /// A root conversation id, for structured extension input.
    pub fn root_conversation(&self) -> [u8; 16] {
        self.state
            .channels
            .get(0)
            .map_or([0; 16], |root| root.conversation_id)
    }

    /// The tree and memberships stay consistent whatever the input, and
    /// nobody keeps a subscription they may not read.
    fn check_invariants(&self) {
        for (session, user) in &self.state.users {
            if let Some(extension) = &user.extension {
                assert!(extension.outstanding.len() <= nuschel_protocol::nuschel::MAX_OUTSTANDING);
                for id in &extension.subscriptions {
                    assert!(
                        self.state.can_read(*session, id),
                        "session {session} kept a subscription it may not read"
                    );
                }
            }
        }
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
        for round in 0..300 {
            let mut harness = Harness::new();
            let root = harness.root_conversation().to_vec();
            let structured = [
                (
                    0x01_u8,
                    nuschel_protocol::nuschel::v1::ChatSend {
                        request_id: round,
                        conversation: root.clone(),
                        idempotency_key: vec![7; 16],
                        format: 1,
                        content: "x".into(),
                        reply_to: 0,
                    }
                    .encode_to_vec(),
                ),
                (
                    0x06,
                    nuschel_protocol::nuschel::v1::Subscribe {
                        request_id: round + 1,
                        conversations: vec![root.clone()],
                    }
                    .encode_to_vec(),
                ),
                (
                    0x04,
                    nuschel_protocol::nuschel::v1::HistoryRequest {
                        request_id: round + 2,
                        conversation: root.clone(),
                        anchor: Some(
                            nuschel_protocol::nuschel::v1::history_request::Anchor::Latest(true),
                        ),
                        limit: 5,
                    }
                    .encode_to_vec(),
                ),
                (
                    0x18,
                    nuschel_protocol::nuschel::v1::KtLookup {
                        request_id: round + 3,
                        target: Some(nuschel_protocol::nuschel::v1::kt_lookup::Target::UserId(1)),
                    }
                    .encode_to_vec(),
                ),
                (
                    0x1a,
                    nuschel_protocol::nuschel::v1::KtHistory {
                        request_id: round + 4,
                        account: vec![1; 16],
                    }
                    .encode_to_vec(),
                ),
            ];
            let mut data = Vec::new();
            for _ in 0..40 {
                let sender = next() as u8;
                if next() % 5 == 0 {
                    let (kind, body) = &structured[(next() % 5) as usize];
                    // The harness maps type bytes from 32 on into the extension block.
                    data.extend_from_slice(&[sender, 32 + kind, body.len() as u8]);
                    data.extend_from_slice(body);
                    continue;
                }
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
