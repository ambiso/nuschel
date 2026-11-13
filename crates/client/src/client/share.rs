// SPDX-License-Identifier: EUPL-1.2
//! Share links (docs/web.md, "Share links"): a link to a channel, and opening
//! one. On a Nuschl server with `share/1` a registered user gets a token from
//! the server, which resolves it for whoever opens the link (with an access
//! token for a private channel); anywhere else the link names the channel by
//! its path, in the fragment.
use super::{Client, Update};
use crate::{
    platform::Request,
    server::{ServerState, Status},
};
use nuschl_core::{
    model::TextTarget,
    share::{ShareLink, encode_token},
};
use nuschl_protocol::nuschl::{capability, v1};

/// What a share-link request was for, by request id, and the server and
/// generation of the connection that asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ShareRequest {
    Create {
        server: String,
        generation: u64,
        channel: u32,
    },
    Resolve {
        server: String,
        generation: u64,
    },
}

impl ShareRequest {
    /// Whether `server`'s current connection made this request.
    fn made_on(&self, server: &ServerState) -> bool {
        let (id, generation) = match self {
            Self::Create {
                server: id,
                generation,
                ..
            }
            | Self::Resolve {
                server: id,
                generation,
            } => (id, *generation),
        };
        *id == server.id && generation == server.generation
    }
}

/// Requests kept at most; older ones are forgotten.
const MAX_REQUESTS: usize = 16;

impl Client {
    /// The share link for `channel` on server `index`: at once when it names
    /// the channel by its path, or `None` when the server issues a token and
    /// [`Update::ShareLink`] brings it.
    pub fn share_link(&mut self, index: usize, channel: u32) -> Result<Option<ShareLink>, String> {
        let server = self.servers.get(index).ok_or("no such server")?;
        let path = server
            .channel_path(channel)
            .ok_or("the channel is not known")?;
        let token = server.status == Status::Connected
            && server.has(capability::SHARE)
            && server.own_user_id().is_some();
        if !token {
            let link = ShareLink {
                channel: path,
                ..self.share_base(index)
            };
            if link.app_url().len() > nuschl_core::share::MAX_LINK_BYTES {
                return Err("the channel's path is too long for a link".into());
            }
            return Ok(Some(link));
        }
        let request_id = super::request_id();
        let (id, generation) = (server.id.clone(), server.generation);
        self.platform.request(
            &id,
            Request::CreateShareLink {
                request_id,
                channel,
                expires_at: 0,
                access_token: String::new(),
            },
        )?;
        self.remember_share(
            request_id,
            ShareRequest::Create {
                server: id,
                generation,
                channel,
            },
        );
        Ok(None)
    }

    /// Open the channel of a link's token: now if server `index` is
    /// connected, else once it is.
    pub fn open_share_token(&mut self, index: usize, token: String) {
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        server.share_token = Some(token);
        if server.status == Status::Connected {
            if server.has(capability::SHARE) {
                self.share_connected(index);
            } else {
                server.share_token = None;
                self.join_pending(index);
            }
        }
    }

    fn remember_share(&mut self, request_id: u64, request: ShareRequest) {
        while self.share_requests.len() >= MAX_REQUESTS {
            self.share_requests.pop_first();
        }
        self.share_requests.insert(request_id, request);
    }

    /// The link's server part: address, pin and policy.
    fn share_base(&self, index: usize) -> ShareLink {
        let server = &self.servers[index];
        ShareLink {
            token: None,
            host: server.bookmark.host.clone(),
            port: server.bookmark.port,
            channel: Vec::new(),
            certificate_sha256: server
                .bookmark
                .certificate_sha256
                .clone()
                .or_else(|| server.peer_sha256.clone()),
            require_nuschl: !server.capabilities.is_empty(),
            name: None,
        }
    }

    /// Connected: resolve a link's token (its channel path is the fallback,
    /// already pending).
    pub(super) fn share_connected(&mut self, index: usize) {
        let server = &mut self.servers[index];
        let Some(token) = server.share_token.take() else {
            return;
        };
        // A token this client cannot read, or a server without share/1: the
        // link's path decides.
        let bytes = nuschl_core::share::decode_token(&token);
        let Some(bytes) = bytes.filter(|_| server.has(capability::SHARE)) else {
            if server.pending_channel.is_empty() {
                server.set_notice("This server cannot open share links.".into());
            }
            return self.join_pending(index);
        };
        let request_id = super::request_id();
        let (id, generation) = (server.id.clone(), server.generation);
        let request = Request::ResolveShareLink {
            request_id,
            token: bytes,
        };
        if self.platform.request(&id, request).is_ok() {
            self.remember_share(
                request_id,
                ShareRequest::Resolve {
                    server: id,
                    generation,
                },
            );
        } else {
            self.join_pending(index);
        }
    }

