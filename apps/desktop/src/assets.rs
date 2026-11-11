// SPDX-License-Identifier: EUPL-1.2
//! Icons are compiled into the binary; nothing is loaded from the filesystem or network.
use gpui_kit::{
    App, Asset, AssetSource, DevicePixels, Hsla, ImageCacheError, Img, Pixels, RenderImage, Result,
    SharedString, Styled, Svg, SvgRenderer, SvgSize, Window, img, px, size, svg,
};
use std::{borrow::Cow, future::Future, sync::Arc};

/// Nuschel's application icon, also installed for the desktop and converted
/// for Windows and macOS by nix/desktop-files.nix.
const APP_ICON: &[u8] = include_bytes!("../assets/nuschel.svg");

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../assets/icons/", $name, ".svg")) as &[u8])),*]
    };
}

const ICONS: &[(&str, &[u8])] = icons!(
    "alert",
    "certificate",
    "check",
    "chevron-down",
    "chevron-right",
    "cog",
    "copy",
    "ear",
    "file",
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
    "paperclip",
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

/// The application icon in colour, rasterised for the window's scale factor
/// so it stays sharp. ([`icon`] draws monochrome masks only.)
pub fn app_icon(edge: Pixels) -> Img {
    img(move |window: &mut Window, cx: &mut App| {
        let pixels = (f32::from(edge) * window.scale_factor()).round() as u32;
        window.use_asset::<AppIcon>(&pixels, cx)
    })
    .size(edge)
    .flex_shrink_0()
}

/// The icon X11 window managers show for the window. Wayland compositors and
/// GNOME take it from the desktop entry named by the window's app id.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn window_icon(cx: &App) -> Option<Arc<image::RgbaImage>> {
    const EDGE: u32 = 128;
    let rendered = render_app_icon(&cx.svg_renderer(), EDGE).ok()?;
    let mut pixels = rendered.as_bytes(0)?.to_vec();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2); // GPUI keeps images as BGRA.
    }
    image::RgbaImage::from_raw(EDGE, EDGE, pixels).map(Arc::new)
}

/// [`APP_ICON`] at an edge length in device pixels; GPUI caches each size.
enum AppIcon {}

impl Asset for AppIcon {
    type Source = u32;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(edge: u32, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move { render_app_icon(&renderer, edge) }
    }
}

fn render_app_icon(renderer: &SvgRenderer, edge: u32) -> Result<Arc<RenderImage>, ImageCacheError> {
    let edge = DevicePixels::from(edge);
    let svg = renderer.parse_svg(APP_ICON)?;
    Ok(renderer.render_parsed(&svg, SvgSize::ExactSize(size(edge, edge)))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_renders_in_colour_at_exact_sizes() {
        let renderer = SvgRenderer::new(Arc::new(Assets));
        for edge in [20, 40, 128] {
            let image = render_app_icon(&renderer, edge).unwrap();
            let pixels = DevicePixels::from(edge);
            assert_eq!(image.size(0), size(pixels, pixels));
            let bytes = image.as_bytes(0).unwrap();
            let pixel = |x: u32, y: u32| {
                let at = 4 * (y * edge + x) as usize;
                <[u8; 4]>::try_from(&bytes[at..at + 4]).unwrap()
            };
            // Transparent outside the rounded tile; the dark indigo tile
            // (BGRA: more blue than red) shows at the left edge, mid height.
            assert_eq!(pixel(0, 0)[3], 0, "{edge}");
            let [blue, _, red, alpha] = pixel(edge / 20, edge / 2);
            assert!(alpha == 255 && blue > red, "{edge}");
        }
    }

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
