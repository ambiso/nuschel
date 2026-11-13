// SPDX-License-Identifier: EUPL-1.2
//! Plugin data (Mumble's `PluginDataTransmission`; what servers forward and
//! the wire rules are in `nuschl_protocol::plugin`): opaque bytes between
//! clients for an embedder's own features. The client never interprets,
//! executes or decrypts them, and the reserved `nuschl/` data IDs are neither
//! sent nor delivered here.
//!
//! Receiving is off until [`Client::accept_plugin_data`]; while it is off the
//! connections skip plugin frames undecoded. What arrives waits in one queue
//! for all servers, at most [`MAX_PLUGIN_QUEUE`] items and
//! [`MAX_PLUGIN_QUEUE_BYTES`]; when it is full, new items are dropped and
//! counted ([`Client::plugin_data_dropped`]). Each item names the connection
//! generation it came from and its sender as the session knew it when the
//! packet arrived (session, presence instance, registration, certificate
//! hash), never as whoever holds that session number later. When a
//! connection ends, reconnects or is replaced, its queued items are
//! discarded, and nothing is handed out for a connection that is not current.
//! The certificate hash is the server's claim: on an honest server it shows
//! that the same key is back, not who that is, and it authorises nothing.
//!
//! Upstream of the queue a packet passes bounded buffers: up to 32 frames
//! read ahead of the session (plugin frames there at most 4,951 bytes; larger
//! ones are read past and dropped), a per-connection receive budget (64
//! items and 64 KiB a second), the session's event channel (128) and the
//! driver's (512). Plugin data never waits for room in those channels; it is
//! dropped instead. With receiving off nothing is retained, but frames still
//! pass the read-ahead buffer.
//!
//! Sending ([`Client::send_plugin_data`]) names each receiver by session and
//! presence instance. The connection checks them again right before the
//! message leaves, so a send waiting in a queue is refused rather than
//! reaching someone who took over a departed receiver's session number. Its
//! outcome comes back as [`PluginDataEvent::Admission`]: admitted means
//! handed to the connection within the local rate (two a second, a burst of
//! ten); the server may still drop it, and nothing confirms delivery.
use super::Client;
use crate::{input::ServerEvent, platform::Request, server::Status};
use nuschl_protocol::{
    nuschl::capability,
    plugin::{self, Payload, Refusal},
};
use std::collections::VecDeque;

/// Receivers one send may name (the session's `MAX_ACTION_TARGETS`).
pub const MAX_PLUGIN_RECEIVERS: usize = 128;
/// The queue of received plugin data and admissions, for all servers.
pub const MAX_PLUGIN_QUEUE: usize = 256;
pub const MAX_PLUGIN_QUEUE_BYTES: usize = 256 * 1024;

/// Plugin data another client sent us, with its sender as the session knew
/// it when the packet arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedPluginData {
    pub sender: u32,
    /// The sender's presence: reply with it, never with the session number alone.
    pub sender_instance: u64,
    pub sender_user_id: Option<u32>,
    /// What the server said; key continuity on an honest server, not identity.
    pub sender_certificate_hash: Option<String>,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginDataEvent {
    Received {
        server: String,
        generation: u64,
        data: ReceivedPluginData,
    },
    /// Our send `request` was admitted (handed to the connection, not
    /// delivered) or refused.
    Admission {
        server: String,
        generation: u64,
        request: u64,
        result: Result<(), Refusal>,
    },
}

impl PluginDataEvent {
    fn origin(&self) -> (&str, u64) {
        match self {
            Self::Received {
                server, generation, ..
            }
            | Self::Admission {
                server, generation, ..
            } => (server, *generation),
        }
    }
    /// What the item holds, for the queue's byte budget.
    fn cost(&self) -> usize {
        64 + match self {
            Self::Received { server, data, .. } => {
                server.len()
                    + data.payload.size()
                    + data.sender_certificate_hash.as_ref().map_or(0, String::len)
            }
            Self::Admission { server, .. } => server.len(),
        }
    }
}

