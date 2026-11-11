// SPDX-License-Identifier: EUPL-1.2
//! Run this parser in the content worker, never on the UI or audio thread.

use std::cell::OnceCell;

use base64::Engine as _;
use image::{DynamicImage, ExtendedColorType, ImageEncoder as _, ImageFormat, ImageReader, Limits};
use nuschel_core::{CodeRun, ContentBlock, MAX_CONTENT_BYTES, MessageDocument};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use thiserror::Error;

mod markup;
pub use markup::{MAX_LINKS, MAX_URL_BYTES};

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
const MAX_IMAGES: usize = 8;
const MAX_DOCUMENT_PIXELS_BYTES: usize = 640 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 4096;

/// Decode PNG/JPEG/GIF (first frame) with dimension and allocation limits and
/// return a thumbnail no larger than `max_width` × `max_height`.
pub fn decode_image(bytes: &[u8], max_width: u32, max_height: u32) -> Option<ContentBlock> {
    let image = decode(
        bytes,
        &[ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif],
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
pub fn decode_icon(bytes: &[u8]) -> Option<ContentBlock> {
    let formats = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::Ico,
        ImageFormat::Bmp,
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
    reader.decode().ok()
}

/// Shrink only: `thumbnail` would also enlarge a small image.
fn shrink(image: DynamicImage, max_width: u32, max_height: u32) -> DynamicImage {
    if image.width() > max_width || image.height() > max_height {
        image.thumbnail(max_width.max(1), max_height.max(1))
    } else {
        image
    }
}

/// Pasted screenshots may span several monitors.
const MAX_PASTE_DIMENSION: u32 = 8192;

/// Re-encode a pasted image as a Mumble image message (an `<img>` element of
/// at most `limit` bytes), as stock Mumble does (`Log::imageToImg`): at most
/// 1600×1000 pixels, as JPEG at the highest quality that fits, or as PNG when
/// that is smaller. When even low quality does not fit, the image shrinks.
pub fn encode_image_message(bytes: &[u8], limit: usize) -> Result<String, Error> {
    let limit = limit.min(MAX_CONTENT_BYTES);
    let formats = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        // Windows hands screenshots over as device-independent bitmaps.
        ImageFormat::Bmp,
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
                return Ok(element);
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
pub fn decode_view(uri: &str) -> Option<ContentBlock> {
    decode_image(&data_uri(uri)?, VIEW_WIDTH, VIEW_HEIGHT)
}

/// Parse `data:image/<type>;base64,<payload>`; nothing else is accepted.
fn data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.trim().strip_prefix("data:image/")?;
    let (kind, payload) = rest.split_once(";base64,")?;
    if !matches!(
        kind.to_ascii_lowercase().as_str(),
        "png" | "jpeg" | "jpg" | "gif"
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

    /// Ordinary message text is literal, including all newlines and blank lines.
    /// Only fenced code blocks are interpreted. HTML and embedded resources stay inert text.
    pub fn render(&self, source: &str) -> Result<MessageDocument, Error> {
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
        // Leading line breaks carry no content; messages that open with a code
        // fence are sent with one so stock Mumble starts the fence on its own line.
        let source = source.trim_start_matches(['\n', '\r']);
        let mut blocks = Vec::new();
        let mut consumed = 0;
        let mut code: Option<(usize, Option<String>, String)> = None;
        for (event, range) in Parser::new(source).into_offset_iter() {
            match event {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                    let language = info
                        .split_whitespace()
                        .next()
                        .map(|value| value.chars().take(64).collect());
                    code = Some((range.start, language, String::new()));
                }
                Event::Text(text) if code.is_some() => {
                    if let Some((_, _, body)) = code.as_mut() {
                        body.push_str(&text);
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    if let Some((start, language, body)) = code.take() {
                        if start > consumed {
                            blocks.push(ContentBlock::text(&source[consumed..start]));
                        }
                        // Mumble links URLs even inside a fence: code shows text only.
                        let body = markup::without_markers(&body);
                        let runs = self.highlight(&body, language.as_deref())?;
                        blocks.push(ContentBlock::Code { language, runs });
                        consumed = range.end;
                    }
                }
                _ => {}
            }
        }
        if consumed < source.len() || blocks.is_empty() {
            blocks.push(ContentBlock::text(&source[consumed..]));
        }
        Ok(MessageDocument {
            blocks: markup::expand(blocks),
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
    use super::*;
    use nuschel_core::EMBED;

    #[test]
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
    fn pasted_images_are_encoded_as_stock_mumble_does_within_the_limit() {
        let limit = 131_072;
        // A large, detailed picture: shrunk to 1600×1000, then by quality or size.
        let element = encode_image_message(&picture(2400, 1350, true), limit).unwrap();
        assert!(element.len() <= limit, "{}", element.len());
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
        let element = encode_image_message(&picture(800, 500, false), limit).unwrap();
        assert!(
            element.starts_with("<img src=\"data:image/png;base64,"),
            "{}",
            &element[..40]
        );
        assert_eq!(received(&element), (800, 500));

        // A small limit shrinks the picture until it fits.
        let element = encode_image_message(&picture(1600, 1000, true), 20_000).unwrap();
        assert!(element.len() <= 20_000);
        let (width, _) = received(&element);
        assert!(width < 1600, "{width}");
    }

    #[test]
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
