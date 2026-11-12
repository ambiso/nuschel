// SPDX-License-Identifier: EUPL-1.2
//! Run this parser in the content worker, never on the UI or audio thread.

use std::cell::OnceCell;

#[cfg(feature = "images")]
use base64::Engine as _;
#[cfg(feature = "images")]
use image::{DynamicImage, ExtendedColorType, ImageEncoder as _, ImageFormat, ImageReader, Limits};
use nuschl_core::{CodeRun, ContentBlock, MAX_CONTENT_BYTES, MessageDocument};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use thiserror::Error;

pub mod markdown;
mod markup;
pub mod math;
pub use markup::{MAX_LINKS, MAX_URL_BYTES, bare_urls};

#[derive(Debug, Error)]
pub enum Error {
    #[error("message exceeds the {MAX_CONTENT_BYTES}-byte content processing limit")]
    TooLarge,
    #[error("syntax highlighting failed: {0}")]
    Highlight(#[from] syntect::Error),
    #[error("the bundled highlighting theme is missing")]
    MissingTheme,
    #[error("unsupported or invalid image")]
    InvalidImage,
    #[error("the image cannot be made small enough for this server's {0}-byte limit")]
    ImageTooLarge(usize),
}

/// Inline images are shown as thumbnails; the worker's reply must stay below
/// the IPC frame limit, so the total decoded size per document is capped.
pub const MAX_IMAGE_WIDTH: u32 = 360;
pub const MAX_IMAGE_HEIGHT: u32 = 270;
/// The image viewer's size limit (its RGBA reply must fit one worker frame).
pub const VIEW_WIDTH: u32 = 1600;
pub const VIEW_HEIGHT: u32 = 1000;
#[cfg(feature = "images")]
const MAX_IMAGES: usize = 8;
#[cfg(feature = "images")]
const MAX_DOCUMENT_PIXELS_BYTES: usize = 640 * 1024;
#[cfg(feature = "images")]
const MAX_SOURCE_DIMENSION: u32 = 4096;

/// Decode PNG/JPEG/GIF (first frame) with dimension and allocation limits and
/// return a thumbnail no larger than `max_width` × `max_height`.
#[cfg(feature = "images")]
pub fn decode_image(bytes: &[u8], max_width: u32, max_height: u32) -> Option<ContentBlock> {
    let image = decode(
        bytes,
        &[
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ],
        MAX_SOURCE_DIMENSION,
        64 * 1024 * 1024,
    )?;
    let thumbnail = shrink(image, max_width, max_height).to_rgba8();
    Some(ContentBlock::Image {
        width: thumbnail.width(),
        height: thumbnail.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(thumbnail.as_raw()),
        source: None,
    })
}

/// A server's website icon (favicon: often ICO), at most 64 × 64.
#[cfg(feature = "images")]
pub fn decode_icon(bytes: &[u8]) -> Option<ContentBlock> {
    let formats = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::Ico,
        ImageFormat::Bmp,
        ImageFormat::WebP,
    ];
    let image = decode(bytes, &formats, 1024, 16 * 1024 * 1024)?;
    let icon = shrink(image, 64, 64).to_rgba8();
    Some(ContentBlock::Image {
        width: icon.width(),
        height: icon.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(icon.as_raw()),
        source: None,
    })
}

/// Decode one of `formats` (guessed from the bytes) within the given limits.
#[cfg(feature = "images")]
fn decode(
    bytes: &[u8],
    formats: &[ImageFormat],
    max_dimension: u32,
    max_alloc: u64,
) -> Option<DynamicImage> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if !reader
        .format()
        .is_some_and(|format| formats.contains(&format))
    {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    // A GIF's screen may be 0 pixels high, and the decoder accepts it: an
    // image without pixels is no image.
    reader
        .decode()
        .ok()
        .filter(|image| image.width() > 0 && image.height() > 0)
}

