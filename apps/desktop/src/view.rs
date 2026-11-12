// SPDX-License-Identifier: EUPL-1.2
use crate::document::{Block, Document, Kind};
use crate::{
    assets::icon,
    chrome,
    dialogs::{self, Tone, Tooltip, icon_button, styled_button},
    layout,
    menus::MenuTarget,
    names,
    presentation::COLLAPSED_HEIGHT,
    scrolling::{EntryAction, Focus, Key, Reveal, Unseen},
    selection::{self, BlockText, Caret, HoveredLink, SelectionArea, TextBox, Texts},
    state::{ChannelRow, Desktop, ServerView, Status, TreeRow},
    theme::{self, *},
    unread::{self, Lane},
};
use gpui_kit::{
    base::{Scrollbar, ScrollbarMode, input::Textarea},
    prelude::*,
    *,
};
use nuschl_client::{HistorySource, SessionSecurity, conversation::Entry};
use nuschl_session::connection::{TextTarget, User};
use std::{collections::HashMap, rc::Rc};

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(feature = "frame-timing")]
        let started = std::time::Instant::now();
        crate::images::collect(window);
        self.ease_voice_toggles(window, cx);
        self.animate_voice(window, cx);
        // Which dialog the last frame showed (see dialogs::modal).
        self.modal_seen.set(self.modal_shown.take());
        self.keep_dialog_focus(window, cx);
        // What commands since the last frame changed (events apply theirs).
        self.apply_updates(cx);
        self.apply_composer_acks(window, cx);
        self.reconcile_selection(window, cx);
        self.settle_timeline();
        self.mark_read();
        self.present_selected();
        let selected = self.selected_indices();
        let title = selected
            .map(|(server, channel)| {
                let server = &self.client.servers[server];
                format!("{} — {}", server.name, server.channels[channel].name)
            })
            .unwrap_or_default();
        let finder = crate::finder::overlay(self, window, cx);
        let body = div()
            .min_w_0()
            .flex_1()
            .min_h_0()
            .flex()
            .child(server_rail(self, selected.map(|(server, _)| server), cx))
            .map(|body| match selected {
                Some((server, channel)) => body
                    .child(sidebar(self, server, channel, window, cx))
                    .child(sidebar_handle(self, cx))
                    .child(conversation(self, server, channel, window, cx)),
                None => body.child(dialogs::empty(self, cx)),
            });
        let content = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(color(MAIN))
            .text_color(color(TEXT))
            .font_family(theme::ui_font())
            .text_size(px(self.settings.appearance.text_size))
            .track_focus(&self.window_focus)
            // A shortcut being recorded takes the key before any focused control.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.record_key(&event.keystroke) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.find_shortcut(&event.keystroke, window, cx) {
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if crate::emoji::shortcut(&event.keystroke)
                    && this.selected_indices().is_some()
                    && !this.dialog_open()
                {
                    this.open_emoji(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if this.key_down(&event.keystroke, event.is_held) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                if this.key_up(&event.keystroke) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.sidebar_drag.is_some() {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.drag_sidebar(f32::from(event.position.x));
                    } else {
                        this.finish_sidebar_drag();
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.sidebar_drag.is_some() {
                        this.finish_sidebar_drag();
                        cx.notify();
                    }
                }),
            )
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                this.modifiers_changed(&event.modifiers);
                cx.notify();
            }))
            .child(chrome::title_bar(title.into(), window, cx))
            .when_some(update_bar(self, cx), |view, bar| view.child(bar))
            .child(body)
            .when(self.form.is_some(), |view| {
                view.child(dialogs::server_form(self, cx))
            })
            .when(self.settings_open, |view| {
                view.child(crate::settings::dialog(
                    self,
                    selected.map(|(server, _)| &self.client.servers[server]),
                    cx,
                ))
            })
            .when_some(crate::panels::panel(self, cx), |view, panel| {
                view.child(panel)
            })
            .when_some(crate::menus::prompt(self, cx), |view, prompt| {
                view.child(prompt)
            })
            .when_some(
                crate::voice::second_server_dialog(self, cx),
                |view, dialog| view.child(dialog),
            )
            .when_some(crate::emoji::overlay(self, cx), |view, picker| {
                view.child(picker)
            })
            .when_some(crate::viewer::overlay(self, window, cx), |view, viewer| {
                view.child(viewer)
            })
            .when_some(finder, |view, finder| view.child(finder))
            .when_some(crate::menus::menu(self, cx), |view, menu| view.child(menu))
            .when_some(crate::appearance::badge(self), |view, badge| {
                view.child(badge)
            })
            .into_any_element();
        let frame = chrome::frame(window, content);
        #[cfg(feature = "frame-timing")]
        crate::frame_timing::rendered(started);
        frame
    }
}

fn server_rail(this: &Desktop, selected: Option<usize>, cx: &mut Context<Desktop>) -> AnyElement {
    let unread_servers = this
        .client
        .servers
        .iter()
        .enumerate()
        .filter_map(|(slot, server)| {
            let first = server.channels.iter().find(|channel| channel.unread > 0)?;
            Some(unread::Candidate {
                slot,
                target: unread::Target {
                    server: server.id.clone(),
                    channel: first.id,
                },
                count: server.channels.iter().fold(0_usize, |count, channel| {
                    count.saturating_add(channel.unread)
                }),
                mention: server
                    .channels
                    .iter()
                    .any(|channel| channel.unread > 0 && channel.unread_mention),
            })
        })
        .collect();
    let servers = this
        .client
        .servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            let active = Some(index) == selected;
            let online = server.status == Status::Connected;
            // Discord-like: the pill marks unread conversations; a red count
            // marks mentions and direct messages.
            let unread = server.channels.iter().any(|channel| channel.unread > 0);
            let mentions: usize = server
                .channels
                .iter()
                .filter(|channel| channel.unread_mention)
                .map(|channel| channel.unread)
                .sum();
            div()
                .id(("server-slot", index))
                .flex_shrink_0()
                .relative()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(if active { 6. } else { 19. }))
                        .w(px(4.))
                        .h(px(if active { 36. } else { 10. }))
                        .rounded_r(px(4.))
                        .bg(color(TEXT))
                        .when(!active && !unread, |pill| pill.invisible()),
                )
                .child(
                    div()
                        .id(("server", index))
                        .role(Role::Button)
                        .tab_index(0)
                        .aria_label({
                            let role = this.client.voice_role(index);
                            let voice =
                                crate::voice::rail_label(role, crate::voice::someone_talks(server));
                            let mut label = format!(
                                "{}; {}; {}",
                                server.name,
                                server.status_label(),
                                security_label(server.security)
                            );
                            if !voice.is_empty() {
                                label = format!("{label}; {voice}");
                            }
                            label
                        })
                        .aria_selected(active)
                        .relative()
                        .size(px(48.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(if active { 16. } else { 24. }))
                        .bg(if active {
                            color(ACCENT).into()
                        } else {
                            theme::avatar(&server.name)
                        })
                        .when(!active && !online, |icon| icon.opacity(0.6))
                        .hover(|style| style.rounded(px(16.)).opacity(1.))
                        .cursor_pointer()
                        .text_color(color(WHITE))
                        .font_weight(FontWeight::BOLD)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.browse_server(index, window, cx);
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                this.open_menu(index, MenuTarget::Server, event.position);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if activation_key(event) {
                                this.browse_server(index, window, cx);
                                cx.notify();
                            }
                        }))
                        .map(|tile| {
                            let icon = this.ui.get(&server.id).and_then(|ui| ui.icon.clone());
                            match icon.filter(|_| this.settings.server_icons) {
                                Some(icon) => {
                                    tile.child(img(icon).size(px(48.)).rounded(px(if active {
                                        16.
                                    } else {
                                        24.
                                    })))
                                }
                                None => tile.child(
                                    text!(theme::initials(&server.name))
                                        .with_id(("server-name", index)),
                                ),
                            }
                        })
                        .child(
                            div()
                                .absolute()
                                .right(px(-4.))
                                .bottom(px(-4.))
                                .size(px(20.))
                                .rounded_full()
                                .bg(color(RAIL))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(security_icon(server.security, index, 12.)),
                        )
                        .when_some(
                            crate::voice::rail_ring(this, server, if active { 16. } else { 24. }),
                            |tile, ring| tile.child(ring),
                        )
                        .when_some(
                            crate::voice::rail_badge(this, index, server),
                            |tile, badge| tile.child(badge),
                        )
                        .when(mentions > 0, |tile| {
                            tile.child(
                                div()
                                    .absolute()
                                    .right(px(-6.))
                                    .top(px(-6.))
                                    .min_w(px(20.))
                                    .h(px(20.))
                                    .px(px(4.))
                                    .rounded_full()
                                    .border_2()
                                    .border_color(color(RAIL))
                                    .bg(color(DANGER))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(color(WHITE))
                                    .child(if mentions > 99 {
                                        "99+".to_string()
                                    } else {
                                        mentions.to_string()
                                    }),
                            )
                        }),
                )
        })
        .collect::<Vec<_>>();
    div()
        .w(px(72.))
        .h_full()
        .flex_shrink_0()
        .pt_3()
        .gap_2()
        .bg(color(RAIL))
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .relative()
                .child(
                    div()
                        .id("server-list")
                        .role(Role::List)
                        .aria_label("Servers")
                        .size_full()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .overflow_y_scroll()
                        .track_scroll(&this.server_scroll)
                        .on_scroll(cx.listener(|this, event, window, cx| {
                            this.smooth_server_scroll(event, window, cx)
                        }))
                        .children(servers),
                )
                .child(unread::overlay(
                    this,
                    Lane::Servers,
                    &this.server_scroll,
                    unread_servers,
                    cx,
                )),
        )
        .child(
            div()
                .w(px(32.))
                .h(px(2.))
                .my_1()
                .rounded_full()
                .bg(color(BORDER_STRONG)),
        )
        .child(
            div()
                .id("add-server")
                .group("add-server")
                .role(Role::Button)
                .aria_label("Add a server")
                .tab_index(0)
                .size(px(48.))
                .rounded(px(24.))
                .bg(color(ELEVATED))
                .text_color(color(SUCCESS))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|style| {
                    style
                        .rounded(px(16.))
                        .bg(color(SUCCESS))
                        .text_color(color(WHITE))
                })
                .tooltip(|_, cx| cx.new(|_| Tooltip("Add a server".into())).into())
                .on_click(cx.listener(|this, _, window, cx| this.open_form(None, window, cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if activation_key(event) {
                        this.open_form(None, window, cx);
                    }
                }))
                .child(
                    icon("plus", 22., color(SUCCESS))
                        .group_hover("add-server", |style| style.text_color(color(WHITE))),
                ),
        )
        .into_any_element()
}

/// The boundary between channels and conversation: drag it to fit long
/// channel names, double-click to restore the default width.
fn sidebar_handle(this: &Desktop, cx: &mut Context<Desktop>) -> AnyElement {
    let width = this.sidebar_width();
    let active = this.sidebar_drag.is_some();
    div()
        .id("sidebar-resize")
        .role(Role::Splitter)
        .aria_label("Resize channel list")
        .w(px(5.))
        .h_full()
        .flex_shrink_0()
        .flex()
        .justify_center()
        .bg(color(SIDEBAR))
        .cursor_col_resize()
        .group("sidebar-resize")
        .child(
            div()
                .w(px(1.))
                .h_full()
                .bg(color(if active { ACCENT } else { BORDER }))
                .group_hover("sidebar-resize", |style| style.bg(color(ACCENT))),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.sidebar_drag = Some((f32::from(event.position.x), width, width));
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
            if event.click_count() == 2 {
                this.sidebar_drag = None;
                this.update_settings(|settings| settings.sidebar_width = 248.);
                cx.notify();
            }
        }))
        .into_any_element()
}

