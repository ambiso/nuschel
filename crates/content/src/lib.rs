// SPDX-License-Identifier: EUPL-1.2
//! Run this parser in the content worker, never on the UI or audio thread.

use std::cell::OnceCell;

use base64::Engine as _;
use image::{DynamicImage, ExtendedColorType, ImageEncoder as _, ImageFormat, ImageReader, Limits};
use nuschel_core::{CodeRun, ContentBlock, EMBED, MAX_CONTENT_BYTES, MessageDocument, Span};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use thiserror::Error;

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
/// Links per message, and the longest link target kept.
pub const MAX_LINKS: usize = 128;
pub const MAX_URL_BYTES: usize = 2048;
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
                            blocks.push(ContentBlock::Text {
                                text: source[consumed..start].to_owned(),
                                spans: Vec::new(),
                            });
                        }
                        // Mumble links URLs even inside a fence: code shows text only.
                        let body = without_markers(&body);
                        let runs = self.highlight(&body, language.as_deref())?;
                        blocks.push(ContentBlock::Code { language, runs });
                        consumed = range.end;
                    }
                }
                _ => {}
            }
        }
        if consumed < source.len() || blocks.is_empty() {
            blocks.push(ContentBlock::Text {
                text: source[consumed..].to_owned(),
                spans: Vec::new(),
            });
        }
        Ok(MessageDocument {
            blocks: expand_markers(blocks),
        })
    }

    /// Legacy Mumble rich text becomes inert text before it crosses back into
    /// the UI. No image, URL, style, script, font or other resource is loaded.
    /// Inline `data:` images and http, https and mumble links are kept as
    /// markers (`EMBED` directive `EMBED`) that [`Self::render`] expands, so
    /// stored history keeps them and its visible text stays searchable.
    pub fn legacy_plain_text(&self, source: &str) -> Result<String, Error> {
        use html5ever::tendril::TendrilSink;
        use markup5ever_rcdom::{NodeData, RcDom};
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
        let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(source);
        let mut pending = vec![(dom.document.clone(), false)];
        let mut text = String::new();
        let mut links = 0;
        while let Some((node, closing)) = pending.pop() {
            match &node.data {
                NodeData::Text { contents } if !closing => {
                    text.extend(contents.borrow().chars().filter(|c| *c != EMBED));
                }
                NodeData::Element { name, attrs, .. } => {
                    let tag = name.local.as_ref();
                    if tag == "img" && !closing {
                        // Only inline data: images are kept; remote URLs are never fetched.
                        let source = attrs
                            .borrow()
                            .iter()
                            .find(|attribute| &*attribute.name.local == "src")
                            .map(|attribute| attribute.value.to_string());
                        match source.filter(|source| data_uri(source).is_some()) {
                            Some(source) => {
                                text.push(EMBED);
                                text.extend(source.chars().filter(|c| !c.is_whitespace()));
                                text.push(EMBED);
                            }
                            None => text.push_str("[image]"),
                        }
                    }
                    if matches!(tag, "script" | "style" | "head" | "iframe" | "object") {
                        continue;
                    }
                    if tag == "a" {
                        if closing {
                            marker(&mut text, "/link");
                        } else if links < MAX_LINKS
                            && let Some(url) = attrs
                                .borrow()
                                .iter()
                                .find(|attribute| &*attribute.name.local == "href")
                                .and_then(|attribute| link_target(&attribute.value))
                        {
                            links += 1;
                            marker(&mut text, &format!("link:{url}"));
                            pending.push((node.clone(), true));
                        }
                    }
                    if tag == "br" && !closing {
                        text.push('\n');
                    }
                    if matches!(
                        tag,
                        "p" | "div" | "li" | "pre" | "blockquote" | "tr" | "h1" | "h2" | "h3"
                    ) {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        if !closing {
                            pending.push((node.clone(), true));
                        }
                    }
                }
                _ => {}
            }
            if text.len() > MAX_CONTENT_BYTES {
                return Err(Error::TooLarge);
            }
            if !closing {
                pending.extend(
                    node.children
                        .borrow()
                        .iter()
                        .rev()
                        .cloned()
                        .map(|child| (child, false)),
                );
            }
        }
        Ok(text)
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

fn marker(text: &mut String, directive: &str) {
    text.push(EMBED);
    text.push_str(directive);
    text.push(EMBED);
}

/// Text with every `EMBED … EMBED` marker removed.
fn without_markers(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(EMBED) {
        text.split(EMBED).step_by(2).collect::<String>().into()
    } else {
        text.into()
    }
}

/// A link target if it may be one: http, https or mumble, without control,
/// invisible or whitespace characters, and not too long.
fn link_target(raw: &str) -> Option<String> {
    if raw.len() > 4 * MAX_URL_BYTES {
        return None;
    }
    let invisible = |c: char| {
        c.is_control()
            || c == EMBED
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    };
    let url: String = raw.chars().filter(|&c| !invisible(c)).collect();
    let url = url.trim();
    (url.len() <= MAX_URL_BYTES
        && !url.contains(char::is_whitespace)
        && nuschel_core::allowed_link(url))
    .then(|| url.to_owned())
}

/// Bare http, https and mumble URLs in `text` outside the existing spans
/// become links; `spans` stays sorted.
fn bare_links(text: &str, spans: &mut Vec<Span>, count: &mut usize) {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    for (colon, _) in text.match_indices("://") {
        if *count >= MAX_LINKS {
            break;
        }
        if colon < from {
            continue;
        }
        let Some(start) = ["https", "http", "mumble"].iter().find_map(|scheme| {
            let start = colon.checked_sub(scheme.len())?;
            bytes[start..colon]
                .eq_ignore_ascii_case(scheme.as_bytes())
                .then_some(start)
        }) else {
            continue;
        };
        // The scheme starts a word: "xhttp://" is no link.
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let mut end = text[colon..]
            .find(|c: char| {
                c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '`')
            })
            .map_or(text.len(), |length| colon + length);
        from = end;
        // Sentence punctuation after a URL is not part of it, nor is a closing
        // bracket that has no opening one inside it.
        while let Some(last) = text[start..end].chars().next_back() {
            let url = &text[start..end];
            let unbalanced = |open, close| {
                last == close && url.matches(open).count() < url.matches(close).count()
            };
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"')
                || unbalanced('(', ')')
                || unbalanced('[', ']')
            {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if spans
            .iter()
            .any(|span| span.start < end && start < span.end)
        {
            continue;
        }
        if let Some(url) = link_target(&text[start..end]) {
            *count += 1;
            found.push(Span {
                start,
                end,
                link: Some(url),
            });
        }
    }
    if !found.is_empty() {
        spans.extend(found);
        spans.sort_by_key(|span| span.start);
    }
}

/// A text block being assembled from marked-up text.
#[derive(Default)]
struct Pending {
    text: String,
    spans: Vec<Span>,
    link: Option<(usize, String)>,
}

impl Pending {
    fn close_link(&mut self) {
        if let Some((start, url)) = self.link.take()
            && start < self.text.len()
        {
            self.spans.push(Span {
                start,
                end: self.text.len(),
                link: Some(url),
            });
        }
    }

    fn flush(&mut self, out: &mut Vec<ContentBlock>, links: &mut usize) {
        self.close_link();
        let text = std::mem::take(&mut self.text);
        let mut spans = std::mem::take(&mut self.spans);
        bare_links(&text, &mut spans, links);
        out.push(ContentBlock::Text { text, spans });
    }
}

/// Expand the markers in text blocks: decoded images become blocks of their
/// own and links become spans, as do bare URLs. Unknown markers are dropped.
fn expand_markers(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut images = 0;
    let mut links = 0;
    let mut budget = MAX_DOCUMENT_PIXELS_BYTES;
    for block in blocks {
        let ContentBlock::Text { text, .. } = block else {
            out.push(block);
            continue;
        };
        let mut pending = Pending::default();
        let mut pushed = false;
        for (index, part) in text.split(EMBED).enumerate() {
            if index % 2 == 0 {
                pending.text.push_str(part);
            } else if let Some(url) = part.strip_prefix("link:") {
                pending.close_link();
                if links < MAX_LINKS
                    && let Some(url) = link_target(url)
                {
                    links += 1;
                    pending.link = Some((pending.text.len(), url));
                }
            } else if part == "/link" {
                pending.close_link();
            } else if part.starts_with("data:") {
                let decoded = (images < MAX_IMAGES)
                    .then(|| data_uri(part))
                    .flatten()
                    .and_then(|bytes| decode_image(&bytes, MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT))
                    .filter(|image| match image {
                        ContentBlock::Image { width, height, .. } => {
                            (*width * *height * 4) as usize <= budget
                        }
                        _ => false,
                    });
                match decoded {
                    Some(mut image) => {
                        if let ContentBlock::Image {
                            width,
                            height,
                            source,
                            ..
                        } = &mut image
                        {
                            budget -= (*width * *height * 4) as usize;
                            *source = Some(part.chars().filter(|c| !c.is_whitespace()).collect());
                        }
                        images += 1;
                        if !pending.text.is_empty() {
                            pending.flush(&mut out, &mut links);
                        }
                        out.push(image);
                        pushed = true;
                    }
                    None => pending.text.push_str("[image]"),
                }
            }
        }
        if !pending.text.is_empty() || !pushed {
            pending.flush(&mut out, &mut links);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            result.blocks,
            vec![ContentBlock::Text {
                text: source.to_owned(),
                spans: Vec::new(),
            }]
        );
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

    /// (visible text, [(linked text, url)]) of each text block.
    fn links(document: &MessageDocument) -> Vec<(String, Vec<(String, String)>)> {
        document
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, spans } => Some((
                    text.clone(),
                    spans
                        .iter()
                        .filter_map(|span| {
                            Some((text[span.start..span.end].to_owned(), span.link.clone()?))
                        })
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    fn html(renderer: &ContentRenderer, source: &str) -> Result<MessageDocument, Error> {
        renderer.render(&renderer.legacy_plain_text(source)?)
    }

    #[test]
    fn anchors_and_bare_urls_become_links_with_allowed_schemes_only() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let document = html(
            &renderer,
            "see <a href=\"https://example.com/a?b=1&amp;c\">the <b>docs</b></a>, \
             <a href='javascript:alert(1)'>js</a> <a href='file:///etc/passwd'>file</a> \
             <a href='data:text/html,x'>data</a> <a href=' MUMBLE://host/ch '>join</a>",
        )?;
        assert_eq!(
            links(&document),
            [(
                "see the docs, js file data join".to_owned(),
                vec![
                    (
                        "the docs".to_owned(),
                        "https://example.com/a?b=1&c".to_owned()
                    ),
                    ("join".to_owned(), "MUMBLE://host/ch".to_owned()),
                ]
            )]
        );
        // Bare URLs in plain messages (and in old history) too, without
        // trailing punctuation or an unmatched closing bracket.
        let document = renderer.render(
            "Read https://en.wikipedia.org/wiki/Rust_(language). (or http://a.example/x) \
             xhttp://no.example ftp://no.example mumble://ok.example:64738/",
        )?;
        let urls: Vec<String> = links(&document)[0]
            .1
            .iter()
            .map(|(_, url)| url.clone())
            .collect();
        assert_eq!(
            urls,
            [
                "https://en.wikipedia.org/wiki/Rust_(language)",
                "http://a.example/x",
                "mumble://ok.example:64738/"
            ]
        );
        Ok(())
    }

    #[test]
    fn hostile_links_are_capped_and_cleaned() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        // Control and bidi characters are removed from targets; whitespace
        // inside one, or a target past the length cap, makes no link.
        let huge = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        let document = html(
            &renderer,
            &format!(
                "<a href=\"https://exa\u{7}mple.com/\u{202E}x\">one</a> \
                 <a href=\"https://a b.example\">two</a> <a href=\"{huge}\">three</a>"
            ),
        )?;
        assert_eq!(
            links(&document)[0].1,
            [("one".to_owned(), "https://example.com/x".to_owned())]
        );
        // Many links: only the first MAX_LINKS are links, all text is kept.
        let many: String = (0..MAX_LINKS + 50)
            .map(|n| format!("<a href=\"https://e.example/{n}\">{n}</a> "))
            .collect();
        let document = html(&renderer, &many)?;
        let (text, found) = &links(&document)[0];
        assert_eq!(found.len(), MAX_LINKS);
        assert!(text.contains(&format!("{}", MAX_LINKS + 49)));
        // Nested and unclosed anchors and markers never yield overlapping or
        // out-of-range spans.
        for source in [
            "<a href='https://a.example'>a<a href='https://b.example'>b</a>c",
            "<a href='https://a.example'><p>para<a href='https://b.example'>x",
            "<b><a href='https://a.example'>unclosed",
            "text\u{FFFC}link:https://a.example\u{FFFC}more\u{FFFC}/link\u{FFFC}\u{FFFC}/link\u{FFFC}",
            "\u{FFFC}link:https://a.example\u{FFFC}",
        ] {
            for document in [html(&renderer, source)?, renderer.render(source)?] {
                for block in &document.blocks {
                    if let ContentBlock::Text { text, spans } = block {
                        let mut end = 0;
                        for span in spans {
                            assert!(end <= span.start && span.start < span.end, "{source}");
                            assert!(
                                text.is_char_boundary(span.start)
                                    && text.is_char_boundary(span.end)
                            );
                            end = span.end;
                        }
                        assert!(end <= text.len() && !text.contains(EMBED), "{source}");
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn linked_urls_inside_a_fence_stay_plain_code() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let document = html(
            &renderer,
            "```<br/>curl <a href=\"https://x.example/\">https://x.example/</a><br/>```",
        )?;
        let Some(ContentBlock::Code { runs, .. }) = document.blocks.first() else {
            panic!("{document:?}");
        };
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "curl https://x.example/\n"
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
                    if let ContentBlock::Text { text, spans } = block {
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
