// SPDX-License-Identifier: EUPL-1.2
//! The UDP voice socket: OCB2 decryption per session, binding a session's UDP
//! address (only from the IP its TLS connection came from, and only by proving
//! the key), nonce resynchronisation, and unencrypted server-browser pings.
use crate::{
    connection::{canonical, send_datagrams},
    limits::LeakyBucket,
    routing::Datagrams,
    server::{PROTOCOL_VERSION, PingLimiter, Server, prefix},
    state::State,
};
use nuschel_protocol::relay::{self, ServerInfo};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

/// Stock Mumble's UDP buffer; anything larger is not a Mumble datagram.
const MAX_DATAGRAM: usize = 1024;
/// OCB2 header plus at least a type byte.
const MIN_DATAGRAM: usize = 5;
/// Sessions tried per datagram from an unknown address on a shared IP.
const MAX_TRIALS: usize = 8;

/// Per-socket state that is not part of the server state.
pub struct Receiver {
    pings: PingLimiter,
    /// Trial decryptions for unknown sources, per address prefix.
    trials: HashMap<IpAddr, LeakyBucket>,
}

impl Receiver {
    pub fn new(allow_ping_amplification: bool) -> Self {
        Self {
            pings: PingLimiter::new(allow_ping_amplification),
            trials: HashMap::new(),
        }
    }
}

impl State {
    /// Handle one datagram; returns an unencrypted ping answer to send back.
    pub fn receive_udp(
        &mut self,
        socket: usize,
        from: SocketAddr,
        packet: &[u8],
        receiver: &mut Receiver,
        out: &mut Datagrams,
        answer: &mut [u8; 64],
    ) -> Option<usize> {
        let now = Instant::now();
        let mut plain = [0_u8; MAX_DATAGRAM];
        if let Some(&session) = self.udp_peers.get(&from) {
            if let Some(user) = self.users.get_mut(&session) {
                let voice = &mut user.voice;
                if voice.blocked_until.is_some_and(|until| until > now) {
                    return None;
                }
                if let Some(length) = voice.crypt.decrypt_into(packet, &mut plain) {
                    voice.last_good = now;
                    self.voice_from_udp(session, &plain[..length], out);
                    return None;
                }
                // Forging needs about 2^24 tries per success with a 24-bit tag:
                // pause decryption for a peer that floods failures.
                if voice.failures.limited(now) {
                    voice.blocked_until = Some(now + Duration::from_secs(1));
                }
                self.request_resync(session);
            }
            return self.answer_ping(from, packet, receiver, answer);
        }
        if let Some(length) = self.answer_ping(from, packet, receiver, answer) {
            return Some(length);
        }
        let candidates: Vec<u32> = self
            .hosts
            .get(&from.ip())?
            .iter()
            .copied()
            .take(MAX_TRIALS)
            .collect();
        if receiver.trials.len() > 65_536 {
            receiver.trials.clear();
        }
        let budget = receiver
            .trials
            .entry(prefix(from.ip()))
            .or_insert_with(|| LeakyBucket::new(50, 200, now));
        if budget.limited(now) {
            return None;
        }
        for session in candidates {
            let Some(user) = self.users.get_mut(&session) else {
                continue;
            };
            let Some(length) = user.voice.crypt.decrypt_into(packet, &mut plain) else {
                continue;
            };
            // Proven by the key, from the address that authenticated: bind (or
            // rebind after a NAT mapping changed).
            if let Some((_, old)) = user.voice.udp.replace((socket, from)) {
                self.udp_peers.remove(&old);
            }
            user.voice.last_good = now;
            self.udp_peers.insert(from, session);
            self.voice_from_udp(session, &plain[..length], out);
            return None;
        }
        None
    }

    fn answer_ping(
        &self,
        from: SocketAddr,
        packet: &[u8],
        receiver: &mut Receiver,
        answer: &mut [u8; 64],
    ) -> Option<usize> {
        let config = &self.settings.config;
        if !config.allow_ping {
            return None;
        }
        let info = ServerInfo {
            version: PROTOCOL_VERSION,
            users: self.users.values().filter(|user| !user.bot).count() as u32,
            max_users: config.max_users,
            max_bandwidth: config.max_bandwidth,
        };
        relay::answer_info_ping(packet, info, receiver.pings.from(from.ip()), answer)
    }
}

pub async fn run(server: Arc<Server>, index: usize, socket: Arc<UdpSocket>) {
    let mut buffer = [0_u8; 2048];
    let mut answer = [0_u8; 64];
    let mut out = Datagrams::default();
    let mut receiver = Receiver::new(server.settings.config.allow_ping_amplification);
    loop {
        let (length, from) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            Err(error) => {
                tracing::debug!(%error, "UDP receive failed");
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        if !(MIN_DATAGRAM..=MAX_DATAGRAM).contains(&length) {
            continue;
        }
        let from = canonical(from);
        let reply = server.lock().receive_udp(
            index,
            from,
            &buffer[..length],
            &mut receiver,
            &mut out,
            &mut answer,
        );
        if let Some(reply) = reply {
            let _ = socket.try_send_to(&answer[..reply], from);
        }
        send_datagrams(&server, &mut out);
    }
}
