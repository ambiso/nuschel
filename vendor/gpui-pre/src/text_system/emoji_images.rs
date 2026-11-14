//! Nuschel patch: emoji drawn from the application's own images.
//!
//! An application registers one [`EmojiImages`] source for the process. Every
//! line GPUI shapes and paints from text ([`ShapedLine`](super::ShapedLine)
//! and [`WrappedLine`](super::WrappedLine), so labels, `StyledText` and text
//! inputs alike) asks it which byte ranges of the line are emoji to draw as
//! images. Those ranges are shaped with the source's emoji font, so they get
//! an emoji's width whatever font the text asked for, and each is painted as
//! one image over the advance its glyphs were shaped with, in place of those
//! glyphs. Layout, wrapping, hit testing and selection stay those of the
//! shaped text.
//!
//! The source decides from the text, not from the font that shaped it, so a
//! character's presentation (U+FE0E text, U+FE0F emoji) is honoured whatever
//! fonts are installed. Ranges without an image, or that cannot be rasterised,
//! keep their glyphs.
use crate::{App, RenderImage, SvgSize, size};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex, OnceLock},
};

/// Emoji images an application provides; see the module documentation.
pub trait EmojiImages: Send + Sync + 'static {
    /// Appends the byte ranges of `text` to draw as images, each with the id
    /// of its image, in ascending order and disjoint. Ranges must lie on
    /// `char` boundaries within `text`.
    fn find(&self, text: &str, found: &mut Vec<(Range<usize>, u32)>);

    /// The SVG drawing of image `id`, rasterised by GPUI's SVG renderer.
    fn svg(&self, id: u32) -> Option<&'static [u8]>;

    /// The font family emoji ranges are shaped with (a colour emoji font
    /// gives them their usual width), or `None` to keep the text's fonts.
    fn font_family(&self) -> Option<&'static str> {
        None
    }
}

static SOURCE: OnceLock<&'static dyn EmojiImages> = OnceLock::new();

/// Rasterised images by (id, edge in device pixels). Never evicted, so an
/// image keeps its atlas tile; bounded, and beyond the bound text keeps the
/// font's glyphs.
const MAX_CACHED: usize = 4096;
/// The largest edge drawn as an image; larger emoji keep the font's glyphs.
const MAX_EDGE: u32 = 512;

fn cache() -> &'static Mutex<HashMap<(u32, u32), Option<Arc<RenderImage>>>> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), Option<Arc<RenderImage>>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Registers the process's emoji images. Only the first call has an effect;
/// returns whether this one did.
pub fn set_emoji_images(source: &'static dyn EmojiImages) -> bool {
    SOURCE.set(source).is_ok()
}

/// The ranges of `text` to draw as images (empty without a source).
pub(crate) fn find(text: &str, found: &mut Vec<(Range<usize>, u32)>) {
    found.clear();
    let Some(source) = SOURCE.get() else {
        return;
    };
    // Every emoji is outside ASCII; most text is not.
    if text.is_ascii() {
        return;
    }
    source.find(text, found);
    // A source must not make GPUI slice text out of bounds or paint twice.
    let mut end = 0;
    found.retain(|(range, _)| {
        let valid = range.start >= end
            && range.start < range.end
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end);
        if valid {
            end = range.end;
        }
        valid
    });
}

/// Gives the emoji ranges of `text` the source's emoji font: splits `runs`
/// (which cover `text`) at the ranges' edges. `resolve` turns a family into
/// a font id.
pub(crate) fn use_emoji_font(
    text: &str,
    runs: &mut Vec<super::FontRun>,
    resolve: impl FnOnce(&'static str) -> crate::FontId,
) {
    let Some(family) = SOURCE.get().and_then(|source| source.font_family()) else {
        return;
    };
    let mut found = Vec::new();
    find(text, &mut found);
    if found.is_empty() {
        return;
    }
    let emoji_font = resolve(family);
    let original = std::mem::take(runs);
    let mut ranges = found.iter().map(|(range, _)| range.clone()).peekable();
    let mut start = 0;
    for run in original {
        let end = start + run.len;
        let mut at = start;
        while at < end {
            // Skip emoji ranges that end before this point.
            while ranges.peek().is_some_and(|range| range.end <= at) {
                ranges.next();
            }
            let (piece_end, font_id) = match ranges.peek() {
                Some(range) if range.start <= at => (range.end.min(end), emoji_font),
                Some(range) => (range.start.min(end), run.font_id),
                None => (end, run.font_id),
            };
            runs.push(super::FontRun {
                len: piece_end - at,
                font_id,
            });
            at = piece_end;
        }
        start = end;
    }
}

/// Image `id` rasterised at `edge` device pixels square.
pub(crate) fn image(id: u32, edge: u32, cx: &App) -> Option<Arc<RenderImage>> {
    if edge == 0 || edge > MAX_EDGE {
        return None;
    }
    let mut cache = cache().lock().ok()?;
    if let Some(image) = cache.get(&(id, edge)) {
        return image.clone();
    }
    if cache.len() >= MAX_CACHED {
        return None;
    }
    let image = SOURCE
        .get()
        .and_then(|source| source.svg(id))
        .and_then(|svg| {
            let renderer = cx.svg_renderer();
            let parsed = renderer.parse_svg(svg).ok()?;
            let edge = crate::DevicePixels::from(edge);
            renderer
                .render_parsed(&parsed, SvgSize::ExactSize(size(edge, edge)))
                .ok()
        });
    cache.insert((id, edge), image.clone());
    image
}
