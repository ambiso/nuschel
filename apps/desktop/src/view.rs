// SPDX-License-Identifier: EUPL-1.2
use std::collections::BTreeSet;

use gpui_kit::{prelude::*, *};
use nuschel_client::{Conversation, HistorySource, Navigation, SessionSecurity};
use nuschel_core::{ContentBlock, MessageDocument};

pub(super) struct MessageView {
    pub author: String,
    pub time: String,
    pub document: MessageDocument,
    pub expanded: bool,
    pub has_more: bool,
}

pub(super) struct ChannelView {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub topic: String,
    pub messages: Vec<MessageView>,
}

pub(super) struct ServerView {
    pub id: String,
    pub name: String,
    pub initials: String,
    pub history_source: HistorySource,
    pub security: SessionSecurity,
    pub channels: Vec<ChannelView>,
}

pub struct Desktop {
    servers: Vec<ServerView>,
    navigation: Navigation,
    collapsed: BTreeSet<(String, u32)>,
    history_enabled: bool,
}

impl Desktop {
    pub fn preview(document: MessageDocument, has_more: bool, history_enabled: bool) -> Self {
        let mut desktop = Self {
            servers: crate::preview::servers(document, has_more),
            navigation: Navigation::default(),
            collapsed: BTreeSet::new(),
            history_enabled,
        };
        desktop.browse_server(0);
        desktop
    }

    fn selected_indices(&self) -> (usize, usize) {
        let selected = self
            .navigation
            .selection()
            .expect("preview has a selection");
        let server = self
            .servers
            .iter()
            .position(|s| s.id == selected.server)
            .unwrap_or(0);
        let channel = self.servers[server]
            .channels
            .iter()
            .position(|c| c.id == selected.channel)
            .unwrap_or(0);
        (server, channel)
    }

    fn browse_server(&mut self, index: usize) {
        let server = &self.servers[index];
        let channel = self
            .navigation
            .remembered_channel(&server.id)
            .filter(|id| server.channels.iter().any(|channel| channel.id == *id))
            .unwrap_or(server.channels[0].id);
        self.browse_channel(index, channel);
    }

    fn browse_channel(&mut self, server: usize, channel: u32) {
        let server = &self.servers[server];
        if server.channels.iter().any(|entry| entry.id == channel) {
            self.navigation.browse(
                Conversation {
                    server: server.id.clone(),
                    channel,
                },
                server.history_source,
            );
        }
    }

    fn toggle_message(&mut self, index: usize) {
        let (server, channel) = self.selected_indices();
        if let Some(message) = self.servers[server].channels[channel]
            .messages
            .get_mut(index)
        {
            message.expanded = !message.expanded;
        }
    }

    /// Iterative traversal keeps arbitrary channel nesting off the Rust call stack.
    fn channel_rows(&self, server: usize) -> Vec<(usize, usize, bool)> {
        let server = &self.servers[server];
        let mut pending = server
            .channels
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, channel)| channel.parent.is_none())
            .map(|(index, _)| (index, 0))
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        let mut rows = Vec::new();
        while let Some((index, depth)) = pending.pop() {
            let channel = &server.channels[index];
            if !visited.insert(channel.id) {
                continue;
            }
            let children = server
                .channels
                .iter()
                .enumerate()
                .filter(|(_, child)| child.parent == Some(channel.id))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            rows.push((index, depth, !children.is_empty()));
            if !self.collapsed.contains(&(server.id.clone(), channel.id)) {
                pending.extend(children.into_iter().rev().map(|index| (index, depth + 1)));
            }
        }
        rows
    }
}

impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (server_index, channel_index) = self.selected_indices();
        let server = &self.servers[server_index];
        let channel = &server.channels[channel_index];
        let server_buttons = self
            .servers
            .iter()
            .enumerate()
            .map(|(index, server)| {
                let selected = index == server_index;
                div()
                    .id(("server", index))
                    .role(Role::Button)
                    .tab_index(0)
                    .aria_label(format!(
                        "{}; {}",
                        server.name,
                        security_label(server.security)
                    ))
                    .aria_selected(selected)
                    .relative()
                    .w(px(48.))
                    .h(px(48.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_xl()
                    .bg(rgb(if selected { 0x5865f2 } else { 0x282d39 }))
                    .hover(|style| style.bg(rgb(0x4a55cf)))
                    .cursor_pointer()
                    .font_weight(FontWeight::BOLD)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browse_server(index);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if activation_key(event) {
                            this.browse_server(index);
                            cx.notify();
                        }
                    }))
                    .child(text!(server.initials.clone()).with_id(("server-name", index)))
                    .child(
                        div()
                            .absolute()
                            .right(px(-3.))
                            .bottom(px(-3.))
                            .p_1()
                            .rounded_md()
                            .bg(rgb(0x11151d))
                            .child(security_icon(server.security, index)),
                    )
            })
            .collect::<Vec<_>>();

        let channel_rows = self
            .channel_rows(server_index)
            .into_iter()
            .map(|(index, depth, has_children)| {
                let entry = &server.channels[index];
                let channel_id = entry.id;
                let selected = index == channel_index;
                let collapsed = self.collapsed.contains(&(server.id.clone(), channel_id));
                let server_id = server.id.clone();
                div()
                    .id(("channel-row", index))
                    .role(Role::TreeItem)
                    .tab_index(0)
                    .aria_label(entry.name.clone())
                    .aria_selected(selected)
                    .aria_level(depth + 1)
                    .when(has_children, |row| row.aria_expanded(!collapsed))
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_h(px(34.))
                    .pr_2()
                    .pl(px(8. + depth as f32 * 14.))
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(if selected { 0xf4f5fa } else { 0xadb5c6 }))
                    .when(selected, |row| row.bg(rgb(0x333b50)))
                    .hover(|style| style.bg(rgb(0x2a3142)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browse_channel(server_index, channel_id);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if activation_key(event) {
                            this.browse_channel(server_index, channel_id);
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .id(("channel-branch", index))
                            .w(px(12.))
                            .text_color(rgb(0x7f8ba3))
                            .when(has_children, |branch| {
                                branch
                                    .role(Role::Button)
                                    .tab_index(0)
                                    .aria_label(format!(
                                        "{} {}",
                                        if collapsed { "Expand" } else { "Collapse" },
                                        entry.name
                                    ))
                                    .on_click(cx.listener({
                                        let server_id = server_id.clone();
                                        move |this, _, _, cx| {
                                            let key = (server_id.clone(), channel_id);
                                            if !this.collapsed.remove(&key) {
                                                this.collapsed.insert(key);
                                            }
                                            cx.stop_propagation();
                                            cx.notify();
                                        }
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if activation_key(event) {
                                                let key = (server_id.clone(), channel_id);
                                                if !this.collapsed.remove(&key) {
                                                    this.collapsed.insert(key);
                                                }
                                                cx.stop_propagation();
                                                cx.notify();
                                            }
                                        },
                                    ))
                            })
                            .child(
                                text!(if has_children {
                                    if collapsed { "▸" } else { "▾" }
                                } else {
                                    "#"
                                })
                                .with_id(("channel-symbol", index)),
                            ),
                    )
                    .child(text!(entry.name.clone()).with_id(("channel-name", index)))
            })
            .collect::<Vec<_>>();

        let messages = channel
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                div()
                    .id(("message", index))
                    .flex()
                    .gap_3()
                    .py_3()
                    .child(
                        div()
                            .w(px(34.))
                            .h(px(34.))
                            .flex_shrink_0()
                            .rounded_lg()
                            .bg(rgb(0x364364))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                text!(message.author.chars().next().unwrap_or('?').to_string())
                                    .with_id(("avatar", index)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(
                                        text!(message.author.clone()).with_id(("author", index)),
                                    ))
                                    .child(div().text_xs().text_color(rgb(0x8b95aa)).child(
                                        text!(message.time.clone()).with_id(("time", index)),
                                    )),
                            )
                            .child(
                                div()
                                    .id(("message-content", index))
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .when(!message.expanded, |content| {
                                        content.max_h(px(300.)).overflow_hidden()
                                    })
                                    .children(document_blocks(&message.document)),
                            )
                            .when(message.has_more, |content| {
                                content.child(
                                    div()
                                        .id(("show-more", index))
                                        .role(Role::Button)
                                        .tab_index(0)
                                        .aria_expanded(message.expanded)
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(rgb(0x9bb6ff))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_message(index);
                                            cx.notify();
                                        }))
                                        .on_key_down(cx.listener(
                                            move |this, event: &KeyDownEvent, _, cx| {
                                                if activation_key(event) {
                                                    this.toggle_message(index);
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .child(
                                            text!(if message.expanded {
                                                "Show less"
                                            } else {
                                                "Show more"
                                            })
                                            .with_id(("expand-label", index)),
                                        ),
                                )
                            }),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .size_full()
            .flex()
            .bg(rgb(0x202632))
            .text_color(rgb(0xe9edf5))
            .font_family("sans-serif")
            .text_base()
            .child(
                div()
                    .w(px(72.))
                    .h_full()
                    .flex_shrink_0()
                    .py_4()
                    .gap_4()
                    .bg(rgb(0x11151d))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8f9bb3))
                            .child(text!("N").with_id("brand")),
                    )
                    .children(server_buttons),
            )
            .child(
                div()
                    .w(px(208.))
                    .h_full()
                    .flex_shrink_0()
                    .bg(rgb(0x191f2a))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(72.))
                            .flex_shrink_0()
                            .px_4()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap_1()
                            .border_b_1()
                            .border_color(rgb(0x2a3243))
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .child(text!(server.name.clone()).with_id("server-heading")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x929eb4))
                                    .child(text!("Preview · Not connected").with_id("connection")),
                            ),
                    )
                    .child(
                        div()
                            .px_4()
                            .pt_5()
                            .pb_2()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0x8f9ab0))
                            .child(text!("CHANNELS").with_id("channels-heading")),
                    )
                    .child(
                        div()
                            .id("channel-tree")
                            .role(Role::Tree)
                            .aria_label("Channels")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(channel_rows),
                    )
                    .child(
                        div()
                            .p_4()
                            .border_t_1()
                            .border_color(rgb(0x2a3243))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .child(text!("Voice disconnected").with_id("voice-status")),
                            )
                            .child(div().text_xs().text_color(rgb(0x8f9ab0)).child(
                                text!("Browse chat without joining voice").with_id("voice-hint"),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(72.))
                            .flex_shrink_0()
                            .px_6()
                            .flex()
                            .items_center()
                            .gap_4()
                            .border_b_1()
                            .border_color(rgb(0x30394b))
                            .child(
                                div()
                                    .text_color(rgb(0x8592ad))
                                    .text_xl()
                                    .child(text!("#").with_id("channel-hash")),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(
                                        text!(channel.name.clone()).with_id("channel-heading"),
                                    ))
                                    .child(div().text_xs().text_color(rgb(0x9aa6bc)).child(
                                        text!(channel.topic.clone()).with_id("channel-topic"),
                                    )),
                            )
                            .child(security_icon(
                                server.security,
                                self.servers.len() + server_index,
                            )),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "timeline:{}:{}",
                                server.id, channel.id
                            )))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_6()
                            .py_4()
                            .flex()
                            .flex_col()
                            .child(
                                div().pb_4().text_xs().text_color(rgb(0x929eb4)).child(
                                    text!(match server.history_source {
                                        HistorySource::LocalObserved =>
                                            "Local history · messages seen on this device",
                                        HistorySource::ServerArchive =>
                                            "Server history · preview messages",
                                    })
                                    .with_id("history-source"),
                                ),
                            )
                            .children(messages),
                    )
                    .child(
                        div()
                            .mx_6()
                            .mb_3()
                            .p_4()
                            .rounded_lg()
                            .bg(rgb(0x2a3242))
                            .text_color(rgb(0x929eb4))
                            .child(
                                text!("Connect to send a message").with_id("composer-placeholder"),
                            ),
                    )
                    .child(
                        div()
                            .px_6()
                            .pb_3()
                            .text_xs()
                            .text_color(rgb(0x8895ad))
                            .child(
                                text!(if self.history_enabled {
                                    "Local history enabled"
                                } else {
                                    "Local history disabled"
                                })
                                .with_id("history-default"),
                            ),
                    ),
            )
    }
}

