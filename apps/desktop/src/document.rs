// SPDX-License-Identifier: EUPL-1.2
//! Worker documents converted once into renderer-ready values: shared text
//! with its styles and links, code with its highlight ranges, and images.
//! Images arrive as raw RGBA from the sandboxed content worker; the UI never
//! parses image formats.
use crate::theme::{ACCENT_HOVER, tint};
use base64::Engine as _;
use gpui_kit::{
    FontStyle, FontWeight, HighlightStyle, Hsla, RenderImage, SharedString, StrikethroughStyle,
    UnderlineStyle, px, rgb,
};
use nuschel_core::{CodeRun, ContentBlock, MessageDocument, Span, Style, TextRole};
use std::{borrow::Cow, ops::Range, sync::Arc};

/// Styles by byte range, sorted and disjoint.
pub type Highlights = Arc<[(Range<usize>, HighlightStyle)]>;
/// Link targets (http, https or mumble) by byte range, sorted and disjoint.
pub type Links = Arc<[(Range<usize>, SharedString)]>;

/// What a text block is.
#[derive(Clone, PartialEq)]
pub enum Kind {
    Paragraph,
    /// Levels 1 to 3.
    Heading(u8),
    /// Nested from depth 1 to 4, with its bullet or number.
    Item {
        depth: u8,
        marker: SharedString,
    },
    Quote,
    /// A horizontal rule, without text.
    Rule,
}

#[derive(Clone)]
pub enum Block {
    /// Text without the newlines that separated it from a neighbouring block.
    Text {
        text: SharedString,
        highlights: Highlights,
        links: Links,
        /// Inline code, shown in a monospace font.
        code: Arc<[Range<usize>]>,
        kind: Kind,
    },
    Code {
        /// The language shown above the code ("code" when unknown).
        language: SharedString,
        text: SharedString,
        /// Syntax colours.
        highlights: Highlights,
    },
    Image {
        image: Arc<RenderImage>,
        width: u32,
        height: u32,
        /// The original `data:` URI (viewer and Save as), when known.
        source: Option<Arc<str>>,
    },
}

impl Block {
    /// The text a block shows, if it shows text (code included).
    pub fn text(&self) -> Option<&SharedString> {
        match self {
            Block::Text { text, .. } | Block::Code { text, .. } => Some(text),
            Block::Image { .. } => None,
        }
    }
}

#[derive(Clone, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

/// Build a GPUI image from worker RGBA pixels (GPUI expects BGRA).
pub fn render_image(width: u32, height: u32, rgba: &str) -> Option<Arc<RenderImage>> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }
    let mut pixels = base64::engine::general_purpose::STANDARD
        .decode(rgba)
        .ok()?;
    if pixels.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, pixels)?;
    Some(crate::images::track(Arc::new(RenderImage::new(vec![
        image::Frame::new(buffer),
    ]))))
}

impl From<MessageDocument> for Document {
    fn from(document: MessageDocument) -> Self {
        let last = document.blocks.len().saturating_sub(1);
        Self {
            blocks: document
                .blocks
                .into_iter()
                .enumerate()
                .map(|(index, block)| match block {
                    ContentBlock::Text { text, spans, role } => {
                        text_block(text, spans, role, index, last)
                    }
                    ContentBlock::Code { language, runs } => code(language, runs),
                    ContentBlock::Image {
                        width,
                        height,
                        rgba,
                        source,
                    } => match render_image(width, height, &rgba) {
                        Some(image) => Block::Image {
                            image,
                            width,
                            height,
                            source: source.map(Arc::from),
                        },
                        None => text_block(
                            "[image]".into(),
                            Vec::new(),
                            TextRole::Paragraph,
                            index,
                            last,
                        ),
                    },
                })
                .collect(),
        }
    }
}

/// A sender's colour made legible on the dark background: its hue stays,
/// a dark one gets lighter.
fn legible([red, green, blue]: [u8; 3]) -> Hsla {
    let mut color: Hsla = rgb(u32::from_be_bytes([0, red, green, blue])).into();
    color.l = color.l.max(0.65);
    color
}

