// SPDX-License-Identifier: EUPL-1.2
//! Window chrome. Who draws the window's frame is the compositor's decision
//! ([`DecorationChoice`] only states a preference); when it answers client-side
//! (GNOME always does), Nuschl draws its frame, window controls and resize edges.
//!
//! GPUI clips overflow to rectangles, so the rounded frame does not round what
//! is inside it: elements that paint into a window corner take [`corner_radii`].
use crate::{
    assets::{app_icon, icon},
    state::Desktop,
    theme::*,
};
use gpui_kit::{prelude::*, *};

/// Who should draw the window's frame on Linux (`--window-decorations`, or
/// `NUSCHL_WINDOW_DECORATIONS`). A preference only: on Wayland the compositor
/// answers, and Nuschl follows the answer (see [`frame`]). Elsewhere the
/// platform's own frame is used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum DecorationChoice {
    /// Wayland: no preference, the compositor decides (niri's `prefer-no-csd`,
    /// KDE and others answer server-side; GNOME client-side). X11: Nuschl draws
    /// its own frame, or the window manager does without a compositing manager.
    #[default]
    Auto,
    /// Ask the compositor or window manager to draw it.
    Server,
    /// Nuschl draws it, unless the compositor insists on drawing it.
    Client,
}

impl DecorationChoice {
    pub const ENVIRONMENT: &str = "NUSCHL_WINDOW_DECORATIONS";

    /// `NUSCHL_WINDOW_DECORATIONS`, if set: `auto`, `server` or `client`.
    pub fn from_environment() -> Result<Option<Self>, String> {
        std::env::var_os(Self::ENVIRONMENT)
            .map(|value| Self::parse(&value))
            .transpose()
    }

    fn parse(value: &std::ffi::OsStr) -> Result<Self, String> {
        value
            .to_str()
            .and_then(|text| <Self as clap::ValueEnum>::from_str(text.trim(), true).ok())
            .ok_or_else(|| {
                format!(
                    "{} is {value:?}, not auto, server or client; using auto",
                    Self::ENVIRONMENT
                )
            })
    }

    /// What to ask GPUI for, given `cx.compositor_name()`.
    pub fn request(self, compositor: &str) -> WindowDecorations {
        if !cfg!(target_os = "linux") {
            return WindowDecorations::Server;
        }
        match self {
            Self::Server => WindowDecorations::Server,
            Self::Client => WindowDecorations::Client,
            // X11 has no answer to a request: a server-side one is only a hint
            // (_MOTIF_WM_HINTS), and GPUI would report server-side decorations
            // even if the window manager drew none. GPUI itself falls back to
            // server-side when no compositing manager runs or the window manager
            // lacks _GTK_FRAME_EXTENTS, which a client-side frame needs.
            Self::Auto if compositor == "X11" => WindowDecorations::Client,
            Self::Auto => WindowDecorations::Compositor,
        }
    }
}

/// Invisible border around client-decorated windows that carries the shadow
/// and resize hit areas.
const INSET: Pixels = px(10.);
const RADIUS: Pixels = px(10.);
/// The frame's border width.
const FRAME_BORDER: Pixels = px(1.);
pub const TITLE_HEIGHT: Pixels = px(38.);

fn client_decorated(window: &Window) -> Option<Tiling> {
    match window.window_decorations() {
        Decorations::Client { tiling } => Some(tiling),
        Decorations::Server => None,
    }
}

/// The edges the frame keeps against something else, when Nuschl draws the frame.
fn frame_edges(window: &Window) -> Option<Tiling> {
    let tiling = client_decorated(window)?;
    Some(if window.is_maximized() || window.is_fullscreen() {
        Tiling::tiled()
    } else {
        tiling
    })
}

/// Keep the content's curve aligned with the border actually painted below.
fn frame_border(edges: Tiling) -> Pixels {
    if edges.is_tiled() {
        px(0.)
    } else {
        FRAME_BORDER
    }
}

/// The frame's corner radii: a corner is round where neither of its edges is tiled.
fn frame_radii(edges: Tiling, radius: Pixels) -> Corners<Pixels> {
    let corner = |a: bool, b: bool| if a || b { px(0.) } else { radius };
    Corners {
        top_left: corner(edges.top, edges.left),
        top_right: corner(edges.top, edges.right),
        bottom_left: corner(edges.bottom, edges.left),
        bottom_right: corner(edges.bottom, edges.right),
    }
}

/// The radii of the window's rounded corners inside its border. An element that
/// paints a background into a corner of the window rounds it by these (see
/// [`round`]); otherwise its square corner shows outside the frame's curve.
pub fn corner_radii(window: &Window) -> Corners<Pixels> {
    frame_edges(window).map(inner_radii).unwrap_or_default()
}

fn inner_radii(edges: Tiling) -> Corners<Pixels> {
    frame_radii(edges, RADIUS - frame_border(edges))
}

/// Rounds `element`'s corners by `radii`, as from [`corner_radii`].
pub fn round<E: Styled>(element: E, radii: Corners<Pixels>) -> E {
    element
        .rounded_tl(radii.top_left)
        .rounded_tr(radii.top_right)
        .rounded_bl(radii.bottom_left)
        .rounded_br(radii.bottom_right)
}