fn activation_key(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "enter" || event.keystroke.key == "space"
}

fn security_label(security: SessionSecurity) -> &'static str {
    match security {
        SessionSecurity::Unknown => "Encryption status unknown",
        SessionSecurity::TransportOnly => "No end-to-end encryption; transport encryption only",
        SessionSecurity::MlsPending => "MLS not established",
        SessionSecurity::MlsEstablished => "MLS end-to-end encryption established",
    }
}

fn security_icon(security: SessionSecurity, id: usize) -> AnyElement {
    let color = match security {
        SessionSecurity::TransportOnly => 0xf07582,
        SessionSecurity::MlsEstablished => 0x5ad19a,
        SessionSecurity::MlsPending => 0xe8bc66,
        SessionSecurity::Unknown => 0x8f9ab0,
    };
    let icon = div()
        .id(("security", id))
        .role(Role::Image)
        .aria_label(security_label(security))
        .w(px(16.))
        .h(px(18.))
        .flex_shrink_0()
        .relative();
    if matches!(
        security,
        SessionSecurity::Unknown | SessionSecurity::MlsPending
    ) {
        return icon
            .text_color(rgb(color))
            .child(
                text!(if security == SessionSecurity::Unknown {
                    "?"
                } else {
                    "…"
                })
                .with_id(("security-pending", id)),
            )
            .into_any_element();
    }
    icon.child(
        div()
            .absolute()
            .left(px(if security == SessionSecurity::TransportOnly {
                7.
            } else {
                3.
            }))
            .top(px(0.))
            .w(px(9.))
            .h(px(11.))
            .rounded_tl_md()
            .rounded_tr_md()
            .border_2()
            .border_color(rgb(color)),
    )
    .child(
        div()
            .absolute()
            .left(px(1.))
            .top(px(8.))
            .w(px(13.))
            .h(px(10.))
            .rounded_sm()
            .bg(rgb(color)),
    )
    .into_any_element()
}