fn sidebar(
    this: &Desktop,
    server_index: usize,
    selected_channel: usize,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = &this.client.servers[server_index];
    let voice_channel = server
        .has_mumble_voice()
        .then(|| server.voice_channel())
        .flatten();
    let unread_channels = unread::channels(server);
    let folded: HashMap<_, _> = unread_channels
        .iter()
        .map(|candidate| (candidate.slot, candidate))
        .collect();
    let rows = server
        .tree_rows()
        .into_iter()
        .enumerate()
        .map(|(position, row)| match row {
            TreeRow::Channel(row) => channel_row(
                this,
                server_index,
                row,
                row.index == selected_channel,
                voice_channel,
                folded.get(&(position + 1)).copied(),
                cx,
            ),
            TreeRow::User { session, depth } => match server.users.get(&session) {
                Some(user) => user_row(this, server, server_index, user, depth, cx),
                None => div().into_any_element(),
            },
            TreeRow::Listener { session, depth } => match server.users.get(&session) {
                Some(user) => div()
                    .id(("listener-row", session as usize))
                    .flex_shrink_0()
                    .role(Role::TreeItem)
                    .aria_label(format!(
                        "{} is listening",
                        names::full_name(user, &this.settings)
                    ))
                    .h(px(26.))
                    .pl(px(user_indent(depth)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(color(TEXT_FAINT))
                    .child(icon("ear", 14., color(TEXT_FAINT)))
                    .child(
                        div().truncate().child(
                            text!(names::display_name(user, &this.settings).to_string())
                                .with_id(("listener-name", session as usize)),
                        ),
                    )
                    .into_any_element(),
                None => div().into_any_element(),
            },
        })
        .collect::<Vec<_>>();
    let directs = server
        .channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| {
            matches!(
                channel.target,
                TextTarget::Direct(_) | TextTarget::DirectConversation(_)
            )
        })
        .map(|(index, channel)| {
            let id = channel.id;
            let selected = index == selected_channel;
            div()
                .id(("direct-row", id as usize))
                .role(Role::TreeItem)
                .tab_index(0)
                .aria_label(channel.name.clone())
                .aria_selected(selected)
                .h(px(30.))
                .flex_shrink_0()
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(color(if selected { TEXT } else { TEXT_MUTED }))
                .when(selected, |row| row.bg(color(SELECTED)))
                .hover(|style| style.bg(color(HOVER)).text_color(color(TEXT)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.browse_channel(server_index, id, window, cx);
                    cx.notify();
                }))
                .child(icon("message", 16., color(TEXT_FAINT)))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .when(channel.unread > 0, |name| {
                            name.font_weight(FontWeight::SEMIBOLD)
                                .text_color(color(TEXT))
                        })
                        .child(channel.name.trim_start_matches("@ ").to_string()),
                )
                .children(unread_badge(channel))
                .into_any_element()
        })
        .collect::<Vec<_>>();
    let width = this.sidebar_width();
    div()
        .w(px(width))
        .h_full()
        .flex_shrink_0()
        .bg(color(SIDEBAR))
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(52.))
                .flex_shrink_0()
                .pl_4()
                .pr_2()
                .flex()
                .items_center()
                .gap_2()
                .border_b_1()
                .border_color(color(BORDER))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .truncate()
                                .child(text!(server.name.clone()).with_id("server-heading")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_xs()
                                .text_color(color(status_color(server.status)))
                                .child(
                                    div()
                                        .size(px(7.))
                                        .rounded_full()
                                        .bg(color(status_color(server.status))),
                                )
                                .child(layout::label("connection", server.status_label())),
                        ),
                )
                .child(icon_button(
                    "server-settings",
                    "cog",
                    "Server settings",
                    TEXT_MUTED,
                    None,
                    move |this, window, cx| {
                        this.open_form(Some(server_index), window, cx);
                        cx.notify();
                    },
                    cx,
                ))
                .child(
                    div()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                this.open_menu(server_index, MenuTarget::Server, event.position);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .child(icon_button(
                            "server-menu",
                            "more",
                            "Server options",
                            TEXT_MUTED,
                            None,
                            |_, _, _| {},
                            cx,
                        )),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    div()
                        .id(SharedString::from(format!("channel-tree:{}", server.id)))
                        .role(Role::Tree)
                        .aria_label("Channels")
                        .size_full()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .overflow_y_scroll()
                        .track_scroll(&this.channel_scroll)
                        .pl_2()
                        .pr_3()
                        .pt_3()
                        .pb_4()
                        .on_scroll(cx.listener(|this, event, window, cx| {
                            this.smooth_scroll(true, event, window, cx)
                        }))
                        .child(section_label("channels-heading", "VOICE CHANNELS"))
                        .children(rows)
                        .when(!directs.is_empty(), |list| {
                            list.child(div().h_3().flex_shrink_0())
                                .child(section_label("direct-heading", "DIRECT MESSAGES"))
                                .children(directs)
                        }),
                )
                .child(
                    div().absolute().inset_0().child(
                        Scrollbar::vertical(&this.channel_scroll)
                            .id("channel-scrollbar")
                            .mode(ScrollbarMode::Always)
                            .styles(|styles| {
                                styles
                                    .track(|track| track.width(px(8.)).bg(color(SIDEBAR).into()))
                                    .thumb(|thumb| {
                                        thumb.width(px(4.)).bg(color(BORDER_STRONG)).radius(px(2.))
                                    })
                                    .thumb_hover(|thumb| thumb.bg(color(TEXT_FAINT)))
                                    .thumb_active(|thumb| thumb.bg(color(TEXT_MUTED)))
                            }),
                    ),
                )
                .child(unread::overlay(
                    this,
                    Lane::Channels,
                    &this.channel_scroll,
                    unread_channels,
                    cx,
                )),
        )
        .when_some(
            crate::voice::card(this, server_index, window, cx),
            |sidebar, card| sidebar.child(card),
        )
        .child(user_panel(this, server, cx))
        .into_any_element()
}

fn section_label(id: &'static str, label: &'static str) -> AnyElement {
    div()
        .flex_shrink_0()
        .px_2()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(color(TEXT_FAINT))
        .child(text!(label).with_id(id))
        .into_any_element()
}