fn highlight(style: Style, link: bool) -> HighlightStyle {
    let line = || px(1.);
    HighlightStyle {
        color: if link {
            Some(rgb(ACCENT_HOVER).into())
        } else {
            style.color.map(legible)
        },
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then(|| UnderlineStyle {
            thickness: line(),
            ..Default::default()
        }),
        strikethrough: style.strike.then(|| StrikethroughStyle {
            thickness: line(),
            ..Default::default()
        }),
        background_color: style.code.then(|| tint(0xffffff, 0x14).into()),
        ..Default::default()
    }
}

/// A text block without the newlines that separate it from a neighbouring
/// block (the block layout spaces them; newlines inside it are kept), its
/// styles and links, and what it is: all checked again here, since the
/// worker's reply is not trusted to be ordered, in range or limited to the
/// allowed schemes.
fn text_block(text: String, spans: Vec<Span>, role: TextRole, index: usize, last: usize) -> Block {
    let mut kept = 0..text.len();
    if index < last {
        kept.end = text.trim_end_matches(['\n', '\r']).len();
    }
    if index > 0 {
        kept.start = kept.end - text[..kept.end].trim_start_matches(['\n', '\r']).len();
    }
    let kind = match role {
        TextRole::Paragraph => Kind::Paragraph,
        TextRole::Heading(level) => Kind::Heading(level.clamp(1, 3)),
        TextRole::Item { depth, marker } => Kind::Item {
            depth: depth.clamp(1, 4),
            marker: marker.chars().take(8).collect::<String>().into(),
        },
        TextRole::Quote => Kind::Quote,
        TextRole::Rule => Kind::Rule,
    };
    if kind == Kind::Rule {
        kept = 0..0;
    }
    let (mut highlights, mut links, mut code) = (Vec::new(), Vec::new(), Vec::new());
    let mut covered = kept.start;
    for span in spans {
        let range = span.start.max(covered)..span.end.min(kept.end);
        if range.is_empty()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }
        covered = range.end;
        let range = range.start - kept.start..range.end - kept.start;
        let link = span.link.filter(|url| nuschel_core::allowed_link(url));
        if span.style.is_plain() && link.is_none() {
            continue;
        }
        highlights.push((range.clone(), highlight(span.style, link.is_some())));
        if span.style.code {
            code.push(range.clone());
        }
        if let Some(url) = link {
            links.push((range, SharedString::from(url)));
        }
    }
    let text = if kept.len() == text.len() {
        text.into()
    } else {
        text[kept].to_owned().into()
    };
    Block::Text {
        text,
        highlights: highlights.into(),
        links: links.into(),
        code: code.into(),
        kind,
    }
}

/// A code block's runs joined into one text with their colours as ranges.
fn code(language: Option<String>, runs: Vec<CodeRun>) -> Block {
    let mut text = String::new();
    let mut highlights = Vec::with_capacity(runs.len());
    for run in runs {
        let start = text.len();
        text.push_str(&run.text);
        let [red, green, blue, _] = run.foreground;
        highlights.push((
            start..text.len(),
            HighlightStyle {
                color: Some(rgb(u32::from_be_bytes([0, red, green, blue])).into()),
                font_weight: run.bold.then_some(FontWeight::BOLD),
                font_style: run.italic.then_some(FontStyle::Italic),
                ..Default::default()
            },
        ));
    }
    let trimmed = text.trim_end_matches(['\n', '\r']).len();
    text.truncate(trimmed);
    highlights.retain_mut(|(range, _)| {
        range.end = range.end.min(trimmed);
        range.start < range.end
    });
    Block::Code {
        language: language.map_or("code".into(), SharedString::from),
        text: text.into(),
        highlights: highlights.into(),
    }
}

