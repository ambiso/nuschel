// SPDX-License-Identifier: EUPL-1.2
//! Share links (`share/1`, docs/web.md "Share links"): random tokens that open
//! a channel, optionally with an access token for a private channel and an
//! expiry. The link itself (`https://<web host>/join/<token>#…`) is built by the
//! client; the server only issues, resolves and revokes tokens.
//!
//! - **Who may.** Registered users who may enter the channel create links (a
//!   link reveals no more than a `mumble://` URL to the channel would); one with
//!   an access token needs Write on the channel (or the root), since it lets
//!   anyone in. The creator, or someone with that Write, revokes. Any
//!   authenticated connection resolves, rate-limited; unknown, revoked and
//!   expired tokens and channels that are gone are the same `NOT_FOUND`.
//! - **Limits** fail only the request: 64 links per channel, 256 per account
//!   (`LIMIT_REACHED`).
//! - **Persistence**: the server's key-value table, `S` ‖ token (16 bytes) →
//!   version 1, channel (4), expiry (8, Unix seconds, 0 never), creator account
//!   (16), access token (the rest, UTF-8).

use crate::state::State;
use nuschl_protocol::{
    ControlFrame,
    nuschl::{
        self, Id, MAX_ACCESS_TOKEN_BYTES, kind,
        v1::{self, error::Code},
    },
};
use prost::Message as _;
use std::{collections::HashMap, time::Instant};

const PREFIX: u8 = b'S';
const VERSION: u8 = 1;
pub const TOKEN_BYTES: usize = 16;
pub const MAX_PER_CHANNEL: usize = 64;
pub const MAX_PER_ACCOUNT: usize = 256;

type Failure = (Code, u64);
type Token = [u8; TOKEN_BYTES];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub channel: u32,
    pub expires_at: u64,
    pub creator: Id,
    pub access_token: String,
}

/// Every link, by token.
#[derive(Default)]
pub struct Links(HashMap<Token, Link>);

fn key(token: &Token) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + TOKEN_BYTES);
    key.push(PREFIX);
    key.extend_from_slice(token);
    key
}

pub fn encode(link: &Link) -> Vec<u8> {
    let mut value = Vec::with_capacity(29 + link.access_token.len());
    value.push(VERSION);
    value.extend_from_slice(&link.channel.to_be_bytes());
    value.extend_from_slice(&link.expires_at.to_be_bytes());
    value.extend_from_slice(&link.creator);
    value.extend_from_slice(link.access_token.as_bytes());
    value
}