fn channel_row(
    this: &Desktop,
    server_index: usize,
    ChannelRow {
        index,
        depth,
        has_children,
        expanded,
        hidden_users,
        hidden_talking,
    }: ChannelRow,
    selected: bool,
    voice_channel: Option<u32>,
    unread: Option<&unread::Candidate>,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = &this.client.servers[server_index];
    let entry = &server.channels[index];
    let id = entry.id;
    let collapsed = !expanded;
    let in_voice = voice_channel == Some(id);
    let full_name = entry.name.clone();
    let unread_count = unread.map_or(entry.unread, |unread| unread.count);
    let mention = unread.map_or(entry.unread_mention, |unread| unread.mention);
    div()
        .id(("channel-row", id as usize))
        .role(Role::TreeItem)
        .tab_index(0)
        .aria_label(entry.name.clone())
        .aria_selected(selected)
        .aria_level(depth + 1)
        .when(has_children, |row| row.aria_expanded(!collapsed))
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(32.))
        .flex_shrink_0()
        .min_w_0()
        .overflow_hidden()
        .pr_2()
        .pl(px(4. + depth.min(10) as f32 * 14.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_color(color(if selected || in_voice {
            TEXT
        } else {
            TEXT_MUTED
        }))
        .when(selected, |row| row.bg(color(SELECTED)))
        .hover(|style| style.bg(color(HOVER)).text_color(color(TEXT)))
        .when(this.menu.is_none(), |row| {
            row.tooltip(move |_, cx| cx.new(|_| Tooltip(full_name.clone().into())).into())
        })
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.browse_channel(server_index, id, window, cx);
            if event.click_count() == 2 {
                this.join_selected();
            }
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if activation_key(event) {
                this.browse_channel(server_index, id, window, cx);
                cx.notify();
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.open_menu(server_index, MenuTarget::Channel(id), event.position);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .child(
            div()
                .id(("branch", id as usize))
                .size(px(18.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_color(color(TEXT_FAINT))
                .when(has_children, |branch| {
                    branch
                        .role(Role::Button)
                        .tab_index(0)
                        .aria_label(format!("Toggle {}", entry.name))
                        .hover(|style| style.bg(color(BORDER_STRONG)).text_color(color(TEXT)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.client.servers[server_index].set_expanded(id, !expanded);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if activation_key(event) {
                                this.client.servers[server_index].set_expanded(id, !expanded);
                                cx.stop_propagation();
                                cx.notify();
                            }
                        }))
                        .child(icon(
                            if collapsed {
                                "chevron-right"
                            } else {
                                "chevron-down"
                            },
                            14.,
                            color(TEXT_FAINT),
                        ))
                }),
        )
        .child(
            div()
                .text_color(color(if in_voice { SUCCESS } else { TEXT_FAINT }))
                .child(icon(
                    "volume",
                    16.,
                    color(if in_voice { SUCCESS } else { TEXT_FAINT }),
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(in_voice || unread_count > 0, |name| {
                    name.font_weight(FontWeight::SEMIBOLD)
                })
                .when(unread_count > 0, |name| name.text_color(color(TEXT)))
                .id(("channel-name", id as usize))
                .child(entry.name.clone()),
        )
        .children(unread_badge_count(entry.id, unread_count, mention))
        .when(!entry.links.is_empty(), |row| {
            row.child(
                div()
                    .id(("channel-links", id as usize))
                    .tooltip({
                        let names = entry
                            .links
                            .iter()
                            .filter_map(|link| {
                                server.channels.iter().find(|channel| channel.id == *link)
                            })
                            .map(|channel| channel.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ");
                        move |_, cx| {
                            cx.new(|_| Tooltip(format!("Linked with {names}").into()))
                                .into()
                        }
                    })
                    .child(icon("link", 14., color(TEXT_FAINT))),
            )
        })
        // People inside a collapsed branch: a person icon and count, green while one talks.
        .when(hidden_users > 0, |row| {
            let shade = if hidden_talking {
                SUCCESS
            } else {
                ACCENT_HOVER
            };
            row.child(
                div()
                    .id(("member-count", index))
                    .aria_label(format!(
                        "{hidden_users} {} inside",
                        if hidden_users == 1 {
                            "person"
                        } else {
                            "people"
                        }
                    ))
                    .tooltip(move |_, cx| {
                        cx.new(|_| {
                            Tooltip(
                                format!(
                                    "{hidden_users} {} in collapsed channels",
                                    if hidden_users == 1 {
                                        "person"
                                    } else {
                                        "people"
                                    }
                                )
                                .into(),
                            )
                        })
                        .into()
                    })
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .pl(px(5.))
                    .pr(px(7.))
                    .py(px(1.))
                    .rounded_full()
                    .bg(tint(shade, 0x2a))
                    .border_1()
                    .border_color(tint(shade, 0x66))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(shade))
                    .child(icon("user", 12., color(shade)))
                    .child(text!(hidden_users.to_string()).with_id(("member-count-text", index))),
            )
        })
        .into_any_element()
}

/// People sit under their channel's name, further in than its sub-channels'
/// icons: `depth` is the channel's depth plus one. The constant is the
/// channel row's inset, chevron, gap, speaker icon and gap.
fn user_indent(depth: usize) -> f32 {
    50. + depth.saturating_sub(1).min(10) as f32 * 14.
}

fn user_row(
    this: &Desktop,
    server: &ServerView,
    server_index: usize,
    user: &User,
    depth: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let session = user.session;
    let own = server.own_session == Some(session);
    let talking = server.talking.contains(&session);
    let muted = user.muted || user.self_muted || user.suppressed;
    let deafened = user.deafened || user.self_deafened;
    let name = names::display_name(user, &this.settings).to_string();
    let full_name = names::full_name(user, &this.settings);
    // The name they were befriended under, when it is no longer theirs.
    let friend = this
        .settings
        .friend(user.certificate_hash.as_deref())
        .map(|added| {
            SharedString::from(if added == user.name {
                "Friend".to_string()
            } else {
                format!("Friend, added as {added}")
            })
        });
    div()
        .id(("user-row", session as usize))
        .role(Role::TreeItem)
        .tab_index(0)
        .aria_label(format!(
            "{full_name}{}{}{}{}",
            if friend.is_some() { ", friend" } else { "" },
            if talking { ", talking" } else { "" },
            if muted { ", muted" } else { "" },
            if deafened { ", deafened" } else { "" }
        ))
        .when(full_name != name, |row| {
            row.tooltip(move |_, cx| cx.new(|_| Tooltip(full_name.clone().into())).into())
        })
        .aria_level(depth + 1)
        .flex()
        .items_center()
        .gap_2()
        .h(px(30.))
        .flex_shrink_0()
        .min_w_0()
        .pr_2()
        .pl(px(user_indent(depth)))
        .rounded(px(6.))
        .cursor_pointer()
        .text_color(color(if own { TEXT } else { TEXT_MUTED }))
        .hover(|style| style.bg(color(HOVER)).text_color(color(TEXT)))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_direct(server_index, session, window, cx);
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.open_menu(server_index, MenuTarget::User(session), event.position);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .child(avatar(
            &name,
            22.,
            talking,
            ("user-avatar", session as usize),
            avatar_image(this, server, Some(user)),
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .when(talking, |name| name.text_color(color(TEXT)))
                .child(text!(name).with_id(("user-name", session as usize))),
        )
        .when_some(friend, |row, friend| {
            row.child(
                div()
                    .id(("friend", session as usize))
                    .tooltip(move |_, cx| cx.new(|_| Tooltip(friend.clone())).into())
                    .child(icon("heart", 14., color(names::FRIEND))),
            )
        })
        .when(user.recording, |row| {
            row.child(
                div()
                    .id(("recording", session as usize))
                    .tooltip(|_, cx| cx.new(|_| Tooltip("Recording".into())).into())
                    .child(icon("record", 12., color(DANGER))),
            )
        })
        .when(user.priority_speaker, |row| {
            row.child(
                div()
                    .id(("priority", session as usize))
                    .tooltip(|_, cx| cx.new(|_| Tooltip("Priority speaker".into())).into())
                    .child(icon("star", 14., color(WARNING))),
            )
        })
        .when(muted, |row| {
            row.child(
                div()
                    .text_color(color(if user.muted || user.suppressed {
                        DANGER
                    } else {
                        TEXT_FAINT
                    }))
                    .child(icon(
                        "mic-off",
                        14.,
                        color(if user.muted || user.suppressed {
                            DANGER
                        } else {
                            TEXT_FAINT
                        }),
                    )),
            )
        })
        .when(deafened, |row| {
            row.child(
                div()
                    .text_color(color(if user.deafened { DANGER } else { TEXT_FAINT }))
                    .child(icon(
                        "headphones-off",
                        14.,
                        color(if user.deafened { DANGER } else { TEXT_FAINT }),
                    )),
            )
        })
        .into_any_element()
}

/// The decoded avatar for a user's texture hash, if we have one.
pub fn avatar_image(
    this: &Desktop,
    server: &ServerView,
    user: Option<&User>,
) -> Option<std::sync::Arc<RenderImage>> {
    let images = &this.ui.get(&server.id)?.avatar_images;
    user.and_then(|user| user.texture_hash.as_ref())
        .and_then(|hash| images.get(hash))
        .cloned()
}

pub fn avatar(
    name: &str,
    size: f32,
    talking: bool,
    id: (&'static str, usize),
    image: Option<std::sync::Arc<RenderImage>>,
) -> AnyElement {
    if let Some(image) = image {
        return div()
            .size(px(size))
            .flex_shrink_0()
            .rounded_full()
            .border_2()
            .border_color(if talking {
                color(SUCCESS)
            } else {
                color(SIDEBAR)
            })
            .overflow_hidden()
            .child(
                img(image)
                    .size_full()
                    .rounded_full()
                    .object_fit(ObjectFit::Cover),
            )
            .into_any_element();
    }
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .border_2()
        .border_color(if talking {
            color(SUCCESS)
        } else {
            color(SIDEBAR)
        })
        .bg(theme::avatar(name))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px((size * 0.42).max(9.)))
        .font_weight(FontWeight::BOLD)
        .text_color(color(WHITE))
        .child(text!(theme::initials(name)).with_id(id))
        .into_any_element()
}

fn user_panel(this: &Desktop, server: &ServerView, cx: &mut Context<Desktop>) -> AnyElement {
    let transmit = this.settings.audio.transmit;
    let own = server
        .own_session
        .and_then(|session| server.users.get(&session));
    let name = own
        .map(|user| user.name.clone())
        .unwrap_or_else(|| server.bookmark.username.clone());
    let talking = server
        .own_session
        .is_some_and(|session| server.talking.contains(&session));
    let connected = server.status == Status::Connected;
    // A short state fits beside the controls; the full audio message is the tooltip.
    let detail = server.audio_status.clone();
    let listening = this.client.server_index(&server.id).is_some_and(|index| {
        this.client.voice_role(index) == nuschl_client::voice::Role::Listening
    });
    let status = if !connected {
        server.status_label().to_string()
    } else if !server.has_mumble_voice() {
        "Voice unavailable".into()
    } else if server.deafened {
        "Deafened".into()
    } else if listening {
        // Your voice goes to another server; this one only plays.
        "Listening".into()
    } else if server.muted {
        "Muted".into()
    } else if this.push_muted {
        "Push to mute".into()
    } else if talking {
        "Talking".into()
    } else if detail.starts_with("Audio ready") {
        match transmit {
            nuschl_core::audio::TransmitMode::VoiceActivity => "Voice activity",
            nuschl_core::audio::TransmitMode::PushToTalk => "Push to talk",
            nuschl_core::audio::TransmitMode::Continuous => "Transmitting",
        }
        .into()
    } else {
        detail
            .split([':', '·'])
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    div()
        .min_h(px(56.))
        .py_2()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .px_2()
        .bg(color(SIDEBAR_FOOTER))
        .border_t_1()
        .border_color(color(BORDER))
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex_basis(px(96.))
                .flex()
                .items_center()
                .gap_2()
                .child(avatar(
                    &name,
                    34.,
                    talking,
                    ("own-avatar", 0),
                    avatar_image(this, server, own),
                ))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex_basis(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(text!(name).with_id("own-name")),
                        )
                        .child(
                            div()
                                .id("audio-status-line")
                                .text_xs()
                                .truncate()
                                .text_color(color(TEXT_MUTED))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| Tooltip(detail.clone().into())).into()
                                })
                                .child(text!(status).with_id("audio-status")),
                        ),
                ),
        )
        .child(
            div()
                .min_w_0()
                .max_w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .child(icon_button(
                    "open-settings",
                    "settings",
                    "Settings",
                    TEXT_MUTED,
                    None,
                    |this, _, cx| {
                        this.open_settings();
                        cx.notify();
                    },
                    cx,
                ))
                .when(connected, |panel| {
                    panel
                        .child(dialogs::toggle_button(
                            "mute",
                            "mic",
                            if server.muted { "Unmute" } else { "Mute" },
                            eased(&this.mute_ease, server.muted),
                            |this, _, cx| {
                                this.set_mute(false);
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(dialogs::toggle_button(
                            "deafen",
                            "headphones",
                            if server.deafened {
                                "Undeafen"
                            } else {
                                "Deafen"
                            },
                            eased(&this.deafen_ease, server.deafened),
                            |this, _, cx| {
                                this.set_mute(true);
                                cx.notify();
                            },
                            cx,
                        ))
                }),
        )
        .into_any_element()
}

/// How many messages arrived out of sight; red for mentions and direct messages.
fn unread_badge(channel: &crate::state::ChannelView) -> Option<AnyElement> {
    unread_badge_count(channel.id, channel.unread, channel.unread_mention)
}

fn unread_badge_count(id: u32, unread: usize, mention: bool) -> Option<AnyElement> {
    (unread > 0).then(|| {
        let count = if unread > 99 {
            "99+".to_string()
        } else {
            unread.to_string()
        };
        div()
            .id(("unread", id as usize))
            .aria_label(format!(
                "{} unread {}",
                unread,
                if unread == 1 { "message" } else { "messages" }
            ))
            .flex_shrink_0()
            .min_w(px(18.))
            .h(px(18.))
            .px(px(5.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .text_xs()
            .font_weight(FontWeight::BOLD)
            .text_color(color(WHITE))
            .bg(color(if mention { DANGER } else { ACCENT }))
            .child(count)
            .into_any_element()
    })
}

/// An animated toggle's value, or its end state before the first render.
fn eased(ease: &Option<crate::motion::Ease>, on: bool) -> f32 {
    ease.as_ref()
        .map_or(f32::from(u8::from(on)), crate::motion::Ease::value)
}

impl Desktop {
    /// Expand or collapse a run of activity lines (see `activity_run`).
    fn toggle_run(&mut self, server: usize, channel: usize, key: i64) {
        if let Some(id) = self
            .client
            .servers
            .get(server)
            .and_then(|server| server.channels.get(channel))
            .map(|channel| channel.id)
            && let Some(ui) = self.ui_of_mut(server)
        {
            let runs = ui.expanded_runs.entry(id).or_default();
            if !runs.remove(&key) {
                runs.insert(key);
            }
        }
    }

    /// A key in the focused timeline, whose keyboard is on entry `current` of
    /// `keys` (`heights` tall): the arrows, Page Up and Down, Home and End
    /// move it and bring the entry into view (at the newest, the timeline
    /// follows new messages again); Enter or Space shows all of a long
    /// message, or a run of activity lines.
    fn timeline_key(
        &mut self,
        keys: &[Key],
        heights: &[Pixels],
        current: usize,
        keystroke: &Keystroke,
        viewport: Pixels,
    ) -> bool {
        if keystroke.modifiers.modified() {
            return false;
        }
        if matches!(keystroke.key.as_str(), "enter" | "space") {
            return self.activate_entry(keys[current]);
        }
        let Some(to) = crate::scrolling::step(heights, current, &keystroke.key, viewport) else {
            return false;
        };
        self.anchor.set_cursor(keys[to]);
        self.focus_link(None);
        if to + 1 == keys.len() {
            self.jump_to_latest();
        } else {
            self.client.focus.following = false;
            self.anchor.show(keys[to]);
        }
        true
    }

    fn activate_entry(&mut self, key: Key) -> bool {
        let Some((server, index)) = self.selected_indices() else {
            return false;
        };
        let channel = &self.client.servers[server].channels[index];
        match key {
            Key::Message(serial) => {
                let shown = channel
                    .timeline
                    .messages
                    .iter()
                    .any(|message| message.serial == serial);
                if !shown || !self.presentation.message(serial).has_more() {
                    return false;
                }
                self.toggle_message(serial);
            }
            Key::Run(serial) => {
                let Some(line) = channel.activity.iter().find(|line| line.serial == serial) else {
                    return false;
                };
                self.toggle_run(server, index, line.observed_at_ms);
            }
            Key::Intro | Key::Activity(_) => return false,
        }
        true
    }

    /// The conversation in sight is read: selected, the window in front and
    /// scrolled to the newest message.
    fn mark_read(&mut self) {
        self.client.mark_read();
    }

    /// The open dialogs as a set, to notice when one opens or closes.
    fn open_dialogs(&self) -> u8 {
        u8::from(self.form.is_some())
            | u8::from(self.settings_open) << 1
            | u8::from(self.panel.is_some()) << 2
            | u8::from(self.prompt.is_some()) << 3
            | u8::from(self.finder.is_open()) << 4
            | u8::from(self.second_server.is_some()) << 5
    }

    pub fn dialog_open(&self) -> bool {
        self.form.is_some()
            || self.settings_open
            || self.panel.is_some()
            || self.prompt.is_some()
            || self.finder.is_open()
            || self.second_server.is_some()
    }

    /// While a dialog is open, keyboard focus stays inside it, so nothing
    /// behind it (the composer: Enter would send) takes keys; when it closes,
    /// the composer gets focus back.
    fn keep_dialog_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.open_dialogs();
        let changed = std::mem::replace(&mut self.dialogs_seen, open) != open;
        if self.dialog_open() {
            if changed {
                // Focus is judged against the last drawn frame, which does not
                // contain a dialog opened since. The field its opener focused
                // (a prompt's input, the form's address) would look outside it
                // and lose focus, so check again once the dialog is drawn.
                window.request_animation_frame();
            } else if !self.modal_focus.contains_focused(window, cx) {
                window.focus(&self.modal_focus, cx);
            }
        } else if self.modal_seen.get().is_some() {
            self.composer
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Retarget the footer's mute and deafen animations to the selected
    /// server's state, and keep frames coming while they move.
    fn ease_voice_toggles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, _)) = self.selected_indices() else {
            return;
        };
        let (muted, deafened) = (
            self.client.servers[server].muted,
            self.client.servers[server].deafened,
        );
        let reduce = cx.reduce_motion();
        let mute = self
            .mute_ease
            .get_or_insert_with(|| crate::motion::Ease::new(muted));
        mute.set(muted, reduce);
        let moving = mute.moving();
        let deafen = self
            .deafen_ease
            .get_or_insert_with(|| crate::motion::Ease::new(deafened));
        deafen.set(deafened, reduce);
        if moving || deafen.moving() {
            window.request_animation_frame();
        }
    }
}

/// Context and controls wrap as complete lanes when their fixed slots exceed
/// the available pane width. Toolbar actions never live outside the viewport.
fn conversation_header(
    this: &Desktop,
    index: usize,
    channel: &crate::state::ChannelView,
    topic: &str,
    is_channel: bool,
    in_this_voice: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = &this.client.servers[index];
    let connected = server.status == Status::Connected;
    let context = div()
        .min_w_0()
        .flex_1()
        .flex_basis(px(120.))
        .flex()
        .items_center()
        .gap_2()
        .child(layout::fixed().child(icon(
            if is_channel { "hash" } else { "message" },
            20.,
            color(TEXT_FAINT),
        )))
        .child(
            layout::flexible()
                .flex()
                .flex_col()
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .child(layout::label("channel-heading", channel.name.clone())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(TEXT_MUTED))
                        .child(layout::label("channel-topic", topic.to_owned())),
                ),
        );
    let controls = div()
        .min_w_0()
        .max_w_full()
        .flex_shrink_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .when(
            connected
                && server.has_mumble_voice()
                && is_channel
                && !in_this_voice
                && server.may_try(channel.id, nuschl_session::model::permission::ENTER),
            |controls| {
                controls.child(styled_button(
                    "join-voice",
                    "Join voice",
                    Tone::Primary,
                    |this, _, cx| {
                        this.join_selected();
                        cx.notify();
                    },
                    cx,
                ))
            },
        )
        .when(!connected, |controls| {
            controls.child(styled_button(
                "connect-server",
                if server.status == Status::Connecting {
                    "Connecting…"
                } else {
                    "Connect"
                },
                Tone::Primary,
                move |this, _, cx| {
                    if this.client.servers[index].status == Status::Disconnected {
                        this.connect(index, None);
                    }
                    cx.notify();
                },
                cx,
            ))
        })
        .child(icon_button(
            "open-search",
            "search",
            "Search messages",
            TEXT_MUTED,
            crate::search::panel_open(this).then_some(ACCENT),
            |this, window, cx| {
                this.toggle_search(window, cx);
                cx.notify();
            },
            cx,
        ))
        .child({
            let (security, label) = this.conversation_lock(index);
            div()
                .id("conversation-security")
                .flex_shrink_0()
                .px_2()
                .tooltip({
                    let label = label.clone();
                    move |_, cx| cx.new(|_| Tooltip(label.clone().into())).into()
                })
                .child(security_icon_labelled(
                    security,
                    label,
                    this.client.servers.len() + index,
                    18.,
                ))
        });
    div()
        .w_full()
        .min_w_0()
        .min_h(px(52.))
        .flex_shrink_0()
        .px_4()
        .py_2()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(color(BORDER))
        .child(context)
        .child(controls)
        .into_any_element()
}

fn conversation(
    this: &Desktop,
    index: usize,
    channel_index: usize,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server_index = index;
    let server = &this.client.servers[index];
    let channel = &server.channels[channel_index];
    let is_channel = matches!(channel.target, TextTarget::Channel(_));
    let connected = server.status == Status::Connected;
    let in_this_voice = server
        .voice_channel()
        .is_some_and(|voice| channel.target == TextTarget::Channel(voice));
    let anchor = this.anchor.frame();
    // The timeline in order: activity lines among the messages, by time.
    let messages = &channel.timeline.messages;
    let expanded = this
        .ui
        .get(&server.id)
        .and_then(|ui| ui.expanded_runs.get(&channel.id));
    let entries = channel.entries(|first| expanded.is_some_and(|runs| runs.contains(&first)));
    // Only what is near the viewport is laid out, the welcome first.
    let height = |entry: &Entry| match *entry {
        Entry::Activity(index) => {
            &this
                .presentation
                .activity(channel.activity[index].serial)
                .height
        }
        Entry::Run { first, .. } => {
            &this
                .presentation
                .activity(channel.activity[first].serial)
                .run_height
        }
        Entry::Message { position, .. } => {
            &this.presentation.message(messages[position].serial).height
        }
    };
    let heights: Rc<[Pixels]> = std::iter::once(this.anchor.intro())
        .chain(entries.iter().map(height))
        .map(|height| height.get())
        .collect();
    let keys: Rc<[Key]> = std::iter::once(Key::Intro)
        .chain(entries.iter().map(|entry| match *entry {
            Entry::Activity(index) => Key::Activity(channel.activity[index].serial),
            Entry::Run { first, .. } => Key::Run(channel.activity[first].serial),
            Entry::Message { position, .. } => Key::Message(messages[position].serial),
        }))
        .collect();
    // What waits to be revealed is laid out wherever it is (a search result,
    // or where the keyboard or a screen reader went); otherwise the newest,
    // or below the timeline's top padding.
    let revealing = this.anchor.revealing().and_then(|reveal| match reveal {
        Reveal::Message(id) => entries
            .iter()
            .position(|entry| {
                matches!(*entry, Entry::Message { position, .. } if messages[position].id == Some(id))
            })
            .map(|entry| entry + 1),
        Reveal::Entry(key) => keys.iter().position(|entry| *entry == key),
    });
    let top = -this.scroll.offset().y - px(16.);
    let focus = match revealing {
        Some(index) => Focus::Entry(index),
        None if this.client.focus.following => Focus::Newest,
        None => Focus::Offset(top),
    };
    let viewport = window.viewport_size().height;
    let shown = crate::scrolling::shown(&heights, focus, viewport);
    // The keyboard's entry: where it moved, else the newest or the first in view.
    let current = this
        .anchor
        .cursor()
        .and_then(|key| keys.iter().position(|entry| *entry == key))
        .unwrap_or_else(|| {
            if this.client.focus.following {
                keys.len() - 1
            } else {
                crate::scrolling::at(&heights, top)
            }
        });
    let ring = this.text.focus.is_focused(window) && window.last_input_was_keyboard();
    // For the keyboard outside rendering: Tab through its links, Enter.
    this.timeline_current.set(keys.get(current).copied());
    // Screen readers get every entry by its label (see `Unseen` for those
    // not laid out); authors are looked up once a frame.
    let a11y = window.is_a11y_active();
    let mut authors: HashMap<&str, &User> = HashMap::new();
    if a11y {
        let users = server.users.values().chain(&channel.direct_peer);
        authors.extend(users.map(|user| (user.name.as_str(), user)));
    }
    let label = |index: usize| {
        let entry = index.checked_sub(1).map(|entry| &entries[entry]);
        entry_label(this, server, channel, &authors, entry)
    };
    let action = entry_action(cx);
    let item = |index: usize| {
        entry_item(keys[index], a11y.then(|| label(index)).flatten(), &action)
            .when(index == current, |item| item.aria_active_descendant())
    };
    let rows: Vec<AnyElement> = shown
        .range
        .clone()
        .map(|index| {
            let (row, id, height) = match index.checked_sub(1).map(|entry| &entries[entry]) {
                None => (
                    timeline_intro(this, server, channel, cx),
                    None,
                    this.anchor.intro(),
                ),
                Some(&Entry::Activity(index)) => {
                    let line = &channel.activity[index];
                    let height = &this.presentation.activity(line.serial).height;
                    (activity_row(line, index), None, height)
                }
                Some(&Entry::Run {
                    first,
                    last,
                    expanded,
                }) => {
                    let row = activity_run(
                        channel,
                        (server_index, channel_index),
                        first,
                        last,
                        expanded,
                        cx,
                    );
                    let serial = channel.activity[first].serial;
                    (row, None, &this.presentation.activity(serial).run_height)
                }
                Some(&Entry::Message { position, grouped }) => {
                    let message = &messages[position];
                    let author = names::author(
                        &message.author,
                        server.users.values(),
                        channel.direct_peer.as_ref(),
                    );
                    let row = message_row(this, server, channel, position, author, grouped, cx);
                    let row = crate::search::mark_jumped(this, message.id, row);
                    let height = &this.presentation.message(message.serial).height;
                    (row, message.id, height)
                }
            };
            // Wrapped only for a screen reader or the focus ring: an element
            // more per row costs every frame.
            let ring = ring && index == current;
            let row = if a11y || ring {
                let item = item(index).relative().child(row);
                let outline = div().absolute().inset_0().rounded(px(6.)).border_2();
                item.when(ring, |item| item.child(outline.border_color(color(ACCENT))))
                    .into_any_element()
            } else {
                row
            };
            anchor.row(keys[index], id, height, row)
        })
        .collect();
    // The rest only for screen readers. The keyboard's entry stays an
    // element that can be the focused one, should it be scrolled away.
    let visible = this.scroll.bounds();
    let scale = window.scale_factor();
    let unseen = |range: std::ops::Range<usize>, y: Pixels| -> Vec<AnyElement> {
        let at = Bounds::new(point(visible.left(), y), size(px(1.), px(1.)));
        range
            .filter(|_| a11y)
            .filter_map(|index| {
                if index == current {
                    return Some(item(index).into_any_element());
                }
                let label = label(index)?.into();
                let (key, action) = (keys[index], action.clone());
                Some(
                    Unseen {
                        key,
                        label,
                        at,
                        scale,
                        action,
                    }
                    .into_any_element(),
                )
            })
            .collect()
    };
    let (above, below) = (
        unseen(0..shown.range.start, visible.top() - px(1.)),
        unseen(shown.range.end..keys.len(), visible.bottom()),
    );
    // Prepending happens while the reader looks up the timeline, with no
    // button or separate mode: the anchor keeper asks once the top is near.
    let older =
        (channel.timeline.next_before.is_some() && !channel.timeline.loading && !this.older_paused)
            .then(|| {
                let desktop = cx.entity().downgrade();
                move |cx: &mut App| {
                    let _ = desktop.update(cx, |this, cx| {
                        if this.load_older() {
                            cx.notify();
                        }
                    });
                }
            });
    let topic = if !is_channel {
        "Private conversation".to_string()
    } else if !server.has_mumble_voice() {
        "End-to-end voice is not available yet on this server.".into()
    } else if in_this_voice {
        "You are talking here".to_string()
    } else if connected {
        "Double-click the channel or use Join voice to talk here".to_string()
    } else {
        channel.topic.clone()
    };
    let dragged = this.dragged_files.clone();
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .relative()
        .flex()
        .flex_col()
        .bg(color(MAIN))
        // Asked about whatever is being dragged, hovered or not (see file_drop).
        .can_drop(move |value, _, _| {
            if let Some(paths) = value.downcast_ref::<ExternalPaths>() {
                dragged.replace(Some(paths.paths().to_vec()));
            }
            true
        })
        .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(color(HOVER)))
        .child(conversation_header(
            this,
            index,
            channel,
            &topic,
            is_channel,
            in_this_voice,
            cx,
        ))
        .child(conversation_notices(this, index, window, cx))
        .map(|view| match crate::search::panel(this, cx) {
            Some(panel) => view.child(panel),
            None => view.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(anchor.keeper(&this.scroll, !this.client.focus.following, older))
                    .when_some(this.text.selection, |view, selection| {
                        view.child(selection::timeline_handlers(cx.weak_entity(), selection))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "timeline:{}:{}",
                                server.id, channel.id
                            )))
                            .track_scroll(&this.scroll)
                            // A list for screen readers; a tab stop, whose
                            // keyboard moves through the entries (and copies a
                            // selection of their text).
                            .role(Role::List)
                            .aria_label(if is_channel {
                                format!("Messages in #{}", channel.name)
                            } else {
                                format!("Messages with {}", channel.name)
                            })
                            .track_focus(&this.text.focus)
                            .on_key_down(cx.listener(
                                move |this, event: &KeyDownEvent, window, cx| {
                                    let keystroke = &event.keystroke;
                                    // Enter opens the link the keyboard is on, or
                                    // the message's only one (timeline_keys.rs).
                                    let link = keystroke.key == "enter"
                                        && !keystroke.modifiers.modified()
                                        && this.open_keyboard_link(window, cx);
                                    if link
                                        || keystroke.key == "c"
                                            && keystroke.modifiers.secondary(cx)
                                            && this.copy_selection(cx)
                                        || this.timeline_key(
                                            &keys, &heights, current, keystroke, viewport,
                                        )
                                        || this.type_into_composer(keystroke, window, cx)
                                    {
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                },
                            ))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_4()
                            .py_4()
                            .child(
                                div()
                                    .id("timeline-content")
                                    .min_h_full()
                                    .flex()
                                    .flex_col()
                                    .justify_end()
                                    .on_scroll(cx.listener(|this, event, window, cx| {
                                        this.smooth_scroll(false, event, window, cx)
                                    }))
                                    .child(div().flex_shrink_0().h(shown.above).children(above))
                                    .children(rows)
                                    .child(div().flex_shrink_0().h(shown.below).children(below)),
                            ),
                    )
                    .when_some(jump_pill(this, channel, cx), |view, pill| view.child(pill)),
            ),
        })
        .child(composer(this, server, channel, window, cx))
        .child(file_drop(this, cx))
        .into_any_element()
}

/// Files dropped anywhere on the conversation are attached. GPUI only drops
/// on a hovered element, and after typing nothing counts as hovered until the
/// pointer moves, which a drag from another application never does: so the
/// conversation remembers what is dragged over it and takes the drop by
/// position instead.
fn file_drop(this: &Desktop, cx: &mut Context<Desktop>) -> impl IntoElement {
    let dragged = this.dragged_files.clone();
    let desktop = cx.entity().downgrade();
    canvas(
        move |_, _, cx| {
            if !cx.has_active_drag() {
                dragged.take();
            }
            dragged
        },
        move |bounds, dragged, window, _| {
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble
                    && bounds.contains(&event.position)
                    && let Some(paths) = dragged.take()
                {
                    let _ = desktop.update(cx, |this, cx| this.attach_files(paths, cx));
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// The top of the timeline: older history on its way, or what the
/// conversation is and where its local history begins.
fn timeline_intro(
    this: &Desktop,
    server: &ServerView,
    channel: &crate::state::ChannelView,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let is_channel = matches!(channel.target, TextTarget::Channel(_));
    if !channel.timeline.loaded && channel.timeline.loading {
        // The first page usually arrives within a frame or two: showing the
        // welcome or a loading line meanwhile would only flash and shift.
        return div().into_any_element();
    }
    let loading = channel.timeline.loading.then(|| {
        let label = "Loading older messages…";
        div()
            .id("history-loading")
            .role(Role::Status)
            .aria_label(label)
            .py_2()
            .flex()
            .justify_center()
            .text_sm()
            .text_color(color(TEXT_FAINT))
            .child(text!(label).with_id("history-loading-label"))
    });
    if channel.timeline.next_before.is_some() {
        // More above: the welcome would only flash by before the next page.
        // The indicator's room is kept, so it appears without moving anything.
        return div()
            .h(px(40.))
            .flex()
            .flex_col()
            .justify_center()
            .children(loading)
            .into_any_element();
    }
    div()
        .pb_4()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .size(px(56.))
                .rounded_full()
                .bg(color(ELEVATED))
                .flex()
                .items_center()
                .justify_center()
                .text_color(color(TEXT_MUTED))
                .child(icon(
                    if is_channel { "hash" } else { "message" },
                    30.,
                    color(TEXT_MUTED),
                )),
        )
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .child(text!(format!("Welcome to {}", channel.name)).with_id("timeline-heading")),
        )
        .child(
            div().text_sm().text_color(color(TEXT_MUTED)).child(
                // Honest per conversation: stock direct messages on a Nuschl
                // server are what this device saw.
                text!(match server.history_source {
                    HistorySource::ServerArchive if server.archived(channel) => "Server history",
                    _ => "Local history · messages seen on this device",
                })
                .with_id("history-source"),
            ),
        )
        .when_some(
            this.ui
                .get(&server.id)
                .and_then(|ui| ui.descriptions.get(&channel.id))
                // Rendered from the description shown now, not an older one.
                .filter(|(source, _)| is_channel && *source == channel.description_raw)
                .map(|(_, document)| document),
            |view, description| {
                view.child(
                    div()
                        .id("channel-description")
                        .mt_2()
                        .p_3()
                        .rounded(px(8.))
                        .bg(color(SIDEBAR))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .text_sm()
                        .children(document_blocks(this, description, cx)),
                )
            },
        )
        .when_some(
            server
                .welcome
                .as_ref()
                .filter(|_| is_channel && channel.id == 0),
            |view, welcome| {
                view.child(
                    div()
                        .mt_2()
                        .p_3()
                        .rounded(px(8.))
                        .bg(color(SIDEBAR))
                        .border_l_4()
                        .border_color(color(ACCENT))
                        .text_sm()
                        .child(text!(welcome.clone()).with_id("server-welcome")),
                )
            },
        )
        .children(loading)
        .when(
            channel.timeline.beginning && !channel.timeline.loading,
            |view| view.child(history_start(server.archived(channel))),
        )
        .into_any_element()
}

/// Where local history starts: nothing older was seen on this device.
fn history_start(server: bool) -> AnyElement {
    let rule = || div().flex_1().h(px(1.)).bg(color(BORDER));
    let label = if server {
        "Beginning of server history"
    } else {
        "Beginning of local history"
    };
    div()
        .id("history-start")
        // A status, like the activity lines: announced once scrolling reaches it.
        .role(Role::Status)
        .aria_label(label)
        .mt_3()
        .flex()
        .items_center()
        .gap_3()
        .text_xs()
        .text_color(color(TEXT_FAINT))
        .child(rule())
        .child(text!(label).with_id("history-start-label"))
        .child(rule())
        .into_any_element()
}

/// Floats over the timeline's bottom edge while the reader is scrolled up:
/// with what has arrived since, or once the latest is far below.
fn jump_pill(
    this: &Desktop,
    channel: &crate::state::ChannelView,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    let below = this.scroll.max_offset().y + this.scroll.offset().y;
    let far = below > this.scroll.bounds().size.height * 2.;
    if this.client.focus.following || !(channel.new_messages > 0 || far) {
        return None;
    }
    let label = match channel.new_messages {
        0 => "Jump to latest".to_string(),
        1 => "1 new message · Jump to latest".to_string(),
        count => format!("{count} new messages · Jump to latest"),
    };
    Some(
        div()
            .absolute()
            .bottom(px(12.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("jump-to-latest")
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .tab_index(0)
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_4()
                    .py(px(6.))
                    .rounded_full()
                    .bg(color(ACCENT))
                    .hover(|style| style.bg(color(ACCENT_HOVER)))
                    .shadow_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color(WHITE))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.jump_to_latest();
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if activation_key(event) {
                            this.jump_to_latest();
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(icon("chevron-down", 16., color(WHITE)))
                    .child(text!(label).with_id("jump-to-latest-label")),
            )
            .into_any_element(),
    )
}

/// What a run of activity lines says ("Ada connected, Bob moved to Lobby and
/// 3 more", or "5 updates" when expanded), and when ("Today at 05:27", or
/// "Today at 05:27–05:31" within one day).
fn run_summary(
    channel: &crate::state::ChannelView,
    first: usize,
    last: usize,
    expanded: bool,
) -> (String, String) {
    let lines = &channel.activity;
    let count = last - first + 1;
    let summary = if expanded {
        format!("{count} updates")
    } else if count == 2 {
        format!("{}, {}", lines[first].text, lines[last].text)
    } else {
        format!(
            "{}, {} and {} more",
            lines[first].text,
            lines[first + 1].text,
            count - 2
        )
    };
    let (start, end) = (
        crate::state::local_time(lines[first].observed_at_ms),
        crate::state::local_time(lines[last].observed_at_ms),
    );
    let times = match (start.rsplit_once(' '), end.rsplit_once(' ')) {
        _ if start == end => start,
        (Some((day, _)), Some((same, clock))) if day == same => format!("{start}–{clock}"),
        _ => format!("{start} – {end}"),
    };
    (summary, times)
}

/// A screen reader asking to see a timeline entry, or to move the keyboard
/// there: it comes into view as the keyboard would bring it.
fn entry_action(cx: &mut Context<Desktop>) -> EntryAction {
    let desktop = cx.weak_entity();
    Rc::new(move |key, action, window, cx| {
        let _ = desktop.update(cx, |this, cx| {
            this.client.focus.following = false;
            this.anchor.show(key);
            if action == AccessibleAction::Focus {
                this.anchor.set_cursor(key);
                window.focus(&this.text.focus, cx);
            }
            cx.notify();
        });
    })
}

/// A timeline entry as screen readers see it: a list item named `label`
/// (none while no screen reader listens) that they can scroll into view and
/// move the keyboard to.
fn entry_item(key: Key, label: Option<String>, action: &EntryAction) -> Stateful<Div> {
    let item = div().id(key.element_id());
    let Some(label) = label else {
        return item;
    };
    let on = |kind| {
        let action = action.clone();
        move |_: Option<&accesskit::ActionData>, window: &mut Window, cx: &mut App| {
            action(key, kind, window, cx)
        }
    };
    item.role(Role::ListItem)
        .aria_label(label)
        .on_a11y_action(
            AccessibleAction::ScrollIntoView,
            on(AccessibleAction::ScrollIntoView),
        )
        .on_a11y_action(AccessibleAction::Focus, on(AccessibleAction::Focus))
}

/// What a screen reader reads for a timeline entry, or for the welcome
/// above them (`entry` none; nothing while the first page loads).
fn entry_label(
    this: &Desktop,
    server: &ServerView,
    channel: &crate::state::ChannelView,
    authors: &HashMap<&str, &User>,
    entry: Option<&Entry>,
) -> Option<String> {
    let time = crate::state::local_time;
    let Some(entry) = entry else {
        if channel.timeline.loading {
            return channel
                .timeline
                .loaded
                .then(|| "Loading older messages…".into());
        }
        if channel.timeline.next_before.is_some() {
            return Some("Older messages".into());
        }
        let source = if server.archived(channel) {
            "Server history"
        } else {
            "Local history, messages seen on this device"
        };
        let start = match (channel.timeline.beginning, server.archived(channel)) {
            (true, true) => ". Beginning of server history",
            (true, false) => ". Beginning of local history",
            (false, _) => "",
        };
        return Some(format!("Welcome to {}. {source}{start}", channel.name));
    };
    Some(match *entry {
        Entry::Activity(index) => {
            let line = &channel.activity[index];
            format!("{}, {}", line.text, time(line.observed_at_ms))
        }
        Entry::Run {
            first,
            last,
            expanded,
        } => {
            let (summary, times) = run_summary(channel, first, last, expanded);
            format!("{summary}, {times}")
        }
        Entry::Message { position, .. } => {
            let message = &channel.timeline.messages[position];
            let ui = this.presentation.message(message.serial);
            let author = authors.get(message.author.as_str()).copied();
            let name = names::author_name(&message.author, author, &this.settings);
            if let Some(deletion) = message.deleted {
                return Some(format!(
                    "{name}: message deleted{}, {}",
                    if deletion.moderator {
                        " by a moderator"
                    } else {
                        ""
                    },
                    ui.time
                ));
            }
            let mut text = ui.document.plain_text();
            let images = ui.document.blocks.iter();
            let images = images.filter(|block| matches!(block, Block::Image { .. }));
            match images.count() {
                0 => {}
                1 if text.is_empty() => text = "Image".into(),
                1 => text.push_str(" (image)"),
                count => text.push_str(&format!(" ({count} images)")),
            }
            let status = message.status.label().map(|status| format!(", {status}"));
            // Tab reaches them (timeline_keys.rs).
            match crate::timeline_keys::message_links(message, &ui.document).len() {
                0 => {}
                1 => text.push_str(", 1 link"),
                count => text.push_str(&format!(", {count} links")),
            }
            let edited = if message.edited_ms.is_some() {
                ", edited"
            } else {
                ""
            };
            format!(
                "{name}: {text}, {}{edited}{}",
                ui.time,
                status.unwrap_or_default()
            )
        }
    })
}

/// A run of routine activity lines as one line ("Ada connected, Bob moved to
/// Lobby and 3 more · 12:01–12:05"); a click expands it, and when expanded
/// it is the header that collapses it again.
fn activity_run(
    channel: &crate::state::ChannelView,
    (server, channel_index): (usize, usize),
    first: usize,
    last: usize,
    expanded: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let count = last - first + 1;
    let (summary, times) = run_summary(channel, first, last, expanded);
    let key = channel.activity[first].observed_at_ms;
    div()
        .id(("activity-run", first))
        .role(Role::Button)
        .aria_label(if expanded {
            format!("{count} updates, expanded; collapse")
        } else {
            format!("{summary}; show all {count}")
        })
        .aria_expanded(expanded)
        .tab_index(0)
        .cursor_pointer()
        .px_2()
        .py_1()
        .rounded(px(6.))
        .flex()
        .items_center()
        .gap_3()
        .text_sm()
        .text_color(color(TEXT_FAINT))
        .hover(|style| style.bg(color(HOVER)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_run(server, channel_index, key);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if activation_key(event) {
                this.toggle_run(server, channel_index, key);
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .child(div().w(px(40.)).flex().justify_center().child(icon(
            if expanded {
                "chevron-down"
            } else {
                "chevron-right"
            },
            14.,
            color(TEXT_FAINT),
        )))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(text!(summary).with_id(("activity-run-text", first))),
        )
        .child(
            div()
                .text_xs()
                .child(text!(times).with_id(("activity-run-time", first))),
        )
        .into_any_element()
}

fn activity_row(line: &crate::state::ActivityLine, index: usize) -> AnyElement {
    let tone = if line.warning { WARNING } else { TEXT_FAINT };
    div()
        .id(("activity", index))
        .role(Role::Status)
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .gap_3()
        .text_sm()
        .text_color(color(tone))
        .child(div().w(px(40.)).flex().justify_center().child(icon(
            if line.warning { "alert" } else { "info" },
            14.,
            color(tone),
        )))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(text!(line.text.clone()).with_id(("activity-text", index))),
        )
        .child(div().text_xs().child(
            text!(crate::state::local_time(line.observed_at_ms)).with_id(("activity-time", index)),
        ))
        .into_any_element()
}

/// A release is available, downloading or installed (Windows and macOS).
fn update_bar(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    use crate::update::UpdateState;
    let (text, action): (String, Option<AnyElement>) = match &this.update {
        UpdateState::Available(update) => (
            format!("Nuschl {} is available.", update.version),
            Some(styled_button(
                "install-update",
                "Update and restart",
                Tone::Primary,
                |this, _, cx| {
                    this.install_update();
                    cx.notify();
                },
                cx,
            )),
        ),
        UpdateState::Installing { done, total } => (
            if *total > 0 {
                format!(
                    "Downloading the update… {}%",
                    done.saturating_mul(100) / total
                )
            } else {
                "Downloading the update…".into()
            },
            None,
        ),
        UpdateState::Installed => (
            "The update is installed.".into(),
            Some(styled_button(
                "restart-update",
                "Restart now",
                Tone::Primary,
                |this, _, cx| match crate::update::relaunch() {
                    Ok(()) => cx.quit(),
                    Err(error) => {
                        this.update = crate::update::UpdateState::Failed(error);
                        cx.notify();
                    }
                },
                cx,
            )),
        ),
        _ => return None,
    };
    Some(
        div()
            .id("update-bar")
            .role(Role::Status)
            .flex_shrink_0()
            .w_full()
            .min_w_0()
            .px_4()
            .py(px(6.))
            .flex()
            .items_center()
            .gap_3()
            .bg(tint(ACCENT, 0x26))
            .border_b_1()
            .border_color(tint(ACCENT, 0x55))
            .text_sm()
            .child(icon("star", 16., color(ACCENT_HOVER)))
            .child(layout::label("update-text", text))
            .when_some(action, |bar, action| bar.child(action))
            .into_any_element(),
    )
}

pub(crate) fn banner(
    id: &'static str,
    tone: Token,
    icon_name: &'static str,
    text: String,
    action: Option<AnyElement>,
) -> AnyElement {
    let body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(layout::text((id, 1_usize), text))
        .when_some(action, |body, action| {
            body.child(layout::actions().child(action))
        })
        .into_any_element();
    layout::notice(id, tone, icon_name, body, None).into_any_element()
}

/// Every conversation warning shares one scroll budget. A long path in one
/// notice cannot allocate another viewport or move its close action offscreen.
fn conversation_notices(
    this: &Desktop,
    index: usize,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = &this.client.servers[index];
    let mut notices = Vec::new();
    // Security decisions come first; a verbose informational notice must not
    // bury a certificate or downgrade prompt below itself.
    if let Some(certificate) = &server.certificate {
        notices.push(certificate_card(certificate, index, cx));
    }
    if let Some(reason) = &server.downgrade {
        notices.push(downgrade_card(reason, index, cx));
    }
    if let Some(text) = crate::panels::reconnect_banner(server) {
        notices.push(banner("reconnect-banner", WARNING, "signal", text, None));
    }
    if let Some(version) = crate::version_notice::banner(this, index, cx) {
        notices.push(version);
    }
    if let Some(text) = &server.whisper {
        notices.push(banner(
            "whisper-banner",
            ACCENT,
            "whisper",
            format!("{text} · your voice goes only there"),
            Some(styled_button(
                "stop-whisper",
                "Stop",
                Tone::Secondary,
                move |this, _, cx| {
                    this.stop_whisper(index);
                    cx.notify();
                },
                cx,
            )),
        ));
    }
    if let Some(notice) = server.notice.clone().or_else(|| this.error.clone()) {
        let from_server = server.notice.is_some();
        let dismiss = icon_button(
            "dismiss-connection-notice",
            "window-close",
            "Dismiss notice",
            DANGER_TEXT,
            None,
            move |this, _, cx| {
                if from_server {
                    this.client.servers[index].dismiss_notice();
                } else {
                    this.error = None;
                }
                cx.notify();
            },
            cx,
        );
        notices.push(
            layout::notice(
                "connection-notice-banner",
                DANGER,
                "alert",
                layout::text("connection-notice", notice),
                Some(dismiss),
            )
            .aria_label("Connection notice")
            .text_color(color(DANGER_TEXT))
            .into_any_element(),
        );
    }
    if let Some(text) = &server.suggestion {
        let ptt = text.contains("push-to-talk");
        let actions = layout::actions()
            .when(ptt, |actions| {
                actions.child(styled_button(
                    "use-ptt",
                    "Use push-to-talk",
                    Tone::Primary,
                    move |this, _, cx| {
                        this.update_settings(|settings| {
                            settings.audio.transmit = nuschl_core::audio::TransmitMode::PushToTalk
                        });
                        this.client.servers[index].suggestion = None;
                        if !this
                            .settings
                            .shortcuts
                            .contains_key(&nuschl_session::config::ShortcutAction::PushToTalk)
                        {
                            this.open_settings();
                        }
                        cx.notify();
                    },
                    cx,
                ))
            })
            .child(styled_button(
                "dismiss-suggest",
                "Dismiss",
                Tone::Ghost,
                move |this, _, cx| {
                    this.client.servers[index].suggestion = None;
                    cx.notify();
                },
                cx,
            ))
            .into_any_element();
        notices.push(banner(
            "suggest-banner",
            ACCENT,
            "info",
            text.clone(),
            Some(actions),
        ));
    }
    layout::notices(window, cx, notices)
}

fn message_row(
    this: &Desktop,
    server: &ServerView,
    channel: &crate::state::ChannelView,
    index: usize,
    author: Option<&User>,
    grouped: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let message = &channel.timeline.messages[index];
    let ui = this.presentation.message(message.serial);
    if let Some(deletion) = message.deleted {
        return tombstone_row(this, message, index, deletion, author, grouped);
    }
    // A file offer shows as a card in place of its text (transfer.rs).
    let card = crate::transfer::card(this, server, channel, index, cx);
    let editing = this
        .editing
        .as_ref()
        .is_some_and(|editing| editing.serial == message.serial);
    let serial = message.serial;
    let name = names::author_name(&message.author, author, &this.settings).to_string();
    // With a nickname shown, the name they sent it under.
    let real_name = author
        .map(|author| names::full_name(author, &this.settings))
        .filter(|full| *full != name);
    let order = selection::order(message);
    let selected = this
        .text
        .selection
        .and_then(|selection| selection.carets(order));
    let hovered = this
        .text
        .hovered
        .as_ref()
        .filter(|hovered| hovered.message == serial);
    let mut texts = Vec::new();
    // A file offer shows as a card in place of its text (transfer.rs).
    let blocks = card.map_or_else(
        || blocks(this, &ui.document, selected, hovered, &mut texts, cx),
        |card| vec![card],
    );
    div()
        .id(("message", index))
        .when_some(
            hovered.filter(|hovered| hovered.deceptive && this.menu.is_none()),
            |row, hovered| row.child(link_target(hovered)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.open_message_menu(serial, event.position);
                cx.notify();
            }),
        )
        .flex()
        .gap_3()
        .px_2()
        .when(grouped, |row| row.py(px(2.)))
        .when(!grouped, |row| row.pt_3().pb(px(2.)))
        .rounded(px(6.))
        .group("message")
        .hover(|style| style.bg(color(ROW_HOVER)))
        // The message the composer edits.
        .when(editing, |row| row.bg(tint(ACCENT, 0x1a)))
        .child(if grouped {
            div().w(px(40.)).flex_shrink_0().into_any_element()
        } else {
            div()
                .w(px(40.))
                .flex_shrink_0()
                .child(avatar(
                    &name,
                    40.,
                    false,
                    ("avatar", index),
                    avatar_image(this, server, author),
                ))
                .into_any_element()
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .when(!grouped, |column| {
                    column.child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .id(("author-name", index))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::name_color(&name))
                                    .when_some(real_name, |author, real_name| {
                                        author.tooltip(move |_, cx| {
                                            cx.new(|_| Tooltip(real_name.clone().into())).into()
                                        })
                                    })
                                    .child(text!(name.clone()).with_id(("author", index))),
                            )
                            .child(
                                div().text_xs().text_color(color(TEXT_FAINT)).child(
                                    text!(match message.status.label() {
                                        Some(status) => format!("{} · {status}", ui.time),
                                        None => ui.time.clone(),
                                    })
                                    .with_id(("time", index)),
                                ),
                            ),
                    )
                })
                .child({
                    let measured = ui.measured.clone();
                    let clamped = !ui.expanded;
                    div()
                        .on_children_prepainted(move |bounds, window, _| {
                            // The inner column never shrinks, so its height is
                            // the full content height even while clamped.
                            let Some(inner) = bounds.first() else { return };
                            let height = f32::from(inner.size.height);
                            let before = measured.replace(height);
                            if (before > COLLAPSED_HEIGHT + 1.) != (height > COLLAPSED_HEIGHT + 1.)
                            {
                                // A refresh while drawing is dropped; ask for the next frame.
                                window.on_next_frame(|window, _| window.refresh());
                            }
                        })
                        .id(("message-content", index))
                        .flex()
                        .flex_col()
                        .text_color(color(BODY))
                        .when(clamped, |content| {
                            content
                                .relative()
                                .max_h(px(COLLAPSED_HEIGHT))
                                .overflow_hidden()
                        })
                        .child(SelectionArea::new(
                            cx.weak_entity(),
                            &this.text,
                            order,
                            hovered.map(|hovered| (hovered.block, hovered.link)),
                            texts,
                            div()
                                .flex()
                                .flex_col()
                                .flex_shrink_0()
                                .gap_2()
                                .children(blocks),
                        ))
                        .when(clamped && ui.has_more(), |content| {
                            // Fade the cut-off edge into the row background
                            // (which darkens slightly while hovered).
                            let fade = |background: Token| {
                                linear_gradient(
                                    180.,
                                    linear_color_stop(tint(background, 0), 0.),
                                    linear_color_stop(color(background), 1.),
                                )
                            };
                            content.child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .bottom_0()
                                    .h(px(56.))
                                    .bg(fade(MAIN))
                                    .group_hover("message", move |style| {
                                        style.bg(fade(MAIN_HOVERED))
                                    }),
                            )
                        })
                })
                .when_some(message.edited_ms, |view, edited_ms| {
                    let when = format!("Edited {}", crate::state::local_time(edited_ms));
                    view.child(
                        div()
                            .id(("edited", index))
                            .text_xs()
                            .text_color(color(TEXT_FAINT))
                            .tooltip(move |_, cx| cx.new(|_| Tooltip(when.clone().into())).into())
                            .child(text!("(edited)").with_id(("edited-label", index))),
                    )
                })
                .when(ui.has_more(), |view| {
                    view.child(
                        div()
                            .id(("show-more", index))
                            .role(Role::Button)
                            .tab_index(0)
                            .aria_label(if ui.expanded {
                                "Show less"
                            } else {
                                "Show more"
                            })
                            .cursor_pointer()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color(ACCENT_HOVER))
                            .hover(|style| style.underline())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_message(serial);
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                if activation_key(event) {
                                    this.toggle_message(serial);
                                    cx.notify();
                                }
                            }))
                            .child(
                                text!(if ui.expanded {
                                    "Show less"
                                } else {
                                    "Show more"
                                })
                                .with_id(("show-more-label", index)),
                            ),
                    )
                }),
        )
        .into_any_element()
}

