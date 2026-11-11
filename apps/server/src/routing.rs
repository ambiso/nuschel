// SPDX-License-Identifier: EUPL-1.2
//! Voice routing without decoding (Murmur's `processMsg`): who hears a packet,
//! in which context, and re-encoding it per receiver for UDP (OCB2) or the TLS
//! tunnel. Buffers are reused across packets, so steady-state routing does not
//! allocate except for tunneled frames.
use crate::{authz::Action, state::State};
use nuschel_protocol::{
    control::permission_denied::DenyType,
    crypt, encode_tunnel, kind,
    relay::{self, Audio, Context, Datagram, Delivery},
};
use std::{
    net::SocketAddr,
    ops::Range,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Receiver {
    pub session: u32,
    pub context: Context,
    pub volume: f32,
}

/// Encrypted datagrams produced while the state lock is held, sent after it is
/// released. Reused by each task, so it stops allocating once warm.
#[derive(Default)]
pub struct Datagrams {
    bytes: Vec<u8>,
    packets: Vec<(usize, SocketAddr, Range<usize>)>,
}

impl Datagrams {
    pub fn iter(&self) -> impl Iterator<Item = (usize, SocketAddr, &[u8])> {
        self.packets
            .iter()
            .map(|(socket, address, range)| (*socket, *address, &self.bytes[range.clone()]))
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.packets.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Encrypt `plain` for one receiver.
    pub fn push(
        &mut self,
        socket: usize,
        address: SocketAddr,
        state: &mut crypt::CryptState,
        plain: &[u8],
    ) {
        let start = self.bytes.len();
        self.bytes.resize(start + plain.len() + crypt::HEADER, 0);
        match state.encrypt_into(plain, &mut self.bytes[start..]) {
            Some(length) => self.packets.push((socket, address, start..start + length)),
            None => self.bytes.truncate(start),
        }
    }
}

/// Reusable per-packet scratch space kept in the state.
#[derive(Default)]
pub struct Scratch {
    receivers: Vec<Receiver>,
    /// Tunnel frames already encoded for this packet, by encoding variant.
    tunnel: Vec<((bool, Context, u32, bool), bytes::Bytes)>,
}

/// IP, UDP and OCB2 header overhead Murmur adds to each packet's payload when
/// checking the bandwidth limit.
const PACKET_OVERHEAD: usize = 20 + 8 + 4;

impl State {
    /// A voice packet the client sent through the TLS tunnel. Any tunneled
    /// packet means the client is not using UDP right now (Murmur's
    /// `aiUdpFlag`), so its voice goes back through the tunnel too.
    pub fn voice_from_tcp(&mut self, session: u32, payload: &[u8], out: &mut Datagrams) {
        if payload.len() < 2 || payload.len() > relay::MAX_PLAIN + crypt::HEADER {
            return;
        }
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        user.voice.udp_active = false;
        let protobuf = relay::uses_protobuf(user.info.version);
        if let Some(Datagram::Audio(audio)) = relay::parse(protobuf, payload) {
            self.route(session, &audio, out);
        }
    }

    /// A decrypted datagram from a session's UDP address.
    pub fn voice_from_udp(&mut self, session: u32, plain: &[u8], out: &mut Datagrams) {
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        let protobuf = relay::uses_protobuf(user.info.version);
        match relay::parse(protobuf, plain) {
            Some(Datagram::Audio(audio)) => {
                user.voice.udp_active = true;
                self.route(session, &audio, out);
            }
            Some(Datagram::LegacyPing) => {
                if let Some((socket, address)) = user.voice.udp {
                    out.push(socket, address, &mut user.voice.crypt, plain);
                }
            }
            Some(Datagram::Ping {
                timestamp,
                request_extended_information: false,
            }) => {
                let mut reply = [0; 16];
                if let (Some((socket, address)), Some(length)) =
                    (user.voice.udp, relay::encode_ping(timestamp, &mut reply))
                {
                    out.push(socket, address, &mut user.voice.crypt, &reply[..length]);
                }
            }
            _ => {}
        }
    }

    /// Deliver one packet from `speaker` to everyone who should hear it.
    pub fn route(&mut self, speaker: u32, audio: &Audio, out: &mut Datagrams) {
        self.counters
            .voice_packets_in
            .fetch_add(1, Ordering::Relaxed);
        self.counters.voice_bytes_in.fetch_add(
            (PACKET_OVERHEAD + audio.opus.len()) as u64,
            Ordering::Relaxed,
        );
        let max_bandwidth = u64::from(self.settings.config.max_bandwidth / 8);
        let Some(user) = self.users.get_mut(&speaker) else {
            return;
        };
        if user.mute || user.suppress || user.self_mute {
            return;
        }
        if !user.voice.bandwidth.add_frame(
            PACKET_OVERHEAD + audio.opus.len(),
            max_bandwidth,
            Instant::now(),
        ) {
            return;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.receivers.clear();
        scratch.tunnel.clear();
        self.collect_receivers(speaker, audio.target, &mut scratch.receivers);
        // One entry per receiver: the lowest context and the loudest volume win.
        scratch
            .receivers
            .sort_unstable_by_key(|receiver| receiver.session);
        scratch.receivers.dedup_by(|later, kept| {
            if later.session != kept.session {
                return false;
            }
            kept.context = kept.context.min(later.context);
            kept.volume = kept.volume.max(later.volume);
            true
        });
        let loopback = audio.target == relay::TARGET_LOOPBACK;
        // Whisper and shout targets share a global fan-out budget; a packet
        // over it reaches nobody (not an arbitrary subset), and the speaker
        // learns why, at most every ten seconds.
        if audio.target != relay::TARGET_NORMAL
            && !loopback
            && let Some(budget) = self.fanout.as_mut()
            && !budget.take(scratch.receivers.len() as u64, Instant::now())
        {
            self.counters.fanout_dropped.fetch_add(1, Ordering::Relaxed);
            self.scratch = scratch;
            self.fanout_notice(speaker);
            return;
        }
        // Positional data is only passed between users of the same game context.
        let context = self
            .users
            .get_mut(&speaker)
            .map(|user| std::mem::take(&mut user.plugin_context))
            .unwrap_or_default();
        let mut plain = [0_u8; relay::MAX_PLAIN];
        let (mut sent, mut sent_bytes) = (0_u64, 0_u64);
        for receiver in &scratch.receivers {
            let Some(user) = self.users.get_mut(&receiver.session) else {
                continue;
            };
            if !loopback && (receiver.session == speaker || user.deaf || user.self_deaf) {
                continue;
            }
            let protobuf = relay::uses_protobuf(user.info.version);
            let delivery = Delivery {
                sender: speaker,
                context: receiver.context,
                volume: receiver.volume,
                positional: audio.positional.is_some() && user.plugin_context == context,
            };
            match user.voice.udp.filter(|_| user.voice.udp_active) {
                Some((socket, address)) => {
                    if let Some(length) = relay::encode(protobuf, audio, delivery, &mut plain) {
                        out.push(socket, address, &mut user.voice.crypt, &plain[..length]);
                        sent += 1;
                        sent_bytes += (PACKET_OVERHEAD + length) as u64;
                    }
                }
                None => {
                    let key = (
                        protobuf,
                        delivery.context,
                        delivery.volume.to_bits(),
                        delivery.positional,
                    );
                    let frame = match scratch.tunnel.iter().find(|(variant, _)| *variant == key) {
                        Some((_, frame)) => frame.clone(),
                        None => {
                            let Some(length) = relay::encode(protobuf, audio, delivery, &mut plain)
                            else {
                                continue;
                            };
                            let frame = encode_tunnel(&plain[..length]);
                            scratch.tunnel.push((key, frame.clone()));
                            frame
                        }
                    };
                    sent += 1;
                    sent_bytes += frame.len() as u64;
                    user.link.send_voice(frame);
                }
            }
        }
        self.counters
            .voice_packets_out
            .fetch_add(sent, Ordering::Relaxed);
        self.counters
            .voice_bytes_out
            .fetch_add(sent_bytes, Ordering::Relaxed);
        if let Some(user) = self.users.get_mut(&speaker) {
            user.plugin_context = context;
        }
        self.scratch = scratch;
    }

    /// Tell a speaker that a whisper exceeded the fan-out budget (at most
    /// every ten seconds).
    fn fanout_notice(&mut self, speaker: u32) {
        let now = Instant::now();
        let Some(user) = self.users.get_mut(&speaker) else {
            return;
        };
        if user
            .fanout_notice
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(10))
        {
            return;
        }
        user.fanout_notice = Some(now);
        self.deny_with(
            speaker,
            DenyType::Text,
            Some(
                "The server is busy: your whisper did not reach its listeners. Try again shortly.",
            ),
            None,
        );
    }

    /// Murmur's receiver selection, before deafness and self are filtered.
    fn collect_receivers(&self, speaker: u32, target: u8, receivers: &mut Vec<Receiver>) {
        let Some(user) = self.users.get(&speaker) else {
            return;
        };
        match target {
            relay::TARGET_LOOPBACK => receivers.push(Receiver {
                session: speaker,
                context: Context::Normal,
                volume: 1.,
            }),
            relay::TARGET_NORMAL => {
                let Some(channel) = self.channels.get(user.channel) else {
                    return;
                };
                self.push_channel(channel, receivers);
                if !channel.links.is_empty() {
                    for linked in self.channels.all_links(user.channel) {
                        if linked != user.channel
                            && self.allowed(speaker, Action::Speak { channel: linked })
                            && let Some(linked) = self.channels.get(linked)
                        {
                            self.push_channel(linked, receivers);
                        }
                    }
                }
            }
            _ => {
                if !user.targets.contains_key(&target) {
                    return;
                }
                let cached = user
                    .target_cache
                    .borrow()
                    .get(&target)
                    .filter(|cache| cache.generation == self.generation)
                    .map(|cache| receivers.extend_from_slice(&cache.receivers))
                    .is_some();
                if cached {
                    return;
                }
                let start = receivers.len();
                self.whisper_receivers(speaker, target, receivers);
                user.target_cache.borrow_mut().insert(
                    target,
                    crate::state::TargetCache {
                        generation: self.generation,
                        receivers: receivers[start..].to_vec(),
                    },
                );
            }
        }
    }

    /// Everyone in a channel and everyone listening to it.
    fn push_channel(&self, channel: &crate::state::Channel, receivers: &mut Vec<Receiver>) {
        self.push_listeners(channel.id, &channel.listeners, None, receivers);
        receivers.extend(channel.users.iter().map(|&session| Receiver {
            session,
            context: Context::Normal,
            volume: 1.,
        }));
    }

    fn push_listeners(
        &self,
        channel: u32,
        listeners: &std::collections::BTreeSet<u32>,
        group: Option<&str>,
        receivers: &mut Vec<Receiver>,
    ) {
        for &session in listeners {
            let Some(listener) = self.users.get(&session) else {
                continue;
            };
            if let Some(group) = group
                && !self.in_group(session, channel, group)
            {
                continue;
            }
            receivers.push(Receiver {
                session,
                context: Context::Listen,
                volume: listener.listening.get(&channel).copied().unwrap_or(1.),
            });
        }
    }

    /// Murmur's `createWhisperTargetCacheFor`.
    fn whisper_receivers(&self, speaker: u32, target: u8, receivers: &mut Vec<Receiver>) {
        let Some(spec) = self
            .users
            .get(&speaker)
            .and_then(|user| user.targets.get(&target))
        else {
            return;
        };
        let start = receivers.len();
        for (id, children, links, group) in &spec.channels {
            let Some(channel) = self.channels.get(*id) else {
                continue;
            };
            let links = *links && !channel.links.is_empty();
            let children = *children && !channel.children.is_empty();
            let group = (!group.is_empty()).then_some(group.as_str());
            let mut targets = if links {
                self.channels.all_links(*id)
            } else {
                std::collections::BTreeSet::from([*id])
            };
            if children {
                targets.extend(self.channels.subtree(*id));
            }
            for sub in targets {
                if !self.allowed(speaker, Action::Whisper { channel: sub }) {
                    continue;
                }
                let Some(channel) = self.channels.get(sub) else {
                    continue;
                };
                for &session in &channel.users {
                    let applies = group.is_none_or(|group| self.in_group(session, sub, group));
                    if applies {
                        receivers.push(Receiver {
                            session,
                            context: Context::Shout,
                            volume: 1.,
                        });
                    }
                }
                self.push_listeners(sub, &channel.listeners, group, receivers);
            }
        }
        let shouted: Vec<u32> = receivers[start..]
            .iter()
            .filter(|receiver| receiver.context == Context::Shout)
            .map(|receiver| receiver.session)
            .collect();
        for &session in &spec.sessions {
            if !self.users.contains_key(&session) {
                continue;
            }
            if !shouted.contains(&session)
                && self.allowed(speaker, Action::WhisperUser { user: session })
            {
                receivers.push(Receiver {
                    session,
                    context: Context::Whisper,
                    volume: 1.,
                });
            }
        }
        let mut index = start;
        while index < receivers.len() {
            if receivers[index].session == speaker {
                receivers.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    /// Ask a client for its nonce after its packets stopped decrypting
    /// (Murmur: after five seconds without a good packet, at most every five).
    pub fn request_resync(&mut self, session: u32) {
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        let now = Instant::now();
        let quiet = now
            .saturating_duration_since(user.voice.last_good)
            .as_secs()
            >= 5;
        let due = user
            .voice
            .last_resync_request
            .is_none_or(|last| now.saturating_duration_since(last).as_secs() >= 5);
        if quiet && due {
            user.voice.last_resync_request = Some(now);
            user.link.message(
                kind::CRYPT_SETUP,
                &nuschel_protocol::control::CryptSetup::default(),
            );
        }
    }
}