    pub(super) fn share_reply(&mut self, index: usize, link: v1::ShareLink) {
        // Another connection's request waits for its own answer, whatever
        // this one holds.
        if self
            .share_requests
            .get(&link.request_id)
            .is_some_and(|request| !request.made_on(&self.servers[index]))
        {
            return;
        }
        let Some(request) = self.share_requests.remove(&link.request_id) else {
            return;
        };
        let Ok(token) = <[u8; 16]>::try_from(link.token.as_slice()) else {
            return;
        };
        match request {
            ShareRequest::Create {
                server, channel, ..
            } if server == self.servers[index].id => {
                let path = self.servers[index]
                    .channel_path(channel)
                    .unwrap_or_default();
                let mut share = ShareLink {
                    token: Some(encode_token(&token)),
                    // For the landing page, and if the token cannot be resolved.
                    channel: path,
                    ..self.share_base(index)
                };
                if share.app_url().len() > nuschl_core::share::MAX_LINK_BYTES {
                    share.channel.clear();
                }
                self.updates.push(Update::ShareLink {
                    server,
                    channel,
                    link: share,
                    web_host: Some(link.web_host).filter(|host| !host.is_empty()),
                });
            }
            ShareRequest::Resolve { server, .. } if server == self.servers[index].id => {
                let state = &mut self.servers[index];
                // The link's channel path is only the fallback.
                state.pending_channel.clear();
                if !link.access_token.is_empty()
                    && !state.bookmark.tokens.contains(&link.access_token)
                {
                    state.bookmark.tokens.push(link.access_token);
                    let tokens = state.bookmark.tokens.clone();
                    let _ = self.platform.request(&server, Request::SetTokens(tokens));
                    self.updates.push(Update::BookmarksChanged);
                }
                let _ = self.platform.request(&server, Request::Join(link.channel));
                if self.servers[index]
                    .channels
                    .iter()
                    .any(|view| view.target == TextTarget::Channel(link.channel))
                {
                    self.updates.push(Update::Browse {
                        server,
                        channel: link.channel,
                    });
                }
            }
            _ => {}
        }
    }

    /// An error for a share-link request: `true` if it was one.
    pub(super) fn share_failed(&mut self, index: usize, error: &v1::Error) -> bool {
        // Another connection's request waits on; the error is not this
        // connection's share request.
        if self
            .share_requests
            .get(&error.request_id)
            .is_some_and(|request| !request.made_on(&self.servers[index]))
        {
            return false;
        }
        let Some(request) = self.share_requests.remove(&error.request_id) else {
            return false;
        };
        let server = &mut self.servers[index];
        match request {
            ShareRequest::Create { .. } => {
                server.set_notice(format!("No share link: {}", error.detail));
            }
            ShareRequest::Resolve { .. } if server.pending_channel.is_empty() => {
                server.set_notice("This share link no longer works.".into());
            }
            // The link's channel path may still lead somewhere.
            ShareRequest::Resolve { .. } => self.join_pending(index),
        }
        true
    }
}

#[cfg(test)]
mod owner_tests {
    //! Answers to the client's own requests (share links, direct
    //! conversations) are taken only by the connection that asked.
    use super::*;
    use crate::{
        HistoryTicket,
        input::{Input, ServerEvent},
        model::{Channel, User},
        names::Preferences,
        platform::{CacheKey, ChangeRequest, Platform, RenderKey},
        timeline::Page,
    };
    use nuschl_core::{SearchCursor, audio::Sound, bookmark::ServerBookmark};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    /// Records requests; every connection gets the next generation.
    #[derive(Default)]
    struct Recording {
        requests: Mutex<Vec<(String, Request)>>,
        generations: AtomicU64,
    }

