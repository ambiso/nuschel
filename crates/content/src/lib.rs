// SPDX-License-Identifier: EUPL-1.2
//! Run this parser in the content worker, never on the UI or audio thread.

use base64::Engine as _;
use nuschel_core::{CodeRun, ContentBlock, EMBED, MAX_CONTENT_BYTES, MessageDocument};
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
}

/// Inline images are shown as thumbnails; the worker's reply must stay below
/// the IPC frame limit, so the total decoded size per document is capped.
pub const MAX_IMAGE_WIDTH: u32 = 360;
pub const MAX_IMAGE_HEIGHT: u32 = 270;
const MAX_IMAGES: usize = 8;
const MAX_DOCUMENT_PIXELS_BYTES: usize = 640 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 4096;

/// Decode PNG/JPEG/GIF (first frame) with dimension and allocation limits and
/// return a thumbnail no larger than `max_width` × `max_height`.
pub fn decode_image(bytes: &[u8], max_width: u32, max_height: u32) -> Option<ContentBlock> {
    use image::{ImageReader, Limits};
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Gif)
    ) {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let thumbnail = image
        .thumbnail(max_width.max(1), max_height.max(1))
        .to_rgba8();
    Some(ContentBlock::Image {
        width: thumbnail.width(),
        height: thumbnail.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(thumbnail.as_raw()),
    })
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
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()
}

pub struct ContentRenderer {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl ContentRenderer {
    pub fn new() -> Result<Self, Error> {
        let mut themes = ThemeSet::load_defaults();
        Ok(Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes
                .themes
                .remove("base16-ocean.dark")
                .ok_or(Error::MissingTheme)?,
        })
    }

    /// Ordinary message text is literal, including all newlines and blank lines.
    /// Only fenced code blocks are interpreted. HTML and embedded resources stay inert text.
    pub fn render(&self, source: &str) -> Result<MessageDocument, Error> {
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
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
                            });
                        }
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
            });
        }
        Ok(MessageDocument {
            blocks: expand_embeds(blocks),
        })
    }

    /// Legacy Mumble rich text becomes inert text before it crosses back into
    /// the UI. No image, URL, style, script, font or other resource is loaded.
    pub fn legacy_plain_text(&self, source: &str) -> Result<String, Error> {
        use html5ever::tendril::TendrilSink;
        use markup5ever_rcdom::{NodeData, RcDom};
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
        let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(source);
        let mut pending = vec![(dom.document.clone(), false)];
        let mut text = String::new();
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
        let syntax = language
            .and_then(|token| self.syntaxes.find_syntax_by_token(token))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut runs = Vec::new();
        for line in LinesWithEndings::from(source) {
            for (style, text) in highlighter.highlight_line(line, &self.syntaxes)? {
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

/// Replace `EMBED data:… EMBED` spans in text blocks with decoded images.
fn expand_embeds(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut images = 0;
    let mut budget = MAX_DOCUMENT_PIXELS_BYTES;
    for block in blocks {
        let ContentBlock::Text { text } = block else {
            out.push(block);
            continue;
        };
        if !text.contains(EMBED) {
            out.push(ContentBlock::Text { text });
            continue;
        }
        let mut parts = text.split(EMBED);
        let mut pending = parts.next().unwrap_or_default().to_string();
        while let Some(embed) = parts.next() {
            let tail = parts.next();
            let decoded = (images < MAX_IMAGES)
                .then(|| data_uri(embed))
                .flatten()
                .and_then(|bytes| decode_image(&bytes, MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT))
                .filter(|image| match image {
                    ContentBlock::Image { width, height, .. } => {
                        (*width * *height * 4) as usize <= budget
                    }
                    _ => false,
                });
            match decoded {
                Some(image) => {
                    if let ContentBlock::Image { width, height, .. } = &image {
                        budget -= (*width * *height * 4) as usize;
                    }
                    images += 1;
                    if !pending.is_empty() {
                        out.push(ContentBlock::Text {
                            text: std::mem::take(&mut pending),
                        });
                    }
                    out.push(image);
                }
                None => pending.push_str("[image]"),
            }
            if let Some(tail) = tail {
                pending.push_str(tail);
            }
        }
        if !pending.is_empty() {
            out.push(ContentBlock::Text { text: pending });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let Some(ContentBlock::Text { text }) = document.blocks.get(2) else {
            panic!()
        };
        assert_eq!(
            text, "[image]done",
            "remote image is a placeholder; stray markers are stripped"
        );
        Ok(())
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
                text: source.to_owned()
            }]
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
}