/// Shrink only: `thumbnail` would also enlarge a small image.
#[cfg(feature = "images")]
fn shrink(image: DynamicImage, max_width: u32, max_height: u32) -> DynamicImage {
    if image.width() > max_width || image.height() > max_height {
        image.thumbnail(max_width.max(1), max_height.max(1))
    } else {
        image
    }
}

/// Pasted screenshots may span several monitors.
#[cfg(feature = "images")]
const MAX_PASTE_DIMENSION: u32 = 8192;
/// The composer shows an attached image at most this large (device pixels).
pub const ATTACHMENT_THUMBNAIL: u32 = 128;

/// Re-encode a pasted image as a Mumble image message (an `<img>` element of
/// at most `limit` bytes), as stock Mumble does (`Log::imageToImg`): at most
/// 1600×1000 pixels, as JPEG at the highest quality that fits, or as PNG when
/// that is smaller. When even low quality does not fit, the image shrinks.
/// Also returns a thumbnail of at most [`ATTACHMENT_THUMBNAIL`] pixels.
#[cfg(feature = "images")]
pub fn encode_image_message(bytes: &[u8], limit: usize) -> Result<(String, ContentBlock), Error> {
    let limit = limit.min(MAX_CONTENT_BYTES);
    let formats = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        // Windows hands screenshots over as device-independent bitmaps.
        ImageFormat::Bmp,
        // Sent re-encoded as JPEG or PNG, which every Mumble client shows.
        ImageFormat::WebP,
    ];
    let mut image = decode(bytes, &formats, MAX_PASTE_DIMENSION, 256 * 1024 * 1024)
        .ok_or(Error::InvalidImage)?;
    if image.width() > VIEW_WIDTH || image.height() > VIEW_HEIGHT {
        image = image.resize(
            VIEW_WIDTH,
            VIEW_HEIGHT,
            image::imageops::FilterType::Triangle,
        );
    }
    let thumbnail =
        if image.width() > ATTACHMENT_THUMBNAIL || image.height() > ATTACHMENT_THUMBNAIL {
            image.thumbnail(ATTACHMENT_THUMBNAIL, ATTACHMENT_THUMBNAIL)
        } else {
            image.clone()
        }
        .to_rgba8();
    let thumbnail = ContentBlock::Image {
        width: thumbnail.width(),
        height: thumbnail.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(thumbnail.as_raw()),
        source: None,
    };
    loop {
        let png = encode_png(&image)?;
        // JPEG has no transparency: blend it onto white.
        let opaque = flatten(&image);
        let mut smallest = png.len();
        for quality in [85, 70, 55] {
            let jpeg = encode_jpeg(&opaque, quality)?;
            let (kind, bytes) = if png.len() <= jpeg.len() {
                ("png", &png)
            } else {
                ("jpeg", &jpeg)
            };
            let element = img_element(kind, bytes);
            if element.len() <= limit {
                return Ok((element, thumbnail));
            }
            smallest = smallest.min(jpeg.len());
        }
        // Encoded size follows the pixel count: aim a little below the limit.
        // An element is about 1.44 times its image (base64, percent-encoding, lines).
        let scale = ((limit as f64 / 1.44) / smallest as f64).sqrt().min(0.85) * 0.95;
        let (width, height) = (
            (image.width() as f64 * scale) as u32,
            (image.height() as f64 * scale) as u32,
        );
        if width < 16 || height < 16 {
            return Err(Error::ImageTooLarge(limit));
        }
        image = image.resize(width, height, image::imageops::FilterType::Triangle);
    }
}

#[cfg(feature = "images")]
fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    let (width, height) = (image.width(), image.height());
    if image.color().has_alpha() {
        let pixels = image.to_rgba8();
        encoder.write_image(pixels.as_raw(), width, height, ExtendedColorType::Rgba8)
    } else {
        let pixels = image.to_rgb8();
        encoder.write_image(pixels.as_raw(), width, height, ExtendedColorType::Rgb8)
    }
    .map_err(|_| Error::InvalidImage)?;
    Ok(bytes)
}