pub fn decode(value: &[u8]) -> Option<Link> {
    let (&VERSION, rest) = value.split_first()? else {
        return None;
    };
    let channel = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
    let expires_at = u64::from_be_bytes(rest.get(4..12)?.try_into().ok()?);
    let creator: Id = rest.get(12..28)?.try_into().ok()?;
    let access_token = std::str::from_utf8(rest.get(28..)?).ok()?;
    (access_token.len() <= MAX_ACCESS_TOKEN_BYTES).then(|| Link {
        channel,
        expires_at,
        creator,
        access_token: access_token.to_owned(),
    })
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Links {
    /// Every stored link that has not expired.
    pub async fn load(storage: &crate::storage::StorageHandle) -> Option<Self> {
        use nuschl_ipc::server::{ServerStorageRequest as Request, ServerStorageResponse};
        let records = match storage
            .request(Request::KtScan {
                prefix: base64(&[PREFIX]),
            })
            .await?
        {
            ServerStorageResponse::KtRecords(records) => records,
            _ => return None,
        };
        let now = now_s();
        let mut links = HashMap::new();
        for (key, value) in records {
            let (Some(key), Some(value)) = (unbase64(&key), unbase64(&value)) else {
                continue;
            };
            let Some(token) = key.get(1..).and_then(|token| Token::try_from(token).ok()) else {
                continue;
            };
            if let Some(link) = decode(&value).filter(|link| !expired(link, now)) {
                links.insert(token, link);
            }
        }
        Some(Self(links))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, token: &Token) -> Option<&Link> {
        self.0.get(token)
    }

    pub fn insert(&mut self, token: Token, link: Link) {
        self.0.insert(token, link);
    }
}

fn expired(link: &Link, now: u64) -> bool {
    link.expires_at != 0 && link.expires_at <= now
}

fn token(bytes: &[u8]) -> Result<Token, Failure> {
    bytes.try_into().map_err(|_| (Code::Malformed, 0))
}

impl State {
    /// A `share/1` frame (the dispatcher checked the capability).
    pub fn share_frame(&mut self, session: u32, frame: &ControlFrame) {
        let request_id = v1::Done::decode(frame.payload.clone()).map_or(0, |done| done.request_id);
        let result = match frame.kind {
            kind::SHARE_LINK_CREATE => v1::ShareLinkCreate::decode(frame.payload.clone())
                .map_err(|_| (Code::Malformed, 0))
                .and_then(|message| self.share_create(session, message)),
            kind::SHARE_LINK_REVOKE => v1::ShareLinkRevoke::decode(frame.payload.clone())
                .map_err(|_| (Code::Malformed, 0))
                .and_then(|message| self.share_revoke(session, message)),
            kind::SHARE_LINK_RESOLVE => v1::ShareLinkResolve::decode(frame.payload.clone())
                .map_err(|_| (Code::Malformed, 0))
                .and_then(|message| self.share_resolve(session, message)),
            _ => Err((Code::Unsupported, 0)),
        };
        if let Err(failure) = result {
            self.extension_error(session, request_id, failure);
        }
    }

    fn share_limited(&mut self, session: u32) -> Result<(), Failure> {
        let limited = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
            .is_none_or(|extension| extension.requests.limited(Instant::now()));
        if limited {
            return Err((Code::RateLimited, 1000));
        }
        Ok(())
    }

    fn share_create(&mut self, session: u32, message: v1::ShareLinkCreate) -> Result<(), Failure> {
        use crate::authz::Action;
        self.share_limited(session)?;
        let creator = self
            .users
            .get(&session)
            .filter(|user| user.user_id.is_some())
            .and_then(|user| user.account)
            .ok_or((Code::NotFound, 0))?;
        let channel = message.channel;
        if self.channels.get(channel).is_none() {
            return Err((Code::NotFound, 0));
        }
        self.authorize(session, Action::Enter { channel })?;
        let access_token = message.access_token.trim().to_owned();
        if access_token.len() > MAX_ACCESS_TOKEN_BYTES || access_token.contains(char::is_control) {
            return Err((Code::Malformed, 0));
        }
        if !access_token.is_empty() {
            self.authorize(session, Action::EditAcl { channel })?;
        }
        let now = now_s();
        if message.expires_at != 0 && message.expires_at <= now {
            return Err((Code::Malformed, 0));
        }
        self.share_forget_expired(now);
        let links = &self.share_links.0;
        if links
            .values()
            .filter(|link| link.channel == channel)
            .count()
            >= MAX_PER_CHANNEL
            || links
                .values()
                .filter(|link| link.creator == creator)
                .count()
                >= MAX_PER_ACCOUNT
        {
            return Err((Code::LimitReached, 0));
        }
        let token: Token = crate::state::random();
        let link = Link {
            channel,
            expires_at: message.expires_at,
            creator,
            access_token,
        };
        self.storage
            .send(nuschl_ipc::server::ServerStorageRequest::KtWrite {
                records: vec![(base64(&key(&token)), base64(&encode(&link)))],
            });
        tracing::info!(actor = %self.log_name(session), channel, expires_at = link.expires_at, private = !link.access_token.is_empty(), "created a share link");
        self.send(
            session,
            kind::SHARE_LINK,
            &self.share_reply(message.request_id, &token, &link),
        );
        self.share_links.insert(token, link);
        Ok(())
    }

    fn share_revoke(&mut self, session: u32, message: v1::ShareLinkRevoke) -> Result<(), Failure> {
        use crate::authz::Action;
        self.share_limited(session)?;
        let token = token(&message.token)?;
        let link = self.share_links.get(&token).ok_or((Code::NotFound, 0))?;
        let account = self.users.get(&session).and_then(|user| user.account);
        if account != Some(link.creator) {
            self.authorize(
                session,
                Action::EditAcl {
                    channel: link.channel,
                },
            )?;
        }
        self.share_links.0.remove(&token);
        self.share_delete(&token);
        tracing::info!(actor = %self.log_name(session), "revoked a share link");
        self.send(
            session,
            kind::DONE,
            &v1::Done {
                request_id: message.request_id,
            },
        );
        Ok(())
    }

    fn share_resolve(
        &mut self,
        session: u32,
        message: v1::ShareLinkResolve,
    ) -> Result<(), Failure> {
        self.share_limited(session)?;
        let token = token(&message.token)?;
        let now = now_s();
        let link = match self.share_links.get(&token) {
            Some(link) if expired(link, now) => {
                self.share_links.0.remove(&token);
                self.share_delete(&token);
                return Err((Code::NotFound, 0));
            }
            Some(link) if self.channels.get(link.channel).is_some() => link.clone(),
            _ => return Err((Code::NotFound, 0)),
        };
        self.send(
            session,
            kind::SHARE_LINK,
            &self.share_reply(message.request_id, &token, &link),
        );
        Ok(())
    }

    fn share_reply(&self, request_id: u64, token: &Token, link: &Link) -> v1::ShareLink {
        v1::ShareLink {
            request_id,
            token: token.to_vec(),
            channel: link.channel,
            expires_at: link.expires_at,
            access_token: link.access_token.clone(),
            web_host: self.settings.config.share_links.web_host.clone(),
        }
    }

    fn share_delete(&self, token: &Token) {
        // An empty value deletes the key.
        self.storage
            .send(nuschl_ipc::server::ServerStorageRequest::KtWrite {
                records: vec![(base64(&key(token)), String::new())],
            });
    }

    /// Drop expired links (on creation, so limits count live links only).
    fn share_forget_expired(&mut self, now: u64) {
        let gone: Vec<Token> = self
            .share_links
            .0
            .iter()
            .filter(|(_, link)| expired(link, now))
            .map(|(token, _)| *token)
            .collect();
        for token in gone {
            self.share_links.0.remove(&token);
            self.share_delete(&token);
        }
    }
}

/// Whether `host` may be announced as the web host for links.
pub fn valid_web_host(host: &str) -> bool {
    host.is_empty()
        || (host.len() <= nuschl::MAX_WEB_HOST_BYTES
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_garbage() {
        let link = Link {
            channel: 7,
            expires_at: 1_900_000_000,
            creator: [3; 16],
            access_token: "secret token".into(),
        };
        assert_eq!(decode(&encode(&link)), Some(link.clone()));
        let mut wrong = encode(&link);
        wrong[0] = 2;
        assert_eq!(decode(&wrong), None);
        assert_eq!(decode(&encode(&link)[..20]), None);
        assert_eq!(decode(&[]), None);
        let mut long = encode(&link);
        long.extend(std::iter::repeat_n(b'x', MAX_ACCESS_TOKEN_BYTES));
        assert_eq!(decode(&long), None);
        assert!(expired(
            &Link {
                expires_at: 5,
                ..link.clone()
            },
            5
        ));
        assert!(!expired(
            &Link {
                expires_at: 0,
                ..link
            },
            5
        ));
        assert!(valid_web_host("web.example.org") && valid_web_host(""));
        assert!(!valid_web_host("web/x") && !valid_web_host("a..b"));
    }
}