#[derive(Default)]
pub(super) struct PluginQueue {
    accept: bool,
    items: VecDeque<PluginDataEvent>,
    bytes: usize,
    dropped: u64,
}

impl PluginQueue {
    fn push(&mut self, item: PluginDataEvent) {
        let cost = item.cost();
        if self.items.len() >= MAX_PLUGIN_QUEUE || self.bytes + cost > MAX_PLUGIN_QUEUE_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.bytes += cost;
        self.items.push_back(item);
    }
    fn retain(&mut self, keep: impl Fn(&PluginDataEvent) -> bool) {
        self.items.retain(|item| keep(item));
        self.bytes = self.items.iter().map(PluginDataEvent::cost).sum();
    }
}

impl Client {
    /// Receive plugin data on every connection (off by default). Turning it
    /// off discards what is queued and stops the connections decoding it.
    pub fn accept_plugin_data(&mut self, accept: bool) {
        self.plugin_data.accept = accept;
        if !accept {
            self.plugin_data
                .retain(|item| matches!(item, PluginDataEvent::Admission { .. }));
        }
        self.platform.accept_plugin_data(accept);
    }

    /// Send plugin data on server `index` to these receivers, each named by
    /// session and presence instance (as a [`User`](crate::model::User) or a
    /// [`ReceivedPluginData`] names them). Returns the request its
    /// [`PluginDataEvent::Admission`] answers; refused at once when it cannot
    /// be sent as asked.
    pub fn send_plugin_data(
        &mut self,
        index: usize,
        receivers: &[(u32, u64)],
        data_id: &str,
        data: &[u8],
    ) -> Result<u64, Refusal> {
        let server = self.servers.get(index).ok_or(Refusal::NotConnected)?;
        if server.status != Status::Connected {
            return Err(Refusal::NotConnected);
        }
        if server.has(capability::E2EE) {
            return Err(Refusal::EndToEnd);
        }
        if plugin::is_reserved(data_id) {
            return Err(Refusal::Reserved);
        }
        if data.len() > plugin::MAX_DATA_BYTES {
            return Err(Refusal::Payload(plugin::Error::DataTooLarge));
        }
        if data_id.len() > plugin::MAX_DATA_ID_BYTES {
            return Err(Refusal::Payload(plugin::Error::DataIdTooLong));
        }
        let payload = Payload::new(data_id.to_owned(), data.to_vec()).map_err(Refusal::Payload)?;
        if receivers.is_empty() || receivers.len() > MAX_PLUGIN_RECEIVERS {
            return Err(Refusal::Receivers(MAX_PLUGIN_RECEIVERS));
        }
        let own = server
            .own_session
            .and_then(|session| server.users.get(&session))
            .filter(|own| own.instance != 0)
            .ok_or(Refusal::NotConnected)?;
        let mut users = receivers.to_vec();
        users.sort_unstable();
        users.dedup();
        if users.iter().any(|&(session, instance)| {
            instance == 0
                || server
                    .users
                    .get(&session)
                    .is_none_or(|user| user.instance != instance)
        }) {
            return Err(Refusal::ReceiverChanged);
        }
        let request = super::request_id();
        let authority = nuschl_core::model::MetadataAuthority {
            generation: server.generation,
            own: (own.session, own.instance),
            users: users.clone(),
            channels: Vec::new(),
        };
        let id = server.id.clone();
        self.platform
            .request_metadata(
                &id,
                authority,
                Request::PluginData {
                    request,
                    receivers: users,
                    payload,
                },
            )
            .map_err(Refusal::Unavailable)?;
        Ok(request)
    }

