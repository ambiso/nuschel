// SPDX-License-Identifier: EUPL-1.2
//! Share links (docs/web.md, "Share links"): a link to a channel, and opening
//! one. On a Nuschl server with `share/1` a registered user gets a token from
//! the server, which resolves it for whoever opens the link (with an access
//! token for a private channel); anywhere else the link names the channel by
//! its path, in the fragment.
use super::{Client, Update};
use crate::{platform::Request, server::Status};
use nuschl_core::{
    model::TextTarget,
    share::{ShareLink, encode_token},
};
use nuschl_protocol::nuschl::{capability, v1};

/// What a share-link request was for, by request id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ShareRequest {
    Create { server: String, channel: u32 },
    Resolve { server: String },
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
        let id = server.id.clone();
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
        let id = server.id.clone();
        let request = Request::ResolveShareLink {
            request_id,
            token: bytes,
        };
        if self.platform.request(&id, request).is_ok() {
            self.remember_share(request_id, ShareRequest::Resolve { server: id });
        } else {
            self.join_pending(index);
        }
    }

    pub(super) fn share_reply(&mut self, index: usize, link: v1::ShareLink) {
        let Some(request) = self.share_requests.remove(&link.request_id) else {
            return;
        };
        let Ok(token) = <[u8; 16]>::try_from(link.token.as_slice()) else {
            return;
        };
        match request {
            ShareRequest::Create { server, channel } if server == self.servers[index].id => {
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
            ShareRequest::Resolve { server } if server == self.servers[index].id => {
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
