// SPDX-License-Identifier: EUPL-1.2
use crate::{
    assets::{app_icon, icon},
    state::Desktop,
    theme::*,
};
use gpui_kit::{
    base::input::{Input, InputState},
    prelude::*,
    *,
};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Secondary,
    Danger,
    Ghost,
}

fn activation(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "enter" || event.keystroke.key == "space"
}

/// A labelled, keyboard-activatable button.
pub fn styled_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: Tone,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let label = label.into();
    named_button(id, label.clone(), label, tone, action, cx)
}

/// A button whose name for assistive technology says more than its label
/// ("Record" in a row is announced as "Record Toggle mute shortcut").
pub fn named_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    name: impl Into<SharedString>,
    tone: Tone,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let id = id.into();
    let reveal_id = id.clone();
    let action = Rc::new(action);
    let click = action.clone();
    let label = label.into();
    let (background, hover, foreground) = match tone {
        Tone::Primary => (ACCENT, ACCENT_HOVER, WHITE),
        Tone::Secondary => (ELEVATED, SELECTED, TEXT),
        Tone::Danger => (DANGER, DANGER_HOVER, WHITE),
        Tone::Ghost => (MAIN, HOVER, TEXT_MUTED),
    };
    let control = div()
        .id(id)
        .role(Role::Button)
        .aria_label(name.into())
        .tab_index(0)
        .cursor_pointer()
        .flex_shrink_0()
        .max_w_full()
        .min_w_0()
        .px_3()
        .py(px(6.))
        .rounded(px(6.))
        .bg(color(background))
        .hover(move |style| style.bg(color(hover)))
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color(foreground))
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if activation(event) {
                action(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .min_w_0()
                .whitespace_normal()
                .child(text!(label).with_id("button-label")),
        );
    crate::layout::focusable(reveal_id, control)
}

/// A square icon button; `label` is announced to assistive technology and shown as tooltip.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    label: impl Into<SharedString>,
    foreground: Token,
    active_background: Option<Token>,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let id = id.into();
    let reveal_id = id.clone();
    let action = Rc::new(action);
    let click = action.clone();
    let label = label.into();
    let tooltip = label.clone();
    let control = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .cursor_pointer()
        .size(px(32.))
        .flex_shrink_0()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(color(foreground))
        .when_some(active_background, |button, background| {
            button.bg(tint(background, 0x2e))
        })
        .hover(|style| style.bg(color(SELECTED)))
        .tooltip(move |_, cx| cx.new(|_| Tooltip(tooltip.clone())).into())
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if activation(event) {
                action(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(icon(name, 18., color(foreground)));
    crate::layout::focusable(reveal_id, control)
}

/// A voice toggle (mute, deafen): `on` from 0 to 1 eases the background to
/// red and draws a slash across the icon, so a toggle animates instead of
/// swapping icons (see `motion::Ease`).
pub fn toggle_button(
    id: impl Into<ElementId>,
    name: &'static str,
    label: impl Into<SharedString>,
    on: f32,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let id = id.into();
    let action = Rc::new(action);
    let click = action.clone();
    let label = label.into();
    let tooltip = label.clone();
    let foreground = mix(TEXT_MUTED, DANGER, on);
    let control = div()
        .id(id.clone())
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .cursor_pointer()
        .size(px(32.))
        .flex_shrink_0()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .when(on > 0., |button| {
            button.bg(tint(DANGER, (f32::from(0x2e_u8) * on) as u8))
        })
        // Hovering deepens the red rather than replacing it.
        .hover(move |style| {
            if on > 0.5 {
                style.bg(tint(DANGER, 0x48))
            } else {
                style.bg(color(SELECTED))
            }
        })
        .tooltip(move |_, cx| cx.new(|_| Tooltip(tooltip.clone())).into())
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if activation(event) {
                action(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .relative()
                .size(px(18.))
                .child(icon(name, 18., foreground))
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            if on <= 0. {
                                return;
                            }
                            let inset = bounds.size.width * 0.1;
                            let a = point(bounds.left() + inset, bounds.top() + inset);
                            let reach = (bounds.size.width - inset * 2.) * on;
                            let mut slash = PathBuilder::stroke(px(2.));
                            slash.move_to(a);
                            slash.line_to(point(a.x + reach, a.y + reach));
                            if let Ok(path) = slash.build() {
                                window.paint_path(path, foreground);
                            }
                        },
                    )
                    .absolute()
                    .inset_0(),
                ),
        );
    crate::layout::focusable(id, control)
}

/// The colour `t` of the way from `a` to `b`.
pub fn mix(a: Token, b: Token, t: f32) -> Rgba {
    let (a, b) = (color(a), color(b));
    let t = t.clamp(0., 1.);
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

pub struct Tooltip(pub SharedString);
impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .max_w(px(420.).min(window.viewport_size().width * 0.90))
            .max_h(window.viewport_size().height * 0.50)
            .min_w_0()
            .overflow_hidden()
            .whitespace_normal()
            .rounded(px(6.))
            .bg(color(APP))
            .border_1()
            .border_color(color(BORDER_STRONG))
            .text_sm()
            .text_color(color(TEXT))
            .shadow_md()
            .child(text!(self.0.clone()).with_id("tooltip-full-text"))
    }
}