#[cfg(feature = "images")]
fn encode_jpeg(image: &image::RgbImage, quality: u8) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|_| Error::InvalidImage)?;
    Ok(bytes)
}

#[cfg(feature = "images")]
fn flatten(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [red, green, blue, alpha] = source.0.map(u32::from);
        let blend = |channel: u32| ((channel * alpha + 255 * (255 - alpha)) / 255) as u8;
        target.0 = [blend(red), blend(green), blend(blue)];
    }
    rgb
}

/// `<img src="data:image/<kind>;base64,…" />` as stock Mumble writes it: the
/// base64 text in lines of 72 characters, each percent-encoded.
#[cfg(feature = "images")]
fn img_element(kind: &str, bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut element = String::with_capacity(encoded.len() * 9 / 8 + 48);
    element.push_str("<img src=\"data:image/");
    element.push_str(kind);
    element.push_str(";base64,");
    for (index, line) in encoded.as_bytes().chunks(72).enumerate() {
        if index > 0 {
            element.push('\n');
        }
        for &byte in line {
            match byte {
                b'+' => element.push_str("%2B"),
                b'/' => element.push_str("%2F"),
                b'=' => element.push_str("%3D"),
                other => element.push(char::from(other)),
            }
        }
    }
    element.push_str("\" />");
    element
}

/// Decode an inline `data:` image for the viewer, up to `VIEW_WIDTH` × `VIEW_HEIGHT`.
#[cfg(feature = "images")]
pub fn decode_view(uri: &str) -> Option<ContentBlock> {
    decode_image(&data_uri(uri)?, VIEW_WIDTH, VIEW_HEIGHT)
}

/// Parse `data:image/<type>;base64,<payload>`; nothing else is accepted.
#[cfg(feature = "images")]
fn data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.trim().strip_prefix("data:image/")?;
    let (kind, payload) = rest.split_once(";base64,")?;
    if !matches!(
        kind.to_ascii_lowercase().as_str(),
        "png" | "jpeg" | "jpg" | "gif" | "webp"
    ) {
        return None;
    }
    let payload: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    if payload.len() > MAX_CONTENT_BYTES {
        return None;
    }
    // Stock Mumble percent-encodes the base64 text (`+` as %2B, `/` as %2F,
    // `=` as %3D) and wraps it every 72 characters (Log::imageToImg).
    base64::engine::general_purpose::STANDARD
        .decode(percent_decode(&payload)?)
        .ok()
}

/// `%XX` escapes to bytes; anything malformed rejects the payload.
#[cfg(feature = "images")]
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Some(out)
}