/// A deleted message's tombstone: one faint line where it was, so the
/// conversation keeps its shape (and its author's header when it led a group).
fn tombstone_row(
    this: &Desktop,
    message: &nuschl_client::timeline::Message,
    index: usize,
    deletion: nuschl_client::timeline::Deletion,
    author: Option<&User>,
    grouped: bool,
) -> AnyElement {
    let text = if deletion.moderator {
        "Message deleted by a moderator"
    } else {
        "Message deleted"
    };
    let when = format!("Deleted {}", crate::state::local_time(deletion.at_ms));
    let name = names::author_name(&message.author, author, &this.settings).to_string();
    div()
        .id(("message", index))
        .flex()
        .gap_3()
        .px_2()
        .py(px(2.))
        .when(!grouped, |row| row.pt_3())
        .rounded(px(6.))
        .child(div().w(px(40.)).flex_shrink_0())
        .child(
            div()
                .id(("tombstone", index))
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .italic()
                .text_color(color(TEXT_FAINT))
                .tooltip(move |_, cx| cx.new(|_| Tooltip(when.clone().into())).into())
                .child(icon("trash", 14., color(TEXT_FAINT)))
                .child(text!(text).with_id(("tombstone-label", index)))
                .when(!grouped, |line| {
                    line.child(
                        div()
                            .text_xs()
                            .not_italic()
                            .child(text!(format!("· {name}")).with_id(("tombstone-author", index))),
                    )
                }),
        )
        .into_any_element()
}

