// SPDX-License-Identifier: EUPL-1.2
//! Finite layout contracts for chrome, notices and status cards.
//!
//! A row gives remaining width to `flexible`, never the text's intrinsic
//! max-content width. Icons and short actions live in `fixed`. Wrapping and
//! ellipsis remain GPUI's job (including its shaping cache and accessibility
//! text); callers must not measure or edit strings to make them fit.
use crate::{assets::icon, dialogs::Tooltip, theme::*};
use gpui_kit::{prelude::*, *};

/// The shrinking lane of a row. `min_w_0` alone does not override flex-basis
/// auto: use zero basis so a long label cannot reserve its intrinsic width.
pub fn flexible() -> Div {
    div().flex_1().flex_basis(px(0.)).min_w_0().min_h_0()
}

/// A finite horizontal row; each child must choose a flexible or fixed lane.
pub fn row() -> Div {
    div().w_full().min_w_0().flex().items_start().gap_2()
}

/// A stable icon or compact action lane, independent of label length.
pub fn fixed() -> Div {
    div().flex_shrink_0().min_w_0()
}

/// A compact label, with the entire original text still announced and shown
/// on hover. The shaping system, not a byte/character limit, chooses ellipsis.
pub fn label(id: impl Into<ElementId>, text: impl Into<SharedString>) -> AnyElement {
    let id = id.into();
    let text = text.into();
    let full = text.clone();
    flexible()
        .id(id.clone())
        .truncate()
        .tooltip(move |_, cx| cx.new(|_| Tooltip(full.clone())).into())
        .child(Text::new(id, text))
        .into_any_element()
}

/// Complete wrapped text. A finite parent lane is essential for native
/// emergency wrapping of paths, hashes, Unicode and other unbroken tokens.
pub fn text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .whitespace_normal()
        .child(Text::new(id.into(), text.into()))
        .into_any_element()
}

/// Actions flow onto additional rows. Individual buttons also constrain their
/// label (dialogs::named_button), so even one long action remains finite.
pub fn actions() -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_start()
        .gap_2()
}

/// Common notice shell. Only compact controls belong in `trailing`; longer
/// action labels go below the text through `actions`, not beside its width.
pub fn notice(
    id: &'static str,
    tone: Token,
    icon_name: &'static str,
    body: AnyElement,
    trailing: Option<AnyElement>,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Status)
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .p_3()
        .rounded(px(8.))
        .bg(tint(tone, 0x22))
        .border_1()
        .border_color(tint(tone, 0x60))
        .text_sm()
        .child(
            row()
                .child(fixed().pt(px(2.)).child(icon(icon_name, 16., color(tone))))
                .child(flexible().child(body))
                .when_some(trailing, |row, action| row.child(fixed().child(action))),
        )
}

/// A bounded scroll region with real keyboard navigation as well as wheel
/// scrolling. Its handle is retained by GPUI under the element key, not rebuilt
/// per frame or stored in a growing global cache.
pub fn scroll_region(
    id: &'static str,
    label: &'static str,
    height: Pixels,
    window: &mut Window,
    cx: &mut App,
) -> Stateful<Div> {
    let state = window.use_keyed_state((id, 0_usize), cx, |_, _| ScrollHandle::new());
    let handle = state.read(cx).clone();
    let keys = handle.clone();
    div()
        .id(id)
        .role(Role::Group)
        .aria_label(label)
        .tab_index(0)
        .w_full()
        .min_w_0()
        .min_h_0()
        .max_h(height)
        .overflow_y_scroll()
        .track_scroll(&handle)
        .on_key_down(move |event, window, cx| {
            if event.keystroke.modifiers.modified() {
                return;
            }
            let mut offset = keys.offset();
            let page = keys.bounds().size.height * 0.85;
            offset.y = match event.keystroke.key.as_str() {
                "down" => offset.y - window.rem_size() * 2.,
                "up" => offset.y + window.rem_size() * 2.,
                "pagedown" => offset.y - page,
                "pageup" => offset.y + page,
                "home" => px(0.),
                "end" => -keys.max_offset().y,
                _ => return,
            }
            .clamp(-keys.max_offset().y, px(0.));
            keys.set_offset(offset);
            window.refresh();
            cx.stop_propagation();
        })
}

/// One height budget for the entire notice stack, in zoom-aware logical pixels.
pub fn notices(window: &mut Window, cx: &mut App, notices: Vec<AnyElement>) -> AnyElement {
    scroll_region(
        "notification-stack",
        "Notifications",
        window.viewport_size().height * 0.34,
        window,
        cx,
    )
    .flex_shrink_0()
    .when(!notices.is_empty(), |stack| stack.px_4().py_3())
    .child(
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .children(notices),
    )
    .into_any_element()
}
