// SPDX-License-Identifier: EUPL-1.2
//! Worker documents converted once into renderer-ready values: shared text
//! with its styles and links, code with its highlight ranges, and images.
//! Images arrive as raw RGBA from the sandboxed content worker; the UI never
//! parses image formats.
use crate::theme::{self, CODE_SPAN, LINK, MAIN};
use base64::Engine as _;
use gpui_kit::{
    FontStyle, FontWeight, HighlightStyle, Hsla, RenderImage, SharedString, StrikethroughStyle,
    UnderlineStyle, px, rgb, rgba,
};
use nuschl_core::{CodeRun, ContentBlock, MessageDocument, Span, Style, TextRole};
use std::{borrow::Cow, ops::Range, sync::Arc};

type Styles = Vec<(Range<usize>, HighlightStyle)>;

/// Styles by byte range, sorted and disjoint, in the dark and in the light
/// palette's colours. It dereferences to the active palette's, so a theme
/// change needs no conversion again.
#[derive(Clone)]
pub struct Highlights {
    dark: Arc<[(Range<usize>, HighlightStyle)]>,
    light: Arc<[(Range<usize>, HighlightStyle)]>,
}

impl Highlights {
    fn new(dark: Styles, light: Styles) -> Self {
        if dark == light {
            return dark.into();
        }
        Self {
            dark: dark.into(),
            light: light.into(),
        }
    }
}

/// The same styles in both palettes.
impl From<Styles> for Highlights {
    fn from(styles: Styles) -> Self {
        let styles: Arc<[_]> = styles.into();
        Self {
            dark: styles.clone(),
            light: styles,
        }
    }
}

impl std::ops::Deref for Highlights {
    type Target = [(Range<usize>, HighlightStyle)];
    fn deref(&self) -> &Self::Target {
        if theme::is_light() {
            &self.light
        } else {
            &self.dark
        }
    }
}
/// Link targets (http, https or mumble) by byte range, sorted and disjoint.
pub type Links = Arc<[(Range<usize>, SharedString)]>;

/// An inline formula: its text as written (`$…$`, `\(…\)`) by byte range,
/// and the LaTeX the math worker renders.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineMath {
    pub range: Range<usize>,
    pub tex: SharedString,
}

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
        /// Inline formulas, sorted and disjoint.
        math: Arc<[InlineMath]>,
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
    /// A display formula: `source` as written (copied, read out, and shown
    /// when the formula cannot be), `tex` for the math worker. `joined`:
    /// written on the same line as the text before it, and after it (copying
    /// adds no line break there).
    Math {
        source: SharedString,
        tex: SharedString,
        joined: (bool, bool),
    },
}

