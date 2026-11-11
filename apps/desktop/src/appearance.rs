// SPDX-License-Identifier: EUPL-1.2
//! Light and dark (following the desktop by default), fonts, text size and
//! zoom: applied to the palette and the window, and their Settings section.
//!
//! Zoom scales the whole window like a browser's page zoom, through the
//! patched GPUI window (vendor/gpui-pre/NUSCHEL-PATCH.md): layout, text,
//! images and input all grow, and glyphs are rasterised at the new size.
use crate::{
    dialogs::{Tone, styled_button},
    settings::{Action, managed, segmented},
    state::{Desktop, accessible},
    theme::{self, *},
};
use gpui_kit::{
    base::input::{Input, InputEvent, InputState},
    prelude::*,
    *,
};
use nuschel_session::config::{TEXT_SIZE, ThemeChoice, ZOOM};
use std::{rc::Rc, time::Duration};

actions!(nuschel, [ZoomIn, ZoomOut, ResetZoom]);

/// Ctrl (Cmd on macOS) with = or + zooms in, with - out, with 0 back to 100 %.
/// None of these type text, so the composer keeps every key it needs.
pub fn bind_keys(cx: &mut App) {
    let (zoom_in, plus, zoom_out, reset) = if cfg!(target_os = "macos") {
        ("cmd-=", "cmd-+", "cmd--", "cmd-0")
    } else {
        ("ctrl-=", "ctrl-+", "ctrl--", "ctrl-0")
    };
    cx.bind_keys([
        KeyBinding::new(zoom_in, ZoomIn, None),
        KeyBinding::new(plus, ZoomIn, None),
        KeyBinding::new(zoom_out, ZoomOut, None),
        KeyBinding::new(reset, ResetZoom, None),
    ]);
}

/// Browser-like zoom levels.
const STEPS: [f32; 13] = [
    0.5, 0.67, 0.75, 0.8, 0.9, 1., 1.1, 1.25, 1.5, 1.75, 2., 2.5, 3.,
];
/// How long the zoom level shows after it changes.
const BADGE: Duration = Duration::from_millis(1200);

/// The next level up from `zoom`.
fn larger(zoom: f32) -> f32 {
    let next = STEPS.iter().find(|level| **level > zoom + 0.001);
    next.copied().unwrap_or(zoom).min(*ZOOM.end())
}

