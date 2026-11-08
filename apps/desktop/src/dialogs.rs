// SPDX-License-Identifier: EUPL-1.2
use crate::state::Desktop;
use gpui_kit::{
    base::input::{Input, InputState},
    prelude::*,
    *,
};
use std::rc::Rc;

pub fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let action = Rc::new(action);
    let click = action.clone();
    div()
        .id(id)
        .role(Role::Button)
        .tab_index(0)
        .cursor_pointer()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(0x35405a))
        .hover(|style| style.bg(rgb(0x46557a)))
        .text_sm()
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                action(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(text!(label.into()).with_id("button-label"))
        .into_any_element()
}
fn field(id: &'static str, label: &'static str, state: &Entity<InputState>) -> AnyElement {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .text_color(rgb(0xb5c0d6))
                .child(text!(label).with_id("field-label")),
        )
        .child(
            div()
                .p_3()
                .rounded_md()
                .bg(rgb(0x131a25))
                .border_1()
                .border_color(rgb(0x3a455c))
                .child(Input::new(state)),
        )
        .into_any_element()
}
pub fn server_form(this: &Desktop, cx: &mut Context<Desktop>) -> AnyElement {
    let Some(form) = &this.form else {
        return div().into_any_element();
    };
    div().id("server-dialog-backdrop").absolute().inset_0().bg(rgba(0x000000bb)).flex().items_center().justify_center().occlude()
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| { if event.keystroke.key == "escape" { this.form = None; cx.stop_propagation(); cx.notify(); } }))
        .child(div().id("server-dialog").role(Role::Dialog).aria_label("Add or connect to a Mumble server")
            .w(px(500.)).max_h(relative(0.95)).overflow_y_scroll().p_6().rounded_xl().bg(rgb(0x202a3a))
            .border_1().border_color(rgb(0x45516b)).flex().flex_col().gap_4()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child(text!(if form.editing.is_some() { "Connect to server" } else { "Add a server" }).with_id("dialog-heading")))
            .child(field("server-name-field", "Server name", &form.name))
            .child(field("server-host-field", "Hostname or IP address", &form.host))
            .child(field("server-port-field", "Port", &form.port))
            .child(field("server-username-field", "Username", &form.username))
            .child(field("server-password-field", "Password (optional)", &form.password))
            .child(div().text_xs().text_color(rgb(0x9caac3)).child(text!("Passwords are not saved. Self-signed certificates require confirmation before login.").with_id("credential-note")))
            .when_some(form.error.clone(), |view, error| view.child(div().text_sm().text_color(rgb(0xff9caa)).child(text!(error).with_id("server-form-error"))))
            .child(div().flex().gap_3().justify_end()
                .when(form.editing.is_some(), |view| view.child(button("remove-server", "Remove saved server", |this, window, cx| this.remove_server(window, cx), cx)))
                .child(button("cancel-server", "Cancel", |this, _, cx| { this.form = None; cx.notify(); }, cx))
                .child(button("save-connect", "Save and connect", |this, window, cx| this.submit_form(window, cx), cx))))
        .into_any_element()
}
pub fn empty(this: &Desktop, cx: &mut Context<Desktop>) -> AnyElement {
    div()
        .size_full()
        .relative()
        .flex()
        .bg(rgb(0x202632))
        .text_color(rgb(0xe9edf5))
        .font_family("sans-serif")
        .child(
            div()
                .w(px(72.))
                .h_full()
                .py_5()
                .flex_shrink_0()
                .bg(rgb(0x11151d))
                .flex()
                .flex_col()
                .items_center()
                .gap_5()
                .child(text!("N").with_id("brand"))
                .child(button(
                    "add-server",
                    "+",
                    |this, window, cx| this.open_form(None, window, cx),
                    cx,
                )),
        )
        .child(
            div()
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .justify_center()
                .items_center()
                .gap_5()
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::BOLD)
                        .child(text!("Welcome to Nuschel").with_id("welcome-heading")),
                )
                .child(
                    text!("Add a Mumble server to start a conversation.").with_id("welcome-text"),
                )
                .child(button(
                    "welcome-add-server",
                    "Add a server",
                    |this, window, cx| this.open_form(None, window, cx),
                    cx,
                )),
        )
        .when(this.form.is_some(), |view| {
            view.child(server_form(this, cx))
        })
        .into_any_element()
}
