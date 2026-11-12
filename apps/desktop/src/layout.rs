// SPDX-License-Identifier: EUPL-1.2
//! Finite layout contracts for chrome, notices and status cards.
//!
//! A row gives remaining width to `flexible`, never the text's intrinsic
//! max-content width. Icons and short actions live in `fixed`. Wrapping and
//! ellipsis remain GPUI's job (including its shaping cache and accessibility
//! text); callers must not measure or edit strings to make them fit.
use crate::{assets::icon, dialogs::Tooltip, theme::*};
use gpui_kit::{prelude::*, *};
use std::{cell::Cell, rc::Rc};

/// A control's retained focus state; no text/layout cache or global registry.
struct FocusState {
    handle: FocusHandle,
    reveal: Rc<Cell<bool>>,
    _focus: Subscription,
}

#[derive(IntoElement)]
struct Focusable {
    id: ElementId,
    child: Stateful<Div>,
    outset: Pixels,
    handle: Option<FocusHandle>,
}

impl RenderOnce for Focusable {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let key = SharedString::from(format!("{}:focus-reveal", self.id));
        let trace = key.clone();
        let state = window.use_keyed_state(key, cx, |window, cx| {
            let handle = self.handle.unwrap_or_else(|| cx.focus_handle());
            let subscription = cx.on_focus(
                &handle,
                window,
                move |state: &mut FocusState, window, cx| {
                    tracing::debug!(control = %trace, "layout control received focus");
                    state.reveal.set(true);
                    cx.notify();
                    // Focus listeners run during DrawPhase::Focus: refresh is
                    // suppressed there, and request_animation_frame requires
                    // a prepaint/paint view. Schedule one ordinary next frame.
                    window.on_next_frame(|window, _| window.refresh());
                },
            );
            FocusState {
                handle,
                reveal: Rc::default(),
                _focus: subscription,
            }
        });
        let state = state.read(cx);
        // Div does not apply its tab_index metadata to an explicitly tracked
        // handle; retain keyboard reachability when supplying our own handle.
        let handle = state.handle.clone().tab_stop(true).tab_index(0);
        let observed = state.handle.clone();
        let reveal = state.reveal.clone();
        let trace = self.id.clone();
        let outset = self.outset;
        self.child.track_focus(&handle).relative().child(
            canvas(
                move |bounds, window, _| {
                    if reveal.replace(false) && observed.is_focused(window) {
                        tracing::debug!(control = %trace, ?bounds, "layout control requested reveal");
                        window.request_autoscroll(bounds.dilate(outset));
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
    }
}

/// Newly focused controls ask their nearest supporting scroll region to reveal
/// them. Keeping focus in place never fights intentional wheel/keyboard reading.
pub fn focusable(id: impl Into<ElementId>, child: Stateful<Div>) -> AnyElement {
    focusable_with_outset(id, child, px(0.))
}

/// Reveal a control's paint decorations as well as its layout box.
pub fn focusable_with_outset(
    id: impl Into<ElementId>,
    child: Stateful<Div>,
    outset: Pixels,
) -> AnyElement {
    Focusable {
        id: id.into(),
        child,
        outset,
        handle: None,
    }
    .into_any_element()
}

/// Reveal a virtualized list using its retained keyboard-navigation handle.
/// The caller owns the handle for this control's lifetime; its element key must
/// change when that lifetime ends, just like other retained focus state.
pub fn focusable_with_handle(
    id: impl Into<ElementId>,
    handle: FocusHandle,
    child: Stateful<Div>,
) -> AnyElement {
    Focusable {
        id: id.into(),
        child,
        outset: px(0.),
        handle: Some(handle),
    }
    .into_any_element()
}

/// Reserve paint space inside scroll content for rings/badges extending beyond
/// the control's layout box. Every slot gets the same gutter, including both
/// endpoints; two logical pixels protect antialiased edges at fractional zoom.
pub fn decorated_slot(outset: Pixels) -> Div {
    div().relative().flex_shrink_0().p(outset + px(2.))
}

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
        .flex()
        .flex_col()
        .gap_2()
        .child(
            row()
                .items_center()
                .child(fixed().child(icon(icon_name, 16., color(tone))))
                .child(flexible())
                .when_some(trailing, |row, action| row.child(fixed().child(action))),
        )
        .child(div().w_full().min_w_0().child(body))
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
    scrollable(id, label, handle).max_h(height)
}

/// The same keyboard/focus contract for a caller-owned scroll handle (dialogs
/// already retain one for smooth wheel scrolling).
pub fn scrollable(id: &'static str, label: &'static str, handle: ScrollHandle) -> Stateful<Div> {
    let keys = handle.clone();
    let reveal = handle.clone();
    let outer_request = Rc::new(Cell::new(None));
    let begin = outer_request.clone();
    div()
        .on_children_prepainted(move |_, window, _| {
            let prior_request = outer_request.take();
            if let Some(target) = window.take_autoscroll() {
                let clip = reveal.bounds();
                let dy = if target.top() < clip.top() {
                    target.top() - clip.top()
                } else if target.bottom() > clip.bottom() {
                    target.bottom() - clip.bottom()
                } else {
                    px(0.)
                };
                let mut offset = reveal.offset();
                let prior = offset;
                offset.y = (offset.y - dy).clamp(-reveal.max_offset().y, px(0.));
                tracing::debug!(region = id, ?target, ?clip, before = ?prior, after = ?offset, max = ?reveal.max_offset(), "layout region received reveal");
                if offset != prior {
                    reveal.set_offset(offset);
                    // refresh is suppressed during prepaint. One extra frame
                    // paints the changed offset, without an idle redraw loop.
                    window.request_animation_frame();
                }
                // A nested region may itself be outside its parent's clip.
                // Reveal the visible part of the focused child through every
                // ancestor, after accounting for our actual clamped movement.
                let mut visible = target;
                visible.origin.y += offset.y - prior.y;
                let visible = visible.intersect(&clip);
                window.request_autoscroll(visible);
            } else if let Some(prior) = prior_request {
                window.request_autoscroll(prior);
            }
        })
        .id(id)
        .role(Role::Group)
        .aria_label(label)
        .tab_index(0)
        .w_full()
        .min_w_0()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&handle)
        // Establish an autoscroll scope before prepainting descendants; a
        // request from a preceding sibling must not scroll this region.
        .child(
            canvas(
                move |_, window, _| {
                    begin.set(window.take_autoscroll());
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size(px(0.)),
        )
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
