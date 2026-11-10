// SPDX-License-Identifier: EUPL-1.2
//! One dark palette for the whole shell, so panels read as a single system.
use gpui_kit::{Hsla, Rgba, rgb, rgba};

pub const APP: u32 = 0x0f1218;
pub const RAIL: u32 = 0x0f1218;
pub const SIDEBAR: u32 = 0x161a22;
pub const SIDEBAR_FOOTER: u32 = 0x12151c;
pub const MAIN: u32 = 0x1c212b;
/// MAIN under the message row's hover tint (black at 0x18 alpha).
pub const MAIN_HOVERED: u32 = 0x191e27;
pub const ELEVATED: u32 = 0x262c38;
pub const INPUT: u32 = 0x232935;
pub const HOVER: u32 = 0x232a36;
pub const SELECTED: u32 = 0x2c3443;
pub const BORDER: u32 = 0x262d3a;
pub const BORDER_STRONG: u32 = 0x353e4e;

pub const TEXT: u32 = 0xe8ebf2;
pub const TEXT_MUTED: u32 = 0x9aa4b7;
pub const TEXT_FAINT: u32 = 0x6c778c;

pub const ACCENT: u32 = 0x5b7cfa;
pub const ACCENT_HOVER: u32 = 0x6f8cff;
pub const SUCCESS: u32 = 0x3ccf8e;
pub const WARNING: u32 = 0xf0b232;
pub const DANGER: u32 = 0xf2545b;
pub const DANGER_HOVER: u32 = 0xff6b72;

pub fn color(value: u32) -> Rgba {
    rgb(value)
}

pub fn tint(value: u32, alpha: u8) -> Rgba {
    rgba((value << 8) | u32::from(alpha))
}

fn hue(name: &str) -> f32 {
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    (hash % 360) as f32 / 360.
}

/// Stable, readable avatar colours derived from a display name.
pub fn avatar(name: &str) -> Hsla {
    gpui_kit::hsla(hue(name), 0.45, 0.42, 1.)
}

/// The same hue, light enough for author names on the dark timeline.
pub fn name_color(name: &str) -> Hsla {
    gpui_kit::hsla(hue(name), 0.55, 0.74, 1.)
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
    #[test]
    fn initials_handle_unicode_and_single_words() {
        assert_eq!(super::initials("ada lovelace"), "AL");
        assert_eq!(super::initials("Ölf"), "ÖL");
        assert_eq!(super::initials(""), "?");
    }
}
