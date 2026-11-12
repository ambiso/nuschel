// SPDX-License-Identifier: EUPL-1.2
//! Informational and administrative dialogs.
use crate::{
    assets::icon,
    dialogs::{Tone, modal, styled_button},
    menus::ActionOrigin,
    state::{Desktop, ServerView, Status},
    theme::{self, *},
    view::document_blocks,
};
use gpui_kit::{
    base::input::{Input, InputState},
    prelude::*,
    *,
};
use nuschl_session::{
    connection::Command,
    model::{AclEntry, permission},
};

pub enum Panel {
    UserInfo {
        origin: ActionOrigin,
        server: usize,
        session: u32,
    },
    /// Self-monitoring found someone else's key on our account, or the server's
    /// key log contradicted itself (docs/e2ee.md §4).
    IdentityAlarm {
        server: usize,
    },
    Connection {
        origin: ActionOrigin,
        server: usize,
    },
    RegisteredUsers {
        origin: ActionOrigin,
        server: usize,
    },
    Bans {
        origin: ActionOrigin,
        server: usize,
    },
    Acl {
        origin: ActionOrigin,
        server: usize,
        channel: u32,
        editor: Box<crate::acl_editor::Editor>,
    },
    ChannelEdit {
        origin: ActionOrigin,
        server: usize,
        channel: u32,
        name: Entity<InputState>,
        description: Entity<InputState>,
        position: Entity<InputState>,
        max_users: Entity<InputState>,
    },
}

impl Panel {
    pub(crate) fn origin(&self) -> Option<&ActionOrigin> {
        match self {
            Self::UserInfo { origin, .. }
            | Self::Connection { origin, .. }
            | Self::RegisteredUsers { origin, .. }
            | Self::Bans { origin, .. }
            | Self::Acl { origin, .. }
            | Self::ChannelEdit { origin, .. } => Some(origin),
            Self::IdentityAlarm { .. } => None,
        }
    }
    pub(crate) fn set_server(&mut self, index: usize) {
        match self {
            Self::UserInfo { server, .. }
            | Self::Connection { server, .. }
            | Self::RegisteredUsers { server, .. }
            | Self::Bans { server, .. }
            | Self::Acl { server, .. }
            | Self::ChannelEdit { server, .. } => *server = index,
            Self::IdentityAlarm { .. } => {}
        }
    }
}

fn heading(title: String, acl_token: Option<u64>, cx: &mut Context<Desktop>) -> AnyElement {
    div()
        .p_6()
        .pb_4()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xl()
                .font_weight(FontWeight::BOLD)
                .child(text!(title).with_id("panel-title")),
        )
        .child(styled_button(
            SharedString::from(acl_token.map(|token|format!("acl-close-{token}")).unwrap_or_else(||"panel-done".into())),
            "Done",
            Tone::Primary,
            move |this, _, cx| {
                if acl_token.is_none() || matches!(this.panel.as_ref(),Some(Panel::Acl {editor,..}) if Some(editor.token)==acl_token) {
                    this.panel = None;
                    cx.notify();
                }
            },
            cx,
        ))
        .into_any_element()
}

fn row(label: &'static str, value: String, index: usize) -> AnyElement {
    div()
        .flex()
        .gap_3()
        .py(px(5.))
        .border_b_1()
        .border_color(color(BORDER))
        .text_sm()
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                .text_color(color(TEXT_MUTED))
                .child(text!(label).with_id(("row-label", index))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(text!(value).with_id(("row-value", index))),
        )
        .into_any_element()
}

fn duration(seconds: u32) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        _ => format!("{} h {} min", seconds / 3600, (seconds % 3600) / 60),
    }
}