fn field(id: &'static str, label: &'static str, state: &Entity<InputState>) -> AnyElement {
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
                .child(text!(label.to_uppercase()).with_id("field-label")),
        )
        .child(
            div()
                .px_3()
                .py(px(9.))
                .rounded(px(6.))
                .bg(color(APP))
                .border_1()
                .border_color(color(BORDER))
                .child(Input::new(state)),
        )
        .into_any_element()
}

pub fn modal(
    this: &Desktop,
    id: &'static str,
    label: &'static str,
    width: f32,
    content: AnyElement,
    cx: &mut Context<Desktop>,
    close: impl Fn(&mut Desktop) + 'static,
) -> AnyElement {
    this.modal_shown.set(Some(id));
    if this.modal_seen.get() != Some(id) {
        tracing::debug!(dialog = id, previous = ?this.modal_seen.get(), "reset dialog scroll");
        this.modal_scroll.set_offset(point(px(0.), px(0.)));
        this.nested.forget_all();
    }
    div()
        .id(SharedString::from(format!("{id}-backdrop")))
        .absolute()
        .inset_0()
        .bg(color(SCRIM))
        .map(|backdrop| crate::chrome::round(backdrop, this.window_corners))
        .flex()
        .items_center()
        .justify_center()
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                close(this);
                cx.notify();
            }),
        )
        .child(
            crate::layout::scrollable(id, label, this.modal_scroll.clone())
                .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
                    if event.button == MouseButton::Left {
                        this.cancel_modal_scroll();
                    }
                }))
                .track_focus(&this.modal_focus)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .role(Role::Dialog)
                .aria_label(label)
                .w(px(width))
                .max_h(relative(0.92))
                .overflow_y_scroll()
                .track_scroll(&this.modal_scroll)
                .rounded(px(12.))
                .bg(color(MAIN))
                .border_1()
                .border_color(color(BORDER_STRONG))
                .shadow_lg()
                .child(
                    // Wheel steps ease like the timeline's (the content sees
                    // them before the scroll container).
                    div()
                        .id(SharedString::from(format!("{id}-content")))
                        .on_scroll(cx.listener(|this, event, window, cx| {
                            this.scroll_modal(None, event, window, cx)
                        }))
                        .child(content),
                )
                .child(crate::layout::scrollbar(
                    SharedString::from(format!("{id}-scrollbar")),
                    &this.modal_scroll,
                    MAIN,
                )),
        )
        .into_any_element()
}

/// A list that scrolls inside a dialog, at most `max_height` tall, around
/// `content`. The content takes each wheel step before any scroll container
/// does, and `Desktop::scroll_modal` moves either the list or the dialog. So
/// that the content covers all of the list, the list itself has no padding or
/// border; a frame goes around it.
pub fn nested_list(
    this: &Desktop,
    id: &'static str,
    max_height: Pixels,
    content: Div,
    cx: &mut Context<Desktop>,
) -> Stateful<Div> {
    crate::layout::scrollable(id, id, this.nested.handle(id))
        .max_h(max_height)
        .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
            if event.button == MouseButton::Left {
                this.cancel_modal_scroll();
            }
        }))
        .child(
            content.on_scroll(cx.listener(move |this, event, window, cx| {
                this.scroll_modal(Some(id), event, window, cx)
            })),
        )
        .child(crate::layout::scrollbar(
            SharedString::from(format!("{id}-scrollbar")),
            &this.nested.handle(id),
            MAIN,
        ))
}

/// A virtualized list inside a modal participates in the same latched wheel
/// transaction as ordinary nested lists, using its original retained handle.
pub fn nested_virtual_list(
    id: &'static str,
    version: u64,
    scroll: &UniformListScrollHandle,
    list: UniformList,
    cx: &mut Context<Desktop>,
) -> Stateful<Div> {
    let handle = scroll.0.borrow().base_handle.clone();
    crate::layout::scroll_frame(
        id,
        scroll,
        list.pr(px(crate::layout::SCROLLBAR_GUTTER)),
        MAIN,
    )
    .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
        if event.button == MouseButton::Left {
            this.cancel_modal_scroll();
        }
    }))
    .child(crate::layout::capture_scroll(cx.listener(
        move |this, event, window, cx| {
            this.nested.register(id, version, handle.clone());
            this.scroll_modal(Some(id), event, window, cx)
        },
    )))
}