/// Where a link leads, below the pointer, when its text says otherwise.
fn link_target(hovered: &HoveredLink) -> impl IntoElement {
    deferred(
        anchored()
            .position(hovered.position + point(px(0.), px(20.)))
            .snap_to_window_with_margin(px(8.))
            .child(
                // As a tooltip looks.
                div()
                    .px_2()
                    .py_1()
                    .max_w(px(420.))
                    .rounded(px(6.))
                    .bg(color(APP))
                    .border_1()
                    .border_color(color(BORDER_STRONG))
                    .text_sm()
                    .text_color(color(TEXT))
                    .shadow_md()
                    .child(hovered.url.clone()),
            ),
    )
    .with_priority(1)
}

fn composer(
    this: &Desktop,
    server: &ServerView,
    channel: &crate::state::ChannelView,
    window: &Window,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let channel_name = channel.name.clone();
    // The link the keyboard is on in the timeline (where it really leads),
    // or who is typing here, takes the place of the composer's target: the
    // line is always there, so nothing moves when it changes.
    let link = this
        .link_status()
        .filter(|_| this.text.focus.is_focused(window));
    let linked = link.is_some();
    let typing =
        link.or_else(|| server.typing_line(channel, &this.settings, crate::state::monotonic_ms()));
    let note = this
        .attach_note
        .as_ref()
        .filter(|((id, channel), _)| {
            *id == server.id
                && this
                    .client
                    .navigation
                    .selection()
                    .is_some_and(|selected| selected.channel == *channel)
        })
        .map(|(_, note)| note.clone());
    let attachments = this.attachments();
    div()
        .px_4()
        .pb_3()
        .flex()
        .flex_col()
        .gap(px(6.))
        // Ahead of the text input's own paste, which ignores images.
        .capture_action(
            cx.listener(|this, _: &gpui_kit::base::input::Paste, _, cx| {
                let Some(item) = cx.read_from_clipboard() else {
                    return;
                };
                if item.text().is_some_and(|text| !text.is_empty()) {
                    return;
                }
                let image = item.into_entries().find_map(|entry| match entry {
                    ClipboardEntry::Image(image) => Some(image),
                    _ => None,
                });
                if let Some(image) = image {
                    cx.stop_propagation();
                    this.paste_image(image.id, image.bytes, cx);
                }
            }),
        )
        // Esc ends an edit, or in an empty composer lets go of its attachments.
        .capture_action(
            cx.listener(|this, _: &gpui_kit::base::input::Escape, window, cx| {
                if this.cancel_edit(window, cx)
                    || this.composer.read(cx).value().is_empty() && this.clear_attachments()
                {
                    cx.stop_propagation();
                    cx.notify();
                }
            }),
        )
        // Up in an empty composer edits our last message (Nuschl servers).
        .capture_action(
            cx.listener(|this, _: &gpui_kit::base::input::MoveUp, window, cx| {
                if this.edit_last(window, cx) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }),
        )
        .when(this.editing.is_some(), |composer| {
            composer.child(
                div()
                    .id("editing-status")
                    .role(Role::Status)
                    .aria_label("Editing a message. Enter saves, Escape cancels.")
                    .px_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(color(ACCENT_HOVER))
                    .child(icon("pencil", 14., color(ACCENT_HOVER)))
                    .child(
                        div().min_w_0().child(
                            text!("Editing message · Enter to save · Escape to cancel")
                                .with_id("editing-status-label"),
                        ),
                    ),
            )
        })
        .when_some(this.sending_status(), |composer, status| {
            composer.child(
                div()
                    .id("sending-status")
                    .role(Role::Status)
                    .aria_label(status.clone())
                    .px_1()
                    .text_xs()
                    .text_color(color(TEXT_MUTED))
                    .child(text!(status).with_id("sending-status-label")),
            )
        })
        .when_some(note, |composer, note| {
            composer.child(
                div()
                    .id("attach-note")
                    .role(Role::Status)
                    .aria_label(note.clone())
                    .px_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(color(WARNING))
                    .child(icon("alert", 14., color(WARNING)))
                    .child(
                        div()
                            .min_w_0()
                            .child(text!(note).with_id("attach-note-label")),
                    ),
            )
        })
        .when(!attachments.is_empty(), |composer| {
            composer.child(
                div()
                    .id("attachments")
                    .role(Role::List)
                    .aria_label(match attachments.len() {
                        1 => "1 attachment".to_owned(),
                        count => format!("{count} attachments"),
                    })
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(
                        attachments
                            .iter()
                            .map(|attachment| attachment_chip(attachment, cx)),
                    ),
            )
        })
        .child(
            div()
                .pl_1()
                .pr_2()
                .py_2()
                .rounded(px(10.))
                .bg(color(INPUT))
                .border_1()
                .border_color(color(BORDER_STRONG))
                .flex()
                .items_end()
                .gap_2()
                .child(icon_button(
                    "attach-files",
                    "paperclip",
                    "Attach images or files",
                    TEXT_MUTED,
                    None,
                    |this, window, cx| this.choose_files(window, cx),
                    cx,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .py_1()
                        .child(Textarea::new(&this.composer)),
                )
                .child(icon_button(
                    "send-message",
                    "send",
                    if this.sending() { "Sending…" } else { "Send" },
                    ACCENT_HOVER,
                    None,
                    |this, window, cx| this.send_message(window, cx),
                    cx,
                )),
        )
        .child(
            layout::row()
                .px_1()
                .text_xs()
                .text_color(color(TEXT_FAINT))
                .child(
                    layout::flexible()
                        .flex_grow(2.)
                        .child(crate::typing::LiveStatus {
                            id: "typing-status".into(),
                            label: typing.clone().map(SharedString::from),
                            child: match typing {
                                Some(typing) => div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(color(if linked {
                                        ACCENT_HOVER
                                    } else {
                                        TEXT_MUTED
                                    }))
                                    .child(layout::label("typing-label", typing))
                                    .into_any_element(),
                                None => layout::label(
                                    "composer-target",
                                    if channel_name.starts_with("@ ") {
                                        format!("Message {}", channel_name.trim_start_matches("@ "))
                                    } else {
                                        format!("Message #{channel_name}")
                                    },
                                ),
                            },
                        }),
                )
                .child(layout::label(
                    "composer-hint",
                    if this.settings.enter_sends {
                        "Enter to send · Shift+Enter for a new line"
                    } else {
                        "Enter for a new line · Ctrl/Cmd+Enter to send"
                    },
                ))
                .child(layout::label(
                    "history-default",
                    if this.history_enabled {
                        "Local history on · no end-to-end encryption"
                    } else {
                        "Local history off"
                    },
                )),
        )
        .into_any_element()
}

/// An attachment: an image's thumbnail (once prepared) or a file's icon and
/// size, its name, and a button that removes it.
fn attachment_chip(
    attachment: &crate::attachments::Attachment,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    use crate::attachments::Attachment;
    let id = attachment.id();
    let name = attachment.name().to_owned();
    let (preview, detail, label) = match attachment {
        Attachment::Image {
            prepared: Some((_, image)),
            ..
        } => (
            img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            None,
            format!("Image {name}"),
        ),
        Attachment::Image { .. } => (
            text!("…")
                .with_id("attachment-preparing")
                .into_any_element(),
            None,
            format!("Image {name}, being prepared"),
        ),
        Attachment::File { size, .. } => {
            let size = crate::attachments::human_size(*size);
            (
                icon("file", 20., color(TEXT_MUTED)).into_any_element(),
                Some(size.clone()),
                format!("File {name}, {size}"),
            )
        }
    };
    div()
        .id(("attachment", id as usize))
        .role(Role::ListItem)
        .aria_label(label)
        .flex()
        .items_center()
        .gap_2()
        .p_1()
        .rounded(px(8.))
        .bg(color(ELEVATED))
        .border_1()
        .border_color(color(BORDER_STRONG))
        .child(
            div()
                .size(px(40.))
                .flex_shrink_0()
                .rounded(px(6.))
                .overflow_hidden()
                .bg(color(INPUT))
                .flex()
                .items_center()
                .justify_center()
                .text_color(color(TEXT_FAINT))
                .child(preview),
        )
        .child(
            div()
                .max_w(px(180.))
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(name.clone()).with_id("attachment-name")),
                )
                .when_some(detail, |column, detail| {
                    column.child(
                        div()
                            .text_xs()
                            .text_color(color(TEXT_FAINT))
                            .child(text!(detail).with_id("attachment-size")),
                    )
                }),
        )
        .child(icon_button(
            ("remove-attachment", id as usize),
            "window-close",
            format!("Remove {name}"),
            TEXT_MUTED,
            None,
            move |this, _, cx| {
                this.remove_attachment(id);
                cx.notify();
            },
            cx,
        ))
        .into_any_element()
}

