// SPDX-License-Identifier: EUPL-1.2
//! Icons are compiled into the binary; nothing is loaded from the filesystem or network.
use gpui_kit::{AssetSource, Hsla, Result, SharedString, Styled, Svg, px, svg};
use std::borrow::Cow;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../assets/icons/", $name, ".svg")) as &[u8])),*]
    };
}

const ICONS: &[(&str, &[u8])] = icons!(
    "alert",
    "certificate",
    "chevron-down",
    "chevron-right",
    "ear",
    "hang-up",
    "hash",
    "headphones",
    "headphones-off",
    "info",
    "keyboard",
    "link",
    "lock",
    "message",
    "mic",
    "mic-off",
    "more",
    "pencil",
    "plus",
    "record",
    "search",
    "send",
    "settings",
    "shield",
    "signal",
    "star",
    "unlock",
    "user",
    "volume",
    "whisper",
    "window-close",
    "window-maximize",
    "window-minimize",
    "window-restore",
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(path
            .strip_prefix("icons/")
            .and_then(|name| name.strip_suffix(".svg"))
            .and_then(|name| ICONS.iter().find(|(icon, _)| *icon == name))
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(if path.trim_end_matches('/') == "icons" {
            ICONS
                .iter()
                .map(|(name, _)| format!("icons/{name}.svg").into())
                .collect()
        } else {
            Vec::new()
        })
    }
}

/// A monochrome icon. GPUI paints SVGs only with their own text colour, which
/// is not inherited from the parent, so callers always choose one.
pub fn icon(name: &'static str, size: f32, color: impl Into<Hsla>) -> Svg {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(size))
        .flex_shrink_0()
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_icon_is_a_small_svg() {
        for (name, bytes) in ICONS {
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(text.starts_with("<svg") && bytes.len() < 4096, "{name}");
            assert!(Assets.load(&format!("icons/{name}.svg")).unwrap().is_some());
        }
        assert!(Assets.load("icons/../secret").unwrap().is_none());
    }
}
