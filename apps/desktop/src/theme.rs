// SPDX-License-Identifier: EUPL-1.2
//! One palette for the whole shell, so panels read as a single system. Each
//! colour has a dark and a light value side by side; [`color`] and [`tint`]
//! pick the active one, so call sites name what a colour is for and work in
//! both. The active palette and fonts belong to the UI thread
//! ([`crate::appearance`] sets them); a lookup is a thread-local read.
use gpui_kit::{Hsla, Rgba, SharedString, rgba};
use std::cell::{Cell, RefCell};

/// A colour in the dark and in the light palette, as 0xRRGGBBAA.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Token {
    dark: u32,
    light: u32,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "#{:08x}/#{:08x}", self.dark, self.light)
    }
}

impl Token {
    /// Opaque colours, given as 0xRRGGBB.
    const fn new(dark: u32, light: u32) -> Self {
        Self {
            dark: dark << 8 | 0xff,
            light: light << 8 | 0xff,
        }
    }
    /// Translucent colours, given as 0xRRGGBBAA.
    const fn rgba(dark: u32, light: u32) -> Self {
        Self { dark, light }
    }
    /// The same in both palettes.
    const fn fixed(value: u32) -> Self {
        Self::new(value, value)
    }
    /// The colour in the light or the dark palette, as 0xRRGGBBAA.
    pub fn get(self, light: bool) -> u32 {
        if light { self.light } else { self.dark }
    }
}

// Surfaces, from the window behind everything to raised controls.
pub const APP: Token = Token::new(0x0f1218, 0xe6e8ec);
pub const RAIL: Token = Token::new(0x0f1218, 0xe6e8ec);
pub const SIDEBAR: Token = Token::new(0x161a22, 0xf2f3f6);
pub const SIDEBAR_FOOTER: Token = Token::new(0x12151c, 0xeaecf0);
pub const MAIN: Token = Token::new(0x1c212b, 0xffffff);
/// A message row's hover wash, and MAIN under it.
pub const ROW_HOVER: Token = Token::rgba(0x0000_0018, 0x0f17_2a0c);
pub const MAIN_HOVERED: Token = Token::new(0x191e27, 0xf4f4f5);
pub const ELEVATED: Token = Token::new(0x262c38, 0xe9ecf0);
pub const INPUT: Token = Token::new(0x232935, 0xf0f2f5);
pub const HOVER: Token = Token::new(0x232a36, 0xe6e9ee);
pub const SELECTED: Token = Token::new(0x2c3443, 0xd3d9e2);
pub const BORDER: Token = Token::new(0x262d3a, 0xe1e4ea);
pub const BORDER_STRONG: Token = Token::new(0x353e4e, 0xc9cfd9);
/// Behind dialogs.
pub const SCRIM: Token = Token::rgba(0x0000_00aa, 0x0f17_2a66);
/// The shadow of the window's client-side frame.
pub const SHADOW: Token = Token::rgba(0x0000_0073, 0x0f17_2a38);
/// Inline code's background.
pub const CODE_SPAN: Token = Token::rgba(0xffff_ff14, 0x0f17_2a0f);

pub const TEXT: Token = Token::new(0xe8ebf2, 0x1b2130);
/// Message bodies, a little quieter than names and headings.
pub const BODY: Token = Token::new(0xdbdfe8, 0x2a3142);
pub const TEXT_MUTED: Token = Token::new(0x9aa4b7, 0x475066);
/// Timestamps and hints remain AA-readable even on selected/elevated surfaces.
pub const TEXT_FAINT: Token = Token::new(0x949fb2, 0x5d6678);

pub const ACCENT: Token = Token::new(0x5b7cfa, 0x3b5bdb);
pub const ACCENT_HOVER: Token = Token::new(0x6f8cff, 0x2f4bc2);
pub const LINK: Token = Token::new(0x6f8cff, 0x2f50d0);
pub const SUCCESS: Token = Token::new(0x3ccf8e, 0x13804f);
pub const WARNING: Token = Token::new(0xf0b232, 0x8f5b00);
pub const DANGER: Token = Token::new(0xf2545b, 0xc62b34);
pub const DANGER_HOVER: Token = Token::new(0xff6b72, 0xa8222a);
/// Text on a DANGER tint.
pub const DANGER_TEXT: Token = Token::new(0xffc9cc, 0x9b1c23);