fn certificate_card(
    certificate: &crate::state::CertificatePrompt,
    index: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let heading = if certificate.changed {
        "Server certificate changed"
    } else {
        "Confirm server certificate"
    };
    let body = div().w_full().min_w_0().flex().flex_col().gap_2()
        .child(div().font_weight(FontWeight::BOLD).child(layout::text("certificate-heading", heading)))
        .child(div().text_sm().text_color(color(TEXT_MUTED)).child(layout::text("certificate-explanation", "Verify this SHA-256 fingerprint with the server owner before trusting it. No password has been sent.")))
        .child(div().w_full().min_w_0().p_2().rounded(px(6.)).bg(color(APP)).font_family(theme::mono_font()).text_sm().child(layout::text("certificate-fingerprint", certificate.sha256.clone())))
        .child(div().text_xs().text_color(color(TEXT_FAINT)).child(layout::text("certificate-reason", certificate.reason.clone())))
        .child(layout::actions().child(styled_button("trust-certificate", if certificate.changed { "Trust changed certificate and connect" } else { "Trust certificate and connect" }, if certificate.changed { Tone::Danger } else { Tone::Primary }, move |this, _, cx| { this.trust_certificate(index); cx.notify(); }, cx)))
        .into_any_element();
    layout::notice("certificate-banner", WARNING, "certificate", body, None).into_any_element()
}

