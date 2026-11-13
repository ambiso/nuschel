// SPDX-License-Identifier: EUPL-1.2
//! What JavaScript sees: plain records serialised as JSON, built from the
//! SDK's state (read in place) and its updates.

use nuschl_client::{
    Update,
    model::TextTarget,
    server::{ServerState, Status},
};
use nuschl_core::bookmark::ServerBookmark;
use serde::Serialize;

#[derive(Serialize)]
pub struct ServerView {
    /// Stable command index, independent of this snapshot's display position.
    pub runtime_index: usize,
    pub id: String,
    pub generation: String,
    /// Filled by the browser catalog owner; zero is never a valid entry lease.
    pub catalog_instance: String,
    pub own_instance: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// `disconnected`, `connecting` or `connected`.
    pub status: &'static str,
    pub notice: Option<String>,
    /// Opaque current welcome lease; the document arrives once in an update.
    pub welcome_token: Option<String>,
    /// The certificate waiting for the user's decision.
    pub certificate: Option<String>,
    pub own_session: Option<u32>,
    pub own_channel: Option<u32>,
    pub muted: bool,
    pub deafened: bool,
    pub voice_supported: bool,
    pub opus_available: Option<bool>,
    pub capabilities: Vec<String>,
    pub permissions: std::collections::BTreeMap<u32, u32>,
    pub pin: Option<String>,
    pub channels: Vec<ChannelView>,
    pub users: Vec<UserView>,
}

#[derive(Serialize)]
pub struct ChannelView {
    pub id: u32,
    pub instance: String,
    pub peer: Option<crate::actions::PeerRef>,
    pub parent: Option<u32>,
    pub direct: bool,
    pub name: String,
}

#[derive(Serialize)]
pub struct UserView {
    pub session: u32,
    pub instance: String,
    pub name: String,
    pub channel: u32,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub talking: bool,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateView {
    Browse {
        server: String,
        channel: u32,
    },
    LoginRefused {
        server: String,
        reason: String,
    },
    Negotiated {
        server: String,
    },
    Sent {
        server: String,
        channel: u32,
        body: String,
    },
    Error {
        message: String,
    },
    Welcome {
        server: String,
        token: String,
        document: nuschl_core::MessageDocument,
        content_token: Option<String>,
    },
    Changed,
}

pub fn servers(servers: &[ServerState]) -> Vec<ServerView> {
    server_views(servers.iter().enumerate())
}

pub fn ordered_servers(client: &nuschl_client::Client) -> Vec<ServerView> {
    server_views(client.ordered_servers())
}

fn server_views<'a>(servers: impl Iterator<Item = (usize, &'a ServerState)>) -> Vec<ServerView> {
    servers
        .map(|(runtime_index, server)| ServerView {
            runtime_index,
            id: server.id.clone(),
            generation: server.generation.to_string(),
            catalog_instance: "0".into(),
            own_instance: server
                .own_session
                .and_then(|id| server.users.get(&id))
                .map(|user| user.instance.to_string()),
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
            welcome_token: None,
            certificate: server
                .certificate
                .as_ref()
                .map(|prompt| prompt.sha256.clone()),
            own_session: server.own_session,
            own_channel: server.voice_channel(),
            muted: server.muted,
            deafened: server.deafened,
            voice_supported: server.has_mumble_voice(),
            opus_available: server.opus_available,
            capabilities: server.capabilities.clone(),
            permissions: server.permissions.clone(),
            pin: server.bookmark.certificate_sha256.clone(),
            channels: server
                .channels
                .iter()
                .map(|channel| ChannelView {
                    id: channel.id,
                    instance: channel.instance.to_string(),
                    peer: channel
                        .direct_peer
                        .as_ref()
                        .map(|user| crate::actions::PeerRef {
                            session: user.session,
                            instance: user.instance.to_string(),
                        }),
                    parent: channel.parent,
                    direct: !matches!(channel.target, TextTarget::Channel(_)),
                    name: channel.name.clone(),
                })
                .collect(),
            users: server
                .users
                .values()
                .map(|user| UserView {
                    session: user.session,
                    instance: user.instance.to_string(),
                    name: user.name.clone(),
                    channel: user.channel,
                    self_mute: user.self_muted || user.muted || user.suppressed,
                    self_deaf: user.self_deafened || user.deafened,
                    talking: server.talking.contains(&user.session),
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
        Update::Sent { conversation, body } => UpdateView::Sent {
            server: conversation.server.clone(),
            channel: conversation.channel,
            body: body.clone(),
        },
        Update::HistoryFailed(message) => UpdateView::Error {
            message: message.clone(),
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
    if !nuschl_protocol::tunnel::valid_host(&host) || port == 0 {
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
    fn lifetime_metadata_never_rounds_u64_through_javascript_numbers() {
        let mut server =
            ServerState::new(bookmark("owned", "127.0.0.1", 64738, "fixture", None).unwrap());
        server.generation = u64::MAX - 1;
        server.own_session = Some(7);
        server.users.insert(
            7,
            nuschl_client::model::User {
                session: 7,
                instance: u64::MAX - 2,
                ..Default::default()
            },
        );
        server.channels[0].instance = u64::MAX - 3;
        let views = servers(&[server]);
        let view = &views[0];
        assert_eq!(view.generation, (u64::MAX - 1).to_string());
        assert_eq!(view.own_instance, Some((u64::MAX - 2).to_string()));
        assert_eq!(view.users[0].instance, (u64::MAX - 2).to_string());
        assert_eq!(view.channels[0].instance, (u64::MAX - 3).to_string());
        assert_eq!(
            view.catalog_instance, "0",
            "only the browser catalog owner can fill a valid entry lifetime"
        );
    }

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

#[derive(Serialize)]
pub struct TimelineView {
    pub content_context: Option<String>,
    pub instance: String,
    pub peer: Option<crate::actions::PeerRef>,
    pub server: String,
    pub channel: u32,
    pub name: String,
    pub loading: bool,
    pub has_older: bool,
    pub sending: bool,
    pub messages: Vec<MessageView>,
}
#[derive(Serialize)]
pub struct MessageView {
    pub serial: String,
    pub revision: u32,
    pub content_token: Option<String>,
    pub content_failed: bool,
    pub author: String,
    pub at_ms: i64,
    pub outgoing: bool,
    pub status: Option<&'static str>,
    pub document: nuschl_core::MessageDocument,
}
pub fn timeline(client: &nuschl_client::Client) -> Option<TimelineView> {
    let (server, channel) = client.selected_indices()?;
    let server = &client.servers[server];
    let channel = &server.channels[channel];
    Some(TimelineView {
        content_context: None,
        instance: channel.instance.to_string(),
        peer: channel
            .direct_peer
            .as_ref()
            .map(|user| crate::actions::PeerRef {
                session: user.session,
                instance: user.instance.to_string(),
            }),
        server: server.id.clone(),
        channel: channel.id,
        name: channel.name.clone(),
        loading: channel.timeline.loading,
        has_older: channel.timeline.next_before.is_some(),
        sending: client.sending(),
        messages: channel
            .timeline
            .messages
            .iter()
            .map(|message| MessageView {
                serial: message.serial.to_string(),
                revision: message.revision,
                content_token: None,
                content_failed: false,
                author: message.author.clone(),
                at_ms: message.observed_at_ms,
                outgoing: message.outgoing,
                status: message.status.label(),
                document: message.document.clone(),
            })
            .collect(),
    })
}
