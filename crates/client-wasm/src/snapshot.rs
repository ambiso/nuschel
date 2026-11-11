// SPDX-License-Identifier: EUPL-1.2
//! What JavaScript sees: plain records serialised as JSON, built from the
//! SDK's state (read in place) and its updates.

use nuschel_client::{
    Update,
    model::TextTarget,
    server::{ServerState, Status},
};
use nuschel_core::bookmark::ServerBookmark;
use serde::Serialize;

#[derive(Serialize)]
pub struct ServerView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// `disconnected`, `connecting` or `connected`.
    pub status: &'static str,
    pub notice: Option<String>,
    /// The certificate waiting for the user's decision.
    pub certificate: Option<String>,
    pub own_session: Option<u32>,
    pub channels: Vec<ChannelView>,
    pub users: Vec<UserView>,
}

#[derive(Serialize)]
pub struct ChannelView {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
}

#[derive(Serialize)]
pub struct UserView {
    pub session: u32,
    pub name: String,
    pub channel: u32,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateView {
    Browse { server: String, channel: u32 },
    LoginRefused { server: String, reason: String },
    Negotiated { server: String },
    Changed,
}

pub fn servers(servers: &[ServerState]) -> Vec<ServerView> {
    servers
        .iter()
        .map(|server| ServerView {
            id: server.id.clone(),
            name: server.name.clone(),
            host: server.bookmark.host.clone(),
            port: server.bookmark.port,
            username: server.bookmark.username.clone(),
            status: match server.status {
                Status::Disconnected => "disconnected",
                Status::Connecting => "connecting",
                Status::Connected => "connected",
            },
            notice: server.notice.clone(),
            certificate: server
                .certificate
                .as_ref()
                .map(|prompt| prompt.sha256.clone()),
            own_session: server.own_session,
            channels: server
                .channels
                .iter()
                .filter(|channel| matches!(channel.target, TextTarget::Channel(_)))
                .map(|channel| ChannelView {
                    id: channel.id,
                    parent: channel.parent,
                    name: channel.name.clone(),
                })
                .collect(),
            users: server
                .users
                .values()
                .map(|user| UserView {
                    session: user.session,
                    name: user.name.clone(),
                    channel: user.channel,
                })
                .collect(),
        })
        .collect()
}

pub fn update(update: &Update) -> Option<UpdateView> {
    Some(match update {
        Update::Browse { server, channel } => UpdateView::Browse {
            server: server.clone(),
            channel: *channel,
        },
        Update::LoginRefused { server, reason, .. } => UpdateView::LoginRefused {
            server: server.clone(),
            reason: reason.clone(),
        },
        Update::Negotiated { server } => UpdateView::Negotiated {
            server: server.clone(),
        },
        _ => UpdateView::Changed,
    })
}

/// A saved server for the browser (its id is its address and name; the web
/// client's storage comes later).
pub fn bookmark(
    name: &str,
    host: &str,
    port: u16,
    username: &str,
    pin: Option<String>,
) -> Result<ServerBookmark, String> {
    let host = host.trim().to_ascii_lowercase();
    let username = username.trim();
    if !nuschel_protocol::tunnel::valid_host(&host) || port == 0 {
        return Err("Enter a hostname or IP address and a port from 1 to 65535.".into());
    }
    if username.is_empty() || username.len() > 128 || username.contains(char::is_control) {
        return Err("Enter a username of at most 128 bytes without control characters.".into());
    }
    let pin = pin
        .map(|pin| pin.trim().to_ascii_lowercase())
        .filter(|pin| !pin.is_empty());
    if pin
        .as_ref()
        .is_some_and(|pin| pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("A certificate pin is 64 hex digits (SHA-256).".into());
    }
    Ok(ServerBookmark {
        id: format!("web:{username}@{host}:{port}"),
        name: if name.trim().is_empty() {
            host.clone()
        } else {
            name.trim().chars().take(128).collect()
        },
        host,
        port,
        username: username.into(),
        certificate_sha256: pin,
        cached_channels: Vec::new(),
        tokens: Vec::new(),
        auto_connect: false,
        icon: None,
        icon_checked_ms: 0,
        last_channel: None,
        managed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_are_checked() {
        let saved = bookmark("", "Voice.Example.org", 64738, "ada", None).expect("valid");
        assert_eq!(saved.name, "voice.example.org");
        assert_eq!(saved.id, "web:ada@voice.example.org:64738");
        assert!(bookmark("", "a b", 64738, "ada", None).is_err());
        assert!(bookmark("", "a.b", 0, "ada", None).is_err());
        assert!(bookmark("", "a.b", 1, " ", None).is_err());
        assert!(bookmark("", "a.b", 1, "ada", Some("12".into())).is_err());
        assert!(bookmark("", "a.b", 1, "ada", Some("AB".repeat(32))).is_ok());
    }
}