/// A downgrade remains an explicit decision; wrapping its full reason must
/// never make the security-reset action unreachable.
fn downgrade_card(reason: &str, index: usize, cx: &mut Context<Desktop>) -> AnyElement {
    let body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(FontWeight::BOLD)
                .child(layout::text("downgrade-heading", "Server security changed")),
        )
        .child(
            div()
                .text_sm()
                .text_color(color(TEXT_MUTED))
                .child(layout::text("downgrade-reason", reason.to_owned())),
        )
        .child(layout::actions().child(styled_button(
            "reset-security-record",
            "Reset this server's security record and connect",
            Tone::Danger,
            move |this, _, cx| {
                this.reset_security_record(index);
                cx.notify();
            },
            cx,
        )))
        .into_any_element();
    layout::notice("downgrade-banner", DANGER, "shield", body, None).into_any_element()
}

fn status_color(status: Status) -> Token {
    match status {
        Status::Connected => SUCCESS,
        Status::Connecting => WARNING,
        Status::Disconnected => TEXT_FAINT,
    }
}

fn activation_key(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "enter" || event.keystroke.key == "space"
}

pub fn security_label(security: SessionSecurity) -> &'static str {
    match security {
        SessionSecurity::Unknown => "Encryption status unknown",
        SessionSecurity::TransportOnly => "No end-to-end encryption; transport encryption only",
        SessionSecurity::MlsPending => "MLS not established",
        SessionSecurity::MlsEstablished => "MLS end-to-end encryption established",
    }
}

