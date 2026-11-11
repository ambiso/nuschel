// SPDX-License-Identifier: EUPL-1.2
//! Typing indicators on Nuschel servers (`typing/1`, docs/extension.md §7).
//!
//! While the composer changes, Nuschel says so at most every three seconds
//! (the server forwards at most one a conversation in that time and drops the
//! rest). Nothing more is sent once the message is sent, the composer is
//! emptied or editing stops: the protocol has no "stopped typing", so others
//! drop the indicator when that person's message arrives, or six seconds
//! after the last one. Only conversations the server named and we may write
//! in get one; the server forwards it only to those who may read there.
//! Settings → Privacy turns sending off; others' indicators still show.
use crate::state::{ChannelView, Desktop, ServerView};
use gpui_kit::*;
use nuschel_session::{
    connection::{Command, NuschelRequest, TextTarget},
    model::permission as perm,
};
use std::time::{Duration, Instant};

/// How often we say we are typing: the server's limit per conversation.
pub const TYPING_EVERY: Duration = Duration::from_secs(3);
/// How long an indicator stays without a new one (two missed in a row).
pub const TYPING_SHOWN: Duration = Duration::from_secs(6);

/// The line under the composer: who is typing here, or nothing.
pub fn typing_line(
    server: &ServerView,
    channel: &ChannelView,
    settings: &nuschel_session::config::ClientSettings,
    now: Instant,
) -> Option<String> {
    let mut names: Vec<String> = channel
        .typing
        .iter()
        .filter(|(session, at)| {
            Some(**session) != server.own_session && now.duration_since(**at) < TYPING_SHOWN
        })
        .filter_map(|(session, _)| server.users.get(session))
        .map(|user| crate::names::display_name(user, settings).to_string())
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    match names.as_slice() {
        [] => None,
        [one] => Some(format!("{one} is typing…")),
        [one, two] => Some(format!("{one} and {two} are typing…")),
        _ => Some("Several people are typing…".into()),
    }
}

impl ServerView {
    /// The conversation to say we type in, if this one gets indicators.
    pub fn typing_conversation(&self, channel: &ChannelView) -> Option<[u8; 16]> {
        if !self.has("typing/1") || !self.archived(channel) {
            return None;
        }
        // The server forwards it only from those who may write there.
        if let TextTarget::Channel(id) = channel.target
            && !self.may_try(id, perm::TEXT_MESSAGE)
        {
            return None;
        }
        channel.conversation
    }
}

impl Desktop {
    /// The composer changed: say we are typing, now and then.
    pub fn composer_changed(&mut self, cx: &mut Context<Self>) {
        let empty = self.composer.read(cx).value().trim().is_empty();
        if empty || self.editing.is_some() || !self.settings.send_typing {
            self.typing_sent = None;
            return;
        }
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        let view = &self.servers[server];
        let channel = &view.channels[channel];
        let Some(conversation) = view.typing_conversation(channel) else {
            return;
        };
        let key = (view.id.clone(), channel.id);
        let now = Instant::now();
        if self
            .typing_sent
            .as_ref()
            .is_some_and(|(sent, at)| *sent == key && now.duration_since(*at) < TYPING_EVERY)
        {
            return;
        }
        let typing = nuschel_protocol::nuschel::v1::Typing {
            conversation: conversation.to_vec(),
            session: 0,
        };
        let command = Command::Nuschel(NuschelRequest::Typing(typing));
        if self.backend.command(&view.id, command).is_ok() {
            self.typing_sent = Some((key, now));
        }
    }

    /// Someone types in a conversation of server `index`.
    pub fn someone_typing(&mut self, index: usize, conversation: &[u8], session: u32) {
        let server = &mut self.servers[index];
        if server.own_session == Some(session) || !server.users.contains_key(&session) {
            return;
        }
        if let Some(channel) = server
            .channels
            .iter_mut()
            .find(|channel| channel.conversation.is_some_and(|id| id == conversation))
            && (channel.typing.len() < 64 || channel.typing.contains_key(&session))
        {
            channel.typing.insert(session, Instant::now());
        }
    }

    /// Indicators fade on their own: while any is shown, look again each
    /// second and drop those that are over.
    pub fn expire_typing(&mut self, cx: &mut Context<Self>) {
        if self.typing_timer {
            return;
        }
        let any = |this: &Self| {
            this.servers.iter().any(|server| {
                server
                    .channels
                    .iter()
                    .any(|channel| !channel.typing.is_empty())
            })
        };
        if !any(self) {
            return;
        }
        self.typing_timer = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let more = this.update(cx, |this, cx| {
                    let now = Instant::now();
                    let mut changed = false;
                    for server in &mut this.servers {
                        for channel in &mut server.channels {
                            let before = channel.typing.len();
                            channel
                                .typing
                                .retain(|_, at| now.duration_since(*at) < TYPING_SHOWN);
                            changed |= channel.typing.len() != before;
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                    let more = any(this);
                    this.typing_timer = more;
                    more
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}

/// A polite live region for screen readers around `child`: a status whose
/// changes (who is typing) they announce without interrupting. GPUI's
/// elements cannot set AccessKit's `live` property themselves.
pub struct LiveStatus {
    pub id: ElementId,
    pub label: Option<SharedString>,
    pub child: AnyElement,
}

impl IntoElement for LiveStatus {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for LiveStatus {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(Role::Status)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        if let Some(label) = &self.label {
            node.set_label(label.to_string());
        }
        node.set_live(accesskit::Live::Polite);
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{TYPING_SHOWN, typing_line};
    use crate::state::{ChannelView, ServerView};
    use nuschel_session::{config::ClientSettings, connection::User};
    use std::time::{Duration, Instant};

    #[test]
    fn the_line_names_one_or_two_people_and_counts_more() {
        let mut server = ServerView::new_for_tests();
        server.own_session = Some(1);
        for (session, name) in [(1, "Me"), (2, "Bob"), (3, "ada"), (4, "Cy")] {
            server.users.insert(
                session,
                User {
                    session,
                    name: name.into(),
                    ..Default::default()
                },
            );
        }
        let settings = ClientSettings::default();
        let now = Instant::now();
        let mut channel = ChannelView::default();
        assert_eq!(typing_line(&server, &channel, &settings, now), None);
        // Our own session never shows, and an old indicator is over.
        channel.typing.insert(1, now);
        channel
            .typing
            .insert(4, now - TYPING_SHOWN - Duration::from_millis(1));
        assert_eq!(typing_line(&server, &channel, &settings, now), None);
        channel.typing.insert(2, now);
        assert_eq!(
            typing_line(&server, &channel, &settings, now).as_deref(),
            Some("Bob is typing…")
        );
        channel.typing.insert(3, now);
        assert_eq!(
            typing_line(&server, &channel, &settings, now).as_deref(),
            Some("ada and Bob are typing…")
        );
        channel.typing.insert(4, now);
        assert_eq!(
            typing_line(&server, &channel, &settings, now).as_deref(),
            Some("Several people are typing…")
        );
    }
}