pub fn server_form(this: &Desktop, cx: &mut Context<Desktop>) -> AnyElement {
    let Some(form) = &this.form else {
        return div().into_any_element();
    };
    let editing = form.editing.is_some();
    // Saving an offline server connects it; an online one reconnects only
    // when its address, name or password changed.
    let online = form
        .editing
        .as_ref()
        .and_then(|id| this.client.servers.iter().find(|server| &server.id == id))
        .is_some_and(|server| server.status != crate::state::Status::Disconnected);
    let save = match (editing, online) {
        (false, _) => "Add and connect",
        (true, false) => "Save and connect",
        (true, true) => "Save",
    };
    let content = div()
        .flex()
        .flex_col()
        .child(
            div()
                .p_6()
                .pb_4()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .child(text!(if editing { "Server settings" } else { "Add a server" }).with_id("dialog-heading")),
                )
                .child(
                    div().text_sm().text_color(color(TEXT_MUTED)).child(
                        text!("Any Mumble server works. Nuschl servers add persistent chat later.")
                            .with_id("dialog-subtitle"),
                    ),
                ),
        )
        .child(
            div()
                .px_6()
                .flex()
                .flex_col()
                .gap_4()
                // The address comes first (and has focus); the name follows it.
                // A managed server's are disabled and dimmed.
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .when(form.managed, |fields| fields.opacity(0.5))
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .child(div().flex_1().child(field("server-host-field", "Hostname or IP address", &form.host)))
                                .child(div().w(px(110.)).child(field("server-port-field", "Port", &form.port))),
                        )
                        .child(field("server-name-field", "Server name", &form.name))
                        .child(field("server-username-field", "Username", &form.username)),
                )
                .child(field("server-password-field", "Password (optional)", &form.password))
                .child(div().when(form.managed, |row| row.opacity(0.5)).child(crate::settings::toggle(
                    "server-auto-connect",
                    "Connect when Nuschl starts",
                    "You join muted and deafened until you unmute.".into(),
                    form.auto_connect,
                    std::rc::Rc::new(|this, _, _| {
                        if let Some(form) = this.form.as_mut().filter(|form| !form.managed) {
                            form.auto_connect = !form.auto_connect;
                        }
                    }),
                    cx,
                )))
                .when(form.managed, |view| {
                    view.child(
                        div()
                            .flex()
                            .gap_2()
                            .items_start()
                            .text_xs()
                            .text_color(color(TEXT_MUTED))
                            .child(icon("lock", 14., color(TEXT_MUTED)))
                            .child(div().flex_1().min_w_0().child(
                                text!("Set by your system configuration: only the password can be entered here.")
                                    .with_id("managed-server-note"),
                            )),
                    )
                })
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .items_start()
                        .text_xs()
                        .text_color(color(TEXT_FAINT))
                        .child(icon("shield", 14., color(TEXT_FAINT)))
                        .child(div().flex_1().min_w_0().child(
                            text!("Passwords are not saved. Self-signed certificates require confirmation before login.")
                                .with_id("credential-note"),
                        )),
                )
                .when_some(form.error.clone(), |view, error| {
                    view.child(
                        div()
                            .text_sm()
                            .text_color(color(DANGER_HOVER))
                            .child(text!(error).with_id("server-form-error")),
                    )
                }),
        )
        .child(
            div()
                .mt_6()
                .px_6()
                .py_4()
                .bg(color(SIDEBAR))
                .rounded_b(px(12.))
                .w_full()
                .min_w_0()
                .flex()
                .flex_wrap()
                .gap_3()
                .justify_end()
                .when(editing && !form.managed, |view| {
                    view.child(styled_button("remove-server", "Remove saved server", Tone::Ghost, |this, window, cx| this.remove_server(window, cx), cx))
                        .child(div().flex_1())
                })
                .child(styled_button("cancel-server", "Cancel", Tone::Ghost, |this, _, cx| {
                    this.form = None;
                    cx.notify();
                }, cx))
                .child(styled_button("save-connect", save, Tone::Primary, |this, window, cx| this.submit_form(window, cx), cx)),
        )
        .into_any_element();
    modal(
        this,
        "server-dialog",
        "Add or connect to a Mumble server",
        520.,
        content,
        cx,
        |this| this.form = None,
    )
}

/// Main area shown before any server has been saved, with a problem found
/// at start (such as a configuration file that could not be read).
pub fn empty(this: &Desktop, cx: &mut Context<Desktop>) -> AnyElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .gap_4()
        .bg(color(MAIN))
        .rounded_br(this.window_corners.bottom_right)
        .child(app_icon(px(72.)))
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .child(text!("Welcome to Nuschl").with_id("welcome-heading")),
        )
        .child(
            div()
                .text_color(color(TEXT_MUTED))
                .child(text!("Add a Mumble server to start talking.").with_id("welcome-text")),
        )
        .when_some(this.error.clone(), |view, error| {
            view.child(
                div()
                    .id("welcome-error")
                    .role(Role::Status)
                    .w_full()
                    .max_w(px(560.))
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(tint(DANGER, 0x26))
                    .border_1()
                    .border_color(tint(DANGER, 0x60))
                    .flex()
                    .gap_2()
                    .text_sm()
                    .text_color(color(DANGER_TEXT))
                    .child(icon("alert", 16., color(DANGER_HOVER)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(text!(error).with_id("welcome-error-text")),
                    ),
            )
        })
        .child(styled_button(
            "welcome-add-server",
            "Add a server",
            Tone::Primary,
            |this, window, cx| this.open_form(None, window, cx),
            cx,
        ))
        .into_any_element()
}