    /// What arrived since the last call, oldest first: only items of
    /// connections that are still current. Received data also requires a
    /// connected server with our current presence; admissions remain visible
    /// for the current generation, including a refusal while connecting.
    pub fn take_plugin_data(&mut self) -> Vec<PluginDataEvent> {
        let items = std::mem::take(&mut self.plugin_data.items);
        self.plugin_data.bytes = 0;
        items
            .into_iter()
            .filter(|item| {
                let (server, generation) = item.origin();
                self.current(server, generation).is_some_and(|index| {
                    let server = &self.servers[index];
                    !matches!(item, PluginDataEvent::Received { .. })
                        || (server.status == Status::Connected
                            && server
                                .own_session
                                .and_then(|session| server.users.get(&session))
                                .is_some_and(|own| own.instance != 0))
                })
            })
            .collect()
    }

    /// Items dropped because the queue was full, since the client started.
    pub fn plugin_data_dropped(&self) -> u64 {
        self.plugin_data.dropped
    }

    /// A plugin event of server `index`'s current connection.
    pub(super) fn plugin_event(&mut self, index: usize, event: ServerEvent) {
        let server = &self.servers[index];
        let (id, generation) = (server.id.clone(), server.generation);
        let connected = server.status == Status::Connected
            && server
                .own_session
                .and_then(|session| server.users.get(&session))
                .is_some_and(|own| own.instance != 0);
        match event {
            ServerEvent::PluginData(data)
                if connected && self.plugin_data.accept && !data.payload.is_reserved() =>
            {
                self.plugin_data.push(PluginDataEvent::Received {
                    server: id,
                    generation,
                    data,
                });
            }
            ServerEvent::PluginDataAdmission { request, result } => {
                self.plugin_data.push(PluginDataEvent::Admission {
                    server: id,
                    generation,
                    request,
                    result,
                });
            }
            _ => {}
        }
    }