/// The next level down from `zoom`.
fn smaller(zoom: f32) -> f32 {
    let next = STEPS.iter().rev().find(|level| **level < zoom - 0.001);
    next.copied().unwrap_or(zoom).max(*ZOOM.start())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FontRole {
    Interface,
    Code,
}

struct FontPicker {
    role: FontRole,
    search: Entity<InputState>,
    _changes: Subscription,
}

/// The window's appearance state: the font picker, the zoom level badge and
/// the subscription to the desktop's light or dark preference.
pub struct Appearance {
    picker: Option<FontPicker>,
    /// Installed font families, sorted, with their names in lower case for
    /// searching; read when a picker first opens.
    families: Option<Rc<[(SharedString, String)]>>,
    badge: Option<Task<()>>,
    _system: Subscription,
}

impl Appearance {
    /// Follows the desktop's preference while the theme is System, and
    /// zooms from anywhere in the window, focused or not.
    pub fn new(window: &mut Window, cx: &mut Context<Desktop>) -> Self {
        let system = cx.observe_window_appearance(window, |this, window, cx| {
            this.apply_appearance(window, cx);
            cx.notify();
        });
        let desktop = cx.entity().downgrade();
        let handle = window.window_handle();
        let zoom = move |level: fn(f32) -> f32, cx: &mut App| {
            let desktop = desktop.clone();
            // The window is busy dispatching the key; zoom once it is done.
            cx.defer(move |cx| {
                handle
                    .update(cx, |_, window, cx| {
                        desktop.update(cx, |this, cx| {
                            let zoom = level(this.settings.appearance.zoom);
                            this.zoom(zoom, window, cx)
                        })
                    })
                    .ok();
            });
        };
        let (zoom_in, zoom_out, reset) = (zoom.clone(), zoom.clone(), zoom);
        // App-wide handlers: they run whatever has focus, after any element's.
        let app: &mut App = cx;
        app.on_action(move |_: &ZoomIn, cx| zoom_in(larger, cx));
        app.on_action(move |_: &ZoomOut, cx| zoom_out(smaller, cx));
        app.on_action(move |_: &ResetZoom, cx| reset(|_| 1., cx));
        Self {
            picker: None,
            families: None,
            badge: None,
            _system: system,
        }
    }

    pub fn close_picker(&mut self) {
        self.picker = None;
    }
}

impl Desktop {
    /// Bring the palette, fonts and zoom in line with the settings and, for
    /// the System theme, the desktop's preference.
    pub fn apply_appearance(&mut self, window: &mut Window, cx: &mut App) {
        let settings = &self.settings.appearance;
        // macOS draws the window's frame in this appearance (elsewhere a no-op).
        cx.set_window_appearance(match settings.theme {
            ThemeChoice::System => None,
            ThemeChoice::Light => Some(WindowAppearance::Light),
            ThemeChoice::Dark => Some(WindowAppearance::Dark),
        });
        let light = match settings.theme {
            ThemeChoice::System => matches!(
                window.appearance(),
                WindowAppearance::Light | WindowAppearance::VibrantLight
            ),
            ThemeChoice::Light => true,
            ThemeChoice::Dark => false,
        };
        theme::set_light(light);
        theme::set_fonts(
            family(settings.ui_font.as_deref(), DEFAULT_UI_FONT),
            family(settings.mono_font.as_deref(), DEFAULT_MONO_FONT),
        );
        // Inputs draw their text, caret and selection from GPUI Base's theme.
        let base = gpui_kit::base::Theme::global_mut(cx);
        let mut colors = if light {
            base.appearance = gpui_kit::base::ThemeAppearance::Light;
            gpui_kit::base::ColorTokens::light()
        } else {
            base.appearance = gpui_kit::base::ThemeAppearance::Dark;
            gpui_kit::base::ColorTokens::dark()
        };
        colors.foreground = color(TEXT).into();
        colors.muted_foreground = color(TEXT_MUTED).into();
        colors.accent = color(ACCENT).into();
        colors.ring = color(ACCENT).into();
        colors.selection = tint(ACCENT, 0x4d).into();
        colors.border = color(BORDER).into();
        base.tokens.colors = colors;
        window.set_zoom(settings.zoom, cx);
        window.refresh();
    }

    /// Change the appearance settings and show the result.
    fn change_appearance(
        &mut self,
        change: impl FnOnce(&mut nuschel_session::config::AppearanceSettings),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.update_settings(|settings| change(&mut settings.appearance));
        self.apply_appearance(window, cx);
    }

    /// Zoom the window, and show the level for a moment.
    fn zoom(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.change_appearance(|appearance| appearance.zoom = zoom, window, cx);
        self.appearance.badge = Some(cx.spawn(async |this, cx| {
            cx.background_executor().timer(BADGE).await;
            this.update(cx, |this, cx| {
                this.appearance.badge = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_font_picker(&mut self, role: FontRole, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance.picker.as_ref().map(|picker| picker.role) == Some(role) {
            self.appearance.picker = None;
            return;
        }
        if self.appearance.families.is_none() {
            let mut names: Vec<_> = cx
                .text_system()
                .all_font_names()
                .into_iter()
                .filter(|name| !name.starts_with('.'))
                .map(|name| (name.to_lowercase(), name))
                .collect();
            names.sort();
            names.dedup();
            let names = names.into_iter().map(|(key, name)| (name.into(), key));
            self.appearance.families = Some(names.collect());
        }
        let label = match role {
            FontRole::Interface => "Search interface fonts",
            FontRole::Code => "Search code fonts",
        };
        let search = cx.new(|cx| {
            accessible(
                InputState::new(window, cx).placeholder("Search installed fonts"),
                label,
                false,
                false,
            )
        });
        search.update(cx, |input, cx| input.focus(window, cx));
        let changes = cx.subscribe(&search, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        self.appearance.picker = Some(FontPicker {
            role,
            search,
            _changes: changes,
        });
    }
}

fn family(name: Option<&str>, default: &'static str) -> SharedString {
    name.map_or(default.into(), |name| SharedString::from(name.to_string()))
}

/// The zoom level for a moment after it changes, over the window's top.
pub fn badge(this: &Desktop) -> Option<AnyElement> {
    this.appearance.badge.as_ref()?;
    let percent = (this.settings.appearance.zoom * 100.).round();
    Some(
        div()
            .absolute()
            .top(px(52.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("zoom-level")
                    .role(Role::Status)
                    .px_3()
                    .py_1()
                    .rounded(px(8.))
                    .bg(color(ELEVATED))
                    .border_1()
                    .border_color(color(BORDER_STRONG))
                    .shadow_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(text!(format!("Zoom {percent} %")).with_id("zoom-level-text")),
            )
            .into_any_element(),
    )
}

/// A label, its value and − / + buttons (named for screen readers).
fn stepper(
    id: &'static str,
    label: &'static str,
    value: String,
    (down_label, down): (&'static str, Option<Action>),
    (up_label, up): (&'static str, Option<Action>),
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let button = |name: &'static str, glyph: &'static str, action: Option<Action>| {
        let enabled = action.is_some();
        div()
            .id((id, usize::from(glyph == "+")))
            .role(Role::Button)
            .aria_label(name)
            .tab_index(0)
            .size(px(28.))
            .flex_shrink_0()
            .rounded(px(6.))
            .flex()
            .items_center()
            .justify_center()
            .bg(color(ELEVATED))
            .text_color(color(if enabled { TEXT } else { TEXT_FAINT }))
            .when_some(action, |button, action| {
                let key_action = action.clone();
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(color(SELECTED)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        action(this, window, cx);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            key_action(this, window, cx);
                            cx.notify();
                        }
                    }))
            })
            .child(glyph)
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!(label).with_id((id, 10_usize))),
        )
        .child(button(down_label, "−", down))
        .child(
            div()
                .min_w(px(56.))
                .flex()
                .justify_center()
                .text_sm()
                .child(text!(value).with_id((id, 11_usize))),
        )
        .child(button(up_label, "+", up))
        .into_any_element()
}

/// A font's row: its name in that font, opening the picker.
fn font_row(
    this: &Desktop,
    role: FontRole,
    chosen: Option<&str>,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let (id, label, default) = match role {
        FontRole::Interface => ("ui-font", "Interface font", DEFAULT_UI_FONT),
        FontRole::Code => ("code-font", "Code font", DEFAULT_MONO_FONT),
    };
    let open = this.appearance.picker.as_ref().map(|picker| picker.role) == Some(role);
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_1()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!(label).with_id((id, 1_usize))),
        )
        .child(
            div()
                .id(id)
                .role(Role::Button)
                .aria_label(format!("{label}: {}", chosen.unwrap_or("System default")))
                .aria_expanded(open)
                .tab_index(0)
                .max_w(px(260.))
                .px_3()
                .py(px(6.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .bg(color(if open { SELECTED } else { ELEVATED }))
                .hover(|style| style.bg(color(SELECTED)))
                .text_sm()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_font_picker(role, window, cx);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.open_font_picker(role, window, cx);
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(family(chosen, default))
                        .child(
                            text!(chosen.unwrap_or("System default").to_string())
                                .with_id((id, 2_usize)),
                        ),
                )
                .child(crate::assets::icon("chevron-down", 14., color(TEXT_MUTED))),
        )
        .into_any_element()
}

/// At most this many families list at once; the search narrows them down.
const LISTED: usize = 60;

/// The open picker: a search field and the matching installed families.
fn picker(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let picker = this.appearance.picker.as_ref()?;
    let role = picker.role;
    let query = picker.search.read(cx).value().trim().to_lowercase();
    let families = this.appearance.families.clone().unwrap_or_default();
    let chosen = match role {
        FontRole::Interface => this.settings.appearance.ui_font.clone(),
        FontRole::Code => this.settings.appearance.mono_font.clone(),
    };
    let mut matching = families.iter().filter(|(_, key)| key.contains(&query));
    let listed: Vec<SharedString> = matching
        .by_ref()
        .take(LISTED)
        .map(|(name, _)| name.clone())
        .collect();
    let more = matching.count();
    let choices = std::iter::once(None)
        .filter(|_| query.is_empty())
        .chain(listed.into_iter().map(Some));
    let rows = choices.enumerate().map(|(index, name)| {
        let selected = name.as_deref() == chosen.as_deref();
        let label: SharedString = name.clone().unwrap_or("System default".into());
        let choose = move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
            let name = name.as_ref().map(ToString::to_string);
            this.appearance.picker = None;
            this.change_appearance(
                |appearance| match role {
                    FontRole::Interface => appearance.ui_font = name,
                    FontRole::Code => appearance.mono_font = name,
                },
                window,
                cx,
            );
        };
        let choose = Rc::new(choose);
        let key_choose = choose.clone();
        div()
            .id(("font-choice", index))
            .role(Role::RadioButton)
            .aria_label(label.clone())
            .aria_selected(selected)
            .tab_index(0)
            .flex_shrink_0()
            .px_2()
            .py(px(5.))
            .rounded(px(6.))
            .cursor_pointer()
            .text_sm()
            .truncate()
            .when(selected, |row| row.bg(color(SELECTED)))
            .hover(|style| style.bg(color(HOVER)))
            .on_click(cx.listener(move |this, _, window, cx| {
                choose(this, window, cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    key_choose(this, window, cx);
                    cx.notify();
                }
            }))
            .child(text!(label).with_id(("font-choice-name", index)))
    });
    Some(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(px(8.))
            .bg(color(APP))
            .border_1()
            .border_color(color(BORDER))
            .child(
                div()
                    .px_2()
                    .py(px(6.))
                    .rounded(px(6.))
                    .bg(color(INPUT))
                    .border_1()
                    .border_color(color(BORDER))
                    .child(Input::new(&picker.search)),
            )
            .child(
                div()
                    .id("font-choices")
                    .role(Role::RadioGroup)
                    .aria_label("Installed fonts")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .when(more > 0, |list| {
                list.child(
                    div().px_2().text_xs().text_color(color(TEXT_FAINT)).child(
                        text!(format!("{more} more: type to narrow down")).with_id("font-more"),
                    ),
                )
            })
            .into_any_element(),
    )
}

/// Settings → Appearance.
pub fn settings(this: &Desktop, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    let appearance = &this.settings.appearance;
    let set_theme = |choice: ThemeChoice| -> Action {
        Rc::new(
            move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
                this.change_appearance(|appearance| appearance.theme = choice, window, cx)
            },
        )
    };
    // While the theme is System, the palette is the desktop's.
    let system = if theme::is_light() { "light" } else { "dark" };
    let text_size = appearance.text_size;
    let resize = |size: f32| -> Option<Action> {
        (TEXT_SIZE.contains(&size) && size != text_size).then(|| -> Action {
            Rc::new(
                move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
                    this.change_appearance(|appearance| appearance.text_size = size, window, cx)
                },
            )
        })
    };
    let zoom = appearance.zoom;
    let zoom_to = |level: f32| -> Option<Action> {
        (level != zoom).then(|| -> Action {
            Rc::new(
                move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
                    this.zoom(level, window, cx)
                },
            )
        })
    };
    let shortcut = if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    };
    let theme = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!("Theme").with_id("theme-heading")),
        )
        .child(segmented(
            "theme",
            "Theme",
            vec![
                (
                    "System",
                    appearance.theme == ThemeChoice::System,
                    set_theme(ThemeChoice::System),
                ),
                (
                    "Light",
                    appearance.theme == ThemeChoice::Light,
                    set_theme(ThemeChoice::Light),
                ),
                (
                    "Dark",
                    appearance.theme == ThemeChoice::Dark,
                    set_theme(ThemeChoice::Dark),
                ),
            ],
            cx,
        ))
        .child(
            div().text_xs().text_color(color(TEXT_MUTED)).child(
                text!(if appearance.theme == ThemeChoice::System {
                    format!("Follows your desktop's light or dark style (now {system}).")
                } else {
                    "System follows your desktop's light or dark style.".to_string()
                })
                .with_id("theme-detail"),
            ),
        )
        .into_any_element();
    let mut rows = vec![
        managed(this, "appearance.theme", theme),
        managed(
            this,
            "appearance.ui_font",
            font_row(this, FontRole::Interface, appearance.ui_font.as_deref(), cx),
        ),
    ];
    if this.appearance.picker.as_ref().map(|picker| picker.role) == Some(FontRole::Interface) {
        rows.extend(picker(this, cx));
    }
    rows.push(managed(
        this,
        "appearance.mono_font",
        font_row(this, FontRole::Code, appearance.mono_font.as_deref(), cx),
    ));
    if this.appearance.picker.as_ref().map(|picker| picker.role) == Some(FontRole::Code) {
        rows.extend(picker(this, cx));
    }
    let text_size_row = stepper(
        "text-size",
        "Message text size",
        format!("{text_size:.0} px"),
        ("Smaller text", resize(text_size - 1.)),
        ("Larger text", resize(text_size + 1.)),
        cx,
    );
    rows.push(managed(this, "appearance.text_size", text_size_row));
    let render_math = appearance.render_math;
    rows.push(managed(
        this,
        "appearance.render_math",
        crate::settings::toggle(
            "render-math",
            "Render math",
            "Shows LaTeX between $…$ and $$…$$, and math code blocks, as formulas. \
             Others see what was typed."
                .into(),
            render_math,
            Rc::new(
                move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
                    this.change_appearance(
                        |appearance| appearance.render_math = !render_math,
                        window,
                        cx,
                    )
                },
            ),
            cx,
        ),
    ));
    let zoom_row = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(stepper(
            "zoom",
            "Zoom",
            format!("{:.0} %", zoom * 100.),
            ("Zoom out", zoom_to(smaller(zoom))),
            ("Zoom in", zoom_to(larger(zoom))),
            cx,
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div().flex_1().text_xs().text_color(color(TEXT_FAINT)).child(
                        text!(format!(
                            "{shortcut}+= and {shortcut}+− zoom anywhere; {shortcut}+0 returns to 100 %."
                        ))
                        .with_id("zoom-detail"),
                    ),
                )
                .when(zoom != 1., |row| {
                    row.child(styled_button(
                        "reset-zoom",
                        "Reset",
                        Tone::Ghost,
                        |this, window, cx| this.zoom(1., window, cx),
                        cx,
                    ))
                }),
        )
        .into_any_element();
    rows.push(managed(this, "appearance.zoom", zoom_row));
    rows
}

#[cfg(test)]
mod tests {
    use super::{larger, smaller};

    #[test]
    fn zoom_steps_like_a_browser_and_stops_at_the_ends() {
        assert_eq!(larger(1.), 1.1);
        assert_eq!(larger(1.1), 1.25);
        assert_eq!(smaller(1.), 0.9);
        // A level between steps goes to the neighbouring one.
        assert_eq!(larger(1.2), 1.25);
        assert_eq!(smaller(1.2), 1.1);
        assert_eq!(larger(3.), 3.);
        assert_eq!(smaller(0.5), 0.5);
    }
}