    impl Platform for Recording {
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            Ok(self.generations.fetch_add(1, Ordering::Relaxed) + 1)
        }
        fn disconnect(&self, _: &str) {}
        fn request(&self, server: &str, request: Request) -> Result<(), String> {
            self.requests
                .lock()
                .unwrap()
                .push((server.to_owned(), request));
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
            _: Option<CacheKey>,
            _: Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn change_message(&self, _: ChangeRequest) -> Result<(), String> {
            Ok(())
        }
        fn render_html(&self, _: String, _: u64, _: RenderKey, _: String) {}
        fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {}
        fn play_sound(&self, _: &str, _: Sound) {}
        fn chime(&self, _: Sound) {}
        fn search(
            &self,
            _: u64,
            _: (Vec<String>, Option<String>),
            _: String,
            _: Option<SearchCursor>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// An event on server `index`'s current connection.
    fn on(client: &Client, index: usize, event: ServerEvent) -> Input {
        Input::Server {
            server: client.servers[index].id.clone(),
            generation: client.servers[index].generation,
            event,
        }
    }

    /// Server `index` connected with history and share links: we are
    /// registered (5), and `bea` (session 2, registered as 6) is online.
    fn log_in(client: &mut Client, index: usize) {
        client.connect(index, None, false);
        for event in [
            ServerEvent::PeerCertificate("ab".repeat(32)),
            ServerEvent::Channel(Channel {
                id: 2,
                parent: Some(0),
                name: "Two".into(),
                ..Default::default()
            }),
            ServerEvent::Connected { session: 1 },
            ServerEvent::User(User {
                session: 1,
                name: "andi".into(),
                channel: 2,
                user_id: Some(5),
                ..Default::default()
            }),
            ServerEvent::User(User {
                session: 2,
                name: "bea".into(),
                channel: 2,
                user_id: Some(6),
                ..Default::default()
            }),
            ServerEvent::Negotiated {
                capabilities: vec!["history/1".into(), "share/1".into()],
                policy: None,
            },
        ] {
            let input = on(client, index, event);
            client.handle(input);
        }
        client.settle();
        client.take_updates();
    }

    /// Servers a and b, both connected (generations 1 and 2).
    fn two_servers() -> (Client, Arc<Recording>) {
        let recording = Arc::new(Recording::default());
        let bookmarks = vec![
            ServerBookmark::new("A", "a.example", 64738, "andi").unwrap(),
            ServerBookmark::new("B", "b.example", 64738, "andi").unwrap(),
        ];
        let mut client = Client::new(
            Box::new(recording.clone()),
            bookmarks,
            Preferences::default(),
        );
        log_in(&mut client, 0);
        log_in(&mut client, 1);
        (client, recording)
    }

    fn last_request(recording: &Recording, id: impl Fn(&Request) -> Option<u64>) -> u64 {
        recording
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|(_, request)| id(request))
            .expect("asked")
    }
    fn open_direct(client: &mut Client, recording: &Recording) -> u64 {
        assert_eq!(client.open_direct(0, 2), None, "the server opens it");
        last_request(recording, |request| match request {
            Request::OpenDirect { request_id, .. } => Some(*request_id),
            _ => None,
        })
    }
    fn opened(request_id: u64, conversation: Vec<u8>) -> ServerEvent {
        ServerEvent::DirectOpened(v1::DirectOpened {
            request_id,
            direct: Some(v1::Direct {
                conversation,
                user_id: 6,
                name: "bea".into(),
                ..Default::default()
            }),
        })
    }
    fn refused(request_id: u64) -> ServerEvent {
        ServerEvent::ExtensionError(v1::Error {
            request_id,
            detail: "refused".into(),
            ..Default::default()
        })
    }
    fn shared(request_id: u64, token: Vec<u8>) -> ServerEvent {
        ServerEvent::ShareLink(v1::ShareLink {
            request_id,
            token,
            channel: 2,
            access_token: "door".into(),
            ..Default::default()
        })
    }
    fn notices(client: &Client) -> Vec<Option<String>> {
        client
            .servers
            .iter()
            .map(|server| server.notice.clone())
            .collect()
    }
    fn tokens(client: &Client) -> Vec<Vec<String>> {
        client
            .servers
            .iter()
            .map(|server| server.bookmark.tokens.clone())
            .collect()
    }
    fn handle_all(client: &mut Client, index: usize, events: Vec<ServerEvent>) -> Vec<Update> {
        for event in events {
            let input = on(client, index, event);
            client.handle(input);
        }
        client.settle();
        client.take_updates()
    }
    fn shows(updates: &[Update]) -> bool {
        updates.iter().any(|update| {
            matches!(
                update,
                Update::Browse { .. } | Update::ShareLink { .. } | Update::BookmarksChanged
            )
        })
    }

    #[test]
    fn another_server_cannot_take_a_direct_conversation_request() {
        let (mut client, recording) = two_servers();
        let request_id = open_direct(&mut client, &recording);
        let before = notices(&client);
        let updates = handle_all(
            &mut client,
            1,
            vec![
                opened(request_id, vec![7; 16]),
                opened(request_id, vec![7; 3]),
                ServerEvent::DirectOpened(v1::DirectOpened {
                    request_id,
                    direct: None,
                }),
                refused(request_id),
            ],
        );
        assert!(!shows(&updates));
        assert!(client.servers[1].directs.is_empty(), "nothing noted on b");
        assert_eq!(notices(&client), before);
        assert!(client.direct_requests.contains_key(&request_id));
        // a's own answer still opens it.
        let updates = handle_all(&mut client, 0, vec![opened(request_id, vec![7; 16])]);
        let a = client.servers[0].id.clone();
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, Update::Browse { server, .. } if *server == a))
        );
        assert!(client.servers[0].directs.contains_key(&[7; 16]));
        assert!(client.direct_requests.is_empty());
    }

    #[test]
    fn another_server_cannot_take_a_share_request() {
        let (mut client, recording) = two_servers();
        assert_eq!(client.share_link(0, 2).unwrap(), None);
        let create = last_request(&recording, |request| match request {
            Request::CreateShareLink { request_id, .. } => Some(*request_id),
            _ => None,
        });
        client.open_share_token(0, encode_token(&[9; 16]));
        let resolve = last_request(&recording, |request| match request {
            Request::ResolveShareLink { request_id, .. } => Some(*request_id),
            _ => None,
        });
        let (notices_before, tokens_before) = (notices(&client), tokens(&client));
        recording.requests.lock().unwrap().clear();
        for request_id in [create, resolve] {
            let updates = handle_all(
                &mut client,
                1,
                vec![
                    shared(request_id, vec![7; 16]),
                    shared(request_id, vec![7; 3]),
                    refused(request_id),
                ],
            );
            assert!(!shows(&updates));
        }
        assert_eq!(notices(&client), notices_before);
        assert_eq!(tokens(&client), tokens_before, "no access token taken");
        assert!(
            recording.requests.lock().unwrap().is_empty(),
            "nothing sent or joined"
        );
        assert!(client.share_requests.contains_key(&create));
        assert!(client.share_requests.contains_key(&resolve));
        // a's own answers still work.
        let updates = handle_all(&mut client, 0, vec![shared(create, vec![7; 16])]);
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, Update::ShareLink { channel: 2, .. }))
        );
        handle_all(&mut client, 0, vec![shared(resolve, vec![9; 16])]);
        assert_eq!(client.servers[0].bookmark.tokens, ["door".to_string()]);
        let requests: Vec<Request> = recording
            .requests
            .lock()
            .unwrap()
            .drain(..)
            .map(|(_, request)| request)
            .collect();
        assert!(
            requests.contains(&Request::Join(2)),
            "the link's channel is joined"
        );
        assert!(client.share_requests.is_empty());
    }

    #[test]
    fn a_later_connection_of_the_same_server_cannot_take_an_earlier_request() {
        let (mut client, recording) = two_servers();
        let direct = open_direct(&mut client, &recording);
        assert_eq!(client.share_link(0, 2).unwrap(), None);
        let share = last_request(&recording, |request| match request {
            Request::CreateShareLink { request_id, .. } => Some(*request_id),
            _ => None,
        });
        log_in(&mut client, 0);
        assert_eq!(client.servers[0].generation, 3);
        let before = notices(&client);
        let updates = handle_all(
            &mut client,
            0,
            vec![
                opened(direct, vec![7; 16]),
                refused(direct),
                ServerEvent::ShareLink(v1::ShareLink {
                    request_id: share,
                    token: vec![7; 16],
                    ..Default::default()
                }),
                refused(share),
            ],
        );
        assert!(!shows(&updates));
        assert!(client.servers[0].directs.is_empty());
        assert_eq!(notices(&client), before);
        assert!(client.direct_requests.contains_key(&direct));
        assert!(client.share_requests.contains_key(&share));
        // The new connection's own request is answered as before.
        let again = open_direct(&mut client, &recording);
        let updates = handle_all(&mut client, 0, vec![opened(again, vec![7; 16])]);
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, Update::Browse { .. }))
        );
    }

    #[test]
    fn an_unasked_direct_conversation_is_noted_without_opening_it() {
        let (mut client, _) = two_servers();
        let updates = handle_all(&mut client, 0, vec![opened(u64::MAX - 1, vec![8; 16])]);
        assert!(!shows(&updates));
        assert!(client.servers[0].directs.contains_key(&[8; 16]));
        assert!(
            client.servers[0]
                .channels
                .iter()
                .any(|channel| channel.conversation == Some([8; 16]))
        );
    }

    #[test]
    fn the_request_limits_remain() {
        let (mut client, _) = two_servers();
        for _ in 0..20 {
            assert_eq!(client.open_direct(0, 2), None);
            assert_eq!(client.share_link(0, 2).unwrap(), None);
        }
        assert_eq!(client.direct_requests.len(), 16);
        assert_eq!(client.share_requests.len(), MAX_REQUESTS);
    }
}
