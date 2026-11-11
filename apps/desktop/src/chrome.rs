// SPDX-License-Identifier: EUPL-1.2
//! Window chrome. GNOME and other compositors without server-side decorations
//! expect the application to draw its title bar, controls and resize edges.
use crate::{
    assets::{app_icon, icon},
    state::Desktop,
    theme::*,
};
use gpui_kit::{prelude::*, *};

/// Invisible border around client-decorated windows that carries the shadow
/// and resize hit areas.
const INSET: Pixels = px(10.);
const RADIUS: Pixels = px(10.);
pub const TITLE_HEIGHT: Pixels = px(38.);

fn client_decorated(window: &Window) -> Option<Tiling> {
    match window.window_decorations() {
        Decorations::Client { tiling } => Some(tiling),
        Decorations::Server => None,
    }
}

/// Wraps the application in client-side decorations when the platform asks for them.
pub fn frame(window: &mut Window, content: AnyElement) -> AnyElement {
    let Some(tiling) = client_decorated(window) else {
        return div().size_full().child(content).into_any_element();
    };
    let edges = if window.is_maximized() || window.is_fullscreen() {
        Tiling::tiled()
    } else {
        tiling
    };
    window.set_client_inset(if edges.is_tiled() { px(0.) } else { INSET });
    div()
        .id("window-frame")
        .size_full()
        .bg(transparent_black())
        .when(!edges.top, |frame| frame.pt(INSET))
        .when(!edges.bottom, |frame| frame.pb(INSET))
        .when(!edges.left, |frame| frame.pl(INSET))
        .when(!edges.right, |frame| frame.pr(INSET))
        .child(
            canvas(
                |_, window, _| {
                    window.insert_hitbox(
                        Bounds::new(
                            point(px(0.), px(0.)),
                            window.window_bounds().get_bounds().size,
                        ),
                        HitboxBehavior::Normal,
                    )
                },
                move |_, hitbox, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(window.mouse_position(), size, edges) {
                        window.set_cursor_style(cursor(edge), &hitbox);
                    }
                },
            )
            .size_full()
            .absolute()
            .top_0()
            .left_0(),
        )
        .on_mouse_move(|_, window, _| window.refresh())
        .on_mouse_down(MouseButton::Left, move |event, window, _| {
            let size = window.window_bounds().get_bounds().size;
            if let Some(edge) = resize_edge(event.position, size, edges) {
                window.start_window_resize(edge);
            }
        })
        .child(
            div()
                .id("window-surface")
                .size_full()
                .overflow_hidden()
                .cursor(CursorStyle::Arrow)
                .bg(color(MAIN))
                .when(!edges.is_tiled(), |surface| {
                    surface
                        .rounded(RADIUS)
                        .border_1()
                        .border_color(color(BORDER_STRONG))
                        .shadow(vec![
                            BoxShadow::new(px(0.), px(2.), hsla(0., 0., 0., 0.45))
                                .blur_radius(INSET * 0.8),
                        ])
                })
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(content),
        )
        .into_any_element()
}

fn cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

fn resize_edge(position: Point<Pixels>, size: Size<Pixels>, tiling: Tiling) -> Option<ResizeEdge> {
    // Corners get a larger target than the edges, as in GTK/libadwaita.
    let corner = INSET * 2.;
    let top = !tiling.top && position.y < INSET;
    let bottom = !tiling.bottom && position.y > size.height - INSET;
    let left = !tiling.left && position.x < INSET;
    let right = !tiling.right && position.x > size.width - INSET;
    let near_top = position.y < corner;
    let near_bottom = position.y > size.height - corner;
    let near_left = position.x < corner;
    let near_right = position.x > size.width - corner;
    Some(if (top && near_left) || (left && near_top) {
        ResizeEdge::TopLeft
    } else if (top && near_right) || (right && near_top) {
        ResizeEdge::TopRight
    } else if (bottom && near_left) || (left && near_bottom) {
        ResizeEdge::BottomLeft
    } else if (bottom && near_right) || (right && near_bottom) {
        ResizeEdge::BottomRight
    } else if top {
        ResizeEdge::Top
    } else if bottom {
        ResizeEdge::Bottom
    } else if left {
        ResizeEdge::Left
    } else if right {
        ResizeEdge::Right
    } else {
        return None;
    })
}

/// The title bar doubles as a drag handle; double-click toggles maximization.
pub fn title_bar(title: SharedString, window: &Window, cx: &mut Context<Desktop>) -> AnyElement {
    let client = client_decorated(window).is_some();
    let draws_controls = client || cfg!(target_os = "windows");
    let controls = window.window_controls();
    let maximized = window.is_maximized();
    div()
        .id("title-bar")
        .h(TITLE_HEIGHT)
        .flex_shrink_0()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .bg(color(APP))
        .border_b_1()
        .border_color(color(BORDER))
        .window_control_area(WindowControlArea::Drag)
        .when(cfg!(target_os = "macos"), |bar| bar.pl(px(78.)))
        .when(!cfg!(target_os = "macos"), |bar| bar.pl_3())
        .when(client, |bar| {
            bar.on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.title_drag = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.title_drag = false),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.title_drag {
                    this.title_drag = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.is_right_click() {
                    window.show_window_menu(event.position());
                } else if event.click_count() == 2 {
                    window.zoom_window();
                }
            })
        })
        .child(app_icon(px(20.)))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(TEXT_MUTED))
                .child(text!("Nuschel").with_id("app-name")),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_sm()
                .text_color(color(TEXT_FAINT))
                .child(text!(title).with_id("window-title")),
        )
        .when(draws_controls, |bar| {
            bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pr_2()
                    .when(controls.minimize, |row| {
                        row.child(control(
                            "window-minimize",
                            "Minimize",
                            WindowControlArea::Min,
                            |window| window.minimize_window(),
                        ))
                    })
                    .when(controls.maximize, |row| {
                        row.child(control(
                            if maximized {
                                "window-restore"
                            } else {
                                "window-maximize"
                            },
                            if maximized { "Restore" } else { "Maximize" },
                            WindowControlArea::Max,
                            |window| window.zoom_window(),
                        ))
                    })
                    .child(control(
                        "window-close",
                        "Close",
                        WindowControlArea::Close,
                        |window| window.remove_window(),
                    )),
            )
        })
        .into_any_element()
}

fn control(
    name: &'static str,
    label: &'static str,
    area: WindowControlArea,
    action: fn(&mut Window),
) -> AnyElement {
    let close = area == WindowControlArea::Close;
    div()
        .id(name)
        .group(name)
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .window_control_area(area)
        .size(px(26.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color(TEXT_MUTED))
        .bg(color(ELEVATED))
        .hover(move |style| {
            if close {
                style.bg(color(DANGER)).text_color(color(0xffffff))
            } else {
                style.bg(color(SELECTED)).text_color(color(TEXT))
            }
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            action(window);
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                cx.stop_propagation();
                action(window);
            }
        })
        .child(
            icon(name, 14., color(TEXT_MUTED)).group_hover(name, move |style| {
                style.text_color(color(if close { 0xffffff } else { TEXT }))
            }),
        )
        .into_any_element()
}