/// Loading syntect's syntaxes and theme costs most of a worker's start-up, so
/// it happens on the first code block rather than for every message.
#[derive(Default)]
pub struct ContentRenderer {
    highlighter: OnceCell<Highlighter>,
}

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl ContentRenderer {
    /// Cannot fail since the highlighter loads lazily; it keeps its signature.
    pub fn new() -> Result<Self, Error> {
        Ok(Self::default())
    }

    fn highlighter(&self) -> Result<&Highlighter, Error> {
        if let Some(highlighter) = self.highlighter.get() {
            return Ok(highlighter);
        }
        let theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .ok_or(Error::MissingTheme)?;
        Ok(self.highlighter.get_or_init(|| Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
        }))
    }

    /// Message text is Markdown (docs/markdown-rendering-plan.md), parsed
    /// once with the formulas of docs/math.md: CommonMark's emphasis, code,
    /// links to allowed targets, headings, lists, quotes, rules and code
    /// blocks, and `~~strikethrough~~`, with their syntax hidden. Every line
    /// break and blank line is kept. Raw HTML, images, reference links and
    /// entity references stay as typed, and nothing is fetched. Legacy
    /// marked-up text (`legacy_plain_text`) gets the same on its visible text,
    /// its directives left as they are. A file offer is shown as typed.
    pub fn render(&self, source: &str) -> Result<MessageDocument, Error> {
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
        let marked = markdown::Marked::new(source);
        if markdown::literal(&marked.visible) {
            return Ok(MessageDocument {
                blocks: markup::expand(markup::marked_pieces(source), 0),
            });
        }
        let mut translation = markdown::translate(&marked);
        let mut made = Vec::with_capacity(translation.blocks.len());
        let mut fences = 0;
        for (_, block, _) in &mut translation.blocks {
            made.push(match block {
                markdown::Block::Math { source, tex } => {
                    fences += 1;
                    ContentBlock::Math {
                        source: std::mem::take(source),
                        tex: std::mem::take(tex),
                    }
                }
                markdown::Block::Code { language, body } => {
                    let runs = self.highlight(body, language.as_deref())?;
                    ContentBlock::Code {
                        language: language.take(),
                        runs,
                    }
                }
            });
        }
        let pieces = markdown::pieces(&marked, translation, made);
        Ok(MessageDocument {
            blocks: markup::expand(pieces, math::MAX_FORMULAS.saturating_sub(fences)),
        })
    }

    /// Legacy Mumble rich text becomes inert, marked-up text before it crosses
    /// back into the UI; no image, URL, style, script, font or other resource
    /// is loaded. See [`markup`] for what is kept and how.
    pub fn legacy_plain_text(&self, source: &str) -> Result<String, Error> {
        markup::legacy_text(source)
    }

    fn highlight(&self, source: &str, language: Option<&str>) -> Result<Vec<CodeRun>, Error> {
        let Highlighter { syntaxes, theme } = self.highlighter()?;
        let syntax = language
            .and_then(|token| syntaxes.find_syntax_by_token(token))
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut runs = Vec::new();
        for line in LinesWithEndings::from(source) {
            for (style, text) in highlighter.highlight_line(line, syntaxes)? {
                runs.push(CodeRun {
                    text: text.to_owned(),
                    foreground: [
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                        style.foreground.a,
                    ],
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "images"))]
    #[test]
    fn text_only_rendering_refuses_embedded_images_but_keeps_code() {
        let renderer = super::ContentRenderer::default();
        let html = "one<br>two<img src='data:image/png;base64,iVBORw0KGgo='><img src='https://tracker.invalid/x'>";
        let source = renderer.legacy_plain_text(html).unwrap();
        let document = renderer.render(&source).unwrap();
        assert!(
            document
                .blocks
                .iter()
                .all(|block| !matches!(block, nuschl_core::ContentBlock::Image { .. }))
        );
        assert!(document.blocks.iter().any(|block| matches!(block, nuschl_core::ContentBlock::Text { text, .. } if text.contains("one\ntwo") && text.contains("[image]"))));
        let code = renderer.render("```rust\nfn main() {}\n```").unwrap();
        assert!(
            matches!(code.blocks.first(), Some(nuschl_core::ContentBlock::Code { runs, .. }) if !runs.is_empty())
        );
    }

    /// A file offer sent as HTML (servers that allow it) is recognised from the
    /// document the worker renders, though its prepared source keeps markers.
    #[test]
    fn html_file_offers_are_recognised_after_rendering() -> Result<(), super::Error> {
        use nuschl_core::transfer::{Offer, format_offer, offer_in};
        let renderer = super::ContentRenderer::new()?;
        let offer = Offer {
            name: "frost.pdf".into(),
            size: 5_000_000,
            code: Some("2-disruptive-crumpled".into()),
            rendezvous: None,
        };
        let html = format_offer(&offer).html;
        let document = renderer.render(&renderer.legacy_plain_text(&html)?)?;
        assert_eq!(offer_in(&document), Some(offer.clone()));
        // The end of the offer names the same file, so it replaces the card.
        let ended = Offer {
            code: None,
            ..offer
        };
        let html = format_offer(&ended).html;
        let source = renderer.legacy_plain_text(&html)?;
        assert_eq!(nuschl_core::transfer::parse_offer(&source), None);
        assert_eq!(offer_in(&renderer.render(&source)?), Some(ended));
        Ok(())
    }

    use super::*;

    #[test]
    #[cfg(feature = "images")]
    fn webp_images_decode_and_are_sent_as_jpeg_or_png() {
        let mut webp = Vec::new();
        let pixels =
            image::RgbaImage::from_fn(40, 30, |x, y| image::Rgba([x as u8, y as u8, 90, 255]));
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut std::io::Cursor::new(&mut webp), ImageFormat::WebP)
            .unwrap();
        let Some(ContentBlock::Image { width, height, .. }) = decode_image(&webp, 640, 480) else {
            panic!("the WebP image did not decode");
        };
        assert_eq!((width, height), (40, 30));
        let (html, _) = encode_image_message(&webp, 128 * 1024).unwrap();
        assert!(
            html.contains("data:image/jpeg") || html.contains("data:image/png"),
            "{}",
            &html[..html.len().min(80)]
        );
    }
    use nuschl_core::EMBED;

    #[test]
    #[cfg(feature = "images")]
    fn favicons_decode_from_ico() {
        let mut ico = Vec::new();
        let pixels = vec![200u8; 128 * 128 * 4];
        image::codecs::ico::IcoEncoder::new(&mut ico)
            .write_image(&pixels, 128, 128, ExtendedColorType::Rgba8)
            .unwrap();
        let Some(ContentBlock::Image { width, height, .. }) = decode_icon(&ico) else {
            panic!("the icon did not decode");
        };
        assert_eq!((width, height), (64, 64));
        assert!(decode_icon(b"<!doctype html>").is_none());
    }

    #[test]
    fn legacy_html_is_inert_and_preserves_explicit_line_breaks() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        assert_eq!(renderer.legacy_plain_text("<p>one<br/><br/>two &amp; &lt;tag&gt;</p><script>secret()</script><img src='https://example.invalid/a'>")?, "one\n\ntwo & <tag>\n[image]");
        let source = "```rust<br/>fn main() {}<br/>```";
        assert!(matches!(
            renderer
                .render(&renderer.legacy_plain_text(source)?)?
                .blocks
                .first(),
            Some(ContentBlock::Code { .. })
        ));
        Ok(())
    }

    #[cfg(feature = "images")]
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([x as u8, y as u8, 128, 255])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    #[cfg(feature = "images")]
    fn inline_data_images_are_decoded_and_remote_images_ignored() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png(800, 400))
        );
        let html = format!(
            "look<br/><img src=\"{uri}\"/><img src=\"https://tracker.invalid/x.png\"/>done{}",
            EMBED
        );
        let text = renderer.legacy_plain_text(&html)?;
        let document = renderer.render(&text)?;
        let kinds: Vec<&str> = document
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { .. } => "text",
                ContentBlock::Code { .. } => "code",
                ContentBlock::Image { .. } => "image",
                ContentBlock::Math { .. } => "math",
                ContentBlock::Break { .. } => "break",
            })
            .collect();
        assert_eq!(kinds, ["text", "image", "text"]);
        let Some(ContentBlock::Image { width, height, .. }) = document.blocks.get(1) else {
            panic!()
        };
        assert_eq!(
            (*width, *height),
            (360, 180),
            "thumbnail keeps the aspect ratio"
        );
        let Some(ContentBlock::Text { text, .. }) = document.blocks.get(2) else {
            panic!()
        };
        assert_eq!(
            text, "[image]done",
            "remote image is a placeholder; stray markers are stripped"
        );
        Ok(())
    }

    #[test]
    #[cfg(feature = "images")]
    fn images_pasted_in_stock_mumble_are_decoded() -> Result<(), Error> {
        // Log::imageToImg: base64, cut into 72-character lines, each
        // percent-encoded, joined with newlines.
        let encoded = base64::engine::general_purpose::STANDARD.encode(png(40, 30));
        let lines: Vec<String> = encoded
            .as_bytes()
            .chunks(72)
            .map(|line| {
                line.iter()
                    .map(|&byte| match byte {
                        b'+' => "%2B".to_string(),
                        b'/' => "%2F".to_string(),
                        b'=' => "%3D".to_string(),
                        other => (other as char).to_string(),
                    })
                    .collect()
            })
            .collect();
        assert!(
            lines.concat().contains('%'),
            "the fixture must exercise percent-encoding"
        );
        let html = format!(
            "<p><img src=\"data:image/PNG;base64,{}\" /></p>",
            lines.join("\n")
        );
        let renderer = ContentRenderer::new()?;
        let document = renderer.render(&renderer.legacy_plain_text(&html)?)?;
        assert!(
            document
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "{:?}",
            document
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Image { .. } => "image".to_string(),
                    ContentBlock::Text { text, .. } => text.chars().take(40).collect(),
                    _ => "other".into(),
                })
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    /// A screenshot-like image: flat areas with some text-like detail, or noise.
    #[cfg(feature = "images")]
    fn picture(width: u32, height: u32, noisy: bool) -> Vec<u8> {
        let mut state = 0x9e37_79b9_u32;
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if noisy {
                image::Rgb([
                    (x / 3) as u8 ^ state as u8,
                    (y / 2) as u8,
                    (state >> 8) as u8,
                ])
            } else if (x / 7 + y / 11) % 5 == 0 {
                image::Rgb([20, 20, 30])
            } else {
                image::Rgb([235, 235, 240])
            }
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// What a receiver (including stock Mumble's format) makes of an element.
    #[cfg(feature = "images")]
    fn received(element: &str) -> (u32, u32) {
        let renderer = ContentRenderer::default();
        let document = renderer
            .render(&renderer.legacy_plain_text(element).unwrap())
            .unwrap();
        let [ContentBlock::Image { source, .. }] = document.blocks.as_slice() else {
            panic!("{:?}", document.blocks.len())
        };
        let Some(ContentBlock::Image { width, height, .. }) = decode_view(source.as_ref().unwrap())
        else {
            panic!("the viewer cannot decode it")
        };
        (width, height)
    }

    #[test]
    #[cfg(feature = "images")]
    fn pasted_images_are_encoded_as_stock_mumble_does_within_the_limit() {
        let limit = 131_072;
        // A large, detailed picture: shrunk to 1600×1000, then by quality or size.
        let (element, thumbnail) = encode_image_message(&picture(2400, 1350, true), limit).unwrap();
        assert!(element.len() <= limit, "{}", element.len());
        // With a thumbnail for the composer.
        let ContentBlock::Image {
            width: 128,
            height: 72,
            rgba,
            ..
        } = thumbnail
        else {
            panic!("a 128×72 thumbnail")
        };
        let pixels = base64::engine::general_purpose::STANDARD.decode(rgba);
        assert_eq!(pixels.map(|pixels| pixels.len()).ok(), Some(128 * 72 * 4));
        assert!(element.starts_with("<img src=\"data:image/jpeg;base64,"));
        assert!(element.ends_with("\" />"));
        let payload = &element["<img src=\"data:image/jpeg;base64,".len()..element.len() - 4];
        assert!(
            payload.contains('%'),
            "percent-encoded like Log::imageToImg"
        );
        let lines: Vec<&str> = payload.split('\n').collect();
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| {
            line.replace("%2B", "+")
                .replace("%2F", "/")
                .replace("%3D", "=")
                .len()
                <= 72
        }));
        let (width, height) = received(&element);
        assert!(width <= 1600 && height <= 1000, "{width}×{height}");
        assert!(
            (width * 9).abs_diff(height * 16) <= 16,
            "keeps 16:9: {width}×{height}"
        );

        // Flat screenshots stay lossless: PNG is smaller than JPEG there.
        let (element, _) = encode_image_message(&picture(800, 500, false), limit).unwrap();
        assert!(
            element.starts_with("<img src=\"data:image/png;base64,"),
            "{}",
            &element[..40]
        );
        assert_eq!(received(&element), (800, 500));

        // A small limit shrinks the picture until it fits.
        let (element, _) = encode_image_message(&picture(1600, 1000, true), 20_000).unwrap();
        assert!(element.len() <= 20_000);
        let (width, _) = received(&element);
        assert!(width < 1600, "{width}");
    }

    #[test]
    #[cfg(feature = "images")]
    fn pasted_images_that_cannot_fit_or_decode_are_rejected() {
        assert!(matches!(
            encode_image_message(&picture(64, 64, true), 40),
            Err(Error::ImageTooLarge(40))
        ));
        assert!(matches!(
            encode_image_message(b"not an image", 131_072),
            Err(Error::InvalidImage)
        ));
        assert!(matches!(
            encode_image_message(&picture(64, 64, false)[..40], 131_072),
            Err(Error::InvalidImage)
        ));
        assert!(matches!(
            encode_image_message(b"<svg xmlns='http://www.w3.org/2000/svg'/>", 131_072),
            Err(Error::InvalidImage)
        ));
    }

    #[test]
    fn plain_messages_never_load_the_highlighter() {
        let renderer = ContentRenderer::default();
        renderer.render("no code here").unwrap();
        renderer.legacy_plain_text("<b>nor here</b>").unwrap();
        assert!(renderer.highlighter.get().is_none());
        renderer.render("```\ncode\n```").unwrap();
        assert!(renderer.highlighter.get().is_some());
    }

    #[test]
    #[cfg(feature = "images")]
    fn hostile_image_payloads_are_rejected() {
        assert!(decode_image(b"not an image", 10, 10).is_none());
        assert!(
            decode_image(&png(1, 1)[..20], 10, 10).is_none(),
            "truncated"
        );
        assert!(
            data_uri("data:image/svg+xml;base64,PHN2Zz4=").is_none(),
            "SVG is not accepted"
        );
        assert!(data_uri("https://example.invalid/a.png").is_none());
    }

    #[test]
    #[cfg(feature = "images")]
    fn images_without_pixels_are_rejected() {
        // Found by fuzzing: a GIF whose screen is 110 × 0 pixels became an
        // image block of that size without any pixels.
        let gif = base64::engine::general_purpose::STANDARD
            .decode("R0lGODlhbgAAAAAAACwBAAACAQAEAIB4RwAAACwBiVA=")
            .unwrap();
        assert!(decode_image(&gif, 640, 480).is_none());
        assert!(decode_icon(&gif).is_none());
        let uri = format!(
            "data:image/gif;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&gif)
        );
        assert!(decode_view(&uri).is_none());
        assert!(matches!(
            encode_image_message(&gif, 131_072),
            Err(Error::InvalidImage)
        ));
    }

    #[test]
    fn literal_messages_keep_blank_lines_and_html_inert() -> Result<(), Error> {
        let source = "first\n\nsecond\n<script>never execute</script>";
        let result = ContentRenderer::new()?.render(source)?;
        assert_eq!(result.blocks, vec![ContentBlock::text(source)]);
        Ok(())
    }

    #[test]
    fn a_leading_line_break_before_a_fence_is_dropped() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let plain = renderer.legacy_plain_text("<br/>```rust<br/>fn main() {}<br/>```")?;
        let document = renderer.render(&plain)?;
        assert!(
            matches!(document.blocks.first(), Some(ContentBlock::Code { .. })),
            "{document:?}"
        );
        Ok(())
    }

    #[test]
    fn rust_code_is_highlighted_and_retains_its_newlines() -> Result<(), Error> {
        let result = ContentRenderer::new()?.render("before\n```rust\nfn main() {}\n```\nafter")?;
        let Some(ContentBlock::Code { language, runs }) = result.blocks.get(1) else {
            panic!("expected a code block between the two text blocks");
        };
        assert_eq!(language.as_deref(), Some("rust"));
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "fn main() {}\n"
        );
        assert!(
            runs.windows(2)
                .any(|pair| pair[0].foreground != pair[1].foreground)
        );
        Ok(())
    }

    #[test]
    fn code_blocks_count_towards_the_block_limit() -> Result<(), Error> {
        // 256 KiB of fences were 32,766 code blocks, each laid out by the
        // desktop; blocks are capped at 512 per message.
        let source = "```\n".repeat(MAX_CONTENT_BYTES / 4);
        let document = ContentRenderer::new()?.render(&source)?;
        let code = |block: &&ContentBlock| matches!(block, ContentBlock::Code { .. });
        assert!(document.blocks.iter().filter(code).count() > 200);
        assert!(document.blocks.len() <= 513, "{}", document.blocks.len());
        Ok(())
    }

    #[test]
    fn rejects_large_messages_before_parsing() -> Result<(), Error> {
        assert!(matches!(
            ContentRenderer::new()?.render(&"x".repeat(MAX_CONTENT_BYTES + 1)),
            Err(Error::TooLarge)
        ));
        Ok(())
    }

    #[test]
    fn hostile_markup_and_images_never_panic() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let fragments = [
            "<",
            ">",
            "<img src=\"data:image/png;base64,",
            "\"/>",
            "```",
            "rust\n",
            "<br/>",
            "&amp;",
            "&#x0;",
            "<script>",
            "</p>",
            "<table><tr><td>",
            "\u{FFFC}",
            "data:image/gif;base64,R0lGODlh",
            "AAAA",
            "\n\n",
            "<b><i><u>",
            "<a href='javascript:x'>",
            "~~~",
            "`",
            "\u{202E}",
            "<!--",
            "-->",
            "<![CDATA[",
            "<a href=\"https://x.example/(a)\">",
            "</a>",
            "https://",
            "mumble://h",
            ").",
            "\u{FFFC}link:https://y.example\u{FFFC}",
            "\u{FFFC}/link\u{FFFC}",
            "<ul><li>",
            "</li>",
            "<ol start='9'>",
            "<blockquote>",
            "<pre>",
            "</pre>",
            "<h1>",
            "<hr>",
            "<th>",
            "<font color='#abc'>",
            "<span style='color:rgb(1,2,3);font-weight:700;text-decoration:underline line-through'>",
            "\u{FFFC}item:9:\u{FFFC}",
            "\u{FFFC}quote\u{FFFC}",
            "\u{FFFC}h2\u{FFFC}",
        ];
        let mut state = 99_u64;
        for _ in 0..3_000 {
            let mut text = String::new();
            for _ in 0..(state % 24) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                text.push_str(fragments[(state % fragments.len() as u64) as usize]);
            }
            let plain = renderer.legacy_plain_text(&text)?;
            for document in [renderer.render(&plain)?, renderer.render(&text)?] {
                for block in document.blocks {
                    if let ContentBlock::Text { text, spans, .. } = block {
                        let mut end = 0;
                        for span in spans {
                            assert!(end <= span.start && span.start < span.end);
                            assert!(text.is_char_boundary(span.start));
                            end = span.end;
                        }
                        assert!(end <= text.len() && text.is_char_boundary(end));
                    }
                }
            }
        }
        // Image decoders reject arbitrary bytes behind valid magic numbers.
        #[cfg(feature = "images")]
        for magic in [&b"\x89PNG\r\n\x1a\n"[..], b"\xff\xd8\xff", b"GIF89a"] {
            for length in [0, 1, 7, 64, 512] {
                let mut bytes = magic.to_vec();
                bytes.extend((0..length).map(|index| (index * 37 % 251) as u8));
                assert!(decode_image(&bytes, 64, 64).is_none());
            }
        }
        Ok(())
    }
}