pub fn panel(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let panel = this.panel.as_ref()?;
    let (title, width, body): (String, f32, AnyElement) = match panel {
        Panel::UserInfo {
            server: _,
            session,
            origin,
        } => {
            let server = &origin.live_index(this)?;
            let server_view = this.client.servers.get(*server)?;
            let user = server_view.users.get(session)?;
            let stats = server_view.stats.get(session);
            let mut rows = vec![
                row(
                    "Channel",
                    server_view
                        .channels
                        .iter()
                        .find(|channel| channel.id == user.channel)
                        .map(|channel| channel.name.clone())
                        .unwrap_or_default(),
                    0,
                ),
                row(
                    "Registered",
                    if user.user_id.is_some() {
                        "Yes".into()
                    } else {
                        "No".into()
                    },
                    1,
                ),
                row(
                    "Certificate",
                    user.certificate_hash
                        .clone()
                        .unwrap_or_else(|| "None".into()),
                    2,
                ),
            ];
            let shown_name = crate::names::display_name(user, &this.settings).to_string();
            if shown_name != user.name {
                rows.push(row("Name on the server", user.name.clone(), 14));
            }
            if let Some(added) = this.settings.friend(user.certificate_hash.as_deref()) {
                rows.push(row(
                    "Friend",
                    if added == user.name {
                        "Yes".into()
                    } else {
                        format!("Yes, added as {added}")
                    },
                    15,
                ));
            }
            let mut status = Vec::new();
            for (flag, text) in [
                (user.muted, "server-muted"),
                (user.deafened, "server-deafened"),
                (user.suppressed, "suppressed"),
                (user.self_muted, "muted"),
                (user.self_deafened, "deafened"),
                (user.priority_speaker, "priority speaker"),
                (user.recording, "recording"),
            ] {
                if flag {
                    status.push(text);
                }
            }
            rows.push(row(
                "Status",
                if status.is_empty() {
                    "Talking normally".into()
                } else {
                    status.join(", ")
                },
                3,
            ));
            if let Some(stats) = stats {
                let version = [
                    stats.release.clone(),
                    stats
                        .version
                        .clone()
                        .map(|version| format!("protocol {version}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                if !version.is_empty() {
                    rows.push(row("Client", version, 4));
                }
                if let Some(os) = &stats.os {
                    rows.push(row("Operating system", os.clone(), 5));
                }
                if let Some(online) = stats.online_seconds {
                    rows.push(row("Online", duration(online), 6));
                }
                if let Some(idle) = stats.idle_seconds {
                    rows.push(row("Idle", duration(idle), 7));
                }
                if let (Some(udp), Some(tcp)) = (stats.udp_ping_ms, stats.tcp_ping_ms) {
                    rows.push(row("Ping", format!("UDP {udp:.0} ms · TCP {tcp:.0} ms"), 8));
                }
                for (label, packets, id) in [
                    ("Packets to server", stats.from_client.as_ref(), 9),
                    ("Packets to client", stats.from_server.as_ref(), 16),
                ] {
                    rows.push(row(
                        label,
                        packets.map_or_else(
                            || "Not reported".into(),
                            |packets| {
                                format!(
                                    "{} good · {} late · {} lost",
                                    packets.good, packets.late, packets.lost
                                )
                            },
                        ),
                        id,
                    ));
                }
                if let Some(bandwidth) = stats.bandwidth {
                    rows.push(row(
                        "Bandwidth",
                        format!("{:.1} kbit/s", f64::from(bandwidth) / 125.0),
                        10,
                    ));
                }
                if let Some(address) = &stats.address {
                    rows.push(row("Address", address.clone(), 11));
                }
                // What the user's voice is sent with, as far as known: Nuschl
                // sends only Opus; others are judged by what was heard from
                // them, then by the extended statistics (a client version from
                // 1.2.4 on supports Opus). Without either, no guess.
                let modern = stats.version.as_deref().is_some_and(|version| {
                    let parts: Vec<u32> = version
                        .split('.')
                        .filter_map(|part| part.parse().ok())
                        .collect();
                    parts >= vec![1, 2, 4]
                });
                let codec = if server_view.own_session == Some(*session) {
                    Some("Opus")
                } else if server_view.legacy_voice.contains(session) {
                    Some("Legacy (CELT/Speex)")
                } else if stats.opus == Some(true) || modern {
                    Some("Opus")
                } else if stats.opus == Some(false) {
                    Some("Legacy (CELT/Speex)")
                } else {
                    None
                };
                if let Some(codec) = codec {
                    rows.push(row("Codec", codec.into(), 12));
                }
            } else {
                rows.push(row("Statistics", "Loading…".into(), 13));
            }
            let comment = this
                .ui_of(*server)
                .and_then(|ui| ui.comments.get(session))
                .map(|(_, document)| {
                    document_blocks(
                        this,
                        server_view,
                        nuschl_client::platform::RenderKey::Comment(*session),
                        document,
                        cx,
                    )
                });
            let body = div()
                .px_6()
                .pb_6()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .size(px(56.))
                                .rounded_full()
                                .bg(theme::avatar(&shown_name))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xl()
                                .font_weight(FontWeight::BOLD)
                                .text_color(color(WHITE))
                                .child(text!(theme::initials(&shown_name)).with_id("info-avatar")),
                        )
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(text!(shown_name).with_id("info-name")),
                        ),
                )
                // Who they are comes first: the identity, then what they say.
                .when_some(user.user_id, |column, user_id| {
                    let own = server_view.own_session == Some(*session);
                    column.child(crate::identity::section(
                        this, *server, user_id, own, origin, cx,
                    ))
                })
                .when_some(comment, |column, blocks| {
                    column.child(
                        div()
                            .p_3()
                            .rounded(px(8.))
                            .bg(color(SIDEBAR))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(blocks),
                    )
                })
                .child(div().flex().flex_col().children(rows))
                .child(div().flex().child(styled_button(
                    "refresh-stats",
                    "Refresh",
                    Tone::Secondary,
                    {
                        let session = *session;
                        let origin = origin.clone();
                        move |this, _, cx| {
                            origin.run(this, Command::RequestStats(session));
                            cx.notify();
                        }
                    },
                    cx,
                )))
                .into_any_element();
            ("Profile".into(), 520., body)
        }
        Panel::Connection { server: _, origin } => {
            let server = &origin.local_index(this)?;
            let view = this.client.servers.get(*server)?;
            let mut rows = vec![
                row(
                    "Server",
                    format!("{}:{}", view.bookmark.host, view.bookmark.port),
                    0,
                ),
                row("Status", view.status_label(), 1),
                row(
                    "Certificate (SHA-256)",
                    view.peer_sha256
                        .clone()
                        .or(view.bookmark.certificate_sha256.clone())
                        .unwrap_or_default(),
                    2,
                ),
                row(
                    "Encryption",
                    "TLS for control, OCB2-AES128 for UDP voice · no end-to-end encryption".into(),
                    3,
                ),
            ];
            if let Some(stats) = &view.transport {
                rows.push(row(
                    "Voice transport",
                    if stats.udp_active {
                        "UDP".into()
                    } else if stats.forced_tcp {
                        "TCP (forced)".into()
                    } else {
                        "TCP tunnel (UDP unavailable)".into()
                    },
                    4,
                ));
                rows.push(row(
                    "UDP ping",
                    format!(
                        "{:.1} ms ± {:.1} ({} pings)",
                        stats.udp.average_ms,
                        stats.udp.variance_ms.sqrt(),
                        stats.udp.packets
                    ),
                    5,
                ));
                rows.push(row(
                    "TCP ping",
                    format!(
                        "{:.1} ms ± {:.1} ({} pings)",
                        stats.tcp.average_ms,
                        stats.tcp.variance_ms.sqrt(),
                        stats.tcp.packets
                    ),
                    6,
                ));
                let total = (f64::from(stats.good) + f64::from(stats.lost)).max(1.);
                rows.push(row(
                    "UDP from server",
                    format!(
                        "{} good · {} late · {} lost ({:.1}%) · {} resyncs",
                        stats.good,
                        stats.late,
                        stats.lost,
                        f64::from(stats.lost) * 100. / total,
                        stats.resync
                    ),
                    7,
                ));
            }
            if let Some(limits) = &view.limits {
                rows.push(row(
                    "Message limit",
                    format!(
                        "{}{}",
                        if limits.message_length == 0 {
                            "Unlimited".into()
                        } else {
                            format!("{} characters", limits.message_length)
                        },
                        if limits.allow_html {
                            " · HTML allowed"
                        } else {
                            ""
                        }
                    ),
                    8,
                ));
                rows.push(row(
                    "Image limit",
                    if limits.image_message_length == 0 {
                        "Unlimited".into()
                    } else {
                        format!("{} characters", limits.image_message_length)
                    },
                    9,
                ));
                if let Some(max) = limits.max_users {
                    rows.push(row("Max users", max.to_string(), 10));
                }
                if let Some(bandwidth) = limits.max_bandwidth {
                    rows.push(row(
                        "Max bandwidth",
                        format!("{} kbit/s", bandwidth / 1000),
                        11,
                    ));
                }
            }
            (
                "Connection".into(),
                560.,
                div()
                    .px_6()
                    .pb_6()
                    .flex()
                    .flex_col()
                    .children(rows)
                    .into_any_element(),
            )
        }
        Panel::RegisteredUsers { server: _, origin } => {
            let server = &origin.live_index(this)?;
            let view = this.client.servers.get(*server)?;
            let index = *server;
            let body = match &view.registered_users {
                None => div()
                    .px_6()
                    .pb_6()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(text!("Loading…").with_id("loading"))
                    .into_any_element(),
                Some(users) => div()
                    .px_6()
                    .pb_6()
                    .flex()
                    .flex_col()
                    .children(users.iter().enumerate().take(500).map(|(position, user)| {
                        let id = user.user_id;
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .py_1()
                            .border_b_1()
                            .border_color(color(BORDER))
                            .text_sm()
                            .child(
                                div()
                                    .w(px(40.))
                                    .text_color(color(TEXT_FAINT))
                                    .child(text!(format!("#{id}")).with_id(("user-id", position))),
                            )
                            .child(
                                div().flex_1().min_w_0().truncate().child(
                                    text!(user.name.clone().unwrap_or_default())
                                        .with_id(("registered-name", position)),
                                ),
                            )
                            .child(
                                div().text_xs().text_color(color(TEXT_FAINT)).child(
                                    text!(user.last_seen.clone().unwrap_or_default())
                                        .with_id(("last-seen", position)),
                                ),
                            )
                            .when(id != 0, |row| {
                                row.child(styled_button(
                                    ("unregister", position),
                                    "Unregister",
                                    Tone::Ghost,
                                    {
                                        let origin = origin.clone();
                                        move |this, _, cx| {
                                            let Some(index) = origin.live_index(this) else {
                                                return;
                                            };
                                            origin.with_scope(this, |this| {
                                                if let Some(users) = this.client.servers[index]
                                                    .registered_users
                                                    .as_mut()
                                                {
                                                    users.retain(|user| user.user_id != id);
                                                }
                                                // A user without a name is unregistered by the server.
                                                this.run(
                                                    index,
                                                    Command::UpdateRegisteredUsers(vec![
                                                        nuschl_session::model::RegisteredUser {
                                                            user_id: id,
                                                            name: None,
                                                            last_seen: None,
                                                            last_channel: None,
                                                        },
                                                    ]),
                                                );
                                                cx.notify();
                                            });
                                        }
                                    },
                                    cx,
                                ))
                            })
                    }))
                    .into_any_element(),
            };
            ("Registered users".into(), 560., body)
        }
        Panel::IdentityAlarm { server } => {
            let view = this.client.servers.get(*server)?;
            let alarm = this.ui_of(*server)?.identity.alarm.as_deref()?;
            let body = crate::identity::alarm(this, *server, alarm, cx);
            (format!("Your identity on {}", view.name), 520., body)
        }
        Panel::Bans { server: _, origin } => {
            let server = &origin.live_index(this)?;
            let view = this.client.servers.get(*server)?;
            let index = *server;
            let body = match &view.bans {
                None => div()
                    .px_6()
                    .pb_6()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(text!("Loading…").with_id("loading"))
                    .into_any_element(),
                Some(bans) if bans.is_empty() => div()
                    .px_6()
                    .pb_6()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(text!("No bans.").with_id("no-bans"))
                    .into_any_element(),
                Some(bans) => div()
                    .px_6()
                    .pb_6()
                    .flex()
                    .flex_col()
                    .children(bans.iter().enumerate().take(1000).map(|(position, ban)| {
                        let detail = [
                            ban.name.clone(),
                            ban.reason.clone(),
                            ban.start.clone(),
                            ban.duration
                                .filter(|seconds| *seconds > 0)
                                .map(|seconds| format!("for {}", duration(seconds))),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · ");
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .py_1()
                            .border_b_1()
                            .border_color(color(BORDER))
                            .text_sm()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        text!(ban.address_text())
                                            .with_id(("ban-address", position)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(color(TEXT_FAINT))
                                            .child(text!(detail).with_id(("ban-detail", position))),
                                    ),
                            )
                            .child(styled_button(
                                ("unban", position),
                                "Remove",
                                Tone::Ghost,
                                {
                                    let origin = origin.clone();
                                    move |this, _, cx| {
                                        let Some(index) = origin.live_index(this) else {
                                            return;
                                        };
                                        origin.with_scope(this, |this| {
                                            if let Some(bans) =
                                                this.client.servers[index].bans.as_mut()
                                            {
                                                if position < bans.len() {
                                                    bans.remove(position);
                                                }
                                                let bans = bans.clone();
                                                this.run(index, Command::UpdateBans(bans));
                                            }
                                            cx.notify();
                                        });
                                    }
                                },
                                cx,
                            ))
                    }))
                    .into_any_element(),
            };
            ("Ban list".into(), 560., body)
        }
        Panel::Acl {
            server: _,
            channel,
            origin,
            editor,
        } => {
            if let Some(server) = origin
                .live_index(this)
                .and_then(|index| this.client.servers.get(index))
            {
                (
                    format!("Permissions · {}", channel_name(server, *channel)),
                    760.,
                    crate::acl_editor::render(server, editor, cx),
                )
            } else {
                (
                    format!("Retained permissions · channel {channel}"),
                    760.,
                    crate::acl_editor::render_stale(editor, cx),
                )
            }
        }
        Panel::ChannelEdit {
            origin,
            name,
            description,
            position,
            max_users,
            channel,
            server,
        } => {
            let server = &origin.live_index(this)?;
            let view = this.client.servers.get(*server)?;
            let field = |id: &'static str, label: &'static str, input: &Entity<InputState>| {
                div()
                    .id(id)
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(color(TEXT_MUTED))
                            .child(text!(label).with_id((id, 1_usize))),
                    )
                    .child(
                        div()
                            .px_3()
                            .py(px(9.))
                            .rounded(px(6.))
                            .bg(color(APP))
                            .border_1()
                            .border_color(color(BORDER))
                            .child(Input::new(input)),
                    )
            };
            let body = div()
                .px_6()
                .pb_6()
                .flex()
                .flex_col()
                .gap_4()
                .child(field("channel-name-field", "NAME", name))
                .child(field(
                    "channel-description-field",
                    "DESCRIPTION",
                    description,
                ))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_1().child(field(
                            "channel-position-field",
                            "POSITION",
                            position,
                        )))
                        .child(div().flex_1().child(field(
                            "channel-max-users-field",
                            "MAXIMUM USERS (0 = unlimited)",
                            max_users,
                        ))),
                )
                .child(div().flex().justify_end().child(styled_button(
                    "save-channel",
                    "Save channel",
                    Tone::Primary,
                    |this, _, cx| {
                        this.save_channel_editor(cx);
                        cx.notify();
                    },
                    cx,
                )))
                .into_any_element();
            (format!("Edit {}", channel_name(view, *channel)), 520., body)
        }
    };
    let content = div()
        .flex()
        .flex_col()
        .child(heading(
            title,
            match panel {
                Panel::Acl { editor, .. } => Some(editor.token),
                _ => None,
            },
            cx,
        ))
        .child(body)
        .into_any_element();
    Some(modal(
        this,
        "panel",
        "Details",
        width,
        content,
        cx,
        |this| this.panel = None,
    ))
}

fn channel_name(view: &ServerView, channel: u32) -> String {
    view.channels
        .iter()
        .find(|entry| entry.id == channel && !entry.storage_key.starts_with("direct:"))
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| format!("channel {channel}"))
}

/// Status text for the connection banner shown above the conversation.
pub fn reconnect_banner(view: &ServerView) -> Option<String> {
    (view.status == Status::Connecting)
        .then(|| view.reconnecting.clone())
        .flatten()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{AclEntry, permission};
    use crate::acl_editor::cycle;
    #[test]
    fn acl_bits_cycle_allow_deny_unset() {
        let mut entry = AclEntry {
            apply_here: true,
            apply_subs: true,
            inherited: false,
            user_id: None,
            group: Some("all".into()),
            grant: 0,
            deny: 0,
        };
        cycle(&mut entry, permission::SPEAK);
        assert_eq!((entry.grant, entry.deny), (permission::SPEAK, 0));
        cycle(&mut entry, permission::SPEAK);
        assert_eq!((entry.grant, entry.deny), (0, permission::SPEAK));
        cycle(&mut entry, permission::SPEAK);
        assert_eq!((entry.grant, entry.deny), (0, 0));
    }
}