/// Text and icons on accent, danger and avatar colours, and switch knobs.
pub const WHITE: Token = Token::fixed(0xffffff);
/// The friend marker (a heart).
pub const FRIEND: Token = Token::new(0xee6ea8, 0xc2185b);
/// File cards: archives.
pub const ARCHIVE: Token = Token::new(0xb48cf2, 0x7e57c2);
pub const BLACK: Token = Token::fixed(0x000000);
/// The image viewer stays dark in both palettes.
pub const BACKDROP: Token = Token::fixed(0x07090e);

thread_local! {
    static LIGHT: Cell<bool> = const { Cell::new(false) };
    static FONTS: RefCell<(SharedString, SharedString)> =
        RefCell::new((DEFAULT_UI_FONT.into(), DEFAULT_MONO_FONT.into()));
}

pub const DEFAULT_UI_FONT: &str = "sans-serif";
pub const DEFAULT_MONO_FONT: &str = "monospace";

/// Whether the light palette is active.
pub fn is_light() -> bool {
    LIGHT.get()
}

pub fn set_light(light: bool) {
    LIGHT.set(light);
}

pub fn color(token: Token) -> Rgba {
    rgba(token.get(is_light()))
}

/// The colour at `alpha`.
pub fn tint(token: Token, alpha: u8) -> Rgba {
    rgba(token.get(is_light()) & !0xff | u32::from(alpha))
}

/// The interface's font family.
pub fn ui_font() -> SharedString {
    FONTS.with_borrow(|(ui, _)| ui.clone())
}

/// The family for code, inline and in blocks.
pub fn mono_font() -> SharedString {
    FONTS.with_borrow(|(_, mono)| mono.clone())
}

pub fn set_fonts(ui: SharedString, mono: SharedString) {
    FONTS.set((ui, mono));
}

fn hue(name: &str) -> f32 {
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    (hash % 360) as f32 / 360.
}

/// Stable, readable avatar colours derived from a display name (under white
/// initials, in both palettes).
pub fn avatar(name: &str) -> Hsla {
    gpui_kit::hsla(hue(name), 0.45, 0.42, 1.)
}

/// The same hue for author names: light on the dark timeline, dark on the
/// light one (at least 4.5:1 for every hue).
pub fn name_color(name: &str) -> Hsla {
    if is_light() {
        gpui_kit::hsla(hue(name), 0.5, 0.3, 1.)
    } else {
        gpui_kit::hsla(hue(name), 0.55, 0.74, 1.)
    }
}