fn security_icon(security: SessionSecurity, id: usize, size: f32) -> AnyElement {
    security_icon_labelled(security, security_label(security).into(), id, size)
}

fn security_icon_labelled(
    security: SessionSecurity,
    label: String,
    id: usize,
    size: f32,
) -> AnyElement {
    let (name, tone) = match security {
        SessionSecurity::TransportOnly => ("unlock", DANGER),
        SessionSecurity::MlsEstablished => ("lock", SUCCESS),
        SessionSecurity::MlsPending => ("lock", WARNING),
        SessionSecurity::Unknown => ("unlock", TEXT_FAINT),
    };
    div()
        .id(("security", id))
        .role(Role::Image)
        .aria_label(label)
        .text_color(color(tone))
        .child(icon(name, size, color(tone)))
        .into_any_element()
}

/// A document's blocks, where its text is not selectable (descriptions, comments).
pub fn document_blocks(
    this: &Desktop,
    document: &Document,
    cx: &mut Context<Desktop>,
) -> Vec<AnyElement> {
    blocks(this, document, None, None, &mut Vec::new(), cx)
}

/// A document's blocks with the `selected` part and the `hovered` link
/// highlighted; their laid-out text is collected into `texts` for selecting.
fn blocks(
    this: &Desktop,
    document: &Document,
    selected: Option<(Caret, Caret)>,
    hovered: Option<&HoveredLink>,
    texts: &mut Texts,
    cx: &mut Context<Desktop>,
) -> Vec<AnyElement> {
    crate::math::prepare(this, cx);
    document
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| match block {
            Block::Text {
                kind: Kind::Rule, ..
            } => div()
                .my_1()
                .h(px(1.))
                .bg(color(BORDER_STRONG))
                .into_any_element(),
            Block::Text {
                text,
                highlights,
                links,
                code,
                math,
                kind,
            } => {
                let selected =
                    selected.and_then(|carets| selection::block_range(carets, index, text.len()));
                let underlined = hovered
                    .filter(|hovered| hovered.block == index)
                    .and_then(|hovered| Some(links.get(hovered.link)?.0.clone()));
                let links = (!links.is_empty()).then(|| links.clone());
                let id = Some(("body", index).into());
                let content = BlockText::with_math(
                    id,
                    text.clone(),
                    highlights,
                    code,
                    selected,
                    underlined,
                    links.clone(),
                    math.clone(),
                );
                texts.push(TextBox {
                    block: index,
                    text: text.clone(),
                    layout: content.layout(),
                    links,
                });
                let block = div().id(("text", index));
                match kind {
                    Kind::Heading(level) => block
                        .font_weight(FontWeight::BOLD)
                        .text_size(px(match level {
                            1 => 22.,
                            2 => 19.,
                            _ => 17.,
                        }))
                        .child(content),
                    Kind::Quote => block
                        .pl_3()
                        .border_l_2()
                        .border_color(color(BORDER_STRONG))
                        .text_color(color(TEXT_MUTED))
                        .child(content),
                    Kind::Item { depth, marker } => {
                        // Items of one list sit closer than other blocks.
                        let follows = index > 0
                            && matches!(
                                document.blocks[index - 1],
                                Block::Text {
                                    kind: Kind::Item { .. },
                                    ..
                                }
                            );
                        block
                            .flex()
                            .pl(px(f32::from(depth - 1) * 18.))
                            .when(follows, |item| item.mt(px(-4.)))
                            .child(
                                div()
                                    .min_w(px(18.))
                                    .pr_1()
                                    .flex_shrink_0()
                                    .text_color(color(TEXT_MUTED))
                                    .child(marker.clone()),
                            )
                            .child(div().flex_1().min_w_0().child(content))
                    }
                    Kind::Paragraph | Kind::Rule => block.child(content),
                }
                .into_any_element()
            }
            Block::Image {
                image,
                width,
                height,
                source,
            } => {
                let shown = crate::viewer::ViewerImage {
                    image: image.clone(),
                    width: *width,
                    height: *height,
                    source: source.clone(),
                };
                let menu = shown.clone();
                div()
                    .id(("image", index))
                    .role(Role::Image)
                    .aria_label("Image (click to enlarge)")
                    .tab_index(0)
                    .w(px(*width as f32))
                    .h(px(*height as f32))
                    .rounded(px(8.))
                    .overflow_hidden()
                    .border_1()
                    .border_color(color(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.border_color(color(ACCENT)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_viewer(shown.clone());
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.open_image_menu(menu.clone(), event.position);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(img(image.clone()).size_full())
                    .into_any_element()
            }
            Block::Math { source, tex, .. } => {
                let em = px(this.settings.appearance.text_size);
                crate::math::display(index, source, tex, em, cx)
            }
            Block::Code {
                language,
                text,
                highlights,
            } => {
                let selected =
                    selected.and_then(|carets| selection::block_range(carets, index, text.len()));
                let id = Some(("code-text", index).into());
                let content =
                    BlockText::new(id, text.clone(), highlights, &[], selected, None, None);
                texts.push(TextBox {
                    block: index,
                    text: text.clone(),
                    layout: content.layout(),
                    links: None,
                });
                let copied = this.text.copied.as_ref() == Some(text);
                div()
                    .id(("code", index))
                    .rounded(px(8.))
                    .bg(color(APP))
                    .border_1()
                    .border_color(color(BORDER))
                    .font_family(theme::mono_font())
                    .text_sm()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pl_3()
                            .pr_1()
                            .py(px(2.))
                            .border_b_1()
                            .border_color(color(BORDER))
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_xs()
                            .text_color(color(TEXT_FAINT))
                            .child(text!(language.clone()).with_id(("language", index)))
                            .child(copy_code_button(index, text.clone(), copied, cx)),
                    )
                    .child(div().p_3().child(content))
                    .into_any_element()
            }
        })
        .collect()
}

/// The small button in a code block's header; a check shows for a moment
/// after copying.
fn copy_code_button(
    index: usize,
    code: SharedString,
    copied: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let key = code.clone();
    div()
        .id(("copy-code", index))
        .role(Role::Button)
        .aria_label("Copy code")
        .tab_index(0)
        .cursor_pointer()
        .size(px(22.))
        .flex_shrink_0()
        .rounded(px(5.))
        .flex()
        .items_center()
        .justify_center()
        .hover(|style| style.bg(color(SELECTED)))
        .tooltip(|_, cx| cx.new(|_| Tooltip("Copy code".into())).into())
        .on_click(cx.listener(move |this, _, _, cx| this.copy_code(code.clone(), cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if activation_key(event) {
                this.copy_code(key.clone(), cx);
                cx.stop_propagation();
            }
        }))
        .child(if copied {
            icon("check", 14., color(SUCCESS))
        } else {
            icon("copy", 14., color(TEXT_FAINT))
        })
        .into_any_element()
}