/// The file inside an inline `data:image/...;base64,` URI: its bytes and the
/// extension for its type. Only base64 (with Mumble's percent-encoding) is
/// decoded here; the image format itself is never parsed in this process.
pub fn data_uri_file(uri: &str) -> Option<(Vec<u8>, &'static str)> {
    let rest = uri.trim().strip_prefix("data:image/")?;
    let (kind, payload) = rest.split_once(";base64,")?;
    let extension = match kind.to_ascii_lowercase().as_str() {
        "png" => "png",
        "jpeg" | "jpg" => "jpg",
        "gif" => "gif",
        _ => return None,
    };
    let mut text = String::with_capacity(payload.len());
    let mut chars = payload.chars().filter(|c| !c.is_whitespace());
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            text.push(u8::from_str_radix(&hex, 16).ok()? as char);
        } else {
            text.push(c);
        }
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    Some((bytes, extension))
}

impl Document {
    pub fn first_image(&self) -> Option<Arc<RenderImage>> {
        self.blocks.iter().find_map(|block| match block {
            Block::Image { image, .. } => Some(image.clone()),
            _ => None,
        })
    }

    /// What "Copy message" copies: text as text, code as its code, one block
    /// per line; images are left out.
    pub fn plain_text(&self) -> String {
        let parts: Vec<Cow<str>> = self
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text {
                    text,
                    kind: Kind::Item { marker, .. },
                    ..
                } => Some(format!("{marker} {text}").into()),
                _ => Some(Cow::from(block.text()?.as_ref())),
            })
            .filter(|text| !text.is_empty())
            .collect();
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;
    use super::{Block, Document, render_image};
    use base64::Engine as _;
    use nuschel_core::{CodeRun, ContentBlock, MessageDocument, Span, Style, TextRole};

    #[test]
    fn saved_images_keep_their_bytes_and_type() {
        // Mumble percent-encodes the base64 text: [0xfb, 0xff, 0x00] is "+/8A".
        let (bytes, extension) = super::data_uri_file("data:image/PNG;base64,%2B%2F8A").unwrap();
        assert_eq!(extension, "png");
        assert_eq!(bytes, [0xfb, 0xff, 0x00]);
        assert!(super::data_uri_file("data:image/svg+xml;base64,PHN2Zz4=").is_none());
        assert!(super::data_uri_file("https://example.invalid/a.png").is_none());
    }

    #[test]
    fn converts_worker_images_and_rejects_inconsistent_sizes() {
        let rgba = base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(render_image(2, 1, &rgba).is_some());
        assert!(render_image(3, 1, &rgba).is_none());
        assert!(render_image(2, 1, "not base64!").is_none());
        let document = Document::from(MessageDocument {
            blocks: vec![ContentBlock::Image {
                width: 9,
                height: 9,
                rgba,
                source: None,
            }],
        });
        assert!(matches!(document.blocks[0], Block::Text { ref text, .. } if text == "[image]"));
    }

    #[test]
    fn builds_blocks_once_and_copies_their_plain_text() {
        let run = |text: &str| CodeRun {
            text: text.into(),
            foreground: [0x11, 0x22, 0x33, 0xff],
            bold: true,
            italic: false,
        };
        let document = Document::from(MessageDocument {
            blocks: vec![
                ContentBlock::text("Hi!\nThe patch:\n\n"),
                ContentBlock::Code {
                    language: None,
                    runs: vec![run("fn main() {}"), run("\n")],
                },
                ContentBlock::Image {
                    width: 1,
                    height: 1,
                    rgba: base64::engine::general_purpose::STANDARD.encode([0, 0, 0, 0]),
                    source: None,
                },
                ContentBlock::text("\nThanks\n"),
            ],
        });
        assert!(
            matches!(&document.blocks[0], Block::Text { text, .. } if text == "Hi!\nThe patch:")
        );
        let Block::Code {
            language,
            text,
            highlights,
        } = &document.blocks[1]
        else {
            panic!("not code");
        };
        assert_eq!((language.as_ref(), text.as_ref()), ("code", "fn main() {}"));
        // The trailing newline's run is gone with the newline.
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].0, 0..12);
        assert_eq!(
            highlights[0].1.font_weight,
            Some(gpui_kit::FontWeight::BOLD)
        );
        // Only separators next to other blocks are trimmed: the last block keeps its end.
        assert!(matches!(&document.blocks[3], Block::Text { text, .. } if text == "Thanks\n"));
        assert_eq!(
            document.plain_text(),
            "Hi!\nThe patch:\nfn main() {}\nThanks\n"
        );
    }

    #[test]
    fn links_from_the_worker_are_checked_and_follow_the_trimmed_text() {
        let span = |start, end, url: &str| Span {
            start,
            end,
            link: Some(url.into()),
            ..Default::default()
        };
        // Between two code blocks, so its separating newlines are trimmed.
        let text = "\nsee a.example, b and c\n";
        let code = || ContentBlock::Code {
            language: None,
            runs: Vec::new(),
        };
        let document = Document::from(MessageDocument {
            blocks: vec![
                code(),
                ContentBlock::Text {
                    text: text.into(),
                    spans: vec![
                        span(5, 14, "https://a.example"),
                        // Overlapping, disallowed or out of range: clipped or dropped.
                        span(10, 17, "https://overlap.example"),
                        span(16, 17, "javascript:alert(1)"),
                        span(22, 99, "https://c.example"),
                    ],
                    role: TextRole::Paragraph,
                },
                code(),
            ],
        });
        let Block::Text {
            text,
            links,
            highlights,
            ..
        } = &document.blocks[1]
        else {
            panic!("not text");
        };
        assert_eq!(text.as_ref(), "see a.example, b and c");
        let found: Vec<_> = links
            .iter()
            .map(|(range, url)| (&text[range.clone()], url.as_ref()))
            .collect();
        assert_eq!(
            found,
            [
                ("a.example", "https://a.example"),
                (", b", "https://overlap.example"),
                ("c", "https://c.example")
            ]
        );
        assert_eq!(highlights.len(), 3);
    }

    #[test]
    fn styles_and_roles_are_checked_and_colours_kept_legible() {
        let text = |text: &str, spans, role| ContentBlock::Text {
            text: text.into(),
            spans,
            role,
        };
        let styled = |start, end, style| Span {
            start,
            end,
            style,
            link: None,
        };
        let dark_bold = Style {
            bold: true,
            color: Some([0, 0, 96]),
            ..Style::default()
        };
        let code = Style {
            code: true,
            ..Style::default()
        };
        let document = Document::from(MessageDocument {
            blocks: vec![
                text(
                    "dark code",
                    vec![styled(0, 4, dark_bold), styled(5, 9, code)],
                    TextRole::Heading(9),
                ),
                text(
                    "item",
                    Vec::new(),
                    TextRole::Item {
                        depth: 40,
                        marker: "1234567890.".into(),
                    },
                ),
                text("not shown", Vec::new(), TextRole::Rule),
            ],
        });
        let Block::Text {
            highlights,
            code,
            kind,
            ..
        } = &document.blocks[0]
        else {
            panic!("not text");
        };
        assert!(*kind == Kind::Heading(3));
        assert_eq!(
            highlights[0].1.font_weight,
            Some(gpui_kit::FontWeight::BOLD)
        );
        let color = highlights[0].1.color.unwrap();
        assert!(color.l >= 0.65 && (color.h - 240. / 360.).abs() < 0.01);
        assert!(highlights[1].1.background_color.is_some());
        assert_eq!(code.first(), Some(&(5..9)));
        assert!(matches!(
            &document.blocks[1],
            Block::Text { kind: Kind::Item { depth: 4, marker }, .. } if marker == "12345678"
        ));
        assert!(
            matches!(&document.blocks[2], Block::Text { text, kind: Kind::Rule, .. } if text.is_empty())
        );
        assert_eq!(document.plain_text(), "dark code\n12345678 item");
    }
}