    /// The connection of `server` ended, reconnects or was replaced: what it
    /// left in the queue is obsolete.
    pub(super) fn forget_plugin_data(&mut self, server: &str) {
        self.plugin_data.retain(|item| item.origin().0 != server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HistoryTicket, input::Input, model::User, platform::Platform, timeline::Page};
    use nuschl_core::{audio::Sound, bookmark::ServerBookmark, model::MetadataAuthority};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(String, MetadataAuthority, Request)>>,
        accept: Mutex<Vec<bool>>,
        generation: Mutex<u64>,
    }
    impl Platform for Recorder {
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            let mut generation = self.generation.lock().unwrap();
            *generation += 1;
            Ok(*generation)
        }
        fn disconnect(&self, _: &str) {}
        fn request(&self, _: &str, _: Request) -> Result<(), String> {
            Err("plugin data needs authority".into())
        }
        fn request_metadata(
            &self,
            server: &str,
            authority: MetadataAuthority,
            request: Request,
        ) -> Result<(), String> {
            self.sent
                .lock()
                .unwrap()
                .push((server.into(), authority, request));
            Ok(())
        }
        fn local_history(
            &self,
            _: String,
            _: String,
            _: String,
            _: HistoryTicket,
            _: Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn server_history(
            &self,
            _: String,
            _: [u8; 16],
            _: HistoryTicket,
            _: Option<crate::platform::CacheKey>,
            _: Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn change_message(&self, _: crate::platform::ChangeRequest) -> Result<(), String> {
            Ok(())
        }
        fn render_html(&self, _: String, _: u64, _: crate::platform::RenderKey, _: String) {}
        fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {}
        fn play_sound(&self, _: &str, _: Sound) {}
        fn chime(&self, _: Sound) {}
        fn search(
            &self,
            _: u64,
            _: (Vec<String>, Option<String>),
            _: String,
            _: Option<nuschl_core::SearchCursor>,
        ) -> Result<(), String> {
            Ok(())
        }
        fn accept_plugin_data(&self, accept: bool) {
            self.accept.lock().unwrap().push(accept);
        }
    }

    fn user(session: u32, instance: u64) -> User {
        User {
            session,
            instance,
            name: format!("User {session}"),
            ..User::default()
        }
    }

    fn server(name: &str) -> ServerBookmark {
        ServerBookmark::new(name, "localhost", 64738, "tester").unwrap()
    }

    /// A client connected to `a` and `b` as session 1 with peers 2 and 3.
    fn connected() -> (Client, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let mut client = Client::new(
            Box::new(recorder.clone()),
            vec![server("a"), server("b")],
            Default::default(),
        );
        for index in 0..2 {
            client.connect(index, None, false);
            for event in [
                ServerEvent::User(user(1, 11 + index as u64 * 100)),
                ServerEvent::User(user(2, 22 + index as u64 * 100)),
                ServerEvent::User(user(3, 33 + index as u64 * 100)),
                ServerEvent::Connected { session: 1 },
            ] {
                client.handle(input(&client, index, event));
            }
        }
        (client, recorder)
    }

    fn input(client: &Client, index: usize, event: ServerEvent) -> Input {
        Input::Server {
            server: client.servers[index].id.clone(),
            generation: client.servers[index].generation,
            event,
        }
    }

    fn data(sender: u32, instance: u64, bytes: &[u8]) -> ServerEvent {
        ServerEvent::PluginData(ReceivedPluginData {
            sender,
            sender_instance: instance,
            sender_user_id: None,
            sender_certificate_hash: Some("cafe".into()),
            payload: Payload::new("game".into(), bytes.to_vec()).unwrap(),
        })
    }

    #[test]
    fn sends_name_presences_and_are_refused_when_they_cannot_be_kept() {
        let (mut client, recorder) = connected();
        let request = client
            .send_plugin_data(0, &[(3, 33), (2, 22), (3, 33)], "game", b"\x01")
            .unwrap();
        let sent = recorder.sent.lock().unwrap().pop().unwrap();
        let (
            server,
            authority,
            Request::PluginData {
                request: sent_request,
                receivers,
                payload,
            },
        ) = sent
        else {
            panic!("a plugin data request");
        };
        assert_eq!((&server, sent_request), (&client.servers[0].id, request));
        assert_eq!(receivers, [(2, 22), (3, 33)]);
        assert_eq!(authority.users, [(2, 22), (3, 33)]);
        assert_eq!(authority.own, (1, 11));
        assert_eq!(authority.generation, client.servers[0].generation);
        assert_eq!(payload.data(), b"\x01");
        // A presence that is not (or no longer) the session's occupant.
        assert_eq!(
            client.send_plugin_data(0, &[(2, 122)], "game", b""),
            Err(Refusal::ReceiverChanged)
        );
        client.handle(input(&client, 0, ServerEvent::User(user(2, 23))));
        assert_eq!(
            client.send_plugin_data(0, &[(2, 22)], "game", b""),
            Err(Refusal::ReceiverChanged)
        );
        let many: Vec<(u32, u64)> = (0..=128).map(|session| (session, 1)).collect();
        for (receivers, data_id, data, refusal) in [
            (&[][..], "game", &b""[..], Refusal::Receivers(128)),
            (&many[..], "game", &b""[..], Refusal::Receivers(128)),
            (&[(2, 23)][..], "nuschl/rtc", &b""[..], Refusal::Reserved),
            (
                &[(2, 23)][..],
                "game",
                &[0; 1001][..],
                Refusal::Payload(plugin::Error::DataTooLarge),
            ),
        ] {
            assert_eq!(
                client.send_plugin_data(0, receivers, data_id, data),
                Err(refusal)
            );
        }
        client.disconnect(1);
        assert_eq!(
            client.send_plugin_data(1, &[(2, 122)], "game", b""),
            Err(Refusal::NotConnected)
        );
        assert_eq!(recorder.sent.lock().unwrap().len(), 0);
    }

    #[test]
    fn payload_byte_limits_refuse_without_sending_or_changing_the_queue() {
        let (mut client, recorder) = connected();
        let exact_id = "é".repeat(50);
        let long_ascii_id = "i".repeat(101);
        let long_utf8_id = "é".repeat(51);
        let before = (
            client.servers[0].status,
            client.servers[0].generation,
            client.servers[0].own_session,
            client.servers[0].users.len(),
            client.updates.len(),
        );
        for (id, bytes, error) in [
            ("game", &[0; 1001][..], plugin::Error::DataTooLarge),
            (
                long_ascii_id.as_str(),
                &b""[..],
                plugin::Error::DataIdTooLong,
            ),
            (
                long_utf8_id.as_str(),
                &b""[..],
                plugin::Error::DataIdTooLong,
            ),
        ] {
            assert_eq!(
                client.send_plugin_data(0, &[(2, 22)], id, bytes),
                Err(Refusal::Payload(error))
            );
            assert!(recorder.sent.lock().unwrap().is_empty());
            assert!(client.plugin_data.items.is_empty());
            assert_eq!(client.plugin_data.bytes, 0);
            assert_eq!(client.plugin_data.dropped, 0);
            assert_eq!(
                (
                    client.servers[0].status,
                    client.servers[0].generation,
                    client.servers[0].own_session,
                    client.servers[0].users.len(),
                    client.updates.len(),
                ),
                before
            );
        }
        client
            .send_plugin_data(0, &[(2, 22)], &exact_id, &[0; 1000])
            .unwrap();
        let sent = recorder.sent.lock().unwrap();
        let [(_, _, Request::PluginData { payload, .. })] = sent.as_slice() else {
            panic!("one send at the exact byte limits");
        };
        assert_eq!(payload.data_id(), exact_id);
        assert_eq!(payload.data().len(), 1000);
    }

    #[test]
    fn late_data_is_refused_after_disconnect_and_while_reconnecting() {
        let (mut client, _) = connected();
        client.accept_plugin_data(true);
        let generation = client.servers[0].generation;
        let late = input(&client, 0, data(2, 22, b"late"));
        client.handle(input(&client, 0, data(2, 22, b"queued")));
        client.disconnect(0);
        assert_eq!(client.servers[0].generation, generation);
        client.handle(late);
        assert!(client.take_plugin_data().is_empty());

        let generation = client.servers[1].generation;
        let late = input(&client, 1, data(2, 122, b"late"));
        client.handle(input(&client, 1, data(2, 122, b"queued")));
        client.handle(Input::Reconnecting {
            server: client.servers[1].id.clone(),
            generation,
            attempt: 1,
            delay_secs: 1,
            reason: "test connection loss".into(),
        });
        assert_eq!(client.servers[1].generation, generation);
        client.handle(late);
        assert!(client.take_plugin_data().is_empty());
        // A connected status without our own known presence cannot accept
        // data, either before it is queued or when it is taken.
        client.servers[1].status = Status::Connected;
        client.handle(input(&client, 1, data(2, 122, b"no own session")));
        assert!(client.take_plugin_data().is_empty());
    }

    #[test]
    fn queued_data_requires_our_presence_when_taken() {
        let (mut client, _) = connected();
        client.accept_plugin_data(true);
        for unavailable in 0..4 {
            client.handle(input(&client, 0, data(2, 22, b"queued")));
            let own = client.servers[0].users.get(&1).unwrap().clone();
            match unavailable {
                0 => client.servers[0].status = Status::Disconnected,
                1 => client.servers[0].own_session = None,
                2 => {
                    client.servers[0].users.remove(&1);
                }
                _ => client.servers[0].users.get_mut(&1).unwrap().instance = 0,
            }
            assert!(client.take_plugin_data().is_empty());
            client.handle(input(&client, 0, data(2, 22, b"late")));
            assert!(client.take_plugin_data().is_empty());
            client.servers[0].status = Status::Connected;
            client.servers[0].own_session = Some(1);
            client.servers[0].users.insert(1, own);
        }
        // Admissions are connection-generation results, not incoming data.
        client.disconnect(0);
        client.handle(input(
            &client,
            0,
            ServerEvent::PluginDataAdmission {
                request: 8,
                result: Err(Refusal::NotConnected),
            },
        ));
        assert!(matches!(
            client.take_plugin_data().as_slice(),
            [PluginDataEvent::Admission { request: 8, .. }]
        ));
    }

    #[test]
    fn received_data_is_opt_in_bound_to_its_connection_and_bounded() {
        let (mut client, recorder) = connected();
        client.handle(input(&client, 0, data(2, 22, b"early")));
        assert!(client.take_plugin_data().is_empty(), "off by default");
        client.accept_plugin_data(true);
        assert_eq!(*recorder.accept.lock().unwrap(), [true]);
        client.handle(input(&client, 0, data(2, 22, b"one")));
        client.handle(input(&client, 1, data(3, 133, b"two")));
        let items = client.take_plugin_data();
        assert_eq!(items.len(), 2);
        let PluginDataEvent::Received {
            server,
            data: first,
            ..
        } = &items[0]
        else {
            panic!("received data");
        };
        assert_eq!(
            (server, first.sender, first.sender_instance),
            (&client.servers[0].id, 2, 22)
        );
        // Session 2 is taken over by someone else: a queued item keeps its
        // first occupant, and replying to that presence is refused.
        client.handle(input(&client, 0, data(2, 22, b"queued")));
        client.handle(input(&client, 0, ServerEvent::User(user(2, 24))));
        let items = client.take_plugin_data();
        let [PluginDataEvent::Received { data: queued, .. }] = items.as_slice() else {
            panic!("{items:?}");
        };
        assert_eq!(queued.sender_instance, 22);
        assert_eq!(
            client.send_plugin_data(0, &[(queued.sender, queued.sender_instance)], "game", b""),
            Err(Refusal::ReceiverChanged)
        );
        // A reconnect discards what the old connection left, and late events
        // of the old generation never arrive.
        client.handle(input(&client, 0, data(2, 24, b"before reconnect")));
        let old = input(&client, 0, data(2, 24, b"late"));
        client.connect(0, None, false);
        client.handle(old);
        assert!(client.take_plugin_data().is_empty());
        // Admissions of the current connection always arrive; the reserved
        // namespace never does.
        client.handle(input(
            &client,
            1,
            ServerEvent::PluginDataAdmission {
                request: 7,
                result: Err(Refusal::RateLimited {
                    retry_after_ms: 250,
                }),
            },
        ));
        client.handle(input(
            &client,
            1,
            ServerEvent::PluginData(ReceivedPluginData {
                payload: Payload::new("nuschl/x".into(), vec![]).unwrap(),
                ..match data(3, 133, b"") {
                    ServerEvent::PluginData(data) => data,
                    _ => unreachable!(),
                }
            }),
        ));
        assert_eq!(
            client.take_plugin_data(),
            [PluginDataEvent::Admission {
                server: client.servers[1].id.clone(),
                generation: client.servers[1].generation,
                request: 7,
                result: Err(Refusal::RateLimited {
                    retry_after_ms: 250
                }),
            }]
        );
        // One queue for every server: items and bytes are bounded together.
        // Use connected servers: the reconnect above left server A connecting.
        let (mut client, recorder) = connected();
        client.accept_plugin_data(true);
        for index in 0..MAX_PLUGIN_QUEUE + 10 {
            client.handle(input(
                &client,
                index % 2,
                data(3, 33 + (index % 2) as u64 * 100, b""),
            ));
        }
        assert_eq!(client.plugin_data_dropped(), 10);
        let big = [0; plugin::MAX_DATA_BYTES];
        assert_eq!(client.take_plugin_data().len(), MAX_PLUGIN_QUEUE);
        for _ in 0..MAX_PLUGIN_QUEUE {
            client.handle(input(&client, 1, data(3, 133, &big)));
        }
        let kept = client.take_plugin_data().len();
        assert!(
            kept < MAX_PLUGIN_QUEUE && kept * 1000 <= MAX_PLUGIN_QUEUE_BYTES,
            "{kept}"
        );
        // Turning it off drops what waits and tells every connection.
        client.handle(input(&client, 1, data(3, 133, b"waiting")));
        client.accept_plugin_data(false);
        assert!(client.take_plugin_data().is_empty());
        assert_eq!(*recorder.accept.lock().unwrap(), [true, false]);
    }
}
