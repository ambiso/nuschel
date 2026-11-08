// SPDX-License-Identifier: EUPL-1.2
use crate::{
    dialogs::{self, button},
    state::{Desktop, Status},
};
use gpui_kit::{base::input::Textarea, prelude::*, *};
use nuschel_client::{HistorySource, SessionSecurity};
use nuschel_core::{ContentBlock, MessageDocument};
use nuschel_session::connection::TextTarget;

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_composer_acks(window, cx);
        self.reconcile_selection(window, cx);
        let Some((server, channel)) = self.selected_indices() else {
            return dialogs::empty(self, cx);
        };
        div()
            .size_full()
            .relative()
            .flex()
            .bg(rgb(0x202632))
            .text_color(rgb(0xe9edf5))
            .font_family("sans-serif")
            .text_base()
            .child(server_rail(self, server, cx))
            .child(channel_panel(self, server, channel, cx))
            .child(conversation(self, server, channel, cx))
            .when(self.form.is_some(), |view| {
                view.child(dialogs::server_form(self, cx))
            })
            .into_any_element()
    }
}
fn server_rail(this: &Desktop, selected: usize, cx: &mut Context<Desktop>) -> AnyElement {
    let servers = this
        .servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            div()
                .id(("server", index))
                .role(Role::Button)
                .tab_index(0)
                .aria_label(format!(
                    "{}; {}",
                    server.name,
                    security_label(server.security)
                ))
                .aria_selected(index == selected)
                .relative()
                .w(px(48.))
                .h(px(48.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_xl()
                .bg(rgb(if index == selected {
                    0x5865f2
                } else {
                    0x282d39
                }))
                .hover(|style| style.bg(rgb(0x4a55cf)))
                .cursor_pointer()
                .font_weight(FontWeight::BOLD)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.browse_server(index, window, cx);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if activation_key(event) {
                        this.browse_server(index, window, cx);
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
        .children(servers)
        .child(button(
            "add-server",
            "+",
            |this, window, cx| this.open_form(None, window, cx),
            cx,
        ))
        .into_any_element()
}
fn channel_panel(
    this: &Desktop,
    server_index: usize,
    selected_channel: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = &this.servers[server_index];
    let rows = this
        .channel_rows(server_index)
        .into_iter()
        .map(|(index, depth, has_children)| {
            let entry = &server.channels[index];
            let id = entry.id;
            let collapsed = this.collapsed.contains(&(server.id.clone(), id));
            let server_id = server.id.clone();
            let members = server
                .users
                .values()
                .filter(|user| entry.target == TextTarget::Channel(user.channel))
                .count();
            div()
                .id(("channel-row", index))
                .role(Role::TreeItem)
                .tab_index(0)
                .aria_label(entry.name.clone())
                .aria_selected(index == selected_channel)
                .aria_level(depth + 1)
                .when(has_children, |row| row.aria_expanded(!collapsed))
                .flex()
                .items_center()
                .gap_2()
                .min_h(px(34.))
                .pr_2()
                .pl(px(8. + depth.min(10) as f32 * 12.))
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(if index == selected_channel {
                    0xf4f5fa
                } else {
                    0xadb5c6
                }))
                .when(index == selected_channel, |row| row.bg(rgb(0x333b50)))
                .hover(|style| style.bg(rgb(0x2a3142)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.browse_channel(server_index, id, window, cx);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if activation_key(event) {
                        this.browse_channel(server_index, id, window, cx);
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .id(("branch", index))
                        .w(px(12.))
                        .text_color(rgb(0x7f8ba3))
                        .when(has_children, |branch| {
                            branch
                                .role(Role::Button)
                                .tab_index(0)
                                .aria_label(format!("Toggle {}", entry.name))
                                .on_click(cx.listener({
                                    let server_id = server_id.clone();
                                    move |this, _, _, cx| {
                                        let key = (server_id.clone(), id);
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
                                            let key = (server_id.clone(), id);
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
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(text!(entry.name.clone()).with_id(("channel-name", index))),
                )
                .when(members > 0, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x8998b3))
                            .child(text!(members.to_string()).with_id(("member-count", index))),
                    )
                })
        })
        .collect::<Vec<_>>();
    let voice = server
        .voice_channel()
        .and_then(|id| {
            server
                .channels
                .iter()
                .find(|channel| channel.target == TextTarget::Channel(id))
        })
        .map(|channel| format!("Voice · {}", channel.name))
        .unwrap_or_else(|| "Voice disconnected".into());
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
                        .child(text!(server.status_label()).with_id("connection")),
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
                .children(rows),
        )
        .child(
            div()
                .p_4()
                .border_t_1()
                .border_color(rgb(0x2a3243))
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_sm().child(text!(voice).with_id("voice-status")))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8f9ab0))
                        .child(text!(server.audio_status.clone()).with_id("audio-status")),
                ),
        )
        .into_any_element()
}
fn conversation(
    this: &Desktop,
    index: usize,
    channel_index: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = &this.servers[index];
    let channel = &server.channels[channel_index];
    let members = server
        .users
        .values()
        .filter(|user| {
            channel.target == TextTarget::Channel(user.channel)
                && Some(user.session) != server.own_session
        })
        .take(100)
        .map(|user| {
            let session = user.session;
            div().id(("member", session as usize)).child(button(
                "direct-message-user",
                format!("@ {}", user.name),
                move |this, window, cx| this.open_direct(index, session, window, cx),
                cx,
            ))
        })
        .collect::<Vec<_>>();
    let messages =
        channel
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
                                    .when(message.has_more && !message.expanded, |content| {
                                        content.max_h(px(300.)).overflow_hidden()
                                    })
                                    .children(document_blocks(&message.document)),
                            )
                            .when(message.has_more, |view| {
                                view.child(button(
                                    "show-more",
                                    if message.expanded {
                                        "Show less"
                                    } else {
                                        "Show more"
                                    },
                                    move |this, _, cx| {
                                        this.toggle_message(index);
                                        cx.notify();
                                    },
                                    cx,
                                ))
                            }),
                    )
            })
            .collect::<Vec<_>>();
    div().flex_1().min_w_0().h_full().flex().flex_col()
        .child(div().min_h(px(72.)).flex_shrink_0().px_5().py_3().flex().flex_wrap().items_center().gap_3().border_b_1().border_color(rgb(0x30394b))
            .child(div().min_w(px(100.)).flex_1().font_weight(FontWeight::SEMIBOLD).child(text!(channel.name.clone()).with_id("channel-heading")))
            .when(server.status == Status::Connected, |view| view
                .when(matches!(channel.target, TextTarget::Channel(_)), |view| view.child(button("join-voice", "Join voice", |this, _, cx| { this.join_selected(); cx.notify(); }, cx)))
                .child(button("mute", if server.muted { "Unmute" } else { "Mute" }, |this, _, cx| { this.set_mute(false); cx.notify(); }, cx))
                .child(button("deafen", if server.deafened { "Undeafen" } else { "Deafen" }, |this, _, cx| { this.set_mute(true); cx.notify(); }, cx))
                .child(button("disconnect", "Disconnect", move |this, _, cx| { this.disconnect(index); cx.notify(); }, cx)))
            .when(server.status != Status::Connected, |view| view.child(button("connect-server", "Connect", move |this, window, cx| this.open_form(Some(index), window, cx), cx)))
            .child(button("edit-server", "Edit", move |this, window, cx| this.open_form(Some(index), window, cx), cx))
            .child(security_icon(server.security, this.servers.len() + index)))
        .when_some(server.notice.clone().or_else(|| this.error.clone()), |view, notice| view.child(div().px_6().py_2().bg(rgb(0x493340)).text_color(rgb(0xffc0cb)).text_sm().child(text!(notice).with_id("connection-notice"))))
        .when_some(server.certificate.as_ref(), |view, certificate| view.child(div().p_5().bg(rgb(0x40303a)).flex().flex_col().gap_3()
            .child(div().font_weight(FontWeight::BOLD).child(text!(if certificate.changed { "Server certificate changed" } else { "Confirm server certificate" }).with_id("certificate-heading")))
            .child(text!("Verify this SHA-256 fingerprint with the server owner before trusting it. No password has been sent.").with_id("certificate-explanation"))
            .child(div().font_family("monospace").text_sm().child(text!(certificate.sha256.as_bytes().chunks(8).map(|chunk| std::str::from_utf8(chunk).unwrap_or("?")).collect::<Vec<_>>().join(" ")).with_id("certificate-fingerprint")))
            .child(div().text_xs().child(text!(certificate.reason.clone()).with_id("certificate-reason")))
            .child(button("trust-certificate", if certificate.changed { "Trust changed certificate and connect" } else { "Trust certificate and connect" }, move |this, _, cx| { this.trust_certificate(index); cx.notify(); }, cx))))
        .child(div().px_6().pt_2().flex().gap_3()
            .when(channel.next_before.is_some() && !channel.loading, |view| view.child(button("older-history", "Load older messages", |this, _, cx| { this.load_older(); cx.notify(); }, cx)))
            .when(channel.viewing_older, |view| view.child(button("latest-history", format!("Jump to latest ({})", channel.new_messages), |this, _, cx| { this.load_history(None); cx.notify(); }, cx)))
            .when(channel.loading, |view| view.child(text!("Loading local history…").with_id("history-loading"))))
        .child(div().px_6().pt_2().flex().flex_wrap().gap_2().children(members))
        .child(div().id(SharedString::from(format!("timeline:{}:{}", server.id, channel.id))).track_scroll(&this.scroll).flex_1().min_h_0().overflow_y_scroll().px_6().py_4().flex().flex_col()
            .when_some(server.welcome.as_ref(), |view, welcome| view.child(div().p_3().mb_4().rounded_md().bg(rgb(0x29354a)).text_sm().child(text!(welcome.clone()).with_id("server-welcome"))))
            .child(div().pb_3().text_xs().text_color(rgb(0x929eb4)).child(text!(match server.history_source { HistorySource::LocalObserved => "Local history · messages seen on this device", HistorySource::ServerArchive => "Server history" }).with_id("history-source")))
            .child(div().pb_3().text_xs().text_color(rgb(0x929eb4)).child(text!(server.users.values().filter(|user| channel.target == TextTarget::Channel(user.channel))
                .map(|user| format!("{}{}", user.name, if user.muted || user.self_muted { " (muted)" } else { "" })).collect::<Vec<_>>().join(" · ")).with_id("channel-users")))
            .children(messages))
        .child(div().mx_6().mb_3().p_3().rounded_lg().bg(rgb(0x2a3242)).flex().flex_col().gap_2().child(Textarea::new(&this.composer))
            .child(div().flex().items_center().justify_between()
                .child(div().text_xs().text_color(rgb(0x929eb4)).child(text!("Enter for a new line · Ctrl/Cmd+Enter to send").with_id("composer-hint")))
                .child(button("send-message", if this.sending() { "Sending…" } else { "Send" }, |this, window, cx| this.send_message(window, cx), cx))))
        .child(div().px_6().pb_3().text_xs().text_color(rgb(0x8895ad)).child(text!(if this.history_enabled { "Local history enabled · no end-to-end encryption" } else { "Local history disabled" }).with_id("history-default")))
        .into_any_element()
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