impl Block {
    /// The text a block shows, if it shows text (code included), or
    /// stands for (a formula's source).
    pub fn text(&self) -> Option<&SharedString> {
        match self {
            Block::Text { text, .. } | Block::Code { text, .. } => Some(text),
            Block::Math { source, .. } => Some(source),
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
        // Whether each block continues the line of the text before it, and
        // the text after it continues its line.
        let text = |index: Option<usize>| match index.and_then(|index| document.blocks.get(index)) {
            Some(ContentBlock::Text { text, .. }) if !text.is_empty() => Some(text.clone()),
            _ => None,
        };
        let joined: Vec<(bool, bool)> = (0..document.blocks.len())
            .map(|index| {
                let before = text(index.checked_sub(1)).is_some_and(|text| !text.ends_with('\n'));
                let after = text(Some(index + 1)).is_some_and(|text| !text.starts_with('\n'));
                (before, after)
            })
            .collect();
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
                    ContentBlock::Math { source, tex } => {
                        math_block(source, tex, joined[index], index, last)
                    }
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

fn opaque([red, green, blue]: [u8; 3]) -> Hsla {
    rgb(u32::from_be_bytes([0, red, green, blue])).into()
}

/// A span's style in the light or the dark palette. A sender's colour keeps
/// its hue and gets as much lighter or darker as it needs to be readable.
fn highlight(style: Style, link: bool, light: bool) -> HighlightStyle {
    let line = || px(1.);
    HighlightStyle {
        color: if link {
            Some(rgba(LINK.get(light)).into())
        } else {
            style
                .color
                .map(|color| theme::readable(opaque(color), MAIN, light))
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
        background_color: style.code.then(|| rgba(CODE_SPAN.get(light)).into()),
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
    let (mut dark, mut light, mut links, mut code, mut math) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
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
        if let Some(tex) = span.math {
            // Only whole formulas within the limit; their text stays plain.
            if range.len() == span.end - span.start && tex.len() <= nuschl_ipc::math::MAX_TEX_BYTES
            {
                math.push(InlineMath {
                    range,
                    tex: tex.into(),
                });
            }
            continue;
        }
        let link = span.link.filter(|url| nuschl_core::allowed_link(url));
        if span.style.is_plain() && link.is_none() {
            continue;
        }
        dark.push((range.clone(), highlight(span.style, link.is_some(), false)));
        light.push((range.clone(), highlight(span.style, link.is_some(), true)));
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
        highlights: Highlights::new(dark, light),
        links: links.into(),
        code: code.into(),
        math: math.into(),
        kind,
    }
}

/// A display formula, or its source as a paragraph when its LaTeX is too
/// long to send to the math worker.
fn math_block(
    source: String,
    tex: String,
    joined: (bool, bool),
    index: usize,
    last: usize,
) -> Block {
    if tex.len() > nuschl_ipc::math::MAX_TEX_BYTES || tex.trim().is_empty() {
        return text_block(source, Vec::new(), TextRole::Paragraph, index, last);
    }
    Block::Math {
        source: source.into(),
        tex: tex.into(),
        joined,
    }
}

/// A code block's runs joined into one text with their colours as ranges.
fn code(language: Option<String>, runs: Vec<CodeRun>) -> Block {
    let mut text = String::new();
    let mut dark = Vec::with_capacity(runs.len());
    for run in runs {
        let start = text.len();
        text.push_str(&run.text);
        let [red, green, blue, _] = run.foreground;
        dark.push((
            start..text.len(),
            HighlightStyle {
                color: Some(opaque([red, green, blue])),
                font_weight: run.bold.then_some(FontWeight::BOLD),
                font_style: run.italic.then_some(FontStyle::Italic),
                ..Default::default()
            },
        ));
    }
    let trimmed = text.trim_end_matches(['\n', '\r']).len();
    text.truncate(trimmed);
    dark.retain_mut(|(range, _)| {
        range.end = range.end.min(trimmed);
        range.start < range.end
    });
    let light = dark
        .iter()
        .map(|(range, style)| {
            let color = style.color.map(|color| theme::code_color(color, true));
            (range.clone(), HighlightStyle { color, ..*style })
        })
        .collect();
    Block::Code {
        language: language.map_or("code".into(), SharedString::from),
        text: text.into(),
        highlights: Highlights::new(dark, light),
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
        "webp" => "webp",
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

    /// What "Copy message" copies: text as text, code as its code, formulas
    /// as written, one block per line (a formula written within a line stays
    /// in it); images are left out.
    pub fn plain_text(&self) -> String {
        self.join(self.blocks.iter().enumerate().filter_map(|(index, block)| {
            let text = match block {
                Block::Text {
                    text,
                    kind: Kind::Item { marker, .. },
                    ..
                } => format!("{marker} {text}").into(),
                _ => Cow::from(block.text()?.as_ref()),
            };
            Some((index, text))
        }))
    }

    /// Blocks' texts (by block index, in order) on lines of their own, apart
    /// from display formulas written within a line.
    pub fn join<'a>(&self, parts: impl IntoIterator<Item = (usize, Cow<'a, str>)>) -> String {
        let mut joined = String::new();
        let mut previous = None;
        for (index, part) in parts {
            if part.is_empty() {
                continue;
            }
            if previous.is_some() {
                let same_line = previous == index.checked_sub(1)
                    && (matches!(
                        self.blocks.get(index),
                        Some(Block::Math {
                            joined: (true, _),
                            ..
                        })
                    ) || matches!(
                        index
                            .checked_sub(1)
                            .and_then(|index| self.blocks.get(index)),
                        Some(Block::Math {
                            joined: (_, true),
                            ..
                        })
                    ));
                if !same_line {
                    joined.push('\n');
                }
            }
            joined.push_str(&part);
            previous = Some(index);
        }
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;
    use super::{Block, Document, render_image};
    use base64::Engine as _;
    use nuschl_core::{CodeRun, ContentBlock, MessageDocument, Span, Style, TextRole};

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
            math: None,
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
        // The light palette keeps navy as it is: it reads on white.
        crate::theme::set_light(true);
        let color = highlights[0].1.color.unwrap();
        assert!(color.l < 0.2 && (color.h - 240. / 360.).abs() < 0.01);
        crate::theme::set_light(false);
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

    #[test]
    fn formulas_keep_their_source_for_copying_and_screen_readers() {
        let math = |start, end, tex: &str| Span {
            start,
            end,
            math: Some(tex.into()),
            ..Default::default()
        };
        let document = Document::from(MessageDocument {
            blocks: vec![
                ContentBlock::Text {
                    text: "Area $\\pi r^2$ and $x$\n".into(),
                    spans: vec![math(5, 14, "\\pi r^2"), math(19, 22, "x")],
                    role: TextRole::Paragraph,
                },
                ContentBlock::Math {
                    source: "$$\\sum_i x_i$$".into(),
                    tex: "\\sum_i x_i".into(),
                },
                ContentBlock::Math {
                    source: "$$too long$$".into(),
                    tex: "x".repeat(nuschl_ipc::math::MAX_TEX_BYTES + 1),
                },
            ],
        });
        let Block::Text {
            text,
            math,
            highlights,
            ..
        } = &document.blocks[0]
        else {
            panic!("not text");
        };
        let formulas: Vec<_> = math
            .iter()
            .map(|formula| (&text[formula.range.clone()], formula.tex.as_ref()))
            .collect();
        assert_eq!(formulas, [("$\\pi r^2$", "\\pi r^2"), ("$x$", "x")]);
        // Formulas are not styled text.
        assert!(highlights.is_empty());
        assert!(matches!(&document.blocks[1], Block::Math { tex, .. } if tex == "\\sum_i x_i"));
        // A formula too long for the worker is its source as text.
        assert!(matches!(&document.blocks[2], Block::Text { text, .. } if text == "$$too long$$"));
        assert_eq!(
            document.plain_text(),
            "Area $\\pi r^2$ and $x$\n$$\\sum_i x_i$$\n$$too long$$"
        );
        // A display formula within a line is copied within it.
        let document = Document::from(MessageDocument {
            blocks: vec![
                ContentBlock::text("so "),
                ContentBlock::Math {
                    source: "$$x$$".into(),
                    tex: "x".into(),
                },
                ContentBlock::text(" and\n"),
                ContentBlock::Math {
                    source: "$$y$$".into(),
                    tex: "y".into(),
                },
            ],
        });
        assert_eq!(document.plain_text(), "so $$x$$ and\n$$y$$");
    }
}