/// Wraps the application in client-side decorations when the platform asks for them.
pub fn frame(window: &mut Window, content: AnyElement) -> AnyElement {
    let Some(edges) = frame_edges(window) else {
        return div().size_full().child(content).into_any_element();
    };
    let outer = frame_radii(edges, RADIUS);
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
                        Bounds::new(point(px(0.), px(0.)), window.viewport_size()),
                        HitboxBehavior::Normal,
                    )
                },
                move |_, hitbox, window, _| {
                    let size = window.viewport_size();
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
            let size = window.viewport_size();
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
                .map(|surface| round(surface, outer))
                .border(frame_border(edges))
                .when(!edges.is_tiled(), |surface| {
                    surface.border_color(color(BORDER_STRONG)).shadow(vec![
                        BoxShadow::new(px(0.), px(2.), color(SHADOW).into())
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
    let corners = corner_radii(window);
    div()
        .id("title-bar")
        .h(TITLE_HEIGHT)
        .flex_shrink_0()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .bg(color(APP))
        .rounded_tl(corners.top_left)
        .rounded_tr(corners.top_right)
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
                .child(text!("Nuschl").with_id("app-name")),
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
                style.bg(color(DANGER)).text_color(color(WHITE))
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
                style.text_color(color(if close { WHITE } else { TEXT }))
            }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{DecorationChoice, RADIUS, frame_border, frame_radii, inner_radii};
    use gpui_kit::{Corners, Pixels, Tiling, WindowDecorations, px};

    fn corner_contains((x, y): (f32, f32), radius: f32) -> bool {
        x >= radius || y >= radius || (x - radius).powi(2) + (y - radius).powi(2) <= radius.powi(2)
    }

    fn four(corners: Corners<Pixels>) -> [Pixels; 4] {
        [
            corners.top_left,
            corners.top_right,
            corners.bottom_left,
            corners.bottom_right,
        ]
    }

    #[test]
    fn partially_tiled_content_cannot_paint_outside_the_unbordered_curve() {
        let edges = Tiling {
            left: true,
            top: false,
            right: false,
            bottom: false,
        };
        let point = (1.5, 4.5);
        // The old 9px inner curve admitted this point outside the 10px frame:
        // with no border both curves start at the same corner, not 1px apart.
        assert!(corner_contains(point, 9.));
        assert!(!corner_contains(point, 10.));
        assert_eq!(frame_border(edges), px(0.));
        assert!(!corner_contains(
            point,
            f32::from(inner_radii(edges).top_right)
        ));
        assert_eq!(inner_radii(Tiling::default()).top_right, px(9.));
        assert_eq!(frame_border(Tiling::default()), px(1.));
        assert_eq!(four(inner_radii(Tiling::tiled())), [px(0.); 4]);
    }

    #[test]
    fn every_tiling_pattern_keeps_inner_paint_inside_the_outer_corners() {
        for bits in 0..16 {
            let edges = Tiling {
                top: bits & 1 != 0,
                left: bits & 2 != 0,
                right: bits & 4 != 0,
                bottom: bits & 8 != 0,
            };
            let border = f32::from(frame_border(edges));
            for (outer, inner) in four(frame_radii(edges, RADIUS))
                .into_iter()
                .zip(four(inner_radii(edges)))
            {
                for x in 0..48 {
                    for y in 0..48 {
                        let point = ((x as f32 + 0.5) / 4., (y as f32 + 0.5) / 4.);
                        let child = (point.0 - border, point.1 - border);
                        if child.0 >= 0.
                            && child.1 >= 0.
                            && corner_contains(child, f32::from(inner))
                        {
                            assert!(
                                corner_contains(point, f32::from(outer)),
                                "tiling={bits}, outer={outer:?}, inner={inner:?}, border={border}, point={point:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn decoration_choices_become_requests() {
        let expected = |linux| {
            if cfg!(target_os = "linux") {
                linux
            } else {
                WindowDecorations::Server
            }
        };
        let auto = DecorationChoice::Auto;
        assert_eq!(
            auto.request("Wayland"),
            expected(WindowDecorations::Compositor)
        );
        assert_eq!(auto.request("X11"), expected(WindowDecorations::Client));
        for compositor in ["Wayland", "X11"] {
            assert_eq!(
                DecorationChoice::Server.request(compositor),
                WindowDecorations::Server
            );
            assert_eq!(
                DecorationChoice::Client.request(compositor),
                expected(WindowDecorations::Client)
            );
        }
    }

    #[test]
    fn the_environment_names_a_choice() {
        let parse = |value: &str| DecorationChoice::parse(value.as_ref());
        assert_eq!(parse("auto"), Ok(DecorationChoice::Auto));
        assert_eq!(parse(" Server\n"), Ok(DecorationChoice::Server));
        assert_eq!(parse("CLIENT"), Ok(DecorationChoice::Client));
        for wrong in ["", "csd", "none"] {
            assert!(parse(wrong).is_err(), "{wrong:?}");
        }
    }
}