fn luminance(color: Hsla) -> f32 {
    let channel = |value: f32| {
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let Rgba { r, g, b, .. } = color.to_rgb();
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// The WCAG contrast ratio of two colours.
pub fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `color` with the least change of lightness (hue and saturation kept)
/// that gives AA contrast (4.5:1) on `background`: lighter on a dark
/// background, darker on a light one.
pub fn readable(color: Hsla, background: Token, light: bool) -> Hsla {
    const AA: f32 = 4.5;
    let background: Hsla = rgba(background.get(light)).into();
    if contrast(color, background) >= AA {
        return color;
    }
    // Lightness towards the background's opposite end until it contrasts.
    let (mut near, mut far) = (color.l, if light { 0. } else { 1. });
    for _ in 0..16 {
        let middle = (near + far) / 2.;
        if contrast(Hsla { l: middle, ..color }, background) >= AA {
            far = middle;
        } else {
            near = middle;
        }
    }
    Hsla { l: far, ..color }
}

/// A syntax colour from the dark highlighting theme, for the palette: as is
/// on dark, and with its lightness mirrored on light, so the theme's
/// structure (quiet comments, bright keywords) carries over, then made
/// readable on the code block.
pub fn code_color(color: Hsla, light: bool) -> Hsla {
    if light {
        readable(
            Hsla {
                l: 1. - color.l,
                ..color
            },
            APP,
            true,
        )
    } else {
        color
    }
}

pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|word| word.chars().next());
    let second = words.next().and_then(|word| word.chars().next());
    match (first, second) {
        (Some(a), Some(b)) => [a, b].iter().flat_map(|c| c.to_uppercase()).collect(),
        (Some(_), None) => name.chars().take(2).flat_map(char::to_uppercase).collect(),
        _ => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsla(token: Token, light: bool) -> Hsla {
        rgba(token.get(light)).into()
    }

    #[test]
    fn initials_handle_unicode_and_single_words() {
        assert_eq!(super::initials("ada lovelace"), "AL");
        assert_eq!(super::initials("Ölf"), "ÖL");
        assert_eq!(super::initials(""), "?");
    }

    #[test]
    fn colors_follow_the_active_palette() {
        set_light(false);
        assert_eq!(color(TEXT), rgba(0xe8eb_f2ff));
        assert_eq!(tint(ACCENT, 0x26), rgba(0x5b7c_fa26));
        assert_eq!(color(SCRIM), rgba(0x0000_00aa));
        set_light(true);
        assert_eq!(color(TEXT), rgba(0x1b21_30ff));
        assert_eq!(tint(ACCENT, 0x26), rgba(0x3b5b_db26));
        assert_eq!(color(WHITE), rgba(0xffff_ffff));
        assert!(is_light());
        set_light(false);
        assert_eq!(color(WHITE), rgba(0xffff_ffff));
    }

    #[test]
    fn the_dark_palette_keeps_secondary_text_readable() {
        for surface in [
            APP,
            RAIL,
            SIDEBAR,
            SIDEBAR_FOOTER,
            MAIN,
            MAIN_HOVERED,
            ELEVATED,
            INPUT,
            HOVER,
            SELECTED,
        ] {
            let ratio = contrast(hsla(TEXT_FAINT, false), hsla(surface, false));
            assert!(ratio >= 4.5, "faint text on {surface:?}: {ratio}");
        }
        // Secondary labels stay quieter than regular text rather than becoming
        // another bright foreground, and the light palette is independent.
        for pair in [TEXT_FAINT, TEXT_MUTED, BODY, TEXT].windows(2) {
            assert!(luminance(hsla(pair[0], false)) < luminance(hsla(pair[1], false)));
        }
    }

    #[test]
    fn the_light_palette_keeps_text_readable() {
        let surfaces = [MAIN, SIDEBAR, ELEVATED, INPUT, HOVER, MAIN_HOVERED];
        for text in [TEXT, BODY, TEXT_MUTED, LINK, ACCENT, DANGER, WARNING] {
            for surface in surfaces {
                let ratio = contrast(hsla(text, true), hsla(surface, true));
                assert!(ratio >= 4.5, "{text:?} on {surface:?}: {ratio}");
            }
        }
        // Timestamps and hints: AA where they appear.
        for surface in [MAIN, SIDEBAR, INPUT, MAIN_HOVERED, APP, ELEVATED] {
            assert!(contrast(hsla(TEXT_FAINT, true), hsla(surface, true)) >= 4.5);
        }
        for button in [ACCENT, ACCENT_HOVER, DANGER, DANGER_HOVER, SUCCESS] {
            assert!(contrast(hsla(WHITE, true), hsla(button, true)) >= 4.5);
        }
        for light in [false, true] {
            for name in ["Ada", "Grace", "Linus", "Margaret", "yellow?"] {
                set_light(light);
                let ratio = contrast(name_color(name), hsla(MAIN, light));
                assert!(ratio >= 4.5, "{name} ({light}): {ratio}");
            }
        }
        set_light(false);
    }

    #[test]
    fn sender_and_syntax_colours_become_readable_and_keep_their_hue() {
        let yellow: Hsla = rgba(0xffff_00ff).into();
        let navy: Hsla = rgba(0x0000_80ff).into();
        for light in [false, true] {
            for color in [yellow, navy] {
                let shown = readable(color, MAIN, light);
                assert!(contrast(shown, hsla(MAIN, light)) >= 4.5);
                assert_eq!((shown.h, shown.s), (color.h, color.s));
            }
        }
        // Already readable: unchanged.
        assert_eq!(readable(navy, MAIN, true), navy);
        assert_eq!(readable(yellow, MAIN, false), yellow);
        // base16-ocean's plain text and comments stay distinct on light.
        let text = code_color(rgba(0xc0c5_ceff).into(), true);
        let comment = code_color(rgba(0x6573_7eff).into(), true);
        assert!(text.l < comment.l);
        assert!(contrast(comment, hsla(APP, true)) >= 4.5);
        assert_eq!(code_color(yellow, false), yellow);
    }
}
