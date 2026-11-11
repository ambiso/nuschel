// SPDX-License-Identifier: EUPL-1.2
//! Worker documents converted once into renderer-ready values: shared text,
//! code with its highlight ranges, and images. Images arrive as raw RGBA from
//! the sandboxed content worker; the UI never parses image formats.
use base64::Engine as _;
use gpui_kit::{FontStyle, FontWeight, HighlightStyle, RenderImage, SharedString, rgb};
use nuschel_core::{CodeRun, ContentBlock, MessageDocument, Span};
use std::{ops::Range, sync::Arc};

/// Styles by byte range, sorted and disjoint.
pub type Highlights = Arc<[(Range<usize>, HighlightStyle)]>;
/// Link targets (http, https or mumble) by byte range, sorted and disjoint.
pub type Links = Arc<[(Range<usize>, SharedString)]>;

#[derive(Clone)]
pub enum Block {
    /// Text without the newlines that separated it from a neighbouring block.
    Text {
        text: SharedString,
        highlights: Highlights,
        links: Links,
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
                    ContentBlock::Text { text, spans } => text_block(text, spans, index, last),
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
                        None => text_block("[image]".into(), Vec::new(), index, last),
                    },
                })
                .collect(),
        }
    }
}

/// A text block without the newlines that separate it from a neighbouring
/// block (the block layout spaces them; newlines inside it are kept), and
/// its links: checked again here, since the worker's reply is not trusted
/// to be ordered, in range or limited to the allowed schemes.
fn text_block(text: String, spans: Vec<Span>, index: usize, last: usize) -> Block {
    let mut kept = 0..text.len();
    if index < last {
        kept.end = text.trim_end_matches(['\n', '\r']).len();
    }
    if index > 0 {
        kept.start = kept.end - text[..kept.end].trim_start_matches(['\n', '\r']).len();
    }
    let mut links = Vec::new();
    let mut covered = kept.start;
    for span in spans {
        let Some(url) = span.link.filter(|url| nuschel_core::allowed_link(url)) else {
            continue;
        };
        let range = span.start.max(covered)..span.end.min(kept.end);
        if range.is_empty()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }
        covered = range.end;
        links.push((
            range.start - kept.start..range.end - kept.start,
            SharedString::from(url),
        ));
    }
    let link = HighlightStyle {
        color: Some(rgb(crate::theme::ACCENT_HOVER).into()),
        ..Default::default()
    };
    let highlights = links
        .iter()
        .map(|(range, _)| (range.clone(), link))
        .collect();
    let text = if kept.len() == text.len() {
        text.into()
    } else {
        text[kept].to_owned().into()
    };
    Block::Text {
        text,
        highlights,
        links: links.into(),
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
        let parts: Vec<&str> = self
            .blocks
            .iter()
            .filter_map(Block::text)
            .map(|text| text.as_ref())
            .filter(|text| !text.is_empty())
            .collect();
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::{Block, Document, render_image};
    use base64::Engine as _;
    use nuschel_core::{CodeRun, ContentBlock, MessageDocument, Span};

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
                ContentBlock::Text {
                    text: "Hi!\nThe patch:\n\n".into(),
                    spans: Vec::new(),
                },
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
                ContentBlock::Text {
                    text: "\nThanks\n".into(),
                    spans: Vec::new(),
                },
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
                },
                code(),
            ],
        });
        let Block::Text {
            text,
            links,
            highlights,
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
}