fn document_blocks(document: &MessageDocument) -> Vec<AnyElement> {
    document
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| match block {
            ContentBlock::Text { text: source } => div()
                .id(("text", index))
                .child(text!(source.clone()).with_id(("body", index)))
                .into_any_element(),
            ContentBlock::Code { language, runs } => {
                let mut source = String::new();
                let mut highlights = Vec::new();
                for run in runs {
                    let start = source.len();
                    source.push_str(&run.text);
                    let color = (u32::from(run.foreground[0]) << 16)
                        | (u32::from(run.foreground[1]) << 8)
                        | u32::from(run.foreground[2]);
                    highlights.push((
                        start..source.len(),
                        HighlightStyle {
                            color: Some(rgb(color).into()),
                            font_weight: run.bold.then_some(FontWeight::BOLD),
                            font_style: run.italic.then_some(FontStyle::Italic),
                            ..Default::default()
                        },
                    ));
                }
                div()
                    .id(("code", index))
                    .p_4()
                    .rounded_md()
                    .bg(rgb(0x141923))
                    .font_family("monospace")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div().text_xs().text_color(rgb(0x8f9ab0)).child(
                            text!(language.clone().unwrap_or_else(|| "code".into()))
                                .with_id(("language", index)),
                        ),
                    )
                    .child(
                        div()
                            .id(("code-text", index))
                            .role(Role::Label)
                            .aria_label(source.clone())
                            .child(StyledText::new(source).with_highlights(highlights)),
                    )
                    .into_any_element()
            }
        })
        .collect()
}
